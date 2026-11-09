//! Who holds an app and who runs it, decided the same way on every node from
//! the gossiped view alone. No leader, no coordinator, no vote.
//!
//! * **Holders.** Each app wants `replicas` holders. Nodes that already hold
//!   it keep it; the shortfall is filled by the highest-ranked live apps
//!   nodes with room (rendezvous hashing on app id + peer id). Above
//!   `replicas`, the lowest-ranked standby lets go.
//! * **Primary.** Exactly one holder runs the container. A live holder that
//!   claims primary keeps it (sticky: a new node joining never moves a running
//!   app). If two claim (a healed partition), the higher `(epoch, rank)` wins
//!   and the other stops. With no live claimant, the ready holder with the
//!   newest snapshot is elected at `epoch + 1`; the epoch fences the old
//!   primary should it come back.

use std::collections::HashMap;

use c0mpute_proto::Hash;
use serde::{Deserialize, Serialize};

/// A holder's role for one app.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppRole {
    Primary,
    Standby,
}

/// A snapshot of an app's state, ordered by `(epoch, n)`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapRef {
    /// blake3 (hex) of the snapshot manifest chunk.
    pub manifest: String,
    pub epoch: u64,
    /// Counter within the epoch.
    pub n: u64,
    /// Unix seconds.
    pub taken_at: u64,
}

impl SnapRef {
    pub fn order(&self) -> (u64, u64) {
        (self.epoch, self.n)
    }
}

/// What a node says about one app it holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldApp {
    pub app_id: String,
    pub seq: u64,
    pub role: AppRole,
    /// The primary epoch this holder last served under (0 = never ran).
    pub epoch: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<SnapRef>,
    /// Primary: the last health probe passed.
    #[serde(default)]
    pub healthy: bool,
    /// Holds the opened secrets (or the app needs none).
    #[serde(default)]
    pub has_secrets: bool,
    /// The image is loaded: it could start now.
    #[serde(default)]
    pub ready: bool,
}

/// One live apps node, as last heard.
#[derive(Clone, Debug, Default)]
pub struct NodeView {
    pub free_mb: u32,
    /// Its long-lived X25519 key (hex), for sealing secrets to it.
    pub x25519: String,
    pub apps: HashMap<String, HeldApp>,
    /// Takes new apps (false for an owner's CLI or a full node).
    pub hosts: bool,
    /// Owner keys (hex) whose apps it runs; empty = anyone's.
    pub owners: Vec<String>,
}

impl NodeView {
    /// Would this node run an app signed by `owner`?
    pub fn accepts(&self, owner: &str) -> bool {
        self.owners.is_empty() || self.owners.iter().any(|o| o == owner)
    }
}

/// Rendezvous score: higher ranks first.
pub fn score(app_id: &str, peer: &str) -> [u8; 32] {
    Hash::of(format!("c0mpute-apps-rank\n{app_id}\n{peer}").as_bytes()).0
}

fn by_rank<'a>(app_id: &str, peers: impl Iterator<Item = &'a String>) -> Vec<String> {
    let mut v: Vec<(&String, [u8; 32])> = peers.map(|p| (p, score(app_id, p))).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    v.into_iter().map(|(p, _)| p.clone()).collect()
}

/// Live nodes that hold `app_id`, best-ranked first.
pub fn holders(app_id: &str, nodes: &HashMap<String, NodeView>) -> Vec<String> {
    by_rank(
        app_id,
        nodes
            .iter()
            .filter(|(_, n)| n.apps.contains_key(app_id))
            .map(|(p, _)| p),
    )
}

/// Nodes that should take a copy now (empty when there are enough).
pub fn fill(
    app_id: &str,
    owner: &str,
    replicas: usize,
    memory_mb: u32,
    nodes: &HashMap<String, NodeView>,
) -> Vec<String> {
    let have = holders(app_id, nodes).len();
    if have >= replicas {
        return Vec::new();
    }
    let candidates = nodes
        .iter()
        .filter(|(_, n)| n.hosts && n.accepts(owner) && !n.apps.contains_key(app_id) && n.free_mb >= memory_mb)
        .map(|(p, _)| p);
    by_rank(app_id, candidates)
        .into_iter()
        .take(replicas - have)
        .collect()
}

