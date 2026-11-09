//! Is this node doing paid work right now?
//!
//! The one question idle-donation workloads (the `crowdgpt` plugin) ask before
//! they start and every couple of seconds while they run. It is answered from
//! the paths every network job already goes through, not from a new load
//! heuristic:
//!
//! - **bids in flight**: [`crate::dispatch`] notes each bid it publishes with
//!   the bid's expiry. A standing bid is a job we may be handed in seconds.
//! - **jobs running**: dispatch marks a job started when it wins the accept
//!   and [`crate::runner::publish_receipt`] (the single receipt path every
//!   runner uses) marks it finished. A job whose receipt never goes out is
//!   forgotten after [`MAX_JOB_HOLD`], so one lost receipt cannot stop
//!   donation for good.
//! - **compute slots**: the supervisor's node-wide slot semaphore
//!   ([`crate::runner::WorkerLoad`]) is checked by the caller as well; a slot
//!   held by anything means busy.
//!
//! Process-wide state, like [`crate::private`]: every worker loop in this
//! process reports here.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Longest a won job counts as running without a receipt.
pub const MAX_JOB_HOLD: Duration = Duration::from_secs(12 * 3600);

static BID_UNTIL_MS: AtomicU64 = AtomicU64::new(0);
static LAST_ACTIVITY_MS: AtomicU64 = AtomicU64::new(0);
static RUNNING: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// A bid we published stands until `expires_at_ms`.
pub fn note_bid(expires_at_ms: u64) {
    BID_UNTIL_MS.fetch_max(expires_at_ms, Ordering::Relaxed);
    LAST_ACTIVITY_MS.fetch_max(now_ms(), Ordering::Relaxed);
}

/// We won `job_id`; it runs until its receipt goes out.
pub fn job_started(job_id: &str) {
    let now = now_ms();
    let mut g = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
    g.get_or_insert_with(HashMap::new).insert(job_id.to_string(), now);
    LAST_ACTIVITY_MS.fetch_max(now, Ordering::Relaxed);
}

/// `job_id`'s receipt went out (completed or failed).
pub fn job_finished(job_id: &str) {
    let mut g = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(m) = g.as_mut() {
        m.remove(job_id);
    }
    LAST_ACTIVITY_MS.fetch_max(now_ms(), Ordering::Relaxed);
}

/// Network jobs running now (receipt not yet sent, not older than [`MAX_JOB_HOLD`]).
pub fn running_jobs() -> usize {
    let now = now_ms();
    // Forget jobs whose receipt never went out.
    if let Some(m) = RUNNING.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        m.retain(|_, started| now.saturating_sub(*started) < MAX_JOB_HOLD.as_millis() as u64);
    }
    running_jobs_at(now)
}

fn running_jobs_at(now: u64) -> usize {
    let g = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
    let Some(m) = g.as_ref() else { return 0 };
    m.values().filter(|started| now.saturating_sub(**started) < MAX_JOB_HOLD.as_millis() as u64).count()
}

/// A bid is standing right now.
pub fn bid_in_flight() -> bool {
    BID_UNTIL_MS.load(Ordering::Relaxed) > now_ms()
}

/// Paid work is running or may arrive any second.
pub fn busy() -> bool {
    bid_in_flight() || running_jobs() > 0
}

/// How long since the last bid, win or receipt (`None` before the first).
pub fn since_last_activity() -> Option<Duration> {
    match LAST_ACTIVITY_MS.load(Ordering::Relaxed) {
        0 => None,
        t => Some(Duration::from_millis(now_ms().saturating_sub(t))),
    }
}

// ── failure backoff ──────────────────────────────────────────────────────
//
// A node whose jobs of a workload keep failing (renders killed by the host's
// OOM killer, or timing out on a box too loaded to finish them) kept bidding
// and winning, and the buyer's queue kept handing it jobs it could not
// finish: vienna's last six image and song jobs all hit sd-cli's 900 s
// timeout, and dev2's were OOM-killed seconds in (2026-11-09). After
// FAILS_BEFORE_BACKOFF failures in a row a workload stops bidding for a
// while, doubling each further failure, and a success clears it.

