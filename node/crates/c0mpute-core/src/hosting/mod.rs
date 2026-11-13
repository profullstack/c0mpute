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
pub mod fetch;
pub mod git;
mod git_browse;
pub mod market;
pub mod meta;
pub mod origin;
pub mod proof;
pub mod relay;
pub mod replicas;
pub mod seed;
pub mod settle;
pub mod site;
pub mod stripe;

use std::collections::{BTreeSet, HashMap, HashSet};
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
use site::{Manifest, ManifestPart, PartRef, SiteManifest, SiteRecord};

pub const TOPIC: &str = "c0mpute/hosting/v1";
pub const ROLE_TAG: &str = "c0mpute:role:hosting";
/// Copies the network keeps of every site.
pub const TARGET_REPLICAS: usize = 3;
/// The hard ceiling: above this many copies the lowest-ranked free holder
/// lets go at once. Between the target and this, free copies are trimmed back
/// to the target only after a grace period (see [`replicas::releases`]).
pub const MAX_REPLICAS: usize = 5;
/// `[hosting] max_bytes = 0` (the default): the budget follows the disk, see
/// [`auto_budget`].
pub const DEFAULT_MAX_BYTES: u64 = 0;
/// Share of the free space above the free-disk floor an automatic budget may
/// use, in percent. Half: a node listed on d1sks offers its disk, and at a
/// quarter the network could not place three copies of a 600 GiB collection
/// with 10 TB free across its disks (11-11). The free-disk floor still
/// guards the rest.
pub const AUTO_BUDGET_PERCENT: u64 = 50;
/// Most bytes of FREE sites a node holds for any one owner key, so one
/// publisher cannot fill every volunteer's disk. Sites a node is a preferred
/// (paid) host for are not counted: it was picked, and is paid, for them.
pub const MAX_BYTES_PER_OWNER: u64 = 16 * GIB_BYTES;
/// Sites per holding message, so one message stays well under gossipsub's
/// 64 KiB limit.
const SITES_PER_MESSAGE: usize = 200;
/// Other holders asked for a chunk before this node lets a surplus copy go.
const DROP_PROBES: usize = 12;
/// How long a passed challenge keeps a holder counted toward the copies a
/// site needs, in holder TTLs: 108 x 200 s = 6 h. Long enough that the nodes
/// next in line for a site re-ask each holder a few times a day, not every
/// repair round; short enough that a holder that deleted its copy and kept
/// announcing stops counting the same day.
const PROOF_TTL_HOLDER_TTLS: u32 = 108;
/// Background challenges a repair round may start: in all, of one peer, and
/// for one site. A node that just started knows no proofs and may be next in
/// line for thousands of sites; it works through them at this rate, and until
/// a site's turn comes its announced holders are taken at their word.
const PROOF_ASKS_PER_ROUND: usize = 32;
const PROOF_ASKS_PER_PEER: usize = 4;
const PROOF_ASKS_PER_SITE: usize = MAX_REPLICAS;
/// Background challenges in flight at once.
const PROOF_SLOTS: usize = 8;
/// Before this node gives up a copy because other holders have the site, it
/// must have seen at least [`TARGET_REPLICAS`] of them pass its challenges at
/// this many separate times, over at least this long. Filling asks for one
/// recent pass only, so a new site still settles in minutes; letting go of
/// data is the step that cannot be undone. Keys that only announce and relay
/// then have to stay up and relay for hours before any honest holder trims,
/// and since a relay through the challenger itself is seen (see
/// [`Hosting::holds_now`]), the last honest holder is never talked into
/// dropping by peers that hold nothing.
pub const DROP_STANDING_PASSES: u32 = 3;
pub const DROP_STANDING_SPAN: Duration = Duration::from_secs(6 * 3600);
/// Unfinished multi-part holding lists kept at once, and for how long. Each
/// is a peer's word, so neither the number nor the size is open-ended.
const MAX_PARTIAL_HOLDINGS: usize = 1024;
const PARTIAL_HOLDING_TTL: Duration = Duration::from_secs(120);

/// Whether part `part` of a `parts`-part holding list may be added to one
/// that has `got` parts so far.
fn holding_part_fits(part: u32, parts: u32, got: u32) -> bool {
    part < parts && got < parts
}
/// Bytes of site entries per holding message (group bitmaps make entries of
/// striped sites larger than the rest).
const HOLDING_MESSAGE_BYTES: usize = 40 * 1024;
/// A group short of copies for this long brings in one more candidate per
/// period, so a chosen node that cannot take it does not stall the group.
const WIDEN_AFTER: Duration = Duration::from_secs(180);
/// The group number [`Hosting::trim_due`] keys a whole (unstriped) site on.
const WHOLE: u32 = u32::MAX;
/// Most hosting peers a node keeps itself connected to (see
/// [`Hosting::connect_hosts`]).
const CONNECT_HOSTS_MAX: usize = 24;

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
        /// Bytes it can still take for the network. Sent only by nodes that
        /// place striped sites, which is how others know to count on them for
        /// a group. Absent from older nodes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        room: Option<u64>,
        /// An opaque id of the machine and disk it stores on (see
        /// [`failure_domain`]): two nodes with the same id are one copy, even
        /// when neither has a public address to compare. Absent from older
        /// nodes (they are told apart by address only).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        machine: Option<String>,
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
    /// Striped site held in part: bitmap of the groups held
    /// ([`stripe::encode_groups`]). Absent = the whole site.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub groups: Option<String>,
}

