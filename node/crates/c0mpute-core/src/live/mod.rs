//! Live streaming: p2p HLS with BitTorrent-style segment swarming.
//!
//! * A broadcaster (`c0mpute stream go`) runs ffmpeg into HLS, puts every new
//!   segment in its chunk store and gossips a signed [`record::LiveRecord`] on
//!   [`TOPIC`]: the latest playlist window as segment hashes + durations +
//!   media sequence. Each record is a few KiB, well under gossipsub's 64 KiB.
//! * Every node with the `live-stream` role subscribes, verifies records and
//!   (on a gateway) serves `/live/<stream_id>/index.m3u8` built from the newest
//!   verified record, and `/live/<stream_id>/<segment_hash>.ts`.
//! * A segment a relay does not hold is fetched over libp2p, chunk by chunk,
//!   from any peer that says it holds it (`Have` gossip), unmeasured and
//!   fastest peers first, the broadcaster last among equals, failing over to
//!   the next. A relay that fetched a segment holds it for the live window and
//!   answers chunk requests for it, so load spreads across the swarm instead
//!   of landing on the broadcaster. Segments that leave the window are evicted.
//! * Streams with active viewers get their newest segments prefetched as each
//!   record arrives, so playback does not wait on the first fetch.
//!
//! * Every node counts the bytes it serves per stream (HTTP to viewers,
//!   chunks to other nodes) and gossips a signed receipt per stream every
//!   minute; status aggregators turn those into `live.relay` jobs credited
//!   to the node ([`meter`]). An account-bound stream that is not paid is
//!   served to at most its viewer cap of clients per gateway; a stream with
//!   no binding (`c0mpute stream go`) has no cap.
//!
//! No coordinator: records are self-certifying, holders are learned from
//! gossip, and every byte is checked against its blake3 hash.

pub mod broadcast;
pub mod ingest;
pub mod meter;
pub mod playlist;
pub mod record;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Path as AxPath, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use c0mpute_net::{GossipMessage, Libp2pNetwork};
use c0mpute_proto::{ChunkRequest, Hash};
use c0mpute_store::ChunkStore;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock, Semaphore};
use tracing::{debug, info, warn};

use record::{LiveRecord, Segment};

pub const TOPIC: &str = "c0mpute/live/v1";
pub const ROLE_TAG: &str = "c0mpute:role:live-stream";
/// Most segment hashes in one `Have` message.
const MAX_HAVE: usize = record::MAX_RENDITIONS * record::MAX_WINDOW * 2;
/// Round-trip assumed for the broadcaster on top of what was measured, so
/// relays that already hold a segment are asked first and the origin's
/// upload is saved for peers nobody else can serve.
const ORIGIN_PENALTY_MS: f64 = 50.0;
/// Round-trip recorded for a peer that failed a fetch.
const FAILED_RTT_MS: f64 = 5_000.0;

/// How often things happen. Tests shrink these.
#[derive(Clone, Debug)]
pub struct Timing {
    /// Re-announce held segments this often.
    pub have_every: Duration,
    /// A holder not heard from in this long stops counting.
    pub holder_ttl: Duration,
    pub fetch_timeout: Duration,
    pub evict_every: Duration,
    /// Keep a segment this long after it leaves the window (players that are
    /// a little behind still find it).
    pub grace: Duration,
    /// Forget a stream with no new record for this long.
    pub stream_ttl: Duration,
    /// A rendition with a playlist request within this long has viewers.
    pub viewer_ttl: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            have_every: Duration::from_secs(10),
            holder_ttl: Duration::from_secs(30),
            fetch_timeout: Duration::from_secs(10),
            evict_every: Duration::from_secs(2),
            grace: Duration::from_secs(30),
            stream_ttl: Duration::from_secs(120),
            viewer_ttl: Duration::from_secs(30),
        }
    }
}

/// Gossip on [`TOPIC`].
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum LiveMsg {
    /// The broadcaster's newest signed window.
    Record { record: LiveRecord },
    /// "I hold these segments of this stream" (the full list; replaces what
    /// the sender said before). `addrs` let peers dial the sender.
    Have {
        stream_id: String,
        segs: Vec<String>,
        #[serde(default)]
        addrs: Vec<String>,
    },
}

/// A segment a node knows about, and when a record last listed it.
#[derive(Clone, Debug)]
pub struct Known {
    pub seg: Segment,
    pub rendition: String,
    pub last_ref: Instant,
}

/// Drop segments that are not in the current window (`current`) and that no
/// record has listed within `grace`. Returns them. The current window is
/// never evicted, however long the broadcaster goes quiet; a stream that
/// stays quiet is forgotten whole after `Timing::stream_ttl`.
pub fn evict(
    segs: &mut HashMap<String, Known>,
    current: &HashSet<&str>,
    now: Instant,
    grace: Duration,
) -> Vec<Known> {
    let gone: Vec<String> = segs
        .iter()
        .filter(|(h, k)| !current.contains(h.as_str()) && now.duration_since(k.last_ref) > grace)
        .map(|(h, _)| h.clone())
        .collect();
    gone.into_iter().filter_map(|h| segs.remove(&h)).collect()
}

