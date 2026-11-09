//! `video.txt2vid` / `video.img2vid` — Wan 2.2 video generation.
//!
//! This is the first *container* workload: unlike transcode, TTS and STT,
//! the work happens in `ghcr.io/profullstack/c0mpute-video-runner` rather than in this
//! process. The node's side of the contract is small and entirely files:
//!
//!   1. Create the job directory, write `job.json`, and for `img2vid` fetch
//!      the conditioning image out of the chunk store beside it.
//!   2. `docker run` the image with the job directory bind-mounted at /job
//!      and a node-owned weights cache at /weights.
//!   3. Read `out.mp4`, check it against the job, store it, announce it.
//!
//! The runner image validates the job again and probes its own output with
//! ffprobe. Both sides checking is deliberate: the node must refuse a job it
//! cannot bill for before paying to start a container, and the runner is the
//! only side that can see what the model actually produced.
//!
//! Why no cross-worker verification: a generated clip is not reproducible
//! across workers (different driver, different kernels, same seed), so there
//! is nothing to compare hashes against. The receipt comes from one worker,
//! as with render.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use c0mpute_net::topics::JobStatus;
use c0mpute_net::{Libp2pNetwork, Network};
use c0mpute_store::ChunkStore;
use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::runner::{AcceptedJob, WorkerLoad, publish_receipt};

pub const WORKLOAD_TXT2VID: &str = "video.txt2vid";
pub const WORKLOAD_IMG2VID: &str = "video.img2vid";

/// Advertised only when the container runtime, the image and an NVIDIA GPU
/// are all present — see [`readiness`].
pub const ROLE_TAG: &str = "c0mpute:role:video";

/// Where `.github/workflows/video-runner.yml` publishes the runner.
pub const RUNNER_REPOSITORY: &str = "ghcr.io/profullstack/c0mpute-video-runner";

/// The stable tag the workflow moves on every master build.
pub const RUNNER_TAG: &str = "stable";

/// The published runner this release trusts, pinned by digest: a tag that
/// moves cannot change what a node runs. Bump it (and
/// `plugins/video/module.toml` [module.dispatch]) to the digest the workflow
/// prints in its job summary after a runner change.
pub const RUNNER_DIGEST: &str = "sha256:2ba52bedfaa2f8b0bb76a2778a3184c6baf591b9879c7fa090abb56bbddf141c";

/// What `c0mpute video setup` pulls: the repository at the pinned digest.
pub const RUNNER_PULL_REF: &str = "ghcr.io/profullstack/c0mpute-video-runner@sha256:2ba52bedfaa2f8b0bb76a2778a3184c6baf591b9879c7fa090abb56bbddf141c";

/// The local name the node runs. Setup tags the pinned pull as this, or
/// builds it from source under this name when the pull fails, so the worker
/// never depends on how the image arrived.
pub const RUNNER_IMAGE: &str = "c0mpute/video-runner:stable";

/// Where a node can build the runner itself when it cannot pull it.
///
/// The registry is a convenience, not a dependency: this repo is public, so
/// any node with docker and git can produce the same image from source. That
/// matters because a container registry is exactly the kind of thing that is
/// private by default, rate-limited, or down.
pub const RUNNER_SOURCE_REPO: &str = "https://github.com/profullstack/c0mpute.git";

/// Caps, mirroring the runner's own. The node checks first so it never pays
/// to start a container for a job it would have to fail.
pub const MAX_FRAMES: u32 = 241;
pub const MAX_STEPS: u32 = 60;
pub const MAX_PIXELS: u32 = 1280 * 720;
pub const MAX_PROMPT_CHARS: usize = 2000;
/// A clip is orders of magnitude bigger than a transcript; still bounded, so
/// one job cannot fill the store.
pub const MAX_RESULT_BYTES: usize = 256 * 1024 * 1024;
pub const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;

/// How long a single clip may take before the container is killed. Generous:
/// a 14B model at 720p on a busy card is minutes, and the alternative to
/// waiting is failing a job that was nearly done.
pub const JOB_TIMEOUT: Duration = Duration::from_secs(45 * 60);

