//! The house LLM runtime on a GPU box: a user-space Ollama.
//!
//! [`crate::inference_auto`] provisions a CPU llama.cpp for nodes with no
//! runtime of their own. On a node with a real GPU (see [`crate::gpu`]: any
//! NVIDIA card, or a discrete AMD card; never an integrated APU) that leaves
//! the card idle, so instead the worker installs the official Ollama Linux
//! release (it bundles the CUDA and ROCm runtimes, so no driver toolkit or
//! sudo is needed) under `<data dir>/inference/`, runs `ollama serve`
//! supervised on [`OLLAMA_URL`], and pulls chat models sized to the card:
//!
//! - 8 GiB of VRAM or more: the house GPU model `eriktonon/qwen3.8-uncensored:9b`
//!   (plain-text chats for the 4B house default accept it as an upgrade)
//!   alone (~6.9 GiB resident with its projector). It used to share the card
//!   with `qwen2.5:7b`; both do not fit 12 GiB, so Ollama evicted and reloaded
//!   on every switch and jobs timed out (Vast RTX 3060, 2026-11-18);
//! - 6 GiB or more: `qwen3:4b`;
//! - less: `qwen2.5:1.5b`.
//!
//! Models the house Ollama pulled under older releases and no longer serves
//! ([`RETIRED_MODELS`]) are deleted from its own store; an operator's Ollama is
//! never touched.
//!
//! It runs [`parallel_for_vram`] answers at once (`OLLAMA_NUM_PARALLEL`, and
//! the worker takes that many jobs), with flash attention and a q8_0 KV cache
//! so the slots' context fits next to the weights, and caps a model whose
//! Modelfile asks for more context than the card holds (the uncensored 9B
//! says 65536 = 13.6 GiB) at [`ctx_cap_for_vram`] per answer.
//!
//! The worker then serves `infernet.inference` from it exactly as it would
//! from an operator's own Ollama. Any failure (no Linux x86_64, download,
//! unpack, serve, pull) backs off for 6 h and the CPU path runs instead.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::process::Command;
use tracing::{info, warn};

use crate::gpu::{Gpu, GpuKind};

/// Ollama's own default address; the worker only starts its copy when nothing
/// answers there.
pub const OLLAMA_URL: &str = "http://127.0.0.1:11434";
const OLLAMA_HOST: &str = "127.0.0.1:11434";

/// Ollama release, pinned. Bump the tag and every SHA-256 together.
pub const OLLAMA_VERSION: &str = "v0.40.16";
const RETRY_BACKOFF_SECS: u64 = 6 * 60 * 60;
const GIB: u64 = 1024 * 1024 * 1024;

/// (file, url, sha256) of one release archive.
type Asset = (&'static str, &'static str, &'static str);

const OLLAMA_BASE: Asset = (
    "ollama-linux-amd64.tar.zst",
    "https://github.com/ollama/ollama/releases/download/v0.40.16/ollama-linux-amd64.tar.zst",
    "87bfafb2acbc89b2e6a1be0a9043de6aa75fb450696f86bc426e65fc448f788d",
);
/// AMD cards also need the ROCm libraries, unpacked over the base archive.
const OLLAMA_ROCM: Asset = (
    "ollama-linux-amd64-rocm.tar.zst",
    "https://github.com/ollama/ollama/releases/download/v0.40.16/ollama-linux-amd64-rocm.tar.zst",
    "50ba6b7d9f4ae9b143c5c20496bf82be597dab0a06a50276b637e22150226f9b",
);

/// The archives a box with this GPU needs, or None off Linux x86_64.
pub fn assets_for(kind: GpuKind) -> Option<Vec<Asset>> {
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        return None;
    }
    Some(match kind {
        GpuKind::Nvidia => vec![OLLAMA_BASE],
        GpuKind::Amd => vec![OLLAMA_BASE, OLLAMA_ROCM],
    })
}

/// The chat models a card with `vram_mib` serves, biggest-value first.
pub fn models_for_vram(vram_mib: u64) -> &'static [&'static str] {
    if vram_mib >= 8 * 1024 {
        &[crate::inference::HOUSE_GPU_MODEL]
    } else if vram_mib >= 6 * 1024 {
        &["qwen3:4b"]
    } else {
        &["qwen2.5:1.5b"]
    }
}