/// The order to ask holders in. `cands` is (peer, measured rtt ms if any,
/// is_origin). Unmeasured relays go first (optimistic: they get a chance to
/// prove themselves), the origin pays [`ORIGIN_PENALTY_MS`], and peers in the
/// same 25 ms band take turns by `turn`.
pub fn source_order(cands: &[(String, Option<f64>, bool)], turn: usize) -> Vec<String> {
    let mut v: Vec<(u64, String)> = cands
        .iter()
        .map(|(p, rtt, origin)| {
            let ms = rtt.unwrap_or(0.0) + if *origin { ORIGIN_PENALTY_MS } else { 0.0 };
            ((ms / 25.0).floor() as u64, p.clone())
        })
        .collect();
    v.sort();
    v.dedup_by(|a, b| a.1 == b.1);
    let mut out = Vec::with_capacity(v.len());
    let mut i = 0;
    while i < v.len() {
        let mut j = i;
        while j < v.len() && v[j].0 == v[i].0 {
            j += 1;
        }
        let group: Vec<String> = v[i..j].iter().map(|(_, p)| p.clone()).collect();
        let r = turn % group.len();
        out.extend(group[r..].iter().cloned());
        out.extend(group[..r].iter().cloned());
        i = j;
    }
    out
}

struct Stream {
    record: LiveRecord,
    /// The peer that published the record (the broadcaster).
    origin: Option<String>,
    last_record: Instant,
    segs: HashMap<String, Known>,
    /// Segments complete in the local store.
    held: HashSet<String>,
    /// peer -> (segments it holds, when it said so)
    holders: HashMap<String, (HashSet<String>, Instant)>,
    /// rendition -> last playlist request
    viewers: HashMap<String, Instant>,
    /// Chunks fetched, per peer they came from.
    fetched_from: HashMap<String, u64>,
}

impl Stream {
    fn new(record: LiveRecord, origin: Option<String>, now: Instant) -> Self {
        let mut s = Stream {
            record: record.clone(),
            origin,
            last_record: now,
            segs: HashMap::new(),
            held: HashSet::new(),
            holders: HashMap::new(),
            viewers: HashMap::new(),
            fetched_from: HashMap::new(),
        };
        s.reference(&record, now);
        s
    }

    fn reference(&mut self, record: &LiveRecord, now: Instant) {
        for r in &record.body.renditions {
            for seg in &r.segments {
                self.segs.insert(
                    seg.hash.clone(),
                    Known {
                        seg: seg.clone(),
                        rendition: r.name.clone(),
                        last_ref: now,
                    },
                );
            }
        }
    }
}

struct Inner {
    net: Arc<Libp2pNetwork>,
    store: ChunkStore,
    me: String,
    timing: Timing,
    streams: RwLock<HashMap<String, Stream>>,
    rtt_ms: RwLock<HashMap<String, f64>>,
    /// Chunks this service wrote to the store (only these are deleted on
    /// eviction, so a chunk something else also stored is never removed).
    owned: Mutex<HashSet<String>>,
    /// One fetch per segment at a time, however many viewers ask.
    inflight: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    turn: AtomicUsize,
    stopped: AtomicBool,
    fetch_slots: Semaphore,
    /// Bytes served per stream since the last receipt.
    served: meter::Served,
    /// Highest whole-stream bitrate seen per stream (bounds receipts).
    peak_bps: std::sync::Mutex<HashMap<String, u64>>,
    /// stream -> client -> last request, for streams with a viewer cap.
    clients: std::sync::Mutex<HashMap<String, HashMap<String, Instant>>>,
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Who a request is from, for the viewer cap: the connecting address, or,
/// when that is a local or private proxy (or unknown), the client address
/// the proxy passed on (`cf-connecting-ip`, `x-real-ip`, the first
/// `x-forwarded-for` hop).
pub fn client_key(headers: &HeaderMap, direct: Option<std::net::IpAddr>) -> String {
    let local = |ip: &std::net::IpAddr| match ip {
        std::net::IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        std::net::IpAddr::V6(v6) => v6.is_loopback() || (v6.segments()[0] & 0xfe00) == 0xfc00,
    };
    if let Some(ip) = &direct
        && !local(ip)
    {
        return ip.to_string();
    }
    for name in ["cf-connecting-ip", "x-real-ip", "x-forwarded-for"] {
        if let Some(v) = headers.get(name).and_then(|v| v.to_str().ok())
            && let Some(first) = v.split(',').next().map(str::trim).filter(|s| !s.is_empty() && s.len() <= 64)
        {
            return first.to_string();
        }
    }
    direct.map(|ip| ip.to_string()).unwrap_or_else(|| "unknown".to_string())
}

fn request_client(req: &Request) -> String {
    let direct = req.extensions().get::<ConnectInfo<std::net::SocketAddr>>().map(|c| c.0.ip());
    client_key(req.headers(), direct)
}

/// The live-stream service: one per node. Cheap to clone.
#[derive(Clone)]
pub struct LiveStream(Arc<Inner>);

/// Per-stream numbers for listings and tests.
#[derive(Clone, Debug, Serialize)]
pub struct StreamInfo {
    pub stream_id: String,
    pub name: String,
    pub owner: String,
    pub seq: u64,
    pub ended: bool,
    pub origin: Option<String>,
    pub age_ms: u64,
    pub held: usize,
    pub holders: usize,
    pub fetched_from: HashMap<String, u64>,
    pub renditions: Vec<serde_json::Value>,
    /// The c0mpute.com account the stream is bound to, when it is.
    pub account: Option<String>,
    /// Relays of this stream are paid per GB.
    pub paid: bool,
    /// Viewers per gateway (`None`: no cap, a paid stream).
    pub max_viewers: Option<u32>,
}

impl LiveStream {
    /// Subscribe to [`TOPIC`] and start the gossip, announce and eviction loops.
    pub async fn start(net: Arc<Libp2pNetwork>, store: ChunkStore, timing: Timing) -> Result<Self> {
        let me = net.peer_id().to_base58();
        let ls = LiveStream(Arc::new(Inner {
            net: net.clone(),
            store,
            me,
            timing,
            streams: RwLock::new(HashMap::new()),
            rtt_ms: RwLock::new(HashMap::new()),
            owned: Mutex::new(HashSet::new()),
            inflight: Mutex::new(HashMap::new()),
            turn: AtomicUsize::new(0),
            stopped: AtomicBool::new(false),
            fetch_slots: Semaphore::new(16),
            served: meter::Served::default(),
            peak_bps: std::sync::Mutex::new(HashMap::new()),
            clients: std::sync::Mutex::new(HashMap::new()),
        }));
        net.subscribe(TOPIC).await.context("subscribe to live topic")?;
        // Relay receipts travel on their own topic; being in its mesh lets
        // them reach an aggregator this node is not directly connected to.
        if let Err(e) = net.subscribe(&meter::topic()).await {
            debug!(err = %e, "live: receipt topic subscribe failed");
        }
        ls.spawn_gossip_reader();
        ls.spawn_loops();
        ls.spawn_metering();
        Ok(ls)
    }

