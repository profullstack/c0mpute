//! Scallion Network: privacy-aware route planning, not an encrypted data plane.
//!
//! This module only ranks independently operated three-hop candidates using
//! fresh, externally measured link samples. It NEVER forwards a packet, creates
//! an encrypted circuit, or claims to anonymize a connection. A separate,
//! security-reviewed Scallion transport must consume the plans before use.
//!
//! Guard selection is deliberately out of scope: callers pin a stable entry
//! identity rather than cycling guards every time they look for a faster path.
//! See docs/protocol/scallion-network.md for the threat model and limitations.

use std::collections::HashMap;

/// Used in locally observed link samples for the client's first hop.
pub const CLIENT: &str = "@client";

/// An eligible node, populated by a verified peer record. The advertised
/// operator/family/ASN identifiers are NOT proofs of independent ownership:
/// consumers must independently validate them before making anonymity claims.
#[derive(Clone, Debug)]
pub struct RelayCandidate {
    pub peer_id: String,
    pub operator_id: String,
    pub family_id: String,
    pub asn: u32,
    pub last_seen_ms: u64,
    pub capacity_kbps: u64,
    pub can_guard: bool,
    pub can_middle: bool,
    pub can_exit: bool,
    /// A node must explicitly opt in to carrying egress traffic.
    pub exit_opt_in: bool,
}

/// Latency/loss for one directed edge (client -> guard, guard -> middle, etc).
/// Only accept measurements collected by the client or a trusted measurement
/// process, never unauthenticated self-advertised "fastest" claims.
#[derive(Clone, Debug)]
pub struct LinkSample {
    pub from: String,
    pub to: String,
    pub rtt_ms: u64,
    pub loss_ppm: u32,
    pub measured_at_ms: u64,
}

/// An estimated Scallion three-hop path, not an established or encrypted circuit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoutePlan {
    pub guard: String,
    pub middle: String,
    pub exit: String,
    pub estimated_ms: u64,
    pub bottleneck_kbps: u64,
}

fn fresh(now: u64, recorded: u64, max_age_ms: u64) -> bool {
    now.checked_sub(recorded).is_some_and(|age| age <= max_age_ms)
}

fn independent(a: &RelayCandidate, b: &RelayCandidate) -> bool {
    a.peer_id != b.peer_id
        && !a.operator_id.is_empty()
        && !b.operator_id.is_empty()
        && !a.family_id.is_empty()
        && !b.family_id.is_empty()
        && a.operator_id != b.operator_id
        && a.family_id != b.family_id
        && a.asn != 0
        && b.asn != 0
        && a.asn != b.asn
}

fn fresh_link<'a>(
    links: &'a HashMap<(&str, &str), &'a LinkSample>,
    from: &str,
    to: &str,
    now_ms: u64,
    max_age_ms: u64,
) -> Option<&'a LinkSample> {
    let link = *links.get(&(from, to))?;
    if link.loss_ppm >= 1_000_000 || !fresh(now_ms, link.measured_at_ms, max_age_ms) {
        return None;
    }
    Some(link)
}

