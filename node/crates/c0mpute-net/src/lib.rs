//! libp2p networking layer for c0mpute.
//!
//! Scope: peer discovery (Kad-DHT under `/c0mpute/kad/1.0.0`), capability
//! announcement (gossipsub), chunk request/response transport, parallel-
//! fetch racing. See c0mpute v1 PRD + DIP-0010 (bootstrap seed nodes).
//!
//! Status: SCAFFOLD. This crate is currently ~56 lines: a `Network` trait
//! + an in-memory `Loopback` impl for tests. The real libp2p stack hasn't
//! been wired up yet — that's the load-bearing piece blocking the network
//! actually existing. See DIP-0010 for the bootstrap design that lands
//! alongside the libp2p implementation.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use c0mpute_proto::{ChunkRequest, Hash};

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