    // ── metering ─────────────────────────────────────────────────────────

    /// Credit chunks this node serves over libp2p to the live streams they
    /// belong to. Call once, with the tap the node's chunk source reports to.
    pub fn attach_tap(&self, tap: &meter::Tap) {
        let Some(mut rx) = tap.attach() else { return };
        let ls = self.clone();
        tokio::spawn(async move {
            while let Some((hash, len)) = rx.recv().await {
                if ls.is_stopped() {
                    return;
                }
                ls.count_chunk(&hash.to_hex(), len).await;
            }
        });
    }

    /// `bytes` of `stream_id` left this node (to a viewer or another node).
    pub fn count_served(&self, stream_id: &str, bytes: u64) {
        self.0.served.add(stream_id, bytes);
    }

    /// A chunk left this node over libp2p: credit the live stream it is
    /// part of, if any. Returns the stream.
    pub async fn count_chunk(&self, chunk_hex: &str, bytes: u64) -> Option<String> {
        let id = {
            let streams = self.0.streams.read().await;
            streams
                .iter()
                .find(|(_, s)| s.segs.values().any(|k| k.seg.chunks.iter().any(|c| c == chunk_hex)))
                .map(|(id, _)| id.clone())
        }?;
        self.0.served.add(&id, bytes);
        Some(id)
    }

    /// Bytes of `stream_id` served since the last receipt.
    pub fn served_bytes(&self, stream_id: &str) -> u64 {
        self.0.served.peek(stream_id)
    }

    /// The most bytes this node may claim for each stream in one epoch.
    async fn receipt_bounds(&self) -> HashMap<String, u64> {
        let streams = self.0.streams.read().await;
        let peaks = self.0.peak_bps.lock().unwrap_or_else(|e| e.into_inner());
        streams
            .iter()
            .map(|(id, s)| {
                let bps = peaks.get(id).copied().unwrap_or(0).max(meter::bitrate_bps(&s.record));
                (id.clone(), meter::max_epoch_bytes(bps, meter::viewer_bound(&s.record)))
            })
            .collect()
    }

    /// Sign and gossip one receipt per stream served since the last call,
    /// for `epoch`. Returns what was signed.
    pub async fn publish_receipts(&self, epoch: u64) -> Vec<meter::RelayReceipt> {
        let served = self.0.served.take();
        if served.is_empty() {
            return Vec::new();
        }
        let bounds = self.receipt_bounds().await;
        let receipts = meter::receipts_for(&self.0.net, served, &bounds, epoch);
        let topic = meter::topic();
        for r in &receipts {
            if let Ok(bytes) = serde_json::to_vec(r)
                && let Err(e) = self.0.net.publish(&topic, bytes).await
            {
                debug!(err = %e, stream = %r.stream_id, "live: receipt not published (no aggregator in reach?)");
            }
        }
        receipts
    }

    fn spawn_metering(&self) {
        let ls = self.clone();
        tokio::spawn(async move {
            loop {
                let now = unix_ms();
                let epoch = meter::epoch_of(now);
                let next = (epoch + 1) * meter::EPOCH_SECS * 1000;
                tokio::time::sleep(Duration::from_millis(next.saturating_sub(now) + 50)).await;
                if ls.is_stopped() {
                    return;
                }
                let sent = ls.publish_receipts(epoch).await;
                if !sent.is_empty() {
                    debug!(epoch, receipts = sent.len(), "live: relay receipts sent");
                }
            }
        });
    }

