//! E2E encrypted p2p messaging for the c0mpute network (DIP-0018).
//!
//! * [`keys`]: hybrid classical + post-quantum keys from one seed, with a
//!   password-encrypted keyfile.
//! * [`message`]: the signed, hybrid-encrypted DM envelope.
//! * [`identity`]: addresses (= public keys), fingerprints, and the signed
//!   key and home records nodes gossip instead of a directory.
//! * [`sealed`]: the outer layer that hides sender and recipient from the
//!   network; recipient tags; replay protection.
//! * [`mailbox`]: the bounded store-and-forward store every mailbox node runs.
//! * [`placement`]: rendezvous hashing that picks mailbox nodes per tag.
//! * [`state`]: the client's contacts, blocklist and inbox, encrypted at rest.
//!
//! The network service (gossip + libp2p request/response) lives in
//! `c0mpute_core::dm`.

pub mod identity;
pub mod keys;
pub mod mailbox;
pub mod message;
pub mod placement;
pub mod sealed;
pub mod state;

pub use identity::{HomeRecord, KeyRecord, PublicIdentity};
pub use keys::{ChatKey, KeyFile, decrypt_keyfile, key_file_path, load_keyfile, save_keyfile};
pub use message::{DmEnvelope, DmPlaintext, decrypt_dm, encrypt_dm};
pub use sealed::{Sealed, SealedMessage};
pub use state::LocalState;

/// Seconds since the Unix epoch.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