/// One video job, as it arrives in `JobAccept.spec_inline`.
#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct VideoJobInline {
    /// Model id from the plugin manifest, e.g. `wan2.2-ti2v-5b`.
    pub model: String,
    pub prompt: String,
    #[serde(default)]
    pub negative_prompt: String,
    /// img2vid only: content hash of the whole conditioning image (one
    /// chunk, or the pieces below).
    #[serde(default)]
    pub image_hash: Option<String>,
    /// Image size in bytes; present with `image_pieces`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_size: Option<u64>,
    /// Hashes of the image's [`crate::pieces::INPUT_PIECE_BYTES`] pieces,
    /// in order. Absent: `image_hash` is a single chunk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_pieces: Option<Vec<String>>,
    /// Peer holding `image_hash`, when the buyer knows it.
    #[serde(default)]
    pub image_provider_peer_id: Option<String>,
    #[serde(default = "default_image_format")]
    pub image_format: String,
    #[serde(default = "default_frames")]
    pub frames: u32,
    #[serde(default = "default_fps")]
    pub fps: u32,
    #[serde(default = "default_steps")]
    pub steps: u32,
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_height")]
    pub height: u32,
    #[serde(default)]
    pub seed: Option<u64>,
    #[serde(default)]
    pub guidance_scale: Option<f32>,
}

fn default_frames() -> u32 {
    81
}
fn default_fps() -> u32 {
    24
}
fn default_steps() -> u32 {
    30
}
fn default_width() -> u32 {
    1280
}
fn default_height() -> u32 {
    704
}
fn default_image_format() -> String {
    "png".to_string()
}

impl VideoJobInline {
    /// The conditioning image as a checked input, when there is one.
    pub fn image_input(&self) -> Option<std::result::Result<crate::pieces::InputRef, String>> {
        self.image_hash.as_deref().map(|hash| {
            crate::pieces::InputRef::parse(
                "image",
                hash,
                self.image_size,
                self.image_pieces.as_deref(),
                MAX_IMAGE_BYTES,
            )
        })
    }

    /// Reject anything the runner would reject, plus anything that would make
    /// the container argv wrong. `workload` decides whether an image is
    /// required.
    pub fn validate(&self, workload: &str) -> std::result::Result<(), String> {
        if self.model.is_empty()
            || self.model.len() > 64
            || !self
                .model
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        {
            // The model id reaches the container inside job.json, not the
            // command line, but a loose id would still name a directory.
            return Err("model must be a short alphanumeric id".into());
        }
        let prompt = self.prompt.trim();
        if prompt.is_empty() {
            return Err("prompt must not be empty".into());
        }
        if prompt.chars().count() > MAX_PROMPT_CHARS {
            return Err(format!("prompt exceeds {MAX_PROMPT_CHARS} characters"));
        }
        if !(1..=MAX_FRAMES).contains(&self.frames) {
            return Err(format!("frames must be 1..{MAX_FRAMES}"));
        }
        if !(1..=60).contains(&self.fps) {
            return Err("fps must be 1..60".into());
        }
        if !(1..=MAX_STEPS).contains(&self.steps) {
            return Err(format!("steps must be 1..{MAX_STEPS}"));
        }
        if self.width % 16 != 0 || self.height % 16 != 0 {
            return Err("width and height must be multiples of 16".into());
        }
        if self.width == 0 || self.height == 0 {
            return Err("width and height must be positive".into());
        }
        if self.width.saturating_mul(self.height) > MAX_PIXELS {
            return Err(format!("width*height must not exceed {MAX_PIXELS}"));
        }
        if !["png", "jpg", "jpeg", "webp"].contains(&self.image_format.as_str()) {
            return Err("image format must be png, jpg, jpeg or webp".into());
        }
        match workload {
            WORKLOAD_TXT2VID => {}
            WORKLOAD_IMG2VID => {
                match self.image_input() {
                    None => return Err("video.img2vid needs an image_hash".into()),
                    Some(Err(e)) => return Err(e),
                    Some(Ok(_)) => {}
                }
            }
            other => return Err(format!("unsupported workload {other}")),
        }
        if self.image_hash.is_none() && (self.image_size.is_some() || self.image_pieces.is_some()) {
            return Err("image_size and image_pieces need an image_hash".into());
        }
        if let Some(scale) = self.guidance_scale {
            if !(0.0..=20.0).contains(&scale) || !scale.is_finite() {
                return Err("guidance_scale must be 0..20".into());
            }
        }
        Ok(())
    }

