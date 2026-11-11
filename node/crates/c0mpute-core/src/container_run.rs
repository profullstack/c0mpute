//! `container.run` (DIP 0033): build a Dockerfile, or run a command in a
//! digest-pinned image, on another machine, and get the result back. No git:
//! the input is a plain tarball of any directory.
//!
//! - **build**: the buyer sends a build context (tar or tar.gz) plus the
//!   Dockerfile path, build args and target. The worker runs `docker buildx
//!   build` with the daemon's own BuildKit (no privileged builder), then
//!   returns the image as `docker save | gzip` with its image id.
//! - **run**: `cmd` runs in a digest-pinned `image` (or the image the same
//!   job just built). An optional input tarball is copied into the
//!   container's workdir with `docker cp` (no host mounts); the paths named in
//!   `outputs` are copied back out as tar.gz files.
//!
//! Every blob (context, input, image, outputs) moves sealed to the job's keys
//! (DIP-0032) in content-addressed pieces, like render frames, and the buyer
//! checks each one's size and hash after opening it.
//!
//! Trust runs both ways, so the role is opt-in and closed by default: a
//! `containers` worker builds only for buyers it trusts (identities on this
//! machine plus `[containers] buyers`, unless `open = true`), and the CLI
//! sends builds only to builders the buyer names (`--peer`, `[containers]
//! builders`, `C0MPUTE_BUILDERS`) unless told `--any-worker`. A builder
//! controls the bytes of the image it returns; see the DIP for the
//! supply-chain notes and the N-of-M reproducible-digest hook
//! ([`verify_reproducible`]).

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use c0mpute_net::topics::{JobOffer, JobStatus};
use c0mpute_net::{GossipMessage, Libp2pNetwork, PeerId, RpcHandler};
use c0mpute_proto::Hash;
use c0mpute_store::ChunkStore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::pieces::{INPUT_PIECE_BYTES, InputRef};
use crate::runner::{AcceptedJob, WorkerLoad, publish_receipt};

pub const WORKLOAD: &str = "container.run";
/// Capability tag: docker with BuildKit answers on this node and the
/// `containers` role is on.
pub const ROLE_TAG: &str = "c0mpute:role:containers";
/// Largest build context or input tarball.
pub const MAX_INPUT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Largest image (`docker save | gzip`) a worker returns.
pub const MAX_IMAGE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Largest single output (tar.gz of one path).
pub const MAX_OUTPUT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Most output paths per job.
pub const MAX_OUTPUTS: usize = 16;
/// Cap on the result JSON (metadata and log tails; blobs are separate).
pub const MAX_RESULT_BYTES: usize = 1024 * 1024;
/// Log bytes kept per stage in the result.
pub const LOG_TAIL_BYTES: usize = 16 * 1024;
/// Most of a job's live log a worker keeps for its buyer to stream.
const LIVE_LOG_BYTES: usize = 512 * 1024;
/// Limits a spec gets when it names none, and the most it may ask.
pub const DEFAULT_CPUS: f64 = 2.0;
pub const DEFAULT_MEMORY_MB: u64 = 4096;
pub const DEFAULT_DISK_MB: u64 = 20 * 1024;
pub const DEFAULT_TIMEOUT_SECS: u64 = 1800;
pub const MAX_CPUS: f64 = 64.0;
pub const MAX_MEMORY_MB: u64 = 256 * 1024;
pub const MAX_DISK_MB: u64 = 1024 * 1024;
pub const MAX_TIMEOUT_SECS: u64 = 6 * 3600;
/// How long a worker keeps a job's sealed result pieces for its buyer.
const RESULT_KEEP: Duration = Duration::from_secs(6 * 3600);
/// Name of the bridge network run steps use, and its Linux interface.
pub const JOBS_NETWORK: &str = "c0mpute-jobs";
pub const JOBS_BRIDGE: &str = "c0jobs0";
/// Live log service: a buyer streams its own job's log from the worker.
pub const LOG_SERVICE: &str = "c0mpute.container.log.v1";

// ─── config ──────────────────────────────────────────────────────────────

/// `[containers]` in config.toml: the `containers` role (worker side) and the
/// builders this machine sends `c0mpute build` / `c0mpute run` to (buyer side).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ContainersConfig {
    /// Where per-job scratch (context, outputs, the saved image) goes.
    /// Default `<data dir>/containers`. Put it on the big disk.
    pub work_dir: Option<PathBuf>,
    /// Jobs at once.
    pub max_jobs: usize,
    /// Most CPUs / memory one run step may ask for (0 = half the machine).
    pub max_cpus: f64,
    pub max_memory_mb: u64,
    /// Most scratch disk one job may ask for, GiB.
    pub max_disk_gb: u64,
    pub max_timeout_secs: u64,
    /// Free space kept on the scratch and docker disks, GiB; a job that
    /// would go under it is refused or stopped (0 = min(20 GiB, 10%)).
    pub min_free_gb: u64,
    /// BuildKit cache kept between builds, GiB (pruned after every build).
    pub cache_gb: u64,
    /// Buyers (peer ids) trusted to run jobs here, besides the identities on
    /// this machine.
    pub buyers: Vec<String>,
    /// Serve any buyer. Off by default: a build runs the buyer's Dockerfile.
    pub open: bool,
    /// Drop run-step traffic to private, loopback, link-local (metadata) and
    /// CGNAT ranges with iptables (needs root; reported per job).
    pub egress_guard: bool,
    /// Also guard this bridge (e.g. "docker0"), the one BuildKit's RUN steps
    /// use. It is shared with every default-network container on the host,
    /// so it is opt-in.
    pub guard_build_bridge: Option<String>,
    /// Buyer side: builders (peer ids) this machine sends jobs to.
    pub builders: Vec<String>,
}

impl Default for ContainersConfig {
    fn default() -> Self {
        Self {
            work_dir: None,
            max_jobs: 1,
            max_cpus: 0.0,
            max_memory_mb: 0,
            max_disk_gb: 100,
            max_timeout_secs: 3 * 3600,
            min_free_gb: 0,
            cache_gb: 20,
            buyers: Vec::new(),
            open: false,
            egress_guard: true,
            guard_build_bridge: None,
            builders: Vec::new(),
        }
    }
}

impl ContainersConfig {
    pub fn work_dir(&self) -> PathBuf {
        self.work_dir.clone().unwrap_or_else(|| {
            crate::config::data_dir()
                .unwrap_or_else(|| PathBuf::from("./c0mpute-data"))
                .join("containers")
        })
    }

    pub fn cpu_cap(&self) -> f64 {
        if self.max_cpus > 0.0 {
            self.max_cpus
        } else {
            let n = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(2);
            ((n / 2).max(1)) as f64
        }
    }

    pub fn memory_cap_mb(&self) -> u64 {
        if self.max_memory_mb > 0 {
            return self.max_memory_mb;
        }
        let mut sys = sysinfo::System::new();
        sys.refresh_memory();
        (sys.total_memory() / 1024 / 1024 / 2).max(512)
    }

    /// Builders from config plus `C0MPUTE_BUILDERS` (comma-separated).
    pub fn builders_with_env(&self) -> Vec<String> {
        let mut out: Vec<String> = self.builders.clone();
        if let Ok(v) = std::env::var("C0MPUTE_BUILDERS") {
            out.extend(crate::jobs_gateway::parse_allow_peers(&v));
        }
        let mut seen = BTreeSet::new();
        out.retain(|p| seen.insert(p.clone()));
        out
    }
}

/// Free space to keep on a disk of `total` bytes.
pub fn min_free_bytes(cfg: &ContainersConfig, total: u64) -> u64 {
    if cfg.min_free_gb > 0 {
        return cfg.min_free_gb.saturating_mul(1 << 30);
    }
    (20u64 << 30).min(total / 10)
}

// ─── buyer trust (worker side) ───────────────────────────────────────────

/// Who may hire this node for `container.run`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BuyerPolicy {
    pub open: bool,
    pub trusted: BTreeSet<String>,
}

impl BuyerPolicy {
    pub fn from_config(cfg: &ContainersConfig, local: impl IntoIterator<Item = String>) -> Self {
        let mut trusted: BTreeSet<String> = cfg
            .buyers
            .iter()
            .map(|b| b.trim().to_string())
            .filter(|b| !b.is_empty())
            .collect();
        trusted.extend(local);
        Self {
            open: cfg.open,
            trusted,
        }
    }

    /// The signed gossip publisher must be the offer's buyer, and that buyer
    /// trusted (or this node), unless the node is open.
    pub fn admits(&self, msg: &GossipMessage, offer: &JobOffer, our_peer_id: &str) -> bool {
        if self.open {
            return true;
        }
        if !crate::buyer::source_matches_claim(msg, &offer.buyer_peer_id) {
            return false;
        }
        offer.buyer_peer_id == our_peer_id || self.trusted.contains(&offer.buyer_peer_id)
    }
}

static POLICY: RwLock<Option<BuyerPolicy>> = RwLock::new(None);

/// Install the process-wide buyer policy (at worker start).
pub fn install_policy(policy: BuyerPolicy) {
    if let Ok(mut p) = POLICY.write() {
        *p = Some(policy);
    }
}

/// The dispatch gate: closed (own identities only) until a policy is installed.
pub fn admits_buyer(msg: &GossipMessage, offer: &JobOffer, our_peer_id: &str) -> bool {
    let policy = POLICY
        .read()
        .ok()
        .and_then(|p| p.clone())
        .unwrap_or_else(|| {
            BuyerPolicy::from_config(
                &ContainersConfig::default(),
                crate::private::local_identities(),
            )
        });
    policy.admits(msg, offer, our_peer_id)
}

// ─── spec ────────────────────────────────────────────────────────────────

/// `build`: what to build from the context.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BuildSpec {
    #[serde(default = "default_dockerfile")]
    pub dockerfile: String,
    #[serde(default)]
    pub build_args: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

fn default_dockerfile() -> String {
    "Dockerfile".into()
}

/// The whole spec (`spec_inline`, sealed to the winner). Blob references use
/// the [`crate::pieces`] field names under the `context` and `input`
/// prefixes (`context_hash`, `context_size`, `context_pieces`,
/// `context_key`, `context_provider_peer_id`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Spec {
    pub v: u32,
    /// `build` (return the image) or `run` (run `cmd`, return outputs).
    pub mode: String,
    #[serde(default)]
    pub build: Option<BuildSpec>,
    /// Digest-pinned image for run mode (`name@sha256:<64 hex>`).
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub cmd: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub workdir: Option<String>,
    #[serde(default)]
    pub outputs: Vec<String>,
    /// Run mode with `build`: also return the built image.
    #[serde(default)]
    pub return_image: bool,
    #[serde(default)]
    pub cpus: Option<f64>,
    #[serde(default)]
    pub memory_mb: Option<u64>,
    #[serde(default)]
    pub disk_mb: Option<u64>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Network for builds and run steps (default on; public addresses only
    /// where the worker's egress guard is up).
    #[serde(default)]
    pub network: Option<bool>,
    #[serde(default)]
    pub context_hash: Option<String>,
    #[serde(default)]
    pub context_size: Option<u64>,
    #[serde(default)]
    pub context_pieces: Option<Vec<String>>,
    #[serde(default)]
    pub context_key: Option<String>,
    #[serde(default)]
    pub context_provider_peer_id: Option<String>,
    #[serde(default)]
    pub input_hash: Option<String>,
    #[serde(default)]
    pub input_size: Option<u64>,
    #[serde(default)]
    pub input_pieces: Option<Vec<String>>,
    #[serde(default)]
    pub input_key: Option<String>,
    #[serde(default)]
    pub input_provider_peer_id: Option<String>,
}

