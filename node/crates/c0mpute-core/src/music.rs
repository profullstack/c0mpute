//! `music.txt2music`: songs from a text prompt (and optional lyrics) with
//! ACE-Step 1.5 (MIT code, MIT weights, commercial use of the output allowed).
//!
//! Like CPU diffusion (see `diffusion.rs`), this is a runtime the node installs
//! itself in the background and only advertises once it works:
//!
//! - a Python 3.12 environment built by `uv` (the box's own, or a pinned,
//!   SHA-256-checked uv release), so no system Python or git is needed;
//! - CPU PyTorch plus ACE-Step from a pinned GitHub commit tarball;
//! - the turbo DiT, Qwen3 text encoder and VAE from Hugging Face, pinned to a
//!   repo commit (~6 GB; the 5 Hz language model is skipped).
//!
//! Jobs run DiT-only in bfloat16 on CPU (~7 GB of RAM instead of ~14 GB in
//! ACE-Step's default float32), or on a GPU when torch sees one. Output is
//! MP3 when the box has ffmpeg, else FLAC.
//!
//! Gates, all re-checked on every advertise tick so a busy box stops bidding:
//! Linux x86_64; `min_ram_gb` total and `min_free_ram_gb` available; a per-job
//! CPU budget of `min_cpu_threads`; `min_free_disk_gb` before installing; a
//! load ceiling; one song at a time. Opt out with `C0MPUTE_MUSIC_AUTO=0` or
//! `[music] auto_install = false`.

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

pub const WORKLOAD_TXT2MUSIC: &str = "music.txt2music";
pub const ROLE_TAG: &str = "c0mpute:role:music";
pub const MODEL_ID: &str = "ace-step-1.5";
pub const MODEL_TAG: &str = "c0mpute:music:model:ace-step-1.5";

/// Largest song a worker returns or a buyer accepts. A 2-minute 128 kb/s MP3
/// is ~2 MB; the FLAC fallback for a 3-minute song is up to ~30 MB.
pub const MAX_RESULT_BYTES: usize = 48 * 1024 * 1024;
/// What a buyer may send. Longer than the model reads on purpose: callers
/// paste whole briefs and songs, and `clip_for_model` trims them.
pub const MAX_PROMPT_CHARS: usize = 100_000;
pub const MAX_LYRICS_CHARS: usize = 100_000;
/// What reaches the worker: ACE-Step's tokenizer keeps 256 caption tokens
/// (~1000 chars) and 2048 lyric tokens, and the spec rides in a gossiped
/// offer capped at 64 KiB, so the gateway clips to these before auctioning.
pub const MODEL_PROMPT_CHARS: usize = 1_000;
pub const MODEL_LYRICS_CHARS: usize = 4_000;
pub const MIN_SECONDS: u32 = 10;
pub const MAX_SECONDS: u32 = 180;
pub const DEFAULT_SECONDS: u32 = 30;
/// Worker-side limit for one song, model load included. A 30 s song takes
/// ~130 s on 16 CPU threads and time grows with length, so a 3-minute song
/// on a busy box needs well over 10 minutes.
pub const JOB_TIMEOUT: Duration = Duration::from_secs(25 * 60);
const INSTALL_RETRY_BACKOFF_SECS: u64 = 6 * 60 * 60;

const WORKER: &str = include_str!("music_worker.py");

/// ACE-Step 1.5 source, pinned. Installed from GitHub's commit tarball so the
/// box needs no git.
const ACESTEP_COMMIT: &str = "ca1e85fe9430179831e6bc6be790c332190a3866";
/// Hugging Face `ACE-Step/Ace-Step1.5` revision for the weights.
const WEIGHTS_REPO: &str = "ACE-Step/Ace-Step1.5";
const WEIGHTS_REVISION: &str = "19671f406d603126926c1b7e2adc169acbcade22";
const TORCH_PIN: [&str; 3] = ["torch==2.10.0", "torchaudio==2.10.0", "torchvision==0.25.0"];
const TORCH_CPU_INDEX: &str = "https://download.pytorch.org/whl/cpu";
/// CUDA 12.8 wheels of the same torch pin, for nodes with an NVIDIA GPU.
/// On a GPU ACE-Step renders a 2-3 minute song in well under a minute, so
/// one card carries more songs than every CPU node together.
const TORCH_CUDA_INDEX: &str = "https://download.pytorch.org/whl/cu128";

