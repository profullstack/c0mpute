//! What a node seeds: the candidates it is allowed to seed, cut to its cap.
//!
//! The only inputs are a manifest that already verified (plus the list
//! compiled into the binary) and the operator's own local config. Nothing a
//! peer, a job or a gateway request says is an input here.

use std::collections::HashSet;
use std::path::PathBuf;

use crate::manifest::{Entry, normalize_info_hash};

/// Where a candidate came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Named in our signed manifest (or compiled in); fetched by info hash
    /// from the trackers the manifest lists.
    Published { trackers: Vec<String> },
    /// A `.torrent` file the operator listed in `[seeding] local_torrents`.
    Local { torrent: PathBuf },
}

impl Origin {
    pub fn label(&self) -> &'static str {
        match self {
            Origin::Published { .. } => "published",
            Origin::Local { .. } => "local",
        }
    }
}

/// One torrent the node may seed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub info_hash: String,
    pub name: String,
    /// Bytes on disk once complete (for a published torrent: an upper bound
    /// the engine enforces).
    pub bytes: u64,
    pub license: String,
    pub source: String,
    pub origin: Origin,
}

/// A candidate left out, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skipped {
    pub candidate: Candidate,
    pub reason: String,
}

/// The outcome of planning.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub seed: Vec<Candidate>,
    pub skipped: Vec<Skipped>,
}

impl Plan {
    /// Bytes the planned torrents take when complete.
    pub fn bytes(&self) -> u64 {
        self.seed.iter().map(|c| c.bytes).sum()
    }
}

/// The operator's narrowing of the published list.
#[derive(Clone, Debug, Default)]
pub struct Narrow {
    /// When not empty, only these info hashes are taken from the published list.
    pub only: Vec<String>,
    /// Info hashes never taken from the published list.
    pub skip: Vec<String>,
}

impl Narrow {
    fn set(list: &[String]) -> HashSet<String> {
        list.iter().filter_map(|h| normalize_info_hash(h)).collect()
    }

    /// `None` when `info_hash` passes, else why it does not.
    fn refuses(&self, info_hash: &str) -> Option<&'static str> {
        let only = Self::set(&self.only);
        if !self.only.is_empty() && !only.contains(info_hash) {
            return Some("not in [seeding] only");
        }
        Self::set(&self.skip)
            .contains(info_hash)
            .then_some("listed in [seeding] skip")
    }
}

/// The candidates: published entries (compiled in, then the verified
/// manifest's, in its order) that pass the operator's `only`/`skip`, then the
/// operator's local torrents. `published` must come from a verified manifest;
/// this function is where the list of what a node may seed is put together,
/// and it has no other source.
pub fn candidates(
    published: &[Entry],
    narrow: &Narrow,
    local: Vec<Candidate>,
) -> (Vec<Candidate>, Vec<Skipped>) {
    let mut out = Vec::new();
    let mut skipped = Vec::new();
    let mut seen = HashSet::new();
    let published = published.iter().map(|e| Candidate {
        info_hash: e.info_hash.clone(),
        name: e.name.clone(),
        bytes: e.bytes,
        license: e.license.clone(),
        source: e.source.clone(),
        origin: Origin::Published {
            trackers: e.trackers.clone(),
        },
    });
    for c in published.chain(local) {
        if !seen.insert(c.info_hash.clone()) {
            continue;
        }
        let refused = match c.origin {
            Origin::Published { .. } => narrow.refuses(&c.info_hash),
            Origin::Local { .. } => None,
        };
        match refused {
            Some(reason) => skipped.push(Skipped {
                candidate: c,
                reason: reason.to_string(),
            }),
            None => out.push(c),
        }
    }
    (out, skipped)
}

/// Cut `candidates` to `cap_bytes`, in order: a torrent is taken when it
/// still fits whole, and skipped otherwise (a smaller one after it may still
/// fit). The plan never adds up to more than the cap.
pub fn plan(candidates: Vec<Candidate>, cap_bytes: u64) -> Plan {
    let mut out = Plan::default();
    let mut used = 0u64;
    for c in candidates {
        match used.checked_add(c.bytes) {
            Some(total) if total <= cap_bytes => {
                used = total;
                out.seed.push(c);
            }
            _ => out.skipped.push(Skipped {
                reason: format!(
                    "{} bytes would exceed the seeding cap ({} of {cap_bytes} bytes planned)",
                    c.bytes, used
                ),
                candidate: c,
            }),
        }
    }
    out
}