/// Rank three-hop candidate paths while preserving role and operator/ASN
/// separation. A stale or missing measurement is NOT treated as a fast link.
///
/// `estimated_payload_kib` trades latency against bandwidth: interactive
/// requests prefer low RTT, while large GPU transfers penalize slow exits.
/// These are hints, not security guarantees or verified transfer benchmarks.
/// `pinned_guard` stays fixed across route rotations.
pub fn rank_routes(
    pinned_guard: &str,
    relays: &[RelayCandidate],
    samples: &[LinkSample],
    now_ms: u64,
    max_age_ms: u64,
    estimated_payload_kib: u64,
    min_bandwidth_kbps: u64,
) -> Vec<RoutePlan> {
    let eligible: Vec<_> = relays
        .iter()
        .filter(|r| fresh(now_ms, r.last_seen_ms, max_age_ms))
        .filter(|r| r.capacity_kbps > 0 && r.capacity_kbps >= min_bandwidth_kbps)
        .collect();
    let Some(guard) = eligible.iter().copied().find(|r| r.peer_id == pinned_guard && r.can_guard) else {
        return Vec::new();
    };

    let mut links: HashMap<(&str, &str), &LinkSample> = HashMap::new();
    for sample in samples {
        // Keep the most recent reading per edge; never trust a stale fast
        // reading over a newer slow one.
        let key = (sample.from.as_str(), sample.to.as_str());
        if links.get(&key).is_none_or(|old| sample.measured_at_ms > old.measured_at_ms) {
            links.insert(key, sample);
        }
    }
    let Some(first) = fresh_link(&links, CLIENT, &guard.peer_id, now_ms, max_age_ms) else {
        return Vec::new();
    };

    let mut routes = Vec::new();
    for middle in eligible.iter().copied().filter(|r| r.can_middle) {
        if !independent(guard, middle) {
            continue;
        }
        let Some(second) = fresh_link(&links, &guard.peer_id, &middle.peer_id, now_ms, max_age_ms) else {
            continue;
        };
        for exit in eligible.iter().copied().filter(|r| r.can_exit && r.exit_opt_in) {
            if !independent(guard, exit) || !independent(middle, exit) {
                continue;
            }
            let Some(third) = fresh_link(&links, &middle.peer_id, &exit.peer_id, now_ms, max_age_ms) else {
                continue;
            };
            let bottleneck_kbps = guard.capacity_kbps.min(middle.capacity_kbps).min(exit.capacity_kbps);
            // 1 KiB = 8192 bits; kbps = 1000 bits/sec, so KiB*8192/kbps
            // is milliseconds. Use saturating math for untrusted metrics.
            let transfer_ms = estimated_payload_kib.saturating_mul(8192) / bottleneck_kbps;
            let rtt_ms = first.rtt_ms.saturating_add(second.rtt_ms).saturating_add(third.rtt_ms);
            let max_loss = first.loss_ppm.max(second.loss_ppm).max(third.loss_ppm);
            let loss_penalty = rtt_ms.saturating_mul(max_loss as u64) / 1_000_000;
            routes.push(RoutePlan {
                guard: guard.peer_id.clone(),
                middle: middle.peer_id.clone(),
                exit: exit.peer_id.clone(),
                estimated_ms: rtt_ms.saturating_add(transfer_ms).saturating_add(loss_penalty),
                bottleneck_kbps,
            });
        }
    }
    routes.sort_by(|a, b| {
        (a.estimated_ms, &a.middle, &a.exit)
            .cmp(&(b.estimated_ms, &b.middle, &b.exit))
    });
    routes
}

/// Select among routes no more than `slack_percent` slower than the best.
/// Avoids concentrating all traffic on a single fastest path. `entropy`
/// MUST be generated via a CSPRNG (e.g. OsRng), never a stable user ID.
pub fn choose_near_fastest<'a>(
    ranked: &'a [RoutePlan],
    slack_percent: u8,
    entropy: u64,
) -> Option<&'a RoutePlan> {
    let best = ranked.iter().map(|r| r.estimated_ms).min()?;
    let allowed = best.saturating_add(best.saturating_mul(slack_percent as u64) / 100);
    let eligible: Vec<_> = ranked.iter().filter(|r| r.estimated_ms <= allowed).collect();
    eligible.get((entropy % eligible.len() as u64) as usize).copied()
}

/// A process-level *route lease* registry. Existing streams are pinned to
/// their circuit: expiration applies only to NEW streams. This matches Tor's
/// stream lifetime semantics and avoids killing SSH, API and inference flows.
///
/// This is bookkeeping only. It does not make established TCP sessions
/// migrate across exit IPs, and cannot replace encrypted transport.
#[derive(Default)]
pub struct CircuitLeases {
    active_id: Option<u64>,
    next_id: u64,
    circuits: HashMap<u64, LeasedCircuit>,
    sessions: HashMap<String, u64>,
    rotate_after_ms: u64,
    slack_percent: u8,
}

struct LeasedCircuit {
    path: RoutePlan,
    opened_ms: u64,
    session_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRoute {
    pub circuit_id: u64,
    pub path: RoutePlan,
}

impl CircuitLeases {
    pub fn new(rotate_after_ms: u64, slack_percent: u8) -> Self {
        Self { rotate_after_ms: rotate_after_ms.max(1), slack_percent, ..Self::default() }
    }

