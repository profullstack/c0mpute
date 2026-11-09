//! The `crowdgpt` plugin: donate this node's idle compute to CrowdGPT.
//!
//! [CrowdGPT](https://github.com/Vxtzq/CrowdGPT) trains an open LLM
//! (huggingface.co/Vxtzq/Crowd-v1) from volunteered compute: its client joins
//! a pool of three volunteers, downloads the current weights, trains on the
//! same shard/seed/steps as its pool for about an hour, and uploads the weight
//! delta; the coordinator drops outliers and merges the rest.
//!
//! This EARNS NOTHING. It is a donation, off unless the operator turns it on
//! (`c0mpute crowdgpt on`), and it only runs while the node has no paid work:
//!
//! - **idle only**: every [`TICK`] the governor asks [`crate::activity`]
//!   (bids in flight, jobs won and not yet receipted) and the supervisor's
//!   node-wide compute slots. Only after [`CrowdgptConfig::idle_secs`] of
//!   continuous idleness does it start the client.
//! - **preempted at once**: the moment a bid goes out or a job lands it runs
//!   `docker stop -t <stop_grace_secs>` (SIGTERM, then SIGKILL after the
//!   grace period; the client stops at its next safe step on the first
//!   SIGTERM). The round in progress is forfeited, upload included, so a
//!   multi-GB delta upload never competes with paid work for the uplink. It
//!   resumes after the node has been idle again for `idle_secs`.
//! - **private mode**: a private node does not donate (the client talks to
//!   CrowdGPT's coordinator and Hugging Face) unless `allow_in_private`.
//!
//! # GPL isolation
//!
//! The client is GPL-3.0. c0mpute never vendors, links or imports it: it runs
//! as a separate process in its own container, built from CrowdGPT's public
//! repository at the reviewed commit [`CROWDGPT_COMMIT`] by our MIT Dockerfile
//! (node/crates/c0mpute-core/crowdgpt-runner). c0mpute talks to it only through `docker run` /
//! `docker stop` and its log lines.
//!
//! # Containment
//!
//! `docker run --rm --init --read-only --cap-drop ALL --security-opt
//! no-new-privileges`, a CPU quota (`--cpus`, half the node by default) at the
//! lowest CPU weight (`--cpu-shares 2`) plus `nice 19` inside, a hard memory
//! limit with no swap, `--oom-score-adj 1000` so the kernel and earlyoom kill
//! it before anything else, a pid limit, the default bridge network (outbound
//! HTTPS to Hugging Face and the coordinator; nothing published), the node
//! user's uid, and exactly one host mount: its own cache dir as `/data`.
//! Self-update is off twice: `--no-update`, and a read-only `/app` it could
//! not write an update into anyway. A newer coordinator may then refuse this
//! pinned client; that shows as "client pin outdated" and is fixed by bumping
//! [`CROWDGPT_COMMIT`] in a reviewed PR, never by following upstream.
//!
//! The engram table costs ~1.4 GB of RAM; it is off on nodes under 16 GB.
//!
//! A watchdog inside the container stops the client when the governor's
//! heartbeat file goes stale, so a crashed or killed worker cannot leave
//! CrowdGPT running unsupervised.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::{info, warn};

use crate::runner::WorkerLoad;

/// The CrowdGPT commit the runner image is built from. Reviewed: headless
/// client path, signal handling (first SIGTERM = graceful stop), self-update
/// (`--no-update` honoured at start and between rounds), weights loaded with
/// safetensors (no pickle), tokenizer `trust_remote_code` skipped with
/// `--no-tokenizer`. Bump only after reviewing the diff.
pub const CROWDGPT_COMMIT: &str = "b7d5b399c74c1c2e1d23008b56d2ce953a8189d8";
pub const CROWDGPT_REPO: &str = "https://github.com/Vxtzq/CrowdGPT.git";
/// Image label carrying the commit, checked before every run.
pub const COMMIT_LABEL: &str = "org.c0mpute.crowdgpt.commit";
/// Published by .github/workflows/crowdgpt-runner.yml.
pub const RUNNER_REPOSITORY: &str = "ghcr.io/profullstack/c0mpute-crowdgpt-runner";
/// The local name the governor runs.
pub const RUNNER_IMAGE: &str = "c0mpute/crowdgpt-runner:stable";
/// Container name prefix; each run gets `<prefix>-<n>` so a container the
/// daemon is slow to remove never blocks the next start. All carry [`LABEL`].
pub const CONTAINER: &str = "c0mpute-crowdgpt";
pub const LABEL: &str = "c0mpute.workload=crowdgpt";