/// The cap that applies right now: `max_bytes`, lowered when the disk cannot
/// take that much without going under `floor_bytes` free. `held_bytes` is
/// what seeding already has on this disk (it is not free space, but it is
/// already ours). An unknown disk (`disk_total == 0`) leaves `max_bytes`.
pub fn effective_cap(
    max_bytes: u64,
    held_bytes: u64,
    disk_free: u64,
    disk_total: u64,
    floor_bytes: u64,
) -> u64 {
    if disk_total == 0 {
        return max_bytes;
    }
    max_bytes.min(held_bytes.saturating_add(disk_free.saturating_sub(floor_bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::testkit::{HASH_A, HASH_B, entry};

    const HASH_C: &str = "cccccccccccccccccccccccccccccccccccccccc";

    fn local(info_hash: &str, bytes: u64) -> Candidate {
        Candidate {
            info_hash: info_hash.to_string(),
            name: "mine".into(),
            bytes,
            license: String::new(),
            source: String::new(),
            origin: Origin::Local {
                torrent: PathBuf::from("/tmp/mine.torrent"),
            },
        }
    }

    fn hashes(list: &[Candidate]) -> Vec<&str> {
        list.iter().map(|c| c.info_hash.as_str()).collect()
    }

    #[test]
    fn a_torrent_that_would_exceed_the_cap_is_skipped() {
        let (c, _) = candidates(
            &[entry(HASH_A, 60), entry(HASH_B, 50), entry(HASH_C, 40)],
            &Narrow::default(),
            vec![],
        );
        let p = plan(c.clone(), 100);
        // B would take the total to 110: skipped. C still fits after A.
        assert_eq!(hashes(&p.seed), [HASH_A, HASH_C]);
        assert_eq!(p.bytes(), 100);
        assert_eq!(p.skipped.len(), 1);
        assert_eq!(p.skipped[0].candidate.info_hash, HASH_B);
        assert!(p.skipped[0].reason.contains("exceed the seeding cap"));

        // Whatever the cap, the plan never adds up to more than it.
        for cap in [0, 1, 39, 40, 59, 60, 99, 100, 149, 150, u64::MAX] {
            let p = plan(c.clone(), cap);
            assert!(p.bytes() <= cap, "cap {cap}: planned {}", p.bytes());
            assert_eq!(p.seed.len() + p.skipped.len(), 3);
        }
        assert!(plan(c.clone(), 0).seed.is_empty());
        assert_eq!(plan(c, 150).seed.len(), 3);
        // Sizes that overflow when added are skipped, not wrapped.
        let (big, _) = candidates(
            &[entry(HASH_A, u64::MAX), entry(HASH_B, 2)],
            &Narrow::default(),
            vec![],
        );
        let p = plan(big, u64::MAX);
        assert_eq!(hashes(&p.seed), [HASH_A]);
    }

    #[test]
    fn the_operator_can_narrow_the_list_and_add_local_torrents() {
        let published = [entry(HASH_A, 10), entry(HASH_B, 10)];
        let (c, s) = candidates(&published, &Narrow::default(), vec![local(HASH_C, 5)]);
        assert_eq!(hashes(&c), [HASH_A, HASH_B, HASH_C]);
        assert!(s.is_empty());

        let only = Narrow {
            only: vec![HASH_B.to_uppercase()],
            skip: vec![],
        };
        let (c, s) = candidates(&published, &only, vec![local(HASH_C, 5)]);
        // `only` narrows the published list; the operator's own stay.
        assert_eq!(hashes(&c), [HASH_B, HASH_C]);
        assert_eq!(s[0].reason, "not in [seeding] only");

        let skip = Narrow {
            only: vec![],
            skip: vec![HASH_A.into()],
        };
        let (c, _) = candidates(&published, &skip, vec![]);
        assert_eq!(hashes(&c), [HASH_B]);

        // `only` cannot add a torrent that is not published.
        let stranger = Narrow {
            only: vec![HASH_C.into()],
            skip: vec![],
        };
        let (c, _) = candidates(&published, &stranger, vec![]);
        assert!(c.is_empty());
        // The same hash listed locally too is one torrent, the published one.
        let (c, _) = candidates(&published, &Narrow::default(), vec![local(HASH_A, 10)]);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].origin.label(), "published");
    }

    #[test]
    fn with_no_published_list_only_local_torrents_are_candidates() {
        let (c, s) = candidates(&[], &Narrow::default(), vec![]);
        assert!(c.is_empty() && s.is_empty());
        let (c, _) = candidates(&[], &Narrow::default(), vec![local(HASH_C, 5)]);
        assert_eq!(hashes(&c), [HASH_C]);
    }

    #[test]
    fn the_cap_follows_the_disk_floor() {
        let g = 1u64 << 30;
        // Plenty of disk: the configured cap.
        assert_eq!(effective_cap(10 * g, 0, 500 * g, 1000 * g, 50 * g), 10 * g);
        // 52 GiB free over a 50 GiB floor: only 2 GiB of room.
        assert_eq!(effective_cap(10 * g, 0, 52 * g, 1000 * g, 50 * g), 2 * g);
        // What seeding already holds stays counted as its own.
        assert_eq!(effective_cap(10 * g, 6 * g, 52 * g, 1000 * g, 50 * g), 8 * g);
        // Under the floor: nothing new, but what is held is not over the cap.
        assert_eq!(effective_cap(10 * g, 3 * g, 40 * g, 1000 * g, 50 * g), 3 * g);
        // Never above the configured cap.
        assert_eq!(effective_cap(10 * g, 9 * g, 500 * g, 1000 * g, 50 * g), 10 * g);
        // Unknown disk: the configured cap.
        assert_eq!(effective_cap(10 * g, 0, 0, 0, 50 * g), 10 * g);
    }
}
