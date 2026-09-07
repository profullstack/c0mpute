//! Native c0mpute network identity.
//!
//! A c0mpute peer's protocol identity is an ed25519 keypair rendered as a
//! DID:
//!
//! ```text
//! did:c0mpute:z6MkhaXgBZDvotDkL5257faiztiGiC2QtKLGpbnnEGta2doK
//!             ^ multibase base58btc of (0xed 0x01 || 32-byte public key)
//! ```
//!
//! The encoding after `z` is byte-identical to `did:key` for ed25519, so
//! any DID tooling that already resolves `did:key:z6Mk…` resolves a
//! c0mpute identity by swapping the method name. That is deliberate:
//! the network needs a stable, self-certifying identifier, not a new
//! registry.
//!
//! **Identity is not payment.** A node generates this locally, offline,
//! with no account anywhere. CoinPay wallets, KYC attestations, hardware
//! attestations, and organization membership all attach to this key as
//! separate credentials (see `docs/protocol/identity.md`); none of them
//! are required to join the network, advertise capacity, or be paid by an
//! adapter that does not use them.
//!
//! The bytes are the same ed25519 key libp2p already persists at
//! `<config_dir>/identity.key`, so a node has exactly one identity across
//! transport and protocol layers. This crate stays free of any libp2p
//! dependency — [`SigningIdentity::from_seed`] takes the raw 32 bytes and
//! the transport layer does the bridging.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::Error;

/// DID method prefix for a c0mpute network identity.
pub const DID_PREFIX: &str = "did:c0mpute:";

/// Multicodec varint for an ed25519 public key.
const MULTICODEC_ED25519_PUB: [u8; 2] = [0xed, 0x01];

/// A peer's public identity: `did:c0mpute:z…`.
///
/// Self-certifying — the DID *is* the public key, so verifying a signature
/// needs no lookup, no registry, and no network round trip.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Did {
    key: VerifyingKey,
    text: String,
}

impl Did {
    /// Build a DID from a raw 32-byte ed25519 public key.
    pub fn from_public_key_bytes(bytes: &[u8; 32]) -> Result<Self, Error> {
        let key = VerifyingKey::from_bytes(bytes)
            .map_err(|e| Error::Identity(format!("not a valid ed25519 public key: {e}")))?;
        Ok(Self::from_verifying_key(key))
    }

    fn from_verifying_key(key: VerifyingKey) -> Self {
        let mut multi = Vec::with_capacity(34);
        multi.extend_from_slice(&MULTICODEC_ED25519_PUB);
        multi.extend_from_slice(key.as_bytes());
        let text = format!("{DID_PREFIX}z{}", bs58::encode(&multi).into_string());
        Self { key, text }
    }

    /// Parse `did:c0mpute:z…` back into a verifiable public key.
    pub fn parse(s: &str) -> Result<Self, Error> {
        let body = s
            .strip_prefix(DID_PREFIX)
            .ok_or_else(|| Error::Identity(format!("DID must start with {DID_PREFIX:?}")))?;
        let b58 = body.strip_prefix('z').ok_or_else(|| {
            Error::Identity("DID body must use multibase base58btc (leading 'z')".into())
        })?;
        let multi = bs58::decode(b58)
            .into_vec()
            .map_err(|e| Error::Identity(format!("DID body is not valid base58btc: {e}")))?;
        let Some((codec, key_bytes)) = multi.split_at_checked(2) else {
            return Err(Error::Identity("DID body is too short".into()));
        };
        if codec != MULTICODEC_ED25519_PUB {
            return Err(Error::Identity(format!(
                "unsupported key type {codec:02x?}; c0mpute identities are ed25519"
            )));
        }
        let arr: [u8; 32] = key_bytes
            .try_into()
            .map_err(|_| Error::Identity("ed25519 public key must be 32 bytes".into()))?;
        Self::from_public_key_bytes(&arr)
    }

    /// The DID string, e.g. `did:c0mpute:z6Mk…`.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Raw 32-byte ed25519 public key.
    pub fn public_key_bytes(&self) -> [u8; 32] {
        self.key.to_bytes()
    }

    /// Verify a detached signature over `msg`.
    pub fn verify(&self, msg: &[u8], sig: &EnvelopeSignature) -> Result<(), Error> {
        self.key
            .verify(msg, &sig.0)
            .map_err(|_| Error::BadSignature)
    }
}

impl std::fmt::Display for Did {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

impl std::fmt::Debug for Did {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Did({})", self.text)
    }
}

impl Serialize for Did {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.text)
    }
}

impl<'de> Deserialize<'de> for Did {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Did::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// A node's private half: the key that signs envelopes.
///
/// Never serialized. Loaded from the same on-disk key the transport uses.
pub struct SigningIdentity {
    key: SigningKey,
    did: Did,
}

impl SigningIdentity {
    /// Build from a raw 32-byte ed25519 secret key (an "expanded seed" in
    /// RFC 8032 terms). This is the same 32 bytes libp2p's
    /// `Keypair::try_into_ed25519()?.secret()` yields, so transport and
    /// protocol identity stay one key.
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        let key = SigningKey::from_bytes(seed);
        let did = Did::from_verifying_key(key.verifying_key());
        Self { key, did }
    }