/// A blob the job reads: where it is, how it is sealed, who holds it.
#[derive(Clone, Debug)]
pub struct BlobRef {
    pub at: InputRef,
    pub key: Option<c0mpute_net::topics::JobKey>,
    pub provider: Option<PeerId>,
}

/// A spec after its checks.
#[derive(Clone, Debug)]
pub struct Parsed {
    pub spec: Spec,
    pub build: Option<BuildSpec>,
    pub image: Option<String>,
    pub cpus: f64,
    pub memory_mb: u64,
    pub disk_mb: u64,
    pub timeout: Duration,
    pub network: bool,
    pub workdir: String,
    pub context: Option<BlobRef>,
    pub input: Option<BlobRef>,
}

impl Parsed {
    pub fn is_build(&self) -> bool {
        self.spec.mode == "build"
    }
    /// The job returns the built image.
    pub fn returns_image(&self) -> bool {
        self.build.is_some() && (self.is_build() || self.spec.return_image)
    }
}

fn valid_env_key(k: &str) -> bool {
    !k.is_empty()
        && k.len() <= 128
        && k.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn check_map(what: &str, m: &BTreeMap<String, String>) -> std::result::Result<(), String> {
    if m.len() > 128 {
        return Err(format!("{what} holds at most 128 entries"));
    }
    for (k, v) in m {
        if !valid_env_key(k) {
            return Err(format!(
                "{what} key {k:?} must be letters, digits and _ (not starting with a digit)"
            ));
        }
        if v.len() > 8192 || v.contains('\0') {
            return Err(format!(
                "{what} value for {k} is over 8192 bytes or holds a NUL"
            ));
        }
    }
    Ok(())
}

/// A relative path inside the context or workdir: no `..`, no NUL, does not
/// start with `-`.
fn safe_rel_path(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= 512
        && !p.starts_with('-')
        && !p.contains('\0')
        && !p.starts_with('/')
        && !p.split('/').any(|c| c == "..")
}

/// A container path: absolute or relative to the workdir, no `..`.
fn safe_container_path(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= 512
        && !p.contains('\0')
        && !p.starts_with('-')
        && !p.split('/').any(|c| c == "..")
}

/// `name[:tag]@sha256:<64 hex>`.
pub fn valid_pinned_image(r: &str) -> bool {
    let Some((name, digest)) = r.split_once("@sha256:") else {
        return false;
    };
    !name.is_empty()
        && r.len() <= 256
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "./:_-".contains(c))
        && digest.len() == 64
        && digest
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

fn blob_ref(
    what: &str,
    hash: Option<&String>,
    size: Option<u64>,
    pieces: Option<&Vec<String>>,
    key: Option<&String>,
    provider: Option<&String>,
) -> std::result::Result<Option<BlobRef>, String> {
    let Some(hash) = hash else {
        if size.is_some() || pieces.is_some() || key.is_some() {
            return Err(format!("{what}_size/_pieces/_key need {what}_hash"));
        }
        return Ok(None);
    };
    let limit = crate::pieces::input_limit(MAX_INPUT_BYTES as usize, key.is_some());
    let at = InputRef::parse(what, hash, size, pieces.map(Vec::as_slice), limit)?;
    let key = crate::pieces::input_key(key.map(String::as_str))
        .map_err(|_| format!("{what}_key must be a hex key"))?;
    let provider = provider
        .map(|p| p.parse::<PeerId>())
        .transpose()
        .map_err(|_| format!("{what}_provider_peer_id must be a peer id"))?;
    Ok(Some(BlobRef { at, key, provider }))
}

/// Check a spec. Errors are for the requester (`ok: false` with the reason).
pub fn parse_spec(value: &Value) -> std::result::Result<Parsed, String> {
    if !value.is_object() {
        return Err("spec must be a JSON object".into());
    }
    let spec: Spec = serde_json::from_value(value.clone())
        .map_err(|e| format!("invalid container.run spec: {e}"))?;
    if spec.v != 1 {
        return Err(format!(
            "unsupported spec version {} (this worker speaks 1)",
            spec.v
        ));
    }
    let build = spec.build.clone();
    let image = spec.image.clone();
    match spec.mode.as_str() {
        "build" => {
            if build.is_none() {
                return Err("mode build needs `build`".into());
            }
            if image.is_some() || !spec.cmd.is_empty() || !spec.outputs.is_empty() {
                return Err("mode build takes no image, cmd or outputs (use mode run)".into());
            }
        }
        "run" => {
            if spec.cmd.is_empty() {
                return Err("mode run needs `cmd`".into());
            }
            match (&build, &image) {
                (Some(_), Some(_)) => return Err("give `build` or `image`, not both".into()),
                (None, None) => return Err("mode run needs an `image` or a `build`".into()),
                _ => {}
            }
        }
        other => return Err(format!("unknown mode {other:?} (build or run)")),
    }
    if let Some(b) = &build {
        if !safe_rel_path(&b.dockerfile) {
            return Err("build.dockerfile must be a relative path inside the context".into());
        }
        check_map("build.build_args", &b.build_args)?;
        if let Some(t) = &b.target
            && (t.is_empty()
                || t.len() > 128
                || !t
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
                || t.starts_with('-'))
        {
            return Err("build.target must be 1-128 of letters, digits, _ . -".into());
        }
    }
    if let Some(i) = &image
        && !valid_pinned_image(i)
    {
        return Err("image must be pinned by digest: name@sha256:<64 hex>".into());
    }
    if spec.cmd.len() > 256 || spec.cmd.iter().any(|a| a.len() > 8192 || a.contains('\0')) {
        return Err("cmd holds at most 256 arguments of 8192 bytes, no NUL".into());
    }
    if spec.cmd.first().is_some_and(|c| c.is_empty()) {
        return Err("cmd[0] must not be empty".into());
    }
    check_map("env", &spec.env)?;
    let workdir = spec.workdir.clone().unwrap_or_else(|| "/work".into());
    if !workdir.starts_with('/') || !safe_container_path(&workdir) {
        return Err("workdir must be an absolute path without ..".into());
    }
    if spec.outputs.len() > MAX_OUTPUTS || spec.outputs.iter().any(|p| !safe_container_path(p)) {
        return Err(format!(
            "outputs: at most {MAX_OUTPUTS} paths, no .., not starting with -"
        ));
    }
    let cpus = spec.cpus.unwrap_or(DEFAULT_CPUS);
    if !cpus.is_finite() || !(0.1..=MAX_CPUS).contains(&cpus) {
        return Err(format!("cpus must be between 0.1 and {MAX_CPUS}"));
    }
    let memory_mb = spec.memory_mb.unwrap_or(DEFAULT_MEMORY_MB);
    if !(64..=MAX_MEMORY_MB).contains(&memory_mb) {
        return Err(format!("memory_mb must be between 64 and {MAX_MEMORY_MB}"));
    }
    let disk_mb = spec.disk_mb.unwrap_or(DEFAULT_DISK_MB);
    if !(64..=MAX_DISK_MB).contains(&disk_mb) {
        return Err(format!("disk_mb must be between 64 and {MAX_DISK_MB}"));
    }
    let timeout_secs = spec.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
    if !(10..=MAX_TIMEOUT_SECS).contains(&timeout_secs) {
        return Err(format!(
            "timeout_secs must be between 10 and {MAX_TIMEOUT_SECS}"
        ));
    }
    let context = blob_ref(
        "context",
        spec.context_hash.as_ref(),
        spec.context_size,
        spec.context_pieces.as_ref(),
        spec.context_key.as_ref(),
        spec.context_provider_peer_id.as_ref(),
    )?;
    let input = blob_ref(
        "input",
        spec.input_hash.as_ref(),
        spec.input_size,
        spec.input_pieces.as_ref(),
        spec.input_key.as_ref(),
        spec.input_provider_peer_id.as_ref(),
    )?;
    if build.is_some() && context.is_none() {
        return Err("a build needs a context (context_hash)".into());
    }
    if build.is_none() && context.is_some() {
        return Err("context_hash is only for builds; a run step's files go in input_hash".into());
    }
    Ok(Parsed {
        network: spec.network.unwrap_or(true),
        spec,
        build,
        image,
        cpus,
        memory_mb,
        disk_mb,
        timeout: Duration::from_secs(timeout_secs),
        workdir,
        context,
        input,
    })
}

/// Spec keys that go in the public (gossiped) view: the mode and the limits.
/// Everything else (Dockerfile path, args, env, cmd, image, blobs) is sealed.
pub const PUBLIC_KEYS: &[&str] = &[
    "v",
    "mode",
    "cpus",
    "memory_mb",
    "disk_mb",
    "timeout_secs",
    "network",
];

// ─── runtime probe ───────────────────────────────────────────────────────

/// What the node found at start: docker, BuildKit (buildx), the data root.
#[derive(Clone, Debug, Default)]
pub struct Runtime {
    pub docker_version: Option<String>,
    pub buildx: bool,
    pub docker_root: Option<PathBuf>,
    pub reason: Option<String>,
}

impl Runtime {
    pub fn ready(&self) -> bool {
        self.docker_version.is_some() && self.buildx
    }
}

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

fn run_quiet(args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = crate::container::command_sync()
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .spawn()
        .ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = String::new();
                if let Some(mut s) = child.stdout.take() {
                    use std::io::Read;
                    let _ = s.read_to_string(&mut out);
                }
                return status.success().then(|| out.trim().to_string());
            }
            Ok(None) if started.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(_) => return None,
        }
    }
}

fn probe() -> Runtime {
    let t = Duration::from_secs(20);
    let Some(version) =
        run_quiet(&["info", "--format", "{{.ServerVersion}}"], t).filter(|v| !v.is_empty())
    else {
        return Runtime {
            reason: Some(
                "docker is not answering (install docker, and let this user reach the daemon)"
                    .into(),
            ),
            ..Default::default()
        };
    };
    let buildx = run_quiet(&["buildx", "version"], t).is_some();
    let docker_root = run_quiet(&["info", "--format", "{{.DockerRootDir}}"], t)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    Runtime {
        reason: (!buildx)
            .then(|| "docker buildx is missing (install the docker-buildx plugin)".into()),
        docker_version: Some(version),
        buildx,
        docker_root,
    }
}

/// Probe once (blocking: call from `spawn_blocking`).
pub fn runtime() -> &'static Runtime {
    RUNTIME.get_or_init(probe)
}

