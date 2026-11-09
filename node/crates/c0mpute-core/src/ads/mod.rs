//! Ads on c0mpute, peer to peer (DIP-0020/0021/0023, v1):
//!
//! * [`campaign`] — `ads-manager`: an advertiser's signed campaign record
//!   (ed25519 key, id = hash(owner, name), seq, creatives by hash, targeting,
//!   schedule, frequency cap, max CPM, landing URL). Records are gossiped on
//!   [`TOPIC`]; every node keeps the index. Creatives are files in a hosting
//!   site owned by the same key, so they live on three self-healing copies
//!   and every gateway fetches them with failover. No ad server, no registry.
//! * [`select`] — the deterministic, explainable choice of a campaign for a
//!   slot (filters, highest bid, rendezvous tie-break, frequency cap).
//! * [`receipt`] — signed, chained impression receipts from the gateways that
//!   served the ads; advertisers count them with their own CLI.
//! * [`stitch`] — `podcast-ads`: dynamic ad insertion into c0mpute-hosted
//!   episodes (`?ads=1`), only for shows whose signed site opts in.
//! * [`vast`] — `ctv-ads`: VAST 4.2 and VMAP 1.0 for video players.
//!
//! Not here yet: billing and settlement. Bids are a ranking signal only; no
//! money moves and nothing is escrowed.

pub mod campaign;
pub mod manage;
pub mod receipt;
pub mod select;
pub mod stitch;
pub mod vast;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use axum::Router;
use axum::body::Body;
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use c0mpute_net::libp2p::identity::Keypair;
use c0mpute_net::{GossipMessage, Libp2pNetwork};
use c0mpute_proto::Hash;
use serde::{Deserialize, Serialize};
use tokio::sync::{RwLock, Semaphore};
use tracing::{debug, info, warn};

use crate::hosting::site::SiteRecord;
use crate::hosting::{Hosting, ServeHook};
use campaign::{CampaignRecord, Creative, Placement};
use receipt::{Event, Receipt, ReceiptBatch, ReceiptLog};
use select::{AdContext, Explained, FreqLedger};

pub const TOPIC: &str = "c0mpute/ads/v1";
/// Campaigns that ended longer ago than this are forgotten.
const FORGET_ENDED_AFTER: i64 = 7 * 86_400;
/// Tracking links stay valid this long after the ad was served.
const BEACON_TTL_SECS: i64 = 6 * 3600;

/// Verify an ed25519 signature (hex key, hex sig).
pub fn verify_sig(owner_hex: &str, bytes: &[u8], sig_hex: &str) -> bool {
    let (Ok(key), Ok(sig)) = (hex::decode(owner_hex), hex::decode(sig_hex)) else {
        return false;
    };
    let Ok(public) = c0mpute_net::libp2p::identity::ed25519::PublicKey::try_from_bytes(&key) else {
        return false;
    };
    public.verify(bytes, &sig)
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Gossip on [`TOPIC`].
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum AdsMsg {
    /// A signed campaign (published, or re-gossiped by any node).
    Campaign { record: CampaignRecord },
    /// "I just joined": peers re-gossip what they know soon.
    Hello,
    /// A gateway's signed receipt batch.
    Receipts { batch: ReceiptBatch },
    /// An advertiser asks gateways to re-gossip their receipts for a campaign.
    ReportWant { campaign_id: String, since: u64 },
}

/// How often things happen. Tests shrink these.
#[derive(Clone, Debug)]
pub struct Timing {
    /// A node re-gossips a campaign when nobody else has for about this long.
    pub regossip_every: Duration,
    pub tick: Duration,
    pub flush_every: Duration,
    /// How long a stitched episode is reused for the same listener.
    pub stitch_ttl: Duration,
    pub stitch_timeout: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            regossip_every: Duration::from_secs(600),
            tick: Duration::from_secs(5),
            flush_every: Duration::from_secs(60),
            stitch_ttl: Duration::from_secs(600),
            stitch_timeout: Duration::from_secs(300),
        }
    }
}

pub struct Options {
    /// Signs receipts (the node's identity). Without it the node serves no ads.
    pub signer: Option<Keypair>,
    /// Index, receipt log and stitch cache live here.
    pub data_dir: Option<PathBuf>,
    /// This node answers ad requests (a gateway).
    pub serve: bool,
    pub ffmpeg: Option<PathBuf>,
    pub ffprobe: Option<PathBuf>,
    /// Absolute origin for URLs in VAST (else from the request's Host).
    pub public_origin: Option<String>,
    pub timing: Timing,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            signer: None,
            data_dir: None,
            serve: false,
            ffmpeg: None,
            ffprobe: None,
            public_origin: None,
            timing: Timing::default(),
        }
    }
}

struct Stitched {
    path: PathBuf,
    mime: String,
    created: Instant,
    summary: String,
}

struct Inner {
    net: Arc<Libp2pNetwork>,
    hosting: Hosting,
    signer: Option<Keypair>,
    gateway_key: String,
    data_dir: Option<PathBuf>,
    serve: bool,
    ffmpeg: Option<PathBuf>,
    ffprobe: Option<PathBuf>,
    public_origin: Option<String>,
    timing: Timing,
    index: RwLock<HashMap<String, CampaignRecord>>,
    dirty: std::sync::atomic::AtomicBool,
    next_gossip: Mutex<HashMap<String, Instant>>,
    freq: Mutex<FreqLedger>,
    log: Mutex<ReceiptLog>,
    secret: [u8; 32],
    seen_beacons: Mutex<HashMap<String, i64>>,
    watching: Mutex<HashSet<String>>,
    collected: Mutex<HashMap<String, HashMap<(String, u64), ReceiptBatch>>>,
    answered: Mutex<HashMap<String, Instant>>,
    stitched: Mutex<HashMap<String, Stitched>>,
    stitch_slots: Semaphore,
}

/// The ads service: one per node. Cheap to clone.
#[derive(Clone)]
pub struct Ads(Arc<Inner>);

