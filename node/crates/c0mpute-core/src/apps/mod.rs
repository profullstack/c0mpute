//! Apps: backends (APIs, MCP servers, workers with a database) run by the
//! network itself, with three copies and no coordinator. DIP 0031.
//!
//! * An owner signs an [`record::AppRecord`]: a container image (published
//!   as a hosting site, so the network keeps it even while the owner is
//!   offline), a port, public env, an optional state directory, `replicas`.
//! * Every `apps` node gossips what it holds on [`TOPIC`] every
//!   [`Timing::announce_every`]. From that alone every node computes the same
//!   holders and the same primary ([`placement`]). The highest-ranked nodes
//!   with room take a copy; nobody is elected by anybody.
//! * The primary runs the container (hard limits, loopback port only) and
//!   snapshots its state every `snapshot_secs`: paused for the copy, so the
//!   snapshot is crash-consistent, then encrypted under the app key in
//!   deduplicating 1 MiB pieces ([`snapshot`]). Standbys pull each new
//!   snapshot (only the pieces that changed) and keep the image loaded.
//! * When the primary stops announcing for [`Timing::node_ttl`], the standby
//!   with the newest snapshot promotes itself at `epoch + 1`, restores, and
//!   starts. A node that comes back with a lower epoch sees the new primary
//!   and stays down. Recovery point: one snapshot interval.
//! * Secrets (and the app key) reach holders sealed to each one's X25519
//!   key: from the owner's CLI at deploy, and from any holder that has them
//!   to a newcomer, so the owner can go offline.
//! * Every gateway serves `/a/<app_id>/...` and custom domains (TXT
//!   `_c0mpute-app.<domain>` = `app=<app_id>`), forwarding to the primary
//!   over the `apps.http` RPC ([`proxy`]), NAT or not.

pub mod deploy;
pub mod placement;
pub mod proxy;
pub mod record;
pub mod runtime;
pub mod snapshot;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use axum::Router;
use axum::body::Body;
use axum::extract::{Path as AxPath, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{any, get, post};
use c0mpute_net::{GossipMessage, Libp2pNetwork, RpcHandler};
use c0mpute_proto::{ChunkRequest, Hash};
use c0mpute_store::ChunkStore;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::hosting::Hosting;
use crate::hosting::dns;
use crate::temporal::crypto;
use placement::{AppRole, HeldApp, NodeView, Primary, SnapRef};
use record::{AppRecord, ImageSource};
use snapshot::{AppSecrets, SnapManifest};

pub const TOPIC: &str = "c0mpute/apps/v1";
pub const ROLE_TAG: &str = "c0mpute:role:apps";
pub const RPC_HTTP: &str = "apps.http";
pub const RPC_LOGS: &str = "apps.logs";
pub const RPC_RECORD: &str = "apps.record";
/// Apps per announcement, so one message stays well under gossipsub's limit.
const APPS_PER_MESSAGE: usize = 100;

/// How often things happen. Tests shrink these.
#[derive(Clone, Debug)]
pub struct Timing {
    pub announce_every: Duration,
    pub records_every: Duration,
    pub tick: Duration,
    /// A node silent this long is gone (its primary claim lapses).
    pub node_ttl: Duration,
    /// A freshly started node listens this long before it elects itself.
    pub warmup: Duration,
    pub fetch_timeout: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            announce_every: Duration::from_secs(10),
            records_every: Duration::from_secs(60),
            tick: Duration::from_secs(5),
            node_ttl: Duration::from_secs(35),
            warmup: Duration::from_secs(30),
            fetch_timeout: Duration::from_secs(30),
        }
    }
}

/// Gossip on [`TOPIC`].
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum AppsMsg {
    Record {
        record: AppRecord,
    },
    /// "This is me and what I hold." Owners' CLIs send it with `hosts: false`.
    Node {
        x25519: String,
        free_mb: u32,
        hosts: bool,
        apps: Vec<HeldApp>,
        #[serde(default)]
        addrs: Vec<String>,
        /// Owner keys whose apps this node runs (empty = anyone's).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        owners: Vec<String>,
    },
    /// Secrets sealed to one holder (`to`), by the owner or another holder.
    Seal {
        app_id: String,
        secrets_hash: String,
        to: String,
        from_pub: String,
        ct: String,
    },
}

/// One app this node holds.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Local {
    rec: AppRecord,
    role: AppRole,
    epoch: u64,
    #[serde(skip)]
    ready_seq: Option<u64>,
    secrets: Option<AppSecrets>,
    snapshot: Option<SnapRef>,
    /// The epoch this node's state volume was last live under as primary.
    volume_epoch: Option<u64>,
    #[serde(skip)]
    running_seq: Option<u64>,
    #[serde(skip)]
    host_port: Option<u16>,
    #[serde(skip)]
    healthy: bool,
    #[serde(skip)]
    busy: bool,
    #[serde(skip)]
    last_snap: Option<Instant>,
    #[serde(skip)]
    failures: u32,
}

impl Local {
    fn new(rec: AppRecord) -> Self {
        Local {
            rec,
            role: AppRole::Standby,
            epoch: 0,
            ready_seq: None,
            secrets: None,
            snapshot: None,
            volume_epoch: None,
            running_seq: None,
            host_port: None,
            healthy: false,
            busy: false,
            last_snap: None,
            failures: 0,
        }
    }

    fn ready(&self) -> bool {
        self.ready_seq == Some(self.rec.seq)
    }

    fn has_secrets(&self) -> bool {
        match (&self.rec.secrets_hash, &self.secrets) {
            (None, _) => true,
            (Some(h), Some(s)) => s.hash() == *h,
            _ => false,
        }
    }