/// The probe result, if it has run.
pub fn probed() -> Option<&'static Runtime> {
    RUNTIME.get()
}

static WORKER_CFG: OnceLock<ContainersConfig> = OnceLock::new();

/// Free bytes on the scratch disk and the docker disk (the smaller).
fn free_now(cfg: &ContainersConfig) -> (u64, u64) {
    let mut free = u64::MAX;
    let mut floor = 0u64;
    let mut dirs = vec![cfg.work_dir()];
    if let Some(root) = probed().and_then(|r| r.docker_root.clone()) {
        dirs.push(root);
    }
    for d in dirs {
        let (f, total) = c0mpute_store::free_space_at(&d);
        if f < free {
            free = f;
            floor = min_free_bytes(cfg, total);
        }
    }
    (free, floor)
}

/// Can this node take a job with these public limits now?
pub fn can_serve(cfg: &ContainersConfig, public: &Value) -> bool {
    let Some(rt) = probed() else { return false };
    if !rt.ready() {
        return false;
    }
    let num = |k: &str| public.get(k).and_then(Value::as_f64);
    let cpus = num("cpus").unwrap_or(DEFAULT_CPUS);
    let mem = num("memory_mb").unwrap_or(DEFAULT_MEMORY_MB as f64);
    let disk = num("disk_mb").unwrap_or(DEFAULT_DISK_MB as f64);
    let timeout = num("timeout_secs").unwrap_or(DEFAULT_TIMEOUT_SECS as f64);
    if cpus > cfg.cpu_cap() || mem > cfg.memory_cap_mb() as f64 {
        return false;
    }
    if disk > (cfg.max_disk_gb * 1024) as f64 || timeout > cfg.max_timeout_secs as f64 {
        return false;
    }
    if public
        .get("v")
        .and_then(Value::as_u64)
        .is_some_and(|v| v != 1)
    {
        return false;
    }
    let (free, floor) = free_now(cfg);
    free > floor.saturating_add((disk as u64).saturating_mul(1 << 20))
}

/// Bid filter: within this node's limits and disk headroom.
pub fn offer_filter(cfg: ContainersConfig) -> crate::dispatch::OfferFilter {
    Arc::new(
        move |offer: &JobOffer| match crate::job_e2e::bid_spec(offer) {
            Some(crate::job_e2e::BidSpec::Public(v)) => can_serve(&cfg, v),
            Some(crate::job_e2e::BidSpec::Full(spec)) => {
                parse_spec(spec).is_ok()
                    && can_serve(&cfg, &crate::job_e2e::public_view(WORKLOAD, spec))
            }
            None => false,
        },
    )
}

/// The load a `containers` subscriber uses: its own slots (`max_jobs`).
pub fn worker_load(cfg: &ContainersConfig) -> WorkerLoad {
    let n = cfg.max_jobs.max(1);
    WorkerLoad::new(Arc::new(tokio::sync::Semaphore::new(n)), n)
}

// ─── live log ────────────────────────────────────────────────────────────

struct LiveLog {
    buyer: String,
    stage: String,
    /// Absolute offset of `buf[0]`.
    start: u64,
    buf: VecDeque<u8>,
    done: bool,
    at: Instant,
}

fn live_logs() -> &'static Mutex<HashMap<String, LiveLog>> {
    static L: OnceLock<Mutex<HashMap<String, LiveLog>>> = OnceLock::new();
    L.get_or_init(Default::default)
}

fn live_open(job_id: &str, buyer: &str) {
    if let Ok(mut m) = live_logs().lock() {
        m.retain(|_, l| !l.done || l.at.elapsed() < Duration::from_secs(3600));
        m.insert(
            job_id.into(),
            LiveLog {
                buyer: buyer.into(),
                stage: "starting".into(),
                start: 0,
                buf: VecDeque::new(),
                done: false,
                at: Instant::now(),
            },
        );
    }
}

fn live_stage(job_id: &str, stage: &str) {
    if let Ok(mut m) = live_logs().lock()
        && let Some(l) = m.get_mut(job_id)
    {
        l.stage = stage.into();
        l.at = Instant::now();
    }
    live_line(job_id, &format!("[c0mpute] {stage}"));
}

fn live_line(job_id: &str, line: &str) {
    if let Ok(mut m) = live_logs().lock()
        && let Some(l) = m.get_mut(job_id)
    {
        l.buf.extend(line.as_bytes());
        l.buf.push_back(b'\n');
        while l.buf.len() > LIVE_LOG_BYTES {
            l.buf.pop_front();
            l.start += 1;
        }
    }
}

fn live_done(job_id: &str) {
    if let Ok(mut m) = live_logs().lock()
        && let Some(l) = m.get_mut(job_id)
    {
        l.done = true;
        l.at = Instant::now();
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LogRequest {
    pub job_id: String,
    pub from: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LogResponse {
    pub known: bool,
    pub stage: String,
    pub text: String,
    /// Offset to ask from next time.
    pub next: u64,
    pub done: bool,
}

/// Answer a log request from `peer` (only the job's buyer gets its log).
pub fn log_since(peer: &str, req: &LogRequest) -> LogResponse {
    let unknown = LogResponse {
        known: false,
        stage: String::new(),
        text: String::new(),
        next: req.from,
        done: false,
    };
    let Ok(m) = live_logs().lock() else {
        return unknown;
    };
    let Some(l) = m.get(&req.job_id).filter(|l| l.buyer == peer) else {
        return unknown;
    };
    let end = l.start + l.buf.len() as u64;
    let from = req.from.clamp(l.start, end);
    let skip = (from - l.start) as usize;
    let take = (end - from).min(64 * 1024) as usize;
    let bytes: Vec<u8> = l.buf.iter().skip(skip).take(take).copied().collect();
    LogResponse {
        known: true,
        stage: l.stage.clone(),
        text: String::from_utf8_lossy(&bytes).into_owned(),
        next: from + take as u64,
        done: l.done && from + take as u64 >= end,
    }
}

struct LogHandler;

#[async_trait::async_trait]
impl RpcHandler for LogHandler {
    async fn handle(&self, peer: PeerId, body: Vec<u8>) -> std::result::Result<Vec<u8>, String> {
        let req: LogRequest =
            serde_json::from_slice(&body).map_err(|_| "bad request".to_string())?;
        serde_json::to_vec(&log_since(&peer.to_base58(), &req)).map_err(|e| e.to_string())
    }
}

/// Buyer side: ask `worker` for `job_id`'s log from offset `from`.
pub async fn fetch_log(
    net: &Libp2pNetwork,
    worker: PeerId,
    job_id: &str,
    from: u64,
) -> Result<LogResponse> {
    let body = serde_json::to_vec(&LogRequest {
        job_id: job_id.into(),
        from,
    })?;
    let bytes = net.rpc(worker, LOG_SERVICE, body).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

// ─── running processes ───────────────────────────────────────────────────

/// The last `cap` bytes of a stream of lines.
#[derive(Debug, Default)]
pub struct LogTail {
    buf: VecDeque<u8>,
    cap: usize,
}

impl LogTail {
    pub fn new(cap: usize) -> Self {
        Self {
            buf: VecDeque::new(),
            cap,
        }
    }
    pub fn push_line(&mut self, line: &str) {
        self.buf.extend(line.as_bytes());
        self.buf.push_back(b'\n');
        while self.buf.len() > self.cap {
            self.buf.pop_front();
        }
    }
    pub fn text(&self) -> String {
        let v: Vec<u8> = self.buf.iter().copied().collect();
        String::from_utf8_lossy(&v).into_owned()
    }
}

/// Why a process was stopped before it finished.
#[derive(Debug)]
pub enum Stop {
    Timeout,
    Disk(String),
    Failed(anyhow::Error),
}

impl std::fmt::Display for Stop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Stop::Timeout => f.write_str("timed out"),
            Stop::Disk(m) => write!(f, "stopped: {m}"),
            Stop::Failed(e) => write!(f, "{e:#}"),
        }
    }
}

/// What a job may use on disk while it runs.
#[derive(Clone, Debug)]
pub struct DiskGuard {
    pub job_dir: PathBuf,
    pub job_cap: u64,
    pub watch: Vec<PathBuf>,
    pub cfg: ContainersConfig,
}

fn dir_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(meta) = e.metadata() else { continue };
            if meta.is_dir() {
                stack.push(e.path());
            } else {
                total += meta.len();
            }
        }
    }
    total
}

impl DiskGuard {
    /// `Some(reason)` when the job must stop.
    pub fn check(&self) -> Option<String> {
        let used = dir_size(&self.job_dir);
        if used > self.job_cap {
            return Some(format!(
                "job scratch is {} MiB, over its {} MiB disk limit",
                used >> 20,
                self.job_cap >> 20
            ));
        }
        for d in &self.watch {
            let (free, total) = c0mpute_store::free_space_at(d);
            let floor = min_free_bytes(&self.cfg, total);
            if free < floor {
                return Some(format!(
                    "{} is down to {} GiB free (keeps {} GiB)",
                    d.display(),
                    free >> 30,
                    floor >> 30
                ));
            }
        }
        None
    }
}

async fn pump(stream: impl AsyncRead + Unpin, tx: mpsc::UnboundedSender<String>) {
    let mut lines = BufReader::new(stream).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = if line.len() > 4096 {
            format!("{}...", &line[..line.floor_char_boundary(4096)])
        } else {
            line
        };
        if tx.send(line).is_err() {
            break;
        }
    }
}

/// Run a container CLI command to completion, its output into `tail` and the
/// job's live log, killed on timeout or when `guard` says stop.
pub async fn run_logged(
    args: &[String],
    stdin_file: Option<&Path>,
    timeout: Duration,
    guard: Option<&DiskGuard>,
    job_id: &str,
    tail: &mut LogTail,
) -> std::result::Result<std::process::ExitStatus, Stop> {
    let mut cmd = crate::container::command();
    cmd.args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    match stdin_file {
        Some(p) => {
            let f = std::fs::File::open(p)
                .map_err(|e| Stop::Failed(anyhow::anyhow!("open {}: {e}", p.display())))?;
            cmd.stdin(Stdio::from(f));
        }
        None => {
            cmd.stdin(Stdio::null());
        }
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| Stop::Failed(anyhow::anyhow!("run the container CLI: {e}")))?;
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    if let Some(out) = child.stdout.take() {
        tokio::spawn(pump(out, tx.clone()));
    }
    if let Some(err) = child.stderr.take() {
        tokio::spawn(pump(err, tx.clone()));
    }
    drop(tx);
    let deadline = tokio::time::Instant::now() + timeout;
    let mut check = tokio::time::interval(Duration::from_secs(5));
    check.tick().await;
    let mut lines_open = true;
    loop {
        tokio::select! {
            status = child.wait() => {
                // Drain what is left of the output.
                while let Ok(Some(line)) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                    tail.push_line(&line);
                    live_line(job_id, &line);
                }
                return status.map_err(|e| Stop::Failed(e.into()));
            }
            line = rx.recv(), if lines_open => match line {
                Some(line) => {
                    tail.push_line(&line);
                    live_line(job_id, &line);
                }
                None => lines_open = false,
            },
            _ = tokio::time::sleep_until(deadline) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(Stop::Timeout);
            }
            _ = check.tick() => {
                if let Some(g) = guard {
                    let g = g.clone();
                    if let Ok(Some(reason)) = tokio::task::spawn_blocking(move || g.check()).await {
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        return Err(Stop::Disk(reason));
                    }
                }
            }
        }
    }
}

