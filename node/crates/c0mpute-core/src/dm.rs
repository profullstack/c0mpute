//! Secure-chat transport (DIP-0018): mailboxes, direct delivery and key
//! lookup over gossip plus `/c0mpute/rpc/1.0.0`. No coordinator anywhere.
//!
//! * Every mailbox node (the `mailbox` role, on by default; gateways too)
//!   gossips "mailbox here" on [`TOPIC`] once a minute. From that every node
//!   knows the mailbox set.
//! * A sender seals the message ([`c0mpute_secure_chat::sealed`]) and first
//!   looks for the recipient's home node: a [`HomeRecord`] signed by the
//!   recipient naming a libp2p peer, gossiped by that peer while it is up.
//!   If the home answers, the message is delivered directly into its spool,
//!   where the recipient's local `c0mpute dm` reads it.
//! * Otherwise the sealed message goes to the [`STORE_REPLICAS`] mailbox
//!   nodes that rank highest for its recipient tag (rendezvous hashing, the
//!   same on every node). They keep it up to 30 days, within size, count and
//!   per-sender rate caps. The recipient polls the top [`PULL_REPLICAS`] for
//!   each of its tags, opens what it finds, and acks; the ack reveals a
//!   secret only the recipient could read, which deletes the message.
//! * Key lookup: users publish a self-signed [`KeyRecord`]; mailbox nodes
//!   cache it (gossip plus a direct copy to the nodes ranked for its
//!   fingerprint). A lookup by fingerprint or address asks those nodes and
//!   checks the answer against what was asked: the address hashes every
//!   public key, so a node cannot substitute another key.
//!
//! Mailbox nodes see recipient tags, sizes and the peer that handed them a
//! message; never sender identities, recipient addresses or content.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::prelude::*;
use c0mpute_net::{GossipMessage, Libp2pNetwork, PeerId, RpcHandler};
use c0mpute_secure_chat::identity::{matches_query, normalize_query};
use c0mpute_secure_chat::mailbox::{Limits, Mailbox, Put};
use c0mpute_secure_chat::placement::{PULL_REPLICAS, STORE_REPLICAS, place, rank};
use c0mpute_secure_chat::sealed::{Sealed, SealedMessage, is_hex32, pull_tags};
use c0mpute_secure_chat::{HomeRecord, KeyRecord, PublicIdentity, unix_now};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, info};

pub const TOPIC: &str = "c0mpute/dm/v1";
pub const RPC_SERVICE: &str = "dm/1";
pub const ROLE_TAG: &str = "c0mpute:role:mailbox";
/// Key records a node caches.
const MAX_KEYS: usize = 20_000;
/// Tags per fetch request.
const MAX_FETCH_TAGS: usize = 16;
/// Direct deliveries accepted per sending peer per minute.
const DELIVER_PER_MIN: usize = 60;

/// How often things happen. Tests shrink these.
#[derive(Clone, Debug)]
pub struct Timing {
    pub announce_every: Duration,
    /// A mailbox or home not heard from for this long is gone.
    pub peer_ttl: Duration,
    pub rpc_timeout: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            announce_every: Duration::from_secs(60),
            peer_ttl: Duration::from_secs(200),
            rpc_timeout: Duration::from_secs(20),
        }
    }
}

/// Where a node keeps its secure-chat files, under its storage root.
#[derive(Clone, Debug)]
pub struct Paths {
    /// Other people's sealed messages (the mailbox role).
    pub mailbox: PathBuf,
    /// Direct deliveries for the users whose home this node is.
    pub spool: PathBuf,
    /// Signed home records (`<address>.json`) the local `c0mpute dm` wrote.
    pub homes: PathBuf,
}

impl Paths {
    pub fn under(root: &Path) -> Self {
        let d = root.join("dm");
        Self {
            mailbox: d.join("mailbox"),
            spool: d.join("spool"),
            homes: d.join("homes"),
        }
    }
}