/// Failures in a row before a workload backs off.
pub const FAILS_BEFORE_BACKOFF: u32 = 2;
/// First pause; doubles per further failure.
pub const FIRST_BACKOFF: Duration = Duration::from_secs(15 * 60);
/// Longest pause.
pub const MAX_BACKOFF: Duration = Duration::from_secs(2 * 60 * 60);

static OUTCOMES: Mutex<Option<HashMap<String, (u32, u64)>>> = Mutex::new(None);

/// Pause after `fails` failures in a row (`None` while under the threshold).
pub fn backoff_for(fails: u32) -> Option<Duration> {
    let over = fails.checked_sub(FAILS_BEFORE_BACKOFF)?;
    Some(FIRST_BACKOFF.saturating_mul(1u32 << over.min(8)).min(MAX_BACKOFF))
}

/// A job of `workload` finished here: completed (`ok`) or failed.
pub fn note_outcome(workload: &str, ok: bool) {
    note_outcome_at(workload, ok, now_ms());
}

fn note_outcome_at(workload: &str, ok: bool, now: u64) {
    let mut g = OUTCOMES.lock().unwrap_or_else(|e| e.into_inner());
    let m = g.get_or_insert_with(HashMap::new);
    if ok {
        m.remove(workload);
        return;
    }
    let fails = m.get(workload).map(|(f, _)| f + 1).unwrap_or(1);
    let until = backoff_for(fails).map(|d| now + d.as_millis() as u64).unwrap_or(0);
    m.insert(workload.to_string(), (fails, until));
}

/// How much longer `workload` should not bid, if it is backing off.
pub fn bid_paused(workload: &str) -> Option<Duration> {
    bid_paused_at(workload, now_ms())
}

fn bid_paused_at(workload: &str, now: u64) -> Option<Duration> {
    let g = OUTCOMES.lock().unwrap_or_else(|e| e.into_inner());
    let (_, until) = g.as_ref()?.get(workload)?;
    (*until > now).then(|| Duration::from_millis(until - now))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bids_and_jobs_make_the_node_busy() {
        // Process-wide state: use ids no other test touches.
        job_started("activity-test-job");
        assert!(busy());
        assert!(running_jobs() >= 1);
        job_finished("activity-test-job");
        job_finished("activity-test-lost-receipt-other");
        // A standing bid alone is busy; an expired one is not.
        note_bid(now_ms() + 60_000);
        assert!(bid_in_flight());
        assert!(since_last_activity().unwrap() < Duration::from_secs(5));
    }

    #[test]
    fn a_job_without_a_receipt_is_forgotten() {
        job_started("activity-test-lost-receipt");
        let later = now_ms() + MAX_JOB_HOLD.as_millis() as u64 + 1;
        let before = running_jobs();
        assert!(running_jobs_at(later) < before);
    }

    #[test]
    fn repeated_failures_pause_bidding_until_a_success() {
        let w = "test.backoff";
        let t = 1_000_000_000;
        note_outcome_at(w, false, t);
        assert_eq!(bid_paused_at(w, t), None, "one failure is not a pattern");
        note_outcome_at(w, false, t);
        assert_eq!(bid_paused_at(w, t), Some(FIRST_BACKOFF));
        note_outcome_at(w, false, t);
        assert_eq!(bid_paused_at(w, t), Some(FIRST_BACKOFF * 2), "doubles");
        assert_eq!(bid_paused_at(w, t + 3 * 60 * 60 * 1000), None, "and expires");
        note_outcome_at(w, true, t);
        assert_eq!(bid_paused_at(w, t), None, "a success clears it");
        assert_eq!(backoff_for(20), Some(MAX_BACKOFF));
    }

}