/// Models earlier releases pulled into the house Ollama's store, removed once
/// the card's current models are in.
pub const RETIRED_MODELS: &[&str] = &["qwen2.5:7b"];

/// Answers the house Ollama runs at once on a card with `vram_mib`: two on
/// a consumer card (both models' weights plus two contexts fit 10-12 GiB with
/// a q8_0 KV cache), four on 20 GiB or more.
pub fn parallel_for_vram(vram_mib: u64) -> usize {
    if vram_mib >= 20 * 1024 { 4 } else { 2 }
}

/// Context per answer (tokens) a model may load with on this card. A
/// Modelfile asking for more is re-created with this `num_ctx` after each
/// pull. 32768 holds the largest prompt a spec carries (64 KB) plus a full
/// answer; smaller cards get half.
pub fn ctx_cap_for_vram(vram_mib: u64) -> u64 {
    if vram_mib >= 10 * 1024 { 32768 } else { 16384 }
}

static HOUSE_PARALLEL: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// `OLLAMA_NUM_PARALLEL` of the house Ollama this worker runs, if it runs one.
pub fn house_parallel() -> Option<usize> {
    match HOUSE_PARALLEL.load(std::sync::atomic::Ordering::Relaxed) {
        0 => None,
        n => Some(n),
    }
}

/// `num_ctx` in an `/api/show` `parameters` block (`num_ctx  65536`).
pub fn parse_num_ctx(parameters: &str) -> Option<u64> {
    parameters.lines().find_map(|l| {
        let mut it = l.split_whitespace();
        (it.next() == Some("num_ctx")).then(|| it.next()?.parse().ok()).flatten()
    })
}

/// Free disk (GB) to install Ollama (~5 GB unpacked, plus the archive while
/// it unpacks) and these models.
pub fn disk_needed_gb(kind: GpuKind, models: &[&str]) -> u64 {
    let runtime = match kind {
        GpuKind::Nvidia => 8,
        GpuKind::Amd => 14,
    };
    let weights: u64 = models
        .iter()
        .map(|m| match *m {
            "eriktonon/qwen3.8-uncensored:9b" => 7,
            "qwen3:4b" => 3,
            _ => 2,
        })
        .sum();
    runtime + weights + 2
}

