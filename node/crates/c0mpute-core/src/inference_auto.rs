//! The house LLM runtime: every capable node also answers `infernet.inference`.
//!
//! `inference.rs` serves chat from the operator's own OpenAI-compatible
//! runtime (Ollama on `127.0.0.1:11434` by default). Most nodes run none, so
//! only a handful advertised `infernet:inference`. When nothing answers at the
//! default address, the operator did not point `C0MPUTE_INFERENCE_URL`
//! somewhere, and the box passes the gates, the worker provisions its own:
//!
//! - a prebuilt llama.cpp `llama-server` from the ggml-org GitHub release for
//!   this platform, pinned to a build tag and SHA-256 (no cmake, no sudo);
//! - one house GGUF from Hugging Face, pinned to a repo revision and SHA-256
//!   (every shard of a split file): Qwen2.5 7B Instruct Q4_K_M (~4.7 GB, the
//!   model the fleet's apps ask for) on a box with twice `min_ram_gb`
//!   (16 GiB), Qwen2.5 1.5B Instruct (~1.1 GB) on one with `min_ram_gb`
//!   (8 GiB), else Qwen2.5 0.5B Instruct (~0.5 GB) on one with half of that;
//!
//! and runs it as a supervised child on `127.0.0.1:AUTO_PORT` (restarted when
//! it exits), with the model served under the Ollama-style id buyers ask for
//! (`qwen2.5:7b`, `qwen2.5:1.5b` or `qwen2.5:0.5b`), so the node advertises
//! `infernet:inference` and `infernet:inference:model:<that id>`.
//!
//! The runtime runs at low CPU priority (`--prio -1`) and the node bids on a
//! chat only while no song or image is rendering and the load is under
//! `max_load_per_cpu`, so music, image and video work keeps the box.
//! Gates before installing: `min_ram_gb / 2` total RAM, `min_free_ram_gb`
//! available (halved for 0.5B), `min_free_disk_gb` free in the data dir.
//! Opt out with `C0MPUTE_INFERENCE_AUTO=0` or `[inference] auto_install = false`.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::{info, warn};

use crate::config::InferenceConfig;

/// Loopback port of the house runtime (Ollama keeps 11434).
pub const AUTO_PORT: u16 = 11439;
const GIB: u64 = 1024 * 1024 * 1024;
const INSTALL_RETRY_BACKOFF_SECS: u64 = 6 * 60 * 60;
/// Whole context, shared by the slots: 4096 tokens per answer.
const CTX_TOKENS: u32 = 8192;
const SLOTS: u32 = crate::inference::MAX_CONCURRENT as u32;
const MAX_THREADS: usize = 8;

pub fn auto_url() -> String {
    format!("http://127.0.0.1:{AUTO_PORT}")
}

/// llama.cpp build, pinned. Bump the tag and every SHA-256 together.
pub const LLAMA_BUILD: &str = "b12061";

/// The release tarball for this platform: (url, sha256), or None where no
/// prebuilt CPU build is pinned.
fn llama_asset() -> Option<(&'static str, &'static str)> {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some((
            "https://github.com/ggml-org/llama.cpp/releases/download/b12061/llama-b12061-bin-ubuntu-x64.tar.gz",
            "8a968cd14e2546d0762db37c1d8b83bbd38fa62eeb63f99a01ad197f3d0c9c23",
        ))
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some((
            "https://github.com/ggml-org/llama.cpp/releases/download/b12061/llama-b12061-bin-ubuntu-arm64.tar.gz",
            "c4dad78ffd0000aa4d817fa9cbdd05dc658b73ab90b27f7143bd5c5542d57f9c",
        ))
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some((
            "https://github.com/ggml-org/llama.cpp/releases/download/b12061/llama-b12061-bin-macos-arm64.tar.gz",
            "b107a8373f7af640ff2744521792d3bc4742b9a86e73f52aa8636b30e3e7c554",
        ))
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some((
            "https://github.com/ggml-org/llama.cpp/releases/download/b12061/llama-b12061-bin-macos-x64.tar.gz",
            "e5790e87e886f621b857514699ffc797b6df8b518a1fc2acd0d0a2d9c60e42c3",
        ))
    } else {
        None
    }
}

pub fn platform_supported() -> bool {
    llama_asset().is_some()
}

