//! `diffusion.txt2img` on CPU: stable-diffusion.cpp + Z-Image-Turbo.
//!
//! Like Kokoro speech (see `speech.rs`), this is a runtime the node installs
//! itself, in the background, and only advertises once it works:
//!
//! - `sd-cli` from a pinned stable-diffusion.cpp release (MIT), SHA-256 checked;
//! - three Apache-2.0 weight files from Hugging Face, pinned to a repo commit
//!   and SHA-256 checked: Z-Image-Turbo Q4_K (the diffusion model), Qwen3-4B
//!   Instruct Q4_K_M (its text encoder) and the FLUX autoencoder.
//!
//! About 6.7 GB on disk and ~4 GB of RAM while a job runs (weights stream
//! from disk with --params-backend disk). A 768x512 image
//! takes 90-120 s on a 16-core Ryzen, so this is for asynchronous work (ad
//! heroes, chat images), not interactive previews.
//!
//! Gates, all re-checked on every advertise tick so a busy box stops bidding
//! instead of swapping:
//! - Linux x86_64 only (the only CPU build of the pinned release we run; it is
//!   built on Ubuntu 26.04 and needs glibc 2.43, which the self-test checks);
//! - at least `min_ram_gb` total RAM and `min_free_ram_gb` available right now;
//! - a per-job CPU budget of at least `min_cpu_threads` (a 16-thread box);
//! - at least `min_free_disk_gb` free before installing;
//! - opt-out: `C0MPUTE_DIFFUSION_AUTO=0` or `[diffusion] auto_install = false`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use c0mpute_net::Libp2pNetwork;
use c0mpute_net::topics::JobStatus;
use c0mpute_store::ChunkStore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::{Semaphore, mpsc};
use tracing::{info, warn};

use crate::runner::{AcceptedJob, WorkerLoad, cleanup_job_dir, publish_receipt, safe_job_dir};

pub const WORKLOAD_TXT2IMG: &str = "diffusion.txt2img";
pub const ROLE_TAG: &str = "c0mpute:role:diffusion";
pub const MODEL_ID: &str = "z-image-turbo";
pub const MODEL_TAG: &str = "c0mpute:diffusion:model:z-image-turbo";

/// Largest PNG a worker returns or a buyer accepts. A 1024x1024 PNG is ~2 MB;
/// the libp2p request-response codec caps a single blob at 10 MiB.
pub const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_PROMPT_CHARS: usize = 2_000;
const JOB_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const INSTALL_RETRY_BACKOFF_SECS: u64 = 6 * 60 * 60;

const SDCPP_RELEASE: &str = "master-1060-0b3b98d";
const SDCPP_ZIP_URL: &str = "https://github.com/leejet/stable-diffusion.cpp/releases/download/master-1060-0b3b98d/sd-master-0b3b98d-bin-Linux-Ubuntu-26.04-x86_64.zip";
const SDCPP_ZIP_SHA256: &str = "eae29803481aba47442272e1dbbff00579c9c4de850433c2cd96cd0df55f3eee";

struct WeightFile {
    name: &'static str,
    url: &'static str,
    sha256: &'static str,
}

const WEIGHTS: [WeightFile; 3] = [
    WeightFile {
        name: "z_image_turbo-Q4_K.gguf",
        url: "https://huggingface.co/leejet/Z-Image-Turbo-GGUF/resolve/c61c0e422dc8b541b7548cf33a4ef8302b0f8085/z_image_turbo-Q4_K.gguf",
        sha256: "14b375ab4f226bc5378f68f37e899ef3c2242b8541e61e2bc1aff40976086fbd",
    },
    WeightFile {
        name: "Qwen3-4B-Instruct-2507-Q4_K_M.gguf",
        url: "https://huggingface.co/unsloth/Qwen3-4B-Instruct-2507-GGUF/resolve/a06e946bb6b655725eafa393f4a9745d460374c9/Qwen3-4B-Instruct-2507-Q4_K_M.gguf",
        sha256: "3605803b982cb64aead44f6c1b2ae36e3acdb41d8e46c8a94c6533bc4c67e597",
    },
    WeightFile {
        name: "ae.safetensors",
        url: "https://huggingface.co/Comfy-Org/Z-Image-Turbo/resolve/6fc90a3b1b653e935a0d175e260736de25b84df5/split_files/vae/ae.safetensors",
        sha256: "afc8e28272cd15db3919bacdb6918ce9c1ed22e96cb12c4d5ed0fba823529e38",
    },
];

