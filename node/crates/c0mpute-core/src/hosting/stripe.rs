//! Striped placement for large sites.
//!
//! A site at or below [`STRIPE_ABOVE`] is replicated whole, as it always was.
//! Above it, the site's chunks are bucketed by hash into one group per 64 MiB,
//! at most [`MAX_GROUPS`] ([`group_count`] of the signed size; a site over
//! 512 GiB gets bigger groups, 512 MiB each at the 4 TiB cap) and every group is placed on its own: the hosting nodes that rank
//! highest for `(site, group)` (rendezvous hashing, the same answer on every
//! node) take that group. No node needs the whole site, a 50 GiB folder spreads
//! over the whole network, and healing, copy counting, garbage collection,
//! challenges and the free-disk floor all work one group at a time.
//!
//! Groups are defined over chunk hashes only, never over file contents, so the
//! same layout works for a site whose chunks are ciphertext, and a gateway can
//! tell which group a chunk is in from its hash and the record's size alone.

use std::collections::{BTreeSet, HashMap};

use base64::Engine;
use c0mpute_proto::Hash;

use super::replicas::{ReplicaMap, chosen, one_per_machine, releases, same_machine, score};
use super::site::{FILE_CHUNK_BYTES, FileEntry, STRIPE_ABOVE, SiteManifest};

/// Chunks per group: 64 MiB of a site.
pub const GROUP_CHUNKS: usize = 64;
/// Most groups any site can have: 512 GiB of 64 MiB groups. Larger sites (up
/// to the 4 TiB cap) keep this many groups and make each one bigger, so a
/// group bitmap read off the gossip stays at most 1 KiB.
pub const MAX_GROUPS: usize = 8192;

static THRESHOLD: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(STRIPE_ABOVE);

/// The size above which sites are striped: [`STRIPE_ABOVE`], unless a test
/// lowered it.
pub fn stripe_above() -> u64 {
    THRESHOLD.load(std::sync::atomic::Ordering::Relaxed)
}

/// Lower the striping threshold for this process, so an end-to-end test can
/// stripe a few hundred MiB instead of writing gigabytes. Every node in such a
/// test must run in the same process. Never call it outside tests.
#[doc(hidden)]
pub fn set_stripe_above_for_tests(bytes: u64) {
    THRESHOLD.store(bytes, std::sync::atomic::Ordering::Relaxed);
}

/// True when a site of `size` bytes is striped.
pub fn is_striped(size: u64) -> bool {
    size > stripe_above()
}

/// Bytes of a site per group, on average.
pub const GROUP_BYTES: u64 = (GROUP_CHUNKS * FILE_CHUNK_BYTES) as u64;

/// How many groups a striped site of `size` bytes has: one per 64 MiB, at
/// most [`MAX_GROUPS`]. The
/// size is in the signed record, so every node agrees without the manifest.
pub fn group_count(size: u64) -> usize {
    (size.div_ceil(GROUP_BYTES).max(1) as usize).min(MAX_GROUPS)
}

/// The group a chunk belongs to: its hash, bucketed. A gateway finds the
/// holders of any chunk from the record alone, without loading every part of
/// a large manifest, and it works the same over ciphertext.
pub fn group_of_hash(h: &Hash, n: usize) -> u32 {
    let x = u64::from_le_bytes(h.0[..8].try_into().expect("8 bytes"));
    (x % n.max(1) as u64) as u32
}

/// How a striped site's chunks fall into groups.
#[derive(Debug)]
pub struct Layout {
    groups: Vec<Vec<Hash>>,
    bytes: Vec<u64>,
}

impl Layout {
    /// The groups of a site from all its files ([`group_count`] of its size).
    pub fn of(manifest: &SiteManifest) -> Self {
        Self::new(group_count(manifest.total_bytes()), manifest.files.values())
    }

