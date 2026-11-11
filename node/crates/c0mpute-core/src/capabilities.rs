//! Capability advertisement + peer registry.
//!
//! Two halves:
//!
//!   1. **`advertise_loop`** — spawned per-worker. Builds a CapabilityAd
//!      from local config + hardware probe and publishes it to the
//!      `c0mpute/cap/v1` gossipsub topic at startup and every `interval`
//!      thereafter. Sets a TTL implicitly via `published_at_ms`;
//!      receivers ignore ads older than `MAX_AD_AGE`.
//!
//!   2. **`Registry`** — keeps a HashMap<PeerId, CapabilityAd> populated
//!      from inbound `c0mpute/cap/v1` messages. Exposes `find_with_tag`
//!      so the rest of the daemon can do "find peers with capability X"
//!      lookups for job dispatch / shard placement.
//!
//! Real-world capability tags use colon-separated hierarchy:
//!
//!   c0mpute:role:storage
//!   c0mpute:role:transcode
//!   c0mpute:role:gateway
//!   c0mpute:transcode:h264:nvenc
//!   c0mpute:gpu:nvidia
//!   c0mpute:storage:hot
//!
//! The `tags_from_config` helper builds an opinionated default set from
//! a `Config`. Workers can override via `[worker.capabilities]` in their
//! config file (not yet exposed; lands when we wire that knob).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use c0mpute_net::topics::{CAPABILITY_TOPIC, CapabilityAd};
use c0mpute_net::{GossipMessage, Libp2pNetwork, PeerId};
use c0mpute_proto::Role;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::Config;

/// Reject capability ads older than this. Loose enough that a node
/// publishing every 60s with some clock skew still works; tight enough
/// that stale peer state expires within a few minutes.
pub const MAX_AD_AGE: Duration = Duration::from_secs(5 * 60);

/// Default re-advertise interval. Each worker republishes its
/// CapabilityAd this often so receivers' TTLs don't expire.
pub const DEFAULT_ADVERTISE_INTERVAL: Duration = Duration::from_secs(60);

/// Build a default capability-tag set from config: one `c0mpute:role:*` per
/// configured role, plus exactly one hardware tag so schedulers can match
/// GPU-only work (transcode/inference) to GPU boxes.
pub fn tags_from_config(config: &Config) -> Vec<String> {
    let mut tags = role_tags(config);
    tags.push(detect_hardware_tag());
    tags
}

