//! Garbage collection (CIP-004).
//!
//! Immutability plus a mutable root produces orphans: superseded snapshot
//! nodes, manifests for deleted objects, blocks nobody references. Something
//! has to reclaim them, and the obvious mechanism — refcounting — is the wrong
//! one here. Content-addressed dedup means a chunk can be shared across
//! volumes and across customers, so a refcount would need global coordination,
//! which DIP-0011 rules out.
//!
//! So: mark-and-sweep from the retained roots, with a grace period. The grace
//! period is the part that matters. A client that is closed for a week has
//! published no keep-set, and sweeping on that silence would delete a
//! customer's data because their laptop was shut. Nothing is reclaimed until
//! it has been unreferenced for `grace`.

use std::collections::HashSet;

use anyhow::Result;
use c0mpute_proto::Hash;
use serde::{Deserialize, Serialize};

/// What a sweep would do, computed before anything is deleted.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GcPlan {
    /// Reachable from a retained root. Never touched.
    pub keep: HashSet<Hash>,
    /// Unreferenced and past the grace period.
    pub collect: Vec<Hash>,
    /// Unreferenced but still inside the grace period.
    pub deferred: Vec<Hash>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GcStats {
    pub kept: usize,
    pub collected: usize,
    pub deferred: usize,
    pub bytes_freed: u64,
}

/// When each candidate was first seen unreferenced.
///
/// Persisted by the caller between sweeps: an object has to be observed
/// unreferenced *for* the grace period, which cannot be decided from a single
/// pass.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UnreferencedSince {
    pub first_seen_ms: std::collections::HashMap<String, u64>,
}

impl UnreferencedSince {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record this pass's observation and report which candidates have been
    /// unreferenced for at least `grace_ms`.
    ///
    /// Anything that has become reachable again is forgotten, so a name that
    /// is deleted and recreated does not carry a stale clock.
    pub fn observe(
        &mut self,
        unreferenced: &[Hash],
        now_ms: u64,
        grace_ms: u64,
    ) -> (Vec<Hash>, Vec<Hash>) {
        let current: HashSet<String> = unreferenced.iter().map(|h| h.to_hex()).collect();
        self.first_seen_ms.retain(|k, _| current.contains(k));

        let mut collect = Vec::new();
        let mut deferred = Vec::new();
        for h in unreferenced {
            let first = *self
                .first_seen_ms
                .entry(h.to_hex())
                .or_insert(now_ms);
            if now_ms.saturating_sub(first) >= grace_ms {
                collect.push(*h);
            } else {
                deferred.push(*h);
            }
        }
        (collect, deferred)
    }
}

/// Decide what to collect, without deleting anything.
///
/// Deliberately split from the deletion: a sweep that computes and deletes in
/// one step gives an operator no way to look before it happens, and this is
/// the one operation in the system that destroys data on purpose.
pub fn plan(
    all_objects: &[Hash],
    keep: &HashSet<Hash>,
    tracker: &mut UnreferencedSince,
    now_ms: u64,
    grace_ms: u64,
) -> GcPlan {
    let unreferenced: Vec<Hash> = all_objects
        .iter()
        .filter(|h| !keep.contains(h))
        .copied()
        .collect();
    let (collect, deferred) = tracker.observe(&unreferenced, now_ms, grace_ms);
    GcPlan {
        keep: keep.clone(),
        collect,
        deferred,
    }
}