/// "cu128" on a box with an NVIDIA GPU, else "cpu". Probed once.
pub fn torch_variant() -> &'static str {
    static VARIANT: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    VARIANT.get_or_init(|| if crate::capabilities::nvidia_present() { "cu128" } else { "cpu" })
}

fn on_gpu() -> bool {
    torch_variant() != "cpu"
}
/// What ACE-Step imports at inference time. Its own dependency list adds
/// gradio, the training stack and CUDA-only wheels, none of which a worker
/// runs, so it is installed with --no-deps on top of these.
const RUNTIME_DEPS: &[&str] = &[
    "transformers>=4.51.0,<4.58.0",
    "diffusers>=0.37.0",
    "scipy>=1.10.1",
    "soundfile>=0.13.1",
    "loguru>=0.7.3",
    "einops>=0.8.1",
    "accelerate>=1.12.0",
    "numba>=0.63.1",
    "vector-quantize-pytorch>=1.27.15",
    "toml",
    "matplotlib",
    "diskcache",
    "huggingface_hub[hf_xet]",
];
const PYTHON_VERSION: &str = "3.12";

/// uv, for boxes that do not have it. Pinned and SHA-256 checked.
const UV_VERSION: &str = "0.12.13";
const UV_TARBALL_URL: &str =
    "https://github.com/astral-sh/uv/releases/download/0.12.13/uv-x86_64-unknown-linux-gnu.tar.gz";
const UV_TARBALL_SHA256: &str = "745765a3b6e360ad76743599ae5c42e9278c7edf8bbff9fc76d05bf2623a04dd";

/// What the ready marker must say. Bumping a pin changes it, so nodes
/// re-provision instead of running stale files.
fn ready_stamp() -> String {
    format!(
        // CPU nodes keep the exact stamp they were installed with ("-cpu");
        // a GPU node re-provisions once to get CUDA wheels.
        "acestep={ACESTEP_COMMIT} weights={WEIGHTS_REVISION} torch={}-{} worker={}\n",
        TORCH_PIN[0].trim_start_matches("torch=="),
        torch_variant(),
        &hex::encode(Sha256::digest(WORKER.as_bytes()))[..12],
    )
}

pub fn music_root() -> PathBuf {
    crate::config::data_dir()
        .unwrap_or_else(|| PathBuf::from("./c0mpute-data"))
        .join("music")
}

fn venv_dir() -> PathBuf {
    music_root().join("venv")
}
fn python() -> PathBuf {
    crate::speech::venv_python(&venv_dir())
}
fn worker_script() -> PathBuf {
    music_root().join("music_worker.py")
}
fn checkpoints_dir() -> PathBuf {
    music_root().join("checkpoints")
}
fn ready_marker() -> PathBuf {
    music_root().join(".ready")
}
fn retry_marker() -> PathBuf {
    music_root().join(".retry-after")
}

fn platform_supported() -> bool {
    cfg!(all(target_os = "linux", target_arch = "x86_64"))
}

/// True when the pinned runtime and weights are installed.
pub fn installed() -> bool {
    platform_supported()
        && python().is_file()
        && worker_script().is_file()
        && std::fs::read_to_string(ready_marker()).is_ok_and(|s| s == ready_stamp())
}

const GIB: u64 = 1024 * 1024 * 1024;

/// Pure decision: can a box with this memory take a song right now?
pub fn has_headroom_with(
    config: &crate::config::MusicConfig,
    mem: crate::diffusion::MemorySnapshot,
) -> bool {
    mem.total >= config.min_ram_gb.saturating_mul(GIB)
        && mem.available >= config.min_free_ram_gb.saturating_mul(GIB)
}

/// Pure decision: is this per-job thread budget fast enough for songs?
pub fn fast_enough_with(config: &crate::config::MusicConfig, job_threads: usize) -> bool {
    job_threads >= config.min_cpu_threads
}

