//! Storage keys, derived from a user's c0mpute identity seed.
//!
//! The c0mpute identity (`c0mpute dm keygen`, `~/.config/c0mpute/chat.key`)
//! is one 32-byte seed kept under an Argon2id password and backed up as an
//! encrypted JSON blob. Storage keys are HKDF-derived from that same seed
//! under their own labels, so:
//!
//! * there is nothing new to back up: restoring the identity backup restores
//!   access to every private object;
//! * storage keys are cryptographically independent of the chat keys (a
//!   different label is a different key), so neither protocol can be turned
//!   against the other;
//! * per-object signing keys are derived from the seed and the object's
//!   random name, so each object's network record is signed by a key nobody
//!   can link to the owner, yet the owner can re-derive it on any machine to
//!   update, share, revoke or delete the object.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hkdf::Hkdf;
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroizing;

const LABEL_X25519: &[u8] = b"c0mpute-private-storage-x25519-v1";
const LABEL_SIGN: &[u8] = b"c0mpute-private-storage-object-sign-v1";
const LABEL_LOCAL: &[u8] = b"c0mpute-private-storage-local-index-v1";
/// Prefix of a shareable storage identity string.
pub const SHARE_PREFIX: &str = "c0ps1.";

/// A user's storage secrets. Every field zeroizes on drop
/// (`StaticSecret` zeroizes itself; the rest are `Zeroizing`).
pub struct StorageSecret {
    x25519: StaticSecret,
    sign_root: Zeroizing<[u8; 32]>,
    local_key: Zeroizing<[u8; 32]>,
}

impl StorageSecret {
    /// Derive from the 32-byte c0mpute identity seed.
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        let hk = Hkdf::<Sha256>::new(None, seed);
        let mut x = Zeroizing::new([0u8; 32]);
        hk.expand(LABEL_X25519, x.as_mut()).expect("32 bytes is a valid HKDF length");
        let mut s = Zeroizing::new([0u8; 32]);
        hk.expand(LABEL_SIGN, s.as_mut()).expect("32 bytes is a valid HKDF length");
        let mut l = Zeroizing::new([0u8; 32]);
        hk.expand(LABEL_LOCAL, l.as_mut()).expect("32 bytes is a valid HKDF length");
        Self {
            x25519: StaticSecret::from(*x),
            sign_root: s,
            local_key: l,
        }
    }

    /// A fresh random secret (tests, throwaway identities).
    pub fn generate() -> Self {
        let mut seed = Zeroizing::new([0u8; 32]);
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, seed.as_mut());
        Self::from_seed(&seed)
    }

    pub fn public(&self) -> StoragePublic {
        StoragePublic(PublicKey::from(&self.x25519).to_bytes())
    }

    pub(crate) fn x25519(&self) -> &StaticSecret {
        &self.x25519
    }

    /// The ed25519 secret key bytes that sign the network record of the
    /// object named `object_name`. Callers turn these into whatever signing
    /// type their transport needs and must not persist them.
    pub fn object_signing_seed(&self, object_name: &str) -> Zeroizing<[u8; 32]> {
        let hk = Hkdf::<Sha256>::new(Some(object_name.as_bytes()), self.sign_root.as_ref());
        let mut out = Zeroizing::new([0u8; 32]);
        hk.expand(b"object-record-key", out.as_mut()).expect("32 bytes is a valid HKDF length");
        out
    }

    /// The ed25519 signing key for `object_name`.
    pub fn object_signing_key(&self, object_name: &str) -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&self.object_signing_seed(object_name))
    }

    /// A stable, unlinkable network name for one of the owner's named
    /// objects (a private site called `label`): the same label always gives
    /// the same name for this owner, and nobody without the secret can tell
    /// two owners' names apart or link a name to its label.
    pub fn named_object(&self, label: &str) -> String {
        let hk = Hkdf::<Sha256>::new(Some(b"c0mpute-private-named-object-v1"), self.sign_root.as_ref());
        let mut out = [0u8; 16];
        hk.expand(label.as_bytes(), &mut out).expect("16 bytes is a valid HKDF length");
        crate::format::base32_name(&out)
    }

    /// Key for encrypting the owner's local object index at rest.
    pub fn local_index_key(&self) -> &[u8; 32] {
        &self.local_key
    }
}

impl std::fmt::Debug for StorageSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print key material, even in debug output.
        f.debug_struct("StorageSecret").field("public", &self.public()).finish_non_exhaustive()
    }
}

/// A storage public key: what someone needs to share an object with you.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct StoragePublic(pub [u8; 32]);

impl StoragePublic {
    /// `c0ps1.<base64url>`: paste this to someone who wants to share with you.
    pub fn to_share_string(&self) -> String {
        format!("{SHARE_PREFIX}{}", URL_SAFE_NO_PAD.encode(self.0))
    }

    pub fn from_share_string(s: &str) -> anyhow::Result<Self> {
        let body = s
            .trim()
            .strip_prefix(SHARE_PREFIX)
            .ok_or_else(|| anyhow::anyhow!("not a storage identity (expected {SHARE_PREFIX}...)"))?;
        let bytes = URL_SAFE_NO_PAD.decode(body)?;
        let arr: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("storage identity must be 32 bytes"))?;
        if arr == [0u8; 32] {
            anyhow::bail!("storage identity is the all-zero key");
        }
        Ok(Self(arr))
    }

    /// Short human check value: first 8 bytes of blake3, colon-separated.
    pub fn fingerprint(&self) -> String {
        let h = blake3::hash(&self.0);
        h.as_bytes()[..8].iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
    }
}

impl std::fmt::Debug for StoragePublic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "StoragePublic({})", self.fingerprint())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derivation_is_deterministic_and_label_separated() {
        let a = StorageSecret::from_seed(&[1u8; 32]);
        let b = StorageSecret::from_seed(&[1u8; 32]);
        let c = StorageSecret::from_seed(&[2u8; 32]);
        assert_eq!(a.public(), b.public());
        assert_ne!(a.public(), c.public());
        assert_eq!(*a.object_signing_seed("x"), *b.object_signing_seed("x"));
        assert_ne!(*a.object_signing_seed("x"), *a.object_signing_seed("y"));
        assert_ne!(a.local_index_key(), &*a.object_signing_seed("x"));
    }

    #[test]
    fn share_string_round_trips() {
        let p = StorageSecret::generate().public();
        let s = p.to_share_string();
        assert!(s.starts_with(SHARE_PREFIX));
        assert_eq!(StoragePublic::from_share_string(&s).unwrap(), p);
        assert!(StoragePublic::from_share_string("c0dm1.abc").is_err());
        assert!(StoragePublic::from_share_string(&format!("{SHARE_PREFIX}{}", URL_SAFE_NO_PAD.encode([0u8; 32]))).is_err());
    }

    #[test]
    fn debug_never_prints_secrets() {
        let s = StorageSecret::from_seed(&[9u8; 32]);
        let dbg = format!("{s:?}");
        let secret_hex: String = s.local_index_key().iter().map(|b| format!("{b:02x}")).collect();
        assert!(!dbg.contains(&secret_hex[..16]));
    }
}
