//! Seeding for a running worker: config, the loop that keeps the node's
//! torrents in line with the verified list, and what it reports.
//!
//! Off by default. A node that opts in (`[seeding] enabled = true`,
//! `c0mpute seeding on`, or `C0MPUTE_SEEDING=1`) seeds the torrents c0mpute
//! publishes: the entries of a manifest whose minisign signature verifies
//! against the release key pinned in the binary, cut to the node's disk cap.
//! The operator may narrow that list (`only`, `skip`) and add their own
//! `.torrent` files (`local_torrents`). There is no other way in: no job, no
//! peer, no capability ad and no gateway request can add a torrent, and the
//! gateway route here is a read-only status page. No DHT, no crawling, no
//! index.
//!
//! The pieces live in the `c0mpute-seeding` crate (manifest, plan, engine).
//! Operator docs: docs/seeding.md.

use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use c0mpute_seeding::manifest::{self, Manifest, ManifestStore};
use c0mpute_seeding::{Candidate, Engine, EngineOptions, Narrow, Origin, Skipped};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{info, warn};

/// `1`/`true`/`yes`/`on` opts this node in; `0`/`false`/`no`/`off` forces
/// seeding off. Any other value leaves it off.
pub const ENABLED_ENV: &str = "C0MPUTE_SEEDING";
/// The disk cap in bytes, overriding `[seeding] max_bytes`.
pub const MAX_BYTES_ENV: &str = "C0MPUTE_SEEDING_MAX_BYTES";
/// The upload cap in KiB/s (`0`: no cap), overriding `max_upload_kib_per_sec`.
pub const MAX_UPLOAD_ENV: &str = "C0MPUTE_SEEDING_MAX_UPLOAD_KIB";
/// The TCP listen port (`0`: any free port), overriding `[seeding] port`.
pub const PORT_ENV: &str = "C0MPUTE_SEEDING_PORT";
/// Where the signed manifest is fetched, overriding `[seeding] manifest_url`.
/// The key it must be signed with is pinned and cannot be changed.
pub const MANIFEST_URL_ENV: &str = "C0MPUTE_SEEDING_MANIFEST_URL";
/// Where seeding keeps its data, overriding `[seeding] dir`.
pub const DIR_ENV: &str = "C0MPUTE_SEEDING_DIR";

/// The key of the seeding summary inside a capability ad's `hardware` blob.
pub const AD_FIELD: &str = "seeding";
/// The local gateway path with what a running worker seeds. Read-only.
pub const STATUS_PATH: &str = "/seeding/v1/status";

/// Default disk cap: 20 GiB.
pub const DEFAULT_MAX_BYTES: u64 = 20 * 1024 * 1024 * 1024;
/// Default upload cap: 2048 KiB/s.
pub const DEFAULT_MAX_UPLOAD_KIB: u32 = 2048;
/// Default TCP listen port.
pub const DEFAULT_PORT: u16 = 4241;

/// Said wherever seeding is shown.
pub const WHAT_IT_SEEDS: &str = "Seeding shares only torrents c0mpute publishes (a signed list this node verifies) and .torrent files you list in [seeding] local_torrents. Nothing from the network can add one.";

/// How often the manifest is fetched again and the plan re-applied.
const REFRESH_EVERY: Duration = Duration::from_secs(60 * 60);
/// Sooner than that when something is not in place yet (the engine could not
/// start, or a torrent is waiting for its metadata).
const RETRY_EVERY: Duration = Duration::from_secs(5 * 60);
/// How often the published status is refreshed from the engine.
const STATS_EVERY: Duration = Duration::from_secs(30);

/// `[seeding]`: seed the torrents c0mpute publishes. Off by default.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeedingConfig {
    /// Seed. Off by default: it costs disk and upstream bandwidth, so the
    /// operator opts in (`c0mpute seeding on` or `C0MPUTE_SEEDING=1`).
    #[serde(default)]
    pub enabled: bool,
    /// Most bytes of torrents this node holds. Never exceeded: a torrent
    /// that does not fit whole is skipped. The cap is also lowered while the
    /// disk is short of the free-space floor hosting keeps.
    #[serde(default = "default_max_bytes")]
    pub max_bytes: u64,
    /// Upload cap for all torrents together, KiB per second. `0`: no cap.
    #[serde(default = "default_max_upload")]
    pub max_upload_kib_per_sec: u32,
    /// TCP port peers connect to. `0`: any free port (give each worker its
    /// own when several run on one machine).
    #[serde(default = "default_port")]
    pub port: u16,
    /// Where the signed list is fetched (`<url>.minisig` is its signature).
    /// The key that must have signed it is pinned in the binary.
    #[serde(default = "default_manifest_url")]
    pub manifest_url: String,
    /// When not empty, seed only these info hashes of the published list.
    #[serde(default)]
    pub only: Vec<String>,
    /// Info hashes of the published list this node does not seed.
    #[serde(default)]
    pub skip: Vec<String>,
    /// Your own `.torrent` files to seed as well (absolute paths). They
    /// count against `max_bytes`. Only add content you may distribute.
    #[serde(default)]
    pub local_torrents: Vec<PathBuf>,
    /// Where seeding keeps its data. Default: `<storage root>/seeding`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<PathBuf>,
}