fn fast_enough(config: &crate::config::MusicConfig) -> bool {
    // On a GPU the card does the work; a few CPU threads are enough.
    on_gpu() || fast_enough_with(config, crate::resources::cpu_threads_per_job())
}

/// Installed, enabled, idle enough and with the free memory for a song.
/// Called on every advertise tick and every offer.
pub fn can_serve(config: &crate::config::MusicConfig) -> bool {
    config.auto_install_enabled()
        && installed()
        && fast_enough(config)
        && has_headroom_with(config, crate::diffusion::memory())
        && (on_gpu()
            || crate::diffusion::idle_enough_with(
            sysinfo::System::load_average().one,
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
            config.max_load_per_cpu,
        ))
        && node_slot().available_permits() > 0
        // An image render holds the same CPU; never run both at once.
        && !crate::diffusion::busy()
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

/// Make sure this node can serve `music.txt2music`, installing it if it is
/// missing and the box is big enough. Idempotent and never fatal.
pub async fn ensure(config: &crate::config::MusicConfig) -> bool {
    if !platform_supported() {
        tracing::debug!("music: Linux x86_64 only; not installing");
        return false;
    }
    if !config.auto_install_enabled() {
        tracing::debug!("music auto-provisioning is off; not installing");
        return false;
    }
    if !fast_enough(config) {
        info!(
            job_threads = crate::resources::cpu_threads_per_job(),
            required = config.min_cpu_threads,
            "skipping music: CPU budget too small for songs in reasonable time"
        );
        return false;
    }
    if installed() {
        return true;
    }
    let mem = crate::diffusion::memory();
    if mem.total < config.min_ram_gb.saturating_mul(GIB) {
        info!(
            total_gb = mem.total / GIB,
            required_gb = config.min_ram_gb,
            "skipping music install: not enough RAM"
        );
        return false;
    }
    let root = music_root();
    let free = crate::speech::free_bytes_for(&root);
    if free < config.min_free_disk_gb.saturating_mul(GIB) {
        info!(
            free_gb = free / GIB,
            required_gb = config.min_free_disk_gb,
            "skipping music install: not enough free disk"
        );
        return false;
    }
    if in_install_backoff() {
        tracing::debug!("music install failed recently; waiting out the backoff");
        return false;
    }
    info!("installing ACE-Step 1.5 (~8 GB) so this node can serve music.txt2music");
    match install().await {
        Ok(()) => {
            let _ = tokio::fs::remove_file(retry_marker()).await;
            info!(root = %root.display(), "music ready");
            true
        }
        Err(error) => {
            let _ = tokio::fs::create_dir_all(&root).await;
            let retry_after = now_secs().saturating_add(INSTALL_RETRY_BACKOFF_SECS);
            let _ = tokio::fs::write(retry_marker(), retry_after.to_string()).await;
            warn!(
                err = %format!("{error:#}"),
                "could not install music; this node will not serve songs. \
                 Turn it off with C0MPUTE_MUSIC_AUTO=0."
            );
            false
        }
    }
}

/// The box's uv, or a pinned one downloaded into the music root.
async fn uv_binary() -> Result<PathBuf> {
    if let Some(uv) = crate::speech::find_uv() {
        return Ok(uv);
    }
    let dir = music_root().join("uv");
    let uv = dir.join("uv");
    if uv.is_file() {
        return Ok(uv);
    }
    tokio::fs::create_dir_all(&dir).await?;
    let tarball = dir.join("uv.tar.gz");
    download_verified(UV_TARBALL_URL, &tarball, UV_TARBALL_SHA256)
        .await
        .with_context(|| format!("download uv {UV_VERSION}"))?;
    let mut tar = Command::new("tar");
    tar.arg("-xzf")
        .arg(&tarball)
        .arg("-C")
        .arg(&dir)
        .arg("--strip-components=1");
    crate::speech::run_checked(tar, "unpack uv").await?;
    let _ = tokio::fs::remove_file(&tarball).await;
    if !uv.is_file() {
        bail!("uv tarball did not contain a uv binary");
    }
    Ok(uv)
}

/// Build the environment, fetch the weights, self-test. Public so a
/// foreground `c0mpute music setup` can run it.
pub async fn install() -> Result<()> {
    if !platform_supported() {
        bail!("music runs on Linux x86_64 only");
    }
    let root = music_root();
    tokio::fs::create_dir_all(&root).await?;
    let _ = tokio::fs::remove_file(ready_marker()).await;
    let uv = uv_binary().await?;
    // Keep uv's managed CPython and cache under the music root, so removing
    // the directory removes everything this installed.
    let uv_env = |command: &mut Command| {
        command
            .env("UV_PYTHON_INSTALL_DIR", root.join("python"))
            .env("UV_CACHE_DIR", root.join("uv-cache"))
            .env("UV_NO_PROGRESS", "1");
    };

    if !python().is_file() {
        let mut venv = Command::new(&uv);
        uv_env(&mut venv);
        venv.args(["venv", "--python", PYTHON_VERSION]).arg(venv_dir());
        crate::speech::run_checked(venv, "create the music Python environment with uv").await?;
    }
    let py = python();
    let pip = |args: &[&str]| {
        let mut command = Command::new(&uv);
        uv_env(&mut command);
        command.args(["pip", "install", "--python"]).arg(&py).args(args);
        command
    };
    let mut torch: Vec<&str> = TORCH_PIN.to_vec();
    if on_gpu() {
        // --reinstall: a box that had the CPU build swaps it for CUDA.
        torch.extend(["--reinstall", "--index-url", TORCH_CUDA_INDEX]);
        crate::speech::run_checked(pip(&torch), "install CUDA PyTorch").await?;
    } else {
        torch.extend(["--index-url", TORCH_CPU_INDEX]);
        crate::speech::run_checked(pip(&torch), "install CPU PyTorch").await?;
    }
    crate::speech::run_checked(pip(RUNTIME_DEPS), "install ACE-Step's runtime dependencies").await?;
    let source = format!(
        "ace-step @ https://github.com/ace-step/ACE-Step-1.5/archive/{ACESTEP_COMMIT}.tar.gz"
    );
    crate::speech::run_checked(pip(&["--no-deps", &source]), "install ACE-Step 1.5").await?;

    // Weights, pinned to a repo commit; hf verifies each file's hash. The
    // 5 Hz LM (3.8 GB) is skipped: jobs run DiT-only.
    let mut download = Command::new(venv_dir().join("bin/hf"));
    download
        .args(["download", WEIGHTS_REPO, "--revision", WEIGHTS_REVISION, "--local-dir"])
        .arg(checkpoints_dir())
        .args(["--exclude", "acestep-5Hz-lm-*/*"])
        .env("HF_HUB_DISABLE_TELEMETRY", "1")
        .env("HF_HOME", root.join("hf-home"));
    crate::speech::run_checked(download, "download the ACE-Step 1.5 weights").await?;
    let _ = tokio::fs::remove_dir_all(root.join("uv-cache")).await;
    let _ = tokio::fs::remove_dir_all(checkpoints_dir().join(".cache")).await;

    tokio::fs::write(worker_script(), WORKER).await?;
    let mut selftest = Command::new(&py);
    selftest
        .args(["-c", "import acestep.handler, acestep.inference, torch; print(torch.__version__)"])
        .env("HF_HUB_OFFLINE", "1");
    crate::speech::run_checked(selftest, "import ACE-Step").await?;
    tokio::fs::write(ready_marker(), ready_stamp()).await?;
    Ok(())
}

async fn download_verified(url: &str, path: &Path, expected_sha256: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(10 * 60))
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

/// The inline job spec a buyer sends with `music.txt2music`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Txt2MusicSpec {
    /// Style caption: genre, instruments, mood, tempo, voice.
    pub prompt: String,
    /// Song lyrics with optional [verse]/[chorus] tags; empty = instrumental.
    #[serde(default)]
    pub lyrics: String,
    /// Length in seconds.
    #[serde(default = "default_duration")]
    pub duration: u32,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_steps")]
    pub steps: u32,
    #[serde(default)]
    pub seed: Option<u32>,
}