fn jittered(d: Duration) -> Duration {
    // 0.5x .. 1.5x
    let ms = d.as_millis() as u64;
    Duration::from_millis(ms / 2 + rand::random::<u64>() % ms.max(1))
}

impl Ads {
    /// Load the index, subscribe to [`TOPIC`], start gossip and flushing.
    pub async fn start(net: Arc<Libp2pNetwork>, hosting: Hosting, opts: Options) -> Result<Self> {
        let gateway_key = opts
            .signer
            .as_ref()
            .and_then(crate::hosting::site::owner_hex)
            .unwrap_or_default();
        let mut index = HashMap::new();
        if let Some(d) = &opts.data_dir
            && let Ok(b) = std::fs::read(d.join("campaigns.json"))
            && let Ok(list) = serde_json::from_slice::<Vec<CampaignRecord>>(&b)
        {
            for r in list.into_iter().filter(CampaignRecord::verify) {
                index.insert(r.campaign_id.clone(), r);
            }
        }
        let log = ReceiptLog::open(opts.data_dir.as_ref().map(|d| d.join("receipts")));
        let inner = Inner {
            net: net.clone(),
            hosting,
            signer: opts.signer,
            gateway_key,
            data_dir: opts.data_dir,
            serve: opts.serve,
            ffmpeg: opts.ffmpeg,
            ffprobe: opts.ffprobe,
            public_origin: opts.public_origin.map(|o| o.trim_end_matches('/').to_string()),
            timing: opts.timing,
            index: RwLock::new(index),
            dirty: std::sync::atomic::AtomicBool::new(false),
            next_gossip: Mutex::new(HashMap::new()),
            freq: Mutex::new(FreqLedger::default()),
            log: Mutex::new(log),
            secret: rand::random(),
            seen_beacons: Mutex::new(HashMap::new()),
            watching: Mutex::new(HashSet::new()),
            collected: Mutex::new(HashMap::new()),
            answered: Mutex::new(HashMap::new()),
            stitched: Mutex::new(HashMap::new()),
            stitch_slots: Semaphore::new(2),
        };
        let ads = Ads(Arc::new(inner));
        net.subscribe(TOPIC).await.context("subscribe to ads topic")?;
        ads.spawn_gossip_reader();
        ads.spawn_ticker();
        ads.publish(&AdsMsg::Hello).await;
        Ok(ads)
    }

    /// Serve `?ads=1` podcast episodes through this node's hosting gateway.
    pub fn install_podcast_hook(&self) -> bool {
        self.0.hosting.set_serve_hook(Arc::new(PodcastHook(self.clone())))
    }

    pub fn gateway_key(&self) -> &str {
        &self.0.gateway_key
    }

    pub fn can_stitch(&self) -> bool {
        self.0.ffmpeg.is_some() && self.0.ffprobe.is_some()
    }

    // ── gossip ───────────────────────────────────────────────────────────

    async fn publish(&self, msg: &AdsMsg) {
        if let Ok(bytes) = serde_json::to_vec(msg)
            && let Err(e) = self.0.net.publish(TOPIC, bytes).await
        {
            debug!(err = %e, "ads: publish (mesh forming?)");
        }
    }

    /// Apply a record to the index; true if it was new or newer.
    pub async fn apply(&self, record: CampaignRecord) -> bool {
        if !record.verify() {
            return false;
        }
        let id = record.campaign_id.clone();
        let mut index = self.0.index.write().await;
        if index.get(&id).is_some_and(|old| old.seq >= record.seq) {
            return false;
        }
        index.insert(id, record);
        self.0.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        true
    }

    fn push_back_regossip(&self, id: &str) {
        let at = Instant::now() + jittered(self.0.timing.regossip_every);
        self.0.next_gossip.lock().unwrap().insert(id.to_string(), at);
    }

    /// Publish a (new version of a) campaign to the network.
    pub async fn publish_campaign(&self, record: CampaignRecord) -> Result<()> {
        if !record.verify() {
            bail!("campaign record does not verify");
        }
        let newer = self.apply(record.clone()).await;
        if !newer
            && self
                .0
                .index
                .read()
                .await
                .get(&record.campaign_id)
                .is_some_and(|o| o.seq > record.seq)
        {
            bail!("the network already has a newer version of this campaign");
        }
        self.push_back_regossip(&record.campaign_id);
        self.publish(&AdsMsg::Campaign { record }).await;
        Ok(())
    }

    /// Re-gossip a campaign now (publishers repeat while the mesh forms).
    pub async fn regossip(&self, record: &CampaignRecord) {
        self.publish(&AdsMsg::Campaign { record: record.clone() }).await;
    }