    /// May `client` watch `stream_id` here? Always for a paid stream and for
    /// a stream with no account binding. An account-bound stream that is
    /// not paid has a viewer cap per gateway: a client seen
    /// within `viewer_ttl` keeps its place, a new one gets in while fewer
    /// than the cap are active.
    pub async fn admit_viewer(&self, stream_id: &str, client: &str) -> bool {
        let Some(cap) = self.record(stream_id).await.and_then(|r| r.viewer_cap()) else {
            return true;
        };
        let now = Instant::now();
        let ttl = self.0.timing.viewer_ttl;
        let mut clients = self.0.clients.lock().unwrap_or_else(|e| e.into_inner());
        let set = clients.entry(stream_id.to_string()).or_default();
        set.retain(|_, seen| now.duration_since(*seen) <= ttl);
        if set.contains_key(client) || set.len() < cap as usize {
            set.insert(client.to_string(), now);
            true
        } else {
            false
        }
    }

    /// Clients counted against `stream_id`'s viewer cap right now.
    pub fn active_viewers(&self, stream_id: &str) -> usize {
        let now = Instant::now();
        let ttl = self.0.timing.viewer_ttl;
        self.0
            .clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(stream_id)
            .map_or(0, |m| m.values().filter(|seen| now.duration_since(**seen) <= ttl).count())
    }

    pub fn peer_id(&self) -> &str {
        &self.0.me
    }

    pub fn stop(&self) {
        self.0.stopped.store(true, Ordering::Relaxed);
    }

    fn is_stopped(&self) -> bool {
        self.0.stopped.load(Ordering::Relaxed)
    }

    // ── gossip ───────────────────────────────────────────────────────────

    fn spawn_gossip_reader(&self) {
        let ls = self.clone();
        let mut rx = self.0.net.messages();
        tokio::spawn(async move {
            loop {
                let msg: GossipMessage = match rx.recv().await {
                    Ok(m) => m,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                };
                if msg.topic != TOPIC || ls.is_stopped() {
                    continue;
                }
                let Some(source) = msg.source else { continue };
                let Ok(m) = serde_json::from_slice::<LiveMsg>(&msg.data) else {
                    continue;
                };
                match m {
                    LiveMsg::Record { record } => {
                        if ls.apply_record(record, Some(source.to_base58())).await {
                            debug!(from = %source, "live: new window");
                        }
                    }
                    LiveMsg::Have { stream_id, segs, addrs } => {
                        for a in addrs.iter().take(8) {
                            if let Ok(ma) = a.parse::<c0mpute_net::Multiaddr>() {
                                ls.0.net.add_peer_address(source, ma).await;
                            }
                        }
                        let mut streams = ls.0.streams.write().await;
                        if let Some(s) = streams.get_mut(&stream_id) {
                            let set: HashSet<String> = segs.into_iter().take(MAX_HAVE).collect();
                            s.holders.insert(source.to_base58(), (set, Instant::now()));
                        }
                    }
                }
            }
        });
    }

    /// Take a record if it verifies and is newer. Returns true when it did.
    /// Streams with viewers get their newest segments prefetched.
    pub async fn apply_record(&self, record: LiveRecord, origin: Option<String>) -> bool {
        let now = Instant::now();
        let id = record.stream_id().to_string();
        let prefetch: Vec<Segment> = {
            let mut streams = self.0.streams.write().await;
            let cur = streams.get(&id).map(|s| &s.record);
            if !record::supersedes(cur, &record) {
                return false;
            }
            {
                let mut peaks = self.0.peak_bps.lock().unwrap_or_else(|e| e.into_inner());
                let e = peaks.entry(id.clone()).or_insert(0);
                *e = (*e).max(meter::bitrate_bps(&record));
            }
            match streams.get_mut(&id) {
                Some(s) => {
                    s.reference(&record, now);
                    s.record = record.clone();
                    s.last_record = now;
                    if origin.is_some() {
                        s.origin = origin;
                    }
                }
                None => {
                    info!(stream = %id, name = %record.body.name, "live: new stream");
                    streams.insert(id.clone(), Stream::new(record.clone(), origin, now));
                }
            }
            let s = &streams[&id];
            let watched: Vec<&String> = s
                .viewers
                .iter()
                .filter(|(_, t)| now.duration_since(**t) <= self.0.timing.viewer_ttl)
                .map(|(r, _)| r)
                .collect();
            record
                .body
                .renditions
                .iter()
                .filter(|r| watched.contains(&&r.name))
                .flat_map(|r| r.segments.iter().rev().take(2))
                .filter(|seg| !s.held.contains(&seg.hash))
                .cloned()
                .collect()
        };
        for seg in prefetch {
            let ls = self.clone();
            let id = id.clone();
            tokio::spawn(async move {
                if let Err(e) = ls.segment_bytes(&id, &seg.hash).await {
                    debug!(stream = %id, err = %e, "live: prefetch failed");
                }
            });
        }
        true
    }

    async fn publish(&self, msg: &LiveMsg) {
        if let Ok(bytes) = serde_json::to_vec(msg)
            && let Err(e) = self.0.net.publish(TOPIC, bytes).await
        {
            debug!(err = %e, "live: publish (mesh forming?)");
        }
    }

    async fn dialable_addrs(&self) -> Vec<String> {
        self.0
            .net
            .advertised_addrs()
            .await
            .into_iter()
            .map(|a| a.to_string())
            .filter(|a| !a.contains("/ip4/0.0.0.0") && !a.contains("/ip6/::/"))
            .take(8)
            .collect()
    }

