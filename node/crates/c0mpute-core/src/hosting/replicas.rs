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
//!
//! Free sites settle at three too: a copy beyond the target (two nodes
//! filling the same gap from views a moment apart) is released by the
//! lowest-ranked free holder once the site has been over the target for a
//! grace period, and at once above the hard ceiling ([`releases`]).

use std::collections::{BTreeSet, HashMap};
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
    /// Free disk above its floor, GiB, when it says. More room ranks higher
    /// in the fill (see [`space_tier`]).
    pub room_gib: Option<u64>,
    /// Striped sites it holds only some groups of: site id -> groups. A site
    /// in `sites` but not here is held whole.
    pub groups: HashMap<String, BTreeSet<u32>>,
    /// Bytes it can still take. Present only from nodes that place striped
    /// sites, so older nodes are never counted on for a group.
    pub room: Option<u64>,
    /// Where to dial it (announced multiaddrs).
    pub addrs: Vec<String>,
    /// Its failure-domain id (machine + disk), when it says.
    pub machine: Option<String>,
}

/// Site records and receipt lists a node keeps. Both come signed, but keys
/// are free, so neither table is open-ended: past the cap, entries for sites
/// nobody announces go first, and then new ones are refused.
pub const MAX_RECORDS: usize = 200_000;
pub const MAX_RECEIPT_LISTS: usize = 200_000;
/// Proofs (see [`Proof`]) a node remembers. Past the cap the one passed
/// longest ago makes room.
pub const MAX_PROOFS: usize = 200_000;

/// What this node has itself seen one holder of one site prove: it asked the
/// holder for chunks it picked at random and got the right bytes back. A
/// holding announcement is only a claim (any key can gossip one); this is
/// what makes a claimed copy count.
#[derive(Clone, Debug)]
pub struct Proof {
    /// The version the passes were for. A new version starts over.
    pub manifest: String,
    /// The first pass of this run of passes, and the latest.
    pub first: Instant,
    pub last: Instant,
    /// Passes counted in the run. At most one is counted per gap (see
    /// [`ReplicaMap::note_pass`]), so three passes are three separate times.
    pub passes: u32,
    /// When a pass was last counted.
    counted: Instant,
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
    /// Challenges this node saw pass, per (site id, holder).
    pub proofs: HashMap<(String, String), Proof>,
    /// Count a holder on its announcement alone, as every version up to
    /// 0.3.64 did (`[hosting] require_proofs` off, the default). With it
    /// false a holder counts only once proven ([`Self::proven`]) and may only
    /// be relied on for a drop once it has standing ([`Self::has_standing`]).
    pub count_claims: bool,
}

impl ReplicaMap {
    /// Keep a record if it verifies and is newer than what we have. Returns
    /// true when it changed the map.
    pub fn apply_record(&mut self, rec: SiteRecord) -> bool {
        self.apply_record_capped(rec, MAX_RECORDS)
    }

    /// [`Self::apply_record`] with the table's cap given (tests use a small one).
    pub(crate) fn apply_record_capped(&mut self, rec: SiteRecord, max: usize) -> bool {
        if !rec.verify() {
            return false;
        }
        match self.records.get(&rec.site_id) {
            Some(cur) if cur.seq >= rec.seq => false,
            Some(_) => {
                self.records.insert(rec.site_id.clone(), rec);
                true
            }
            None => {
                if self.records.len() >= max {
                    // Make room from the sites no live peer announces.
                    let announced: std::collections::HashSet<&String> =
                        self.peers.values().flat_map(|h| h.sites.keys()).collect();
                    let unheld: Vec<String> =
                        self.records.keys().filter(|s| !announced.contains(s)).cloned().collect();
                    for s in unheld {
                        self.records.remove(&s);
                    }
                    if self.records.len() >= max {
                        return false;
                    }
                }
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
        self.apply_holding_full(peer, sites, HashMap::new(), now, hosts, price, payout, None, Vec::new());
    }

    /// A holding list with everything a current node announces.
    #[allow(clippy::too_many_arguments)]
    pub fn apply_holding_full(
        &mut self,
        peer: &str,
        sites: HashMap<String, (String, u64)>,
        groups: HashMap<String, BTreeSet<u32>>,
        now: Instant,
        hosts: bool,
        price: Option<Micros>,
        payout: Option<Payee>,
        room: Option<u64>,
        addrs: Vec<String>,
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
                room_gib: None,
                groups,
                room,
                addrs,
                machine: None,
            },
        );
    }

    /// Record the failure domain `peer` announced (see [`Holding::machine`]).
    pub fn set_machine(&mut self, peer: &str, machine: Option<String>) {
        if let Some(h) = self.peers.get_mut(peer) {
            h.machine = machine;
        }
    }

    /// Record how much room `peer` says it has (see [`Holding::room_gib`]).
    pub fn set_room(&mut self, peer: &str, room_gib: Option<u64>) {
        if let Some(h) = self.peers.get_mut(peer) {
            h.room_gib = room_gib;
        }
    }

    /// Mark whether `peer` can take new copies (see [`Holding::full`]).
    pub fn set_full(&mut self, peer: &str, full: bool) {
        if let Some(h) = self.peers.get_mut(peer) {
            h.full = full;
        }
    }