fn default_duration() -> u32 {
    DEFAULT_SECONDS
}
fn default_model() -> String {
    MODEL_ID.to_string()
}
fn default_steps() -> u32 {
    8
}

/// Clip to `max` characters, preferring to end at a line break or space.
pub fn clip_text(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max).collect();
    let cut = head.rfind(['\n', ' ']).filter(|&i| i * 5 > head.len() * 4);
    cut.map_or(head.as_str(), |i| &head[..i]).trim().to_string()
}

impl Txt2MusicSpec {
    /// The same request trimmed to what the model reads (see `MODEL_*_CHARS`).
    pub fn clip_for_model(&self) -> Self {
        Self {
            prompt: clip_text(&self.prompt, MODEL_PROMPT_CHARS),
            lyrics: clip_text(&self.lyrics, MODEL_LYRICS_CHARS),
            ..self.clone()
        }
    }

    /// Shared by the gateway (reject early) and the worker (never trust the buyer).
    pub fn validate(&self) -> std::result::Result<(), String> {
        let chars = self.prompt.trim().chars().count();
        if chars == 0 || self.prompt.chars().count() > MAX_PROMPT_CHARS {
            return Err(format!("prompt must contain 1 to {MAX_PROMPT_CHARS} characters"));
        }
        if self.lyrics.chars().count() > MAX_LYRICS_CHARS {
            return Err(format!("lyrics must be at most {MAX_LYRICS_CHARS} characters"));
        }
        if !(MIN_SECONDS..=MAX_SECONDS).contains(&self.duration) {
            return Err(format!("duration must be from {MIN_SECONDS} to {MAX_SECONDS} seconds"));
        }
        if self.model != MODEL_ID {
            return Err(format!("model must be {MODEL_ID}"));
        }
        if !(1..=16).contains(&self.steps) {
            return Err("steps must be from 1 to 16".into());
        }
        Ok(())
    }
}

