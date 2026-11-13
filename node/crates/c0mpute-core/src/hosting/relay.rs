//! A holder does not serve the peers that claim to hold the same site.
//!
//! A storage challenge ([`super::proof`]) cannot tell a peer that keeps its
//! copy from one that fetches the challenged chunk from a real holder and
//! passes it on. The cheapest place to fetch it is the real holders
//! themselves, so they refuse: a peer that announces the current version of a
//! site whole has every chunk of it and no reason to ask for one, and a node
//! that holds the site answers such a fetch as if the chunk were absent
//! ([`refuses`]). A relay is left with holders that do not run this rule and
//! with identities that announce nothing.
//!
//! Only whole claims are refused. A peer that announces another version, or
//! only some groups of a striped site, or nothing, is served as before: that
//! is every node still fetching (a copy counts, and is announced, only once
//! it is complete), so two nodes that each hold some groups always serve
//! each other the rest.

use std::collections::HashMap;
use std::sync::Arc;

use c0mpute_proto::Hash;

use super::replicas::ReplicaMap;

/// Chunks the index covers. Sites that do not fit are left out, and fetches
/// of their chunks are served to everyone as before.
pub const MAX_INDEXED_CHUNKS: usize = 131_072;

/// Which held sites use a chunk, for the chunks of the sites this node
/// holds. Kept up from the catalog, bounded by [`MAX_INDEXED_CHUNKS`].
#[derive(Default)]
pub struct ChunkIndex {
    chunks: HashMap<Hash, Vec<Arc<str>>>,
    /// Per indexed site: what was indexed (version and groups) and its
    /// chunks. A site too big for the index keeps its stamp and no chunks,
    /// so it is not listed again until it changes.
    sites: HashMap<String, (String, Vec<Hash>)>,
    indexed: usize,
}

impl ChunkIndex {
    /// What `site` was last indexed as (see [`Self::set`]).
    pub fn stamp(&self, site: &str) -> Option<&str> {
        self.sites.get(site).map(|(s, _)| s.as_str())
    }

    /// Chunks the index can still take.
    pub fn room(&self) -> usize {
        MAX_INDEXED_CHUNKS.saturating_sub(self.indexed)
    }

    /// Index `chunks` as the chunks of `site` held here, replacing what was
    /// indexed for it. `stamp` names the version and groups, so the caller
    /// can tell when to index again. Returns false (and indexes none of
    /// them) when they do not fit.
    pub fn set(&mut self, site: &str, stamp: &str, chunks: Vec<Hash>) -> bool {
        self.set_capped(site, stamp, chunks, MAX_INDEXED_CHUNKS)
    }

    /// [`Self::set`] with the cap given (tests use a small one).
    pub(crate) fn set_capped(&mut self, site: &str, stamp: &str, mut chunks: Vec<Hash>, max: usize) -> bool {
        self.remove(site);
        chunks.sort_by(|a, b| a.0.cmp(&b.0));
        chunks.dedup();
        if self.indexed + chunks.len() > max {
            self.sites.insert(site.to_string(), (stamp.to_string(), Vec::new()));
            return false;
        }
        let name: Arc<str> = Arc::from(site);
        for h in &chunks {
            self.chunks.entry(*h).or_default().push(name.clone());
        }
        self.indexed += chunks.len();
        self.sites.insert(site.to_string(), (stamp.to_string(), chunks));
        true
    }

    /// Remember `site` by its stamp without indexing any of its chunks (it
    /// is too big for the index).
    pub fn skip(&mut self, site: &str, stamp: &str) {
        self.remove(site);
        self.sites.insert(site.to_string(), (stamp.to_string(), Vec::new()));
    }

    /// Forget `site`.
    pub fn remove(&mut self, site: &str) {
        let Some((_, chunks)) = self.sites.remove(site) else {
            return;
        };
        self.indexed -= chunks.len();
        for h in chunks {
            if let Some(v) = self.chunks.get_mut(&h) {
                v.retain(|s| &**s != site);
                if v.is_empty() {
                    self.chunks.remove(&h);
                }
            }
        }
    }

    /// Forget every site `keep` does not name.
    pub fn retain_sites(&mut self, keep: &dyn Fn(&str) -> bool) {
        let gone: Vec<String> = self.sites.keys().filter(|s| !keep(s)).cloned().collect();
        for s in gone {
            self.remove(&s);
        }
    }

    /// The held sites that use `chunk`.
    pub fn sites_of(&self, chunk: &Hash) -> &[Arc<str>] {
        self.chunks.get(chunk).map(|v| v.as_slice()).unwrap_or(&[])
    }
}

/// Whether `peer` announces the current version of `site_id` whole: the
/// record's manifest, with no list of groups.
pub fn claims_whole(map: &ReplicaMap, site_id: &str, peer: &str) -> bool {
    let (Some(rec), Some(h)) = (map.records.get(site_id), map.peers.get(peer)) else {
        return false;
    };
    h.sites.get(site_id).is_some_and(|(m, _)| *m == rec.manifest) && !h.groups.contains_key(site_id)
}