pub struct Options {
    /// Hold messages for others (the mailbox role).
    pub serve_mailbox: bool,
    /// Persist under these paths; `None` = memory only, no home/spool.
    pub paths: Option<Paths>,
    pub limits: Limits,
    pub timing: Timing,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            serve_mailbox: false,
            paths: None,
            limits: Limits::default(),
            timing: Timing::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum Gossip {
    Mailbox { addrs: Vec<String> },
    Key { record: KeyRecord },
    Home {
        record: HomeRecord,
        /// Dial hints for the home peer (unsigned: the peer id in the
        /// signed record is what authenticates the connection).
        #[serde(default)]
        addrs: Vec<String>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Request {
    Store { msg: String, ack_hash: String, ttl: u64 },
    Fetch { tags: Vec<String> },
    Ack { tag: String, id: String, secret: String },
    Deliver { msg: String, ack_hash: String, ttl: u64 },
    Lookup { query: String },
    PublishKey { record: KeyRecord },
    Presence { address: String },
    Peers,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Item {
    pub tag: String,
    pub id: String,
    pub msg: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PeerInfo {
    pub peer: String,
    pub addrs: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Response {
    Stored { duplicate: bool },
    Messages { items: Vec<Item> },
    Acked { deleted: bool },
    Delivered,
    Key { record: Option<KeyRecord> },
    Presence {
        record: Option<HomeRecord>,
        #[serde(default)]
        addrs: Vec<String>,
    },
    Peers { peers: Vec<PeerInfo> },
    Ok,
}

/// One message found by [`Dm::fetch`], with the mailboxes holding it.
#[derive(Clone, Debug)]
pub struct Fetched {
    pub tag: String,
    pub id: String,
    pub msg: SealedMessage,
    pub holders: Vec<String>,
}

/// What [`Dm::send`] did.
#[derive(Clone, Debug, Default)]
pub struct SendReport {
    /// The recipient's home node, when it took the message directly.
    pub direct: Option<String>,
    /// Mailboxes that stored it (empty after a direct delivery).
    pub stored_on: Vec<String>,
    /// Why direct delivery or individual mailboxes failed.
    pub errors: Vec<String>,
}

struct Inner {
    net: Arc<Libp2pNetwork>,
    me: String,
    serves: bool,
    limits: Limits,
    timing: Timing,
    paths: Option<Paths>,
    mailbox: Mutex<Mailbox>,
    /// Mailbox peers: last heard, dial addresses.
    mailboxes: RwLock<HashMap<String, (Instant, Vec<String>)>>,
    keys: RwLock<HashMap<String, KeyRecord>>,
    /// Remote presence: address -> (record, last gossiped by its peer).
    homes: RwLock<HashMap<String, (HomeRecord, Instant, Vec<String>)>>,
    deliver_rate: Mutex<HashMap<String, VecDeque<u64>>>,
    stopped: std::sync::atomic::AtomicBool,
}

/// The secure-chat service: one per node (or per `c0mpute dm` client).
#[derive(Clone)]
pub struct Dm(Arc<Inner>);

struct Handler(Dm);

#[async_trait::async_trait]
impl RpcHandler for Handler {
    async fn handle(&self, peer: PeerId, body: Vec<u8>) -> std::result::Result<Vec<u8>, String> {
        let req: Request = serde_json::from_slice(&body).map_err(|e| format!("bad request: {e}"))?;
        let resp = self.0.serve(&peer.to_base58(), req).await.map_err(|e| e.to_string())?;
        serde_json::to_vec(&resp).map_err(|e| e.to_string())
    }
}

fn b64d(s: &str) -> Result<Vec<u8>> {
    BASE64_URL_SAFE_NO_PAD.decode(s).context("bad base64")
}

fn b64e(b: &[u8]) -> String {
    BASE64_URL_SAFE_NO_PAD.encode(b)
}

impl Dm {
    pub async fn start(net: Arc<Libp2pNetwork>, opts: Options) -> Result<Self> {
        let now = unix_now();
        let mailbox = match (&opts.paths, opts.serve_mailbox) {
            (Some(p), true) => Mailbox::open(&p.mailbox, opts.limits.clone(), now),
            _ => Mailbox::new(opts.limits.clone()),
        };
        let dm = Dm(Arc::new(Inner {
            me: net.peer_id().to_base58(),
            net: net.clone(),
            serves: opts.serve_mailbox,
            limits: opts.limits,
            timing: opts.timing,
            paths: opts.paths,
            mailbox: Mutex::new(mailbox),
            mailboxes: RwLock::new(HashMap::new()),
            keys: RwLock::new(HashMap::new()),
            homes: RwLock::new(HashMap::new()),
            deliver_rate: Mutex::new(HashMap::new()),
            stopped: std::sync::atomic::AtomicBool::new(false),
        }));
        net.subscribe(TOPIC).await.context("subscribe to dm topic")?;
        net.register_rpc(RPC_SERVICE, Arc::new(Handler(dm.clone())));
        dm.spawn_gossip_reader();
        dm.spawn_announcer();
        if dm.0.serves {
            info!("secure-chat: serving as a mailbox");
        }
        Ok(dm)
    }

    pub fn peer_id(&self) -> &str {
        &self.0.me
    }

    pub fn serves_mailbox(&self) -> bool {
        self.0.serves
    }

    pub fn stop(&self) {
        self.0.stopped.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Messages this node holds for others.
    pub fn mailbox_count(&self) -> usize {
        self.0.mailbox.lock().map(|m| m.count()).unwrap_or(0)
    }

    // ── gossip ───────────────────────────────────────────────────────────

    fn spawn_gossip_reader(&self) {
        let dm = self.clone();
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
                let Ok(g) = serde_json::from_slice::<Gossip>(&msg.data) else {
                    continue;
                };
                dm.apply_gossip(source, g).await;
            }
        });
    }

    async fn apply_gossip(&self, source: PeerId, g: Gossip) {
        let src = source.to_base58();
        let now = unix_now();
        match g {
            Gossip::Mailbox { addrs } => {
                let addrs: Vec<String> = addrs.into_iter().take(8).collect();
                for a in &addrs {
                    if let Ok(ma) = a.parse::<c0mpute_net::Multiaddr>() {
                        self.0.net.add_peer_address(source, ma).await;
                    }
                }
                self.0.mailboxes.write().await.insert(src, (Instant::now(), addrs));
            }
            Gossip::Key { record } => {
                if record.verify(now).is_ok() {
                    self.cache_key(record).await;
                }
            }
            Gossip::Home { record, addrs } => {
                // Presence counts only from the named peer itself.
                if record.peer == src && record.verify(now).is_ok() {
                    self.learn_home(record, addrs).await;
                }
            }
        }
    }

    /// Remember a verified home record and teach libp2p how to dial its peer.
    async fn learn_home(&self, record: HomeRecord, addrs: Vec<String>) {
        let addrs: Vec<String> = addrs.into_iter().take(8).collect();
        if let Ok(pid) = record.peer.parse::<PeerId>() {
            for a in &addrs {
                if let Ok(ma) = a.parse::<c0mpute_net::Multiaddr>() {
                    self.0.net.add_peer_address(pid, ma).await;
                }
            }
        }
        let addr = record.address();
        self.0.homes.write().await.insert(addr, (record, Instant::now(), addrs));
    }

    async fn cache_key(&self, record: KeyRecord) {
        let addr = record.address();
        let mut keys = self.0.keys.write().await;
        if let Some(cur) = keys.get(&addr)
            && cur.created_at >= record.created_at
        {
            return;
        }
        if keys.len() >= MAX_KEYS && !keys.contains_key(&addr) {
            let now = unix_now();
            keys.retain(|_, r| r.expires_at > now);
            if keys.len() >= MAX_KEYS
                && let Some(oldest) = keys.iter().min_by_key(|(_, r)| r.expires_at).map(|(a, _)| a.clone())
            {
                keys.remove(&oldest);
            }
        }
        keys.insert(addr, record);
    }

    async fn publish(&self, g: &Gossip) {
        if let Ok(bytes) = serde_json::to_vec(g)
            && let Err(e) = self.0.net.publish(TOPIC, bytes).await
        {
            debug!(err = %e, "dm: publish (mesh forming?)");
        }
    }

    async fn my_addrs(&self) -> Vec<String> {
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

    /// Home records in `<homes>/` naming this node, verified and live.
    pub fn local_homes(&self) -> Vec<HomeRecord> {
        let Some(p) = &self.0.paths else { return Vec::new() };
        let now = unix_now();
        let Ok(rd) = std::fs::read_dir(&p.homes) else { return Vec::new() };
        rd.flatten()
            .filter_map(|e| std::fs::read(e.path()).ok())
            .filter_map(|b| serde_json::from_slice::<HomeRecord>(&b).ok())
            .filter(|r| r.peer == self.0.me && r.verify(now).is_ok())
            .collect()
    }

    /// Make this node the home of `record`'s address (writes it under
    /// `homes/`, then gossips it). The `c0mpute dm` client does the same by
    /// writing the file; the announcer picks it up.
    pub async fn add_local_home(&self, record: &HomeRecord) -> Result<()> {
        let p = self.0.paths.as_ref().ok_or_else(|| anyhow!("no data dir for home records"))?;
        anyhow::ensure!(record.peer == self.0.me, "home record names another peer");
        record.verify(unix_now())?;
        write_home_record(&p.homes, record)?;
        let addrs = self.my_addrs().await;
        self.publish(&Gossip::Home {
            record: record.clone(),
            addrs,
        })
        .await;
        Ok(())
    }

    /// Gossip once: mailbox presence and the homes this node serves.
    pub async fn announce(&self) {
        if self.0.serves {
            let addrs = self.my_addrs().await;
            self.0
                .mailboxes
                .write()
                .await
                .insert(self.0.me.clone(), (Instant::now(), addrs.clone()));
            self.publish(&Gossip::Mailbox { addrs }).await;
        }
        let homes = self.local_homes();
        if !homes.is_empty() {
            let addrs = self.my_addrs().await;
            for record in homes {
                self.publish(&Gossip::Home {
                    record,
                    addrs: addrs.clone(),
                })
                .await;
            }
        }
    }

    fn spawn_announcer(&self) {
        let dm = self.clone();
        tokio::spawn(async move {
            loop {
                if dm.0.stopped.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                dm.announce().await;
                let ttl = dm.0.timing.peer_ttl;
                dm.0.mailboxes
                    .write()
                    .await
                    .retain(|_, (t, _)| t.elapsed() <= ttl);
                dm.0.homes.write().await.retain(|_, (r, t, _)| t.elapsed() <= ttl && r.expires_at > unix_now());
                if let Ok(mut mb) = dm.0.mailbox.lock() {
                    mb.prune(unix_now());
                }
                tokio::time::sleep(crate::hosting::jitter(dm.0.timing.announce_every)).await;
            }
        });
    }

    // ── serving ──────────────────────────────────────────────────────────

    async fn serve(&self, peer: &str, req: Request) -> Result<Response> {
        let now = unix_now();
        match req {
            Request::Store { msg, ack_hash, ttl } => {
                if !self.0.serves {
                    bail!("not a mailbox");
                }
                let bytes = b64d(&msg)?;
                let r = self
                    .0
                    .mailbox
                    .lock()
                    .map_err(|_| anyhow!("mailbox lock"))?
                    .put(peer, bytes, &ack_hash, ttl, now)?;
                Ok(Response::Stored {
                    duplicate: r == Put::Duplicate,
                })
            }
            Request::Fetch { tags } => {
                let mb = self.0.mailbox.lock().map_err(|_| anyhow!("mailbox lock"))?;
                let mut items = Vec::new();
                let mut budget = self.0.limits.max_fetch_bytes;
                for tag in tags.into_iter().take(MAX_FETCH_TAGS) {
                    for (id, bytes) in mb.fetch(&tag, now) {
                        if bytes.len() > budget && !items.is_empty() {
                            break;
                        }
                        budget = budget.saturating_sub(bytes.len());
                        items.push(Item {
                            tag: tag.clone(),
                            id,
                            msg: b64e(&bytes),
                        });
                    }
                }
                Ok(Response::Messages { items })
            }
            Request::Ack { tag, id, secret } => {
                let deleted = self
                    .0
                    .mailbox
                    .lock()
                    .map_err(|_| anyhow!("mailbox lock"))?
                    .ack(&tag, &id, &secret);
                Ok(Response::Acked { deleted })
            }
            Request::Deliver { msg, ack_hash, ttl } => {
                let p = self.0.paths.as_ref().ok_or_else(|| anyhow!("not a home node"))?;
                let bytes = b64d(&msg)?;
                let sealed = SealedMessage::parse(&bytes)?;
                let mine = self
                    .local_homes()
                    .iter()
                    .any(|h| pull_tags(&h.address(), now).contains(&sealed.tag));
                if !mine {
                    bail!("no user here for that tag");
                }
                {
                    let mut rate = self.0.deliver_rate.lock().map_err(|_| anyhow!("rate lock"))?;
                    let w = rate.entry(peer.to_string()).or_default();
                    while w.front().is_some_and(|t| *t + 60 <= now) {
                        w.pop_front();
                    }
                    if w.len() >= DELIVER_PER_MIN {
                        bail!("rate limited");
                    }
                    w.push_back(now);
                }
                // The spool is shared with the local client process, which
                // deletes what it has read: re-read it each time.
                let mut spool = Mailbox::open(&p.spool, self.0.limits.clone(), now);
                spool.put("direct", bytes, &ack_hash, ttl, now).map_err(|e| anyhow!("{e}"))?;
                Ok(Response::Delivered)
            }
            Request::Lookup { query } => {
                let q = normalize_query(&query).ok_or_else(|| anyhow!("bad query"))?;
                let record = self
                    .0
                    .keys
                    .read()
                    .await
                    .values()
                    .find(|r| matches_query(&r.identity, &q) && r.expires_at > now)
                    .cloned();
                Ok(Response::Key { record })
            }
            Request::PublishKey { record } => {
                record.verify(now)?;
                self.cache_key(record).await;
                Ok(Response::Ok)
            }
            Request::Presence { address } => {
                let ttl = self.0.timing.peer_ttl;
                let known = self
                    .0
                    .homes
                    .read()
                    .await
                    .get(&address)
                    .filter(|(r, t, _)| t.elapsed() <= ttl && r.expires_at > now)
                    .map(|(r, _, a)| (r.clone(), a.clone()));
                let (record, addrs) = match known {
                    Some((r, a)) => (Some(r), a),
                    None => match self.local_homes().into_iter().find(|h| h.address() == address) {
                        Some(r) => (Some(r), self.my_addrs().await),
                        None => (None, Vec::new()),
                    },
                };
                Ok(Response::Presence { record, addrs })
            }
            Request::Peers => {
                let ttl = self.0.timing.peer_ttl;
                let mut peers: Vec<PeerInfo> = self
                    .0
                    .mailboxes
                    .read()
                    .await
                    .iter()
                    .filter(|(_, (t, _))| t.elapsed() <= ttl)
                    .map(|(p, (_, a))| PeerInfo {
                        peer: p.clone(),
                        addrs: a.clone(),
                    })
                    .take(256)
                    .collect();
                if self.0.serves && !peers.iter().any(|p| p.peer == self.0.me) {
                    peers.push(PeerInfo {
                        peer: self.0.me.clone(),
                        addrs: self.my_addrs().await,
                    });
                }
                Ok(Response::Peers { peers })
            }
        }
    }

    // ── client ───────────────────────────────────────────────────────────

    async fn call(&self, peer: &str, req: &Request) -> Result<Response> {
        let pid: PeerId = peer.parse().map_err(|_| anyhow!("bad peer id {peer}"))?;
        let body = serde_json::to_vec(req)?;
        let out = tokio::time::timeout(self.0.timing.rpc_timeout, self.0.net.rpc(pid, RPC_SERVICE, body))
            .await
            .map_err(|_| anyhow!("{peer}: timed out"))??;
        Ok(serde_json::from_slice(&out)?)
    }

    /// Live mailbox peers this node knows, itself included when it serves.
    pub async fn mailboxes(&self) -> Vec<String> {
        let ttl = self.0.timing.peer_ttl;
        let mut v: Vec<String> = self
            .0
            .mailboxes
            .read()
            .await
            .iter()
            .filter(|(_, (t, _))| t.elapsed() <= ttl)
            .map(|(p, _)| p.clone())
            .collect();
        if self.0.serves && !v.contains(&self.0.me) {
            v.push(self.0.me.clone());
        }
        v
    }

    /// Ask every connected peer for the mailboxes it knows (a client that
    /// just joined learns the set without waiting for a gossip round).
    pub async fn discover(&self) {
        for pid in self.0.net.connected_peers().await {
            let peer = pid.to_base58();
            if let Ok(Response::Peers { peers }) = self.call(&peer, &Request::Peers).await {
                for p in peers.into_iter().take(256) {
                    let Ok(id) = p.peer.parse::<PeerId>() else { continue };
                    for a in p.addrs.iter().take(8) {
                        if let Ok(ma) = a.parse::<c0mpute_net::Multiaddr>() {
                            self.0.net.add_peer_address(id, ma).await;
                        }
                    }
                    self.0
                        .mailboxes
                        .write()
                        .await
                        .entry(p.peer)
                        .or_insert((Instant::now(), p.addrs));
                }
            }
        }
    }

    /// Discover until at least `min` mailboxes are known or `wait` passes.
    pub async fn wait_for_mailboxes(&self, min: usize, wait: Duration) -> usize {
        let deadline = Instant::now() + wait;
        loop {
            let n = self.mailboxes().await.len();
            if n >= min || Instant::now() >= deadline {
                return n;
            }
            self.discover().await;
            let n = self.mailboxes().await.len();
            if n >= min {
                return n;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// The recipient's live home record, if any node has heard it lately.
    pub async fn find_home(&self, address: &str) -> Option<HomeRecord> {
        let now = unix_now();
        let ttl = self.0.timing.peer_ttl;
        if let Some((r, t, _)) = self.0.homes.read().await.get(address)
            && t.elapsed() <= ttl
            && r.expires_at > now
        {
            return Some(r.clone());
        }
        let peers = self.mailboxes().await;
        for peer in place(address, &peers, PULL_REPLICAS) {
            if peer == self.0.me {
                continue;
            }
            if let Ok(Response::Presence { record: Some(r), addrs }) = self
                .call(
                    &peer,
                    &Request::Presence {
                        address: address.to_string(),
                    },
                )
                .await
                && r.address() == address
                && r.verify(now).is_ok()
            {
                self.learn_home(r.clone(), addrs).await;
                return Some(r);
            }
        }
        None
    }

    /// Deliver a sealed message: directly to the recipient's home node when
    /// it is up, otherwise onto [`STORE_REPLICAS`] mailboxes.
    pub async fn send(&self, sealed: &Sealed, recipient_address: &str, ttl: u64) -> Result<SendReport> {
        let mut report = SendReport::default();
        let msg = b64e(&sealed.bytes);
        if let Some(home) = self.find_home(recipient_address).await {
            let req = Request::Deliver {
                msg: msg.clone(),
                ack_hash: sealed.ack_hash.clone(),
                ttl,
            };
            match self.call(&home.peer, &req).await {
                Ok(Response::Delivered) => {
                    report.direct = Some(home.peer);
                    return Ok(report);
                }
                Ok(other) => report.errors.push(format!("home {}: unexpected {other:?}", home.peer)),
                Err(e) => report.errors.push(format!("home {}: {e}", home.peer)),
            }
        }
        let peers = self.mailboxes().await;
        if peers.is_empty() {
            bail!("no mailbox nodes known, and the recipient's home node is not reachable");
        }
        let req = Request::Store {
            msg,
            ack_hash: sealed.ack_hash.clone(),
            ttl,
        };
        for peer in rank(&sealed.msg.tag, &peers) {
            if report.stored_on.len() >= STORE_REPLICAS {
                break;
            }
            match self.call(&peer, &req).await {
                Ok(Response::Stored { .. }) => report.stored_on.push(peer),
                Ok(other) => report.errors.push(format!("{peer}: unexpected {other:?}")),
                Err(e) => report.errors.push(format!("{peer}: {e}")),
            }
        }
        if report.stored_on.is_empty() {
            bail!("no mailbox accepted the message: {}", report.errors.join("; "));
        }
        Ok(report)
    }

    /// Everything mailboxes hold for `address` (by its tags), deduplicated.
    pub async fn fetch(&self, address: &str) -> Vec<Fetched> {
        let tags = pull_tags(address, unix_now());
        let peers = self.mailboxes().await;
        let mut by_peer: HashMap<String, Vec<String>> = HashMap::new();
        for tag in &tags {
            for p in place(tag, &peers, PULL_REPLICAS) {
                by_peer.entry(p).or_default().push(tag.clone());
            }
        }
        let mut found: HashMap<String, Fetched> = HashMap::new();
        for (peer, tags) in by_peer {
            let Ok(Response::Messages { items }) = self.call(&peer, &Request::Fetch { tags: tags.clone() }).await else {
                continue;
            };
            for it in items {
                if !tags.contains(&it.tag) || !is_hex32(&it.id) {
                    continue;
                }
                let Ok(bytes) = b64d(&it.msg) else { continue };
                if c0mpute_secure_chat::sealed::sha256_hex(&bytes) != it.id {
                    continue;
                }
                let Ok(msg) = SealedMessage::parse(&bytes) else { continue };
                if msg.tag != it.tag {
                    continue;
                }
                found
                    .entry(it.id.clone())
                    .or_insert_with(|| Fetched {
                        tag: it.tag,
                        id: it.id,
                        msg,
                        holders: Vec::new(),
                    })
                    .holders
                    .push(peer.clone());
            }
        }
        found.into_values().collect()
    }

    /// Delete a fetched message from its mailboxes. Returns how many did.
    pub async fn ack(&self, f: &Fetched, secret: &str) -> usize {
        let req = Request::Ack {
            tag: f.tag.clone(),
            id: f.id.clone(),
            secret: secret.to_string(),
        };
        let mut n = 0;
        for peer in &f.holders {
            if let Ok(Response::Acked { deleted: true }) = self.call(peer, &req).await {
                n += 1;
            }
        }
        n
    }

    /// Publish a key record: cache, gossip, and hand it to the mailboxes
    /// ranked for its fingerprint (where lookups ask first).
    pub async fn publish_key(&self, record: &KeyRecord) -> Result<usize> {
        record.verify(unix_now())?;
        self.cache_key(record.clone()).await;
        self.publish(&Gossip::Key { record: record.clone() }).await;
        let fp = record.address()[..16].to_string();
        let peers = self.mailboxes().await;
        let req = Request::PublishKey { record: record.clone() };
        let mut n = 0;
        for peer in place(&fp, &peers, STORE_REPLICAS) {
            if peer == self.0.me || matches!(self.call(&peer, &req).await, Ok(Response::Ok)) {
                n += 1;
            }
        }
        Ok(n)
    }

    /// Resolve a fingerprint or address to a verified public identity.
    pub async fn lookup(&self, query: &str) -> Option<PublicIdentity> {
        let q = normalize_query(query)?;
        let now = unix_now();
        if let Some(r) = self
            .0
            .keys
            .read()
            .await
            .values()
            .find(|r| matches_query(&r.identity, &q) && r.expires_at > now)
        {
            return Some(r.identity.clone());
        }
        let peers = self.mailboxes().await;
        let ranked = rank(&q[..16], &peers);
        for peer in ranked.into_iter().take(8) {
            if peer == self.0.me {
                continue;
            }
            if let Ok(Response::Key { record: Some(r) }) = self.call(&peer, &Request::Lookup { query: q.clone() }).await
                && matches_query(&r.identity, &q)
                && r.verify(now).is_ok()
            {
                let id = r.identity.clone();
                self.cache_key(r).await;
                return Some(id);
            }
        }
        None
    }
}

/// Write a home record where the node's announcer reads it.
pub fn write_home_record(dir: &Path, record: &HomeRecord) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.json", record.address()));
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(record)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// The worker's libp2p peer id on this machine (its identity lives in the
/// config dir), if it has ever run.
pub fn worker_peer_id(config_dir: &Path) -> Option<String> {
    c0mpute_net::identity::existing_peer_id(config_dir).map(|p| p.to_base58())
}

/// A bare libp2p node for the `c0mpute dm` client: its own identity, the
/// usual bootstrap list (plus mDNS for a local worker), no chunk store.
pub async fn boot_client(identity_dir: &Path) -> Result<Arc<Libp2pNetwork>> {
    std::fs::create_dir_all(identity_dir)?;
    let bootstrap = c0mpute_net::bootstrap::fetch_or_fallback(
        c0mpute_net::bootstrap::DEFAULT_BOOTSTRAP_URL,
        c0mpute_net::bootstrap::default_seeds(),
    )
    .await;
    let cfg = c0mpute_net::NetworkConfig::for_dir(identity_dir).with_bootstrap(bootstrap);
    Ok(Arc::new(Libp2pNetwork::spawn(cfg).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_formats_are_tagged() {
        let r = serde_json::to_string(&Request::Fetch { tags: vec!["a".into()] }).unwrap();
        assert!(r.contains("\"t\":\"fetch\""));
        let p: Response = serde_json::from_str("{\"t\":\"delivered\"}").unwrap();
        assert!(matches!(p, Response::Delivered));
        let paths = Paths::under(Path::new("/x"));
        assert_eq!(paths.spool, Path::new("/x/dm/spool"));
    }
}