/// What the ready marker must say. Bumping a pin changes it, so nodes
/// re-provision instead of running stale files.
fn ready_stamp() -> String {
    format!("sdcpp={SDCPP_RELEASE} model={MODEL_ID}-q4k\n")
}

pub fn diffusion_root() -> PathBuf {
    crate::config::data_dir()
        .unwrap_or_else(|| PathBuf::from("./c0mpute-data"))
        .join("diffusion")
}

fn bin_dir() -> PathBuf {
    diffusion_root().join("bin")
}
fn models_dir() -> PathBuf {
    diffusion_root().join("models")
}
fn sd_cli() -> PathBuf {
    bin_dir().join("sd-cli")
}
fn ready_marker() -> PathBuf {
    diffusion_root().join(".ready")
}
fn retry_marker() -> PathBuf {
    diffusion_root().join(".retry-after")
}

fn platform_supported() -> bool {
    cfg!(all(target_os = "linux", target_arch = "x86_64"))
}

/// True when the pinned runtime and weights are installed.
pub fn installed() -> bool {
    platform_supported()
        && sd_cli().is_file()
        && std::fs::read_to_string(ready_marker()).is_ok_and(|s| s == ready_stamp())
}

#[derive(Clone, Copy, Debug)]
pub struct MemorySnapshot {
    pub total: u64,
    pub available: u64,
}

pub fn memory() -> MemorySnapshot {
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    MemorySnapshot {
        total: system.total_memory(),
        available: system.available_memory(),
    }
}

const GIB: u64 = 1024 * 1024 * 1024;

/// Pure decision: can a box with this memory take a diffusion job right now?
pub fn has_headroom_with(config: &crate::config::DiffusionConfig, mem: MemorySnapshot) -> bool {
    mem.total >= config.min_ram_gb.saturating_mul(GIB)
        && mem.available >= config.min_free_ram_gb.saturating_mul(GIB)
}

/// Pure decision: is this per-job thread budget fast enough to serve images?
pub fn fast_enough_with(config: &crate::config::DiffusionConfig, job_threads: usize) -> bool {
    job_threads >= config.min_cpu_threads
}

fn fast_enough(config: &crate::config::DiffusionConfig) -> bool {
    fast_enough_with(config, crate::resources::cpu_threads_per_job())
}

/// Installed, enabled, and enough free memory to run a job without swapping.
/// Called on every advertise tick, so a busy node stops bidding within a minute.
pub fn can_serve(config: &crate::config::DiffusionConfig) -> bool {
    config.auto_install_enabled()
        && installed()
        && fast_enough(config)
        && has_headroom_with(config, memory())
        && idle_enough_with(load_avg_1m(), logical_cpus(), config.max_load_per_cpu)
        && node_slot().available_permits() > 0
}

/// Busiest a box may be (1-minute load average per logical CPU) and still
/// take a render. A render needs half the box for ~2 minutes; on a box
/// already this busy it takes 10+ and the buyer is better told "no worker".
pub const MAX_LOAD_PER_CPU: f64 = 0.75;

/// Pure decision: is the box idle enough to start an image now?
pub fn idle_enough_with(load_1m: f64, cpus: usize, max_per_cpu: f64) -> bool {
    cpus > 0 && load_1m.is_finite() && load_1m < cpus as f64 * max_per_cpu
}

fn load_avg_1m() -> f64 {
    sysinfo::System::load_average().one
}

fn logical_cpus() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn in_install_backoff() -> bool {
    std::fs::read_to_string(retry_marker())
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .is_some_and(|retry_after| now_secs() < retry_after)
}

fn free_disk_bytes(root: &Path) -> u64 {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let mut best: Option<(usize, u64)> = None;
    let mut largest = 0u64;
    for disk in disks.list() {
        largest = largest.max(disk.available_space());
        let mount = disk.mount_point();
        if root.starts_with(mount) {
            let depth = mount.components().count();
            if best.is_none_or(|(best_depth, _)| depth > best_depth) {
                best = Some((depth, disk.available_space()));
            }
        }
    }
    best.map(|(_, free)| free).unwrap_or(largest)
}