    /// Gossip which segments of `stream_id` this node holds.
    pub async fn announce_have(&self, stream_id: &str) {
        let segs: Vec<String> = {
            let streams = self.0.streams.read().await;
            let Some(s) = streams.get(stream_id) else { return };
            s.held.iter().filter(|h| s.segs.contains_key(*h)).take(MAX_HAVE).cloned().collect()
        };
        if segs.is_empty() {
            return;
        }
        let addrs = self.dialable_addrs().await;
        self.publish(&LiveMsg::Have {
            stream_id: stream_id.to_string(),
            segs,
            addrs,
        })
        .await;
    }

    /// The broadcaster's side: store a segment's chunks locally (before
    /// publishing the record that lists it).
    pub async fn store_segment(&self, pieces: &[Vec<u8>]) -> Result<()> {
        for p in pieces {
            self.put_owned(p).await?;
        }
        Ok(())
    }

    /// Gossip a record this node signed (the broadcaster), mark every segment
    /// it lists that is complete locally as held, and announce them.
    pub async fn publish_record(&self, record: &LiveRecord) -> Result<()> {
        if !self.apply_record(record.clone(), Some(self.0.me.clone())).await {
            bail!("record rejected (bad signature or not newer)");
        }
        let mut local = Vec::new();
        for seg in record.segments() {
            let mut all = true;
            for c in &seg.chunks {
                match Hash::from_hex(c) {
                    Ok(h) if self.0.store.has(&h).await => {}
                    _ => all = false,
                }
            }
            if all {
                local.push(seg.hash.clone());
            }
        }
        if let Some(s) = self.0.streams.write().await.get_mut(record.stream_id()) {
            s.held.extend(local);
        }
        self.publish(&LiveMsg::Record { record: record.clone() }).await;
        self.announce_have(record.stream_id()).await;
        Ok(())
    }

    async fn put_owned(&self, bytes: &[u8]) -> Result<Hash> {
        let hash = Hash::of(bytes);
        if !self.0.store.has(&hash).await {
            self.0.store.put(bytes).await?;
            self.0.owned.lock().await.insert(hash.to_hex());
        }
        Ok(hash)
    }

    fn spawn_loops(&self) {
        let ls = self.clone();
        tokio::spawn(async move {
            let mut last_have = Instant::now();
            loop {
                tokio::time::sleep(ls.0.timing.evict_every).await;
                if ls.is_stopped() {
                    return;
                }
                ls.evict_once().await;
                if last_have.elapsed() >= ls.0.timing.have_every {
                    last_have = Instant::now();
                    let ids: Vec<String> = ls.0.streams.read().await.keys().cloned().collect();
                    for id in ids {
                        ls.announce_have(&id).await;
                    }
                }
            }
        });
    }

