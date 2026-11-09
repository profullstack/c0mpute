//! libp2p networking layer for c0mpute.
//!
//! Phase 1 surface (this crate today):
//!
//!   - `Network` trait — abstract `announce(hash)` / `fetch(req)`.
//!   - `Loopback` — in-memory single-node impl, kept for unit tests.
//!   - `swarm` module — real libp2p swarm (Kad-DHT + request/response).
//!   - `Libp2pNetwork` — `Network` trait impl backed by the swarm.
//!
//! See c0mpute v1 PRD §"Network Protocol" + DIP-0010 (bootstrap seed
//! nodes) + DIP-0011 (no central backend).
//!
//! Protocol IDs:
//!   /c0mpute/kad/1.0.0          — DHT (peer + content discovery)
//!   /c0mpute/chunk-fetch/1.0.0  — request/response for chunk bytes
//!   /c0mpute/rpc/1.0.0          — small named-service request/response

pub mod bootstrap;
pub mod identity;
pub mod swarm;
pub mod topics;

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use c0mpute_proto::{ChunkRequest, Hash};

pub use libp2p::{Multiaddr, PeerId, multiaddr::Protocol};
/// The libp2p this crate is built on, for identity and key types.
pub use libp2p;
pub use swarm::{GossipMessage, Libp2pNetwork, NetworkConfig, RpcHandler, instance_started_ms, new_instance_id};

/// Trait that abstracts the underlying network so the rest of the node can be
/// developed and tested without booting libp2p.
#[async_trait]
pub trait Network: Send + Sync + 'static {
    /// Announce that this node holds the given chunk.
    async fn announce(&self, hash: &Hash) -> Result<()>;

    /// Fetch a chunk by hash from any peer that has announced it. Returns the
    /// raw bytes; the caller is responsible for re-hashing to verify.
    async fn fetch(&self, req: &ChunkRequest) -> Result<Vec<u8>>;
}

/// In-memory loopback "network" — handy for tests. A handle to a single node
/// pretends to be the whole p2p mesh.
pub struct Loopback {
    store: Arc<dyn ChunkSource>,
}

impl Loopback {
    pub fn new(store: Arc<dyn ChunkSource>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Network for Loopback {
    async fn announce(&self, _hash: &Hash) -> Result<()> {
        Ok(())
    }

    async fn fetch(&self, req: &ChunkRequest) -> Result<Vec<u8>> {
        self.store.read_chunk(&req.chunk_hash).await
    }
}

#[async_trait]
pub trait ChunkSource: Send + Sync + 'static {
    async fn read_chunk(&self, hash: &Hash) -> Result<Vec<u8>>;
}
