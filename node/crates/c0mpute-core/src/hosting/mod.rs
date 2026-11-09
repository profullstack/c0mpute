//! Hosting: static sites kept alive by the network itself.
//!
//! * Every hosting node gossips what it holds on [`TOPIC`] once a minute, and
//!   re-gossips the signed records of the sites it holds. From that alone every
//!   node knows where each site's copies are ([`replicas::ReplicaMap`]).
//! * When a site has fewer than [`TARGET_REPLICAS`] live copies, the hosting
//!   nodes that rank highest for it (rendezvous hashing, computed the same on
//!   every node) fetch a copy. A node that drops off stops announcing, its
//!   copy stops counting after [`Timing::holder_ttl`], and another node takes
//!   over. No leader, no coordinator.
//! * Every gateway is a proxy: `/s/<site_id>/...`, or a custom domain whose
//!   DNS TXT `_c0mpute-hosting.<domain>` names the site, is served from local
//!   chunks when this node holds them and otherwise fetched over libp2p from
//!   the fastest live holder (round-robin among equally fast ones), failing
//!   over to the next.

pub mod dns;
pub mod replicas;
pub mod site;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use axum::Router;
use axum::body::Body;
use axum::extract::{Path as AxPath, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use c0mpute_net::GossipMessage;
use c0mpute_net::{Libp2pNetwork, Network};
use c0mpute_proto::{ChunkRequest, Hash};
use c0mpute_store::ChunkStore;
use serde::{Deserialize, Serialize};
use tokio::sync::{RwLock, Semaphore};
use tracing::{debug, info, warn};

use replicas::{Action, ReplicaMap};
use site::{SiteManifest, SiteRecord};

pub const TOPIC: &str = "c0mpute/hosting/v1";
pub const ROLE_TAG: &str = "c0mpute:role:hosting";
/// Copies the network keeps of every site.
pub const TARGET_REPLICAS: usize = 3;
/// Above this many copies, the lowest-ranked holder lets go.
pub const MAX_REPLICAS: usize = 5;
/// Default bytes a node will hold for other people's sites.
pub const DEFAULT_MAX_BYTES: u64 = 5 * 1024 * 1024 * 1024;
/// Most bytes a node holds for any one owner key, so one publisher cannot
/// fill every node's budget.
pub const MAX_BYTES_PER_OWNER: u64 = 1024 * 1024 * 1024;
/// Sites per holding message, so one message stays well under gossipsub's
/// 64 KiB limit.
const SITES_PER_MESSAGE: usize = 200;

/// How often things happen. Tests shrink these.
#[derive(Clone, Debug)]
pub struct Timing {
    pub announce_every: Duration,
    pub records_every: Duration,
    pub repair_every: Duration,
    pub holder_ttl: Duration,
    pub fetch_timeout: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            announce_every: Duration::from_secs(60),
            records_every: Duration::from_secs(300),
            repair_every: Duration::from_secs(30),
            holder_ttl: Duration::from_secs(200),
            fetch_timeout: Duration::from_secs(20),
        }
    }
}

/// Gossip on [`TOPIC`].
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum HostingMsg {
    /// A signed site record (published, or re-gossiped by a holder).
    Record { record: SiteRecord },
    /// "I host, and these are the site versions I hold." `part`/`parts` split
    /// long lists over several messages.
    Holding {
        sites: Vec<Held>,
        #[serde(default)]
        part: u32,
        #[serde(default = "one")]
        parts: u32,
        /// False for a publisher seeding its own site: a source, not a copy.
        #[serde(default = "yes")]
        hosts: bool,
        /// Where to dial the announcer (multiaddrs, at most 8), so any node
        /// can fetch from a holder it has no connection to yet.
        #[serde(default)]
        addrs: Vec<String>,
    },
}

fn one() -> u32 {
    1
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Held {
    pub site_id: String,
    pub manifest: String,
    pub seq: u64,
}

/// Sites this node holds, persisted next to the chunk store.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Catalog {
    sites: HashMap<String, SiteRecord>,
}