    /// One pass: forget dead streams and silent holders, evict old segments
    /// and delete their chunks.
    pub async fn evict_once(&self) {
        let now = Instant::now();
        let t = &self.0.timing;
        let mut dropped: Vec<Known> = Vec::new();
        let live_chunks: HashSet<String> = {
            let mut streams = self.0.streams.write().await;
            streams.retain(|id, s| {
                let keep = now.duration_since(s.last_record) <= t.stream_ttl;
                if !keep {
                    info!(stream = %id, "live: stream gone quiet; forgetting it");
                    dropped.extend(s.segs.drain().map(|(_, k)| k));
                }
                keep
            });
            self.0
                .peak_bps
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|id, _| streams.contains_key(id));
            self.0
                .clients
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|id, _| streams.contains_key(id));
            for s in streams.values_mut() {
                s.holders.retain(|_, (_, seen)| now.duration_since(*seen) <= t.holder_ttl);
                let current: HashSet<&str> = s.record.segments().map(|g| g.hash.as_str()).collect();
                let gone = evict(&mut s.segs, &current, now, t.grace);
                for k in &gone {
                    s.held.remove(&k.seg.hash);
                }
                dropped.extend(gone);
            }
            streams
                .values()
                .flat_map(|s| s.segs.values().flat_map(|k| k.seg.chunks.iter().cloned()))
                .collect()
        };
        if dropped.is_empty() {
            return;
        }
        let mut owned = self.0.owned.lock().await;
        for k in dropped {
            for c in &k.seg.chunks {
                if !live_chunks.contains(c)
                    && owned.remove(c)
                    && let Ok(h) = Hash::from_hex(c)
                {
                    let _ = self.0.store.delete(&h).await;
                }
            }
        }
    }

    // ── fetching ─────────────────────────────────────────────────────────

    /// The bytes of one segment of a stream: from the local store, else
    /// fetched from the swarm (and then held and announced).
    pub async fn segment_bytes(&self, stream_id: &str, seg_hash: &str) -> Result<Vec<u8>> {
        let seg = {
            let streams = self.0.streams.read().await;
            let s = streams.get(stream_id).context("unknown stream")?;
            s.segs.get(seg_hash).map(|k| k.seg.clone()).context("segment is not in this stream's window")?
        };
        let lock = {
            let mut m = self.0.inflight.lock().await;
            m.entry(seg.hash.clone()).or_insert_with(|| Arc::new(Mutex::new(()))).clone()
        };
        let _guard = lock.lock().await;
        let result = self.assemble(stream_id, &seg).await;
        self.0.inflight.lock().await.remove(&seg.hash);
        let (bytes, fetched) = result?;
        if fetched {
            if let Some(s) = self.0.streams.write().await.get_mut(stream_id)
                && s.segs.contains_key(&seg.hash)
            {
                s.held.insert(seg.hash.clone());
            }
            let ls = self.clone();
            let id = stream_id.to_string();
            tokio::spawn(async move { ls.announce_have(&id).await });
        }
        Ok(bytes)
    }

    /// Returns (bytes, whether anything came over the network).
    async fn assemble(&self, stream_id: &str, seg: &Segment) -> Result<(Vec<u8>, bool)> {
        let mut body = Vec::with_capacity(seg.size as usize);
        let mut fetched = false;
        for c in &seg.chunks {
            let hash = Hash::from_hex(c).context("bad chunk hash")?;
            if self.0.store.has(&hash).await
                && let Ok(b) = self.0.store.get(&hash).await
            {
                body.extend_from_slice(&b);
                continue;
            }
            let b = self.fetch_chunk(stream_id, &seg.hash, &hash).await?;
            self.put_owned(&b).await?;
            body.extend_from_slice(&b);
            fetched = true;
        }
        if Hash::of(&body).to_hex() != seg.hash || body.len() as u64 != seg.size {
            bail!("segment {} reassembled to the wrong bytes", seg.hash);
        }
        Ok((body, fetched))
    }

    /// Peers to ask for a segment, best first.
    async fn sources(&self, stream_id: &str, seg_hash: &str) -> Vec<String> {
        let streams = self.0.streams.read().await;
        let Some(s) = streams.get(stream_id) else {
            return Vec::new();
        };
        let rtt = self.0.rtt_ms.read().await;
        let now = Instant::now();
        let mut cands: Vec<(String, Option<f64>, bool)> = s
            .holders
            .iter()
            .filter(|(p, (segs, seen))| {
                **p != self.0.me && segs.contains(seg_hash) && now.duration_since(*seen) <= self.0.timing.holder_ttl
            })
            .map(|(p, _)| (p.clone(), rtt.get(p).copied(), s.origin.as_deref() == Some(p.as_str())))
            .collect();
        // The broadcaster holds its whole window even before its Have lands.
        if let Some(o) = &s.origin
            && *o != self.0.me
            && !cands.iter().any(|(p, _, _)| p == o)
        {
            cands.push((o.clone(), rtt.get(o).copied(), true));
        }
        source_order(&cands, self.0.turn.fetch_add(1, Ordering::Relaxed))
    }

    async fn fetch_chunk(&self, stream_id: &str, seg_hash: &str, hash: &Hash) -> Result<Vec<u8>> {
        let _slot = self.0.fetch_slots.acquire().await?;
        let order = self.sources(stream_id, seg_hash).await;
        let req = ChunkRequest {
            chunk_hash: *hash,
            shard_index: None,
        };
        let mut last = String::from("no peer holds it");
        for peer in order {
            let Ok(pid) = peer.parse::<c0mpute_net::PeerId>() else {
                continue;
            };
            let started = Instant::now();
            let outcome =
                tokio::time::timeout(self.0.timing.fetch_timeout, self.0.net.fetch_from_peer(pid, &req)).await;
            let ms = match outcome {
                Ok(Ok(bytes)) if Hash::of(&bytes) == *hash => {
                    self.observe_rtt(&peer, started.elapsed().as_secs_f64() * 1000.0).await;
                    if let Some(s) = self.0.streams.write().await.get_mut(stream_id) {
                        *s.fetched_from.entry(peer.clone()).or_default() += 1;
                    }
                    return Ok(bytes);
                }
                Ok(Ok(_)) => {
                    last = format!("{peer} sent bytes that do not match the hash");
                    FAILED_RTT_MS
                }
                Ok(Err(e)) => {
                    last = format!("{peer}: {e}");
                    FAILED_RTT_MS
                }
                Err(_) => {
                    last = format!("{peer}: timed out");
                    FAILED_RTT_MS
                }
            };
            self.observe_rtt(&peer, ms).await;
        }
        bail!("could not fetch chunk {}: {last}", hash.to_hex())
    }

    async fn observe_rtt(&self, peer: &str, ms: f64) {
        let mut m = self.0.rtt_ms.write().await;
        let e = m.entry(peer.to_string()).or_insert(ms);
        *e = *e * 0.7 + ms * 0.3;
    }

    // ── reading state ────────────────────────────────────────────────────

    pub async fn record(&self, stream_id: &str) -> Option<LiveRecord> {
        self.0.streams.read().await.get(stream_id).map(|s| s.record.clone())
    }

    /// Peers (other than this node) known to hold `seg_hash`.
    pub async fn holders_of(&self, stream_id: &str, seg_hash: &str) -> Vec<String> {
        let streams = self.0.streams.read().await;
        let Some(s) = streams.get(stream_id) else {
            return Vec::new();
        };
        let mut v: Vec<String> = s
            .holders
            .iter()
            .filter(|(p, (segs, _))| **p != self.0.me && segs.contains(seg_hash))
            .map(|(p, _)| p.clone())
            .collect();
        v.sort();
        v
    }

    pub async fn holds(&self, stream_id: &str, seg_hash: &str) -> bool {
        self.0
            .streams
            .read()
            .await
            .get(stream_id)
            .is_some_and(|s| s.held.contains(seg_hash))
    }

    pub async fn streams(&self) -> Vec<StreamInfo> {
        let now = Instant::now();
        let streams = self.0.streams.read().await;
        let mut v: Vec<StreamInfo> = streams
            .values()
            .map(|s| StreamInfo {
                stream_id: s.record.body.stream_id.clone(),
                name: s.record.body.name.clone(),
                owner: s.record.body.owner.clone(),
                seq: s.record.body.seq,
                ended: s.record.body.ended,
                origin: s.origin.clone(),
                age_ms: now.duration_since(s.last_record).as_millis() as u64,
                held: s.held.len(),
                holders: s.holders.len(),
                fetched_from: s.fetched_from.clone(),
                account: s.record.account.clone(),
                paid: s.record.is_paid(),
                max_viewers: s.record.viewer_cap(),
                renditions: s
                    .record
                    .body
                    .renditions
                    .iter()
                    .map(|r| {
                        let (peak, avg) = playlist::bandwidth(r);
                        serde_json::json!({
                            "name": r.name,
                            "resolution": r.resolution,
                            "media_seq": r.media_seq,
                            "segments": r.segments.len(),
                            "bandwidth": peak,
                            "average_bandwidth": avg,
                        })
                    })
                    .collect(),
            })
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name).then(a.stream_id.cmp(&b.stream_id)));
        v
    }

    pub async fn info(&self, stream_id: &str) -> Option<StreamInfo> {
        self.streams().await.into_iter().find(|s| s.stream_id == stream_id)
    }

    async fn note_viewer(&self, stream_id: &str, rendition: &str) {
        if let Some(s) = self.0.streams.write().await.get_mut(stream_id) {
            s.viewers.insert(rendition.to_string(), Instant::now());
        }
    }

    // ── serving ──────────────────────────────────────────────────────────

    /// Gateway routes: `/live/v1/streams`, `/live/{id}/index.m3u8`,
    /// `/live/{id}/{hash}.ts`, and per rendition `/live/{id}/{r}/...`.
    pub fn router(&self) -> Router {
        // Every answer (errors too) carries `access-control-allow-origin: *`
        // and OPTIONS is answered, so a browser player on another origin
        // (hls.js on c0mpute.com against a gateway) can fetch all of it.
        Router::new()
            .route("/live/v1/streams", get(list_streams).options(preflight))
            .route("/live/{id}/{file}", get(serve_top).options(preflight))
            .route("/live/{id}/{rendition}/{file}", get(serve_rendition).options(preflight))
            .layer(axum::middleware::map_response(cors))
            .with_state(self.clone())
    }

    async fn serve_playlist(&self, stream_id: &str, rendition: Option<&str>, client: &str) -> Response {
        let Some(rec) = self.record(stream_id).await else {
            return not_found("no such live stream on the network (yet)");
        };
        if !self.admit_viewer(stream_id, client).await {
            return viewer_limit();
        }
        let text = match rendition {
            None => {
                if let [only] = rec.body.renditions.as_slice() {
                    self.note_viewer(stream_id, &only.name).await;
                }
                playlist::index_playlist(&rec)
            }
            Some(r) => match rec.rendition(r) {
                Some(rend) => {
                    self.note_viewer(stream_id, r).await;
                    playlist::media_playlist(rend, rec.body.ended)
                }
                None => return not_found("no such rendition"),
            },
        };
        let mut resp = (StatusCode::OK, text).into_response();
        let h = resp.headers_mut();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static(playlist::PLAYLIST_CONTENT_TYPE));
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
        resp
    }

    async fn serve_segment(&self, stream_id: &str, file: &str, client: &str) -> Response {
        let Some(hash) = file.strip_suffix(&format!(".{}", playlist::SEGMENT_EXT)) else {
            return not_found("not found");
        };
        if Hash::from_hex(hash).is_err() {
            return not_found("not found");
        }
        if !self.admit_viewer(stream_id, client).await {
            return viewer_limit();
        }
        match self.segment_bytes(stream_id, hash).await {
            Ok(bytes) => {
                self.count_served(stream_id, bytes.len() as u64);
                let mut resp = (StatusCode::OK, Body::from(bytes)).into_response();
                let h = resp.headers_mut();
                h.insert(header::CONTENT_TYPE, HeaderValue::from_static(playlist::SEGMENT_CONTENT_TYPE));
                // Content-addressed: a segment never changes.
                h.insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=3600, immutable"));
                h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
                resp
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("unknown stream") || msg.contains("not in this stream") {
                    not_found("segment is not in this stream's live window")
                } else {
                    warn!(stream = %stream_id, err = %msg, "live: segment unavailable");
                    (StatusCode::BAD_GATEWAY, "segment is unavailable right now").into_response()
                }
            }
        }
    }
}