    /// Pin a new session; an existing session always keeps its old route,
    /// including when the lease is overdue or no longer advertised.
    pub fn attach(
        &mut self,
        session_id: &str,
        now_ms: u64,
        ranked: &[RoutePlan],
        entropy: u64,
    ) -> Option<SessionRoute> {
        if let Some(id) = self.sessions.get(session_id) {
            return self.circuits.get(id).map(|c| SessionRoute {
                circuit_id: *id,
                path: c.path.clone(),
            });
        }

        let rotate = self.active_id.and_then(|id| self.circuits.get(&id)).is_none_or(|active| {
            now_ms.saturating_sub(active.opened_ms) >= self.rotate_after_ms
                || !ranked.iter().any(|r| (r.guard.as_str(), r.middle.as_str(), r.exit.as_str())
                    == (active.path.guard.as_str(), active.path.middle.as_str(), active.path.exit.as_str()))
        });
        if rotate {
            let previous = self.active_id.and_then(|id| self.circuits.get(&id)).map(|c| &c.path);
            // When another near-fast path exists, prefer it at rotation
            // boundaries rather than continually returning to the same exit.
            let best = ranked.iter().map(|r| r.estimated_ms).min()?;
            let allowed = best.saturating_add(best.saturating_mul(self.slack_percent as u64) / 100);
            let eligible: Vec<_> = ranked.iter().filter(|r| r.estimated_ms <= allowed).collect();
            let alternatives: Vec<_> = eligible.iter().copied().filter(|r| {
                previous.is_none_or(|old| old.middle != r.middle || old.exit != r.exit)
            }).collect();
            let selection = if alternatives.is_empty() { &eligible } else { &alternatives };
            let path = (*selection.get((entropy % selection.len() as u64) as usize)?).clone();
            self.next_id = self.next_id.saturating_add(1);
            let id = self.next_id;
            self.circuits.insert(id, LeasedCircuit { path, opened_ms: now_ms, session_count: 0 });
            self.active_id = Some(id);
        }

        let id = self.active_id?;
        let circuit = self.circuits.get_mut(&id)?;
        circuit.session_count += 1;
        self.sessions.insert(session_id.to_owned(), id);
        Some(SessionRoute { circuit_id: id, path: circuit.path.clone() })
    }

    /// Release a closed session and prune its *drained* old circuit. Active
    /// sessions never get forced off a path merely because its TTL passed.
    pub fn detach(&mut self, session_id: &str) {
        if let Some(id) = self.sessions.remove(session_id) {
            if let Some(circuit) = self.circuits.get_mut(&id) {
                circuit.session_count = circuit.session_count.saturating_sub(1);
                if circuit.session_count == 0 && self.active_id != Some(id) {
                    self.circuits.remove(&id);
                }
            }
        }
    }

