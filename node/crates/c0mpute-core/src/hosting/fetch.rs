//! Multi-source chunk fetching, BitTorrent style.
//!
//! Replication and gateway reads used to fetch one chunk at a time from one
//! peer, waiting a full round trip for each 1 MiB, so a copy moved at one
//! chunk per round trip whatever the link could carry. Now:
//!
//! * many chunks are in flight at once (callers keep a window of them);
//! * each request goes to the source with the lowest expected wait, which is
//!   its measured time per chunk times the requests already queued on it, so
//!   load spreads over every holder in proportion to how fast it is;
//! * a request that is slow compared with that peer's own record is hedged:
//!   the same chunk is asked of the next best source and the first answer
//!   wins (work stealing from slow peers);
//! * a failed source is retried elsewhere and sorts last until it answers
//!   again; the DHT is the last resort;
//! * requests ride the peers' existing libp2p connections (one yamux stream
//!   per request), so nothing is dialled per chunk.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use c0mpute_net::{Libp2pNetwork, Network};
use c0mpute_proto::{ChunkRequest, Hash};
use futures::stream::{FuturesUnordered, StreamExt};

/// Chunks a bulk copy keeps in flight.
pub const BULK_WINDOW: usize = 32;
/// Chunks a gateway reads ahead of what it is sending.
pub const READ_AHEAD: usize = 8;
/// Most requests in flight to one peer. Each peer's own limit starts at
/// [`START_LIMIT`] and adapts (AIMD): up while its answers come back well
/// inside the timeout, halved when one is slow or fails, so a peer on a thin
/// uplink is never asked for more than it can send before requests time out.
const PER_PEER_CAP: f64 = 16.0;
const START_LIMIT: f64 = 4.0;
/// Assumed time per chunk for a peer never measured.
const UNKNOWN_MS: f64 = 400.0;

#[derive(Clone, Debug, Default)]
struct PeerStat {
    inflight: usize,
    /// Smoothed time per chunk, ms.
    ms: Option<f64>,
    /// Consecutive failures.
    fails: u32,
    last_fail: Option<Instant>,
    bytes: u64,
    /// Requests this peer may have in flight (AIMD), when adapted.
    limit: Option<f64>,
}

impl PeerStat {
    fn limit(&self) -> f64 {
        self.limit.unwrap_or(START_LIMIT)
    }
}

/// Per-peer speed and load, shared by every fetch on this node.
#[derive(Default)]
pub struct Stats {
    peers: Mutex<HashMap<String, PeerStat>>,
}

impl Stats {
    fn cost(&self, s: &PeerStat) -> f64 {
        let ms = s.ms.unwrap_or(UNKNOWN_MS);
        let penalty = match s.last_fail {
            // A peer that failed recently waits its turn behind everyone.
            Some(t) if t.elapsed() < Duration::from_secs(60) => 1.0 + 8.0 * s.fails.min(4) as f64,
            _ => 1.0,
        };
        ms * (s.inflight as f64 + 1.0) * penalty
    }

    /// Reserve a request slot on the best of `sources` that has one free
    /// (skipping `tried`). `None` when every untried source is at its limit
    /// or there is none.
    fn take(&self, sources: &[String], tried: &[String], turn: usize) -> Option<String> {
        let order = self.rank(sources, turn);
        let mut peers = self.peers.lock().expect("stats lock");
        for p in order {
            if tried.contains(&p) {
                continue;
            }
            let s = peers.entry(p.clone()).or_default();
            if (s.inflight as f64) < s.limit() {
                s.inflight += 1;
                return Some(p);
            }
        }
        None
    }

