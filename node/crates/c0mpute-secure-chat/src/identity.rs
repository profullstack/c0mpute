//! Public identities, addresses, and the signed records nodes gossip.
//!
//! A user's address is their public key: the SHA-256 of all four public keys
//! (X25519, Ed25519, ML-KEM-768, ML-DSA-65), shown as 64 hex characters. The
//! short fingerprint is the first 8 bytes of the address as colon-separated
//! hex pairs. Because the address commits to every key, a record fetched from
//! any node is checked against the address (or fingerprint) the user typed:
//! nobody can answer a lookup with a different key.
//!
//! * [`KeyRecord`]: a self-published, self-signed copy of a public identity.
//!   Gossiped and cached by mailbox nodes; that is the whole "directory".
//! * [`HomeRecord`]: "messages for me may be delivered directly to libp2p peer
//!   P until T", signed by the user. The node named there accepts direct
//!   deliveries for the address and gossips the record while it is online.

use base64::prelude::*;
use ed25519_dalek::{Signature as EdSig, Signer, Verifier, VerifyingKey as EdVk};
use ml_dsa::{KeyExport as _, MlDsa65, VerifyingKey as MlDsaVk};
use ml_kem::{EncapsulationKey, MlKem768};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use x25519_dalek::PublicKey as X25519Public;

use crate::keys::{ChatKey, KeyFile};

/// Longest a key or home record may claim to be valid for.
pub const MAX_RECORD_SECS: u64 = 31 * 86_400;
/// Clock skew tolerated on record and message timestamps.
pub const CLOCK_SKEW_SECS: u64 = 300;

/// The four public keys of a chat identity (same encoding as the keyfile's
/// `pubkey_*` fields: base64url, no padding).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicIdentity {
    pub pubkey_enc: String,
    pub pubkey_kem: String,
    pub pubkey_sig: String,
    pub pubkey_ml_dsa: String,
}

/// Parsed public keys, ready for crypto.
pub struct PublicKeys {
    pub x25519: X25519Public,
    pub ed25519: EdVk,
    pub ml_kem: EncapsulationKey<MlKem768>,
    pub ml_dsa: MlDsaVk<MlDsa65>,
}

impl PublicIdentity {
    pub fn of(key: &ChatKey) -> Self {
        Self {
            pubkey_enc: BASE64_URL_SAFE_NO_PAD.encode(key.x25519_public.as_bytes()),
            pubkey_kem: BASE64_URL_SAFE_NO_PAD.encode(&key.ml_kem_ek().to_bytes()[..]),
            pubkey_sig: BASE64_URL_SAFE_NO_PAD.encode(key.ed25519_public.as_bytes()),
            pubkey_ml_dsa: BASE64_URL_SAFE_NO_PAD.encode(&key.ml_dsa_public.to_bytes()[..]),
        }
    }

    pub fn from_keyfile(kf: &KeyFile) -> Self {
        Self {
            pubkey_enc: kf.pubkey_enc.clone(),
            pubkey_kem: kf.pubkey_kem.clone(),
            pubkey_sig: kf.pubkey_sig.clone(),
            pubkey_ml_dsa: kf.pubkey_ml_dsa.clone(),
        }
    }

    /// Decode and validate all four keys.
    pub fn keys(&self) -> anyhow::Result<PublicKeys> {
        let enc = BASE64_URL_SAFE_NO_PAD.decode(&self.pubkey_enc)?;
        let x: [u8; 32] = enc.as_slice().try_into().map_err(|_| anyhow::anyhow!("bad x25519 key length"))?;
        let sig = BASE64_URL_SAFE_NO_PAD.decode(&self.pubkey_sig)?;
        let s: [u8; 32] = sig.as_slice().try_into().map_err(|_| anyhow::anyhow!("bad ed25519 key length"))?;
        let ed25519 = EdVk::from_bytes(&s).map_err(|e| anyhow::anyhow!("bad ed25519 key: {e}"))?;
        let kem = BASE64_URL_SAFE_NO_PAD.decode(&self.pubkey_kem)?;
        let kem_arr: ml_kem::Key<EncapsulationKey<MlKem768>> = kem
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("bad ml-kem key length"))?;
        let ml_kem = EncapsulationKey::<MlKem768>::new(&kem_arr).map_err(|_| anyhow::anyhow!("bad ml-kem key"))?;
        let dsa = BASE64_URL_SAFE_NO_PAD.decode(&self.pubkey_ml_dsa)?;
        let dsa_arr: ml_dsa::EncodedVerifyingKey<MlDsa65> = dsa
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("bad ml-dsa key length"))?;
        let ml_dsa = MlDsaVk::<MlDsa65>::decode(&dsa_arr);
        Ok(PublicKeys {
            x25519: X25519Public::from(x),
            ed25519,
            ml_kem,
            ml_dsa,
        })
    }

    /// The address: hex SHA-256 over all four public keys.
    pub fn address(&self) -> String {
        let mut h = Sha256::new();
        h.update(b"c0mpute-dm-address-v1");
        for part in [&self.pubkey_enc, &self.pubkey_sig, &self.pubkey_kem, &self.pubkey_ml_dsa] {
            h.update((part.len() as u32).to_be_bytes());
            h.update(part.as_bytes());
        }
        hex::encode(h.finalize())
    }

    pub fn fingerprint(&self) -> String {
        fingerprint_of(&self.address())
    }

    /// A single pasteable string carrying the whole public identity, for
    /// sharing a key out of band (`c0mpute dm key show --full`).
    pub fn to_share_string(&self) -> String {
        format!(
            "c0dm1.{}",
            BASE64_URL_SAFE_NO_PAD.encode(serde_json::to_vec(self).unwrap_or_default())
        )
    }

    pub fn from_share_string(s: &str) -> anyhow::Result<Self> {
        let body = s.trim().strip_prefix("c0dm1.").ok_or_else(|| anyhow::anyhow!("not a c0dm1. identity"))?;
        let id: Self = serde_json::from_slice(&BASE64_URL_SAFE_NO_PAD.decode(body)?)?;
        id.keys()?;
        Ok(id)
    }
}