fn default_max_bytes() -> u64 {
    DEFAULT_MAX_BYTES
}

fn default_max_upload() -> u32 {
    DEFAULT_MAX_UPLOAD_KIB
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

fn default_manifest_url() -> String {
    manifest::DEFAULT_MANIFEST_URL.to_string()
}

impl Default for SeedingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_bytes: default_max_bytes(),
            max_upload_kib_per_sec: default_max_upload(),
            port: default_port(),
            manifest_url: default_manifest_url(),
            only: Vec::new(),
            skip: Vec::new(),
            local_torrents: Vec::new(),
            dir: None,
        }
    }
}

/// The environment values that override `[seeding]`.
#[derive(Clone, Debug, Default)]
pub struct SeedingEnv {
    pub enabled: Option<String>,
    pub max_bytes: Option<String>,
    pub max_upload: Option<String>,
    pub port: Option<String>,
    pub manifest_url: Option<String>,
    pub dir: Option<String>,
}

impl SeedingEnv {
    /// This process's environment (empty values count as unset).
    pub fn from_process() -> Self {
        let get = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        Self {
            enabled: get(ENABLED_ENV),
            max_bytes: get(MAX_BYTES_ENV),
            max_upload: get(MAX_UPLOAD_ENV),
            port: get(PORT_ENV),
            manifest_url: get(MANIFEST_URL_ENV),
            dir: get(DIR_ENV),
        }
    }
}

/// What a node will do, and where the decision came from.
#[derive(Clone, Debug)]
pub struct Resolved {
    pub enabled: bool,
    pub max_bytes: u64,
    pub max_upload_kib_per_sec: u32,
    pub port: u16,
    pub manifest_url: String,
    pub dir: PathBuf,
    /// Which setting decided `enabled`.
    pub source: String,
    /// Settings that were ignored, each worded for the operator.
    pub warnings: Vec<String>,
}

fn is_off(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "no" | "off"
    )
}

