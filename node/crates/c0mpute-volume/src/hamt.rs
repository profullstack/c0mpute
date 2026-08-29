//! Content-addressed HAMT (CIP-004).
//!
//! A snapshot has to be cheap to **update**, not merely to read. Serialising a
//! volume's index as one document would mean rewriting every entry to change
//! one — a million-entry volume paying a million-entry write per file created.
//!
//! A hash array mapped trie rewrites only the ~log₃₂(N) nodes on the path to
//! the changed key and shares every other node with the previous version,
//! because nodes are addressed by their content: an unchanged subtree has an
//! unchanged hash, so the new root simply points at the old node. That is what
//! makes CIP-004's "all mutability lives in one 32-byte pointer" affordable,
//! and it makes point-in-time snapshots free as a side effect — the old root
//! still names a complete, immutable tree.
//!
//! 32-way branching: five bits of the key's blake3 per level, so a million
//! entries sit about four levels deep.

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use c0mpute_proto::Hash;
use serde::{Deserialize, Serialize};

use crate::store::ObjectSink;

/// Bits of hash consumed per level. 5 bits = 32-way branching.
const BITS: u32 = 5;
const WIDTH: usize = 1 << BITS; // 32
const MASK: u32 = (WIDTH as u32) - 1;
/// blake3 gives 256 bits, so 51 levels before the key is exhausted. Reaching
/// that means ~51 collisions of the same 255-bit prefix; the bucket below
/// handles it rather than pretending it cannot happen.
const MAX_DEPTH: u32 = 256 / BITS;

/// One node of the trie, stored as its own content-addressed object.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Node {
    /// Occupied slots are named by `bitmap`; `children` holds only those,
    /// densely. A sparse 32-slot array would make every node 32 hashes wide
    /// regardless of how few are used.
    Branch {
        bitmap: u32,
        children: Vec<Hash>,
    },
    /// Entries sharing a hash prefix to this depth. Usually one.
    Leaf { entries: Vec<(String, Hash)> },
}

impl Node {
    fn empty_branch() -> Self {
        Node::Branch {
            bitmap: 0,
            children: Vec::new(),
        }
    }
}

fn key_hash(key: &str) -> [u8; 32] {
    *blake3::hash(key.as_bytes()).as_bytes()
}

/// The 5-bit slot index for `key` at `depth`.
fn slot(hash: &[u8; 32], depth: u32) -> u32 {
    let bit_offset = depth * BITS;
    let byte = (bit_offset / 8) as usize;
    if byte >= 32 {
        return 0;
    }
    // Read 16 bits so a slot straddling a byte boundary still resolves.
    let lo = hash[byte] as u32;
    let hi = if byte + 1 < 32 { hash[byte + 1] as u32 } else { 0 };
    let window = (lo << 8) | hi;
    let shift = 16 - BITS - (bit_offset % 8);
    (window >> shift) & MASK
}

fn dense_index(bitmap: u32, slot: u32) -> usize {
    (bitmap & ((1u32 << slot) - 1)).count_ones() as usize
}

/// A persistent map from string keys to hashes, rooted at one node hash.
///
/// `None` is the empty map; it costs no storage.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hamt {
    pub root: Option<Hash>,
    pub len: u64,
}

impl Hamt {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub async fn get<S: ObjectSink>(&self, store: &S, key: &str) -> Result<Option<Hash>> {
        let Some(root) = self.root else {
            return Ok(None);
        };
        let kh = key_hash(key);
        let mut current = root;
        let mut depth = 0u32;

        loop {
            match load(store, &current).await? {
                Node::Leaf { entries } => {
                    return Ok(entries.iter().find(|(k, _)| k == key).map(|(_, v)| *v));
                }
                Node::Branch { bitmap, children } => {
                    let s = slot(&kh, depth);
                    if bitmap & (1 << s) == 0 {
                        return Ok(None);
                    }
                    current = children[dense_index(bitmap, s)];
                    depth += 1;
                    if depth > MAX_DEPTH {
                        bail!("HAMT deeper than the key has bits");
                    }
                }
            }
        }
    }

    /// Insert or replace, returning the new map. `self` is untouched — the old
    /// root still names the old tree, which is what makes snapshots free.
    pub async fn insert<S: ObjectSink>(
        &self,
        store: &S,
        key: &str,
        value: Hash,
    ) -> Result<Self> {
        let kh = key_hash(key);
        let existed = self.get(store, key).await?.is_some();
        let new_root = match self.root {
            None => {
                let leaf = Node::Leaf {
                    entries: vec![(key.to_string(), value)],
                };
                save(store, &leaf).await?
            }
            Some(root) => insert_at(store, root, &kh, key, value, 0).await?,
        };
        Ok(Self {
            root: Some(new_root),
            len: if existed { self.len } else { self.len + 1 },
        })
    }