    /// `n` groups over the chunks of `files` (each chunk once, sorted within
    /// its group).
    pub fn new<'a>(n: usize, files: impl Iterator<Item = &'a FileEntry>) -> Self {
        let n = n.max(1);
        let mut size_of: HashMap<Hash, u64> = HashMap::new();
        for f in files {
            for (i, h) in f.chunks.iter().enumerate() {
                if let Ok(hash) = Hash::from_hex(h) {
                    let off = i as u64 * FILE_CHUNK_BYTES as u64;
                    size_of
                        .entry(hash)
                        .or_insert(f.size.saturating_sub(off).min(FILE_CHUNK_BYTES as u64));
                }
            }
        }
        let mut groups: Vec<Vec<Hash>> = vec![Vec::new(); n];
        let mut bytes = vec![0u64; n];
        for (h, len) in &size_of {
            let g = group_of_hash(h, n) as usize;
            groups[g].push(*h);
            bytes[g] += len;
        }
        for g in groups.iter_mut() {
            g.sort_by(|a, b| a.0.cmp(&b.0));
        }
        Self { groups, bytes }
    }

    pub fn len(&self) -> usize {
        self.groups.len()
    }

    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    pub fn group(&self, g: u32) -> &[Hash] {
        self.groups
            .get(g as usize)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Bytes of data in group `g`.
    pub fn bytes(&self, g: u32) -> u64 {
        self.bytes.get(g as usize).copied().unwrap_or(0)
    }

    pub fn group_of(&self, h: &Hash) -> Option<u32> {
        (!self.groups.is_empty()).then(|| group_of_hash(h, self.groups.len()))
    }

    /// Bytes of a set of groups.
    pub fn bytes_of<'a>(&self, groups: impl IntoIterator<Item = &'a u32>) -> u64 {
        groups.into_iter().map(|g| self.bytes(*g)).sum()
    }
}

/// A set of groups as it travels on the gossip: a bitmap, base64url.
pub fn encode_groups(groups: &BTreeSet<u32>) -> String {
    let n = groups
        .iter()
        .next_back()
        .map(|g| *g as usize / 8 + 1)
        .unwrap_or(0);
    let mut bits = vec![0u8; n];
    for g in groups {
        bits[*g as usize / 8] |= 1 << (g % 8);
    }
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bits)
}

/// Parse a group bitmap; `None` when it is not one or is too long.
pub fn decode_groups(s: &str) -> Option<BTreeSet<u32>> {
    if s.len() > MAX_GROUPS / 6 + 4 {
        return None;
    }
    let bits = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s)
        .ok()?;
    let mut out = BTreeSet::new();
    for (i, b) in bits.iter().enumerate() {
        for j in 0..8 {
            if b & (1 << j) != 0 {
                out.insert((i * 8 + j) as u32);
            }
        }
    }
    Some(out)
}

/// The rendezvous key of one group.
pub fn group_key(site_id: &str, g: u32) -> String {
    format!("{site_id}/{g}")
}

/// What this node should do about the groups of one striped site.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Groups to fetch, most wanted first.
    pub fetch: Vec<u32>,
    /// Groups to let go.
    pub drop: Vec<u32>,
}

/// Per-group copy health of a striped site.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Health {
    pub groups: usize,
    /// Fewest live copies of any group.
    pub min_copies: usize,
    /// Groups below the target.
    pub short: usize,
    /// Groups with no live copy at all.
    pub missing: usize,
}

/// Copy health of a site from the per-group holder lists.
pub fn health(holders: &[Vec<String>], target: usize) -> Health {
    Health {
        groups: holders.len(),
        min_copies: holders.iter().map(|h| h.len()).min().unwrap_or(0),
        short: holders.iter().filter(|h| h.len() < target).count(),
        missing: holders.iter().filter(|h| h.is_empty()).count(),
    }
}

/// The preferred hosts that take group `g`: all of them when there are at
/// most `target` (a paid host holds the whole site), else the `target` that
/// rank highest for the group.
pub fn preferred_takers(site_id: &str, g: u32, preferred: &[String], target: usize) -> Vec<String> {
    if preferred.len() <= target {
        preferred.to_vec()
    } else {
        chosen(&group_key(site_id, g), preferred, target)
    }
}