fn root() -> PathBuf {
    crate::inference_auto::root()
}
fn ollama_dir() -> PathBuf {
    root().join(format!("ollama-{OLLAMA_VERSION}"))
}
pub fn ollama_bin() -> PathBuf {
    ollama_dir().join("bin").join("ollama")
}
fn models_dir() -> PathBuf {
    root().join("ollama-models")
}
fn log_path() -> PathBuf {
    root().join("ollama.log")
}
fn ready_marker(kind: GpuKind) -> PathBuf {
    root().join(format!(".ready-ollama-{OLLAMA_VERSION}-{}", kind.as_str()))
}
fn retry_marker() -> PathBuf {
    root().join(".retry-after-gpu")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn in_backoff() -> bool {
    std::fs::read_to_string(retry_marker())
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .is_some_and(|t| now_secs() < t)
}

pub fn installed(kind: GpuKind) -> bool {
    ollama_bin().is_file() && ready_marker(kind).is_file()
}

/// Why this GPU box would not run the house Ollama, or None when it would.
pub fn gate(config: &crate::config::InferenceConfig, gpu: Option<Gpu>, free_disk: u64, is_installed: bool) -> Option<String> {
    if !config.auto_install_enabled() {
        return Some("turned off (C0MPUTE_INFERENCE_AUTO=0, [inference] auto_install = false, or house models off from the fleet)".into());
    }
    let Some(gpu) = gpu else {
        return Some("no NVIDIA or discrete AMD GPU".into());
    };
    if assets_for(gpu.kind).is_none() {
        return Some("no pinned Ollama build for this platform".into());
    }
    let need = disk_needed_gb(gpu.kind, models_for_vram(gpu.vram_mib));
    if !is_installed && free_disk < need.saturating_mul(GIB) {
        return Some(format!("{:.1} GB free disk, Ollama + models need {need} GB", free_disk as f64 / GIB as f64));
    }
    None
}

/// Decompress a .tar.zst with a pure-Rust zstd decoder (boxes often lack the
/// `zstd` binary), then unpack it with `tar`.
async fn unpack_zst(archive: &Path, dest: &Path) -> Result<()> {
    let tar_path = archive.with_extension("");
    let (a, t) = (archive.to_path_buf(), tar_path.clone());
    tokio::task::spawn_blocking(move || -> Result<()> {
        let input = std::io::BufReader::new(std::fs::File::open(&a)?);
        let mut decoder = ruzstd::decoding::StreamingDecoder::new(input).map_err(|e| anyhow::anyhow!("zstd: {e}"))?;
        let mut out = std::io::BufWriter::new(std::fs::File::create(&t)?);
        std::io::copy(&mut decoder, &mut out)?;
        Ok(())
    })
    .await??;
    let mut tar = Command::new("tar");
    tar.arg("-xf").arg(&tar_path).arg("-C").arg(dest);
    let result = crate::speech::run_checked(tar, "unpack Ollama").await;
    let _ = tokio::fs::remove_file(&tar_path).await;
    result
}

/// Download, verify and unpack Ollama for this GPU. Idempotent.
pub async fn install(kind: GpuKind) -> Result<()> {
    let Some(assets) = assets_for(kind) else {
        bail!("no pinned Ollama build for this platform");
    };
    let root = root();
    tokio::fs::create_dir_all(&root).await?;
    let _ = tokio::fs::remove_file(ready_marker(kind)).await;
    let dir = ollama_dir();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    tokio::fs::create_dir_all(&dir).await?;
    for (file, url, sha) in assets {
        let archive = root.join(file);
        crate::diffusion::download_verified_streaming(url, &archive, sha)
            .await
            .with_context(|| format!("download Ollama {OLLAMA_VERSION} ({file})"))?;
        unpack_zst(&archive, &dir).await.with_context(|| format!("unpack {file}"))?;
        let _ = tokio::fs::remove_file(&archive).await;
    }
    if !ollama_bin().is_file() {
        bail!("the Ollama archive has no bin/ollama");
    }
    // Older pinned versions go once this one is in.
    if let Ok(mut entries) = tokio::fs::read_dir(&root).await {
        while let Ok(Some(e)) = entries.next_entry().await {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("ollama-v") && e.path() != dir {
                let _ = tokio::fs::remove_dir_all(e.path()).await;
            }
        }
    }
    tokio::fs::write(ready_marker(kind), format!("ollama={OLLAMA_VERSION}\n")).await?;
    Ok(())
}

fn serve_command(parallel: usize) -> Command {
    let mut cmd = Command::new(ollama_bin());
    cmd.arg("serve")
        .env("OLLAMA_HOST", OLLAMA_HOST)
        .env("OLLAMA_MODELS", models_dir())
        // Ollama keeps its key pair under $HOME/.ollama; the worker's HOME may
        // not be writable (containers run it as a user with root's HOME).
        .env("HOME", root())
        // Never unload: a reload costs a job its deadline.
        .env("OLLAMA_KEEP_ALIVE", "-1")
        // As many answers at once as the worker takes (it reads this back).
        .env("OLLAMA_NUM_PARALLEL", parallel.to_string())
        // Smaller KV cache per slot, so parallel contexts fit beside the weights.
        .env("OLLAMA_FLASH_ATTENTION", "1")
        .env("OLLAMA_KV_CACHE_TYPE", "q8_0")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    if let Ok(log) = std::fs::File::create(log_path()) {
        if let Ok(err) = log.try_clone() {
            cmd.stderr(err);
        }
        cmd.stdout(log);
    }
    crate::inference_auto::die_with_parent(&mut cmd);
    cmd
}

/// Run `ollama serve` forever, restarting it with the same backoff as the CPU runtime.
async fn supervise(parallel: usize) {
    let mut backoff = Duration::from_secs(5);
    loop {
        if crate::inference_auto::kill_strays(&ollama_bin()) > 0 {
            info!("inference: stopped an Ollama left by the previous worker binary");
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let started = Instant::now();
        match serve_command(parallel).spawn() {
            Ok(mut child) => {
                info!(pid = ?child.id(), url = OLLAMA_URL, "inference: house Ollama (GPU) started");
                let status = child.wait().await;
                warn!(?status, log = %log_path().display(), "inference: house Ollama exited; restarting");
            }
            Err(e) => warn!(err = %e, "inference: could not start Ollama"),
        }
        backoff = if started.elapsed() > Duration::from_secs(10 * 60) {
            Duration::from_secs(5)
        } else {
            (backoff * 2).min(Duration::from_secs(300))
        };
        tokio::time::sleep(backoff).await;
    }
}

async fn wait_up(timeout: Duration) -> bool {
    let client = reqwest::Client::new();
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if client.get(format!("{OLLAMA_URL}/api/version")).timeout(Duration::from_secs(3)).send().await.is_ok_and(|r| r.status().is_success()) {
            return true;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    false
}

async fn pull(model: &str) -> Result<()> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(2 * 60 * 60)).build()?;
    let resp = client
        .post(format!("{OLLAMA_URL}/api/pull"))
        .json(&serde_json::json!({ "model": model, "stream": false }))
        .send()
        .await?
        .error_for_status()?;
    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    if body.get("status").and_then(|s| s.as_str()) != Some("success") {
        bail!("ollama pull {model}: {body}");
    }
    Ok(())
}

/// Delete `model` from the house Ollama's store; true when it was there.
async fn remove(model: &str) -> bool {
    let Ok(client) = reqwest::Client::builder().timeout(Duration::from_secs(60)).build() else { return false };
    client
        .delete(format!("{OLLAMA_URL}/api/delete"))
        .json(&serde_json::json!({ "model": model }))
        .send()
        .await
        .is_ok_and(|r| r.status().is_success())
}

/// Re-create `model` under its own name with `num_ctx` = `cap` when its
/// Modelfile asks for more (a pull restores the upstream value, so this runs
/// after every pull). Ollama reserves `num_ctx` x `OLLAMA_NUM_PARALLEL` of KV
/// cache when it loads a model; 65536 does not fit a 12 GiB card.
async fn cap_context(model: &str, cap: u64) -> Result<()> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(120)).build()?;
    let show: serde_json::Value = client
        .post(format!("{OLLAMA_URL}/api/show"))
        .json(&serde_json::json!({ "model": model }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let Some(ctx) = show.get("parameters").and_then(|p| p.as_str()).and_then(parse_num_ctx) else {
        return Ok(());
    };
    if ctx <= cap {
        return Ok(());
    }
    client
        .post(format!("{OLLAMA_URL}/api/create"))
        .json(&serde_json::json!({ "model": model, "from": model, "parameters": { "num_ctx": cap }, "stream": false }))
        .send()
        .await?
        .error_for_status()?;
    info!(model, from = ctx, to = cap, "inference: capped the model's context to fit the GPU");
    Ok(())
}

/// Start the house Ollama on this GPU, installing it first, and make sure its
/// models are pulled. Returns its URL, or None (the CPU path then runs).
pub async fn start(config: &crate::config::InferenceConfig) -> Option<String> {
    let gpu = crate::gpu::detect()?;
    let is_installed = installed(gpu.kind);
    let disk = crate::speech::free_bytes_for(&root());
    if let Some(reason) = gate(config, Some(gpu), disk, is_installed) {
        info!(%reason, "inference: not running the house Ollama on the GPU");
        return None;
    }
    if in_backoff() {
        tracing::debug!("inference: GPU Ollama setup failed recently; waiting out the backoff");
        return None;
    }
    let models = models_for_vram(gpu.vram_mib);
    let parallel = parallel_for_vram(gpu.vram_mib);
    HOUSE_PARALLEL.store(parallel, std::sync::atomic::Ordering::Relaxed);
    match setup(gpu, is_installed, models, parallel).await {
        Ok(()) => {
            let _ = tokio::fs::remove_file(retry_marker()).await;
            info!(gpu = gpu.kind.as_str(), vram_mib = gpu.vram_mib, ?models, parallel, "inference: serving chat from the house Ollama on the GPU");
            Some(OLLAMA_URL.to_string())
        }
        Err(e) => {
            let _ = tokio::fs::create_dir_all(root()).await;
            let _ = tokio::fs::write(retry_marker(), now_secs().saturating_add(RETRY_BACKOFF_SECS).to_string()).await;
            HOUSE_PARALLEL.store(0, std::sync::atomic::Ordering::Relaxed);
            warn!(err = %format!("{e:#}"), "could not set up Ollama on the GPU; falling back to the CPU runtime");
            None
        }
    }
}

async fn setup(gpu: Gpu, is_installed: bool, models: &[&str], parallel: usize) -> Result<()> {
    if !is_installed {
        info!(gpu = gpu.kind.as_str(), version = OLLAMA_VERSION, "installing Ollama so this GPU node can serve infernet.inference");
        install(gpu.kind).await?;
    }
    tokio::fs::create_dir_all(models_dir()).await?;
    static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if STARTED.set(()).is_ok() {
        tokio::spawn(supervise(parallel));
    }
    if !wait_up(Duration::from_secs(60)).await {
        bail!("Ollama did not answer on {OLLAMA_URL} (see {})", log_path().display());
    }
    let cap = ctx_cap_for_vram(gpu.vram_mib);
    for m in models {
        pull(m).await.with_context(|| format!("pull {m}"))?;
        // Not fatal: the model still serves, just with a bigger reservation.
        if let Err(e) = cap_context(m, cap).await {
            warn!(model = m, err = %format!("{e:#}"), "inference: could not cap the model's context");
        }
    }
    for m in RETIRED_MODELS.iter().filter(|r| !models.contains(r)) {
        if remove(m).await {
            info!(model = m, "inference: removed a model the house Ollama no longer serves");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::InferenceConfig;

    #[test]
    fn models_by_vram() {
        // One model per card: two that do not fit together thrash.
        assert_eq!(models_for_vram(24 * 1024), ["eriktonon/qwen3.8-uncensored:9b"]);
        assert_eq!(models_for_vram(12 * 1024), ["eriktonon/qwen3.8-uncensored:9b"]);
        assert_eq!(models_for_vram(8 * 1024), ["eriktonon/qwen3.8-uncensored:9b"]);
        assert_eq!(models_for_vram(6 * 1024), ["qwen3:4b"]);
        assert_eq!(models_for_vram(4 * 1024), ["qwen2.5:1.5b"]);
        for v in [4, 6, 8, 12, 24] {
            assert!(!models_for_vram(v * 1024).contains(&"qwen2.5:7b"));
        }
    }

    #[test]
    fn parallel_and_context_by_vram() {
        assert_eq!(parallel_for_vram(12 * 1024), 2);
        assert_eq!(parallel_for_vram(8 * 1024), 2);
        assert_eq!(parallel_for_vram(24 * 1024), 4);
        assert_eq!(ctx_cap_for_vram(12 * 1024), 32768);
        assert_eq!(ctx_cap_for_vram(6 * 1024), 16384);
        // /api/show of the uncensored 9B.
        assert_eq!(parse_num_ctx("num_ctx                        65536\nstop                           \"<|im_end|>\""), Some(65536));
        assert_eq!(parse_num_ctx("temperature 0.7\nnum_ctx 4096"), Some(4096));
        assert_eq!(parse_num_ctx("temperature 0.7"), None);
    }

    #[test]
    fn gates() {
        let c = InferenceConfig::default();
        let nv = Gpu { kind: GpuKind::Nvidia, vram_mib: 12 * 1024 };
        assert!(gate(&c, None, 100 * GIB, false).unwrap().contains("GPU"));
        if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            assert_eq!(gate(&c, Some(nv), 100 * GIB, false), None);
            assert!(gate(&c, Some(nv), 5 * GIB, false).unwrap().contains("disk"));
            // Disk only matters before the install.
            assert_eq!(gate(&c, Some(nv), 5 * GIB, true), None);
        }
        let off = InferenceConfig { auto_install: false, ..InferenceConfig::default() };
        if std::env::var("C0MPUTE_INFERENCE_AUTO").is_err() {
            assert!(gate(&off, Some(nv), 100 * GIB, false).unwrap().starts_with("turned off"));
        }
    }

    #[test]
    fn amd_needs_rocm_libs() {
        if let Some(a) = assets_for(GpuKind::Amd) {
            assert_eq!(a.len(), 2);
            assert_eq!(assets_for(GpuKind::Nvidia).unwrap().len(), 1);
        }
    }

    #[test]
    fn pins_are_well_formed() {
        for (file, url, sha) in [OLLAMA_BASE, OLLAMA_ROCM] {
            assert!(url.ends_with(file));
            assert!(url.contains(OLLAMA_VERSION));
            assert_eq!(sha.len(), 64);
        }
        assert!(disk_needed_gb(GpuKind::Nvidia, models_for_vram(12 * 1024)) >= 15);
    }
}
