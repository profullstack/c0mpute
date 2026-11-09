//! Which mailbox nodes hold a key: rendezvous (highest random weight)
//! hashing, the same scheme hosting uses for site replicas. Sender and
//! recipient compute the same ranking from the same peer set with no
//! coordination; when the set changes, only the keys whose top nodes changed
//! move.

use sha2::{Digest, Sha256};

/// Mailboxes a sender stores each message on.
pub const STORE_REPLICAS: usize = 3;
/// Mailboxes a recipient polls per tag: the store set plus two, so a pull
/// still finds messages when the peer set churned a little since the send.
pub const PULL_REPLICAS: usize = STORE_REPLICAS + 2;

fn score(key: &str, peer: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"c0mpute-dm-rendezvous-v1|");
    h.update(key.as_bytes());
    h.update(b"|");
    h.update(peer.as_bytes());
    h.finalize().into()
}

/// All `peers`, best-ranked first, for `key`. Duplicates are removed.
pub fn rank<S: AsRef<str>>(key: &str, peers: &[S]) -> Vec<String> {
    let mut v: Vec<(String, [u8; 32])> = peers.iter().map(|p| (p.as_ref().to_string(), score(key, p.as_ref()))).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v.dedup_by(|a, b| a.0 == b.0);
    v.into_iter().map(|(p, _)| p).collect()
}

/// The top `n` of [`rank`].
pub fn place<S: AsRef<str>>(key: &str, peers: &[S], n: usize) -> Vec<String> {
    let mut r = rank(key, peers);
    r.truncate(n);
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peers(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("12D3KooPeer{i:03}")).collect()
    }

    #[test]
    fn placement_is_deterministic_and_order_independent() {
        let p = peers(20);
        let mut shuffled = p.clone();
        shuffled.reverse();
        shuffled.rotate_left(7);
        let a = place("tag-1", &p, STORE_REPLICAS);
        let b = place("tag-1", &shuffled, STORE_REPLICAS);
        assert_eq!(a, b);
        assert_eq!(a.len(), 3);
        // Different keys land on different sets (overwhelmingly).
        let spread: std::collections::HashSet<Vec<String>> =
            (0..50).map(|i| place(&format!("tag-{i}"), &p, 3)).collect();
        assert!(spread.len() > 40);
        // Fewer peers than replicas: all of them.
        assert_eq!(place("k", &peers(2), 3).len(), 2);
        // Duplicates collapse.
        let dup = vec!["a".to_string(), "a".to_string(), "b".to_string()];
        assert_eq!(rank("k", &dup).len(), 2);
    }

    #[test]
    fn churn_moves_only_affected_keys_and_pull_set_covers_store_set() {
        let p = peers(20);
        let mut grown = p.clone();
        grown.push("12D3KooNewcomer".into());
        let mut moved = 0;
        for i in 0..200 {
            let key = format!("tag-{i}");
            let before = place(&key, &p, STORE_REPLICAS);
            let after_pull = place(&key, &grown, PULL_REPLICAS);
            // A message stored before the newcomer joined is still found by
            // a pull after: at most one slot shifts.
            assert!(before.iter().all(|x| after_pull.contains(x)), "{key}");
            if place(&key, &grown, STORE_REPLICAS) != before {
                moved += 1;
            }
        }
        // Roughly 3/21 of keys should be affected.
        assert!(moved < 60, "{moved} keys moved");
    }
}
