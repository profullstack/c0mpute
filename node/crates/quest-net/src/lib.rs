//! libp2p networking layer for Quest.
//!
//! Scope: peer discovery (Kad-DHT under `/quest/kad/1.0.0`), capability
//! announcement, chunk request/response transport, parallel-fetch racing.
//! See PRD §14.
//!
//! Status: scaffold. The real libp2p stack lands in M0/M1; for now this crate
//! exposes the trait surface the rest of the node will program against, plus
//! an in-process `Loopback` impl used in unit tests of higher layers.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use quest_proto::{ChunkRequest, Hash};

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
