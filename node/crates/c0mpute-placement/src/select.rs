//! Peer selection for shard placement (CIP-003).
//!
//! Choosing `n` peers for a block is where the durability model in CIP-001
//! actually lives. Those availability figures assume shard hosts fail
//! *independently*; fourteen shards behind one ISP are not fourteen
//! independent samples, and nothing downstream can detect that the assumption
//! was violated. So this module fails loudly rather than placing badly.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::peer::{FailureDomain, PeerInfo};

/// Rules a placement must satisfy. Defaults come from CIP-001.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlacementPolicy {
    /// Minimum `c0mpute-verify::reputation`.
    pub min_reputation: f32,
    /// Minimum 30-day uptime. CIP-001's durability table collapses from ~6.7
    /// nines to ~3.4 between 0.99 and 0.95, so this gate is doing more work
    /// than the parity count is.
    pub min_uptime_30d: f32,
    /// Most shards of one block allowed in a single failure domain.
    /// `floor(parity / 2)` by default: half the parity budget can be lost to
    /// one ISP or region going dark, and the object still reads.
    pub max_per_domain: usize,
    /// Whether peers whose failure domain could not be determined may be used.
    ///
    /// When `false` (the default) they are excluded. When `true` they are all
    /// treated as members of a *single* shared domain, which is the
    /// conservative reading — the alternative, giving each unknown peer its
    /// own domain, would let fourteen unlocatable peers satisfy every
    /// constraint while providing no real diversity at all.
    pub allow_unknown_domain: bool,
}

impl PlacementPolicy {
    /// The policy for a tier's `(k, parity)`.
    pub fn for_parity(parity: usize) -> Self {
        Self {
            min_reputation: 0.9,
            min_uptime_30d: 0.99,
            max_per_domain: (parity / 2).max(1),
            allow_unknown_domain: false,
        }
    }

    /// How many distinct failure domains a placement of `n` shards needs.
    pub fn domains_required(&self, n: usize) -> usize {
        n.div_ceil(self.max_per_domain)
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum PlacementError {
    #[error(
        "not enough eligible peers: need {needed}, found {eligible} \
         (of {total} known; {below_bar} below reputation {min_reputation} or uptime {min_uptime}, \
         {too_full} without room for a {shard_bytes}-byte shard, {unknown_domain} with an unknown failure domain)"
    )]
    InsufficientPeers {
        needed: usize,
        eligible: usize,
        total: usize,
        below_bar: usize,
        too_full: usize,
        unknown_domain: usize,
        min_reputation: f32,
        min_uptime: f32,
        shard_bytes: u64,
    },
    #[error(
        "failure-domain diversity unsatisfiable: {needed} shards at most {max_per_domain} \
         per domain needs {domains_required} distinct domains, but only {domains_available} \
         are available ({placed} shards could be placed)"
    )]
    DiversityUnsatisfiable {
        needed: usize,
        placed: usize,
        max_per_domain: usize,
        domains_required: usize,
        domains_available: usize,
    },
}

/// One shard assigned to one peer.
#[derive(Clone, Debug, PartialEq)]
pub struct Assignment {
    pub shard_index: u8,
    pub peer: PeerInfo,
}

/// Rank a peer. Higher is better.
///
/// Reputation and uptime dominate because they are what the durability model
/// is sensitive to; latency is a weak tiebreak, deliberately. Preferring fast
/// peers too strongly would concentrate placement on whichever few nodes are
/// nearest, which is the opposite of what diversity is for.
///
/// CIP-003 sketched this as `reputation * uptime * (1 / (1 + rtt/100))`. That
/// weighting does not match the intent: it makes a 400 ms peer score 20% below
/// a 1 ms one, so a fast flaky node outranks a slow reliable one — exactly the
/// trade CIP-001 says not to make, since availability drives durability and
/// latency does not. The latency term is therefore scaled into a narrow band:
/// it separates otherwise-equal peers and cannot overturn a reputation gap.
pub fn score(peer: &PeerInfo) -> f32 {
    let latency_factor = 1.0 / (1.0 + peer.rtt_ms as f32 / 100.0);
    peer.reputation * peer.uptime_30d * (0.9 + 0.1 * latency_factor)
}