    fn spawn_gossip_reader(&self) {
        let ads = self.clone();
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
                let Ok(m) = serde_json::from_slice::<AdsMsg>(&msg.data) else {
                    continue;
                };
                ads.handle(m).await;
            }
        });
    }

    async fn handle(&self, m: AdsMsg) {
        match m {
            AdsMsg::Campaign { record } => {
                let id = record.campaign_id.clone();
                let known_seq = self.0.index.read().await.get(&id).map(|r| r.seq);
                if self.apply(record.clone()).await {
                    debug!(campaign = %id, name = %record.name, seq = record.seq, "ads: campaign");
                    self.push_back_regossip(&id);
                } else if known_seq == Some(record.seq) {
                    // Someone else re-gossiped it: no need for us to.
                    self.push_back_regossip(&id);
                }
            }
            AdsMsg::Hello => {
                let soon = Instant::now() + jittered(self.0.timing.tick * 2);
                let ids: Vec<String> = self.0.index.read().await.keys().cloned().collect();
                let mut next = self.0.next_gossip.lock().unwrap();
                for id in ids {
                    let e = next.entry(id).or_insert(soon);
                    if *e > soon {
                        *e = soon;
                    }
                }
            }
            AdsMsg::Receipts { batch } => {
                let watching = self.0.watching.lock().unwrap().contains(&batch.campaign_id);
                if watching && batch.verify() {
                    self.0
                        .collected
                        .lock()
                        .unwrap()
                        .entry(batch.campaign_id.clone())
                        .or_default()
                        .insert((batch.gateway.clone(), batch.seq), batch);
                }
            }
            AdsMsg::ReportWant { campaign_id, since } => {
                if !self.0.serve {
                    return;
                }
                {
                    let mut answered = self.0.answered.lock().unwrap();
                    if answered
                        .get(&campaign_id)
                        .is_some_and(|t| t.elapsed() < Duration::from_secs(3))
                    {
                        return;
                    }
                    answered.insert(campaign_id.clone(), Instant::now());
                }
                self.flush_receipts().await;
                let batches = self.0.log.lock().unwrap().batches(&campaign_id, since);
                for batch in batches.into_iter().take(2_000) {
                    self.publish(&AdsMsg::Receipts { batch }).await;
                }
            }
        }
    }

    fn spawn_ticker(&self) {
        let ads = self.clone();
        tokio::spawn(async move {
            let mut last_flush = Instant::now();
            loop {
                tokio::time::sleep(ads.0.timing.tick).await;
                ads.regossip_due().await;
                let pending = ads.0.log.lock().unwrap().pending_len();
                if pending >= receipt::MAX_RECEIPTS_PER_BATCH || (pending > 0 && last_flush.elapsed() >= ads.0.timing.flush_every) {
                    ads.flush_receipts().await;
                    last_flush = Instant::now();
                }
                ads.housekeeping().await;
            }
        });
    }

    async fn regossip_due(&self) {
        let now = Instant::now();
        let records: Vec<CampaignRecord> = self.0.index.read().await.values().cloned().collect();
        let mut due = Vec::new();
        {
            let mut next = self.0.next_gossip.lock().unwrap();
            for r in records {
                let at = *next
                    .entry(r.campaign_id.clone())
                    .or_insert_with(|| now + jittered(self.0.timing.regossip_every));
                if at <= now {
                    next.insert(r.campaign_id.clone(), now + jittered(self.0.timing.regossip_every));
                    due.push(r);
                }
            }
        }
        for record in due {
            self.publish(&AdsMsg::Campaign { record }).await;
        }
    }

    async fn housekeeping(&self) {
        let now = now_unix();
        {
            let mut index = self.0.index.write().await;
            let before = index.len();
            index.retain(|_, r| r.schedule.end.is_none_or(|e| e + FORGET_ENDED_AFTER > now));
            if index.len() != before {
                self.0.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
        if self.0.dirty.swap(false, std::sync::atomic::Ordering::Relaxed)
            && let Some(d) = &self.0.data_dir
        {
            let list: Vec<CampaignRecord> = self.0.index.read().await.values().cloned().collect();
            let _ = std::fs::create_dir_all(d);
            let tmp = d.join("campaigns.json.tmp");
            if std::fs::write(&tmp, serde_json::to_vec(&list).unwrap_or_default()).is_ok() {
                let _ = std::fs::rename(&tmp, d.join("campaigns.json"));
            }
        }
        self.0
            .seen_beacons
            .lock()
            .unwrap()
            .retain(|_, t| *t + BEACON_TTL_SECS > now);
        let ttl = self.0.timing.stitch_ttl;
        let expired: Vec<PathBuf> = {
            let mut st = self.0.stitched.lock().unwrap();
            let gone: Vec<String> = st
                .iter()
                .filter(|(_, s)| s.created.elapsed() > ttl)
                .map(|(k, _)| k.clone())
                .collect();
            gone.into_iter().filter_map(|k| st.remove(&k)).map(|s| s.path).collect()
        };
        for p in expired {
            let _ = std::fs::remove_file(p);
        }
    }

    // ── index ────────────────────────────────────────────────────────────

    pub async fn campaigns(&self) -> Vec<CampaignRecord> {
        let mut v: Vec<CampaignRecord> = self.0.index.read().await.values().cloned().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name).then(a.campaign_id.cmp(&b.campaign_id)));
        v
    }

    pub async fn campaign(&self, id: &str) -> Option<CampaignRecord> {
        self.0.index.read().await.get(id).cloned()
    }

    /// Pick an ad for `ctx` from this node's index.
    pub async fn select(&self, ctx: &AdContext) -> Explained {
        let index = self.0.index.read().await;
        let freq = self.0.freq.lock().unwrap();
        select::select(index.values(), ctx, &freq)
    }

    // ── creatives ────────────────────────────────────────────────────────

    /// A creative's bytes from the campaign's hosting site, checked against
    /// the hash the advertiser signed.
    pub async fn creative_bytes(&self, c: &CampaignRecord, cr: &Creative) -> Result<Vec<u8>> {
        let site = self
            .0
            .hosting
            .record_for(&c.creative_site)
            .await
            .context("the creative site is not known to this node yet")?;
        if site.owner != c.owner {
            bail!("creative site has a different owner");
        }
        let (_, bytes) = self
            .0
            .hosting
            .read_file(&site, &cr.path)
            .await?
            .context("creative missing from its site")?;
        if Hash::of(&bytes).to_hex() != cr.hash {
            bail!("creative bytes do not match the signed hash");
        }
        Ok(bytes)
    }

    // ── receipts ─────────────────────────────────────────────────────────

    pub fn record(&self, r: Receipt) {
        if r.event == Event::Impression && !r.listener.is_empty() {
            self.0.freq.lock().unwrap().record(&r.listener, &r.campaign_id, r.ts);
        }
        self.0.log.lock().unwrap().add(r);
    }

    /// Sign pending receipts into batches and gossip them.
    pub async fn flush_receipts(&self) -> Vec<ReceiptBatch> {
        let Some(kp) = &self.0.signer else {
            return vec![];
        };
        let batches = self.0.log.lock().unwrap().flush(kp);
        for b in &batches {
            self.publish(&AdsMsg::Receipts { batch: b.clone() }).await;
        }
        batches
    }

    /// This gateway's own batches for a campaign.
    pub fn local_batches(&self, campaign_id: &str, since: u64) -> Vec<ReceiptBatch> {
        self.0.log.lock().unwrap().batches(campaign_id, since)
    }

    /// Ask gateways for their receipts for a campaign; they arrive in
    /// [`Self::collected`].
    pub async fn request_report(&self, campaign_id: &str) {
        self.0.watching.lock().unwrap().insert(campaign_id.to_string());
        self.publish(&AdsMsg::ReportWant {
            campaign_id: campaign_id.to_string(),
            since: 0,
        })
        .await;
    }

    pub fn collected(&self, campaign_id: &str) -> Vec<ReceiptBatch> {
        self.0
            .collected
            .lock()
            .unwrap()
            .get(campaign_id)
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default()
    }

    // ── beacons ──────────────────────────────────────────────────────────

    fn sign_beacon(&self, b: &Beacon) -> String {
        use base64::Engine;
        let body = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(b).unwrap_or_default());
        let mac = self.mac(body.as_bytes());
        format!("{body}.{mac}")
    }

    fn mac(&self, data: &[u8]) -> String {
        use hmac::{Hmac, Mac};
        let mut m = <Hmac<sha2::Sha256> as Mac>::new_from_slice(&self.0.secret).expect("hmac key");
        m.update(data);
        hex::encode(&m.finalize().into_bytes()[..16])
    }

    fn open_beacon(&self, tok: &str) -> Option<Beacon> {
        use base64::Engine;
        let (body, mac) = tok.split_once('.')?;
        if !crate::gateway_util::constant_time_eq(self.mac(body.as_bytes()).as_bytes(), mac.as_bytes()) {
            return None;
        }
        let b: Beacon = serde_json::from_slice(&base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(body).ok()?).ok()?;
        (b.exp > now_unix()).then_some(b)
    }

    /// Record a tracking event once per (play, event).
    fn beacon_event(&self, b: &Beacon, ev: Event) -> bool {
        let key = format!("{}|{:?}", b.s, ev);
        {
            let mut seen = self.0.seen_beacons.lock().unwrap();
            if seen.contains_key(&key) {
                return false;
            }
            seen.insert(key, now_unix());
        }
        self.record(Receipt {
            campaign_id: b.c.clone(),
            creative: b.cr.clone(),
            placement: b.p,
            event: ev,
            ts: now_unix(),
            listener: b.l.clone(),
            serving_id: b.s.clone(),
            site: b.site.clone(),
            country: b.cc.clone(),
        });
        true
    }

    // ── HTTP ─────────────────────────────────────────────────────────────

    /// Gateway routes under `/ads/v1/` (plus `/ads/vast`, `/ads/vmap`).
    pub fn router(&self) -> Router {
        Router::new()
            .route("/ads/v1/campaigns", get(http_campaigns))
            .route("/ads/v1/select", get(http_select))
            .route("/ads/v1/vast", get(http_vast))
            .route("/ads/vast", get(http_vast))
            .route("/ads/v1/vmap", get(http_vmap))
            .route("/ads/vmap", get(http_vmap))
            .route("/ads/v1/t", get(http_track))
            .route("/ads/v1/creative/{campaign}/{hash}", get(http_creative))
            .route("/ads/v1/receipts", get(http_receipts))
            .with_state(self.clone())
    }

    fn origin(&self, headers: &HeaderMap) -> String {
        if let Some(o) = &self.0.public_origin {
            return o.clone();
        }
        let h = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).map(|s| s.split(',').next().unwrap_or("").trim().to_string());
        let host = h("x-forwarded-host")
            .or_else(|| h("host"))
            .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || ".-:[]".contains(c)))
            .unwrap_or_else(|| "127.0.0.1:7777".into());
        let proto = h("x-forwarded-proto").filter(|p| p == "https").unwrap_or_else(|| "http".into());
        format!("{proto}://{host}")
    }

    fn beacon_url(&self, origin: &str, ev: &str, tok: &str) -> String {
        format!("{origin}/ads/v1/t?ev={ev}&tok={tok}")
    }

    /// A served ad's beacon (and its serving id).
    fn beacon_for(&self, d: &select::Decision, placement: Placement, ctx: &AdContext, site: Option<String>) -> Beacon {
        Beacon {
            c: d.campaign.campaign_id.clone(),
            cr: d.creative.hash.clone(),
            p: placement,
            l: ctx.listener.clone().unwrap_or_default(),
            s: uuid::Uuid::new_v4().simple().to_string(),
            site,
            cc: ctx.country.clone(),
            exp: now_unix() + BEACON_TTL_SECS,
        }
    }
}

