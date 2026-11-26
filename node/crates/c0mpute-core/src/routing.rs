//! Smart routing: which bid wins an auction.
//!
//! The default policy sends a job to the most capable worker that is not
//! overloaded, instead of simply the cheapest bid:
//!
//!   1. Overloaded bids go last: a queue load at or above
//!      [`OVERLOAD_THRESHOLD`], or an ETA the deadline cannot absorb. If
//!      every bid is overloaded, the least loaded wins.
//!   2. A smaller alternative model waits while an idle worker offers the
//!      asked-for model or a better one. An upgrade (the GPU 9B answering a
//!      chat for the 4B default) is not a smaller alternative: before
//!      0.3.112 it waited too, so an idle CPU node at ~67 s a chat beat an
//!      idle GPU at ~15 s (2026-11-23).
//!   3. A GPU with room goes before every CPU bid.
//!   4. Then the fastest ETA. ETAs within 1.5x of the fastest share a
//!      bucket. The ETA is measured (recent job time times queue depth), so
//!      it already says how fast the box runs under its real load; a
//!      synthetic CPU benchmark does not. 2026-11-14 a node at load 32/16
//!      with eta 301 s beat one with eta 60 s on its higher bench score,
//!      and the job waited a minute for nothing.
//!   5. Among those, the bidder that won from this buyer least recently:
//!      comparable nodes share the work. Before 0.3.112 the higher CPU bench
//!      score took every job, also between two GPUs, where it says nothing.
//!   6. Then the highest benchmark score (scores within about 10% of each
//!      other share a bucket), then the lower ETA, then the lower price,
//!      then the earlier bid.
//!
//! Exact ties are rotated by the caller so parallel chunks spread out.
//! Bids from workers older than 0.2.49 carry no score or load: they count
//! as unbenchmarked and unloaded, so they still win when nothing better
//! bids. `SelectionPolicy::Cheapest` keeps the old lowest-price auction.
//!
//! Scores and load are self-reported in the bid (the same numbers the
//! worker gossips in its capability ad); a worker that lies wins jobs it
//! then runs slowly. Verified benchmarks belong to the reputation layer.

use std::cmp::Ordering;
use std::collections::HashMap;

use c0mpute_net::topics::JobBid;

/// Queue load (0.0 empty .. 1.0 full) at which a worker counts as overloaded.
pub const OVERLOAD_THRESHOLD: f32 = 0.85;

/// Benchmark scores within this ratio share a bucket.
const BENCH_BUCKET_RATIO: f64 = 1.10;

/// ETAs within this ratio of the fastest share a bucket.
const ETA_BUCKET_RATIO: f64 = 1.5;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SelectionPolicy {
    /// Most capable worker that is not overloaded (the default).
    #[default]
    Smart,
    /// Lowest price wins (the pre-0.2.49 auction).
    Cheapest,
}

impl std::str::FromStr for SelectionPolicy {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "smart" | "capable" | "best" => Ok(Self::Smart),
            "cheapest" | "price" | "lowest-price" => Ok(Self::Cheapest),
            other => Err(format!("unknown selection policy `{other}` (smart|cheapest)")),
        }
    }
}

fn load_of(bid: &JobBid) -> f32 {
    bid.load
        .filter(|l| l.is_finite())
        .map(|l| l.clamp(0.0, 1.0))
        .unwrap_or(0.0)
}

/// True when the bid should only win if nothing better is on offer.
pub fn is_overloaded(bid: &JobBid, deadline_secs: u64) -> bool {
    load_of(bid) >= OVERLOAD_THRESHOLD || bid.eta_seconds > deadline_secs
}

/// Distance from the best score among the bids, in ~10% steps: 0 is within
/// 10% of the top score, 1 within 21%, ... Unbenchmarked bids rank last.
fn bench_bucket(bid: &JobBid, best: u32) -> i64 {
    match bid.bench_score {
        Some(s) if s > 0 && best > 0 => {
            let ratio = (best as f64 / s as f64).max(1.0);
            (ratio.ln() / BENCH_BUCKET_RATIO.ln()).floor() as i64
        }
        _ => i64::MAX,
    }
}