struct Inner {
    net: Arc<Libp2pNetwork>,
    store: ChunkStore,
    me: String,
    /// This node holds sites for the network (the `hosting` role).
    hosts: bool,
    max_bytes: u64,
    timing: Timing,
    map: RwLock<ReplicaMap>,
    catalog: RwLock<Catalog>,
    catalog_path: Option<PathBuf>,
    /// Parsed manifests by hash (small cache).
    manifests: RwLock<HashMap<String, Arc<SiteManifest>>>,
    replicating: RwLock<HashSet<String>>,
    turn: AtomicUsize,
    stopped: std::sync::atomic::AtomicBool,
    fetch_slots: Semaphore,
    dns: dns::Verifier,
    /// Optional handler for requests with a query string (podcast ad
    /// insertion installs one); it may decline and let the plain serve run.
    hook: std::sync::OnceLock<Arc<dyn ServeHook>>,
}

/// A handler a gateway can put in front of plain site serving for requests
/// that carry a query string (e.g. `?ads=1` on a podcast episode).
#[async_trait::async_trait]
pub trait ServeHook: Send + Sync {
    /// `None` = not mine, serve the file as usual.
    async fn serve(&self, hosting: &Hosting, rec: &SiteRecord, path: &str, query: &str, headers: &HeaderMap) -> Option<Response>;
}

/// The hosting service: one per node. Cheap to clone.
#[derive(Clone)]
pub struct Hosting(Arc<Inner>);

/// Options for [`Hosting::start`].
pub struct Options {
    pub hosts: bool,
    pub max_bytes: u64,
    pub catalog_path: Option<PathBuf>,
    pub timing: Timing,
    pub dns: dns::Verifier,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            hosts: false,
            max_bytes: DEFAULT_MAX_BYTES,
            catalog_path: None,
            timing: Timing::default(),
            dns: dns::Verifier::default(),
        }
    }
}