/// Make sure this node can serve `diffusion.txt2img`, installing it if it is
/// missing and the box is big enough. Idempotent and never fatal.
pub async fn ensure(config: &crate::config::DiffusionConfig) -> bool {
    if !platform_supported() {
        tracing::debug!("diffusion: no CPU build for this platform; not installing");
        return false;
    }
    if !config.auto_install_enabled() {
        tracing::debug!("diffusion auto-provisioning is off; not installing");
        return false;
    }
    if !fast_enough(config) {
        info!(
            job_threads = crate::resources::cpu_threads_per_job(),
            required = config.min_cpu_threads,
            "skipping diffusion: CPU budget too small for images in reasonable time"
        );
        return false;
    }
    if installed() {
        return true;
    }
    let mem = memory();
    if mem.total < config.min_ram_gb.saturating_mul(GIB) {
        info!(
            total_gb = mem.total / GIB,
            required_gb = config.min_ram_gb,
            "skipping diffusion install: not enough RAM"
        );
        return false;
    }
    let root = diffusion_root();
    let free = free_disk_bytes(&root);
    if free < config.min_free_disk_gb.saturating_mul(GIB) {
        info!(
            free_gb = free / GIB,
            required_gb = config.min_free_disk_gb,
            "skipping diffusion install: not enough free disk"
        );
        return false;
    }
    if in_install_backoff() {
        tracing::debug!("diffusion install failed recently; waiting out the backoff");
        return false;
    }
    info!(
        "installing stable-diffusion.cpp + Z-Image-Turbo (~6.7 GB) so this node can serve diffusion.txt2img"
    );
    match install().await {
        Ok(()) => {
            let _ = tokio::fs::remove_file(retry_marker()).await;
            info!(root = %root.display(), "diffusion ready");
            true
        }
        Err(error) => {
            let _ = tokio::fs::create_dir_all(&root).await;
            let retry_after = now_secs().saturating_add(INSTALL_RETRY_BACKOFF_SECS);
            let _ = tokio::fs::write(retry_marker(), retry_after.to_string()).await;
            warn!(
                err = %format!("{error:#}"),
                "could not install diffusion; this node will not serve images. \
                 Turn it off with C0MPUTE_DIFFUSION_AUTO=0."
            );
            false
        }
    }
}

/// Download, verify and self-test. Public so `c0mpute diffusion setup` can run it
/// in the foreground.
pub async fn install() -> Result<()> {
    if !platform_supported() {
        bail!("diffusion has a CPU build for Linux x86_64 only");
    }
    let root = diffusion_root();
    tokio::fs::create_dir_all(models_dir()).await?;
    let _ = tokio::fs::remove_file(ready_marker()).await;

    let zip_path = root.join("sdcpp.zip");
    download_verified_streaming(SDCPP_ZIP_URL, &zip_path, SDCPP_ZIP_SHA256).await?;
    let staging = root.join("bin.staging");
    let _ = tokio::fs::remove_dir_all(&staging).await;
    {
        let zip_path = zip_path.clone();
        let staging = staging.clone();
        tokio::task::spawn_blocking(move || extract_zip(&zip_path, &staging))
            .await
            .context("join zip extraction")??;
    }
    let _ = tokio::fs::remove_dir_all(bin_dir()).await;
    tokio::fs::rename(&staging, bin_dir()).await?;
    let _ = tokio::fs::remove_file(&zip_path).await;

    for weight in &WEIGHTS {
        download_verified_streaming(weight.url, &models_dir().join(weight.name), weight.sha256)
            .await
            .with_context(|| format!("download {}", weight.name))?;
    }

    // The binary links glibc and libstdc++ from the host. Running it once is
    // the only honest check that this box can load it.
    let output = Command::new(sd_cli())
        .arg("--help")
        .env("LD_LIBRARY_PATH", bin_dir())
        .output()
        .await
        .context("run sd-cli self-test")?;
    let text = String::from_utf8_lossy(&output.stdout);
    if !text.contains("--diffusion-model") {
        bail!(
            "sd-cli self-test failed (exit {:?}): {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(300)
                .collect::<String>()
        );
    }
    tokio::fs::write(ready_marker(), ready_stamp()).await?;
    Ok(())
}