/// With more than `replicas` holders, the lowest-ranked standby that lets go.
pub fn surplus(app_id: &str, replicas: usize, nodes: &HashMap<String, NodeView>) -> Option<String> {
    let h = holders(app_id, nodes);
    if h.len() <= replicas {
        return None;
    }
    h.into_iter().rev().find(|p| {
        nodes
            .get(p)
            .and_then(|n| n.apps.get(app_id))
            .map(|a| a.role)
            == Some(AppRole::Standby)
    })
}

/// Who runs the app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Primary {
    /// A live holder claims it and wins any tie.
    Running { peer: String, epoch: u64 },
    /// Nobody claims it: `peer` should start at `epoch`.
    Elect { peer: String, epoch: u64 },
    /// Nobody can run it yet (no holder has the image and the secrets).
    None,
}

impl Primary {
    pub fn peer(&self) -> Option<&str> {
        match self {
            Primary::Running { peer, .. } | Primary::Elect { peer, .. } => Some(peer),
            Primary::None => None,
        }
    }
}

/// Decide the primary for `app_id` from the live view.
pub fn primary(app_id: &str, needs_secrets: bool, nodes: &HashMap<String, NodeView>) -> Primary {
    let held: Vec<(&String, &HeldApp)> = nodes
        .iter()
        .filter_map(|(p, n)| n.apps.get(app_id).map(|a| (p, a)))
        .collect();
    let claim = held
        .iter()
        .filter(|(_, a)| a.role == AppRole::Primary)
        .max_by(|a, b| {
            a.1.epoch
                .cmp(&b.1.epoch)
                .then(score(app_id, a.0).cmp(&score(app_id, b.0)))
        });
    if let Some((p, a)) = claim {
        return Primary::Running {
            peer: (*p).clone(),
            epoch: a.epoch,
        };
    }
    let top_epoch = held
        .iter()
        .map(|(_, a)| {
            a.epoch
                .max(a.snapshot.as_ref().map(|s| s.epoch).unwrap_or(0))
        })
        .max()
        .unwrap_or(0);
    let best = held
        .iter()
        .filter(|(_, a)| a.ready && (a.has_secrets || !needs_secrets))
        .max_by(|a, b| {
            let sa = a.1.snapshot.as_ref().map(|s| s.order()).unwrap_or((0, 0));
            let sb = b.1.snapshot.as_ref().map(|s| s.order()).unwrap_or((0, 0));
            sa.cmp(&sb)
                .then(score(app_id, a.0).cmp(&score(app_id, b.0)))
        });
    match best {
        Some((p, _)) => Primary::Elect {
            peer: (*p).clone(),
            epoch: top_epoch + 1,
        },
        None => Primary::None,
    }
}

