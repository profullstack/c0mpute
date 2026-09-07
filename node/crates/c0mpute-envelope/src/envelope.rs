//! The signed envelope every c0mpute protocol message travels in.
//!
//! ```json
//! {
//!   "v": 1,
//!   "type": "c0mpute.provider.advert/v1",
//!   "signer": "did:c0mpute:z6Mk…",
//!   "payload": { … },
//!   "sig": "z3yf…"
//! }
//! ```
//!
//! Why an envelope at all, when gossipsub already signs each message with
//! the publisher's key? Because a transport signature dies at the first
//! hop. An advert relayed by an indexer, a receipt stored in a buyer's
//! local log, an offer forwarded by a gateway, a job replayed from disk
//! after a restart — none of those carry the gossipsub signature, and all
//! of them need to be verifiable by someone who was not on the wire when
//! the message was published. The v1 types in `c0mpute-net::topics`
//! leaned on transport authenticity; that is what made reputation
//! non-portable and required a database to be believed.
//!
//! Signatures are domain-separated by [`DOMAIN`] and by the payload type,
//! so a signature harvested from one message type can never be replayed as
//! another.

use serde::{Serialize, de::DeserializeOwned};

use crate::Error;
use crate::canonical::canonicalize;
use crate::identity::{Did, EnvelopeSignature, SigningIdentity};
use crate::types::{ContentHash, Timestamp};

/// Envelope format version. Bumped only for a breaking change to the
/// envelope's own shape, never for a payload change (payload types carry
/// their own version in [`Payload::TYPE`]).
pub const ENVELOPE_VERSION: u32 = 1;

/// Domain-separation tag prefixed to every signing input.
pub const DOMAIN: &[u8] = b"c0mpute-envelope/v1\n";

/// A payload that can travel in a signed envelope.
pub trait Payload: Serialize + DeserializeOwned {
    /// Stable type identifier, e.g. `c0mpute.job/v2`. Part of the signing
    /// input, so it is not forgeable after the fact.
    const TYPE: &'static str;

    /// Structural checks that must hold before a payload is signed and
    /// after it is verified. A malformed payload should never reach
    /// application code with a valid signature attached.
    fn validate(&self) -> Result<(), Error>;

    /// When this payload stops being usable, if it says so itself.
    /// Adverts and offers expire; receipts are permanent records and do
    /// not.
    fn expires_at(&self) -> Option<&Timestamp> {
        None
    }
}

/// A payload plus the signature of the peer that vouched for it.
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct Envelope<T> {
    /// Envelope format version.
    pub v: u32,
    /// Payload type identifier.
    #[serde(rename = "type")]
    pub type_id: String,
    /// The identity that signed the payload.
    pub signer: Did,
    /// The payload itself.
    pub payload: T,
    /// Detached signature over the domain-separated canonical bytes.
    pub sig: EnvelopeSignature,
}

/// The exact fields a signature covers, in the order serde emits them.
/// Canonicalization sorts keys, so field order here is irrelevant to the
/// output — this struct exists only to avoid cloning the payload.
#[derive(Serialize)]
struct SigningView<'a, T> {
    v: u32,
    #[serde(rename = "type")]
    type_id: &'a str,
    signer: &'a Did,
    payload: &'a T,
}

/// Build the bytes a signature is computed over.
fn signing_input<T: Serialize>(type_id: &str, signer: &Did, payload: &T) -> Result<Vec<u8>, Error> {
    let view = SigningView {
        v: ENVELOPE_VERSION,
        type_id,
        signer,
        payload,
    };
    let canonical = canonicalize(&view)?;
    let mut input = Vec::with_capacity(DOMAIN.len() + type_id.len() + 1 + canonical.len());
    input.extend_from_slice(DOMAIN);
    input.extend_from_slice(type_id.as_bytes());
    input.push(b'\n');
    input.extend_from_slice(&canonical);
    Ok(input)
}

impl<T: Payload> Envelope<T> {
    /// Validate, then sign, a payload.
    ///
    /// Validation happens *before* signing on purpose: a signature is a
    /// claim that the signer stands behind the contents, and there is no
    /// reason to stand behind a payload we already know is malformed.
    pub fn seal(identity: &SigningIdentity, payload: T) -> Result<Self, Error> {
        payload.validate()?;
        let signer = identity.did().clone();
        let input = signing_input(T::TYPE, &signer, &payload)?;
        let sig = identity.sign(&input);
        Ok(Self {
            v: ENVELOPE_VERSION,
            type_id: T::TYPE.to_string(),
            signer,
            payload,
            sig,
        })
    }

