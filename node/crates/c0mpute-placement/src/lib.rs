//! Cross-node shard placement for c0mpute storage (CIP-003).
//!
//! CIP-002 made the storage engine reachable over HTTP, but every shard still
//! landed on one disk — which means the erasure coding was pure overhead with
//! no durability behind it. This crate spreads a block's `n` shards across `n`
//! peers chosen for reputation and failure-domain diversity, and reads them
//! back from whichever `k` answer first.
//!
//! Three pieces:
//!
//!   - [`peer`] — who the storage peers are, and which failure domain each
//!     belongs to.
//!   - [`select`] — choosing `n` of them under CIP-001's durability rules.
//!     Pure; no network I/O, because a slow peer lookup must never become a
//!     slow write.
//!   - [`transport`] — moving shard bytes. HTTP against the CIP-002 endpoints
//!     today; the libp2p `/c0mpute/shard/1.0.0` protocol becomes a second
//!     implementation of the same trait.
//!
//! and [`distributed::DistributedStorage`], which composes them.
//!
//! The load-bearing decision is that placement **fails loudly** when the
//! network cannot satisfy the diversity policy. CIP-001's durability figures
//! assume shard hosts fail independently; fourteen shards behind one ISP are
//! one sample, not fourteen, and nothing downstream can detect that the
//! assumption was broken. A write that cannot be made durable is an error,
//! not a warning.

pub mod distributed;
pub mod peer;
pub mod repair;
pub mod select;
pub mod transport;

pub use distributed::{BlockHealth, BlockState, DistributedConfig, DistributedStorage};
pub use peer::{FailureDomain, PeerCatalog, PeerInfo};
pub use repair::{
    FailureTracker, RepairAttestation, RepairConfig, RepairPlan, RepairReport, Repairer,
    elect_repairer,
};
pub use select::{
    Assignment, PlacementContext, PlacementError, PlacementPolicy, score, select, select_peers,
};
pub use transport::{HttpTransport, ShardTransport};
