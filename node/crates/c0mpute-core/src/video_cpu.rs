//! `video.txt2vid` on CPU: stable-diffusion.cpp + Wan 2.1 T2V 1.3B.
//!
//! The GPU path (`video`) runs a diffusers container and needs an NVIDIA
//! card. Most of the network has none, so a CPU-only node can serve the one
//! model small enough to be worth it on CPU, through the same pinned sd.cpp
//! build that serves CPU images (`diffusion`), in `-M vid_gen` mode:
//!
//! - `sd-cli`: shared with `diffusion` (one install, one runtime marker);
//! - three Apache-2.0 weight files, pinned to a repo commit and SHA-256:
//!   Wan2.1-T2V-1.3B Q8_0 (off-grid-ai, converted with sd.cpp itself),
//!   UMT5-XXL encoder Q4_K_M (city96) and the Wan 2.1 VAE (Comfy-Org).
//!   5.43 GB in all.
//! - ffmpeg on PATH: sd-cli writes PNG frames, ffmpeg makes the H.264 MP4
//!   every video buyer gets.
//!
//! Slow and batch: a 33-frame 480x272 clip at 20 steps is 20-30 minutes of 16
//! threads (52-93 s per sampling step on dev2 under load, 2026-11-09). So it
//! is opt-in (see
//! `config::VideoConfig`), capped at that size, and gated per offer on free
//! RAM and load exactly like CPU images. One sd-cli job at a time per node,
//! images and clips together (`diffusion::node_slot`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use c0mpute_net::Libp2pNetwork;
use c0mpute_net::topics::JobStatus;
use c0mpute_store::ChunkStore;
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::config::VideoConfig;
use crate::diffusion::{self, GIB};
use crate::runner::{AcceptedJob, WorkerLoad, cleanup_job_dir, publish_receipt, safe_job_dir};
use crate::video::{self, VideoJobInline, WORKLOAD_TXT2VID};

/// The one model a CPU node serves.
pub const MODEL_ID: &str = "wan2.1-t2v-1.3b";

/// Advertised beside the role and model tags, so a buyer (and /api/nodes)
/// can tell a CPU clip from a GPU one.
pub const CPU_TAG: &str = "c0mpute:video:cpu";

/// Caps: the defaults are the ceiling. Time grows with frames x pixels x
/// steps, and this size already takes 20-30 minutes of 16 threads; anything
/// bigger would blow the 45-minute job timeout on a slower box.
pub const MAX_FRAMES: u32 = 33;
pub const MAX_PIXELS: u32 = 480 * 272;
pub const MAX_STEPS: u32 = 20;

const INSTALL_RETRY_BACKOFF_SECS: u64 = 6 * 60 * 60;

/// Wan's own recommended negative prompt (from the model card and sd.cpp's
/// docs/wan.md). It is in Chinese because that is what the model was
/// trained to read; it lists the usual artifacts (overexposure, static
/// frames, blur, subtitles, extra fingers, malformed limbs, crowds).
const DEFAULT_NEGATIVE: &str = "色调艳丽，过曝，静态，细节模糊不清，字幕，风格，作品，画作，画面，静止，整体发灰，最差质量，低质量，JPEG压缩残留，丑陋的，残缺的，多余的手指，画得不好的手部，画得不好的脸部，畸形的，毁容的，形态畸形的肢体，手指融合，静止不动的画面，杂乱的背景，三条腿，背景人很多，倒着走";

struct WeightFile {
    name: &'static str,
    url: &'static str,
    sha256: &'static str,
    bytes: u64,
}

const WEIGHTS: [WeightFile; 3] = [
    WeightFile {
        name: "Wan2.1-T2V-1.3B-Q8_0.gguf",
        url: "https://huggingface.co/off-grid-ai/Wan2.1-T2V-1.3B-GGUF/resolve/1ba777b2426d5717dd0fddb1100078604a9d4667/Wan2.1-T2V-1.3B-Q8_0.gguf",
        sha256: "de0eb6b6b75abe801b59d7b2004ecc0ca424d4ed0eeb9eed34f0d8aaf321f888",
        bytes: 1_519_435_360,
    },
    WeightFile {
        name: "umt5-xxl-encoder-Q4_K_M.gguf",
        url: "https://huggingface.co/city96/umt5-xxl-encoder-gguf/resolve/b535255bee98c2b0a59ea7c0ae2dcd0c6657b3b7/umt5-xxl-encoder-Q4_K_M.gguf",
        sha256: "17cf97a5bbbc60a646d6105b832b6f657ce904a8a1ad970e4b59df0c67584a40",
        bytes: 3_655_145_312,
    },
    WeightFile {
        name: "wan_2.1_vae.safetensors",
        url: "https://huggingface.co/Comfy-Org/Wan-2.1/resolve/123acf1cc74bccbb9bfff8ac1ee72edc08c2341d/split_files/vae/wan_2.1_vae.safetensors",
        sha256: "2fc39d31359a4b0a64f55876d8ff7fa8d780956ae2cb13463b0223e15148976b",
        bytes: 253_815_318,
    },
];