/// One house model: what buyers call it, and where its GGUF comes from.
#[derive(Debug, PartialEq, Eq)]
pub struct HouseModel {
    /// The id served at `/v1/models` and advertised as a model tag.
    pub id: &'static str,
    pub file: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub size: &'static str,
    /// Further shards of a split GGUF (`-0000N-of-0000M`), downloaded next to
    /// `file`; llama-server loads them when pointed at the first one.
    pub parts: &'static [GgufPart],
    /// Free disk (GB) needed to install it; the config floor applies too.
    pub disk_gb: u64,
}

/// One extra shard of a split GGUF, pinned like the first.
#[derive(Debug, PartialEq, Eq)]
pub struct GgufPart {
    pub file: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
}

/// Qwen2.5 7B Instruct, the model the fleet's apps ask for by default
/// (`@profullstack/ai-providers`). Qwen ships the Q4_K_M as two shards.
pub const QWEN_7B: HouseModel = HouseModel {
    id: "qwen2.5:7b",
    file: "qwen2.5-7b-instruct-q4_k_m-00001-of-00002.gguf",
    url: "https://huggingface.co/Qwen/Qwen2.5-7B-Instruct-GGUF/resolve/bb5d59e06d9551d752d08b292a50eb208b07ab1f/qwen2.5-7b-instruct-q4_k_m-00001-of-00002.gguf",
    sha256: "dfce12e3862a5283ccfb88221b48480e58745165de856439950d0f22590580db",
    size: "~4.7 GB",
    parts: &[GgufPart {
        file: "qwen2.5-7b-instruct-q4_k_m-00002-of-00002.gguf",
        url: "https://huggingface.co/Qwen/Qwen2.5-7B-Instruct-GGUF/resolve/bb5d59e06d9551d752d08b292a50eb208b07ab1f/qwen2.5-7b-instruct-q4_k_m-00002-of-00002.gguf",
        sha256: "539cf93f78e887edea1c04e2d7d8cdaca9d01dae9c9025bcb8accbe29df3d72a",
    }],
    disk_gb: 10,
};

pub const QWEN_1_5B: HouseModel = HouseModel {
    id: "qwen2.5:1.5b",
    file: "qwen2.5-1.5b-instruct-q4_k_m.gguf",
    url: "https://huggingface.co/Qwen/Qwen2.5-1.5B-Instruct-GGUF/resolve/91cad51170dc346986eccefdc2dd33a9da36ead9/qwen2.5-1.5b-instruct-q4_k_m.gguf",
    sha256: "6a1a2eb6d15622bf3c96857206351ba97e1af16c30d7a74ee38970e434e9407e",
    size: "~1.1 GB",
    parts: &[],
    disk_gb: 0,
};

pub const QWEN_0_5B: HouseModel = HouseModel {
    id: "qwen2.5:0.5b",
    file: "qwen2.5-0.5b-instruct-q4_k_m.gguf",
    url: "https://huggingface.co/Qwen/Qwen2.5-0.5B-Instruct-GGUF/resolve/9217f5db79a29953eb74d5343926648285ec7e67/qwen2.5-0.5b-instruct-q4_k_m.gguf",
    sha256: "74a4da8c9fdbcd15bd1f6d01d621410d31c6fc00986f5eb687824e7b93d7a9db",
    size: "~0.5 GB",
    parts: &[],
    disk_gb: 0,
};

/// Every house model, biggest first.
pub const HOUSE_MODELS: [&HouseModel; 3] = [&QWEN_7B, &QWEN_1_5B, &QWEN_0_5B];

/// The model a box with `total_ram` bytes runs, and the available RAM it
/// needs to start it (GiB), or None when the box is too small for either.
pub fn pick_model(config: &InferenceConfig, total_ram: u64) -> Option<(&'static HouseModel, u64)> {
    // Twice the 1.5B floor (16 GiB by default) runs the 7B the fleet asks for.
    if total_ram >= config.min_ram_gb.saturating_mul(2).saturating_mul(GIB) {
        Some((&QWEN_7B, config.min_free_ram_gb.saturating_mul(2)))
    } else if total_ram >= config.min_ram_gb.saturating_mul(GIB) {
        Some((&QWEN_1_5B, config.min_free_ram_gb))
    } else if total_ram >= (config.min_ram_gb / 2).max(1).saturating_mul(GIB) {
        Some((&QWEN_0_5B, (config.min_free_ram_gb / 2).max(1)))
    } else {
        None
    }
}