/// The image the governor runs: [`RUNNER_IMAGE`], or `$C0MPUTE_CROWDGPT_IMAGE`
/// (tests and operators trying a local build under another name).
pub fn runner_image() -> String {
    std::env::var("C0MPUTE_CROWDGPT_IMAGE").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| RUNNER_IMAGE.to_string())
}
pub const DONATE_TAG: &str = "c0mpute:donate:crowdgpt";
/// How often the governor looks at the node.
pub const TICK: Duration = Duration::from_secs(2);
/// The watchdog in the container stops the client after this long without a heartbeat.
pub const HEARTBEAT_STALE: Duration = Duration::from_secs(90);

/// The pull reference for the pinned commit (`:crowdgpt-<sha12>`).
pub fn pull_ref() -> String {
    format!("{RUNNER_REPOSITORY}:crowdgpt-{}", &CROWDGPT_COMMIT[..12])
}

/// Our Dockerfile and watchdog (node/crates/c0mpute-core/crowdgpt-runner,
/// inside the crate so builds that copy only `node/` still compile), embedded so `c0mpute crowdgpt setup --build`
/// needs only docker (CrowdGPT's source is fetched inside the build).
pub const DOCKERFILE: &str = include_str!("../crowdgpt-runner/Dockerfile");
pub const WATCHDOG: &str = include_str!("../crowdgpt-runner/c0mpute-crowdgpt-run");

/// `[crowdgpt]` in config.toml. Off by default.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CrowdgptConfig {
    /// Donate idle compute (`c0mpute crowdgpt on|off`).
    pub enabled: bool,
    /// Also donate while private mode is on.
    pub allow_in_private: bool,
    /// CPU quota for the container; 0 = half of this node's CPUs.
    pub cpus: f64,
    /// Memory limit in GB (no swap); 0 = a third of RAM, between 4 and 16.
    pub memory_gb: u32,
    /// Train the engram table (+~1.4 GB RAM). Unset = on with 16 GB+ RAM.
    pub engram: Option<bool>,
    /// `cpu` (default) or `cuda` (needs the NVIDIA container runtime and a
    /// CUDA build: `c0mpute crowdgpt setup --build --backend cuda`).
    pub backend: String,
    /// Seconds of continuous idleness before the client starts.
    pub idle_secs: u64,
    /// Seconds between SIGTERM and SIGKILL when paid work arrives.
    pub stop_grace_secs: u64,
}

impl Default for CrowdgptConfig {
    fn default() -> Self {
        Self { enabled: false, allow_in_private: false, cpus: 0.0, memory_gb: 0, engram: None, backend: "cpu".into(), idle_secs: 120, stop_grace_secs: 20 }
    }
}

/// Where the plugin keeps its files: `<data dir>/crowdgpt`.
pub fn home() -> PathBuf {
    crate::config::data_dir().unwrap_or_else(|| PathBuf::from("./c0mpute-data")).join("crowdgpt")
}
/// The container's only mount (`/data`): weights cache, checkpoints, session.
pub fn cache_dir() -> PathBuf {
    home().join("cache")
}
pub fn status_path() -> PathBuf {
    home().join("status.json")
}
/// An operator's CrowdGPT token, 0600, read by the watchdog inside the
/// container (never on a command line or in `docker inspect`).
pub fn token_path() -> PathBuf {
    cache_dir().join("c0mpute-token")
}
fn heartbeat_path() -> PathBuf {
    cache_dir().join("c0mpute-heartbeat")
}

/// The name CrowdGPT should credit: `c0mpute-<node nickname>`.
pub fn worker_name() -> String {
    match crate::capabilities::node_name() {
        Some(n) => format!("c0mpute-{n}"),
        None => "c0mpute-node".to_string(),
    }
}

fn total_ram_gb() -> u64 {
    crate::text::local_ram_mb() / 1024
}

impl CrowdgptConfig {
    pub fn effective_cpus(&self) -> f64 {
        if self.cpus > 0.0 { self.cpus.min(crate::resources::available_cpu_capacity()) } else { (crate::resources::available_cpu_capacity() / 2.0).max(1.0) }
    }
    pub fn effective_memory_gb(&self, ram_gb: u64) -> u64 {
        if self.memory_gb > 0 { self.memory_gb as u64 } else { (ram_gb / 3).clamp(4, 16) }
    }
    /// CROWDGPT_ENGRAM: off under 16 GB unless the operator says otherwise.
    pub fn engram_on(&self, ram_gb: u64) -> bool {
        self.engram.unwrap_or(ram_gb >= 16)
    }
}