fn is_on(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

impl SeedingConfig {
    /// Decide whether this node seeds and within which limits, from the
    /// config, the environment and private mode. Seeding is on only when it
    /// was explicitly asked for and every limit that was given is readable;
    /// every doubtful case leaves it off. `storage_root` is where the default
    /// seeding directory goes.
    pub fn resolve(&self, env: &SeedingEnv, private: bool, storage_root: &Path) -> Resolved {
        let mut warnings = Vec::new();
        let (mut enabled, mut source) = match env.enabled.as_deref() {
            Some(v) if is_on(v) => (true, ENABLED_ENV.to_string()),
            Some(v) if is_off(v) => (false, ENABLED_ENV.to_string()),
            Some(v) => {
                warnings.push(format!(
                    "{ENABLED_ENV}={v} is not 1/true/yes/on or 0/false/no/off: seeding stays off"
                ));
                (false, ENABLED_ENV.to_string())
            }
            None => (self.enabled, "[seeding] enabled".to_string()),
        };
        // A limit from the environment that cannot be read is not replaced
        // by a guess: the node does not seed until it is fixed.
        let mut number = |name: &str, raw: &Option<String>, configured: u64, max: u64| match raw {
            None => configured,
            Some(v) => match v.trim().parse::<u64>() {
                Ok(n) if n <= max => n,
                _ => {
                    warnings.push(format!(
                        "{name}={v} is not a number from 0 to {max}: seeding stays off"
                    ));
                    enabled = false;
                    source = name.to_string();
                    configured
                }
            },
        };
        let max_bytes = number(MAX_BYTES_ENV, &env.max_bytes, self.max_bytes, u64::MAX);
        let max_upload = number(
            MAX_UPLOAD_ENV,
            &env.max_upload,
            self.max_upload_kib_per_sec as u64,
            (u32::MAX / 1024) as u64,
        ) as u32;
        let port = number(PORT_ENV, &env.port, self.port as u64, u16::MAX as u64) as u16;
        if private && enabled {
            enabled = false;
            source = "private mode (a private node does not seed)".to_string();
        }
        if enabled && max_bytes == 0 {
            warnings.push("the seeding cap is 0 bytes: nothing fits".to_string());
        }
        let dir = env
            .dir
            .as_deref()
            .map(PathBuf::from)
            .or_else(|| self.dir.clone())
            .unwrap_or_else(|| storage_root.join("seeding"));
        Resolved {
            enabled,
            max_bytes,
            max_upload_kib_per_sec: max_upload.min(u32::MAX / 1024),
            port,
            manifest_url: env
                .manifest_url
                .clone()
                .unwrap_or_else(|| self.manifest_url.clone()),
            dir,
            source,
            warnings,
        }
    }
}

/// `n` bytes for people (binary units).
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// `resolved` as JSON for status output.
pub fn status_json(resolved: &Resolved) -> Value {
    json!({
        "enabled": resolved.enabled,
        "source": resolved.source,
        "max_bytes": resolved.max_bytes,
        "max_upload_kib_per_sec": resolved.max_upload_kib_per_sec,
        "port": resolved.port,
        "manifest_url": resolved.manifest_url,
        "dir": resolved.dir,
        "warnings": resolved.warnings,
        "note": WHAT_IT_SEEDS,
    })
}

/// One line for text status output.
pub fn status_line(resolved: &Resolved) -> String {
    if !resolved.enabled {
        return format!(
            "seeding: off ({}); opt in with `c0mpute seeding on`",
            resolved.source
        );
    }
    let upload = match resolved.max_upload_kib_per_sec {
        0 => "no upload cap".to_string(),
        n => format!("upload cap {n} KiB/s"),
    };
    format!(
        "seeding: on, up to {} of torrents c0mpute publishes, {upload}, TCP port {} (`c0mpute seeding list`)",
        human_bytes(resolved.max_bytes),
        resolved.port
    )
}

// ---------------------------------------------------------------------------
// The running worker
// ---------------------------------------------------------------------------

/// The latest status of this process's seeding, set by the loop [`start`]
/// spawns. `None`: seeding is not running here.
static STATE: Mutex<Option<Value>> = Mutex::new(None);

fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Where the published list came from on the last refresh.
#[derive(Clone, Debug, Default)]
struct ListView {
    manifest: Option<Manifest>,
    /// `network` (fetched and verified now), `cache` (the last verified one,
    /// verified again) or `none`.
    from: &'static str,
    error: Option<String>,
}

/// Fetch the manifest and verify it; fall back to the last verified one.
/// A manifest that does not verify contributes nothing.
async fn refresh_list(store: &ManifestStore, url: &str) -> ListView {
    let now = unix_now_secs();
    let fetched = match manifest::fetch(url).await {
        Ok((bytes, sig)) => store.accept(&bytes, &sig, now),
        Err(e) => Err(e),
    };
    match fetched {
        Ok(m) => ListView {
            manifest: Some(m),
            from: "network",
            error: None,
        },
        Err(e) => {
            let cached = store.cached(now);
            ListView {
                from: if cached.is_some() { "cache" } else { "none" },
                manifest: cached,
                error: Some(format!("{e:#}")),
            }
        }
    }
}

/// The operator's own `.torrent` files as candidates; unreadable ones are
/// reported, not guessed at.
pub fn local_candidates(paths: &[PathBuf]) -> (Vec<Candidate>, Vec<Skipped>) {
    let mut out = Vec::new();
    let mut skipped = Vec::new();
    for path in paths {
        let origin = Origin::Local {
            torrent: path.clone(),
        };
        let read = std::fs::read(path)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| c0mpute_seeding::inspect_torrent(&bytes));
        match read {
            Ok(info) => out.push(Candidate {
                info_hash: info.info_hash,
                name: info.name,
                bytes: info.bytes,
                license: String::new(),
                source: path.display().to_string(),
                origin,
            }),
            Err(e) => skipped.push(Skipped {
                candidate: Candidate {
                    info_hash: String::new(),
                    name: path.display().to_string(),
                    bytes: 0,
                    license: String::new(),
                    source: path.display().to_string(),
                    origin,
                },
                reason: format!("{e:#}"),
            }),
        }
    }
    (out, skipped)
}

/// What this node may seed and what fits `cap_bytes`. The inputs are a
/// verified manifest (or none), the list compiled into the binary, and the
/// operator's `[seeding]` config; there is no parameter for anything else.
pub fn build_plan(
    verified: Option<&Manifest>,
    config: &SeedingConfig,
    cap_bytes: u64,
) -> c0mpute_seeding::Plan {
    let mut published = manifest::builtin_entries();
    if let Some(m) = verified {
        published.extend(m.torrents.iter().cloned());
    }
    let narrow = Narrow {
        only: config.only.clone(),
        skip: config.skip.clone(),
    };
    let (local, unreadable) = local_candidates(&config.local_torrents);
    let (candidates, narrowed) = c0mpute_seeding::candidates(&published, &narrow, local);
    let mut plan = c0mpute_seeding::plan(candidates, cap_bytes);
    plan.skipped.extend(narrowed);
    plan.skipped.extend(unreadable);
    plan
}