/// Distance from the fastest ETA among the bids, in 1.5x steps: 0 is within
/// 1.5x of the fastest.
fn eta_bucket(bid: &JobBid, fastest: u64) -> i64 {
    let ratio = (bid.eta_seconds.max(1) as f64 / fastest.max(1) as f64).max(1.0);
    (ratio.ln() / ETA_BUCKET_RATIO.ln()).floor() as i64
}

/// Fastest ETA among the bids (0 when there are none).
pub fn fastest_eta(bids: &[JobBid]) -> u64 {
    bids.iter().map(|b| b.eta_seconds).min().unwrap_or(0)
}

/// Highest bench score among the bids (0 when none is benchmarked).
pub fn best_bench(bids: &[JobBid]) -> u32 {
    bids.iter().filter_map(|b| b.bench_score).max().unwrap_or(0)
}

fn price_cmp(a: &JobBid, b: &JobBid) -> Ordering {
    a.price_usd.partial_cmp(&b.price_usd).unwrap_or(Ordering::Equal)
}

/// What the buyer knows beyond the bids themselves.
#[derive(Clone, Debug, Default)]
pub struct Hints {
    /// Models at least as good as the asked-for one (the GPU 9B for the 4B
    /// house default). A bid answering with one is an upgrade, not a smaller
    /// alternative: it never waits behind the asked-for model.
    pub upgrades: Vec<String>,
    /// When each bidder last won a job from this buyer (unix ms). A bidder
    /// that is absent never won.
    pub last_won_ms: HashMap<String, u64>,
    /// Bidders the caller would rather have (the node that served the
    /// conversation's previous turn, so its prompt cache is warm). A preferred
    /// bid wins over its peers when it is not overloaded, answers with the
    /// asked-for model (or better) and is as GPU as the best of them; when the
    /// preferred node does not bid, nothing changes.
    pub preferred: Vec<String>,
}

/// A bid from a preferred bidder that is not a smaller alternative.
fn is_preferred(bid: &JobBid, hints: &Hints) -> bool {
    !hints.preferred.is_empty() && hints.preferred.contains(&bid.bidder_peer_id) && !is_downgrade(bid, hints)
}

/// A smaller alternative to the asked-for model (an upgrade is not one).
fn is_downgrade(bid: &JobBid, hints: &Hints) -> bool {
    bid.fallback == Some(true) && !bid.model.as_ref().is_some_and(|m| hints.upgrades.contains(m))
}

/// True when some bid for the asked-for model or better (not a smaller
/// alternative) comes from an idle worker: smaller alternatives then wait
/// their turn.
pub fn preferred_idle(bids: &[JobBid], deadline_secs: u64, hints: &Hints) -> bool {
    bids.iter()
        .any(|b| !is_downgrade(b, hints) && load_of(b) == 0.0 && !is_overloaded(b, deadline_secs))
}

/// A smaller-model bid while an idle worker offers the asked-for model.
fn yields(bid: &JobBid, preferred_idle: bool, hints: &Hints) -> bool {
    preferred_idle && is_downgrade(bid, hints)
}

/// The bid does not answer from a GPU (false sorts first). Workers older
/// than 0.3.112 do not say, and count as CPU.
fn not_gpu(bid: &JobBid) -> bool {
    bid.gpu != Some(true)
}

fn last_won(bid: &JobBid, hints: &Hints) -> u64 {
    hints.last_won_ms.get(&bid.bidder_peer_id).copied().unwrap_or(0)
}