/// Run a short container CLI command; stdout on success.
async fn docker_out(args: &[String], timeout: Duration) -> Result<String> {
    let mut cmd = crate::container::command();
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let out = tokio::time::timeout(timeout, cmd.output())
        .await
        .with_context(|| {
            format!(
                "docker {} timed out",
                args.first().map(String::as_str).unwrap_or("")
            )
        })??;
    if !out.status.success() {
        bail!(
            "docker {}: {}",
            args.first().map(String::as_str).unwrap_or(""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

/// A writer that counts bytes and refuses past `max`.
struct Capped<W> {
    inner: W,
    written: u64,
    max: u64,
}

impl<W: std::io::Write> std::io::Write for Capped<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.written + buf.len() as u64 > self.max {
            return Err(std::io::Error::other("over the size limit"));
        }
        let n = self.inner.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Run `docker <args>` and gzip its stdout into `dest` (at most `max`
/// compressed bytes). Returns `(size, blake3 hex)` of the file.
pub async fn export_gz(
    args: Vec<String>,
    dest: &Path,
    timeout: Duration,
    max: u64,
) -> Result<(u64, String)> {
    let mut child = crate::container::command_sync()
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("run the container CLI")?;
    let stdout = child.stdout.take().context("no stdout")?;
    let mut stderr = child.stderr.take().context("no stderr")?;
    let dest_owned = dest.to_path_buf();
    let copy = tokio::task::spawn_blocking(move || -> Result<(u64, String)> {
        use std::io::{Read, Write};
        let file = std::fs::File::create(&dest_owned)?;
        let capped = Capped {
            inner: std::io::BufWriter::new(file),
            written: 0,
            max,
        };
        let mut gz = flate2::write::GzEncoder::new(capped, flate2::Compression::new(4));
        let mut reader = std::io::BufReader::new(stdout);
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            gz.write_all(&buf[..n])?;
        }
        let mut capped = gz.finish()?;
        capped.flush()?;
        drop(capped);
        // Hash the file as stored.
        let mut f = std::fs::File::open(&dest_owned)?;
        let mut h = blake3::Hasher::new();
        let mut size = 0u64;
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            size += n as u64;
            h.update(&buf[..n]);
        }
        Ok((size, h.finalize().to_hex().to_string()))
    });
    let waited = tokio::time::timeout(timeout, copy).await;
    let result = match waited {
        Ok(joined) => joined.context("export task")?,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(dest);
            bail!(
                "docker {} timed out",
                args.first().map(String::as_str).unwrap_or("")
            );
        }
    };
    let status = child.wait()?;
    if !status.success() || result.is_err() {
        let mut err = String::new();
        use std::io::Read;
        let _ = stderr.read_to_string(&mut err);
        let _ = std::fs::remove_file(dest);
        if !status.success() {
            bail!(
                "docker {}: {}",
                args.first().map(String::as_str).unwrap_or(""),
                err.trim()
            );
        }
    }
    result
}

// ─── docker argv (pure, tested without docker) ───────────────────────────

/// `docker buildx build` argv for a build. The context comes on stdin.
pub fn build_args(
    b: &BuildSpec,
    tag: &str,
    iidfile: &Path,
    network: bool,
    job_id: &str,
) -> Vec<String> {
    let mut a = s(&[
        "buildx",
        "build",
        "--builder",
        "default",
        "--progress=plain",
        "--load",
    ]);
    a.push(format!("--tag={tag}"));
    a.push(format!("--iidfile={}", iidfile.display()));
    a.push(format!("--file={}", b.dockerfile));
    a.push(format!("--label=c0mpute.job={job_id}"));
    a.push(format!(
        "--network={}",
        if network { "default" } else { "none" }
    ));
    for (k, v) in &b.build_args {
        a.push(format!("--build-arg={k}={v}"));
    }
    if let Some(t) = &b.target {
        a.push(format!("--target={t}"));
    }
    a.push("-".into());
    a
}

/// `docker create` argv for a run step: hard limits, no host mounts, most
/// capabilities dropped, no new privileges, the jobs network (or none).
pub fn create_args(
    p: &Parsed,
    name: &str,
    image: &str,
    network: &str,
    job_id: &str,
) -> Vec<String> {
    let mut a = s(&["create"]);
    a.push(format!("--name={name}"));
    a.push(format!("--label=c0mpute.job={job_id}"));
    a.push(format!("--network={network}"));
    a.push(format!("--cpus={}", p.cpus));
    a.push(format!("--memory={}m", p.memory_mb));
    a.push(format!("--memory-swap={}m", p.memory_mb));
    a.push("--pids-limit=4096".into());
    a.push("--cap-drop=ALL".into());
    for cap in [
        "CHOWN",
        "DAC_OVERRIDE",
        "FOWNER",
        "FSETID",
        "SETUID",
        "SETGID",
        "KILL",
        "NET_BIND_SERVICE",
    ] {
        a.push(format!("--cap-add={cap}"));
    }
    a.push("--security-opt=no-new-privileges".into());
    a.push(format!("--workdir={}", p.workdir));
    for (k, v) in &p.spec.env {
        a.push(format!("--env={k}={v}"));
    }
    a.push(format!("--entrypoint={}", p.spec.cmd[0]));
    a.push(image.into());
    a.extend(p.spec.cmd.iter().skip(1).cloned());
    a
}

/// Where an output path lives in the container.
pub fn output_path(workdir: &str, p: &str) -> String {
    if p.starts_with('/') {
        p.to_string()
    } else {
        format!("{}/{}", workdir.trim_end_matches('/'), p)
    }
}

/// Private, loopback, link-local (cloud metadata), CGNAT, multicast and
/// reserved IPv4 ranges the egress guard drops.
pub const GUARDED_RANGES: &[&str] = &[
    "0.0.0.0/8",
    "10.0.0.0/8",
    "100.64.0.0/10",
    "127.0.0.0/8",
    "169.254.0.0/16",
    "172.16.0.0/12",
    "192.0.0.0/24",
    "192.168.0.0/16",
    "198.18.0.0/15",
    "224.0.0.0/4",
    "240.0.0.0/4",
];

/// iptables rules (as argv after `iptables`) that keep `bridge` off private
/// ranges and off the host itself.
pub fn guard_rules(bridge: &str) -> Vec<Vec<String>> {
    let mut rules: Vec<Vec<String>> = GUARDED_RANGES
        .iter()
        .map(|r| s(&["DOCKER-USER", "-i", bridge, "-d", r, "-j", "DROP"]))
        .collect();
    rules.push(s(&["INPUT", "-i", bridge, "-j", "DROP"]));
    rules
}

fn is_root() -> bool {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions.
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Insert the guard rules for `bridge` (idempotent). True when they are in.
async fn ensure_guard(bridge: &str) -> bool {
    if !is_root() {
        return false;
    }
    for rule in guard_rules(bridge) {
        let (chain, rest) = rule.split_first().expect("non-empty rule");
        let mut check = vec!["-C".to_string(), chain.clone()];
        check.extend(rest.iter().cloned());
        let present = tokio::process::Command::new("iptables")
            .args(&check)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .is_ok_and(|s| s.success());
        if present {
            continue;
        }
        let mut insert = vec!["-I".to_string(), chain.clone()];
        insert.extend(rest.iter().cloned());
        let ok = tokio::process::Command::new("iptables")
            .args(&insert)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .is_ok_and(|s| s.success());
        if !ok {
            return false;
        }
    }
    true
}

/// Make sure the jobs network exists (containers on it cannot reach each
/// other) and, when configured, guard it. Returns whether egress is guarded.
async fn ensure_jobs_network(cfg: &ContainersConfig) -> Result<bool> {
    let t = Duration::from_secs(30);
    if docker_out(&s(&["network", "inspect", JOBS_NETWORK]), t)
        .await
        .is_err()
    {
        let create = s(&[
            "network",
            "create",
            "--driver=bridge",
            &format!("--opt=com.docker.network.bridge.name={JOBS_BRIDGE}"),
            "--opt=com.docker.network.bridge.enable_icc=false",
            "--label=c0mpute.jobs=1",
            JOBS_NETWORK,
        ]);
        if let Err(e) = docker_out(&create, t).await
            && docker_out(&s(&["network", "inspect", JOBS_NETWORK]), t)
                .await
                .is_err()
        {
            return Err(e).context("create the jobs network");
        }
    }
    Ok(cfg.egress_guard && ensure_guard(JOBS_BRIDGE).await)
}

/// The N-of-M hook: a buyer that ran the same build on several builders can
/// require their image ids to agree. Today it only compares; a reproducible
/// build (pinned bases, `SOURCE_DATE_EPOCH`, no network-fetched floating
/// versions) is what makes ids agree across machines.
pub fn verify_reproducible(image_ids: &[String], required: usize) -> Result<String> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for id in image_ids {
        *counts.entry(id.as_str()).or_default() += 1;
    }
    match counts.into_iter().max_by_key(|(_, n)| *n) {
        Some((id, n)) if n >= required.max(1) => Ok(id.to_string()),
        Some((_, n)) => bail!(
            "only {n} of {} builders agree on the image id (need {required})",
            image_ids.len()
        ),
        None => bail!("no image ids to compare"),
    }
}

// ─── worker runner ───────────────────────────────────────────────────────

/// Spawn the runner; hand the sender to the dispatch loop.
pub fn spawn_container_runner(
    net: Arc<Libp2pNetwork>,
    store: ChunkStore,
    cfg: ContainersConfig,
    load: WorkerLoad,
) -> mpsc::Sender<AcceptedJob> {
    let _ = WORKER_CFG.set(cfg.clone());
    net.register_rpc(LOG_SERVICE, Arc::new(LogHandler));
    let (tx, mut rx) = mpsc::channel::<AcceptedJob>(8);
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let Ok(slot) = load.compute_slots().acquire_owned().await else {
                return;
            };
            let net = net.clone();
            let store = store.clone();
            let cfg = cfg.clone();
            let load = load.clone();
            tokio::spawn(async move {
                let _slot = slot;
                let started = Instant::now();
                let job_id = job.accept.job_id.clone();
                live_open(&job_id, &job.accept.buyer_peer_id);
                let job_dir = match crate::runner::safe_job_dir(&cfg.work_dir(), &job_id) {
                    Ok(d) => d,
                    Err(e) => {
                        warn!(%job_id, err = %e, "container runner: bad job id");
                        publish_receipt(&net, &job.accept, None, JobStatus::Failed).await;
                        return;
                    }
                };
                let result = crate::job_control::cancellable(
                    &job_id,
                    run_job(&net, &store, &cfg, &job, &job_dir),
                )
                .await;
                cleanup(&job_id).await;
                let _ = tokio::fs::remove_dir_all(&job_dir).await;
                live_done(&job_id);
                match result {
                    Ok(hash) => {
                        load.record_job(started.elapsed());
                        info!(%job_id, %hash, "container runner: job complete");
                        publish_receipt(&net, &job.accept, Some(hash), JobStatus::Completed).await;
                    }
                    Err(error) => {
                        warn!(%job_id, err = %format!("{error:#}"), "container runner: job failed");
                        publish_receipt(&net, &job.accept, None, JobStatus::Failed).await;
                    }
                }
            });
        }
    });
    tx
}