/// MIME type of a finished song, from its magic bytes; None if it is not one.
pub fn audio_content_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() < 16 {
        return None;
    }
    if bytes.starts_with(b"fLaC") {
        return Some("audio/flac");
    }
    if bytes.starts_with(b"ID3") || (bytes[0] == 0xFF && bytes[1] & 0xE0 == 0xE0) {
        return Some("audio/mpeg");
    }
    if bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WAVE" {
        return Some("audio/wav");
    }
    None
}

#[derive(Debug, Deserialize)]
struct WorkerOutput {
    path: PathBuf,
    #[serde(default)]
    secs: f64,
    #[serde(default)]
    load_secs: f64,
    #[serde(default)]
    dtype: String,
    #[serde(default)]
    device: String,
}

/// Make one song in `job_dir`; returns the audio file's path.
pub async fn generate(spec: &Txt2MusicSpec, job_dir: &Path) -> Result<PathBuf> {
    spec.validate().map_err(anyhow::Error::msg)?;
    if !installed() {
        bail!("music runtime is not installed");
    }
    let out_dir = job_dir.join("out");
    tokio::fs::create_dir_all(&out_dir).await?;
    let mut command = Command::new(python());
    let threads = crate::resources::apply_job_limits(&mut command);
    let request = serde_json::json!({
        "prompt": spec.prompt.trim(),
        "lyrics": spec.lyrics.trim(),
        "duration": spec.duration,
        "steps": spec.steps,
        "seed": spec.seed,
        "threads": threads.max(1),
        "device": "auto",
        "dtype": "bfloat16",
    });
    let request_path = job_dir.join("request.json");
    tokio::fs::write(&request_path, serde_json::to_vec(&request)?).await?;
    command
        .arg(worker_script())
        .arg(music_root())
        .arg(&request_path)
        .arg(&out_dir)
        .current_dir(job_dir)
        .env("HF_HOME", music_root().join("hf-home"))
        .env("ACESTEP_TMPDIR", job_dir)
        // ACE-Step aborts its own diffusion loop after 600 s, which an 8-thread
        // CPU budget passes on a 2-minute song (vienna, 2026-11-09). Our job
        // timeout is the real limit; leave a margin for model load and decode.
        .env(
            "ACESTEP_GENERATION_TIMEOUT",
            JOB_TIMEOUT.saturating_sub(Duration::from_secs(3 * 60)).as_secs().to_string(),
        )
        .env("TMPDIR", job_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let child = command.spawn().context("start the ACE-Step worker")?;
    let result = tokio::time::timeout(JOB_TIMEOUT, child.wait_with_output())
        .await
        .context("ACE-Step timed out")??;
    if !result.status.success() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(5).collect();
        bail!(
            "ACE-Step failed (exit {:?}): {}",
            result.status.code(),
            tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
        );
    }
    let stdout = String::from_utf8_lossy(&result.stdout);
    let line = stdout
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'))
        .context("ACE-Step printed no result")?;
    let output: WorkerOutput = serde_json::from_str(line).context("decode the ACE-Step result")?;
    // The worker must have written inside the job's output directory.
    let path = tokio::fs::canonicalize(&output.path).await?;
    if !path.starts_with(tokio::fs::canonicalize(&out_dir).await?) {
        bail!("ACE-Step wrote outside its output directory");
    }
    info!(
        secs = output.secs,
        load_secs = output.load_secs,
        dtype = %output.dtype,
        device = %output.device,
        duration = spec.duration,
        threads,
        "music: song generated"
    );
    Ok(path)
}