    /// The `job.json` the runner reads. `image` is a bare filename because
    /// the runner refuses anything with a path separator.
    pub fn to_job_json(&self, job_id: &str, workload: &str, image: Option<&str>) -> serde_json::Value {
        let mut job = serde_json::json!({
            "job_id": job_id,
            "workload": workload,
            "model": self.model,
            "prompt": self.prompt.trim(),
            "negative_prompt": self.negative_prompt.trim(),
            "frames": self.frames,
            "fps": self.fps,
            "steps": self.steps,
            "width": self.width,
            "height": self.height,
        });
        if let Some(seed) = self.seed {
            job["seed"] = serde_json::json!(seed & 0xffff_ffff);
        }
        if let Some(scale) = self.guidance_scale {
            job["guidance_scale"] = serde_json::json!(scale);
        }
        if let Some(name) = image {
            job["image"] = serde_json::json!(name);
        }
        job
    }
}

/// Everything the container invocation needs. Separated from the runner so
/// the argv can be unit-tested without a GPU, a daemon or an image.
#[derive(Clone, Debug)]
pub struct ContainerRun {
    pub image: String,
    /// `txt2vid` or `img2vid` — the runner's first positional argument.
    pub command: String,
    pub job_dir: PathBuf,
    pub weights_dir: PathBuf,
    pub container_name: String,
    /// Run as this uid:gid so the clip is owned by the node, not by root.
    /// Without it the node cannot clean up its own job directory.
    pub uid: u32,
    pub gid: u32,
}

/// Build the `docker run` argv.
///
/// Network is left at the default because a cold start pulls weights from
/// Hugging Face. Once weights come from the storage plugin (DIP-0012) this
/// should become `--network none`, which is the real prize: a workload
/// container with no network at all.
pub fn docker_argv(run: &ContainerRun) -> Vec<String> {
    vec![
        "run".to_string(),
        "--rm".to_string(),
        // Named so a timeout can kill exactly this container.
        "--name".to_string(),
        run.container_name.clone(),
        "--gpus".to_string(),
        "all".to_string(),
        "--user".to_string(),
        format!("{}:{}", run.uid, run.gid),
        // The container gets the job directory and the weights cache, and
        // nothing else of the host.
        "-v".to_string(),
        format!("{}:/job", run.job_dir.display()),
        "-v".to_string(),
        format!("{}:/weights", run.weights_dir.display()),
        "-w".to_string(),
        "/job".to_string(),
        run.image.clone(),
        run.command.clone(),
        "--job".to_string(),
        "/job/job.json".to_string(),
    ]
}

/// `docker build` argv for the runner, from a checkout of this repo.
///
/// The context is `plugins/video`, not the runner directory: the image needs
/// `module.toml` (its model registry), which lives one level up.
pub fn build_argv(checkout: &Path, image: &str) -> Vec<String> {
    let context = checkout.join("plugins").join("video");
    let dockerfile = context.join("runner").join("Dockerfile");
    vec![
        "build".to_string(),
        "-t".to_string(),
        image.to_string(),
        "-f".to_string(),
        dockerfile.display().to_string(),
        context.display().to_string(),
    ]
}

/// How `c0mpute video setup` got the runner image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupSource {
    /// Pulled at the pinned digest and tagged as [`RUNNER_IMAGE`].
    Pulled,
    /// The pull failed (or `--build`): built from a clone of the source repo.
    Built,
}