    fn held(&self) -> HeldApp {
        HeldApp {
            app_id: self.rec.app_id.clone(),
            seq: self.rec.seq,
            role: self.role,
            epoch: self.epoch,
            snapshot: self.snapshot.clone(),
            healthy: self.healthy,
            has_secrets: self.has_secrets(),
            ready: self.ready(),
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Persisted {
    records: HashMap<String, AppRecord>,
    local: HashMap<String, Local>,
    #[serde(default)]
    owned: HashMap<String, AppSecrets>,
}

struct Inner {
    net: Arc<Libp2pNetwork>,
    store: ChunkStore,
    me: String,
    hosts: bool,
    max_apps: usize,
    max_memory_mb: u32,
    owners: Vec<String>,
    timing: Timing,
    x25519: crypto::KeyPair,
    hosting: Option<Hosting>,
    dns: dns::Verifier,
    records: RwLock<HashMap<String, AppRecord>>,
    nodes: RwLock<HashMap<String, (NodeView, Instant)>>,
    local: RwLock<HashMap<String, Local>>,
    /// Owner side: secrets this machine hands to the holders of its apps.
    owned: RwLock<HashMap<String, AppSecrets>>,
    sealed_to: RwLock<HashMap<(String, String), Instant>>,
    /// Apps this node let go as surplus, and when: it does not take them back
    /// for a few TTLs, so a late announcement cannot make copies flap.
    released: RwLock<HashMap<String, Instant>>,
    state_path: Option<PathBuf>,
    started: Instant,
    stopped: std::sync::atomic::AtomicBool,
}

/// Options for [`Apps::start`].
pub struct Options {
    /// Runs apps for the network (the `apps` role, needs docker).
    pub hosts: bool,
    pub max_apps: usize,
    /// Memory all apps on this node may reserve together.
    pub max_memory_mb: u32,
    /// Where this node keeps its app state (records, secrets, X25519 key).
    pub dir: Option<PathBuf>,
    /// Run only apps signed by these owner keys (hex ed25519); empty = anyone's.
    pub owners: Vec<String>,
    pub timing: Timing,
    /// To read images published as hosting sites.
    pub hosting: Option<Hosting>,
    pub dns: dns::Verifier,
    /// Capability ads, to find and dial other apps nodes and gateways. The
    /// apps topic has few subscribers, and gossipsub only carries a topic
    /// between peers that are connected and both subscribed.
    pub registry: Option<crate::capabilities::Registry>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            hosts: false,
            max_apps: 4,
            max_memory_mb: 2048,
            dir: None,
            owners: Vec::new(),
            timing: Timing::default(),
            hosting: None,
            dns: dns::Verifier::default(),
            registry: None,
        }
    }
}

/// What the gateway API says about an app.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppInfo {
    pub app_id: String,
    pub name: String,
    pub owner: String,
    pub seq: u64,
    pub replicas: u8,
    pub holders: Vec<String>,
    pub primary: Option<String>,
    pub epoch: u64,
    pub healthy: bool,
    pub snapshot: Option<SnapRef>,
    pub domains: Vec<String>,
    pub path: String,
}

/// The apps service: one per node. Cheap to clone.
#[derive(Clone)]
pub struct Apps(Arc<Inner>);

fn load_x25519(dir: Option<&PathBuf>) -> crypto::KeyPair {
    let Some(dir) = dir else {
        return crypto::keypair();
    };
    let path = dir.join("x25519.key");
    if let Ok(h) = std::fs::read_to_string(&path)
        && let Ok(secret) = crypto::secret_from_hex(h.trim())
    {
        let public_hex = hex::encode(x25519_dalek::PublicKey::from(&secret).as_bytes());
        return crypto::KeyPair { secret, public_hex };
    }
    let kp = crypto::keypair();
    let _ = std::fs::create_dir_all(dir);
    let _ = write_private(&path, hex::encode(kp.secret.to_bytes()).as_bytes());
    kp
}

/// The view as this app sees it: nodes off its host allowlist do not exist
/// for its placement, primary election, snapshots or secrets.
fn scoped(rec: &AppRecord, view: &HashMap<String, NodeView>) -> HashMap<String, NodeView> {
    if rec.hosts.is_empty() {
        return view.clone();
    }
    view.iter()
        .filter(|(p, _)| rec.allows(p))
        .map(|(p, n)| (p.clone(), n.clone()))
        .collect()
}

