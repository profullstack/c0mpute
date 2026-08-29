//! Volumes: durable metadata for c0mpute storage (CIP-004).
//!
//! CIP-002 and CIP-003 made object bytes durable. Their *manifests* were not:
//! a manifest sat as JSON on whichever node ran the write, and losing that
//! node orphaned every shard it described. DIP-0012 flagged this and left it
//! open. This crate closes it.
//!
//! The shape:
//!
//! ```text
//!   Root pointer   volume -> snapshot hash        mutable, signed, 32 bytes
//!        │
//!   Snapshot       name -> object hash (HAMT)     immutable, content-addressed
//!        │
//!   Manifests      object -> blocks -> shards     immutable, content-addressed
//!        │
//!   Shards         the bytes                      RS-coded across n peers
//! ```
//!
//! Everything below the root is immutable and content-addressed, so it
//! inherits CIP-003's placement durability for free. **Only the root is
//! mutable, and it is one signed pointer.** Concentrating all mutability into
//! a single tiny value is the trick the rest of the program depends on — it is
//! what lets CIP-007 build a read/write filesystem over an immutable store
//! without making the store mutable, and it is what makes `rename` atomic for
//! free.
//!
//! Three pieces:
//!
//!   - [`hamt`] — the persistent map. Structural sharing is what makes a
//!     mutable root affordable: one changed entry rewrites ~log₃₂(N) nodes,
//!     not the tree.
//!   - [`volume`] — root pointers, signing, sequence, history, rollback and
//!     the GC keep-set.
//!   - [`anchor`] — where the root lives. CoinPay's registry arrives with
//!     CIP-006; the trait carries the compare-and-set that matters.

pub mod anchor;
pub mod gc;
pub mod hamt;
pub mod store;
pub mod volume;

pub use anchor::{FileAnchor, RootAnchor};
pub use gc::{GcPlan, GcStats, sweep};
pub use hamt::Hamt;
pub use store::{LocalSink, ObjectSink};
pub use volume::{DEFAULT_RETAINED_ROOTS, RootPointer, Volume};