/// Total download for the weights (the runtime is shared with images).
pub fn download_bytes() -> u64 {
    WEIGHTS.iter().map(|w| w.bytes).sum()
}

fn ready_marker() -> PathBuf {
    diffusion::diffusion_root().join(".video-ready")
}
fn retry_marker() -> PathBuf {
    diffusion::diffusion_root().join(".video-retry-after")
}

/// What the ready marker must say. Changing a pin changes it, so nodes
/// re-provision instead of running stale files.
fn ready_stamp() -> String {
    let hashes: Vec<&str> = WEIGHTS.iter().map(|w| &w.sha256[..12]).collect();
    format!("video={MODEL_ID} {}\n", hashes.join(","))
}

/// Weights and runtime are in place. (Serving also needs ffmpeg and the
/// operator's consent; see [`can_serve`].)
pub fn installed() -> bool {
    diffusion::platform_supported()
        && diffusion::sd_cli().is_file()
        && std::fs::read_to_string(ready_marker()).is_ok_and(|s| s == ready_stamp())
}

fn ffmpeg_present() -> bool {
    crate::supervisor::which_ffmpeg().is_some()
}

/// Pure decision: does a box with this memory have room for a clip now?
pub fn has_headroom_with(config: &VideoConfig, mem: diffusion::MemorySnapshot) -> bool {
    mem.total >= config.min_ram_gb.saturating_mul(GIB)
        && mem.available >= config.min_free_ram_gb.saturating_mul(GIB)
}

/// Pure decision: is this per-job thread budget enough for a clip in time?
pub fn fast_enough_with(config: &VideoConfig, job_threads: usize) -> bool {
    job_threads >= config.min_cpu_threads
}

fn fast_enough(config: &VideoConfig) -> bool {
    fast_enough_with(config, crate::resources::cpu_threads_per_job())
}

/// Allowed, installed, ffmpeg present, and idle with free memory right now.
/// Called on every advertise tick and every offer.
pub fn can_serve(config: &VideoConfig) -> bool {
    config.cpu_setting() != Some(false)
        && installed()
        && ffmpeg_present()
        && fast_enough(config)
        && has_headroom_with(config, diffusion::memory())
        && diffusion::idle_enough_with(
            diffusion::load_avg_1m(),
            diffusion::logical_cpus(),
            config.max_load_per_cpu,
        )
        && diffusion::node_slot().available_permits() > 0
        // Never beside a song either (same rule as images).
        && !crate::music::busy()
}

/// Tags a CPU video node advertises while it can take a clip.
pub fn tags() -> Vec<String> {
    vec![
        video::ROLE_TAG.to_string(),
        video::model_tag(MODEL_ID),
        CPU_TAG.to_string(),
    ]
}

/// Whether a job fits what a CPU node will do: the small model, text to
/// video, within the CPU caps. On top of [`VideoJobInline::validate`].
pub fn fits(job: &VideoJobInline, workload: &str) -> std::result::Result<(), String> {
    job.validate(workload)?;
    if workload != WORKLOAD_TXT2VID {
        return Err("CPU video is text-to-video only".into());
    }
    if job.model != MODEL_ID {
        return Err(format!("CPU video serves {MODEL_ID} only"));
    }
    if job.frames > MAX_FRAMES {
        return Err(format!("CPU video: frames must be at most {MAX_FRAMES}"));
    }
    if job.width.saturating_mul(job.height) > MAX_PIXELS {
        return Err(format!("CPU video: width*height must not exceed {MAX_PIXELS} (480x272)"));
    }
    if job.steps > MAX_STEPS {
        return Err(format!("CPU video: steps must be at most {MAX_STEPS}"));
    }
    Ok(())
}

fn in_install_backoff() -> bool {
    std::fs::read_to_string(retry_marker())
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .is_some_and(|retry_after| diffusion::now_secs() < retry_after)
}