/// Write a file only its owner can read.
pub fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        use std::io::Write;
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        let mut f = o.open(&tmp)?;
        f.write_all(bytes)?;
    }
    std::fs::rename(tmp, path)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Apps {
    /// Subscribe to [`TOPIC`], register the RPCs and start the loops.
    pub async fn start(net: Arc<Libp2pNetwork>, store: ChunkStore, opts: Options) -> Result<Self> {
        let x25519 = load_x25519(opts.dir.as_ref());
        let state_path = opts.dir.as_ref().map(|d| d.join("apps.json"));
        let persisted: Persisted = state_path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let me = net.peer_id().to_base58();
        let mut local = persisted.local;
        // Never resume as primary: the network may have moved on. A node that
        // is still the best choice is re-elected after warmup.
        for l in local.values_mut() {
            l.role = AppRole::Standby;
        }
        let records = persisted
            .records
            .into_iter()
            .filter(|(_, r)| r.verify())
            .collect();
        let a = Apps(Arc::new(Inner {
            net: net.clone(),
            store,
            me,
            hosts: opts.hosts,
            max_apps: opts.max_apps.max(1),
            max_memory_mb: opts.max_memory_mb,
            owners: opts.owners,
            timing: opts.timing,
            x25519,
            hosting: opts.hosting,
            dns: opts.dns,
            records: RwLock::new(records),
            nodes: RwLock::new(HashMap::new()),
            local: RwLock::new(if opts.hosts { local } else { HashMap::new() }),
            owned: RwLock::new(persisted.owned),
            sealed_to: RwLock::new(HashMap::new()),
            released: RwLock::new(HashMap::new()),
            state_path,
            started: Instant::now(),
            stopped: std::sync::atomic::AtomicBool::new(false),
        }));
        net.subscribe(TOPIC)
            .await
            .context("subscribe to apps topic")?;
        net.register_rpc(RPC_HTTP, Arc::new(HttpHandler(a.clone())));
        net.register_rpc(RPC_LOGS, Arc::new(LogsHandler(a.clone())));
        net.register_rpc(RPC_RECORD, Arc::new(RecordHandler(a.clone())));
        a.spawn_gossip_reader();
        a.spawn_announcer();
        a.spawn_supervisor();
        if let Some(reg) = opts.registry {
            a.spawn_linker(reg);
        }
        Ok(a)
    }

    pub fn peer_id(&self) -> &str {
        &self.0.me
    }

    /// Stop announcing and supervising (the node is leaving). Its claims
    /// lapse once the network's node TTL passes. Containers are left as they
    /// are; a node that starts again sweeps them.
    pub fn stop(&self) {
        self.0
            .stopped
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn is_stopped(&self) -> bool {
        self.0.stopped.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// What this node says about an app it holds.
    pub async fn held(&self, app_id: &str) -> Option<HeldApp> {
        self.0.local.read().await.get(app_id).map(|l| l.held())
    }

    pub fn x25519_public(&self) -> &str {
        &self.0.x25519.public_hex
    }

    async fn persist(&self) {
        let Some(path) = &self.0.state_path else {
            return;
        };
        let p = Persisted {
            records: self.0.records.read().await.clone(),
            local: self.0.local.read().await.clone(),
            owned: self.0.owned.read().await.clone(),
        };
        if let Ok(b) = serde_json::to_vec(&p) {
            if let Some(d) = path.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            let _ = write_private(path, &b);
        }
    }

    // ── gossip ───────────────────────────────────────────────────────────

    async fn publish(&self, msg: &AppsMsg) {
        if let Ok(bytes) = serde_json::to_vec(msg)
            && let Err(e) = self.0.net.publish(TOPIC, bytes).await
        {
            debug!(err = %e, "apps: publish (mesh forming?)");
        }
    }

    /// Accept a record if it verifies and is newer. Returns true on change.
    pub async fn apply_record(&self, rec: AppRecord) -> bool {
        if !rec.verify() {
            return false;
        }
        let mut records = self.0.records.write().await;
        match records.get(&rec.app_id) {
            Some(old) if old.seq >= rec.seq => false,
            _ => {
                records.insert(rec.app_id.clone(), rec);
                true
            }
        }
    }

    /// Publish a record (owner side, or the gateway API) and keep it.
    pub async fn publish_record(&self, rec: AppRecord) -> Result<()> {
        if !rec.verify() {
            return Err(anyhow!("record does not verify"));
        }
        self.apply_record(rec.clone()).await;
        self.publish(&AppsMsg::Record { record: rec }).await;
        self.persist().await;
        Ok(())
    }

    /// Owner side: hand these secrets to the app's holders as they appear.
    pub async fn own_secrets(&self, app_id: &str, secrets: AppSecrets) {
        self.0
            .owned
            .write()
            .await
            .insert(app_id.to_string(), secrets);
    }

    fn spawn_gossip_reader(&self) {
        let a = self.clone();
        let mut rx = self.0.net.messages();
        tokio::spawn(async move {
            loop {
                let msg: GossipMessage = match rx.recv().await {
                    Ok(m) => m,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                };
                if msg.topic != TOPIC {
                    continue;
                }
                let Some(source) = msg.source else { continue };
                let Ok(m) = serde_json::from_slice::<AppsMsg>(&msg.data) else {
                    continue;
                };
                a.handle_msg(source, m).await;
            }
        });
    }

    async fn handle_msg(&self, source: c0mpute_net::PeerId, m: AppsMsg) {
        let src = source.to_base58();
        match m {
            AppsMsg::Record { record } => {
                if self.apply_record(record).await {
                    debug!(from = %src, "apps: new record");
                }
            }
            AppsMsg::Node {
                x25519,
                free_mb,
                hosts,
                apps,
                addrs,
                owners,
            } => {
                if !crate::handoff::valid_pubkey(&x25519) {
                    return;
                }
                for a in addrs.iter().take(8) {
                    if let Ok(ma) = a.parse::<c0mpute_net::Multiaddr>() {
                        self.0.net.add_peer_address(source, ma).await;
                    }
                }
                let view = NodeView {
                    free_mb,
                    x25519,
                    apps: apps
                        .into_iter()
                        .take(APPS_PER_MESSAGE)
                        .map(|h| (h.app_id.clone(), h))
                        .collect(),
                    hosts,
                    owners: owners.into_iter().take(64).collect(),
                };
                self.0
                    .nodes
                    .write()
                    .await
                    .insert(src, (view, Instant::now()));
            }
            AppsMsg::Seal {
                app_id,
                secrets_hash,
                to,
                from_pub,
                ct,
            } => {
                if to != self.0.me {
                    return;
                }
                self.open_seal(&src, &app_id, &secrets_hash, &from_pub, &ct)
                    .await;
            }
        }
    }

    async fn open_seal(
        &self,
        src: &str,
        app_id: &str,
        secrets_hash: &str,
        from_pub: &str,
        ct: &str,
    ) {
        let Some(rec) = self.0.records.read().await.get(app_id).cloned() else {
            return;
        };
        if rec.secrets_hash.as_deref() != Some(secrets_hash) {
            return;
        }
        // Only the owner or a node that holds the app may hand out secrets.
        let from_owner = rec.owner_peer().as_deref() == Some(src);
        if !from_owner && !rec.allows(src) {
            return;
        }
        let from_holder = self
            .0
            .nodes
            .read()
            .await
            .get(src)
            .map(|(n, _)| n.apps.contains_key(app_id))
            .unwrap_or(false);
        if !from_owner && !from_holder {
            return;
        }
        let Ok(key) = crypto::session_key(
            &self.0.x25519.secret,
            from_pub,
            &snapshot::seal_session(app_id, secrets_hash),
        ) else {
            return;
        };
        let Ok(plain) = crypto::open(&key, ct) else {
            return;
        };
        let Ok(secrets) = serde_json::from_slice::<AppSecrets>(&plain) else {
            return;
        };
        if secrets.hash() != secrets_hash {
            return;
        }
        let mut local = self.0.local.write().await;
        if let Some(l) = local.get_mut(app_id)
            && !l.has_secrets()
        {
            l.secrets = Some(secrets);
            info!(app = %app_id, from = %src, "apps: secrets received");
        }
        drop(local);
        self.persist().await;
    }

    fn spawn_announcer(&self) {
        let a = self.clone();
        tokio::spawn(async move {
            let mut last_records = Instant::now() - a.0.timing.records_every;
            loop {
                if a.is_stopped() {
                    return;
                }
                a.announce().await;
                if last_records.elapsed() >= a.0.timing.records_every {
                    last_records = Instant::now();
                    a.regossip_records().await;
                }
                tokio::time::sleep(a.0.timing.announce_every).await;
            }
        });
    }

    async fn my_view(&self) -> NodeView {
        let local = self.0.local.read().await;
        let used: u32 = local.values().map(|l| l.rec.memory_mb).sum();
        let room = local.len() < self.0.max_apps;
        NodeView {
            free_mb: if room {
                self.0.max_memory_mb.saturating_sub(used)
            } else {
                0
            },
            x25519: self.0.x25519.public_hex.clone(),
            apps: local.iter().map(|(k, l)| (k.clone(), l.held())).collect(),
            hosts: self.0.hosts,
            owners: self.0.owners.clone(),
        }
    }

    /// Announce this node (and count itself in its own view).
    pub async fn announce(&self) {
        let view = self.my_view().await;
        let addrs = self
            .0
            .net
            .advertised_addrs()
            .await
            .into_iter()
            .take(8)
            .map(|a| a.to_string())
            .collect();
        self.0
            .nodes
            .write()
            .await
            .insert(self.0.me.clone(), (view.clone(), Instant::now()));
        self.publish(&AppsMsg::Node {
            x25519: view.x25519,
            free_mb: view.free_mb,
            hosts: view.hosts,
            apps: view.apps.into_values().take(APPS_PER_MESSAGE).collect(),
            addrs,
            owners: view.owners,
        })
        .await;
    }

    /// Re-gossip the records of apps this node holds or owns, so newcomers
    /// learn them without the owner.
    pub async fn regossip_records(&self) {
        let mut ids: HashSet<String> = self.0.local.read().await.keys().cloned().collect();
        ids.extend(self.0.owned.read().await.keys().cloned());
        let records = self.0.records.read().await;
        for id in ids {
            if let Some(r) = records.get(&id) {
                self.publish(&AppsMsg::Record { record: r.clone() }).await;
            }
        }
    }

    /// Live nodes (heard within the TTL), this one included.
    pub async fn live_view(&self) -> HashMap<String, NodeView> {
        let ttl = self.0.timing.node_ttl;
        let mut nodes = self.0.nodes.write().await;
        nodes.retain(|_, (_, at)| at.elapsed() < ttl * 4);
        let mut v: HashMap<String, NodeView> = nodes
            .iter()
            .filter(|(_, (_, at))| at.elapsed() < ttl)
            .map(|(p, (n, _))| (p.clone(), n.clone()))
            .collect();
        drop(nodes);
        v.insert(self.0.me.clone(), self.my_view().await);
        v
    }

    /// Keep connections to the other apps nodes (and a few gateways), found
    /// through their capability ads, so the apps gossip reaches them.
    fn spawn_linker(&self, registry: crate::capabilities::Registry) {
        let a = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                if a.is_stopped() {
                    return;
                }
                let mut peers: Vec<c0mpute_net::PeerId> = registry
                    .find_with_tag(ROLE_TAG)
                    .await
                    .into_iter()
                    .map(|(p, _)| p)
                    .take(16)
                    .collect();
                peers.extend(
                    registry
                        .find_with_tag("c0mpute:role:gateway")
                        .await
                        .into_iter()
                        .map(|(p, _)| p)
                        .take(8),
                );
                for p in peers {
                    if p == a.0.net.peer_id() || a.0.net.is_connected(p).await {
                        continue;
                    }
                    if tokio::time::timeout(Duration::from_secs(10), a.0.net.connect(p))
                        .await
                        .is_ok()
                    {
                        debug!(peer = %p, "apps: linked");
                    }
                }
                tokio::time::sleep(Duration::from_secs(20)).await;
            }
        });
    }

    // ── supervision ──────────────────────────────────────────────────────

    fn spawn_supervisor(&self) {
        let a = self.clone();
        tokio::spawn(async move {
            if a.0.hosts {
                a.sweep_orphans().await;
            }
            loop {
                tokio::time::sleep(a.0.timing.tick).await;
                if a.is_stopped() {
                    return;
                }
                a.tick().await;
            }
        });
    }

    /// Remove app containers this node no longer holds (after a crash).
    async fn sweep_orphans(&self) {
        let held: HashSet<String> = self.0.local.read().await.keys().cloned().collect();
        for id in runtime::labelled().await {
            if !held.contains(&id) {
                let _ = runtime::remove(&id).await;
            } else {
                // Held, but this process did not start it: stop it until the
                // network says this node is primary again.
                let _ = runtime::remove(&id).await;
            }
        }
    }

    /// One supervision pass over every known app.
    pub async fn tick(&self) {
        let records: Vec<AppRecord> = self.0.records.read().await.values().cloned().collect();
        let view = self.live_view().await;
        let mut changed = false;
        for rec in records {
            let v = scoped(&rec, &view);
            changed |= self.step(&rec, &v).await;
            self.seal_to_holders(&rec, &v).await;
        }
        // Apps this node holds whose record vanished: nothing to do but wait.
        if changed {
            self.announce().await;
            self.persist().await;
        }
    }

    /// Bring this node's part of one app in line with the view. True when
    /// this node's own state changed.
    async fn step(&self, rec: &AppRecord, view: &HashMap<String, NodeView>) -> bool {
        let id = rec.app_id.clone();
        let holding = self.0.local.read().await.contains_key(&id);
        if holding && !rec.deleted && !rec.allows(&self.0.me) {
            info!(app = %id, "apps: no longer on the app's host list; dropping");
            self.drop_app(&id).await;
            return true;
        }
        if rec.deleted {
            if holding {
                info!(app = %id, "apps: deleted by owner; dropping");
                self.drop_app(&id).await;
                return true;
            }
            return false;
        }
        if !holding {
            if !self.0.hosts {
                return false;
            }
            if let Some(at) = self.0.released.read().await.get(&id)
                && at.elapsed() < self.0.timing.node_ttl * 4
            {
                return false;
            }
            let fill = placement::fill(&id, &rec.owner, rec.replicas as usize, rec.memory_mb, view);
            if fill.iter().any(|p| *p == self.0.me) {
                info!(app = %id, name = %rec.name, "apps: taking a copy");
                self.0
                    .local
                    .write()
                    .await
                    .insert(id.clone(), Local::new(rec.clone()));
                self.spawn_prepare(id);
                return true;
            }
            return false;
        }

        let mut changed = false;
        // A newer record: take it, reload the image, restart if primary.
        {
            let mut local = self.0.local.write().await;
            if let Some(l) = local.get_mut(&id)
                && l.rec.seq < rec.seq
            {
                l.rec = rec.clone();
                if l.secrets
                    .as_ref()
                    .map(|s| Some(s.hash()) != rec.secrets_hash)
                    .unwrap_or(false)
                {
                    l.secrets = None;
                }
                changed = true;
            }
        }
        if placement::surplus(&id, rec.replicas as usize, view).as_deref()
            == Some(self.0.me.as_str())
        {
            info!(app = %id, "apps: surplus copy; letting go");
            self.0
                .released
                .write()
                .await
                .insert(id.clone(), Instant::now());
            self.drop_app(&id).await;
            return true;
        }
        let (busy, ready, role, running_seq, snap_due, host_port) = {
            let local = self.0.local.read().await;
            let Some(l) = local.get(&id) else {
                return changed;
            };
            let snap_due = l.rec.state_path.is_some()
                && l.last_snap
                    .map(|t| t.elapsed() >= Duration::from_secs(l.rec.snapshot_secs as u64))
                    .unwrap_or(true);
            (
                l.busy,
                l.ready(),
                l.role,
                l.running_seq,
                snap_due,
                l.host_port,
            )
        };
        if busy {
            return changed;
        }
        if !ready {
            self.spawn_prepare(id);
            return changed;
        }
        match placement::primary(&id, rec.needs_secrets(), view) {
            Primary::Running { peer, .. } if peer == self.0.me => {
                // Primary: keep it running the current version, healthy, snapshotted.
                if running_seq != Some(rec.seq) {
                    // (Re)start from the newest state any live holder has;
                    // pull it first when that is not this node.
                    if self.behind_snapshot(rec, view).await {
                        self.maybe_fetch_snapshot(rec, view).await;
                    } else {
                        self.spawn_start(id, placement::newest_snapshot(&rec.app_id, view));
                    }
                } else if let Some(port) = host_port {
                    let ok = runtime::healthy(port, &rec.health_path).await;
                    let alive = ok || runtime::running(&id).await;
                    let mut local = self.0.local.write().await;
                    if let Some(l) = local.get_mut(&id) {
                        if l.healthy != ok {
                            changed = true;
                        }
                        l.healthy = ok;
                        if !alive {
                            warn!(app = %id, "apps: container stopped; restarting");
                            l.running_seq = None;
                            l.failures += 1;
                        }
                    }
                    drop(local);
                    if alive && snap_due {
                        self.spawn_snapshot(id);
                    }
                }
            }
            Primary::Running { epoch, .. } => {
                // Someone else runs it. If this node thought it did, step down.
                // (The guard must be gone before any await that takes `local`
                // again: tokio's RwLock is not reentrant.)
                let stepped_down = {
                    let mut local = self.0.local.write().await;
                    match local.get_mut(&id) {
                        Some(l) if l.role == AppRole::Primary => {
                            warn!(app = %id, epoch, "apps: another primary wins; stepping down");
                            l.role = AppRole::Standby;
                            l.running_seq = None;
                            l.healthy = false;
                            l.volume_epoch = None;
                            true
                        }
                        _ => false,
                    }
                };
                if stepped_down {
                    changed = true;
                    let _ = runtime::remove(&id).await;
                }
                self.maybe_fetch_snapshot(rec, view).await;
            }
            Primary::Elect { peer, epoch } if peer == self.0.me => {
                if self.0.started.elapsed() >= self.0.timing.warmup || role == AppRole::Primary {
                    info!(app = %id, epoch, "apps: promoting to primary");
                    let mut local = self.0.local.write().await;
                    if let Some(l) = local.get_mut(&id) {
                        l.role = AppRole::Primary;
                        l.epoch = epoch;
                        l.running_seq = None;
                    }
                    drop(local);
                    self.announce().await;
                    // Started on the next pass, once the newest snapshot is here.
                    changed = true;
                }
            }
            Primary::Elect { .. } | Primary::None => {
                self.maybe_fetch_snapshot(rec, view).await;
            }
        }
        changed
    }

    async fn set_busy(&self, id: &str, busy: bool) -> bool {
        let mut local = self.0.local.write().await;
        match local.get_mut(id) {
            Some(l) if busy && l.busy => false,
            Some(l) => {
                l.busy = busy;
                true
            }
            None => false,
        }
    }

    async fn drop_app(&self, id: &str) {
        let l = self.0.local.write().await.remove(id);
        let _ = runtime::remove(id).await;
        let _ = runtime::remove_volume(id).await;
        if let Some(l) = l
            && let Some(s) = l.snapshot
        {
            self.delete_snapshot(&s, None).await;
        }
        self.persist().await;
    }

    /// Load (or pull) the image for the current record.
    fn spawn_prepare(&self, id: String) {
        let a = self.clone();
        tokio::spawn(async move {
            if !a.set_busy(&id, true).await {
                return;
            }
            let rec = a.0.local.read().await.get(&id).map(|l| l.rec.clone());
            let res = match rec.as_ref() {
                Some(rec) => a.prepare_image(rec).await,
                None => Ok(()),
            };
            let mut local = a.0.local.write().await;
            if let Some(l) = local.get_mut(&id) {
                l.busy = false;
                match res {
                    Ok(()) => {
                        if let Some(rec) = rec {
                            l.ready_seq = Some(rec.seq);
                            info!(app = %id, seq = rec.seq, "apps: image ready");
                        }
                    }
                    Err(e) => {
                        l.failures += 1;
                        warn!(app = %id, err = %e, "apps: image not ready");
                    }
                }
            }
        });
    }

    async fn prepare_image(&self, rec: &AppRecord) -> Result<()> {
        if runtime::image_present(rec.image_ref()).await {
            return Ok(());
        }
        match &rec.image {
            ImageSource::Pull { reference } => runtime::pull(reference).await,
            ImageSource::Site {
                site_id,
                manifest,
                file,
                tag,
            } => {
                let hosting = self
                    .0
                    .hosting
                    .as_ref()
                    .context("hosting is off on this node")?;
                let site = self
                    .site_record(hosting, site_id)
                    .await
                    .context("image site is not known yet")?;
                if site.manifest != *manifest {
                    anyhow::bail!(
                        "image site is at another version (waiting for the matching record)"
                    );
                }
                let (_, bytes) = hosting
                    .read_file(&site, file)
                    .await?
                    .context("image file missing from site")?;
                let dir = std::env::temp_dir().join(format!("c0mpute-app-{}", rec.app_id));
                std::fs::create_dir_all(&dir)?;
                let path = dir.join("image.tar");
                tokio::fs::write(&path, &bytes).await?;
                drop(bytes);
                let res = runtime::load(&path).await;
                let _ = std::fs::remove_dir_all(&dir);
                res?;
                if !runtime::image_present(tag).await {
                    anyhow::bail!("image loaded but {tag} is not among its tags");
                }
                Ok(())
            }
        }
    }

    async fn site_record(
        &self,
        hosting: &Hosting,
        site_id: &str,
    ) -> Option<crate::hosting::site::SiteRecord> {
        if let Some(r) = hosting.record_for(site_id).await {
            return Some(r);
        }
        hosting.lookup_record(site_id).await
    }

    /// Become (or stay) the running primary: restore if needed, create, start.
    fn spawn_start(&self, id: String, newest: Option<(String, SnapRef)>) {
        let a = self.clone();
        tokio::spawn(async move {
            if !a.set_busy(&id, true).await {
                return;
            }
            let res = a.start_container(&id, newest).await;
            let mut local = a.0.local.write().await;
            if let Some(l) = local.get_mut(&id) {
                l.busy = false;
                match res {
                    Ok(port) => {
                        l.running_seq = Some(l.rec.seq);
                        l.host_port = Some(port);
                        l.volume_epoch = Some(l.epoch);
                        l.failures = 0;
                        info!(app = %id, epoch = l.epoch, port, "apps: running as primary");
                    }
                    Err(e) => {
                        l.failures += 1;
                        warn!(app = %id, err = %e, "apps: start failed");
                        // A local snapshot that does not read back (a piece
                        // missing or damaged) is forgotten, so the next pass
                        // pulls a fresh copy from the other holders instead
                        // of failing on the same bytes again.
                        let msg = format!("{e:#}");
                        if msg.contains("chunk") || msg.contains("snapshot") {
                            l.snapshot = None;
                        }
                        // Give the role up after repeated failures so another
                        // holder can try.
                        if l.failures >= 3 {
                            l.role = AppRole::Standby;
                            l.ready_seq = None;
                        }
                    }
                }
            }
            drop(local);
            a.persist().await;
            a.announce().await;
        });
    }

    async fn start_container(&self, id: &str, newest: Option<(String, SnapRef)>) -> Result<u16> {
        let (rec, secrets, mine, volume_epoch, epoch) = {
            let local = self.0.local.read().await;
            let l = local.get(id).context("not held")?;
            (
                l.rec.clone(),
                l.secrets.clone(),
                l.snapshot.clone(),
                l.volume_epoch,
                l.epoch,
            )
        };
        let secret_env = secrets.as_ref().map(|s| s.env.clone()).unwrap_or_default();
        let _ = runtime::remove(id).await;
        if let Some(state_path) = &rec.state_path {
            let secrets = secrets.context("no secrets for a stateful app")?;
            let key = secrets.key()?;
            // The state to start from: this node's live volume when it was the
            // last primary and nothing newer exists, else the newest snapshot.
            let newest = newest
                .map(|(_, s)| s)
                .into_iter()
                .chain(mine.clone())
                .max_by_key(|s| s.order());
            let keep_volume = match (volume_epoch, &newest) {
                (Some(v), Some(s)) => v >= s.epoch && v + 1 >= epoch,
                (Some(_), None) => true,
                (None, _) => false,
            };
            if !keep_volume {
                // Fresh volume, the snapshot unpacked into it, then the app
                // container on top (see `runtime::restore_state`).
                let _ = runtime::remove_volume(id).await;
                if let Some(s) = newest {
                    let have = mine.as_ref().map(|m| m.order()) == Some(s.order());
                    if !have {
                        anyhow::bail!("newest snapshot is not here yet; fetching first");
                    }
                    let m = self.snap_manifest(&s.manifest).await?;
                    let tar = snapshot::load(&self.0.store, &key, &m).await?;
                    runtime::restore_state(id, &tar).await?;
                    info!(app = %id, epoch = s.epoch, n = s.n, bytes = tar.len(), path = %state_path, "apps: state restored");
                }
                runtime::create(&rec, &secret_env).await?;
            } else {
                runtime::create(&rec, &secret_env).await?;
            }
        } else {
            runtime::create(&rec, &secret_env).await?;
        }
        runtime::start(id).await?;
        let port = runtime::host_port(id, rec.port).await?;
        Ok(port)
    }

    async fn snap_manifest(&self, hash_hex: &str) -> Result<SnapManifest> {
        let h = Hash::from_hex(hash_hex)?;
        SnapManifest::from_bytes(&self.0.store.get(&h).await?)
    }

    fn spawn_snapshot(&self, id: String) {
        let a = self.clone();
        tokio::spawn(async move {
            if !a.set_busy(&id, true).await {
                return;
            }
            let res = a.take_snapshot(&id).await;
            let mut local = a.0.local.write().await;
            if let Some(l) = local.get_mut(&id) {
                l.busy = false;
                l.last_snap = Some(Instant::now());
                match res {
                    Ok(Some((s, old))) => {
                        debug!(app = %id, epoch = s.epoch, n = s.n, "apps: snapshot");
                        l.snapshot = Some(s.clone());
                        drop(local);
                        if let Some(old) = old {
                            a.delete_snapshot(&old, Some(&s)).await;
                        }
                        a.persist().await;
                    }
                    Ok(None) => {}
                    Err(e) => warn!(app = %id, err = %e, "apps: snapshot failed"),
                }
            }
        });
    }

    async fn take_snapshot(&self, id: &str) -> Result<Option<(SnapRef, Option<SnapRef>)>> {
        let (rec, secrets, epoch, prev) = {
            let local = self.0.local.read().await;
            let l = local.get(id).context("not held")?;
            (
                l.rec.clone(),
                l.secrets.clone(),
                l.epoch,
                l.snapshot.clone(),
            )
        };
        let (Some(path), Some(secrets)) = (rec.state_path.as_ref(), secrets) else {
            return Ok(None);
        };
        let tar = runtime::snapshot_state(id, path).await?;
        let n = match &prev {
            Some(p) if p.epoch == epoch => p.n + 1,
            _ => 1,
        };
        let taken_at = now_secs();
        let (h, _) =
            snapshot::store(&self.0.store, &secrets.key()?, id, epoch, n, taken_at, &tar).await?;
        let s = SnapRef {
            manifest: h.to_hex(),
            epoch,
            n,
            taken_at,
        };
        if prev
            .as_ref()
            .map(|p| p.manifest == s.manifest)
            .unwrap_or(false)
        {
            return Ok(Some((s, None)));
        }
        Ok(Some((s, prev)))
    }

    /// Delete an old snapshot's chunks that the new one does not share.
    async fn delete_snapshot(&self, old: &SnapRef, keep: Option<&SnapRef>) {
        let Ok(m) = self.snap_manifest(&old.manifest).await else {
            return;
        };
        let keep_pieces: HashSet<String> = match keep {
            Some(k) => self
                .snap_manifest(&k.manifest)
                .await
                .map(|m| m.pieces.into_iter().collect())
                .unwrap_or_default(),
            None => HashSet::new(),
        };
        if keep_pieces.is_empty() && keep.is_some() {
            return; // could not read the new manifest: keep everything
        }
        for p in m.pieces.iter().filter(|p| !keep_pieces.contains(*p)) {
            if let Ok(h) = Hash::from_hex(p) {
                let _ = self.0.store.delete(&h).await;
            }
        }
        if let Ok(h) = Hash::from_hex(&old.manifest) {
            let _ = self.0.store.delete(&h).await;
        }
    }

    /// Another live holder has a newer snapshot than this node.
    async fn behind_snapshot(&self, rec: &AppRecord, view: &HashMap<String, NodeView>) -> bool {
        if rec.state_path.is_none() {
            return false;
        }
        let Some((peer, s)) = placement::newest_snapshot(&rec.app_id, view) else {
            return false;
        };
        if peer == self.0.me {
            return false;
        }
        let mine = self
            .0
            .local
            .read()
            .await
            .get(&rec.app_id)
            .and_then(|l| l.snapshot.clone());
        !mine.map(|m| m.order() >= s.order()).unwrap_or(false)
    }

    /// Standby: pull the newest snapshot when a holder has one newer.
    async fn maybe_fetch_snapshot(&self, rec: &AppRecord, view: &HashMap<String, NodeView>) {
        if rec.state_path.is_none() {
            return;
        }
        let Some((peer, s)) = placement::newest_snapshot(&rec.app_id, view) else {
            return;
        };
        if peer == self.0.me {
            return;
        }
        let mine = self
            .0
            .local
            .read()
            .await
            .get(&rec.app_id)
            .and_then(|l| l.snapshot.clone());
        if mine
            .as_ref()
            .map(|m| m.order() >= s.order())
            .unwrap_or(false)
        {
            return;
        }
        let a = self.clone();
        let id = rec.app_id.clone();
        let others: Vec<String> = placement::holders(&id, view)
            .into_iter()
            .filter(|p| *p != peer && *p != a.0.me)
            .collect();
        tokio::spawn(async move {
            if !a.set_busy(&id, true).await {
                return;
            }
            let res = a.fetch_snapshot(&peer, &others, &s).await;
            let mut local = a.0.local.write().await;
            if let Some(l) = local.get_mut(&id) {
                l.busy = false;
                match res {
                    Ok(()) => {
                        let old = l.snapshot.replace(s.clone());
                        drop(local);
                        if let Some(old) = old {
                            a.delete_snapshot(&old, Some(&s)).await;
                        }
                        a.persist().await;
                    }
                    Err(e) => debug!(app = %id, err = %e, "apps: snapshot fetch"),
                }
            }
        });
    }

    async fn fetch_chunk(&self, hash: &Hash, peers: &[String]) -> Result<Vec<u8>> {
        if self.0.store.has(hash).await {
            return self.0.store.get(hash).await;
        }
        let req = ChunkRequest {
            chunk_hash: *hash,
            shard_index: None,
        };
        let mut last = "no peers".to_string();
        for p in peers {
            let Ok(pid) = p.parse::<c0mpute_net::PeerId>() else {
                continue;
            };
            match tokio::time::timeout(
                self.0.timing.fetch_timeout,
                self.0.net.fetch_from_peer(pid, &req),
            )
            .await
            {
                Ok(Ok(b)) if Hash::of(&b) == *hash => {
                    self.0.store.put(&b).await?;
                    return Ok(b);
                }
                Ok(Ok(_)) => last = format!("{p}: bytes do not match"),
                Ok(Err(e)) => last = format!("{p}: {e}"),
                Err(_) => last = format!("{p}: timed out"),
            }
        }
        anyhow::bail!("chunk {}: {last}", hash.to_hex())
    }

    async fn fetch_snapshot(&self, peer: &str, others: &[String], s: &SnapRef) -> Result<()> {
        let mut peers = vec![peer.to_string()];
        peers.extend(others.iter().cloned());
        let mh = Hash::from_hex(&s.manifest)?;
        let m = SnapManifest::from_bytes(&self.fetch_chunk(&mh, &peers).await?)?;
        for p in &m.pieces {
            self.fetch_chunk(&Hash::from_hex(p)?, &peers).await?;
        }
        Ok(())
    }

    /// Seal secrets to holders that lack them: the owner for every holder,
    /// a holder for the others (rate-limited per peer).
    async fn seal_to_holders(&self, rec: &AppRecord, view: &HashMap<String, NodeView>) {
        let Some(hash) = rec.secrets_hash.clone() else {
            return;
        };
        let secrets = match self.0.owned.read().await.get(&rec.app_id).cloned() {
            Some(s) => Some(s),
            None => self
                .0
                .local
                .read()
                .await
                .get(&rec.app_id)
                .and_then(|l| l.secrets.clone()),
        };
        let Some(secrets) = secrets.filter(|s| s.hash() == hash) else {
            return;
        };
        for (peer, n) in view {
            if *peer == self.0.me {
                continue;
            }
            let Some(h) = n.apps.get(&rec.app_id) else {
                continue;
            };
            if h.has_secrets || h.seq != rec.seq {
                continue;
            }
            let key = (rec.app_id.clone(), peer.clone());
            if let Some(at) = self.0.sealed_to.read().await.get(&key)
                && at.elapsed() < Duration::from_secs(30)
            {
                continue;
            }
            let Ok(k) = crypto::session_key(
                &self.0.x25519.secret,
                &n.x25519,
                &snapshot::seal_session(&rec.app_id, &hash),
            ) else {
                continue;
            };
            let Ok(ct) = crypto::seal(&k, &secrets.to_bytes()) else {
                continue;
            };
            self.0.sealed_to.write().await.insert(key, Instant::now());
            debug!(app = %rec.app_id, to = %peer, "apps: sealing secrets");
            self.publish(&AppsMsg::Seal {
                app_id: rec.app_id.clone(),
                secrets_hash: hash.clone(),
                to: peer.clone(),
                from_pub: self.0.x25519.public_hex.clone(),
                ct,
            })
            .await;
        }
    }

    // ── queries ──────────────────────────────────────────────────────────

    pub async fn record(&self, app_id: &str) -> Option<AppRecord> {
        self.0.records.read().await.get(app_id).cloned()
    }

    /// Ask connected peers for a record this node has not heard yet.
    pub async fn lookup_record(&self, app_id: &str) -> Option<AppRecord> {
        if let Some(r) = self.record(app_id).await {
            return Some(r);
        }
        for p in self.0.net.connected_peers().await.into_iter().take(12) {
            let Ok(Ok(body)) = tokio::time::timeout(
                Duration::from_secs(5),
                self.0.net.rpc(p, RPC_RECORD, app_id.as_bytes().to_vec()),
            )
            .await
            else {
                continue;
            };
            if let Ok(rec) = serde_json::from_slice::<AppRecord>(&body)
                && rec.app_id == app_id
                && self.apply_record(rec.clone()).await
            {
                return Some(rec);
            }
        }
        self.record(app_id).await
    }

    pub async fn info(&self, rec: &AppRecord, view: &HashMap<String, NodeView>) -> AppInfo {
        let view = &scoped(rec, view);
        let holders = placement::holders(&rec.app_id, view);
        let p = placement::primary(&rec.app_id, rec.needs_secrets(), view);
        let (primary, epoch, healthy) = match &p {
            Primary::Running { peer, epoch } => (
                Some(peer.clone()),
                *epoch,
                view.get(peer)
                    .and_then(|n| n.apps.get(&rec.app_id))
                    .map(|a| a.healthy)
                    .unwrap_or(false),
            ),
            _ => (None, 0, false),
        };
        AppInfo {
            app_id: rec.app_id.clone(),
            name: rec.name.clone(),
            owner: rec.owner.clone(),
            seq: rec.seq,
            replicas: rec.replicas,
            holders,
            primary,
            epoch,
            healthy,
            snapshot: placement::newest_snapshot(&rec.app_id, view).map(|(_, s)| s),
            domains: rec.domains.clone(),
            path: format!("/a/{}/", rec.app_id),
        }
    }

    pub async fn apps(&self) -> Vec<AppInfo> {
        let view = self.live_view().await;
        let records: Vec<AppRecord> = self
            .0
            .records
            .read()
            .await
            .values()
            .filter(|r| !r.deleted)
            .cloned()
            .collect();
        let mut out = Vec::with_capacity(records.len());
        for r in &records {
            out.push(self.info(r, &view).await);
        }
        out.sort_by(|a, b| a.name.cmp(&b.name).then(a.app_id.cmp(&b.app_id)));
        out
    }

    /// Owner side: the last `tail` log lines from the primary.
    pub async fn logs(&self, app_id: &str, tail: u32) -> Result<String> {
        let rec = self.lookup_record(app_id).await.context("unknown app")?;
        let view = scoped(&rec, &self.live_view().await);
        let peer = placement::primary(app_id, rec.needs_secrets(), &view)
            .peer()
            .map(str::to_string)
            .context("no primary is running this app right now")?;
        let pid: c0mpute_net::PeerId = peer.parse().map_err(|_| anyhow!("bad peer id"))?;
        let body = serde_json::to_vec(&serde_json::json!({"app_id": app_id, "tail": tail}))?;
        let out = self.0.net.rpc(pid, RPC_LOGS, body).await?;
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    // ── serving ──────────────────────────────────────────────────────────

    /// Forward one request to the app's primary (local or over libp2p).
    pub async fn forward(&self, req: proxy::HttpReq) -> Response {
        let Some(rec) = self.lookup_record(&req.app_id).await.filter(|r| !r.deleted) else {
            return (StatusCode::NOT_FOUND, "no such app").into_response();
        };
        let view = scoped(&rec, &self.live_view().await);
        let Some(peer) = placement::primary(&rec.app_id, rec.needs_secrets(), &view)
            .peer()
            .map(str::to_string)
        else {
            return proxy::unavailable("this app has no running copy right now");
        };
        if peer == self.0.me {
            return proxy::into_response(self.serve_local(&req).await);
        }
        let Ok(pid) = peer.parse::<c0mpute_net::PeerId>() else {
            return proxy::unavailable("bad primary peer id");
        };
        let Ok(body) = serde_json::to_vec(&req) else {
            return (StatusCode::BAD_REQUEST, "bad request").into_response();
        };
        match self.0.net.rpc(pid, RPC_HTTP, body).await {
            Ok(b) => match serde_json::from_slice::<proxy::HttpResp>(&b) {
                Ok(r) => proxy::into_response(r),
                Err(_) => proxy::unavailable("the primary sent a malformed answer"),
            },
            Err(e) => {
                debug!(app = %rec.app_id, err = %e, "apps: forward failed");
                proxy::unavailable("the app's primary did not answer; failover may be in progress")
            }
        }
    }

    async fn serve_local(&self, req: &proxy::HttpReq) -> proxy::HttpResp {
        let port = {
            let local = self.0.local.read().await;
            local
                .get(&req.app_id)
                .filter(|l| l.role == AppRole::Primary && l.running_seq.is_some())
                .and_then(|l| l.host_port)
        };
        match port {
            Some(p) => proxy::forward_local(p, req).await,
            None => proxy::HttpResp {
                status: 503,
                headers: vec![("retry-after".into(), "10".into())],
                body: b"not the primary for this app".to_vec(),
            },
        }
    }

    /// Serve a request whose Host is a custom domain of an app. `None` when
    /// no app claims the host (the caller serves it some other way).
    pub async fn serve_host(&self, host: &str, req: Request) -> Result<Response, Request> {
        let candidates: Vec<AppRecord> = self
            .0
            .records
            .read()
            .await
            .values()
            .filter(|r| !r.deleted && r.domains.iter().any(|d| d == host))
            .cloned()
            .collect();
        for rec in candidates {
            if self.0.dns.confirms_app(host, &rec.app_id).await {
                let path = req
                    .uri()
                    .path_and_query()
                    .map(|p| p.as_str().to_string())
                    .unwrap_or_else(|| "/".into());
                return Ok(self
                    .proxy_request(&rec.app_id, &path, Some(host), req, None)
                    .await);
            }
        }
        Err(req)
    }

    async fn proxy_request(
        &self,
        app_id: &str,
        path: &str,
        host: Option<&str>,
        req: Request,
        prefix: Option<String>,
    ) -> Response {
        let (parts, body) = req.into_parts();
        let Ok(bytes) = axum::body::to_bytes(body, proxy::MAX_REQUEST_BODY).await else {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body too large for an app",
            )
                .into_response();
        };
        let client = parts
            .headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let proto = parts
            .headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("http")
            .to_string();
        let mut env = match proxy::wrap(
            app_id,
            &parts.method,
            path,
            &parts.headers,
            bytes.to_vec(),
            host,
            &proto,
            client.as_deref(),
        ) {
            Ok(e) => e,
            Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
        };
        if let Some(p) = prefix {
            env.headers.push(("x-forwarded-prefix".into(), p));
        }
        self.forward(env).await
    }

    pub fn router(&self) -> Router {
        Router::new()
            .route("/apps/v1/apps", get(api_list))
            .route("/apps/v1/apps/{id}", get(api_get))
            .route("/apps/v1/records", post(api_post_record))
            .route("/a/{app}", any(redirect_root))
            .route("/a/{app}/", any(proxy_root))
            .route("/a/{app}/{*path}", any(proxy_path))
            .with_state(self.clone())
    }
}