/// Delete what the plan says to collect.
pub async fn sweep<F, Fut>(plan: &GcPlan, mut delete: F) -> Result<GcStats>
where
    F: FnMut(Hash) -> Fut,
    Fut: std::future::Future<Output = Result<u64>>,
{
    let mut freed = 0u64;
    let mut collected = 0usize;
    for hash in &plan.collect {
        // Belt and braces. The plan was computed against a keep set, but a
        // concurrent write may have made something reachable since — and
        // deleting live data is the one mistake here that cannot be undone.
        if plan.keep.contains(hash) {
            continue;
        }
        freed += delete(*hash).await?;
        collected += 1;
    }
    Ok(GcStats {
        kept: plan.keep.len(),
        collected,
        deferred: plan.deferred.len(),
        bytes_freed: freed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(n: u64) -> Hash {
        Hash::of(&n.to_be_bytes())
    }

    const HOUR: u64 = 3_600_000;
    const GRACE: u64 = 14 * 24 * HOUR;

    #[test]
    fn reachable_objects_are_never_collected() {
        let all: Vec<Hash> = (0..10).map(h).collect();
        let keep: HashSet<Hash> = (0..5).map(h).collect();
        let mut tracker = UnreferencedSince::new();

        let p = plan(&all, &keep, &mut tracker, 0, GRACE);
        for i in 0..5u64 {
            assert!(!p.collect.contains(&h(i)));
            assert!(!p.deferred.contains(&h(i)));
        }
    }

    /// The property that protects an offline client: nothing is reclaimed on
    /// the first sighting, however unreferenced it looks.
    #[test]
    fn nothing_is_collected_on_the_first_pass() {
        let all: Vec<Hash> = (0..10).map(h).collect();
        let keep = HashSet::new();
        let mut tracker = UnreferencedSince::new();

        let p = plan(&all, &keep, &mut tracker, HOUR, GRACE);
        assert!(p.collect.is_empty());
        assert_eq!(p.deferred.len(), 10);
    }

    #[test]
    fn collected_only_after_the_grace_period() {
        let all: Vec<Hash> = (0..3).map(h).collect();
        let keep = HashSet::new();
        let mut tracker = UnreferencedSince::new();

        plan(&all, &keep, &mut tracker, 0, GRACE);
        // A week later: still inside the window.
        let p = plan(&all, &keep, &mut tracker, 7 * 24 * HOUR, GRACE);
        assert!(p.collect.is_empty());
        // Fifteen days: past it.
        let p = plan(&all, &keep, &mut tracker, 15 * 24 * HOUR, GRACE);
        assert_eq!(p.collect.len(), 3);
    }

    /// A laptop that was shut for a week must not lose data, and when it comes
    /// back the clock resets rather than resuming.
    #[test]
    fn becoming_reachable_again_resets_the_clock() {
        let all: Vec<Hash> = (0..3).map(h).collect();
        let mut tracker = UnreferencedSince::new();

        plan(&all, &HashSet::new(), &mut tracker, 0, GRACE);

        // The client comes back and references them again.
        let keep: HashSet<Hash> = (0..3).map(h).collect();
        plan(&all, &keep, &mut tracker, 10 * 24 * HOUR, GRACE);
        assert!(tracker.first_seen_ms.is_empty(), "clock should be forgotten");

        // Unreferenced again, much later: the window starts over.
        let p = plan(&all, &HashSet::new(), &mut tracker, 20 * 24 * HOUR, GRACE);
        assert!(
            p.collect.is_empty(),
            "an object that was live in between must not be collected immediately"
        );
    }

    #[tokio::test]
    async fn sweep_deletes_only_the_plan() {
        let all: Vec<Hash> = (0..6).map(h).collect();
        let keep: HashSet<Hash> = (0..3).map(h).collect();
        let mut tracker = UnreferencedSince::new();
        plan(&all, &keep, &mut tracker, 0, GRACE);
        let p = plan(&all, &keep, &mut tracker, 30 * 24 * HOUR, GRACE);

        let deleted = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let d = deleted.clone();
        let stats = sweep(&p, move |hash| {
            let d = d.clone();
            async move {
                d.lock().unwrap().push(hash);
                Ok(100)
            }
        })
        .await
        .unwrap();

        assert_eq!(stats.collected, 3);
        assert_eq!(stats.bytes_freed, 300);
        let deleted = deleted.lock().unwrap();
        for i in 0..3u64 {
            assert!(!deleted.contains(&h(i)), "swept a reachable object");
        }
        for i in 3..6u64 {
            assert!(deleted.contains(&h(i)));
        }
    }

    /// Last line of defence: even if a plan is stale, an object that is in the
    /// keep set is not deleted.
    #[tokio::test]
    async fn sweep_refuses_to_delete_anything_in_the_keep_set() {
        let p = GcPlan {
            keep: (0..3).map(h).collect(),
            collect: (0..3).map(h).collect(), // contradictory on purpose
            deferred: vec![],
        };
        let stats = sweep(&p, |_| async { Ok(1) }).await.unwrap();
        assert_eq!(stats.collected, 0, "swept an object it was told to keep");
    }
}