fn short_id(job_id: &str) -> String {
    job_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(12)
        .collect::<String>()
        .to_ascii_lowercase()
}

fn job_tag(job_id: &str) -> String {
    format!("c0mpute-job/{}:build", short_id(job_id))
}

fn container_name(job_id: &str) -> String {
    format!("c0mpute-job-{}", short_id(job_id))
}

/// Remove whatever a job left in docker (container, built image). Runs after
/// every job, cancelled or not.
async fn cleanup(job_id: &str) {
    let t = Duration::from_secs(120);
    let _ = docker_out(&s(&["rm", "-f", "-v", &container_name(job_id)]), t).await;
    let _ = docker_out(&s(&["image", "rm", "-f", &job_tag(job_id)]), t).await;
}

/// Keep the BuildKit cache under `cache_gb` (new flag first, then the old one).
async fn prune_build_cache(cfg: &ContainersConfig) {
    let t = Duration::from_secs(600);
    let gb = format!("{}gb", cfg.cache_gb);
    if docker_out(&s(&["builder", "prune", "-f", "--max-used-space", &gb]), t)
        .await
        .is_err()
    {
        let _ = docker_out(&s(&["builder", "prune", "-f", "--keep-storage", &gb]), t).await;
    }
}

/// One stored artifact, as the result JSON lists it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Artifact {
    /// The output path asked for (absent for the image).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Plaintext bytes and blake3, checked by the buyer after opening.
    pub size: u64,
    pub hash: String,
    /// The stored (sealed) object the buyer fetches.
    pub stored: String,
    pub format: String,
    /// The image id (`sha256:...`) for an image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

async fn fetch_blob(net: &Arc<Libp2pNetwork>, b: &BlobRef, buyer: &str, dest: &Path) -> Result<()> {
    let provider = match b.provider {
        Some(p) => Some(p),
        None => buyer.parse().ok(),
    };
    b.at.fetch_to_file_opened(
        net,
        provider,
        MAX_INPUT_BYTES as usize,
        b.key.as_ref(),
        dest,
    )
    .await
}

async fn store_artifact(
    net: &Libp2pNetwork,
    store: &ChunkStore,
    job: &AcceptedJob,
    file: &Path,
    max: u64,
    size: u64,
    hash: String,
    format: &str,
) -> Result<Artifact> {
    let stored = crate::pieces::publish_result_file(net, store, &job.accept, file, max).await?;
    if job.accept.e2e.is_some() {
        forget_later(store.clone(), stored.clone());
    }
    Ok(Artifact {
        path: None,
        size,
        hash,
        stored,
        format: format.into(),
        id: None,
    })
}

/// Drop a sealed result's pieces after [`RESULT_KEEP`]. Sealed bytes are
/// unique to the job, so no other object shares these chunks.
fn forget_later(store: ChunkStore, root_hex: String) {
    tokio::spawn(async move {
        tokio::time::sleep(RESULT_KEEP).await;
        let Ok(root) = Hash::from_hex(&root_hex) else {
            return;
        };
        if let Ok(bytes) = store.get(&root).await
            && let Some(Ok(m)) = crate::pieces::ResultManifest::decode(&bytes)
        {
            for p in &m.pieces {
                if let Ok(h) = Hash::from_hex(p) {
                    let _ = store.delete(&h).await;
                }
            }
        }
        let _ = store.delete(&root).await;
    });
}

async fn run_job(
    net: &Arc<Libp2pNetwork>,
    store: &ChunkStore,
    cfg: &ContainersConfig,
    job: &AcceptedJob,
    job_dir: &Path,
) -> Result<String> {
    let started = Instant::now();
    let job_id = job.accept.job_id.clone();
    let spec = crate::workflow::resolve_spec(net, job).await?;
    let parsed = match parse_spec(&spec) {
        Ok(p) => p,
        Err(e) => {
            return publish_json(
                net,
                store,
                job,
                &json!({"v": 1, "ok": false, "error": e, "duration_ms": 0}),
            )
            .await;
        }
    };
    let mut result = run_parsed(net, store, cfg, job, job_dir, &parsed).await;
    result["duration_ms"] = json!(started.elapsed().as_millis() as u64);
    result["mode"] = json!(parsed.spec.mode);
    let rt = probed().cloned().unwrap_or_default();
    result["worker"] = json!({ "docker": rt.docker_version });
    live_stage(
        &job_id,
        if result["ok"] == json!(true) {
            "done"
        } else {
            "failed"
        },
    );
    publish_json(net, store, job, &result).await
}

async fn publish_json(
    net: &Libp2pNetwork,
    store: &ChunkStore,
    job: &AcceptedJob,
    v: &Value,
) -> Result<String> {
    let mut bytes = serde_json::to_vec(v)?;
    if bytes.len() > MAX_RESULT_BYTES {
        let mut v = v.clone();
        v["build_log_tail"] = json!("(trimmed)");
        v["log_tail"] = json!("(trimmed)");
        bytes = serde_json::to_vec(&v)?;
    }
    crate::pieces::publish_result_bytes(net, store, &job.accept, &bytes)
        .await
        .context("publish container result")
}

/// The job proper. Never errs for the requester's mistakes (a failed build,
/// a non-zero exit): those are `ok: false` results with the log tail.
async fn run_parsed(
    net: &Arc<Libp2pNetwork>,
    store: &ChunkStore,
    cfg: &ContainersConfig,
    job: &AcceptedJob,
    job_dir: &Path,
    p: &Parsed,
) -> Value {
    let job_id = job.accept.job_id.clone();
    let fail = |e: String| json!({"v": 1, "ok": false, "error": e});
    if let Err(e) = tokio::fs::create_dir_all(job_dir).await {
        return fail(format!("worker could not create its scratch dir: {e}"));
    }
    let deadline = Instant::now() + p.timeout;
    let left = || deadline.saturating_duration_since(Instant::now());
    let mut watch = vec![cfg.work_dir()];
    if let Some(root) = probed().and_then(|r| r.docker_root.clone()) {
        watch.push(root);
    }
    let guard = DiskGuard {
        job_dir: job_dir.to_path_buf(),
        job_cap: p.disk_mb.saturating_mul(1 << 20),
        watch,
        cfg: cfg.clone(),
    };
    let buyer = job.accept.buyer_peer_id.clone();
    let mut out = json!({"v": 1, "ok": false, "outputs": []});

    // 1. Build.
    let mut image_ref = p.image.clone();
    let mut build_guarded = false;
    if let (Some(b), Some(ctx)) = (&p.build, &p.context) {
        live_stage(&job_id, "fetching the build context");
        let ctx_file = job_dir.join("context.tar");
        if let Err(e) = fetch_blob(net, ctx, &buyer, &ctx_file).await {
            return fail(format!("could not fetch the build context: {e:#}"));
        }
        if let Some(bridge) = &cfg.guard_build_bridge {
            build_guarded = cfg.egress_guard && ensure_guard(bridge).await;
        }
        live_stage(&job_id, "building");
        let tag = job_tag(&job_id);
        let iid = job_dir.join("iid");
        let args = build_args(b, &tag, &iid, p.network, &job_id);
        let mut tail = LogTail::new(LOG_TAIL_BYTES);
        let status = run_logged(
            &args,
            Some(&ctx_file),
            left(),
            Some(&guard),
            &job_id,
            &mut tail,
        )
        .await;
        let _ = tokio::fs::remove_file(&ctx_file).await;
        out["build_log_tail"] = json!(tail.text());
        out["egress_guard"] = json!({ "build": build_guarded });
        let built = match status {
            Ok(s) if s.success() => true,
            Ok(s) => {
                out["error"] = json!(format!("build failed (exit {})", s.code().unwrap_or(-1)));
                false
            }
            Err(stop) => {
                out["error"] = json!(format!("build {stop}"));
                false
            }
        };
        prune_build_cache(cfg).await;
        if !built {
            return out;
        }
        // The iidfile can be unreadable when the CLI writes it as another user
        // (a `sudo docker` wrapper); the daemon's answer is the same id.
        let mut id = tokio::fs::read_to_string(&iid)
            .await
            .unwrap_or_default()
            .trim()
            .to_string();
        if id.is_empty() {
            id = docker_out(
                &s(&["image", "inspect", "--format", "{{.Id}}", &tag]),
                Duration::from_secs(30),
            )
            .await
            .unwrap_or_default();
        }
        image_ref = Some(tag.clone());
        if p.returns_image() {
            live_stage(&job_id, "saving the image");
            let file = job_dir.join("image.tar.gz");
            match export_gz(
                s(&["save", &tag]),
                &file,
                left().max(Duration::from_secs(60)),
                MAX_IMAGE_BYTES,
            )
            .await
            {
                Ok((size, hash)) => {
                    live_stage(&job_id, &format!("storing the image ({} MiB)", size >> 20));
                    match store_artifact(
                        net,
                        store,
                        job,
                        &file,
                        MAX_IMAGE_BYTES,
                        size,
                        hash,
                        "docker-save+gzip",
                    )
                    .await
                    {
                        Ok(mut a) => {
                            a.id = Some(id.clone());
                            out["image"] = json!(a);
                        }
                        Err(e) => return fail(format!("could not store the image: {e:#}")),
                    }
                }
                Err(e) => {
                    out["error"] = json!(format!("could not save the image: {e:#}"));
                    return out;
                }
            }
            let _ = tokio::fs::remove_file(&file).await;
        } else {
            out["image"] = json!({ "id": id });
        }
        if p.is_build() {
            out["ok"] = json!(true);
            return out;
        }
    }

    // 2. Run.
    let Some(image) = image_ref else {
        return fail("nothing to run".into());
    };
    let pulled = if p.build.is_none() {
        let present = docker_out(
            &s(&["image", "inspect", "--format", "{{.Id}}", &image]),
            Duration::from_secs(30),
        )
        .await
        .is_ok();
        if !present {
            live_stage(&job_id, "pulling the image");
            let mut tail = LogTail::new(LOG_TAIL_BYTES);
            match run_logged(
                &s(&["pull", &image]),
                None,
                left(),
                Some(&guard),
                &job_id,
                &mut tail,
            )
            .await
            {
                Ok(st) if st.success() => {}
                Ok(_) | Err(_) => {
                    return fail(format!("could not pull {image}: {}", tail.text().trim()));
                }
            }
        }
        !present
    } else {
        false
    };
    let network = if p.network {
        match ensure_jobs_network(cfg).await {
            Ok(guarded) => {
                out["egress_guard"]["run"] = json!(guarded);
                JOBS_NETWORK.to_string()
            }
            Err(e) => return fail(format!("worker could not set up its jobs network: {e:#}")),
        }
    } else {
        "none".to_string()
    };
    let name = container_name(&job_id);
    let run_result = async {
        docker_out(
            &create_args(p, &name, &image, &network, &job_id),
            Duration::from_secs(120),
        )
        .await
        .map_err(|e| format!("could not create the container: {e:#}"))?;
        if let Some(input) = &p.input {
            live_stage(&job_id, "fetching the input");
            let file = job_dir.join("input.tar");
            fetch_blob(net, input, &buyer, &file)
                .await
                .map_err(|e| format!("could not fetch the input: {e:#}"))?;
            let dest = format!("{name}:{}", p.workdir);
            let mut tail = LogTail::new(4096);
            let st = run_logged(
                &s(&["cp", "-", &dest]),
                Some(&file),
                left(),
                Some(&guard),
                &job_id,
                &mut tail,
            )
            .await;
            let _ = tokio::fs::remove_file(&file).await;
            match st {
                Ok(s) if s.success() => {}
                Ok(_) => {
                    return Err(format!(
                        "could not copy the input in: {}",
                        tail.text().trim()
                    ));
                }
                Err(stop) => return Err(format!("copying the input in {stop}")),
            }
        }
        live_stage(&job_id, "running");
        let mut tail = LogTail::new(LOG_TAIL_BYTES);
        let st = run_logged(
            &s(&["start", "--attach", &name]),
            None,
            left(),
            Some(&guard),
            &job_id,
            &mut tail,
        )
        .await;
        out["log_tail"] = json!(tail.text());
        match st {
            Ok(_) => {}
            Err(stop) => {
                let _ = docker_out(&s(&["kill", &name]), Duration::from_secs(30)).await;
                return Err(format!("run {stop}"));
            }
        }
        let code = docker_out(
            &s(&["inspect", "--format", "{{.State.ExitCode}}", &name]),
            Duration::from_secs(30),
        )
        .await
        .ok()
        .and_then(|c| c.parse::<i64>().ok())
        .unwrap_or(-1);
        let oom = docker_out(
            &s(&["inspect", "--format", "{{.State.OOMKilled}}", &name]),
            Duration::from_secs(30),
        )
        .await
        .is_ok_and(|v| v == "true");
        out["exit_code"] = json!(code);
        if oom {
            out["oom_killed"] = json!(true);
        }
        // 3. Outputs, also from a failed run (test reports).
        let mut outputs = Vec::new();
        for (i, path) in p.spec.outputs.iter().enumerate() {
            live_stage(&job_id, &format!("copying out {path}"));
            let file = job_dir.join(format!("out-{i}.tar.gz"));
            let src = format!("{name}:{}", output_path(&p.workdir, path));
            match export_gz(
                s(&["cp", &src, "-"]),
                &file,
                left().max(Duration::from_secs(60)),
                MAX_OUTPUT_BYTES,
            )
            .await
            {
                Ok((size, hash)) => match store_artifact(
                    net,
                    store,
                    job,
                    &file,
                    MAX_OUTPUT_BYTES,
                    size,
                    hash,
                    "tar+gzip",
                )
                .await
                {
                    Ok(mut a) => {
                        a.path = Some(path.clone());
                        outputs.push(json!(a));
                    }
                    Err(e) => return Err(format!("could not store output {path}: {e:#}")),
                },
                Err(e) => outputs
                    .push(json!({ "path": path, "missing": true, "error": format!("{e:#}") })),
            }
            let _ = tokio::fs::remove_file(&file).await;
        }
        out["outputs"] = json!(outputs);
        out["ok"] = json!(code == 0);
        if code != 0 {
            out["error"] = json!(format!(
                "command exited {code}{}",
                if oom { " (out of memory)" } else { "" }
            ));
        }
        Ok(())
    }
    .await;
    let _ = docker_out(&s(&["rm", "-f", "-v", &name]), Duration::from_secs(120)).await;
    if pulled {
        let _ = docker_out(&s(&["image", "rm", &image]), Duration::from_secs(120)).await;
    }
    if let Err(e) = run_result {
        out["ok"] = json!(false);
        out["error"] = json!(e);
    }
    out
}

