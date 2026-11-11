//! The swarm service every node runs: joined swarms, their event logs on
//! disk, gossip, catch-up sync over RPC, and strikes against peers that send
//! garbage.
//!
//! * Gossip: each swarm's sealed events go out on
//!   `c0mpute/swarm/v1/<topic id>`. An event too big for one gossip message
//!   goes out as a hint (`{t, lamport}`) and receivers pull it by RPC.
//! * Sync (`c0mpute.swarm.v1` RPC): a member asks a peer for events with
//!   `lamport >= since`, paged. Each round first compares `(count, digest)`
//!   so an up-to-date pair exchanges one small message. On a mismatch the
//!   asker pulls from a window below its own clock, and every few rounds
//!   from zero, which also catches old events it never saw.
//! * Strikes: every event that fails to open or verify is a strike against
//!   the peer that handed it over; 20 strikes is a 10-minute ban (gossip from
//!   it is dropped, its RPCs refused).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use c0mpute_net::{Libp2pNetwork, PeerId, RpcHandler};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use super::event::{Body, Event, LOBBY_NAME, Sealed, SwarmKey, now_secs, open, seal};
use super::reducer::State;

pub const RPC_SERVICE: &str = "c0mpute.swarm.v1";
/// Largest gossip message we send (gossipsub's default cap is 64 KiB).
const GOSSIP_MAX: usize = 60 * 1024;
/// Response budget per sync page (under the RPC response cap).
const PAGE_BYTES: usize = 4 * 1024 * 1024;
const STRIKES_TO_BAN: u32 = 20;
const BAN_FOR: Duration = Duration::from_secs(600);
const SYNC_EVERY: Duration = Duration::from_secs(30);
/// Peers asked per sync round.
const SYNC_PEERS: usize = 3;
/// Window below our clock pulled on a digest mismatch.
const SYNC_WINDOW: u64 = 256;
/// Every this many rounds, pull from zero.
const FULL_EVERY: u64 = 10;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JoinedRecord {
    pub key: String,
    pub name: String,
    pub joined_at: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct SwarmInfo {
    pub name: String,
    pub topic: String,
    pub link: String,
    pub events: usize,
    pub lamport: u64,
    pub lobby: bool,
}

struct Joined {
    key: SwarmKey,
    name: String,
    events: BTreeMap<(u64, String), Event>,
    ids: HashSet<String>,
    max_lamport: u64,
    digest: [u8; 32],
    file: PathBuf,
    /// Bytes of `file` already read (another process may append).
    offset: u64,
}

impl Joined {
    fn insert(&mut self, e: Event) -> bool {
        if !self.ids.insert(e.id.clone()) {
            return false;
        }
        if let Ok(b) = hex::decode(&e.id) {
            for (d, x) in self.digest.iter_mut().zip(b) {
                *d ^= x;
            }
        }
        self.max_lamport = self.max_lamport.max(e.lamport);
        self.events.insert((e.lamport, e.id.clone()), e);
        true
    }
}

#[derive(Default)]
struct Inner {
    swarms: HashMap<String, Joined>,
    strikes: HashMap<PeerId, u32>,
    banned: HashMap<PeerId, Instant>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "k")]
enum GossipMsg {
    #[serde(rename = "ev")]
    Ev(Sealed),
    #[serde(rename = "hint")]
    Hint { t: String, lamport: u64 },
}

#[derive(Serialize, Deserialize)]
struct SyncReq {
    t: String,
    since: u64,
    count: usize,
    digest: String,
}

#[derive(Serialize, Deserialize)]
struct SyncResp {
    same: bool,
    count: usize,
    events: Vec<Sealed>,
    /// Ask again from here for more (None: that was everything).
    next: Option<u64>,
}

/// Something changed in a swarm (topic id).
#[derive(Clone, Debug)]
pub struct Changed(pub String);