    pub fn live_circuits(&self) -> usize {
        self.circuits.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relay(id: &str, owner: &str, asn: u32, roles: (bool, bool, bool), bandwidth: u64) -> RelayCandidate {
        RelayCandidate {
            peer_id: id.into(), operator_id: owner.into(), family_id: owner.into(),
            asn, last_seen_ms: 900, capacity_kbps: bandwidth,
            can_guard: roles.0, can_middle: roles.1, can_exit: roles.2,
            exit_opt_in: roles.2,
        }
    }

    fn peers() -> Vec<RelayCandidate> {
        vec![
            relay("guard", "alpha", 100, (true, false, false), 100_000),
            relay("middle1", "beta", 200, (false, true, false), 100_000),
            relay("middle2", "delta", 400, (false, true, false), 100_000),
            relay("exit1", "gamma", 300, (false, false, true), 100_000),
            relay("exit2", "epsilon", 500, (false, false, true), 100_000),
        ]
    }

    fn edge(from: &str, to: &str, ms: u64) -> LinkSample {
        LinkSample { from: from.into(), to: to.into(), rtt_ms: ms, loss_ppm: 0, measured_at_ms: 950 }
    }

    fn links() -> Vec<LinkSample> {
        vec![
            edge(CLIENT, "guard", 20),
            edge("guard", "middle1", 20), edge("guard", "middle2", 30),
            edge("middle1", "exit1", 20), edge("middle1", "exit2", 30),
            edge("middle2", "exit1", 30), edge("middle2", "exit2", 20),
        ]
    }

    fn ranked() -> Vec<RoutePlan> {
        rank_routes("guard", &peers(), &links(), 1_000, 200, 0, 10)
    }

    #[test]
    fn fastest_is_ranked_with_three_independent_and_opted_in_nodes() {
        let routes = ranked();
        assert_eq!(routes.len(), 4);
        assert_eq!((routes[0].guard.as_str(), routes[0].middle.as_str(), routes[0].exit.as_str()),
            ("guard", "middle1", "exit1"));
        assert_eq!(routes[0].estimated_ms, 60);
    }

    #[test]
    fn unknown_diversity_or_opt_out_fails_closed() {
        let mut nodes = peers();
        nodes[1].asn = 100; // middle matches guard
        nodes[2].operator_id = "alpha".into(); // alternate middle matches guard owner
        assert!(rank_routes("guard", &nodes, &links(), 1_000, 200, 0, 10).is_empty());
        let mut nodes = peers();
        nodes[3].exit_opt_in = false;
        nodes[4].exit_opt_in = false;
        assert!(rank_routes("guard", &nodes, &links(), 1_000, 200, 0, 10).is_empty());
    }

    #[test]
    fn only_fresh_link_metrics_are_eligible() {
        assert!(rank_routes("guard", &peers(), &links(), 5_000, 200, 0, 10).is_empty());
        let mut samples = links();
        samples.retain(|s| s.from != CLIENT);
        assert!(rank_routes("guard", &peers(), &samples, 1_000, 200, 0, 10).is_empty());
    }

    #[test]
    fn replacement_measurement_wins_and_large_jobs_account_for_bandwidth() {
        let mut samples = links();
        samples.push(LinkSample { from: "guard".into(), to: "middle1".into(), rtt_ms: 600, loss_ppm: 0, measured_at_ms: 975 });
        let routes = rank_routes("guard", &peers(), &samples, 1_000, 200, 0, 10);
        assert_eq!(routes[0].middle, "middle2");
        let mut nodes = peers();
        nodes[1].capacity_kbps = 50;
        let routes = rank_routes("guard", &nodes, &links(), 1_000, 200, 10_000, 10);
        assert_eq!(routes[0].middle, "middle2");
    }

    #[test]
    fn near_fast_selection_is_limited_to_slowdown_budget() {
        let routes = ranked();
        // All 4 are within 50% of 60ms; only best is within 0%.
        assert_eq!(choose_near_fastest(&routes, 0, 999).unwrap().estimated_ms, 60);
        assert_ne!(choose_near_fastest(&routes, 50, 1).unwrap().middle, "");
    }

    #[test]
    fn existing_stream_survives_rotation_new_stream_gets_new_circuit() {
        let routes = ranked();
        let mut leases = CircuitLeases::new(60_000, 50);
        let a = leases.attach("ssh-connection", 1_000, &routes, 0).unwrap();
        let b = leases.attach("api-connection", 62_000, &routes, 1).unwrap();
        assert_ne!(a.circuit_id, b.circuit_id);
        assert_eq!(leases.attach("ssh-connection", 90_000, &[], 42).unwrap(), a);
        assert_eq!(leases.live_circuits(), 2);
        leases.detach("ssh-connection");
        assert_eq!(leases.live_circuits(), 1);
        assert_eq!(leases.attach("api-connection", 180_000, &[], 0).unwrap(), b);
    }

    #[test]
    fn a_missing_guard_or_no_eligible_exit_does_not_fallback_to_direct() {
        assert!(rank_routes("unknown", &peers(), &links(), 1_000, 200, 0, 10).is_empty());
        let mut leases = CircuitLeases::new(60_000, 25);
        assert!(leases.attach("first", 1_000, &[], 0).is_none());
    }
}