pub fn root() -> PathBuf {
    crate::config::data_dir().unwrap_or_else(|| PathBuf::from("./c0mpute-data")).join("inference")
}
fn llama_dir() -> PathBuf {
    root().join(format!("llama-{LLAMA_BUILD}"))
}
pub fn llama_server() -> PathBuf {
    llama_dir().join("llama-server")
}
fn model_path(model: &HouseModel) -> PathBuf {
    root().join("models").join(model.file)
}
fn ready_marker(model: &HouseModel) -> PathBuf {
    root().join(format!(".ready-{}", model.file))
}
fn retry_marker() -> PathBuf {
    root().join(".retry-after")
}
fn log_path() -> PathBuf {
    root().join("llama-server.log")
}
fn ready_stamp(model: &HouseModel) -> String {
    let parts: String = model.parts.iter().map(|p| format!(" part={}", p.sha256)).collect();
    format!("llama={LLAMA_BUILD} model={}{parts}\n", model.sha256)
}

/// The runtime binary and this model are installed and checked.
pub fn installed(model: &HouseModel) -> bool {
    platform_supported()
        && llama_server().is_file()
        && model_path(model).is_file()
        && model.parts.iter().all(|p| root().join("models").join(p.file).is_file())
        && std::fs::read_to_string(ready_marker(model)).is_ok_and(|s| s == ready_stamp(model))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn in_install_backoff() -> bool {
    std::fs::read_to_string(retry_marker())
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .is_some_and(|retry_after| now_secs() < retry_after)
}

fn gib(bytes: u64) -> f64 {
    bytes as f64 / GIB as f64
}

/// Why this box would not run the house runtime, or None when it would.
/// Ignores the install itself: [`gate_reason`] words that part.
pub fn host_gate(config: &InferenceConfig, total: u64, available: u64, free_disk: u64, installed: bool) -> Option<String> {
    if !config.auto_install_enabled() {
        return Some("turned off (C0MPUTE_INFERENCE_AUTO=0 or [inference] auto_install = false)".into());
    }
    let Some((model, min_free)) = pick_model(config, total) else {
        return Some(format!("{:.0} GiB RAM, the house model needs {} GiB", gib(total), (config.min_ram_gb / 2).max(1)));
    };
    let disk_gb = config.min_free_disk_gb.max(model.disk_gb);
    if !installed && free_disk < disk_gb.saturating_mul(GIB) {
        return Some(format!("{:.1} GB free disk, installing {} needs {} GB", gib(free_disk), model.id, disk_gb));
    }
    if available < min_free.saturating_mul(GIB) {
        return Some(format!("load gate: {:.1} GiB RAM free, {} needs {min_free} GiB free", gib(available), model.id));
    }
    None
}

/// What `c0mpute status` says about the house runtime when it is not
/// answering: off, too small, or not installed/started yet.
pub fn gate_reason(config: &InferenceConfig) -> String {
    if !platform_supported() {
        return "no Ollama or llama-server, and no prebuilt llama.cpp for this platform".into();
    }
    let mem = crate::diffusion::memory();
    let model = pick_model(config, mem.total).map(|(m, _)| m);
    let is_installed = model.is_some_and(installed);
    let disk = crate::speech::free_bytes_for(&root());
    if let Some(reason) = host_gate(config, mem.total, mem.available, disk, is_installed) {
        return reason;
    }
    let model = model.expect("host_gate passed, so a model was picked");
    if is_installed {
        format!("{} is installed; the worker starts llama-server on {} (see {})", model.id, auto_url(), log_path().display())
    } else if in_install_backoff() {
        format!("installing {} failed recently; the worker retries within 6 h (see the worker log)", model.id)
    } else {
        format!("not installed yet: the worker installs llama.cpp + {} ({}) in the background", model.id, model.size)
    }
}

// ─── install ──────────────────────────────────────────────────────────────

async fn download_verified(url: &str, path: &Path, expected_sha256: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(2 * 60 * 60))
        .build()?;
    let mut response = client.get(url).send().await?.error_for_status()?;
    let temp = path.with_extension("partial");
    let mut file = tokio::fs::File::create(&temp).await?;
    let mut hasher = Sha256::new();
    while let Some(chunk) = response.chunk().await? {
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    drop(file);
    let actual = hex::encode(hasher.finalize());
    if actual != expected_sha256 {
        let _ = tokio::fs::remove_file(&temp).await;
        bail!("SHA-256 mismatch for {url}: got {actual}");
    }
    tokio::fs::rename(&temp, path).await?;
    Ok(())
}

async fn file_sha256(path: &Path) -> Option<String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut f = std::fs::File::open(path).ok()?;
        let mut h = Sha256::new();
        std::io::copy(&mut f, &mut h).ok()?;
        Some(hex::encode(h.finalize()))
    })
    .await
    .ok()
    .flatten()
}