    /// `sources` best first for one more request right now.
    pub fn rank(&self, sources: &[String], turn: usize) -> Vec<String> {
        let peers = self.peers.lock().expect("stats lock");
        let mut v: Vec<(f64, usize, String)> = sources
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let s = peers.get(p).cloned().unwrap_or_default();
                // Rotate among equals so unmeasured peers all get tried.
                let rot = (i + turn) % sources.len().max(1);
                (self.cost(&s), rot, p.clone())
            })
            .collect();
        v.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        v.into_iter().map(|(_, _, p)| p).collect()
    }

    #[cfg(test)]
    fn start(&self, peer: &str) {
        self.peers
            .lock()
            .expect("stats lock")
            .entry(peer.to_string())
            .or_default()
            .inflight += 1;
    }

    fn finish(&self, peer: &str, ok: Option<(Duration, usize)>, timeout: Duration) {
        let mut peers = self.peers.lock().expect("stats lock");
        let s = peers.entry(peer.to_string()).or_default();
        s.inflight = s.inflight.saturating_sub(1);
        let limit = s.limit();
        match ok {
            Some((took, _)) if took > timeout / 2 => s.limit = Some((limit / 2.0).max(1.0)),
            Some((took, _)) if took < timeout / 4 => {
                s.limit = Some((limit + 0.5).min(PER_PEER_CAP))
            }
            Some(_) => {}
            None => s.limit = Some((limit / 2.0).max(1.0)),
        }
        match ok {
            Some((took, len)) => {
                let ms = (took.as_secs_f64() * 1000.0).max(1.0);
                s.ms = Some(match s.ms {
                    Some(old) => old * 0.7 + ms * 0.3,
                    None => ms,
                });
                s.fails = 0;
                s.bytes += len as u64;
            }
            None => {
                s.fails += 1;
                s.last_fail = Some(Instant::now());
            }
        }
    }

    /// Expected time per chunk for a peer, when measured.
    pub fn ms(&self, peer: &str) -> Option<f64> {
        self.peers
            .lock()
            .expect("stats lock")
            .get(peer)
            .and_then(|s| s.ms)
    }

    /// Bytes received from each peer so far.
    pub fn bytes_by_peer(&self) -> Vec<(String, u64)> {
        let mut v: Vec<(String, u64)> = self
            .peers
            .lock()
            .expect("stats lock")
            .iter()
            .filter(|(_, s)| s.bytes > 0)
            .map(|(p, s)| (p.clone(), s.bytes))
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        v
    }
}

/// How long to wait on a request before hedging it with another source.
fn hedge_after(ms: Option<f64>) -> Duration {
    let base = ms.unwrap_or(UNKNOWN_MS) * 3.0;
    Duration::from_millis(base.clamp(800.0, 6000.0) as u64)
}

