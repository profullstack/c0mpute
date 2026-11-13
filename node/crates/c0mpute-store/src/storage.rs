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

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Weak};

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

    /// Whether the blocks tile the object the way range reads assume: every
    /// block but the last is `block_size` long, the last is no longer, and
    /// the lengths add up to `original_len`. A manifest that does not would
    /// make a ranged read return other bytes than the range asked for.
    pub fn tiles(&self) -> bool {
        let Some((last, full)) = self.blocks.split_last() else {
            return false;
        };
        self.block_size > 0
            && full.iter().all(|b| b.len == self.block_size)
            && last.len <= self.block_size
            && self.blocks.iter().map(|b| b.len as u64).sum::<u64>() == self.original_len
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

/// What keeps a delete from removing a shard a write is about to depend on
/// (see [`Storage::delete`]). One per store root in a process, shared by
/// every [`Storage`] opened on it. Nothing here is on disk: after a crash
/// there are no writes in progress to protect.
#[derive(Debug, Default)]
struct Guard {
    /// Held while a write commits its manifest, and for the whole of a
    /// delete: while a delete looks through the manifests, none appears.
    commit: tokio::sync::Mutex<()>,
    /// Shards the writes in progress have produced or found already present
    /// (a count each: two writes can share a shard). Their manifests are not
    /// on disk yet, so this is the only record that they are needed.
    writing: std::sync::Mutex<HashMap<Hash, usize>>,
}

/// The guard of each store root opened in this process.
static GUARDS: std::sync::Mutex<Vec<(PathBuf, Weak<Guard>)>> = std::sync::Mutex::new(Vec::new());

fn guard_for(root: &std::path::Path) -> Arc<Guard> {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut guards = GUARDS.lock().unwrap_or_else(|e| e.into_inner());
    guards.retain(|(_, g)| g.strong_count() > 0);
    if let Some(g) = guards.iter().find(|(r, _)| *r == root).and_then(|(_, g)| g.upgrade()) {
        return g;
    }
    let g = Arc::new(Guard::default());
    guards.push((root, Arc::downgrade(&g)));
    g
}

/// The shards one write in progress has registered in [`Guard::writing`].
/// Dropping it (the write finished, failed, or was cancelled with its
/// request) takes them out again.
struct Writing {
    guard: Arc<Guard>,
    hashes: Vec<Hash>,
}

impl Writing {
    /// Register `hashes` before they are written or relied on.
    fn add(&mut self, hashes: &[Hash]) {
        let mut writing = self.guard.writing.lock().unwrap_or_else(|e| e.into_inner());
        for h in hashes {
            *writing.entry(*h).or_insert(0) += 1;
        }
        self.hashes.extend_from_slice(hashes);
    }

    fn release(&mut self) {
        let mut writing = self.guard.writing.lock().unwrap_or_else(|e| e.into_inner());
        for h in self.hashes.drain(..) {
            if let Some(n) = writing.get_mut(&h) {
                *n -= 1;
                if *n == 0 {
                    writing.remove(&h);
                }
            }
        }
    }
}

impl Drop for Writing {
    fn drop(&mut self) {
        self.release();
    }
}

#[derive(Clone, Debug)]
pub struct Storage {
    inner: ChunkStore,
    guard: Arc<Guard>,
}

impl Storage {
    pub fn new(inner: ChunkStore) -> Self {
        let guard = guard_for(inner.root());
        Self { inner, guard }
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
        // Every shard this write depends on, new or already here, kept from
        // a concurrent delete until the manifest that names them is on disk.
        let mut writing = Writing {
            guard: self.guard.clone(),
            hashes: Vec::new(),
        };

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
                    match self.seal_block(&buf, blocks.len() as u32, k, parity, &mut writing).await {
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
            match self.seal_block(&buf, blocks.len() as u32, k, parity, &mut writing).await {
                Ok((entry, created)) => {
                    written.extend(created);
                    blocks.push(entry);
                }
                Err(e) => failed = Some(e),
            }
        }

        if let Some(e) = failed {
            writing.release();
            self.rollback(&written).await;
            return Err(e);
        }

        let object_hash = Hash(*object_hasher.finalize().as_bytes());
        if let Some(want) = expected
            && want != object_hash
        {
            writing.release();
            self.rollback(&written).await;
            bail!("object integrity failure: committed to {want} but body hashes to {object_hash}");
        }

        // A zero-length object still needs one (empty) block so reads have
        // something to iterate.
        if blocks.is_empty() {
            let (entry, _) = self.seal_block(&[], 0, k, parity, &mut writing).await?;
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
        {
            // Not while a delete is looking through the manifests: it would
            // miss this one. The shards stay registered until it is written.
            let _commit = self.guard.commit.lock().await;
            self.write_manifest(&manifest).await?;
        }
        drop(writing);
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
        writing: &mut Writing,
    ) -> Result<(BlockEntry, Vec<Hash>)> {
        let (shards, _) = erasure::encode(plaintext, k, parity)?;
        // Registered before any of them is written or found present: from
        // here on a delete leaves them alone (see [`Storage::delete`]).
        let hashes: Vec<Hash> = shards.iter().map(|s| Hash::of(&s.bytes)).collect();
        writing.add(&hashes);
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
    ///
    /// A shard this write created may since have been taken up by another
    /// object (written, or being written, with the same content), so the
    /// same two checks as [`Storage::delete`] apply.
    async fn rollback(&self, hashes: &[Hash]) {
        if hashes.is_empty() {
            return;
        }
        let _commit = self.guard.commit.lock().await;
        let mut doomed: HashSet<Hash> = hashes.iter().copied().collect();
        self.spare_referenced(&mut doomed, None).await;
        self.remove_unwritten(doomed).await;
    }

    /// Take out of `doomed` every shard a manifest on this disk names,
    /// leaving aside the manifest of `except` (the object being deleted).
    /// Call with [`Guard::commit`] held.
    async fn spare_referenced(&self, doomed: &mut HashSet<Hash>, except: Option<&Hash>) {
        let others = match self.list().await {
            Ok(v) => v,
            Err(e) => {
                // Without the list nothing can be shown to be unshared.
                warn!(err = %e, "cannot list manifests; keeping every shard");
                doomed.clear();
                return;
            }
        };
        for other in others {
            if doomed.is_empty() {
                return;
            }
            if Some(&other) == except {
                continue;
            }
            match self.read_manifest(&other).await {
                Ok(m) => {
                    for shard in m.blocks.iter().flat_map(|b| &b.shards) {
                        doomed.remove(&shard.hash);
                    }
                }
                // Unreadable as a manifest, so unreadable as an object too:
                // it cannot be shown to need any shard.
                Err(e) => warn!(object_hash = %other, err = %e, "skipping an unreadable manifest"),
            }
        }
    }

    /// Remove the shards of `doomed` that no write in progress depends on.
    /// Each check and removal is one step with respect to
    /// [`Writing::add`]: a write that registers a shard afterwards finds it
    /// gone and writes it again.
    async fn remove_unwritten(&self, doomed: HashSet<Hash>) {
        let (guard, store) = (self.guard.clone(), self.inner.clone());
        let removal = tokio::task::spawn_blocking(move || {
            for h in doomed {
                let writing = guard.writing.lock().unwrap_or_else(|e| e.into_inner());
                if writing.contains_key(&h) {
                    continue;
                }
                match std::fs::remove_file(store.chunk_path(&h)) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => warn!(hash = %h, err = %e, "failed to remove shard"),
                }
            }
        });
        if let Err(e) = removal.await {
            warn!(err = %e, "shard removal did not finish");
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

    /// Stream `[offset, offset + len)` of an object (clamped to its end),
    /// reconstructing one block at a time: a range of a huge object costs
    /// one block of memory, like [`Storage::read_stream`], not the range.
    ///
    /// The manifest should [tile](ObjectManifest::tiles); a block shorter
    /// than its place in the range ends the stream with an error.
    pub fn read_range_stream(
        &self,
        manifest: ObjectManifest,
        offset: u64,
        len: u64,
    ) -> Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>> {
        let end = offset.saturating_add(len).min(manifest.original_len);
        let range = manifest.blocks_for_range(offset, end.saturating_sub(offset));
        let storage = self.clone();
        Box::pin(futures::stream::unfold(
            (storage, manifest, range.start, range.end),
            move |(storage, manifest, i, stop)| async move {
                if i >= stop {
                    return None;
                }
                let item = storage.read_block(&manifest, i).await.and_then(|block| {
                    let bs = manifest.block_size as u64;
                    let block_start = i as u64 * bs;
                    let from = offset.max(block_start) - block_start;
                    let to = end.min(block_start + bs) - block_start;
                    if to > block.len() as u64 || from > to {
                        bail!(
                            "block {i} of object {} is {} bytes, shorter than its place in the object",
                            manifest.object_hash,
                            block.len()
                        );
                    }
                    Ok(Bytes::from(block).slice(from as usize..to as usize))
                });
                // Nothing after a failed block: the bytes would be misplaced.
                let next = if item.is_ok() { i + 1 } else { stop };
                Some((item, (storage, manifest, next, stop)))
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
                    // The index is the manifest's word: one past the block's
                    // shards is an error, not a slot to write.
                    let Some(slot) = received.get_mut(shard.index as usize) else {
                        bail!(
                            "block {index} of object {}: shard index {} is outside its {n} shards",
                            manifest.object_hash,
                            shard.index
                        );
                    };
                    if slot.is_none() {
                        found += 1;
                    }
                    *slot = Some(Shard {
                        index: shard.index,
                        bytes,
                    });
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

    /// Delete an object's manifest and the shards only it points at.
    ///
    /// Shards are content-addressed, so two objects with a block in common
    /// share that block's shards (see [`Storage::rollback`]): a shard goes
    /// only when no other manifest on this disk names it and no write in
    /// progress depends on it. The other manifests are read to find out,
    /// which makes a delete cost one pass over them; there is no reference
    /// count to keep right, and none to rebuild or to be wrong after a
    /// crash.
    ///
    /// The shards go first and the manifest last. A crash in between leaves
    /// the manifest of a half-deleted object, never shards nothing names:
    /// deleting it again finishes the job, and no other object has lost
    /// anything.
    pub async fn delete(&self, object_hash: &Hash) -> Result<()> {
        let _commit = self.guard.commit.lock().await;
        let manifest = match self.read_manifest(object_hash).await {
            Ok(m) => m,
            Err(_) => return Ok(()),
        };
        let mut doomed: HashSet<Hash> = manifest
            .blocks
            .iter()
            .flat_map(|b| &b.shards)
            .map(|s| s.hash)
            .collect();
        self.spare_referenced(&mut doomed, Some(object_hash)).await;
        self.remove_unwritten(doomed).await;
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
    async fn deleting_one_of_two_objects_that_share_a_block_keeps_the_other_intact() {
        let s = store().await;
        // Two objects with the same first block and different second ones.
        let common = varied(DEFAULT_BLOCK_SIZE as usize);
        let mut a = common.clone();
        a.extend_from_slice(b"the tail of a");
        let mut b = common.clone();
        b.extend_from_slice(b"and b ends another way");
        let ma = s.put(&a).await.unwrap();
        let mb = s.put(&b).await.unwrap();
        let shared: Vec<Hash> = ma.blocks[0].shards.iter().map(|x| x.hash).collect();
        assert_eq!(shared, mb.blocks[0].shards.iter().map(|x| x.hash).collect::<Vec<_>>());
        assert_ne!(ma.blocks[1].hash, mb.blocks[1].hash);

        s.delete(&ma.object_hash).await.unwrap();
        assert!(!s.has(&ma.object_hash).await);
        for h in &shared {
            assert!(s.inner.has(h).await, "a shard b still needs was deleted");
        }
        // What only a used is gone...
        for shard in &ma.blocks[1].shards {
            assert!(!s.inner.has(&shard.hash).await);
        }
        // ...and b reads back whole, with every shard in place.
        assert_eq!(s.get(&mb.object_hash).await.unwrap(), b);
        for shard in mb.blocks.iter().flat_map(|x| &x.shards) {
            assert!(s.inner.has(&shard.hash).await);
        }

        // Deleting both removes the shared shards too: nothing is left.
        s.delete(&mb.object_hash).await.unwrap();
        let left = walk(&s.inner.root().join("shards")).into_iter().filter(|p| p.is_file()).count();
        assert_eq!(left, 0, "shards nothing names were left behind");
        assert!(s.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_delete_leaves_alone_shards_a_write_in_progress_depends_on() {
        let s = store().await;
        let data = varied(20_000);
        let m = s.put(&data).await.unwrap();
        let hashes: Vec<Hash> = m.blocks[0].shards.iter().map(|x| x.hash).collect();
        // Another write of the same content has sealed its block (the
        // shards were already here) but not written its manifest yet. It
        // goes through a second handle on the same store, as another part
        // of the process would.
        let other = Storage::new(s.inner.clone());
        let mut writing = Writing {
            guard: other.guard.clone(),
            hashes: Vec::new(),
        };
        writing.add(&hashes);
        s.delete(&m.object_hash).await.unwrap();
        assert!(!s.has(&m.object_hash).await);
        for h in &hashes {
            assert!(s.inner.has(h).await, "deleted under a write in progress");
        }
        // That write commits: its object is whole.
        other.write_manifest(&m).await.unwrap();
        drop(writing);
        assert_eq!(other.get(&m.object_hash).await.unwrap(), data);
        // With no write in progress the shards go with the object.
        s.delete(&m.object_hash).await.unwrap();
        for h in &hashes {
            assert!(!s.inner.has(h).await);
        }
        assert!(s.guard.writing.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_failed_write_does_not_take_back_a_shard_another_object_took_up() {
        let s = store().await;
        let block = varied(DEFAULT_BLOCK_SIZE as usize);
        // A write that will fail (wrong committed hash) seals its first
        // block, creating the shards, and then waits for more of its body.
        let (sealed_tx, sealed_rx) = tokio::sync::oneshot::channel::<()>();
        let (go_tx, go_rx) = tokio::sync::oneshot::channel::<()>();
        let first = Bytes::from(block.clone());
        let stream = futures::stream::once(async move { Ok(first) }).chain(futures::stream::once(async move {
            let _ = sealed_tx.send(());
            let _ = go_rx.await;
            Ok(Bytes::from_static(b"the rest of a body that does not match"))
        }));
        let failing = s.put_stream(stream, Some(Hash::of(b"not the data")), Tier::Standard, None);
        // Meanwhile the same block is stored for real: its shards are the
        // ones the failing write created.
        let good = async {
            sealed_rx.await.unwrap();
            let m = s.put(&block).await.unwrap();
            go_tx.send(()).unwrap();
            m
        };
        let (failed, good) = tokio::join!(failing, good);
        assert!(failed.unwrap_err().to_string().contains("integrity failure"));
        // The failed write took back only what nothing else names.
        assert_eq!(s.get(&good.object_hash).await.unwrap(), block);
        let kept: HashSet<Hash> = good.blocks.iter().flat_map(|b| &b.shards).map(|x| x.hash).collect();
        let left = walk(&s.inner.root().join("shards")).into_iter().filter(|p| p.is_file()).count();
        assert_eq!(left, kept.len(), "exactly the stored object's shards remain");
    }

    #[tokio::test]
    async fn a_shard_index_outside_the_block_is_an_error_not_a_panic() {
        let s = store().await;
        let data = varied(5_000);
        let mut m = s.put(&data).await.unwrap();
        m.blocks[0].shards[3].index = 200;
        let err = s.read_block(&m, 0).await.unwrap_err().to_string();
        assert!(err.contains("outside its 14 shards"), "{err}");
        // The same slot named twice is one shard, not two.
        let mut m = s.read_manifest(&m.object_hash).await.unwrap();
        for shard in m.blocks[0].shards.iter_mut() {
            shard.index = 0;
        }
        let err = s.read_block(&m, 0).await.unwrap_err().to_string();
        assert!(err.contains("need at least"), "{err}");
    }

    #[tokio::test]
    async fn a_range_streams_block_by_block_and_matches_the_bytes() {
        let s = store().await;
        let bs = DEFAULT_BLOCK_SIZE as u64;
        let data = varied(bs as usize * 2 + 1000);
        let m = s.put(&data).await.unwrap();
        assert!(m.tiles());
        let read = |offset: u64, len: u64| {
            let (s, m) = (s.clone(), m.clone());
            async move {
                let mut chunks = Vec::new();
                let mut stream = s.read_range_stream(m, offset, len);
                while let Some(c) = stream.next().await {
                    chunks.push(c.unwrap());
                }
                chunks
            }
        };
        // Across all three blocks: three pieces, none bigger than a block.
        let chunks = read(100, bs * 2 + 500).await;
        assert_eq!(chunks.len(), 3);
        assert!(chunks.iter().all(|c| c.len() as u64 <= bs));
        assert_eq!(chunks.concat(), &data[100..(bs * 2 + 600) as usize]);
        // Inside one block: one piece of exactly the range.
        let chunks = read(bs + 7, 10).await;
        assert_eq!(chunks.concat(), &data[(bs + 7) as usize..(bs + 17) as usize]);
        assert_eq!(chunks.len(), 1);
        // The whole object, a range past the end (clamped), and nothing.
        assert_eq!(read(0, u64::MAX).await.concat(), data);
        assert_eq!(read(bs * 2 + 900, 5000).await.concat(), &data[(bs * 2 + 900) as usize..]);
        assert!(read(data.len() as u64, 10).await.is_empty());
        assert!(read(5, 0).await.is_empty());
    }

    #[tokio::test]
    async fn a_range_over_a_manifest_that_does_not_tile_fails_instead_of_misplacing_bytes() {
        let s = store().await;
        let bs = DEFAULT_BLOCK_SIZE as usize;
        let data = varied(bs + 100);
        let good = s.put(&data).await.unwrap();
        // A manifest whose first block is the short one.
        let mut bad = good.clone();
        bad.blocks.swap(0, 1);
        assert!(!bad.tiles());
        let mut stream = s.read_range_stream(bad, 0, bs as u64 + 100);
        assert!(stream.next().await.unwrap().is_err());
        assert!(stream.next().await.is_none(), "nothing follows a failed block");
        // Lengths that do not add up, and no blocks at all.
        let mut bad = good.clone();
        bad.original_len += 1;
        assert!(!bad.tiles());
        let mut bad = good.clone();
        bad.blocks.clear();
        assert!(!bad.tiles());
        // An empty object tiles (one empty block).
        assert!(s.put(b"").await.unwrap().tiles());
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