pub struct SwarmService {
    net: Arc<Libp2pNetwork>,
    dir: PathBuf,
    inner: Mutex<Inner>,
    changed: broadcast::Sender<Changed>,
}

impl SwarmService {
    /// Start the service: load joined swarms from `dir`, join the lobby when
    /// `lobby` is set, subscribe, serve sync, and keep everything current.
    pub async fn start(net: Arc<Libp2pNetwork>, dir: &Path, lobby: bool) -> Result<Arc<Self>> {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let (changed, _) = broadcast::channel(256);
        let svc = Arc::new(Self {
            net: net.clone(),
            dir: dir.to_path_buf(),
            inner: Mutex::new(Inner::default()),
            changed,
        });
        if lobby {
            svc.edit_joined(|recs| {
                if !recs.iter().any(|r| SwarmKey::from_hex(&r.key).is_ok_and(|k| k.is_lobby())) {
                    recs.push(JoinedRecord {
                        key: SwarmKey::lobby().hex(),
                        name: LOBBY_NAME.into(),
                        joined_at: now_secs(),
                    });
                }
            })?;
        }
        svc.reload().await;
        let records = svc.list();
        net.register_rpc(RPC_SERVICE, Arc::new(SyncHandler(Arc::downgrade(&svc))));
        tokio::spawn(gossip_loop(Arc::downgrade(&svc), net.messages()));
        tokio::spawn(sync_loop(Arc::downgrade(&svc)));
        info!(swarms = records.len(), "agent swarm: running");
        Ok(svc)
    }