/// Sites this node holds, persisted next to the chunk store.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Catalog {
    sites: HashMap<String, SiteRecord>,
    /// Striped sites: the groups of `sites[id]`'s manifest held here. A site
    /// in `sites` without an entry is held whole.
    #[serde(default)]
    stripes: HashMap<String, BTreeSet<u32>>,
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
    /// Bytes of copies being fetched right now: admitted by [`Hosting::check_room`]
    /// but not in the catalog yet, so the next admission must count them.
    reserving: std::sync::atomic::AtomicU64,
    /// Serializes admission (check, then reserve).
    admit: tokio::sync::Mutex<()>,
    /// Per-peer speed and load for multi-source fetching.
    stats: fetch::Stats,
    /// Group layouts of striped manifests, by manifest hash.
    layouts: RwLock<HashMap<String, Arc<stripe::Layout>>>,
    /// When each short group was first seen short, for widening.
    short_since: std::sync::Mutex<HashMap<(String, u32), Instant>>,
    /// When each site ([`WHOLE`]) or group was first seen over the target,
    /// for trimming (see [`Hosting::trim_due`]).
    over_since: std::sync::Mutex<HashMap<(String, u32), Instant>>,
    /// When this node first asked the announced holders of a site to prove
    /// their copies because it would otherwise take one itself (see
    /// [`Hosting::fill_due`]).
    unproven_since: std::sync::Mutex<HashMap<String, Instant>>,
    /// Background challenges in flight, as (site id, holder).
    proving: std::sync::Mutex<HashSet<(String, String)>>,
    prove_slots: Semaphore,
    /// Which held sites use a chunk (see [`relay`]). Filled only once
    /// [`Hosting::refuses_fetch`] is in use.
    chunk_index: std::sync::Mutex<relay::ChunkIndex>,
    guards_fetches: std::sync::atomic::AtomicBool,
    dns: dns::Verifier,
    /// `[gateway] site_host_suffix`, normalized: `<site_id>.<suffix>` is
    /// that site's own origin (see [`origin`]).
    site_host_suffix: Option<String>,
    /// `[gateway] sandbox_shared_sites`: pages under `/s/` are sandboxed.
    sandbox_shared: bool,
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
    disk_override: Option<(u64, u64)>,
    /// This node's failure domain, worked out once (see [`failure_domain`]).
    machine: std::sync::OnceLock<Option<String>>,
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
    /// `[hosting] require_proofs`: count a holder toward a site's copies only
    /// after this node saw it pass a challenge, and rely on it for a drop
    /// only once it has standing. Off by default: announcements count, as
    /// before 0.3.65, until the stricter rule has been run on real networks
    /// (while a small site is still being copied its chunks are read
    /// constantly, challenges are voided, and every host takes a copy).
    pub require_proofs: bool,
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
    /// Pretend the disk has (free, total) bytes instead of asking the OS.
    /// For tests on machines whose disk is below the free-disk floor.
    pub disk_override: Option<(u64, u64)>,
    /// `[gateway] site_host_suffix`: serve each site on its own host under
    /// this suffix. `None`: as this machine is configured
    /// ([`origin::configured`]); `Some("")`: off whatever is configured.
    pub site_host_suffix: Option<String>,
    /// `[gateway] sandbox_shared_sites`. `None`: as this machine is
    /// configured (on unless turned off).
    pub sandbox_shared_sites: Option<bool>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            hosts: false,
            max_bytes: DEFAULT_MAX_BYTES,
            catalog_path: None,
            timing: Timing::default(),
            dns: dns::Verifier::default(),
            require_proofs: false,
            challenger: None,
            challenge: ChallengeScope::Own,
            challenge_every: Duration::from_secs(600),
            price: None,
            payout: None,
            receipts_path: None,
            disk_override: None,
            site_host_suffix: None,
            sandbox_shared_sites: None,
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
        // Read the machine's configuration only for what the caller left open.
        let (site_host_suffix, sandbox_shared) = match (opts.site_host_suffix, opts.sandbox_shared_sites) {
            (Some(suffix), Some(sandbox)) => origin::resolve(Some(suffix), sandbox, None, None),
            (suffix, sandbox) => {
                let (cfg_suffix, cfg_sandbox) = origin::configured();
                (
                    match suffix {
                        Some(s) => origin::resolve(Some(s), true, None, None).0,
                        None => cfg_suffix,
                    },
                    sandbox.unwrap_or(cfg_sandbox),
                )
            }
        };
        if let Some(suffix) = &site_host_suffix {
            // The gateway's listener keeps every other route off those hosts.
            c0mpute_gateway::site_host::configure(suffix);
            info!(%suffix, "hosting: each site is served on its own host, <site_id>.<suffix>");
        }
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
            map: RwLock::new(ReplicaMap {
                count_claims: !(opts.require_proofs || std::env::var("C0MPUTE_HOSTING_REQUIRE_PROOFS").is_ok_and(|v| v == "1")),
                ..Default::default()
            }),
            catalog: RwLock::new(Catalog::default()),
            catalog_path: opts.catalog_path,
            records_path,
            records_dirty: std::sync::atomic::AtomicBool::new(false),
            manifests: RwLock::new(HashMap::new()),
            parts: RwLock::new(HashMap::new()),
            replicating: RwLock::new(HashSet::new()),
            turn: AtomicUsize::new(0),
            stopped: std::sync::atomic::AtomicBool::new(false),
            fetch_slots: Semaphore::new(128),
            reserving: std::sync::atomic::AtomicU64::new(0),
            admit: tokio::sync::Mutex::new(()),
            stats: fetch::Stats::default(),
            layouts: RwLock::new(HashMap::new()),
            short_since: std::sync::Mutex::new(HashMap::new()),
            over_since: std::sync::Mutex::new(HashMap::new()),
            unproven_since: std::sync::Mutex::new(HashMap::new()),
            proving: std::sync::Mutex::new(HashSet::new()),
            prove_slots: Semaphore::new(PROOF_SLOTS),
            chunk_index: std::sync::Mutex::new(relay::ChunkIndex::default()),
            guards_fetches: std::sync::atomic::AtomicBool::new(false),
            dns: opts.dns,
            site_host_suffix,
            sandbox_shared,
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
            disk_override: opts.disk_override,
            machine: std::sync::OnceLock::new(),
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
            match catalog.stripes.get(&id) {
                Some(groups) => {
                    let kept = h.complete_groups(&rec, groups).await;
                    if kept.is_empty() {
                        continue;
                    }
                    let mut cat = h.0.catalog.write().await;
                    cat.sites.insert(id.clone(), rec.clone());
                    cat.stripes.insert(id, kept);
                }
                None => {
                    if !h.has_complete_copy(&rec).await {
                        continue;
                    }
                    h.0.catalog.write().await.sites.insert(id, rec.clone());
                }
            }
            h.0.map.write().await.apply_record(rec);
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
            type Part = (u32, HashMap<String, (String, u64)>, HashMap<String, BTreeSet<u32>>);
            let mut partial: HashMap<String, Part> = HashMap::new();
            // When each unfinished list began: one that never completes goes.
            let mut partial_at: HashMap<String, Instant> = HashMap::new();
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
                        room,
                        machine,
                    } => {
                        if let Some(pid) = msg.source {
                            for a in addrs.iter().take(8) {
                                if let Ok(ma) = a.parse::<c0mpute_net::Multiaddr>() {
                                    h.0.net.add_peer_address(pid, ma).await;
                                }
                            }
                        }
                        let parts = parts.clamp(1, 1000);
                        partial_at.retain(|_, at| at.elapsed() < PARTIAL_HOLDING_TTL);
                        partial.retain(|s, _| partial_at.contains_key(s));
                        let got = if part == 0 { 0 } else { partial.get(&source).map(|e| e.0).unwrap_or(0) };
                        // A part past the count it states, more parts than it
                        // states, or one more unfinished list than we keep:
                        // the list so far is dropped, not grown.
                        if !holding_part_fits(part, parts, got)
                            || (!partial.contains_key(&source) && partial.len() >= MAX_PARTIAL_HOLDINGS)
                        {
                            partial.remove(&source);
                            partial_at.remove(&source);
                            continue;
                        }
                        let entry = partial
                            .entry(source.clone())
                            .or_insert((0, HashMap::new(), HashMap::new()));
                        if part == 0 {
                            *entry = (0, HashMap::new(), HashMap::new());
                            partial_at.insert(source.clone(), Instant::now());
                        }
                        partial_at.entry(source.clone()).or_insert_with(Instant::now);
                        for s in sites.into_iter().take(SITES_PER_MESSAGE) {
                            if let Some(g) = &s.groups {
                                // An unreadable bitmap: skip the entry rather
                                // than count the peer as holding everything.
                                let Some(gs) = stripe::decode_groups(g) else {
                                    continue;
                                };
                                entry.2.insert(s.site_id.clone(), gs);
                            }
                            entry.1.insert(s.site_id, (s.manifest, s.seq));
                        }
                        entry.0 += 1;
                        if part + 1 >= parts {
                            let (_, all, groups) = partial.remove(&source).unwrap_or_default();
                            partial_at.remove(&source);
                            let price = price.as_deref().and_then(|p| crate::payments::parse_usd(p).ok());
                            let payout = payout.filter(|p| p.valid());
                            let addrs: Vec<String> = addrs.into_iter().take(8).collect();
                            let mut map = h.0.map.write().await;
                            map.apply_holding_full(
                                &source,
                                all,
                                groups,
                                Instant::now(),
                                hosts,
                                price,
                                payout,
                                room,
                                addrs,
                            );
                            map.set_full(&source, full);
                            map.set_room(&source, room_gib);
                            map.set_machine(&source, machine.filter(|m| m.len() <= 64));
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
        let (held, mine, my_groups): (
            Vec<Held>,
            HashMap<String, (String, u64)>,
            HashMap<String, BTreeSet<u32>>,
        ) = {
            let cat = self.0.catalog.read().await;
            let held = cat
                .sites
                .values()
                .map(|r| Held {
                    site_id: r.site_id.clone(),
                    manifest: r.manifest.clone(),
                    seq: r.seq,
                    groups: cat.stripes.get(&r.site_id).map(stripe::encode_groups),
                })
                .collect();
            let mine = cat
                .sites
                .values()
                .map(|r| (r.site_id.clone(), (r.manifest.clone(), r.seq)))
                .collect();
            (held, mine, cat.stripes.clone())
        };
        let (price, payout) = if self.0.hosts {
            (self.0.price, self.0.payout.clone())
        } else {
            (None, None)
        };
        let room = if self.0.hosts {
            Some(self.room().await)
        } else {
            None
        };
        let full = room.is_some_and(|r| r < FULL_HEADROOM_BYTES);
        let room_gib = room.map(|r| r / GIB_BYTES);
        if !self.0.hosts && mine.is_empty() {
            return;
        }
        let addrs: Vec<String> = self
            .0
            .net
            .advertised_addrs()
            .await
            .into_iter()
            .filter(|a| {
                !a.to_string().contains("/ip4/0.0.0.0") && !a.to_string().contains("/ip6/::/")
            })
            .take(8)
            .map(|a| a.to_string())
            .collect();
        self.0.map.write().await.apply_holding_full(
            &self.0.me,
            mine,
            my_groups,
            Instant::now(),
            self.0.hosts,
            price,
            payout.clone(),
            room,
            addrs.clone(),
        );
        // A pretend disk (tests running many nodes in one process) is no
        // failure domain: those nodes are told apart by address.
        let machine = self.machine().await;
        {
            let mut map = self.0.map.write().await;
            map.set_full(&self.0.me, full);
            map.set_room(&self.0.me, room_gib);
            map.set_machine(&self.0.me, machine.clone());
        }
        // Split by count and by size: group bitmaps make striped entries big.
        let mut chunks: Vec<Vec<Held>> = vec![Vec::new()];
        let mut bytes = 0usize;
        for h in held {
            let len = 120 + h.groups.as_ref().map(|g| g.len()).unwrap_or(0);
            let cur = chunks.last_mut().expect("one chunk");
            if !cur.is_empty()
                && (cur.len() >= SITES_PER_MESSAGE || bytes + len > HOLDING_MESSAGE_BYTES)
            {
                chunks.push(Vec::new());
                bytes = 0;
            }
            chunks.last_mut().expect("one chunk").push(h);
            bytes += len;
        }
        let parts = chunks.len() as u32;
        for (i, c) in chunks.into_iter().enumerate() {
            self.publish(&HostingMsg::Holding {
                sites: c,
                part: i as u32,
                parts,
                hosts: self.0.hosts,
                addrs: addrs.clone(),
                price: price.map(crate::payments::format_usd),
                payout: payout.clone(),
                full,
                room_gib,
                room,
                machine: machine.clone(),
            })
            .await;
        }
    }

    /// This node's failure domain, for hosting nodes only (a publisher or a
    /// gateway holds no copies). Worked out once, off the async threads: the
    /// disk lookup lists every mount. A pretend disk (tests running many
    /// nodes in one process) is no failure domain: those nodes are told
    /// apart by address.
    async fn machine(&self) -> Option<String> {
        if !self.0.hosts || self.0.disk_override.is_some() {
            return None;
        }
        if let Some(m) = self.0.machine.get() {
            return m.clone();
        }
        let root = self.0.store.root().to_path_buf();
        let m = tokio::task::spawn_blocking(move || {
            let total = crate::opendisk::disk_for(&root).1;
            failure_domain(&root, total)
        })
        .await
        .unwrap_or(None);
        self.0.machine.get_or_init(|| m).clone()
    }

    /// Bytes this node holds for the network (striped sites: the groups held).
    async fn held_bytes(&self) -> u64 {
        let (recs, stripes): (Vec<SiteRecord>, HashMap<String, BTreeSet<u32>>) = {
            let cat = self.0.catalog.read().await;
            (cat.sites.values().cloned().collect(), cat.stripes.clone())
        };
        let mut total = 0;
        for r in recs {
            total += match stripes.get(&r.site_id) {
                Some(gs) => match self.layout(&r).await {
                    Ok(l) => l.bytes_of(gs),
                    Err(_) => {
                        gs.len() as u64 * (stripe::GROUP_CHUNKS * site::FILE_CHUNK_BYTES) as u64
                    }
                },
                None => r.size,
            };
        }
        total
    }

    /// (free, total) bytes of the disk the chunk store is on.
    fn disk(&self) -> (u64, u64) {
        self.0
            .disk_override
            .unwrap_or_else(|| crate::opendisk::disk_for(self.0.store.root()))
    }

    /// The most this node holds for the network right now: `[hosting]
    /// max_bytes` when set, else [`auto_budget`] of the disk.
    pub async fn budget(&self) -> u64 {
        let held = self.held_bytes().await;
        self.budget_with(held)
    }

    fn budget_with(&self, held: u64) -> u64 {
        if self.0.max_bytes > 0 {
            return self.0.max_bytes;
        }
        let (free, total) = self.disk();
        auto_budget(held, free, total)
    }

    /// Bytes this node can still take: within its budget, and leaving the
    /// free-disk floor.
    pub async fn room(&self) -> u64 {
        let held = self.held_bytes().await;
        let budget = self.budget_with(held);
        let (free, total) = self.disk();
        let by_disk = if total == 0 {
            u64::MAX
        } else {
            free.saturating_sub(free_disk_floor(total))
        };
        budget.saturating_sub(held).min(by_disk)
    }

    /// Keep a connection open to the hosting peers, and the publishers seeding
    /// a site, that announced a public address. c0mpute has no relay: a node
    /// behind NAT cannot be dialled, so other nodes could never fetch the
    /// copies it holds, and it can only fetch from a source it dials itself.
    /// Dialling out gives every such peer a connection both sides can fetch
    /// over (gossipsub keeps it open).
    ///
    /// Publishers announce `hosts: false`, and they used to be skipped here,
    /// so a NAT'd host replicating a site reached its only source by a
    /// peer-id dial that works only when the DHT already holds the
    /// publisher's address. When it did not, every chunk request fell back to
    /// slow DHT lookups: a 582 GiB site crawled at ~0 MB/s to NAT'd hosts
    /// with 1.6 TiB free each. So seeding publishers come first, and every
    /// target is dialled at the addresses it announced.
    pub async fn connect_hosts(&self) {
        let connected: HashSet<String> = self
            .0
            .net
            .connected_peers()
            .await
            .iter()
            .map(|p| p.to_base58())
            .collect();
        let targets: Vec<(String, Vec<String>)> = {
            let map = self.0.map.read().await;
            let mut v: Vec<(bool, String, Vec<String>)> = map
                .peers
                .iter()
                .filter(|(p, h)| (h.hosts || !h.sites.is_empty()) && **p != self.0.me && !connected.contains(*p))
                .filter_map(|(p, h)| {
                    let addrs: Vec<String> = h.addrs.iter().filter(|a| public_tcp_addr(a)).cloned().collect();
                    (!addrs.is_empty()).then(|| (h.hosts, p.clone(), addrs))
                })
                .collect();
            // Seeding publishers (hosts == false) first: a site being
            // replicated may have no other source yet.
            v.sort();
            v.into_iter().map(|(_, p, a)| (p, a)).collect()
        };
        let room = CONNECT_HOSTS_MAX.saturating_sub(connected.len().min(CONNECT_HOSTS_MAX));
        for (p, addrs) in targets.into_iter().take(room.max(4)) {
            let Ok(pid) = p.parse::<c0mpute_net::PeerId>() else {
                continue;
            };
            let mut dialled = false;
            for a in addrs.iter().take(4) {
                let Some(ma) = dial_addr(a, pid) else {
                    continue;
                };
                self.0.net.add_peer_address(pid, ma.clone()).await;
                if self.0.net.dial(ma).await.is_ok() {
                    dialled = true;
                }
            }
            if !dialled {
                let _ = self.0.net.connect(pid).await;
            }
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
                h.refresh_chunk_index().await;
                h.connect_hosts().await;
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
        let mut asks = Asks::new(self.0.proving.lock().expect("proving").clone());
        let (whole, striped): (Vec<(SiteRecord, Action)>, Vec<SiteRecord>) = {
            let mut map = self.0.map.write().await;
            let now = Instant::now();
            let now_secs = crate::payments::now_secs();
            let ttl = self.proof_ttl();
            let gap = self.standing_gap();
            map.prune(now, self.0.timing.holder_ttl);
            map.prune_proofs(now, ttl.max(self.standing_span()));
            self.0
                .over_since
                .lock()
                .expect("over_since")
                .retain(|(s, _), _| map.records.contains_key(s));
            self.0
                .unproven_since
                .lock()
                .expect("unproven_since")
                .retain(|s, _| map.records.contains_key(s));
            let mut whole: Vec<(SiteRecord, Action)> = Vec::new();
            let mut striped = Vec::new();
            // Sites (by place in `whole`) whose unproven holders are to be
            // asked, and sites this node is building standing for.
            let mut doubted: Vec<(usize, Vec<String>)> = Vec::new();
            let mut standing: Vec<(SiteRecord, Vec<String>)> = Vec::new();
            for r in map.records.values() {
                if stripe::is_striped(r.size) {
                    striped.push(r.clone());
                    continue;
                }
                let holders = map.holders(&r.site_id);
                let trim = self.trim_due(&r.site_id, WHOLE, holders.len(), now);
                // An announced holder fills a slot once it has proven its
                // copy; announcing alone is free.
                let proven = |p: &str| p == self.0.me || map.proven(&r.site_id, p, now, now_secs, ttl);
                let a = replicas::decide_counting(
                    &map,
                    &r.site_id,
                    &self.0.me,
                    can_take,
                    TARGET_REPLICAS,
                    MAX_REPLICAS,
                    trim,
                    &proven,
                );
                let short_on_claims = || {
                    replicas::decide(&map, &r.site_id, &self.0.me, can_take, TARGET_REPLICAS, MAX_REPLICAS, trim)
                        == Action::Replicate
                };
                if a == Action::Replicate && !short_on_claims() {
                    // Only unproven announcements stand between this node
                    // and a copy: they are asked below, and the copy waits
                    // out the grace.
                    let unproven: Vec<String> = holders.iter().filter(|p| !proven(p)).cloned().collect();
                    doubted.push((whole.len(), unproven));
                } else {
                    self.0.unproven_since.lock().expect("unproven_since").remove(&r.site_id);
                }
                // A free copy of a site that is over the target may be let
                // go later, and that takes standing (see
                // [`DROP_STANDING_SPAN`]): start counting the others' passes
                // now, whichever holder ends up being the one to let go.
                if a == Action::Nothing
                    && holders.len() > TARGET_REPLICAS
                    && holders.iter().any(|p| *p == self.0.me)
                    && !map.effective_preferred(&r.site_id).contains(&self.0.me)
                {
                    let due: Vec<String> = holders
                        .iter()
                        .filter(|p| **p != self.0.me && map.pass_due(&r.site_id, p, now, gap))
                        .cloned()
                        .collect();
                    standing.push((r.clone(), due));
                }
                whole.push((r.clone(), a));
            }
            // The round's challenges go first to sites already waiting on
            // their holders (longest first), so a holder that was slow once
            // is asked again before its site's grace runs out; then to
            // sites not asked yet; and last to building standing.
            {
                let since = self.0.unproven_since.lock().expect("unproven_since");
                doubted.sort_by_key(|(i, _)| match since.get(&whole[*i].0.site_id) {
                    Some(t) => (0, Some(*t)),
                    None => (1, None),
                });
            }
            for (i, unproven) in doubted {
                let most = unproven.len().min(PROOF_ASKS_PER_SITE);
                let asked = asks.take(&whole[i].0, unproven, most);
                if !self.fill_due(&whole[i].0.site_id, asked >= most, now) {
                    whole[i].1 = Action::Nothing;
                }
            }
            for (rec, due) in standing {
                asks.take(&rec, due, PROOF_ASKS_PER_SITE);
            }
            (whole, striped)
        };
        self.spawn_proofs(std::mem::take(&mut asks.list));
        for (rec, action) in whole {
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
                    // The surplus is counted from announcements, which any
                    // key can make. Our copy goes only once enough of the
                    // others have handed over a chunk we picked.
                    if !self.others_prove_whole(&rec, TARGET_REPLICAS).await {
                        debug!(site = %rec.site_id, "hosting: keeping a surplus copy, too few other holders proved theirs");
                        continue;
                    }
                    if let Err(e) = self.drop_site(&rec.site_id).await {
                        warn!(site = %rec.site_id, err = %e, "hosting: drop failed");
                    }
                }
                Action::Nothing if stale => self.spawn_replicate(rec),
                Action::Nothing => {}
            }
        }
        for rec in striped {
            self.repair_striped(rec).await;
        }
    }

    /// One striped site: work out which groups this node should take or let
    /// go, then do it.
    async fn repair_striped(&self, rec: SiteRecord) {
        let held = self.0.catalog.read().await.sites.get(&rec.site_id).cloned();
        // Only nodes that host, or already hold groups, need the manifest.
        if !self.0.hosts && held.is_none() {
            return;
        }
        // Holding the current version whole (a publisher seeding it): nothing
        // to place. An older whole copy is replaced group by group below.
        if held.as_ref().is_some_and(|h| h.seq >= rec.seq)
            && !self
                .0
                .catalog
                .read()
                .await
                .stripes
                .contains_key(&rec.site_id)
        {
            return;
        }
        if self.0.replicating.read().await.contains(&rec.site_id) {
            return;
        }
        let layout = match self.layout(&rec).await {
            Ok(l) => l,
            Err(e) => {
                debug!(site = %rec.site_id, err = %e, "hosting: striped manifest unavailable");
                return;
            }
        };
        let plan = {
            let map = self.0.map.read().await;
            let holders = map.distinct_group_holders(&rec.site_id, layout.len());
            let now = Instant::now();
            let mut short = self.0.short_since.lock().expect("short_since");
            for (g, hs) in holders.iter().enumerate() {
                let key = (rec.site_id.clone(), g as u32);
                if hs.len() < TARGET_REPLICAS {
                    short.entry(key).or_insert(now);
                } else {
                    short.remove(&key);
                }
            }
            let widen = |g: u32| -> usize {
                short
                    .get(&(rec.site_id.clone(), g))
                    .map(|t| (now.duration_since(*t).as_secs() / WIDEN_AFTER.as_secs()) as usize)
                    .unwrap_or(0)
            };
            // The grace clock runs on every holder, a second node on one box
            // included: once a group is over the target on that count for
            // the grace, the planner lets the box's duplicate go (or, past
            // the target on distinct machines, the usual surplus).
            let raw = map.group_holders(&rec.site_id, layout.len(), true);
            let trims: Vec<bool> = raw
                .iter()
                .enumerate()
                .map(|(g, hs)| self.trim_due(&rec.site_id, g as u32, hs.len(), now))
                .collect();
            let trim = |g: u32| trims.get(g as usize).copied().unwrap_or(false);
            // As for a whole site: a holder fills a group's slot once it
            // has proven a copy, and while only unproven announcements
            // stand between this node and a group they are asked first.
            let now_secs = crate::payments::now_secs();
            let ttl = self.proof_ttl();
            let proven = |p: &str| p == self.0.me || map.proven(&rec.site_id, p, now, now_secs, ttl);
            let plan_with = |counts: &dyn Fn(&str) -> bool| {
                stripe::plan_counting(
                    &map,
                    &rec.site_id,
                    &layout,
                    &self.0.me,
                    self.0.hosts,
                    TARGET_REPLICAS,
                    MAX_REPLICAS,
                    &widen,
                    &trim,
                    counts,
                )
            };
            let strict = plan_with(&proven);
            let lenient = plan_with(&|_| true);
            if strict.fetch == lenient.fetch {
                self.0.unproven_since.lock().expect("unproven_since").remove(&rec.site_id);
                strict
            } else {
                let mut unproven: Vec<String> = raw.iter().flatten().filter(|p| !proven(p)).cloned().collect();
                unproven.sort();
                unproven.dedup();
                let mut asks = Asks::new(self.0.proving.lock().expect("proving").clone());
                let most = unproven.len().min(PROOF_ASKS_PER_SITE);
                let asked = asks.take(&rec, unproven, most);
                self.spawn_proofs(asks.list);
                if self.fill_due(&rec.site_id, asked >= most, now) { strict } else { lenient }
            }
        };
        let stale = held.as_ref().is_some_and(|h| h.seq < rec.seq);
        if !plan.drop.is_empty() && !stale {
            // As for a whole site: a group goes only when enough of the
            // other holders counted for it proved they have it.
            let proven = self.groups_others_prove(&rec, &plan.drop, TARGET_REPLICAS).await;
            if proven.len() < plan.drop.len() {
                debug!(site = %rec.site_id, kept = plan.drop.len() - proven.len(), "hosting: keeping surplus groups, too few other holders proved theirs");
            }
            if !proven.is_empty() {
                self.drop_groups(&rec.site_id, &proven).await;
            }
        }
        let mine: BTreeSet<u32> = self
            .0
            .catalog
            .read()
            .await
            .stripes
            .get(&rec.site_id)
            .cloned()
            .unwrap_or_default();
        if stale || plan.fetch.iter().any(|g| !mine.contains(g)) {
            self.spawn_replicate_groups(rec, plan.fetch);
        }
    }

    /// Whether a site ([`WHOLE`]) or one group of a striped site, now at
    /// `copies` live copies, may shed a free copy down toward the target.
    ///
    /// Only once it has been over [`TARGET_REPLICAS`] without a break for one
    /// holder TTL: by then a holder that went away has stopped counting and
    /// every node's view has caught up, so the excess is real and the nodes
    /// agree on who lets go. Dropping back to the target resets the clock,
    /// so a copy that is extra only for a moment is never released (and the
    /// fill, which acts only below the target, does not take it back).
    fn trim_due(&self, site_id: &str, group: u32, copies: usize, now: Instant) -> bool {
        let mut over = self.0.over_since.lock().expect("over_since");
        let key = (site_id.to_string(), group);
        if copies <= TARGET_REPLICAS {
            over.remove(&key);
            return false;
        }
        let since = *over.entry(key).or_insert(now);
        now.duration_since(since) >= self.0.timing.holder_ttl
    }

    /// How long a passed challenge keeps a holder counted (see
    /// [`PROOF_TTL_HOLDER_TTLS`]).
    fn proof_ttl(&self) -> Duration {
        self.0.timing.holder_ttl * PROOF_TTL_HOLDER_TTLS
    }

    /// [`DROP_STANDING_SPAN`] on a node with the normal timing. Tests shrink
    /// [`Timing`] to seconds and cannot wait six hours for a surplus copy to
    /// go; there the span shrinks with the holder TTL.
    fn standing_span(&self) -> Duration {
        if self.0.timing.holder_ttl >= Timing::default().holder_ttl {
            DROP_STANDING_SPAN
        } else {
            self.0.timing.holder_ttl * 2
        }
    }

    /// The least time between two passes that both count toward standing.
    fn standing_gap(&self) -> Duration {
        self.standing_span() / DROP_STANDING_PASSES
    }

    /// Whether this node may now take a copy of a site whose announced
    /// holders it has not seen prove theirs. `asked`: a challenge of each of
    /// them (of [`PROOF_ASKS_PER_SITE`] of them, when there are more) was
    /// queued this round or is already running.
    ///
    /// Not before they have had the grace ([`Self::proof_grace`], one holder
    /// TTL) to answer, counted from the first round they were asked: honest holders that are slow or
    /// unreachable for a moment are asked again every round meanwhile, so a
    /// healthy site does not gain a copy it then has to trim. The clock is
    /// the site's, not the holders': announcing under new keys does not
    /// restart it, and it only stops when the site has its proven copies (or
    /// the unproven announcements go away, which lets the fill run at once).
    /// A site whose turn to be asked has not come (the round's challenges
    /// were used up) has no clock yet and is left as it is.
    fn fill_due(&self, site_id: &str, asked: bool, now: Instant) -> bool {
        let mut since = self.0.unproven_since.lock().expect("unproven_since");
        fill_clock(&mut since, site_id, asked, now, self.proof_grace())
    }

    /// How long announced holders have to prove their copies before a node
    /// next in line takes one anyway: one holder TTL (what the network
    /// already waits before it replaces a holder that went quiet), and
    /// never less than two challenges at their slowest, so an answer that
    /// is still on its way is not read as no answer (tests set the TTL
    /// below the fetch timeout).
    fn proof_grace(&self) -> Duration {
        self.0.timing.holder_ttl.max(self.0.timing.fetch_timeout * 2)
    }

    /// Challenge each (site, holder) in the background, a few at a time.
    /// Passes land in the map ([`Self::holds_now`]); the next repair round
    /// reads them.
    fn spawn_proofs(&self, list: Vec<(SiteRecord, String)>) {
        for (rec, holder) in list {
            let key = (rec.site_id.clone(), holder.clone());
            if !self.0.proving.lock().expect("proving").insert(key.clone()) {
                continue;
            }
            let h = self.clone();
            tokio::spawn(async move {
                if let Ok(_slot) = h.0.prove_slots.acquire().await
                    && !h.is_stopped()
                {
                    let ok = h.holds_now(&rec, &holder, None).await;
                    debug!(site = %rec.site_id, %holder, ok, "hosting: asked a holder to prove its copy");
                }
                h.0.proving.lock().expect("proving").remove(&key);
            });
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

    /// True when this node cannot take another copy: what it may still take
    /// (budget and free-disk floor, see [`Self::room`]) is under a little
    /// headroom.
    pub async fn is_full(&self) -> bool {
        self.room().await < FULL_HEADROOM_BYTES
    }

    fn spawn_replicate_groups(&self, rec: SiteRecord, groups: Vec<u32>) {
        let h = self.clone();
        tokio::spawn(async move {
            {
                let mut busy = h.0.replicating.write().await;
                if !busy.insert(rec.site_id.clone()) {
                    return;
                }
            }
            let id = rec.site_id.clone();
            match h.replicate_groups(&rec, &groups).await {
                Ok(n) if n > 0 => {
                    info!(site = %id, name = %rec.name, groups = n, "hosting: holding groups of a striped site");
                }
                Ok(_) => {}
                Err(e) => warn!(site = %id, err = %e, "hosting: replicate failed"),
            }
            h.announce_holding().await;
            h.0.replicating.write().await.remove(&id);
        });
    }

    /// Bytes this node holds of the owner's other free sites (the per-owner
    /// limit applies to free sites only: every holder of a paid site is paid).
    async fn owner_bytes_excluding(&self, rec: &SiteRecord) -> u64 {
        let recs: Vec<SiteRecord> = self
            .0
            .catalog
            .read()
            .await
            .sites
            .values()
            .filter(|r| r.owner == rec.owner && r.site_id != rec.site_id && r.terms().is_none())
            .cloned()
            .collect();
        let stripes = self.0.catalog.read().await.stripes.clone();
        let mut total = 0;
        for r in recs {
            total += match stripes.get(&r.site_id) {
                Some(gs) => self.layout(&r).await.map(|l| l.bytes_of(gs)).unwrap_or(0),
                None => r.size,
            };
        }
        total
    }

    /// Whether this node may take `extra` more bytes of `rec`: budget, the
    /// free-disk floor, and (free copies only) the per-owner limit. The error
    /// says which limit.
    async fn check_room(&self, rec: &SiteRecord, extra: u64, this_site_held: u64) -> Result<()> {
        // Copies still being fetched count: several sites admitted at once
        // each saw the same `held` and together went past the budget.
        let reserving = self.0.reserving.load(Ordering::Relaxed);
        let held = self.held_bytes().await.saturating_add(reserving);
        let budget = self.budget_with(held);
        if held + extra > budget {
            bail!(
                "over this node's hosting budget ({} GiB held of {} GiB; set [hosting] max_bytes to change it)",
                held / GIB_BYTES,
                budget / GIB_BYTES
            );
        }
        let (disk_free, disk_total) = self.disk();
        if !leaves_free_floor(disk_free, disk_total, extra.saturating_add(reserving)) {
            bail!(
                "disk nearly full: {} GiB free, a copy must leave at least {} GiB",
                disk_free / GIB_BYTES,
                free_disk_floor(disk_total) / GIB_BYTES
            );
        }
        // Free sites only, and not during the beta (hosting is free while
        // c0mpute is in beta): a paid (v2) site's payer pays every holder for
        // the bytes it holds (settle::dues), so its groups spread over any
        // node with room instead of stopping at 16 GiB per node.
        if !site::BETA_FREE_HOSTING && rec.terms().is_none() {
            let owner = self.owner_bytes_excluding(rec).await;
            if owner + this_site_held + extra > MAX_BYTES_PER_OWNER {
                bail!(
                    "over this node's per-owner limit for free hosting ({} GiB)",
                    MAX_BYTES_PER_OWNER / GIB_BYTES
                );
            }
        }
        Ok(())
    }

    /// [`Self::check_room`], then hold `extra` bytes as spoken for until the
    /// returned guard drops (when the copy is in the catalog, or failed).
    async fn admit(&self, rec: &SiteRecord, extra: u64, this_site_held: u64) -> Result<Reserved> {
        let _turn = self.0.admit.lock().await;
        self.check_room(rec, extra, this_site_held).await?;
        self.0.reserving.fetch_add(extra, Ordering::Relaxed);
        Ok(Reserved(self.clone(), extra))
    }

    /// Fetch every chunk of `rec` and start holding it.
    pub async fn replicate(&self, rec: &SiteRecord) -> Result<()> {
        let previous = self.0.catalog.read().await.sites.get(&rec.site_id).cloned();
        let already = previous.as_ref().map(|p| p.size).unwrap_or(0);
        let _reserved = self.admit(rec, rec.size.saturating_sub(already), 0).await?;
        let manifest = self.manifest(rec).await?;
        if manifest.total_bytes() != rec.size || manifest.name() != rec.name {
            bail!("manifest does not match its record");
        }
        let holders = self.0.map.read().await.fetch_sources(&rec.site_id);
        // Part by part for a parted manifest (its chunk list never sits in
        // memory whole), many chunks in flight; the root goes in last,
        // marking the copy complete.
        match &*manifest {
            Manifest::Inline(m) => {
                let wanted: Vec<(Hash, Vec<String>)> = m
                    .chunk_hashes()
                    .iter()
                    .filter_map(|h| Hash::from_hex(h).ok())
                    .map(|h| (h, holders.clone()))
                    .collect();
                self.fetch_into_store(wanted, fetch::BULK_WINDOW).await?;
            }
            Manifest::Parted(r) => {
                for pr in &r.parts {
                    let part = self.part(&rec.site_id, pr).await?;
                    let mut hs: Vec<Hash> = part.chunk_hashes().filter_map(|h| Hash::from_hex(h).ok()).collect();
                    hs.sort_by(|a, b| a.0.cmp(&b.0));
                    hs.dedup();
                    let wanted = hs.into_iter().map(|h| (h, holders.clone())).collect();
                    self.fetch_into_store(wanted, fetch::BULK_WINDOW).await?;
                }
            }
        }
        self.store_manifest(rec, &manifest).await?;
        {
            let mut cat = self.0.catalog.write().await;
            cat.sites.insert(rec.site_id.clone(), rec.clone());
            cat.stripes.remove(&rec.site_id);
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

    /// Store a manifest's parts and root locally (every holder keeps them,
    /// so any holder can answer for the manifest). The root goes in last.
    async fn store_manifest(&self, rec: &SiteRecord, manifest: &Manifest) -> Result<()> {
        if let Manifest::Parted(r) = manifest {
            for pr in &r.parts {
                let part = self.part(&rec.site_id, pr).await?;
                self.0.store.put(&serde_json::to_vec(&*part)?).await?;
            }
        }
        let mh = Hash::from_hex(&rec.manifest).context("manifest hash")?;
        if !self.0.store.has(&mh).await {
            let holders = self.0.map.read().await.fetch_sources(&rec.site_id);
            let bytes = self.fetch_chunk(&mh, &holders).await?;
            self.0.store.put(&bytes).await?;
        }
        Ok(())
    }

    /// Fetch chunks that are not local yet, `window` at a time, from their
    /// sources, into the store. Chunks that fail are retried once (sources
    /// may have changed); the error names how many are still missing.
    async fn fetch_into_store(
        &self,
        wanted: Vec<(Hash, Vec<String>)>,
        window: usize,
    ) -> Result<u64> {
        use futures::StreamExt;
        let mut todo = Vec::new();
        for (h, src) in wanted {
            if !self.0.store.has(&h).await {
                todo.push((h, src));
            }
        }
        let mut fetched = 0u64;
        for attempt in 0..2 {
            if todo.is_empty() {
                break;
            }
            let results: Vec<(Hash, Vec<String>, Result<usize>)> =
                futures::stream::iter(todo.into_iter())
                    .map(|(h, src)| async move {
                        let r = match self.fetch_chunk(&h, &src).await {
                            // A site's bytes are budgeted by the sizes its
                            // manifest states, a chunk per FILE_CHUNK_BYTES:
                            // a longer one is bytes nobody accounted for.
                            Ok(bytes) if bytes.len() > site::FILE_CHUNK_BYTES => {
                                Err(anyhow::anyhow!("chunk {} is {} bytes, over the {} a site chunk may be", h.to_hex(), bytes.len(), site::FILE_CHUNK_BYTES))
                            }
                            Ok(bytes) => self.0.store.put(&bytes).await.map(|_| bytes.len()),
                            Err(e) => Err(e),
                        };
                        (h, src, r)
                    })
                    .buffer_unordered(window.max(1))
                    .collect()
                    .await;
            todo = Vec::new();
            let mut last_err = None;
            for (h, src, r) in results {
                match r {
                    Ok(n) => fetched += n as u64,
                    Err(e) => {
                        last_err = Some(e);
                        todo.push((h, src));
                    }
                }
            }
            if attempt == 1
                && let Some(e) = last_err
            {
                bail!("{} chunks still missing: {e}", todo.len());
            }
        }
        Ok(fetched)
    }

    /// Fetch the given groups of a striped site (those not held yet) and
    /// hold them. A newer version of a site held in part is fetched in full
    /// (chunks it shares with the old version are not fetched again) before
    /// it replaces the old one. Returns how many groups were added.
    pub async fn replicate_groups(&self, rec: &SiteRecord, groups: &[u32]) -> Result<usize> {
        let manifest = self.manifest(rec).await?;
        if manifest.total_bytes() != rec.size || manifest.name() != rec.name {
            bail!("manifest does not match its record");
        }
        let layout = self.layout(rec).await?;
        // A newer version of a site this node holds: switch to it now,
        // holding every group whose chunks are already here, then fetch the
        // rest group by group. Waiting for every planned group of the new
        // version before holding any (as this did) kept a node that had
        // most of a 600 GiB site counted as holding none of it for hours,
        // and lost the whole switch to any failed group.
        self.switch_version_in_place(rec, &manifest, &layout).await;
        let previous = self.0.catalog.read().await.sites.get(&rec.site_id).cloned();
        let same_version = previous
            .as_ref()
            .is_some_and(|p| p.manifest == rec.manifest);
        let incremental = holds_each_group_on_arrival(previous.as_ref(), rec);
        let mine: BTreeSet<u32> = if same_version {
            self.0
                .catalog
                .read()
                .await
                .stripes
                .get(&rec.site_id)
                .cloned()
                .unwrap_or_default()
        } else {
            BTreeSet::new()
        };
        if same_version && previous.as_ref().is_some_and(|p| p.seq < rec.seq) {
            // Same content, newer record (a re-publish): nothing to fetch.
            self.0
                .catalog
                .write()
                .await
                .sites
                .insert(rec.site_id.clone(), rec.clone());
            self.save_catalog().await;
        }
        let todo: Vec<u32> = groups
            .iter()
            .copied()
            .filter(|g| !mine.contains(g) && (*g as usize) < layout.len())
            .collect();
        if todo.is_empty() {
            if !same_version && previous.is_some() {
                // The new version needs nothing from us: let the old one go.
                self.drop_site(&rec.site_id).await?;
            }
            return Ok(0);
        }
        self.store_manifest(rec, &manifest).await?;
        let this_site = std::sync::atomic::AtomicU64::new(layout.bytes_of(&mine));
        let mut added: BTreeSet<u32> = BTreeSet::new();
        let mut last_announce = Instant::now();
        let mut first_err: Option<anyhow::Error> = None;
        let full = std::sync::atomic::AtomicBool::new(false);
        {
            use futures::StreamExt;
            // Two groups at a time keeps the pipe full between groups.
            let layout2 = layout.clone();
            let mut results = futures::stream::iter(todo)
                .map(|g| {
                    let layout = layout2.clone();
                    let this_site = &this_site;
                    let full = &full;
                    async move {
                        if full.load(Ordering::Relaxed) || self.is_stopped() {
                            return (g, Err(anyhow::anyhow!("stopped")));
                        }
                        let need = layout.bytes(g);
                        let _reserved = match self.admit(rec, need, this_site.load(Ordering::Relaxed)).await {
                            Ok(r) => r,
                            Err(e) => {
                                full.store(true, Ordering::Relaxed);
                                return (g, Err(e));
                            }
                        };
                        this_site.fetch_add(need, Ordering::Relaxed);
                        let sources = self.0.map.read().await.group_sources(&rec.site_id, g);
                        let wanted: Vec<(Hash, Vec<String>)> = layout
                            .group(g)
                            .iter()
                            .map(|h| (*h, sources.clone()))
                            .collect();
                        let r = self.fetch_into_store(wanted, fetch::BULK_WINDOW).await;
                        if r.is_err() {
                            this_site.fetch_sub(need, Ordering::Relaxed);
                        }
                        (
                            g,
                            r.map(|_| ()).map_err(|e| e.context(format!("group {g}"))),
                        )
                    }
                })
                .buffer_unordered(2);
            while let Some((g, r)) = results.next().await {
                if let Err(e) = r {
                    if !e.to_string().contains("stopped") {
                        first_err.get_or_insert(e);
                    }
                    continue;
                }
                added.insert(g);
                if incremental {
                    // Hold each group as soon as it is complete, so the copy
                    // counts (and a restart resumes) group by group.
                    {
                        let mut cat = self.0.catalog.write().await;
                        cat.sites.insert(rec.site_id.clone(), rec.clone());
                        cat.stripes
                            .entry(rec.site_id.clone())
                            .or_default()
                            .insert(g);
                    }
                    self.save_catalog().await;
                    if last_announce.elapsed() > Duration::from_secs(20) {
                        self.announce_holding().await;
                        last_announce = Instant::now();
                    }
                }
            }
        }
        if !incremental && !added.is_empty() {
            {
                let mut cat = self.0.catalog.write().await;
                cat.sites.insert(rec.site_id.clone(), rec.clone());
                cat.stripes.insert(rec.site_id.clone(), added.clone());
            }
            self.save_catalog().await;
            if let Some(old) = &previous {
                self.collect_garbage(old, None).await;
            }
        }
        if previous.is_none() || !same_version {
            let _ = self
                .0
                .net
                .announce(&Hash::from_hex(&rec.manifest).context("manifest hash")?)
                .await;
        }
        match first_err {
            Some(e) if added.is_empty() => Err(e),
            Some(e) => {
                warn!(site = %rec.site_id, err = %e, "hosting: some groups not taken");
                Ok(added.len())
            }
            None => Ok(added.len()),
        }
    }

    /// Move this node's copy of a striped site to the newer version `rec`
    /// in one step: the catalog holds `rec` with the groups whose chunks are
    /// all in the store already (a site that grew shares nearly all of
    /// them), and only chunks the new version does not use are deleted.
    /// Nothing to do for a first copy or the version already held.
    async fn switch_version_in_place(&self, rec: &SiteRecord, manifest: &Manifest, layout: &stripe::Layout) {
        let Some(old) = self.0.catalog.read().await.sites.get(&rec.site_id).cloned() else {
            return;
        };
        if old.manifest == rec.manifest || old.seq >= rec.seq {
            return;
        }
        let mut present: BTreeSet<u32> = BTreeSet::new();
        for g in 0..layout.len() as u32 {
            let mut all = true;
            for h in layout.group(g) {
                if !self.0.store.has(h).await {
                    all = false;
                    break;
                }
            }
            if all {
                present.insert(g);
            }
        }
        {
            let mut cat = self.0.catalog.write().await;
            // Another task may have switched meanwhile.
            if cat.sites.get(&rec.site_id).is_none_or(|r| r.manifest != old.manifest) {
                return;
            }
            cat.sites.insert(rec.site_id.clone(), rec.clone());
            cat.stripes.insert(rec.site_id.clone(), present.clone());
        }
        self.save_catalog().await;
        self.collect_garbage(&old, Some(manifest)).await;
        self.announce_holding().await;
        info!(
            site = %rec.site_id,
            name = %rec.name,
            groups = present.len(),
            of = layout.len(),
            "hosting: switched to the new version, holding the groups already here"
        );
    }

    /// Let go of some groups of a striped site: announce first, delete their
    /// chunks once peers have seen it (as [`Self::drop_site`] does).
    async fn drop_groups(&self, site_id: &str, groups: &[u32]) {
        let rec = {
            let mut cat = self.0.catalog.write().await;
            let Some(rec) = cat.sites.get(site_id).cloned() else {
                return;
            };
            let Some(mine) = cat.stripes.get_mut(site_id) else {
                return;
            };
            for g in groups {
                mine.remove(g);
            }
            if mine.is_empty() {
                cat.stripes.remove(site_id);
                cat.sites.remove(site_id);
            }
            rec
        };
        self.save_catalog().await;
        self.announce_holding().await;
        info!(site = %site_id, groups = groups.len(), "hosting: released surplus groups");
        let h = self.clone();
        let grace = self.0.timing.holder_ttl;
        tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            h.collect_garbage(&rec, None).await;
        });
    }

    async fn drop_site(&self, site_id: &str) -> Result<()> {
        let Some(rec) = self.0.catalog.write().await.sites.remove(site_id) else {
            return Ok(());
        };
        self.0.catalog.write().await.stripes.remove(site_id);
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

    /// The chunks (data and manifest) a held site needs kept.
    async fn live_chunks_of(&self, rec: &SiteRecord, groups: Option<&BTreeSet<u32>>, live: &mut HashSet<String>) {
        live.insert(rec.manifest.clone());
        let Ok(m) = self.manifest(rec).await else {
            return;
        };
        match groups {
            Some(gs) => {
                if let Manifest::Parted(r) = &*m {
                    live.extend(r.parts.iter().map(|p| p.hash.clone()));
                }
                if let Ok(layout) = self.layout(rec).await {
                    for g in gs {
                        live.extend(layout.group(*g).iter().map(|h| h.to_hex()));
                    }
                }
            }
            // Whole: every chunk, parts included.
            None => live.extend(self.chunk_set(&rec.site_id, &m).await),
        }
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
        let (held, stripes): (Vec<SiteRecord>, HashMap<String, BTreeSet<u32>>) = {
            let cat = self.0.catalog.read().await;
            (cat.sites.values().cloned().collect(), cat.stripes.clone())
        };
        for r in held {
            self.live_chunks_of(&r, stripes.get(&r.site_id), &mut live).await;
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
        let Ok(files) = self.local_files(rec).await else {
            return false;
        };
        for f in &files {
            for h in &f.chunks {
                match Hash::from_hex(h) {
                    Ok(hash) if self.0.store.has(&hash).await => {}
                    _ => return false,
                }
            }
        }
        true
    }

    /// The groups of `groups` whose chunks are all still here.
    async fn complete_groups(&self, rec: &SiteRecord, groups: &BTreeSet<u32>) -> BTreeSet<u32> {
        let Ok(files) = self.local_files(rec).await else {
            return BTreeSet::new();
        };
        let layout = stripe::Layout::new(stripe::group_count(rec.size), files.iter());
        let mut out = BTreeSet::new();
        'g: for g in groups {
            if *g as usize >= layout.len() {
                continue;
            }
            for h in layout.group(*g) {
                if !self.0.store.has(h).await {
                    continue 'g;
                }
            }
            out.insert(*g);
        }
        out
    }

    /// Every file entry of a site, from local chunks only (root and parts).
    async fn local_files(&self, rec: &SiteRecord) -> Result<Vec<site::FileEntry>> {
        let mh = Hash::from_hex(&rec.manifest).context("manifest hash")?;
        let bytes = self.0.store.get(&mh).await?;
        match Manifest::from_bytes(&bytes)? {
            Manifest::Inline(m) => Ok(m.files.into_values().collect()),
            Manifest::Parted(r) => {
                let mut out = Vec::new();
                for pr in &r.parts {
                    let ph = Hash::from_hex(&pr.hash).context("part hash")?;
                    let pb = self.0.store.get(&ph).await?;
                    out.extend(ManifestPart::from_bytes(&pb, pr)?.files.into_values());
                }
                Ok(out)
            }
        }
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

    /// Hand a site scanned in place to the network: its chunks are served
    /// straight from the source files ([`seed`]), only the manifest is stored.
    /// Publishing the same version again (to resume) is accepted.
    pub async fn publish_scanned(&self, scanned: &site::ScannedSite, record: SiteRecord) -> Result<()> {
        seed::register(&scanned.locations);
        let stored = scanned.manifest.stored();
        for p in &stored.parts {
            self.0.store.put(p).await?;
        }
        let mh = self.0.store.put(&stored.root).await?;
        if mh.to_hex() != record.manifest {
            bail!("record does not point at this manifest");
        }
        let applied = self.0.map.write().await.apply_record(record.clone());
        if !applied && self.record_for(&record.site_id).await.as_ref() != Some(&record) {
            bail!("record rejected (bad signature, or a newer version exists)");
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

    /// Per-group holders of a striped site, from this node's map.
    pub async fn group_holders(&self, rec: &SiteRecord) -> Result<(Arc<stripe::Layout>, Vec<Vec<String>>)> {
        let layout = self.layout(rec).await?;
        let holders = self.0.map.read().await.group_holders(&rec.site_id, layout.len(), true);
        Ok((layout, holders))
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
    /// Of `peers`, those running on this machine: they advertise one of this
    /// machine's globally unique addresses. A copy there sits on the
    /// publisher's own disk, so it is no copy at all.
    pub async fn on_this_machine(&self, peers: &[String]) -> HashSet<String> {
        let mine = self.my_ips().await;
        if mine.is_empty() {
            return HashSet::new();
        }
        self.machine_ips(peers)
            .await
            .into_iter()
            .filter(|(_, ips)| !ips.is_disjoint(&mine))
            .map(|(p, _)| p)
            .collect()
    }

    /// This machine's globally unique addresses.
    async fn my_ips(&self) -> HashSet<std::net::IpAddr> {
        self.0
            .net
            .listen_addrs()
            .await
            .unwrap_or_default()
            .iter()
            .filter_map(unique_ip)
            .collect()
    }

    /// Each peer's globally unique addresses (empty when unknown, e.g. a node
    /// behind NAT): two peers sharing one run on the same machine.
    pub async fn machine_ips(&self, peers: &[String]) -> HashMap<String, HashSet<std::net::IpAddr>> {
        let lookups = peers.iter().filter_map(|p| {
            let id: c0mpute_net::PeerId = p.parse().ok()?;
            let net = self.0.net.clone();
            Some(async move {
                // What its connections and identify showed; the DHT only for
                // a peer we never met (it often knows nothing of direct peers).
                let mut addrs = net.peer_addrs(id).await;
                if addrs.is_empty() {
                    addrs = net.find_peer(id, Duration::from_secs(8)).await;
                }
                (p.clone(), addrs.iter().filter_map(unique_ip).collect())
            })
        });
        futures::future::join_all(lookups).await.into_iter().collect()
    }

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

    /// Per-group copy health of a striped site (`None` for a whole site).
    pub async fn health(&self, rec: &SiteRecord) -> Option<stripe::Health> {
        if !stripe::is_striped(rec.size) {
            return None;
        }
        let map = self.0.map.read().await;
        Some(stripe::health(
            &map.distinct_group_holders(&rec.site_id, stripe::group_count(rec.size)),
            TARGET_REPLICAS,
        ))
    }

    /// Who holds a striped site, and how much of it: (peer, groups held,
    /// room in bytes it last announced), most groups first.
    pub async fn holder_breakdown(&self, rec: &SiteRecord) -> Vec<(String, usize, Option<u64>)> {
        if !stripe::is_striped(rec.size) {
            return Vec::new();
        }
        let map = self.0.map.read().await;
        let mut n: HashMap<String, usize> = HashMap::new();
        for hs in map.group_holders(&rec.site_id, stripe::group_count(rec.size), true) {
            for p in hs {
                *n.entry(p).or_default() += 1;
            }
        }
        let mut v: Vec<(String, usize, Option<u64>)> = n
            .into_iter()
            .map(|(p, c)| {
                let r = map.room_of(&p);
                (p, c, r)
            })
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v
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

    /// A chunk of `rec` nobody could predict, to ask `holder` for: of a
    /// striped site, one from a group it says it holds (among `only`, when
    /// given).
    async fn challenge_chunk(&self, rec: &SiteRecord, holder: &str, only: Option<&[u32]>) -> Result<String> {
        let manifest = self.manifest(rec).await?;
        let nonce: [u8; 32] = rand::random();
        let striped_pick = if stripe::is_striped(rec.size) {
            // Only a chunk of a group the holder says it holds.
            let layout = self.layout(rec).await?;
            let groups: Vec<u32> = {
                let map = self.0.map.read().await;
                map.group_holders(&rec.site_id, layout.len(), true)
                    .iter()
                    .enumerate()
                    .filter(|(_, hs)| hs.iter().any(|p| p == holder))
                    .filter(|(g, _)| only.is_none_or(|o| o.contains(&(*g as u32))))
                    .map(|(g, _)| g as u32)
                    .collect()
            };
            Some(proof::pick_chunk_of(&layout, &groups, &nonce).context("holder holds no groups")?)
        } else {
            None
        };
        let chunk = match &*manifest {
            _ if striped_pick.is_some() => striped_pick,
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
        Ok(chunk)
    }

    /// Whether `holder` hands over a random chunk of `rec` right now, asked
    /// directly. A holding announcement is only a claim (any key can gossip
    /// one); this is the check a node makes before it counts the copy or
    /// lets its own go. A pass is remembered ([`ReplicaMap::note_pass`]).
    ///
    /// A peer with no copy can still pass by fetching the chunk from a real
    /// holder while it is being asked. When the real holder it asks is this
    /// node, that shows: the chunk is read from this node's store between
    /// the question and the answer. Such an answer is not a pass. The read
    /// is matched by chunk, not by peer (a relay would ask under another
    /// key), so now and then an unrelated read of the same chunk voids an
    /// honest answer; the holder is simply asked again later.
    async fn holds_now(&self, rec: &SiteRecord, holder: &str, only: Option<&[u32]>) -> bool {
        let Ok(chunk) = self.challenge_chunk(rec, holder, only).await else {
            return false;
        };
        let (Ok(hash), Ok(pid)) = (Hash::from_hex(&chunk), holder.parse::<c0mpute_net::PeerId>()) else {
            return false;
        };
        let req = ChunkRequest {
            chunk_hash: hash,
            shard_index: None,
        };
        let watch = self.0.store.watch_reads(&hash);
        let got = tokio::time::timeout(self.0.timing.fetch_timeout, self.0.net.fetch_from_peer(pid, &req)).await;
        let ok = answer_passes(matches!(&got, Ok(Ok(bytes)) if Hash::of(bytes) == hash), watch.seen());
        if ok {
            self.note_pass(rec, holder).await;
        }
        ok
    }

    /// The announced holders of `rec` whose copy this node has seen proven,
    /// after asking those it has not (at most [`DROP_PROBES`] of them, at
    /// once). What a publisher counts before it stops seeding: three keys
    /// that only announce would otherwise read as three copies, the
    /// publisher would leave, and the site would have no source left.
    pub async fn verified_holders(&self, rec: &SiteRecord) -> Vec<String> {
        let (now_secs, ttl) = (crate::payments::now_secs(), self.proof_ttl());
        let holders = self.0.map.read().await.holders(&rec.site_id);
        let unproven: Vec<String> = {
            let map = self.0.map.read().await;
            let now = Instant::now();
            holders
                .iter()
                .filter(|p| **p != self.0.me && !map.proven(&rec.site_id, p, now, now_secs, ttl))
                .take(DROP_PROBES)
                .cloned()
                .collect()
        };
        futures::future::join_all(unproven.iter().map(|p| self.holds_now(rec, p, None))).await;
        let map = self.0.map.read().await;
        let now = Instant::now();
        holders
            .into_iter()
            .filter(|p| *p == self.0.me || map.proven(&rec.site_id, p, now, now_secs, ttl))
            .collect()
    }

    /// Remember that `holder` just passed a challenge for `rec`.
    async fn note_pass(&self, rec: &SiteRecord, holder: &str) {
        self.0.map.write().await.note_pass(
            &rec.site_id,
            holder,
            &rec.manifest,
            Instant::now(),
            self.standing_gap(),
            self.standing_span(),
        );
    }

    /// The holders among `others` worth asking before a copy is let go:
    /// those that would have standing with a pass now, and those whose pass
    /// would add to their count. Asking the rest proves nothing new.
    async fn drop_asks(&self, rec: &SiteRecord, others: Vec<String>) -> Vec<String> {
        let (now, gap, span) = (Instant::now(), self.standing_gap(), self.standing_span());
        let map = self.0.map.read().await;
        others
            .into_iter()
            .filter(|p| {
                map.has_standing(&rec.site_id, p, now, DROP_STANDING_PASSES, span)
                    || map.pass_due(&rec.site_id, p, now, gap)
            })
            .take(DROP_PROBES)
            .collect()
    }

    /// Whether `holder` (which just passed) has the standing a drop needs.
    async fn has_standing(&self, rec: &SiteRecord, holder: &str) -> bool {
        self.0.map.read().await.has_standing(
            &rec.site_id,
            holder,
            Instant::now(),
            DROP_STANDING_PASSES,
            self.standing_span(),
        )
    }

    /// Whether at least `need` other announced holders of `rec` prove their
    /// copy now (see [`Self::holds_now`]) and have proven it before, often
    /// and long enough to have standing (see [`DROP_STANDING_SPAN`]). At
    /// most [`DROP_PROBES`] are asked.
    async fn others_prove_whole(&self, rec: &SiteRecord, need: usize) -> bool {
        let mut others: Vec<String> = self.0.map.read().await.holders(&rec.site_id);
        others.retain(|p| *p != self.0.me);
        {
            use rand::seq::SliceRandom;
            others.shuffle(&mut rand::thread_rng());
        }
        let others = self.drop_asks(rec, others).await;
        let mut proven = 0;
        for holder in others.iter() {
            if self.holds_now(rec, holder, None).await && self.has_standing(rec, holder).await {
                proven += 1;
                if proven >= need {
                    return true;
                }
            }
        }
        false
    }

    /// Of `groups` (ones this node is about to let go), those that at least
    /// `need` other announced holders prove they hold, with the standing a
    /// whole site asks for ([`Self::others_prove_whole`]). Each holder is
    /// asked once, for a chunk of one of the groups in question that it
    /// claims.
    async fn groups_others_prove(&self, rec: &SiteRecord, groups: &[u32], need: usize) -> Vec<u32> {
        let Ok(layout) = self.layout(rec).await else {
            return Vec::new();
        };
        let by_group: Vec<Vec<String>> = self.0.map.read().await.group_holders(&rec.site_id, layout.len(), true);
        let mut claimants: Vec<String> = groups
            .iter()
            .filter_map(|g| by_group.get(*g as usize))
            .flatten()
            .filter(|p| **p != self.0.me)
            .cloned()
            .collect();
        claimants.sort();
        claimants.dedup();
        {
            use rand::seq::SliceRandom;
            claimants.shuffle(&mut rand::thread_rng());
        }
        let claimants = self.drop_asks(rec, claimants).await;
        let mut proven: HashSet<String> = HashSet::new();
        for holder in claimants.iter() {
            if self.holds_now(rec, holder, Some(groups)).await && self.has_standing(rec, holder).await {
                proven.insert(holder.clone());
            }
        }
        groups
            .iter()
            .copied()
            .filter(|g| {
                by_group
                    .get(*g as usize)
                    .is_some_and(|hs| hs.iter().filter(|p| proven.contains(*p)).count() >= need)
            })
            .collect()
    }

    /// Challenge one holder of one site and record the signed receipt.
    pub async fn challenge_one(
        &self,
        kp: &c0mpute_net::libp2p::identity::Keypair,
        rec: &SiteRecord,
        holder: &str,
    ) -> Result<Receipt> {
        let chunk = self.challenge_chunk(rec, holder, None).await?;
        let hash = Hash::from_hex(&chunk).context("bad chunk hash")?;
        let pid: c0mpute_net::PeerId = holder.parse().context("holder is not a peer id")?;
        let req = ChunkRequest {
            chunk_hash: hash,
            shard_index: None,
        };
        let started = Instant::now();
        let watch = self.0.store.watch_reads(&hash);
        let got = tokio::time::timeout(self.0.timing.fetch_timeout, self.0.net.fetch_from_peer(pid, &req)).await;
        let right = matches!(&got, Ok(Ok(bytes)) if Hash::of(bytes) == hash);
        if right && watch.seen() {
            // The chunk was read from this node while the holder was being
            // asked for it (see [`Self::holds_now`]): neither a pass nor,
            // since the read may have been someone else's, a signed failure.
            bail!("{holder} answered while chunk {chunk} was being read from this node");
        }
        let ok = right;
        if ok {
            self.note_pass(rec, holder).await;
        }
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

    /// What this node (as payer) owes the holders of its paid sites for
    /// `[from, to)`.
    pub async fn dues(&self, from: u64, to: u64) -> Vec<settle::Due> {
        let Some(payer) = self.0.challenger_hex.clone() else {
            return Vec::new();
        };
        let held = self.held_by_others(&payer).await;
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
        settle::dues(&records, &receipts, &payouts, &held, &payer, from, to)
    }

    /// Bytes each hosting peer holds now of the paid sites `payer` pays for,
    /// keyed (site id, peer), leaving out preferred hosts (they are paid
    /// their agreed share). A peer holding a striped site in part holds the
    /// bytes of its groups; one holding it whole holds all of it.
    async fn held_by_others(&self, payer: &str) -> HashMap<(String, String), u64> {
        let holdings: Vec<(SiteRecord, Vec<(String, Option<BTreeSet<u32>>)>)> = {
            let map = self.0.map.read().await;
            map.records
                .values()
                .filter(|r| r.terms().is_some() && r.payer_key() == payer)
                .map(|r| {
                    let hs = map
                        .peers
                        .iter()
                        .filter(|(p, h)| {
                            h.hosts
                                && !r.prefers(p)
                                && h.sites.get(&r.site_id).is_some_and(|(m, _)| *m == r.manifest)
                        })
                        .map(|(p, h)| (p.clone(), h.groups.get(&r.site_id).cloned()))
                        .collect();
                    (r.clone(), hs)
                })
                .filter(|(_, hs): &(SiteRecord, Vec<_>)| !hs.is_empty())
                .collect()
        };
        let mut out = HashMap::new();
        for (rec, hs) in holdings {
            let layout = if hs.iter().any(|(_, g)| g.is_some()) {
                match self.layout(&rec).await {
                    Ok(l) => Some(l),
                    Err(e) => {
                        debug!(site = %rec.site_id, err = %e, "hosting: no layout to price held groups");
                        None
                    }
                }
            } else {
                None
            };
            for (peer, groups) in hs {
                let bytes = match (&groups, &layout) {
                    (None, _) => rec.size,
                    (Some(gs), Some(l)) => l.bytes_of(gs),
                    (Some(_), None) => continue,
                };
                out.insert((rec.site_id.clone(), peer), bytes);
            }
        }
        out
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

    // ── relay guard ──────────────────────────────────────────────────────

    /// Whether a chunk fetch from `peer` should be answered as if the chunk
    /// were not here: `peer` announces, whole, every site held here that
    /// uses the chunk (see [`relay`]). For whatever serves libp2p chunk
    /// fetches; gateway HTTP reads never come this way.
    ///
    /// The first call turns the chunk index on. Until the next announce
    /// round has filled it, and for chunks of sites too big for it, the
    /// answer is false (serve).
    pub async fn refuses_fetch(&self, peer: &str, chunk: &Hash) -> bool {
        self.0.guards_fetches.store(true, Ordering::Relaxed);
        let map = self.0.map.read().await;
        let index = self.0.chunk_index.lock().expect("chunk_index");
        relay::refuses(&map, &index, peer, chunk)
    }

    /// Bring the chunk index in line with the catalog: sites let go are
    /// forgotten, new or changed ones (another version, other groups)
    /// indexed. Runs with the announcer, so it is at most one round behind;
    /// behind means a fetch is served that could have been refused, or (a
    /// site just let go) refused to a peer that claims the site anyway.
    async fn refresh_chunk_index(&self) {
        if !self.0.guards_fetches.load(Ordering::Relaxed) {
            return;
        }
        let (held, stripes): (Vec<SiteRecord>, HashMap<String, BTreeSet<u32>>) = {
            let cat = self.0.catalog.read().await;
            (cat.sites.values().cloned().collect(), cat.stripes.clone())
        };
        let ids: HashSet<&str> = held.iter().map(|r| r.site_id.as_str()).collect();
        self.0
            .chunk_index
            .lock()
            .expect("chunk_index")
            .retain_sites(&|s| ids.contains(s));
        for r in &held {
            let groups = stripes.get(&r.site_id);
            let stamp = format!("{}:{}", r.manifest, groups.map(stripe::encode_groups).unwrap_or_default());
            let room = {
                let index = self.0.chunk_index.lock().expect("chunk_index");
                if index.stamp(&r.site_id) == Some(stamp.as_str()) {
                    continue;
                }
                index.room()
            };
            // A site plainly too big is not listed chunk by chunk to find
            // that out (a whole one; a striped one is judged by its groups).
            if groups.is_none() && r.size / site::FILE_CHUNK_BYTES as u64 > room as u64 {
                self.0.chunk_index.lock().expect("chunk_index").skip(&r.site_id, &stamp);
                continue;
            }
            let mut live = HashSet::new();
            self.live_chunks_of(r, groups, &mut live).await;
            let chunks: Vec<Hash> = live.iter().filter_map(|h| Hash::from_hex(h).ok()).collect();
            self.0.chunk_index.lock().expect("chunk_index").set(&r.site_id, &stamp, chunks);
        }
    }

    // ── fetching ─────────────────────────────────────────────────────────

    /// One chunk: local (store, or a file this process is publishing), else
    /// from the best of `holders` (see [`fetch`]).
    async fn fetch_chunk(&self, hash: &Hash, holders: &[String]) -> Result<Vec<u8>> {
        if self.0.store.has(hash).await {
            return self.0.store.get(hash).await;
        }
        if let Some(bytes) = seed::read_async(hash).await {
            return Ok(bytes);
        }
        let _slot = self.0.fetch_slots.acquire().await?;
        let turn = self.0.turn.fetch_add(1, Ordering::Relaxed);
        fetch::fetch_one(
            &self.0.net,
            &self.0.stats,
            &self.0.me,
            hash,
            holders,
            self.0.timing.fetch_timeout,
            turn,
        )
        .await
    }

    /// Bytes received from each peer by this node's fetches so far.
    pub fn fetched_by_peer(&self) -> Vec<(String, u64)> {
        self.0.stats.bytes_by_peer()
    }

    /// Every file entry of a site (all parts of a parted manifest).
    async fn files_of(&self, rec: &SiteRecord, m: &Manifest) -> Result<Vec<site::FileEntry>> {
        match m {
            Manifest::Inline(m) => Ok(m.files.values().cloned().collect()),
            Manifest::Parted(r) => {
                let mut out = Vec::new();
                for pr in &r.parts {
                    out.extend(self.part(&rec.site_id, pr).await?.files.values().cloned());
                }
                Ok(out)
            }
        }
    }

    /// The group layout of a striped site's current manifest.
    async fn layout(&self, rec: &SiteRecord) -> Result<Arc<stripe::Layout>> {
        if let Some(l) = self.0.layouts.read().await.get(&rec.manifest) {
            return Ok(l.clone());
        }
        let m = self.manifest(rec).await?;
        let files = self.files_of(rec, &m).await?;
        let l = Arc::new(stripe::Layout::new(stripe::group_count(rec.size), files.iter()));
        let mut cache = self.0.layouts.write().await;
        if cache.len() > 64 {
            cache.clear();
        }
        cache.insert(rec.manifest.clone(), l.clone());
        Ok(l)
    }

    /// Where to fetch the chunks of `rec` from: per chunk for a striped site
    /// (the holders of its group, found from its hash), else every source.
    async fn chunk_sources(&self, rec: &SiteRecord) -> ChunkSources {
        let map = self.0.map.read().await;
        if !stripe::is_striped(rec.size) {
            return ChunkSources::Whole(map.fetch_sources(&rec.site_id));
        }
        let n = stripe::group_count(rec.size);
        ChunkSources::Striped {
            n,
            per_group: map.group_holders(&rec.site_id, n, false),
        }
    }

    /// One part of a large site's manifest (local, cached, or fetched).
    async fn part(&self, site_id: &str, r: &PartRef) -> Result<Arc<ManifestPart>> {
        if let Some(p) = self.0.parts.read().await.get(&r.hash) {
            return Ok(p.clone());
        }
        let hash = Hash::from_hex(&r.hash).context("part hash")?;
        let holders = self.0.map.read().await.fetch_sources(site_id);
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

    /// The generated HTML listing of directory `dir` (`""` or ending in `/`),
    /// or `None` when nothing lives under it.
    async fn listing(&self, rec: &SiteRecord, m: &Manifest, dir: &str) -> Result<Option<String>> {
        /// Most manifest parts one listing reads.
        const MAX_LISTING_PARTS: usize = 16;
        let (dirs, files) = match m {
            Manifest::Inline(sm) => site::dir_children(sm.files.iter(), dir),
            Manifest::Parted(r) => {
                // Parts are in path order: start at the part that would hold
                // `dir` and stop once a part begins past the prefix.
                let start = r.parts.partition_point(|p| p.first.as_str() <= dir).saturating_sub(1);
                let mut dirs = std::collections::BTreeMap::new();
                let mut files = Vec::new();
                for (n, pr) in r.parts[start..].iter().enumerate() {
                    if n > 0 && pr.first.as_str() > dir && !pr.first.starts_with(dir) {
                        break;
                    }
                    if n >= MAX_LISTING_PARTS || files.len() + dirs.len() >= site::LISTING_MAX_ROWS {
                        break;
                    }
                    let part = self.part(&rec.site_id, pr).await?;
                    let (d, f) = site::dir_children(part.files.iter(), dir);
                    for (k, (c, b)) in d {
                        let e: &mut (u64, u64) = dirs.entry(k).or_default();
                        e.0 += c;
                        e.1 += b;
                    }
                    files.extend(f);
                }
                (dirs, files)
            }
        };
        if dirs.is_empty() && files.is_empty() {
            return Ok(None);
        }
        Ok(Some(site::render_listing(m.name(), dir, &dirs, &files)))
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
        let holders = self.0.map.read().await.fetch_sources(&rec.site_id);
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
        // The size is the publisher's word and the whole file lands in
        // memory: nothing read this way (ref maps, packs, sealed objects,
        // ad creatives) is bigger than a repo may be.
        if entry.size > MAX_READ_FILE_BYTES {
            bail!("file is {} bytes, over the {} read whole", entry.size, MAX_READ_FILE_BYTES);
        }
        let sources = self.chunk_sources(rec).await;
        let wanted: Vec<(u64, Hash)> = entry
            .chunks
            .iter()
            .enumerate()
            .map(|(i, h)| {
                Hash::from_hex(h)
                    .map(|h| (i as u64, h))
                    .context("bad chunk hash")
            })
            .collect::<Result<_>>()?;
        self.read_range(rec, &sources, &wanted, 0, entry.size.saturating_sub(1))
            .await
    }

    /// Serve `path` of a site: from local chunks, else proxied from holders.
    pub async fn serve(&self, rec: &SiteRecord, path: &str) -> Response {
        self.serve_request(rec, path, &HeaderMap::new()).await
    }

    /// Serve `path` of a site for a request with these headers: Range
    /// requests (206 with only the chunks the range needs), conditional
    /// requests (304), and large files streamed with read-ahead so video
    /// seeks and plays without loading the whole file first.
    pub async fn serve_request(
        &self,
        rec: &SiteRecord,
        path: &str,
        headers: &HeaderMap,
    ) -> Response {
        let manifest = match self.manifest(rec).await {
            Ok(m) => m,
            Err(e) => {
                warn!(site = %rec.site_id, err = %e, "hosting: manifest unavailable");
                return (StatusCode::BAD_GATEWAY, "site is unavailable right now").into_response();
            }
        };
        let resolved = self.resolve_entry(rec, &manifest, path).await;
        // A directory with no index.html (a folder of mp3s, say) gets a
        // generated listing instead of a bare 404.
        if let Ok(None) = resolved
            && let Some(dir) = site::listing_dir(path)
        {
            match self.listing(rec, &manifest, &dir).await {
                Ok(Some(html)) => {
                    let mut resp = (StatusCode::OK, html).into_response();
                    let h = resp.headers_mut();
                    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
                    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=60"));
                    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
                    if let Ok(v) = HeaderValue::from_str(&rec.site_id) {
                        h.insert("x-c0mpute-site", v);
                    }
                    return resp;
                }
                Ok(None) => {}
                Err(e) => {
                    warn!(site = %rec.site_id, err = %e, "hosting: manifest part unavailable");
                    return (StatusCode::BAD_GATEWAY, "site is unavailable right now").into_response();
                }
            }
        }
        let found = match resolved {
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
        let etag = format!(
            "\"{}\"",
            &Hash::of(entry.chunks.join(",").as_bytes()).to_hex()[..32]
        );
        let html = entry.content_type.starts_with("text/html");
        let cache = if html || status != StatusCode::OK {
            "public, max-age=60"
        } else {
            // Media and assets: a new version of the site changes the ETag.
            "public, max-age=3600"
        };
        let base_headers = |resp: &mut Response| {
            let hdr = resp.headers_mut();
            if let Ok(v) = HeaderValue::from_str(&entry.content_type) {
                hdr.insert(header::CONTENT_TYPE, v);
            }
            if let Ok(v) = HeaderValue::from_str(&etag) {
                hdr.insert(header::ETAG, v);
            }
            hdr.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
            hdr.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
            hdr.insert(
                "x-content-type-options",
                HeaderValue::from_static("nosniff"),
            );
            hdr.insert(
                "x-c0mpute-site",
                HeaderValue::from_str(&rec.site_id).unwrap_or(HeaderValue::from_static("")),
            );
        };
        if status == StatusCode::OK
            && headers
                .get(header::IF_NONE_MATCH)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.split(',').any(|t| t.trim() == etag || t.trim() == "*"))
        {
            let mut resp = StatusCode::NOT_MODIFIED.into_response();
            base_headers(&mut resp);
            resp.headers_mut().remove(header::CONTENT_TYPE);
            return resp;
        }
        // A Range applies only to the real file, and only when If-Range (if
        // any) still names this version.
        let range = if status == StatusCode::OK {
            let if_range_ok = headers
                .get(header::IF_RANGE)
                .and_then(|v| v.to_str().ok())
                .is_none_or(|v| v.trim() == etag);
            headers
                .get(header::RANGE)
                .and_then(|v| v.to_str().ok())
                .filter(|_| if_range_ok)
                .map(|v| parse_range(v, entry.size))
        } else {
            None
        };
        let (start, end_incl, status) = match range {
            None | Some(RangeSpec::Ignore) => (0, entry.size.saturating_sub(1), status),
            Some(RangeSpec::Unsatisfiable) => {
                let mut resp = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
                base_headers(&mut resp);
                resp.headers_mut().remove(header::CONTENT_TYPE);
                if let Ok(v) = HeaderValue::from_str(&format!("bytes */{}", entry.size)) {
                    resp.headers_mut().insert(header::CONTENT_RANGE, v);
                }
                return resp;
            }
            Some(RangeSpec::Bytes(a, b)) => (a, b, StatusCode::PARTIAL_CONTENT),
        };
        let len = if entry.size == 0 {
            0
        } else {
            end_incl - start + 1
        };
        let sources = Arc::new(self.chunk_sources(rec).await);
        let cb = site::FILE_CHUNK_BYTES as u64;
        let (first, last) = if len == 0 {
            (0, 0)
        } else {
            (start / cb, end_incl / cb)
        };
        let wanted: Vec<(u64, Hash)> = entry
            .chunks
            .iter()
            .enumerate()
            .skip(first as usize)
            .take((last - first + 1) as usize)
            .filter_map(|(i, h)| Hash::from_hex(h).ok().map(|h| (i as u64, h)))
            .collect();
        // Small bodies are read whole first, so a missing chunk is a clean 502.
        let small = wanted.len() <= fetch::READ_AHEAD;
        // A page small enough to have its meta tags rewritten is read whole;
        // a bigger one streams like any other file (its size is the
        // publisher's word, and one chunk may be listed any number of times).
        let whole_html = html && entry.size <= MAX_META_REWRITE_BYTES as u64;
        let body = if small || whole_html {
            match self
                .read_range(rec, &sources, &wanted, start, end_incl)
                .await
            {
                Ok(b) => Body::from(b),
                Err(e) => {
                    warn!(site = %rec.site_id, err = %e, "hosting: chunk unavailable");
                    return (StatusCode::BAD_GATEWAY, "site is unavailable right now")
                        .into_response();
                }
            }
        } else {
            // Fetch the first chunk before answering, so an unavailable site
            // still answers 502 rather than a truncated 200.
            let (i0, h0) = wanted[0];
            let first_bytes = match self.fetch_chunk(&h0, sources.for_chunk(&h0)).await {
                Ok(b) => b,
                Err(e) => {
                    warn!(site = %rec.site_id, err = %e, "hosting: chunk unavailable");
                    return (StatusCode::BAD_GATEWAY, "site is unavailable right now")
                        .into_response();
                }
            };
            Body::from_stream(self.stream_range(
                sources,
                wanted,
                (i0, first_bytes),
                start,
                end_incl,
            ))
        };
        let mut resp = (status, body).into_response();
        base_headers(&mut resp);
        if let Ok(v) = HeaderValue::from_str(&len.to_string()) {
            resp.headers_mut().insert(header::CONTENT_LENGTH, v);
        }
        if status == StatusCode::PARTIAL_CONTENT
            && let Ok(v) =
                HeaderValue::from_str(&format!("bytes {start}-{end_incl}/{}", entry.size))
        {
            resp.headers_mut().insert(header::CONTENT_RANGE, v);
        }
        resp
    }

    /// The bytes `start..=end` of a file whose chunks `wanted` (index, hash)
    /// cover that range, read whole.
    async fn read_range(
        &self,
        _rec: &SiteRecord,
        sources: &ChunkSources,
        wanted: &[(u64, Hash)],
        start: u64,
        end_incl: u64,
    ) -> Result<Vec<u8>> {
        use futures::StreamExt;
        let parts: Vec<Result<(u64, Vec<u8>)>> = futures::stream::iter(wanted.iter().copied())
            .map(|(i, h)| async move {
                self.fetch_chunk(&h, sources.for_chunk(&h))
                    .await
                    .map(|b| (i, b))
            })
            .buffered(fetch::READ_AHEAD)
            .collect()
            .await;
        let mut out = Vec::new();
        for p in parts {
            let (i, b) = p?;
            out.extend_from_slice(slice_chunk(i, &b, start, end_incl));
        }
        Ok(out)
    }

    /// Stream the bytes `start..=end`, fetching [`fetch::READ_AHEAD`] chunks
    /// ahead of what has been sent.
    fn stream_range(
        &self,
        sources: Arc<ChunkSources>,
        wanted: Vec<(u64, Hash)>,
        first: (u64, Vec<u8>),
        start: u64,
        end_incl: u64,
    ) -> impl futures::Stream<Item = std::result::Result<axum::body::Bytes, std::io::Error>>
    + Send
    + 'static {
        use futures::StreamExt;
        let h = self.clone();
        let head = futures::stream::once(async move { Ok::<_, anyhow::Error>(first) });
        let rest = futures::stream::iter(wanted.into_iter().skip(1))
            .map(move |(i, hash)| {
                let h = h.clone();
                let sources = sources.clone();
                async move {
                    h.fetch_chunk(&hash, sources.for_chunk(&hash))
                        .await
                        .map(|b| (i, b))
                }
            })
            .buffered(fetch::READ_AHEAD);
        head.chain(rest).map(move |r| match r {
            Ok((i, b)) => Ok(axum::body::Bytes::copy_from_slice(slice_chunk(
                i, &b, start, end_incl,
            ))),
            Err(e) => {
                warn!(err = %e, "hosting: chunk unavailable mid-stream");
                Err(std::io::Error::other(e.to_string()))
            }
        })
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
            .merge(git::router())
            .fallback(serve_by_host)
            // Last, so it is in front of every route above and the fallback.
            .layer(axum::middleware::from_fn_with_state(self.clone(), serve_site_host))
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

/// Whether an answer to a challenge is a pass: the right bytes came back,
/// and the chunk was not read from this node while the holder was being
/// asked (see [`Hosting::holds_now`]).
fn answer_passes(right_bytes: bool, read_from_us_meanwhile: bool) -> bool {
    right_bytes && !read_from_us_meanwhile
}

/// [`Hosting::fill_due`] on its table of clocks: true once `site_id`'s clock
/// has run for `grace`. The clock starts the first round the site's unproven
/// holders were all asked (`asked`): a site only some of whose holders could
/// be asked has not had its chance yet.
fn fill_clock(since: &mut HashMap<String, Instant>, site_id: &str, asked: bool, now: Instant, grace: Duration) -> bool {
    match since.get(site_id) {
        Some(t) => now.duration_since(*t) >= grace,
        None => {
            if asked {
                since.insert(site_id.to_string(), now);
            }
            false
        }
    }
}

/// The background challenges one repair round starts, within its limits
/// ([`PROOF_ASKS_PER_ROUND`], [`PROOF_ASKS_PER_PEER`]).
struct Asks {
    /// Challenges already running, as (site id, holder).
    busy: HashSet<(String, String)>,
    left: usize,
    per_peer: HashMap<String, usize>,
    list: Vec<(SiteRecord, String)>,
}

impl Asks {
    fn new(busy: HashSet<(String, String)>) -> Self {
        Self {
            busy,
            left: PROOF_ASKS_PER_ROUND,
            per_peer: HashMap::new(),
            list: Vec::new(),
        }
    }

    /// Queue challenges of up to `most` of `holders` of `rec`, picked at
    /// random (so which of many announcers are asked cannot be arranged),
    /// as far as the round's limits allow. Returns how many are queued or
    /// already running.
    fn take(&mut self, rec: &SiteRecord, mut holders: Vec<String>, most: usize) -> usize {
        use rand::seq::SliceRandom;
        holders.shuffle(&mut rand::thread_rng());
        let mut n = 0;
        for holder in holders {
            if n >= most {
                break;
            }
            if self.busy.contains(&(rec.site_id.clone(), holder.clone())) {
                n += 1;
                continue;
            }
            let of_peer = self.per_peer.entry(holder.clone()).or_insert(0);
            if self.left == 0 || *of_peer >= PROOF_ASKS_PER_PEER {
                continue;
            }
            self.left -= 1;
            *of_peer += 1;
            self.busy.insert((rec.site_id.clone(), holder.clone()));
            self.list.push((rec.clone(), holder));
            n += 1;
        }
        n
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

/// Where to fetch each chunk of one site from.
enum ChunkSources {
    Whole(Vec<String>),
    Striped { n: usize, per_group: Vec<Vec<String>> },
}

impl ChunkSources {
    fn for_chunk(&self, h: &Hash) -> &[String] {
        match self {
            ChunkSources::Whole(v) => v,
            ChunkSources::Striped { n, per_group } => per_group
                .get(stripe::group_of_hash(h, *n) as usize)
                .map(|v| v.as_slice())
                .unwrap_or(&[]),
        }
    }
}

/// A `Range` header against a file of `size` bytes.
#[derive(Debug, PartialEq, Eq)]
enum RangeSpec {
    /// Serve the whole file (no usable range: several ranges, other units).
    Ignore,
    Unsatisfiable,
    /// Inclusive byte range.
    Bytes(u64, u64),
}

fn parse_range(v: &str, size: u64) -> RangeSpec {
    let Some(spec) = v.trim().strip_prefix("bytes=") else {
        return RangeSpec::Ignore;
    };
    if spec.contains(',') {
        return RangeSpec::Ignore;
    }
    let Some((a, b)) = spec.trim().split_once('-') else {
        return RangeSpec::Ignore;
    };
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() {
        // Suffix: the last `b` bytes.
        let Ok(n) = b.parse::<u64>() else {
            return RangeSpec::Ignore;
        };
        if n == 0 || size == 0 {
            return RangeSpec::Unsatisfiable;
        }
        return RangeSpec::Bytes(size.saturating_sub(n), size - 1);
    }
    let Ok(start) = a.parse::<u64>() else {
        return RangeSpec::Ignore;
    };
    if start >= size {
        return RangeSpec::Unsatisfiable;
    }
    let end = if b.is_empty() {
        size - 1
    } else {
        match b.parse::<u64>() {
            Ok(e) if e >= start => e.min(size - 1),
            _ => return RangeSpec::Ignore,
        }
    };
    RangeSpec::Bytes(start, end)
}

/// The part of chunk `i` (1 MiB each) inside the inclusive range.
fn slice_chunk(i: u64, bytes: &[u8], start: u64, end_incl: u64) -> &[u8] {
    let cb = site::FILE_CHUNK_BYTES as u64;
    let c0 = i * cb;
    let from = start.saturating_sub(c0).min(bytes.len() as u64) as usize;
    let to = (end_incl + 1).saturating_sub(c0).min(bytes.len() as u64) as usize;
    &bytes[from..to.max(from)]
}

/// The automatic hosting budget (`[hosting] max_bytes = 0`): what the node
/// already holds plus [`AUTO_BUDGET_PERCENT`] of the free space above the
/// free-disk floor. It follows the disk: a nearly full disk takes nothing new,
/// a big empty one takes a lot.
/// Whether [`Hosting::replicate_groups`] commits each group to the catalog
/// (and announces it) as soon as it lands, rather than all at the end.
///
/// Yes for a first copy and for more groups of the version already held: a
/// large site takes hours to copy, and until a group is held it neither
/// counts toward the site's copies (so a publisher waiting on progress gives
/// up and stops serving) nor survives a restart. Only a new version replacing
/// an older one switches over at the end, since the catalog holds one
/// version of a site at a time.
fn holds_each_group_on_arrival(previous: Option<&SiteRecord>, rec: &SiteRecord) -> bool {
    previous.is_none_or(|p| p.manifest == rec.manifest)
}

pub fn auto_budget(held: u64, disk_free: u64, disk_total: u64) -> u64 {
    if disk_total == 0 {
        return held + 5 * GIB_BYTES;
    }
    held + disk_free.saturating_sub(free_disk_floor(disk_total)) / 100 * AUTO_BUDGET_PERCENT
}

/// A multiaddr string with a public IP over TCP.
/// An announced address as a dialable multiaddr for `pid`: `/p2p/<pid>` is
/// appended when missing, and an address naming another peer is refused.
fn dial_addr(a: &str, pid: c0mpute_net::PeerId) -> Option<c0mpute_net::Multiaddr> {
    let ma: c0mpute_net::Multiaddr = a.parse().ok()?;
    let mut named = None;
    for p in ma.iter() {
        if let c0mpute_net::Protocol::P2p(id) = p {
            named = Some(id);
        }
    }
    match named {
        Some(id) if id == pid => Some(ma),
        Some(_) => None,
        None => Some(ma.with(c0mpute_net::Protocol::P2p(pid))),
    }
}

fn public_tcp_addr(a: &str) -> bool {
    let Ok(ma) = a.parse::<c0mpute_net::Multiaddr>() else {
        return false;
    };
    let mut ip = false;
    let mut tcp = false;
    for p in ma.iter() {
        match p {
            c0mpute_net::Protocol::Ip4(v) => ip = crate::net_guard::is_public_ip(v.into()),
            c0mpute_net::Protocol::Ip6(v) => ip = crate::net_guard::is_public_ip(v.into()),
            c0mpute_net::Protocol::Tcp(_) => tcp = true,
            _ => {}
        }
    }
    ip && tcp
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
    let mut sites: Vec<serde_json::Value> = Vec::new();
    for (r, copies) in h.sites().await {
        // Striped sites: copies = the fewest copies of any group.
        let health = h.health(&r).await;
        let copies = health.as_ref().map(|x| x.min_copies).unwrap_or(copies);
        let mut v = serde_json::json!({
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
            });
        if let Some(x) = health {
            v["striped"] = serde_json::json!(true);
            v["groups"] = serde_json::json!(x.groups);
            v["short_groups"] = serde_json::json!(x.short);
            v["missing_groups"] = serde_json::json!(x.missing);
            let holders: Vec<serde_json::Value> = h
                .holder_breakdown(&r)
                .await
                .into_iter()
                .map(|(p, g, room)| serde_json::json!({ "peer": p, "groups": g, "room": room }))
                .collect();
            v["holders"] = serde_json::json!(holders);
        }
        sites.push(v);
    }
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
    serve_site(&h, &site, "", Some(""), uri.query(), &headers).await
}

async fn serve_path(
    State(h): State<Hosting>,
    AxPath((site, path)): AxPath<(String, String)>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    // The path as it was sent (still percent-encoded), for a redirect.
    let prefix = format!("/s/{site}/");
    serve_site(&h, &site, &path, uri.path().strip_prefix(&prefix), uri.query(), &headers).await
}

/// Serve `path` of a site under the shared `/s/<site>/` path. `rest`: the
/// path after `/s/<site>/` as the visitor sent it, when it could be told.
async fn serve_site(
    h: &Hosting,
    site: &str,
    path: &str,
    rest: Option<&str>,
    query: Option<&str>,
    headers: &HeaderMap,
) -> Response {
    // With per-site hosts on, a browser opening a page here is sent to the
    // site's own host, where the page has an origin of its own. Fetches
    // that are not navigations (podcast apps, feed readers, the CLI, the
    // `/p/` loader reading its ciphertext from this same origin) are
    // answered here as before: they run no page on this origin, and a
    // redirect to another host would break the ones that cannot follow it.
    if let (Some(suffix), Some(rest)) = (h.0.site_host_suffix.as_deref(), rest)
        && origin::wants_document(headers)
        && let Some(url) = c0mpute_gateway::site_host::site_url(site, suffix, rest, query)
    {
        return origin::redirect_to_site_host(&url);
    }
    let mut resp = match h.record_for(site).await {
        Some(rec) => {
            let resp = h.serve_with_hook(&rec, path, query, headers).await;
            let page = meta::public_origin(headers).map(|o| format!("{o}/s/{site}/{path}"));
            with_absolute_meta(resp, page.as_deref()).await
        }
        None => (StatusCode::NOT_FOUND, "no such site on the network (yet)").into_response(),
    };
    origin::apply(resp.headers_mut(), origin::Served::Shared, h.0.sandbox_shared);
    if h.0.site_host_suffix.is_some() {
        origin::vary_on_navigation(resp.headers_mut());
    }
    resp
}

/// In front of every hosting route: a request on a per-site host
/// (`<site_id>.<suffix>`) is that site and nothing else, so it never reaches
/// the routes behind (`/hosting/v1/...`, `/p/`, git, another site's `/s/`).
/// Any other request passes through.
///
/// The gateway's listener rewrites such a request to `/s/<site><path>` and
/// marks it before routing (`c0mpute_gateway::site_host::isolate`), which is
/// what keeps the other services' routes off these hosts; a router used on
/// its own (tests, the podcast listener) gets the request as it was sent.
/// Both are handled here.
async fn serve_site_host(State(h): State<Hosting>, req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    use c0mpute_gateway::site_host::{self, SiteHost};
    let Some(suffix) = h.0.site_host_suffix.as_deref() else {
        return next.run(req).await;
    };
    let (site, path) = match req.extensions().get::<SiteHost>() {
        Some(mark) => {
            let prefix = format!("/s/{}", mark.site);
            let path = req.uri().path().strip_prefix(&prefix).unwrap_or("/");
            (mark.site.clone(), path.to_string())
        }
        None => {
            let Some(site) =
                site_host::request_host(req.headers(), req.uri()).and_then(|x| site_host::site_of_host(&x, suffix))
            else {
                return next.run(req).await;
            };
            if req.method() != axum::http::Method::GET && req.method() != axum::http::Method::HEAD {
                return (StatusCode::NOT_FOUND, "not found").into_response();
            }
            (site, req.uri().path().to_string())
        }
    };
    let (headers, query) = (req.headers(), req.uri().query());
    let mut resp = match h.record_for(&site).await {
        Some(rec) => {
            let resp = h.serve_with_hook(&rec, &path, query, headers).await;
            let page = meta::public_origin(headers).map(|o| format!("{o}{path}"));
            with_absolute_meta(resp, page.as_deref()).await
        }
        None => (StatusCode::NOT_FOUND, "no such site on the network (yet)").into_response(),
    };
    origin::apply(resp.headers_mut(), origin::Served::SiteHost, h.0.sandbox_shared);
    resp
}

/// Largest HTML page whose social meta tags get rewritten.
const MAX_META_REWRITE_BYTES: usize = 8 * 1024 * 1024;
/// Largest file [`Hosting::read_file`] reads into memory.
const MAX_READ_FILE_BYTES: u64 = crate::git::MAX_REPO_BYTES;

/// Rewrite relative `og:image` / `twitter:image` / `og:url` tags of an HTML
/// response to absolute URLs (see [`meta`]); anything else passes through.
async fn with_absolute_meta(resp: Response, page_url: Option<&str>) -> Response {
    let Some(page) = page_url.and_then(|u| url::Url::parse(u).ok()) else {
        return resp;
    };
    // A byte range of a page cannot be rewritten (its length is promised).
    if resp.status() == StatusCode::PARTIAL_CONTENT {
        return resp;
    }
    let html = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.to_ascii_lowercase().starts_with("text/html"));
    if !html {
        return resp;
    }
    // Only a page of a known, small size is buffered; a bigger one (it was
    // streamed) passes through as it is.
    let len = resp
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if len.is_some_and(|n| n > MAX_META_REWRITE_BYTES as u64) {
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

/// Bytes admitted for a copy that is still being fetched (see [`Hosting::admit`]).
struct Reserved(Hosting, u64);

impl Drop for Reserved {
    fn drop(&mut self) {
        self.0.0.reserving.fetch_sub(self.1, Ordering::Relaxed);
    }
}

impl Hosting {
    async fn serve_with_hook(&self, rec: &SiteRecord, path: &str, query: Option<&str>, headers: &HeaderMap) -> Response {
        if let (Some(q), Some(hook)) = (query.filter(|q| !q.is_empty()), self.0.hook.get())
            && let Some(resp) = hook.serve(self, rec, path, q, headers).await
        {
            return resp;
        }
        self.serve_request(rec, path, headers).await
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
            let mut resp = with_absolute_meta(resp, page.as_deref()).await;
            origin::apply(resp.headers_mut(), origin::Served::Domain, h.0.sandbox_shared);
            return resp;
        }
    }
    (StatusCode::NOT_FOUND, "not found").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_relayed_through_the_challenger_is_not_a_pass() {
        assert!(answer_passes(true, false));
        assert!(!answer_passes(true, true), "the chunk was read from us while we asked for it");
        assert!(!answer_passes(false, false));
        assert!(!answer_passes(false, true));
    }

    #[tokio::test]
    async fn a_challenge_sees_the_chunk_read_from_this_node_meanwhile() {
        // What Hosting::holds_now does around its fetch: a relay that has
        // no copy can only answer by reading the chunk from a holder, and
        // when that holder is the challenger the read is seen.
        let dir = std::env::temp_dir().join(format!("c0mpute-relay-test-{}", rand::random::<u64>()));
        let store = ChunkStore::open(&dir).await.unwrap().with_min_free_bytes(0);
        let hash = store.put(b"the challenged chunk").await.unwrap();
        let serving = store.clone();
        // An honest holder answers from its own disk: nothing read here.
        let watch = store.watch_reads(&hash);
        assert!(answer_passes(true, watch.seen()));
        // A relay asks this node (through the chunk server's clone of the
        // store) for the very chunk it was challenged on.
        let relayed = serving.get(&hash).await.unwrap();
        assert!(!answer_passes(Hash::of(&relayed) == hash, watch.seen()));
        drop(watch);
        // The next challenge starts clean.
        assert!(answer_passes(true, store.watch_reads(&hash).seen()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unproven_holders_hold_their_slots_for_one_grace_and_no_longer() {
        let grace = Duration::from_secs(200);
        let t0 = Instant::now();
        let mut since = HashMap::new();
        // Not asked yet (the round's challenges were used up): no clock, no copy.
        assert!(!fill_clock(&mut since, "s", false, t0, grace));
        assert!(since.is_empty());
        // Asked: the clock starts, and the holders have the grace to answer.
        assert!(!fill_clock(&mut since, "s", true, t0, grace));
        assert!(!fill_clock(&mut since, "s", true, t0 + Duration::from_secs(199), grace));
        // A round that could not ask again (or new keys still to be asked)
        // does not restart it.
        assert!(!fill_clock(&mut since, "s", false, t0 + Duration::from_secs(150), grace));
        assert!(fill_clock(&mut since, "s", true, t0 + grace, grace));
        // And it stays due while the site is short, asked this round or not.
        assert!(fill_clock(&mut since, "s", false, t0 + grace * 2, grace));
        // Another site has its own clock.
        assert!(!fill_clock(&mut since, "t", true, t0 + grace * 2, grace));
    }

    #[test]
    fn a_round_of_proof_asks_is_bounded_per_round_per_peer_and_per_site() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let rec = |name: &str| SiteRecord::sign(&kp, name, &Hash::of(name.as_bytes()), 1, 10, vec![]).unwrap();
        let peers = |n: usize| -> Vec<String> { (0..n).map(|i| format!("p{i}")).collect() };
        // Per site: at most `most`, however many announce it.
        let mut asks = Asks::new(HashSet::new());
        assert_eq!(asks.take(&rec("a"), peers(40), PROOF_ASKS_PER_SITE), PROOF_ASKS_PER_SITE);
        assert_eq!(asks.list.len(), PROOF_ASKS_PER_SITE);
        // Per peer: one peer announcing many sites is asked about a few.
        let mut asks = Asks::new(HashSet::new());
        let asked: usize = (0..10).map(|i| asks.take(&rec(&format!("s{i}")), peers(1), 5)).sum();
        assert_eq!(asked, PROOF_ASKS_PER_PEER);
        // Per round.
        let mut asks = Asks::new(HashSet::new());
        for i in 0..100 {
            asks.take(&rec(&format!("s{i}")), vec![format!("q{i}")], 5);
        }
        assert_eq!(asks.list.len(), PROOF_ASKS_PER_ROUND);
        // A challenge still running counts as asked and is not started twice.
        let a = rec("a");
        let mut asks = Asks::new([(a.site_id.clone(), "p0".to_string())].into());
        assert_eq!(asks.take(&a, peers(1), 5), 1);
        assert!(asks.list.is_empty());
    }

    #[test]
    fn a_holding_part_past_its_own_count_does_not_grow_the_list() {
        assert!(holding_part_fits(0, 1, 0));
        assert!(holding_part_fits(2, 3, 2));
        assert!(!holding_part_fits(3, 3, 0), "part index past the count it states");
        assert!(!holding_part_fits(1, 1000, 1000), "more parts than it states");
        // The repeat-forever shape: part 1 of 3, again and again.
        assert!(holding_part_fits(1, 3, 2));
        assert!(!holding_part_fits(1, 3, 3));
    }

    #[test]
    fn dial_addr_names_the_peer_it_dials() {
        let pid = c0mpute_net::PeerId::random();
        let other = c0mpute_net::PeerId::random();
        let bare = dial_addr("/ip4/152.53.47.37/tcp/46343", pid).unwrap();
        assert_eq!(bare.to_string(), format!("/ip4/152.53.47.37/tcp/46343/p2p/{pid}"));
        let named = format!("/ip4/152.53.47.37/tcp/46343/p2p/{pid}");
        assert_eq!(dial_addr(&named, pid).unwrap().to_string(), named);
        assert!(dial_addr(&format!("/ip4/152.53.47.37/tcp/46343/p2p/{other}"), pid).is_none());
        assert!(dial_addr("not an address", pid).is_none());
    }

    #[test]
    fn copies_go_to_different_machines() {
        let disk = |p: &str| market::HostDisk {
            peer_id: p.into(),
            name: p.into(),
            price: 4000,
            free_bytes: 1 << 40,
            standing: None,
            countries: vec![],
            reachable: None,
            heartbeat_age_secs: None,
            live: true,
        };
        let ip = |s: &str| -> std::net::IpAddr { s.parse().unwrap() };
        let ips: HashMap<String, HashSet<std::net::IpAddr>> = [
            ("source-box", vec!["152.53.47.37"]),
            ("big-a", vec!["203.0.113.9"]),
            ("big-a-twin", vec!["203.0.113.9"]),
            ("other", vec!["198.51.100.7"]),
            ("behind-nat", vec![]),
        ]
        .into_iter()
        .map(|(p, v)| (p.to_string(), v.into_iter().map(ip).collect()))
        .collect();
        let ranked = ["source-box", "big-a", "big-a-twin", "other", "behind-nat"]
            .map(disk)
            .to_vec();
        let mine: HashSet<_> = [ip("152.53.47.37")].into();
        let picked: Vec<String> = one_per_machine(ranked, &ips, mine)
            .into_iter()
            .map(|d| d.peer_id)
            .collect();
        // Not the publisher's own box, and one of the twins only.
        assert_eq!(picked, ["big-a", "other", "behind-nat"]);
    }

    #[test]
    fn only_addresses_unique_to_one_machine_identify_it() {
        let ip = |s: &str| unique_ip(&s.parse::<c0mpute_net::Multiaddr>().unwrap());
        // Public and tailscale addresses name one machine.
        assert!(ip("/ip4/152.53.47.37/tcp/4001").is_some());
        assert!(ip("/ip4/100.90.185.25/tcp/4001").is_some());
        assert!(ip("/ip6/2a0a:4cc0::1/tcp/4001").is_some());
        // These repeat on every machine (docker bridges, LANs, loopback).
        for a in [
            "/ip4/127.0.0.1/tcp/4001",
            "/ip4/172.17.0.1/tcp/4001",
            "/ip4/192.168.1.5/tcp/4001",
            "/ip4/10.0.0.2/tcp/4001",
            "/ip4/169.254.1.1/tcp/4001",
            "/ip4/0.0.0.0/tcp/4001",
            "/ip6/::1/tcp/4001",
            "/ip6/fe80::1/tcp/4001",
            "/ip6/fd00::1/tcp/4001",
        ] {
            assert!(ip(a).is_none(), "{a}");
        }
    }

    #[test]
    fn a_first_striped_copy_is_held_group_by_group() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let v1 = SiteRecord::sign(&kp, "big", &Hash::of(b"v1"), 1, 1 << 35, vec![]).unwrap();
        let v1_again = SiteRecord::sign(&kp, "big", &Hash::of(b"v1"), 2, 1 << 35, vec![]).unwrap();
        let v2 = SiteRecord::sign(&kp, "big", &Hash::of(b"v2"), 3, 1 << 35, vec![]).unwrap();
        // Nothing held yet: each group counts as soon as it lands.
        assert!(holds_each_group_on_arrival(None, &v1));
        // More groups of the version held (or a re-publish of it): same.
        assert!(holds_each_group_on_arrival(Some(&v1), &v1));
        assert!(holds_each_group_on_arrival(Some(&v1), &v1_again));
        // A new version replaces the old one only once its groups are in.
        assert!(!holds_each_group_on_arrival(Some(&v1), &v2));
    }

    #[test]
    fn ranges_parse_like_browsers_send_them() {
        let n = 10 * 1024 * 1024;
        assert_eq!(parse_range("bytes=0-99", n), RangeSpec::Bytes(0, 99));
        assert_eq!(parse_range("bytes=100-", n), RangeSpec::Bytes(100, n - 1));
        assert_eq!(parse_range("bytes=-500", n), RangeSpec::Bytes(n - 500, n - 1));
        assert_eq!(parse_range("bytes=-99999999999", n), RangeSpec::Bytes(0, n - 1));
        assert_eq!(parse_range("bytes=5-99999999999", n), RangeSpec::Bytes(5, n - 1));
        assert_eq!(parse_range("bytes=0-0", n), RangeSpec::Bytes(0, 0));
        assert_eq!(parse_range(&format!("bytes={n}-"), n), RangeSpec::Unsatisfiable);
        assert_eq!(parse_range("bytes=-0", n), RangeSpec::Unsatisfiable);
        assert_eq!(parse_range("bytes=0-1,5-9", n), RangeSpec::Ignore);
        assert_eq!(parse_range("items=0-1", n), RangeSpec::Ignore);
        assert_eq!(parse_range("bytes=9-3", n), RangeSpec::Ignore);
        assert_eq!(parse_range("bytes=0-", 0), RangeSpec::Unsatisfiable);
    }

    #[test]
    fn chunk_slices_cover_a_range_exactly() {
        let cb = site::FILE_CHUNK_BYTES as u64;
        let size = 3 * cb + 17;
        let file: Vec<u8> = (0..size).map(|i| (i % 253) as u8).collect();
        let chunks: Vec<&[u8]> = file.chunks(cb as usize).collect();
        for (a, b) in [(0, 0), (0, cb - 1), (cb - 3, cb + 3), (5, size - 1), (2 * cb, 2 * cb + 1), (size - 1, size - 1)] {
            let mut out = Vec::new();
            for i in a / cb..=b / cb {
                out.extend_from_slice(slice_chunk(i, chunks[i as usize], a, b));
            }
            assert_eq!(out, file[a as usize..=b as usize].to_vec(), "{a}-{b}");
        }
    }

    #[test]
    fn auto_budget_follows_the_disk() {
        let g = GIB_BYTES;
        // 1.8 TiB disk with 57 GiB free: floor 50 GiB, its share of 7 GiB.
        assert_eq!(auto_budget(0, 57 * g, 1843 * g), 7 * g / 100 * AUTO_BUDGET_PERCENT);
        // Below the floor: nothing new.
        assert_eq!(auto_budget(3 * g, 40 * g, 1843 * g), 3 * g);
        // A big empty disk takes a lot.
        assert!(auto_budget(0, 3000 * g, 4000 * g) > 700 * g);
        // Unknown disk: the old 5 GiB.
        assert_eq!(auto_budget(0, 0, 0), 5 * g);
    }

    #[test]
    fn a_holding_message_of_striped_sites_fits_gossipsub() {
        // Entries are packed up to HOLDING_MESSAGE_BYTES; the largest bitmap
        // (every group of a 4 TiB site, MAX_GROUPS) still leaves room.
        let all: BTreeSet<u32> = (0..stripe::group_count(site::MAX_SITE_BYTES) as u32).collect();
        let g = stripe::encode_groups(&all);
        let per = HOLDING_MESSAGE_BYTES / (120 + g.len()) + 1;
        let sites: Vec<Held> = (0..per)
            .map(|i| Held {
                site_id: format!("{:020}", i),
                manifest: "f".repeat(64),
                seq: u64::MAX,
                groups: Some(g.clone()),
            })
            .collect();
        let bytes = serde_json::to_vec(&HostingMsg::Holding {
            sites,
            part: 0,
            parts: 1,
            hosts: true,
            addrs: vec!["/ip4/203.0.113.7/tcp/40001".into(); 8],
            price: Some("0.004".into()),
            payout: None,
            full: false,
            room_gib: Some(u64::MAX),
            room: Some(u64::MAX),
            machine: None,
        })
        .unwrap();
        assert!(bytes.len() < 64 * 1024, "{} bytes", bytes.len());
    }

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


    #[test]
    fn holding_messages_round_trip() {
        let m = HostingMsg::Holding {
            sites: vec![Held {
                site_id: "a".into(),
                manifest: "b".into(),
                seq: 1,
                groups: None,
            }],
            part: 0,
            parts: 1,
            hosts: true,
            addrs: vec![],
            price: Some("0.004".into()),
            payout: None,
            full: false,
            room_gib: Some(3000),
            room: None,
            machine: None,
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
                groups: None,
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
            room: Some(u64::MAX),
            machine: None,
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

/// An opaque id for the failure domain a node's copies live in: this OS
/// install (`/etc/machine-id`) and the filesystem the chunk store is on (its
/// fsid and size). Two nodes on one box and disk get the same id, and so
/// count as one copy; a hash, so it says nothing about the machine. `None`
/// when there is no machine id (many containers), and then nodes are told
/// apart by address.
pub fn failure_domain(store_root: &std::path::Path, disk_total: u64) -> Option<String> {
    let id = ["/etc/machine-id", "/var/lib/dbus/machine-id"]
        .iter()
        .find_map(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())?;
    let fsid = fs_id(store_root).unwrap_or(0);
    let h = Hash::of(format!("c0mpute-failure-domain\n{id}\n{fsid}\n{disk_total}").as_bytes());
    Some(h.to_hex()[..32].to_string())
}

#[cfg(unix)]
fn fs_id(path: &std::path::Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    // The store directory may not exist yet: walk up to one that does.
    let mut p = path;
    while !p.exists() {
        p = p.parent()?;
    }
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: c is a valid NUL-terminated path, st a valid out-pointer.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    #[allow(clippy::unnecessary_cast)]
    Some(st.f_fsid as u64)
}

#[cfg(not(unix))]
fn fs_id(_path: &std::path::Path) -> Option<u64> {
    None
}

/// The IP of `addr` when it names one machine only: not loopback, private,
/// link-local or unspecified (those repeat across machines, e.g. every
/// docker host has 172.17.0.1). Tailscale's 100.64/10 addresses are per
/// machine, so they count.
fn unique_ip(addr: &c0mpute_net::Multiaddr) -> Option<std::net::IpAddr> {
    use c0mpute_net::Protocol;
    addr.iter().find_map(|p| match p {
        Protocol::Ip4(ip) => {
            let local = ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast();
            (!local).then_some(std::net::IpAddr::V4(ip))
        }
        Protocol::Ip6(ip) => {
            let seg = ip.segments()[0];
            let local = ip.is_loopback()
                || ip.is_unspecified()
                || (seg & 0xfe00) == 0xfc00
                || (seg & 0xffc0) == 0xfe80;
            (!local).then_some(std::net::IpAddr::V6(ip))
        }
        _ => None,
    })
}

/// `ranked` with at most one disk per machine, in order, and none on a
/// machine in `taken` (addresses already holding a copy: the publisher's, a
/// kept host's). Disks with unknown addresses are kept: nothing says they
/// share a machine.
fn one_per_machine(
    ranked: Vec<market::HostDisk>,
    ips: &HashMap<String, HashSet<std::net::IpAddr>>,
    mut taken: HashSet<std::net::IpAddr>,
) -> Vec<market::HostDisk> {
    let mut out = Vec::new();
    for d in ranked {
        let mine = ips.get(&d.peer_id).cloned().unwrap_or_default();
        if !mine.is_disjoint(&taken) {
            eprintln!("marketplace: skipping {} ({}): same machine as a copy already placed", d.peer_id, d.name);
            continue;
        }
        taken.extend(mine);
        out.push(d);
    }
    out
}

/// The preferred hosts of `rec` that cannot make a copy now: not seen live
/// on the hosting gossip, or running on this (the publishing) machine.
async fn unusable_hosts(hosting: &Hosting, rec: &SiteRecord) -> Vec<String> {
    let live = hosting.wait_for_hosts(&rec.preferred_hosts, GOSSIP_CHECK).await;
    let here = hosting.on_this_machine(&rec.preferred_hosts).await;
    rec.preferred_hosts
        .iter()
        .filter(|p| !live.contains(p) || here.contains(*p))
        .cloned()
        .collect()
}

/// Terms that keep `keep` and fill the rest of `m.copies` with live d1sks
/// disks not in `avoid` and not on this machine. `None` when nothing can
/// replace them (the record then stays as it is).
async fn replacement_terms(
    m: &MarketOptions,
    size: u64,
    hosting: &Hosting,
    rec: &SiteRecord,
    keep: &[String],
    avoid: &[String],
) -> Option<site::HostingTerms> {
    let mut disks = market_disks(m, Some(hosting)).await.ok()?;
    disks.retain(|d| d.live && !keep.contains(&d.peer_id) && !avoid.contains(&d.peer_id));
    let mut ids: Vec<String> = disks.iter().map(|d| d.peer_id.clone()).collect();
    ids.extend(keep.iter().cloned());
    let ips = hosting.machine_ips(&ids).await;
    let mut taken = hosting.my_ips().await;
    for k in keep {
        taken.extend(ips.get(k).cloned().unwrap_or_default());
    }
    let want = m.copies.clamp(1, site::MAX_PREFERRED_HOSTS).saturating_sub(keep.len());
    let ranked = one_per_machine(market::rank(&disks, &m.query, size), &ips, taken);
    let (chosen, _) = market::choose(&ranked, want, None)?;
    let old_price = rec
        .price_per_gib_month
        .as_deref()
        .and_then(|p| crate::payments::parse_usd(p).ok())
        .unwrap_or(0);
    let price = chosen.iter().map(|d| d.price).max().unwrap_or(0).max(old_price);
    for d in &chosen {
        eprintln!(
            "  new preferred host {}  {}  ${}/GiB-month",
            d.peer_id,
            d.name,
            crate::payments::format_usd(d.price)
        );
    }
    let mut preferred_hosts = keep.to_vec();
    preferred_hosts.extend(chosen.into_iter().map(|d| d.peer_id));
    Some(site::HostingTerms {
        preferred_hosts,
        price_per_gib_month: price,
        payer: rec.payer.clone(),
    })
}

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
    let mut ranked = market::rank(&disks, &m.query, size);
    // One copy per machine, and none on this one (the source's own disk).
    if let Some(h) = hosting {
        let ids: Vec<String> = ranked.iter().map(|d| d.peer_id.clone()).collect();
        let ips = h.machine_ips(&ids).await;
        ranked = one_per_machine(ranked, &ips, h.my_ips().await);
    }
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

/// Prints a progress bar on one line of stderr (a plain line every few
/// seconds when stderr is not a terminal).
struct Progress {
    tty: bool,
    last: Instant,
    every: Duration,
}

impl Progress {
    fn new() -> Self {
        use std::io::IsTerminal;
        let tty = std::io::stderr().is_terminal();
        Self {
            tty,
            last: Instant::now() - Duration::from_secs(3600),
            every: if tty {
                Duration::from_millis(250)
            } else {
                Duration::from_secs(15)
            },
        }
    }

    fn show(&mut self, line: &str, force: bool) {
        if !force && self.last.elapsed() < self.every {
            return;
        }
        self.last = Instant::now();
        if self.tty {
            eprint!("\r\x1b[2K{line}");
        } else {
            eprintln!("{line}");
        }
    }

    fn done(&mut self, line: &str) {
        self.show(line, true);
        if self.tty {
            eprintln!();
        }
    }
}

fn bar(frac: f64) -> String {
    let n = (frac.clamp(0.0, 1.0) * 24.0).round() as usize;
    format!("[{}{}]", "#".repeat(n), ".".repeat(24 - n))
}

fn gib(b: u64) -> f64 {
    b as f64 / GIB_BYTES as f64
}

/// [`publish_dir`], optionally picking paid hosts from the d1sks marketplace
/// first and signing them into a v2 record.
///
/// The folder is scanned in place (nothing is copied; re-running reuses the
/// hashes of unchanged files) and served from the files while the network
/// takes copies. Running it again with the same folder resumes: the record
/// already on the network is reused, and holders keep what they have.
/// Large sites are striped; the wait ends once every group has
/// [`TARGET_REPLICAS`] copies, or after `wait` with no progress.
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
    // Before hashing anything: a big folder takes minutes to scan, and an
    // unreadable identity (created by `sudo c0mpute ...`) used to fail only
    // after the scan finished.
    publisher_keypair()?;
    let cache_path = crate::config::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("hosting")
        .join("scan-cache.json");
    let scanned = {
        let dir = dir.to_path_buf();
        let name = name.to_string();
        let cache_path = cache_path.clone();
        tokio::task::spawn_blocking(move || -> Result<site::ScannedSite> {
            let mut cache = site::ScanCache::load(&cache_path);
            let mut p = Progress::new();
            let started = Instant::now();
            let mut show = |done: u64, total: u64| {
                let secs = started.elapsed().as_secs_f64().max(0.001);
                p.show(
                    &format!(
                        "  hashing     {} {:>6.2}/{:.2} GiB  {:.0} MB/s",
                        bar(done as f64 / total.max(1) as f64),
                        gib(done),
                        gib(total),
                        done as f64 / secs / 1e6
                    ),
                    done == total,
                );
            };
            let r = site::scan_dir(&dir, &name, &mut cache, &mut show);
            cache.save(&cache_path);
            if r.is_ok() {
                eprintln!();
            }
            r
        })
        .await
        .context("scan task")??
    };
    let total = scanned.manifest.total_bytes();
    site::check_site_size(total, market.is_some())?;
    let striped = stripe::is_striped(total);
    let manifest_hash = scanned.manifest.stored().hash();
    let supervisor = crate::Supervisor::boot_buyer(crate::Config::default()).await?;
    let keypair = publisher_keypair()?;
    let owner = site::owner_hex(&keypair).context("hosting needs an ed25519 identity")?;
    let site_id = site::site_id_for(&owner, name);
    let seq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    // Join the hosting gossip first: picking marketplace hosts checks the
    // d1sks listing against which hosting nodes are live right now.
    let hosting = Hosting::start(
        supervisor.libp2p.clone(),
        supervisor.store.clone(),
        Options::default(),
    )
    .await?;
    // Let the swarm reach peers before the first gossip.
    tokio::time::sleep(Duration::from_secs(5)).await;
    // Resume: the same version already on the network is reused as it is,
    // so holders keep every group they have.
    let existing = hosting.lookup_record(&site_id).await.filter(|r| {
        r.manifest == manifest_hash.to_hex()
            && r.domains == domains
            && market.is_some() == !r.preferred_hosts.is_empty()
    });
    // Striped over more preferred hosts than the copy target, each holds
    // only its share: ask for disks with room for that.
    let share_for = |m: &MarketOptions| -> u64 {
        let n = m.copies.clamp(1, site::MAX_PREFERRED_HOSTS) as u64;
        let t = TARGET_REPLICAS as u64;
        if striped && n > t { total / n * t } else { total }
    };
    // Resuming with a preferred host that is gone (or on this very machine)
    // would wait forever for a copy nobody may make: replace those hosts,
    // keep the live ones (they keep what they hold: same content).
    let existing = match (existing, &market) {
        (Some(r), Some(m)) if !r.preferred_hosts.is_empty() => {
            let gone = unusable_hosts(&hosting, &r).await;
            if gone.is_empty() {
                Some(r)
            } else {
                eprintln!(
                    "preferred host(s) not usable (offline, or on this machine): {}",
                    gone.join(", ")
                );
                let keep: Vec<String> = r
                    .preferred_hosts
                    .iter()
                    .filter(|p| !gone.contains(p))
                    .cloned()
                    .collect();
                match replacement_terms(m, share_for(m), &hosting, &r, &keep, &gone).await {
                    Some(terms) => {
                        eprintln!("re-signing {name} with {} preferred host(s); content unchanged", terms.preferred_hosts.len());
                        Some(SiteRecord::sign_with_terms(
                            &keypair,
                            name,
                            &manifest_hash,
                            seq,
                            total,
                            domains.clone(),
                            Some(terms),
                        )?)
                    }
                    None => {
                        eprintln!("no live d1sks disk can replace them; keeping the current hosts");
                        Some(r)
                    }
                }
            }
        }
        (e, _) => e,
    };
    let record = match existing {
        Some(r) if r.seq == seq => r,
        Some(r) => {
            eprintln!(
                "resuming: the network already has this version of {name} (seq {})",
                r.seq
            );
            r
        }
        None => {
            let terms = match &market {
                Some(m) => pick_terms(m, share_for(m), Some(&hosting)).await,
                None => None,
            };
            if terms.is_none() && total > site::MAX_FREE_SITE_BYTES {
                bail!(
                    "no d1sks hosting disk has room for this site ({:.1} GiB each), and it is too big to publish free \
                     (at most {} GiB). Try `c0mpute hosting disks` to see what is listed, --copies 5 to spread it \
                     over more paid hosts, or split the folder into several sites.",
                    gib(total),
                    site::MAX_FREE_SITE_BYTES / GIB_BYTES
                );
            }
            SiteRecord::sign_with_terms(&keypair, name, &manifest_hash, seq, total, domains, terms)?
        }
    };
    hosting.publish_scanned(&scanned, record.clone()).await?;
    eprintln!(
        "published {} ({} files, {:.2} GiB{}) as site {}",
        name,
        scanned.manifest.files.len(),
        gib(record.size),
        if striped {
            let n = stripe::group_count(record.size) as u64;
            format!(", striped in {n} groups of ~{} MiB", record.size.div_ceil(n) >> 20)
        } else {
            String::new()
        },
        record.site_id
    );
    eprintln!(
        "  keep this running until the network holds copies; if it stops, run the same command again to resume"
    );
    let copies = if striped {
        wait_striped(&hosting, &record, &scanned.manifest, wait).await
    } else {
        wait_whole(&hosting, &record, wait).await
    };
    Ok((record, copies))
}

/// What [`download`] did.
pub struct Download {
    pub bytes: u64,
    pub took: Duration,
    /// Bytes per source peer.
    pub by_peer: Vec<(String, u64)>,
}

/// `c0mpute hosting get`: fetch one file of a site from the network into
/// `out`, the way a gateway does. `sequential` fetches one chunk at a time
/// from the first source that answers (how every release up to 0.2.99
/// replicated and proxied), for comparison. `from` limits the sources to
/// these peers.
pub async fn download(
    site_id: &str,
    path: &str,
    out: &std::path::Path,
    sequential: bool,
    from: Vec<String>,
) -> Result<Download> {
    use futures::StreamExt;
    use std::io::Write;
    let supervisor = crate::Supervisor::boot_buyer(crate::Config::default()).await?;
    let hosting = Hosting::start(
        supervisor.libp2p.clone(),
        supervisor.store.clone(),
        Options::default(),
    )
    .await?;
    // `--from` takes peer ids or full multiaddrs (`/ip4/../tcp/../p2p/<id>`):
    // dial the addresses, keep the ids.
    let mut from_ids = Vec::new();
    for f in &from {
        match f.parse::<c0mpute_net::Multiaddr>() {
            Ok(ma) => {
                if let Some(id) = ma.iter().find_map(|p| match p {
                    c0mpute_net::Protocol::P2p(id) => Some(id.to_base58()),
                    _ => None,
                }) {
                    from_ids.push(id);
                }
                let _ = hosting.0.net.dial(ma).await;
            }
            Err(_) => from_ids.push(f.clone()),
        }
    }
    let from = from_ids;
    for p in &from {
        if let Ok(pid) = p.parse::<c0mpute_net::PeerId>() {
            let _ = hosting.0.net.connect(pid).await;
        }
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    let started = Instant::now();
    let mut rec = None;
    while started.elapsed() < Duration::from_secs(90) {
        // Ask the named sources first: they certainly know the record.
        for p in &from {
            let Ok(pid) = p.parse::<c0mpute_net::PeerId>() else {
                continue;
            };
            if let Ok(Ok(body)) = tokio::time::timeout(
                Duration::from_secs(5),
                hosting.0.net.rpc(pid, RPC_RECORD, site_id.as_bytes().to_vec()),
            )
            .await
                && let Ok(r) = serde_json::from_slice::<SiteRecord>(&body)
                && r.site_id == site_id
            {
                hosting.0.map.write().await.apply_record(r);
            }
        }
        if let Some(r) = hosting.lookup_record(site_id).await {
            rec = Some(r);
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    let rec = rec.with_context(|| format!("no record for site {site_id} on the network"))?;
    // Named sources serve the whole site whatever the gossip has said so far.
    for p in &from {
        let mut sites = HashMap::new();
        sites.insert(rec.site_id.clone(), (rec.manifest.clone(), rec.seq));
        hosting.0.map.write().await.apply_holding_full(
            p,
            sites,
            HashMap::new(),
            Instant::now(),
            false,
            None,
            None,
            None,
            Vec::new(),
        );
    }
    // Wait for holders to announce themselves (once a minute).
    let mut sources = Vec::new();
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(75) {
        sources = hosting.0.map.read().await.sources(&rec.site_id);
        sources.extend(hosting.holders(&rec.site_id).await);
        sources.sort();
        sources.dedup();
        if !from.is_empty() {
            sources.retain(|p| from.contains(p));
        }
        if !sources.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    if sources.is_empty() {
        bail!("no live holder of {site_id} announced itself");
    }
    for p in &sources {
        if let Ok(pid) = p.parse::<c0mpute_net::PeerId>() {
            let _ = hosting.0.net.connect(pid).await;
        }
    }
    let manifest = hosting.manifest(&rec).await?;
    let entry = hosting
        .resolve_entry(&rec, &manifest, path)
        .await?
        .with_context(|| format!("{path} is not in site {site_id}"))?;
    let chunk_sources = Arc::new(hosting.chunk_sources(&rec).await);
    let only: Option<Vec<String>> = (!from.is_empty()).then(|| from.clone());
    let pick = move |src: &ChunkSources, h: &Hash| -> Vec<String> {
        let v = src.for_chunk(h).to_vec();
        match &only {
            Some(o) => v.into_iter().filter(|p| o.contains(p)).collect(),
            None => v,
        }
    };
    let hashes: Vec<Hash> = entry
        .chunks
        .iter()
        .filter_map(|h| Hash::from_hex(h).ok())
        .collect();
    let mut f = std::fs::File::create(out).with_context(|| format!("create {}", out.display()))?;
    let mut p = Progress::new();
    let t0 = Instant::now();
    let mut done = 0u64;
    let mut by_peer: HashMap<String, u64> = HashMap::new();
    let report = |done: u64, p: &mut Progress, force: bool| {
        let secs = t0.elapsed().as_secs_f64().max(0.001);
        p.show(
            &format!(
                "  downloading {} {:.1}/{:.1} MiB  {:.1} MB/s",
                bar(done as f64 / entry.size.max(1) as f64),
                done as f64 / 1048576.0,
                entry.size as f64 / 1048576.0,
                done as f64 / secs / 1e6
            ),
            force,
        );
    };
    if sequential {
        for h in &hashes {
            let req = ChunkRequest {
                chunk_hash: *h,
                shard_index: None,
            };
            let mut got = None;
            for peer in pick(&chunk_sources, h) {
                let Ok(pid) = peer.parse::<c0mpute_net::PeerId>() else {
                    continue;
                };
                if let Ok(Ok(b)) = tokio::time::timeout(
                    Duration::from_secs(20),
                    hosting.0.net.fetch_from_peer(pid, &req),
                )
                .await
                    && Hash::of(&b) == *h
                {
                    *by_peer.entry(peer).or_default() += b.len() as u64;
                    got = Some(b);
                    break;
                }
            }
            let b = got.with_context(|| format!("chunk {} unavailable", h.to_hex()))?;
            f.write_all(&b)?;
            done += b.len() as u64;
            report(done, &mut p, false);
        }
    } else {
        let h2 = hosting.clone();
        let src2 = chunk_sources.clone();
        let mut stream = futures::stream::iter(hashes.clone())
            .map(move |h| {
                let hosting = h2.clone();
                let srcs = pick(&src2, &h);
                async move { hosting.fetch_chunk(&h, &srcs).await }
            })
            .buffered(fetch::BULK_WINDOW);
        while let Some(r) = stream.next().await {
            let b = r?;
            f.write_all(&b)?;
            done += b.len() as u64;
            report(done, &mut p, false);
        }
        by_peer = hosting.fetched_by_peer().into_iter().collect();
    }
    report(done, &mut p, true);
    p.done("");
    let mut by_peer: Vec<(String, u64)> = by_peer.into_iter().collect();
    by_peer.sort_by(|a, b| b.1.cmp(&a.1));
    Ok(Download {
        bytes: done,
        took: t0.elapsed(),
        by_peer,
    })
}

/// `c0mpute hosting rm`: replace a site with an empty version (no files).
/// Holders switch to it and delete the old chunks; gateways answer 404. The
/// record stays (a few hundred bytes), so the name can be published again.
/// Works on every node version: it is a plain v1 record.
pub async fn unpublish(name: &str, wait: Duration) -> Result<SiteRecord> {
    let manifest = SiteManifest {
        v: 1,
        name: name.to_string(),
        files: std::collections::BTreeMap::new(),
    };
    let scanned = site::ScannedSite {
        manifest,
        locations: HashMap::new(),
    };
    let manifest_hash = scanned.manifest.stored().hash();
    let supervisor = crate::Supervisor::boot_buyer(crate::Config::default()).await?;
    let keypair = publisher_keypair()?;
    let seq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let record = SiteRecord::sign(&keypair, name, &manifest_hash, seq, 0, vec![])?;
    let hosting = Hosting::start(
        supervisor.libp2p.clone(),
        supervisor.store.clone(),
        Options::default(),
    )
    .await?;
    tokio::time::sleep(Duration::from_secs(5)).await;
    hosting.publish_scanned(&scanned, record.clone()).await?;
    eprintln!("site {} ({name}) is now empty; telling the network", record.site_id);
    wait_whole(&hosting, &record, wait).await;
    Ok(record)
}

/// Wait for a whole (small) site to reach its copies, healing as we go:
/// re-announce on a Fibonacci schedule (1, 1, 2, 3, 5, ... s, capped at
/// [`REANNOUNCE_CAP`]) that starts over whenever a new copy shows up, so a
/// stalled publish keeps knocking without hammering the mesh.
async fn wait_whole(hosting: &Hosting, record: &SiteRecord, wait: Duration) -> usize {
    let started = Instant::now();
    let mut copies = 0;
    let mut schedule = crate::backoff::Fibonacci::new(REANNOUNCE_CAP);
    let mut next_announce = started + schedule.next_delay();
    while started.elapsed() < wait {
        let left = wait.saturating_sub(started.elapsed());
        tokio::time::sleep(COPY_POLL.min(left)).await;
        // Copies that proved themselves, not announcements (anyone can
        // announce): this is what decides when the publisher may leave.
        let holders = hosting.verified_holders(record).await;
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
            hosting.reannounce(record).await;
            next_announce = Instant::now() + schedule.next_delay();
        }
    }
    copies
}

/// Wait for every group of a striped site to reach its copies, showing
/// progress. Gives up after `wait` without progress. Returns the fewest
/// copies of any group.
async fn wait_striped(
    hosting: &Hosting,
    record: &SiteRecord,
    manifest: &SiteManifest,
    wait: Duration,
) -> usize {
    let layout = stripe::Layout::of(manifest);
    let want = record.size.saturating_mul(TARGET_REPLICAS as u64).max(1);
    let mut p = Progress::new();
    let mut schedule = crate::backoff::Fibonacci::new(REANNOUNCE_CAP);
    let mut next_announce = Instant::now() + schedule.next_delay();
    let mut last_progress = Instant::now();
    let mut best = 0u64;
    let mut rate = 0f64;
    let mut prev = (Instant::now(), 0u64);
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if Instant::now() >= next_announce {
            hosting.reannounce(record).await;
            next_announce = Instant::now() + schedule.next_delay();
        }
        let holders = hosting
            .0
            .map
            .read()
            .await
            .distinct_group_holders(&record.site_id, layout.len());
        let placed: u64 = holders
            .iter()
            .enumerate()
            .map(|(g, hs)| hs.len().min(TARGET_REPLICAS) as u64 * layout.bytes(g as u32))
            .sum();
        let h = stripe::health(&holders, TARGET_REPLICAS);
        let dt = prev.0.elapsed().as_secs_f64();
        if dt >= 2.0 {
            let r = placed.saturating_sub(prev.1) as f64 / dt;
            rate = if rate == 0.0 { r } else { rate * 0.7 + r * 0.3 };
            prev = (Instant::now(), placed);
        }
        if placed > best {
            best = placed;
            last_progress = Instant::now();
            schedule.reset();
        }
        let line = format!(
            "  replicating {} {:>5.1}%  {:.2}/{:.2} GiB of {} copies, groups {}/{} complete, {} without a copy, {:.1} MB/s",
            bar(placed as f64 / want as f64),
            placed as f64 * 100.0 / want as f64,
            gib(placed),
            gib(want),
            TARGET_REPLICAS,
            h.groups - h.short,
            h.groups,
            h.missing,
            rate / 1e6
        );
        if h.short == 0 && h.groups > 0 {
            p.done(&line);
            return h.min_copies;
        }
        p.show(&line, false);
        if last_progress.elapsed() >= wait {
            if h.missing > 0 {
                // This publisher is the only source of those groups: leaving
                // now would lose them. Keep seeding (and say why) until some
                // host holds every group.
                if last_progress.elapsed().as_secs() % 600 < 2 {
                    p.done(&line);
                    eprintln!(
                        "  {} group(s) are held by no host yet: staying up as their only source.",
                        h.missing
                    );
                }
                continue;
            }
            p.done(&line);
            let still = want.saturating_sub(placed);
            let (room, nodes) = {
                let map = hosting.0.map.read().await;
                other_room(&map, &hosting.0.me)
            };
            eprintln!(
                "  no progress for {} s: stopping. Every group is held by at least one host, so the \
                 network keeps filling the missing copies on its own as room appears; nothing here \
                 needs to stay running.",
                wait.as_secs()
            );
            if room < still {
                eprintln!(
                    "  short of disk: {:.0} GiB of copies still to place, hosting nodes on other \
                     machines announce {:.0} GiB free ({nodes} node(s)). Add a hosting node with \
                     room (`c0mpute worker start` on another machine) and the copies follow.",
                    gib(still),
                    gib(room)
                );
            } else {
                eprintln!(
                    "  {:.0} GiB of copies still to place and {:.0} GiB free on {nodes} hosting node(s) \
                     elsewhere: some of them are not fetching (unreachable or relayed only). Run the \
                     same command to resume and watch.",
                    gib(still),
                    gib(room)
                );
            }
            return h.min_copies;
        }
    }
}

/// Room (bytes) on hosting nodes that place striped sites, one node per
/// machine and none on `me`'s machine, and how many such nodes there are.
fn other_room(map: &replicas::ReplicaMap, me: &str) -> (u64, usize) {
    let machines = map.machines();
    let peers: Vec<String> = map
        .striping_peers()
        .into_iter()
        .filter(|p| !replicas::same_machine(&machines, p, me))
        .collect();
    // Per machine, its biggest announced room (two nodes on one disk show
    // the same free space; adding both would count it twice).
    let mut seen: Vec<(String, u64)> = Vec::new();
    for p in peers {
        let r = map.room_of(&p).unwrap_or(0);
        match seen.iter_mut().find(|(q, _)| replicas::same_machine(&machines, q, &p)) {
            Some(e) => e.1 = e.1.max(r),
            None => seen.push((p, r)),
        }
    }
    (seen.iter().map(|(_, r)| *r).sum(), seen.len())
}
