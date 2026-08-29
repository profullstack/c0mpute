//! Erasure-coded object storage on top of `ChunkStore` (CIP-002).
//!
//! An object is split into fixed-size **blocks**; each block is independently
//! Reed-Solomon encoded into `n` shards, and each shard is written to the
//! content-addressed chunk store under its own blake3 hash. A manifest records
//! the block and shard layout.
//!
//! Blocks are what make the rest of the storage program possible:
//!
//!   * memory is bounded by block size, not object size, so a 1 TiB object
//!     does not need 1 TiB of RAM to write or read;
//!   * `get_range` fetches only the blocks a byte range touches, which is what
//!     random-access file reads (CIP-007) are built on;
//!   * a single damaged block is repairable (CIP-005) without touching the
//!     rest of the object.
//!
//! Block size scales with object size so that manifests stay small — see
//! [`block_size_for`].

use std::path::PathBuf;
use std::pin::Pin;

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use c0mpute_proto::Hash;
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::fs;
use tracing::{debug, warn};

use crate::ChunkStore;
use crate::erasure::{self, Shard};
use crate::tier::Tier;

/// Current manifest format version.
pub const MANIFEST_VERSION: u8 = 2;

/// Smallest (and default) block size: 4 MiB.
pub const DEFAULT_BLOCK_SIZE: u32 = 4 * 1024 * 1024;

/// Largest block size: 256 MiB.
pub const MAX_BLOCK_SIZE: u32 = 256 * 1024 * 1024;

/// Manifests stay small by targeting at most this many blocks per object.
pub const TARGET_BLOCKS_PER_OBJECT: u64 = 4096;

/// Choose a block size for an object of `len` bytes.
///
/// Fixed 4 MiB blocks would give a 1 TiB object ~262k blocks and a manifest in
/// the tens of megabytes, which then needs its own durability story. Doubling
/// the block size until the object fits in [`TARGET_BLOCKS_PER_OBJECT`] keeps
/// every manifest under about a megabyte.
pub fn block_size_for(len: u64) -> u32 {
    let ideal = len.div_ceil(TARGET_BLOCKS_PER_OBJECT);
    let mut size = DEFAULT_BLOCK_SIZE as u64;
    while size < ideal && size < MAX_BLOCK_SIZE as u64 {
        size *= 2;
    }
    size.min(MAX_BLOCK_SIZE as u64) as u32
}

/// One shard of one block.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShardEntry {
    pub index: u8,
    pub hash: Hash,
    /// Peer holding this shard. `None` means local-only. Populated by
    /// cross-node placement (CIP-003).
    ///
    /// This is a *hint*, not the source of truth — repair (CIP-005) relocates
    /// shards without being able to sign the customer's manifest, so a stale
    /// hint is normal and readers fall back to DHT provider records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_hint: Option<String>,
}

/// One block of an object: its plaintext hash, plaintext length, and shards.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlockEntry {
    pub index: u32,
    /// Plaintext length of this block, before RS padding. The final block of
    /// an object is usually short.
    pub len: u32,
    /// blake3 of this block's plaintext. Lets a reader verify per block
    /// instead of only at the end of a whole object.
    pub hash: Hash,
    pub shards: Vec<ShardEntry>,
}

/// Maps an object hash onto the blocks and shards that reconstruct it.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ObjectManifest {
    pub version: u8,
    pub object_hash: Hash,
    pub original_len: u64,
    pub block_size: u32,
    pub k: u8,
    pub parity: u8,
    pub tier: Tier,
    pub blocks: Vec<BlockEntry>,
}

impl ObjectManifest {
    pub fn n(&self) -> usize {
        self.k as usize + self.parity as usize
    }

    /// Total shards recorded across every block.
    pub fn shard_count(&self) -> usize {
        self.blocks.iter().map(|b| b.shards.len()).sum()
    }