/// The `docker run` arguments (after the CLI name).
pub fn run_argv(cfg: &CrowdgptConfig, name: &str, cache: &Path, uid_gid: Option<(u32, u32)>, ram_gb: u64) -> Vec<String> {
    let cpus = cfg.effective_cpus();
    let mem = cfg.effective_memory_gb(ram_gb);
    let threads = (cpus.floor() as u64).max(1);
    let mut a: Vec<String> = [
        "run", "--rm", "--init", "--name", name, "--label", LABEL,
        "--read-only", "--tmpfs", "/tmp:rw,size=512m", "--security-opt", "no-new-privileges", "--cap-drop", "ALL",
        "--network", "bridge", "--pids-limit", "512", "--cpu-shares", "2", "--oom-score-adj", "1000",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    a.extend(["--cpus".into(), format!("{cpus:.2}"), "--memory".into(), format!("{mem}g"), "--memory-swap".into(), format!("{mem}g")]);
    a.extend(["--shm-size".into(), "1g".into()]);
    if let Some((u, g)) = uid_gid {
        a.extend(["--user".into(), format!("{u}:{g}")]);
    }
    for (k, v) in [
        ("HOME", "/data/home".to_string()),
        ("CROWDGPT_ENGRAM", if cfg.engram_on(ram_gb) { "1" } else { "0" }.to_string()),
        ("CROWDGPT_BACKEND", if cfg.backend == "cuda" { "cuda" } else { "cpu" }.to_string()),
        ("C0MPUTE_WORKER_NAME", worker_name()),
        ("C0MPUTE_HEARTBEAT_STALE_S", HEARTBEAT_STALE.as_secs().to_string()),
        ("OMP_NUM_THREADS", threads.to_string()),
        ("MKL_NUM_THREADS", threads.to_string()),
    ] {
        a.extend(["-e".into(), format!("{k}={v}")]);
    }
    if cfg.backend == "cuda" {
        a.extend(["--gpus".into(), "all".into()]);
    }
    a.extend(["-v".into(), format!("{}:/data", cache.display()), runner_image()]);
    a
}

// ── status ──────────────────────────────────────────────────────────────

/// What the governor knows, written to [`status_path`] for `c0mpute crowdgpt
/// status` and `c0mpute status` (other processes).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Status {
    /// disabled | blocked | waiting-idle | running | stopping | backoff
    pub state: String,
    /// Why it is blocked / backing off, or what the client is doing.
    pub detail: String,
    pub updated_at: u64,
    pub rounds_started: u64,
    /// Deltas the coordinator accepted ("Submitted successfully").
    pub rounds_submitted: u64,
    /// Rounds the coordinator refused or did not count.
    pub rounds_lost: u64,
    /// Times paid work stopped the client.
    pub preemptions: u64,
    /// Tokens trained, estimated from the client's tokens/sec (includes
    /// rounds later forfeited to preemption).
    pub tokens_trained_estimate: u64,
    pub last_loss: Option<f64>,
    pub last_tokens_per_sec: Option<f64>,
    pub last_submitted_at: Option<u64>,
    pub last_exit: Option<String>,
    /// The coordinator refused this pinned client version.
    pub client_outdated: bool,
    pub crowdgpt_commit: String,
    pub worker_name: String,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Read the last status the governor wrote (`None` before the first).
pub fn read_status() -> Option<Status> {
    std::fs::read(status_path()).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

fn write_status(s: &Status) {
    let p = status_path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(b) = serde_json::to_vec_pretty(s) {
        let tmp = p.with_extension("tmp");
        if std::fs::write(&tmp, b).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }
}

/// One line for `c0mpute status`.
pub fn summary(s: &Status) -> String {
    let mut out = format!("{} ({})", s.state, if s.detail.is_empty() { "-" } else { &s.detail });
    out.push_str(&format!(
        "; {} rounds submitted, {} started, {} preempted, ~{} tokens trained",
        s.rounds_submitted, s.rounds_started, s.preemptions, s.tokens_trained_estimate
    ));
    if s.client_outdated {
        out.push_str("; CLIENT PIN OUTDATED: update c0mpute");
    }
    out
}

/// Fold one client log line into the status. `dt` is the time since the
/// previous training-progress line (for the token estimate).
pub fn apply_log_line(s: &mut Status, line: &str, since_last_train: Option<Duration>) {
    if let Some(rest) = line.split("[train] ").nth(1) {
        // "[train] step 12/340 loss=7.1234 tps=812 left=41m"
        let field = |k: &str| rest.split_whitespace().find_map(|w| w.strip_prefix(k)).and_then(|v| v.trim_end_matches('m').parse::<f64>().ok());
        if let Some(l) = field("loss=") {
            s.last_loss = Some(l);
        }
        if let Some(t) = field("tps=") {
            s.last_tokens_per_sec = Some(t);
            if let Some(dt) = since_last_train {
                s.tokens_trained_estimate += (t * dt.min(Duration::from_secs(30)).as_secs_f64()) as u64;
            }
        }
        s.detail = "training".into();
    } else if line.contains("Submitted successfully") {
        s.rounds_submitted += 1;
        s.last_submitted_at = Some(unix_now());
        s.detail = "delta submitted".into();
    } else if line.contains("Round lost") || line.contains("Not counted by server") {
        s.rounds_lost += 1;
        if line.contains("426") || line.contains("update required") {
            s.client_outdated = true;
        }
    } else if line.contains("no longer accepted") || line.contains("update the client") {
        s.client_outdated = true;
        s.detail = "client pin outdated".into();
    } else if line.contains("Pool matchmaking") {
        s.detail = "waiting for a pool of 3".into();
    } else if line.contains("Uploading") && line.contains("delta") {
        s.detail = "uploading delta".into();
    } else if let Some(n) = round_number(line) {
        let _ = n;
        s.rounds_started += 1;
        s.detail = "round started".into();
    }
}

/// "Round 3" / "Round 3/5" as the headless client logs a new round.
fn round_number(line: &str) -> Option<u64> {
    let rest = line.rsplit("Round ").next().filter(|_| line.contains("Round "))?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let tail = &rest[digits.len()..];
    if digits.is_empty() || !(tail.is_empty() || tail.starts_with('/')) {
        return None;
    }
    digits.parse().ok()
}

/// What an exit code from the client means.
pub fn explain_exit(code: Option<i32>) -> (&'static str, bool) {
    match code {
        Some(0) => ("client exited", false),
        Some(2) => ("CrowdGPT rejected the session: re-run `c0mpute crowdgpt on --token <token>` or `c0mpute crowdgpt register`", false),
        Some(3) => ("backend setup failed in the container", false),
        Some(5) => ("CrowdGPT's coordinator is unreachable", false),
        Some(6) => ("CrowdGPT rejected this hardware architecture", false),
        Some(7) => ("CrowdGPT's coordinator refused this client: the pinned client is probably outdated", true),
        Some(137) => ("killed (memory limit or SIGKILL)", false),
        Some(143) => ("stopped", false),
        _ => ("client exited unexpectedly", false),
    }
}

// ── readiness and setup ─────────────────────────────────────────────────

/// Why this node cannot donate right now, `None` when it can.
pub fn blocked_reason(cfg: &CrowdgptConfig) -> Option<String> {
    if !cfg.enabled {
        return Some("off (`c0mpute crowdgpt on` to donate idle compute)".into());
    }
    if crate::private::current().enabled && !cfg.allow_in_private {
        return Some("private mode is on (set [crowdgpt] allow_in_private = true to donate anyway)".into());
    }
    if crate::container::which().is_none() {
        return Some("no container runtime: install docker".into());
    }
    let image = runner_image();
    match image_commit(&image) {
        None => Some(format!("runner image {image} is missing: run `c0mpute crowdgpt setup`")),
        Some(c) if c != CROWDGPT_COMMIT => Some(format!("runner image is built from CrowdGPT {c}, this c0mpute pins {CROWDGPT_COMMIT}: run `c0mpute crowdgpt setup`")),
        Some(_) => None,
    }
}

/// The CrowdGPT commit an image was built from (its label), if present.
pub fn image_commit(image: &str) -> Option<String> {
    let out = crate::container::command_sync()
        .args(["image", "inspect", "--format", &format!("{{{{index .Config.Labels \"{COMMIT_LABEL}\"}}}}"), image])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let c = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !c.is_empty() && c != "<no value>").then_some(c)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SetupSource {
    Pulled,
    Built,
}

/// Put the runner image on this node as [`RUNNER_IMAGE`]: pull the pinned
/// build, else build it from CrowdGPT's repo at [`CROWDGPT_COMMIT`]. Either
/// way the image's commit label must match the pin.
pub async fn setup(build_only: bool, backend: &str, say: &(dyn Fn(&str) + Sync)) -> Result<SetupSource> {
    if crate::container::which().is_none() {
        bail!("no container runtime: install docker (or set C0MPUTE_DOCKER)");
    }
    if !build_only && backend != "cuda" {
        let r = pull_ref();
        say(&format!("pulling {r}"));
        let pulled = crate::container::command().args(["pull", &r]).status().await.map(|s| s.success()).unwrap_or(false);
        if pulled && image_commit(&r).as_deref() == Some(CROWDGPT_COMMIT) {
            crate::container::command().args(["tag", &r, RUNNER_IMAGE]).status().await?;
            return Ok(SetupSource::Pulled);
        }
        say("the registry did not serve the pinned image; building it from source");
    }
    let ctx = std::env::temp_dir().join(format!("c0mpute-crowdgpt-build-{}", std::process::id()));
    std::fs::create_dir_all(&ctx)?;
    std::fs::write(ctx.join("Dockerfile"), DOCKERFILE)?;
    std::fs::write(ctx.join("c0mpute-crowdgpt-run"), WATCHDOG)?;
    say(&format!("building {RUNNER_IMAGE} from {CROWDGPT_REPO} @ {} ({backend}; several minutes, ~2 GB)", &CROWDGPT_COMMIT[..12]));
    let status = crate::container::command().args(build_argv(&ctx, backend)).status().await.context("docker build")?;
    let _ = std::fs::remove_dir_all(&ctx);
    if !status.success() {
        bail!("docker build failed ({status})");
    }
    if image_commit(RUNNER_IMAGE).as_deref() != Some(CROWDGPT_COMMIT) {
        bail!("the built image does not carry the pinned commit label");
    }
    Ok(SetupSource::Built)
}

pub fn build_argv(ctx: &Path, backend: &str) -> Vec<String> {
    let mut a = vec!["build".to_string(), "-t".into(), RUNNER_IMAGE.into(), "--build-arg".into(), format!("CROWDGPT_COMMIT={CROWDGPT_COMMIT}")];
    if backend == "cuda" {
        a.extend([
            "--build-arg".into(),
            "BASE_IMAGE=pytorch/pytorch:2.6.0-cuda12.4-cudnn9-runtime".into(),
            "--build-arg".into(),
            "TORCH_INDEX=".into(),
        ]);
    }
    a.push(ctx.display().to_string());
    a
}

// ── governor ────────────────────────────────────────────────────────────

static DONATING: AtomicBool = AtomicBool::new(false);

/// Enabled and able to run: advertised as [`DONATE_TAG`] on the node's card.
pub fn donating() -> bool {
    DONATING.load(Ordering::Relaxed)
}

/// Pure decision for one tick, so it can be tested without docker.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    Nothing,
    Start,
    Preempt,
}

pub fn decide(running: bool, busy: bool, blocked: bool, idle_for: Duration, idle_needed: Duration, backoff_over: bool) -> Action {
    match (running, busy || blocked) {
        (true, true) => Action::Preempt,
        (true, false) => Action::Nothing,
        (false, true) => Action::Nothing,
        (false, false) if idle_for >= idle_needed && backoff_over => Action::Start,
        (false, false) => Action::Nothing,
    }
}

/// The uid:gid owning `dir`, which the container runs as (`None` off Unix).
#[cfg(unix)]
pub fn owner_of(dir: &Path) -> Option<(u32, u32)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(dir).ok().map(|m| (m.uid(), m.gid()))
}

