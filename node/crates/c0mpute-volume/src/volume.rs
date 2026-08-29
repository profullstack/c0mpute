//! Volumes and the root pointer (CIP-004).
//!
//! This resolves the problem DIP-0012 flagged and left open:
//!
//! > A manifest = a small JSON saying "these 14 shards on these 14 hosts make
//! > object X." If the manifest is lost, the data is unrecoverable even though
//! > shards exist.
//!
//! Everything below the root is immutable and content-addressed, so it
//! inherits CIP-003's placement durability for free. **Only the root is
//! mutable, and it is a signed 32-byte pointer.** Concentrating all mutability
//! into one tiny value is what makes the rest of the design tractable — and it
//! is why CIP-007 can build a read/write filesystem on top of an immutable
//! store without the store becoming mutable.

use anyhow::{Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use c0mpute_proto::Hash;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::anchor::RootAnchor;
use crate::hamt::Hamt;
use crate::store::ObjectSink;

/// Ancestors retained for undo and for GC reachability.
pub const DEFAULT_RETAINED_ROOTS: usize = 32;

/// The one mutable value in the system.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RootPointer {
    pub volume: String,
    /// Increments by exactly one per update. A reader seeing a gap knows it is
    /// missing history; a writer seeing its expected sequence already taken
    /// knows it lost a race (CIP-010).
    pub sequence: u64,
    /// Hash of the snapshot this root names.
    pub snapshot: Option<Hash>,
    pub parent: Option<Hash>,
    pub written_at_ms: u64,
    pub writer_did: String,
    /// base64url ed25519 over [`RootPointer::signing_bytes`].
    pub signature: String,
}