/// Make sure this node can serve CPU video if it should. Installs only with
/// `cpu = true` / `C0MPUTE_VIDEO_CPU=1`; with the default it only reports
/// whether an operator already ran `c0mpute video setup --cpu`. Never fatal.
pub async fn ensure(config: &VideoConfig) -> bool {
    let setting = config.cpu_setting();
    if setting == Some(false) || !diffusion::platform_supported() {
        return false;
    }
    if !fast_enough(config) {
        info!(
            job_threads = crate::resources::cpu_threads_per_job(),
            required = config.min_cpu_threads,
            "skipping CPU video: CPU budget too small for a clip in time"
        );
        return false;
    }
    if installed() {
        return true;
    }
    if setting != Some(true) {
        tracing::debug!("CPU video not installed and not enabled; `c0mpute video setup --cpu` opts in");
        return false;
    }
    let mem = diffusion::memory();
    if mem.total < config.min_ram_gb.saturating_mul(GIB) {
        info!(total_gb = mem.total / GIB, required_gb = config.min_ram_gb, "skipping CPU video install: not enough RAM");
        return false;
    }
    let root = diffusion::diffusion_root();
    let free = diffusion::free_disk_bytes(&root);
    if free < config.min_free_disk_gb.saturating_mul(GIB) {
        info!(free_gb = free / GIB, required_gb = config.min_free_disk_gb, "skipping CPU video install: not enough free disk");
        return false;
    }
    if in_install_backoff() {
        tracing::debug!("CPU video install failed recently; waiting out the backoff");
        return false;
    }
    info!(
        gb = download_bytes() / 1_000_000_000,
        "installing Wan 2.1 1.3B for stable-diffusion.cpp so this node can serve video.txt2vid on CPU"
    );
    match install().await {
        Ok(()) => {
            let _ = tokio::fs::remove_file(retry_marker()).await;
            info!("CPU video ready");
            true
        }
        Err(error) => {
            let _ = tokio::fs::create_dir_all(&root).await;
            let retry_after = diffusion::now_secs().saturating_add(INSTALL_RETRY_BACKOFF_SECS);
            let _ = tokio::fs::write(retry_marker(), retry_after.to_string()).await;
            warn!(err = %format!("{error:#}"), "could not install CPU video; this node will not serve clips");
            false
        }
    }
}

/// Download, verify and self-test. Public for `c0mpute video setup --cpu`.
pub async fn install() -> Result<()> {
    if !diffusion::platform_supported() {
        bail!("CPU video has a build for Linux x86_64 only");
    }
    let _ = tokio::fs::remove_file(ready_marker()).await;
    tokio::fs::create_dir_all(diffusion::models_dir()).await?;
    diffusion::install_runtime().await?;
    for weight in &WEIGHTS {
        diffusion::download_verified_streaming(
            weight.url,
            &diffusion::models_dir().join(weight.name),
            weight.sha256,
        )
        .await
        .with_context(|| format!("download {}", weight.name))?;
    }
    // The pinned build must know the video mode, or every job would fail.
    let output = Command::new(diffusion::sd_cli())
        .arg("--help")
        .env("LD_LIBRARY_PATH", diffusion::bin_dir())
        .output()
        .await
        .context("run sd-cli self-test")?;
    if !String::from_utf8_lossy(&output.stdout).contains("vid_gen") {
        bail!("sd-cli has no vid_gen mode (exit {:?})", output.status.code());
    }
    tokio::fs::write(ready_marker(), ready_stamp()).await?;
    Ok(())
}

