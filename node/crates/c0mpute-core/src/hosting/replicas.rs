//! The replica map every node keeps: which sites exist (their latest signed
//! record), which peers hold which version, and which peers host at all.
//!
//! Built only from gossip, so every node converges on the same picture with
//! no coordinator. Decisions made from it are deterministic: when a site has
//! too few live copies, every node ranks the candidate peers the same way
//! (rendezvous hashing on site id + peer id) and the top-ranked ones take a
//! copy. Nobody is elected; everybody computes the same answer.
//!
//! Marketplace sites (v2 records) name preferred hosts. A preferred host that
//! is live, asks no more than the agreed price and has not lost preference
//! through failed challenges replicates whether or not the site already has
//! three copies; the rendezvous fill still brings every site to three copies
//! on its own, and once preferred hosts hold copies the surplus free copies
//! are released one at a time. A site never drops below three copies.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use c0mpute_proto::Hash;

use super::proof::{self, Receipt};
use super::site::SiteRecord;
use crate::payments::{Micros, Payee};

/// What one peer says it holds: site id -> (manifest hash, seq).
#[derive(Clone, Debug, Default)]
pub struct Holding {
    pub sites: HashMap<String, (String, u64)>,
    pub seen: Option<Instant>,
    /// A hosting node (its copies count toward the target). A publisher that
    /// is only seeding its own site until the network has copies does not.
    pub hosts: bool,
    /// The hosting price it asks (micro-USD per GiB-month), when it says.
    pub price: Option<Micros>,
    /// Where it wants marketplace payments, when it says.
    pub payout: Option<Payee>,
    /// It hosts but cannot take another copy right now; left out of the
    /// rendezvous fill (its existing copies still count).
    pub full: bool,
}