    /// This identity's public DID.
    pub fn did(&self) -> &Did {
        &self.did
    }

    /// Sign arbitrary bytes. Callers should prefer the envelope helpers,
    /// which apply domain separation before reaching this.
    pub fn sign(&self, msg: &[u8]) -> EnvelopeSignature {
        EnvelopeSignature(self.key.sign(msg))
    }
}

impl std::fmt::Debug for SigningIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never render key material, not even by accident in a log line.
        write!(f, "SigningIdentity({})", self.did)
    }
}

/// A detached ed25519 signature, wire-encoded as multibase base58btc
/// (`z…`) to match the DID encoding.
#[derive(Clone, PartialEq, Eq)]
pub struct EnvelopeSignature(Signature);

impl EnvelopeSignature {
    pub fn to_wire(&self) -> String {
        format!("z{}", bs58::encode(self.0.to_bytes()).into_string())
    }

    pub fn parse(s: &str) -> Result<Self, Error> {
        let b58 = s.strip_prefix('z').ok_or_else(|| {
            Error::Identity("signature must use multibase base58btc (leading 'z')".into())
        })?;
        let bytes = bs58::decode(b58)
            .into_vec()
            .map_err(|e| Error::Identity(format!("signature is not valid base58btc: {e}")))?;
        let arr: [u8; 64] = bytes
            .try_into()
            .map_err(|_| Error::Identity("ed25519 signature must be 64 bytes".into()))?;
        Ok(Self(Signature::from_bytes(&arr)))
    }
}

impl std::fmt::Debug for EnvelopeSignature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "EnvelopeSignature({})", self.to_wire())
    }
}

impl Serialize for EnvelopeSignature {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_wire())
    }
}

impl<'de> Deserialize<'de> for EnvelopeSignature {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        EnvelopeSignature::parse(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixed seeds keep the vectors deterministic — another implementation
    /// can check itself against the DIDs these produce.
    pub(crate) fn test_identity(byte: u8) -> SigningIdentity {
        SigningIdentity::from_seed(&[byte; 32])
    }

    #[test]
    fn did_uses_the_did_key_ed25519_encoding() {
        let id = test_identity(1);
        let did = id.did().as_str();
        assert!(did.starts_with("did:c0mpute:z6Mk"), "got {did}");
    }

    #[test]
    fn did_round_trips_through_its_string_form() {
        let id = test_identity(7);
        let parsed = Did::parse(id.did().as_str()).unwrap();
        assert_eq!(&parsed, id.did());
        assert_eq!(parsed.public_key_bytes(), id.did().public_key_bytes());
    }

    #[test]
    fn distinct_seeds_give_distinct_dids() {
        assert_ne!(test_identity(1).did(), test_identity(2).did());
    }

    #[test]
    fn signature_round_trips_and_verifies() {
        let id = test_identity(3);
        let sig = id.sign(b"hello c0mpute");
        let wire = sig.to_wire();
        let parsed = EnvelopeSignature::parse(&wire).unwrap();
        id.did().verify(b"hello c0mpute", &parsed).unwrap();
    }

    #[test]
    fn signature_does_not_verify_for_other_bytes() {
        let id = test_identity(3);
        let sig = id.sign(b"hello c0mpute");
        let err = id.did().verify(b"hello c0mputer", &sig).unwrap_err();
        assert!(matches!(err, Error::BadSignature));
    }

    #[test]
    fn signature_does_not_verify_under_another_did() {
        let a = test_identity(4);
        let b = test_identity(5);
        let sig = a.sign(b"payload");
        assert!(matches!(
            b.did().verify(b"payload", &sig).unwrap_err(),
            Error::BadSignature
        ));
    }

    #[test]
    fn parse_rejects_a_foreign_method() {
        // The example ed25519 identifier from the did:key spec. It is a
        // *public* key — being public is the entire point of a DID — but
        // it is 44 characters of base58 and gitleaks' generic-api-key rule
        // scores it on entropy alone, so it needs the allow directive.
        let did_key = "did:key:z6MkhaXgBZDvotDkL5257faiztiGiC2QtKLGpbnnEGta2doK"; // gitleaks:allow
        assert!(Did::parse(did_key).is_err());
    }

    #[test]
    fn parse_rejects_a_non_ed25519_multicodec() {
        // 0xe7 0x01 is secp256k1-pub.
        let mut multi = vec![0xe7, 0x01];
        multi.extend_from_slice(&[9u8; 32]);
        let did = format!("{DID_PREFIX}z{}", bs58::encode(&multi).into_string());
        assert!(Did::parse(&did).is_err());
    }

    #[test]
    fn parse_rejects_a_truncated_body() {
        let did = format!("{DID_PREFIX}z{}", bs58::encode([0xed]).into_string());
        assert!(Did::parse(&did).is_err());
    }

    #[test]
    fn debug_never_prints_key_material() {
        let id = test_identity(6);
        let rendered = format!("{id:?}");
        assert!(rendered.contains(id.did().as_str()));
        assert!(!rendered.contains("SigningKey"));
    }
}