    /// Live hosting peers that place striped sites (they announce `room`),
    /// sorted.
    pub fn striping_peers(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .peers
            .iter()
            .filter(|(_, h)| h.hosts && h.room.is_some())
            .map(|(p, _)| p.clone())
            .collect();
        v.sort();
        v
    }

    /// The room a peer announced.
    pub fn room_of(&self, peer: &str) -> Option<u64> {
        self.peers.get(peer).and_then(|h| h.room)
    }

    /// For each of a striped site's `n` groups, the peers holding it in the
    /// current version, sorted. `hosting_only` = copies that count (else
    /// every source, seeding publishers included). A peer that holds the site
    /// whole holds every group.
    pub fn group_holders(&self, site_id: &str, n: usize, hosting_only: bool) -> Vec<Vec<String>> {
        let mut out = vec![Vec::new(); n];
        let Some(rec) = self.records.get(site_id) else {
            return out;
        };
        let mut peers: Vec<(&String, &Holding)> = self.peers.iter().collect();
        peers.sort_by(|a, b| a.0.cmp(b.0));
        for (p, h) in peers {
            if hosting_only && !h.hosts {
                continue;
            }
            if !h.sites.get(site_id).is_some_and(|(m, _)| *m == rec.manifest) {
                continue;
            }
            match h.groups.get(site_id) {
                Some(gs) => {
                    for g in gs {
                        if let Some(v) = out.get_mut(*g as usize) {
                            v.push(p.clone());
                        }
                    }
                }
                None => {
                    for v in out.iter_mut() {
                        v.push(p.clone());
                    }
                }
            }
        }
        out
    }

    /// Every source of one group of a striped site (holders and seeding
    /// publishers), sorted.
    pub fn group_sources(&self, site_id: &str, g: u32) -> Vec<String> {
        self.sources(site_id)
            .into_iter()
            .filter(|p| {
                self.peers
                    .get(p)
                    .is_some_and(|h| h.groups.get(site_id).is_none_or(|gs| gs.contains(&g)))
            })
            .collect()
    }

    /// Announced addresses of a peer.
    pub fn addrs_of(&self, peer: &str) -> &[String] {
        self.peers.get(peer).map(|h| h.addrs.as_slice()).unwrap_or(&[])
    }

    /// Keep a verified receipt (deduplicated, newest [`proof::KEEP_PER_HOLDER`]
    /// per holder). Returns true when it was new.
    pub fn apply_receipt(&mut self, r: Receipt) -> bool {
        if !r.verify() {
            return false;
        }
        let key = (r.site_id.clone(), r.holder.clone());
        if !self.receipts.contains_key(&key) && self.receipts.len() >= MAX_RECEIPT_LISTS {
            // Make room from the sites we have no record of.
            let records = &self.records;
            self.receipts.retain(|(site, _), _| records.contains_key(site));
            if self.receipts.len() >= MAX_RECEIPT_LISTS {
                return false;
            }
        }
        let list = self.receipts.entry(key).or_default();
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

    /// `holder` just passed a challenge by this node for `manifest` of
    /// `site_id`. A pass less than `gap` after the last counted one renews
    /// the proof without adding to the count; a run with no pass for `lapse`
    /// (or for another version) starts over.
    pub fn note_pass(&mut self, site_id: &str, holder: &str, manifest: &str, now: Instant, gap: Duration, lapse: Duration) {
        self.note_pass_capped(site_id, holder, manifest, now, gap, lapse, MAX_PROOFS);
    }

    /// [`Self::note_pass`] with the table's cap given (tests use a small one).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn note_pass_capped(
        &mut self,
        site_id: &str,
        holder: &str,
        manifest: &str,
        now: Instant,
        gap: Duration,
        lapse: Duration,
        max: usize,
    ) {
        let key = (site_id.to_string(), holder.to_string());
        if let Some(p) = self.proofs.get_mut(&key)
            && p.manifest == manifest
            && now.duration_since(p.last) <= lapse
        {
            p.last = now;
            if now.duration_since(p.counted) >= gap {
                p.passes += 1;
                p.counted = now;
            }
            return;
        }
        if !self.proofs.contains_key(&key) && self.proofs.len() >= max {
            let oldest = self.proofs.iter().min_by_key(|(_, p)| p.last).map(|(k, _)| k.clone());
            if let Some(k) = oldest {
                self.proofs.remove(&k);
            }
        }
        self.proofs.insert(
            key,
            Proof {
                manifest: manifest.to_string(),
                first: now,
                last: now,
                passes: 1,
                counted: now,
            },
        );
    }

    /// Forget proofs with no pass for `lapse`, and those of sites no longer
    /// known.
    pub fn prune_proofs(&mut self, now: Instant, lapse: Duration) {
        let records = &self.records;
        self.proofs
            .retain(|(site, _), p| now.duration_since(p.last) <= lapse && records.contains_key(site));
    }