/// `aa:bb:cc:dd:ee:ff:00:11` from a 64-hex address.
pub fn fingerprint_of(address: &str) -> String {
    address
        .as_bytes()
        .chunks(2)
        .take(8)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect::<Vec<_>>()
        .join(":")
}

/// What a user typed to name someone: a full address, or a fingerprint
/// (16 hex chars, colons optional). Lowercased, colons removed.
pub fn normalize_query(q: &str) -> Option<String> {
    let s: String = q.trim().chars().filter(|c| *c != ':').collect::<String>().to_ascii_lowercase();
    let ok = (s.len() == 16 || s.len() == 64) && s.chars().all(|c| c.is_ascii_hexdigit());
    ok.then_some(s)
}

/// Does `identity` answer `query` (normalized address or fingerprint)?
pub fn matches_query(identity: &PublicIdentity, query: &str) -> bool {
    identity.address().starts_with(query)
}

impl ChatKey {
    pub fn public_identity(&self) -> PublicIdentity {
        PublicIdentity::of(self)
    }

    pub fn address(&self) -> String {
        self.public_identity().address()
    }

    /// Ed25519 + ML-DSA-65 signatures over `msg`, base64url.
    pub fn sign_both(&self, msg: &[u8]) -> anyhow::Result<(String, String)> {
        let ed: EdSig = self.ed25519_key.sign(msg);
        let ml = ml_dsa::Signer::try_sign(&self.ml_dsa_key, msg).map_err(|e| anyhow::anyhow!("ml-dsa sign: {e}"))?;
        Ok((
            BASE64_URL_SAFE_NO_PAD.encode(ed.to_bytes()),
            BASE64_URL_SAFE_NO_PAD.encode(&ml.encode()[..]),
        ))
    }
}

/// Check both signatures from [`ChatKey::sign_both`] against `keys`.
pub fn verify_both(keys: &PublicKeys, msg: &[u8], sig: &str, ml_dsa_sig: &str) -> anyhow::Result<()> {
    let s = BASE64_URL_SAFE_NO_PAD.decode(sig)?;
    let arr: [u8; 64] = s.as_slice().try_into().map_err(|_| anyhow::anyhow!("bad ed25519 signature length"))?;
    keys.ed25519
        .verify(msg, &EdSig::from_bytes(&arr))
        .map_err(|_| anyhow::anyhow!("ed25519 signature invalid"))?;
    let m = BASE64_URL_SAFE_NO_PAD.decode(ml_dsa_sig)?;
    let enc: ml_dsa::EncodedSignature<MlDsa65> =
        m.as_slice().try_into().map_err(|_| anyhow::anyhow!("bad ml-dsa signature length"))?;
    let sig = ml_dsa::Signature::<MlDsa65>::decode(&enc).ok_or_else(|| anyhow::anyhow!("malformed ml-dsa signature"))?;
    ml_dsa::Verifier::verify(&keys.ml_dsa, msg, &sig).map_err(|_| anyhow::anyhow!("ml-dsa signature invalid"))?;
    Ok(())
}

/// A self-published public identity, signed by itself.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KeyRecord {
    pub v: u8,
    pub identity: PublicIdentity,
    pub created_at: u64,
    pub expires_at: u64,
    pub sig: String,
    pub ml_dsa_sig: String,
}

fn key_record_bytes(address: &str, created_at: u64, expires_at: u64) -> Vec<u8> {
    format!("c0mpute-dm-keyrecord-v1|{address}|{created_at}|{expires_at}").into_bytes()
}

impl KeyRecord {
    pub fn sign(key: &ChatKey, now: u64, valid_secs: u64) -> anyhow::Result<Self> {
        let identity = key.public_identity();
        let expires_at = now + valid_secs.min(MAX_RECORD_SECS);
        let (sig, ml_dsa_sig) = key.sign_both(&key_record_bytes(&identity.address(), now, expires_at))?;
        Ok(Self {
            v: 1,
            identity,
            created_at: now,
            expires_at,
            sig,
            ml_dsa_sig,
        })
    }

