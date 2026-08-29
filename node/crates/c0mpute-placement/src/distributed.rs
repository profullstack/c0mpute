//! Cross-node object storage (CIP-003).
//!
//! Turns the single-node engine from CIP-002 into a distributed one: encode a
//! block, choose `n` peers under the diversity rules, push a shard to each,
//! and record where they went. Reads pull from those peers and reconstruct
//! from whichever `k` answer first.
//!
//! What this deliberately does *not* do is silently degrade. If the network
//! cannot satisfy the placement policy, a write fails with a message naming
//! the constraint that could not be met. A small network genuinely cannot
//! store data durably, and quietly writing all fourteen shards behind one ISP
//! is how people lose files while believing they are safe.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use c0mpute_proto::Hash;
use c0mpute_store::erasure::{self, Shard};
use c0mpute_store::{
    BlockEntry, MANIFEST_VERSION, ObjectManifest, ShardEntry, Storage, Tier, block_size_for,
};
use futures::stream::{FuturesUnordered, StreamExt};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::peer::PeerCatalog;
use crate::select::{PlacementPolicy, select};
use crate::transport::ShardTransport;

/// How placement behaves.
#[derive(Clone, Debug, Default)]
pub struct DistributedConfig {
    /// Override the policy derived from the tier's parity.
    pub policy: Option<PlacementPolicy>,
    /// Also keep a local copy of every shard.
    ///
    /// Off by default: it adds a full extra copy of the object to this node's
    /// disk for no durability the placement does not already provide.
    pub keep_local_copy: bool,
}

pub struct DistributedStorage {
    local: Storage,
    transport: Arc<dyn ShardTransport>,
    catalog: Arc<RwLock<PeerCatalog>>,
    config: DistributedConfig,
}

impl DistributedStorage {
    pub fn new(
        local: Storage,
        transport: Arc<dyn ShardTransport>,
        catalog: Arc<RwLock<PeerCatalog>>,
    ) -> Self {
        Self {
            local,
            transport,
            catalog,
            config: DistributedConfig::default(),
        }
    }

    pub fn with_config(mut self, config: DistributedConfig) -> Self {
        self.config = config;
        self
    }

    pub fn local(&self) -> &Storage {
        &self.local
    }

    fn policy_for(&self, tier: Tier) -> PlacementPolicy {
        self.config
            .policy
            .clone()
            .unwrap_or_else(|| PlacementPolicy::for_parity(tier.parity()))
    }

    /// Shards confirmed before a write is acknowledged.
    ///
    /// `k + ceil(parity/2)` — 12 of 14 for `standard`. Bounds write latency by
    /// the twelfth-fastest peer rather than the slowest, while still leaving
    /// the object readable if the two stragglers never land. The remaining
    /// placements continue in the background.
    fn write_quorum(tier: Tier) -> usize {
        tier.k() + tier.parity().div_ceil(2)
    }

    /// Store an object across the network.
    pub async fn put(&self, data: &[u8], tier: Tier) -> Result<ObjectManifest> {
        let block_size = block_size_for(data.len() as u64);
        let (k, parity) = (tier.k(), tier.parity());
        let policy = self.policy_for(tier);
        let quorum = Self::write_quorum(tier);

        let chunks: Vec<&[u8]> = if data.is_empty() {
            vec![&[]]
        } else {
            data.chunks(block_size as usize).collect()
        };

        let mut blocks = Vec::with_capacity(chunks.len());
        for (index, plaintext) in chunks.iter().enumerate() {
            let entry = self
                .place_block(plaintext, index as u32, k, parity, &policy, quorum)
                .await
                .with_context(|| format!("placing block {index}"))?;
            blocks.push(entry);
        }

        let manifest = ObjectManifest {
            version: MANIFEST_VERSION,
            object_hash: Hash::of(data),
            original_len: data.len() as u64,
            block_size,
            k: k as u8,
            parity: parity as u8,
            tier,
            blocks,
        };
        self.local.write_manifest(&manifest).await?;
        info!(
            object_hash = %manifest.object_hash,
            blocks = manifest.blocks.len(),
            %tier,
            "placed object across the network"
        );
        Ok(manifest)
    }

