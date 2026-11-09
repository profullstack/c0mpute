//! One identity running in two places at once (a "twin").
//!
//! A peer id is a key pair on disk. Copy the identity dir to a second machine,
//! or run a system and a user service on one box, and two processes bid under
//! one peer id with different per-job keys. The buyer seals the spec to one
//! of them; the other cannot open it. Before 0.3.17 that one answered with a
//! Failed receipt, the buyer took it as the result and benched the node.
//!
//! Every process now carries a random instance id
//! ([`c0mpute_net::Libp2pNetwork::instance_id`]) on its bids, receipts and
//! capability ads, and buyers name the winning instance on the accept. A
//! process that sees an accept for its peer id naming another instance has a
//! twin. The older of the two (lower instance id: ids sort by start time)
//! stops bidding while the twin is active, so jobs never split between them;
//! the newer keeps working. Both say so in their capability ad, which
//! c0mpute.com/nodes shows.
//!
//! Gossipsub never delivers a message to the peer id that published it, so
//! twins cannot see each other's ads directly: evidence comes from accepts
//! (and sealed specs that do not open), and the node board sees both ads.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use tracing::{info, warn};

/// How long one piece of evidence keeps the older twin out of auctions. Each
/// new accept for the twin refreshes it; once the twin is gone (stopped,
/// removed) the older one resumes bidding by itself.
pub const TWIN_HOLD: Duration = Duration::from_secs(15 * 60);

/// What one process (keyed by its own instance id) knows about its twin.
#[derive(Clone, Debug)]
struct Seen {
    /// The twin's instance id, when the evidence named it.
    other: Option<String>,
    at: Instant,
    count: u64,
    how: &'static str,
}

fn state() -> &'static Mutex<HashMap<String, Seen>> {
    static S: OnceLock<Mutex<HashMap<String, Seen>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

/// Record evidence that another process runs our identity. `ours` is this
/// process's instance id, `other` the twin's when known.
pub fn note(ours: &str, other: Option<&str>, how: &'static str) {
    if other == Some(ours) {
        return;
    }
    let mut map = state().lock().unwrap_or_else(|e| e.into_inner());
    let first = !map.get(ours).is_some_and(|s| s.at.elapsed() < TWIN_HOLD);
    let entry = map.entry(ours.to_string()).or_insert(Seen {
        other: None,
        at: Instant::now(),
        count: 0,
        how,
    });
    entry.at = Instant::now();
    entry.count += 1;
    entry.how = how;
    if let Some(o) = other {
        entry.other = Some(o.to_string());
    }
    let yielding = is_older(ours, entry.other.as_deref());
    if first || entry.count % 20 == 0 {
        warn!(
            instance = ours,
            twin = entry.other.as_deref().unwrap_or("unknown"),
            how,
            yielding,
            "DUPLICATE IDENTITY: another c0mpute worker runs this node's identity (a copied identity dir, or a \
             second service on this box). {} Run `c0mpute doctor` on each machine; give a copy its own identity \
             by deleting identity.key in its config dir before starting it.",
            if yielding {
                "This process is the older one and stops bidding while the twin is active."
            } else {
                "This process is the newer one and keeps working; the older one stops bidding."
            }
        );
    }
}

/// `ours` is the older instance (it sorts first), so it yields. Unknown
/// twin: no order, nobody yields on it.
fn is_older(ours: &str, other: Option<&str>) -> bool {
    other.is_some_and(|o| ours < o)
}

/// Whether this process should stay out of auctions now: a newer twin was
/// seen within [`TWIN_HOLD`].
pub fn yielding(ours: &str) -> bool {
    let map = state().lock().unwrap_or_else(|e| e.into_inner());
    map.get(ours)
        .is_some_and(|s| s.at.elapsed() < TWIN_HOLD && is_older(ours, s.other.as_deref()))
}

/// The `twin` object for the capability ad, while a twin is active.
pub fn ad_field(ours: &str) -> Option<serde_json::Value> {
    let map = state().lock().unwrap_or_else(|e| e.into_inner());
    let s = map.get(ours).filter(|s| s.at.elapsed() < TWIN_HOLD)?;
    Some(serde_json::json!({
        "other": s.other,
        "seen_s": s.at.elapsed().as_secs(),
        "count": s.count,
        "how": s.how,
        "yielding": is_older(ours, s.other.as_deref()),
    }))
}

/// Log once when a held-back twin resumes (called from the bidding path).
pub fn log_resume_if_cleared(ours: &str, was_yielding: &mut bool) {
    let now = yielding(ours);
    if *was_yielding && !now {
        info!(instance = ours, "duplicate identity: no twin seen for a while; bidding again");
    }
    *was_yielding = now;
}

#[cfg(test)]
pub(crate) fn clear(ours: &str) {
    state().lock().unwrap().remove(ours);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_older_twin_yields_and_the_newer_keeps_bidding() {
        let old = "0000000000001-aaaaaaaa";
        let new = "0000000000002-bbbbbbbb";
        clear(old);
        clear(new);
        note(old, Some(new), "test");
        note(new, Some(old), "test");
        assert!(yielding(old));
        assert!(!yielding(new));
        assert_eq!(ad_field(old).unwrap()["yielding"], true);
        assert_eq!(ad_field(new).unwrap()["yielding"], false);
    }

    #[test]
    fn an_unknown_twin_is_reported_but_nobody_yields() {
        let me = "0000000000003-cccccccc";
        clear(me);
        note(me, None, "test");
        assert!(!yielding(me));
        assert!(ad_field(me).is_some());
        // Our own instance is never a twin.
        let solo = "0000000000004-dddddddd";
        clear(solo);
        note(solo, Some(solo), "test");
        assert!(ad_field(solo).is_none());
    }
}