impl Hosting {
    /// Subscribe to [`TOPIC`] and start the gossip, announce and repair loops.
    pub async fn start(net: Arc<Libp2pNetwork>, store: ChunkStore, opts: Options) -> Result<Self> {
        let catalog = opts
            .catalog_path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice::<Catalog>(&b).ok())
            .unwrap_or_default();
        let me = net.peer_id().to_base58();
        let inner = Arc::new(Inner {
            net: net.clone(),
            store,
            me,
            hosts: opts.hosts,
            max_bytes: opts.max_bytes,
            timing: opts.timing,
            map: RwLock::new(ReplicaMap::default()),
            catalog: RwLock::new(Catalog::default()),
            catalog_path: opts.catalog_path,
            manifests: RwLock::new(HashMap::new()),
            replicating: RwLock::new(HashSet::new()),
            turn: AtomicUsize::new(0),
            stopped: std::sync::atomic::AtomicBool::new(false),
            fetch_slots: Semaphore::new(8),
            dns: opts.dns,
            hook: std::sync::OnceLock::new(),
        });
        let h = Hosting(inner);
        // Sites held before a restart: keep them only if their chunks are
        // still here.
        for (id, rec) in catalog.sites {
            if h.has_complete_copy(&rec).await {
                h.0.catalog.write().await.sites.insert(id, rec.clone());
                h.0.map.write().await.apply_record(rec);
            }
        }
        net.subscribe(TOPIC).await.context("subscribe to hosting topic")?;
        h.spawn_gossip_reader();
        h.spawn_announcer();
        h.spawn_repair();
        Ok(h)
    }

    pub fn peer_id(&self) -> &str {
        &self.0.me
    }

    /// Stop announcing and repairing (the node is leaving). Its copies stop
    /// counting once the network's holder TTL passes.
    pub fn stop(&self) {
        self.0.stopped.store(true, Ordering::Relaxed);
    }

    fn is_stopped(&self) -> bool {
        self.0.stopped.load(Ordering::Relaxed)
    }

    // ── gossip ───────────────────────────────────────────────────────────

    fn spawn_gossip_reader(&self) {
        let h = self.clone();
        let mut rx = self.0.net.messages();
        tokio::spawn(async move {
            // Partial holding lists from multi-part announcements.
            let mut partial: HashMap<String, (u32, HashMap<String, (String, u64)>)> = HashMap::new();
            loop {
                let msg: GossipMessage = match rx.recv().await {
                    Ok(m) => m,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                };
                if msg.topic != TOPIC {
                    continue;
                }
                let Some(source) = msg.source.map(|p| p.to_base58()) else {
                    continue;
                };
                let Ok(m) = serde_json::from_slice::<HostingMsg>(&msg.data) else {
                    continue;
                };
                match m {
                    HostingMsg::Record { record } => {
                        let changed = h.0.map.write().await.apply_record(record);
                        if changed {
                            debug!(from = %source, "hosting: new site record");
                        }
                    }
                    HostingMsg::Holding {
                        sites,
                        part,
                        parts,
                        hosts,
                        addrs,
                    } => {
                        if let Some(pid) = msg.source {
                            for a in addrs.iter().take(8) {
                                if let Ok(ma) = a.parse::<c0mpute_net::Multiaddr>() {
                                    h.0.net.add_peer_address(pid, ma).await;
                                }
                            }
                        }
                        let parts = parts.clamp(1, 1000);
                        let entry = partial.entry(source.clone()).or_insert((0, HashMap::new()));
                        if part == 0 {
                            *entry = (0, HashMap::new());
                        }
                        for s in sites.into_iter().take(SITES_PER_MESSAGE) {
                            entry.1.insert(s.site_id, (s.manifest, s.seq));
                        }
                        entry.0 += 1;
                        if part + 1 >= parts {
                            let (_, all) = partial.remove(&source).unwrap_or_default();
                            h.0.map
                                .write()
                                .await
                                .apply_holding_as(&source, all, Instant::now(), hosts);
                        }
                    }
                }
            }
        });
    }

    async fn publish(&self, msg: &HostingMsg) {
        if let Ok(bytes) = serde_json::to_vec(msg)
            && let Err(e) = self.0.net.publish(TOPIC, bytes).await
        {
            debug!(err = %e, "hosting: publish (mesh forming?)");
        }
    }

    /// Announce what this node holds, and count itself in its own map.
    pub async fn announce_holding(&self) {
        let held: Vec<Held> = self
            .0
            .catalog
            .read()
            .await
            .sites
            .values()
            .map(|r| Held {
                site_id: r.site_id.clone(),
                manifest: r.manifest.clone(),
                seq: r.seq,
            })
            .collect();
        let mine: HashMap<String, (String, u64)> = held
            .iter()
            .map(|h| (h.site_id.clone(), (h.manifest.clone(), h.seq)))
            .collect();
        if self.0.hosts || !mine.is_empty() {
            self.0
                .map
                .write()
                .await
                .apply_holding_as(&self.0.me, mine, Instant::now(), self.0.hosts);
        } else {
            return;
        }
        let chunks: Vec<&[Held]> = if held.is_empty() {
            vec![&[]]
        } else {
            held.chunks(SITES_PER_MESSAGE).collect()
        };
        let parts = chunks.len() as u32;
        let addrs: Vec<String> = self
            .0
            .net
            .advertised_addrs()
            .await
            .into_iter()
            .filter(|a| !a.to_string().contains("/ip4/0.0.0.0") && !a.to_string().contains("/ip6/::/"))
            .take(8)
            .map(|a| a.to_string())
            .collect();
        for (i, c) in chunks.into_iter().enumerate() {
            self.publish(&HostingMsg::Holding {
                sites: c.to_vec(),
                part: i as u32,
                parts,
                hosts: self.0.hosts,
                addrs: addrs.clone(),
            })
            .await;
        }
    }

    async fn announce_records(&self) {
        let recs: Vec<SiteRecord> = self.0.catalog.read().await.sites.values().cloned().collect();
        for record in recs {
            self.publish(&HostingMsg::Record { record }).await;
        }
    }

    fn spawn_announcer(&self) {
        let h = self.clone();
        tokio::spawn(async move {
            let mut last_records = Instant::now() - h.0.timing.records_every;
            loop {
                if h.is_stopped() {
                    return;
                }
                h.announce_holding().await;
                if last_records.elapsed() >= h.0.timing.records_every {
                    h.announce_records().await;
                    last_records = Instant::now();
                }
                tokio::time::sleep(jitter(h.0.timing.announce_every)).await;
            }
        });
    }

    // ── repair ───────────────────────────────────────────────────────────

    fn spawn_repair(&self) {
        let h = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(jitter(h.0.timing.repair_every)).await;
                if h.is_stopped() {
                    return;
                }
                h.repair_once().await;
            }
        });
    }

    /// One pass: prune silent peers, then replicate, update or drop.
    pub async fn repair_once(&self) {
        let actions: Vec<(SiteRecord, Action)> = {
            let mut map = self.0.map.write().await;
            map.prune(Instant::now(), self.0.timing.holder_ttl);
            map.records
                .values()
                .map(|r| {
                    let a = replicas::decide(
                        &map,
                        &r.site_id,
                        &self.0.me,
                        self.0.hosts,
                        TARGET_REPLICAS,
                        MAX_REPLICAS,
                    );
                    (r.clone(), a)
                })
                .collect()
        };
        for (rec, action) in actions {
            // A site we hold whose record moved on: fetch the new version.
            let stale = self
                .0
                .catalog
                .read()
                .await
                .sites
                .get(&rec.site_id)
                .is_some_and(|held| held.seq < rec.seq);
            match action {
                Action::Replicate => self.spawn_replicate(rec),
                Action::Drop => {
                    if let Err(e) = self.drop_site(&rec.site_id).await {
                        warn!(site = %rec.site_id, err = %e, "hosting: drop failed");
                    }
                }
                Action::Nothing if stale => self.spawn_replicate(rec),
                Action::Nothing => {}
            }
        }
    }

    fn spawn_replicate(&self, rec: SiteRecord) {
        let h = self.clone();
        tokio::spawn(async move {
            {
                let mut busy = h.0.replicating.write().await;
                if !busy.insert(rec.site_id.clone()) {
                    return;
                }
            }
            let id = rec.site_id.clone();
            match h.replicate(&rec).await {
                Ok(()) => {
                    info!(site = %id, name = %rec.name, seq = rec.seq, "hosting: holding a copy");
                    h.announce_holding().await;
                }
                Err(e) => warn!(site = %id, err = %e, "hosting: replicate failed"),
            }
            h.0.replicating.write().await.remove(&id);
        });
    }

    async fn held_bytes(&self) -> u64 {
        self.0.catalog.read().await.sites.values().map(|r| r.size).sum()
    }

    /// Fetch every chunk of `rec` and start holding it.
    pub async fn replicate(&self, rec: &SiteRecord) -> Result<()> {
        let previous = self.0.catalog.read().await.sites.get(&rec.site_id).cloned();
        let already = previous.as_ref().map(|p| p.size).unwrap_or(0);
        if self.held_bytes().await - already + rec.size > self.0.max_bytes {
            bail!("over this node's hosting budget");
        }
        let owner_bytes: u64 = self
            .0
            .catalog
            .read()
            .await
            .sites
            .values()
            .filter(|r| r.owner == rec.owner && r.site_id != rec.site_id)
            .map(|r| r.size)
            .sum();
        if owner_bytes + rec.size > MAX_BYTES_PER_OWNER {
            bail!("over this node's per-owner hosting budget");
        }
        let manifest = self.manifest(rec).await?;
        if manifest.total_bytes() != rec.size || manifest.name != rec.name {
            bail!("manifest does not match its record");
        }
        let holders = self.0.map.read().await.sources(&rec.site_id);
        for h in manifest.chunk_hashes() {
            let hash = Hash::from_hex(&h).context("bad chunk hash")?;
            if !self.0.store.has(&hash).await {
                let bytes = self.fetch_chunk(&hash, &holders).await?;
                self.0.store.put(&bytes).await?;
            }
        }
        let mbytes = manifest.to_bytes();
        self.0.store.put(&mbytes).await?;
        {
            let mut cat = self.0.catalog.write().await;
            cat.sites.insert(rec.site_id.clone(), rec.clone());
        }
        self.save_catalog().await;
        if let Some(old) = previous
            && old.manifest != rec.manifest
        {
            self.collect_garbage(&old, Some(&manifest)).await;
        }
        let _ = self
            .0
            .net
            .announce(&Hash::from_hex(&rec.manifest).context("manifest hash")?)
            .await;
        Ok(())
    }

    async fn drop_site(&self, site_id: &str) -> Result<()> {
        let Some(rec) = self.0.catalog.write().await.sites.remove(site_id) else {
            return Ok(());
        };
        self.save_catalog().await;
        self.collect_garbage(&rec, None).await;
        info!(site = %site_id, "hosting: released a surplus copy");
        self.announce_holding().await;
        Ok(())
    }

    /// Delete chunks of `old` that no held site (and not `keep`) still uses.
    async fn collect_garbage(&self, old: &SiteRecord, keep: Option<&SiteManifest>) {
        let Ok(old_manifest) = self.manifest(old).await else {
            return;
        };
        let mut live: HashSet<String> = keep.map(|m| m.chunk_hashes().into_iter().collect()).unwrap_or_default();
        let held: Vec<SiteRecord> = self.0.catalog.read().await.sites.values().cloned().collect();
        for r in held {
            live.insert(r.manifest.clone());
            if let Ok(m) = self.manifest(&r).await {
                live.extend(m.chunk_hashes());
            }
        }
        for h in old_manifest.chunk_hashes().into_iter().chain([old.manifest.clone()]) {
            if !live.contains(&h)
                && let Ok(hash) = Hash::from_hex(&h)
            {
                let _ = self.0.store.delete(&hash).await;
            }
        }
    }

    async fn save_catalog(&self) {
        let Some(path) = &self.0.catalog_path else {
            return;
        };
        let bytes = serde_json::to_vec_pretty(&*self.0.catalog.read().await).unwrap_or_default();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, &bytes).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }

    async fn has_complete_copy(&self, rec: &SiteRecord) -> bool {
        let Ok(mh) = Hash::from_hex(&rec.manifest) else {
            return false;
        };
        let Ok(bytes) = self.0.store.get(&mh).await else {
            return false;
        };
        let Ok(m) = SiteManifest::from_bytes(&bytes) else {
            return false;
        };
        for h in m.chunk_hashes() {
            match Hash::from_hex(&h) {
                Ok(hash) if self.0.store.has(&hash).await => {}
                _ => return false,
            }
        }
        true
    }

    // ── publishing ───────────────────────────────────────────────────────

    /// Store a built site locally, hold it, and gossip its record. The caller
    /// keeps the node up until enough peers hold copies ([`Self::holders`]).
    pub async fn publish_site(&self, built: &site::BuiltSite, record: SiteRecord) -> Result<()> {
        for c in &built.chunks {
            self.0.store.put(c).await?;
        }
        let mbytes = built.manifest.to_bytes();
        let mh = self.0.store.put(&mbytes).await?;
        if mh.to_hex() != record.manifest {
            bail!("record does not point at this manifest");
        }
        if !self.0.map.write().await.apply_record(record.clone()) {
            bail!("record rejected (bad signature, or an equal or newer version exists)");
        }
        self.0
            .catalog
            .write()
            .await
            .sites
            .insert(record.site_id.clone(), record.clone());
        self.save_catalog().await;
        self.publish(&HostingMsg::Record { record }).await;
        self.announce_holding().await;
        let _ = self.0.net.announce(&mh).await;
        Ok(())
    }

    /// Re-gossip a record (publishers repeat it while the mesh forms).
    pub async fn regossip(&self, record: &SiteRecord) {
        self.publish(&HostingMsg::Record {
            record: record.clone(),
        })
        .await;
        self.announce_holding().await;
    }

    /// Live holders of a site's current version.
    pub async fn holders(&self, site_id: &str) -> Vec<String> {
        self.0.map.read().await.holders(site_id)
    }

    /// Every known site: record plus live copy count.
    pub async fn sites(&self) -> Vec<(SiteRecord, usize)> {
        let map = self.0.map.read().await;
        let mut v: Vec<(SiteRecord, usize)> = map
            .records
            .values()
            .map(|r| (r.clone(), map.holders(&r.site_id).len()))
            .collect();
        v.sort_by(|a, b| a.0.name.cmp(&b.0.name));
        v
    }

    // ── fetching ─────────────────────────────────────────────────────────

    async fn fetch_chunk(&self, hash: &Hash, holders: &[String]) -> Result<Vec<u8>> {
        let _slot = self.0.fetch_slots.acquire().await?;
        if self.0.store.has(hash).await {
            return self.0.store.get(hash).await;
        }
        let order = {
            let turn = self.0.turn.fetch_add(1, Ordering::Relaxed);
            self.0.map.read().await.proxy_order(holders, turn)
        };
        let req = ChunkRequest {
            chunk_hash: *hash,
            shard_index: None,
        };
        let mut last = String::from("no holders");
        for peer in order.iter().filter(|p| **p != self.0.me) {
            let Ok(pid) = peer.parse::<c0mpute_net::PeerId>() else {
                continue;
            };
            let started = Instant::now();
            match tokio::time::timeout(self.0.timing.fetch_timeout, self.0.net.fetch_from_peer(pid, &req)).await {
                Ok(Ok(bytes)) if Hash::of(&bytes) == *hash => {
                    let ms = started.elapsed().as_secs_f64() * 1000.0;
                    self.0.map.write().await.observe_rtt(peer, ms);
                    return Ok(bytes);
                }
                Ok(Ok(_)) => last = format!("{peer} sent bytes that do not match the hash"),
                Ok(Err(e)) => last = format!("{peer}: {e}"),
                Err(_) => last = format!("{peer}: timed out"),
            }
            // A failed holder sorts last until it proves itself again.
            self.0.map.write().await.observe_rtt(peer, 5_000.0);
        }
        // Last resort: the DHT (any peer that announced the chunk).
        match tokio::time::timeout(self.0.timing.fetch_timeout, self.0.net.fetch(&req)).await {
            Ok(Ok(bytes)) if Hash::of(&bytes) == *hash => Ok(bytes),
            _ => bail!("could not fetch chunk {}: {last}", hash.to_hex()),
        }
    }

    async fn manifest(&self, rec: &SiteRecord) -> Result<Arc<SiteManifest>> {
        if let Some(m) = self.0.manifests.read().await.get(&rec.manifest) {
            return Ok(m.clone());
        }
        let hash = Hash::from_hex(&rec.manifest).context("manifest hash")?;
        let holders = self.0.map.read().await.sources(&rec.site_id);
        let bytes = self.fetch_chunk(&hash, &holders).await?;
        let m = Arc::new(SiteManifest::from_bytes(&bytes)?);
        let mut cache = self.0.manifests.write().await;
        if cache.len() > 256 {
            cache.clear();
        }
        cache.insert(rec.manifest.clone(), m.clone());
        Ok(m)
    }

    // ── serving ──────────────────────────────────────────────────────────

    /// The current record of a site, when this node knows it.
    pub async fn record_for(&self, site_id: &str) -> Option<SiteRecord> {
        self.0.map.read().await.records.get(site_id).cloned()
    }

    /// Put a [`ServeHook`] in front of site serving (once per node).
    pub fn set_serve_hook(&self, hook: Arc<dyn ServeHook>) -> bool {
        self.0.hook.set(hook).is_ok()
    }

    /// The file a request path resolves to, without its bytes.
    pub async fn resolve(&self, rec: &SiteRecord, path: &str) -> Result<Option<site::FileEntry>> {
        let manifest = self.manifest(rec).await?;
        Ok(site::resolve_path(&manifest, path).map(|(_, e)| e.clone()))
    }

    /// Read a whole file of a site (local chunks, else fetched from holders).
    /// `Ok(None)` when the site has no such file.
    pub async fn read_file(&self, rec: &SiteRecord, path: &str) -> Result<Option<(site::FileEntry, Vec<u8>)>> {
        let manifest = self.manifest(rec).await?;
        let Some(entry) = manifest.files.get(path).cloned() else {
            return Ok(None);
        };
        let body = self.read_entry(rec, &entry).await?;
        Ok(Some((entry, body)))
    }

    async fn read_entry(&self, rec: &SiteRecord, entry: &site::FileEntry) -> Result<Vec<u8>> {
        let holders = self.0.map.read().await.sources(&rec.site_id);
        let mut body = Vec::with_capacity(entry.size as usize);
        for h in &entry.chunks {
            let hash = Hash::from_hex(h).context("bad chunk hash")?;
            body.extend_from_slice(&self.fetch_chunk(&hash, &holders).await?);
        }
        Ok(body)
    }

    /// Serve `path` of a site: from local chunks, else proxied from holders.
    pub async fn serve(&self, rec: &SiteRecord, path: &str) -> Response {
        let manifest = match self.manifest(rec).await {
            Ok(m) => m,
            Err(e) => {
                warn!(site = %rec.site_id, err = %e, "hosting: manifest unavailable");
                return (StatusCode::BAD_GATEWAY, "site is unavailable right now").into_response();
            }
        };
        let (entry, status) = match site::resolve_path(&manifest, path) {
            Some((_, e)) => (e.clone(), StatusCode::OK),
            None => match manifest.files.get("404.html") {
                Some(e) => (e.clone(), StatusCode::NOT_FOUND),
                None => return (StatusCode::NOT_FOUND, "not found").into_response(),
            },
        };
        let body = match self.read_entry(rec, &entry).await {
            Ok(b) => b,
            Err(e) => {
                warn!(site = %rec.site_id, err = %e, "hosting: chunk unavailable");
                return (StatusCode::BAD_GATEWAY, "site is unavailable right now").into_response();
            }
        };
        let etag = format!("\"{}\"", &Hash::of(entry.chunks.join(",").as_bytes()).to_hex()[..32]);
        let mut resp = (status, Body::from(body)).into_response();
        let hdr = resp.headers_mut();
        if let Ok(v) = HeaderValue::from_str(&entry.content_type) {
            hdr.insert(header::CONTENT_TYPE, v);
        }
        if let Ok(v) = HeaderValue::from_str(&etag) {
            hdr.insert(header::ETAG, v);
        }
        hdr.insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=60"));
        hdr.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
        hdr.insert("x-c0mpute-site", HeaderValue::from_str(&rec.site_id).unwrap_or(HeaderValue::from_static("")));
        resp
    }

    /// Gateway routes: `/s/{site}/...`, `/hosting/v1/sites`, and custom
    /// domains (any other Host whose DNS names a site).
    pub fn router(&self) -> Router {
        Router::new()
            .route("/hosting/v1/sites", get(list_sites))
            .route("/s/{site}", get(serve_root))
            .route("/s/{site}/", get(serve_root))
            .route("/s/{site}/{*path}", get(serve_path))
            .fallback(serve_by_host)
            .with_state(self.clone())
    }
}

