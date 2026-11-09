//! Sealed objects: the DIP-0012 `private` storage tier.
//!
//! A sealed object is a byte blob that is encrypted on the client before it
//! goes anywhere near the network. Nodes store it, replicate it, serve it and
//! answer storage challenges over it exactly as they would any other bytes,
//! because to them it *is* just bytes: content addressing, replication and
//! proof-of-storage all run over the ciphertext.
//!
//! What a node holding a sealed object can learn:
//!
//! * that it is a sealed object (the header has a fixed magic and shape);
//! * its padded size, rounded up to a Padmé bucket (at most ~12% over the
//!   real size, which leaves only a few bits of the true length);
//! * when it is written and read.
//!
//! What it cannot learn: the content, the file name, MIME type, exact size,
//! who owns it or who it is shared with. The owner's identity never appears
//! in the blob; key-wrap slots carry no recipient id (recipients find their
//! slot by trial decryption); and the network-facing record is signed by a
//! per-object key that is derived from the owner's secret but unlinkable to
//! it.
//!
//! Layout (see [`format`] for the exact construction):
//!
//! ```text
//! header (1881 bytes, fixed)
//!   magic      8   "C0PRIV\0\1"
//!   stream_id 16   random per encryption
//!   commit    32   HKDF(DEK, stream_id, "commit"): pins exactly one DEK
//!   slots      4   u16 count (always 16) + u16 reserved
//!   slot x16  80   X25519 ephemeral pub (32) + AES-256-GCM wrapped DEK (48),
//!                  unused slots are random bytes
//!   roster   541   who it is wrapped to, AES-256-GCM under an owner-only key
//! stream (AES-256-GCM, 64 KiB plaintext chunks, STREAM construction)
//!   u32 meta_len | meta JSON | ed25519 sig (64) | content | zero padding
//! ```
//!
//! Keys belong to users only. A random data key (DEK) is drawn for every
//! encryption and never reused for different plaintext; it is wrapped to
//! each recipient's X25519 storage key, which is derived from the user's
//! c0mpute identity seed ([`keys`]). Nothing in this crate sends anything
//! anywhere; it never logs key material, and every secret type zeroizes on
//! drop.

pub mod format;
pub mod keys;
pub mod pad;

pub use format::{
    CHUNK_PLAINTEXT, DataKey, HEADER_LEN, Kind, MAX_RECIPIENTS, Meta, Opened, PartRef, SealParams,
    Sealed, SiteFile, now_version, open, open_with_secret, random_name, rewrap, roster, seal, seal_with_key,
    site_id_for, unwrap_dek,
};
pub use keys::{StoragePublic, StorageSecret};

/// Errors a reader can hit. Deliberately coarse: an attacker probing a
/// decryptor should not learn *which* check failed beyond what is needed to
/// debug honest mistakes.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SealError {
    #[error("not a sealed object (bad magic or too short)")]
    NotSealed,
    #[error("no key slot opens with this key (not shared with you, or wrong key)")]
    NoSlot,
    #[error("data key does not match this object's commitment (wrong key)")]
    WrongKey,
    #[error("ciphertext failed authentication (tampered, reordered or truncated)")]
    Tampered,
    #[error("object metadata is malformed: {0}")]
    BadMeta(String),
    #[error("object signature does not verify (forged or substituted object)")]
    BadSignature,
    #[error("object is not the one requested (expected site {expected}, got {got})")]
    WrongObject { expected: String, got: String },
    #[error("too many recipients: at most {0}")]
    TooManyRecipients(usize),
    #[error("object too large")]
    TooLarge,
}