/// What a tracking URL carries (signed with this gateway's per-process key,
/// so beacons count only for ads this gateway actually served).
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Beacon {
    c: String,
    cr: String,
    p: Placement,
    l: String,
    s: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    site: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cc: Option<String>,
    exp: i64,
}

/// Query parameters every ad request takes.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct AdQuery {
    pub placement: Option<String>,
    /// Comma-separated categories.
    #[serde(alias = "cat")]
    pub categories: Option<String>,
    #[serde(alias = "kw")]
    pub keywords: Option<String>,
    pub country: Option<String>,
    /// Device / player id (hashed before use).
    pub uid: Option<String>,
    /// Floor CPM in USD.
    pub floor: Option<String>,
    pub slot: Option<String>,
    /// VMAP: comma-separated break offsets.
    pub breaks: Option<String>,
}

/// Anonymous listener id: a hash of the player id, else of IP + user agent.
pub fn listener_id(headers: &HeaderMap, uid: Option<&str>) -> Option<String> {
    let hashed = |s: String| Some(Hash::of(s.as_bytes()).to_hex()[..16].to_string());
    if let Some(u) = uid.filter(|u| !u.is_empty() && u.len() <= 256) {
        return hashed(format!("c0mpute-ads-uid\n{u}"));
    }
    let h = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let ip = h("x-forwarded-for").split(',').next().unwrap_or("").trim().to_string();
    let ip = if ip.is_empty() { h("x-real-ip") } else { ip };
    let ua = h("user-agent");
    if ip.is_empty() && ua.is_empty() {
        return None;
    }
    hashed(format!("c0mpute-ads-listener\n{ip}\n{ua}"))
}