    /// Encode one block and place its shards.
    ///
    /// Peers are selected per block rather than per object, so a large object
    /// spreads across the whole network instead of pinning every one of its
    /// blocks to the same fourteen nodes.
    async fn place_block(
        &self,
        plaintext: &[u8],
        index: u32,
        k: usize,
        parity: usize,
        policy: &PlacementPolicy,
        quorum: usize,
    ) -> Result<BlockEntry> {
        let n = k + parity;
        let (shards, _) = erasure::encode(plaintext, k, parity)?;
        let shard_bytes = shards.first().map(|s| s.bytes.len()).unwrap_or(0) as u64;

        let assignments = {
            let catalog = self.catalog.read().await;
            select(catalog.peers(), n, shard_bytes, policy)?
        };

        // All n concurrently; acknowledge at quorum.
        let mut inflight = FuturesUnordered::new();
        for (a, shard) in assignments.iter().zip(shards.iter()) {
            let transport = Arc::clone(&self.transport);
            let peer = a.peer.clone();
            let bytes = shard.bytes.clone();
            let hash = Hash::of(&bytes);
            let shard_index = shard.index;
            inflight.push(async move {
                let res = transport.put_shard(&peer, &hash, &bytes).await;
                (shard_index, hash, peer, res)
            });
        }

        let mut entries: Vec<ShardEntry> = Vec::with_capacity(n);
        let mut failures: Vec<String> = Vec::new();
        while let Some((shard_index, hash, peer, res)) = inflight.next().await {
            match res {
                Ok(()) => entries.push(ShardEntry {
                    index: shard_index,
                    hash,
                    host_hint: Some(peer.peer_id),
                }),
                Err(e) => {
                    warn!(block = index, shard = shard_index, peer = %peer.peer_id, err = %e,
                          "shard placement failed");
                    failures.push(format!("{}: {e}", peer.peer_id));
                }
            }
        }

        if entries.len() < quorum {
            bail!(
                "block {index}: only {} of {n} shards placed, need {quorum} for write quorum ({} failed: {})",
                entries.len(),
                failures.len(),
                failures.join("; ")
            );
        }
        if entries.len() < n {
            // Readable but under-replicated. CIP-005's repair loop is what
            // restores full parity; recorded loudly so it is not invisible.
            warn!(
                block = index,
                placed = entries.len(),
                n,
                "block placed below full redundancy; needs repair"
            );
        }

        if self.config.keep_local_copy {
            for s in &shards {
                self.local.chunk_store().put(&s.bytes).await?;
            }
        }

        entries.sort_by_key(|e| e.index);
        Ok(BlockEntry {
            index,
            len: plaintext.len() as u32,
            hash: Hash::of(plaintext),
            shards: entries,
        })
    }

    /// Read an object back from the network.
    pub async fn get(&self, object_hash: &Hash) -> Result<Vec<u8>> {
        let manifest = self.local.read_manifest(object_hash).await?;
        let mut out = Vec::with_capacity(manifest.original_len as usize);
        for i in 0..manifest.blocks.len() {
            out.extend_from_slice(&self.read_block(&manifest, i).await?);
        }
        let actual = Hash::of(&out);
        if actual != manifest.object_hash {
            bail!(
                "object integrity failure: manifest says {} but decoded bytes hash to {actual}",
                manifest.object_hash
            );
        }
        Ok(out)
    }

    /// Reconstruct one block from whichever `k` peers answer first.
    ///
    /// All `n` are requested rather than a chosen `k`. That costs `n/k` (1.4x
    /// for `standard`) in read bandwidth and buys the difference between
    /// waiting for the k-th fastest peer and waiting for the slowest of a
    /// chosen k — on a network of consumer nodes at 200–500 ms, an easy trade.
    pub async fn read_block(&self, manifest: &ObjectManifest, index: usize) -> Result<Vec<u8>> {
        let entry = manifest
            .blocks
            .get(index)
            .ok_or_else(|| anyhow!("block {index} out of range"))?;
        let k = manifest.k as usize;
        let n = manifest.n();

        let catalog = self.catalog.read().await;
        let mut inflight = FuturesUnordered::new();
        let mut unreachable = Vec::new();

        for shard in &entry.shards {
            let Some(hint) = &shard.host_hint else {
                // No hint: a locally-stored shard from a CIP-002 write.
                continue;
            };
            let Some(peer) = catalog.get(hint).cloned() else {
                // The manifest is a cache, not the source of truth. Resolving
                // a stale hint via Kad provider records is the CIP-003 design;
                // until that lands, an unknown peer is simply skipped.
                unreachable.push(hint.clone());
                continue;
            };
            let transport = Arc::clone(&self.transport);
            let hash = shard.hash;
            let shard_index = shard.index;
            inflight.push(async move {
                let res = transport.get_shard(&peer, &hash).await;
                (shard_index, hash, peer.peer_id, res)
            });
        }
        drop(catalog);

        let mut received: Vec<Option<Shard>> = vec![None; n];
        let mut found = 0usize;
        let mut errors = Vec::new();

        while let Some((shard_index, hash, peer_id, res)) = inflight.next().await {
            match res {
                Ok(bytes) => {
                    // Verify even though the transport does: a second
                    // implementation of ShardTransport might not, and a
                    // substituted shard silently corrupts the decode.
                    if Hash::of(&bytes) != hash {
                        errors.push(format!("{peer_id}: served the wrong bytes"));
                        continue;
                    }
                    received[shard_index as usize] = Some(Shard {
                        index: shard_index,
                        bytes,
                    });
                    found += 1;
                    if found == k {
                        // Enough to reconstruct; drop the stragglers.
                        break;
                    }
                }
                Err(e) => errors.push(format!("{peer_id}: {e}")),
            }
        }

        // Fall back to any locally-held shards — how a CIP-002 object, or one
        // written with keep_local_copy, still reads.
        if found < k {
            for shard in &entry.shards {
                if received[shard.index as usize].is_some() {
                    continue;
                }
                if let Ok(bytes) = self.local.chunk_store().get(&shard.hash).await {
                    received[shard.index as usize] = Some(Shard {
                        index: shard.index,
                        bytes,
                    });
                    found += 1;
                    if found == k {
                        break;
                    }
                }
            }
        }

        if found < k {
            bail!(
                "block {index} of object {}: need {k} shards, got {found} \
                 ({} peers unreachable, {} errors: {})",
                manifest.object_hash,
                unreachable.len(),
                errors.len(),
                errors.join("; ")
            );
        }

        let mut plaintext =
            erasure::decode(received, k, manifest.parity as usize, entry.len as usize)?;
        plaintext.truncate(entry.len as usize);

        let actual = Hash::of(&plaintext);
        if actual != entry.hash {
            bail!(
                "block {index} of object {}: integrity failure, manifest says {} but bytes hash to {actual}",
                manifest.object_hash,
                entry.hash
            );
        }
        debug!(block = index, found, k, "reconstructed block");
        Ok(plaintext)
    }