// ─── buyer ───────────────────────────────────────────────────────────────

/// Files left out of every packed directory: the context goes to another
/// machine, so local credentials stay home. Patterns match at any depth (GNU
/// and BSD tar both treat an unanchored `--exclude` that way). A build that
/// really needs one of these sets `C0MPUTE_PACK_INCLUDE_SECRETS=1`.
pub const SECRET_EXCLUDES: &[&str] = &[
    ".env",
    ".env.local",
    ".env.*.local",
    ".env.production",
    ".env.development",
    ".npmrc",
    ".netrc",
    ".pypirc",
    ".ssh",
    ".aws",
    ".docker/config.json",
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    "*.p12",
    "*.pfx",
    "*.keystore",
    "*.jks",
];

fn include_secrets() -> bool {
    std::env::var("C0MPUTE_PACK_INCLUDE_SECRETS").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// Pack `dir` into a gzipped tarball at `dest` with the system `tar`. For a
/// build context, `.dockerignore` patterns are applied as excludes where GNU
/// tar can express them exactly (the file still ships, and docker applies it
/// again on the worker).
pub fn pack_dir(dir: &Path, dest: &Path, dockerignore: bool) -> Result<u64> {
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    let mut args: Vec<String> = Vec::new();
    let gnu = std::process::Command::new("tar")
        .arg("--version")
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("GNU tar"));
    if dockerignore
        && gnu
        && let Ok(text) = std::fs::read_to_string(dir.join(".dockerignore"))
    {
        args.extend(dockerignore_excludes(&text));
    }
    if !include_secrets() {
        args.extend(SECRET_EXCLUDES.iter().map(|p| format!("--exclude={p}")));
    }
    args.extend([
        "-C".into(),
        dir.display().to_string(),
        "-czf".into(),
        dest.display().to_string(),
        ".".into(),
    ]);
    // Exclude options must come before the members; GNU tar reads them in order.
    let status = std::process::Command::new("tar")
        .args(&args)
        .status()
        .context("run tar (is it installed?)")?;
    if !status.success() {
        bail!("tar failed packing {}", dir.display());
    }
    Ok(std::fs::metadata(dest)?.len())
}

/// GNU tar `--exclude` options for a `.dockerignore`, or none at all when it
/// has `!` exceptions or patterns tar cannot match the same way.
pub fn dockerignore_excludes(text: &str) -> Vec<String> {
    let mut anchored = Vec::new();
    let mut anywhere = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('!') {
            return Vec::new();
        }
        let pat = line.trim_start_matches('/').trim_end_matches('/');
        if pat.is_empty() || pat == "." || pat == "*" || pat == "**" {
            return Vec::new();
        }
        if let Some(rest) = pat.strip_prefix("**/") {
            if rest.contains("**") || rest.contains('/') {
                return Vec::new();
            }
            anywhere.push(rest.to_string());
        } else {
            if pat.contains("**") {
                return Vec::new();
            }
            anchored.push(format!("./{pat}"));
        }
    }
    let mut out = vec!["--no-wildcards-match-slash".to_string()];
    if !anchored.is_empty() {
        out.push("--anchored".into());
        out.extend(anchored.into_iter().map(|p| format!("--exclude={p}")));
    }
    if !anywhere.is_empty() {
        out.push("--no-anchored".into());
        out.extend(anywhere.into_iter().map(|p| format!("--exclude={p}")));
    }
    if out.len() == 1 { Vec::new() } else { out }
}

/// What to run, buyer side.
#[derive(Clone, Debug, Default)]
pub struct Request {
    /// Build context tarball (tar / tar.gz), for a build.
    pub context: Option<PathBuf>,
    pub build: Option<BuildSpec>,
    pub image: Option<String>,
    pub cmd: Vec<String>,
    /// Tarball copied into the workdir before a run step.
    pub input: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub workdir: Option<String>,
    pub outputs: Vec<String>,
    pub return_image: bool,
    pub cpus: Option<f64>,
    pub memory_mb: Option<u64>,
    pub disk_mb: Option<u64>,
    pub timeout_secs: Option<u64>,
    pub network: Option<bool>,
    /// Builders to accept bids from (empty = anyone, only with `any_worker`).
    pub builders: Vec<String>,
    pub any_worker: bool,
    pub max_price_usd: f64,
}

/// What came back, with every artifact fetched, opened and checked.
#[derive(Debug)]
pub struct Outcome {
    pub job_id: String,
    pub worker: String,
    pub price_usd: f64,
    pub result: Value,
    /// The image tarball (`docker save | gzip`), when one came back.
    pub image: Option<PathBuf>,
    /// Output tarballs `(path asked for, local tar.gz)`.
    pub outputs: Vec<(String, PathBuf)>,
}

impl Outcome {
    pub fn ok(&self) -> bool {
        self.result.get("ok") == Some(&json!(true))
    }
}

/// Progress lines for the caller (stderr for the CLI, a log for MCP).
pub type Progress = Arc<dyn Fn(&str) + Send + Sync>;

/// Build the spec for `req` with its blobs stored (sealed) on this node.
async fn spec_for(
    net: &Libp2pNetwork,
    store: &ChunkStore,
    req: &Request,
    stored: &mut Vec<String>,
) -> Result<Value> {
    let mode = if req.cmd.is_empty() { "build" } else { "run" };
    let mut spec = json!({
        "v": 1,
        "mode": mode,
        "cpus": req.cpus.unwrap_or(DEFAULT_CPUS),
        "memory_mb": req.memory_mb.unwrap_or(DEFAULT_MEMORY_MB),
        "disk_mb": req.disk_mb.unwrap_or(DEFAULT_DISK_MB),
        "timeout_secs": req.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS),
        "network": req.network.unwrap_or(true),
    });
    if let Some(b) = &req.build {
        spec["build"] = json!(b);
    }
    if let Some(i) = &req.image {
        spec["image"] = json!(i);
    }
    if !req.cmd.is_empty() {
        spec["cmd"] = json!(req.cmd);
    }
    if !req.env.is_empty() {
        spec["env"] = json!(req.env);
    }
    if let Some(w) = &req.workdir {
        spec["workdir"] = json!(w);
    }
    if !req.outputs.is_empty() {
        spec["outputs"] = json!(req.outputs);
    }
    if req.return_image {
        spec["return_image"] = json!(true);
    }
    let me = net.peer_id().to_base58();
    for (prefix, file) in [("context", &req.context), ("input", &req.input)] {
        let Some(file) = file else { continue };
        let (pieces, key) = crate::pieces::put_file_pieces_sealed(
            net,
            store,
            file,
            MAX_INPUT_BYTES,
            INPUT_PIECE_BYTES,
        )
        .await?;
        stored.extend(pieces.pieces.iter().cloned());
        stored.push(pieces.hash.clone());
        let obj = spec.as_object_mut().expect("object");
        obj.extend(pieces.sealed_spec_fields(prefix, &key));
        obj.insert(format!("{prefix}_provider_peer_id"), json!(me));
    }
    parse_spec(&spec).map_err(anyhow::Error::msg)?;
    Ok(spec)
}