/// Order two bids under `policy`, ignoring publish time (the tie-breaker).
/// `preferred_idle` is [`preferred_idle`] over all the bids.
#[allow(clippy::too_many_arguments)]
pub fn compare_with(
    a: &JobBid,
    b: &JobBid,
    policy: SelectionPolicy,
    deadline_secs: u64,
    best: u32,
    fastest: u64,
    preferred_idle: bool,
    hints: &Hints,
) -> Ordering {
    match policy {
        SelectionPolicy::Cheapest => price_cmp(a, b),
        SelectionPolicy::Smart => {
            let (oa, ob) = (is_overloaded(a, deadline_secs), is_overloaded(b, deadline_secs));
            oa.cmp(&ob)
                .then_with(|| {
                    if oa && ob {
                        load_of(a).partial_cmp(&load_of(b)).unwrap_or(Ordering::Equal)
                    } else {
                        Ordering::Equal
                    }
                })
                .then_with(|| yields(a, preferred_idle, hints).cmp(&yields(b, preferred_idle, hints)))
                .then_with(|| not_gpu(a).cmp(&not_gpu(b)))
                // A warm prompt cache saves more than an ETA bucket is worth
                // (re-reading a long history is most of a chat turn's time).
                .then_with(|| is_preferred(b, hints).cmp(&is_preferred(a, hints)))
                .then_with(|| eta_bucket(a, fastest).cmp(&eta_bucket(b, fastest)))
                .then_with(|| last_won(a, hints).cmp(&last_won(b, hints)))
                .then_with(|| bench_bucket(a, best).cmp(&bench_bucket(b, best)))
                .then_with(|| a.eta_seconds.cmp(&b.eta_seconds))
                .then_with(|| price_cmp(a, b))
        }
    }
}

/// [`rank_with`] for a buyer that knows nothing beyond the bids.
pub fn rank(bids: &mut [JobBid], policy: SelectionPolicy, deadline_secs: u64) -> usize {
    rank_with(bids, policy, deadline_secs, &Hints::default())
}