/// Download, unpack and self-test llama-server, then fetch `model`.
/// Idempotent; public so a foreground command can run it.
pub async fn install(model: &HouseModel) -> Result<()> {
    let Some((url, sha)) = llama_asset() else {
        bail!("no prebuilt llama.cpp is pinned for this platform");
    };
    let root = root();
    tokio::fs::create_dir_all(&root).await?;
    let _ = tokio::fs::remove_file(ready_marker(model)).await;

    if !llama_server().is_file() {
        let dir = llama_dir();
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await?;
        let tarball = root.join("llama.tar.gz");
        download_verified(url, &tarball, sha).await.with_context(|| format!("download llama.cpp {LLAMA_BUILD}"))?;
        let mut tar = Command::new("tar");
        tar.arg("-xzf").arg(&tarball).arg("-C").arg(&dir).arg("--strip-components=1");
        crate::speech::run_checked(tar, "unpack llama.cpp").await?;
        let _ = tokio::fs::remove_file(&tarball).await;
        if !llama_server().is_file() {
            bail!("the llama.cpp tarball has no llama-server");
        }
        // Old builds go once the new one is in place.
        if let Ok(mut entries) = tokio::fs::read_dir(&root).await {
            while let Ok(Some(e)) = entries.next_entry().await {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with("llama-b") && e.path() != dir {
                    let _ = tokio::fs::remove_dir_all(e.path()).await;
                }
            }
        }
    }
    let mut version = Command::new(llama_server());
    version.arg("--version").stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    crate::speech::run_checked(version, "run llama-server --version").await?;

    let shards = std::iter::once((model.file, model.url, model.sha256))
        .chain(model.parts.iter().map(|p| (p.file, p.url, p.sha256)));
    for (file, url, sha) in shards {
        let path = root.join("models").join(file);
        if file_sha256(&path).await.as_deref() != Some(sha) {
            let _ = tokio::fs::remove_file(&path).await;
            // Resumable with retries: the 7B shards are gigabytes.
            crate::diffusion::download_verified_streaming(url, &path, sha)
                .await
                .with_context(|| format!("download {} ({file})", model.id))?;
        }
    }
    // The other house models are dead weight once this one is in.
    for other in HOUSE_MODELS {
        if other.file != model.file {
            let _ = tokio::fs::remove_file(model_path(other)).await;
            for p in other.parts {
                let _ = tokio::fs::remove_file(root.join("models").join(p.file)).await;
            }
            let _ = tokio::fs::remove_file(ready_marker(other)).await;
        }
    }
    tokio::fs::write(ready_marker(model), ready_stamp(model)).await?;
    Ok(())
}

/// Installed already, or installed now if the box passes the gates.
/// Never fatal; a failure backs off for 6 h.
async fn ensure(config: &InferenceConfig) -> Option<&'static HouseModel> {
    if !platform_supported() {
        tracing::debug!("inference: no prebuilt llama.cpp for this platform; not installing");
        return None;
    }
    let mem = crate::diffusion::memory();
    let (model, _) = pick_model(config, mem.total)?;
    let is_installed = installed(model);
    let disk = crate::speech::free_bytes_for(&root());
    if let Some(reason) = host_gate(config, mem.total, mem.available, disk, is_installed) {
        info!(%reason, "inference: not running the house llama-server");
        return None;
    }
    if is_installed {
        return Some(model);
    }
    if in_install_backoff() {
        tracing::debug!("inference: house runtime install failed recently; waiting out the backoff");
        return None;
    }
    info!(model = model.id, size = model.size, build = LLAMA_BUILD, "installing llama.cpp + a house model so this node can serve infernet.inference");
    match install(model).await {
        Ok(()) => {
            let _ = tokio::fs::remove_file(retry_marker()).await;
            info!(model = model.id, root = %root().display(), "inference: house runtime installed");
            Some(model)
        }
        Err(e) => {
            let _ = tokio::fs::create_dir_all(root()).await;
            let _ = tokio::fs::write(retry_marker(), now_secs().saturating_add(INSTALL_RETRY_BACKOFF_SECS).to_string()).await;
            warn!(err = %format!("{e:#}"), "could not install the house LLM runtime; turn it off with C0MPUTE_INFERENCE_AUTO=0");
            None
        }
    }
}