/// The sd-cli argv for a clip (everything after the binary). Pure, so the
/// contract with sd.cpp is tested without the binary.
pub fn sd_args(job: &VideoJobInline, models: &Path, frames_pattern: &Path, threads: usize, seed: u32) -> Vec<String> {
    let prompt = job.prompt.trim();
    // sd-cli reads the next argv as the prompt, but keep a leading dash from
    // ever looking like a flag.
    let prompt = if prompt.starts_with('-') { format!(" {prompt}") } else { prompt.to_string() };
    let negative = match job.negative_prompt.trim() {
        "" => DEFAULT_NEGATIVE.to_string(),
        n if n.starts_with('-') => format!(" {n}"),
        n => n.to_string(),
    };
    let path = |name: &str| models.join(name).display().to_string();
    vec![
        "-M".into(),
        "vid_gen".into(),
        "--diffusion-model".into(),
        path(WEIGHTS[0].name),
        "--t5xxl".into(),
        path(WEIGHTS[1].name),
        "--vae".into(),
        path(WEIGHTS[2].name),
        "--cfg-scale".into(),
        job.guidance_scale.unwrap_or(6.0).to_string(),
        "--sampling-method".into(),
        "euler".into(),
        "--flow-shift".into(),
        "3.0".into(),
        "--steps".into(),
        job.steps.to_string(),
        "-W".into(),
        job.width.to_string(),
        "-H".into(),
        job.height.to_string(),
        "--video-frames".into(),
        job.frames.to_string(),
        "--seed".into(),
        seed.to_string(),
        // Flash attention in the DiT only (this build's spelling; the old
        // --diffusion-fa is deprecated) and a tiled VAE decode, the memory
        // peak on CPU.
        "--flash-attn".into(),
        "off,diffusion=on".into(),
        "--vae-tiling".into(),
        // The 3.7 GB text encoder runs once per clip and the VAE once at
        // the end, so they stream from disk; the DiT, used every step, stays
        // in RAM. Measured on dev2: 5.3 GB RSS with everything in RAM (and
        // earlyoom killed it twice on the busy host), 1.8 GB like this at the
        // same speed. All-disk was also 1.8 GB but over twice as slow.
        "--params-backend".into(),
        "te=disk,vae=disk".into(),
        "-t".into(),
        threads.max(1).to_string(),
        "-o".into(),
        frames_pattern.display().to_string(),
        "-p".into(),
        prompt,
        "-n".into(),
        negative,
    ]
}

/// The ffmpeg argv: PNG frames -> H.264 MP4 that plays in a browser.
pub fn ffmpeg_args(frames_pattern: &Path, fps: u32, frames: u32, out: &Path) -> Vec<String> {
    vec![
        "-y".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-framerate".into(),
        fps.to_string(),
        "-i".into(),
        frames_pattern.display().to_string(),
        "-frames:v".into(),
        frames.to_string(),
        "-c:v".into(),
        "libx264".into(),
        "-preset".into(),
        "medium".into(),
        "-crf".into(),
        "18".into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-movflags".into(),
        "+faststart".into(),
        out.display().to_string(),
    ]
}

fn stderr_tail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = text.lines().rev().take(5).collect();
    lines.into_iter().rev().collect::<Vec<_>>().join(" | ")
}

/// Render one clip into `job_dir/out.mp4` and return its path.
pub async fn generate(job: &VideoJobInline, job_dir: &Path) -> Result<PathBuf> {
    fits(job, WORKLOAD_TXT2VID).map_err(anyhow::Error::msg)?;
    if !installed() {
        bail!("CPU video is not installed");
    }
    let ffmpeg = crate::supervisor::which_ffmpeg().context("CPU video needs ffmpeg on PATH")?;
    let frame_dir = job_dir.join("frames");
    tokio::fs::create_dir_all(&frame_dir).await?;
    let pattern = frame_dir.join("frame_%05d.png");
    let seed = job.seed.map(|s| (s & 0xffff_ffff) as u32).unwrap_or_else(rand::random::<u32>);

    let mut command = Command::new(diffusion::sd_cli());
    let threads = crate::resources::apply_job_limits(&mut command);
    command
        .env("LD_LIBRARY_PATH", diffusion::bin_dir())
        .args(sd_args(job, &diffusion::models_dir(), &pattern, threads, seed))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    // Kills sd-cli's whole process group on a timeout or a cancel.
    let result =
        crate::job_process::run_with_timeout(&mut command, video::JOB_TIMEOUT, "sd-cli").await?;
    if !result.status.success() {
        bail!("sd-cli failed (exit {:?}): {}", result.status.code(), stderr_tail(&result.stderr));
    }

    // sd-cli numbers from 0 when the pattern has %d; ffmpeg starts there too.
    let mut written = 0u32;
    let mut entries = tokio::fs::read_dir(&frame_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_name().to_string_lossy().ends_with(".png") {
            written += 1;
        }
    }
    if written < job.frames {
        bail!("sd-cli wrote {written} frames, asked for {}", job.frames);
    }

    let out = job_dir.join("out.mp4");
    let encoded = Command::new(ffmpeg)
        .args(ffmpeg_args(&pattern, job.fps, job.frames, &out))
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .context("run ffmpeg")?;
    if !encoded.status.success() {
        bail!("ffmpeg encode failed: {}", stderr_tail(&encoded.stderr));
    }
    // The frames are many times the clip; only out.mp4 leaves this dir.
    let _ = tokio::fs::remove_dir_all(&frame_dir).await;
    Ok(out)
}

