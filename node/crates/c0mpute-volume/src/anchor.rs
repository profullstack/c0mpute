//! Where the root pointer durably lives (CIP-004).
//!
//! The root is the one thing whose loss is unrecoverable, so CIP-004 gives it
//! three homes: the CoinPay registry (authoritative), gossip (fast, not
//! authoritative), and the writing client's local journal (enough to recover
//! alone).
//!
//! CoinPay integration arrives with CIP-006, which is where DIDs and the
//! registry land. Until then this trait carries the *shape* — in particular
//! the compare-and-set, which is what makes concurrent writers fail loudly
//! instead of silently clobbering each other — with a local-file
//! implementation behind it. Swapping in the registry is then one impl, not a
//! redesign.

use anyhow::{Result, bail};
use async_trait::async_trait;

use crate::volume::RootPointer;

#[async_trait]
pub trait RootAnchor: Send + Sync {
    async fn read(&self, volume: &str) -> Result<Option<RootPointer>>;

    /// Advance the root, but only if the current sequence is `expected`.
    ///
    /// `None` means "must not exist yet". The compare-and-set is the whole
    /// point: a writer that lost a race finds its expected sequence already
    /// taken and errors, rather than overwriting the winner's update.
    async fn write(
        &self,
        volume: &str,
        root: &RootPointer,
        expected_sequence: Option<u64>,
    ) -> Result<()>;

    async fn list(&self) -> Result<Vec<String>>;
}

/// Root pointers as files under a directory.
///
/// Single-node and single-writer, which is what CIP-004 assumes; CIP-010 adds
/// the lease that makes the assumption enforceable across machines.
pub struct FileAnchor {
    dir: std::path::PathBuf,
}

impl FileAnchor {
    pub fn new(dir: impl Into<std::path::PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn path(&self, volume: &str) -> std::path::PathBuf {
        self.dir.join(format!("{volume}.root.json"))
    }
}

#[async_trait]
impl RootAnchor for FileAnchor {
    async fn read(&self, volume: &str) -> Result<Option<RootPointer>> {
        let path = self.path(volume);
        match tokio::fs::read(&path).await {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn write(
        &self,
        volume: &str,
        root: &RootPointer,
        expected_sequence: Option<u64>,
    ) -> Result<()> {
        let current = self.read(volume).await?;
        match (&current, expected_sequence) {
            (Some(c), Some(expected)) if c.sequence != expected => bail!(
                "root for {volume} moved under us: expected sequence {expected}, found {}",
                c.sequence
            ),
            (Some(_), None) => bail!("volume {volume} already exists"),
            (None, Some(expected)) => {
                bail!("volume {volume} has no root, but sequence {expected} was expected")
            }
            _ => {}
        }

        tokio::fs::create_dir_all(&self.dir).await?;
        let path = self.path(volume);
        // Write-then-rename: a half-written root is a lost volume.
        let tmp = path.with_extension("tmp");
        tokio::fs::write(&tmp, serde_json::to_vec_pretty(root)?).await?;
        tokio::fs::rename(&tmp, &path).await?;
        Ok(())
    }

    async fn list(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut rd = match tokio::fs::read_dir(&self.dir).await {
            Ok(rd) => rd,
            Err(_) => return Ok(out),
        };
        while let Some(entry) = rd.next_entry().await? {
            if let Some(name) = entry.file_name().to_str()
                && let Some(vol) = name.strip_suffix(".root.json")
            {
                out.push(vol.to_string());
            }
        }
        out.sort();
        Ok(out)
    }
}

#[cfg(any(test, feature = "testing"))]
pub use memory::MemoryAnchor;

#[cfg(any(test, feature = "testing"))]
mod memory {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Default)]
    pub struct MemoryAnchor {
        roots: Arc<Mutex<HashMap<String, RootPointer>>>,
    }

    impl MemoryAnchor {
        pub fn new() -> Self {
            Self::default()
        }
    }

    #[async_trait]
    impl RootAnchor for MemoryAnchor {
        async fn read(&self, volume: &str) -> Result<Option<RootPointer>> {
            Ok(self.roots.lock().unwrap().get(volume).cloned())
        }

        async fn write(
            &self,
            volume: &str,
            root: &RootPointer,
            expected_sequence: Option<u64>,
        ) -> Result<()> {
            let mut roots = self.roots.lock().unwrap();
            match (roots.get(volume), expected_sequence) {
                (Some(c), Some(expected)) if c.sequence != expected => bail!(
                    "root for {volume} moved under us: expected sequence {expected}, found {}",
                    c.sequence
                ),
                (Some(_), None) => bail!("volume {volume} already exists"),
                (None, Some(expected)) => {
                    bail!("volume {volume} has no root, but sequence {expected} was expected")
                }
                _ => {}
            }
            roots.insert(volume.to_string(), root.clone());
            Ok(())
        }

        async fn list(&self) -> Result<Vec<String>> {
            let mut out: Vec<String> = self.roots.lock().unwrap().keys().cloned().collect();
            out.sort();
            Ok(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::volume::RootPointer;

    fn root(volume: &str, sequence: u64) -> RootPointer {
        RootPointer {
            volume: volume.into(),
            sequence,
            snapshot: None,
            parent: None,
            written_at_ms: 0,
            writer_did: "did:x".into(),
            signature: String::new(),
        }
    }

    fn tmpdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "c0mpute-anchor-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    async fn check_anchor<A: RootAnchor>(a: A) {
        assert_eq!(a.read("v").await.unwrap(), None);

        a.write("v", &root("v", 0), None).await.unwrap();
        assert_eq!(a.read("v").await.unwrap().unwrap().sequence, 0);

        // Creating twice is refused.
        assert!(a.write("v", &root("v", 0), None).await.is_err());

        a.write("v", &root("v", 1), Some(0)).await.unwrap();
        assert_eq!(a.read("v").await.unwrap().unwrap().sequence, 1);

        // The compare-and-set that stops a loser silently clobbering a winner.
        let err = a.write("v", &root("v", 2), Some(0)).await.unwrap_err();
        assert!(err.to_string().contains("moved under us"), "{err}");

        assert_eq!(a.list().await.unwrap(), vec!["v".to_string()]);
    }

    #[tokio::test]
    async fn memory_anchor_honours_compare_and_set() {
        check_anchor(MemoryAnchor::new()).await;
    }

    #[tokio::test]
    async fn file_anchor_honours_compare_and_set() {
        check_anchor(FileAnchor::new(tmpdir())).await;
    }

    #[tokio::test]
    async fn file_anchor_survives_a_reopen() {
        let dir = tmpdir();
        {
            let a = FileAnchor::new(&dir);
            a.write("v", &root("v", 0), None).await.unwrap();
            a.write("v", &root("v", 1), Some(0)).await.unwrap();
        }
        let reopened = FileAnchor::new(&dir);
        assert_eq!(reopened.read("v").await.unwrap().unwrap().sequence, 1);
    }

    #[tokio::test]
    async fn listing_an_empty_or_missing_dir_is_not_an_error() {
        let a = FileAnchor::new(std::env::temp_dir().join("c0mpute-does-not-exist-xyz"));
        assert!(a.list().await.unwrap().is_empty());
    }
}