/// Whether a fetch of `chunk` by `peer` is refused: the chunk belongs to
/// sites held here, and `peer` claims every one of them whole. A chunk two
/// sites share is served unless the peer claims both, since it may be
/// fetching the one it does not have.
pub fn refuses(map: &ReplicaMap, index: &ChunkIndex, peer: &str, chunk: &Hash) -> bool {
    let sites = index.sites_of(chunk);
    !sites.is_empty() && sites.iter().all(|s| claims_whole(map, s, peer))
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};
    use std::time::Instant;

    use super::*;
    use crate::hosting::site::SiteRecord;

    fn rec(name: &str) -> SiteRecord {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        SiteRecord::sign(&kp, name, &Hash::of(name.as_bytes()), 1, 10, vec![]).unwrap()
    }

    fn announce(m: &mut ReplicaMap, peer: &str, held: &[(&SiteRecord, Option<&str>, Option<&[u32]>)]) {
        let mut sites = HashMap::new();
        let mut groups = HashMap::new();
        for (r, manifest, gs) in held {
            sites.insert(r.site_id.clone(), (manifest.unwrap_or(&r.manifest).to_string(), r.seq));
            if let Some(gs) = gs {
                groups.insert(r.site_id.clone(), gs.iter().copied().collect::<BTreeSet<u32>>());
            }
        }
        m.apply_holding_full(peer, sites, groups, Instant::now(), true, None, None, None, vec![]);
    }

    #[test]
    fn a_peer_claiming_the_site_whole_is_refused_and_every_other_peer_is_served() {
        let a = rec("a");
        let mut m = ReplicaMap::default();
        m.apply_record(a.clone());
        let chunk = Hash::of(b"chunk of a");
        let mut idx = ChunkIndex::default();
        assert!(idx.set(&a.site_id, "v1", vec![chunk]));

        announce(&mut m, "whole", &[(&a, None, None)]);
        announce(&mut m, "nothing", &[]);
        announce(&mut m, "old-version", &[(&a, Some("an-older-manifest"), None)]);
        announce(&mut m, "some-groups", &[(&a, None, Some(&[0u32, 2][..]))]);

        assert!(refuses(&m, &idx, "whole", &chunk), "claims whole: has no reason to fetch");
        assert!(!refuses(&m, &idx, "nothing", &chunk), "a node taking its first copy");
        assert!(!refuses(&m, &idx, "old-version", &chunk), "a holder moving to the new version");
        assert!(!refuses(&m, &idx, "some-groups", &chunk), "a holder of other groups");
        assert!(!refuses(&m, &idx, "never-heard-of", &chunk));
        // A chunk of no site held here is nobody's business.
        assert!(!refuses(&m, &idx, "whole", &Hash::of(b"something else")));
        // It stops claiming (a repairing node must): served again.
        announce(&mut m, "whole", &[]);
        assert!(!refuses(&m, &idx, "whole", &chunk));
    }

    #[test]
    fn a_chunk_two_sites_share_is_refused_only_to_a_peer_claiming_both() {
        let (a, b) = (rec("a"), rec("b"));
        let mut m = ReplicaMap::default();
        m.apply_record(a.clone());
        m.apply_record(b.clone());
        let shared = Hash::of(b"shared");
        let only_a = Hash::of(b"only a");
        let mut idx = ChunkIndex::default();
        idx.set(&a.site_id, "v1", vec![shared, only_a]);
        idx.set(&b.site_id, "v1", vec![shared]);
        announce(&mut m, "has-a", &[(&a, None, None)]);
        announce(&mut m, "has-both", &[(&a, None, None), (&b, None, None)]);
        assert!(refuses(&m, &idx, "has-a", &only_a));
        assert!(!refuses(&m, &idx, "has-a", &shared), "it may be fetching b");
        assert!(refuses(&m, &idx, "has-both", &shared));
        // Letting b go leaves the chunk as a's alone.
        idx.remove(&b.site_id);
        assert!(refuses(&m, &idx, "has-a", &shared));
    }

    #[test]
    fn two_nodes_holding_different_groups_serve_each_other() {
        // Each announces the groups it has; neither claims the site whole,
        // so each can fetch from the other the groups it lacks.
        let a = rec("a");
        let mut m = ReplicaMap::default();
        m.apply_record(a.clone());
        announce(&mut m, "left", &[(&a, None, Some(&[0u32, 1][..]))]);
        announce(&mut m, "right", &[(&a, None, Some(&[2u32, 3][..]))]);
        let (g0, g2) = (Hash::of(b"group 0"), Hash::of(b"group 2"));
        // What "left" and "right" each index: the chunks of their groups.
        let mut left = ChunkIndex::default();
        left.set(&a.site_id, "v1:0,1", vec![g0]);
        let mut right = ChunkIndex::default();
        right.set(&a.site_id, "v1:2,3", vec![g2]);
        assert!(!refuses(&m, &left, "right", &g0));
        assert!(!refuses(&m, &right, "left", &g2));
    }

    #[test]
    fn the_index_is_bounded_and_forgets_sites_let_go() {
        let mut idx = ChunkIndex::default();
        let h = |i: u32| Hash::of(&i.to_le_bytes());
        assert!(idx.set_capped("a", "v1", vec![h(1), h(2), h(2)], 3));
        assert_eq!(idx.indexed, 2, "a chunk listed twice is one chunk");
        // Too big for what is left: remembered by its stamp, not indexed.
        assert!(!idx.set_capped("b", "v1", vec![h(3), h(4)], 3));
        assert_eq!(idx.stamp("b"), Some("v1"));
        assert!(idx.sites_of(&h(3)).is_empty());
        idx.skip("c", "v1");
        assert_eq!((idx.stamp("c"), idx.indexed), (Some("v1"), 2));
        // A new version of a replaces the old one's chunks.
        assert!(idx.set_capped("a", "v2", vec![h(5)], 3));
        assert!(idx.sites_of(&h(1)).is_empty());
        assert_eq!(idx.sites_of(&h(5)).len(), 1);
        idx.retain_sites(&|s| s == "b");
        assert_eq!((idx.indexed, idx.chunks.len(), idx.stamp("a")), (0, 0, None));
    }
}