    pub async fn remove<S: ObjectSink>(&self, store: &S, key: &str) -> Result<Self> {
        let Some(root) = self.root else {
            return Ok(self.clone());
        };
        if self.get(store, key).await?.is_none() {
            return Ok(self.clone());
        }
        let kh = key_hash(key);
        let new_root = remove_at(store, root, &kh, key, 0).await?;
        Ok(Self {
            root: new_root,
            len: self.len.saturating_sub(1),
        })
    }

    /// Every entry, in unspecified order.
    pub async fn entries<S: ObjectSink>(&self, store: &S) -> Result<BTreeMap<String, Hash>> {
        let mut out = BTreeMap::new();
        if let Some(root) = self.root {
            collect(store, &root, &mut out).await?;
        }
        Ok(out)
    }

    /// Every node hash the tree reaches, including the root.
    ///
    /// This is the reachability walk GC needs: anything not in this set, for
    /// any retained root, is garbage.
    pub async fn node_hashes<S: ObjectSink>(&self, store: &S) -> Result<Vec<Hash>> {
        let mut out = Vec::new();
        if let Some(root) = self.root {
            walk_nodes(store, &root, &mut out).await?;
        }
        Ok(out)
    }
}

async fn load<S: ObjectSink>(store: &S, hash: &Hash) -> Result<Node> {
    let bytes = store.get_object(hash).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

async fn save<S: ObjectSink>(store: &S, node: &Node) -> Result<Hash> {
    store.put_object(&serde_json::to_vec(node)?).await
}

/// Recursion through `Box::pin` because these are async and self-referential.
fn insert_at<'a, S: ObjectSink>(
    store: &'a S,
    node_hash: Hash,
    kh: &'a [u8; 32],
    key: &'a str,
    value: Hash,
    depth: u32,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Hash>> + Send + 'a>> {
    Box::pin(async move {
        if depth > MAX_DEPTH {
            bail!("HAMT deeper than the key has bits");
        }
        match load(store, &node_hash).await? {
            Node::Leaf { entries } => {
                // Replace in place when the key is already here.
                if let Some(pos) = entries.iter().position(|(k, _)| k == key) {
                    let mut next = entries.clone();
                    next[pos].1 = value;
                    return save(store, &Node::Leaf { entries: next }).await;
                }
                // A leaf holding a different key has to split into a branch,
                // unless the keys collide all the way down — then they share a
                // bucket, which is why `Leaf` holds a vector.
                if depth >= MAX_DEPTH {
                    let mut next = entries.clone();
                    next.push((key.to_string(), value));
                    next.sort_by(|a, b| a.0.cmp(&b.0));
                    return save(store, &Node::Leaf { entries: next }).await;
                }
                let mut branch = Node::empty_branch();
                let branch_hash = save(store, &branch).await?;
                let mut acc = branch_hash;
                for (k, v) in &entries {
                    acc = insert_at(store, acc, &key_hash(k), k, *v, depth).await?;
                }
                branch = load(store, &acc).await?;
                let _ = branch;
                insert_at(store, acc, kh, key, value, depth).await
            }
            Node::Branch { bitmap, children } => {
                let s = slot(kh, depth);
                let bit = 1u32 << s;
                let idx = dense_index(bitmap, s);

                if bitmap & bit == 0 {
                    let leaf = Node::Leaf {
                        entries: vec![(key.to_string(), value)],
                    };
                    let leaf_hash = save(store, &leaf).await?;
                    let mut next = children.clone();
                    next.insert(idx, leaf_hash);
                    return save(
                        store,
                        &Node::Branch {
                            bitmap: bitmap | bit,
                            children: next,
                        },
                    )
                    .await;
                }

                let child = children[idx];
                let new_child = insert_at(store, child, kh, key, value, depth + 1).await?;
                // Unchanged subtree, unchanged hash: nothing to write.
                if new_child == child {
                    return Ok(node_hash);
                }
                let mut next = children.clone();
                next[idx] = new_child;
                save(
                    store,
                    &Node::Branch {
                        bitmap,
                        children: next,
                    },
                )
                .await
            }
        }
    })
}