pub fn spawn_cpu_video_runner(
    net: Arc<Libp2pNetwork>,
    store: ChunkStore,
    cache_root: PathBuf,
    load: WorkerLoad,
) -> mpsc::Sender<AcceptedJob> {
    diffusion::kill_orphaned_renders();
    let (tx, mut rx) = mpsc::channel::<AcceptedJob>(4);
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let Ok(_compute_slot) = load.compute_slots().acquire_owned().await else {
                return;
            };
            let Ok(_node_slot) = diffusion::node_slot().acquire_owned().await else {
                return;
            };
            let started = Instant::now();
            let job_id = job.accept.job_id.clone();
            let result = crate::job_control::cancellable(&job.accept.job_id, run_job(&net, &store, &cache_root, &job)).await;
            cleanup_job_dir(&cache_root, &job_id).await;
            match result {
                Ok(hash) => {
                    load.record_job(started.elapsed());
                    info!(%job_id, %hash, secs = started.elapsed().as_secs(), "cpu video runner: job complete");
                    publish_receipt(&net, &job.accept, Some(hash), JobStatus::Completed).await;
                }
                Err(error) => {
                    warn!(%job_id, err = %format!("{error:#}"), "cpu video runner: job failed");
                    publish_receipt(&net, &job.accept, None, JobStatus::Failed).await;
                }
            }
        }
    });
    tx
}