/// The source repo to build from: `$C0MPUTE_VIDEO_SOURCE_REPO` (the same
/// override `install.sh` takes), else [`RUNNER_SOURCE_REPO`].
pub fn source_repo() -> String {
    std::env::var("C0MPUTE_VIDEO_SOURCE_REPO").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| RUNNER_SOURCE_REPO.to_string())
}

async fn run_status(cmd: &mut tokio::process::Command, what: &str) -> Result<bool> {
    Ok(cmd
        .stdin(std::process::Stdio::null())
        .status()
        .await
        .with_context(|| format!("could not run {what}"))?
        .success())
}

/// Put the runner image on this node as [`RUNNER_IMAGE`]: pull the pinned
/// digest and tag it, or, when the registry will not serve it (private,
/// rate-limited, offline) or `build_only`, shallow-clone the public repo
/// into `scratch` and build the same Dockerfile. `say` gets progress lines.
pub async fn setup(build_only: bool, scratch: &Path, say: &(dyn Fn(&str) + Sync)) -> Result<SetupSource> {
    if !build_only {
        say(&format!("pulling {RUNNER_PULL_REF} (this is a large image)"));
        let pulled = run_status(crate::container::command().args(["pull", RUNNER_PULL_REF]), "docker; install a container runtime first").await?;
        if pulled && run_status(crate::container::command().args(["tag", RUNNER_PULL_REF, RUNNER_IMAGE]), "docker tag").await? {
            return Ok(SetupSource::Pulled);
        }
        // A registry is the kind of dependency that is private by default,
        // rate-limited or down, and none of those are a reason a node with a
        // card cannot serve video. The repo is public: build it.
        say("could not pull the runner image (private, rate-limited or offline).");
        say("building it from source instead: same Dockerfile, same runner.");
    }
    // A fresh directory per run: a half-finished clone from an earlier
    // attempt would make `git clone` fail rather than resume.
    let checkout = scratch.join(format!("c0mpute-video-src-{}", std::process::id()));
    let _ = fs::remove_dir_all(&checkout).await;
    let repo = source_repo();
    say(&format!("cloning {repo} (shallow)"));
    let cloned = run_status(tokio::process::Command::new("git").args(["clone", "--depth", "1", &repo]).arg(&checkout), "git (building the runner needs git)").await;
    let built = match cloned {
        Ok(true) => {
            say(&format!("building {RUNNER_IMAGE} (several minutes)"));
            run_status(crate::container::command().args(build_argv(&checkout, RUNNER_IMAGE)), "docker build").await
        }
        Ok(false) => Err(anyhow::anyhow!("git clone of {repo} failed")),
        Err(e) => Err(e),
    };
    // The checkout is scratch whether or not the build worked.
    let _ = fs::remove_dir_all(&checkout).await;
    if !built? {
        bail!("docker build of the runner failed");
    }
    say(&format!("built {RUNNER_IMAGE}"));
    Ok(SetupSource::Built)
}

/// The runner command for a workload type.
pub fn command_for(workload: &str) -> Option<&'static str> {
    match workload {
        WORKLOAD_TXT2VID => Some("txt2vid"),
        WORKLOAD_IMG2VID => Some("img2vid"),
        _ => None,
    }
}

/// Why this node cannot serve video, or `None` when it can.
///
/// Checked before subscribing: a node that advertises video without a GPU,
/// a container runtime or the image wins auctions and then fails them, which
/// costs it the receipt and the buyer the wait.
#[derive(Debug, PartialEq, Eq)]
pub enum NotReady {
    NoDocker,
    NoImage,
    NoNvidiaGpu,
}

impl NotReady {
    pub fn reason(&self) -> &'static str {
        match self {
            NotReady::NoDocker => "no container runtime on PATH (install docker)",
            NotReady::NoImage => "runner image not pulled (run `c0mpute video setup`)",
            NotReady::NoNvidiaGpu => "no NVIDIA GPU detected (nvidia-smi found none)",
        }
    }
}