    /// Which block indices cover `[offset, offset + len)`.
    fn blocks_for_range(&self, offset: u64, len: u64) -> std::ops::Range<usize> {
        if len == 0 || offset >= self.original_len {
            return 0..0;
        }
        let end = (offset + len).min(self.original_len);
        let bs = self.block_size as u64;
        let first = (offset / bs) as usize;
        let last = ((end - 1) / bs) as usize;
        first..(last + 1).min(self.blocks.len())
    }
}

/// Wire form that accepts both v1 (flat `shards`, one implicit block) and v2
/// manifests. There is no production v1 data to migrate, but keeping the shim
/// means the original round-trip tests stay meaningful.
#[derive(Deserialize)]
struct RawManifest {
    #[serde(default = "v1_version")]
    version: u8,
    object_hash: Hash,
    original_len: u64,
    #[serde(default)]
    block_size: Option<u32>,
    k: u8,
    parity: u8,
    #[serde(default)]
    tier: Option<Tier>,
    #[serde(default)]
    blocks: Option<Vec<BlockEntry>>,
    /// v1 only: shards of the single implicit block.
    #[serde(default)]
    shards: Option<Vec<ShardEntry>>,
}

fn v1_version() -> u8 {
    1
}

impl<'de> Deserialize<'de> for ObjectManifest {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = RawManifest::deserialize(d)?;
        let tier = raw
            .tier
            .or_else(|| Tier::from_params(raw.k, raw.parity))
            .unwrap_or_default();

        let blocks = match (raw.blocks, raw.shards) {
            (Some(blocks), _) => blocks,
            // v1: one block covering the whole object. The block plaintext is
            // the object plaintext, so they share a hash.
            (None, Some(shards)) => vec![BlockEntry {
                index: 0,
                len: u32::try_from(raw.original_len).map_err(|_| {
                    serde::de::Error::custom("v1 manifest longer than one block can hold")
                })?,
                hash: raw.object_hash,
                shards,
            }],
            (None, None) => {
                return Err(serde::de::Error::custom(
                    "manifest has neither blocks nor shards",
                ));
            }
        };

        let block_size = raw.block_size.unwrap_or_else(|| {
            u32::try_from(raw.original_len.max(1)).unwrap_or(DEFAULT_BLOCK_SIZE)
        });

        Ok(ObjectManifest {
            version: raw.version,
            object_hash: raw.object_hash,
            original_len: raw.original_len,
            block_size,
            k: raw.k,
            parity: raw.parity,
            tier,
            blocks,
        })
    }
}

#[derive(Clone, Debug)]
pub struct Storage {
    inner: ChunkStore,
}

impl Storage {
    pub fn new(inner: ChunkStore) -> Self {
        Self { inner }
    }

    pub fn chunk_store(&self) -> &ChunkStore {
        &self.inner
    }

    fn manifest_path(&self, object_hash: &Hash) -> PathBuf {
        let hex = object_hash.to_hex();
        self.inner
            .root()
            .join("manifests")
            .join(&hex[0..2])
            .join(format!("{hex}.json"))
    }

    // ---------------------------------------------------------------- writes

    /// Store an in-memory object at the default tier.
    ///
    /// Thin wrapper over [`Storage::put_stream`] so there is one write path,
    /// not two that can drift.
    pub async fn put(&self, data: &[u8]) -> Result<ObjectManifest> {
        self.put_tiered(data, Tier::default()).await
    }

    /// Store an in-memory object at a given tier.
    pub async fn put_tiered(&self, data: &[u8], tier: Tier) -> Result<ObjectManifest> {
        let len = data.len() as u64;
        let chunk = Bytes::copy_from_slice(data);
        let stream = futures::stream::once(async move { Ok(chunk) });
        self.put_stream(stream, None, tier, Some(len)).await
    }