impl RootPointer {
    /// Exactly what is signed. Deliberately excludes `signature` itself, and
    /// includes the volume id so a root cannot be replayed onto another
    /// volume.
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"c0mpute-root-v1\n");
        out.extend_from_slice(self.volume.as_bytes());
        out.push(b'\n');
        out.extend_from_slice(&self.sequence.to_be_bytes());
        out.extend_from_slice(self.snapshot.map(|h| h.0).unwrap_or([0u8; 32]).as_slice());
        out.extend_from_slice(self.parent.map(|h| h.0).unwrap_or([0u8; 32]).as_slice());
        out.extend_from_slice(&self.written_at_ms.to_be_bytes());
        out.extend_from_slice(self.writer_did.as_bytes());
        out
    }

    pub fn sign(&mut self, key: &SigningKey) {
        let sig = key.sign(&self.signing_bytes());
        self.signature = URL_SAFE_NO_PAD.encode(sig.to_bytes());
    }

    pub fn verify(&self, key: &VerifyingKey) -> Result<()> {
        let raw = URL_SAFE_NO_PAD.decode(&self.signature)?;
        let arr: [u8; 64] = raw
            .try_into()
            .map_err(|_| anyhow::anyhow!("signature is not 64 bytes"))?;
        key.verify(&self.signing_bytes(), &Signature::from_bytes(&arr))
            .map_err(|_| anyhow::anyhow!("root signature does not verify"))?;
        Ok(())
    }

    /// The root's own content hash, used to chain ancestors.
    pub fn hash(&self) -> Hash {
        Hash::of(&serde_json::to_vec(self).expect("root serialises"))
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A named, mutable dataset.
pub struct Volume<S: ObjectSink, A: RootAnchor> {
    id: String,
    store: S,
    anchor: A,
    key: SigningKey,
    did: String,
    root: RootPointer,
    index: Hamt,
    retained: usize,
}

impl<S: ObjectSink, A: RootAnchor> Volume<S, A> {
    /// Create a volume that does not exist yet.
    pub async fn create(
        id: impl Into<String>,
        store: S,
        anchor: A,
        key: SigningKey,
        did: impl Into<String>,
    ) -> Result<Self> {
        let id = id.into();
        let did = did.into();
        if anchor.read(&id).await?.is_some() {
            bail!("volume {id} already exists");
        }
        let mut root = RootPointer {
            volume: id.clone(),
            sequence: 0,
            snapshot: None,
            parent: None,
            written_at_ms: now_ms(),
            writer_did: did.clone(),
            signature: String::new(),
        };
        root.sign(&key);
        anchor.write(&id, &root, None).await?;

        Ok(Self {
            id,
            store,
            anchor,
            key,
            did,
            root,
            index: Hamt::new(),
            retained: DEFAULT_RETAINED_ROOTS,
        })
    }

    /// Open an existing volume from its anchored root.
    ///
    /// This is the recovery path too: given only the DID key and the volume
    /// id, everything else is reachable. Losing every client machine costs a
    /// re-sync, not the data.
    pub async fn open(
        id: impl Into<String>,
        store: S,
        anchor: A,
        key: SigningKey,
        did: impl Into<String>,
    ) -> Result<Self> {
        let id = id.into();
        let did = did.into();
        let root = anchor
            .read(&id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no volume {id}"))?;
        root.verify(&key.verifying_key())?;

        let index = match root.snapshot {
            None => Hamt::new(),
            Some(h) => {
                let bytes = store.get_object(&h).await?;
                serde_json::from_slice(&bytes)?
            }
        };
        Ok(Self {
            id,
            store,
            anchor,
            key,
            did,
            root,
            index,
            retained: DEFAULT_RETAINED_ROOTS,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn root(&self) -> &RootPointer {
        &self.root
    }

    pub fn sequence(&self) -> u64 {
        self.root.sequence
    }

    pub fn len(&self) -> u64 {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    pub fn with_retained_roots(mut self, n: usize) -> Self {
        self.retained = n;
        self
    }

    pub async fn get(&self, name: &str) -> Result<Option<Hash>> {
        self.index.get(&self.store, name).await
    }

    pub async fn list(&self) -> Result<std::collections::BTreeMap<String, Hash>> {
        self.index.entries(&self.store).await
    }

    /// Bind a name to an object hash and advance the root.
    pub async fn put(&mut self, name: &str, object: Hash) -> Result<()> {
        let index = self.index.insert(&self.store, name, object).await?;
        self.commit(index).await
    }

    pub async fn remove(&mut self, name: &str) -> Result<()> {
        let index = self.index.remove(&self.store, name).await?;
        self.commit(index).await
    }

    /// Write the new snapshot, then advance the root — in that order.
    ///
    /// A crash between the two leaves the previous root valid and some
    /// unreferenced garbage, which is the correct failure mode. The reverse
    /// order would leave a root naming a snapshot that was never written,
    /// which is data loss.
    async fn commit(&mut self, index: Hamt) -> Result<()> {
        let snapshot = self
            .store
            .put_object(&serde_json::to_vec(&index)?)
            .await?;

        let mut next = RootPointer {
            volume: self.id.clone(),
            sequence: self.root.sequence + 1,
            snapshot: Some(snapshot),
            parent: Some(self.root.hash()),
            written_at_ms: now_ms(),
            writer_did: self.did.clone(),
            signature: String::new(),
        };
        next.sign(&self.key);

        // Keep the ancestor readable so history can be walked for undo and for
        // GC reachability.
        self.store
            .put_object(&serde_json::to_vec(&self.root)?)
            .await?;

        // Compare-and-set on the sequence we believe we hold. A concurrent
        // writer that advanced it first makes this fail rather than silently
        // clobbering their update (CIP-010 turns this into a lease).
        self.anchor
            .write(&self.id, &next, Some(self.root.sequence))
            .await?;

        self.root = next;
        self.index = index;
        Ok(())
    }

    /// Walk back through retained ancestors, newest first.
    pub async fn history(&self) -> Result<Vec<RootPointer>> {
        let mut out = vec![self.root.clone()];
        let mut cursor = self.root.parent;
        while let Some(h) = cursor {
            if out.len() >= self.retained {
                break;
            }
            let Ok(bytes) = self.store.get_object(&h).await else {
                break;
            };
            let Ok(root) = serde_json::from_slice::<RootPointer>(&bytes) else {
                break;
            };
            cursor = root.parent;
            out.push(root);
        }
        Ok(out)
    }

    /// Restore the volume to an earlier sequence.
    ///
    /// Implemented as a *new* root naming the old snapshot rather than by
    /// rewinding the sequence: history stays append-only, so a rollback is
    /// itself undoable and no reader ever sees the sequence go backwards.
    pub async fn rollback(&mut self, to_sequence: u64) -> Result<()> {
        let target = self
            .history()
            .await?
            .into_iter()
            .find(|r| r.sequence == to_sequence)
            .ok_or_else(|| {
                anyhow::anyhow!("sequence {to_sequence} is not in the retained history")
            })?;

        let index = match target.snapshot {
            None => Hamt::new(),
            Some(h) => serde_json::from_slice(&self.store.get_object(&h).await?)?,
        };
        self.commit(index).await
    }

    /// Every hash reachable from the retained roots.
    ///
    /// The keep-set for GC: anything outside it, past the grace period, is
    /// garbage. Computed from the client rather than tracked by refcount,
    /// because content-addressed dedup across volumes would make refcounts
    /// need global coordination — which DIP-0011 rules out.
    pub async fn keep_set(&self) -> Result<std::collections::HashSet<Hash>> {
        let mut keep = std::collections::HashSet::new();
        for root in self.history().await? {
            keep.insert(root.hash());
            let Some(snapshot_hash) = root.snapshot else {
                continue;
            };
            keep.insert(snapshot_hash);
            let Ok(bytes) = self.store.get_object(&snapshot_hash).await else {
                continue;
            };
            let Ok(index) = serde_json::from_slice::<Hamt>(&bytes) else {
                continue;
            };
            for node in index.node_hashes(&self.store).await? {
                keep.insert(node);
            }
            for object in index.entries(&self.store).await?.into_values() {
                keep.insert(object);
            }
        }
        Ok(keep)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anchor::MemoryAnchor;
    use crate::store::MemorySink;

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[3u8; 32])
    }

    fn h(n: u64) -> Hash {
        Hash::of(&n.to_be_bytes())
    }

    async fn vol() -> Volume<MemorySink, MemoryAnchor> {
        Volume::create(
            "vol_test",
            MemorySink::new(),
            MemoryAnchor::new(),
            key(),
            "did:coinpay:test",
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn a_new_volume_is_empty_at_sequence_zero() {
        let v = vol().await;
        assert_eq!(v.sequence(), 0);
        assert!(v.is_empty());
        assert_eq!(v.root().snapshot, None);
    }

    #[tokio::test]
    async fn sequence_increases_by_exactly_one_per_write() {
        let mut v = vol().await;
        for i in 1..=50u64 {
            v.put(&format!("f{i}"), h(i)).await.unwrap();
            assert_eq!(v.sequence(), i, "a gap means lost history");
        }
        assert_eq!(v.len(), 50);
    }

    #[tokio::test]
    async fn roots_are_signed_and_bound_to_their_volume() {
        let v = vol().await;
        v.root().verify(&key().verifying_key()).unwrap();

        // A different key does not verify.
        let other = SigningKey::from_bytes(&[9u8; 32]);
        assert!(v.root().verify(&other.verifying_key()).is_err());

        // Nor does the same root replayed onto another volume.
        let mut stolen = v.root().clone();
        stolen.volume = "vol_someone_else".into();
        assert!(stolen.verify(&key().verifying_key()).is_err());
    }

    #[tokio::test]
    async fn tampering_with_the_snapshot_invalidates_the_root() {
        let mut v = vol().await;
        v.put("a", h(1)).await.unwrap();
        let mut tampered = v.root().clone();
        tampered.snapshot = Some(h(42));
        assert!(tampered.verify(&key().verifying_key()).is_err());
    }

    /// The recovery story: with only the DID key and the volume id, everything
    /// comes back. Losing every client costs a re-sync, not the data.
    #[tokio::test]
    async fn recovers_from_the_key_and_volume_id_alone() {
        let store = MemorySink::new();
        let anchor = MemoryAnchor::new();
        {
            let mut v = Volume::create(
                "vol_r",
                store.clone(),
                anchor.clone(),
                key(),
                "did:coinpay:test",
            )
            .await
            .unwrap();
            for i in 0..100u64 {
                v.put(&format!("file{i}"), h(i)).await.unwrap();
            }
        } // client is gone

        let recovered = Volume::open("vol_r", store, anchor, key(), "did:coinpay:test")
            .await
            .unwrap();
        assert_eq!(recovered.sequence(), 100);
        assert_eq!(recovered.len(), 100);
        for i in 0..100u64 {
            assert_eq!(recovered.get(&format!("file{i}")).await.unwrap(), Some(h(i)));
        }
    }

    #[tokio::test]
    async fn opening_with_the_wrong_key_is_refused() {
        let store = MemorySink::new();
        let anchor = MemoryAnchor::new();
        Volume::create("v", store.clone(), anchor.clone(), key(), "did:x")
            .await
            .unwrap();
        let wrong = SigningKey::from_bytes(&[7u8; 32]);
        assert!(Volume::open("v", store, anchor, wrong, "did:x").await.is_err());
    }

    #[tokio::test]
    async fn history_walks_back_through_ancestors() {
        let mut v = vol().await;
        for i in 1..=10u64 {
            v.put(&format!("f{i}"), h(i)).await.unwrap();
        }
        let hist = v.history().await.unwrap();
        assert_eq!(hist[0].sequence, 10);
        // Newest first, strictly decreasing.
        for w in hist.windows(2) {
            assert!(w[0].sequence > w[1].sequence);
        }
    }

    /// Rollback is a new root naming an old snapshot, not a rewind. History
    /// stays append-only, so the rollback is itself undoable.
    #[tokio::test]
    async fn rollback_moves_forward_to_an_older_state() {
        let mut v = vol().await;
        v.put("a", h(1)).await.unwrap(); // seq 1
        v.put("b", h(2)).await.unwrap(); // seq 2
        v.put("c", h(3)).await.unwrap(); // seq 3
        assert_eq!(v.len(), 3);

        v.rollback(1).await.unwrap();
        assert_eq!(v.len(), 1, "should hold only what seq 1 held");
        assert_eq!(v.get("a").await.unwrap(), Some(h(1)));
        assert_eq!(v.get("c").await.unwrap(), None);
        assert_eq!(v.sequence(), 4, "rollback advances, never rewinds");
    }

    #[tokio::test]
    async fn rollback_past_the_retained_window_is_refused() {
        let mut v = vol().await.with_retained_roots(3);
        for i in 1..=10u64 {
            v.put(&format!("f{i}"), h(i)).await.unwrap();
        }
        assert!(v.rollback(1).await.is_err());
    }

    #[tokio::test]
    async fn removing_a_name_advances_the_root() {
        let mut v = vol().await;
        v.put("a", h(1)).await.unwrap();
        v.put("b", h(2)).await.unwrap();
        v.remove("a").await.unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v.get("a").await.unwrap(), None);
        assert_eq!(v.sequence(), 3);
    }

    #[tokio::test]
    async fn keep_set_covers_everything_reachable() {
        let mut v = vol().await;
        for i in 0..40u64 {
            v.put(&format!("f{i}"), h(i)).await.unwrap();
        }
        let keep = v.keep_set().await.unwrap();

        // Every named object, the snapshot, and the root itself.
        for i in 0..40u64 {
            assert!(keep.contains(&h(i)), "object {i} not in the keep set");
        }
        assert!(keep.contains(&v.root().snapshot.unwrap()));
        assert!(keep.contains(&v.root().hash()));
    }

    #[tokio::test]
    async fn creating_a_volume_twice_is_refused() {
        let store = MemorySink::new();
        let anchor = MemoryAnchor::new();
        Volume::create("dup", store.clone(), anchor.clone(), key(), "did:x")
            .await
            .unwrap();
        assert!(
            Volume::create("dup", store, anchor, key(), "did:x")
                .await
                .is_err()
        );
    }
}
