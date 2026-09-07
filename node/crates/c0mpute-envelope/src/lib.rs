//! Signed protocol envelopes for the c0mpute network.
//!
//! This crate is the spine of c0mpute v2: canonical serialization, native
//! network identity, and the four payload types the open market runs on.
//!
//! ```text
//! provider.advert/v1   a provider says what it can run, and until when
//! job/v2               a buyer describes work, constraints, price, trust
//! offer/v1             a provider quotes a price for one job
//! receipt/v1           both sides sign what actually happened
//! ```
//!
//! Everything a node needs in order to believe one of those four things
//! is inside the message. There is no lookup against a registry, no shared
//! database, and no hosted API in the verification path — which is the
//! property that lets the network keep working when c0mpute.com does not.
//!
//! ## Deliberate non-dependencies
//!
//! This crate does not depend on libp2p, on any HTTP client, or on any
//! settlement implementation. Transport, discovery, and payment are all
//! bound at the edges:
//!
//! - identity is raw ed25519 bytes ([`SigningIdentity::from_seed`]), which
//!   the transport layer supplies from the same key libp2p persists;
//! - settlement is named by an open [`SettlementAdapter`] string rather
//!   than a closed enum, so CoinPay is the default rather than a
//!   requirement.
//!
//! Keeping those out is what makes "core protocol crates must not depend
//! on hosted-service SDKs" a rule a build can enforce rather than a slogan.
//!
//! ## Example
//!
//! ```
//! use c0mpute_envelope::{Envelope, Offer, Money, SigningIdentity, Timestamp, ContentHash};
//!
//! let provider = SigningIdentity::from_seed(&[7u8; 32]);
//! let offer = Offer {
//!     job: ContentHash::of(b"a job manifest"),
//!     price: Money::new("0.042", "USD"),
//!     expected_duration_ms: 12_000,
//!     start_before: Timestamp::parse("2026-09-06T16:02:00.000Z").unwrap(),
//!     expires_at: Timestamp::parse("2026-09-06T16:00:10.000Z").unwrap(),
//!     capability_advert: None,
//!     coordinator: false,
//! };
//!
//! let sealed = Envelope::seal(&provider, offer).unwrap();
//! let json = sealed.to_canonical_json().unwrap();
//!
//! // A peer that was never on the wire can still verify it.
//! let received: Envelope<Offer> = Envelope::from_json(&json).unwrap();
//! let verified = received.open().unwrap();
//! assert_eq!(verified.price.amount, "0.042");
//! ```

pub mod advert;
pub mod canonical;
pub mod envelope;
pub mod identity;
pub mod job;
pub mod offer;
pub mod receipt;
pub mod types;

pub use advert::{Credential, GpuSpec, ProviderAdvert, ProviderCapabilities, ProviderTrust};
pub use canonical::{canonicalize, canonicalize_to_string};
pub use envelope::{DOMAIN, ENVELOPE_VERSION, Envelope, Payload};
pub use identity::{DID_PREFIX, Did, EnvelopeSignature, SigningIdentity};
pub use job::{Economics, Execution, Isolation, JobInput, JobManifest, JobOutput, Requirements};
pub use offer::Offer;
pub use receipt::{
    Receipt, ReceiptAcceptance, SettlementRecord, ValidationOutcome, ValidationStatus,
};
pub use types::{
    ContentHash, Money, SettlementAdapter, Timestamp, TrustTier, ValidationLevel, ValidationPolicy,
};

/// Everything that can go wrong reading or writing a protocol message.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A value cannot be canonically serialized, so it cannot be signed:
    /// a float, an out-of-range integer, or a non-ASCII object key.
    #[error("value is not canonically serializable: {0}")]
    NonCanonical(String),

    /// A DID or signature could not be parsed.
    #[error("identity error: {0}")]
    Identity(String),

    /// A payload is structurally invalid.
    #[error("malformed payload: {0}")]
    Format(String),

    /// The signature does not match the signer and the bytes.
    #[error("signature verification failed")]
    BadSignature,

    /// The envelope carries a different payload type than the caller
    /// expected.
    #[error("expected payload type {expected}, found {found}")]
    TypeMismatch {
        expected: &'static str,
        found: String,
    },

    /// The payload was authentic but is no longer valid.
    #[error("payload expired at {expired_at} (now {now})")]
    Expired { expired_at: String, now: String },

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}