fn jitter(d: Duration) -> Duration {
    let ms = d.as_millis() as u64;
    let spread = ms / 10;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|t| t.subsec_nanos() as u64)
        .unwrap_or(0);
    Duration::from_millis(ms - spread + if spread == 0 { 0 } else { nanos % (2 * spread) })
}

async fn list_sites(State(h): State<Hosting>) -> Response {
    let sites: Vec<serde_json::Value> = h
        .sites()
        .await
        .into_iter()
        .map(|(r, copies)| {
            serde_json::json!({
                "site_id": r.site_id,
                "name": r.name,
                "owner": r.owner,
                "seq": r.seq,
                "size": r.size,
                "domains": r.domains,
                "copies": copies,
                "path": format!("/s/{}/", r.site_id),
            })
        })
        .collect();
    axum::Json(serde_json::json!({ "target_copies": TARGET_REPLICAS, "sites": sites })).into_response()
}

async fn serve_root(
    State(h): State<Hosting>,
    AxPath(site): AxPath<String>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    serve_site(&h, &site, "", uri.query(), &headers).await
}

async fn serve_path(
    State(h): State<Hosting>,
    AxPath((site, path)): AxPath<(String, String)>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    serve_site(&h, &site, &path, uri.query(), &headers).await
}

async fn serve_site(h: &Hosting, site: &str, path: &str, query: Option<&str>, headers: &HeaderMap) -> Response {
    match h.record_for(site).await {
        Some(rec) => h.serve_with_hook(&rec, path, query, headers).await,
        None => (StatusCode::NOT_FOUND, "no such site on the network (yet)").into_response(),
    }
}

