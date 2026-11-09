//! The replica map every node keeps: which sites exist (their latest signed
//! record), which peers hold which version, and which peers host at all.
//!
//! Built only from gossip, so every node converges on the same picture with
//! no coordinator. Decisions made from it are deterministic: when a site has
//! too few live copies, every node ranks the candidate peers the same way
//! (rendezvous hashing on site id + peer id) and the top-ranked ones take a
//! copy. Nobody is elected; everybody computes the same answer.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use c0mpute_proto::Hash;

use super::site::SiteRecord;

/// What one peer says it holds: site id -> (manifest hash, seq).
#[derive(Clone, Debug, Default)]
pub struct Holding {
    pub sites: HashMap<String, (String, u64)>,
    pub seen: Option<Instant>,
    /// A hosting node (its copies count toward the target). A publisher that
    /// is only seeding its own site until the network has copies does not.
    pub hosts: bool,
}

#[derive(Debug, Default)]
pub struct ReplicaMap {
    /// Latest verified record per site id.
    pub records: HashMap<String, SiteRecord>,
    /// Per hosting peer (base58 peer id): what it holds and when it last said so.
    pub peers: HashMap<String, Holding>,
    /// Smoothed fetch round-trip per peer, in ms.
    pub rtt_ms: HashMap<String, f64>,
}

impl ReplicaMap {
    /// Keep a record if it verifies and is newer than what we have. Returns
    /// true when it changed the map.
    pub fn apply_record(&mut self, rec: SiteRecord) -> bool {
        if !rec.verify() {
            return false;
        }
        match self.records.get(&rec.site_id) {
            Some(cur) if cur.seq >= rec.seq => false,
            _ => {
                self.records.insert(rec.site_id.clone(), rec);
                true
            }
        }
    }

    /// A peer's full holding list (replaces what it said before).
    pub fn apply_holding(&mut self, peer: &str, sites: HashMap<String, (String, u64)>, now: Instant) {
        self.apply_holding_as(peer, sites, now, true);
    }

    pub fn apply_holding_as(
        &mut self,
        peer: &str,
        sites: HashMap<String, (String, u64)>,
        now: Instant,
        hosts: bool,
    ) {
        self.peers.insert(
            peer.to_string(),
            Holding {
                sites,
                seen: Some(now),
                hosts,
            },
        );
    }

    /// Forget peers not heard from within `ttl`.
    pub fn prune(&mut self, now: Instant, ttl: Duration) {
        self.peers
            .retain(|_, h| h.seen.is_some_and(|t| now.duration_since(t) <= ttl));
    }

    /// Live hosting peers, sorted.
    pub fn live_peers(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .peers
            .iter()
            .filter(|(_, h)| h.hosts)
            .map(|(p, _)| p.clone())
            .collect();
        v.sort();
        v
    }

    /// Hosting peers holding the current version of `site_id`, sorted: the
    /// copies that count.
    pub fn holders(&self, site_id: &str) -> Vec<String> {
        self.with_current(site_id, true)
    }

    /// Every peer that can serve the current version (holders plus seeding
    /// publishers), sorted: where to fetch from.
    pub fn sources(&self, site_id: &str) -> Vec<String> {
        self.with_current(site_id, false)
    }

    fn with_current(&self, site_id: &str, hosting_only: bool) -> Vec<String> {
        let Some(rec) = self.records.get(site_id) else {
            return Vec::new();
        };
        let mut v: Vec<String> = self
            .peers
            .iter()
            .filter(|(_, h)| !hosting_only || h.hosts)
            .filter(|(_, h)| {
                h.sites
                    .get(site_id)
                    .is_some_and(|(m, _)| *m == rec.manifest)
            })
            .map(|(p, _)| p.clone())
            .collect();
        v.sort();
        v
    }

    /// Record a fetch round-trip to `peer` (exponential moving average).
    pub fn observe_rtt(&mut self, peer: &str, ms: f64) {
        let e = self.rtt_ms.entry(peer.to_string()).or_insert(ms);
        *e = *e * 0.7 + ms * 0.3;
    }

