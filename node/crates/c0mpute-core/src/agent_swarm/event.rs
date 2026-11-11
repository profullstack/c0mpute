//! Swarm events: the signed, encrypted records a swarm's log is made of
//! (DIP 0034).
//!
//! An event is canonical JSON with a fixed field set. Its `id` is the
//! SHA-256 of the canonical encoding of every other field, and `sig` is the
//! author's node-identity signature over the id, so anyone holding the event
//! can check who wrote it and that nothing changed. Bodies have one strict
//! schema per kind: an event cannot carry behaviour, only data.
//!
//! On the wire an event travels sealed with AES-256-GCM under a key derived
//! from the swarm key, so relays and gossip peers that are not members see
//! only the topic hash and ciphertext.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, Result, anyhow, bail};
use c0mpute_net::PeerId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Event format version.
pub const VERSION: u32 = 1;
/// Largest encoded event (plaintext canonical JSON).
pub const MAX_EVENT_BYTES: usize = 256 * 1024;
/// Largest submission body, in characters.
pub const MAX_SUBMISSION_CHARS: usize = 100_000;
/// Most tasks one `job.post` may carry.
pub const MAX_INITIAL_TASKS: usize = 64;
/// Most tasks a job may ever hold (including the final task).
pub const MAX_TASKS_PER_JOB: usize = 200;
/// Most dependencies per task.
pub const MAX_NEEDS: usize = 8;
/// Most job owners (identities whose votes decide and who may close).
pub const MAX_OWNERS: usize = 4;
/// Claim lease bounds and default, seconds.
pub const MIN_LEASE_SECS: u64 = 10;
pub const MAX_LEASE_SECS: u64 = 3600;
pub const DEFAULT_LEASE_SECS: u64 = 360;
/// How far in the future an event's timestamp may be.
pub const MAX_FUTURE_SECS: u64 = 300;
/// Lamport clock ceiling.
pub const MAX_LAMPORT: u64 = 1 << 40;

/// The key of the final task every job has.
pub const FINAL_KEY: &str = "final";

// ── swarm keys and links ────────────────────────────────────────────────

/// A swarm's 32-byte shared secret. Knowing it is membership.
#[derive(Clone, PartialEq, Eq)]
pub struct SwarmKey(pub [u8; 32]);

impl std::fmt::Debug for SwarmKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SwarmKey({})", self.topic_id())
    }
}

/// Seed of the public lobby every node joins. Its key is public by design:
/// the lobby is where open, paid work is posted for any node to pick up.
pub const LOBBY_SEED: &[u8] = b"c0mpute/swarm/lobby/v1";
pub const LOBBY_NAME: &str = "lobby";

impl SwarmKey {
    pub fn random() -> Self {
        Self(rand::random())
    }

    pub fn lobby() -> Self {
        Self(Sha256::digest(LOBBY_SEED).into())
    }

    pub fn is_lobby(&self) -> bool {
        *self == Self::lobby()
    }

    pub fn hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn from_hex(s: &str) -> Result<Self> {
        let b = hex::decode(s.trim()).context("swarm key is not hex")?;
        let a: [u8; 32] = b.try_into().map_err(|_| anyhow!("swarm key must be 32 bytes"))?;
        Ok(Self(a))
    }

    /// Public topic id: `sha256("c0mpute/swarm/topic/v1" || key)`, hex. The
    /// network only ever sees this hash.
    pub fn topic_id(&self) -> String {
        let mut h = Sha256::new();
        h.update(b"c0mpute/swarm/topic/v1");
        h.update(self.0);
        hex::encode(h.finalize())
    }

    /// The gossipsub topic this swarm's events travel on.
    pub fn gossip_topic(&self) -> String {
        gossip_topic(&self.topic_id())
    }

    fn aead(&self) -> Aes256Gcm {
        let hk = hkdf::Hkdf::<Sha256>::new(Some(b"c0mpute/swarm/aead/v1"), &self.0);
        let mut k = zeroize::Zeroizing::new([0u8; 32]);
        hk.expand(b"events", &mut *k).expect("32-byte okm");
        Aes256Gcm::new_from_slice(&*k).expect("32-byte key")
    }