/// The role half of the tag set, without the hardware tag.
///
/// Kept separate because it is cheap and can change while a worker runs: a
/// node that finishes provisioning Kokoro a minute after boot should start
/// advertising `c0mpute:role:tts` on the next tick rather than at the next
/// restart. The hardware tag, which shells out to `nvidia-smi`, is probed
/// once and reused.
pub fn role_tags(config: &Config) -> Vec<String> {
    // Not a workload: this node takes end-to-end encrypted jobs (DIP-0032).
    let mut tags = vec![crate::job_e2e::JOB_E2E_TAG.to_string()];
    if crate::job_e2e::attestation_provider().is_some() {
        tags.push(crate::job_e2e::CONFIDENTIAL_TAG.to_string());
    }
    // Not a workload: shows on the node's card that it donates idle compute.
    if crate::crowdgpt::donating() {
        tags.push(crate::crowdgpt::DONATE_TAG.to_string());
    }
    // Not a workload: the operator opted into adult (18+) image and video
    // jobs. Buyers require it on adult offers and award them only to nodes
    // that advertise it.
    if config.adult.serve_enabled() {
        tags.push(crate::content_policy::ADULT_TAG.to_string());
    }
    for role in &config.roles {
        match role {
            Role::Tts => {
                let engines = crate::speech::available_tts_engines();
                if !engines.is_empty() {
                    tags.push("c0mpute:role:tts".to_string());
                    tags.extend(
                        engines
                            .into_iter()
                            .map(|engine| format!("c0mpute:tts:engine:{engine}")),
                    );
                }
            }
            Role::Stt => {
                if crate::speech::stt_ready() {
                    tags.push("c0mpute:role:stt".to_string());
                    tags.push("c0mpute:stt:engine:faster-whisper".to_string());
                }
            }
            Role::Storage => tags.push("c0mpute:role:storage".to_string()),
            Role::Transcode => tags.push("c0mpute:role:transcode".to_string()),
            Role::Gateway => tags.push("c0mpute:role:gateway".to_string()),
            Role::Verifier => tags.push("c0mpute:role:verifier".to_string()),
            Role::WorkflowStep => tags.push(crate::workflow::ROLE_TAG.to_string()),
            Role::Crawl => tags.push(crate::crawl::ROLE_TAG.to_string()),
            Role::Hosting => tags.push(crate::hosting::ROLE_TAG.to_string()),
            Role::Apps => tags.push(crate::apps::ROLE_TAG.to_string()),
            Role::Ocr => {
                // Only where `tesseract` actually runs (probed once).
                if crate::ocr::runtime().ready() {
                    tags.push(crate::ocr::ROLE_TAG.to_string());
                }
            }
            // Docker works, a public address is known and a game image is
            // present (probed at most once a minute).
            Role::Gameserver => tags.extend(crate::gameserver::role_tags(config)),
            Role::Captcha => {
                // Appears once background provisioning finishes.
                if crate::captcha::ready() {
                    tags.push(crate::captcha::ROLE_TAG.to_string());
                }
            }
            Role::Embed => {
                // Like TTS: appears once background provisioning finishes.
                let models = crate::embed::ready_models();
                if !models.is_empty() {
                    tags.push(crate::embed::ROLE_TAG.to_string());
                    tags.extend(models.into_iter().map(crate::embed::model_tag));
                }
            }
            Role::LiveStream => tags.push(crate::live::ROLE_TAG.to_string()),
            Role::Inference => {
                // Only while the local runtime lists chat models (refreshed
                // every minute), one tag per model.
                let models = crate::inference::ready_models();
                if !models.is_empty() {
                    tags.push(crate::inference::ROLE_TAG.to_string());
                    tags.extend(models.iter().map(|m| crate::inference::model_tag(m)));
                }
            }
            Role::Mailbox => tags.push(crate::dm::ROLE_TAG.to_string()),
            Role::Render => {
                // Only where `blender` runs. The probe (a few seconds) runs
                // once at worker start; until it finishes, nothing is shown.
                if let Some(rt) = crate::render::probed() {
                    tags.extend(rt.tags());
                }
            }
            Role::Video => {
                // Same rule as TTS: advertise only what this box can do now.
                // A node without the GPU, the runtime or the image would win
                // auctions and fail them.
                // One model tag per model the card can hold. With no GPU,
                // CPU video (opt-in) advertises the small Wan model while
                // the box is idle with free RAM, re-checked each tick.
                if crate::video::readiness_cached(crate::video::RUNNER_IMAGE).is_none() {
                    tags.extend(crate::video::gpu_tags());
                } else if crate::video_cpu::can_serve(&config.video) {
                    tags.extend(crate::video_cpu::tags());
                }
            }
            Role::Diffusion => {
                // Installed AND enough free memory right now: re-checked each
                // tick, so a box that gets busy stops winning image jobs.
                if crate::diffusion::can_serve(&config.diffusion) {
                    tags.push(crate::diffusion::ROLE_TAG.to_string());
                    tags.push(crate::diffusion::MODEL_TAG.to_string());
                }
            }
            Role::Music => {
                // Same rule as diffusion: installed, idle and with free memory now.
                if crate::music::can_serve(&config.music) {
                    tags.push(crate::music::ROLE_TAG.to_string());
                    tags.push(crate::music::MODEL_TAG.to_string());
                }
            }
        }
    }
    tags
}

/// Detect the host's compute hardware and return the matching capability tag.
/// Exactly one is returned; a box with no usable accelerator advertises
/// `c0mpute:cpu`. Runs once at startup (the tag set is built then passed to
/// the advertise loop), so a one-off `nvidia-smi`/`rocm-smi` probe is cheap.
pub fn detect_hardware_tag() -> String {
    // Apple Silicon — Metal GPU on macOS aarch64.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        return "c0mpute:gpu:apple".to_string();
    }
    if nvidia_present() {
        return "c0mpute:gpu:nvidia".to_string();
    }
    if amd_present() {
        return "c0mpute:gpu:amd".to_string();
    }
    "c0mpute:cpu".to_string()
}