fn extract_zip(zip_path: &Path, dest: &Path) -> Result<()> {
    let file = std::fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file).context("open sd.cpp zip")?;
    std::fs::create_dir_all(dest)?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        // enclosed_name rejects absolute paths and `..` components.
        let Some(relative) = entry.enclosed_name() else {
            bail!("sd.cpp zip contains an unsafe path");
        };
        let out = dest.join(relative);
        if entry.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut writer = std::fs::File::create(&out)?;
        std::io::copy(&mut entry, &mut writer)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = entry.unix_mode().unwrap_or(0o644) & 0o755;
            std::fs::set_permissions(&out, std::fs::Permissions::from_mode(mode | 0o400))?;
        }
    }
    Ok(())
}

/// Stream a large file to disk while hashing it; never holds it in memory.
/// A file already present with the right hash is kept.
async fn download_verified_streaming(url: &str, path: &Path, expected_sha256: &str) -> Result<()> {
    if path.is_file() && sha256_file(path).await? == expected_sha256 {
        return Ok(());
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

async fn sha256_file(path: &Path) -> Result<String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<String> {
        let mut file = std::fs::File::open(path)?;
        let mut hasher = Sha256::new();
        std::io::copy(&mut file, &mut hasher)?;
        Ok(hex::encode(hasher.finalize()))
    })
    .await?
}

/// The inline job spec a buyer sends with `diffusion.txt2img`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Txt2ImgSpec {
    pub prompt: String,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_height")]
    pub height: u32,
    #[serde(default = "default_steps")]
    pub steps: u32,
    #[serde(default)]
    pub seed: Option<u32>,
}

fn default_model() -> String {
    MODEL_ID.to_string()
}
fn default_width() -> u32 {
    768
}
fn default_height() -> u32 {
    512
}
fn default_steps() -> u32 {
    8
}

impl Txt2ImgSpec {
    /// Shared by the gateway (reject early) and the worker (never trust the buyer).
    pub fn validate(&self) -> std::result::Result<(), String> {
        let chars = self.prompt.trim().chars().count();
        if chars == 0 || self.prompt.chars().count() > MAX_PROMPT_CHARS {
            return Err(format!(
                "prompt must contain 1 to {MAX_PROMPT_CHARS} characters"
            ));
        }
        if self.model != MODEL_ID {
            return Err(format!("model must be {MODEL_ID}"));
        }
        for (name, value) in [("width", self.width), ("height", self.height)] {
            if !(256..=1024).contains(&value) || value % 64 != 0 {
                return Err(format!("{name} must be a multiple of 64 from 256 to 1024"));
            }
        }
        if !(1..=12).contains(&self.steps) {
            return Err("steps must be from 1 to 12".into());
        }
        Ok(())
    }
}

pub fn is_png(bytes: &[u8]) -> bool {
    bytes.len() > 8 && bytes.starts_with(b"\x89PNG\r\n\x1a\n")
}

/// Render one image to `output` with sd-cli.
pub async fn generate(spec: &Txt2ImgSpec, output: &Path) -> Result<()> {
    spec.validate().map_err(anyhow::Error::msg)?;
    if !installed() {
        bail!("diffusion runtime is not installed");
    }
    let models = models_dir();
    let mut command = Command::new(sd_cli());
    let threads = crate::resources::apply_job_limits(&mut command);
    // sd-cli reads the next argv as the prompt, but keep a leading dash from
    // ever looking like a flag.
    let prompt = if spec.prompt.starts_with('-') {
        format!(" {}", spec.prompt)
    } else {
        spec.prompt.clone()
    };
    let seed = spec.seed.unwrap_or_else(rand::random::<u32>);
    command
        .env("LD_LIBRARY_PATH", bin_dir())
        .arg("--diffusion-model")
        .arg(models.join(WEIGHTS[0].name))
        .arg("--llm")
        .arg(models.join(WEIGHTS[1].name))
        .arg("--vae")
        .arg(models.join(WEIGHTS[2].name))
        .args(["--cfg-scale", "1.0"])
        // Weights stay on disk and stream in per module: 4.0 GB peak RSS
        // instead of 7.6 GB, same ~125 s on dev2 (measured 2026-11-09). On a
        // busy host the bigger footprint got the render killed by earlyoom.
        .args(["--params-backend", "disk"])
        .arg("--steps")
        .arg(spec.steps.to_string())
        .arg("-W")
        .arg(spec.width.to_string())
        .arg("-H")
        .arg(spec.height.to_string())
        .arg("-t")
        .arg(threads.max(1).to_string())
        .arg("--seed")
        .arg(seed.to_string())
        .arg("-o")
        .arg(output)
        .arg("-p")
        .arg(prompt)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let child = command.spawn().context("start sd-cli")?;
    let result = tokio::time::timeout(JOB_TIMEOUT, child.wait_with_output())
        .await
        .context("sd-cli timed out")??;
    if !result.status.success() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        let tail: String = stderr
            .lines()
            .rev()
            .take(5)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join(" | ");
        bail!("sd-cli failed (exit {:?}): {tail}", result.status.code());
    }
    Ok(())
}