/// The newest snapshot any live holder has, and who has it.
pub fn newest_snapshot(
    app_id: &str,
    nodes: &HashMap<String, NodeView>,
) -> Option<(String, SnapRef)> {
    nodes
        .iter()
        .filter_map(|(p, n)| {
            n.apps
                .get(app_id)
                .and_then(|a| a.snapshot.clone())
                .map(|s| (p.clone(), s))
        })
        .max_by(|a, b| a.1.order().cmp(&b.1.order()).then(b.0.cmp(&a.0)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(free: u32) -> NodeView {
        NodeView {
            free_mb: free,
            x25519: "00".repeat(32),
            apps: HashMap::new(),
            hosts: true,
            owners: vec![],
        }
    }

    fn held(
        app: &str,
        role: AppRole,
        epoch: u64,
        snap: Option<(u64, u64)>,
        ready: bool,
    ) -> HeldApp {
        HeldApp {
            app_id: app.into(),
            seq: 1,
            role,
            epoch,
            snapshot: snap.map(|(e, n)| SnapRef {
                manifest: format!("{e}-{n}"),
                epoch: e,
                n,
                taken_at: 0,
            }),
            healthy: true,
            has_secrets: true,
            ready,
        }
    }

    fn view(n: usize) -> HashMap<String, NodeView> {
        (0..n).map(|i| (format!("peer{i}"), node(4096))).collect()
    }

    #[test]
    fn fill_is_deterministic_and_stops_at_replicas() {
        let nodes = view(8);
        let a = fill("app1", "o", 3, 256, &nodes);
        let b = fill("app1", "o", 3, 256, &nodes);
        assert_eq!(a.len(), 3);
        assert_eq!(a, b, "every node computes the same holders");
        let mut nodes = nodes;
        for p in &a {
            nodes
                .get_mut(p)
                .unwrap()
                .apps
                .insert("app1".into(), held("app1", AppRole::Standby, 0, None, true));
        }
        assert!(fill("app1", "o", 3, 256, &nodes).is_empty());
    }

    #[test]
    fn fill_skips_full_and_non_hosting_nodes() {
        let mut nodes = view(4);
        nodes.get_mut("peer0").unwrap().free_mb = 100;
        nodes.get_mut("peer1").unwrap().hosts = false;
        let f = fill("app1", "o", 3, 256, &nodes);
        assert!(!f.contains(&"peer0".to_string()) && !f.contains(&"peer1".to_string()));
        assert_eq!(f.len(), 2, "only two nodes have room");
    }

    #[test]
    fn fill_respects_owner_allowlists() {
        let mut nodes = view(4);
        for p in ["peer0", "peer1"] {
            nodes.get_mut(p).unwrap().owners = vec!["alice".into()];
        }
        let f = fill("app1", "bob", 3, 256, &nodes);
        assert_eq!(f.len(), 2, "nodes that only run alice's apps never take bob's");
        assert!(f.iter().all(|p| p == "peer2" || p == "peer3"));
        assert_eq!(fill("app1", "alice", 3, 256, &nodes).len(), 3);
    }

    #[test]
    fn primary_is_sticky_then_fenced_by_epoch() {
        let mut nodes = view(3);
        nodes
            .get_mut("peer0")
            .unwrap()
            .apps
            .insert("a".into(), held("a", AppRole::Standby, 0, None, true));
        nodes.get_mut("peer1").unwrap().apps.insert(
            "a".into(),
            held("a", AppRole::Primary, 2, Some((2, 7)), true),
        );
        nodes.get_mut("peer2").unwrap().apps.insert(
            "a".into(),
            held("a", AppRole::Standby, 0, Some((2, 6)), true),
        );
        assert_eq!(
            primary("a", true, &nodes),
            Primary::Running {
                peer: "peer1".into(),
                epoch: 2
            }
        );
        // Two claimants after a partition heals: the higher epoch wins.
        nodes.get_mut("peer0").unwrap().apps.insert(
            "a".into(),
            held("a", AppRole::Primary, 3, Some((3, 1)), true),
        );
        assert_eq!(
            primary("a", true, &nodes),
            Primary::Running {
                peer: "peer0".into(),
                epoch: 3
            }
        );
    }

    #[test]
    fn failover_elects_the_newest_snapshot_at_a_higher_epoch() {
        let mut nodes = view(3);
        // peer1 (the old primary at epoch 4) is gone from the view.
        nodes.remove("peer1");
        nodes.get_mut("peer0").unwrap().apps.insert(
            "a".into(),
            held("a", AppRole::Standby, 0, Some((4, 9)), true),
        );
        nodes.get_mut("peer2").unwrap().apps.insert(
            "a".into(),
            held("a", AppRole::Standby, 0, Some((4, 8)), true),
        );
        assert_eq!(
            primary("a", true, &nodes),
            Primary::Elect {
                peer: "peer0".into(),
                epoch: 5
            }
        );
        // A holder whose image is not loaded is never elected.
        nodes
            .get_mut("peer0")
            .unwrap()
            .apps
            .get_mut("a")
            .unwrap()
            .ready = false;
        assert_eq!(
            primary("a", true, &nodes),
            Primary::Elect {
                peer: "peer2".into(),
                epoch: 5
            }
        );
        // Nor one without the secrets, when the app needs them.
        nodes
            .get_mut("peer2")
            .unwrap()
            .apps
            .get_mut("a")
            .unwrap()
            .has_secrets = false;
        assert_eq!(primary("a", true, &nodes), Primary::None);
        assert!(matches!(primary("a", false, &nodes), Primary::Elect { .. }));
    }

    #[test]
    fn surplus_never_drops_the_primary() {
        let mut nodes = view(4);
        for (i, p) in ["peer0", "peer1", "peer2", "peer3"].iter().enumerate() {
            let role = if i == 0 {
                AppRole::Primary
            } else {
                AppRole::Standby
            };
            nodes
                .get_mut(*p)
                .unwrap()
                .apps
                .insert("a".into(), held("a", role, 1, None, true));
        }
        let s = surplus("a", 3, &nodes).unwrap();
        assert_ne!(s, "peer0");
        assert_eq!(
            s,
            *holders("a", &nodes)
                .iter()
                .rev()
                .find(|p| *p != "peer0")
                .unwrap()
        );
        assert!(surplus("a", 4, &nodes).is_none());
    }
}