/// Fetch one chunk from the best of `sources` (never `me`), hedging slow
/// requests and failing over, then the DHT. Bytes are checked against the
/// hash.
pub async fn fetch_one(
    net: &Libp2pNetwork,
    stats: &Stats,
    me: &str,
    hash: &Hash,
    sources: &[String],
    timeout: Duration,
    turn: usize,
) -> Result<Vec<u8>> {
    let sources: Vec<String> = sources.iter().filter(|p| *p != me).cloned().collect();
    let req = ChunkRequest {
        chunk_hash: *hash,
        shard_index: None,
    };
    let mut tried: Vec<String> = Vec::new();
    let mut inflight = FuturesUnordered::new();
    let mut last = String::from("no holders");
    let launch = |peer: String| {
        let req = req.clone();
        async move {
            let Ok(pid) = peer.parse::<c0mpute_net::PeerId>() else {
                return (peer, Err(anyhow::anyhow!("not a peer id")), Duration::ZERO);
            };
            let started = Instant::now();
            let got = match tokio::time::timeout(timeout, net.fetch_from_peer(pid, &req)).await {
                Ok(r) => r,
                Err(_) => Err(anyhow::anyhow!("timed out")),
            };
            (peer, got, started.elapsed())
        }
    };
    let begun = Instant::now();
    let mut hedge = hedge_after(None);
    // Wait for a free slot on some source (every source busy = back off).
    while !sources.is_empty() && inflight.is_empty() && begun.elapsed() < timeout {
        match stats.take(&sources, &tried, turn) {
            Some(p) => {
                hedge = hedge_after(stats.ms(&p));
                tried.push(p.clone());
                inflight.push(launch(p));
            }
            None if tried.len() >= sources.len() => break,
            None => tokio::time::sleep(Duration::from_millis(40)).await,
        }
    }
    while !inflight.is_empty() {
        tokio::select! {
            done = inflight.next() => {
                let Some((peer, got, took)) = done else { break };
                match got {
                    Ok(bytes) if Hash::of(&bytes) == *hash => {
                        stats.finish(&peer, Some((took, bytes.len())), timeout);
                        return Ok(bytes);
                    }
                    Ok(_) => {
                        stats.finish(&peer, None, timeout);
                        last = format!("{peer} sent bytes that do not match the hash");
                    }
                    Err(e) => {
                        stats.finish(&peer, None, timeout);
                        last = format!("{peer}: {e}");
                    }
                }
                // Fail over: the next source with a free slot.
                while inflight.is_empty() && tried.len() < sources.len() && begun.elapsed() < timeout * 2 {
                    match stats.take(&sources, &tried, turn) {
                        Some(p) => {
                            hedge = hedge_after(stats.ms(&p));
                            tried.push(p.clone());
                            inflight.push(launch(p));
                        }
                        None => tokio::time::sleep(Duration::from_millis(40)).await,
                    }
                }
            }
            _ = tokio::time::sleep(hedge), if inflight.len() < 2 => {
                // Slow for this peer: ask the next best one too, if it has room.
                if let Some(p) = stats.take(&sources, &tried, turn) {
                    tried.push(p.clone());
                    inflight.push(launch(p));
                } else {
                    hedge = timeout;
                }
            }
        }
    }
    // Last resort: the DHT (any peer that announced the chunk).
    match tokio::time::timeout(timeout, net.fetch(&req)).await {
        Ok(Ok(bytes)) if Hash::of(&bytes) == *hash => Ok(bytes),
        _ => bail!("could not fetch chunk {}: {last}", hash.to_hex()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn faster_and_idler_peers_rank_first() {
        let s = Stats::default();
        let peers = vec!["slow".to_string(), "fast".to_string(), "new".to_string()];
        s.start("slow");
        s.finish(
            "slow",
            Some((Duration::from_millis(900), 1 << 20)),
            Duration::from_secs(20),
        );
        s.start("fast");
        s.finish(
            "fast",
            Some((Duration::from_millis(100), 1 << 20)),
            Duration::from_secs(20),
        );
        assert_eq!(s.rank(&peers, 0)[0], "fast");
        // Load the fast peer until the unmeasured one is cheaper.
        for _ in 0..6 {
            s.start("fast");
        }
        assert_eq!(s.rank(&peers, 0)[0], "new");
        // A failed peer sorts last.
        s.start("new");
        s.finish("new", None, Duration::from_secs(20));
        assert_eq!(s.rank(&peers, 0).last().unwrap(), "new");
    }

    #[test]
    fn per_peer_limits_adapt_to_how_fast_answers_come_back() {
        let s = Stats::default();
        let t = Duration::from_secs(20);
        let one = vec!["p".to_string()];
        // Starts at 4 in flight.
        for _ in 0..4 {
            assert!(s.take(&one, &[], 0).is_some());
        }
        assert!(s.take(&one, &[], 0).is_none(), "at its limit");
        // Quick answers raise the limit.
        for _ in 0..4 {
            s.finish("p", Some((Duration::from_millis(200), 1 << 20)), t);
        }
        let mut n = 0;
        while s.take(&one, &[], 0).is_some() {
            n += 1;
        }
        assert_eq!(n, 6);
        // A slow answer halves it.
        s.finish("p", Some((Duration::from_secs(15), 1 << 20)), t);
        for _ in 0..5 {
            s.finish("p", Some((Duration::from_secs(6), 1 << 20)), t);
        }
        for _ in 0..3 {
            assert!(s.take(&one, &[], 0).is_some());
        }
        assert!(s.take(&one, &[], 0).is_none(), "limit halved to 3");
        // Tried peers are skipped.
        assert!(s.take(&one, &["p".to_string()], 0).is_none());
    }

    #[test]
    fn hedging_waits_longer_for_slow_peers() {
        assert_eq!(hedge_after(Some(10.0)), Duration::from_millis(800));
        assert_eq!(hedge_after(Some(1000.0)), Duration::from_millis(3000));
        assert_eq!(hedge_after(Some(1e6)), Duration::from_millis(6000));
    }
}
