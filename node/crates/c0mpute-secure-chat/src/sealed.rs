//! Sealed messages: what actually crosses the network.
//!
//! A [`DmEnvelope`] (signed, hybrid-encrypted, from [`crate::message`]) names
//! its sender and recipient, so it never travels bare. It is wrapped, with
//! the sender's public identity and a random ack secret, in a second
//! encryption layer to the recipient from a fresh ephemeral X25519 key plus a
//! fresh ML-KEM-768 encapsulation. The outer [`SealedMessage`] carries only:
//!
//! * `tag`: SHA-256 of the recipient address and the current epoch
//!   ([`recipient_tag`]), the mailbox routing key. It changes every
//!   [`EPOCH_SECS`], so a mailbox cannot link one recipient's messages across
//!   epochs without already knowing the recipient's address;
//! * the ephemeral public key, KEM ciphertext, nonce and ciphertext.
//!
//! No sender, no timestamp, no plaintext length beyond the total size. The
//! mailbox stores the message under `id = SHA-256(bytes)` with `ack_hash =
//! SHA-256(ack secret)`; only the recipient can read the secret, so only the
//! recipient can delete the message.

use std::collections::BTreeMap;

use aes_gcm::{
    Aes256Gcm, Key, Nonce,
    aead::{Aead, AeadCore, KeyInit, Payload},
};
use base64::prelude::*;
use hkdf::Hkdf;
use ml_kem::{Decapsulate, Encapsulate};
use rand::RngCore;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey as X25519Public, StaticSecret as X25519Secret};

use crate::identity::{CLOCK_SKEW_SECS, PublicIdentity};
use crate::keys::ChatKey;
use crate::message::{DmEnvelope, decrypt_dm, encrypt_dm_at, verify_envelope_sig};