/// Probe the host. Blocking, and only called at boot or from the CLI.
pub fn readiness(image: &str) -> Option<NotReady> {
    if crate::container::which().is_none() {
        return Some(NotReady::NoDocker);
    }
    if !nvidia_gpu_present() {
        return Some(NotReady::NoNvidiaGpu);
    }
    if !image_present(image) {
        return Some(NotReady::NoImage);
    }
    None
}

/// Set once this node has been seen to be video-capable.
static KNOWN_READY: AtomicBool = AtomicBool::new(false);

/// [`readiness`], but it remembers a yes.
///
/// The advertise loop asks every 60 s, and `readiness` spawns `nvidia-smi`
/// and talks to the docker daemon. In practice readiness only flips one way:
/// an operator pulls the image and the role starts advertising, the same
/// shape as TTS appearing once Kokoro finishes provisioning. A card that
/// disappears mid-run is a failed job, not a stale tag, so a remembered yes
/// costs nothing and a remembered no would strand the operator.
pub fn readiness_cached(image: &str) -> Option<NotReady> {
    if KNOWN_READY.load(Ordering::Relaxed) {
        return None;
    }
    let verdict = readiness(image);
    if verdict.is_none() {
        KNOWN_READY.store(true, Ordering::Relaxed);
    }
    verdict
}

fn nvidia_gpu_present() -> bool {
    std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=name", "--format=csv,noheader"])
        .output()
        .map(|out| out.status.success() && !out.stdout.is_empty())
        .unwrap_or(false)
}

fn image_present(image: &str) -> bool {
    crate::container::command_sync()
        .args(["image", "inspect", image])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Spawn the video runner. One job at a time per node: a clip wants the whole
/// card, and two concurrent jobs on one GPU is how both of them OOM.
pub fn spawn_video_runner(
    net: Arc<Libp2pNetwork>,
    store: ChunkStore,
    cache_root: PathBuf,
    workload: String,
    image: String,
    load: WorkerLoad,
) -> mpsc::Sender<AcceptedJob> {
    let (tx, mut rx) = mpsc::channel::<AcceptedJob>(4);
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let Ok(_compute_slot) = load.compute_slots().acquire_owned().await else {
                return;
            };
            let started = Instant::now();
            let job_id = job.accept.job_id.clone();
            let result = run_video_job(&net, &store, &cache_root, &workload, &image, &job).await;
            crate::runner::cleanup_job_dir(&cache_root, &job_id).await;
            match result {
                Ok(hash) => {
                    load.record_job(started.elapsed());
                    info!(%job_id, %hash, elapsed_s = started.elapsed().as_secs(), "video runner: job complete");
                    publish_receipt(&net, &job.accept, Some(hash), JobStatus::Completed).await;
                }
                Err(error) => {
                    warn!(%job_id, err = %error, "video runner: job failed");
                    publish_receipt(&net, &job.accept, None, JobStatus::Failed).await;
                }
            }
        }
    });
    tx
}