async fn api_list(State(a): State<Apps>) -> Response {
    axum::Json(serde_json::json!({ "apps": a.apps().await })).into_response()
}

async fn api_get(State(a): State<Apps>, AxPath(id): AxPath<String>) -> Response {
    let Some(rec) = a.lookup_record(&id).await else {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({"error": "no such app"})),
        )
            .into_response();
    };
    let view = a.live_view().await;
    let info = a.info(&rec, &view).await;
    axum::Json(serde_json::json!({ "app": info, "record": rec })).into_response()
}

/// Accept an owner-signed record from any client that can sign one.
async fn api_post_record(State(a): State<Apps>, body: axum::body::Bytes) -> Response {
    let Ok(rec) = serde_json::from_slice::<AppRecord>(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({"error": "not an app record"})),
        )
            .into_response();
    };
    match a.publish_record(rec.clone()).await {
        Ok(()) => (
            StatusCode::ACCEPTED,
            axum::Json(serde_json::json!({"app_id": rec.app_id, "seq": rec.seq})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn redirect_root(AxPath(app): AxPath<String>, uri: axum::http::Uri) -> Response {
    if !record::valid_app_id(&app) {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    let q = uri.query().map(|q| format!("?{q}")).unwrap_or_default();
    Redirect::permanent(&format!("/a/{app}/{q}")).into_response()
}

async fn proxy_root(State(a): State<Apps>, AxPath(app): AxPath<String>, req: Request) -> Response {
    proxy_to(a, app, String::new(), req).await
}

async fn proxy_path(
    State(a): State<Apps>,
    AxPath((app, path)): AxPath<(String, String)>,
    req: Request,
) -> Response {
    proxy_to(a, app, path, req).await
}

async fn proxy_to(a: Apps, app: String, path: String, req: Request<Body>) -> Response {
    if !record::valid_app_id(&app) {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let q = req
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let full = format!("/{path}{q}");
    a.proxy_request(&app, &full, host.as_deref(), req, Some(format!("/a/{app}")))
        .await
}

/// Lets the hosting fallback hand custom-domain requests to apps first.
#[async_trait::async_trait]
impl crate::hosting::HostHook for Apps {
    async fn serve_host(&self, host: &str, req: Request) -> Result<Response, Request> {
        Apps::serve_host(self, host, req).await
    }
}

struct HttpHandler(Apps);

#[async_trait::async_trait]
impl RpcHandler for HttpHandler {
    async fn handle(
        &self,
        _peer: c0mpute_net::PeerId,
        body: Vec<u8>,
    ) -> std::result::Result<Vec<u8>, String> {
        let req: proxy::HttpReq =
            serde_json::from_slice(&body).map_err(|_| "bad request".to_string())?;
        let resp = self.0.serve_local(&req).await;
        serde_json::to_vec(&resp).map_err(|e| e.to_string())
    }
}

struct LogsHandler(Apps);

#[async_trait::async_trait]
impl RpcHandler for LogsHandler {
    async fn handle(
        &self,
        peer: c0mpute_net::PeerId,
        body: Vec<u8>,
    ) -> std::result::Result<Vec<u8>, String> {
        #[derive(Deserialize)]
        struct Q {
            app_id: String,
            tail: u32,
        }
        let q: Q = serde_json::from_slice(&body).map_err(|_| "bad request".to_string())?;
        let rec = self.0.record(&q.app_id).await.ok_or("unknown app")?;
        if rec.owner_peer().as_deref() != Some(peer.to_base58().as_str()) {
            return Err("only the app's owner may read its logs".into());
        }
        if !self.0.0.local.read().await.contains_key(&q.app_id) {
            return Err("this node does not hold the app".into());
        }
        runtime::logs(&q.app_id, q.tail)
            .await
            .map(String::into_bytes)
            .map_err(|e| e.to_string())
    }
}

struct RecordHandler(Apps);

#[async_trait::async_trait]
impl RpcHandler for RecordHandler {
    async fn handle(
        &self,
        _peer: c0mpute_net::PeerId,
        body: Vec<u8>,
    ) -> std::result::Result<Vec<u8>, String> {
        let id = String::from_utf8(body).map_err(|_| "bad id".to_string())?;
        let rec = self.0.record(id.trim()).await.ok_or("unknown app")?;
        serde_json::to_vec(&rec).map_err(|e| e.to_string())
    }
}
