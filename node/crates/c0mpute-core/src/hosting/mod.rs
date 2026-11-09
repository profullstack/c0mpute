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
//! * Marketplace sites (v2 records) name preferred hosts picked from d1sks
//!   ([`market`]). Holders are challenged with random chunk fetches and the
//!   challenger signs availability receipts ([`proof`]); preferred holders
//!   that pass are paid per GiB-month by the site's payer ([`settle`],
//!   through [`crate::payments`], off unless the operator turns it on).

pub mod dns;
mod edge_api;
pub mod market;
pub mod meta;
pub mod proof;
pub mod replicas;
pub mod settle;
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

use crate::payments::{Micros, Payee};
use proof::Receipt;
use replicas::{Action, ReplicaMap};
use site::{Manifest, ManifestPart, PartRef, SiteRecord};

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
        /// The hosting price this node asks to be a preferred host, USD per
        /// GiB-month (decimal). Absent from older nodes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        price: Option<String>,
        /// Where it is paid for marketplace copies.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payout: Option<Payee>,
        /// It hosts but cannot take another copy (disk at its free floor or
        /// hosting budget used up). Other nodes leave it out of the
        /// rendezvous fill, so a site it would rank first for still gets its
        /// copies from the next ranked nodes. Absent (false) from older nodes.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        full: bool,
        /// Free disk above its floor, whole GiB: nodes with more room rank
        /// first in the rendezvous fill. Absent from older nodes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        room_gib: Option<u64>,
    },
    /// A signed availability receipt (proof-of-storage challenge result).
    /// Older nodes do not know this variant and skip it.
    Receipt { receipt: Receipt },
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
    /// Every site record this node has seen (records.json next to the
    /// catalog), so a restart does not forget sites whose holders are quiet.
    records_path: Option<PathBuf>,
    /// A record arrived since records.json was last written.
    records_dirty: std::sync::atomic::AtomicBool,
    /// Parsed manifests by hash (small cache).
    manifests: RwLock<HashMap<String, Arc<Manifest>>>,
    /// Parts of large sites' manifests, by hash (a small cache: a gateway
    /// loads only the parts requests touch).
    parts: RwLock<HashMap<String, Arc<ManifestPart>>>,
    replicating: RwLock<HashSet<String>>,
    turn: AtomicUsize,
    stopped: std::sync::atomic::AtomicBool,
    fetch_slots: Semaphore,
    dns: dns::Verifier,
    /// Optional handler for requests with a query string (podcast ad
    /// insertion installs one); it may decline and let the plain serve run.
    hook: std::sync::OnceLock<Arc<dyn ServeHook>>,
    /// Optional handler for custom domains that are not sites (apps).
    host_hook: std::sync::OnceLock<Arc<dyn HostHook>>,
    /// Signs challenge receipts (and is the payer identity for settlement).
    challenger: Option<c0mpute_net::libp2p::identity::Keypair>,
    challenger_hex: Option<String>,
    challenge: ChallengeScope,
    challenge_every: Duration,
    price: Option<Micros>,
    payout: Option<Payee>,
    /// Receipts this node signed, for settlement (kept on disk too).
    own_receipts: RwLock<Vec<Receipt>>,
    receipts_path: Option<PathBuf>,
}

/// Which sites a node challenges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChallengeScope {
    Off,
    /// Sites whose owner or payer is the challenger key.
    Own,
    /// Own sites, plus a sample of every other site's holders.
    All,
}

impl ChallengeScope {
    pub fn parse(s: &str) -> Self {
        match s.trim() {
            "off" | "none" | "false" => ChallengeScope::Off,
            "all" => ChallengeScope::All,
            _ => ChallengeScope::Own,
        }
    }
}

/// Most (site, holder) challenges in one round, so a gateway that knows many
/// sites stays at a few MiB of challenge traffic per round.
const CHALLENGES_PER_ROUND: usize = 32;
/// Receipts kept in memory/on disk for settlement.
const OWN_RECEIPTS_MAX: usize = 200_000;

/// A handler a gateway can put in front of plain site serving for requests
/// that carry a query string (e.g. `?ads=1` on a podcast episode).
#[async_trait::async_trait]
pub trait ServeHook: Send + Sync {
    /// `None` = not mine, serve the file as usual.
    async fn serve(&self, hosting: &Hosting, rec: &SiteRecord, path: &str, query: &str, headers: &HeaderMap) -> Option<Response>;
}

/// Something else a gateway serves on custom domains (apps), tried before
/// static sites. It takes the whole request (any method, with its body) and
/// hands it back when the host is not its own.
#[async_trait::async_trait]
pub trait HostHook: Send + Sync {
    async fn serve_host(&self, host: &str, req: axum::extract::Request) -> std::result::Result<Response, axum::extract::Request>;
}

