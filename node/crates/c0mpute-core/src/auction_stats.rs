//! How auctions are going, per workload, for this process.
//!
//! Every buyer auction ([`crate::buyer::run_auction`]) records how many bids
//! it got when its window closed, and the canary probes
//! ([`crate::buyer::probe_auction`]) record theirs separately. The speech
//! gateway serves the snapshot at `GET /v1/auctions` and the status aggregator
//! folds its canary into the status JSON, so "no bids received" stops being
//! something found only by grepping logs.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

/// Events older than this drop out of the rolling numbers.
pub const WINDOW: Duration = Duration::from_secs(60 * 60);

/// At most this many events are kept (a busy gateway runs a few thousand
/// auctions an hour).
const MAX_EVENTS: usize = 50_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A real buyer auction.
    Auction,
    /// A canary probe (offer published, bids counted, nothing accepted).
    Probe,
}

#[derive(Clone, Debug)]
struct Event {
    at_ms: u64,
    kind: Kind,
    workload: String,
    bids: usize,
    /// Nodes advertising the workload's role when the probe ran (probes only).
    capable: Option<usize>,
}

static EVENTS: Mutex<VecDeque<Event>> = Mutex::new(VecDeque::new());

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn push(event: Event) {
    let mut events = EVENTS.lock().unwrap_or_else(|e| e.into_inner());
    let cutoff = event.at_ms.saturating_sub(WINDOW.as_millis() as u64);
    while events.front().is_some_and(|e| e.at_ms < cutoff) || events.len() >= MAX_EVENTS {
        events.pop_front();
    }
    events.push_back(event);
}

/// A buyer auction's bid window closed with `bids` live bids.
pub fn record_auction(workload: &str, bids: usize) {
    push(Event {
        at_ms: now_ms(),
        kind: Kind::Auction,
        workload: workload.to_string(),
        bids,
        capable: None,
    });
}

/// A canary probe got `bids` bids while `capable` nodes advertised the role.
pub fn record_probe(workload: &str, bids: usize, capable: usize) {
    push(Event {
        at_ms: now_ms(),
        kind: Kind::Probe,
        workload: workload.to_string(),
        bids,
        capable: Some(capable),
    });
}

/// Rolling numbers for one workload and kind.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct Stats {
    pub auctions: usize,
    pub with_bids: usize,
    /// `with_bids / auctions`, `None` with no auctions.
    pub success_rate: Option<f64>,
    pub bids_total: usize,
    /// Probes only: those run while at least one capable node was advertised,
    /// and how many of them got a bid. This is the number the network is
    /// judged on (a probe with nobody able to serve cannot get a bid).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub with_capable: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub with_capable_and_bids: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capable_success_rate: Option<f64>,
    pub last_at_ms: Option<u64>,
    pub last_bids: Option<usize>,
}

fn summarize<'a>(events: impl Iterator<Item = &'a Event>, probes: bool) -> Stats {
    let mut s = Stats::default();
    let (mut with_capable, mut with_capable_and_bids) = (0, 0);
    for e in events {
        s.auctions += 1;
        s.bids_total += e.bids;
        if e.bids > 0 {
            s.with_bids += 1;
        }
        if e.capable.unwrap_or(0) > 0 {
            with_capable += 1;
            if e.bids > 0 {
                with_capable_and_bids += 1;
            }
        }
        if s.last_at_ms.is_none_or(|t| e.at_ms >= t) {
            s.last_at_ms = Some(e.at_ms);
            s.last_bids = Some(e.bids);
        }
    }
    s.success_rate = rate(s.with_bids, s.auctions);
    if probes {
        s.with_capable = Some(with_capable);
        s.with_capable_and_bids = Some(with_capable_and_bids);
        s.capable_success_rate = rate(with_capable_and_bids, with_capable);
    }
    s
}

fn rate(part: usize, whole: usize) -> Option<f64> {
    (whole > 0).then(|| (part as f64 / whole as f64 * 1000.0).round() / 1000.0)
}

/// Per-workload numbers over the last [`WINDOW`], for `kind`.
pub fn snapshot(kind: Kind) -> BTreeMap<String, Stats> {
    let cutoff = now_ms().saturating_sub(WINDOW.as_millis() as u64);
    let events = EVENTS.lock().unwrap_or_else(|e| e.into_inner());
    let mut workloads: Vec<&str> = events
        .iter()
        .filter(|e| e.kind == kind && e.at_ms >= cutoff)
        .map(|e| e.workload.as_str())
        .collect();
    workloads.sort_unstable();
    workloads.dedup();
    workloads
        .into_iter()
        .map(|w| {
            let stats = summarize(
                events
                    .iter()
                    .filter(|e| e.kind == kind && e.at_ms >= cutoff && e.workload == w),
                kind == Kind::Probe,
            );
            (w.to_string(), stats)
        })
        .collect()
}

/// The JSON served at the gateway's `GET /v1/auctions`.
pub fn snapshot_json() -> serde_json::Value {
    serde_json::json!({
        "window_secs": WINDOW.as_secs(),
        "auctions": snapshot(Kind::Auction),
        "canary": snapshot(Kind::Probe),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(workload: &str, bids: usize, capable: Option<usize>, at_ms: u64) -> Event {
        Event {
            at_ms,
            kind: if capable.is_some() { Kind::Probe } else { Kind::Auction },
            workload: workload.into(),
            bids,
            capable,
        }
    }

    #[test]
    fn auctions_summarize_to_a_success_rate() {
        let events = [ev("a", 0, None, 1), ev("a", 2, None, 3), ev("a", 1, None, 2)];
        let s = summarize(events.iter(), false);
        assert_eq!(s.auctions, 3);
        assert_eq!(s.with_bids, 2);
        assert_eq!(s.bids_total, 3);
        assert_eq!(s.success_rate, Some(0.667));
        assert_eq!(s.last_at_ms, Some(3));
        assert_eq!(s.last_bids, Some(2));
        assert_eq!(s.with_capable, None);
    }

    #[test]
    fn probes_are_judged_only_where_a_capable_node_was_advertised() {
        let events = [ev("v", 0, Some(0), 1), ev("v", 0, Some(2), 2), ev("v", 1, Some(1), 3)];
        let s = summarize(events.iter(), true);
        assert_eq!(s.success_rate, Some(0.333));
        assert_eq!(s.with_capable, Some(2));
        assert_eq!(s.with_capable_and_bids, Some(1));
        assert_eq!(s.capable_success_rate, Some(0.5));
    }

    #[test]
    fn recorded_events_show_up_in_the_snapshot() {
        record_auction("test.snapshot", 0);
        record_auction("test.snapshot", 4);
        record_probe("test.snapshot", 1, 3);
        let a = snapshot(Kind::Auction);
        assert_eq!(a["test.snapshot"].auctions, 2);
        assert_eq!(a["test.snapshot"].with_bids, 1);
        let p = snapshot(Kind::Probe);
        assert_eq!(p["test.snapshot"].capable_success_rate, Some(1.0));
        assert!(snapshot_json()["auctions"]["test.snapshot"].is_object());
    }
}