fn skipped_json(s: &Skipped) -> Value {
    json!({
        "info_hash": s.candidate.info_hash,
        "name": s.candidate.name,
        "bytes": s.candidate.bytes,
        "origin": s.candidate.origin.label(),
        "reason": s.reason,
    })
}

struct Running {
    resolved: Resolved,
    config: SeedingConfig,
    engine: Option<Engine>,
    engine_error: Option<String>,
    list: ListView,
    skipped: Vec<Skipped>,
    cap_now: u64,
}

impl Running {
    fn data_dir(&self) -> PathBuf {
        self.resolved.dir.join("data")
    }

    fn status(&self) -> Value {
        let torrents = self
            .engine
            .as_ref()
            .map(Engine::snapshot)
            .unwrap_or_default();
        let held: u64 = torrents.iter().map(|t| t.held_bytes).sum();
        let m = self.list.manifest.as_ref();
        json!({
            "enabled": true,
            "max_bytes": self.resolved.max_bytes,
            "cap_bytes_now": self.cap_now,
            "held_bytes": held,
            "max_upload_kib_per_sec": self.resolved.max_upload_kib_per_sec,
            "port": self.engine.as_ref().and_then(Engine::listen_port).unwrap_or(self.resolved.port),
            "dir": self.resolved.dir,
            "engine_error": self.engine_error,
            "list": {
                "url": self.resolved.manifest_url,
                "from": self.list.from,
                "seq": m.map(|m| m.seq),
                "issued_at": m.map(|m| m.issued_at),
                "expires_at": m.map(|m| m.expires_at),
                "torrents": m.map(|m| m.torrents.len()).unwrap_or(0),
                "error": self.list.error,
            },
            "torrents": torrents,
            "skipped": self.skipped.iter().map(skipped_json).collect::<Vec<_>>(),
            "note": WHAT_IT_SEEDS,
        })
    }

    fn publish(&self) {
        *STATE.lock().unwrap_or_else(|e| e.into_inner()) = Some(self.status());
    }

    /// One round: make sure the engine is up, refresh and verify the list,
    /// plan within the cap, and hand the plan to the engine. Returns whether
    /// everything planned is in place.
    async fn refresh(&mut self) -> bool {
        if self.engine.is_none() {
            let upload = NonZeroU32::new(self.resolved.max_upload_kib_per_sec.saturating_mul(1024));
            match Engine::start(EngineOptions::new(self.data_dir(), self.resolved.port, upload))
                .await
            {
                Ok(e) => {
                    self.engine = Some(e);
                    self.engine_error = None;
                }
                Err(e) => {
                    let why = format!("{e:#}");
                    warn!(err = %why, "seeding: the engine could not start; trying again later");
                    self.engine_error = Some(why);
                }
            }
        }
        let store = ManifestStore::new(&self.resolved.dir.join("manifest"));
        self.list = refresh_list(&store, &self.resolved.manifest_url).await;
        if let Some(e) = &self.list.error {
            info!(err = %e, using = self.list.from, "seeding: no fresh verified list");
        }

        let data_dir = self.data_dir();
        let held = c0mpute_seeding::dir_bytes(&data_dir);
        let (free, total) = crate::opendisk::disk_for(&data_dir);
        self.cap_now = c0mpute_seeding::effective_cap(
            self.resolved.max_bytes,
            held,
            free,
            total,
            crate::hosting::free_disk_floor(total),
        );
        let plan = build_plan(self.list.manifest.as_ref(), &self.config, self.cap_now);
        info!(
            seed = plan.seed.len(),
            skipped = plan.skipped.len(),
            bytes = plan.bytes(),
            cap = self.cap_now,
            "seeding: plan"
        );
        self.skipped = plan.skipped;
        self.publish();
        let Some(engine) = &self.engine else {
            return false;
        };
        engine.apply(&plan.seed).await;
        self.publish();
        !engine.snapshot().iter().any(|t| t.state == "waiting")
    }

    async fn run(mut self) {
        let mut ticker = tokio::time::interval(STATS_EVERY);
        let mut next_refresh = Instant::now();
        loop {
            ticker.tick().await;
            if Instant::now() >= next_refresh {
                let settled = self.refresh().await;
                next_refresh =
                    Instant::now() + if settled { REFRESH_EVERY } else { RETRY_EVERY };
            }
            self.publish();
        }
    }
}

