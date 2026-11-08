//! Buyer-side memory of workers that won and did not deliver.
//!
//! A worker that wins an auction and then fails the job, never sends a
//! receipt, or cannot be fetched from is benched for this gateway's
//! auctions: 30 minutes for the first strike, doubling per strike up to
//! 6 hours. A delivered job clears the record. Without this, a fast-bidding
//! worker that cannot deliver (behind NAT, too slow, broken runtime) wins
//! every auction and every caller waits out the full receipt timeout.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const FIRST_BENCH: Duration = Duration::from_secs(30 * 60);
pub const MAX_BENCH: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Clone, Default)]
pub struct PenaltyBox {
    inner: Arc<Mutex<HashMap<String, (Instant, u32)>>>,
}

/// How long the `strikes`-th strike benches a worker.
pub fn bench_for(strikes: u32) -> Duration {
    let factor = 1u32.checked_shl(strikes.saturating_sub(1)).unwrap_or(u32::MAX);
    FIRST_BENCH.saturating_mul(factor).min(MAX_BENCH)
}

impl PenaltyBox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Peers benched at `now`.
    pub fn benched_at(&self, now: Instant) -> Vec<String> {
        let map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.iter()
            .filter(|(_, (until, _))| *until > now)
            .map(|(peer, _)| peer.clone())
            .collect()
    }

    pub fn benched(&self) -> Vec<String> {
        self.benched_at(Instant::now())
    }

    /// Record a non-delivery; returns how long the peer is benched.
    pub fn strike_at(&self, peer: &str, now: Instant) -> Duration {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let strikes = map.get(peer).map(|(_, s)| s + 1).unwrap_or(1);
        let bench = bench_for(strikes);
        map.insert(peer.to_string(), (now + bench, strikes));
        bench
    }

    pub fn strike(&self, peer: &str) -> Duration {
        self.strike_at(peer, Instant::now())
    }

    /// A delivered job wipes the record.
    pub fn clear(&self, peer: &str) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).remove(peer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bench_doubles_and_caps() {
        assert_eq!(bench_for(1), Duration::from_secs(30 * 60));
        assert_eq!(bench_for(2), Duration::from_secs(60 * 60));
        assert_eq!(bench_for(3), Duration::from_secs(120 * 60));
        assert_eq!(bench_for(10), MAX_BENCH);
        assert_eq!(bench_for(100), MAX_BENCH);
    }

    #[test]
    fn struck_peers_are_benched_until_expiry_and_cleared_on_delivery() {
        let pb = PenaltyBox::new();
        let t0 = Instant::now();
        pb.strike_at("a", t0);
        assert_eq!(pb.benched_at(t0 + Duration::from_secs(60)), vec!["a".to_string()]);
        assert!(pb.benched_at(t0 + FIRST_BENCH + Duration::from_secs(1)).is_empty());
        assert_eq!(pb.strike_at("a", t0), Duration::from_secs(60 * 60), "second strike doubles");
        pb.clear("a");
        assert!(pb.benched_at(t0).is_empty());
    }
}