    /// Whether `holder`'s copy of the current version of `site_id` counts
    /// toward the copies the site needs: this node saw it pass a challenge
    /// within `ttl`, or the latest receipt for it signed by the site's owner
    /// or payer (the keys [`proof::demoted`] trusts) is a pass no older than
    /// `ttl`. `now_secs` is the wall clock receipts are dated with.
    pub fn proven(&self, site_id: &str, holder: &str, now: Instant, now_secs: u64, ttl: Duration) -> bool {
        if self.count_claims {
            return true;
        }
        let Some(rec) = self.records.get(site_id) else {
            return false;
        };
        if self
            .proofs
            .get(&(site_id.to_string(), holder.to_string()))
            .is_some_and(|p| p.manifest == rec.manifest && now.duration_since(p.last) <= ttl)
        {
            return true;
        }
        let trusted = [rec.owner.as_str(), rec.payer_key()];
        self.receipts_for(site_id, holder)
            .iter()
            .filter(|r| r.manifest == rec.manifest && trusted.contains(&r.challenger.as_str()))
            // A receipt dated ahead of this node's clock is not fresher
            // than one dated now.
            .filter(|r| r.at <= now_secs.saturating_add(proof::CLOCK_SKEW_SECS))
            .max_by(|a, b| a.at.cmp(&b.at).then(a.sig.cmp(&b.sig)))
            .is_some_and(|r| r.ok && now_secs.saturating_sub(r.at) <= ttl.as_secs())
    }

    /// Whether `holder` has the standing a copy needs before this node gives
    /// up its own on the strength of it: this node saw it pass at least
    /// `passes` challenges at separate times, the first at least `span` ago,
    /// with no gap of `span` between passes since. Receipts from other keys
    /// do not count here: only what this node asked for itself.
    pub fn has_standing(&self, site_id: &str, holder: &str, now: Instant, passes: u32, span: Duration) -> bool {
        if self.count_claims {
            return true;
        }
        let Some(rec) = self.records.get(site_id) else {
            return false;
        };
        self.proofs
            .get(&(site_id.to_string(), holder.to_string()))
            .is_some_and(|p| {
                p.manifest == rec.manifest
                    && p.passes >= passes
                    && now.duration_since(p.first) >= span
                    && now.duration_since(p.last) <= span
            })
    }