fn remove_at<'a, S: ObjectSink>(
    store: &'a S,
    node_hash: Hash,
    kh: &'a [u8; 32],
    key: &'a str,
    depth: u32,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Option<Hash>>> + Send + 'a>> {
    Box::pin(async move {
        match load(store, &node_hash).await? {
            Node::Leaf { entries } => {
                let next: Vec<_> = entries.into_iter().filter(|(k, _)| k != key).collect();
                if next.is_empty() {
                    // Collapse: an empty leaf is removed from its parent
                    // rather than stored, so the tree does not accumulate
                    // hollow nodes as entries come and go.
                    return Ok(None);
                }
                Ok(Some(save(store, &Node::Leaf { entries: next }).await?))
            }
            Node::Branch { bitmap, children } => {
                let s = slot(kh, depth);
                let bit = 1u32 << s;
                if bitmap & bit == 0 {
                    return Ok(Some(node_hash));
                }
                let idx = dense_index(bitmap, s);
                let new_child = remove_at(store, children[idx], kh, key, depth + 1).await?;

                let (bitmap, children) = match new_child {
                    Some(c) => {
                        let mut next = children.clone();
                        next[idx] = c;
                        (bitmap, next)
                    }
                    None => {
                        let mut next = children.clone();
                        next.remove(idx);
                        (bitmap & !bit, next)
                    }
                };
                if bitmap == 0 {
                    return Ok(None);
                }
                Ok(Some(
                    save(store, &Node::Branch { bitmap, children }).await?,
                ))
            }
        }
    })
}

fn collect<'a, S: ObjectSink>(
    store: &'a S,
    node_hash: &'a Hash,
    out: &'a mut BTreeMap<String, Hash>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(async move {
        match load(store, node_hash).await? {
            Node::Leaf { entries } => {
                for (k, v) in entries {
                    out.insert(k, v);
                }
            }
            Node::Branch { children, .. } => {
                for c in children {
                    collect(store, &c, out).await?;
                }
            }
        }
        Ok(())
    })
}