/// Placement that is already in place, which a new selection must respect.
///
/// Repair (CIP-005) regenerates a few shards of a block whose other shards are
/// still healthy somewhere. Selecting for those replacements as though the
/// block were empty would let a block drift into a single failure domain one
/// repair at a time — each repair individually satisfying the cap, the block as
/// a whole quietly losing the independence its durability depends on.
#[derive(Clone, Debug, Default)]
pub struct PlacementContext {
    /// Peers already holding a shard of this block. Never reuse one: two
    /// shards on one host is one host, not two.
    pub exclude_peers: std::collections::HashSet<String>,
    /// Domains the surviving shards already occupy, counted against the cap.
    pub used_domains: HashMap<FailureDomain, usize>,
}

impl PlacementContext {
    /// Build the context implied by the peers currently holding a block.
    pub fn from_holders<'a>(holders: impl IntoIterator<Item = &'a PeerInfo>) -> Self {
        let mut ctx = Self::default();
        for p in holders {
            ctx.exclude_peers.insert(p.peer_id.clone());
            *ctx.used_domains.entry(p.domain()).or_insert(0) += 1;
        }
        ctx
    }
}

/// Choose `n` peers for one block's shards.
///
/// Greedy by score under a per-domain cap is **optimal here**, not just a
/// heuristic: "at most `max_per_domain` from each domain" is a partition
/// matroid, and greedy is optimal over a matroid. So if this returns
/// `DiversityUnsatisfiable`, no other assignment would have worked either —
/// there is no need to backtrack, and no better answer being missed.
pub fn select(
    candidates: &[PeerInfo],
    n: usize,
    shard_bytes: u64,
    policy: &PlacementPolicy,
) -> Result<Vec<Assignment>, PlacementError> {
    let peers = select_peers(
        candidates,
        n,
        shard_bytes,
        policy,
        &PlacementContext::default(),
    )?;
    Ok(peers
        .into_iter()
        .enumerate()
        .map(|(i, peer)| Assignment {
            shard_index: i as u8,
            peer,
        })
        .collect())
}