#[derive(Debug, Default)]
pub struct ReplicaMap {
    /// Latest verified record per site id.
    pub records: HashMap<String, SiteRecord>,
    /// Per hosting peer (base58 peer id): what it holds and when it last said so.
    pub peers: HashMap<String, Holding>,
    /// Smoothed fetch round-trip per peer, in ms.
    pub rtt_ms: HashMap<String, f64>,
    /// Recent availability receipts per (site id, holder), newest last.
    pub receipts: HashMap<(String, String), Vec<Receipt>>,
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
        self.apply_holding_meta(peer, sites, now, hosts, None, None);
    }

    /// A holding list plus what the peer asks for and where it is paid.
    pub fn apply_holding_meta(
        &mut self,
        peer: &str,
        sites: HashMap<String, (String, u64)>,
        now: Instant,
        hosts: bool,
        price: Option<Micros>,
        payout: Option<Payee>,
    ) {
        self.peers.insert(
            peer.to_string(),
            Holding {
                sites,
                seen: Some(now),
                hosts,
                price,
                payout,
                full: false,
            },
        );
    }

    /// Mark whether `peer` can take new copies (see [`Holding::full`]).
    pub fn set_full(&mut self, peer: &str, full: bool) {
        if let Some(h) = self.peers.get_mut(peer) {
            h.full = full;
        }
    }

    /// Keep a verified receipt (deduplicated, newest [`proof::KEEP_PER_HOLDER`]
    /// per holder). Returns true when it was new.
    pub fn apply_receipt(&mut self, r: Receipt) -> bool {
        if !r.verify() {
            return false;
        }
        let list = self.receipts.entry((r.site_id.clone(), r.holder.clone())).or_default();
        if list.iter().any(|x| x.sig == r.sig) {
            return false;
        }
        list.push(r);
        list.sort_by(|a, b| a.at.cmp(&b.at).then(a.sig.cmp(&b.sig)));
        if list.len() > proof::KEEP_PER_HOLDER {
            let extra = list.len() - proof::KEEP_PER_HOLDER;
            list.drain(..extra);
        }
        true
    }

    /// Receipts kept for one holder of one site, oldest first.
    pub fn receipts_for(&self, site_id: &str, holder: &str) -> &[Receipt] {
        self.receipts
            .get(&(site_id.to_string(), holder.to_string()))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// True when `holder` failed too many recent challenges by the site's
    /// owner or payer (see [`proof::demoted`]).
    pub fn is_demoted(&self, site_id: &str, holder: &str) -> bool {
        let Some(rec) = self.records.get(site_id) else {
            return false;
        };
        let trusted = [rec.owner.as_str(), rec.payer_key()];
        proof::demoted(self.receipts_for(site_id, holder), &trusted)
    }

    /// The preferred hosts that count right now, in the record's order: live
    /// hosting peers that ask no more than the agreed price and have not lost
    /// preference.
    pub fn effective_preferred(&self, site_id: &str) -> Vec<String> {
        let Some(rec) = self.records.get(site_id) else {
            return Vec::new();
        };
        let Some(terms) = rec.terms() else {
            return Vec::new();
        };
        terms
            .preferred_hosts
            .into_iter()
            .filter(|p| {
                self.peers.get(p).is_some_and(|h| {
                    h.hosts && h.price.is_none_or(|ask| ask <= terms.price_per_gib_month)
                })
            })
            .filter(|p| !self.is_demoted(site_id, p))
            .take(super::site::MAX_PREFERRED_HOSTS)
            .collect()
    }

    /// Forget peers not heard from within `ttl`.
    pub fn prune(&mut self, now: Instant, ttl: Duration) {
        self.peers
            .retain(|_, h| h.seen.is_some_and(|t| now.duration_since(t) <= ttl));
    }

    /// Live hosting peers that can take a new copy, sorted.
    pub fn candidate_peers(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .peers
            .iter()
            .filter(|(_, h)| h.hosts && !h.full)
            .map(|(p, _)| p.clone())
            .collect();
        v.sort();
        v
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
    let preferred = map.effective_preferred(site_id);
    if i_hold {
        if preferred.is_empty() {
            if surplus(site_id, &holders, max).as_deref() == Some(me) {
                return Action::Drop;
            }
            return Action::Nothing;
        }
        // Marketplace site: free copies make room for paid ones, never below
        // `target` in total. One release per round, lowest rank first.
        let free: Vec<String> = holders.iter().filter(|p| !preferred.contains(p)).cloned().collect();
        let paid = holders.len() - free.len();
        let allowed = target.saturating_sub(paid);
        if free.len() > allowed && surplus(site_id, &free, 0).as_deref() == Some(me) {
            return Action::Drop;
        }
        return Action::Nothing;
    }
    if !i_host {
        return Action::Nothing;
    }
    // A preferred host takes its copy whatever the count: it is paid to.
    if preferred.iter().any(|p| p == me) {
        return Action::Replicate;
    }
    if holders.len() >= target {
        return Action::Nothing;
    }
    // The rendezvous fill. Preferred hosts that do not hold yet are left out:
    // they replicate on their own, and counting on them would leave the site
    // short if one never manages to.
    // Full peers are left out too: one that ranks first but cannot take the
    // copy would otherwise hold the slot from the next ranked node forever.
    let mut candidates: Vec<String> = map
        .candidate_peers()
        .into_iter()
        .filter(|p| !holders.contains(p) && !preferred.contains(p))
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

    #[test]
    fn a_full_peer_that_ranks_first_does_not_hold_the_slot() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let r = rec(&kp, 1);
        let site = r.site_id.clone();
        let mut m = ReplicaMap::default();
        m.apply_record(r);
        let now = Instant::now();
        let peers: Vec<String> = (0..6).map(|i| format!("h{i}")).collect();
        for p in &peers {
            m.apply_holding_meta(p, HashMap::new(), now, true, None, None);
        }
        // Whoever ranks first for this site is full.
        let first = chosen(&site, &peers, 1)[0].clone();
        m.set_full(&first, true);
        let takers: Vec<String> = peers
            .iter()
            .filter(|p| decide(&m, &site, p, !m.peers[*p].full, 3, 5) == Action::Replicate)
            .cloned()
            .collect();
        assert_eq!(takers.len(), 3, "three other nodes fill the site: {takers:?}");
        assert!(!takers.contains(&first));
    }

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

    fn pid() -> String {
        c0mpute_net::libp2p::identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id()
            .to_base58()
    }

    fn paid_rec(kp: &c0mpute_net::libp2p::identity::Keypair, preferred: Vec<String>, price: u64) -> SiteRecord {
        SiteRecord::sign_with_terms(
            kp,
            "paid",
            &Hash::of(b"paid"),
            1,
            10,
            vec![],
            Some(super::super::site::HostingTerms {
                preferred_hosts: preferred,
                price_per_gib_month: price,
                payer: None,
            }),
        )
        .unwrap()
    }

    /// Run repair rounds over a shared map until nothing changes; every peer
    /// decides from the same map, like the real network once gossip settles.
    /// Returns the holder count after each round.
    fn settle(m: &mut ReplicaMap, site: &str, manifest: &str, peers: &[String]) -> Vec<usize> {
        let mut counts = Vec::new();
        for _ in 0..20 {
            let actions: Vec<(String, Action)> = peers
                .iter()
                .map(|p| (p.clone(), decide(m, site, p, true, 3, 5)))
                .collect();
            if actions.iter().all(|(_, a)| *a == Action::Nothing) {
                break;
            }
            let now = Instant::now();
            for (p, a) in actions {
                let mut h = m.peers.get(&p).cloned().unwrap_or_default();
                match a {
                    Action::Replicate => {
                        h.sites.insert(site.to_string(), (manifest.to_string(), 1));
                    }
                    Action::Drop => {
                        h.sites.remove(site);
                    }
                    Action::Nothing => {}
                }
                m.apply_holding_meta(&p, h.sites, now, true, h.price, h.payout);
            }
            counts.push(m.holders(site).len());
        }
        counts
    }

    fn hosting_peers(m: &mut ReplicaMap, peers: &[String]) {
        for p in peers {
            m.apply_holding(p, HashMap::new(), Instant::now());
        }
    }

    #[test]
    fn preferred_hosts_replicate_first_and_the_site_settles_at_three() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let peers: Vec<String> = (0..5).map(|_| pid()).collect();
        let preferred = vec![peers[1].clone(), peers[3].clone()];
        let r = paid_rec(&kp, preferred.clone(), 4_000);
        let mut m = ReplicaMap::default();
        m.apply_record(r.clone());
        hosting_peers(&mut m, &peers);
        assert_eq!(m.effective_preferred(&r.site_id), preferred);
        // Round one: both preferred hosts replicate, whatever their rank.
        for p in &preferred {
            assert_eq!(decide(&m, &r.site_id, p, true, 3, 5), Action::Replicate);
        }
        let counts = settle(&mut m, &r.site_id, &r.manifest, &peers);
        assert!(counts.iter().all(|c| *c >= 3), "never below three copies: {counts:?}");
        let holders = m.holders(&r.site_id);
        assert_eq!(holders.len(), 3, "settles at the target: {counts:?}");
        for p in &preferred {
            assert!(holders.contains(p), "preferred {p} holds a copy");
        }
    }

    #[test]
    fn preferred_placement_is_the_same_on_every_node() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let peers: Vec<String> = (0..7).map(|_| pid()).collect();
        let r = paid_rec(&kp, vec![peers[6].clone()], 4_000);
        let run = |order: Vec<String>| {
            let mut m = ReplicaMap::default();
            m.apply_record(r.clone());
            hosting_peers(&mut m, &order);
            settle(&mut m, &r.site_id, &r.manifest, &order);
            m.holders(&r.site_id)
        };
        let a = run(peers.clone());
        let mut rev = peers.clone();
        rev.reverse();
        let b = run(rev);
        assert_eq!(a, b, "same inputs, same copies, whatever order peers are seen in");
        assert_eq!(a.len(), 3);
        assert!(a.contains(&peers[6]));
    }

    #[test]
    fn without_live_preferred_hosts_the_fill_still_keeps_three() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let peers: Vec<String> = (0..5).map(|_| pid()).collect();
        // Both preferred hosts are offline (not in the map).
        let r = paid_rec(&kp, vec![pid(), pid()], 4_000);
        let mut m = ReplicaMap::default();
        m.apply_record(r.clone());
        hosting_peers(&mut m, &peers);
        assert!(m.effective_preferred(&r.site_id).is_empty());
        settle(&mut m, &r.site_id, &r.manifest, &peers);
        assert_eq!(m.holders(&r.site_id).len(), 3);
    }

    #[test]
    fn a_preferred_host_asking_more_than_the_agreed_price_is_not_preferred() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let peers: Vec<String> = (0..4).map(|_| pid()).collect();
        let r = paid_rec(&kp, vec![peers[0].clone(), peers[1].clone()], 4_000);
        let mut m = ReplicaMap::default();
        m.apply_record(r.clone());
        hosting_peers(&mut m, &peers);
        m.apply_holding_meta(&peers[0], HashMap::new(), Instant::now(), true, Some(9_000), None);
        m.apply_holding_meta(&peers[1], HashMap::new(), Instant::now(), true, Some(4_000), None);
        assert_eq!(m.effective_preferred(&r.site_id), vec![peers[1].clone()]);
    }

    #[test]
    fn a_preferred_host_that_fails_challenges_loses_preference() {
        use super::super::proof::{Challenge, Receipt};
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let peers: Vec<String> = (0..5).map(|_| pid()).collect();
        let bad = peers[2].clone();
        let r = paid_rec(&kp, vec![bad.clone()], 4_000);
        let mut m = ReplicaMap::default();
        m.apply_record(r.clone());
        hosting_peers(&mut m, &peers);
        settle(&mut m, &r.site_id, &r.manifest, &peers);
        assert!(m.holders(&r.site_id).contains(&bad));
        let chunk = Hash::of(b"c").to_hex();
        let fail = |signer: &c0mpute_net::libp2p::identity::Keypair, at: u64| {
            Receipt::sign(
                signer,
                Challenge {
                    site_id: &r.site_id,
                    manifest: &r.manifest,
                    holder: &bad,
                    chunk: &chunk,
                    ok: false,
                    rtt_ms: 0,
                    at,
                },
            )
            .unwrap()
        };
        // A stranger's failures change nothing.
        let stranger = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        for at in 0..5 {
            assert!(m.apply_receipt(fail(&stranger, at)));
        }
        assert_eq!(m.effective_preferred(&r.site_id), vec![bad.clone()]);
        // Three failures signed by the owner: demoted.
        for at in 10..13 {
            assert!(m.apply_receipt(fail(&kp, at)));
        }
        assert!(!m.apply_receipt(fail(&kp, 12)), "duplicates are ignored");
        assert!(m.is_demoted(&r.site_id, &bad));
        assert!(m.effective_preferred(&r.site_id).is_empty());
        // The site still has three copies; the demoted host is now an
        // ordinary holder.
        settle(&mut m, &r.site_id, &r.manifest, &peers);
        assert!(m.holders(&r.site_id).len() >= 3);
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