    /// Whether a pass by `holder` now would be counted (see
    /// [`Self::note_pass`]): nothing counted for the current version within
    /// `gap`. Asking sooner proves nothing new.
    pub fn pass_due(&self, site_id: &str, holder: &str, now: Instant, gap: Duration) -> bool {
        let Some(rec) = self.records.get(site_id) else {
            return false;
        };
        self.proofs
            .get(&(site_id.to_string(), holder.to_string()))
            .is_none_or(|p| p.manifest != rec.manifest || now.duration_since(p.counted) >= gap)
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

    /// Where to fetch a site's chunks from by hash: [`Self::sources`] first,
    /// then every peer announcing any other version of the site. Right after
    /// a new version lands nobody has announced it yet, but the holders of
    /// the previous version still have every chunk the two share; fetched
    /// bytes are checked against their hash, so a stale holder is safe.
    pub fn fetch_sources(&self, site_id: &str) -> Vec<String> {
        let mut v = self.sources(site_id);
        let mut rest: Vec<String> = self
            .peers
            .iter()
            .filter(|(p, h)| h.sites.contains_key(site_id) && !v.contains(p))
            .map(|(p, _)| p.clone())
            .collect();
        rest.sort();
        v.extend(rest);
        v
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

/// Where a peer's copies live: the failure-domain id it announced, and the
/// globally unique IPs it announced for direct dials (relayed `p2p-circuit`
/// addresses name the relay, not the peer, and private ones repeat across
/// machines).
#[derive(Clone, Debug, Default)]
pub struct Machine {
    pub domain: Option<String>,
    pub ips: Vec<std::net::IpAddr>,
}

/// [`Machine`] per peer.
pub type Machines = HashMap<String, Machine>;

impl ReplicaMap {
    /// [`Machines`] for every known peer.
    pub fn machines(&self) -> Machines {
        self.peers
            .iter()
            .map(|(p, h)| {
                let mut ips: Vec<std::net::IpAddr> = h
                    .addrs
                    .iter()
                    .filter(|a| !a.contains("p2p-circuit"))
                    .filter_map(|a| a.parse::<c0mpute_net::Multiaddr>().ok())
                    .filter_map(|m| super::unique_ip(&m))
                    .collect();
                ips.sort();
                ips.dedup();
                (p.clone(), Machine { domain: h.machine.clone(), ips })
            })
            .collect()
    }

    /// Per group of a striped site, its holders with at most one per
    /// machine (see [`one_per_machine`]): what a copy count means. Two
    /// nodes on one box are one copy: the box going away takes both.
    pub fn distinct_group_holders(&self, site_id: &str, n: usize) -> Vec<Vec<String>> {
        let machines = self.machines();
        let preferred = self.effective_preferred(site_id);
        self.group_holders(site_id, n, true)
            .into_iter()
            .enumerate()
            .map(|(g, hs)| one_per_machine(&machines, &super::stripe::group_key(site_id, g as u32), &hs, &preferred).0)
            .collect()
    }
}

/// Whether `a` and `b` run on one machine (they share a unique IP).
pub fn same_machine(machines: &Machines, a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    match (machines.get(a), machines.get(b)) {
        // Both say which machine and disk: that settles it (two homes
        // behind one NAT address are still two copies).
        (Some(Machine { domain: Some(x), .. }), Some(Machine { domain: Some(y), .. })) => x == y,
        (Some(x), Some(y)) => x.ips.iter().any(|ip| y.ips.contains(ip)),
        _ => false,
    }
}

/// Split `holders` of one site or group (`key`) into one holder per machine
/// and the extra holders on a machine already counted. Per machine the
/// keeper is a `keep_first` peer (a paid host) if there is one, else the
/// one ranking highest for `key`, so every node picks the same keeper.
pub fn one_per_machine(
    machines: &Machines,
    key: &str,
    holders: &[String],
    keep_first: &[String],
) -> (Vec<String>, Vec<String>) {
    if holders.len() < 2 {
        return (holders.to_vec(), Vec::new());
    }
    let mut ranked: Vec<&String> = holders.iter().collect();
    ranked.sort_by(|a, b| {
        let pa = keep_first.contains(*a);
        let pb = keep_first.contains(*b);
        pb.cmp(&pa).then(score(key, b).cmp(&score(key, a))).then(a.cmp(b))
    });
    let mut keep: Vec<String> = Vec::new();
    let mut extra: Vec<String> = Vec::new();
    for p in ranked {
        if keep.iter().any(|k| same_machine(machines, k, p)) {
            extra.push(p.clone());
        } else {
            keep.push(p.clone());
        }
    }
    keep.sort();
    extra.sort();
    (keep, extra)
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

/// A peer's priority tier from the room it reports: the bit length of its
/// free GiB, so a node with about twice the room ranks one tier higher. A
/// peer that does not say (an older node) sits at the 64 GiB tier. Integer
/// only, so every node computes the same tiers.
pub fn space_tier(room_gib: Option<u64>) -> u32 {
    let gib = room_gib.unwrap_or(64);
    64 - gib.leading_zeros()
}

/// Like [`chosen`], but nodes with more free disk come first: candidates are
/// ordered by [`space_tier`], then by rendezvous score within a tier, so the
/// biggest disks take copies first while load still spreads among equals.
pub fn chosen_by_space(map: &ReplicaMap, site_id: &str, candidates: &[String], need: usize) -> Vec<String> {
    let tier = |p: &String| space_tier(map.peers.get(p).and_then(|h| h.room_gib));
    let mut ranked: Vec<(&String, u32, [u8; 32])> =
        candidates.iter().map(|p| (p, tier(p), score(site_id, p))).collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.cmp(&a.2)).then(a.0.cmp(b.0)));
    ranked.into_iter().take(need).map(|(p, _, _)| p.clone()).collect()
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

/// Whether `me`, one of the `free` holders (copies nobody is paid to keep),
/// lets its copy go. `holders` counts every current copy, paid ones included.
///
/// Free copies are released lowest rendezvous rank first, one per round, and
/// never below `target` copies in total; a paid copy is never released. Above
/// `max` copies the release is immediate (the hard ceiling). Between `target`
/// and `max` it waits for `trim`: the caller sets it once the site (or group)
/// has been over `target` without a break for a grace period, so a copy that
/// is only briefly extra (two nodes filling the same gap, a view that has not
/// caught up yet) does not churn.
pub fn releases(key: &str, holders: usize, free: &[String], me: &str, target: usize, max: usize, trim: bool) -> bool {
    let paid = holders.saturating_sub(free.len());
    if free.len() <= target.saturating_sub(paid) {
        return false;
    }
    if holders <= max && !trim {
        return false;
    }
    surplus(key, free, 0).as_deref() == Some(me)
}

/// What this node should do about one site, given the map.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Nothing,
    /// Fetch the current version and start holding it.
    Replicate,
    /// Drop our copy (more copies than needed, and ours is the lowest-ranked
    /// free one).
    Drop,
}

/// Decide for `me` (which may or may not host), taking every announced
/// holder at its word. `trim`: the site has had more than `target` copies for
/// the whole grace period (see [`releases`]).
pub fn decide(
    map: &ReplicaMap,
    site_id: &str,
    me: &str,
    i_host: bool,
    target: usize,
    max: usize,
    trim: bool,
) -> Action {
    decide_counting(map, site_id, me, i_host, target, max, trim, &|_| true)
}

/// [`decide`], where an announced holder fills one of the `target` copies
/// only when `counts` says so (it proved its copy, see
/// [`ReplicaMap::proven`]). Announcing is free, so three keys that only
/// announce would otherwise keep a site from ever being copied.
///
/// A holder that does not count is still left out of the candidates and is
/// still a source to fetch from. The fill takes the `target` minus counted
/// top-ranked candidates, so however many nodes doubt the announced holders,
/// the same few (never more than `target`) are the ones to act.
#[allow(clippy::too_many_arguments)]
pub fn decide_counting(
    map: &ReplicaMap,
    site_id: &str,
    me: &str,
    i_host: bool,
    target: usize,
    max: usize,
    trim: bool,
    counts: &dyn Fn(&str) -> bool,
) -> Action {
    let holders = map.holders(site_id);
    let i_hold = holders.iter().any(|p| p == me);
    let preferred = map.effective_preferred(site_id);
    if i_hold {
        // Free copies make room for paid ones and settle at `target` in
        // total; a preferred host keeps its copy whatever the count.
        if preferred.iter().any(|p| p == me) {
            return Action::Nothing;
        }
        let free: Vec<String> = holders.iter().filter(|p| !preferred.contains(p)).cloned().collect();
        if releases(site_id, holders.len(), &free, me, target, max, trim) {
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
    let counted = holders.iter().filter(|p| counts(p)).count();
    if counted >= target {
        return Action::Nothing;
    }
    // The rendezvous fill. Preferred hosts that do not hold yet are left out:
    // they replicate on their own, and counting on them would leave the site
    // short if one never manages to.
    // Full peers are left out too: one that ranks first but cannot take the
    // copy would otherwise hold the slot from the next ranked node forever.
    // Peers that announced too little room for the site are skipped, so a
    // full node ranked first cannot leave a site short forever. Older nodes
    // do not announce room and stay candidates.
    let size = map.records.get(site_id).map(|r| r.size).unwrap_or(0);
    let mut candidates: Vec<String> = map
        .candidate_peers()
        .into_iter()
        .filter(|p| !holders.contains(p) && !preferred.contains(p))
        .filter(|p| map.room_of(p).is_none_or(|r| r >= size))
        .collect();
    if !candidates.iter().any(|p| p == me) {
        if map.room_of(me).is_some_and(|r| r < size) {
            return Action::Nothing;
        }
        candidates.push(me.to_string());
    }
    if chosen_by_space(map, site_id, &candidates, target - counted)
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
    fn the_record_table_is_capped_and_sheds_unannounced_sites_first() {
        let key = || c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let mut m = ReplicaMap::default();
        let (a, b, c) = (rec(&key(), 1), rec(&key(), 1), rec(&key(), 1));
        assert!(m.apply_record_capped(a.clone(), 2));
        assert!(m.apply_record_capped(b.clone(), 2));
        // `a` is announced by a live peer, `b` by nobody.
        let held: HashMap<String, (String, u64)> = [(a.site_id.clone(), (a.manifest.clone(), a.seq))].into();
        m.apply_holding_meta("h1", held, Instant::now(), true, None, None);
        assert!(m.apply_record_capped(c.clone(), 2), "room is made from the unannounced site");
        assert!(m.records.contains_key(&a.site_id) && m.records.contains_key(&c.site_id));
        assert!(!m.records.contains_key(&b.site_id));
        // Everything left is announced or new: with no room, a new site is refused.
        let held: HashMap<String, (String, u64)> = [
            (a.site_id.clone(), (a.manifest.clone(), a.seq)),
            (c.site_id.clone(), (c.manifest.clone(), c.seq)),
        ]
        .into();
        m.apply_holding_meta("h1", held, Instant::now(), true, None, None);
        assert!(!m.apply_record_capped(rec(&key(), 1), 2));
        assert_eq!(m.records.len(), 2);
    }

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
            .filter(|p| decide(&m, &site, p, !m.peers[*p].full, 3, 5, true) == Action::Replicate)
            .cloned()
            .collect();
        assert_eq!(takers.len(), 3, "three other nodes fill the site: {takers:?}");
        assert!(!takers.contains(&first));
    }

    #[test]
    fn nodes_with_more_room_take_copies_first() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let r = rec(&kp, 1);
        let site = r.site_id.clone();
        let mut m = ReplicaMap::default();
        m.apply_record(r);
        let now = Instant::now();
        let peers: Vec<String> = (0..8).map(|i| format!("h{i}")).collect();
        for p in &peers {
            m.apply_holding_meta(p, HashMap::new(), now, true, None, None);
            m.set_room(p, Some(20));
        }
        // The three that rank LAST by hash have the big disks.
        let mut by_hash = chosen(&site, &peers, peers.len());
        let big: Vec<String> = by_hash.split_off(peers.len() - 3);
        for p in &big {
            m.set_room(p, Some(3000));
        }
        let takers: Vec<String> = peers
            .iter()
            .filter(|p| decide(&m, &site, p, true, 3, 5, true) == Action::Replicate)
            .cloned()
            .collect();
        let mut want = big.clone();
        want.sort();
        assert_eq!(takers, want, "the roomiest disks fill the site");
        assert_eq!(space_tier(Some(0)), 0);
        assert_eq!(space_tier(Some(1)), 1);
        assert_eq!(space_tier(Some(3000)), 12);
        assert_eq!(space_tier(None), space_tier(Some(64)));
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
            .filter(|p| decide(&m, &site, p, true, 3, 5, true) == Action::Replicate)
            .collect();
        assert_eq!(takers.len(), 2, "exactly the missing two take a copy: {takers:?}");
        assert!(!takers.iter().any(|p| *p == "peer0"));
        // A node that does not host never takes one.
        assert_eq!(decide(&m, &site, "outsider", false, 3, 5, true), Action::Nothing);
    }

    #[test]
    fn full_nodes_are_not_counted_on_for_a_copy() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let r = rec(&kp, 1);
        let site = r.site_id.clone();
        let mut m = ReplicaMap::default();
        m.apply_record(r.clone());
        let now = Instant::now();
        for (p, room) in [("a", Some(0)), ("b", Some(0)), ("c", Some(1 << 30)), ("d", None), ("e", Some(1 << 30))] {
            m.apply_holding_full(p, HashMap::new(), HashMap::new(), now, true, None, None, room, vec![]);
        }
        let takers: Vec<&str> = ["a", "b", "c", "d", "e"]
            .into_iter()
            .filter(|p| decide(&m, &site, p, true, 3, 5, true) == Action::Replicate)
            .collect();
        assert_eq!(takers, vec!["c", "d", "e"], "full a and b are skipped; d (no room field) is not");
    }

    #[test]
    fn fetch_sources_fall_back_to_holders_of_an_older_version() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let r = rec(&kp, 1);
        let site = r.site_id.clone();
        let mut m = ReplicaMap::default();
        m.apply_record(r.clone());
        let now = Instant::now();
        let mut cur = HashMap::new();
        cur.insert(site.clone(), (r.manifest.clone(), 1));
        m.apply_holding("fresh", cur, now);
        let mut old = HashMap::new();
        old.insert(site.clone(), ("an-older-manifest".to_string(), 1));
        m.apply_holding("stale", old, now);
        m.apply_holding("other", HashMap::new(), now);
        assert_eq!(m.sources(&site), vec!["fresh".to_string()]);
        assert_eq!(
            m.fetch_sources(&site),
            vec!["fresh".to_string(), "stale".to_string()],
            "current holders first, then holders of another version; never a peer without the site"
        );
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
        assert_eq!(m.fetch_sources(&site), vec!["publisher".to_string()]);
        let takers = (0..5)
            .filter(|i| decide(&m, &site, &format!("h{i}"), true, 3, 5, true) == Action::Replicate)
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
            .filter(|p| decide(&m, &site, p, true, 3, 5, true) == Action::Drop)
            .collect();
        assert_eq!(droppers.len(), 1);
    }