/// Start seeding in this worker process when `resolved` says so. Does
/// nothing otherwise. A failure in here never stops the worker.
pub fn start(config: &SeedingConfig, resolved: &Resolved) {
    for w in &resolved.warnings {
        warn!("{w}");
    }
    if !resolved.enabled {
        info!(reason = %resolved.source, "seeding: off");
        return;
    }
    info!(
        max_bytes = resolved.max_bytes,
        max_upload_kib_per_sec = resolved.max_upload_kib_per_sec,
        port = resolved.port,
        dir = %resolved.dir.display(),
        list = %resolved.manifest_url,
        "seeding: on (torrents c0mpute publishes, verified against the pinned release key)"
    );
    let running = Running {
        resolved: resolved.clone(),
        config: config.clone(),
        engine: None,
        engine_error: None,
        list: ListView {
            from: "none",
            ..Default::default()
        },
        skipped: Vec::new(),
        cap_now: resolved.max_bytes,
    };
    running.publish();
    tokio::spawn(running.run());
}

/// What a running worker seeds, or `{"enabled": false}`.
pub fn running_status() -> Value {
    STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_else(|| json!({ "enabled": false, "note": WHAT_IT_SEEDS }))
}

/// The ad field for a status: how much this node seeds. A summary only.
pub fn ad_value(status: &Value) -> Value {
    let seeding = status["torrents"]
        .as_array()
        .map(|t| t.iter().filter(|t| t["state"] == "seeding").count())
        .unwrap_or(0);
    json!({
        "v": 1,
        "torrents": seeding,
        "held_bytes": status["held_bytes"].as_u64().unwrap_or(0),
        "max_bytes": status["max_bytes"].as_u64().unwrap_or(0),
    })
}

/// Put this node's seeding summary into an outgoing ad's hardware blob, or
/// make sure there is none when it does not seed. Nodes publish this; no
/// node reads another's.
pub fn attach(hardware: &mut Value) {
    let Some(obj) = hardware.as_object_mut() else {
        return;
    };
    let state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    match state.as_ref() {
        Some(status) => {
            obj.insert(AD_FIELD.to_string(), ad_value(status));
        }
        None => {
            obj.remove(AD_FIELD);
        }
    }
}

/// Headers a reverse proxy adds; a request carrying one came from elsewhere
/// even when the connection is from loopback.
const PROXY_HEADERS: [&str; 4] = ["forwarded", "x-forwarded-for", "x-real-ip", "cf-connecting-ip"];

/// Whether a status request comes from this machine: a loopback connection
/// with no proxy headers. Unknown peers count as remote.
fn is_local_request(peer: Option<std::net::SocketAddr>, headers: &axum::http::HeaderMap) -> bool {
    peer.is_some_and(|p| p.ip().is_loopback()) && !PROXY_HEADERS.iter().any(|h| headers.contains_key(*h))
}

/// The status a request gets. The full status (data directory, local
/// `.torrent` paths, errors) is for the operator's own machine; anyone else
/// (a gateway can be public) gets the summary the capability ad already
/// carries.
fn status_for(local: bool, state: Option<&Value>) -> Value {
    match state {
        None => json!({ "enabled": false, "note": WHAT_IT_SEEDS }),
        Some(s) if local => s.clone(),
        Some(s) => json!({ "enabled": true, "summary": ad_value(s), "note": WHAT_IT_SEEDS }),
    }
}

/// Gateway route [`STATUS_PATH`]: what this worker seeds. GET only; there is
/// no route that adds, removes or changes a torrent.
pub fn router() -> axum::Router {
    use axum::extract::{ConnectInfo, Request};
    use axum::routing::get;
    use axum::{Json, Router};

    async fn status(req: Request) -> Json<Value> {
        let peer = req
            .extensions()
            .get::<ConnectInfo<std::net::SocketAddr>>()
            .map(|c| c.0);
        let local = is_local_request(peer, req.headers());
        let state = STATE.lock().unwrap_or_else(|e| e.into_inner()).clone();
        Json(status_for(local, state.as_ref()))
    }
    Router::new().route(STATUS_PATH, get(status))
}

#[cfg(test)]
mod tests {
    use c0mpute_seeding::manifest::{Entry, MANIFEST_KIND, MANIFEST_VERSION};

    use super::*;

    const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn root() -> PathBuf {
        PathBuf::from("/var/lib/c0mpute")
    }

    fn env(enabled: Option<&str>) -> SeedingEnv {
        SeedingEnv {
            enabled: enabled.map(str::to_string),
            ..SeedingEnv::default()
        }
    }