/// Choose `count` peers, respecting placement that already exists.
///
/// Returns peers rather than assignments: repair needs to map them onto
/// specific missing shard indices, not onto `0..n`.
pub fn select_peers(
    candidates: &[PeerInfo],
    count: usize,
    shard_bytes: u64,
    policy: &PlacementPolicy,
    ctx: &PlacementContext,
) -> Result<Vec<PeerInfo>, PlacementError> {
    let n = count;
    let total = candidates.len();
    let mut below_bar = 0usize;
    let mut too_full = 0usize;
    let mut unknown_domain = 0usize;

    let mut eligible: Vec<&PeerInfo> = Vec::new();
    for p in candidates {
        if ctx.exclude_peers.contains(&p.peer_id) {
            continue;
        }
        if p.reputation < policy.min_reputation || p.uptime_30d < policy.min_uptime_30d {
            below_bar += 1;
            continue;
        }
        if p.free_bytes < shard_bytes {
            too_full += 1;
            continue;
        }
        if matches!(p.domain(), FailureDomain::Unknown) && !policy.allow_unknown_domain {
            unknown_domain += 1;
            continue;
        }
        eligible.push(p);
    }

    if eligible.len() < n {
        return Err(PlacementError::InsufficientPeers {
            needed: n,
            eligible: eligible.len(),
            total,
            below_bar,
            too_full,
            unknown_domain,
            min_reputation: policy.min_reputation,
            min_uptime: policy.min_uptime_30d,
            shard_bytes,
        });
    }

    // Best first. `total_cmp` rather than `partial_cmp().unwrap()`: a NaN
    // score from a malformed peer record should sort, not panic.
    eligible.sort_by(|a, b| score(b).total_cmp(&score(a)));

    // Seeded with the domains surviving shards already occupy, so replacements
    // are capped against the block as a whole rather than against this
    // selection in isolation.
    let mut per_domain: HashMap<FailureDomain, usize> = ctx.used_domains.clone();
    let mut chosen: Vec<&PeerInfo> = Vec::with_capacity(n);
    for p in &eligible {
        if chosen.len() == n {
            break;
        }
        let domain = p.domain();
        let used = per_domain.entry(domain).or_insert(0);
        if *used >= policy.max_per_domain {
            continue;
        }
        *used += 1;
        chosen.push(p);
    }

    if chosen.len() < n {
        let domains_available = eligible
            .iter()
            .map(|p| p.domain())
            .collect::<std::collections::HashSet<_>>()
            .len();
        return Err(PlacementError::DiversityUnsatisfiable {
            needed: n,
            placed: chosen.len(),
            max_per_domain: policy.max_per_domain,
            domains_required: policy.domains_required(n),
            domains_available,
        });
    }

    Ok(chosen.into_iter().cloned().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(id: &str, asn: Option<u32>, rep: f32, uptime: f32) -> PeerInfo {
        PeerInfo {
            peer_id: id.to_string(),
            endpoint: format!("http://{id}.test"),
            reputation: rep,
            uptime_30d: uptime,
            free_bytes: 1 << 30,
            rtt_ms: 50,
            asn,
            region: None,
            ip_prefix: None,
        }
    }

    /// n peers, each in its own ASN — the easy case.
    fn diverse(n: usize) -> Vec<PeerInfo> {
        (0..n)
            .map(|i| peer(&format!("p{i}"), Some(1000 + i as u32), 0.95, 0.995))
            .collect()
    }

    #[test]
    fn selects_n_distinct_peers() {
        let policy = PlacementPolicy::for_parity(4);
        let got = select(&diverse(20), 14, 1024, &policy).unwrap();
        assert_eq!(got.len(), 14);

        let ids: std::collections::HashSet<_> = got.iter().map(|a| &a.peer.peer_id).collect();
        assert_eq!(ids.len(), 14, "same peer used twice for one block");

        let idx: Vec<u8> = got.iter().map(|a| a.shard_index).collect();
        assert_eq!(idx, (0..14).collect::<Vec<u8>>());
    }

    #[test]
    fn rejects_peers_below_the_reputation_bar() {
        let policy = PlacementPolicy::for_parity(4);
        let mut peers = diverse(14);
        for p in peers.iter_mut().take(3) {
            p.reputation = 0.5;
        }
        let err = select(&peers, 14, 1024, &policy).unwrap_err();
        match err {
            PlacementError::InsufficientPeers {
                eligible,
                below_bar,
                ..
            } => {
                assert_eq!(eligible, 11);
                assert_eq!(below_bar, 3);
            }
            other => panic!("unexpected: {other}"),
        }
    }

    /// CIP-001: RS 10/14 drops from ~6.7 nines to ~3.4 between 0.99 and 0.95
    /// per-node availability, so the uptime gate is load-bearing.
    #[test]
    fn rejects_peers_below_the_uptime_bar() {
        let policy = PlacementPolicy::for_parity(4);
        let mut peers = diverse(16);
        for p in peers.iter_mut().take(5) {
            p.uptime_30d = 0.95;
        }
        assert!(select(&peers, 14, 1024, &policy).is_err());
    }

    #[test]
    fn rejects_peers_without_room() {
        let policy = PlacementPolicy::for_parity(4);
        let mut peers = diverse(15);
        for p in peers.iter_mut().take(4) {
            p.free_bytes = 10;
        }
        let err = select(&peers, 14, 1_000_000, &policy).unwrap_err();
        assert!(matches!(
            err,
            PlacementError::InsufficientPeers { too_full: 4, .. }
        ));
    }

    /// The constraint that actually matters. Plenty of healthy peers, but they
    /// are all behind two ASNs, so a placement would be 14 correlated samples
    /// wearing the costume of 14 independent ones.
    #[test]
    fn refuses_to_place_without_failure_domain_diversity() {
        let policy = PlacementPolicy::for_parity(4);
        let peers: Vec<PeerInfo> = (0..20)
            .map(|i| {
                peer(
                    &format!("p{i}"),
                    Some(if i < 10 { 100 } else { 200 }),
                    0.95,
                    0.995,
                )
            })
            .collect();

        let err = select(&peers, 14, 1024, &policy).unwrap_err();
        match err {
            PlacementError::DiversityUnsatisfiable {
                placed,
                domains_required,
                domains_available,
                max_per_domain,
                ..
            } => {
                assert_eq!(max_per_domain, 2);
                assert_eq!(placed, 4, "2 domains x 2 per domain");
                assert_eq!(domains_required, 7);
                assert_eq!(domains_available, 2);
            }
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn caps_shards_per_domain() {
        let policy = PlacementPolicy::for_parity(4);
        // 7 ASNs, 4 peers each: enough peers and exactly enough domains.
        let peers: Vec<PeerInfo> = (0..28)
            .map(|i| peer(&format!("p{i}"), Some(100 + (i as u32 % 7)), 0.95, 0.995))
            .collect();
        let got = select(&peers, 14, 1024, &policy).unwrap();

        let mut per_asn: HashMap<u32, usize> = HashMap::new();
        for a in &got {
            *per_asn.entry(a.peer.asn.unwrap()).or_default() += 1;
        }
        assert_eq!(per_asn.len(), 7);
        for (asn, count) in per_asn {
            assert!(count <= 2, "asn {asn} got {count} shards, cap is 2");
        }
    }

    /// Unknown-domain peers share one domain rather than each getting their
    /// own — otherwise a network of unlocatable peers would satisfy every
    /// constraint while providing no diversity.
    #[test]
    fn unknown_domains_are_one_domain_not_many() {
        let mut policy = PlacementPolicy::for_parity(4);
        policy.allow_unknown_domain = true;
        let peers: Vec<PeerInfo> = (0..20)
            .map(|i| peer(&format!("p{i}"), None, 0.95, 0.995))
            .collect();

        let err = select(&peers, 14, 1024, &policy).unwrap_err();
        assert!(matches!(
            err,
            PlacementError::DiversityUnsatisfiable {
                placed: 2,
                domains_available: 1,
                ..
            }
        ));
    }

    #[test]
    fn unknown_domain_peers_are_excluded_by_default() {
        let policy = PlacementPolicy::for_parity(4);
        assert!(!policy.allow_unknown_domain);
        let peers: Vec<PeerInfo> = (0..20)
            .map(|i| peer(&format!("p{i}"), None, 0.95, 0.995))
            .collect();
        let err = select(&peers, 14, 1024, &policy).unwrap_err();
        assert!(matches!(
            err,
            PlacementError::InsufficientPeers {
                unknown_domain: 20,
                ..
            }
        ));
    }

    /// Falls back to IP-prefix diversity when the ASN is unknown, which is
    /// weaker than ASN but never wrong.
    #[test]
    fn ip_prefix_substitutes_for_an_unknown_asn() {
        let policy = PlacementPolicy::for_parity(4);
        let peers: Vec<PeerInfo> = (0..14)
            .map(|i| {
                let mut p = peer(&format!("p{i}"), None, 0.95, 0.995);
                p.ip_prefix = Some(format!("10.{i}"));
                p
            })
            .collect();
        assert_eq!(select(&peers, 14, 1024, &policy).unwrap().len(), 14);
    }

    #[test]
    fn prefers_higher_scoring_peers() {
        let policy = PlacementPolicy::for_parity(2); // hot: max 1 per domain
        let mut peers = diverse(6);
        peers[3].reputation = 1.0;
        peers[3].rtt_ms = 5;
        let got = select(&peers, 3, 1024, &policy).unwrap();
        assert!(
            got.iter().any(|a| a.peer.peer_id == "p3"),
            "the best peer should have been chosen"
        );
    }

    /// Latency is a tiebreak, not a driver: a fast but unreliable peer must
    /// not outrank a slower, more available one.
    #[test]
    fn reputation_outweighs_latency() {
        let fast_flaky = PeerInfo {
            rtt_ms: 1,
            reputation: 0.90,
            ..peer("fast", Some(1), 0.90, 0.99)
        };
        let slow_solid = PeerInfo {
            rtt_ms: 400,
            reputation: 1.0,
            ..peer("slow", Some(2), 1.0, 1.0)
        };
        assert!(score(&slow_solid) > score(&fast_flaky));
    }

    #[test]
    fn hot_tier_needs_three_domains() {
        let policy = PlacementPolicy::for_parity(2);
        assert_eq!(policy.max_per_domain, 1);
        assert_eq!(policy.domains_required(3), 3);

        let two_domains: Vec<PeerInfo> = (0..6)
            .map(|i| {
                peer(
                    &format!("p{i}"),
                    Some(if i < 3 { 1 } else { 2 }),
                    0.95,
                    0.995,
                )
            })
            .collect();
        assert!(select(&two_domains, 3, 1024, &policy).is_err());
        assert!(select(&diverse(3), 3, 1024, &policy).is_ok());
    }

    #[test]
    fn nan_scores_do_not_panic() {
        let policy = PlacementPolicy {
            min_reputation: 0.0,
            min_uptime_30d: 0.0,
            ..PlacementPolicy::for_parity(4)
        };
        let mut peers = diverse(14);
        peers[2].reputation = f32::NAN;
        // Whatever the ordering, it must not panic.
        let _ = select(&peers, 14, 1024, &policy);
    }

    #[test]
    fn error_message_names_what_is_missing() {
        let policy = PlacementPolicy::for_parity(4);
        let peers: Vec<PeerInfo> = (0..20)
            .map(|i| peer(&format!("p{i}"), Some(100 + (i as u32 % 3)), 0.95, 0.995))
            .collect();
        let msg = select(&peers, 14, 1024, &policy).unwrap_err().to_string();
        assert!(msg.contains("7 distinct domains"), "unhelpful: {msg}");
        assert!(msg.contains("only 3"), "unhelpful: {msg}");
    }
}