fn walk_nodes<'a, S: ObjectSink>(
    store: &'a S,
    node_hash: &'a Hash,
    out: &'a mut Vec<Hash>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(async move {
        out.push(*node_hash);
        if let Node::Branch { children, .. } = load(store, node_hash).await? {
            for c in children {
                walk_nodes(store, &c, out).await?;
            }
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemorySink;

    fn h(n: u64) -> Hash {
        Hash::of(&n.to_be_bytes())
    }

    #[tokio::test]
    async fn empty_map_costs_nothing() {
        let s = MemorySink::new();
        let m = Hamt::new();
        assert!(m.is_empty());
        assert_eq!(m.len(), 0);
        assert_eq!(m.get(&s, "anything").await.unwrap(), None);
        assert_eq!(s.object_count(), 0);
    }

    #[tokio::test]
    async fn insert_and_get_round_trip() {
        let s = MemorySink::new();
        let mut m = Hamt::new();
        for i in 0..200u64 {
            m = m.insert(&s, &format!("key{i}"), h(i)).await.unwrap();
        }
        assert_eq!(m.len(), 200);
        for i in 0..200u64 {
            assert_eq!(m.get(&s, &format!("key{i}")).await.unwrap(), Some(h(i)));
        }
        assert_eq!(m.get(&s, "missing").await.unwrap(), None);
    }

    #[tokio::test]
    async fn replacing_a_key_does_not_grow_the_map() {
        let s = MemorySink::new();
        let m = Hamt::new().insert(&s, "a", h(1)).await.unwrap();
        let m2 = m.insert(&s, "a", h(2)).await.unwrap();
        assert_eq!(m2.len(), 1);
        assert_eq!(m2.get(&s, "a").await.unwrap(), Some(h(2)));
    }

    /// The property the whole design rests on: changing one entry must rewrite
    /// only the path to it, not the tree. Without this a mutable root pointer
    /// is unaffordable.
    #[tokio::test]
    async fn one_change_writes_only_the_path() {
        let s = MemorySink::new();
        let mut m = Hamt::new();
        for i in 0..10_000u64 {
            m = m.insert(&s, &format!("key{i}"), h(i)).await.unwrap();
        }
        let before = s.object_count();

        let m2 = m.insert(&s, "key5000", h(999_999)).await.unwrap();
        let written = s.object_count() - before;

        assert!(
            written < 10,
            "changing one of 10k entries wrote {written} nodes; \
             structural sharing is not working"
        );
        assert_eq!(m2.get(&s, "key5000").await.unwrap(), Some(h(999_999)));
        // And the old map still reads — a free point-in-time snapshot.
        assert_eq!(m.get(&s, "key5000").await.unwrap(), Some(h(5000)));
    }

    #[tokio::test]
    async fn the_old_root_is_a_complete_snapshot() {
        let s = MemorySink::new();
        let mut v1 = Hamt::new();
        for i in 0..100u64 {
            v1 = v1.insert(&s, &format!("k{i}"), h(i)).await.unwrap();
        }
        let mut v2 = v1.clone();
        for i in 0..100u64 {
            v2 = v2.insert(&s, &format!("k{i}"), h(i + 1000)).await.unwrap();
        }
        for i in 0..100u64 {
            assert_eq!(v1.get(&s, &format!("k{i}")).await.unwrap(), Some(h(i)));
            assert_eq!(v2.get(&s, &format!("k{i}")).await.unwrap(), Some(h(i + 1000)));
        }
        assert_eq!(v1.len(), 100);
        assert_eq!(v2.len(), 100);
    }

    #[tokio::test]
    async fn remove_works_and_collapses_empty_nodes() {
        let s = MemorySink::new();
        let mut m = Hamt::new();
        for i in 0..50u64 {
            m = m.insert(&s, &format!("k{i}"), h(i)).await.unwrap();
        }
        for i in 0..50u64 {
            m = m.remove(&s, &format!("k{i}")).await.unwrap();
        }
        assert_eq!(m.len(), 0);
        assert!(m.is_empty(), "removing everything should leave the empty map");
    }

    #[tokio::test]
    async fn removing_an_absent_key_is_a_no_op() {
        let s = MemorySink::new();
        let m = Hamt::new().insert(&s, "a", h(1)).await.unwrap();
        let m2 = m.remove(&s, "nope").await.unwrap();
        assert_eq!(m, m2);
    }

    /// Differential test against a plain map. A trie with a subtle slot or
    /// bitmap bug passes small hand-written cases and fails here.
    #[tokio::test]
    async fn behaves_like_a_btreemap() {
        let s = MemorySink::new();
        let mut hamt = Hamt::new();
        let mut model: BTreeMap<String, Hash> = BTreeMap::new();

        // Deterministic pseudo-random operations.
        let mut state: u64 = 0x243f_6a88_85a3_08d3;
        for step in 0..3000u64 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let key = format!("k{}", state % 400);
            if state % 3 == 0 {
                hamt = hamt.remove(&s, &key).await.unwrap();
                model.remove(&key);
            } else {
                let v = h(step);
                hamt = hamt.insert(&s, &key, v).await.unwrap();
                model.insert(key, v);
            }
            assert_eq!(hamt.len(), model.len() as u64, "len diverged at step {step}");
        }
        assert_eq!(hamt.entries(&s).await.unwrap(), model);
        for (k, v) in &model {
            assert_eq!(hamt.get(&s, k).await.unwrap(), Some(*v));
        }
    }

    #[tokio::test]
    async fn entries_lists_everything() {
        let s = MemorySink::new();
        let mut m = Hamt::new();
        for i in 0..500u64 {
            m = m.insert(&s, &format!("k{i}"), h(i)).await.unwrap();
        }
        let all = m.entries(&s).await.unwrap();
        assert_eq!(all.len(), 500);
        assert_eq!(all["k42"], h(42));
    }

    #[tokio::test]
    async fn node_hashes_reaches_the_whole_tree() {
        let s = MemorySink::new();
        let mut m = Hamt::new();
        for i in 0..300u64 {
            m = m.insert(&s, &format!("k{i}"), h(i)).await.unwrap();
        }
        let nodes = m.node_hashes(&s).await.unwrap();
        assert!(!nodes.is_empty());
        assert!(nodes.contains(&m.root.unwrap()));
        // Every reachable node must be loadable, or GC would sweep live data.
        for n in &nodes {
            assert!(s.has_object(n), "unreachable node {n} in the walk");
        }
    }

    #[test]
    fn slots_are_five_bits_and_within_range() {
        for i in 0..200u64 {
            let kh = key_hash(&format!("key{i}"));
            for depth in 0..MAX_DEPTH {
                assert!(slot(&kh, depth) < WIDTH as u32);
            }
        }
    }

    #[test]
    fn dense_index_counts_preceding_slots() {
        assert_eq!(dense_index(0b0000, 3), 0);
        assert_eq!(dense_index(0b0001, 3), 1);
        assert_eq!(dense_index(0b0101, 3), 2);
        assert_eq!(dense_index(0b1111, 0), 0);
    }
}