/// Country from the query, else a CDN/proxy header; unknown otherwise (no
/// GeoIP database on gateways).
pub fn country_of(headers: &HeaderMap, q: Option<&str>) -> Option<String> {
    let from_header = || {
        ["cf-ipcountry", "x-country", "x-geo-country", "cloudfront-viewer-country"]
            .iter()
            .find_map(|k| headers.get(*k).and_then(|v| v.to_str().ok()).map(String::from))
    };
    let c = q.map(String::from).or_else(from_header)?.trim().to_ascii_uppercase();
    (c.len() == 2 && c.chars().all(|x| x.is_ascii_uppercase()) && c != "XX" && c != "T1").then_some(c)
}

impl Ads {
    fn context(&self, q: &AdQuery, headers: &HeaderMap, placement: Option<Placement>) -> AdContext {
        AdContext {
            placement,
            categories: campaign::norm_terms(q.categories.iter()),
            keywords: campaign::norm_terms(q.keywords.iter()),
            country: country_of(headers, q.country.as_deref()),
            listener: listener_id(headers, q.uid.as_deref()),
            now: now_unix(),
            floor_micros: q.floor.as_deref().and_then(|f| campaign::parse_usd_micros(f).ok()).unwrap_or(0),
            slot_key: q.slot.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            exclude: vec![],
            max_duration_ms: None,
        }
    }
}

fn cors(mut r: Response) -> Response {
    r.headers_mut()
        .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    r
}

fn xml(body: String) -> Response {
    let mut r = (StatusCode::OK, body).into_response();
    r.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/xml; charset=utf-8"));
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    cors(r)
}

async fn http_campaigns(State(ads): State<Ads>) -> Response {
    let now = now_unix();
    let list: Vec<serde_json::Value> = ads
        .campaigns()
        .await
        .into_iter()
        .map(|c| {
            serde_json::json!({
                "campaign_id": c.campaign_id,
                "name": c.name,
                "owner": c.owner,
                "seq": c.seq,
                "active": !c.paused && c.schedule.contains(now),
                "paused": c.paused,
                "placements": c.targeting.placements,
                "max_cpm_usd": c.max_cpm_micros as f64 / campaign::MICROS as f64,
                "creatives": c.creatives.len(),
                "landing_url": c.landing_url,
            })
        })
        .collect();
    cors(axum::Json(serde_json::json!({ "gateway": ads.gateway_key(), "campaigns": list })).into_response())
}

async fn http_select(State(ads): State<Ads>, Query(q): Query<AdQuery>, headers: HeaderMap) -> Response {
    let Some(placement) = q.placement.as_deref().and_then(Placement::parse) else {
        return (StatusCode::BAD_REQUEST, "placement= one of podcast_pre, podcast_mid, podcast_post, ctv_preroll, ctv_midroll, ctv_postroll, display").into_response();
    };
    let ctx = ads.context(&q, &headers, Some(placement));
    let e = ads.select(&ctx).await;
    let origin = ads.origin(&headers);
    let winner = e.winner.as_ref().map(|d| {
        let b = ads.beacon_for(d, placement, &ctx, None);
        let tok = ads.sign_beacon(&b);
        serde_json::json!({
            "campaign_id": d.campaign.campaign_id,
            "name": d.campaign.name,
            "bid_cpm_usd": d.bid_micros as f64 / campaign::MICROS as f64,
            "rank": d.rank,
            "creative": d.creative,
            "creative_url": format!("{origin}/ads/v1/creative/{}/{}", d.campaign.campaign_id, d.creative.hash),
            "landing_url": d.campaign.landing_url,
            "impression_url": ads.beacon_url(&origin, "impression", &tok),
            "click_url": ads.beacon_url(&origin, "click", &tok),
        })
    });
    let rejected: Vec<serde_json::Value> = e
        .rejected
        .iter()
        .map(|(id, name, why)| serde_json::json!({"campaign_id": id, "name": name, "reason": why}))
        .collect();
    cors(
        axum::Json(serde_json::json!({
            "placement": placement,
            "context": {
                "categories": ctx.categories,
                "keywords": ctx.keywords,
                "country": ctx.country,
                "listener": ctx.listener,
                "floor_cpm_usd": ctx.floor_micros as f64 / campaign::MICROS as f64,
            },
            "winner": winner,
            "rejected": rejected,
        }))
        .into_response(),
    )
}

async fn http_vast(State(ads): State<Ads>, Query(q): Query<AdQuery>, headers: HeaderMap) -> Response {
    let placement = match q.placement.as_deref() {
        None => Placement::CtvPreroll,
        Some(p) => match Placement::parse(p) {
            Some(p) if p.is_ctv() => p,
            _ => return (StatusCode::BAD_REQUEST, "VAST placement is ctv_preroll, ctv_midroll or ctv_postroll").into_response(),
        },
    };
    if !ads.0.serve || ads.0.signer.is_none() {
        return xml(vast::empty());
    }
    let ctx = ads.context(&q, &headers, Some(placement));
    let Some(d) = ads.select(&ctx).await.winner else {
        return xml(vast::empty());
    };
    let origin = ads.origin(&headers);
    let b = ads.beacon_for(&d, placement, &ctx, None);
    let tok = ads.sign_beacon(&b);
    let ad = vast::VastAd {
        ad_id: d.campaign.campaign_id.clone(),
        serving_id: b.s.clone(),
        title: d.campaign.name.clone(),
        advertiser: format!("c0mpute:{}", &d.campaign.owner[..16.min(d.campaign.owner.len())]),
        creative_id: d.creative.hash.clone(),
        media_url: format!("{origin}/ads/v1/creative/{}/{}", d.campaign.campaign_id, d.creative.hash),
        mime: d.creative.mime.clone(),
        width: d.creative.width.unwrap_or(1280),
        height: d.creative.height.unwrap_or(720),
        duration_ms: d.creative.duration_ms.unwrap_or(0),
        bytes: d.creative.bytes,
        impression_url: ads.beacon_url(&origin, "impression", &tok),
        tracking: ["start", "firstQuartile", "midpoint", "thirdQuartile", "complete"]
            .iter()
            .map(|e| (e.to_string(), ads.beacon_url(&origin, e, &tok)))
            .collect(),
        click_through: d.campaign.landing_url.clone(),
        click_tracking: ads.beacon_url(&origin, "click", &tok),
        error_url: format!("{origin}/ads/v1/t?ev=error&code=[ERRORCODE]&tok={tok}"),
    };
    xml(vast::inline(&ad))
}

