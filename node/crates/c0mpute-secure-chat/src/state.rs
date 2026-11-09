//! The client's local state: contacts, blocklist, cached keys, inbox and the
//! replay guard, encrypted at rest under a key derived from the chat seed
//! (AES-256-GCM, HKDF label `secure-chat-local-state-v1`). Without the key
//! password the file reveals nothing, not even who your contacts are.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use aes_gcm::{
    Aes256Gcm, Key, Nonce,
    aead::{Aead, AeadCore, KeyInit},
};
use base64::prelude::*;
use hkdf::Hkdf;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::identity::{PublicIdentity, matches_query, normalize_query};
use crate::keys::ChatKey;
use crate::sealed::{Opened, ReplayGuard, SealedMessage, open};

/// Inbox entries kept (oldest dropped first).
pub const MAX_INBOX: usize = 5_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Contact {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub identity: PublicIdentity,
    pub added_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InboxEntry {
    /// Envelope id.
    pub id: String,
    /// Sender address.
    pub from: String,
    pub text: String,
    pub created_at: u64,
    pub received_at: u64,
    /// `direct` (pushed to this user's home node) or `mailbox`.
    pub via: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LocalState {
    #[serde(default)]
    pub contacts: BTreeMap<String, Contact>,
    #[serde(default)]
    pub blocked: BTreeSet<String>,
    /// Identities learned from lookups and received messages.
    #[serde(default)]
    pub known: BTreeMap<String, PublicIdentity>,
    #[serde(default)]
    pub inbox: Vec<InboxEntry>,
    #[serde(default)]
    pub replay: ReplayGuard,
}

#[derive(Serialize, Deserialize)]
struct EncryptedFile {
    v: u8,
    nonce: String,
    ct: String,
}

/// What happened to one received message.
#[derive(Debug)]
pub enum Received {
    /// New message, now in the inbox.
    New(Opened),
    /// From a blocked sender: dropped (still acked so it is deleted).
    Blocked(Opened),
}

impl Received {
    pub fn ack(&self) -> &str {
        match self {
            Received::New(o) | Received::Blocked(o) => &o.ack,
        }
    }
}

impl ChatKey {
    fn local_state_key(&self) -> Zeroizing<[u8; 32]> {
        let hk = Hkdf::<Sha256>::new(None, self.seed.as_ref());
        let mut k = Zeroizing::new([0u8; 32]);
        hk.expand(b"secure-chat-local-state-v1", k.as_mut())
            .expect("32 bytes is a valid HKDF length");
        k
    }
}

/// `chat-state.enc` next to `chat.key`.
pub fn state_file_path() -> anyhow::Result<PathBuf> {
    Ok(crate::keys::key_file_path()?.with_file_name("chat-state.enc"))
}

impl LocalState {
    /// Load and decrypt; a missing file is an empty state.
    pub fn load(path: &Path, key: &ChatKey) -> anyhow::Result<Self> {
        let raw = match std::fs::read(path) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(anyhow::anyhow!("read {}: {e}", path.display())),
        };
        let f: EncryptedFile = serde_json::from_slice(&raw)?;
        anyhow::ensure!(f.v == 1, "unsupported state file version");
        let k = key.local_state_key();
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(k.as_ref()));
        let nonce = BASE64_URL_SAFE_NO_PAD.decode(&f.nonce)?;
        anyhow::ensure!(nonce.len() == 12, "bad nonce");
        let plain = Zeroizing::new(
            cipher
                .decrypt(Nonce::from_slice(&nonce), BASE64_URL_SAFE_NO_PAD.decode(&f.ct)?.as_ref())
                .map_err(|_| anyhow::anyhow!("cannot decrypt {} with this key", path.display()))?,
        );
        Ok(serde_json::from_slice(&plain)?)
    }

    /// Encrypt and write atomically (0600).
    pub fn save(&self, path: &Path, key: &ChatKey) -> anyhow::Result<()> {
        let plain = Zeroizing::new(serde_json::to_vec(self)?);
        let k = key.local_state_key();
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(k.as_ref()));
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ct = cipher
            .encrypt(&nonce, plain.as_ref())
            .map_err(|e| anyhow::anyhow!("encrypt state: {e}"))?;
        let f = EncryptedFile {
            v: 1,
            nonce: BASE64_URL_SAFE_NO_PAD.encode(nonce.as_slice()),
            ct: BASE64_URL_SAFE_NO_PAD.encode(ct),
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        crate::keys::write_secret_file(&tmp, &serde_json::to_vec(&f)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Resolve what a user typed (contact name, address, fingerprint, or a
    /// `c0dm1.` share string) from local knowledge only.
    pub fn resolve(&self, query: &str) -> Option<PublicIdentity> {
        if let Ok(id) = PublicIdentity::from_share_string(query) {
            return Some(id);
        }
        if let Some(c) = self.contacts.values().find(|c| c.name.as_deref() == Some(query.trim())) {
            return Some(c.identity.clone());
        }
        let q = normalize_query(query)?;
        self.contacts
            .values()
            .map(|c| &c.identity)
            .chain(self.known.values())
            .find(|id| matches_query(id, &q))
            .cloned()
    }

    pub fn remember(&mut self, id: &PublicIdentity) {
        if self.known.len() >= 10_000
            && let Some(first) = self.known.keys().next().cloned()
        {
            self.known.remove(&first);
        }
        self.known.insert(id.address(), id.clone());
    }

    pub fn add_contact(&mut self, id: PublicIdentity, name: Option<String>, now: u64) -> String {
        let addr = id.address();
        self.contacts.insert(
            addr.clone(),
            Contact {
                name,
                identity: id,
                added_at: now,
            },
        );
        addr
    }

    /// Display name for an address: contact name, else its fingerprint.
    pub fn name_of(&self, address: &str) -> String {
        match self.contacts.get(address).and_then(|c| c.name.clone()) {
            Some(n) => n,
            None => crate::identity::fingerprint_of(address),
        }
    }

    /// Open a sealed message for `me`: verify, replay-check, apply the
    /// blocklist, and file it in the inbox.
    pub fn receive(&mut self, me: &ChatKey, msg: &SealedMessage, via: &str, now: u64) -> anyhow::Result<Received> {
        self.replay.prune(now);
        let o = open(me, msg, now, &mut self.replay)?;
        if self.blocked.contains(&o.sender_address) {
            return Ok(Received::Blocked(o));
        }
        self.remember(&o.sender);
        self.inbox.push(InboxEntry {
            id: o.envelope_id.clone(),
            from: o.sender_address.clone(),
            text: o.text.clone(),
            created_at: o.created_at,
            received_at: now,
            via: via.to_string(),
        });
        if self.inbox.len() > MAX_INBOX {
            let extra = self.inbox.len() - MAX_INBOX;
            self.inbox.drain(..extra);
        }
        Ok(Received::New(o))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sealed::seal;

    const NOW: u64 = 1_800_000_000;

    #[test]
    fn state_is_encrypted_at_rest_and_roundtrips() {
        let dir = std::env::temp_dir().join(format!("c0dm-state-{}", rand::random::<u64>()));
        let path = dir.join("chat-state.enc");
        let me = ChatKey::generate();
        let friend = ChatKey::generate();
        let mut st = LocalState::default();
        st.add_contact(friend.public_identity(), Some("carol".into()), NOW);
        st.save(&path, &me).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("carol"));
        assert!(!raw.contains(&friend.address()));
        let back = LocalState::load(&path, &me).unwrap();
        assert_eq!(back.contacts.len(), 1);
        assert!(LocalState::load(&path, &ChatKey::generate()).is_err(), "wrong key");
        // Resolution by name, fingerprint, address.
        assert_eq!(back.resolve("carol").unwrap().address(), friend.address());
        assert_eq!(back.resolve(&friend.public_identity().fingerprint()).unwrap().address(), friend.address());
        assert_eq!(back.resolve(&friend.address()).unwrap().address(), friend.address());
        assert!(back.resolve("dave").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn blocked_senders_are_dropped_and_replays_rejected() {
        let bob = ChatKey::generate();
        let alice = ChatKey::generate();
        let mallory = ChatKey::generate();
        let mut st = LocalState::default();
        st.blocked.insert(mallory.address());

        let good = seal(&alice, &bob.public_identity(), "hello", 3600, NOW).unwrap();
        let spam = seal(&mallory, &bob.public_identity(), "buy now", 3600, NOW).unwrap();
        assert!(matches!(st.receive(&bob, &good.msg, "mailbox", NOW).unwrap(), Received::New(_)));
        assert!(matches!(st.receive(&bob, &spam.msg, "mailbox", NOW).unwrap(), Received::Blocked(_)));
        assert_eq!(st.inbox.len(), 1);
        assert_eq!(st.inbox[0].text, "hello");
        assert!(st.receive(&bob, &good.msg, "direct", NOW + 5).is_err(), "replay");
        assert_eq!(st.inbox.len(), 1);
        assert!(st.known.contains_key(&alice.address()));
    }
}
