//! Seeding: a c0mpute node seeds the torrents c0mpute publishes.
//!
//! The rule this crate exists to hold: a node seeds only what is on a list we
//! control and the node verified, plus what its own operator explicitly listed
//! in local config. Nothing that arrives over the network (a job, a peer, a
//! gateway request, a capability ad) can add a torrent. There is no DHT, no
//! crawling, no index and no add API.
//!
//! - [`manifest`]: the signed list (minisign, checked against the release key
//!   pinned in the binary), with replay and expiry checks.
//! - [`plan`]: which of the allowed torrents fit the node's disk cap.
//! - [`engine`]: the BitTorrent engine that seeds exactly that plan.
//!
//! Operator docs: docs/seeding.md.

pub mod engine;
pub mod manifest;
pub mod plan;

pub use engine::{Engine, EngineOptions, TorrentFileInfo, TorrentStatus, dir_bytes, inspect_torrent};
pub use manifest::{
    DEFAULT_MANIFEST_URL, Entry, MANIFEST_FILE, MANIFEST_KIND, Manifest, ManifestStore,
    PINNED_PUBKEY,
};
pub use plan::{Candidate, Narrow, Origin, Plan, Skipped, candidates, effective_cap, plan};