#[cfg(not(unix))]
pub fn owner_of(_dir: &Path) -> Option<(u32, u32)> {
    None
}

/// Running crowdgpt containers (by [`LABEL`]).
pub async fn running_containers() -> Vec<String> {
    let out = crate::container::command().args(["ps", "-q", "--filter", &format!("label={LABEL}")]).stderr(std::process::Stdio::null()).output().await;
    out.map(|o| String::from_utf8_lossy(&o.stdout).split_whitespace().map(String::from).collect()).unwrap_or_default()
}

/// Stop one container: SIGTERM, SIGKILL after `grace`.
pub async fn stop_container(name: &str, grace: Duration) {
    let fut = crate::container::command().args(["stop", "-t", &grace.as_secs().to_string(), name]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
    if tokio::time::timeout(grace + Duration::from_secs(15), fut).await.is_err() {
        let _ = crate::container::command().args(["kill", name]).status().await;
    }
}

/// Stop every crowdgpt container on this node (`c0mpute crowdgpt off`).
pub async fn stop_all(grace: Duration) {
    for id in running_containers().await {
        stop_container(&id, grace).await;
    }
}

/// Run the governor for this worker's lifetime. `load` is the supervisor's
/// node-wide compute slots; `config_path` is re-read every few seconds so
/// `c0mpute crowdgpt on|off` applies without a restart.
pub fn spawn_governor(config_path: PathBuf, initial: CrowdgptConfig, load: Option<WorkerLoad>) {
    tokio::spawn(async move { governor(config_path, initial, load).await });
}

async fn governor(config_path: PathBuf, mut cfg: CrowdgptConfig, load: Option<WorkerLoad>) {
    let status = std::sync::Arc::new(Mutex::new(Status {
        state: "disabled".into(),
        crowdgpt_commit: CROWDGPT_COMMIT.into(),
        worker_name: worker_name(),
        ..read_status().unwrap_or_default()
    }));
    let set = |state: &str, detail: &str| {
        let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
        s.state = state.into();
        s.detail = detail.into();
        s.updated_at = unix_now();
        s.crowdgpt_commit = CROWDGPT_COMMIT.into();
        write_status(&s);
    };
    // A container left by a previous worker run is not ours to trust.
    if crate::container::which().is_some() {
        for id in running_containers().await {
            let _ = crate::container::command().args(["rm", "-f", &id]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().await;
        }
    }
    let mut runs: u64 = 0;
    let mut name = String::new();
    let mut child: Option<tokio::process::Child> = None;
    let mut last_busy = Instant::now();
    let mut backoff_until = Instant::now();
    let mut backoff = Duration::from_secs(60);
    let mut last_reload = Instant::now();
    let mut blocked: Option<String> = blocked_reason(&cfg);
    let mut last_blocked_check = Instant::now();
    let mut started_at = Instant::now();
    let ram_gb = total_ram_gb();
    loop {
        tokio::time::sleep(TICK).await;
        if last_reload.elapsed() >= Duration::from_secs(10) {
            last_reload = Instant::now();
            if let Ok(c) = crate::Config::load_or_default(&config_path) {
                if c.crowdgpt != cfg {
                    info!(enabled = c.crowdgpt.enabled, "crowdgpt: config changed");
                    cfg = c.crowdgpt;
                    blocked = blocked_reason(&cfg);
                    last_blocked_check = Instant::now();
                }
            }
        }
        if last_blocked_check.elapsed() >= Duration::from_secs(60) {
            last_blocked_check = Instant::now();
            blocked = blocked_reason(&cfg);
        }
        let busy = crate::activity::busy() || load.as_ref().is_some_and(|l| l.free_slots() < l.total_slots());
        if busy {
            last_busy = Instant::now();
        }
        DONATING.store(cfg.enabled && blocked.is_none(), Ordering::Relaxed);

        // Did the client exit on its own?
        if let Some(c) = child.as_mut()
            && let Ok(Some(st)) = c.try_wait()
        {
            child = None;
            let (why, outdated) = explain_exit(st.code());
            let ran = started_at.elapsed();
            {
                let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
                s.last_exit = Some(format!("{why} ({st})"));
                s.client_outdated |= outdated;
            }
            // Back off harder on quick failures; an outdated pin waits long.
            backoff = if outdated { Duration::from_secs(6 * 3600) } else if ran < Duration::from_secs(300) { (backoff * 2).min(Duration::from_secs(1800)) } else { Duration::from_secs(60) };
            backoff_until = Instant::now() + backoff;
            warn!(%why, ?st, ?backoff, "crowdgpt: client exited");
            set("backoff", why);
        }

        let running = child.is_some();
        if running {
            let _ = std::fs::File::create(heartbeat_path());
        }
        match decide(running, busy, blocked.is_some(), last_busy.elapsed(), Duration::from_secs(cfg.idle_secs), Instant::now() >= backoff_until) {
            Action::Preempt => {
                let why = if busy { "paid work arrived" } else { blocked.as_deref().unwrap_or("disabled") };
                info!(why, "crowdgpt: stopping the client");
                set("stopping", why);
                let t = Instant::now();
                stop_container(&name, Duration::from_secs(cfg.stop_grace_secs)).await;
                if busy {
                    status.lock().unwrap_or_else(|e| e.into_inner()).preemptions += 1;
                }
                set(if busy { "waiting-idle" } else { "disabled" }, if busy { "preempted by paid work" } else { why });
                info!(took_ms = t.elapsed().as_millis() as u64, "crowdgpt: client stopped");
                if let Some(mut c) = child.take() {
                    let _ = tokio::time::timeout(Duration::from_secs(10), c.wait()).await;
                    let _ = c.start_kill();
                }
            }
            Action::Start => {
                let cache = cache_dir();
                let _ = std::fs::create_dir_all(cache.join("home"));
                let _ = std::fs::File::create(heartbeat_path());
                runs += 1;
                name = format!("{CONTAINER}-{}-{runs}", std::process::id());
                let argv = run_argv(&cfg, &name, &cache, owner_of(&cache), ram_gb);
                info!(image = %runner_image(), commit = &CROWDGPT_COMMIT[..12], "crowdgpt: node idle, starting the client");
                match crate::container::command().args(&argv).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).kill_on_drop(true).spawn() {
                    Ok(mut c) => {
                        for pipe in [c.stdout.take().map(|p| Box::new(p) as Box<dyn tokio::io::AsyncRead + Unpin + Send>), c.stderr.take().map(|p| Box::new(p) as Box<dyn tokio::io::AsyncRead + Unpin + Send>)].into_iter().flatten() {
                            let status = status.clone();
                            tokio::spawn(async move {
                                let mut lines = BufReader::new(pipe).lines();
                                let mut last_train: Option<Instant> = None;
                                while let Ok(Some(line)) = lines.next_line().await {
                                    let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
                                    let is_train = line.contains("[train] ");
                                    apply_log_line(&mut s, &line, last_train.map(|t| t.elapsed()));
                                    if is_train {
                                        last_train = Some(Instant::now());
                                    }
                                    s.updated_at = unix_now();
                                    write_status(&s);
                                }
                            });
                        }
                        started_at = Instant::now();
                        child = Some(c);
                        set("running", "starting");
                    }
                    Err(e) => {
                        backoff_until = Instant::now() + Duration::from_secs(300);
                        set("backoff", &format!("docker run failed: {e}"));
                    }
                }
            }
            Action::Nothing => {
                let s = status.lock().unwrap_or_else(|e| e.into_inner()).state.clone();
                if !running {
                    let (state, detail) = match &blocked {
                        Some(b) if !cfg.enabled => ("disabled", b.clone()),
                        Some(b) => ("blocked", b.clone()),
                        None if Instant::now() < backoff_until => ("backoff", status.lock().unwrap_or_else(|e| e.into_inner()).detail.clone()),
                        None if busy => ("waiting-idle", "node is doing paid work".to_string()),
                        None => ("waiting-idle", format!("idle {}s of {}s", last_busy.elapsed().as_secs(), cfg.idle_secs)),
                    };
                    if s != state || state == "waiting-idle" {
                        set(state, &detail);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_by_default() {
        let c = CrowdgptConfig::default();
        assert!(!c.enabled && !c.allow_in_private);
        assert!(blocked_reason(&c).unwrap().starts_with("off"));
        assert_eq!(crate::Config::default().crowdgpt, c);
    }

    #[test]
    fn the_governor_only_runs_on_an_idle_node_and_stops_at_once() {
        let need = Duration::from_secs(120);
        // Idle long enough: start.
        assert_eq!(decide(false, false, false, Duration::from_secs(121), need, true), Action::Start);
        // Not idle long enough, busy, blocked or backing off: wait.
        assert_eq!(decide(false, false, false, Duration::from_secs(30), need, true), Action::Nothing);
        assert_eq!(decide(false, true, false, Duration::from_secs(999), need, true), Action::Nothing);
        assert_eq!(decide(false, false, true, Duration::from_secs(999), need, true), Action::Nothing);
        assert_eq!(decide(false, false, false, Duration::from_secs(999), need, false), Action::Nothing);
        // Running: paid work (or being turned off) preempts immediately.
        assert_eq!(decide(true, true, false, Duration::ZERO, need, true), Action::Preempt);
        assert_eq!(decide(true, false, true, Duration::ZERO, need, true), Action::Preempt);
        assert_eq!(decide(true, false, false, Duration::from_secs(5), need, true), Action::Nothing);
    }

    #[test]
    fn a_bid_makes_the_node_busy_for_the_governor() {
        crate::activity::note_bid(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64 + 30_000);
        assert!(crate::activity::busy());
        assert_eq!(decide(true, crate::activity::busy(), false, Duration::ZERO, Duration::from_secs(120), true), Action::Preempt);
    }

    #[test]
    fn run_argv_is_contained() {
        let c = CrowdgptConfig { enabled: true, cpus: 2.0, memory_gb: 6, ..Default::default() };
        let a = run_argv(&c, "c0mpute-crowdgpt-1", Path::new("/var/lib/c0/crowdgpt/cache"), Some((1000, 1000)), 8);
        let s = a.join(" ");
        for want in [
            "--rm", "--init", "--read-only", "--cap-drop ALL", "no-new-privileges", "--network bridge", "--cpu-shares 2",
            "--oom-score-adj 1000", "--memory 6g", "--memory-swap 6g", "--user 1000:1000", "CROWDGPT_ENGRAM=0",
            "-v /var/lib/c0/crowdgpt/cache:/data",
        ] {
            assert!(s.contains(want), "missing {want}: {s}");
        }
        assert!(!s.contains("--network host") && !s.contains("--privileged") && !s.contains("--gpus"));
        // Exactly one host mount, and the image last.
        assert_eq!(a.iter().filter(|x| *x == "-v").count(), 1);
        assert_eq!(a.last().unwrap(), &runner_image());
        // Engram: off under 16 GB, on at 16+, operator override wins.
        assert!(run_argv(&c, "c0mpute-crowdgpt-1", Path::new("/c"), None, 32).join(" ").contains("CROWDGPT_ENGRAM=1"));
        let forced = CrowdgptConfig { engram: Some(false), ..c.clone() };
        assert!(run_argv(&forced, "n", Path::new("/c"), None, 64).join(" ").contains("CROWDGPT_ENGRAM=0"));
        let cuda = CrowdgptConfig { backend: "cuda".into(), ..c };
        assert!(run_argv(&cuda, "n", Path::new("/c"), None, 8).join(" ").contains("--gpus all"));
        // Memory default: a third of RAM, 4..=16 GB.
        assert_eq!(CrowdgptConfig::default().effective_memory_gb(8), 4);
        assert_eq!(CrowdgptConfig::default().effective_memory_gb(30), 10);
        assert_eq!(CrowdgptConfig::default().effective_memory_gb(256), 16);
    }

    #[test]
    fn client_log_lines_become_status() {
        let mut s = Status::default();
        let lines = [
            "2026-11-09 10:00:00 INFO Round 1",
            "2026-11-09 10:00:01 INFO Pool matchmaking 1/3 (waiting for peers)",
            "2026-11-09 10:00:05 INFO [train] step 10/340 loss=7.1234 tps=800 left=40m",
            "2026-11-09 10:00:15 INFO [train] step 20/340 loss=6.9000 tps=820 left=39m",
            "2026-11-09 10:59:00 INFO ⬆️ Uploading 1830.2 MB delta to server (in background)...",
            "2026-11-09 11:01:00 INFO ✅ Submitted successfully!",
            "2026-11-09 11:01:10 INFO Round 2/5",
            "2026-11-09 11:30:00 INFO ❌ Round lost — client update required (HTTP 426): update the client",
        ];
        for (i, l) in lines.iter().enumerate() {
            apply_log_line(&mut s, l, (i == 3).then_some(Duration::from_secs(10)));
        }
        assert_eq!((s.rounds_started, s.rounds_submitted, s.rounds_lost), (2, 1, 1));
        assert_eq!(s.last_loss, Some(6.9));
        assert_eq!(s.last_tokens_per_sec, Some(820.0));
        assert_eq!(s.tokens_trained_estimate, 8200);
        assert!(s.client_outdated);
        assert!(summary(&s).contains("1 rounds submitted") && summary(&s).contains("OUTDATED"));
        // Words that merely contain "Round" do not count as a round.
        let mut t = Status::default();
        apply_log_line(&mut t, "Round failed: boom", None);
        apply_log_line(&mut t, "Starting Round Cycle #3", None);
        assert_eq!(t.rounds_started, 0);
    }

    #[test]
    fn exit_codes_are_explained() {
        assert!(explain_exit(Some(7)).1, "7 = refused: outdated pin");
        assert!(!explain_exit(Some(143)).1);
        assert!(explain_exit(Some(2)).0.contains("token"));
    }

    #[test]
    fn the_runner_files_pin_the_commit() {
        assert!(DOCKERFILE.contains(&format!("ARG CROWDGPT_COMMIT={CROWDGPT_COMMIT}")), "Dockerfile default must match CROWDGPT_COMMIT");
        assert!(DOCKERFILE.contains(COMMIT_LABEL));
        assert!(WATCHDOG.contains("--no-update") && WATCHDOG.contains("c0mpute-heartbeat"));
        assert_eq!(CROWDGPT_COMMIT.len(), 40);
        assert!(pull_ref().ends_with(&CROWDGPT_COMMIT[..12]));
    }
}