/// One image at a time per node: a job holds ~9 GB and every core, so a second
/// concurrent job would only swap.
fn node_slot() -> Arc<Semaphore> {
    static SLOT: std::sync::OnceLock<Arc<Semaphore>> = std::sync::OnceLock::new();
    SLOT.get_or_init(|| Arc::new(Semaphore::new(1))).clone()
}

/// `/proc/<pid>/stat` -> (state, parent pid). The command name is in
/// parentheses and may contain spaces, so parse after the last `)`.
fn parse_stat(stat: &str) -> Option<(char, u32)> {
    let rest = stat.get(stat.rfind(')')? + 1..)?.trim_start();
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let ppid = fields.next()?.parse().ok()?;
    Some((state, ppid))
}

/// Kill renders left running by the binary this process replaced.
///
/// An auto-update re-executes the worker in place: same PID, so a running
/// sd-cli stays our child but nothing will ever read its result, and it
/// keeps ~6-9 GB and every core busy, so the node stops bidding (free-RAM
/// gate) or runs the next image at half speed. Called once when the runner
/// starts, before it takes any job, so every sd-cli child is such an orphan.
pub fn kill_orphaned_renders() -> usize {
    if !cfg!(target_os = "linux") {
        return 0;
    }
    let me = std::process::id();
    let ours = sd_cli();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return 0;
    };
    let mut killed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let Some((state, ppid)) = parse_stat(&stat) else {
            continue;
        };
        if ppid != me || state == 'Z' {
            continue;
        }
        if std::fs::read_link(format!("/proc/{pid}/exe")).ok().as_deref() != Some(ours.as_path()) {
            continue;
        }
        if std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status()
            .is_ok_and(|s| s.success())
        {
            warn!(pid, "killed an sd-cli render orphaned by an in-place update");
            killed += 1;
        }
    }
    killed
}

pub fn spawn_diffusion_runner(
    net: Arc<Libp2pNetwork>,
    store: ChunkStore,
    cache_root: PathBuf,
    load: WorkerLoad,
) -> mpsc::Sender<AcceptedJob> {
    kill_orphaned_renders();
    let (tx, mut rx) = mpsc::channel::<AcceptedJob>(4);
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let Ok(_compute_slot) = load.compute_slots().acquire_owned().await else {
                return;
            };
            let Ok(_node_slot) = node_slot().acquire_owned().await else {
                return;
            };
            let started = Instant::now();
            let job_id = job.accept.job_id.clone();
            let result = run_job(&net, &store, &cache_root, &job).await;
            cleanup_job_dir(&cache_root, &job_id).await;
            match result {
                Ok(hash) => {
                    load.record_job(started.elapsed());
                    info!(%job_id, %hash, secs = started.elapsed().as_secs(), "diffusion runner: job complete");
                    publish_receipt(&net, &job.accept, Some(hash), JobStatus::Completed).await;
                }
                Err(error) => {
                    warn!(%job_id, err = %format!("{error:#}"), "diffusion runner: job failed");
                    publish_receipt(&net, &job.accept, None, JobStatus::Failed).await;
                }
            }
        }
    });
    tx
}