    /// Holders of `site` at its current version, one map entry each.
    fn holding(m: &mut ReplicaMap, site: &str, manifest: &str, peers: &[String]) {
        for p in peers {
            let mut sites = HashMap::new();
            sites.insert(site.to_string(), (manifest.to_string(), 1));
            m.apply_holding(p, sites, Instant::now());
        }
    }

    fn droppers(m: &ReplicaMap, site: &str, peers: &[String], trim: bool) -> Vec<String> {
        peers
            .iter()
            .filter(|p| decide(m, site, p, true, 3, 5, trim) == Action::Drop)
            .cloned()
            .collect()
    }

    #[test]
    fn extra_free_copies_wait_for_the_grace_then_trim_to_the_target() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let r = rec(&kp, 1);
        let site = r.site_id.clone();
        let peers: Vec<String> = (0..5).map(|i| format!("p{i}")).collect();
        let mut m = ReplicaMap::default();
        m.apply_record(r.clone());
        // Four and five copies: within the ceiling, nobody lets go before the
        // grace runs out.
        for n in [4, 5] {
            holding(&mut m, &site, &r.manifest, &peers[..n]);
            assert!(droppers(&m, &site, &peers, false).is_empty(), "{n} copies, in grace");
        }
        // After it, exactly one copy goes per round (the lowest-ranked) and
        // the site settles at three, never below.
        let mut held: Vec<String> = peers.clone();
        let mut counts = Vec::new();
        for _ in 0..6 {
            let d = droppers(&m, &site, &held, true);
            assert!(d.len() <= 1, "one release per round: {d:?}");
            let Some(gone) = d.first() else { break };
            assert_eq!(surplus(&site, &held, 0).as_ref(), Some(gone), "lowest rank goes first");
            m.apply_holding(gone, HashMap::new(), Instant::now());
            held.retain(|p| p != gone);
            counts.push(m.holders(&site).len());
        }
        assert_eq!(counts, vec![4, 3]);
        // At the target the grace no longer matters.
        assert!(droppers(&m, &site, &held, true).is_empty());
    }

    #[test]
    fn above_the_ceiling_a_free_copy_goes_without_waiting() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let r = rec(&kp, 1);
        let site = r.site_id.clone();
        let peers: Vec<String> = (0..6).map(|i| format!("p{i}")).collect();
        let mut m = ReplicaMap::default();
        m.apply_record(r.clone());
        holding(&mut m, &site, &r.manifest, &peers);
        let d = droppers(&m, &site, &peers, false);
        assert_eq!(d.len(), 1, "six copies: one goes now");
        m.apply_holding(&d[0], HashMap::new(), Instant::now());
        let rest: Vec<String> = peers.iter().filter(|p| **p != d[0]).cloned().collect();
        assert!(droppers(&m, &site, &rest, false).is_empty(), "five: wait for the grace");
    }

    #[test]
    fn trimming_never_releases_a_preferred_copy() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let peers: Vec<String> = (0..7).map(|_| pid()).collect();
        // Four preferred hosts, more than the target: all of them keep their
        // copies, and only the free ones are trimmed (down to none).
        let preferred: Vec<String> = peers[..4].to_vec();
        let r = paid_rec(&kp, preferred.clone(), 4_000);
        let site = r.site_id.clone();
        let mut m = ReplicaMap::default();
        m.apply_record(r.clone());
        holding(&mut m, &site, &r.manifest, &peers);
        assert_eq!(m.effective_preferred(&site), preferred);
        let mut held = peers.clone();
        loop {
            let d = droppers(&m, &site, &held, true);
            assert!(d.iter().all(|p| !preferred.contains(p)), "a preferred copy was released: {d:?}");
            let Some(gone) = d.first().cloned() else { break };
            m.apply_holding(&gone, HashMap::new(), Instant::now());
            held.retain(|p| *p != gone);
        }
        let mut holders = m.holders(&site);
        holders.sort();
        let mut want = preferred.clone();
        want.sort();
        assert_eq!(holders, want, "settles on the preferred hosts alone");
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

    /// A site with `claimants` announcing it and `spare` hosting nodes that
    /// do not hold it. Returns (map, site id, manifest, claimants, spares).
    fn claimed(claimants: usize, spare: usize) -> (ReplicaMap, String, String, Vec<String>, Vec<String>) {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let r = rec(&kp, 1);
        let mut m = ReplicaMap::default();
        m.apply_record(r.clone());
        let fake: Vec<String> = (0..claimants).map(|i| format!("claim{i}")).collect();
        holding(&mut m, &r.site_id, &r.manifest, &fake);
        let spares: Vec<String> = (0..spare).map(|i| format!("spare{i:02}")).collect();
        hosting_peers(&mut m, &spares);
        (m, r.site_id, r.manifest, fake, spares)
    }

    const TTL: Duration = Duration::from_secs(1800);
    const GAP: Duration = Duration::from_secs(7200);
    const SPAN: Duration = Duration::from_secs(6 * 3600);

    /// The `peers` that would take a copy when only proven holders count.
    fn takers(m: &ReplicaMap, site: &str, peers: &[String], now: Instant) -> Vec<String> {
        peers
            .iter()
            .filter(|p| {
                let counts = |h: &str| h == p.as_str() || m.proven(site, h, now, 1_000_000, TTL);
                decide_counting(m, site, p, true, 3, 5, false, &counts) == Action::Replicate
            })
            .cloned()
            .collect()
    }

    #[test]
    fn holders_that_only_announce_do_not_fill_the_target() {
        let (m, site, _, _, spares) = claimed(3, 6);
        let now = Instant::now();
        // Taken at their word, three announcements are three copies.
        assert!(spares.iter().all(|p| decide(&m, &site, p, true, 3, 5, false) == Action::Nothing));
        // Counted only once proven, the site still needs all three, and
        // exactly the three top-ranked spare nodes take them: not everyone
        // who doubts the claims.
        let t = takers(&m, &site, &spares, now);
        assert_eq!(t.len(), 3, "{t:?}");
        let mut want = chosen(&site, &spares, 3);
        want.sort();
        assert_eq!(t, want);
    }

    #[test]
    fn proven_holders_fill_the_target_and_a_stale_proof_expires() {
        let (mut m, site, manifest, fake, spares) = claimed(3, 6);
        let t0 = Instant::now();
        for p in &fake[..2] {
            m.note_pass(&site, p, &manifest, t0, GAP, SPAN);
        }
        // Two proven: one more copy, from the top-ranked spare node alone.
        assert_eq!(takers(&m, &site, &spares, t0), chosen(&site, &spares, 1));
        m.note_pass(&site, &fake[2], &manifest, t0, GAP, SPAN);
        assert!(takers(&m, &site, &spares, t0).is_empty(), "three proven copies: nothing to do");
        assert!(takers(&m, &site, &spares, t0 + TTL).is_empty());
        // Past the TTL the proofs no longer count.
        let late = t0 + TTL + Duration::from_secs(1);
        assert!(!m.proven(&site, &fake[0], late, 1_000_000, TTL));
        assert_eq!(takers(&m, &site, &spares, late).len(), 3);
        // A proof is for one version of the site.
        m.note_pass(&site, &fake[0], "another-manifest", t0, GAP, SPAN);
        assert!(!m.proven(&site, &fake[0], t0, 1_000_000, TTL));
    }

    #[test]
    fn the_ceiling_still_holds_when_claims_do_not_count() {
        // Eight keys announce a site none of them holds. The fill is still
        // three copies from the three top-ranked nodes, however many nodes
        // look, and once those three are proven nobody else takes one.
        let (mut m, site, manifest, fake, spares) = claimed(8, 12);
        let now = Instant::now();
        let first = takers(&m, &site, &spares, now);
        assert_eq!(first.len(), 3, "{first:?}");
        holding(&mut m, &site, &manifest, &first);
        for p in &first {
            m.note_pass(&site, p, &manifest, now, GAP, SPAN);
        }
        let rest: Vec<String> = spares.iter().filter(|p| !first.contains(p)).cloned().collect();
        assert!(takers(&m, &site, &rest, now).is_empty(), "three proven copies stop the fill");
        // With eleven announced copies the site is far over the ceiling, and
        // the rule above it is unchanged: one holder a round is named to let
        // go (whether it does is decided on proofs of the others, see
        // `Hosting::others_prove_whole`).
        let all: Vec<String> = fake.iter().chain(first.iter()).cloned().collect();
        assert_eq!(droppers(&m, &site, &all, false).len(), 1);
    }

    #[test]
    fn a_trusted_receipt_proves_a_holder_until_it_is_stale() {
        use super::super::proof::{Challenge, Receipt};
        let owner = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let stranger = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let r = rec(&owner, 1);
        let mut m = ReplicaMap::default();
        m.apply_record(r.clone());
        holding(&mut m, &r.site_id, &r.manifest, &["h".to_string()]);
        let chunk = Hash::of(b"c").to_hex();
        let receipt = |kp: &c0mpute_net::libp2p::identity::Keypair, ok: bool, at: u64| {
            Receipt::sign(
                kp,
                Challenge {
                    site_id: &r.site_id,
                    manifest: &r.manifest,
                    holder: "h",
                    chunk: &chunk,
                    ok,
                    rtt_ms: 1,
                    at,
                },
            )
            .unwrap()
        };
        let now = Instant::now();
        let proven = |m: &ReplicaMap, secs: u64| m.proven(&r.site_id, "h", now, secs, TTL);
        // Anyone can sign a receipt: a stranger's proves nothing.
        m.apply_receipt(receipt(&stranger, true, 1_000));
        assert!(!proven(&m, 1_000));
        m.apply_receipt(receipt(&owner, true, 1_000));
        assert!(proven(&m, 1_000) && proven(&m, 1_000 + TTL.as_secs()));
        assert!(!proven(&m, 1_001 + TTL.as_secs()), "an old pass is not a copy today");
        // The owner's latest word wins, and a date from the future is not one.
        m.apply_receipt(receipt(&owner, false, 1_100));
        assert!(!proven(&m, 1_200));
        m.apply_receipt(receipt(&owner, true, 900_000));
        assert!(!proven(&m, 1_200));
    }

    #[test]
    fn one_fresh_pass_counts_for_the_fill_but_dropping_needs_standing() {
        let (mut m, site, manifest, fake, _) = claimed(1, 0);
        let h = &fake[0];
        let t0 = Instant::now();
        let standing = |m: &ReplicaMap, at: Instant| m.has_standing(&site, h, at, 3, SPAN);
        assert!(m.pass_due(&site, h, t0, GAP), "never asked: a pass would count");
        m.note_pass(&site, h, &manifest, t0, GAP, SPAN);
        assert!(m.proven(&site, h, t0, 0, TTL), "one pass fills a slot");
        assert!(!standing(&m, t0));
        // Passes close together are one time, however many there are.
        for s in 1..50 {
            m.note_pass(&site, h, &manifest, t0 + Duration::from_secs(s), GAP, SPAN);
        }
        assert_eq!(m.proofs[&(site.clone(), h.clone())].passes, 1);
        assert!(!m.pass_due(&site, h, t0 + Duration::from_secs(60), GAP));
        assert!(!standing(&m, t0 + SPAN), "six hours since one pass is not three passes");
        // Three passes two hours apart, but only four hours in: not yet.
        m.note_pass(&site, h, &manifest, t0 + GAP, GAP, SPAN);
        m.note_pass(&site, h, &manifest, t0 + GAP * 2, GAP, SPAN);
        assert_eq!(m.proofs[&(site.clone(), h.clone())].passes, 3);
        assert!(!standing(&m, t0 + GAP * 2));
        // A pass at six hours: three or more separate times spanning six hours.
        m.note_pass(&site, h, &manifest, t0 + SPAN, GAP, SPAN);
        assert!(standing(&m, t0 + SPAN));
    }

    #[test]
    fn standing_lapses_and_the_count_starts_over() {
        let (mut m, site, manifest, fake, _) = claimed(1, 0);
        let h = &fake[0];
        let t0 = Instant::now();
        for i in 0..4 {
            m.note_pass(&site, h, &manifest, t0 + GAP * i, GAP, SPAN);
        }
        let end = t0 + GAP * 3;
        assert!(m.has_standing(&site, h, end, 3, SPAN));
        // No pass for a whole span: the standing is gone...
        let late = end + SPAN + Duration::from_secs(1);
        assert!(!m.has_standing(&site, h, late, 3, SPAN));
        // ...and the next pass is the first of a new run, not the fifth.
        m.note_pass(&site, h, &manifest, late, GAP, SPAN);
        let p = &m.proofs[&(site.clone(), h.clone())];
        assert_eq!((p.passes, p.first), (1, late));
        // A new version of the site starts over too.
        m.note_pass(&site, h, "next-version", late + GAP, GAP, SPAN);
        assert_eq!(m.proofs[&(site.clone(), h.clone())].passes, 1);
        // Pruning drops what lapsed, and proofs of sites no longer known.
        m.note_pass("gone-site", h, &manifest, late + GAP, GAP, SPAN);
        m.prune_proofs(late + GAP, SPAN);
        assert_eq!(m.proofs.len(), 1);
        m.prune_proofs(late + GAP + SPAN + Duration::from_secs(1), SPAN);
        assert!(m.proofs.is_empty());
    }

    #[test]
    fn the_proof_table_is_capped_and_the_oldest_pass_makes_room() {
        let mut m = ReplicaMap::default();
        let t0 = Instant::now();
        for i in 0..3u64 {
            m.note_pass_capped("s", &format!("h{i}"), "m", t0 + Duration::from_secs(i), GAP, SPAN, 3);
        }
        m.note_pass_capped("s", "h9", "m", t0 + Duration::from_secs(9), GAP, SPAN, 3);
        assert_eq!(m.proofs.len(), 3);
        assert!(!m.proofs.contains_key(&("s".to_string(), "h0".to_string())));
        assert!(m.proofs.contains_key(&("s".to_string(), "h9".to_string())));
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
                .map(|p| (p.clone(), decide(m, site, p, true, 3, 5, true)))
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
            assert_eq!(decide(&m, &r.site_id, p, true, 3, 5, true), Action::Replicate);
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