async fn http_vmap(State(ads): State<Ads>, Query(q): Query<AdQuery>, headers: HeaderMap) -> Response {
    let origin = ads.origin(&headers);
    let list = q.breaks.clone().unwrap_or_else(|| "start".into());
    let mut breaks = Vec::new();
    let mut mid = 0;
    for raw in list.split(',').filter(|s| !s.trim().is_empty()).take(32) {
        let Some(offset) = vast::parse_offset(raw) else {
            return (StatusCode::BAD_REQUEST, format!("bad break offset {raw:?}: use start, end, seconds or HH:MM:SS")).into_response();
        };
        let (id, placement) = match offset.as_str() {
            "start" => ("preroll".to_string(), Placement::CtvPreroll),
            "end" => ("postroll".to_string(), Placement::CtvPostroll),
            _ => {
                mid += 1;
                (format!("midroll-{mid}"), Placement::CtvMidroll)
            }
        };
        let mut url = url::Url::parse(&format!("{origin}/ads/v1/vast")).expect("origin is a URL");
        {
            let mut qp = url.query_pairs_mut();
            qp.append_pair("placement", placement.as_str());
            for (k, v) in [
                ("categories", &q.categories),
                ("keywords", &q.keywords),
                ("country", &q.country),
                ("uid", &q.uid),
                ("floor", &q.floor),
            ] {
                if let Some(v) = v {
                    qp.append_pair(k, v);
                }
            }
            if let Some(s) = &q.slot {
                qp.append_pair("slot", &format!("{s}/{id}"));
            }
        }
        breaks.push(vast::Break {
            id,
            time_offset: offset,
            vast_url: url.to_string(),
        });
    }
    xml(vast::vmap(&breaks))
}

#[derive(Deserialize)]
struct TrackQuery {
    ev: String,
    tok: String,
}

/// 1x1 transparent GIF.
const PIXEL: &[u8] = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff\x21\xf9\x04\x01\x00\x00\x00\x00\x2c\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02\x44\x01\x00\x3b";

async fn http_track(State(ads): State<Ads>, Query(q): Query<TrackQuery>) -> Response {
    let pixel = || {
        let mut r = (StatusCode::OK, PIXEL).into_response();
        r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("image/gif"));
        r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        cors(r)
    };
    let Some(b) = ads.open_beacon(&q.tok) else {
        return cors((StatusCode::FORBIDDEN, "unknown or expired tracking token").into_response());
    };
    if q.ev == "error" {
        return pixel();
    }
    let Some(ev) = Event::parse(&q.ev) else {
        return cors((StatusCode::BAD_REQUEST, "unknown event").into_response());
    };
    ads.beacon_event(&b, ev);
    pixel()
}

async fn http_creative(
    State(ads): State<Ads>,
    AxPath((campaign_id, hash)): AxPath<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let Some(c) = ads.campaign(&campaign_id).await else {
        return cors((StatusCode::NOT_FOUND, "unknown campaign").into_response());
    };
    let Some(cr) = c.creative(&hash).cloned() else {
        return cors((StatusCode::NOT_FOUND, "unknown creative").into_response());
    };
    match ads.creative_bytes(&c, &cr).await {
        Ok(bytes) => {
            let mut r = bytes_response(bytes, &cr.mime, &headers);
            r.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=31536000, immutable"),
            );
            cors(r)
        }
        Err(e) => {
            warn!(campaign = %campaign_id, err = %e, "ads: creative unavailable");
            cors((StatusCode::BAD_GATEWAY, "creative is unavailable right now").into_response())
        }
    }
}

#[derive(Deserialize)]
struct ReceiptsQuery {
    campaign: String,
    #[serde(default)]
    since: u64,
}

async fn http_receipts(State(ads): State<Ads>, Query(q): Query<ReceiptsQuery>) -> Response {
    ads.flush_receipts().await;
    let batches = ads.local_batches(&q.campaign, q.since);
    cors(axum::Json(serde_json::json!({ "gateway": ads.gateway_key(), "batches": batches })).into_response())
}

/// A full body, or one `Range: bytes=a-b` of it (players seek in media).
pub fn bytes_response(bytes: Vec<u8>, mime: &str, headers: &HeaderMap) -> Response {
    let total = bytes.len() as u64;
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| parse_range(v, total));
    let (status, body, content_range) = match range {
        Some((a, b)) => (
            StatusCode::PARTIAL_CONTENT,
            bytes[a as usize..=b as usize].to_vec(),
            Some(format!("bytes {a}-{b}/{total}")),
        ),
        None => (StatusCode::OK, bytes, None),
    };
    let len = body.len();
    let mut r = (status, Body::from(body)).into_response();
    let h = r.headers_mut();
    if let Ok(v) = HeaderValue::from_str(mime) {
        h.insert(header::CONTENT_TYPE, v);
    }
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Some(cr) = content_range.and_then(|c| HeaderValue::from_str(&c).ok()) {
        h.insert(header::CONTENT_RANGE, cr);
    }
    r
}