/// NVIDIA GPU present — a device node or a working `nvidia-smi -L`.
pub fn nvidia_present() -> bool {
    if std::path::Path::new("/dev/nvidia0").exists() {
        return true;
    }
    std::process::Command::new("nvidia-smi")
        .arg("-L")
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false)
}

/// AMD ROCm GPU present — the KFD device node or a working `rocm-smi`.
fn amd_present() -> bool {
    if std::path::Path::new("/dev/kfd").exists() {
        return true;
    }
    std::process::Command::new("rocm-smi")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Where `c0mpute bench` leaves its report for this node.
pub fn bench_report_path() -> Option<std::path::PathBuf> {
    crate::config::data_dir().map(|d| c0mpute_bench::path_in(&d))
}

/// The score this node advertises: the last *full* `c0mpute bench` run, or
/// `None` when the operator has never benchmarked (quick runs don't count).
pub fn local_bench_score() -> Option<u32> {
    bench_report_path().and_then(|p| c0mpute_bench::advertised_score(&p))
}

/// Build the per-host hardware blob attached to capability ads. Today
/// it's a coarse summary plus whatever the last `c0mpute bench` run
/// recorded about the host; future: probe ffmpeg encoders, GPU vendor,
/// free disk, free VRAM, etc.
pub fn hardware_blob(config: &Config) -> serde_json::Value {
    let mut blob = serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "free_disk_root": config.storage.root.display().to_string(),
    });
    let (cpu, cores, ram_bytes) = c0mpute_bench::hardware_summary();
    blob["cpu"] = serde_json::Value::String(cpu);
    blob["cores"] = serde_json::Value::from(cores);
    if let Some(ram_bytes) = ram_bytes {
        blob["ram_bytes"] = serde_json::Value::from(ram_bytes);
    }
    if let Some(report) = bench_report_path().and_then(|p| c0mpute_bench::load(&p).ok().flatten()) {
        blob["bench_at"] = serde_json::Value::String(report.metadata.start_time.clone());
        if !report.metadata.quick && report.score > 0 {
            blob["benchmark"] = benchmark_details(&report);
        }
    }
    if let Some(code) = referral_code() {
        blob["ref"] = serde_json::Value::String(code);
    }
    if let Some(name) = node_name() {
        blob["name"] = serde_json::Value::String(name);
    }
    blob["pricing"] = config.pricing.listing();
    if let Some(origin) = announced_gateway(config) {
        blob["gateway"] = serde_json::Value::String(origin);
    }
    if config.roles.contains(&c0mpute_proto::Role::Storage) {
        let cap = crate::opendisk::capacity(&config.storage, 0);
        blob["storage"] = serde_json::json!({
            "free_bytes": cap.free_bytes,
            "total_bytes": cap.total_bytes,
            "per_gib_month": crate::opendisk::price(&config.storage),
        });
    }
    blob
}

/// The small, public slice of the full local report that the node board needs.
/// Compiler and kernel details stay in the local report, not in capability adverts.
fn benchmark_details(report: &c0mpute_bench::BenchReport) -> serde_json::Value {
    let mut workloads = serde_json::Map::new();
    if let Some(results) = report.results.get(c0mpute_bench::RUNTIME) {
        for name in c0mpute_bench::WORKLOADS {
            let Some(samples) = results.get(name) else {
                continue;
            };
            let rows: Vec<serde_json::Value> = samples
                .iter()
                .take(16)
                .map(|sample| {
                    serde_json::json!({
                        "threads": sample.threads,
                        "duration_us": sample.result.duration_us,
                        "speedup": sample.result.speedup,
                        "throughput": sample.result.throughput,
                    })
                })
                .collect();
            workloads.insert(
                name.to_string(),
                serde_json::json!({
                    "params": samples.first()
                        .map(|sample| sample.params.chars().take(80).collect::<String>())
                        .unwrap_or_default(),
                    "samples": rows,
                }),
            );
        }
    }
    serde_json::json!({
        "run_at": report.metadata.start_time,
        "workloads": workloads,
    })
}

