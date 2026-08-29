//! Where metadata objects live (CIP-004).
//!
//! The volume layer needs to put and get small content-addressed blobs —
//! snapshot nodes, manifests, root history. It does not care whether those
//! land on the local disk or across the network, so it talks to this trait
//! rather than to a concrete store. That is what lets a volume run on a single
//! node today and on `DistributedStorage` at the `hot` tier without the HAMT
//! knowing the difference.

use anyhow::Result;
use async_trait::async_trait;
use c0mpute_proto::Hash;

#[async_trait]
pub trait ObjectSink: Send + Sync {
    /// Store bytes, returning their content hash.
    async fn put_object(&self, bytes: &[u8]) -> Result<Hash>;
    /// Fetch bytes by hash. Implementations verify before returning.
    async fn get_object(&self, hash: &Hash) -> Result<Vec<u8>>;
    async fn has_object(&self, hash: &Hash) -> bool;
}

/// Backed by a node's local `ChunkStore`.
///
/// Metadata nodes are small and hot, so they go in the chunk store directly
/// rather than through erasure coding — a 200-byte HAMT node split into 14
/// shards would be pure overhead (CIP-001's argument for the replicated `hot`
/// tier, taken to its conclusion).
pub struct LocalSink {
    store: c0mpute_store::ChunkStore,
}

impl LocalSink {
    pub fn new(store: c0mpute_store::ChunkStore) -> Self {
        Self { store }
    }

    pub fn chunk_store(&self) -> &c0mpute_store::ChunkStore {
        &self.store
    }
}

#[async_trait]
impl ObjectSink for LocalSink {
    async fn put_object(&self, bytes: &[u8]) -> Result<Hash> {
        self.store.put(bytes).await
    }

    async fn get_object(&self, hash: &Hash) -> Result<Vec<u8>> {
        self.store.get(hash).await
    }

    async fn has_object(&self, hash: &Hash) -> bool {
        self.store.has(hash).await
    }
}

#[cfg(any(test, feature = "testing"))]
pub use memory::MemorySink;

#[cfg(any(test, feature = "testing"))]
mod memory {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use super::*;

    /// In-memory sink that counts writes, so tests can assert on how much a
    /// change actually cost.
    #[derive(Clone, Default)]
    pub struct MemorySink {
        objects: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    }

    impl MemorySink {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn object_count(&self) -> usize {
            self.objects.lock().unwrap().len()
        }

        pub fn has_object(&self, hash: &Hash) -> bool {
            self.objects.lock().unwrap().contains_key(&hash.to_hex())
        }

        pub fn remove(&self, hash: &Hash) {
            self.objects.lock().unwrap().remove(&hash.to_hex());
        }

        pub fn hashes(&self) -> Vec<Hash> {
            self.objects
                .lock()
                .unwrap()
                .keys()
                .filter_map(|k| Hash::from_hex(k).ok())
                .collect()
        }
    }

    #[async_trait]
    impl ObjectSink for MemorySink {
        async fn put_object(&self, bytes: &[u8]) -> Result<Hash> {
            let hash = Hash::of(bytes);
            self.objects
                .lock()
                .unwrap()
                .insert(hash.to_hex(), bytes.to_vec());
            Ok(hash)
        }

        async fn get_object(&self, hash: &Hash) -> Result<Vec<u8>> {
            self.objects
                .lock()
                .unwrap()
                .get(&hash.to_hex())
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no object {hash}"))
        }

        async fn has_object(&self, hash: &Hash) -> bool {
            MemorySink::has_object(self, hash)
        }
    }
}
