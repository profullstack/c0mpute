//! The store-and-forward mailbox a node keeps for other people.
//!
//! It holds opaque sealed messages keyed by recipient tag, each until its
//! TTL, its recipient acks it, or the node needs the room. Everything is
//! bounded: message size, TTL, bytes and count per tag, total bytes, and
//! messages per minute from each sending peer. Messages are persisted one
//! file per message (`<dir>/<tag>/<id>.msg`) so a restart loses nothing.
//!
//! The mailbox never sees a sender, a recipient address or any plaintext:
//! only the tag, the bytes, and the hash of the ack secret that deletes them.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::sealed::{MAX_SEALED_BYTES, MAX_TTL_SECS, SealedMessage, is_hex32, sha256_hex};

#[derive(Clone, Debug)]
pub struct Limits {
    pub max_msg_bytes: usize,
    pub max_ttl_secs: u64,
    pub max_bytes_per_tag: u64,
    pub max_msgs_per_tag: usize,
    pub max_total_bytes: u64,
    /// Messages accepted per sending peer per minute.
    pub per_sender_per_min: usize,
    /// Most bytes one fetch returns (the rest come on the next pull).
    pub max_fetch_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_msg_bytes: MAX_SEALED_BYTES,
            max_ttl_secs: MAX_TTL_SECS,
            max_bytes_per_tag: 10 * 1024 * 1024,
            max_msgs_per_tag: 500,
            max_total_bytes: 256 * 1024 * 1024,
            per_sender_per_min: 60,
            max_fetch_bytes: 4 * 1024 * 1024,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Put {
    Stored,
    /// Already held (another sender's replica, or a retry).
    Duplicate,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PutError {
    TooLarge,
    BadMessage(String),
    RateLimited,
    TagFull,
    MailboxFull,
}

impl std::fmt::Display for PutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PutError::TooLarge => write!(f, "message too large"),
            PutError::BadMessage(m) => write!(f, "bad message: {m}"),
            PutError::RateLimited => write!(f, "rate limited: too many messages from this peer"),
            PutError::TagFull => write!(f, "this recipient's mailbox is full"),
            PutError::MailboxFull => write!(f, "this node's mailbox is full"),
        }
    }
}

impl std::error::Error for PutError {}

#[derive(Clone, Debug)]
struct Entry {
    bytes: Vec<u8>,
    expires_at: u64,
    ack_hash: String,
}

#[derive(Serialize, Deserialize)]
struct Meta {
    expires_at: u64,
    ack_hash: String,
}

pub struct Mailbox {
    dir: Option<PathBuf>,
    limits: Limits,
    tags: HashMap<String, BTreeMap<String, Entry>>,
    total: u64,
    rate: HashMap<String, VecDeque<u64>>,
}

impl Mailbox {
    /// In memory only.
    pub fn new(limits: Limits) -> Self {
        Self {
            dir: None,
            limits,
            tags: HashMap::new(),
            total: 0,
            rate: HashMap::new(),
        }
    }