fn not_found(msg: &'static str) -> Response {
    (StatusCode::NOT_FOUND, msg).into_response()
}

/// A bound, unpaid stream already has its viewer cap of clients here.
fn viewer_limit() -> Response {
    let mut resp = (
        StatusCode::SERVICE_UNAVAILABLE,
        "this stream is on the free tier and has reached its viewer limit on this gateway; try again shortly",
    )
        .into_response();
    resp.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("15"));
    resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

async fn cors(mut resp: Response) -> Response {
    let h = resp.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    h.insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static("content-length, content-type, retry-after"),
    );
    resp
}

async fn preflight() -> Response {
    let mut resp = StatusCode::NO_CONTENT.into_response();
    let h = resp.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET, HEAD, OPTIONS"));
    h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("*"));
    h.insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("86400"));
    resp
}

async fn list_streams(State(ls): State<LiveStream>) -> Response {
    let streams: Vec<serde_json::Value> = ls
        .streams()
        .await
        .into_iter()
        .map(|s| {
            let mut v = serde_json::to_value(&s).unwrap_or_default();
            v["path"] = serde_json::Value::String(format!("/live/{}/index.m3u8", s.stream_id));
            v
        })
        .collect();
    let mut resp = axum::Json(serde_json::json!({ "streams": streams })).into_response();
    resp.headers_mut()
        .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    resp
}