    /// `c0swarm:<hex key>[?name=<name>]`
    pub fn link(&self, name: &str) -> String {
        if name.is_empty() {
            format!("c0swarm:{}", self.hex())
        } else {
            format!("c0swarm:{}?name={}", self.hex(), url_escape(name))
        }
    }

    /// Parse a link (`c0swarm:<hex>[?name=..]`, also `c0swarm://`), or the
    /// word `lobby`.
    pub fn parse_link(link: &str) -> Result<(Self, String)> {
        let l = link.trim();
        if l.eq_ignore_ascii_case(LOBBY_NAME) {
            return Ok((Self::lobby(), LOBBY_NAME.to_string()));
        }
        let rest = l
            .strip_prefix("c0swarm://")
            .or_else(|| l.strip_prefix("c0swarm:"))
            .ok_or_else(|| anyhow!("not a swarm link (expected c0swarm:<key>)"))?;
        let (key, query) = rest.split_once('?').unwrap_or((rest, ""));
        let key = Self::from_hex(key)?;
        let name = query
            .split('&')
            .find_map(|kv| kv.strip_prefix("name="))
            .map(url_unescape)
            .unwrap_or_default();
        Ok((key, valid_name(&name).then_some(name).unwrap_or_default()))
    }
}

pub fn gossip_topic(topic_id: &str) -> String {
    format!("c0mpute/swarm/v1/{topic_id}")
}

/// Swarm names: 1-48 of a-z, 0-9, `-`, `_`, `.`.
pub fn valid_name(s: &str) -> bool {
    (1..=48).contains(&s.len()) && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "-_.".contains(c))
}

fn url_escape(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c.to_string() } else { format!("%{:02X}", c as u32 & 0xff) })
        .collect()
}

fn url_unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ── bodies ──────────────────────────────────────────────────────────────

