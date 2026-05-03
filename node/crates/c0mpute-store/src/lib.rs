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

pub use storage::{Storage, ObjectManifest, ShardEntry};

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use c0mpute_proto::Hash;
use tokio::fs;
use tracing::{debug, instrument};

#[derive(Clone, Debug)]
pub struct ChunkStore {
    root: PathBuf,
}

impl ChunkStore {
    pub async fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(root.join("shards"))
            .await
            .with_context(|| format!("create_dir_all {}", root.display()))?;
        Ok(Self { root })
    }

    fn chunk_path(&self, hash: &Hash) -> PathBuf {
        let hex = hash.to_hex();
        self.root
            .join("shards")
            .join(&hex[0..2])
            .join(&hex[2..4])
            .join(&hex)
    }

    /// Write bytes; the returned hash is computed and is the storage key.
    #[instrument(skip(self, bytes))]
    pub async fn put(&self, bytes: &[u8]) -> Result<Hash> {
        let hash = Hash::of(bytes);
        let path = self.chunk_path(&hash);
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

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "c0mpute-store-test-{}",
            uuid_like_suffix()
        ));
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