/// Current sealed-message format.
pub const SEALED_VERSION: u8 = 1;
/// Recipient tags rotate this often.
pub const EPOCH_SECS: u64 = 7 * 86_400;
/// Longest a mailbox keeps a message, and the replay window.
pub const MAX_TTL_SECS: u64 = 30 * 86_400;
/// Default time a sender asks mailboxes to keep a message.
pub const DEFAULT_TTL_SECS: u64 = MAX_TTL_SECS;
/// Longest message text accepted.
pub const MAX_TEXT_BYTES: usize = 16 * 1024;
/// Largest sealed message on the wire (a 16 KiB text seals to about 40 KiB).
pub const MAX_SEALED_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SealedMessage {
    pub v: u8,
    /// Hex recipient tag (32 bytes).
    pub tag: String,
    /// Base64url ephemeral X25519 public key.
    pub eph: String,
    /// Base64url ML-KEM-768 ciphertext.
    pub kem_ct: String,
    /// Base64url AES-256-GCM nonce.
    pub nonce: String,
    /// Base64url AES-256-GCM ciphertext of [`SealedInner`].
    pub ct: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SealedInner {
    v: u8,
    sender: PublicIdentity,
    envelope: DmEnvelope,
    /// Hex 32-byte secret whose SHA-256 the mailbox holds; revealing it
    /// deletes the message.
    ack: String,
}

/// A sealed message ready to hand to a mailbox or a home node.
pub struct Sealed {
    pub msg: SealedMessage,
    pub bytes: Vec<u8>,
    /// SHA-256 of `bytes` (hex): the mailbox id.
    pub id: String,
    /// SHA-256 of the ack secret (hex).
    pub ack_hash: String,
    /// The inner envelope id (what the recipient deduplicates on).
    pub envelope_id: String,
}

/// A message opened, verified and replay-checked.
#[derive(Clone, Debug)]
pub struct Opened {
    pub sender: PublicIdentity,
    pub sender_address: String,
    pub text: String,
    pub envelope_id: String,
    pub created_at: u64,
    /// Hex ack secret: send it to the mailbox to delete the message.
    pub ack: String,
}

pub fn epoch_of(t: u64) -> u64 {
    t / EPOCH_SECS
}

/// The mailbox routing key for `address` in `epoch`.
pub fn recipient_tag(address: &str, epoch: u64) -> String {
    hex::encode(Sha256::digest(format!("c0mpute-dm-tag-v1|{address}|{epoch}").as_bytes()))
}

/// Every tag a message to `address` could still be stored under at `now`:
/// from the epoch `MAX_TTL_SECS` ago through the next one (clock skew).
pub fn pull_tags(address: &str, now: u64) -> Vec<String> {
    let first = epoch_of(now.saturating_sub(MAX_TTL_SECS + CLOCK_SKEW_SECS));
    let last = epoch_of(now + CLOCK_SKEW_SECS);
    (first..=last).map(|e| recipient_tag(address, e)).collect()
}

pub fn sha256_hex(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

fn outer_key(eph_ss: &[u8; 32], kem_ss: &[u8], eph_pub: &[u8; 32], tag: &str) -> [u8; 32] {
    let mut ikm = Vec::with_capacity(64);
    ikm.extend_from_slice(eph_ss);
    ikm.extend_from_slice(kem_ss);
    let hk = Hkdf::<Sha256>::new(Some(eph_pub), &ikm);
    let mut k = [0u8; 32];
    hk.expand(format!("c0mpute-dm-seal-v1|{tag}").as_bytes(), &mut k)
        .expect("32 bytes is a valid HKDF length");
    k
}

fn outer_aad(tag: &str) -> Vec<u8> {
    format!("c0mpute-dm-sealed-v{SEALED_VERSION}|{tag}").into_bytes()
}

/// Encrypt, sign and seal `text` from `sender` to `recipient` at time `now`.
pub fn seal(sender: &ChatKey, recipient: &PublicIdentity, text: &str, ttl: u64, now: u64) -> anyhow::Result<Sealed> {
    anyhow::ensure!(text.len() <= MAX_TEXT_BYTES, "message longer than {MAX_TEXT_BYTES} bytes");
    let ttl = ttl.clamp(60, MAX_TTL_SECS);
    let rk = recipient.keys()?;
    let sender_id = sender.public_identity();
    let to = recipient.address();
    let envelope = encrypt_dm_at(
        &sender_id.address(),
        &to,
        text,
        &sender.x25519_secret,
        &rk.x25519,
        &rk.ml_kem,
        &sender.ed25519_key,
        &sender.ml_dsa_key,
        ttl,
        now,
    )?;
    let mut ack = [0u8; 32];
    OsRng.fill_bytes(&mut ack);
    let inner = SealedInner {
        v: SEALED_VERSION,
        sender: sender_id,
       
        envelope: envelope.clone(),
        ack: hex::encode(ack),
    };
    let inner_bytes = serde_json::to_vec(&inner)?;

    let tag = recipient_tag(&to, epoch_of(now));
    let eph = X25519Secret::random_from_rng(OsRng);
    let eph_pub = X25519Public::from(&eph);
    let eph_ss = eph.diffie_hellman(&rk.x25519);
    let (kem_ct, kem_ss) = rk.ml_kem.encapsulate();
    let key = outer_key(eph_ss.as_bytes(), kem_ss.as_ref(), eph_pub.as_bytes(), &tag);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let aad = outer_aad(&tag);
    let ct = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: &inner_bytes,
                aad: &aad,
            },
        )
        .map_err(|e| anyhow::anyhow!("seal: {e}"))?;
    let msg = SealedMessage {
        v: SEALED_VERSION,
        tag,
        eph: BASE64_URL_SAFE_NO_PAD.encode(eph_pub.as_bytes()),
        kem_ct: BASE64_URL_SAFE_NO_PAD.encode(&kem_ct[..]),
        nonce: BASE64_URL_SAFE_NO_PAD.encode(nonce.as_slice()),
        ct: BASE64_URL_SAFE_NO_PAD.encode(&ct),
    };
    let bytes = serde_json::to_vec(&msg)?;
    anyhow::ensure!(bytes.len() <= MAX_SEALED_BYTES, "sealed message too large ({} bytes)", bytes.len());
    Ok(Sealed {
        id: sha256_hex(&bytes),
        ack_hash: sha256_hex(&ack),
        envelope_id: envelope.id,
        msg,
        bytes,
    })
}