async fn run_video_job(
    net: &Arc<Libp2pNetwork>,
    store: &ChunkStore,
    cache_root: &Path,
    workload: &str,
    image: &str,
    job: &AcceptedJob,
) -> Result<String> {
    let spec = job
        .accept
        .spec_inline
        .as_ref()
        .context("accept missing spec_inline")?;
    let inline: VideoJobInline =
        serde_json::from_value(spec.clone()).context("decode video job")?;
    if let Err(reason) = inline.validate(workload) {
        bail!("{reason}");
    }
    let command = command_for(workload).context("unsupported video workload")?;

    let job_dir = crate::runner::safe_job_dir(cache_root, &job.accept.job_id)?;
    fs::create_dir_all(&job_dir).await?;

    // img2vid: the conditioning image comes out of the chunk store, verified
    // against its hash, and lands in the job dir under a bare filename.
    let image_name = match inline.image_input() {
        Some(input) => {
            let input = input.map_err(anyhow::Error::msg)?;
            let provider = inline
                .image_provider_peer_id
                .as_deref()
                .map(str::parse)
                .transpose()
                .context("invalid image provider peer id")?;
            let bytes = input
                .fetch(net, provider, MAX_IMAGE_BYTES)
                .await
                .context("fetch the conditioning image (maximum 32 MiB)")?;
            let name = format!("input.{}", inline.image_format);
            fs::write(job_dir.join(&name), &bytes).await?;
            Some(name)
        }
        None => None,
    };

    let job_json = inline.to_job_json(&job.accept.job_id, workload, image_name.as_deref());
    fs::write(
        job_dir.join("job.json"),
        serde_json::to_vec_pretty(&job_json)?,
    )
    .await?;

    let weights_dir = weights_dir(cache_root);
    fs::create_dir_all(&weights_dir).await?;

    let (uid, gid) = owner_of(&job_dir).await?;
    let run = ContainerRun {
        image: image.to_string(),
        command: command.to_string(),
        job_dir: job_dir.clone(),
        weights_dir,
        container_name: format!("c0mpute-video-{}", job.accept.job_id),
        uid,
        gid,
    };

    run_container(&run).await?;

    let out_path = job_dir.join("out.mp4");
    let bytes = fs::read(&out_path)
        .await
        .context("runner produced no out.mp4")?;
    if bytes.is_empty() || bytes.len() > MAX_RESULT_BYTES {
        bail!("video result is empty or exceeds the 256 MiB result limit");
    }
    if !is_mp4(&bytes) {
        bail!("video result is not an MP4");
    }
    let hash = store.put(&bytes).await?;
    net.announce(&hash).await?;
    Ok(hash.to_hex())
}

/// `ftyp` at offset 4 is the ISO-BMFF marker. The runner already ffprobed the
/// file; this is the node refusing to store something that is not a clip at
/// all, which is cheap and catches a broken image rather than a bad model.
pub(crate) fn is_mp4(bytes: &[u8]) -> bool {
    bytes.len() > 12 && &bytes[4..8] == b"ftyp"
}

pub fn weights_dir(cache_root: &Path) -> PathBuf {
    cache_root.join("video").join("weights")
}

#[cfg(unix)]
async fn owner_of(path: &Path) -> Result<(u32, u32)> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata(path).await?;
    Ok((meta.uid(), meta.gid()))
}

/// Video needs Linux + NVIDIA + docker; there is no owner to map elsewhere.
#[cfg(not(unix))]
async fn owner_of(_path: &Path) -> Result<(u32, u32)> {
    anyhow::bail!("video containers run on Unix hosts only")
}