    /// Store an object from a byte stream, RS-encoding block by block.
    ///
    /// `expected` is the hash the caller committed to. If the bytes hash to
    /// anything else the write is rejected *and every shard it wrote is
    /// removed* — a caller must not be able to leave junk behind by lying
    /// about a hash.
    ///
    /// `size_hint` (an HTTP `Content-Length`, typically) picks the block size.
    /// Without it every object gets [`DEFAULT_BLOCK_SIZE`] blocks, which is
    /// correct but produces large manifests for large objects.
    pub async fn put_stream<S>(
        &self,
        stream: S,
        expected: Option<Hash>,
        tier: Tier,
        size_hint: Option<u64>,
    ) -> Result<ObjectManifest>
    where
        S: Stream<Item = Result<Bytes>>,
    {
        let block_size = size_hint.map(block_size_for).unwrap_or(DEFAULT_BLOCK_SIZE);
        let (k, parity) = (tier.k(), tier.parity());

        let mut object_hasher = blake3::Hasher::new();
        let mut blocks: Vec<BlockEntry> = Vec::new();
        // Every shard written so far, so a hash mismatch can be fully undone.
        let mut written: Vec<Hash> = Vec::new();
        let mut buf: Vec<u8> = Vec::with_capacity(block_size as usize);
        let mut total_len: u64 = 0;

        let mut stream = Box::pin(stream);
        let mut failed: Option<anyhow::Error> = None;

        while let Some(item) = stream.next().await {
            let chunk = match item {
                Ok(c) => c,
                Err(e) => {
                    failed = Some(e);
                    break;
                }
            };
            object_hasher.update(&chunk);
            total_len += chunk.len() as u64;
            let mut rest: &[u8] = &chunk;
            while !rest.is_empty() {
                let want = block_size as usize - buf.len();
                let take = want.min(rest.len());
                buf.extend_from_slice(&rest[..take]);
                rest = &rest[take..];
                if buf.len() == block_size as usize {
                    match self.seal_block(&buf, blocks.len() as u32, k, parity).await {
                        Ok((entry, created)) => {
                            written.extend(created);
                            blocks.push(entry);
                        }
                        Err(e) => {
                            failed = Some(e);
                            break;
                        }
                    }
                    buf.clear();
                }
            }
            if failed.is_some() {
                break;
            }
        }

        // Trailing partial block.
        if failed.is_none() && !buf.is_empty() {
            match self.seal_block(&buf, blocks.len() as u32, k, parity).await {
                Ok((entry, created)) => {
                    written.extend(created);
                    blocks.push(entry);
                }
                Err(e) => failed = Some(e),
            }
        }

        if let Some(e) = failed {
            self.rollback(&written).await;
            return Err(e);
        }

        let object_hash = Hash(*object_hasher.finalize().as_bytes());
        if let Some(want) = expected
            && want != object_hash
        {
            self.rollback(&written).await;
            bail!("object integrity failure: committed to {want} but body hashes to {object_hash}");
        }

        // A zero-length object still needs one (empty) block so reads have
        // something to iterate.
        if blocks.is_empty() {
            let (entry, _) = self.seal_block(&[], 0, k, parity).await?;
            blocks.push(entry);
        }

        let manifest = ObjectManifest {
            version: MANIFEST_VERSION,
            object_hash,
            original_len: total_len,
            block_size,
            k: k as u8,
            parity: parity as u8,
            tier,
            blocks,
        };
        self.write_manifest(&manifest).await?;
        debug!(
            object_hash = %object_hash,
            blocks = manifest.blocks.len(),
            shards = manifest.shard_count(),
            %tier,
            "stored object"
        );
        Ok(manifest)
    }

    /// RS-encode one block and write its shards.
    ///
    /// Returns the block entry plus the hashes this call actually created, so
    /// a failed write can be rolled back without touching shards that were
    /// already on disk for some other object.
    async fn seal_block(
        &self,
        plaintext: &[u8],
        index: u32,
        k: usize,
        parity: usize,
    ) -> Result<(BlockEntry, Vec<Hash>)> {
        let (shards, _) = erasure::encode(plaintext, k, parity)?;
        let mut entries = Vec::with_capacity(shards.len());
        let mut created = Vec::new();
        for s in &shards {
            let (hash, is_new) = self.inner.put_new(&s.bytes).await?;
            if is_new {
                created.push(hash);
            }
            entries.push(ShardEntry {
                index: s.index,
                hash,
                host_hint: None,
            });
        }
        Ok((
            BlockEntry {
                index,
                len: plaintext.len() as u32,
                hash: Hash::of(plaintext),
                shards: entries,
            },
            created,
        ))
    }