    /// Verify the envelope and hand back the payload it carries.
    ///
    /// Checks, in order: envelope version, payload type, signature, then
    /// payload structure. Nothing else in the codebase should read
    /// `envelope.payload` directly — going through here is what makes
    /// "we saw this message" and "this peer signed this message" the same
    /// statement.
    pub fn open(&self) -> Result<&T, Error> {
        if self.v != ENVELOPE_VERSION {
            return Err(Error::Format(format!(
                "unsupported envelope version {}; this node speaks v{ENVELOPE_VERSION}",
                self.v
            )));
        }
        if self.type_id != T::TYPE {
            return Err(Error::TypeMismatch {
                expected: T::TYPE,
                found: self.type_id.clone(),
            });
        }
        let input = signing_input(&self.type_id, &self.signer, &self.payload)?;
        self.signer.verify(&input, &self.sig)?;
        self.payload.validate()?;
        Ok(&self.payload)
    }

    /// [`Envelope::open`] plus a freshness check against `now`.
    ///
    /// Expired adverts and offers are the normal case on a churning
    /// network, not an attack — a provider that went offline leaves its
    /// last advert in every peer's cache. Rejecting them on read is what
    /// keeps a stale cache from looking like live capacity.
    pub fn open_fresh(&self, now: &Timestamp) -> Result<&T, Error> {
        let payload = self.open()?;
        if let Some(expiry) = payload.expires_at() {
            if expiry.is_expired_at(now) {
                return Err(Error::Expired {
                    expired_at: expiry.to_string(),
                    now: now.to_string(),
                });
            }
        }
        Ok(payload)
    }

    /// Stable content hash of the sealed envelope.
    ///
    /// Two nodes that received the same envelope compute the same hash, so
    /// this is the identifier to cite an advert, offer, or receipt by
    /// without re-sending it.
    pub fn content_hash(&self) -> Result<ContentHash, Error> {
        Ok(ContentHash::of(&canonicalize(self)?))
    }

    /// Serialize to canonical JSON — the form to store or transmit, so a
    /// re-read produces the same hash.
    pub fn to_canonical_json(&self) -> Result<String, Error> {
        crate::canonical::canonicalize_to_string(self)
    }