    /// Persisted under `dir`; loads what is there and drops what expired.
    pub fn open(dir: &Path, limits: Limits, now: u64) -> Self {
        let mut mb = Self::new(limits);
        mb.dir = Some(dir.to_path_buf());
        let _ = std::fs::create_dir_all(dir);
        let Ok(tags) = std::fs::read_dir(dir) else {
            return mb;
        };
        for t in tags.flatten() {
            let tag = t.file_name().to_string_lossy().into_owned();
            if !is_hex32(&tag) {
                continue;
            }
            let Ok(files) = std::fs::read_dir(t.path()) else {
                continue;
            };
            for f in files.flatten() {
                let path = f.path();
                let Some(id) = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| n.strip_suffix(".msg"))
                    .map(str::to_string)
                else {
                    continue;
                };
                match read_entry(&path) {
                    Some(e) if e.expires_at > now && sha256_hex(&e.bytes) == id => {
                        mb.total += e.bytes.len() as u64;
                        mb.tags.entry(tag.clone()).or_default().insert(id, e);
                    }
                    _ => {
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
        }
        mb
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    pub fn total_bytes(&self) -> u64 {
        self.total
    }

    pub fn count(&self) -> usize {
        self.tags.values().map(|m| m.len()).sum()
    }

    pub fn count_for(&self, tag: &str) -> usize {
        self.tags.get(tag).map(|m| m.len()).unwrap_or(0)
    }

    /// Accept a sealed message from `sender` (the libp2p peer that sent it).
    pub fn put(&mut self, sender: &str, bytes: Vec<u8>, ack_hash: &str, ttl: u64, now: u64) -> Result<Put, PutError> {
        if bytes.len() > self.limits.max_msg_bytes {
            return Err(PutError::TooLarge);
        }
        let msg = SealedMessage::parse(&bytes).map_err(|e| PutError::BadMessage(e.to_string()))?;
        if !is_hex32(ack_hash) {
            return Err(PutError::BadMessage("bad ack hash".into()));
        }
        let id = sha256_hex(&bytes);
        if self.tags.get(&msg.tag).is_some_and(|m| m.contains_key(&id)) {
            return Ok(Put::Duplicate);
        }
        // Rate limit per sending peer over a sliding minute.
        {
            let window = self.rate.entry(sender.to_string()).or_default();
            while window.front().is_some_and(|t| *t + 60 <= now) {
                window.pop_front();
            }
            if window.len() >= self.limits.per_sender_per_min {
                return Err(PutError::RateLimited);
            }
        }
        self.prune(now);
        let size = bytes.len() as u64;
        let (tag_bytes, tag_count) = self
            .tags
            .get(&msg.tag)
            .map(|m| (m.values().map(|e| e.bytes.len() as u64).sum::<u64>(), m.len()))
            .unwrap_or((0, 0));
        if tag_count >= self.limits.max_msgs_per_tag || tag_bytes + size > self.limits.max_bytes_per_tag {
            return Err(PutError::TagFull);
        }
        if self.total + size > self.limits.max_total_bytes {
            return Err(PutError::MailboxFull);
        }
        self.rate.entry(sender.to_string()).or_default().push_back(now);
        let ttl = ttl.clamp(60, self.limits.max_ttl_secs);
        let entry = Entry {
            bytes,
            expires_at: now + ttl,
            ack_hash: ack_hash.to_string(),
        };
        if let Some(dir) = &self.dir {
            write_entry(&dir.join(&msg.tag), &id, &entry);
        }
        self.total += size;
        self.tags.entry(msg.tag).or_default().insert(id, entry);
        // Forget idle senders so the rate table stays small.
        if self.rate.len() > 10_000 {
            self.rate.retain(|_, w| w.back().is_some_and(|t| *t + 60 > now));
        }
        Ok(Put::Stored)
    }

    /// Live messages for `tag`, oldest id order, up to the fetch byte cap.
    pub fn fetch(&self, tag: &str, now: u64) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        let mut bytes = 0usize;
        if let Some(m) = self.tags.get(tag) {
            for (id, e) in m {
                if e.expires_at <= now {
                    continue;
                }
                if bytes + e.bytes.len() > self.limits.max_fetch_bytes && !out.is_empty() {
                    break;
                }
                bytes += e.bytes.len();
                out.push((id.clone(), e.bytes.clone()));
            }
        }
        out
    }

    /// Delete `id` if `secret` (hex) hashes to its ack hash.
    pub fn ack(&mut self, tag: &str, id: &str, secret: &str) -> bool {
        let Ok(raw) = hex::decode(secret) else {
            return false;
        };
        let ok = self
            .tags
            .get(tag)
            .and_then(|m| m.get(id))
            .is_some_and(|e| e.ack_hash == sha256_hex(&raw));
        if ok {
            self.remove(tag, id);
        }
        ok
    }

    fn remove(&mut self, tag: &str, id: &str) {
        if let Some(m) = self.tags.get_mut(tag)
            && let Some(e) = m.remove(id)
        {
            self.total -= e.bytes.len() as u64;
            if let Some(dir) = &self.dir {
                let _ = std::fs::remove_file(dir.join(tag).join(format!("{id}.msg")));
            }
            if m.is_empty() {
                self.tags.remove(tag);
                if let Some(dir) = &self.dir {
                    let _ = std::fs::remove_dir(dir.join(tag));
                }
            }
        }
    }

    /// Drop expired messages. Returns how many went.
    pub fn prune(&mut self, now: u64) -> usize {
        let dead: Vec<(String, String)> = self
            .tags
            .iter()
            .flat_map(|(t, m)| {
                m.iter()
                    .filter(|(_, e)| e.expires_at <= now)
                    .map(move |(id, _)| (t.clone(), id.clone()))
            })
            .collect();
        for (t, id) in &dead {
            self.remove(t, id);
        }
        dead.len()
    }
}

fn write_entry(dir: &Path, id: &str, e: &Entry) {
    let _ = std::fs::create_dir_all(dir);
    let meta = Meta {
        expires_at: e.expires_at,
        ack_hash: e.ack_hash.clone(),
    };
    let mut buf = serde_json::to_vec(&meta).unwrap_or_default();
    buf.push(b'\n');
    buf.extend_from_slice(&e.bytes);
    let path = dir.join(format!("{id}.msg"));
    let tmp = dir.join(format!("{id}.tmp"));
    if std::fs::write(&tmp, &buf).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

fn read_entry(path: &Path) -> Option<Entry> {
    let raw = std::fs::read(path).ok()?;
    let nl = raw.iter().position(|b| *b == b'\n')?;
    let meta: Meta = serde_json::from_slice(&raw[..nl]).ok()?;
    Some(Entry {
        bytes: raw[nl + 1..].to_vec(),
        expires_at: meta.expires_at,
        ack_hash: meta.ack_hash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::ChatKey;
    use crate::sealed::seal;

    const NOW: u64 = 1_800_000_000;

    fn msg_to(bob: &ChatKey) -> crate::sealed::Sealed {
        seal(&ChatKey::from_seed([1; 32]), &bob.public_identity(), "hi", 3600, NOW).unwrap()
    }

    #[test]
    fn put_fetch_ack_and_ttl() {
        let bob = ChatKey::from_seed([2; 32]);
        let mut mb = Mailbox::new(Limits::default());
        let s = msg_to(&bob);
        assert_eq!(mb.put("p1", s.bytes.clone(), &s.ack_hash, 3600, NOW), Ok(Put::Stored));
        assert_eq!(mb.put("p2", s.bytes.clone(), &s.ack_hash, 3600, NOW), Ok(Put::Duplicate));
        let got = mb.fetch(&s.msg.tag, NOW);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, s.id);
        // Wrong secret: nothing deleted.
        assert!(!mb.ack(&s.msg.tag, &s.id, &"00".repeat(32)));
        assert_eq!(mb.count(), 1);
        // TTL: gone after expiry.
        assert!(mb.fetch(&s.msg.tag, NOW + 3601).is_empty());
        assert_eq!(mb.prune(NOW + 3601), 1);
        assert_eq!(mb.total_bytes(), 0);

        // Ack with the secret only the recipient can read.
        mb.put("p1", s.bytes.clone(), &s.ack_hash, 3600, NOW).unwrap();
        let o = crate::sealed::unseal(&bob, &s.msg, NOW).unwrap();
        assert!(mb.ack(&s.msg.tag, &s.id, &o.ack));
        assert_eq!(mb.count(), 0);
        // TTL is capped at the maximum.
        mb.put("p1", s.bytes.clone(), &s.ack_hash, 10 * MAX_TTL_SECS, NOW).unwrap();
        assert_eq!(mb.fetch(&s.msg.tag, NOW + MAX_TTL_SECS + 1).len(), 0);
    }

    #[test]
    fn caps_and_rate_limits() {
        let bob = ChatKey::from_seed([3; 32]);
        // Size cap and garbage.
        let mut mb = Mailbox::new(Limits {
            max_msg_bytes: 100,
            ..Default::default()
        });
        let s = msg_to(&bob);
        assert_eq!(mb.put("p", s.bytes.clone(), &s.ack_hash, 3600, NOW), Err(PutError::TooLarge));
        let mut mb = Mailbox::new(Limits::default());
        assert!(matches!(mb.put("p", b"not json".to_vec(), &s.ack_hash, 3600, NOW), Err(PutError::BadMessage(_))));
        assert!(matches!(mb.put("p", s.bytes.clone(), "nothex", 3600, NOW), Err(PutError::BadMessage(_))));

        // Per-sender rate limit, sliding minute.
        let mut mb = Mailbox::new(Limits {
            per_sender_per_min: 2,
            ..Default::default()
        });
        let m: Vec<_> = (0..4).map(|_| msg_to(&bob)).collect();
        assert_eq!(mb.put("spammer", m[0].bytes.clone(), &m[0].ack_hash, 3600, NOW), Ok(Put::Stored));
        assert_eq!(mb.put("spammer", m[1].bytes.clone(), &m[1].ack_hash, 3600, NOW + 1), Ok(Put::Stored));
        assert_eq!(mb.put("spammer", m[2].bytes.clone(), &m[2].ack_hash, 3600, NOW + 2), Err(PutError::RateLimited));
        assert_eq!(mb.put("someone-else", m[2].bytes.clone(), &m[2].ack_hash, 3600, NOW + 2), Ok(Put::Stored));
        assert_eq!(mb.put("spammer", m[3].bytes.clone(), &m[3].ack_hash, 3600, NOW + 61), Ok(Put::Stored));

        // Per-tag count cap and total byte cap.
        let mut mb = Mailbox::new(Limits {
            max_msgs_per_tag: 2,
            ..Default::default()
        });
        for (i, s) in m.iter().enumerate() {
            let r = mb.put("p", s.bytes.clone(), &s.ack_hash, 3600, NOW);
            assert_eq!(r, if i < 2 { Ok(Put::Stored) } else { Err(PutError::TagFull) });
        }
        let one = m[0].bytes.len() as u64;
        let mut mb = Mailbox::new(Limits {
            max_total_bytes: one * 2,
            ..Default::default()
        });
        let carol = ChatKey::from_seed([4; 32]);
        let to_carol = msg_to(&carol);
        mb.put("p", m[0].bytes.clone(), &m[0].ack_hash, 3600, NOW).unwrap();
        mb.put("p", m[1].bytes.clone(), &m[1].ack_hash, 3600, NOW).unwrap();
        assert_eq!(mb.put("p", to_carol.bytes.clone(), &to_carol.ack_hash, 3600, NOW), Err(PutError::MailboxFull));
    }

    #[test]
    fn persists_across_restart_and_drops_expired() {
        let dir = std::env::temp_dir().join(format!("c0dm-mb-{}", rand::random::<u64>()));
        let bob = ChatKey::from_seed([5; 32]);
        let a = msg_to(&bob);
        let b = msg_to(&bob);
        {
            let mut mb = Mailbox::open(&dir, Limits::default(), NOW);
            mb.put("p", a.bytes.clone(), &a.ack_hash, 3600, NOW).unwrap();
            mb.put("p", b.bytes.clone(), &b.ack_hash, 60, NOW).unwrap();
        }
        let mb = Mailbox::open(&dir, Limits::default(), NOW + 100);
        assert_eq!(mb.count(), 1, "the 60 s message expired across the restart");
        assert_eq!(mb.fetch(&a.msg.tag, NOW + 100)[0].0, a.id);
        assert_eq!(mb.total_bytes(), a.bytes.len() as u64);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