/// The referral code this node was installed with, if any.
///
/// `C0MPUTE_REF` wins; otherwise `~/.c0mpute/ref` (or `$C0MPUTE_HOME/ref`),
/// which `install.sh` writes when it is run as
/// `curl -fsSL https://c0mpute.com/install.sh | C0MPUTE_REF=<code> sh`.
/// The code is a public handle chosen by whoever shared the link. It rides
/// in the hardware blob of the (already public) capability ad so the status
/// aggregator can credit the referrer on c0mpute.com/nodes. It carries no
/// payout and proves nothing about identity.
pub fn referral_code() -> Option<String> {
    handle_setting("C0MPUTE_REF", "ref")
}

/// The operator nickname this node shows on c0mpute.com/nodes, if any.
///
/// `C0MPUTE_NAME` wins; otherwise `~/.c0mpute/name` (or `$C0MPUTE_HOME/name`),
/// written by `c0mpute name <handle>` or `install.sh --name <handle>`. Give
/// every machine you run the same name and the board groups them under it.
/// Unlike `ref` (who invited you), this is who runs the node. Same rules as a
/// referral handle: public, unverified, lowercase `[a-z0-9_-]`, up to 32.
pub fn node_name() -> Option<String> {
    handle_setting("C0MPUTE_NAME", "name")
}

/// Where `c0mpute name` stores the nickname: `$C0MPUTE_HOME/name`, else
/// `~/.c0mpute/name`.
pub fn node_name_path() -> Option<std::path::PathBuf> {
    c0mpute_home().map(|h| h.join("name"))
}