    /// Parse an envelope from JSON. Does **not** verify; call
    /// [`Envelope::open`] next.
    pub fn from_json(s: &str) -> Result<Self, Error> {
        Ok(serde_json::from_str(s)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    struct Ping {
        note: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        expires_at: Option<Timestamp>,
    }

    impl Payload for Ping {
        const TYPE: &'static str = "c0mpute.test.ping/v1";
        fn validate(&self) -> Result<(), Error> {
            if self.note.is_empty() {
                return Err(Error::Format("note must not be empty".into()));
            }
            Ok(())
        }
        fn expires_at(&self) -> Option<&Timestamp> {
            self.expires_at.as_ref()
        }
    }

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    struct Pong {
        note: String,
    }

    impl Payload for Pong {
        // Same shape as Ping, different type id — this is what domain
        // separation has to defeat.
        const TYPE: &'static str = "c0mpute.test.pong/v1";
        fn validate(&self) -> Result<(), Error> {
            Ok(())
        }
    }

    fn identity(byte: u8) -> SigningIdentity {
        SigningIdentity::from_seed(&[byte; 32])
    }

    fn ping(note: &str) -> Ping {
        Ping {
            note: note.into(),
            expires_at: None,
        }
    }

    #[test]
    fn seal_then_open_returns_the_payload() {
        let id = identity(1);
        let env = Envelope::seal(&id, ping("hello")).unwrap();
        assert_eq!(env.open().unwrap().note, "hello");
        assert_eq!(env.signer.as_str(), id.did().as_str());
        assert_eq!(env.type_id, Ping::TYPE);
    }

    #[test]
    fn envelope_survives_a_json_round_trip() {
        let env = Envelope::seal(&identity(2), ping("durable")).unwrap();
        let json = env.to_canonical_json().unwrap();
        let parsed: Envelope<Ping> = Envelope::from_json(&json).unwrap();
        parsed.open().unwrap();
        assert_eq!(parsed.content_hash().unwrap(), env.content_hash().unwrap());
    }

    #[test]
    fn a_tampered_payload_fails_verification() {
        let mut env = Envelope::seal(&identity(3), ping("original")).unwrap();
        env.payload.note = "tampered".into();
        assert!(matches!(env.open().unwrap_err(), Error::BadSignature));
    }

    #[test]
    fn a_swapped_signer_fails_verification() {
        let mut env = Envelope::seal(&identity(4), ping("mine")).unwrap();
        env.signer = identity(5).did().clone();
        assert!(matches!(env.open().unwrap_err(), Error::BadSignature));
    }

    #[test]
    fn a_signature_cannot_be_replayed_across_payload_types() {
        // Identical bytes, different type id: the domain separation in the
        // signing input has to make the signature useless here.
        let id = identity(6);
        let signed_ping = Envelope::seal(&id, ping("same")).unwrap();
        let forged = Envelope::<Pong> {
            v: signed_ping.v,
            type_id: Pong::TYPE.to_string(),
            signer: signed_ping.signer.clone(),
            payload: Pong {
                note: "same".into(),
            },
            sig: signed_ping.sig.clone(),
        };
        assert!(matches!(forged.open().unwrap_err(), Error::BadSignature));
    }

    #[test]
    fn opening_as_the_wrong_type_is_reported_as_a_mismatch() {
        let env = Envelope::seal(&identity(7), ping("typed")).unwrap();
        let json = env.to_canonical_json().unwrap();
        // Deserializing a Ping body into an Envelope<Pong> works
        // structurally; the type id is what catches it.
        let as_pong: Envelope<Pong> = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            as_pong.open().unwrap_err(),
            Error::TypeMismatch {
                expected: "c0mpute.test.pong/v1",
                ..
            }
        ));
    }

    #[test]
    fn an_unknown_envelope_version_is_rejected() {
        let mut env = Envelope::seal(&identity(8), ping("v")).unwrap();
        env.v = 99;
        assert!(matches!(env.open().unwrap_err(), Error::Format(_)));
    }

    #[test]
    fn an_invalid_payload_cannot_be_sealed() {
        let err = Envelope::seal(&identity(9), ping("")).unwrap_err();
        assert!(matches!(err, Error::Format(_)));
    }

    #[test]
    fn expiry_is_enforced_only_by_open_fresh() {
        let id = identity(10);
        let expires = Timestamp::parse("2026-09-06T16:00:00.000Z").unwrap();
        let env = Envelope::seal(
            &id,
            Ping {
                note: "short-lived".into(),
                expires_at: Some(expires.clone()),
            },
        )
        .unwrap();

        let before = Timestamp::parse("2026-09-06T15:59:59.999Z").unwrap();
        let after = Timestamp::parse("2026-09-06T16:00:00.001Z").unwrap();

        env.open_fresh(&before).unwrap();
        assert!(matches!(
            env.open_fresh(&after).unwrap_err(),
            Error::Expired { .. }
        ));
        // `open` still verifies a stale-but-authentic message: an expired
        // advert is history, not a forgery.
        env.open().unwrap();
    }

    #[test]
    fn content_hash_is_stable_across_key_ordering() {
        let env = Envelope::seal(&identity(11), ping("stable")).unwrap();
        let hash = env.content_hash().unwrap();

        // Re-serialize with deliberately shuffled keys, reparse, rehash.
        let value: serde_json::Value =
            serde_json::from_str(&env.to_canonical_json().unwrap()).unwrap();
        let shuffled = serde_json::to_string(&value).unwrap();
        let reparsed: Envelope<Ping> = Envelope::from_json(&shuffled).unwrap();
        assert_eq!(reparsed.content_hash().unwrap(), hash);
    }

    #[test]
    fn signatures_are_deterministic_for_the_same_input() {
        // ed25519 is deterministic; two seals of the same payload by the
        // same key must agree, or the content hash is not a stable id.
        let a = Envelope::seal(&identity(12), ping("det")).unwrap();
        let b = Envelope::seal(&identity(12), ping("det")).unwrap();
        assert_eq!(a.content_hash().unwrap(), b.content_hash().unwrap());
    }
}