    fn entry(info_hash: &str, bytes: u64) -> Entry {
        Entry {
            info_hash: info_hash.to_string(),
            name: "a release".into(),
            bytes,
            license: "CC0-1.0".into(),
            source: "https://example.org/".into(),
            trackers: vec![],
        }
    }

    fn manifest_of(torrents: Vec<Entry>) -> Manifest {
        Manifest {
            kind: MANIFEST_KIND.into(),
            v: MANIFEST_VERSION,
            seq: 1,
            issued_at: 1,
            expires_at: 0,
            torrents,
        }
    }

    #[test]
    fn seeding_is_off_by_default() {
        let cfg = SeedingConfig::default();
        assert!(!cfg.enabled);
        // A config file that never mentions seeding means the same.
        let parsed: crate::Config = toml::from_str("").unwrap();
        assert_eq!(parsed.seeding, cfg);
        let partial: crate::Config = toml::from_str("[seeding]\n").unwrap();
        assert_eq!(partial.seeding, cfg);
        assert_eq!(crate::Config::default().seeding, cfg);

        let r = cfg.resolve(&SeedingEnv::default(), false, &root());
        assert!(!r.enabled);
        assert!(r.warnings.is_empty());
        assert_eq!(r.source, "[seeding] enabled");
        assert_eq!(status_json(&r)["enabled"], json!(false));
        let line = status_line(&r);
        assert!(line.starts_with("seeding: off"), "{line}");
        // Setting only limits (file or environment) opts nobody in.
        let limits: crate::Config = toml::from_str("[seeding]\nmax_bytes = 5\n").unwrap();
        assert!(
            !limits
                .seeding
                .resolve(&SeedingEnv::default(), false, &root())
                .enabled
        );
        let only_limits = SeedingEnv {
            max_bytes: Some("1000".into()),
            port: Some("5000".into()),
            ..SeedingEnv::default()
        };
        assert!(!cfg.resolve(&only_limits, false, &root()).enabled);
    }

    #[test]
    fn the_operator_opts_in_by_config_or_environment() {
        let cfg = SeedingConfig::default();
        let on = SeedingConfig {
            enabled: true,
            ..cfg.clone()
        };
        let r = on.resolve(&SeedingEnv::default(), false, &root());
        assert!(r.enabled);
        assert_eq!(r.max_bytes, DEFAULT_MAX_BYTES);
        assert_eq!(r.max_upload_kib_per_sec, DEFAULT_MAX_UPLOAD_KIB);
        assert_eq!(r.port, DEFAULT_PORT);
        assert_eq!(r.manifest_url, manifest::DEFAULT_MANIFEST_URL);
        assert_eq!(r.dir, root().join("seeding"));
        let line = status_line(&r);
        assert!(
            line.contains("on, up to 20.0 GiB") && line.contains("2048 KiB/s"),
            "{line}"
        );

        // A fleet opts in without editing files, limits included.
        for v in ["1", "true", "YES", "on"] {
            let r = cfg.resolve(&env(Some(v)), false, &root());
            assert!(r.enabled, "{v}");
            assert_eq!(r.source, ENABLED_ENV);
        }
        let fleet = SeedingEnv {
            enabled: Some("1".into()),
            max_bytes: Some("5368709120".into()),
            max_upload: Some("0".into()),
            port: Some("0".into()),
            manifest_url: Some("https://example.org/m.json".into()),
            dir: Some("/mnt/big/seeding".into()),
        };
        let r = cfg.resolve(&fleet, false, &root());
        assert!(r.enabled);
        assert_eq!(r.max_bytes, 5 * 1024 * 1024 * 1024);
        assert_eq!(r.max_upload_kib_per_sec, 0);
        assert_eq!(r.port, 0);
        assert_eq!(r.manifest_url, "https://example.org/m.json");
        assert_eq!(r.dir, PathBuf::from("/mnt/big/seeding"));
        assert!(status_line(&r).contains("no upload cap"));

        // The environment can force it off over the config.
        for v in ["0", "false", "no", "OFF"] {
            assert!(!on.resolve(&env(Some(v)), false, &root()).enabled, "{v}");
        }
        // Private mode never seeds.
        let r = on.resolve(&env(Some("1")), true, &root());
        assert!(!r.enabled);
        assert!(r.source.contains("private mode"));
    }