async fn run_job(
    net: &Libp2pNetwork,
    store: &ChunkStore,
    cache_root: &Path,
    job: &AcceptedJob,
) -> Result<String> {
    let spec = job
        .accept
        .spec_inline
        .as_ref()
        .context("accept missing spec_inline")?;
    let spec: Txt2ImgSpec = serde_json::from_value(spec.clone()).context("decode txt2img job")?;
    spec.validate().map_err(anyhow::Error::msg)?;
    let job_dir = safe_job_dir(cache_root, &job.accept.job_id)?;
    tokio::fs::create_dir_all(&job_dir).await?;
    let output = job_dir.join("image.png");
    generate(&spec, &output).await?;
    let bytes = tokio::fs::read(&output).await?;
    if !is_png(&bytes) || bytes.len() > MAX_IMAGE_BYTES {
        bail!("sd-cli output is not a PNG under the 8 MiB result limit");
    }
    let hash = store.put(&bytes).await?;
    use c0mpute_net::Network;
    net.announce(&hash).await?;
    Ok(hash.to_hex())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DiffusionConfig;

    fn spec() -> Txt2ImgSpec {
        serde_json::from_value(serde_json::json!({ "prompt": "a red bicycle" })).unwrap()
    }

    #[test]
    fn spec_defaults_are_a_768x512_eight_step_z_image_turbo_job() {
        let spec = spec();
        assert_eq!((spec.width, spec.height, spec.steps), (768, 512, 8));
        assert_eq!(spec.model, MODEL_ID);
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn spec_rejects_bad_sizes_steps_models_and_prompts() {
        let mut s = spec();
        s.width = 700;
        assert!(s.validate().is_err());
        let mut s = spec();
        s.height = 2048;
        assert!(s.validate().is_err());
        let mut s = spec();
        s.steps = 0;
        assert!(s.validate().is_err());
        let mut s = spec();
        s.model = "flux".into();
        assert!(s.validate().is_err());
        let mut s = spec();
        s.prompt = "   ".into();
        assert!(s.validate().is_err());
        let mut s = spec();
        s.prompt = "x".repeat(MAX_PROMPT_CHARS + 1);
        assert!(s.validate().is_err());
    }

    #[test]
    fn headroom_needs_both_total_and_available_memory() {
        let config = DiffusionConfig::default();
        let big_idle = MemorySnapshot {
            total: 32 * GIB,
            available: 20 * GIB,
        };
        let big_busy = MemorySnapshot {
            total: 91 * GIB,
            available: 7 * GIB,
        };
        let small = MemorySnapshot {
            total: 8 * GIB,
            available: 7 * GIB,
        };
        assert!(has_headroom_with(&config, big_idle));
        assert!(
            !has_headroom_with(&config, big_busy),
            "a busy box must not bid"
        );
        assert!(!has_headroom_with(&config, small));
    }

    #[test]
    fn a_small_cpu_budget_does_not_serve_images() {
        let config = DiffusionConfig::default();
        assert!(fast_enough_with(&config, 8));
        assert!(fast_enough_with(&config, 16));
        assert!(
            !fast_enough_with(&config, 3),
            "a 6-core box at the 50% share is too slow"
        );
    }

    #[test]
    fn proc_stat_parses_state_and_parent_even_with_spaces_in_the_name() {
        assert_eq!(parse_stat("3655695 (sd-cli) R 4165541 3655695 0"), Some(('R', 4165541)));
        assert_eq!(parse_stat("12 (a b) c) Z 7 12 0"), Some(('Z', 7)));
        assert_eq!(parse_stat("garbage"), None);
    }

    #[test]
    fn a_busy_box_does_not_take_images() {
        assert!(idle_enough_with(4.0, 16, MAX_LOAD_PER_CPU));
        assert!(!idle_enough_with(26.5, 16, MAX_LOAD_PER_CPU), "seed2 at load 26 on 16 vCPUs");
        assert!(!idle_enough_with(12.0, 16, MAX_LOAD_PER_CPU), "75% is the line");
        assert!(!idle_enough_with(f64::NAN, 16, MAX_LOAD_PER_CPU));
        assert!(!idle_enough_with(0.0, 0, MAX_LOAD_PER_CPU));
        // dev2's low-priority container: load 30 on 32 still renders in 106 s.
        assert!(idle_enough_with(30.0, 32, 2.0));
    }

    #[test]
    fn png_magic_is_checked() {
        assert!(is_png(b"\x89PNG\r\n\x1a\n0000"));
        assert!(!is_png(b"RIFF0000WAVE"));
    }

    #[test]
    fn ready_stamp_names_the_pins() {
        assert!(ready_stamp().contains(SDCPP_RELEASE));
        assert!(ready_stamp().contains(MODEL_ID));
    }
}