/// Fetch, open and check one artifact into `dest`.
async fn fetch_artifact(
    net: &Arc<Libp2pNetwork>,
    outcome: &crate::AuctionOutcome,
    a: &Artifact,
    max: u64,
    dest: &Path,
) -> Result<()> {
    if a.size > max {
        bail!("artifact is {} bytes, over the {max}-byte limit", a.size);
    }
    let sealed = dest.with_extension("sealed");
    let sealed_max = crate::job_e2e::sealed_len(max) as usize;
    let fetched = crate::gateway_util::fetch_stored_result_to_file(
        net,
        &a.stored,
        &outcome.accepted_bid.bidder_peer_id,
        sealed_max,
        Duration::from_secs(120),
        &sealed,
    )
    .await;
    let opened = match fetched {
        Ok(_) => crate::job_e2e::open_file(&outcome.result_key, &sealed, dest).await,
        Err(e) => Err(e),
    };
    let _ = tokio::fs::remove_file(&sealed).await;
    let n = opened?;
    let path = dest.to_path_buf();
    let hash = tokio::task::spawn_blocking(move || -> Result<String> {
        use std::io::Read;
        let mut f = std::fs::File::open(&path)?;
        let mut h = blake3::Hasher::new();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let k = f.read(&mut buf)?;
            if k == 0 {
                break;
            }
            h.update(&buf[..k]);
        }
        Ok(h.finalize().to_hex().to_string())
    })
    .await??;
    if n != a.size || hash != a.hash {
        let _ = tokio::fs::remove_file(dest).await;
        bail!("artifact does not match the size and hash the worker reported");
    }
    Ok(())
}