    /// Remove the shards *this* write created, after it failed.
    ///
    /// Only newly-created hashes are passed in, and that distinction is
    /// load-bearing rather than an optimisation. Shards are content-addressed
    /// and therefore shared: PUTting the bytes of an object that already
    /// exists, under a wrong committed hash, produces exactly the same shard
    /// hashes. Rolling back every hash the write touched would delete the
    /// intact object's shards — data loss triggered by one malformed request
    /// from anyone who can obtain the content.
    async fn rollback(&self, hashes: &[Hash]) {
        for h in hashes {
            if let Err(e) = self.inner.delete(h).await {
                warn!(hash = %h, err = %e, "rollback failed to remove shard");
            }
        }
    }

    // ----------------------------------------------------------------- reads

    /// Read a whole object into memory. Prefer [`Storage::read_stream`] for
    /// anything that might be large.
    pub async fn get(&self, object_hash: &Hash) -> Result<Vec<u8>> {
        let manifest = self.read_manifest(object_hash).await?;
        let mut out = Vec::with_capacity(manifest.original_len as usize);
        for i in 0..manifest.blocks.len() {
            out.extend_from_slice(&self.read_block(&manifest, i).await?);
        }
        let actual = Hash::of(&out);
        if actual != manifest.object_hash {
            bail!(
                "object integrity failure: manifest says {} but decoded bytes hash to {}",
                manifest.object_hash,
                actual
            );
        }
        Ok(out)
    }

    /// Stream an object back, reconstructing one block at a time.
    ///
    /// Memory stays bounded by block size regardless of object size.
    pub fn read_stream(
        &self,
        manifest: ObjectManifest,
    ) -> Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>> {
        let storage = self.clone();
        Box::pin(futures::stream::unfold(
            (storage, manifest, 0usize),
            |(storage, manifest, i)| async move {
                if i >= manifest.blocks.len() {
                    return None;
                }
                let item = storage.read_block(&manifest, i).await.map(Bytes::from);
                Some((item, (storage, manifest, i + 1)))
            },
        ))
    }

    /// Read `[offset, offset + len)` of an object, touching only the blocks
    /// that range covers.
    pub async fn get_range(&self, object_hash: &Hash, offset: u64, len: u64) -> Result<Vec<u8>> {
        let manifest = self.read_manifest(object_hash).await?;
        self.get_range_with(&manifest, offset, len).await
    }