async fn run_container(run: &ContainerRun) -> Result<()> {
    let args = docker_argv(run);
    info!(image = %run.image, command = %run.command, "video runner: starting container");
    let mut child = crate::container::command()
        .args(&args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("could not start docker")?;

    match tokio::time::timeout(JOB_TIMEOUT, child.wait()).await {
        Ok(status) => {
            let status = status.context("waiting for the runner container")?;
            if !status.success() {
                bail!("runner exited with {status}");
            }
            Ok(())
        }
        Err(_) => {
            // kill_on_drop reaps `docker run`, but the container it started is
            // a child of the daemon and would keep the GPU busy.
            let _ = crate::container::command()
                .args(["kill", &run.container_name])
                .output()
                .await;
            bail!(
                "runner exceeded the {} minute job timeout",
                JOB_TIMEOUT.as_secs() / 60
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> VideoJobInline {
        VideoJobInline {
            model: "wan2.2-ti2v-5b".into(),
            prompt: "a neon alley in the rain".into(),
            negative_prompt: String::new(),
            image_hash: None,
            image_size: None,
            image_pieces: None,
            image_provider_peer_id: None,
            image_format: "png".into(),
            frames: 81,
            fps: 24,
            steps: 30,
            width: 1280,
            height: 704,
            seed: Some(7),
            guidance_scale: None,
        }
    }

    #[test]
    fn a_well_formed_txt2vid_job_is_accepted() {
        assert!(job().validate(WORKLOAD_TXT2VID).is_ok());
    }

    #[test]
    fn defaults_come_from_serde() {
        let inline: VideoJobInline =
            serde_json::from_value(serde_json::json!({ "model": "m", "prompt": "p" })).unwrap();
        assert_eq!(inline.frames, 81);
        assert_eq!(inline.fps, 24);
        assert_eq!(inline.steps, 30);
        assert_eq!(inline.width, 1280);
        assert_eq!(inline.height, 704);
        assert!(inline.seed.is_none());
    }

    #[test]
    fn unbounded_jobs_are_refused() {
        for (field, mutate) in [
            ("frames", (|j: &mut VideoJobInline| j.frames = 5000) as fn(&mut VideoJobInline)),
            ("zero frames", |j| j.frames = 0),
            ("steps", |j| j.steps = 500),
            ("fps", |j| j.fps = 0),
            ("pixels", |j| {
                j.width = 1920;
                j.height = 1088;
            }),
            ("alignment", |j| j.width = 1281),
            ("empty prompt", |j| j.prompt = "   ".into()),
            ("long prompt", |j| j.prompt = "x".repeat(3000)),
            ("model id", |j| j.model = "../../etc/passwd".into()),
            ("image format", |j| j.image_format = "tiff".into()),
            ("guidance", |j| j.guidance_scale = Some(99.0)),
        ] {
            let mut j = job();
            mutate(&mut j);
            assert!(
                j.validate(WORKLOAD_TXT2VID).is_err(),
                "{field} should be refused"
            );
        }
    }

    #[test]
    fn img2vid_requires_an_image_and_txt2vid_does_not() {
        assert!(job().validate(WORKLOAD_IMG2VID).is_err());
        let mut with_image = job();
        with_image.image_hash = Some("ab".repeat(32));
        assert!(with_image.validate(WORKLOAD_IMG2VID).is_ok());
        // An unknown workload is refused even with a valid spec.
        assert!(with_image.validate("video.gif").is_err());
    }

    #[test]
    fn job_json_carries_the_runner_contract() {
        let j = job();
        let v = j.to_job_json("job-1", WORKLOAD_TXT2VID, None);
        assert_eq!(v["workload"], WORKLOAD_TXT2VID);
        assert_eq!(v["model"], "wan2.2-ti2v-5b");
        assert_eq!(v["frames"], 81);
        assert_eq!(v["seed"], 7);
        assert!(v.get("image").is_none());
        assert!(v.get("guidance_scale").is_none());

        let with_image = j.to_job_json("job-1", WORKLOAD_IMG2VID, Some("input.png"));
        assert_eq!(with_image["image"], "input.png");
    }

    #[test]
    fn a_huge_seed_is_folded_into_32_bits() {
        let mut j = job();
        j.seed = Some(u64::MAX);
        let v = j.to_job_json("job-1", WORKLOAD_TXT2VID, None);
        assert_eq!(v["seed"], 0xffff_ffffu64);
    }

    #[test]
    fn docker_argv_asks_for_the_gpu_and_mounts_only_the_job_and_weights() {
        let run = ContainerRun {
            image: RUNNER_IMAGE.into(),
            command: "txt2vid".into(),
            job_dir: PathBuf::from("/var/c0mpute/jobs/abc"),
            weights_dir: PathBuf::from("/var/c0mpute/video/weights"),
            container_name: "c0mpute-video-abc".into(),
            uid: 1000,
            gid: 1000,
        };
        let argv = docker_argv(&run);

        let window = argv.windows(2).any(|w| w[0] == "--gpus" && w[1] == "all");
        assert!(window, "must request the GPU: {argv:?}");
        assert!(argv.contains(&"--rm".to_string()));
        assert!(argv.contains(&"1000:1000".to_string()), "runs as the node user");
        assert!(argv.contains(&"/var/c0mpute/jobs/abc:/job".to_string()));
        assert!(argv.contains(&"/var/c0mpute/video/weights:/weights".to_string()));
        assert!(argv.contains(&"c0mpute-video-abc".to_string()));
        // The image must come before the runner's own arguments, or docker
        // reads them as its own.
        let image_at = argv.iter().position(|a| a == RUNNER_IMAGE).unwrap();
        let cmd_at = argv.iter().position(|a| a == "txt2vid").unwrap();
        assert!(image_at < cmd_at, "image precedes the command: {argv:?}");
        // Exactly the two mounts, so nothing else of the host is exposed.
        assert_eq!(argv.iter().filter(|a| *a == "-v").count(), 2);
    }

    #[test]
    fn build_argv_points_at_the_manifest_context() {
        let argv = build_argv(Path::new("/src/c0mpute"), "img:tag");
        // The Dockerfile lives under runner/, but the context must be the
        // plugin directory so module.toml is in the image.
        assert!(argv.contains(&"/src/c0mpute/plugins/video/runner/Dockerfile".to_string()));
        assert!(argv.contains(&"/src/c0mpute/plugins/video".to_string()));
        assert!(argv.contains(&"img:tag".to_string()));
        // Context last: docker reads the final positional as the context.
        assert_eq!(argv.last().unwrap(), "/src/c0mpute/plugins/video");
        let dash_f = argv.iter().position(|a| a == "-f").unwrap();
        assert_eq!(argv[dash_f + 1], "/src/c0mpute/plugins/video/runner/Dockerfile");
    }

    #[test]
    fn commands_map_to_workloads() {
        assert_eq!(command_for(WORKLOAD_TXT2VID), Some("txt2vid"));
        assert_eq!(command_for(WORKLOAD_IMG2VID), Some("img2vid"));
        assert_eq!(command_for("ffmpeg.transcode"), None);
    }

    #[test]
    fn mp4_sniffing_rejects_other_files() {
        let mut mp4 = vec![0u8; 16];
        mp4[4..8].copy_from_slice(b"ftyp");
        assert!(is_mp4(&mp4));
        assert!(!is_mp4(b"RIFF....WAVE"));
        assert!(!is_mp4(b"short"));
        assert!(!is_mp4(&[]));
    }

    #[test]
    fn not_ready_reasons_tell_the_operator_what_to_do() {
        assert!(NotReady::NoImage.reason().contains("c0mpute video setup"));
        assert!(NotReady::NoDocker.reason().contains("docker"));
        assert!(NotReady::NoNvidiaGpu.reason().contains("NVIDIA"));
    }

    #[test]
    fn cached_readiness_agrees_with_a_fresh_probe() {
        // Whatever this box is, the two must say the same thing, and asking
        // twice must not change the answer.
        let fresh = readiness(RUNNER_IMAGE).is_none();
        assert_eq!(readiness_cached(RUNNER_IMAGE).is_none(), fresh);
        assert_eq!(readiness_cached(RUNNER_IMAGE).is_none(), fresh);
    }

    #[test]
    fn the_runner_is_pinned_by_digest_and_matches_the_manifest() {
        assert_eq!(RUNNER_PULL_REF, format!("{RUNNER_REPOSITORY}@{RUNNER_DIGEST}"));
        let digest = RUNNER_DIGEST.strip_prefix("sha256:").unwrap();
        assert!(digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()));
        // The local name is not a registry reference: nothing can pull a
        // different image under it.
        assert!(!RUNNER_IMAGE.contains("ghcr.io") && !RUNNER_IMAGE.contains('@'));
        let manifest: toml::Value =
            toml::from_str(include_str!("../../../../plugins/video/module.toml")).unwrap();
        assert_eq!(
            manifest["module"]["dispatch"]["image"].as_str(),
            Some(RUNNER_PULL_REF),
            "plugins/video/module.toml [module.dispatch] image must be the pinned digest"
        );
    }

    #[test]
    fn weights_live_under_the_cache_root() {
        let dir = weights_dir(Path::new("/var/c0mpute"));
        assert_eq!(dir, PathBuf::from("/var/c0mpute/video/weights"));
    }
}