async fn run_job(net: &Libp2pNetwork, store: &ChunkStore, cache_root: &Path, job: &AcceptedJob) -> Result<String> {
    let spec = job.accept.spec_inline.as_ref().context("accept missing spec_inline")?;
    let inline: VideoJobInline = serde_json::from_value(spec.clone()).context("decode video job")?;
    fits(&inline, WORKLOAD_TXT2VID).map_err(anyhow::Error::msg)?;
    let job_dir = safe_job_dir(cache_root, &job.accept.job_id)?;
    tokio::fs::create_dir_all(&job_dir).await?;
    let out = generate(&inline, &job_dir).await?;
    let len = tokio::fs::metadata(&out).await?.len();
    if len == 0 || len > video::MAX_RESULT_BYTES as u64 {
        bail!("video result is empty or exceeds the 256 MiB result limit");
    }
    let mut head = [0u8; 16];
    {
        use tokio::io::AsyncReadExt;
        let mut f = tokio::fs::File::open(&out).await?;
        let n = f.read(&mut head).await?;
        if !video::is_mp4(&head[..n]) {
            bail!("video result is not an MP4");
        }
    }
    crate::pieces::publish_result_file(net, store, &job.accept, &out, video::MAX_RESULT_BYTES as u64).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small() -> VideoJobInline {
        let mut req = serde_json::json!({ "model": MODEL_ID, "prompt": "a fox in the snow", "seed": 42 });
        video::apply_model_defaults(&mut req);
        serde_json::from_value(req).unwrap()
    }

    #[test]
    fn the_models_defaults_fit_the_cpu_caps() {
        let job = small();
        assert_eq!((job.frames, job.width, job.height, job.steps, job.fps), (33, 480, 272, 20, 16));
        assert!(fits(&job, WORKLOAD_TXT2VID).is_ok());
        // Portrait at the same area is fine too.
        let mut portrait = small();
        (portrait.width, portrait.height) = (272, 480);
        assert!(fits(&portrait, WORKLOAD_TXT2VID).is_ok());
    }

    #[test]
    fn cpu_refuses_what_it_cannot_finish() {
        let mut j = small();
        j.model = "wan2.2-ti2v-5b".into();
        assert!(fits(&j, WORKLOAD_TXT2VID).is_err(), "only the small model");
        let mut j = small();
        j.frames = 81;
        assert!(fits(&j, WORKLOAD_TXT2VID).is_err());
        let mut j = small();
        (j.width, j.height) = (832, 480);
        assert!(fits(&j, WORKLOAD_TXT2VID).is_err());
        let mut j = small();
        j.steps = 30;
        assert!(fits(&j, WORKLOAD_TXT2VID).is_err());
        let mut j = small();
        j.image_hash = Some("ab".repeat(32));
        assert!(fits(&j, video::WORKLOAD_IMG2VID).is_err(), "no img2vid on CPU");
    }

    #[test]
    fn the_cpu_model_is_in_the_manifest_and_marked_cpu() {
        let row = video::manifest_model(MODEL_ID).expect("manifest row");
        assert!(row.cpu);
    }

    #[test]
    fn sd_args_select_video_mode_and_the_pinned_files() {
        let job = small();
        let args = sd_args(&job, Path::new("/m"), Path::new("/j/frames/frame_%05d.png"), 16, 42);
        let pair = |flag: &str| args.iter().position(|a| a == flag).map(|i| args[i + 1].as_str());
        assert_eq!(pair("-M"), Some("vid_gen"), "Wan needs -M vid_gen; --video-frames alone is not video mode");
        assert_eq!(pair("--diffusion-model"), Some("/m/Wan2.1-T2V-1.3B-Q8_0.gguf"));
        assert_eq!(pair("--t5xxl"), Some("/m/umt5-xxl-encoder-Q4_K_M.gguf"));
        assert_eq!(pair("--vae"), Some("/m/wan_2.1_vae.safetensors"));
        assert_eq!(pair("--video-frames"), Some("33"));
        assert_eq!(pair("-W"), Some("480"));
        assert_eq!(pair("-H"), Some("272"));
        assert_eq!(pair("--steps"), Some("20"));
        assert_eq!(pair("--seed"), Some("42"));
        assert_eq!(pair("-t"), Some("16"));
        assert_eq!(pair("-o"), Some("/j/frames/frame_%05d.png"));
        assert_eq!(pair("-n"), Some(DEFAULT_NEGATIVE), "empty negative prompt gets Wan's default");
        assert_eq!(pair("-p"), Some("a fox in the snow"));

        let mut dash = small();
        dash.prompt = "--steps 999".into();
        let args = sd_args(&dash, Path::new("/m"), Path::new("/o.png"), 0, 1);
        let p = args.iter().position(|a| a == "-p").unwrap();
        assert_eq!(args[p + 1], " --steps 999", "a prompt never reads as a flag");
        let t = args.iter().position(|a| a == "-t").unwrap();
        assert_eq!(args[t + 1], "1");
    }

    #[test]
    fn ffmpeg_makes_a_browser_playable_mp4_of_exactly_the_frames() {
        let args = ffmpeg_args(Path::new("/f/frame_%05d.png"), 16, 33, Path::new("/j/out.mp4"));
        let pair = |flag: &str| args.iter().position(|a| a == flag).map(|i| args[i + 1].as_str());
        assert_eq!(pair("-framerate"), Some("16"));
        assert_eq!(pair("-frames:v"), Some("33"));
        assert_eq!(pair("-c:v"), Some("libx264"));
        assert_eq!(pair("-pix_fmt"), Some("yuv420p"));
        assert_eq!(args.last().map(String::as_str), Some("/j/out.mp4"));
    }

    #[test]
    fn gates_need_ram_threads_and_consent() {
        let config = VideoConfig::default();
        assert_eq!(config.cpu, None, "opt-in: nothing installs by default");
        let idle = diffusion::MemorySnapshot { total: 32 * GIB, available: 20 * GIB };
        let busy = diffusion::MemorySnapshot { total: 91 * GIB, available: 6 * GIB };
        let small_box = diffusion::MemorySnapshot { total: 8 * GIB, available: 7 * GIB };
        assert!(has_headroom_with(&config, idle));
        assert!(!has_headroom_with(&config, busy));
        assert!(!has_headroom_with(&config, small_box));
        // Half of a 16-core box is enough (the per-job budget is half).
        assert!(fast_enough_with(&config, 8));
        assert!(!fast_enough_with(&config, 4));
    }

    #[test]
    fn tags_name_the_role_the_model_and_cpu() {
        let t = tags();
        assert!(t.contains(&video::ROLE_TAG.to_string()));
        assert!(t.contains(&"c0mpute:video:model:wan2.1-t2v-1.3b".to_string()));
        assert!(t.contains(&CPU_TAG.to_string()));
    }

    #[test]
    fn the_download_is_about_5_4_gb_and_the_stamp_names_the_pins() {
        let gb = download_bytes() as f64 / 1e9;
        assert!((5.3..5.5).contains(&gb), "{gb}");
        assert!(ready_stamp().contains(MODEL_ID));
        assert!(ready_stamp().contains(&WEIGHTS[0].sha256[..12]));
    }
}