// ─── supervised child ─────────────────────────────────────────────────────

/// Kill llama-servers left from our own binary path (an auto-update
/// re-executes the worker in place, and the child keeps the port).
#[cfg(target_os = "linux")]
fn kill_strays(bin: &Path) -> usize {
    let mut killed = 0;
    let Ok(entries) = std::fs::read_dir("/proc") else { return 0 };
    for e in entries.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else { continue };
        let Ok(exe) = std::fs::read_link(e.path().join("exe")) else { continue };
        if exe == bin {
            // SAFETY: plain kill(2) on a pid we just read.
            if unsafe { libc::kill(pid, libc::SIGKILL) } == 0 {
                killed += 1;
            }
        }
    }
    killed
}

#[cfg(not(target_os = "linux"))]
fn kill_strays(_bin: &Path) -> usize {
    0
}

fn server_command(model: &HouseModel) -> Command {
    let threads = crate::resources::cpu_threads_per_job().clamp(1, MAX_THREADS).to_string();
    let mut cmd = Command::new(llama_server());
    cmd.arg("-m")
        .arg(model_path(model))
        .args(["--host", "127.0.0.1", "--port", &AUTO_PORT.to_string()])
        .args(["--alias", model.id])
        .args(["-c", &CTX_TOKENS.to_string(), "-np", &SLOTS.to_string()])
        .args(["-t", &threads, "--threads-batch", &threads])
        // Low CPU priority: songs, images and video keep the box.
        .args(["--prio", "-1", "--no-webui"])
        .env("LD_LIBRARY_PATH", llama_dir())
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    if let Ok(log) = std::fs::File::create(log_path()) {
        if let Ok(err) = log.try_clone() {
            cmd.stderr(err);
        }
        cmd.stdout(log);
    }
    die_with_parent(&mut cmd);
    cmd
}