    pub async fn get_range_with(
        &self,
        manifest: &ObjectManifest,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>> {
        let range = manifest.blocks_for_range(offset, len);
        if range.is_empty() {
            return Ok(Vec::new());
        }
        let bs = manifest.block_size as u64;
        let end = (offset + len).min(manifest.original_len);
        let mut out = Vec::with_capacity((end - offset) as usize);

        for i in range {
            let block = self.read_block(manifest, i).await?;
            let block_start = i as u64 * bs;
            let block_end = block_start + block.len() as u64;
            let take_from = offset.max(block_start) - block_start;
            let take_to = end.min(block_end) - block_start;
            out.extend_from_slice(&block[take_from as usize..take_to as usize]);
        }
        Ok(out)
    }

    /// Reconstruct one block, tolerating up to `parity` missing shards.
    pub async fn read_block(&self, manifest: &ObjectManifest, index: usize) -> Result<Vec<u8>> {
        let entry = manifest
            .blocks
            .get(index)
            .ok_or_else(|| anyhow!("block {index} out of range"))?;

        let n = manifest.n();
        let mut received: Vec<Option<Shard>> = vec![None; n];
        let mut found = 0usize;
        for shard in &entry.shards {
            match self.inner.get(&shard.hash).await {
                Ok(bytes) => {
                    received[shard.index as usize] = Some(Shard {
                        index: shard.index,
                        bytes,
                    });
                    found += 1;
                }
                Err(e) => {
                    warn!(
                        object_hash = %manifest.object_hash,
                        block = index,
                        shard_index = shard.index,
                        err = %e,
                        "shard unreadable; falling back to parity"
                    );
                }
            }
        }
        if found < manifest.k as usize {
            bail!(
                "block {index} of object {}: need at least {} shards, found {found}",
                manifest.object_hash,
                manifest.k
            );
        }

        let mut plaintext = erasure::decode(
            received,
            manifest.k as usize,
            manifest.parity as usize,
            entry.len as usize,
        )?;
        plaintext.truncate(entry.len as usize);

        let actual = Hash::of(&plaintext);
        if actual != entry.hash {
            bail!(
                "block {index} of object {}: integrity failure, manifest says {} but bytes hash to {actual}",
                manifest.object_hash,
                entry.hash
            );
        }
        Ok(plaintext)
    }

    // ------------------------------------------------------------- manifests

    pub async fn has(&self, object_hash: &Hash) -> bool {
        fs::metadata(self.manifest_path(object_hash)).await.is_ok()
    }

    /// Every object this node holds a manifest for.
    ///
    /// Walks the manifest directory rather than keeping an index: the
    /// authoritative object list for a customer lives in their volume
    /// (CIP-004), and this is only "what is on this disk".
    pub async fn list(&self) -> Result<Vec<Hash>> {
        let root = self.inner.root().join("manifests");
        let mut out = Vec::new();
        let mut dirs = vec![root];
        while let Some(dir) = dirs.pop() {
            let mut rd = match fs::read_dir(&dir).await {
                Ok(rd) => rd,
                Err(_) => continue,
            };
            while let Some(entry) = rd.next_entry().await? {
                let path = entry.path();
                if entry.file_type().await?.is_dir() {
                    dirs.push(path);
                } else if let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                    && let Ok(h) = Hash::from_hex(stem)
                {
                    out.push(h);
                }
            }
        }
        out.sort_by_key(|h| h.to_hex());
        Ok(out)
    }

    /// Delete an object's manifest and every shard it points at.
    pub async fn delete(&self, object_hash: &Hash) -> Result<()> {
        let manifest = match self.read_manifest(object_hash).await {
            Ok(m) => m,
            Err(_) => return Ok(()),
        };
        for block in &manifest.blocks {
            for shard in &block.shards {
                let _ = self.inner.delete(&shard.hash).await;
            }
        }
        let _ = fs::remove_file(self.manifest_path(object_hash)).await;
        Ok(())
    }

    pub async fn write_manifest(&self, m: &ObjectManifest) -> Result<()> {
        let path = self.manifest_path(&m.object_hash);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let json = serde_json::to_vec_pretty(m).context("serialize manifest")?;
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, &json).await?;
        fs::rename(&tmp, &path).await?;
        Ok(())
    }