/// One song at a time per node: a job holds ~7-9 GB and every core it is given.
/// True while this node is making a song.
pub fn busy() -> bool {
    node_slot().available_permits() == 0
}

fn node_slot() -> Arc<Semaphore> {
    static SLOT: std::sync::OnceLock<Arc<Semaphore>> = std::sync::OnceLock::new();
    SLOT.get_or_init(|| Arc::new(Semaphore::new(1))).clone()
}

/// Kill songs left running by the binary this process replaced (an in-place
/// auto-update keeps the PID, so they stay our children but nothing will read
/// their result). Matches on the worker script in the command line.
pub fn kill_orphaned_songs() -> usize {
    if !cfg!(target_os = "linux") {
        return 0;
    }
    let me = std::process::id().to_string();
    let script = worker_script();
    let script = script.to_string_lossy();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return 0;
    };
    let mut killed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().filter(|s| s.bytes().all(|b| b.is_ascii_digit())) else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let ppid = stat
            .rfind(')')
            .and_then(|i| stat[i + 1..].split_whitespace().nth(1).map(str::to_string));
        if ppid.as_deref() != Some(me.as_str()) {
            continue;
        }
        let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        let cmdline = String::from_utf8_lossy(&cmdline);
        if !cmdline.split('\0').any(|arg| arg == script) {
            continue;
        }
        if std::process::Command::new("kill")
            .args(["-9", pid])
            .status()
            .is_ok_and(|s| s.success())
        {
            warn!(pid, "killed an ACE-Step song orphaned by an in-place update");
            killed += 1;
        }
    }
    killed
}