/// Sort `bids` best first and return how many share the top spot exactly
/// (so the caller can rotate among them).
pub fn rank_with(bids: &mut [JobBid], policy: SelectionPolicy, deadline_secs: u64, hints: &Hints) -> usize {
    let best = best_bench(bids);
    let fastest = fastest_eta(bids);
    let idle = preferred_idle(bids, deadline_secs, hints);
    bids.sort_by(|a, b| {
        compare_with(a, b, policy, deadline_secs, best, fastest, idle, hints)
            .then(a.published_at_ms.cmp(&b.published_at_ms))
    });
    match bids.first() {
        None => 0,
        Some(top) => bids
            .iter()
            .take_while(|b| compare_with(b, top, policy, deadline_secs, best, fastest, idle, hints) == Ordering::Equal)
            .count(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alt(mut b: JobBid, fallback: bool) -> JobBid {
        b.model = Some(if fallback { "qwen3:4b" } else { "qwen2.5:7b" }.into());
        b.fallback = Some(fallback);
        b
    }

    #[test]
    fn a_smaller_model_waits_while_an_idle_worker_serves_the_asked_for_one() {
        // Idle node with the asked-for model vs a much faster idle node with
        // an alternative: the asked-for model wins.
        let mut bids = vec![
            alt(bid("fast-small", 0.001, 10, Some(9000), Some(0.0)), true),
            alt(bid("idle-7b", 0.001, 90, Some(5000), Some(0.0)), false),
        ];
        rank(&mut bids, SelectionPolicy::Smart, 600);
        assert_eq!(bids[0].bidder_peer_id, "idle-7b");
    }

    #[test]
    fn a_free_smaller_model_beats_a_working_node_with_the_asked_for_one() {
        // Every 7b worker is busy (load 0.5, ETA stretched): the free node
        // with an alternative takes the plain-text prompt.
        let mut bids = vec![
            alt(bid("busy-7b", 0.001, 131, Some(12898), Some(0.5)), false),
            alt(bid("free-small", 0.001, 40, Some(4000), Some(0.0)), true),
        ];
        rank(&mut bids, SelectionPolicy::Smart, 600);
        assert_eq!(bids[0].bidder_peer_id, "free-small");
        // Old workers (no model fields) count as the asked-for model.
        let mut old = vec![bid("old-idle", 0.001, 90, Some(5000), Some(0.0)), alt(bid("free-small", 0.001, 10, Some(9000), Some(0.0)), true)];
        rank(&mut old, SelectionPolicy::Smart, 600);
        assert_eq!(old[0].bidder_peer_id, "old-idle");
    }

    fn bid(peer: &str, price: f64, eta: u64, bench: Option<u32>, load: Option<f32>) -> JobBid {
        JobBid {
            job_id: "j".into(),
            bidder_peer_id: peer.into(),
            bidder_did: None,
            price_usd: price,
            eta_seconds: eta,
            published_at_ms: 0,
            expires_at_ms: None,
            bench_score: bench,
            load,
            e2e: None,
            instance: None,
            model: None,
            fallback: None,
            gpu: None,
        }
    }

    fn winner(mut bids: Vec<JobBid>, policy: SelectionPolicy) -> String {
        rank(&mut bids, policy, 900);
        bids[0].bidder_peer_id.clone()
    }

    #[test]
    fn most_capable_wins_over_cheaper() {
        let bids = vec![
            bid("slow-cheap", 0.10, 60, Some(5_000), Some(0.1)),
            bid("fast", 0.40, 60, Some(20_000), Some(0.1)),
        ];
        assert_eq!(winner(bids, SelectionPolicy::Smart), "fast");
    }

    #[test]
    fn measured_eta_beats_a_higher_bench_score() {
        // 2026-11-14: vienna (bench 4156, eta 301 s) beat bonita (bench 2452,
        // eta 60 s) and the job waited a minute before it moved.
        let bids = vec![
            bid("vienna", 0.006745, 301, Some(4_156), Some(0.0)),
            bid("bonita", 0.006, 60, Some(2_452), Some(0.0)),
        ];
        assert_eq!(winner(bids, SelectionPolicy::Smart), "bonita");
    }

    #[test]
    fn similar_etas_still_go_to_the_more_capable_node() {
        // 60 s vs 80 s is within 1.5x: the bench score decides.
        let bids = vec![
            bid("slow", 0.10, 60, Some(5_000), Some(0.1)),
            bid("fast", 0.40, 80, Some(20_000), Some(0.1)),
        ];
        assert_eq!(winner(bids, SelectionPolicy::Smart), "fast");
    }

    #[test]
    fn overloaded_capable_node_loses_to_free_one() {
        let bids = vec![
            bid("fast-busy", 0.20, 60, Some(20_000), Some(0.95)),
            bid("ok-free", 0.20, 60, Some(8_000), Some(0.2)),
        ];
        assert_eq!(winner(bids, SelectionPolicy::Smart), "ok-free");
    }

    #[test]
    fn eta_past_deadline_counts_as_overloaded() {
        let bids = vec![
            bid("fast-late", 0.20, 5_000, Some(20_000), Some(0.1)),
            bid("ok-on-time", 0.20, 120, Some(8_000), Some(0.1)),
        ];
        assert_eq!(winner(bids, SelectionPolicy::Smart), "ok-on-time");
    }

    #[test]
    fn all_overloaded_picks_least_loaded() {
        let bids = vec![
            bid("a", 0.20, 60, Some(20_000), Some(0.99)),
            bid("b", 0.20, 60, Some(5_000), Some(0.86)),
        ];
        assert_eq!(winner(bids, SelectionPolicy::Smart), "b");
    }

    #[test]
    fn near_equal_scores_fall_through_to_eta_then_price() {
        // 20_000 vs 20_500 is within 10%: same bucket, so ETA decides.
        let bids = vec![
            bid("a", 0.10, 300, Some(20_500), Some(0.1)),
            bid("b", 0.30, 60, Some(20_000), Some(0.1)),
        ];
        assert_eq!(winner(bids, SelectionPolicy::Smart), "b");
        let bids = vec![
            bid("a", 0.10, 60, Some(20_500), Some(0.1)),
            bid("b", 0.30, 60, Some(20_000), Some(0.1)),
        ];
        assert_eq!(winner(bids, SelectionPolicy::Smart), "a");
    }

    #[test]
    fn old_workers_without_fields_still_win_alone_and_rank_below_benchmarked() {
        assert_eq!(winner(vec![bid("old", 0.5, 60, None, None)], SelectionPolicy::Smart), "old");
        let bids = vec![bid("old", 0.1, 60, None, None), bid("new", 0.5, 60, Some(3_000), Some(0.0))];
        assert_eq!(winner(bids, SelectionPolicy::Smart), "new");
    }

    #[test]
    fn cheapest_policy_keeps_old_auction() {
        let bids = vec![
            bid("fast", 0.40, 60, Some(20_000), Some(0.1)),
            bid("cheap", 0.10, 60, Some(5_000), Some(0.99)),
        ];
        assert_eq!(winner(bids, SelectionPolicy::Cheapest), "cheap");
    }

    #[test]
    fn rank_counts_exact_ties_for_rotation() {
        let mut bids = vec![
            bid("a", 0.2, 60, Some(10_000), Some(0.1)),
            bid("b", 0.2, 60, Some(10_100), Some(0.1)),
            bid("c", 0.9, 60, Some(10_000), Some(0.1)),
        ];
        assert_eq!(rank(&mut bids, SelectionPolicy::Smart, 900), 2);
    }

    fn gpu(mut b: JobBid) -> JobBid {
        b.gpu = Some(true);
        b
    }

    fn hints(upgrades: &[&str], won: &[(&str, u64)]) -> Hints {
        Hints {
            upgrades: upgrades.iter().map(|m| m.to_string()).collect(),
            last_won_ms: won.iter().map(|(p, t)| (p.to_string(), *t)).collect(),
            preferred: Vec::new(),
        }
    }

    fn prefer(peers: &[&str], won: &[(&str, u64)]) -> Hints {
        Hints { preferred: peers.iter().map(|p| p.to_string()).collect(), ..hints(&[], won) }
    }

    #[test]
    fn the_preferred_node_wins_over_its_peers_and_over_rotation() {
        // Two idle GPU nodes, same model. "warm" won most recently, so the
        // rotation would hand the job to "cold"; the preference keeps it warm.
        let warm = gpu(bid("warm", 0.002, 30, Some(1500), Some(0.0)));
        let cold = gpu(bid("cold", 0.001, 12, Some(1900), Some(0.0)));
        let h = prefer(&["warm"], &[("warm", 2_000), ("cold", 1_000)]);
        let mut bids = vec![cold.clone(), warm.clone()];
        let tied = rank_with(&mut bids, SelectionPolicy::Smart, 600, &h);
        assert_eq!(bids[0].bidder_peer_id, "warm");
        assert_eq!(tied, 1, "a preferred winner is never rotated away");
        // Without a preference the other one wins (rotation by last win).
        let mut bids = vec![warm.clone(), cold.clone()];
        rank_with(&mut bids, SelectionPolicy::Smart, 600, &hints(&[], &[("warm", 2_000), ("cold", 1_000)]));
        assert_eq!(bids[0].bidder_peer_id, "cold");
    }

    #[test]
    fn a_preference_never_beats_load_gpu_or_the_asked_for_model() {
        let h = prefer(&["warm"], &[]);
        // Overloaded preferred node: the free one wins.
        let mut bids = vec![gpu(bid("warm", 0.002, 30, Some(1500), Some(0.95))), gpu(bid("free", 0.002, 30, Some(1500), Some(0.0)))];
        rank_with(&mut bids, SelectionPolicy::Smart, 600, &h);
        assert_eq!(bids[0].bidder_peer_id, "free");
        // A preferred CPU node does not beat an idle GPU.
        let mut bids = vec![bid("warm", 0.001, 20, Some(9000), Some(0.0)), gpu(bid("gpu", 0.002, 25, Some(1400), Some(0.0)))];
        rank_with(&mut bids, SelectionPolicy::Smart, 600, &h);
        assert_eq!(bids[0].bidder_peer_id, "gpu");
        // A preferred node offering a smaller alternative is not preferred:
        // here only the preference could lift it over the rotation.
        let h = prefer(&["warm"], &[("warm", 2_000)]);
        let mut small = gpu(bid("warm", 0.002, 12, Some(1500), Some(0.2)));
        small.model = Some("small".into());
        small.fallback = Some(true);
        let mut exact = gpu(bid("exact", 0.002, 12, Some(1500), Some(0.2)));
        exact.model = Some("asked".into());
        exact.fallback = Some(false);
        let mut bids = vec![small, exact];
        rank_with(&mut bids, SelectionPolicy::Smart, 600, &h);
        assert_eq!(bids[0].bidder_peer_id, "exact");
    }

    #[test]
    fn an_idle_gpu_goes_before_cpu_bids() {
        // The CPU node has the better bench score and a similar ETA.
        let mut bids = vec![bid("cpu", 0.001, 20, Some(11264), Some(0.0)), gpu(bid("gpu", 0.002, 25, Some(1402), Some(0.0)))];
        rank(&mut bids, SelectionPolicy::Smart, 600);
        assert_eq!(bids[0].bidder_peer_id, "gpu");
        // Not when the GPU is overloaded.
        let mut bids = vec![bid("cpu", 0.001, 20, Some(11264), Some(0.0)), gpu(bid("gpu", 0.002, 25, Some(1402), Some(0.9)))];
        rank(&mut bids, SelectionPolicy::Smart, 600);
        assert_eq!(bids[0].bidder_peer_id, "cpu");
    }

    #[test]
    fn a_gpu_upgrade_does_not_wait_behind_an_idle_cpu_node() {
        // 2026-11-23: a chat for the 4B default. The CPU node serves the 4B,
        // the GPU answers with the 9B (flagged as an alternative).
        let mut cpu = bid("cpu-4b", 0.001, 67, Some(7254), Some(0.0));
        cpu.model = Some("4b".into());
        cpu.fallback = Some(false);
        let mut up = gpu(bid("gpu-9b", 0.002, 15, Some(1935), Some(0.0)));
        up.model = Some("9b".into());
        up.fallback = Some(true);
        let h = hints(&["9b"], &[]);
        let mut bids = vec![cpu.clone(), up.clone()];
        rank_with(&mut bids, SelectionPolicy::Smart, 600, &h);
        assert_eq!(bids[0].bidder_peer_id, "gpu-9b");
        // Without the hint the upgrade still waits (the old behaviour).
        let mut bids = vec![cpu.clone(), up.clone()];
        rank(&mut bids, SelectionPolicy::Smart, 600);
        assert_eq!(bids[0].bidder_peer_id, "cpu-4b");
        // A smaller model on a GPU still waits for the idle asked-for one,
        // and for an idle upgrade.
        let mut small = gpu(bid("gpu-1.5b", 0.001, 5, Some(1402), Some(0.0)));
        small.model = Some("1.5b".into());
        small.fallback = Some(true);
        let mut bids = vec![small.clone(), cpu];
        rank_with(&mut bids, SelectionPolicy::Smart, 600, &h);
        assert_eq!(bids[0].bidder_peer_id, "cpu-4b");
        let mut bids = vec![small, up];
        rank_with(&mut bids, SelectionPolicy::Smart, 600, &h);
        assert_eq!(bids[0].bidder_peer_id, "gpu-9b");
    }

    #[test]
    fn comparable_bids_take_turns() {
        // Two GPUs with similar ETAs: the higher bench score used to win
        // every job. Now the one that won longer ago goes first.
        let bids = || vec![gpu(bid("ours", 0.002, 20, Some(1935), Some(0.0))), gpu(bid("theirs", 0.002, 25, Some(1402), Some(0.0)))];
        let mut b = bids();
        rank_with(&mut b, SelectionPolicy::Smart, 600, &hints(&[], &[("ours", 2_000), ("theirs", 1_000)]));
        assert_eq!(b[0].bidder_peer_id, "theirs");
        let mut b = bids();
        rank_with(&mut b, SelectionPolicy::Smart, 600, &hints(&[], &[("ours", 2_000), ("theirs", 3_000)]));
        assert_eq!(b[0].bidder_peer_id, "ours");
        // A node that never won goes before one that has.
        let mut b = bids();
        rank_with(&mut b, SelectionPolicy::Smart, 600, &hints(&[], &[("ours", 2_000)]));
        assert_eq!(b[0].bidder_peer_id, "theirs");
        // A clearly faster node still wins whoever went last.
        let mut b = vec![bid("slow", 0.001, 90, Some(5000), Some(0.0)), bid("fast", 0.001, 20, Some(5000), Some(0.0))];
        rank_with(&mut b, SelectionPolicy::Smart, 600, &hints(&[], &[("fast", 9_000)]));
        assert_eq!(b[0].bidder_peer_id, "fast");
    }

    #[test]
    fn policy_parses() {
        assert_eq!("smart".parse::<SelectionPolicy>().unwrap(), SelectionPolicy::Smart);
        assert_eq!("cheapest".parse::<SelectionPolicy>().unwrap(), SelectionPolicy::Cheapest);
        assert!("nope".parse::<SelectionPolicy>().is_err());
    }
}