    /// The order to try holders in: fastest first by measured round-trip,
    /// unmeasured peers in the middle, rotated by `turn` among peers of
    /// similar speed so load spreads (round-robin within a speed band).
    pub fn proxy_order(&self, holders: &[String], turn: usize) -> Vec<String> {
        if holders.is_empty() {
            return Vec::new();
        }
        let known: Vec<f64> = holders.iter().filter_map(|p| self.rtt_ms.get(p).copied()).collect();
        let median = if known.is_empty() {
            100.0
        } else {
            let mut k = known.clone();
            k.sort_by(|a, b| a.total_cmp(b));
            k[k.len() / 2]
        };
        // Band = round-trip bucketed to 25 ms; within a band, rotate.
        let mut v: Vec<(u64, String)> = holders
            .iter()
            .map(|p| {
                let ms = self.rtt_ms.get(p).copied().unwrap_or(median);
                ((ms / 25.0).floor() as u64, p.clone())
            })
            .collect();
        v.sort();
        let mut out = Vec::with_capacity(v.len());
        let mut i = 0;
        while i < v.len() {
            let band = v[i].0;
            let mut j = i;
            while j < v.len() && v[j].0 == band {
                j += 1;
            }
            let group: Vec<String> = v[i..j].iter().map(|(_, p)| p.clone()).collect();
            let r = turn % group.len();
            out.extend(group[r..].iter().cloned());
            out.extend(group[..r].iter().cloned());
            i = j;
        }
        out
    }
}

/// Rendezvous score: higher ranks first. Same inputs, same answer, on every
/// node.
pub fn score(site_id: &str, peer: &str) -> [u8; 32] {
    Hash::of(format!("c0mpute-hosting-rank\n{site_id}\n{peer}").as_bytes()).0
}

/// Of `candidates` (peers that host but do not hold the site), the `need`
/// that should take a copy.
pub fn chosen(site_id: &str, candidates: &[String], need: usize) -> Vec<String> {
    let mut ranked: Vec<(&String, [u8; 32])> =
        candidates.iter().map(|p| (p, score(site_id, p))).collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    ranked.into_iter().take(need).map(|(p, _)| p.clone()).collect()
}

/// When a site has more than `max` live copies, the lowest-ranked holder
/// lets its copy go. Returns that holder.
pub fn surplus(site_id: &str, holders: &[String], max: usize) -> Option<String> {
    if holders.len() <= max {
        return None;
    }
    holders
        .iter()
        .min_by(|a, b| score(site_id, a).cmp(&score(site_id, b)).then(b.cmp(a)))
        .cloned()
}

/// What this node should do about one site, given the map.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Nothing,
    /// Fetch the current version and start holding it.
    Replicate,
    /// Drop our copy (too many copies, and we rank last).
    Drop,
}

