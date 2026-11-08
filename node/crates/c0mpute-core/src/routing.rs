//! Smart routing: which bid wins an auction.
//!
//! The default policy sends a job to the most capable worker that is not
//! overloaded, instead of simply the cheapest bid:
//!
//!   1. Overloaded bids go last: a queue load at or above
//!      [`OVERLOAD_THRESHOLD`], or an ETA the deadline cannot absorb. If
//!      every bid is overloaded, the least loaded wins.
//!   2. Among the rest, the highest benchmark score wins. Scores within
//!      about 10% of each other are treated as equal (one bucket), so a
//!      marginally faster node does not take every job.
//!   3. Then the lower ETA, then the lower price, then the earlier bid.
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

use c0mpute_net::topics::JobBid;

/// Queue load (0.0 empty .. 1.0 full) at which a worker counts as overloaded.
pub const OVERLOAD_THRESHOLD: f32 = 0.85;

/// Benchmark scores within this ratio share a bucket.
const BENCH_BUCKET_RATIO: f64 = 1.10;

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

/// Highest bench score among the bids (0 when none is benchmarked).
pub fn best_bench(bids: &[JobBid]) -> u32 {
    bids.iter().filter_map(|b| b.bench_score).max().unwrap_or(0)
}

fn price_cmp(a: &JobBid, b: &JobBid) -> Ordering {
    a.price_usd.partial_cmp(&b.price_usd).unwrap_or(Ordering::Equal)
}

/// Order two bids under `policy`, ignoring publish time (the tie-breaker).
pub fn compare(a: &JobBid, b: &JobBid, policy: SelectionPolicy, deadline_secs: u64, best: u32) -> Ordering {
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
                .then_with(|| bench_bucket(a, best).cmp(&bench_bucket(b, best)))
                .then_with(|| a.eta_seconds.cmp(&b.eta_seconds))
                .then_with(|| price_cmp(a, b))
        }
    }
}

/// Sort `bids` best first and return how many share the top spot exactly
/// (so the caller can rotate among them).
pub fn rank(bids: &mut [JobBid], policy: SelectionPolicy, deadline_secs: u64) -> usize {
    let best = best_bench(bids);
    bids.sort_by(|a, b| {
        compare(a, b, policy, deadline_secs, best).then(a.published_at_ms.cmp(&b.published_at_ms))
    });
    match bids.first() {
        None => 0,
        Some(top) => bids
            .iter()
            .take_while(|b| compare(b, top, policy, deadline_secs, best) == Ordering::Equal)
            .count(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn policy_parses() {
        assert_eq!("smart".parse::<SelectionPolicy>().unwrap(), SelectionPolicy::Smart);
        assert_eq!("cheapest".parse::<SelectionPolicy>().unwrap(), SelectionPolicy::Cheapest);
        assert!("nope".parse::<SelectionPolicy>().is_err());
    }
}
