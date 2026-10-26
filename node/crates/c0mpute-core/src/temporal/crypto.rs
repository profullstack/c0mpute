//! Session call encryption: X25519 key agreement → HKDF-SHA256 →
//! AES-256-GCM. Gossip topics are readable by every subscriber, so the
//! observations and proposals of an inference session travel sealed between
//! the buyer and the one worker holding the session. This protects data on
//! the wire only; the worker necessarily sees plaintext.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use hkdf::Hkdf;
use rand::RngCore;
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};

pub struct KeyPair {
    pub secret: StaticSecret,
    pub public_hex: String,
}

pub fn keypair() -> KeyPair {
    let secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
    let public_hex = hex::encode(PublicKey::from(&secret).as_bytes());
    KeyPair { secret, public_hex }
}

pub fn secret_from_hex(h: &str) -> Result<StaticSecret> {
    let b: [u8; 32] = hex::decode(h)?.try_into().map_err(|_| anyhow!("secret must be 32 bytes"))?;
    Ok(StaticSecret::from(b))
}

/// Shared session key; `session_id` binds it to one session.
pub fn session_key(secret: &StaticSecret, peer_pub_hex: &str, session_id: &str) -> Result<[u8; 32]> {
    let p: [u8; 32] = hex::decode(peer_pub_hex)?.try_into().map_err(|_| anyhow!("public key must be 32 bytes"))?;
    let shared = secret.diffie_hellman(&PublicKey::from(p));
    if !shared.was_contributory() {
        bail!("weak peer public key");
    }
    let hk = Hkdf::<Sha256>::new(Some(b"c0mpute/infernet.temporal/v1"), shared.as_bytes());
    let mut key = [0u8; 32];
    hk.expand(session_id.as_bytes(), &mut key).map_err(|_| anyhow!("hkdf expand"))?;
    Ok(key)
}

pub fn seal(key: &[u8; 32], plaintext: &[u8]) -> Result<String> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| anyhow!("key"))?;
    let mut nonce = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let ct = cipher.encrypt(Nonce::from_slice(&nonce), plaintext).map_err(|_| anyhow!("encrypt"))?;
    let mut out = nonce.to_vec();
    out.extend(ct);
    Ok(B64.encode(out))
}

pub fn open(key: &[u8; 32], sealed: &str) -> Result<Vec<u8>> {
    let raw = B64.decode(sealed)?;
    if raw.len() < 12 + 16 {
        bail!("sealed payload too short");
    }
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| anyhow!("key"))?;
    cipher.decrypt(Nonce::from_slice(&raw[..12]), &raw[12..]).map_err(|_| anyhow!("decrypt failed (wrong key or tampered)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_sides_derive_the_same_key_and_tamper_fails() {
        let a = keypair();
        let b = keypair();
        let ka = session_key(&a.secret, &b.public_hex, "s1").unwrap();
        let kb = session_key(&b.secret, &a.public_hex, "s1").unwrap();
        assert_eq!(ka, kb);
        assert_ne!(ka, session_key(&a.secret, &b.public_hex, "s2").unwrap());
        let s = seal(&ka, b"{\"op\":\"predict\"}").unwrap();
        assert_eq!(open(&kb, &s).unwrap(), b"{\"op\":\"predict\"}");
        let mut raw = B64.decode(&s).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 1;
        assert!(open(&kb, &B64.encode(raw)).is_err());
        let c = keypair();
        let kc = session_key(&c.secret, &a.public_hex, "s1").unwrap();
        assert!(open(&kc, &s).is_err());
    }
}