    /// Signature valid, not expired, not from the future.
    pub fn verify(&self, now: u64) -> anyhow::Result<()> {
        anyhow::ensure!(self.v == 1, "unsupported key record version");
        anyhow::ensure!(self.created_at <= now + CLOCK_SKEW_SECS, "key record from the future");
        anyhow::ensure!(self.expires_at > now, "key record expired");
        anyhow::ensure!(self.expires_at - self.created_at <= MAX_RECORD_SECS, "key record validity too long");
        let keys = self.identity.keys()?;
        verify_both(
            &keys,
            &key_record_bytes(&self.identity.address(), self.created_at, self.expires_at),
            &self.sig,
            &self.ml_dsa_sig,
        )
    }

    pub fn address(&self) -> String {
        self.identity.address()
    }
}

/// "Deliver my messages directly to libp2p peer `peer`", signed by the user.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HomeRecord {
    pub v: u8,
    pub identity: PublicIdentity,
    /// Base58 libp2p peer id of the node that receives for this address.
    pub peer: String,
    pub created_at: u64,
    pub expires_at: u64,
    pub sig: String,
    pub ml_dsa_sig: String,
}

fn home_record_bytes(address: &str, peer: &str, created_at: u64, expires_at: u64) -> Vec<u8> {
    format!("c0mpute-dm-home-v1|{address}|{peer}|{created_at}|{expires_at}").into_bytes()
}

impl HomeRecord {
    pub fn sign(key: &ChatKey, peer: &str, now: u64, valid_secs: u64) -> anyhow::Result<Self> {
        let identity = key.public_identity();
        let expires_at = now + valid_secs.min(MAX_RECORD_SECS);
        let (sig, ml_dsa_sig) = key.sign_both(&home_record_bytes(&identity.address(), peer, now, expires_at))?;
        Ok(Self {
            v: 1,
            identity,
            peer: peer.to_string(),
            created_at: now,
            expires_at,
            sig,
            ml_dsa_sig,
        })
    }

    pub fn verify(&self, now: u64) -> anyhow::Result<()> {
        anyhow::ensure!(self.v == 1, "unsupported home record version");
        anyhow::ensure!(self.created_at <= now + CLOCK_SKEW_SECS, "home record from the future");
        anyhow::ensure!(self.expires_at > now, "home record expired");
        anyhow::ensure!(self.expires_at - self.created_at <= MAX_RECORD_SECS, "home record validity too long");
        let keys = self.identity.keys()?;
        verify_both(
            &keys,
            &home_record_bytes(&self.identity.address(), &self.peer, self.created_at, self.expires_at),
            &self.sig,
            &self.ml_dsa_sig,
        )
    }

    pub fn address(&self) -> String {
        self.identity.address()
    }

    /// The key record this home record implies (same identity), for caches.
    pub fn identity(&self) -> &PublicIdentity {
        &self.identity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_and_fingerprint_are_stable_and_match_keyfile() {
        let k = ChatKey::from_seed([7u8; 32]);
        let id = k.public_identity();
        assert_eq!(id.address().len(), 64);
        assert_eq!(id.address(), ChatKey::from_seed([7u8; 32]).address());
        assert_ne!(id.address(), ChatKey::from_seed([8u8; 32]).address());
        let fp = id.fingerprint();
        assert_eq!(fp.len(), 23);
        assert_eq!(normalize_query(&fp).unwrap(), id.address()[..16]);
        assert!(matches_query(&id, &normalize_query(&fp).unwrap()));
        assert!(matches_query(&id, &normalize_query(&id.address().to_uppercase()).unwrap()));
        assert!(normalize_query("zz").is_none());
        // Same identity from the (password-protected) keyfile's public fields.
        let kf = k.encrypt_to_keyfile("correct horse battery", None).unwrap();
        assert_eq!(PublicIdentity::from_keyfile(&kf), id);
        id.keys().unwrap();
        assert_eq!(PublicIdentity::from_share_string(&id.to_share_string()).unwrap(), id);
    }

    #[test]
    fn key_and_home_records_verify_and_reject_tampering() {
        let k = ChatKey::generate();
        let now = 1_800_000_000;
        let rec = KeyRecord::sign(&k, now, 86_400).unwrap();
        rec.verify(now).unwrap();
        assert!(rec.verify(now + 86_401).is_err(), "expired");
        let mut bad = rec.clone();
        bad.expires_at += 10;
        assert!(bad.verify(now).is_err());
        // Someone else's identity with this signature.
        let mut bad = rec.clone();
        bad.identity = ChatKey::generate().public_identity();
        assert!(bad.verify(now).is_err());

        let home = HomeRecord::sign(&k, "12D3KooWpeer", now, 3600).unwrap();
        home.verify(now).unwrap();
        let mut bad = home.clone();
        bad.peer = "12D3KooWevil".into();
        assert!(bad.verify(now).is_err());
        assert!(home.verify(now - 10_000).is_err(), "from the future");
    }
}