    #[test]
    fn a_doubtful_setting_leaves_seeding_off() {
        let on = SeedingConfig {
            enabled: true,
            ..SeedingConfig::default()
        };
        // An unrecognized switch never turns it on, even over a config that had it on.
        let r = on.resolve(&env(Some("maybe")), false, &root());
        assert!(!r.enabled);
        assert_eq!(r.warnings.len(), 1);
        // A limit that cannot be read is not replaced by a guess.
        for (name, e) in [
            (
                MAX_BYTES_ENV,
                SeedingEnv {
                    max_bytes: Some("20GB".into()),
                    ..SeedingEnv::default()
                },
            ),
            (
                MAX_UPLOAD_ENV,
                SeedingEnv {
                    max_upload: Some("-1".into()),
                    ..SeedingEnv::default()
                },
            ),
            (
                PORT_ENV,
                SeedingEnv {
                    port: Some("70000".into()),
                    ..SeedingEnv::default()
                },
            ),
        ] {
            let r = on.resolve(&e, false, &root());
            assert!(!r.enabled, "{name}");
            assert_eq!(r.source, name);
            assert!(r.warnings[0].contains(name), "{:?}", r.warnings);
        }
        // A cap of 0 seeds nothing and says so.
        let zero = SeedingConfig {
            max_bytes: 0,
            ..on.clone()
        };
        let r = zero.resolve(&SeedingEnv::default(), false, &root());
        assert!(r.warnings.iter().any(|w| w.contains("0 bytes")));
        assert!(
            build_plan(Some(&manifest_of(vec![entry(HASH_A, 1)])), &zero, r.max_bytes)
                .seed
                .is_empty()
        );
    }

    #[test]
    fn the_config_round_trips_through_toml() {
        let mut cfg = crate::Config::default();
        cfg.seeding.enabled = true;
        cfg.seeding.max_bytes = 123;
        cfg.seeding.only = vec![HASH_A.into()];
        cfg.seeding.local_torrents = vec![PathBuf::from("/srv/mine.torrent")];
        let text = toml::to_string_pretty(&cfg).unwrap();
        assert!(text.contains("[seeding]"), "{text}");
        let back: crate::Config = toml::from_str(&text).unwrap();
        assert_eq!(back.seeding, cfg.seeding);
    }

    #[test]
    fn the_plan_respects_the_cap_and_the_operators_narrowing() {
        let m = manifest_of(vec![entry(HASH_A, 60), entry(HASH_B, 50)]);
        let cfg = SeedingConfig::default();
        let p = build_plan(Some(&m), &cfg, 100);
        assert_eq!(p.seed.len(), 1);
        assert_eq!(p.seed[0].info_hash, HASH_A);
        assert!(p.bytes() <= 100);
        assert_eq!(p.skipped.len(), 1);
        assert!(p.skipped[0].reason.contains("exceed the seeding cap"));
        assert_eq!(skipped_json(&p.skipped[0])["info_hash"], json!(HASH_B));

        let skip_a = SeedingConfig {
            skip: vec![HASH_A.into()],
            ..cfg
        };
        let p = build_plan(Some(&m), &skip_a, 100);
        assert_eq!(p.seed.len(), 1);
        assert_eq!(p.seed[0].info_hash, HASH_B);
    }

    #[test]
    fn nothing_but_a_verified_list_and_local_config_makes_a_plan() {
        let cfg = SeedingConfig::default();
        // No verified manifest (unreachable, unsigned, wrong key, expired:
        // all arrive here as None) and no local torrents: nothing is seeded.
        let p = build_plan(None, &cfg, u64::MAX);
        assert!(p.seed.is_empty() && p.skipped.is_empty());

        // `only` narrows; it cannot name a torrent into the plan.
        let only = SeedingConfig {
            only: vec![HASH_B.into()],
            ..cfg.clone()
        };
        assert!(build_plan(None, &only, u64::MAX).seed.is_empty());
        let m = manifest_of(vec![entry(HASH_A, 10)]);
        assert!(build_plan(Some(&m), &only, u64::MAX).seed.is_empty());

        // A local path that is not a .torrent file is reported, not seeded.
        let dir = tempfile::tempdir().unwrap();
        let junk = dir.path().join("junk.torrent");
        std::fs::write(&junk, format!("magnet:?xt=urn:btih:{HASH_A}")).unwrap();
        let local = SeedingConfig {
            local_torrents: vec![junk, dir.path().join("missing.torrent")],
            ..cfg
        };
        let p = build_plan(None, &local, u64::MAX);
        assert!(p.seed.is_empty());
        assert_eq!(p.skipped.len(), 2);

        // What a peer says in its ad is a summary nobody reads back: the ad
        // field has no list of torrents in it at all.
        let ad = ad_value(&json!({
            "max_bytes": 100, "held_bytes": 60,
            "torrents": [{ "info_hash": HASH_A, "state": "seeding" }, { "info_hash": HASH_B, "state": "waiting" }],
        }));
        assert_eq!(ad, json!({ "v": 1, "torrents": 1, "held_bytes": 60, "max_bytes": 100 }));
    }