/// RPC that answers a site record by id, so a client that just joined need
/// not wait for the next record gossip.
pub const RPC_RECORD: &str = "hosting.record";

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
    /// Key that signs proof-of-storage receipts; no challenges without it.
    pub challenger: Option<c0mpute_net::libp2p::identity::Keypair>,
    pub challenge: ChallengeScope,
    pub challenge_every: Duration,
    /// Hosting price this node asks (micro-USD per GiB-month), gossiped.
    pub price: Option<Micros>,
    /// Payout address, gossiped so payers can pay this node.
    pub payout: Option<Payee>,
    /// JSONL file of this node's own receipts (survives restarts).
    pub receipts_path: Option<PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            hosts: false,
            max_bytes: DEFAULT_MAX_BYTES,
            catalog_path: None,
            timing: Timing::default(),
            dns: dns::Verifier::default(),
            challenger: None,
            challenge: ChallengeScope::Own,
            challenge_every: Duration::from_secs(600),
            price: None,
            payout: None,
            receipts_path: None,
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
        let own_receipts: Vec<Receipt> = opts
            .receipts_path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|text| {
                let mut v: Vec<Receipt> = text
                    .lines()
                    .filter_map(|l| serde_json::from_str::<Receipt>(l).ok())
                    .filter(|r| r.verify())
                    .collect();
                if v.len() > OWN_RECEIPTS_MAX {
                    v.drain(..v.len() - OWN_RECEIPTS_MAX);
                }
                v
            })
            .unwrap_or_default();
        let challenger_hex = opts.challenger.as_ref().and_then(site::owner_hex);
        let records_path = opts.catalog_path.as_ref().map(|p| p.with_file_name("records.json"));
        let known: Vec<SiteRecord> = records_path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
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
            records_path,
            records_dirty: std::sync::atomic::AtomicBool::new(false),
            manifests: RwLock::new(HashMap::new()),
            parts: RwLock::new(HashMap::new()),
            replicating: RwLock::new(HashSet::new()),
            turn: AtomicUsize::new(0),
            stopped: std::sync::atomic::AtomicBool::new(false),
            fetch_slots: Semaphore::new(8),
            dns: opts.dns,
            hook: std::sync::OnceLock::new(),
            host_hook: std::sync::OnceLock::new(),
            challenger: opts.challenger,
            challenger_hex,
            challenge: opts.challenge,
            challenge_every: opts.challenge_every,
            price: opts.price,
            payout: opts.payout.filter(|p| p.valid()),
            own_receipts: RwLock::new(own_receipts),
            receipts_path: opts.receipts_path,
        });
        let h = Hosting(inner);
        // Records seen before a restart (each re-verified).
        {
            let mut map = h.0.map.write().await;
            for rec in known {
                map.apply_record(rec);
            }
        }
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
        h.spawn_challenger();
        net.register_rpc(RPC_RECORD, Arc::new(RecordRpc(h.clone())));
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
                            h.0.records_dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                            debug!(from = %source, "hosting: new site record");
                        }
                    }
                    HostingMsg::Holding {
                        sites,
                        part,
                        parts,
                        hosts,
                        addrs,
                        price,
                        payout,
                        full,
                        room_gib,
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
                            let price = price.as_deref().and_then(|p| crate::payments::parse_usd(p).ok());
                            let payout = payout.filter(|p| p.valid());
                            let mut map = h.0.map.write().await;
                            map.apply_holding_meta(&source, all, Instant::now(), hosts, price, payout);
                            map.set_full(&source, full);
                            map.set_room(&source, room_gib);
                        }
                    }
                    HostingMsg::Receipt { receipt } => {
                        h.0.map.write().await.apply_receipt(receipt);
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
        let (price, payout) = if self.0.hosts {
            (self.0.price, self.0.payout.clone())
        } else {
            (None, None)
        };
        let full = self.0.hosts && self.is_full().await;
        let room_gib = if self.0.hosts { self.room_gib() } else { None };
        if self.0.hosts || !mine.is_empty() {
            let mut map = self.0.map.write().await;
            map.apply_holding_meta(&self.0.me, mine, Instant::now(), self.0.hosts, price, payout.clone());
            map.set_full(&self.0.me, full);
            map.set_room(&self.0.me, room_gib);
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
                price: price.map(crate::payments::format_usd),
                payout: payout.clone(),
                full,
                room_gib,
            })
            .await;
        }
    }

    async fn announce_records(&self) {
        let mut recs: Vec<SiteRecord> = self.0.catalog.read().await.sites.values().cloned().collect();
        // Also keep short sites alive: a record we know but do not hold, with
        // fewer than the target live copies, is re-gossiped too (a few per
        // round). Otherwise a site whose holders are quiet on gossip and whose
        // publisher has exited is forgotten by every node that restarts.
        {
            let map = self.0.map.read().await;
            let held: HashSet<&String> = recs.iter().map(|r| &r.site_id).collect();
            let mut short: Vec<&SiteRecord> = map
                .records
                .values()
                .filter(|r| !held.contains(&r.site_id) && map.holders(&r.site_id).len() < TARGET_REPLICAS)
                .collect();
            short.sort_by(|a, b| b.seq.cmp(&a.seq));
            recs.extend(short.into_iter().take(SHORT_RECORDS_PER_ROUND).cloned());
        }
        for record in recs {
            self.publish(&HostingMsg::Record { record }).await;
        }
        self.0.records_dirty.store(false, std::sync::atomic::Ordering::Relaxed);
        self.save_records().await;
    }

    /// Write every known record to records.json (newest first, capped).
    async fn save_records(&self) {
        let Some(path) = &self.0.records_path else {
            return;
        };
        let mut recs: Vec<SiteRecord> = self.0.map.read().await.records.values().cloned().collect();
        recs.sort_by(|a, b| b.seq.cmp(&a.seq));
        recs.truncate(MAX_SAVED_RECORDS);
        let Ok(bytes) = serde_json::to_vec(&recs) else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, &bytes).is_ok() {
            let _ = std::fs::rename(&tmp, path);
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
                } else if h.0.records_dirty.swap(false, std::sync::atomic::Ordering::Relaxed) {
                    h.save_records().await;
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
        // A full node does not put itself forward for new copies (it would be
        // chosen, fail, and hold the slot from the next ranked node).
        let can_take = self.0.hosts && !self.is_full().await;
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
                        can_take,
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

    /// True when this node cannot take another copy: its disk is at the free
    /// floor (with a little headroom) or its hosting budget is used up.
    pub async fn is_full(&self) -> bool {
        let (disk_free, disk_total) = crate::opendisk::disk_for(self.0.store.root());
        !leaves_free_floor(disk_free, disk_total, FULL_HEADROOM_BYTES)
            || self.held_bytes().await.saturating_add(FULL_HEADROOM_BYTES) > self.0.max_bytes
    }

    /// Free disk above the floor in whole GiB, or None when the disk is unknown.
    fn room_gib(&self) -> Option<u64> {
        let (disk_free, disk_total) = crate::opendisk::disk_for(self.0.store.root());
        if disk_total == 0 {
            return None;
        }
        Some(disk_free.saturating_sub(free_disk_floor(disk_total)) / GIB_BYTES)
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
        let (disk_free, disk_total) = crate::opendisk::disk_for(self.0.store.root());
        if !leaves_free_floor(disk_free, disk_total, rec.size.saturating_sub(already)) {
            bail!(
                "disk nearly full: {} GiB free, a copy must leave at least {} GiB",
                disk_free / GIB_BYTES,
                free_disk_floor(disk_total) / GIB_BYTES
            );
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
        if manifest.total_bytes() != rec.size || manifest.name() != rec.name {
            bail!("manifest does not match its record");
        }
        let holders = self.0.map.read().await.sources(&rec.site_id);
        // Files part by part (a large site's chunk list never sits in memory
        // whole); the root goes in last, marking the copy complete.
        let mut todo: Vec<String> = Vec::new();
        match &*manifest {
            Manifest::Inline(m) => todo.extend(m.chunk_hashes()),
            Manifest::Parted(r) => {
                for pr in &r.parts {
                    let part = self.part(&rec.site_id, pr).await?;
                    let hashes: Vec<String> = part.chunk_hashes().cloned().collect();
                    self.store_chunks(&hashes, &holders).await?;
                    self.0.store.put(&serde_json::to_vec(&*part)?).await?;
                }
            }
        }
        self.store_chunks(&todo, &holders).await?;
        let mh = Hash::from_hex(&rec.manifest).context("manifest hash")?;
        let mbytes = self.fetch_chunk(&mh, &holders).await?;
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
        // Tell the network first, and keep the chunks (still served to
        // fetches and challenges) until peers have certainly seen that we no
        // longer hold the site. Deleting first made challengers that still
        // listed us record a failure against a copy we had just let go.
        self.announce_holding().await;
        info!(site = %site_id, "hosting: released a surplus copy");
        let h = self.clone();
        let grace = self.0.timing.holder_ttl;
        tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            // Taken back in the meantime: keep everything.
            if h.0.catalog.read().await.sites.contains_key(&rec.site_id) {
                return;
            }
            h.collect_garbage(&rec, None).await;
        });
        Ok(())
    }

    /// Delete chunks of `old` that no held site (and not `keep`) still uses.
    async fn collect_garbage(&self, old: &SiteRecord, keep: Option<&Manifest>) {
        let Ok(old_manifest) = self.manifest(old).await else {
            return;
        };
        let mut live: HashSet<String> = HashSet::new();
        if let Some(k) = keep {
            live.extend(self.chunk_set(&old.site_id, k).await);
        }
        let held: Vec<SiteRecord> = self.0.catalog.read().await.sites.values().cloned().collect();
        for r in held {
            live.insert(r.manifest.clone());
            if let Ok(m) = self.manifest(&r).await {
                live.extend(self.chunk_set(&r.site_id, &m).await);
            }
        }
        let doomed = self.chunk_set(&old.site_id, &old_manifest).await;
        for h in doomed.into_iter().chain([old.manifest.clone()]) {
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
        let Ok(m) = Manifest::from_bytes(&bytes) else {
            return false;
        };
        let mut hashes: Vec<String> = Vec::new();
        match &m {
            Manifest::Inline(m) => hashes.extend(m.chunk_hashes()),
            Manifest::Parted(r) => {
                for pr in &r.parts {
                    let Ok(ph) = Hash::from_hex(&pr.hash) else { return false };
                    let Ok(pb) = self.0.store.get(&ph).await else { return false };
                    let Ok(part) = ManifestPart::from_bytes(&pb, pr) else { return false };
                    for h in part.chunk_hashes() {
                        match Hash::from_hex(h) {
                            Ok(hash) if self.0.store.has(&hash).await => {}
                            _ => return false,
                        }
                    }
                }
            }
        }
        for h in hashes {
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
        let stored = built.stored();
        for p in &stored.parts {
            self.0.store.put(p).await?;
        }
        let mh = self.0.store.put(&stored.root).await?;
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

    /// A waiting publisher's self-heal step: re-gossip the record, re-announce
    /// the manifest on the DHT, and, when the swarm has no peers left (a seed
    /// or gateway went down), re-dial the bootstrap seeds. Every failure is
    /// logged and skipped; nothing here aborts a publish.
    pub async fn reannounce(&self, record: &SiteRecord) {
        let net = &self.0.net;
        if net.connected_peers().await.is_empty() {
            let seeds = c0mpute_net::bootstrap::fetch_or_fallback(
                c0mpute_net::bootstrap::DEFAULT_BOOTSTRAP_URL,
                c0mpute_net::bootstrap::default_seeds(),
            )
            .await;
            let mut dialed = 0;
            for addr in seeds {
                match net.dial(addr.clone()).await {
                    Ok(()) => dialed += 1,
                    Err(e) => {
                        debug!(%addr, err = %e, "hosting: re-dial seed failed; trying the next")
                    }
                }
            }
            eprintln!("  no peers connected; re-dialed {dialed} bootstrap seed(s)");
        }
        self.regossip(record).await;
        if let Ok(mh) = Hash::from_hex(&record.manifest)
            && let Err(e) = net.announce(&mh).await
        {
            debug!(err = %e, "hosting: manifest re-announce failed (will retry)");
        }
    }

    /// Live holders of a site's current version.
    pub async fn holders(&self, site_id: &str) -> Vec<String> {
        self.0.map.read().await.holders(site_id)
    }

    /// Hosting peers heard announcing themselves on the gossip within the
    /// holder TTL, sorted (this node included when it hosts).
    pub async fn live_hosts(&self) -> Vec<String> {
        let mut map = self.0.map.write().await;
        map.prune(Instant::now(), self.0.timing.holder_ttl);
        map.live_peers()
    }

    /// Listen to the hosting gossip until every peer in `want` has announced
    /// itself as a hosting node, or `max` passes. Returns the live hosts.
    pub async fn wait_for_hosts(&self, want: &[String], max: Duration) -> Vec<String> {
        let started = Instant::now();
        loop {
            let live = self.live_hosts().await;
            if want.iter().all(|w| live.contains(w)) || started.elapsed() >= max {
                return live;
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
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

    // ── proof of storage ─────────────────────────────────────────────────

    fn spawn_challenger(&self) {
        if self.0.challenger.is_none() || self.0.challenge == ChallengeScope::Off {
            return;
        }
        let h = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(jitter(h.0.challenge_every)).await;
                if h.is_stopped() {
                    return;
                }
                let n = h.challenge_round().await.len();
                if n > 0 {
                    debug!(receipts = n, "hosting: challenge round");
                }
            }
        });
    }

    /// The hex key receipts are signed with (the payer identity), if any.
    pub fn challenger_key(&self) -> Option<&str> {
        self.0.challenger_hex.as_deref()
    }

    /// One challenge round: for each site in scope, ask every live holder
    /// for one random chunk, straight from that peer, and sign a receipt.
    /// Receipts are applied locally and gossiped. Returns them.
    pub async fn challenge_round(&self) -> Vec<Receipt> {
        let (Some(kp), Some(me_key)) = (self.0.challenger.clone(), self.0.challenger_hex.clone()) else {
            return Vec::new();
        };
        if self.0.challenge == ChallengeScope::Off {
            return Vec::new();
        }
        let (mut own, mut others): (Vec<(SiteRecord, String)>, Vec<(SiteRecord, String)>) = (Vec::new(), Vec::new());
        {
            let map = self.0.map.read().await;
            for rec in map.records.values() {
                let mine = rec.owner == me_key || rec.payer_key() == me_key;
                if !mine && self.0.challenge != ChallengeScope::All {
                    continue;
                }
                for holder in map.holders(&rec.site_id) {
                    if holder == self.0.me {
                        continue;
                    }
                    if mine {
                        own.push((rec.clone(), holder));
                    } else {
                        others.push((rec.clone(), holder));
                    }
                }
            }
        }
        own.sort_by(|a, b| a.0.site_id.cmp(&b.0.site_id).then(a.1.cmp(&b.1)));
        // Others: a random sample each round.
        {
            use rand::seq::SliceRandom;
            others.shuffle(&mut rand::thread_rng());
        }
        let picks: Vec<(SiteRecord, String)> = own.into_iter().chain(others).take(CHALLENGES_PER_ROUND).collect();
        let mut out = Vec::new();
        for (rec, holder) in picks {
            match self.challenge_one(&kp, &rec, &holder).await {
                Ok(r) => out.push(r),
                Err(e) => debug!(site = %rec.site_id, %holder, err = %e, "hosting: no challenge"),
            }
        }
        out
    }

    /// Challenge one holder of one site and record the signed receipt.
    pub async fn challenge_one(
        &self,
        kp: &c0mpute_net::libp2p::identity::Keypair,
        rec: &SiteRecord,
        holder: &str,
    ) -> Result<Receipt> {
        let manifest = self.manifest(rec).await?;
        let nonce: [u8; 32] = rand::random();
        let chunk = match &*manifest {
            Manifest::Inline(m) => proof::pick_chunk(m, &nonce),
            Manifest::Parted(r) => {
                // A random part, then a random chunk of it (or the part itself).
                let n = u64::from_le_bytes(Hash::of(&nonce).0[8..16].try_into().expect("8 bytes"));
                let pr = &r.parts[(n % r.parts.len() as u64) as usize];
                let part = self.part(&rec.site_id, pr).await?;
                let mut all: Vec<String> = part.chunk_hashes().cloned().collect();
                all.push(pr.hash.clone());
                all.sort();
                all.dedup();
                proof::pick_from(&all, &nonce)
            }
        }
        .context("site has no chunks")?;
        let hash = Hash::from_hex(&chunk).context("bad chunk hash")?;
        let pid: c0mpute_net::PeerId = holder.parse().context("holder is not a peer id")?;
        let req = ChunkRequest {
            chunk_hash: hash,
            shard_index: None,
        };
        let started = Instant::now();
        let got = tokio::time::timeout(self.0.timing.fetch_timeout, self.0.net.fetch_from_peer(pid, &req)).await;
        let ok = matches!(&got, Ok(Ok(bytes)) if Hash::of(bytes) == hash);
        if !ok && !self.0.map.read().await.holders(&rec.site_id).iter().any(|p| p == holder) {
            // The holder announced it let the copy go while we were asking:
            // not a failed proof, just a stale target. No receipt.
            bail!("{holder} no longer holds {}", rec.site_id);
        }
        let rtt_ms = if ok { started.elapsed().as_millis().min(u32::MAX as u128) as u32 } else { 0 };
        let receipt = Receipt::sign(
            kp,
            proof::Challenge {
                site_id: &rec.site_id,
                manifest: &rec.manifest,
                holder,
                chunk: &chunk,
                ok,
                rtt_ms,
                at: crate::payments::now_secs(),
            },
        )?;
        self.record_receipt(receipt.clone()).await;
        Ok(receipt)
    }

    /// Keep a receipt this node signed: in the map, in the settlement log,
    /// and on gossip.
    async fn record_receipt(&self, r: Receipt) {
        self.0.map.write().await.apply_receipt(r.clone());
        {
            let mut own = self.0.own_receipts.write().await;
            own.push(r.clone());
            if own.len() > OWN_RECEIPTS_MAX {
                let extra = own.len() - OWN_RECEIPTS_MAX;
                own.drain(..extra);
            }
        }
        if let Some(p) = &self.0.receipts_path
            && let Ok(line) = serde_json::to_string(&r)
        {
            use std::io::Write;
            if let Some(d) = p.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
                let _ = writeln!(f, "{line}");
            }
        }
        self.publish(&HostingMsg::Receipt { receipt: r }).await;
    }

    /// Receipts this node signed.
    pub async fn own_receipts(&self) -> Vec<Receipt> {
        self.0.own_receipts.read().await.clone()
    }

    /// Receipts the network gossiped for one holder of one site.
    pub async fn receipts_for(&self, site_id: &str, holder: &str) -> Vec<Receipt> {
        self.0.map.read().await.receipts_for(site_id, holder).to_vec()
    }

    /// The preferred hosts of a site that count right now.
    pub async fn preferred(&self, site_id: &str) -> Vec<String> {
        self.0.map.read().await.effective_preferred(site_id)
    }

    // ── settlement ───────────────────────────────────────────────────────

    /// What this node (as payer) owes preferred holders for `[from, to)`.
    pub async fn dues(&self, from: u64, to: u64) -> Vec<settle::Due> {
        let Some(payer) = self.0.challenger_hex.clone() else {
            return Vec::new();
        };
        let (records, payouts, mut receipts) = {
            let map = self.0.map.read().await;
            let records: Vec<SiteRecord> = map.records.values().cloned().collect();
            let payouts: HashMap<String, Payee> = map
                .peers
                .iter()
                .filter_map(|(p, h)| h.payout.clone().map(|po| (p.clone(), po)))
                .collect();
            let receipts: Vec<Receipt> = map.receipts.values().flatten().cloned().collect();
            (records, payouts, receipts)
        };
        receipts.extend(self.own_receipts().await);
        settle::dues(&records, &receipts, &payouts, &payer, from, to)
    }

    /// One settlement round for `[from, to)`: work out the dues and pay them
    /// through `rail` (skipping anything the ledger already paid, capped at
    /// `cap` micro-USD).
    pub async fn settle_round(
        &self,
        rail: &dyn crate::payments::Rail,
        ledger: &crate::payments::Ledger,
        from: u64,
        to: u64,
        cap: Micros,
    ) -> (Vec<settle::Due>, Vec<(crate::payments::Transfer, crate::payments::Outcome)>) {
        let dues = self.dues(from, to).await;
        let Some(payer) = self.0.challenger_hex.clone() else {
            return (dues, Vec::new());
        };
        let transfers = settle::transfers(&dues, &payer);
        let outcomes = crate::payments::pay_all(rail, ledger, &transfers, cap).await;
        (dues, outcomes)
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

    async fn store_chunks(&self, hashes: &[String], holders: &[String]) -> Result<()> {
        for h in hashes {
            let hash = Hash::from_hex(h).context("bad chunk hash")?;
            if !self.0.store.has(&hash).await {
                let bytes = self.fetch_chunk(&hash, holders).await?;
                self.0.store.put(&bytes).await?;
            }
        }
        Ok(())
    }

    /// One part of a large site's manifest (local, cached, or fetched).
    async fn part(&self, site_id: &str, r: &PartRef) -> Result<Arc<ManifestPart>> {
        if let Some(p) = self.0.parts.read().await.get(&r.hash) {
            return Ok(p.clone());
        }
        let hash = Hash::from_hex(&r.hash).context("part hash")?;
        let holders = self.0.map.read().await.sources(site_id);
        let bytes = self.fetch_chunk(&hash, &holders).await?;
        let p = Arc::new(ManifestPart::from_bytes(&bytes, r)?);
        let mut cache = self.0.parts.write().await;
        if cache.len() >= 64 {
            cache.clear();
        }
        cache.insert(r.hash.clone(), p.clone());
        Ok(p)
    }

    /// Every chunk a site needs, its manifest parts included (not the root).
    async fn chunk_set(&self, site_id: &str, m: &Manifest) -> HashSet<String> {
        match m {
            Manifest::Inline(m) => m.chunk_hashes().into_iter().collect(),
            Manifest::Parted(r) => {
                let mut out = HashSet::new();
                for pr in &r.parts {
                    out.insert(pr.hash.clone());
                    if let Ok(p) = self.part(site_id, pr).await {
                        out.extend(p.chunk_hashes().cloned());
                    }
                }
                out
            }
        }
    }

    /// The entry for an exact manifest path.
    async fn lookup(&self, rec: &SiteRecord, m: &Manifest, path: &str) -> Result<Option<site::FileEntry>> {
        match m {
            Manifest::Inline(m) => Ok(m.files.get(path).cloned()),
            Manifest::Parted(_) => match m.part_for(path) {
                Some(pr) => Ok(self.part(&rec.site_id, pr).await?.files.get(path).cloned()),
                None => Ok(None),
            },
        }
    }

    /// The entry a request path resolves to (`/` → index.html, etc.).
    async fn resolve_entry(&self, rec: &SiteRecord, m: &Manifest, request: &str) -> Result<Option<site::FileEntry>> {
        for c in site::candidates(request) {
            if let Some(e) = self.lookup(rec, m, &c).await? {
                return Ok(Some(e));
            }
        }
        Ok(None)
    }

    async fn manifest(&self, rec: &SiteRecord) -> Result<Arc<Manifest>> {
        if let Some(m) = self.0.manifests.read().await.get(&rec.manifest) {
            return Ok(m.clone());
        }
        let hash = Hash::from_hex(&rec.manifest).context("manifest hash")?;
        let holders = self.0.map.read().await.sources(&rec.site_id);
        let bytes = self.fetch_chunk(&hash, &holders).await?;
        let m = Arc::new(Manifest::from_bytes(&bytes)?);
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
    /// Install the custom-domain handler that runs before site serving.
    pub fn set_host_hook(&self, hook: Arc<dyn HostHook>) -> bool {
        self.0.host_hook.set(hook).is_ok()
    }

    /// A site record: known locally, else asked of connected peers.
    pub async fn lookup_record(&self, site_id: &str) -> Option<SiteRecord> {
        if let Some(r) = self.record_for(site_id).await {
            return Some(r);
        }
        for p in self.0.net.connected_peers().await.into_iter().take(12) {
            let Ok(Ok(body)) =
                tokio::time::timeout(Duration::from_secs(5), self.0.net.rpc(p, RPC_RECORD, site_id.as_bytes().to_vec())).await
            else {
                continue;
            };
            if let Ok(rec) = serde_json::from_slice::<SiteRecord>(&body)
                && rec.site_id == site_id
                && rec.verify()
            {
                self.0.map.write().await.apply_record(rec);
                return self.record_for(site_id).await;
            }
        }
        None
    }

    pub fn set_serve_hook(&self, hook: Arc<dyn ServeHook>) -> bool {
        self.0.hook.set(hook).is_ok()
    }

    /// The file a request path resolves to, without its bytes.
    pub async fn resolve(&self, rec: &SiteRecord, path: &str) -> Result<Option<site::FileEntry>> {
        let manifest = self.manifest(rec).await?;
        self.resolve_entry(rec, &manifest, path).await
    }

    /// Read a whole file of a site (local chunks, else fetched from holders).
    /// `Ok(None)` when the site has no such file.
    pub async fn read_file(&self, rec: &SiteRecord, path: &str) -> Result<Option<(site::FileEntry, Vec<u8>)>> {
        let manifest = self.manifest(rec).await?;
        let Some(entry) = self.lookup(rec, &manifest, path).await? else {
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
        let found = match self.resolve_entry(rec, &manifest, path).await {
            Ok(Some(e)) => Some((e, StatusCode::OK)),
            Ok(None) => match self.lookup(rec, &manifest, "404.html").await {
                Ok(Some(e)) => Some((e, StatusCode::NOT_FOUND)),
                Ok(None) => None,
                Err(e) => {
                    warn!(site = %rec.site_id, err = %e, "hosting: manifest part unavailable");
                    return (StatusCode::BAD_GATEWAY, "site is unavailable right now").into_response();
                }
            },
            Err(e) => {
                warn!(site = %rec.site_id, err = %e, "hosting: manifest part unavailable");
                return (StatusCode::BAD_GATEWAY, "site is unavailable right now").into_response();
            }
        };
        let Some((entry, status)) = found else {
            return (StatusCode::NOT_FOUND, "not found").into_response();
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
            .route("/hosting/v1/sites/{site}/resolve", get(edge_api::resolve))
            .route("/hosting/v1/sites/{site}/chunks/{hash}", get(edge_api::chunk))
            .route("/s/{site}", get(redirect_site_root))
            .route("/s/{site}/", get(serve_root))
            .route("/s/{site}/{*path}", get(serve_path))
            .route("/p/{site}", get(redirect_private_root))
            .route("/p/{site}/", get(private_loader))
            .fallback(serve_by_host)
            .with_state(self.clone())
    }
}

/// The browser decrypt loader for private objects (`c0mpute storage link`,
/// `c0mpute hosting publish --private`). A fixed page: it fetches the
/// object's ciphertext from `/s/<site>/o` and decrypts it in the browser with
/// the key from the URL fragment, which browsers never send to the server.
/// This gateway only ever handles ciphertext.
pub const PRIVATE_LOADER_HTML: &str = include_str!("private_loader.html");

/// The loader's CSP. Decrypted pages render in a sandboxed (opaque-origin)
/// blob: frame that inherits this policy, so they can run their own scripts
/// but cannot reach this gateway's origin or any other host: no fetches out,
/// no external images, no form posts.
pub const PRIVATE_LOADER_CSP: &str = "default-src 'none'; script-src 'unsafe-inline' blob: data:; \
style-src 'unsafe-inline' blob: data:; img-src blob: data:; media-src blob: data:; font-src blob: data:; \
connect-src 'self'; frame-src blob:; child-src blob:; object-src 'none'; base-uri 'none'; \
form-action 'none'; frame-ancestors 'none'";

fn valid_site_id(s: &str) -> bool {
    s.len() == 20 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

async fn private_loader(AxPath(site): AxPath<String>) -> Response {
    if !valid_site_id(&site) {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    let mut resp = (StatusCode::OK, PRIVATE_LOADER_HTML).into_response();
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(PRIVATE_LOADER_CSP));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert("cross-origin-opener-policy", HeaderValue::from_static("same-origin"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=300"));
    h.insert("x-robots-tag", HeaderValue::from_static("noindex, nofollow"));
    resp
}

async fn redirect_private_root(AxPath(site): AxPath<String>) -> Response {
    if !valid_site_id(&site) {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    let mut resp = StatusCode::PERMANENT_REDIRECT.into_response();
    if let Ok(v) = HeaderValue::from_str(&format!("/p/{site}/")) {
        resp.headers_mut().insert(header::LOCATION, v);
    }
    resp
}

/// `/s/<site>` -> `/s/<site>/` (query kept). Serving the page at the slashless
/// path made every relative asset (`img/a.png`, `og.png`) resolve to `/s/img/...`.
async fn redirect_site_root(AxPath(site): AxPath<String>, uri: axum::http::Uri) -> Response {
    site_root_redirect(&site, uri.query())
}

fn site_root_redirect(site: &str, query: Option<&str>) -> Response {
    let target = match query {
        Some(q) if !q.is_empty() => format!("/s/{site}/?{q}"),
        _ => format!("/s/{site}/"),
    };
    match HeaderValue::from_str(&target) {
        Ok(location) => {
            let mut resp = StatusCode::PERMANENT_REDIRECT.into_response();
            resp.headers_mut().insert(header::LOCATION, location);
            resp
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

const GIB_BYTES: u64 = 1024 * 1024 * 1024;

/// Free space a hosting copy must leave on the disk: 10 GiB or 5% of it,
/// whichever is more, capped at 50 GiB. Every node hosts by default, including
/// busy production boxes, so the 5 GiB hosting budget alone could fill a disk
/// that was already nearly full.
/// Known-but-not-held records under target re-gossiped per records round.
const SHORT_RECORDS_PER_ROUND: usize = 50;
/// Records kept in records.json across restarts.
const MAX_SAVED_RECORDS: usize = 20_000;

/// Room a node must have beyond its floor/budget to call itself not full.
const FULL_HEADROOM_BYTES: u64 = 256 * 1024 * 1024;

fn free_disk_floor(disk_total: u64) -> u64 {
    (disk_total / 20).clamp(10 * GIB_BYTES, 50 * GIB_BYTES)
}

fn leaves_free_floor(disk_free: u64, disk_total: u64, extra: u64) -> bool {
    // An unknown disk (0/0) is not refused: the budget check still applies.
    disk_total == 0 || disk_free.saturating_sub(extra) >= free_disk_floor(disk_total)
}

pub(crate) fn jitter(d: Duration) -> Duration {
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
                "preferred_hosts": r.preferred_hosts,
                "price_per_gib_month": r.price_per_gib_month,
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
    // `/s/<id>` without the slash would resolve every relative URL on the
    // page (images, CSS, og:image) one level too high: send it to `/s/<id>/`.
    if !uri.path().ends_with('/') {
        let to = match uri.query() {
            Some(q) => format!("{}/?{q}", uri.path()),
            None => format!("{}/", uri.path()),
        };
        return axum::response::Redirect::permanent(&to).into_response();
    }
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
        Some(rec) => {
            let resp = h.serve_with_hook(&rec, path, query, headers).await;
            let page = meta::public_origin(headers).map(|o| format!("{o}/s/{site}/{path}"));
            with_absolute_meta(resp, page.as_deref()).await
        }
        None => (StatusCode::NOT_FOUND, "no such site on the network (yet)").into_response(),
    }
}

/// Largest HTML page whose social meta tags get rewritten.
const MAX_META_REWRITE_BYTES: usize = 8 * 1024 * 1024;

/// Rewrite relative `og:image` / `twitter:image` / `og:url` tags of an HTML
/// response to absolute URLs (see [`meta`]); anything else passes through.
async fn with_absolute_meta(resp: Response, page_url: Option<&str>) -> Response {
    let Some(page) = page_url.and_then(|u| url::Url::parse(u).ok()) else {
        return resp;
    };
    let html = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.to_ascii_lowercase().starts_with("text/html"));
    if !html {
        return resp;
    }
    let (mut parts, body) = resp.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_META_REWRITE_BYTES).await else {
        return (StatusCode::BAD_GATEWAY, "site is unavailable right now").into_response();
    };
    let rewritten = std::str::from_utf8(&bytes).ok().and_then(|text| meta::absolutize(text, &page));
    match rewritten {
        Some(text) => {
            parts.headers.remove(header::CONTENT_LENGTH);
            // The body now depends on the host it was served from.
            parts.headers.insert(header::VARY, HeaderValue::from_static("Host, X-Forwarded-Host"));
            Response::from_parts(parts, Body::from(text))
        }
        None => Response::from_parts(parts, Body::from(bytes)),
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
async fn serve_by_host(State(h): State<Hosting>, req: axum::extract::Request) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(':').next().unwrap_or("").trim_end_matches('.').to_ascii_lowercase())
        .unwrap_or_default();
    let req = match h.0.host_hook.get() {
        Some(hook) if !host.is_empty() && site::valid_domain(&host) => match hook.serve_host(&host, req).await {
            Ok(resp) => return resp,
            Err(req) => req,
        },
        _ => req,
    };
    let headers = req.headers().clone();
    let uri = req.uri().clone();
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
            let resp = h.serve_with_hook(&rec, uri.path(), uri.query(), &headers).await;
            let page = meta::public_origin(&headers).map(|o| format!("{o}{}", uri.path()));
            return with_absolute_meta(resp, page.as_deref()).await;
        }
    }
    (StatusCode::NOT_FOUND, "not found").into_response()
}

#[cfg(test)]
mod tests {
    #[test]
    fn hosting_copy_must_leave_a_free_disk_floor() {
        let g = GIB_BYTES;
        // 1.8 TiB disk: floor is capped at 50 GiB.
        assert_eq!(free_disk_floor(1843 * g), 50 * g);
        // 100 GiB disk: floor is the 10 GiB minimum.
        assert_eq!(free_disk_floor(100 * g), 10 * g);
        // dev2 on 2026-11-09: 20 GiB free on 1.8 TiB, refuse.
        assert!(!leaves_free_floor(20 * g, 1843 * g, g));
        // Plenty free: accept.
        assert!(leaves_free_floor(500 * g, 1843 * g, 5 * g));
        // Exactly at the floor after the copy: accept; one byte under: refuse.
        assert!(leaves_free_floor(11 * g, 100 * g, g));
        assert!(!leaves_free_floor(11 * g, 100 * g, g + 1));
        // Unknown disk: the hosting budget still guards it.
        assert!(leaves_free_floor(0, 0, g));
    }

    #[test]
    fn slashless_site_path_redirects_with_query() {
        let r = site_root_redirect("53021d6e8d5a0e1aac48", None);
        assert_eq!(r.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(r.headers()[header::LOCATION], "/s/53021d6e8d5a0e1aac48/");
        let r = site_root_redirect("53021d6e8d5a0e1aac48", Some("ads=1"));
        assert_eq!(r.headers()[header::LOCATION], "/s/53021d6e8d5a0e1aac48/?ads=1");
        let r = site_root_redirect("bad\r\nset-cookie: x", None);
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }

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
            price: Some("0.004".into()),
            payout: None,
            full: false,
            room_gib: Some(3000),
        };
        let bytes = serde_json::to_vec(&m).unwrap();
        assert!(bytes.len() < 1024);
        let back: HostingMsg = serde_json::from_slice(&bytes).unwrap();
        assert!(matches!(back, HostingMsg::Holding { .. }));
        // A holding message from an older node (no price/payout) still parses.
        let old = br#"{"t":"holding","sites":[],"part":0,"parts":1,"hosts":true,"addrs":[]}"#;
        assert!(matches!(
            serde_json::from_slice::<HostingMsg>(old).unwrap(),
            HostingMsg::Holding { price: None, payout: None, .. }
        ));
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
            price: Some("0.004".into()),
            payout: Some(Payee {
                wallet: "0x".to_string() + &"a".repeat(40),
                coin: "USDC_POL".into(),
            }),
            full: true,
            room_gib: Some(u64::MAX),
        })
        .unwrap();
        assert!(bytes.len() < 64 * 1024, "{} bytes", bytes.len());
    }
}

struct RecordRpc(Hosting);

#[async_trait::async_trait]
impl c0mpute_net::RpcHandler for RecordRpc {
    async fn handle(&self, _peer: c0mpute_net::PeerId, body: Vec<u8>) -> std::result::Result<Vec<u8>, String> {
        let id = String::from_utf8(body).map_err(|_| "bad id".to_string())?;
        let rec = self.0.record_for(id.trim()).await.ok_or("unknown site")?;
        serde_json::to_vec(&rec).map_err(|e| e.to_string())
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
    publish_dir_with(dir, name, domains, wait, None).await
}

/// Marketplace options for [`publish_dir_with`].
#[derive(Clone, Debug)]
pub struct MarketOptions {
    pub query: market::DiskQuery,
    /// Preferred (paid) hosts to pick, 1-5.
    pub copies: usize,
    /// The OpenDisk marketplace to ask (discovery only).
    pub d1sks_url: String,
}

/// How long a publisher listens to the hosting gossip to see which d1sks
/// disks are live hosts (a hosting node announces once a minute).
pub const GOSSIP_CHECK: Duration = Duration::from_secs(70);

/// How long a publisher keeps retrying an unreachable d1sks before it
/// publishes without preferred hosts.
pub const D1SKS_RETRY: Duration = Duration::from_secs(30);

/// Longest gap between a publisher's re-announcements while it waits for
/// copies (the Fibonacci schedule grows to this and stays there).
pub const REANNOUNCE_CAP: Duration = Duration::from_secs(60);

/// How often a publisher checks the replica map while it waits.
const COPY_POLL: Duration = Duration::from_secs(2);

/// The d1sks hosting disks for `m`, checked against the live hosting gossip
/// when `hosting` is given: disks that announced themselves are marked live
/// and rank first. `Err` when d1sks cannot be read.
pub async fn market_disks(m: &MarketOptions, hosting: Option<&Hosting>) -> Result<Vec<market::HostDisk>> {
    // d1sks can blip (a deploy, a gateway restart): retry with Fibonacci
    // backoff for a short while before giving up on preferred hosts.
    let mut disks = crate::backoff::retry_until(
        Instant::now() + D1SKS_RETRY,
        crate::backoff::Fibonacci::new(Duration::from_secs(8)),
        |e: &anyhow::Error, next| match next {
            Some(d) => eprintln!("d1sks: {e:#}; retrying in {}s", d.as_secs()),
            None => debug!(err = %e, "d1sks: giving up"),
        },
        || market::query(&m.d1sks_url, &m.query),
    )
    .await?;
    if let Some(h) = hosting {
        let want: Vec<String> = disks.iter().map(|d| d.peer_id.clone()).collect();
        let live = h.wait_for_hosts(&want, GOSSIP_CHECK).await;
        market::mark_live(&mut disks, &live);
    }
    Ok(disks)
}

/// Pick preferred hosts from d1sks for a site of `size` bytes, preferring
/// disks seen live on the hosting gossip. `None` (with a note on stderr)
/// when d1sks is unreachable or nothing matches: the site then publishes as
/// a plain free site, which the network still keeps.
pub async fn pick_terms(m: &MarketOptions, size: u64, hosting: Option<&Hosting>) -> Option<site::HostingTerms> {
    let disks = match market_disks(m, hosting).await {
        Ok(d) => d,
        Err(e) => {
            eprintln!("d1sks: {e}; publishing without preferred hosts (the network still keeps 3 free copies)");
            return None;
        }
    };
    let ranked = market::rank(&disks, &m.query, size);
    match market::choose(&ranked, m.copies, None) {
        Some((chosen, terms)) => {
            eprintln!(
                "marketplace: {} preferred host(s) at ${}/GiB-month:",
                chosen.len(),
                crate::payments::format_usd(terms.price_per_gib_month)
            );
            for d in &chosen {
                eprintln!(
                    "  {}  {}  ${}/GiB-month  {}  {}",
                    d.peer_id,
                    d.name,
                    crate::payments::format_usd(d.price),
                    if d.live { "live" } else { d.reachable.as_deref().unwrap_or("listed") },
                    d.countries.join(",")
                );
            }
            Some(terms)
        }
        None => {
            eprintln!(
                "d1sks: no c0mpute hosting disk matches ({} listed); publishing without preferred hosts",
                disks.len()
            );
            None
        }
    }
}

/// [`publish_dir`], optionally picking paid hosts from the d1sks marketplace
/// first and signing them into a v2 record.
pub async fn publish_dir_with(
    dir: &std::path::Path,
    name: &str,
    domains: Vec<String>,
    wait: Duration,
    market: Option<MarketOptions>,
) -> Result<(SiteRecord, usize)> {
    for d in &domains {
        if !site::valid_domain(d) {
            bail!("invalid domain {d:?}");
        }
    }
    let built = site::build_from_dir(dir, name)?;
    let supervisor = crate::Supervisor::boot_buyer(crate::Config::default()).await?;
    let keypair = publisher_keypair()?;
    let manifest_hash = built.manifest_hash();
    let seq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    // Join the hosting gossip first: picking marketplace hosts checks the
    // d1sks listing against which hosting nodes are live right now.
    let hosting = Hosting::start(supervisor.libp2p.clone(), supervisor.store.clone(), Options::default()).await?;
    let terms = match &market {
        Some(m) => pick_terms(m, built.manifest.total_bytes(), Some(&hosting)).await,
        None => None,
    };
    let record = SiteRecord::sign_with_terms(
        &keypair,
        name,
        &manifest_hash,
        seq,
        built.manifest.total_bytes(),
        domains,
        terms,
    )?;
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
    // Wait for copies, healing as we go: re-announce on a Fibonacci schedule
    // (1, 1, 2, 3, 5, ... s, capped at REANNOUNCE_CAP) that starts over
    // whenever a new copy shows up, so a stalled publish keeps knocking
    // without hammering the mesh.
    let started = Instant::now();
    let mut copies = 0;
    let mut schedule = crate::backoff::Fibonacci::new(REANNOUNCE_CAP);
    let mut next_announce = started + schedule.next_delay();
    while started.elapsed() < wait {
        let left = wait.saturating_sub(started.elapsed());
        tokio::time::sleep(COPY_POLL.min(left)).await;
        let holders = hosting.holders(&record.site_id).await;
        let others = holders.len();
        if others != copies {
            if others > copies {
                schedule.reset();
                next_announce = Instant::now() + schedule.next_delay();
            }
            copies = others;
            let paid = holders.iter().filter(|h| record.prefers(h)).count();
            if record.preferred_hosts.is_empty() {
                eprintln!("  {copies}/{TARGET_REPLICAS} network copies");
            } else {
                eprintln!(
                    "  {copies}/{TARGET_REPLICAS} network copies ({paid}/{} preferred)",
                    record.preferred_hosts.len()
                );
            }
        }
        let paid_done = record.preferred_hosts.is_empty()
            || holders.iter().filter(|h| record.prefers(h)).count() >= record.preferred_hosts.len();
        // A marketplace site waits a little longer for its preferred hosts,
        // but never past `wait`.
        if copies >= TARGET_REPLICAS && (paid_done || started.elapsed() > wait / 2) {
            break;
        }
        if Instant::now() >= next_announce {
            hosting.reannounce(&record).await;
            next_announce = Instant::now() + schedule.next_delay();
        }
    }
    Ok((record, copies))
}