impl Hosting {
    async fn serve_with_hook(&self, rec: &SiteRecord, path: &str, query: Option<&str>, headers: &HeaderMap) -> Response {
        if let (Some(q), Some(hook)) = (query.filter(|q| !q.is_empty()), self.0.hook.get())
            && let Some(resp) = hook.serve(self, rec, path, q, headers).await
        {
            return resp;
        }
        self.serve(rec, path).await
    }
}

/// Any request not matched by another gateway route: serve it when its Host
/// is a custom domain of a site and DNS agrees.
async fn serve_by_host(State(h): State<Hosting>, headers: HeaderMap, uri: axum::http::Uri) -> Response {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(':').next().unwrap_or("").trim_end_matches('.').to_ascii_lowercase())
        .unwrap_or_default();
    if host.is_empty() || !site::valid_domain(&host) {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    let candidates: Vec<SiteRecord> = h
        .0
        .map
        .read()
        .await
        .records
        .values()
        .filter(|r| r.domains.iter().any(|d| *d == host))
        .cloned()
        .collect();
    for rec in candidates {
        if h.0.dns.confirms(&host, &rec.site_id).await {
            return h.serve_with_hook(&rec, uri.path(), uri.query(), &headers).await;
        }
    }
    (StatusCode::NOT_FOUND, "not found").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holding_messages_round_trip() {
        let m = HostingMsg::Holding {
            sites: vec![Held {
                site_id: "a".into(),
                manifest: "b".into(),
                seq: 1,
            }],
            part: 0,
            parts: 1,
            hosts: true,
            addrs: vec![],
        };
        let bytes = serde_json::to_vec(&m).unwrap();
        assert!(bytes.len() < 1024);
        let back: HostingMsg = serde_json::from_slice(&bytes).unwrap();
        assert!(matches!(back, HostingMsg::Holding { .. }));
    }

    #[test]
    fn a_full_holding_message_fits_gossipsub() {
        let sites: Vec<Held> = (0..SITES_PER_MESSAGE)
            .map(|i| Held {
                site_id: format!("{:020}", i),
                manifest: "f".repeat(64),
                seq: u64::MAX,
            })
            .collect();
        let bytes = serde_json::to_vec(&HostingMsg::Holding {
            sites,
            part: 0,
            parts: 1,
            hosts: true,
            addrs: vec!["/ip4/203.0.113.7/tcp/40001".into(); 8],
        })
        .unwrap();
        assert!(bytes.len() < 64 * 1024, "{} bytes", bytes.len());
    }
}

