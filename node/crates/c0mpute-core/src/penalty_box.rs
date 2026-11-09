//! Buyer-side memory of workers that won and did not deliver.
//!
//! A worker that wins an auction and then fails the job, never sends a
//! receipt, or cannot be fetched from is benched for this gateway's
//! auctions: 30 minutes for the first strike, doubling per strike up to
//! 6 hours. A delivered job clears the record. Without this, a fast-bidding
//! worker that cannot deliver (behind NAT, too slow, broken runtime) wins
//! every auction and every caller waits out the full receipt timeout.
//!
//! Persisted to a small JSON file when one is configured: gateways restart
//! on every auto-update, and an in-memory bench handed the same broken
//! worker a fresh turn after each restart (seen live 2026-11-09).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const FIRST_BENCH: Duration = Duration::from_secs(30 * 60);
pub const MAX_BENCH: Duration = Duration::from_secs(6 * 60 * 60);

type Records = HashMap<String, (u64, u32)>; // peer -> (benched until, unix secs; strikes)

#[derive(Clone, Default)]
pub struct PenaltyBox {
    inner: Arc<Mutex<Records>>,
    file: Option<Arc<PathBuf>>,
}

/// How long the `strikes`-th strike benches a worker.
pub fn bench_for(strikes: u32) -> Duration {
    let factor = 1u32.checked_shl(strikes.saturating_sub(1)).unwrap_or(u32::MAX);
    FIRST_BENCH.saturating_mul(factor).min(MAX_BENCH)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl PenaltyBox {
    pub fn new() -> Self {
        Self::default()
    }

    /// A box that loads from and saves to `path` (best effort, never fatal).
    pub fn persistent(path: PathBuf) -> Self {
        let records: Records = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self {
            inner: Arc::new(Mutex::new(records)),
            file: Some(Arc::new(path)),
        }
    }

    fn save(&self, records: &Records) {
        let Some(path) = &self.file else { return };
        if let Ok(json) = serde_json::to_string(records) {
            let tmp = path.with_extension("tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(&tmp, path.as_ref());
            }
        }
    }

    /// Peers benched at `now` (unix seconds).
    pub fn benched_at(&self, now: u64) -> Vec<String> {
        let map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.iter()
            .filter(|(_, (until, _))| *until > now)
            .map(|(peer, _)| peer.clone())
            .collect()
    }

    pub fn benched(&self) -> Vec<String> {
        self.benched_at(now_secs())
    }

    /// Record a non-delivery; returns how long the peer is benched.
    pub fn strike_at(&self, peer: &str, now: u64) -> Duration {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let strikes = map.get(peer).map(|(_, s)| s + 1).unwrap_or(1);
        let bench = bench_for(strikes);
        map.insert(peer.to_string(), (now + bench.as_secs(), strikes));
        self.save(&map);
        bench
    }

    pub fn strike(&self, peer: &str) -> Duration {
        self.strike_at(peer, now_secs())
    }

    /// A delivered job wipes the record.
    pub fn clear(&self, peer: &str) {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if map.remove(peer).is_some() {
            self.save(&map);
        }
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
        let t0 = 1_000_000;
        pb.strike_at("a", t0);
        assert_eq!(pb.benched_at(t0 + 60), vec!["a".to_string()]);
        assert!(pb.benched_at(t0 + FIRST_BENCH.as_secs() + 1).is_empty());
        assert_eq!(pb.strike_at("a", t0), Duration::from_secs(60 * 60), "second strike doubles");
        pb.clear("a");
        assert!(pb.benched_at(t0).is_empty());
    }

    #[test]
    fn the_bench_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("c0-penalty-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("penalties.json");
        let _ = std::fs::remove_file(&path);
        let now = now_secs();
        PenaltyBox::persistent(path.clone()).strike_at("broken-peer", now);
        let reloaded = PenaltyBox::persistent(path.clone());
        assert_eq!(reloaded.benched_at(now + 1), vec!["broken-peer".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