fn c0mpute_home() -> Option<std::path::PathBuf> {
    std::env::var_os("C0MPUTE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".c0mpute")))
}

/// An env var, else a one-line file under the c0mpute home, sanitized.
fn handle_setting(env: &str, file: &str) -> Option<String> {
    let raw = std::env::var(env)
        .ok()
        .or_else(|| std::fs::read_to_string(c0mpute_home()?.join(file)).ok())?;
    sanitize_ref(&raw)
}

/// The public HTTP origin this node's gateway answers on, when its operator
/// published one (`gateway.announce`) and the node runs the gateway role.
///
/// Opt-in by construction: nothing is advertised unless the operator typed the
/// address into their config, which they already do to be listed on d1sks or
/// to front custom domains. Never in private mode. This is what lets a client
/// that is not a libp2p peer (the Moshpit c0mpute edge, for one) find gateways
/// to fetch `/s/<site>/` from without a central list of them.
pub fn announced_gateway(config: &Config) -> Option<String> {
    if config.private.enabled || !config.roles.contains(&c0mpute_proto::Role::Gateway) {
        return None;
    }
    match config.gateway.announce.as_deref() {
        Some(raw) => gateway_origin(raw),
        // Found and checked by `gateway_announce` (a reachable public IPv4).
        None if crate::gateway_announce::auto_enabled(config) => crate::gateway_announce::auto_gateway(),
        None => None,
    }
}

/// `scheme://host[:port]` of an http(s) URL with a host and no credentials,
/// at most 200 characters, or nothing. Shared with the status aggregator,
/// which re-checks every ad before it publishes one.
pub fn gateway_origin(raw: &str) -> Option<String> {
    let url = reqwest::Url::parse(raw.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    let origin = url.origin().ascii_serialization();
    (origin.len() <= 200).then_some(origin)
}

/// Lowercase `[a-z0-9_-]`, 1..=32 chars, or nothing.
pub fn sanitize_ref(raw: &str) -> Option<String> {
    let code = raw.trim().to_ascii_lowercase();
    let ok = !code.is_empty()
        && code.len() <= 32
        && code
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    ok.then_some(code)
}

/// Periodically publish this worker's capability ad.
pub async fn advertise_loop(
    net: Arc<Libp2pNetwork>,
    tags: Vec<String>,
    hardware: serde_json::Value,
    bench_score: Option<u32>,
    interval: Duration,
) {
    advertise(net, TagSource::Fixed(tags), hardware, bench_score, interval).await
}

/// Like [`advertise_loop`], but re-derives the role tags from `config` on
/// every tick, so TTS appearing once Kokoro finishes installing is advertised
/// within one interval instead of needing a worker restart.
pub async fn advertise_loop_live(
    net: Arc<Libp2pNetwork>,
    config: Config,
    hardware_tag: String,
    hardware: serde_json::Value,
    bench_score: Option<u32>,
    interval: Duration,
) {
    advertise(
        net,
        TagSource::Live {
            config: Box::new(config),
            hardware_tag,
        },
        hardware,
        bench_score,
        interval,
    )
    .await
}

/// Where each tick's tag set comes from.
enum TagSource {
    /// A set fixed at boot.
    Fixed(Vec<String>),
    /// Role tags re-derived from config every tick, plus the hardware tag
    /// probed once. Lets a capability that appears after boot be advertised
    /// without restarting the worker.
    Live {
        config: Box<Config>,
        hardware_tag: String,
    },
}

impl TagSource {
    fn tags(&self) -> Vec<String> {
        match self {
            Self::Fixed(tags) => tags.clone(),
            Self::Live {
                config,
                hardware_tag,
            } => {
                let mut tags = role_tags(config);
                tags.push(hardware_tag.clone());
                tags
            }
        }
    }
}

async fn advertise(
    net: Arc<Libp2pNetwork>,
    tag_source: TagSource,
    hardware: serde_json::Value,
    bench_score: Option<u32>,
    interval: Duration,
) {
    // Always subscribe before we publish — gossipsub requires the
    // publisher to be in the topic's mesh to actually fan out.
    if let Err(e) = net.subscribe(CAPABILITY_TOPIC).await {
        warn!(err = %e, "failed to subscribe to capability topic");
        return;
    }

    let peer_id = net.peer_id().to_base58();
    info!(
        topic = CAPABILITY_TOPIC,
        interval_secs = interval.as_secs(),
        "capability advertisement loop starting"
    );

    let mut ticker = tokio::time::interval(interval);
    loop {
        ticker.tick().await;
        let ad = CapabilityAd::with_score(
            peer_id.clone(),
            tag_source.tags(),
            with_instance(hardware.clone(), net.instance_id()),
            bench_score,
        );
        let payload = match serde_json::to_vec(&ad) {
            Ok(b) => b,
            Err(e) => {
                warn!(err = %e, "serialize capability ad");
                continue;
            }
        };
        match net.publish(CAPABILITY_TOPIC, payload).await {
            Ok(()) => debug!("published capability ad"),
            Err(e) => {
                // The first publish often fails because gossipsub mesh
                // hasn't been built yet. Log at debug, keep trying.
                debug!(err = %e, "capability publish failed (retrying next tick)");
            }
        }
    }
}

/// `hardware` plus this process's instance id and, while a twin (another
/// process with this identity) is active, what this one knows about it, so
/// c0mpute.com/nodes shows the duplicate.
pub fn with_instance(mut hardware: serde_json::Value, instance: &str) -> serde_json::Value {
    if let Some(obj) = hardware.as_object_mut() {
        obj.insert("instance".into(), serde_json::Value::String(instance.to_string()));
        match crate::twin::ad_field(instance) {
            Some(t) => {
                obj.insert("twin".into(), t);
            }
            None => {
                obj.remove("twin");
            }
        }
    }
    hardware
}

/// Instances in `last_ad` (instance id -> its latest ad's `published_at_ms`)
/// that advertised since `cutoff` and were not replaced. An instance whose
/// last ad predates a newer instance's start (the start time is in the id)
/// is the same worker restarted, e.g. by auto-update, not a twin: counting it
/// marked every restarted node "duplicate identity (2 processes)" for the
/// five minutes its old ad stayed fresh. Real twins keep advertising after
/// each other's start, so they still count within one advert interval.
pub fn live_instances(last_ad: &HashMap<String, u64>, cutoff: u64) -> usize {
    let newest_other_start = |me: &str| {
        last_ad
            .keys()
            .filter(|k| k.as_str() != me)
            .filter_map(|k| c0mpute_net::instance_started_ms(k))
            .max()
    };
    last_ad
        .iter()
        .filter(|(_, t)| **t >= cutoff)
        .filter(|(id, t)| newest_other_start(id).is_none_or(|s| **t >= s))
        .count()
}

/// In-memory registry of recently-seen peer capabilities.
#[derive(Clone, Default)]
pub struct Registry {
    inner: Arc<RwLock<HashMap<PeerId, CapabilityAd>>>,
    /// Per peer: instance id -> its latest ad's `published_at_ms`. More than
    /// one fresh instance is one identity running twice.
    instances: Arc<RwLock<HashMap<PeerId, HashMap<String, u64>>>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take the gossipsub broadcast stream and feed messages into the
    /// registry. Spawns a task that runs until the network is dropped.
    pub fn run(&self, net: Arc<Libp2pNetwork>) {
        let inner = self.inner.clone();
        let instances = self.instances.clone();
        tokio::spawn(async move {
            // Subscribe so we receive the topic.
            if let Err(e) = net.subscribe(CAPABILITY_TOPIC).await {
                warn!(err = %e, "registry: subscribe failed");
                return;
            }
            let mut rx = net.messages();
            loop {
                let msg: GossipMessage = match rx.recv().await {
                    Ok(m) => m,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(skipped = n, "registry: broadcast channel lagged");
                        continue;
                    }
                    Err(_) => {
                        info!("registry: gossip channel closed");
                        return;
                    }
                };
                if msg.topic != CAPABILITY_TOPIC {
                    continue;
                }
                let ad: CapabilityAd = match serde_json::from_slice(&msg.data) {
                    Ok(a) => a,
                    Err(e) => {
                        debug!(err = %e, "registry: discard malformed ad");
                        continue;
                    }
                };
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                if now_ms.saturating_sub(ad.published_at_ms) > MAX_AD_AGE.as_millis() as u64 {
                    debug!(peer = %ad.peer_id, "registry: discard stale ad");
                    continue;
                }
                let Some(source) = msg.source else {
                    debug!("registry: discard ad with unknown source peer");
                    continue;
                };
                if let Some(id) = ad.hardware.get("instance").and_then(|v| v.as_str()) {
                    let mut seen = instances.write().await;
                    let mine = seen.entry(source).or_default();
                    if !mine.contains_key(id) && mine.len() < 16 {
                        // A process that last advertised before this one
                        // started was restarted (auto-update, `worker
                        // restart`), not twinned.
                        let started = c0mpute_net::instance_started_ms(id).unwrap_or(0);
                        let fresh = mine
                            .values()
                            .filter(|t| now_ms.saturating_sub(**t) <= MAX_AD_AGE.as_millis() as u64)
                            .filter(|t| **t >= started)
                            .count();
                        if fresh > 0 {
                            warn!(peer = %source, instance = id, "registry: a second process advertises this peer id (duplicate identity)");
                        }
                    }
                    if mine.len() < 16 || mine.contains_key(id) {
                        mine.insert(id.to_string(), ad.published_at_ms);
                    }
                }
                let mut guard = inner.write().await;
                guard.insert(source, ad);
            }
        });

        // Spawn a separate eviction task that drops entries older than
        // MAX_AD_AGE every 30s.
        let inner = self.inner.clone();
        let instances = self.instances.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(30));
            loop {
                ticker.tick().await;
                let cutoff = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0)
                    .saturating_sub(MAX_AD_AGE.as_millis() as u64);
                let mut guard = inner.write().await;
                guard.retain(|_, ad| ad.published_at_ms >= cutoff);
                drop(guard);
                let mut seen = instances.write().await;
                seen.retain(|_, m| {
                    m.retain(|_, t| *t >= cutoff);
                    !m.is_empty()
                });
            }
        });
    }

    /// How many processes (instance ids) are advertising each peer id at
    /// once. More than one: the identity runs twice. See [`live_instances`].
    pub async fn instance_counts(&self) -> Vec<(PeerId, usize)> {
        let cutoff = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
            .saturating_sub(MAX_AD_AGE.as_millis() as u64);
        self.instances
            .read()
            .await
            .iter()
            .map(|(p, m)| (*p, live_instances(m, cutoff)))
            .collect()
    }

    pub async fn list(&self) -> Vec<(PeerId, CapabilityAd)> {
        self.inner
            .read()
            .await
            .iter()
            .map(|(p, a)| (*p, a.clone()))
            .collect()
    }

    pub async fn find_with_tag(&self, tag: &str) -> Vec<(PeerId, CapabilityAd)> {
        self.inner
            .read()
            .await
            .iter()
            .filter(|(_, a)| a.tags.iter().any(|t| t == tag))
            .map(|(p, a)| (*p, a.clone()))
            .collect()
    }

    pub async fn count(&self) -> usize {
        self.inner.read().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_restarted_worker_is_not_a_duplicate() {
        // The old process last advertised at 1_000_060; the new one started at 1_000_090.
        let m = HashMap::from([
            ("0000001000000-aaaaaaaa".to_string(), 1_000_060u64),
            ("0000001000090-bbbbbbbb".to_string(), 1_000_150u64),
        ]);
        assert_eq!(live_instances(&m, 0), 1);
    }

    #[test]
    fn twins_advertising_at_once_still_count() {
        let m = HashMap::from([
            ("0000001000000-aaaaaaaa".to_string(), 1_000_200u64),
            ("0000001000090-bbbbbbbb".to_string(), 1_000_150u64),
        ]);
        assert_eq!(live_instances(&m, 0), 2);
        // Stale ads drop out either way.
        assert_eq!(live_instances(&m, 1_000_160), 1);
    }

    #[test]
    fn unparseable_instance_ids_supersede_nothing() {
        let m = HashMap::from([("x".to_string(), 5u64), ("y".to_string(), 6u64)]);
        assert_eq!(live_instances(&m, 0), 2);
    }
    use c0mpute_net::{Libp2pNetwork, Multiaddr, NetworkConfig, Protocol};

    #[test]
    fn referral_codes_are_sanitized() {
        assert_eq!(sanitize_ref(" Chovy \n").as_deref(), Some("chovy"));
        assert_eq!(sanitize_ref("rig_42-a").as_deref(), Some("rig_42-a"));
        assert_eq!(sanitize_ref(""), None);
        assert_eq!(sanitize_ref("has space"), None);
        assert_eq!(sanitize_ref("<script>"), None);
        assert_eq!(sanitize_ref(&"a".repeat(33)), None);
    }

    #[test]
    fn gateway_origins_are_sanitized() {
        assert_eq!(
            gateway_origin(" http://1.2.3.4:7777/ ").as_deref(),
            Some("http://1.2.3.4:7777")
        );
        assert_eq!(
            gateway_origin("https://gw.example.org/some/path?x=1").as_deref(),
            Some("https://gw.example.org")
        );
        assert_eq!(gateway_origin("ftp://gw.example.org"), None);
        assert_eq!(gateway_origin("javascript:alert(1)"), None);
        assert_eq!(gateway_origin("http://user:pw@gw.example.org"), None);
        assert_eq!(gateway_origin("not a url"), None);
        let long = format!("https://{}.example.org", "a".repeat(250));
        assert_eq!(gateway_origin(&long), None);
    }

    #[test]
    fn gateway_is_advertised_only_when_announced_and_public() {
        let mut config = Config::default();
        config.gateway.announce = Some("http://203.0.113.7:7777".into());
        if !config.roles.contains(&c0mpute_proto::Role::Gateway) {
            config.roles.push(c0mpute_proto::Role::Gateway);
        }
        assert_eq!(
            announced_gateway(&config).as_deref(),
            Some("http://203.0.113.7:7777")
        );

        config.private.enabled = true;
        assert_eq!(announced_gateway(&config), None, "never in private mode");
        config.private.enabled = false;

        config.roles.retain(|r| *r != c0mpute_proto::Role::Gateway);
        assert_eq!(announced_gateway(&config), None, "only gateways");
        config.roles.push(c0mpute_proto::Role::Gateway);

        config.gateway.announce = None;
        assert_eq!(announced_gateway(&config), None, "only when the operator set one");
    }

    fn tempdir(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "c0mpute-cap-test-{}-{}",
            name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    async fn wait_for_listen(net: &Libp2pNetwork) -> Multiaddr {
        for _ in 0..50 {
            let addrs = net.listen_addrs().await.unwrap_or_default();
            if let Some(a) = addrs.into_iter().next() {
                return a;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("node never bound a listen addr");
    }

    /// Two nodes: A advertises with capability tags, B's registry should
    /// observe A's ad with the same tags within a few seconds.
    #[tokio::test]
    async fn registry_observes_advertised_capabilities() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("c0mpute_core=info,c0mpute_net=info,libp2p_gossipsub=warn")
            .with_test_writer()
            .try_init();

        let net_a = Arc::new(
            Libp2pNetwork::spawn(
                NetworkConfig::for_dir(tempdir("cap-a"))
                    .with_listen(vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()]),
            )
            .await
            .unwrap(),
        );
        let a_addr = wait_for_listen(&net_a).await;
        let a_full = a_addr.with(Protocol::P2p(net_a.peer_id()));

        let net_b = Arc::new(
            Libp2pNetwork::spawn(
                NetworkConfig::for_dir(tempdir("cap-b"))
                    .with_listen(vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()])
                    .with_bootstrap(vec![a_full]),
            )
            .await
            .unwrap(),
        );

        // B's registry watches inbound capability ads.
        let registry = Registry::new();
        registry.run(net_b.clone());

        // A advertises every 200ms (faster than prod's 60s) so the
        // test doesn't have to wait long.
        let tags = vec![
            "c0mpute:role:storage".to_string(),
            "c0mpute:role:transcode".to_string(),
        ];
        let hardware = serde_json::json!({"test": true});
        tokio::spawn(advertise_loop(
            net_a.clone(),
            tags.clone(),
            hardware,
            Some(8438),
            Duration::from_millis(200),
        ));

        // Poll the registry until A's ad shows up (up to 5s).
        let mut found: Option<CapabilityAd> = None;
        for _ in 0..50 {
            let entries = registry.list().await;
            if let Some((_, ad)) = entries.iter().find(|(p, _)| *p == net_a.peer_id()) {
                found = Some(ad.clone());
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let ad = found.expect("registry never saw A's advertisement");
        assert_eq!(ad.peer_id, net_a.peer_id().to_base58());
        assert_eq!(ad.tags, tags);
        assert_eq!(ad.bench_score, Some(8438), "bench score travels in the ad");

        // find_with_tag should return A.
        let storage_workers = registry.find_with_tag("c0mpute:role:storage").await;
        assert_eq!(storage_workers.len(), 1);
        assert_eq!(storage_workers[0].0, net_a.peer_id());
    }

    #[test]
    fn detect_hardware_tag_returns_one_known_tag() {
        let tag = super::detect_hardware_tag();
        assert!(
            [
                "c0mpute:gpu:nvidia",
                "c0mpute:gpu:amd",
                "c0mpute:gpu:apple",
                "c0mpute:cpu",
            ]
            .contains(&tag.as_str()),
            "unexpected hardware tag: {tag}"
        );
    }

    #[test]
    fn the_adult_tag_is_advertised_only_when_the_operator_opts_in() {
        // Skip when the environment overrides the config either way.
        if std::env::var("C0MPUTE_ADULT").is_ok() {
            return;
        }
        let adult = crate::content_policy::ADULT_TAG.to_string();
        assert!(!super::role_tags(&Config::default()).contains(&adult), "off by default");
        let on: Config = toml::from_str("[adult]\nserve = true\n").unwrap();
        assert!(super::role_tags(&on).contains(&adult));
    }

    #[test]
    fn role_tags_carry_no_hardware_tag() {
        // The hardware tag is probed once and appended by the caller, so the
        // role half can be recomputed cheaply on every advertise tick.
        let tags = super::role_tags(&Config::default());
        assert!(
            !tags
                .iter()
                .any(|tag| tag.starts_with("c0mpute:gpu:") || tag == "c0mpute:cpu"),
            "role tags should not include the hardware tag: {tags:?}"
        );
    }

    #[test]
    fn tts_is_only_advertised_once_an_engine_is_installed() {
        // Role::Tts is on by default, but a node with no engine installed must
        // not claim it: it would win speech auctions it cannot serve.
        let tags = super::role_tags(&Config::default());
        let claims_tts = tags.iter().any(|tag| tag == "c0mpute:role:tts");
        assert_eq!(
            claims_tts,
            !crate::speech::available_tts_engines().is_empty(),
            "the tts tag must track what is actually installed"
        );
    }

    #[test]
    fn tags_from_config_includes_roles_and_exactly_one_hardware_tag() {
        let cfg = Config::default(); // roles: storage, gateway, verifier
        let tags = super::tags_from_config(&cfg);
        assert!(tags.contains(&"c0mpute:role:storage".to_string()));
        assert!(tags.contains(&"c0mpute:role:gateway".to_string()));
        let hw: Vec<_> = tags
            .iter()
            .filter(|t| t.starts_with("c0mpute:gpu:") || t.as_str() == "c0mpute:cpu")
            .collect();
        assert_eq!(hw.len(), 1, "expected exactly one hardware tag, got {hw:?}");
    }
}