    pub fn peer_id(&self) -> PeerId {
        self.net.peer_id()
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn net(&self) -> &Arc<Libp2pNetwork> {
        &self.net
    }

    pub fn changes(&self) -> broadcast::Receiver<Changed> {
        self.changed.subscribe()
    }

    fn joined_path(&self) -> PathBuf {
        self.dir.join("swarms.json")
    }

    fn read_joined(&self) -> Vec<JoinedRecord> {
        std::fs::read(self.joined_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Read-modify-write `swarms.json` (the node and the CLI share it).
    fn edit_joined(&self, f: impl FnOnce(&mut Vec<JoinedRecord>)) -> Result<()> {
        let mut recs = self.read_joined();
        f(&mut recs);
        recs.sort_by(|a, b| a.name.cmp(&b.name));
        recs.dedup_by(|a, b| a.key == b.key);
        write_private(&self.joined_path(), &serde_json::to_vec_pretty(&recs)?)
    }

    /// Pick up joins, leaves and appended events from other processes that
    /// share this directory (`c0mpute swarm` beside a running node).
    pub async fn reload(&self) {
        let recs = self.read_joined();
        let wanted: HashMap<String, (SwarmKey, String)> = recs
            .iter()
            .filter_map(|r| SwarmKey::from_hex(&r.key).ok().map(|k| (k.topic_id(), (k, r.name.clone()))))
            .collect();
        let (gone, new): (Vec<String>, Vec<(SwarmKey, String)>) = {
            let inner = self.inner.lock().unwrap();
            (
                inner.swarms.keys().filter(|t| !wanted.contains_key(*t)).cloned().collect(),
                wanted
                    .iter()
                    .filter(|(t, _)| !inner.swarms.contains_key(*t))
                    .map(|(_, v)| v.clone())
                    .collect(),
            )
        };
        {
            let mut inner = self.inner.lock().unwrap();
            for t in &gone {
                inner.swarms.remove(t);
            }
        }
        for (k, name) in new {
            let topic = k.gossip_topic();
            self.load(k, name);
            if let Err(e) = self.net.subscribe(&topic).await {
                debug!(err = %e, "agent swarm: subscribe");
            }
        }
        // Events another local process (the CLI) wrote: gossip them from
        // here too, since that process may already have exited.
        for (key, e) in self.tail_all() {
            self.gossip(&key, &e).await;
        }
    }

    /// Read lines appended to each log since we last looked.
    fn tail_all(&self) -> Vec<(SwarmKey, Event)> {
        let now = now_secs();
        let mut fresh = Vec::new();
        let mut inner = self.inner.lock().unwrap();
        for (topic, j) in inner.swarms.iter_mut() {
            let Ok(meta) = std::fs::metadata(&j.file) else { continue };
            if meta.len() <= j.offset {
                continue;
            }
            let Ok(raw) = std::fs::read(&j.file) else { continue };
            let start = (j.offset as usize).min(raw.len());
            // Only whole lines.
            let end = raw.iter().rposition(|b| *b == b'\n').map(|i| i + 1).unwrap_or(start).max(start);
            let mut added = false;
            for line in raw[start..end].split(|b| *b == b'\n') {
                if let Ok(e) = serde_json::from_slice::<Event>(line)
                    && e.verify(topic, now + 86_400).is_ok()
                {
                    if j.insert(e.clone()) {
                        added = true;
                        fresh.push((j.key.clone(), e));
                    }
                }
            }
            j.offset = end as u64;
            if added {
                let _ = self.changed.send(Changed(topic.clone()));
            }
        }
        fresh
    }

    /// Load a swarm's log from disk (every line verified again).
    fn load(&self, key: SwarmKey, name: String) {
        let topic = key.topic_id();
        let file = self.dir.join(&topic).join("events.jsonl");
        let mut j = Joined {
            key,
            name,
            events: BTreeMap::new(),
            ids: HashSet::new(),
            max_lamport: 0,
            digest: [0; 32],
            file: file.clone(),
            offset: 0,
        };
        let now = now_secs();
        if let Ok(raw) = std::fs::read_to_string(&file) {
            j.offset = raw.rfind('\n').map(|i| i + 1).unwrap_or(0) as u64;
            for line in raw[..j.offset as usize].lines() {
                if let Ok(e) = serde_json::from_str::<Event>(line)
                    && e.verify(&topic, now + 86_400).is_ok()
                {
                    j.insert(e);
                }
            }
        }
        self.inner.lock().unwrap().swarms.insert(topic, j);
    }

    /// Join a swarm by link. Returns its topic id.
    pub async fn join(&self, link: &str, name: Option<&str>) -> Result<String> {
        let (key, link_name) = SwarmKey::parse_link(link)?;
        let topic = key.topic_id();
        let name = name.map(String::from).filter(|n| !n.is_empty()).unwrap_or(link_name);
        let name = if name.is_empty() { topic[..12].to_string() } else { name };
        let rec = JoinedRecord {
            key: key.hex(),
            name,
            joined_at: now_secs(),
        };
        self.edit_joined(|recs| {
            if !recs.iter().any(|r| r.key == rec.key) {
                recs.push(rec);
            }
        })?;
        self.reload().await;
        Ok(topic)
    }

    /// Create a new private swarm and join it. Returns its link.
    pub async fn create(&self, name: &str) -> Result<String> {
        if !super::event::valid_name(name) {
            bail!("swarm names are 1-48 of a-z, 0-9, -, _, .");
        }
        let key = SwarmKey::random();
        let link = key.link(name);
        self.join(&link, Some(name)).await?;
        Ok(link)
    }

    /// Leave a swarm and delete the local copy of its log.
    pub fn leave(&self, which: &str) -> Result<()> {
        let topic = self.resolve(which)?;
        let j = self.inner.lock().unwrap().swarms.remove(&topic);
        let key = j.as_ref().map(|j| j.key.hex());
        self.edit_joined(|recs| recs.retain(|r| Some(&r.key) != key.as_ref()))?;
        if let Some(j) = j
            && let Some(d) = j.file.parent()
        {
            let _ = std::fs::remove_dir_all(d);
        }
        Ok(())
    }

    /// Find a joined swarm by name, topic id (or prefix), or link.
    pub fn resolve(&self, which: &str) -> Result<String> {
        let inner = self.inner.lock().unwrap();
        let w = which.trim();
        if let Ok((k, _)) = SwarmKey::parse_link(w) {
            let t = k.topic_id();
            if inner.swarms.contains_key(&t) {
                return Ok(t);
            }
        }
        let hits: Vec<&String> = inner
            .swarms
            .iter()
            .filter(|(t, j)| j.name == w || *t == w || (w.len() >= 6 && t.starts_with(w)))
            .map(|(t, _)| t)
            .collect();
        match hits.as_slice() {
            [one] => Ok((*one).clone()),
            [] => Err(anyhow!("not in a swarm called {w:?} (see `c0mpute swarm ls`)")),
            _ => Err(anyhow!("{w:?} matches more than one swarm")),
        }
    }

    pub fn list(&self) -> Vec<SwarmInfo> {
        let inner = self.inner.lock().unwrap();
        let mut v: Vec<SwarmInfo> = inner
            .swarms
            .iter()
            .map(|(t, j)| SwarmInfo {
                name: j.name.clone(),
                topic: t.clone(),
                link: j.key.link(&j.name),
                events: j.events.len(),
                lamport: j.max_lamport,
                lobby: j.key.is_lobby(),
            })
            .collect();
        v.sort_by(|a, b| (!a.lobby, &a.name).cmp(&(!b.lobby, &b.name)));
        v
    }

    pub fn state(&self, topic: &str) -> Result<State> {
        let inner = self.inner.lock().unwrap();
        let j = inner.swarms.get(topic).ok_or_else(|| anyhow!("not in swarm {topic}"))?;
        Ok(State::build(j.events.values()))
    }

    pub fn events(&self, topic: &str) -> Vec<Event> {
        let inner = self.inner.lock().unwrap();
        inner.swarms.get(topic).map(|j| j.events.values().cloned().collect()).unwrap_or_default()
    }

    /// Write, store and gossip one event as this node.
    pub async fn publish(&self, topic: &str, body: Body) -> Result<Event> {
        let (key, lamport) = {
            let inner = self.inner.lock().unwrap();
            let j = inner.swarms.get(topic).ok_or_else(|| anyhow!("not in swarm {topic}"))?;
            (j.key.clone(), j.max_lamport + 1)
        };
        let me = self.net.peer_id();
        let e = Event::sign(topic, &me, lamport, now_secs(), &body, |m| self.net.sign(m))?;
        self.accept(topic, e.clone()).await?;
        self.gossip(&key, &e).await;
        Ok(e)
    }

    async fn gossip(&self, key: &SwarmKey, e: &Event) {
        let Ok(s) = seal(key, e) else { return };
        let msg = match serde_json::to_vec(&GossipMsg::Ev(s)) {
            Ok(b) if b.len() <= GOSSIP_MAX => b,
            _ => match serde_json::to_vec(&GossipMsg::Hint {
                t: key.topic_id(),
                lamport: e.lamport,
            }) {
                Ok(b) => b,
                Err(_) => return,
            },
        };
        if let Err(err) = self.net.publish(&key.gossip_topic(), msg).await {
            // No peers on the topic yet: sync carries it later.
            debug!(err = %err, "agent swarm: gossip publish");
        }
    }

    /// Store a verified event; true when it was new.
    async fn accept(&self, topic: &str, e: Event) -> Result<bool> {
        let (new, file) = {
            let mut inner = self.inner.lock().unwrap();
            let j = inner.swarms.get_mut(topic).ok_or_else(|| anyhow!("not in swarm"))?;
            (j.insert(e.clone()), j.file.clone())
        };
        if new {
            if let Some(d) = file.parent() {
                tokio::fs::create_dir_all(d).await?;
            }
            let mut line = serde_json::to_vec(&e)?;
            line.push(b'\n');
            let mut f = tokio::fs::OpenOptions::new().create(true).append(true).open(&file).await?;
            f.write_all(&line).await?;
            f.flush().await?;
            let _ = self.changed.send(Changed(topic.to_string()));
        }
        Ok(new)
    }

    /// Open, verify and store one sealed event from `peer`; a failure is a
    /// strike against it.
    async fn ingest(&self, peer: Option<PeerId>, s: &Sealed) -> bool {
        let key = {
            let inner = self.inner.lock().unwrap();
            match inner.swarms.get(&s.t) {
                Some(j) => j.key.clone(),
                None => return false,
            }
        };
        let checked = open(&key, s).and_then(|e| e.verify(&s.t, now_secs()).map(|_| e));
        match checked {
            Ok(e) => self.accept(&s.t, e).await.unwrap_or(false),
            Err(err) => {
                if let Some(p) = peer {
                    self.strike(p, &err.to_string());
                }
                false
            }
        }
    }

    fn strike(&self, peer: PeerId, why: &str) {
        let mut inner = self.inner.lock().unwrap();
        let n = inner.strikes.entry(peer).or_insert(0);
        *n += 1;
        debug!(%peer, strikes = *n, why, "agent swarm: invalid event");
        if *n >= STRIKES_TO_BAN {
            inner.strikes.remove(&peer);
            inner.banned.insert(peer, Instant::now() + BAN_FOR);
            warn!(%peer, "agent swarm: {STRIKES_TO_BAN} invalid events; ignoring this peer for 10 minutes");
        }
    }

    fn banned(&self, peer: &PeerId) -> bool {
        let mut inner = self.inner.lock().unwrap();
        match inner.banned.get(peer) {
            Some(until) if *until > Instant::now() => true,
            Some(_) => {
                inner.banned.remove(peer);
                false
            }
            None => false,
        }
    }

    fn summary(&self, topic: &str) -> Option<(usize, String, u64)> {
        let inner = self.inner.lock().unwrap();
        inner.swarms.get(topic).map(|j| (j.events.len(), hex::encode(j.digest), j.max_lamport))
    }

    fn serve_sync(&self, req: SyncReq) -> Result<SyncResp> {
        let inner = self.inner.lock().unwrap();
        let j = inner.swarms.get(&req.t).ok_or_else(|| anyhow!("not in that swarm"))?;
        if j.events.len() == req.count && hex::encode(j.digest) == req.digest {
            return Ok(SyncResp {
                same: true,
                count: j.events.len(),
                events: vec![],
                next: None,
            });
        }
        let mut out = Vec::new();
        let mut bytes = 0;
        let mut next = None;
        for ((lamport, _), e) in j.events.range((req.since, String::new())..) {
            let s = seal(&j.key, e)?;
            let n = s.c.len() + s.n.len() + s.t.len() + 32;
            if bytes + n > PAGE_BYTES && !out.is_empty() {
                // Resume at this clock value (re-sending its siblings is
                // harmless: receivers dedupe by id).
                next = Some(*lamport);
                break;
            }
            bytes += n;
            out.push(s);
        }
        Ok(SyncResp {
            same: false,
            count: j.events.len(),
            events: out,
            next,
        })
    }

    /// Pull what `peer` has for `topic` that we lack.
    pub async fn sync_with(&self, peer: PeerId, topic: &str, full: bool) -> Result<usize> {
        let mut full = full;
        let mut got = 0;
        'pass: loop {
            let Some((count, digest, max)) = self.summary(topic) else { return Ok(got) };
            let mut since = if full { 0 } else { max.saturating_sub(SYNC_WINDOW) };
            for page in 0..64 {
                let req = SyncReq {
                    t: topic.to_string(),
                    since,
                    count: if page == 0 { count } else { usize::MAX },
                    digest: digest.clone(),
                };
                let body = self.net.rpc(peer, RPC_SERVICE, serde_json::to_vec(&req)?).await?;
                let resp: SyncResp = serde_json::from_slice(&body)?;
                if resp.same {
                    return Ok(got);
                }
                let before = got;
                for s in &resp.events {
                    if self.ingest(Some(peer), s).await {
                        got += 1;
                    }
                }
                // Behind, and the window found nothing new: from zero, once.
                if page == 0 && got == before && !full && resp.count > count && resp.next.is_none() {
                    full = true;
                    continue 'pass;
                }
                match resp.next {
                    // Always move forward, or stop (a page that is all one
                    // clock value cannot advance; the next round catches it).
                    Some(n) if n > since => since = n,
                    _ => break,
                }
            }
            return Ok(got);
        }
    }
}

struct SyncHandler(std::sync::Weak<SwarmService>);

#[async_trait]
impl RpcHandler for SyncHandler {
    async fn handle(&self, peer: PeerId, body: Vec<u8>) -> std::result::Result<Vec<u8>, String> {
        let svc = self.0.upgrade().ok_or("shutting down")?;
        if svc.banned(&peer) {
            return Err("banned for sending invalid events".into());
        }
        let req: SyncReq = serde_json::from_slice(&body).map_err(|e| format!("bad request: {e}"))?;
        let resp = svc.serve_sync(req).map_err(|e| e.to_string())?;
        serde_json::to_vec(&resp).map_err(|e| e.to_string())
    }
}

async fn gossip_loop(svc: std::sync::Weak<SwarmService>, mut rx: broadcast::Receiver<c0mpute_net::GossipMessage>) {
    loop {
        let msg = match rx.recv().await {
            Ok(m) => m,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => return,
        };
        let Some(topic) = msg.topic.strip_prefix("c0mpute/swarm/v1/") else { continue };
        let Some(svc) = svc.upgrade() else { return };
        if msg.source.as_ref().is_some_and(|p| svc.banned(p)) {
            continue;
        }
        match serde_json::from_slice::<GossipMsg>(&msg.data) {
            Ok(GossipMsg::Ev(s)) if s.t == topic => {
                svc.ingest(msg.source, &s).await;
            }
            Ok(GossipMsg::Hint { t, .. }) if t == topic => {
                if let Some(p) = msg.source {
                    let svc = svc.clone();
                    tokio::spawn(async move {
                        let _ = svc.sync_with(p, &t, false).await;
                    });
                }
            }
            _ => {
                if let Some(p) = msg.source {
                    svc.strike(p, "malformed gossip");
                }
            }
        }
    }
}

async fn sync_loop(svc: std::sync::Weak<SwarmService>) {
    let mut round: u64 = 0;
    // First round soon after start, once some peers are connected.
    tokio::time::sleep(Duration::from_secs(5)).await;
    loop {
        let Some(s) = svc.upgrade() else { return };
        s.reload().await;
        round += 1;
        let full = round % FULL_EVERY == 1;
        let mut peers = s.net.connected_peers().await;
        // Rotate who we ask.
        if !peers.is_empty() {
            let k = (round as usize) % peers.len();
            peers.rotate_left(k);
        }
        let topics: Vec<String> = s.inner.lock().unwrap().swarms.keys().cloned().collect();
        for topic in topics {
            let mut asked = 0;
            for p in &peers {
                if asked >= SYNC_PEERS {
                    break;
                }
                if s.banned(p) {
                    continue;
                }
                match tokio::time::timeout(Duration::from_secs(25), s.sync_with(*p, &topic, full)).await {
                    Ok(Ok(n)) => {
                        asked += 1;
                        if n > 0 {
                            debug!(%p, topic = %&topic[..12], n, "agent swarm: synced");
                        }
                    }
                    // Not a member / old node: try another.
                    _ => continue,
                }
            }
        }
        drop(s);
        tokio::time::sleep(SYNC_EVERY).await;
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}