/// Decide for `me` (which may or may not host).
pub fn decide(map: &ReplicaMap, site_id: &str, me: &str, i_host: bool, target: usize, max: usize) -> Action {
    let holders = map.holders(site_id);
    let i_hold = holders.iter().any(|p| p == me);
    if i_hold {
        if surplus(site_id, &holders, max).as_deref() == Some(me) {
            return Action::Drop;
        }
        return Action::Nothing;
    }
    if !i_host || holders.len() >= target {
        return Action::Nothing;
    }
    let mut candidates: Vec<String> = map
        .live_peers()
        .into_iter()
        .filter(|p| !holders.contains(p))
        .collect();
    if !candidates.iter().any(|p| p == me) {
        candidates.push(me.to_string());
    }
    if chosen(site_id, &candidates, target - holders.len())
        .iter()
        .any(|p| p == me)
    {
        Action::Replicate
    } else {
        Action::Nothing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(kp: &c0mpute_net::libp2p::identity::Keypair, seq: u64) -> SiteRecord {
        SiteRecord::sign(kp, "site", &Hash::of(&seq.to_le_bytes()), seq, 10, vec![]).unwrap()
    }

    #[test]
    fn newer_signed_records_win() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let mut m = ReplicaMap::default();
        assert!(m.apply_record(rec(&kp, 2)));
        assert!(!m.apply_record(rec(&kp, 1)), "older seq is ignored");
        assert!(m.apply_record(rec(&kp, 3)));
        let mut forged = rec(&kp, 9);
        forged.manifest = Hash::of(b"evil").to_hex();
        assert!(!m.apply_record(forged), "bad signature is ignored");
        assert_eq!(m.records.values().next().unwrap().seq, 3);
    }

    #[test]
    fn every_node_agrees_who_replicates() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let r = rec(&kp, 1);
        let site = r.site_id.clone();
        let mut m = ReplicaMap::default();
        m.apply_record(r.clone());
        let now = Instant::now();
        let peers: Vec<String> = (0..8).map(|i| format!("peer{i}")).collect();
        // peer0 holds it; the rest host nothing yet.
        for p in &peers {
            let mut sites = HashMap::new();
            if p == "peer0" {
                sites.insert(site.clone(), (r.manifest.clone(), 1));
            }
            m.apply_holding(p, sites, now);
        }
        let takers: Vec<&String> = peers
            .iter()
            .filter(|p| decide(&m, &site, p, true, 3, 5) == Action::Replicate)
            .collect();
        assert_eq!(takers.len(), 2, "exactly the missing two take a copy: {takers:?}");
        assert!(!takers.iter().any(|p| *p == "peer0"));
        // A node that does not host never takes one.
        assert_eq!(decide(&m, &site, "outsider", false, 3, 5), Action::Nothing);
    }

    #[test]
    fn a_seeding_publisher_is_a_source_not_a_copy() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let r = rec(&kp, 1);
        let site = r.site_id.clone();
        let mut m = ReplicaMap::default();
        m.apply_record(r.clone());
        let now = Instant::now();
        let mut sites = HashMap::new();
        sites.insert(site.clone(), (r.manifest.clone(), 1));
        m.apply_holding_as("publisher", sites, now, false);
        for i in 0..5 {
            m.apply_holding(&format!("h{i}"), HashMap::new(), now);
        }
        assert!(m.holders(&site).is_empty());
        assert_eq!(m.sources(&site), vec!["publisher".to_string()]);
        let takers = (0..5)
            .filter(|i| decide(&m, &site, &format!("h{i}"), true, 3, 5) == Action::Replicate)
            .count();
        assert_eq!(takers, 3, "all three copies come from hosting nodes");
    }

    #[test]
    fn surplus_copy_is_dropped_by_exactly_one() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let r = rec(&kp, 1);
        let site = r.site_id.clone();
        let mut m = ReplicaMap::default();
        m.apply_record(r.clone());
        let now = Instant::now();
        for i in 0..7 {
            let mut sites = HashMap::new();
            sites.insert(site.clone(), (r.manifest.clone(), 1));
            m.apply_holding(&format!("p{i}"), sites, now);
        }
        let droppers: Vec<String> = (0..7)
            .map(|i| format!("p{i}"))
            .filter(|p| decide(&m, &site, p, true, 3, 5) == Action::Drop)
            .collect();
        assert_eq!(droppers.len(), 1);
    }

    #[test]
    fn stale_peers_are_pruned_and_old_versions_do_not_count() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let mut m = ReplicaMap::default();
        let r1 = rec(&kp, 1);
        let site = r1.site_id.clone();
        m.apply_record(r1.clone());
        let t0 = Instant::now();
        let mut sites = HashMap::new();
        sites.insert(site.clone(), (r1.manifest.clone(), 1));
        m.apply_holding("a", sites, t0);
        assert_eq!(m.holders(&site), vec!["a".to_string()]);
        m.apply_record(rec(&kp, 2));
        assert!(m.holders(&site).is_empty(), "holding an old version is not a copy");
        m.prune(t0 + Duration::from_secs(500), Duration::from_secs(200));
        assert!(m.live_peers().is_empty());
    }

    #[test]
    fn proxy_order_prefers_fast_and_rotates_ties() {
        let mut m = ReplicaMap::default();
        m.observe_rtt("slow", 400.0);
        m.observe_rtt("fast1", 10.0);
        m.observe_rtt("fast2", 12.0);
        let holders = vec!["slow".to_string(), "fast1".to_string(), "fast2".to_string()];
        let a = m.proxy_order(&holders, 0);
        let b = m.proxy_order(&holders, 1);
        assert_eq!(a.last().unwrap(), "slow");
        assert_eq!(b.last().unwrap(), "slow");
        assert_ne!(a[0], b[0], "equal-speed holders take turns");
    }
}