/// One satisfiable range of `bytes=a-b`, `bytes=a-` or `bytes=-n`.
pub fn parse_range(v: &str, total: u64) -> Option<(u64, u64)> {
    let spec = v.strip_prefix("bytes=")?;
    if spec.contains(',') || total == 0 {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (start, end) = match (a.trim(), b.trim()) {
        ("", n) => {
            let n: u64 = n.parse().ok()?;
            (total.saturating_sub(n), total - 1)
        }
        (a, "") => (a.parse().ok()?, total - 1),
        (a, b) => (a.parse().ok()?, b.parse::<u64>().ok()?.min(total - 1)),
    };
    (start <= end && start < total).then_some((start, end))
}

// ── podcast dynamic ad insertion ─────────────────────────────────────────

struct PodcastHook(Ads);

fn wants_ads(query: &str) -> bool {
    url::form_urlencoded::parse(query.as_bytes()).any(|(k, v)| k == "ads" && (v == "1" || v == "true" || v == "yes"))
}

#[async_trait::async_trait]
impl ServeHook for PodcastHook {
    async fn serve(&self, hosting: &Hosting, rec: &SiteRecord, path: &str, query: &str, headers: &HeaderMap) -> Option<Response> {
        if !wants_ads(query) {
            return None;
        }
        let (resp, why) = match self.0.podcast_dai(hosting, rec, path, headers).await {
            Ok(r) => (r, None),
            Err(why) => (hosting.serve(rec, path).await, Some(why)),
        };
        let mut resp = resp;
        if let Some(why) = why
            && resp.status().is_success()
            && let Ok(v) = HeaderValue::from_str(&format!("none; {why}"))
        {
            resp.headers_mut().insert("x-c0mpute-ads", v);
        }
        Some(resp)
    }
}

impl Ads {
    /// Read a show's opt-in, if its signed site carries one.
    pub async fn show_opt_in(&self, hosting: &Hosting, rec: &SiteRecord) -> Option<stitch::ShowAds> {
        let (_, bytes) = hosting.read_file(rec, stitch::OPT_IN_FILE).await.ok()??;
        stitch::ShowAds::parse(&bytes).ok().filter(|s| s.enabled)
    }

    /// Serve an episode with ads spliced in, or say why not (and the caller
    /// serves it clean).
    async fn podcast_dai(&self, hosting: &Hosting, rec: &SiteRecord, path: &str, headers: &HeaderMap) -> std::result::Result<Response, String> {
        let entry = hosting
            .resolve(rec, path)
            .await
            .map_err(|_| "site unavailable".to_string())?
            .ok_or("no such file")?;
        if !entry.content_type.starts_with("audio/") {
            return Err("not an audio file".into());
        }
        let Some(opt) = self.show_opt_in(hosting, rec).await else {
            return Err("show has not opted in to ads".into());
        };
        if !self.0.serve || self.0.signer.is_none() {
            return Err("this gateway does not serve ads".into());
        }
        let (Some(ffmpeg), Some(ffprobe)) = (self.0.ffmpeg.clone(), self.0.ffprobe.clone()) else {
            return Err("no ffmpeg on this gateway".into());
        };
        let listener = listener_id(headers, None);
        let path_key = path.trim_start_matches('/').split(['?', '#']).next().unwrap_or("").to_string();
        let sticky = Hash::of(
            format!("{}\n{}\n{}\n{}", listener.as_deref().unwrap_or(""), rec.site_id, rec.manifest, path_key).as_bytes(),
        )
        .to_hex();
        let fresh = |s: &Stitched| s.created.elapsed() < self.0.timing.stitch_ttl;
        let cached = {
            let st = self.0.stitched.lock().unwrap();
            st.get(&sticky).filter(|s| fresh(s)).map(|s| (s.path.clone(), s.mime.clone(), s.summary.clone()))
        };
        if let Some((p, mime, summary)) = cached
            && let Ok(bytes) = tokio::fs::read(&p).await
        {
            return Ok(with_ads_header(bytes_response(bytes, &mime, headers), &summary));
        }

        // Choose one campaign per break the show sells.
        let mut ctx = AdContext {
            placement: None,
            categories: opt.categories.clone(),
            keywords: opt.keywords.clone(),
            country: country_of(headers, None),
            listener: listener.clone(),
            now: now_unix(),
            floor_micros: opt.floor_cpm_micros,
            slot_key: String::new(),
            exclude: vec![],
            max_duration_ms: Some(opt.max_ad_secs.saturating_mul(1000)),
        };
        let mut chosen: Vec<(Placement, select::Decision)> = Vec::new();
        for p in [Placement::PodcastPre, Placement::PodcastMid, Placement::PodcastPost] {
            if !opt.allows(p) {
                continue;
            }
            ctx.placement = Some(p);
            ctx.slot_key = format!("{}/{}/{}/{}", rec.site_id, path_key, p, listener.as_deref().unwrap_or(""));
            if let Some(d) = self.select(&ctx).await.winner {
                ctx.exclude.push(d.campaign.campaign_id.clone());
                chosen.push((p, d));
            }
        }
        if chosen.is_empty() {
            return Err("no campaign matched".into());
        }

        let _slot = self.0.stitch_slots.acquire().await.map_err(|_| "shutting down".to_string())?;
        let work = self.stitch_dir().join(uuid::Uuid::new_v4().simple().to_string());
        std::fs::create_dir_all(&work).map_err(|e| format!("cache dir: {e}"))?;
        let cleanup = |w: &std::path::Path| {
            let _ = std::fs::remove_dir_all(w);
        };
        let ext = std::path::Path::new(&path_key)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("mp3")
            .to_ascii_lowercase();
        let Some((_, out_mime)) = stitch::encoder_for(&ext, None) else {
            cleanup(&work);
            return Err(format!("cannot encode .{ext}"));
        };
        let Ok(Some((_, episode))) = hosting.read_file(rec, &path_key).await else {
            cleanup(&work);
            return Err("episode unavailable".into());
        };
        let ep_path = work.join(format!("episode.{ext}"));
        std::fs::write(&ep_path, &episode).map_err(|e| e.to_string())?;
        let info = stitch::probe(&ffprobe, &ep_path).await.unwrap_or_default();
        let mut plan = stitch::StitchPlan {
            episode: ep_path,
            pre: None,
            mid: None,
            post: None,
            out: work.join(format!("out.{ext}")),
            sample_rate: info.sample_rate.unwrap_or(44_100),
            channels: info.channels.unwrap_or(2),
            bit_rate: info.bit_rate,
        };
        let mut used: Vec<(Placement, &select::Decision)> = Vec::new();
        for (p, d) in &chosen {
            let bytes = match self.creative_bytes(&d.campaign, &d.creative).await {
                Ok(b) => b,
                Err(e) => {
                    warn!(campaign = %d.campaign.campaign_id, err = %e, "ads: creative unavailable; skipping");
                    continue;
                }
            };
            let cext = std::path::Path::new(&d.creative.path)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("bin");
            let cpath = work.join(format!("{}.{cext}", p.as_str()));
            if std::fs::write(&cpath, &bytes).is_err() {
                continue;
            }
            match p {
                Placement::PodcastPre => plan.pre = Some(cpath),
                Placement::PodcastPost => plan.post = Some(cpath),
                _ => {
                    let Some(dur) = info.duration_secs else {
                        continue;
                    };
                    let at = opt.mid_at_secs.map(|s| s as f64).unwrap_or(dur / 2.0);
                    if at <= 0.0 || at >= dur {
                        continue;
                    }
                    plan.mid = Some((cpath, at));
                }
            }
            used.push((*p, d));
        }
        if plan.ad_count() == 0 {
            cleanup(&work);
            return Err("no creative could be fetched".into());
        }
        if let Err(e) = stitch::run(&ffmpeg, &plan, self.0.timing.stitch_timeout).await {
            warn!(site = %rec.site_id, err = %e, "ads: stitch failed; serving the clean episode");
            cleanup(&work);
            return Err("stitch failed".into());
        }
        let Ok(bytes) = tokio::fs::read(&plan.out).await else {
            cleanup(&work);
            return Err("stitch output missing".into());
        };
        // Keep only the output for reuse.
        let kept = self.stitch_dir().join(format!("{sticky}.{ext}"));
        let _ = std::fs::rename(&plan.out, &kept);
        cleanup(&work);

        let summary = used
            .iter()
            .map(|(p, d)| format!("{}={}", p.as_str(), d.campaign.campaign_id))
            .collect::<Vec<_>>()
            .join(",");
        for (p, d) in &used {
            let b = self.beacon_for(d, *p, &ctx, Some(rec.site_id.clone()));
            self.beacon_event(&b, Event::Impression);
        }
        info!(site = %rec.site_id, path = %path_key, ads = %summary, "ads: stitched episode");
        self.0.stitched.lock().unwrap().insert(
            sticky,
            Stitched {
                path: kept,
                mime: out_mime.to_string(),
                created: Instant::now(),
                summary: summary.clone(),
            },
        );
        Ok(with_ads_header(bytes_response(bytes, out_mime, headers), &summary))
    }

    fn stitch_dir(&self) -> PathBuf {
        self.0
            .data_dir
            .clone()
            .unwrap_or_else(std::env::temp_dir)
            .join("stitched")
    }
}

fn with_ads_header(mut r: Response, summary: &str) -> Response {
    if let Ok(v) = HeaderValue::from_str(&format!("inserted; {summary}")) {
        r.headers_mut().insert("x-c0mpute-ads", v);
    }
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-9", 100), Some((0, 9)));
        assert_eq!(parse_range("bytes=90-", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=-10", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=50-500", 100), Some((50, 99)));
        assert_eq!(parse_range("bytes=100-", 100), None);
        assert_eq!(parse_range("bytes=5-1", 100), None);
        assert_eq!(parse_range("bytes=0-1,5-6", 100), None);
        assert_eq!(parse_range("items=0-1", 100), None);
    }

    #[test]
    fn listener_and_country() {
        let mut h = HeaderMap::new();
        assert_eq!(listener_id(&h, None), None);
        h.insert("x-forwarded-for", "203.0.113.9, 10.0.0.1".parse().unwrap());
        h.insert("user-agent", "Overcast/3.0".parse().unwrap());
        let a = listener_id(&h, None).unwrap();
        assert_eq!(a.len(), 16);
        assert_eq!(listener_id(&h, None).unwrap(), a, "stable");
        assert_ne!(listener_id(&h, Some("device-1")).unwrap(), a);
        assert!(!listener_id(&h, Some("device-1")).unwrap().contains("device"));
        assert_eq!(country_of(&h, None), None);
        h.insert("cf-ipcountry", "de".parse().unwrap());
        assert_eq!(country_of(&h, None).as_deref(), Some("DE"));
        assert_eq!(country_of(&h, Some("us")).as_deref(), Some("US"));
        assert_eq!(country_of(&h, Some("XX")), None);
        assert_eq!(country_of(&h, Some("usa")), None);
    }

    #[test]
    fn ads_query_flag() {
        assert!(wants_ads("ads=1"));
        assert!(wants_ads("x=2&ads=true"));
        assert!(!wants_ads("ads=0"));
        assert!(!wants_ads("download=1"));
    }

    #[test]
    fn gossip_messages_round_trip_and_fit() {
        let kp = Keypair::generate_ed25519();
        let rec = campaign::tests::sample(&kp, "x", 1).sign(&kp).unwrap();
        let m = AdsMsg::Campaign { record: rec.clone() };
        let bytes = serde_json::to_vec(&m).unwrap();
        assert!(bytes.len() < 64 * 1024);
        match serde_json::from_slice::<AdsMsg>(&bytes).unwrap() {
            AdsMsg::Campaign { record } => assert!(record.verify()),
            _ => panic!(),
        }
        let w = serde_json::to_vec(&AdsMsg::ReportWant { campaign_id: "c".into(), since: 0 }).unwrap();
        assert!(matches!(serde_json::from_slice::<AdsMsg>(&w).unwrap(), AdsMsg::ReportWant { .. }));
    }
}
