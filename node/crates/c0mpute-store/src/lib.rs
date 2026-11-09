//! Content-addressed chunk store backed by the local filesystem.
//!
//! Layout: `<root>/shards/<aa>/<bb>/<full-hash>` (sharded two levels deep by
//! the first four hex chars). The hash IS the integrity check — every read
//! re-hashes by default; callers can opt out with `read_unchecked` when they
//! already trust the source (e.g. just-written file).
//!
//! Default `<root>` is `~/data/c0mpute` so operators can find and migrate
//! their bulk shard data without spelunking dotfiles.
//!
//! Two layers in this crate:
//!
//!   1. `ChunkStore` — opaque byte blobs keyed by blake3 hash.
//!   2. `erasure` module — Reed-Solomon 10/14 encode/decode for the
//!      storage plugin (DIP-0012). Built on top of `ChunkStore`.

pub mod erasure;
pub mod storage;
pub mod tier;

pub use storage::{
    BlockEntry, DEFAULT_BLOCK_SIZE, MANIFEST_VERSION, MAX_BLOCK_SIZE, ObjectManifest, ShardEntry,
    Storage, block_size_for,
};
pub use tier::Tier;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use c0mpute_proto::Hash;
use tokio::fs;
use tracing::{debug, instrument};

/// Free disk a store never writes into unless told otherwise: 20 GiB, or a
/// tenth of the disk when that is smaller (a 32 GB card keeps 3.2 GB).
/// `C0MPUTE_STORAGE_MIN_FREE_GB` sets it exactly (`0` turns the floor off).
///
/// Without a floor a node keeps accepting shards until the disk is full, and
/// on a shared box that takes down everything else on it (dev2, 2026-11-09:
/// four worker containers, each a default disk provider, filled `/` during
/// one large publish; Postgres and Redis on the same disk crashed).
pub const DEFAULT_MIN_FREE_BYTES: u64 = 20 * 1024 * 1024 * 1024;

/// How long a free-space reading is trusted before the disk is asked again.
const FREE_SPACE_TTL: Duration = Duration::from_secs(5);

/// (free, total) bytes of the filesystem holding a path.
type FreeSpaceProbe = Arc<dyn Fn(&Path) -> (u64, u64) + Send + Sync>;

#[derive(Clone)]
pub struct ChunkStore {
    root: PathBuf,
    /// Writes that would leave less than this free are refused. 0 = no
    /// floor; `None` = the default (20 GiB or a tenth of the disk).
    min_free: Option<u64>,
    probe: FreeSpaceProbe,
    /// Last reading: when, and bytes free then minus what was written since.
    free_cache: Arc<Mutex<Option<(Instant, u64, u64)>>>,
}

impl std::fmt::Debug for ChunkStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChunkStore")
            .field("root", &self.root)
            .field("min_free", &self.min_free)
            .finish()
    }
}

/// (free, total) bytes of the filesystem holding `root` (the deepest mount
/// containing it, after resolving symlinks). `(u64::MAX, u64::MAX)` when no
/// disk can be found, so a platform sysinfo cannot read never blocks writes.
pub fn free_space_at(root: &Path) -> (u64, u64) {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let mut best: Option<(usize, u64, u64)> = None;
    for disk in disks.list() {
        let mount = disk.mount_point();
        if root.starts_with(mount) {
            let depth = mount.components().count();
            if best.is_none_or(|(d, _, _)| depth > d) {
                best = Some((depth, disk.available_space(), disk.total_space()));
            }
        }
    }
    best.map(|(_, free, total)| (free, total))
        .unwrap_or((u64::MAX, u64::MAX))
}

fn min_free_from_env() -> Option<u64> {
    std::env::var("C0MPUTE_STORAGE_MIN_FREE_GB")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|gb| gb.saturating_mul(1024 * 1024 * 1024))
}

