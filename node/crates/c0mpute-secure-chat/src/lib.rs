//! E2E encrypted p2p messaging for the c0mpute network (DIP-0018).
//!
//! v0.1 scope: key generation, password-encrypted backups, and DM
//! encrypt/decrypt. Transport (DHT key distribution, gossip relay,
//! store-and-forward) lands in v0.2.

pub mod keys;
pub mod message;

pub use keys::{ChatKey, KeyFile, decrypt_keyfile, key_file_path, load_keyfile, save_keyfile};
pub use message::{DmEnvelope, DmPlaintext, decrypt_dm, encrypt_dm};