async fn serve_top(
    State(ls): State<LiveStream>,
    AxPath((id, file)): AxPath<(String, String)>,
    req: Request,
) -> Response {
    if !record::valid_stream_id(&id) {
        return not_found("not found");
    }
    let client = request_client(&req);
    if file == "index.m3u8" {
        ls.serve_playlist(&id, None, &client).await
    } else {
        ls.serve_segment(&id, &file, &client).await
    }
}

async fn serve_rendition(
    State(ls): State<LiveStream>,
    AxPath((id, rendition, file)): AxPath<(String, String, String)>,
    req: Request,
) -> Response {
    if !record::valid_stream_id(&id) || !record::valid_rendition_name(&rendition) {
        return not_found("not found");
    }
    let client = request_client(&req);
    if file == "index.m3u8" {
        ls.serve_playlist(&id, Some(&rendition), &client).await
    } else {
        ls.serve_segment(&id, &file, &client).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eviction_keeps_the_window_and_grace() {
        let now = Instant::now() + Duration::from_secs(200);
        let mut segs = HashMap::new();
        for (i, age) in [0u64, 5, 31, 100].into_iter().enumerate() {
            let (seg, _) = record::describe_segment(&[i as u8], 2000, false);
            segs.insert(
                seg.hash.clone(),
                Known {
                    seg,
                    rendition: "src".into(),
                    last_ref: now - Duration::from_secs(age),
                },
            );
        }
        let ancient = segs.iter().find(|(_, k)| now.duration_since(k.last_ref).as_secs() == 100).map(|(h, _)| h.clone()).unwrap();
        // the 100 s old one is still in the current window: it stays
        let current: HashSet<&str> = [ancient.as_str()].into_iter().collect();
        let gone = evict(&mut segs, &current, now, Duration::from_secs(30));
        assert_eq!(gone.len(), 1, "only the 31 s old segment outside the window goes");
        assert!(segs.contains_key(&ancient));
        let gone = evict(&mut segs, &HashSet::new(), now, Duration::from_secs(30));
        assert_eq!(gone.len(), 1, "once it leaves the window it goes too");
        assert_eq!(segs.len(), 2);
        assert!(segs.values().all(|k| now.duration_since(k.last_ref) <= Duration::from_secs(30)));
    }

    #[test]
    fn source_order_prefers_relays_then_fast_and_rotates() {
        let c = |p: &str, rtt: Option<f64>, o: bool| (p.to_string(), rtt, o);
        // unmeasured relay beats the unmeasured origin
        let o = source_order(&[c("origin", None, true), c("relay", None, false)], 0);
        assert_eq!(o, vec!["relay", "origin"]);
        // a fast origin still beats a slow relay
        let o = source_order(&[c("origin", Some(5.0), true), c("slow", Some(400.0), false)], 0);
        assert_eq!(o[0], "origin");
        // a failed peer sorts last
        let o = source_order(
            &[c("bad", Some(FAILED_RTT_MS), false), c("origin", Some(10.0), true), c("r", Some(20.0), false)],
            0,
        );
        assert_eq!(o.last().unwrap(), "bad");
        // equal relays take turns
        let peers = [c("a", Some(10.0), false), c("b", Some(12.0), false)];
        assert_ne!(source_order(&peers, 0)[0], source_order(&peers, 1)[0]);
    }

    #[test]
    fn the_viewer_is_the_client_behind_a_local_proxy() {
        let mut h = HeaderMap::new();
        let ip = |s: &str| Some(s.parse::<std::net::IpAddr>().unwrap());
        assert_eq!(client_key(&h, ip("203.0.113.9")), "203.0.113.9");
        assert_eq!(client_key(&h, ip("127.0.0.1")), "127.0.0.1");
        assert_eq!(client_key(&h, None), "unknown");
        h.insert("x-forwarded-for", HeaderValue::from_static("198.51.100.4, 10.0.0.1"));
        assert_eq!(client_key(&h, ip("127.0.0.1")), "198.51.100.4", "a local proxy's header counts");
        assert_eq!(client_key(&h, ip("10.1.2.3")), "198.51.100.4");
        assert_eq!(client_key(&h, None), "198.51.100.4");
        assert_eq!(client_key(&h, ip("203.0.113.9")), "203.0.113.9", "a public client cannot pick its own key");
        h.insert("cf-connecting-ip", HeaderValue::from_static("192.0.2.77"));
        assert_eq!(client_key(&h, ip("::1")), "192.0.2.77");
    }

    #[test]
    fn have_message_fits() {
        let segs: Vec<String> = (0..MAX_HAVE).map(|i| Hash::of(&i.to_le_bytes()).to_hex()).collect();
        let m = LiveMsg::Have {
            stream_id: "0".repeat(20),
            segs,
            addrs: vec!["/ip4/203.0.113.7/tcp/40001/p2p/12D3KooWAbc".into(); 8],
        };
        let n = serde_json::to_vec(&m).unwrap().len();
        assert!(n < 16 * 1024, "{n} bytes");
    }
}