/// A task as posted or added: `key` is unique within the job.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSpec {
    pub key: String,
    pub title: String,
    #[serde(default)]
    pub desc: String,
    #[serde(default)]
    pub needs: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Payee {
    pub wallet: String,
    pub coin: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobPost {
    pub title: String,
    #[serde(default)]
    pub desc: String,
    #[serde(default)]
    pub tasks: Vec<TaskSpec>,
    /// Paid per accepted task, decimal USD ("0.05"); empty = unpaid.
    #[serde(default)]
    pub bounty_usd: String,
    /// Extra identities (peer ids) whose review votes decide and who may
    /// close the job, e.g. the poster's own worker node.
    #[serde(default)]
    pub owners: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskAdd {
    pub job: String,
    pub key: String,
    pub title: String,
    #[serde(default)]
    pub desc: String,
    #[serde(default)]
    pub needs: Vec<String>,
}

impl TaskAdd {
    pub fn spec(&self) -> TaskSpec {
        TaskSpec {
            key: self.key.clone(),
            title: self.title.clone(),
            desc: self.desc.clone(),
            needs: self.needs.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskClaim {
    pub task: String,
    #[serde(default = "default_lease")]
    pub lease_secs: u64,
}

fn default_lease() -> u64 {
    DEFAULT_LEASE_SECS
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskRef {
    pub task: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSubmit {
    pub task: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payee: Option<Payee>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Accept,
    Reject,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskReview {
    pub submission: String,
    pub verdict: Verdict,
    #[serde(default)]
    pub note: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobRef {
    pub job: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskPaid {
    pub submission: String,
    pub amount_usd: String,
    #[serde(default)]
    pub rail_ref: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Msg {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
    pub text: String,
}

/// Every kind of event, typed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Body {
    JobPost(JobPost),
    TaskAdd(TaskAdd),
    TaskClaim(TaskClaim),
    TaskRelease(TaskRef),
    TaskSubmit(TaskSubmit),
    TaskReview(TaskReview),
    TaskPaid(TaskPaid),
    JobClose(JobRef),
    Msg(Msg),
}

impl Body {
    pub fn kind(&self) -> &'static str {
        match self {
            Body::JobPost(_) => "job.post",
            Body::TaskAdd(_) => "task.add",
            Body::TaskClaim(_) => "task.claim",
            Body::TaskRelease(_) => "task.release",
            Body::TaskSubmit(_) => "task.submit",
            Body::TaskReview(_) => "task.review",
            Body::TaskPaid(_) => "task.paid",
            Body::JobClose(_) => "job.close",
            Body::Msg(_) => "msg",
        }
    }

    pub fn to_value(&self) -> Value {
        let v = match self {
            Body::JobPost(b) => serde_json::to_value(b),
            Body::TaskAdd(b) => serde_json::to_value(b),
            Body::TaskClaim(b) => serde_json::to_value(b),
            Body::TaskRelease(b) => serde_json::to_value(b),
            Body::TaskSubmit(b) => serde_json::to_value(b),
            Body::TaskReview(b) => serde_json::to_value(b),
            Body::TaskPaid(b) => serde_json::to_value(b),
            Body::JobClose(b) => serde_json::to_value(b),
            Body::Msg(b) => serde_json::to_value(b),
        };
        v.unwrap_or(Value::Null)
    }

    pub fn parse(kind: &str, body: &Value) -> Result<Self> {
        let b = body.clone();
        Ok(match kind {
            "job.post" => Body::JobPost(serde_json::from_value(b)?),
            "task.add" => Body::TaskAdd(serde_json::from_value(b)?),
            "task.claim" => Body::TaskClaim(serde_json::from_value(b)?),
            "task.release" => Body::TaskRelease(serde_json::from_value(b)?),
            "task.submit" => Body::TaskSubmit(serde_json::from_value(b)?),
            "task.review" => Body::TaskReview(serde_json::from_value(b)?),
            "task.paid" => Body::TaskPaid(serde_json::from_value(b)?),
            "job.close" => Body::JobClose(serde_json::from_value(b)?),
            "msg" => Body::Msg(serde_json::from_value(b)?),
            other => bail!("unknown event kind {other:?}"),
        })
    }

    /// Field limits that serde cannot express.
    pub fn check(&self) -> Result<()> {
        fn len(name: &str, s: &str, min: usize, max: usize) -> Result<()> {
            let n = s.chars().count();
            if n < min || n > max {
                bail!("{name}: {n} characters (allowed {min}-{max})");
            }
            Ok(())
        }
        fn task(t: &TaskSpec) -> Result<()> {
            if !valid_task_key(&t.key) || t.key == FINAL_KEY {
                bail!("task key {:?}: 1-64 of a-z, 0-9, -, _ (and not \"final\")", t.key);
            }
            len("task title", &t.title, 1, 200)?;
            len("task desc", &t.desc, 0, 20_000)?;
            if t.needs.len() > MAX_NEEDS {
                bail!("a task needs at most {MAX_NEEDS} others");
            }
            if t.needs.iter().any(|n| !valid_task_key(n) || n == FINAL_KEY || *n == t.key) {
                bail!("task {}: bad needs", t.key);
            }
            Ok(())
        }
        match self {
            Body::JobPost(j) => {
                len("title", &j.title, 1, 200)?;
                len("desc", &j.desc, 0, 20_000)?;
                if j.tasks.len() > MAX_INITIAL_TASKS {
                    bail!("at most {MAX_INITIAL_TASKS} initial tasks");
                }
                let mut keys = std::collections::HashSet::new();
                for t in &j.tasks {
                    task(t)?;
                    if !keys.insert(&t.key) {
                        bail!("duplicate task key {}", t.key);
                    }
                }
                if !j.bounty_usd.is_empty() {
                    crate::payments::parse_usd(&j.bounty_usd)?;
                }
                if j.owners.len() > MAX_OWNERS || j.owners.iter().any(|o| o.parse::<PeerId>().is_err()) {
                    bail!("owners: at most {MAX_OWNERS} peer ids");
                }
            }
            Body::TaskAdd(a) => {
                hex_id("job", &a.job)?;
                task(&a.spec())?;
            }
            Body::TaskClaim(c) => {
                task_id(&c.task)?;
                if !(MIN_LEASE_SECS..=MAX_LEASE_SECS).contains(&c.lease_secs) {
                    bail!("lease_secs must be {MIN_LEASE_SECS}-{MAX_LEASE_SECS}");
                }
            }
            Body::TaskRelease(r) => task_id(&r.task)?,
            Body::TaskSubmit(s) => {
                task_id(&s.task)?;
                len("content", &s.content, 1, MAX_SUBMISSION_CHARS)?;
                if let Some(p) = &s.payee {
                    let pp = crate::payments::Payee {
                        wallet: p.wallet.clone(),
                        coin: p.coin.clone(),
                    };
                    if !pp.valid() {
                        bail!("payee is not a valid wallet/coin");
                    }
                }
            }
            Body::TaskReview(r) => {
                hex_id("submission", &r.submission)?;
                len("note", &r.note, 0, 2000)?;
            }
            Body::TaskPaid(p) => {
                hex_id("submission", &p.submission)?;
                crate::payments::parse_usd(&p.amount_usd)?;
                len("rail_ref", &p.rail_ref, 0, 200)?;
            }
            Body::JobClose(j) => hex_id("job", &j.job)?,
            Body::Msg(m) => {
                if let Some(j) = &m.job {
                    hex_id("job", j)?;
                }
                len("text", &m.text, 1, 8000)?;
            }
        }
        Ok(())
    }
}

pub fn valid_task_key(s: &str) -> bool {
    (1..=64).contains(&s.len()) && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn hex_id(name: &str, s: &str) -> Result<()> {
    if s.len() != 64 || !s.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()) {
        bail!("{name}: not an event id");
    }
    Ok(())
}

/// `<job id>:<task key>`
pub fn task_id(s: &str) -> Result<()> {
    let (job, key) = s.split_once(':').ok_or_else(|| anyhow!("task id must be <job>:<key>"))?;
    hex_id("task job", job)?;
    if !valid_task_key(key) {
        bail!("task key {key:?} invalid");
    }
    Ok(())
}

pub fn make_task_id(job: &str, key: &str) -> String {
    format!("{job}:{key}")
}

// ── events ──────────────────────────────────────────────────────────────

/// The signed part of an event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Unsigned {
    pub v: u32,
    /// Topic id of the swarm (binds the event to one swarm).
    pub swarm: String,
    /// Author peer id.
    pub author: String,
    pub lamport: u64,
    /// Unix seconds.
    pub ts: u64,
    pub kind: String,
    pub body: Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub v: u32,
    pub swarm: String,
    pub author: String,
    pub lamport: u64,
    pub ts: u64,
    pub kind: String,
    pub body: Value,
    pub id: String,
    pub sig: String,
}

fn canonical<T: Serialize>(v: &T) -> Result<Vec<u8>> {
    c0mpute_envelope::canonical::canonicalize(v).map_err(|e| anyhow!("canonical json: {e}"))
}

impl Unsigned {
    pub fn id(&self) -> Result<String> {
        Ok(hex::encode(Sha256::digest(canonical(self)?)))
    }
}

impl Event {
    pub fn unsigned(&self) -> Unsigned {
        Unsigned {
            v: self.v,
            swarm: self.swarm.clone(),
            author: self.author.clone(),
            lamport: self.lamport,
            ts: self.ts,
            kind: self.kind.clone(),
            body: self.body.clone(),
        }
    }

    /// Build and sign an event with `sign` (the node identity's signer).
    pub fn sign(
        swarm: &str,
        author: &PeerId,
        lamport: u64,
        ts: u64,
        body: &Body,
        sign: impl FnOnce(&[u8]) -> Result<Vec<u8>>,
    ) -> Result<Self> {
        body.check()?;
        let u = Unsigned {
            v: VERSION,
            swarm: swarm.to_string(),
            author: author.to_string(),
            lamport,
            ts,
            kind: body.kind().to_string(),
            body: body.to_value(),
        };
        let id = u.id()?;
        let sig = sign(id.as_bytes())?;
        let e = Event {
            v: u.v,
            swarm: u.swarm,
            author: u.author,
            lamport,
            ts,
            kind: u.kind,
            body: u.body,
            id,
            sig: hex::encode(sig),
        };
        if canonical(&e)?.len() > MAX_EVENT_BYTES {
            bail!("event is over {MAX_EVENT_BYTES} bytes");
        }
        Ok(e)
    }

    /// Check everything about an event that does not need the log: version,
    /// swarm binding, id, signature, clock bounds, body schema and limits.
    pub fn verify(&self, swarm: &str, now: u64) -> Result<Body> {
        if self.v != VERSION {
            bail!("event version {} (want {VERSION})", self.v);
        }
        if self.swarm != swarm {
            bail!("event belongs to another swarm");
        }
        if self.lamport == 0 || self.lamport > MAX_LAMPORT {
            bail!("lamport out of range");
        }
        if self.ts > now + MAX_FUTURE_SECS {
            bail!("event timestamp is in the future");
        }
        if self.unsigned().id()? != self.id {
            bail!("event id does not match its content");
        }
        let author: PeerId = self.author.parse().map_err(|_| anyhow!("author is not a peer id"))?;
        let sig = hex::decode(&self.sig).map_err(|_| anyhow!("signature is not hex"))?;
        if !c0mpute_net::identity::verify_peer_signature(&author, self.id.as_bytes(), &sig) {
            bail!("bad signature");
        }
        if canonical(self)?.len() > MAX_EVENT_BYTES {
            bail!("event is over {MAX_EVENT_BYTES} bytes");
        }
        let body = Body::parse(&self.kind, &self.body)?;
        // Re-encoding must give the same body: no unknown or defaulted
        // fields smuggled in a different shape.
        if body.to_value() != self.body {
            bail!("body is not in canonical shape");
        }
        body.check()?;
        Ok(body)
    }
}

// ── sealing ─────────────────────────────────────────────────────────────

/// A sealed event as it crosses the network.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sealed {
    /// Topic id.
    pub t: String,
    /// 12-byte nonce, hex.
    pub n: String,
    /// Ciphertext, hex.
    pub c: String,
}

pub fn seal(key: &SwarmKey, e: &Event) -> Result<Sealed> {
    let plain = canonical(e)?;
    let topic = key.topic_id();
    let nonce: [u8; 12] = rand::random();
    let c = key
        .aead()
        .encrypt(Nonce::from_slice(&nonce), Payload { msg: &plain, aad: topic.as_bytes() })
        .map_err(|_| anyhow!("seal failed"))?;
    Ok(Sealed {
        t: topic,
        n: hex::encode(nonce),
        c: hex::encode(c),
    })
}

pub fn open(key: &SwarmKey, s: &Sealed) -> Result<Event> {
    if s.t != key.topic_id() {
        bail!("sealed for another swarm");
    }
    let nonce = hex::decode(&s.n).map_err(|_| anyhow!("nonce is not hex"))?;
    if nonce.len() != 12 {
        bail!("nonce must be 12 bytes");
    }
    let c = hex::decode(&s.c).map_err(|_| anyhow!("ciphertext is not hex"))?;
    if c.len() > MAX_EVENT_BYTES + 16 {
        bail!("sealed event too large");
    }
    let plain = key
        .aead()
        .decrypt(Nonce::from_slice(&nonce), Payload { msg: &c, aad: s.t.as_bytes() })
        .map_err(|_| anyhow!("cannot open: wrong key or tampered"))?;
    Ok(serde_json::from_slice(&plain)?)
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use c0mpute_net::libp2p::identity::Keypair;

    pub fn signer() -> (Keypair, PeerId) {
        let k = Keypair::generate_ed25519();
        let p = k.public().to_peer_id();
        (k, p)
    }

    pub fn ev(k: &Keypair, swarm: &str, lamport: u64, ts: u64, body: Body) -> Event {
        let p = k.public().to_peer_id();
        Event::sign(swarm, &p, lamport, ts, &body, |m| Ok(k.sign(m)?)).unwrap()
    }

    #[test]
    fn links_round_trip() {
        let k = SwarmKey::random();
        let (k2, name) = SwarmKey::parse_link(&k.link("team-a")).unwrap();
        assert_eq!(k, k2);
        assert_eq!(name, "team-a");
        let (k3, _) = SwarmKey::parse_link(&format!("c0swarm://{}", k.hex())).unwrap();
        assert_eq!(k, k3);
        assert!(SwarmKey::parse_link("lobby").unwrap().0.is_lobby());
        assert!(SwarmKey::parse_link("swarm://abc").is_err());
        assert_ne!(k.topic_id(), k.hex(), "topic never reveals the key");
    }

    #[test]
    fn sign_verify_and_tamper() {
        let (k, _) = signer();
        let key = SwarmKey::random();
        let t = key.topic_id();
        let e = ev(&k, &t, 1, 1000, Body::Msg(Msg { job: None, text: "hi".into() }));
        assert!(e.verify(&t, 1000).is_ok());
        assert!(e.verify("other", 1000).is_err(), "bound to its swarm");
        let mut bad = e.clone();
        bad.body = serde_json::json!({"text": "bye"});
        assert!(bad.verify(&t, 1000).is_err(), "content change breaks the id");
        let mut forged = e.clone();
        let (k2, p2) = signer();
        forged.author = p2.to_string();
        let _ = k2;
        assert!(forged.verify(&t, 1000).is_err(), "author swap breaks the signature/id");
        assert!(e.verify(&t, 1000 - MAX_FUTURE_SECS - 1).is_err(), "future ts refused");
    }

    #[test]
    fn unknown_fields_and_kinds_refused() {
        assert!(Body::parse("msg", &serde_json::json!({"text": "x", "exec": "rm -rf /"})).is_err());
        assert!(Body::parse("shell.run", &serde_json::json!({})).is_err());
    }

    #[test]
    fn limits() {
        let b = Body::JobPost(JobPost {
            title: "t".into(),
            desc: String::new(),
            tasks: vec![TaskSpec { key: "final".into(), title: "x".into(), desc: String::new(), needs: vec![] }],
            bounty_usd: String::new(),
            owners: vec![],
        });
        assert!(b.check().is_err(), "final is reserved");
        let c = Body::TaskClaim(TaskClaim { task: format!("{}:a", "0".repeat(64)), lease_secs: 5 });
        assert!(c.check().is_err(), "lease too short");
        let s = Body::TaskSubmit(TaskSubmit {
            task: format!("{}:a", "0".repeat(64)),
            content: "x".into(),
            payee: Some(Payee { wallet: "bad wallet!".into(), coin: "USDC_POL".into() }),
        });
        assert!(s.check().is_err());
    }

    #[test]
    fn seal_open() {
        let (k, _) = signer();
        let key = SwarmKey::random();
        let e = ev(&k, &key.topic_id(), 3, 5, Body::Msg(Msg { job: None, text: "secret".into() }));
        let s = seal(&key, &e).unwrap();
        assert!(!s.c.contains(&hex::encode("secret")));
        assert_eq!(open(&key, &s).unwrap(), e);
        let other = SwarmKey::random();
        assert!(open(&other, &s).is_err());
        let mut t = s.clone();
        t.c.replace_range(0..2, if &t.c[0..2] == "00" { "01" } else { "00" });
        assert!(open(&key, &t).is_err());
    }
}