    #[tokio::test]
    async fn an_unsigned_or_wrongly_signed_list_from_the_network_adds_nothing() {
        use axum::routing::get;
        // A server that publishes a well-formed manifest with a signature
        // that is not ours (and one with none at all).
        let body = serde_json::to_vec(&manifest_of(vec![entry(HASH_A, 10)])).unwrap();
        let forged = "untrusted comment: signature from minisign secret key\nRUSgKx30JhkH5uRp7VRolVR4bM+n44NjjP5/duvM3raMiSDyRlUhTyEYAKVmuWzEBxhLEQE8VKRybfYLgQ+53uUy38iJOMrkrwg=\ntrusted comment: timestamp:1 file:seeding-manifest.json\nBlqf7m00l6vWH+cZyF7MfEWS3yzPB72bD9SD4FKBHNxraBa9P0juzgFm7uK6VFEpq3Fn0wr21tqB4o9lMbP2DA==\n";
        let b1 = body.clone();
        let b2 = body.clone();
        let app = axum::Router::new()
            .route("/forged.json", get(move || async move { b1 }))
            .route("/forged.json.minisig", get(move || async move { forged }))
            .route("/unsigned.json", get(move || async move { b2 }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let store = ManifestStore::new(dir.path());
        for path in ["forged.json", "unsigned.json", "missing.json"] {
            let view = refresh_list(&store, &format!("http://{addr}/{path}")).await;
            assert!(view.manifest.is_none(), "{path}");
            assert_eq!(view.from, "none", "{path}");
            assert!(view.error.is_some(), "{path}");
            let plan = build_plan(view.manifest.as_ref(), &SeedingConfig::default(), u64::MAX);
            assert!(plan.seed.is_empty(), "{path}");
        }
        // Nothing was cached either.
        assert!(store.cached(unix_now_secs()).is_none());
    }

    #[tokio::test]
    async fn the_gateway_route_is_read_only() {
        use axum::body::Body;
        use axum::http::{Method, Request, StatusCode};
        use tower::ServiceExt;

        let app = router();
        let ok = app
            .clone()
            .oneshot(Request::get(STATUS_PATH).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
        // No method that could carry a torrent in is routed.
        for method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
            let body = format!(r#"{{"magnet":"magnet:?xt=urn:btih:{HASH_A}"}}"#);
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method.clone())
                        .uri(STATUS_PATH)
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED, "{method}");
        }
        // And there is no add route beside it.
        for path in ["/seeding/v1/add", "/seeding/v1/torrents", "/seeding/v1"] {
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::NOT_FOUND, "{path}");
        }
    }

    #[test]
    fn only_the_operators_own_machine_sees_the_full_status() {
        use axum::http::HeaderMap;
        let loopback: std::net::SocketAddr = "127.0.0.1:50000".parse().unwrap();
        let loopback6: std::net::SocketAddr = "[::1]:50000".parse().unwrap();
        let remote: std::net::SocketAddr = "203.0.113.9:50000".parse().unwrap();
        let none = HeaderMap::new();
        assert!(is_local_request(Some(loopback), &none));
        assert!(is_local_request(Some(loopback6), &none));
        assert!(!is_local_request(Some(remote), &none));
        assert!(!is_local_request(None, &none));
        // A reverse proxy on the same box connects from loopback.
        for h in PROXY_HEADERS {
            let mut headers = HeaderMap::new();
            headers.insert(h, "203.0.113.9".parse().unwrap());
            assert!(!is_local_request(Some(loopback), &headers), "{h}");
        }

        let full = json!({
            "enabled": true, "max_bytes": 100, "held_bytes": 60,
            "dir": "/home/op/.local/share/c0mpute/seeding",
            "torrents": [{ "info_hash": HASH_A, "state": "seeding", "source": "/home/op/mine.torrent" }],
        });
        assert_eq!(status_for(true, Some(&full)), full);
        let public = status_for(false, Some(&full));
        assert_eq!(public["enabled"], json!(true));
        assert_eq!(public["summary"], json!({ "v": 1, "torrents": 1, "held_bytes": 60, "max_bytes": 100 }));
        let text = public.to_string();
        assert!(!text.contains("/home/op") && !text.contains(HASH_A), "{text}");
        for local in [true, false] {
            assert_eq!(status_for(local, None)["enabled"], json!(false));
        }
    }

    #[test]
    fn a_node_that_does_not_seed_reports_and_advertises_nothing() {
        // `start` was never called in this test process.
        assert_eq!(running_status()["enabled"], json!(false));
        let mut hw = json!({ "version": "x", AD_FIELD: { "v": 1 } });
        attach(&mut hw);
        assert!(hw.get(AD_FIELD).is_none());
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(DEFAULT_MAX_BYTES), "20.0 GiB");
    }
}