impl ChunkStore {
    pub async fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(root.join("shards"))
            .await
            .with_context(|| format!("create_dir_all {}", root.display()))?;
        Ok(Self {
            root,
            min_free: min_free_from_env(),
            probe: Arc::new(free_space_at),
            free_cache: Arc::new(Mutex::new(None)),
        })
    }

    /// Set the free-disk floor in bytes (0 = none).
    pub fn with_min_free_bytes(mut self, bytes: u64) -> Self {
        self.min_free = Some(bytes);
        self
    }

    /// Replace how free space is measured (tests).
    pub fn with_free_space_probe(
        mut self,
        probe: impl Fn(&Path) -> (u64, u64) + Send + Sync + 'static,
    ) -> Self {
        self.probe = Arc::new(probe);
        *self.free_cache.lock().unwrap() = None;
        self
    }

    /// The floor in force for a disk of `total` bytes.
    fn floor_for(&self, total: u64) -> u64 {
        self.min_free
            .unwrap_or_else(|| DEFAULT_MIN_FREE_BYTES.min(total / 10))
    }

    /// Refuse a write of `incoming` bytes that would take the disk under the
    /// floor. The reading is cached for a few seconds and decremented by each
    /// accepted write, so a burst of shards cannot slip past a stale number.
    fn reserve(&self, incoming: u64) -> Result<()> {
        if self.min_free == Some(0) {
            return Ok(());
        }
        let mut cache = self.free_cache.lock().unwrap();
        let (at, free, total) = match *cache {
            Some((at, free, total)) if at.elapsed() < FREE_SPACE_TTL => (at, free, total),
            _ => {
                let (free, total) = (self.probe)(&self.root);
                (Instant::now(), free, total)
            }
        };
        let floor = self.floor_for(total);
        if free < floor.saturating_add(incoming) {
            *cache = Some((at, free, total));
            bail!(
                "storage full: {} GiB free under {}, keeping {} GiB free (C0MPUTE_STORAGE_MIN_FREE_GB)",
                free / (1024 * 1024 * 1024),
                self.root.display(),
                floor / (1024 * 1024 * 1024)
            );
        }
        *cache = Some((at, free - incoming, total));
        Ok(())
    }

    fn chunk_path(&self, hash: &Hash) -> PathBuf {
        let hex = hash.to_hex();
        self.root
            .join("shards")
            .join(&hex[0..2])
            .join(&hex[2..4])
            .join(&hex)
    }

    /// Write bytes, reporting whether this call *created* the chunk (`true`)
    /// or found it already present (`false`).
    ///
    /// Callers that may need to undo a partial write must use this rather than
    /// [`ChunkStore::put`]: chunks are content-addressed and therefore shared
    /// between objects, so rolling back a write by deleting every hash it
    /// touched will happily delete chunks a *different, intact* object still
    /// depends on. Deleting only what you created is the difference between
    /// discarding junk and destroying someone else's data.
    pub async fn put_new(&self, bytes: &[u8]) -> Result<(Hash, bool)> {
        let hash = Hash::of(bytes);
        if self.has(&hash).await {
            return Ok((hash, false));
        }
        self.put(bytes).await?;
        Ok((hash, true))
    }

    /// Write bytes; the returned hash is computed and is the storage key.
    #[instrument(skip(self, bytes))]
    pub async fn put(&self, bytes: &[u8]) -> Result<Hash> {
        let hash = Hash::of(bytes);
        let path = self.chunk_path(&hash);
        if fs::metadata(&path).await.is_ok() {
            return Ok(hash);
        }
        self.reserve(bytes.len() as u64)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        // Write atomically via temp file in the same dir to keep the rename
        // crossfs-safe.
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, bytes).await?;
        fs::rename(&tmp, &path).await?;
        debug!(hash = %hash, bytes = bytes.len(), "stored chunk");
        Ok(hash)
    }

    /// Move a file into the store without loading it into memory (a
    /// transcode output can be gigabytes). The file is hashed by streaming,
    /// then renamed into place (copied when the store is on another
    /// filesystem); `src` is consumed either way. The key is the same blake3
    /// hash [`ChunkStore::put`] gives the same bytes.
    pub async fn put_file(&self, src: &Path) -> Result<Hash> {
        let to_hash = src.to_path_buf();
        let hash = tokio::task::spawn_blocking(move || -> Result<Hash> {
            let mut file = std::fs::File::open(&to_hash)
                .with_context(|| format!("open {}", to_hash.display()))?;
            let mut hasher = blake3::Hasher::new();
            std::io::copy(&mut file, &mut hasher)
                .with_context(|| format!("hash {}", to_hash.display()))?;
            Ok(Hash(*hasher.finalize().as_bytes()))
        })
        .await
        .context("hash task panicked")??;
        if self.has(&hash).await {
            let _ = fs::remove_file(src).await;
            return Ok(hash);
        }
        let len = fs::metadata(src).await.map(|m| m.len()).unwrap_or(0);
        self.reserve(len)?;
        let path = self.chunk_path(&hash);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let tmp = path.with_extension("tmp");
        if fs::rename(src, &tmp).await.is_err() {
            fs::copy(src, &tmp)
                .await
                .with_context(|| format!("copy {} into the store", src.display()))?;
            let _ = fs::remove_file(src).await;
        }
        fs::rename(&tmp, &path).await?;
        debug!(hash = %hash, "stored file chunk");
        Ok(hash)
    }

    /// Read and verify the hash. Returns an error if the on-disk bytes don't
    /// match the requested hash — that means corruption or tampering.
    pub async fn get(&self, hash: &Hash) -> Result<Vec<u8>> {
        let bytes = self.read_unchecked(hash).await?;
        let actual = Hash::of(&bytes);
        if actual != *hash {
            bail!(
                "chunk integrity failure: requested {} but on-disk hashes to {}",
                hash,
                actual
            );
        }
        Ok(bytes)
    }

    pub async fn read_unchecked(&self, hash: &Hash) -> Result<Vec<u8>> {
        let path = self.chunk_path(hash);
        fs::read(&path)
            .await
            .with_context(|| format!("read chunk {}", hash))
    }

    pub async fn has(&self, hash: &Hash) -> bool {
        fs::metadata(self.chunk_path(hash)).await.is_ok()
    }

    pub async fn delete(&self, hash: &Hash) -> Result<()> {
        let path = self.chunk_path(hash);
        if fs::metadata(&path).await.is_ok() {
            fs::remove_file(&path).await?;
        }
        Ok(())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn put_get_roundtrip() {
        let dir = tempdir();
        let store = ChunkStore::open(&dir).await.unwrap();
        let h = store.put(b"hello world").await.unwrap();
        assert!(store.has(&h).await);
        let bytes = store.get(&h).await.unwrap();
        assert_eq!(bytes, b"hello world");
    }

    #[tokio::test]
    async fn refuses_writes_that_would_cross_the_free_disk_floor() {
        let dir = tempdir();
        let store = ChunkStore::open(&dir)
            .await
            .unwrap()
            .with_min_free_bytes(1_000)
            .with_free_space_probe(|_| (1_005, 1_000_000));
        // 5 bytes fit above the floor...
        store.put(b"12345").await.unwrap();
        // ...the next 5 do not: the cached reading was decremented.
        let err = store.put(b"67890").await.unwrap_err();
        assert!(err.to_string().contains("storage full"), "{err}");
        // A chunk already on disk is not a new write and still succeeds.
        store.put(b"12345").await.unwrap();
    }

    #[tokio::test]
    async fn no_floor_when_set_to_zero() {
        let dir = tempdir();
        let store = ChunkStore::open(&dir)
            .await
            .unwrap()
            .with_min_free_bytes(0)
            .with_free_space_probe(|_| (0, 1_000_000));
        store.put(b"hello").await.unwrap();
    }

    #[tokio::test]
    async fn default_floor_is_a_tenth_of_a_small_disk() {
        let dir = tempdir();
        // 32 GB disk: the floor is 3.2 GB, not 20 GiB.
        let store = ChunkStore::open(&dir)
            .await
            .unwrap()
            .with_free_space_probe(|_| (4_000_000_000, 32_000_000_000));
        let store = ChunkStore { min_free: None, ..store };
        store.put(b"fits").await.unwrap();
        let store = store.with_free_space_probe(|_| (3_000_000_000, 32_000_000_000));
        assert!(store.put(b"no room").await.is_err());
    }

    #[tokio::test]
    async fn get_detects_corruption() {
        let dir = tempdir();
        let store = ChunkStore::open(&dir).await.unwrap();
        let h = store.put(b"hello world").await.unwrap();

        // Tamper with the file directly.
        let path = store.chunk_path(&h);
        tokio::fs::write(&path, b"goodbye world").await.unwrap();

        let err = store.get(&h).await.unwrap_err();
        assert!(err.to_string().contains("integrity"));
    }

    #[tokio::test]
    async fn put_file_moves_the_file_under_the_same_key_as_put() {
        let dir = tempdir();
        let store = ChunkStore::open(dir.join("store")).await.unwrap();
        let src = dir.join("output.mp4");
        let body: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        tokio::fs::write(&src, &body).await.unwrap();

        let h = store.put_file(&src).await.unwrap();
        assert_eq!(h, Hash::of(&body), "same key as put()");
        assert_eq!(store.get(&h).await.unwrap(), body);
        assert!(!src.exists(), "source consumed");

        // Storing the same bytes again is a no-op that still consumes src.
        tokio::fs::write(&src, &body).await.unwrap();
        assert_eq!(store.put_file(&src).await.unwrap(), h);
        assert!(!src.exists());
    }

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("c0mpute-store-test-{}", uuid_like_suffix()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn uuid_like_suffix() -> String {
        // Tests don't need real UUIDs; nanos are unique enough and avoid a
        // dev-dependency.
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            .to_string()
    }
}