/// Decide, for `me`, which groups of `site_id` to fetch and which to drop.
///
/// Per group this is the whole-site rule from `replicas::decide`, keyed on
/// `(site, group)`: preferred (paid) hosts take their groups whatever the
/// count; below `target` copies the top-ranked striping hosts with room for
/// the group take it (`widen(g)` more of them once a group has been short for
/// a while, so a chosen node that cannot take it does not stall the group);
/// above `target` the lowest-ranked free holder lets go, at once above `max`
/// and otherwise only once `trim(g)` says the group has been over `target`
/// for the grace period (see [`releases`]).
#[allow(clippy::too_many_arguments)]
pub fn plan(
    map: &ReplicaMap,
    site_id: &str,
    layout: &Layout,
    me: &str,
    i_host: bool,
    target: usize,
    max: usize,
    widen: &dyn Fn(u32) -> usize,
    trim: &dyn Fn(u32) -> bool,
) -> Plan {
    let holders = map.group_holders(site_id, layout.len(), true);
    let preferred = map.effective_preferred(site_id);
    let striping = map.striping_peers();
    let machines = map.machines();
    let mut out = Plan::default();
    let mut wanted: Vec<(usize, [u8; 32], u32)> = Vec::new();
    for (gi, all) in holders.iter().enumerate() {
        let g = gi as u32;
        let key = group_key(site_id, g);
        let i_hold = all.iter().any(|p| p == me);
        let takers = preferred_takers(site_id, g, &preferred, target);
        // Copies count per machine: a second node on a box that already
        // holds the group adds nothing (the box going away takes both).
        let (hs, extra) = one_per_machine(&machines, &key, all, &takers);
        if i_hold {
            if extra.iter().any(|p| p == me) && !takers.iter().any(|p| p == me) {
                // A duplicate on a machine already counted: let it go once
                // the group has been over the target for the grace period
                // (so a real copy elsewhere is in place first).
                if trim(g) {
                    out.drop.push(g);
                }
            } else if !takers.iter().any(|p| p == me) {
                let free: Vec<String> = hs.iter().filter(|p| !takers.contains(p)).cloned().collect();
                if releases(&key, hs.len(), &free, me, target, max, trim(g)) {
                    out.drop.push(g);
                }
            }
            continue;
        }
        if !i_host {
            continue;
        }
        if takers.iter().any(|p| p == me) {
            // Paid groups first, then the emptiest.
            wanted.push((0, score(&key, me), g));
            continue;
        }
        if hs.len() >= target {
            continue;
        }
        if hs.iter().any(|h| same_machine(&machines, h, me)) {
            // This box already holds the group.
            continue;
        }
        let need = layout.bytes(g);
        let mut candidates: Vec<String> = striping
            .iter()
            .filter(|p| !all.contains(p) && !takers.contains(p))
            .filter(|p| !hs.iter().any(|h| same_machine(&machines, h, p)))
            .filter(|p| map.room_of(p).is_some_and(|r| r >= need))
            .cloned()
            .collect();
        // One candidate per machine too, or two nodes on one box would
        // both be chosen for the same group.
        candidates = one_per_machine(&machines, &key, &candidates, &[]).0;
        if !candidates.iter().any(|p| p == me) {
            // Not announced yet (or too full): only count ourselves when we
            // have not said we are full.
            if map.room_of(me).is_some_and(|r| r < need) {
                continue;
            }
            // Or another node on this box stands for it.
            if candidates.iter().any(|c| same_machine(&machines, c, me)) {
                continue;
            }
            candidates.push(me.to_string());
        }
        let n = target - hs.len() + widen(g);
        if chosen(&key, &candidates, n).iter().any(|p| p == me) {
            wanted.push((1 + hs.len(), score(&key, me), g));
        }
    }
    // Paid groups, then groups with fewest copies, then a stable order.
    wanted.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
    out.fetch = wanted.into_iter().map(|(_, _, g)| g).collect();
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Instant;

    use super::*;
    use crate::hosting::site::{FileEntry, SiteRecord};

    fn manifest(chunks: usize) -> SiteManifest {
        let mut files = BTreeMap::new();
        let hashes: Vec<String> = (0..chunks)
            .map(|i| Hash::of(&(i as u64).to_le_bytes()).to_hex())
            .collect();
        files.insert(
            "movie.mp4".to_string(),
            FileEntry {
                size: chunks as u64 * FILE_CHUNK_BYTES as u64 - 10,
                content_type: "video/mp4".into(),
                chunks: hashes,
            },
        );
        SiteManifest {
            v: 1,
            name: "m".into(),
            files,
        }
    }

    #[test]
    fn layout_is_deterministic_and_covers_every_chunk_once() {
        let m = manifest(300);
        let a = Layout::of(&m);
        let b = Layout::of(&m);
        assert_eq!(a.len(), 5);
        assert_eq!(a.groups, b.groups);
        let total: usize = (0..a.len() as u32).map(|g| a.group(g).len()).sum();
        assert_eq!(total, 300);
        let bytes: u64 = (0..a.len() as u32).map(|g| a.bytes(g)).sum();
        assert_eq!(bytes, m.total_bytes());
        for h in m.chunk_hashes() {
            let h = Hash::from_hex(&h).unwrap();
            let g = a.group_of(&h).unwrap();
            assert_eq!(g, group_of_hash(&h, group_count(m.total_bytes())));
            assert!(a.group(g).contains(&h));
        }
        // Buckets are roughly even (64 chunks on average).
        assert!((0..a.len() as u32).all(|g| (20..=110).contains(&a.group(g).len())));
        assert_eq!(group_count(1), 1);
        assert_eq!(group_count(256 * 1024 * 1024 * 1024), 4096);
        assert_eq!(group_count(512 * 1024 * 1024 * 1024), MAX_GROUPS);
        // The 4 TiB cap: the group count stops at MAX_GROUPS, groups grow to 512 MiB.
        assert_eq!(group_count(super::super::site::MAX_SITE_BYTES), MAX_GROUPS);
        assert_eq!(super::super::site::MAX_SITE_BYTES / MAX_GROUPS as u64, 512 * 1024 * 1024);
    }

    #[test]
    fn group_bitmaps_round_trip() {
        let s: BTreeSet<u32> = [0, 3, 8, 9, 4095].into_iter().collect();
        assert_eq!(decode_groups(&encode_groups(&s)).unwrap(), s);
        assert_eq!(
            decode_groups(&encode_groups(&BTreeSet::new())).unwrap(),
            BTreeSet::new()
        );
        assert!(decode_groups(&"A".repeat(5000)).is_none());
        assert!(decode_groups("!!").is_none());
    }

    fn big_record(kp: &c0mpute_net::libp2p::identity::Keypair, m: &SiteManifest) -> SiteRecord {
        SiteRecord::sign(kp, "m", &m.stored().hash(), 1, m.total_bytes(), vec![]).unwrap()
    }

    fn map_with(peers: &[String], rec: &SiteRecord) -> ReplicaMap {
        let mut map = ReplicaMap::default();
        assert!(map.apply_record(rec.clone()));
        let now = Instant::now();
        for p in peers {
            map.apply_holding_full(
                p,
                HashMap::new(),
                HashMap::new(),
                now,
                true,
                None,
                None,
                Some(u64::MAX),
                vec![],
            );
        }
        map
    }

    #[test]
    fn every_group_gets_exactly_target_takers_and_no_node_takes_everything() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let m = manifest(64 * 40);
        let rec = big_record(&kp, &m);
        let layout = Layout::of(&m);
        let peers: Vec<String> = (0..8).map(|i| format!("peer{i}")).collect();
        let map = map_with(&peers, &rec);
        let mut takers = vec![0usize; layout.len()];
        for p in &peers {
            let plan = plan(&map, &rec.site_id, &layout, p, true, 3, 5, &|_| 0, &|_| true);
            assert!(plan.drop.is_empty());
            assert!(
                plan.fetch.len() < layout.len(),
                "{p} would hold the whole site"
            );
            for g in plan.fetch {
                takers[g as usize] += 1;
            }
        }
        assert!(takers.iter().all(|t| *t == 3), "{takers:?}");
        // Same inputs, same plan, on every node.
        let a = plan(&map, &rec.site_id, &layout, "peer3", true, 3, 5, &|_| 0, &|_| true);
        let b = plan(&map, &rec.site_id, &layout, "peer3", true, 3, 5, &|_| 0, &|_| true);
        assert_eq!(a, b);
    }

    /// `map_with`, but each peer announces the given direct address.
    fn map_on(peers: &[(String, &str)], rec: &SiteRecord) -> ReplicaMap {
        let mut map = ReplicaMap::default();
        assert!(map.apply_record(rec.clone()));
        let now = Instant::now();
        for (p, addr) in peers {
            map.apply_holding_full(
                p,
                HashMap::new(),
                HashMap::new(),
                now,
                true,
                None,
                None,
                Some(u64::MAX),
                vec![addr.to_string()],
            );
        }
        map
    }

    fn hold_all(map: &mut ReplicaMap, rec: &SiteRecord, p: &str, groups: impl IntoIterator<Item = u32>) {
        let mut sites = HashMap::new();
        sites.insert(rec.site_id.clone(), (rec.manifest.clone(), rec.seq));
        let mut g = HashMap::new();
        g.insert(rec.site_id.clone(), groups.into_iter().collect::<BTreeSet<u32>>());
        let addrs = map.peers.get(p).map(|h| h.addrs.clone()).unwrap_or_default();
        map.apply_holding_full(p, sites, g, Instant::now(), true, None, None, Some(u64::MAX), addrs);
    }

    #[test]
    fn two_nodes_on_one_machine_are_one_copy() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let m = manifest(64 * 8);
        let rec = big_record(&kp, &m);
        let layout = Layout::of(&m);
        // a1 + a2 share a box (the publisher's box and its worker, or two
        // workers on one disk); b and c are their own machines; r is relayed
        // through a's box (its circuit address names a's IP) but is not on it.
        let peers: Vec<(String, &str)> = vec![
            ("a1".into(), "/ip4/203.0.113.1/tcp/1"),
            ("a2".into(), "/ip4/203.0.113.1/tcp/2"),
            ("b".into(), "/ip4/203.0.113.2/tcp/1"),
            ("c".into(), "/ip4/203.0.113.3/tcp/1"),
            ("r".into(), "/ip4/203.0.113.1/tcp/1/p2p/12D3KooWRelay/p2p-circuit"),
        ];
        let mut map = map_on(&peers, &rec);
        let all: Vec<u32> = (0..layout.len() as u32).collect();
        hold_all(&mut map, &rec, "a1", all.clone());
        hold_all(&mut map, &rec, "a2", all.clone());
        hold_all(&mut map, &rec, "b", all.clone());
        // Raw count 3, but only 2 machines.
        let h = health(&map.distinct_group_holders(&rec.site_id, layout.len()), 3);
        assert_eq!((h.min_copies, h.short), (2, layout.len()));
        // Every group goes to exactly one of c and r (one more machine each).
        for g in 0..layout.len() as u32 {
            let takers: Vec<&str> = ["c", "r"]
                .into_iter()
                .filter(|p| plan(&map, &rec.site_id, &layout, p, true, 3, 5, &|_| 0, &|_| false).fetch.contains(&g))
                .collect();
            assert_eq!(takers.len(), 1, "group {g}: {takers:?}");
        }
        // Neither node on the box re-fetches or drops before the grace.
        for p in ["a1", "a2"] {
            let pl = plan(&map, &rec.site_id, &layout, p, true, 3, 5, &|_| 0, &|_| false);
            assert!(pl.fetch.is_empty() && pl.drop.is_empty(), "{p}: {pl:?}");
        }
        // Once c holds everything: 3 machines; after the grace the duplicate
        // on a's box lets go (exactly one of a1/a2, per group), nobody else.
        hold_all(&mut map, &rec, "c", all.clone());
        let h = health(&map.distinct_group_holders(&rec.site_id, layout.len()), 3);
        assert_eq!((h.min_copies, h.short), (3, 0));
        let drops = |p: &str| plan(&map, &rec.site_id, &layout, p, true, 3, 5, &|_| 0, &|_| true).drop;
        for g in 0..layout.len() as u32 {
            let n = ["a1", "a2"].into_iter().filter(|p| drops(p).contains(&g)).count();
            assert_eq!(n, 1, "group {g}");
        }
        assert!(drops("b").is_empty() && drops("c").is_empty());
    }

    #[test]
    fn a_node_on_a_box_that_holds_the_group_is_never_chosen() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let m = manifest(64 * 4);
        let rec = big_record(&kp, &m);
        let layout = Layout::of(&m);
        let peers: Vec<(String, &str)> = vec![
            ("a1".into(), "/ip4/203.0.113.1/tcp/1"),
            ("a2".into(), "/ip4/203.0.113.1/tcp/2"),
        ];
        let mut map = map_on(&peers, &rec);
        hold_all(&mut map, &rec, "a1", 0..layout.len() as u32);
        // Short of copies forever, widened as far as it goes: a2 still waits
        // for another machine instead of adding a copy that counts for nothing.
        let pl = plan(&map, &rec.site_id, &layout, "a2", true, 3, 5, &|_| 10, &|_| false);
        assert!(pl.fetch.is_empty(), "{pl:?}");
    }

    #[test]
    fn a_group_whose_holder_left_is_refilled_by_one_node() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let m = manifest(64 * 6);
        let rec = big_record(&kp, &m);
        let layout = Layout::of(&m);
        let peers: Vec<String> = (0..6).map(|i| format!("peer{i}")).collect();
        let mut map = map_with(&peers, &rec);
        // Everybody takes what it was given.
        let now = Instant::now();
        let plans: Vec<(String, Vec<u32>)> = peers
            .iter()
            .map(|p| {
                (
                    p.clone(),
                    plan(&map, &rec.site_id, &layout, p, true, 3, 5, &|_| 0, &|_| true).fetch,
                )
            })
            .collect();
        for (p, groups) in &plans {
            let mut sites = HashMap::new();
            sites.insert(rec.site_id.clone(), (rec.manifest.clone(), rec.seq));
            let mut g = HashMap::new();
            g.insert(
                rec.site_id.clone(),
                groups.iter().copied().collect::<BTreeSet<u32>>(),
            );
            map.apply_holding_full(p, sites, g, now, true, None, None, Some(u64::MAX), vec![]);
        }
        let h = health(&map.group_holders(&rec.site_id, layout.len(), true), 3);
        assert_eq!((h.min_copies, h.short), (3, 0));
        // A holder of group 0 leaves.
        let gone = map.group_holders(&rec.site_id, layout.len(), true)[0][0].clone();
        map.peers.remove(&gone);
        let lost: BTreeSet<u32> = plans
            .iter()
            .find(|(p, _)| *p == gone)
            .unwrap()
            .1
            .iter()
            .copied()
            .collect();
        let mut refill = vec![0usize; layout.len()];
        for p in peers.iter().filter(|p| **p != gone) {
            let plan = plan(&map, &rec.site_id, &layout, p, true, 3, 5, &|_| 0, &|_| true);
            assert!(plan.drop.is_empty());
            for g in plan.fetch {
                assert!(
                    lost.contains(&g),
                    "{p} fetches group {g} that still has 3 copies"
                );
                refill[g as usize] += 1;
            }
        }
        for g in 0..layout.len() {
            assert_eq!(
                refill[g],
                usize::from(lost.contains(&(g as u32))),
                "group {g}"
            );
        }
    }

    #[test]
    fn nodes_without_room_or_striping_support_are_skipped() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let m = manifest(64 * 4);
        let rec = big_record(&kp, &m);
        let layout = Layout::of(&m);
        let mut map = ReplicaMap::default();
        map.apply_record(rec.clone());
        let now = Instant::now();
        // An older node (no room field) and a full node.
        map.apply_holding_full(
            "old",
            HashMap::new(),
            HashMap::new(),
            now,
            true,
            None,
            None,
            None,
            vec![],
        );
        map.apply_holding_full(
            "full",
            HashMap::new(),
            HashMap::new(),
            now,
            true,
            None,
            None,
            Some(0),
            vec![],
        );
        for p in ["a", "b", "c"] {
            map.apply_holding_full(
                p,
                HashMap::new(),
                HashMap::new(),
                now,
                true,
                None,
                None,
                Some(u64::MAX),
                vec![],
            );
        }
        for p in ["a", "b", "c"] {
            let plan = plan(&map, &rec.site_id, &layout, p, true, 3, 5, &|_| 0, &|_| true);
            assert_eq!(
                plan.fetch.len(),
                layout.len(),
                "{p} must take every group: only 3 usable nodes"
            );
        }
    }

    #[test]
    fn extra_group_copies_are_trimmed_per_group_after_the_grace() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let m = manifest(64 * 3);
        let rec = big_record(&kp, &m);
        let layout = Layout::of(&m);
        assert!(layout.len() >= 2);
        let peers: Vec<String> = (0..5).map(|i| format!("peer{i}")).collect();
        let mut map = map_with(&peers, &rec);
        // Group 0 has four copies (one extra), every other group exactly three.
        let all: BTreeSet<u32> = (0..layout.len() as u32).collect();
        let only0: BTreeSet<u32> = [0].into();
        let now = Instant::now();
        for (i, p) in peers.iter().enumerate() {
            let gs = match i {
                0..=2 => all.clone(),
                3 => only0.clone(),
                _ => BTreeSet::new(),
            };
            let mut sites = HashMap::new();
            if !gs.is_empty() {
                sites.insert(rec.site_id.clone(), (rec.manifest.clone(), 1));
            }
            let mut groups = HashMap::new();
            groups.insert(rec.site_id.clone(), gs);
            map.apply_holding_full(p, sites, groups, now, true, None, None, Some(u64::MAX), vec![]);
        }
        let holders = map.group_holders(&rec.site_id, layout.len(), true);
        assert_eq!(holders[0].len(), 4);
        assert!(holders[1..].iter().all(|h| h.len() == 3));
        let drops = |trim: &dyn Fn(u32) -> bool| -> Vec<(String, u32)> {
            peers
                .iter()
                .flat_map(|p| {
                    plan(&map, &rec.site_id, &layout, p, true, 3, 5, &|_| 0, trim)
                        .drop
                        .into_iter()
                        .map(move |g| (p.clone(), g))
                })
                .collect()
        };
        assert!(drops(&|_| false).is_empty(), "in grace: nothing goes");
        // The grace is per group: over for another group changes nothing.
        assert!(drops(&|g| g != 0).is_empty(), "only group 0 is over");
        let d = drops(&|g| g == 0);
        assert_eq!(d.len(), 1, "exactly one holder lets group 0 go: {d:?}");
        assert_eq!(d[0].1, 0);
    }

    #[test]
    fn widening_brings_in_the_next_ranked_node() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let m = manifest(64);
        let rec = big_record(&kp, &m);
        let layout = Layout::of(&m);
        let peers: Vec<String> = (0..6).map(|i| format!("peer{i}")).collect();
        let map = map_with(&peers, &rec);
        let base = peers
            .iter()
            .filter(|p| {
                !plan(&map, &rec.site_id, &layout, p, true, 3, 5, &|_| 0, &|_| true)
                    .fetch
                    .is_empty()
            })
            .count();
        let wide = peers
            .iter()
            .filter(|p| {
                !plan(&map, &rec.site_id, &layout, p, true, 3, 5, &|_| 1, &|_| true)
                    .fetch
                    .is_empty()
            })
            .count();
        assert_eq!((base, wide), (3, 4));
    }
}