/// The identity sites are published (and owned) with: this machine's buyer
/// key, `<data_dir>/buyer/identity.key`.
pub fn publisher_keypair() -> Result<c0mpute_net::libp2p::identity::Keypair> {
    let id_dir = crate::config::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("buyer");
    c0mpute_net::identity::load_or_create(&id_dir)
}

/// `c0mpute hosting publish`: build a site from `dir`, sign it with this
/// machine's buyer identity, hand it to the network, and stay up until
/// `TARGET_REPLICAS` other nodes hold copies (or `wait` runs out).
pub async fn publish_dir(
    dir: &std::path::Path,
    name: &str,
    domains: Vec<String>,
    wait: Duration,
) -> Result<(SiteRecord, usize)> {
    for d in &domains {
        if !site::valid_domain(d) {
            bail!("invalid domain {d:?}");
        }
    }
    let built = site::build_from_dir(dir, name)?;
    let supervisor = crate::Supervisor::boot_buyer(crate::Config::default()).await?;
    let keypair = publisher_keypair()?;
    let manifest_hash = Hash::of(&built.manifest.to_bytes());
    let seq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let record = SiteRecord::sign(
        &keypair,
        name,
        &manifest_hash,
        seq,
        built.manifest.total_bytes(),
        domains,
    )?;
    let hosting = Hosting::start(supervisor.libp2p.clone(), supervisor.store.clone(), Options::default()).await?;
    // Let the swarm reach peers before the first gossip.
    tokio::time::sleep(Duration::from_secs(5)).await;
    hosting.publish_site(&built, record.clone()).await?;
    eprintln!(
        "published {} ({} files, {} bytes) as site {}",
        name,
        built.manifest.files.len(),
        record.size,
        record.site_id
    );
    let started = Instant::now();
    let mut copies = 0;
    while started.elapsed() < wait {
        tokio::time::sleep(Duration::from_secs(5)).await;
        hosting.regossip(&record).await;
        let others = hosting.holders(&record.site_id).await.len();
        if others != copies {
            copies = others;
            eprintln!("  {copies}/{TARGET_REPLICAS} network copies");
        }
        if copies >= TARGET_REPLICAS {
            break;
        }
    }
    Ok((record, copies))
}