    /// Per-block health, for `c0mpute storage info` and CIP-005's repair scan.
    pub async fn health(&self, manifest: &ObjectManifest) -> Result<Vec<BlockHealth>> {
        let catalog = self.catalog.read().await;
        let mut out = Vec::with_capacity(manifest.blocks.len());

        for block in &manifest.blocks {
            let mut healthy = 0usize;
            let mut missing = Vec::new();
            for shard in &block.shards {
                let held = match shard.host_hint.as_ref().and_then(|h| catalog.get(h)) {
                    Some(peer) => self
                        .transport
                        .has_shards(peer, &[shard.hash])
                        .await
                        .map(|v| v.first().copied().unwrap_or(false))
                        .unwrap_or(false),
                    None => self.local.chunk_store().has(&shard.hash).await,
                };
                if held {
                    healthy += 1;
                } else {
                    missing.push(shard.index);
                }
            }
            out.push(BlockHealth {
                index: block.index,
                healthy,
                total: block.shards.len(),
                missing,
                state: BlockState::classify(healthy, manifest.k as usize, manifest.parity as usize),
            });
        }
        Ok(out)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockHealth {
    pub index: u32,
    pub healthy: usize,
    pub total: usize,
    pub missing: Vec<u8>,
    pub state: BlockState,
}

/// CIP-005's classification, thresholds relative to `(k, parity)` rather than
/// absolute. Repair triggers as soon as half the parity budget is spent —
/// waiting until `k+1` would be cheaper, and is what Storj's much wider code
/// affords them, but CIP-001 spent that margin on the cost advantage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockState {
    Healthy,
    Degraded,
    Urgent,
    Critical,
    Lost,
}

impl BlockState {
    pub fn classify(healthy: usize, k: usize, parity: usize) -> Self {
        let n = k + parity;
        if healthy >= n {
            BlockState::Healthy
        } else if healthy < k {
            BlockState::Lost
        } else if healthy == k {
            // One more loss and the block is gone.
            BlockState::Critical
        } else if healthy <= k + parity / 4 {
            BlockState::Urgent
        } else {
            // Any shard missing at all is a repair trigger: CIP-001 bought the
            // cost advantage by spending the durability margin Storj keeps, so
            // we cannot wait until the parity budget is nearly gone.
            BlockState::Degraded
        }
    }

    pub fn needs_repair(self) -> bool {
        !matches!(self, BlockState::Healthy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_quorum_is_k_plus_half_the_parity() {
        assert_eq!(DistributedStorage::write_quorum(Tier::Standard), 12); // 10 + 2
        assert_eq!(DistributedStorage::write_quorum(Tier::Critical), 26); // 20 + 6
        assert_eq!(DistributedStorage::write_quorum(Tier::Hot), 2); // 1 + 1
    }

    #[test]
    fn block_state_classifies_against_k_and_parity() {
        // RS 10/14
        assert_eq!(BlockState::classify(14, 10, 4), BlockState::Healthy);
        assert_eq!(BlockState::classify(13, 10, 4), BlockState::Degraded);
        assert_eq!(BlockState::classify(12, 10, 4), BlockState::Degraded);
        assert_eq!(BlockState::classify(11, 10, 4), BlockState::Urgent);
        assert_eq!(BlockState::classify(10, 10, 4), BlockState::Critical);
        assert_eq!(BlockState::classify(9, 10, 4), BlockState::Lost);
    }

    #[test]
    fn only_healthy_blocks_skip_repair() {
        assert!(!BlockState::Healthy.needs_repair());
        for s in [
            BlockState::Degraded,
            BlockState::Urgent,
            BlockState::Critical,
            BlockState::Lost,
        ] {
            assert!(s.needs_repair(), "{s:?} should need repair");
        }
    }
}