/// Run one `container.run` job: store the blobs, auction (trusted builders
/// only unless `any_worker`), stream the worker's log through `progress`,
/// then fetch and check every artifact into `out_dir`.
pub async fn submit(
    net: Arc<Libp2pNetwork>,
    store: ChunkStore,
    req: Request,
    out_dir: &Path,
    progress: Progress,
) -> Result<Outcome> {
    if req.builders.is_empty() && !req.any_worker {
        bail!(
            "no trusted builders: pass --peer <peer id> (or set [containers] builders / C0MPUTE_BUILDERS), or --any-worker to accept any builder"
        );
    }
    // Connect to the named builders up front: they fetch the inputs from us
    // and we fetch the results from them, over this connection when one of
    // us cannot be dialed, and gossip reaches them even where no seed relays
    // the topic yet. A builder may be named by peer id or by a full
    // multiaddr (`/ip4/1.2.3.4/tcp/4001/p2p/<id>`).
    let (builders, addrs) = split_builders(&req.builders)?;
    let mut req = req;
    req.builders = builders;
    let mut dialed = false;
    for addr in addrs {
        match net.dial(addr.clone()).await {
            Ok(()) => dialed = true,
            Err(e) => progress(&format!("could not dial {addr}: {e}")),
        }
    }
    for b in &req.builders {
        if let Ok(peer) = b.parse::<PeerId>()
            && !net.is_connected(peer).await
        {
            match net.connect(peer).await {
                Ok(()) => dialed = true,
                Err(e) => progress(&format!(
                    "could not dial builder {b} yet ({e}); trying through the network"
                )),
            }
        }
    }
    if dialed {
        // Let the connections come up and swap topic subscriptions.
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    let mut stored = Vec::new();
    progress("sealing and storing the inputs");
    let spec = spec_for(&net, &store, &req, &mut stored).await?;
    let parsed = parse_spec(&spec).map_err(anyhow::Error::msg)?;
    // Input fetch and the result upload on top of the job's own timeout.
    let wait = parsed.timeout + Duration::from_secs(20 * 60);
    let (notify_tx, mut notify_rx) =
        tokio::sync::mpsc::unbounded_channel::<(String, c0mpute_net::topics::JobBid)>();
    let stream = {
        let net = net.clone();
        let progress = progress.clone();
        tokio::spawn(async move {
            let Some((job_id, bid)) = notify_rx.recv().await else {
                return;
            };
            progress(&format!("job {job_id} accepted by {}", bid.bidder_peer_id));
            let Ok(worker) = bid.bidder_peer_id.parse::<PeerId>() else {
                return;
            };
            let mut from = 0u64;
            let mut misses = 0;
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                match fetch_log(&net, worker, &job_id, from).await {
                    Ok(r) if r.known => {
                        misses = 0;
                        for line in r.text.lines() {
                            progress(line);
                        }
                        from = r.next;
                        if r.done {
                            return;
                        }
                    }
                    _ => {
                        misses += 1;
                        if misses > 30 {
                            progress("(worker log unavailable; waiting for the result)");
                            return;
                        }
                    }
                }
            }
        })
    };
    let mut last_err = None;
    let mut outcome = None;
    for attempt in 1..=3 {
        progress(&format!(
            "auction (attempt {attempt}): asking {}",
            if req.builders.is_empty() {
                "any builder".to_string()
            } else {
                req.builders.join(", ")
            }
        ));
        let auction = crate::JobAuction::new(WORKLOAD, spec.clone())
            .with_required_capabilities(vec![ROLE_TAG.to_string()])
            .with_allowed_bidders(req.builders.clone())
            .with_max_price_usd(req.max_price_usd)
            .with_bid_window(Duration::from_secs(4))
            .with_deadline(wait)
            .with_receipt_timeout(wait)
            .with_accept_notify(notify_tx.clone());
        match crate::run_auction(net.clone(), auction).await {
            Ok(o) => {
                outcome = Some(o);
                break;
            }
            Err(e) => {
                let no_bids = matches!(
                    e.downcast_ref::<crate::buyer::AuctionFailure>(),
                    Some(crate::buyer::AuctionFailure::NoBids(_)) | None
                );
                last_err = Some(e);
                if !no_bids {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
    let forget_inputs = || async {
        for h in &stored {
            if let Ok(h) = Hash::from_hex(h) {
                let _ = store.delete(&h).await;
            }
        }
    };
    let Some(outcome) = outcome else {
        stream.abort();
        forget_inputs().await;
        return Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no builder bid")));
    };
    // Let the log stream print its last lines.
    let _ = tokio::time::timeout(Duration::from_secs(5), stream).await;
    forget_inputs().await;
    let worker = outcome.accepted_bid.bidder_peer_id.clone();
    if outcome.receipt.status != JobStatus::Completed {
        bail!(
            "builder {worker} reported the job failed (job {})",
            outcome.job_id
        );
    }
    progress("fetching the result");
    let bytes = crate::gateway_util::fetch_result(
        &net,
        &outcome,
        MAX_RESULT_BYTES,
        Duration::from_secs(120),
    )
    .await
    .map_err(|e| anyhow::anyhow!("fetch the result from {worker}: {e:?}"))?;
    let result: Value = serde_json::from_slice(&bytes).context("worker result is not JSON")?;
    if result.get("v").and_then(Value::as_u64) != Some(1)
        || !result.get("ok").is_some_and(Value::is_boolean)
    {
        bail!("worker result does not match the container.run schema");
    }
    tokio::fs::create_dir_all(out_dir).await?;
    let mut image = None;
    if let Some(a) = result.get("image").filter(|i| i.get("stored").is_some()) {
        let a: Artifact = serde_json::from_value(a.clone()).context("image entry")?;
        let dest = out_dir.join(format!("image-{}.tar.gz", short_id(&outcome.job_id)));
        progress(&format!("downloading the image ({} MiB)", a.size >> 20));
        fetch_artifact(&net, &outcome, &a, MAX_IMAGE_BYTES, &dest)
            .await
            .context("fetch the image")?;
        image = Some(dest);
    }
    let mut outputs = Vec::new();
    if let Some(list) = result.get("outputs").and_then(Value::as_array) {
        for (i, entry) in list.iter().enumerate() {
            if entry.get("stored").is_none() {
                continue;
            }
            let a: Artifact = serde_json::from_value(entry.clone()).context("output entry")?;
            let path = a.path.clone().unwrap_or_default();
            let dest = out_dir.join(format!("output-{i}.tar.gz"));
            progress(&format!("downloading output {path} ({} KiB)", a.size >> 10));
            fetch_artifact(&net, &outcome, &a, MAX_OUTPUT_BYTES, &dest)
                .await
                .with_context(|| format!("fetch output {path}"))?;
            outputs.push((path, dest));
        }
    }
    Ok(Outcome {
        job_id: outcome.job_id.clone(),
        worker,
        price_usd: outcome.accepted_bid.price_usd,
        result,
        image,
        outputs,
    })
}

/// Builders as given (peer ids or `/.../p2p/<id>` multiaddrs) -> the peer
/// ids to accept bids from, plus the addresses to dial.
pub fn split_builders(list: &[String]) -> Result<(Vec<String>, Vec<c0mpute_net::Multiaddr>)> {
    let mut ids = Vec::new();
    let mut addrs = Vec::new();
    for b in list {
        let b = b.trim();
        if b.starts_with('/') {
            let addr: c0mpute_net::Multiaddr = b
                .parse()
                .with_context(|| format!("{b:?} is not a multiaddr"))?;
            let id = addr
                .iter()
                .find_map(|p| match p {
                    c0mpute_net::Protocol::P2p(id) => Some(id.to_base58()),
                    _ => None,
                })
                .with_context(|| format!("{b} names no /p2p/<peer id>"))?;
            ids.push(id);
            addrs.push(addr);
        } else {
            b.parse::<PeerId>()
                .with_context(|| format!("{b:?} is not a peer id"))?;
            ids.push(b.to_string());
        }
    }
    Ok((ids, addrs))
}

/// Hold an exclusive lock on `<dir>/lock` while this process uses the
/// identity in `dir`: two buyers with one identity on the network see each
/// other's subscriptions as their own and lose auctions.
pub struct IdentityLock(#[allow(dead_code)] std::fs::File);

pub fn lock_identity(dir: &Path, progress: &Progress) -> Result<IdentityLock> {
    std::fs::create_dir_all(dir)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("lock"))?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: flock on a valid descriptor we own.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            progress(&format!(
                "another build is using {}; waiting for it",
                dir.display()
            ));
            // SAFETY: as above, blocking.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                bail!("could not lock {}", dir.display());
            }
        }
    }
    Ok(IdentityLock(file))
}

/// The identity directory `c0mpute build` / `run` use: `C0MPUTE_BUILD_IDENTITY`
/// or `<data dir>/build`. Its peer id is what builders allowlist.
pub fn identity_dir() -> PathBuf {
    std::env::var_os("C0MPUTE_BUILD_IDENTITY")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            crate::config::data_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("build")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_contexts_leave_credentials_home() {
        let src = tempfile::tempdir().unwrap();
        let d = src.path();
        std::fs::write(d.join("Dockerfile"), "FROM scratch").unwrap();
        std::fs::write(d.join(".env"), "SECRET=1").unwrap();
        std::fs::write(d.join(".env.example"), "SECRET=").unwrap();
        std::fs::write(d.join(".npmrc"), "//registry/:_authToken=x").unwrap();
        std::fs::create_dir_all(d.join("deploy/.ssh")).unwrap();
        std::fs::write(d.join("deploy/.ssh/config"), "x").unwrap();
        std::fs::write(d.join("deploy/id_ed25519"), "key").unwrap();
        std::fs::write(d.join("deploy/run.sh"), "echo").unwrap();
        let out = tempfile::tempdir().unwrap();
        let dest = out.path().join("ctx.tar.gz");
        pack_dir(d, &dest, false).unwrap();
        let list = std::process::Command::new("tar").arg("-tzf").arg(&dest).output().unwrap();
        let list = String::from_utf8_lossy(&list.stdout);
        for kept in ["Dockerfile", ".env.example", "deploy/run.sh"] {
            assert!(list.contains(kept), "{kept} is packed: {list}");
        }
        for gone in ["./.env\n", ".npmrc", ".ssh", "id_ed25519"] {
            assert!(!list.contains(gone), "{gone:?} stays home: {list}");
        }
    }

    fn hash() -> String {
        Hash::of(b"ctx").to_hex()
    }

    fn build_spec() -> Value {
        json!({"v":1,"mode":"build","build":{"dockerfile":"Dockerfile","build_args":{"A":"1"}},"context_hash":hash()})
    }

    #[test]
    fn parses_build_and_run_specs() {
        let p = parse_spec(&build_spec()).unwrap();
        assert!(p.is_build() && p.returns_image());
        assert_eq!(p.timeout, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
        let digest = "a".repeat(64);
        let run = json!({"v":1,"mode":"run","image":format!("busybox@sha256:{digest}"),"cmd":["sh","-c","echo hi"],"outputs":["out"],"input_hash":hash()});
        let p = parse_spec(&run).unwrap();
        assert!(!p.is_build() && !p.returns_image() && p.input.is_some());
        assert_eq!(p.workdir, "/work");
    }

    #[test]
    fn refuses_bad_specs() {
        let err = |v: Value| parse_spec(&v).unwrap_err();
        assert!(err(json!({"v":2,"mode":"build"})).contains("version"));
        assert!(err(json!({"v":1,"mode":"exec"})).contains("unknown mode"));
        assert!(err(json!({"v":1,"mode":"build"})).contains("needs `build`"));
        assert!(err(json!({"v":1,"mode":"build","build":{}})).contains("context"));
        assert!(
            err(json!({"v":1,"mode":"run","image":"busybox:latest","cmd":["x"]}))
                .contains("digest")
        );
        assert!(err(json!({"v":1,"mode":"run","cmd":["x"]})).contains("image"));
        let mut s = build_spec();
        s["build"]["dockerfile"] = json!("../Dockerfile");
        assert!(err(s).contains("dockerfile"));
        let mut s = build_spec();
        s["build"]["build_args"] = json!({"1BAD":"x"});
        assert!(err(s).contains("key"));
        let mut s = build_spec();
        s["build"]["target"] = json!("--evil");
        assert!(err(s).contains("target"));
        let mut s = build_spec();
        s["timeout_secs"] = json!(MAX_TIMEOUT_SECS + 1);
        assert!(err(s).contains("timeout"));
        let mut s = build_spec();
        s["build"]["extra"] = json!(1);
        assert!(err(s).contains("invalid"));
        let digest = "a".repeat(64);
        let run = json!({"v":1,"mode":"run","image":format!("busybox@sha256:{digest}"),"cmd":["x"],"outputs":["../etc"]});
        assert!(err(run).contains("outputs"));
        let run = json!({"v":1,"mode":"run","image":format!("busybox@sha256:{digest}"),"cmd":["x"],"context_hash":hash()});
        assert!(err(run).contains("only for builds"));
    }

    #[test]
    fn pinned_images() {
        let d = "0123456789abcdef".repeat(4);
        assert!(valid_pinned_image(&format!(
            "ghcr.io/org/app:1.2@sha256:{d}"
        )));
        assert!(!valid_pinned_image("busybox:1.36"));
        assert!(!valid_pinned_image(&format!("-x@sha256:{d}")));
        assert!(!valid_pinned_image(&format!("Busybox@sha256:{d}")));
        assert!(!valid_pinned_image("busybox@sha256:abc"));
    }

    #[test]
    fn docker_argv_has_no_flag_injection_and_hard_limits() {
        let b = BuildSpec {
            dockerfile: "docker/Dockerfile".into(),
            build_args: [("NEXT_PUBLIC_X".to_string(), "a b".to_string())].into(),
            target: Some("runner".into()),
        };
        let a = build_args(&b, "c0mpute-job/x:build", Path::new("/w/iid"), true, "job");
        assert!(a.contains(&"--file=docker/Dockerfile".to_string()));
        assert!(a.contains(&"--build-arg=NEXT_PUBLIC_X=a b".to_string()));
        assert!(a.contains(&"--target=runner".to_string()));
        assert!(a.contains(&"--network=default".to_string()));
        assert!(!a.iter().any(|x| x.contains("privileged")
            || x.contains("security.insecure")
            || x.contains("--allow")));
        assert_eq!(a.last().unwrap(), "-");

        let digest = "a".repeat(64);
        let p = parse_spec(&json!({"v":1,"mode":"run","image":format!("busybox@sha256:{digest}"),"cmd":["-rf","x"],"env":{"K":"v"},"memory_mb":512,"cpus":1.5})).unwrap();
        let a = create_args(&p, "c0mpute-job-x", "img", JOBS_NETWORK, "job");
        assert!(a.contains(&"--entrypoint=-rf".to_string()));
        assert!(
            a.contains(&"--memory=512m".to_string())
                && a.contains(&"--memory-swap=512m".to_string())
        );
        assert!(a.contains(&"--cpus=1.5".to_string()));
        assert!(a.contains(&"--cap-drop=ALL".to_string()));
        assert!(a.contains(&"--security-opt=no-new-privileges".to_string()));
        assert!(!a.iter().any(|x| x.starts_with("--volume")
            || x.starts_with("-v")
            || x.starts_with("--mount")
            || x.contains("privileged")));
        let img = a.iter().position(|x| x == "img").unwrap();
        assert_eq!(&a[img + 1..], &["x".to_string()]);
    }

    #[test]
    fn builders_by_id_or_multiaddr() {
        let id = "12D3KooWLs9iJBPDWwKkeCBNdvS5bj2egBE4jDPnmTkE6rkb7SBE";
        let (ids, addrs) =
            split_builders(&[id.to_string(), format!("/ip4/1.2.3.4/tcp/4001/p2p/{id}")]).unwrap();
        assert_eq!(ids, vec![id.to_string(), id.to_string()]);
        assert_eq!(addrs.len(), 1);
        assert!(split_builders(&["/ip4/1.2.3.4/tcp/4001".into()]).is_err());
        assert!(split_builders(&["nope".into()]).is_err());
    }

    #[test]
    fn output_paths() {
        assert_eq!(output_path("/work", "dist"), "/work/dist");
        assert_eq!(output_path("/work/", "/out/a"), "/out/a");
    }

    #[test]
    fn guard_rules_cover_metadata_and_host() {
        let rules = guard_rules(JOBS_BRIDGE);
        assert!(
            rules
                .iter()
                .any(|r| r.contains(&"169.254.0.0/16".to_string()))
        );
        assert!(rules.iter().any(|r| r[0] == "INPUT"));
        assert!(rules.iter().all(|r| r.contains(&JOBS_BRIDGE.to_string())));
    }

    #[test]
    fn dockerignore_translation() {
        let ex = dockerignore_excludes("# c\nnode_modules\n**/target\n/.git/\n*.log\n");
        assert_eq!(
            ex,
            vec![
                "--no-wildcards-match-slash",
                "--anchored",
                "--exclude=./node_modules",
                "--exclude=./.git",
                "--exclude=./*.log",
                "--no-anchored",
                "--exclude=target"
            ]
        );
        assert!(dockerignore_excludes("node_modules\n!node_modules/keep\n").is_empty());
        assert!(dockerignore_excludes("a/**/b\n").is_empty());
        assert!(dockerignore_excludes("\n# only comments\n").is_empty());
    }

    #[test]
    fn buyer_policy_is_closed_by_default() {
        let cfg = ContainersConfig::default();
        let p = BuyerPolicy::from_config(&cfg, ["me".to_string()]);
        assert!(!p.open && p.trusted.contains("me"));
        let open = BuyerPolicy::from_config(
            &ContainersConfig {
                open: true,
                ..Default::default()
            },
            [],
        );
        assert!(open.open);
    }

    #[test]
    fn reproducible_quorum() {
        let ids = vec![
            "sha256:a".to_string(),
            "sha256:a".to_string(),
            "sha256:b".to_string(),
        ];
        assert_eq!(verify_reproducible(&ids, 2).unwrap(), "sha256:a");
        assert!(verify_reproducible(&ids, 3).is_err());
        assert!(verify_reproducible(&[], 1).is_err());
    }

    #[test]
    fn live_log_is_for_the_buyer_only() {
        live_open("j-live-1", "buyer");
        live_line("j-live-1", "step 1");
        live_line("j-live-1", "step 2");
        assert!(
            !log_since(
                "other",
                &LogRequest {
                    job_id: "j-live-1".into(),
                    from: 0
                }
            )
            .known
        );
        let r = log_since(
            "buyer",
            &LogRequest {
                job_id: "j-live-1".into(),
                from: 0,
            },
        );
        assert!(r.known && r.text.contains("step 1") && r.text.contains("step 2") && !r.done);
        let r2 = log_since(
            "buyer",
            &LogRequest {
                job_id: "j-live-1".into(),
                from: r.next,
            },
        );
        assert!(r2.text.is_empty());
        live_done("j-live-1");
        assert!(
            log_since(
                "buyer",
                &LogRequest {
                    job_id: "j-live-1".into(),
                    from: r.next
                }
            )
            .done
        );
    }

    #[test]
    fn log_tail_keeps_the_end() {
        let mut t = LogTail::new(10);
        t.push_line("0123456789");
        t.push_line("abc");
        assert_eq!(t.text(), "56789\nabc\n");
    }

    #[test]
    fn public_view_shows_limits_only() {
        let mut s = build_spec();
        s["cpus"] = json!(4);
        s["memory_mb"] = json!(8192);
        let v = crate::job_e2e::public_view(WORKLOAD, &s);
        assert_eq!(v["mode"], "build");
        assert_eq!(v["cpus"], 4);
        assert!(v.get("build").is_none() && v.get("context_hash").is_none());
    }
}