    pub async fn read_manifest(&self, object_hash: &Hash) -> Result<ObjectManifest> {
        let path = self.manifest_path(object_hash);
        let bytes = fs::read(&path)
            .await
            .with_context(|| format!("read manifest {}", path.display()))?;
        let m: ObjectManifest = serde_json::from_slice(&bytes).context("parse manifest JSON")?;
        if m.object_hash != *object_hash {
            return Err(anyhow!(
                "manifest object_hash {} != requested {object_hash}",
                m.object_hash
            ));
        }
        Ok(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> Storage {
        let dir = std::env::temp_dir().join(format!(
            "c0mpute-storage-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cs = ChunkStore::open(&dir).await.unwrap();
        Storage::new(cs)
    }

    /// Pseudo-random bytes. Critical for shard-loss tests: identical shard
    /// content collides in the content-addressed chunk store (real, desirable
    /// dedup) which would make "lose 4 of 14" meaningless.
    fn varied(len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        for _ in 0..len {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            out.push((state & 0xff) as u8);
        }
        out
    }

    #[tokio::test]
    async fn put_get_roundtrip() {
        let s = store().await;
        let data = b"hello c0mpute erasure-coded storage".repeat(50);
        let m = s.put(&data).await.unwrap();
        assert_eq!(m.version, MANIFEST_VERSION);
        assert_eq!(m.tier, Tier::Standard);
        assert_eq!(m.shard_count(), 14);
        assert_eq!(s.get(&m.object_hash).await.unwrap(), data);
    }

    #[tokio::test]
    async fn survives_four_lost_shards() {
        let s = store().await;
        let data = varied(50_000);
        let m = s.put(&data).await.unwrap();
        for shard in m.blocks[0].shards.iter().take(4) {
            s.inner.delete(&shard.hash).await.unwrap();
        }
        assert_eq!(s.get(&m.object_hash).await.unwrap(), data);
    }

    #[tokio::test]
    async fn fails_on_five_lost_shards() {
        let s = store().await;
        let data = varied(20_000);
        let m = s.put(&data).await.unwrap();
        for shard in m.blocks[0].shards.iter().take(5) {
            s.inner.delete(&shard.hash).await.unwrap();
        }
        let err = s.get(&m.object_hash).await.unwrap_err().to_string();
        assert!(err.contains("need at least"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn empty_object_roundtrips() {
        let s = store().await;
        let m = s.put(b"").await.unwrap();
        assert_eq!(m.original_len, 0);
        assert_eq!(s.get(&m.object_hash).await.unwrap(), Vec::<u8>::new());
    }

    #[tokio::test]
    async fn multi_block_object_roundtrips() {
        let s = store().await;
        // Three blocks and a bit, at the smallest block size.
        let data = varied(DEFAULT_BLOCK_SIZE as usize * 3 + 1234);
        let m = s.put(&data).await.unwrap();
        assert_eq!(m.blocks.len(), 4);
        assert_eq!(m.blocks[3].len, 1234);
        assert_eq!(s.get(&m.object_hash).await.unwrap(), data);
    }

    /// `hot` is RS with k=1, which makes every parity shard byte-identical to
    /// the data shard — i.e. genuine 3-copy replication rather than coding.
    ///
    /// That identity is the point: repair is a copy from a survivor (1x
    /// amplification, per CIP-001) instead of a k-shard reconstruction. It
    /// also means that *on one node* the content-addressed store holds a
    /// single chunk for all three, since they share a hash. Durability comes
    /// from placing that hash on 3 distinct hosts (CIP-003), not from three
    /// distinct byte strings on one disk.
    #[tokio::test]
    async fn hot_tier_is_true_replication() {
        let s = store().await;
        let data = varied(10_000);
        let m = s.put_tiered(&data, Tier::Hot).await.unwrap();
        assert_eq!(m.shard_count(), 3);
        assert_eq!(m.k, 1);
        assert_eq!(m.parity, 2);

        let hashes: Vec<_> = m.blocks[0].shards.iter().map(|s| s.hash).collect();
        assert!(
            hashes.iter().all(|h| *h == hashes[0]),
            "hot-tier shards should be identical copies, got {hashes:?}"
        );
        assert_eq!(s.get(&m.object_hash).await.unwrap(), data);
    }

    /// Any one of the three hot-tier shards reconstructs the block. Tested at
    /// the erasure layer because the chunk store dedups the three copies into
    /// one file, so shard loss cannot be simulated by deleting from it.
    #[test]
    fn hot_tier_decodes_from_any_single_shard() {
        let data = varied(4096);
        let (shards, len) = erasure::encode(&data, 1, 2).unwrap();
        assert_eq!(shards.len(), 3);
        for keep in 0..3 {
            let received: Vec<Option<Shard>> = (0..3)
                .map(|i| {
                    if i == keep {
                        Some(shards[i].clone())
                    } else {
                        None
                    }
                })
                .collect();
            let out = erasure::decode(received, 1, 2, len).unwrap();
            assert_eq!(out, data, "failed to rebuild from shard {keep} alone");
        }
    }

    #[tokio::test]
    async fn critical_tier_survives_twelve_losses() {
        let s = store().await;
        let data = varied(40_000);
        let m = s.put_tiered(&data, Tier::Critical).await.unwrap();
        assert_eq!(m.shard_count(), 32);
        for shard in m.blocks[0].shards.iter().take(12) {
            s.inner.delete(&shard.hash).await.unwrap();
        }
        assert_eq!(s.get(&m.object_hash).await.unwrap(), data);
    }

    #[tokio::test]
    async fn wrong_committed_hash_is_rejected_and_leaves_nothing() {
        let s = store().await;
        let data = varied(9_000);
        let bogus = Hash::of(b"not the data");
        let chunk = Bytes::from(data.clone());
        let stream = futures::stream::once(async move { Ok(chunk) });
        let err = s
            .put_stream(stream, Some(bogus), Tier::Standard, Some(data.len() as u64))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("integrity failure"), "unexpected: {err}");

        // Nothing left behind: no manifest, and the shards were rolled back.
        assert!(!s.has(&Hash::of(&data)).await);
        let shard_dir = s.inner.root().join("shards");
        let mut count = 0;
        for entry in walk(&shard_dir) {
            if entry.is_file() {
                count += 1;
            }
        }
        assert_eq!(count, 0, "rollback left {count} shards on disk");
    }

    /// Regression: a rejected write must not damage an object that already
    /// holds the same content.
    ///
    /// Shards are content-addressed, so re-uploading existing bytes under a
    /// wrong committed hash yields identical shard hashes. Rolling back
    /// everything the write touched used to delete the good object's shards,
    /// turning one malformed request into data loss. Found by running the
    /// real HTTP server, not by the unit tests — the original test stored
    /// nothing beforehand, so there was nothing to destroy.
    #[tokio::test]
    async fn failed_write_does_not_delete_an_existing_objects_shards() {
        let s = store().await;
        let data = varied(300_000);

        let good = s.put(&data).await.unwrap();
        assert_eq!(s.get(&good.object_hash).await.unwrap(), data);

        // Same bytes, wrong committed hash.
        let chunk = Bytes::from(data.clone());
        let stream = futures::stream::once(async move { Ok(chunk) });
        let err = s
            .put_stream(
                stream,
                Some(Hash::of(b"a different object entirely")),
                Tier::Standard,
                Some(data.len() as u64),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("integrity failure"), "unexpected: {err}");

        // The original object must be completely intact.
        for block in &good.blocks {
            for shard in &block.shards {
                assert!(
                    s.inner.has(&shard.hash).await,
                    "rollback deleted a shard belonging to an intact object"
                );
            }
        }
        assert_eq!(s.get(&good.object_hash).await.unwrap(), data);
    }

    #[tokio::test]
    async fn put_new_reports_creation_once() {
        let s = store().await;
        let bytes = varied(4096);
        let (h1, created1) = s.inner.put_new(&bytes).await.unwrap();
        let (h2, created2) = s.inner.put_new(&bytes).await.unwrap();
        assert_eq!(h1, h2);
        assert!(created1, "first write should create");
        assert!(!created2, "second write should find it present");
    }

    fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir(dir) else {
            return out;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else {
                out.push(p);
            }
        }
        out
    }

    #[tokio::test]
    async fn range_read_touches_only_needed_blocks() {
        let s = store().await;
        let data = varied(DEFAULT_BLOCK_SIZE as usize * 3);
        let m = s.put(&data).await.unwrap();

        let got = s.get_range(&m.object_hash, 1_000_000, 4096).await.unwrap();
        assert_eq!(got, &data[1_000_000..1_004_096]);

        // A range wholly inside block 2 must read exactly one block.
        let m2 = s.read_manifest(&m.object_hash).await.unwrap();
        let r = m2.blocks_for_range(DEFAULT_BLOCK_SIZE as u64 * 2 + 10, 100);
        assert_eq!(r, 2..3);
    }

    #[tokio::test]
    async fn range_spanning_block_boundary() {
        let s = store().await;
        let data = varied(DEFAULT_BLOCK_SIZE as usize * 2);
        let m = s.put(&data).await.unwrap();
        let off = DEFAULT_BLOCK_SIZE as u64 - 50;
        let got = s.get_range(&m.object_hash, off, 100).await.unwrap();
        assert_eq!(got, &data[off as usize..off as usize + 100]);
    }

    #[tokio::test]
    async fn range_past_end_is_clamped() {
        let s = store().await;
        let data = varied(1000);
        let m = s.put(&data).await.unwrap();
        let got = s.get_range(&m.object_hash, 900, 500).await.unwrap();
        assert_eq!(got, &data[900..1000]);
        assert!(
            s.get_range(&m.object_hash, 5000, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn read_stream_matches_get() {
        let s = store().await;
        let data = varied(DEFAULT_BLOCK_SIZE as usize + 777);
        let m = s.put(&data).await.unwrap();
        let mut out = Vec::new();
        let mut stream = s.read_stream(m.clone());
        while let Some(chunk) = stream.next().await {
            out.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(out, data);
    }

    #[tokio::test]
    async fn block_size_scales_to_keep_manifests_small() {
        assert_eq!(block_size_for(1024), DEFAULT_BLOCK_SIZE);
        assert_eq!(block_size_for(1 << 30), DEFAULT_BLOCK_SIZE); // 1 GiB
        // 1 TiB must not produce a quarter-million blocks.
        let bs = block_size_for(1 << 40);
        let blocks = (1u64 << 40).div_ceil(bs as u64);
        assert!(blocks <= TARGET_BLOCKS_PER_OBJECT, "{blocks} blocks");
        assert!(bs <= MAX_BLOCK_SIZE);
    }

    #[tokio::test]
    async fn v1_manifest_still_deserialises() {
        let v1 = serde_json::json!({
            "object_hash": Hash::of(b"legacy").to_hex(),
            "original_len": 6,
            "k": 10,
            "parity": 4,
            "shards": [ { "index": 0, "hash": Hash::of(b"s0").to_hex() } ],
        });
        let m: ObjectManifest = serde_json::from_value(v1).unwrap();
        assert_eq!(m.version, 1);
        assert_eq!(m.tier, Tier::Standard);
        assert_eq!(m.blocks.len(), 1);
        assert_eq!(m.blocks[0].len, 6);
        assert_eq!(m.blocks[0].hash, m.object_hash);
    }

    #[tokio::test]
    async fn manifest_v2_roundtrips_through_json() {
        let s = store().await;
        let m = s.put(&varied(5000)).await.unwrap();
        let json = serde_json::to_vec(&m).unwrap();
        let back: ObjectManifest = serde_json::from_slice(&json).unwrap();
        assert_eq!(m, back);
    }

    #[tokio::test]
    async fn corrupted_shard_is_detected_not_returned() {
        let s = store().await;
        let data = varied(30_000);
        let m = s.put(&data).await.unwrap();
        // Corrupt enough shards that parity cannot mask it.
        for shard in m.blocks[0].shards.iter().take(5) {
            let path = s.inner.root().join("shards");
            let hex = shard.hash.to_hex();
            let p = path.join(&hex[0..2]).join(&hex[2..4]).join(&hex);
            tokio::fs::write(&p, b"corrupted").await.unwrap();
        }
        assert!(s.get(&m.object_hash).await.is_err());
    }

    #[tokio::test]
    async fn delete_removes_manifest_and_shards() {
        let s = store().await;
        let m = s.put(&varied(8000)).await.unwrap();
        assert!(s.has(&m.object_hash).await);
        s.delete(&m.object_hash).await.unwrap();
        assert!(!s.has(&m.object_hash).await);
        for shard in &m.blocks[0].shards {
            assert!(!s.inner.has(&shard.hash).await);
        }
    }
}