impl SealedMessage {
    /// Parse and sanity-check wire bytes (what a mailbox does: no keys).
    pub fn parse(bytes: &[u8]) -> anyhow::Result<Self> {
        anyhow::ensure!(bytes.len() <= MAX_SEALED_BYTES, "sealed message too large");
        let m: Self = serde_json::from_slice(bytes)?;
        anyhow::ensure!(m.v == SEALED_VERSION, "unsupported sealed message version {}", m.v);
        anyhow::ensure!(is_hex32(&m.tag), "bad tag");
        Ok(m)
    }
}

pub fn is_hex32(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Decrypt and verify a sealed message addressed to `me`, without replay
/// checks (see [`open`]).
pub fn unseal(me: &ChatKey, msg: &SealedMessage, now: u64) -> anyhow::Result<Opened> {
    anyhow::ensure!(msg.v == SEALED_VERSION, "unsupported sealed message version {}", msg.v);
    let my_address = me.address();
    anyhow::ensure!(
        pull_tags(&my_address, now).contains(&msg.tag),
        "not addressed to this key (or outside the message window)"
    );
    let eph = BASE64_URL_SAFE_NO_PAD.decode(&msg.eph)?;
    let eph: [u8; 32] = eph.as_slice().try_into().map_err(|_| anyhow::anyhow!("bad ephemeral key"))?;
    let eph_pub = X25519Public::from(eph);
    let eph_ss = me.x25519_secret.diffie_hellman(&eph_pub);
    let kem_ct = BASE64_URL_SAFE_NO_PAD.decode(&msg.kem_ct)?;
    let kem_ss = me
        .ml_kem_dk
        .decapsulate_slice(&kem_ct)
        .map_err(|_| anyhow::anyhow!("bad ml-kem ciphertext"))?;
    let key = outer_key(eph_ss.as_bytes(), kem_ss.as_ref(), eph_pub.as_bytes(), &msg.tag);
    let nonce = BASE64_URL_SAFE_NO_PAD.decode(&msg.nonce)?;
    anyhow::ensure!(nonce.len() == 12, "bad nonce");
    let ct = BASE64_URL_SAFE_NO_PAD.decode(&msg.ct)?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
    let aad = outer_aad(&msg.tag);
    let inner_bytes = cipher
        .decrypt(Nonce::from_slice(&nonce), Payload { msg: &ct, aad: &aad })
        .map_err(|_| anyhow::anyhow!("cannot open: not for this key, or tampered"))?;
    let inner: SealedInner = serde_json::from_slice(&inner_bytes)?;
    anyhow::ensure!(inner.v == SEALED_VERSION, "unsupported inner version");
    anyhow::ensure!(is_hex32(&inner.ack), "bad ack secret");

    let env = &inner.envelope;
    let sender_address = inner.sender.address();
    anyhow::ensure!(env.from == sender_address, "envelope sender does not match the enclosed identity");
    anyhow::ensure!(env.to == my_address, "envelope is addressed to someone else");
    let sk = inner.sender.keys()?;
    verify_envelope_sig(env, &sk.ed25519, &sk.ml_dsa)?;
    let plain = decrypt_dm(env, &me.x25519_secret, &sk.x25519, &me.ml_kem_dk)?;
    Ok(Opened {
        sender: inner.sender,
        sender_address,
        text: plain.text,
        envelope_id: env.id.clone(),
        created_at: env.created_at,
        ack: inner.ack,
    })
}

/// [`unseal`], then reject replays: a timestamp outside the window, or an
/// envelope id already seen. Records the id on success.
pub fn open(me: &ChatKey, msg: &SealedMessage, now: u64, guard: &mut ReplayGuard) -> anyhow::Result<Opened> {
    let o = unseal(me, msg, now)?;
    guard.check(&o.envelope_id, o.created_at, now)?;
    guard.record(&o.envelope_id, o.created_at);
    Ok(o)
}

/// Envelope ids seen inside the replay window.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ReplayGuard {
    pub seen: BTreeMap<String, u64>,
}

