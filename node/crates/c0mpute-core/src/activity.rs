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
    running_jobs_at(now_ms())
}

fn running_jobs_at(now: u64) -> usize {
    let mut g = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
    let Some(m) = g.as_mut() else { return 0 };
    m.retain(|_, started| now.saturating_sub(*started) < MAX_JOB_HOLD.as_millis() as u64);
    m.len()
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
}