/// Linux: the child gets SIGTERM when the worker goes away.
#[cfg(target_os = "linux")]
fn die_with_parent(cmd: &mut Command) {
    // SAFETY: prctl(2) is async-signal-safe and touches no shared state.
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn die_with_parent(_cmd: &mut Command) {}

/// Run llama-server forever: restart when it exits, backing off from 5 s to
/// 5 min while it keeps dying young.
async fn supervise(model: &'static HouseModel) {
    let mut backoff = Duration::from_secs(5);
    loop {
        let strays = kill_strays(&llama_server());
        if strays > 0 {
            info!(strays, "inference: stopped a llama-server left by the previous worker binary");
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let started = Instant::now();
        match server_command(model).spawn() {
            Ok(mut child) => {
                info!(pid = ?child.id(), model = model.id, url = %auto_url(), "inference: house llama-server started");
                let status = child.wait().await;
                warn!(?status, log = %log_path().display(), "inference: house llama-server exited; restarting");
            }
            Err(e) => warn!(err = %e, "inference: could not start llama-server"),
        }
        backoff = if started.elapsed() > Duration::from_secs(10 * 60) {
            Duration::from_secs(5)
        } else {
            (backoff * 2).min(Duration::from_secs(300))
        };
        tokio::time::sleep(backoff).await;
    }
}

// ─── choosing the runtime ─────────────────────────────────────────────────

static ACTIVE: AtomicBool = AtomicBool::new(false);
/// `max_load_per_cpu` of the running house runtime, as f64 bits.
static MAX_LOAD_BITS: AtomicU64 = AtomicU64::new(0);

/// The worker serves from the house runtime (not the operator's).
pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// Load gates for a bid on the house runtime: never next to a song or an
/// image (they hold the same CPU), and only below the load ceiling.
pub fn can_take_job() -> bool {
    if !active() {
        return true;
    }
    let max = f64::from_bits(MAX_LOAD_BITS.load(Ordering::Relaxed));
    !crate::music::busy()
        && !crate::diffusion::busy()
        && crate::diffusion::idle_enough_with(crate::diffusion::load_avg_1m(), crate::diffusion::logical_cpus(), max)
}

/// The operator set `C0MPUTE_INFERENCE_URL` (to anything but off).
pub fn operator_configured() -> bool {
    std::env::var(crate::inference::RUNTIME_URL_ENV).is_ok_and(|v| !v.trim().is_empty())
}

/// Which runtime the worker serves `infernet.inference` from.
///
/// The operator's when it lists chat models (Ollama can start a little after
/// the worker, so it is asked three times over ~20 s) or when the operator
/// set `C0MPUTE_INFERENCE_URL`; else the house runtime when the box passes
/// the gates (installing it first, and starting it supervised); else the
/// operator's default address anyway, which keeps being probed every minute.
pub async fn choose_runtime(operator: String, config: &InferenceConfig) -> String {
    if operator_configured() {
        return operator;
    }
    for attempt in 0..3 {
        if crate::inference::list_models(&operator).await.is_ok_and(|m| !m.is_empty()) {
            return operator;
        }
        if attempt < 2 {
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    }
    let Some(model) = ensure(config).await else {
        return operator;
    };
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_ok() {
        MAX_LOAD_BITS.store(config.max_load_per_cpu.to_bits(), Ordering::Relaxed);
        ACTIVE.store(true, Ordering::Relaxed);
        tokio::spawn(supervise(model));
    }
    let base = auto_url();
    // Loading the model takes seconds; wait so the first advertise has it.
    for _ in 0..60 {
        if crate::inference::list_models(&base).await.is_ok_and(|m| !m.is_empty()) {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    base
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_tiers() {
        let c = InferenceConfig::default();
        assert_eq!(pick_model(&c, 32 * GIB).map(|(m, f)| (m.id, f)), Some(("qwen2.5:7b", 8)));
        assert_eq!(pick_model(&c, 16 * GIB).map(|(m, f)| (m.id, f)), Some(("qwen2.5:7b", 8)));
        assert_eq!(pick_model(&c, 15 * GIB).map(|(m, f)| (m.id, f)), Some(("qwen2.5:1.5b", 4)));
        assert_eq!(pick_model(&c, 8 * GIB).map(|(m, f)| (m.id, f)), Some(("qwen2.5:1.5b", 4)));
        assert_eq!(pick_model(&c, 6 * GIB).map(|(m, f)| (m.id, f)), Some(("qwen2.5:0.5b", 2)));
        assert_eq!(pick_model(&c, 4 * GIB).map(|(m, _)| m.id), Some("qwen2.5:0.5b"));
        assert!(pick_model(&c, 3 * GIB).is_none());
    }

    #[test]
    fn host_gates() {
        let c = InferenceConfig::default();
        assert_eq!(host_gate(&c, 16 * GIB, 8 * GIB, 50 * GIB, false), None);
        // The 7B wants 10 GB of disk, not just the 5 GB floor.
        assert!(host_gate(&c, 16 * GIB, 8 * GIB, 7 * GIB, false).unwrap().contains("10 GB"));
        assert_eq!(host_gate(&c, 12 * GIB, 8 * GIB, 7 * GIB, false), None);
        assert!(host_gate(&c, 2 * GIB, GIB, 50 * GIB, false).unwrap().contains("RAM"));
        assert!(host_gate(&c, 16 * GIB, 8 * GIB, GIB, false).unwrap().contains("disk"));
        // Disk only matters before the install.
        assert_eq!(host_gate(&c, 16 * GIB, 8 * GIB, GIB, true), None);
        assert!(host_gate(&c, 16 * GIB, GIB, 50 * GIB, true).unwrap().starts_with("load gate"));
        let off = InferenceConfig { auto_install: false, ..InferenceConfig::default() };
        if std::env::var("C0MPUTE_INFERENCE_AUTO").is_err() {
            assert!(host_gate(&off, 16 * GIB, 8 * GIB, 50 * GIB, false).unwrap().starts_with("turned off"));
        }
    }

    #[test]
    fn pins_are_well_formed() {
        for m in HOUSE_MODELS {
            assert_eq!(m.sha256.len(), 64);
            assert!(m.url.ends_with(m.file));
            for p in m.parts {
                assert_eq!(p.sha256.len(), 64);
                assert!(p.url.ends_with(p.file));
            }
            assert!(crate::inference::model_tag(m.id).starts_with("infernet:inference:model:qwen2.5:"));
        }
        if let Some((url, sha)) = llama_asset() {
            assert!(url.contains(LLAMA_BUILD));
            assert_eq!(sha.len(), 64);
        }
    }

    #[test]
    fn idle_box_takes_jobs_when_not_active() {
        // The operator's runtime is never load-gated here.
        assert!(!active());
        assert!(can_take_job());
    }
}