impl ReplayGuard {
    pub fn check(&self, id: &str, created_at: u64, now: u64) -> anyhow::Result<()> {
        anyhow::ensure!(created_at <= now + CLOCK_SKEW_SECS, "message timestamp is in the future");
        anyhow::ensure!(
            created_at + MAX_TTL_SECS + CLOCK_SKEW_SECS >= now,
            "message is older than the replay window"
        );
        anyhow::ensure!(!self.seen.contains_key(id), "replayed message (already received)");
        Ok(())
    }

    pub fn record(&mut self, id: &str, created_at: u64) {
        self.seen.insert(id.to_string(), created_at);
    }

    /// Forget ids older than the window: their timestamps alone reject them.
    pub fn prune(&mut self, now: u64) {
        let floor = now.saturating_sub(MAX_TTL_SECS + 2 * CLOCK_SKEW_SECS);
        self.seen.retain(|_, t| *t >= floor);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;

    #[test]
    fn seal_open_roundtrip_hides_metadata() {
        let alice = ChatKey::generate();
        let bob = ChatKey::generate();
        let s = seal(&alice, &bob.public_identity(), "hi bob", 3600, NOW).unwrap();
        let wire = String::from_utf8(s.bytes.clone()).unwrap();
        assert!(!wire.contains(&alice.address()), "sender address visible on the wire");
        assert!(!wire.contains(&bob.address()), "recipient address visible on the wire");
        assert!(!wire.contains("hi bob"));
        assert_eq!(s.msg.tag, recipient_tag(&bob.address(), epoch_of(NOW)));
        assert_eq!(s.id, sha256_hex(&s.bytes));

        let parsed = SealedMessage::parse(&s.bytes).unwrap();
        let mut guard = ReplayGuard::default();
        let o = open(&bob, &parsed, NOW + 10, &mut guard).unwrap();
        assert_eq!(o.text, "hi bob");
        assert_eq!(o.sender_address, alice.address());
        assert_eq!(sha256_hex(&hex::decode(&o.ack).unwrap()), s.ack_hash);

        // Replay of the same message is rejected.
        let err = open(&bob, &parsed, NOW + 20, &mut guard).unwrap_err();
        assert!(err.to_string().contains("replayed"), "{err}");
        // Eve cannot open it.
        let eve = ChatKey::generate();
        assert!(unseal(&eve, &parsed, NOW).is_err());
    }

    #[test]
    fn tampering_and_stale_messages_are_rejected() {
        let alice = ChatKey::generate();
        let bob = ChatKey::generate();
        let s = seal(&alice, &bob.public_identity(), "hello", 3600, NOW).unwrap();
        // Flip a ciphertext byte.
        let mut t = s.msg.clone();
        let mut ct = BASE64_URL_SAFE_NO_PAD.decode(&t.ct).unwrap();
        ct[10] ^= 1;
        t.ct = BASE64_URL_SAFE_NO_PAD.encode(ct);
        assert!(unseal(&bob, &t, NOW).is_err());
        // Move it to another tag (AAD-bound).
        let mut t = s.msg.clone();
        t.tag = recipient_tag(&bob.address(), epoch_of(NOW) + 1);
        assert!(unseal(&bob, &t, NOW).is_err());
        // Past the window: tag no longer pulled, and the guard says too old.
        assert!(unseal(&bob, &s.msg, NOW + MAX_TTL_SECS + EPOCH_SECS * 2).is_err());
        let guard = ReplayGuard::default();
        assert!(guard.check("x", NOW, NOW + MAX_TTL_SECS + 10 * CLOCK_SKEW_SECS).is_err());
        assert!(guard.check("x", NOW + 2 * CLOCK_SKEW_SECS, NOW).is_err(), "future");
        assert!(guard.check("x", NOW, NOW + 100).is_ok());
    }

    #[test]
    fn forged_sender_is_rejected() {
        // Mallory seals a message whose inner identity claims to be Alice but
        // signs with her own key: the signatures do not verify.
        let alice = ChatKey::generate();
        let bob = ChatKey::generate();
        let mallory = ChatKey::generate();
        let s = seal(&mallory, &bob.public_identity(), "trust me", 3600, NOW).unwrap();
        // Re-seal with the inner sender swapped for Alice's identity.
        let mut forged = s.msg.clone();
        {
            // Decrypt the outer layer as Bob would, swap the sender, re-encrypt.
            let o = unseal(&bob, &forged, NOW).unwrap();
            assert_eq!(o.sender_address, mallory.address());
        }
        let inner_forged = {
            let rk = bob.public_identity().keys().unwrap();
            let env = crate::message::encrypt_dm_at(
                &alice.address(), &bob.address(), "trust me", &mallory.x25519_secret, &rk.x25519, &rk.ml_kem,
                &mallory.ed25519_key, &mallory.ml_dsa_key, 3600, NOW,
            )
            .unwrap();
            SealedInner { v: 1, sender: alice.public_identity(), envelope: env, ack: "00".repeat(32) }
        };
        let rk = bob.public_identity().keys().unwrap();
        let eph = X25519Secret::random_from_rng(OsRng);
        let eph_pub = X25519Public::from(&eph);
        let (kem_ct, kem_ss) = rk.ml_kem.encapsulate();
        let key = outer_key(eph.diffie_hellman(&rk.x25519).as_bytes(), kem_ss.as_ref(), eph_pub.as_bytes(), &forged.tag);
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ct = cipher
            .encrypt(&nonce, Payload { msg: &serde_json::to_vec(&inner_forged).unwrap(), aad: &outer_aad(&forged.tag) })
            .unwrap();
        forged.eph = BASE64_URL_SAFE_NO_PAD.encode(eph_pub.as_bytes());
        forged.kem_ct = BASE64_URL_SAFE_NO_PAD.encode(&kem_ct[..]);
        forged.nonce = BASE64_URL_SAFE_NO_PAD.encode(nonce.as_slice());
        forged.ct = BASE64_URL_SAFE_NO_PAD.encode(ct);
        let err = unseal(&bob, &forged, NOW).unwrap_err();
        assert!(err.to_string().contains("signature"), "{err}");
    }

    #[test]
    fn tags_cover_the_whole_ttl() {
        let addr = "ab".repeat(32);
        let sent_at = NOW;
        let tag = recipient_tag(&addr, epoch_of(sent_at));
        for later in [0, 3600, 7 * 86_400, MAX_TTL_SECS] {
            assert!(pull_tags(&addr, sent_at + later).contains(&tag), "+{later}s");
        }
        assert!(pull_tags(&addr, NOW).len() <= 7);
        assert_ne!(recipient_tag(&addr, 1), recipient_tag(&addr, 2));
        assert!(is_hex32(&tag));
    }

    #[test]
    fn oversized_text_is_refused() {
        let a = ChatKey::generate();
        let b = ChatKey::generate();
        assert!(seal(&a, &b.public_identity(), &"x".repeat(MAX_TEXT_BYTES + 1), 3600, NOW).is_err());
        let s = seal(&a, &b.public_identity(), &"x".repeat(MAX_TEXT_BYTES), 3600, NOW).unwrap();
        assert!(s.bytes.len() <= MAX_SEALED_BYTES);
    }
}