pub fn spawn_music_runner(
    net: Arc<Libp2pNetwork>,
    store: ChunkStore,
    cache_root: PathBuf,
    load: WorkerLoad,
) -> mpsc::Sender<AcceptedJob> {
    kill_orphaned_songs();
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
                    info!(%job_id, %hash, secs = started.elapsed().as_secs(), "music runner: job complete");
                    publish_receipt(&net, &job.accept, Some(hash), JobStatus::Completed).await;
                }
                Err(error) => {
                    warn!(%job_id, err = %format!("{error:#}"), "music runner: job failed");
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
    let spec: Txt2MusicSpec = serde_json::from_value(spec.clone()).context("decode txt2music job")?;
    spec.validate().map_err(anyhow::Error::msg)?;
    let job_dir = safe_job_dir(cache_root, &job.accept.job_id)?;
    tokio::fs::create_dir_all(&job_dir).await?;
    let path = generate(&spec, &job_dir).await?;
    let bytes = tokio::fs::read(&path).await?;
    if audio_content_type(&bytes).is_none() || bytes.len() > MAX_RESULT_BYTES {
        bail!("ACE-Step output is not MP3/FLAC audio under the 48 MiB result limit");
    }
    // Over 1 MiB goes back as pieces: one chunk fetch caps out near 5 MiB.
    crate::pieces::publish_result_bytes(net, store, &bytes).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MusicConfig;
    use crate::diffusion::MemorySnapshot;

    fn spec() -> Txt2MusicSpec {
        serde_json::from_value(serde_json::json!({ "prompt": "lo-fi hip hop, warm rhodes" })).unwrap()
    }

    #[test]
    fn spec_defaults_are_a_30_second_instrumental_ace_step_job() {
        let spec = spec();
        assert_eq!((spec.duration, spec.steps, spec.lyrics.as_str()), (30, 8, ""));
        assert_eq!(spec.model, MODEL_ID);
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn spec_rejects_bad_lengths_models_prompts_and_lyrics() {
        let mut s = spec();
        s.duration = 5;
        assert!(s.validate().is_err());
        let mut s = spec();
        s.duration = MAX_SECONDS + 1;
        assert!(s.validate().is_err());
        let mut s = spec();
        s.model = "musicgen".into();
        assert!(s.validate().is_err());
        let mut s = spec();
        s.prompt = "  ".into();
        assert!(s.validate().is_err());
        let mut s = spec();
        s.lyrics = "x".repeat(MAX_LYRICS_CHARS + 1);
        assert!(s.validate().is_err());
        let mut s = spec();
        s.steps = 0;
        assert!(s.validate().is_err());
    }

    #[test]
    fn long_text_is_accepted_and_clipped_for_the_model() {
        let long = "word ".repeat(19_999) + "word";
        let mut s = spec();
        s.prompt = long.clone();
        s.lyrics = long;
        assert!(s.validate().is_ok());
        let c = s.clip_for_model();
        assert!(c.prompt.chars().count() <= MODEL_PROMPT_CHARS && c.prompt.chars().count() > 800);
        assert!(c.prompt.ends_with("word"));
        assert!(c.lyrics.chars().count() <= MODEL_LYRICS_CHARS);
        assert!(serde_json::to_vec(&c).unwrap().len() < 32 * 1024, "fits a gossip offer");
        assert!(c.validate().is_ok());
        assert_eq!(clip_text("  short ", 10), "short");
        assert_eq!(clip_text(&"\u{e9}".repeat(5000), 1000).chars().count(), 1000);
    }

    #[test]
    fn audio_types_come_from_magic_bytes() {
        let pad = |head: &[u8]| {
            let mut v = head.to_vec();
            v.resize(32, 0);
            v
        };
        assert_eq!(audio_content_type(&pad(b"ID3\x04")), Some("audio/mpeg"));
        assert_eq!(audio_content_type(&pad(&[0xFF, 0xFB, 0x90])), Some("audio/mpeg"));
        assert_eq!(audio_content_type(&pad(b"fLaC")), Some("audio/flac"));
        assert_eq!(audio_content_type(&pad(b"RIFF\0\0\0\0WAVE")), Some("audio/wav"));
        assert_eq!(audio_content_type(&pad(b"\x89PNG\r\n\x1a\n")), None);
        assert_eq!(audio_content_type(b"ID3"), None, "too short to be a song");
    }

    #[test]
    fn headroom_and_cpu_gates() {
        let config = MusicConfig::default();
        let idle = MemorySnapshot { total: 32 * GIB, available: 20 * GIB };
        let busy = MemorySnapshot { total: 91 * GIB, available: 6 * GIB };
        let small = MemorySnapshot { total: 8 * GIB, available: 7 * GIB };
        assert!(has_headroom_with(&config, idle));
        assert!(!has_headroom_with(&config, busy));
        assert!(!has_headroom_with(&config, small));
        assert!(fast_enough_with(&config, 8));
        assert!(!fast_enough_with(&config, 3));
    }

    #[test]
    fn cpu_nodes_keep_their_ready_stamp() {
        // Changing a CPU node's stamp would re-provision every music node (8 GB).
        if torch_variant() == "cpu" {
            assert!(ready_stamp().starts_with(&format!(
                "acestep={ACESTEP_COMMIT} weights={WEIGHTS_REVISION} torch=2.10.0-cpu worker="
            )));
        }
    }

    #[test]
    fn ready_stamp_names_the_pins() {
        let stamp = ready_stamp();
        assert!(stamp.contains(ACESTEP_COMMIT));
        assert!(stamp.contains(WEIGHTS_REVISION));
        assert!(stamp.contains("worker="));
    }
}
