//! Private mode: run a node only for yourself.
//!
//! Not everyone wants to sell their machine to the open market. A private
//! node keeps every workload it already runs, but:
//!
//!   * it only bids on offers published by buyers it trusts: every libp2p
//!     identity that already exists on this machine (so `c0mpute chat`,
//!     `infernet text`, `webhook send` from the same box just work) plus
//!     the peer-ids listed in `[private] buyers` (your other machines);
//!   * the buyer is the gossip *publisher*, which gossipsub signs (Signed +
//!     Strict), so a stranger cannot claim one of your peer-ids in the
//!     offer body and get past the list;
//!   * it stops publishing its capability ad, so it is not listed on the
//!     public status page and buyers outside the list never learn it exists.
//!
//! ```toml
//! [private]
//! enabled = true
//! buyers = ["12D3KooW...laptop", "12D3KooW...desktop"]
//! ```
//!
//! `C0MPUTE_PRIVATE=1` (or `worker start --private`) turns it on without
//! editing the file. The policy is process-wide: the CLI installs it once at
//! startup and every dispatch loop (transcode, speech, text, temporal,
//! webhook) consults it before bidding.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use c0mpute_net::GossipMessage;
use c0mpute_net::topics::JobOffer;
use serde::{Deserialize, Serialize};

/// `[private]` in config.toml.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivateConfig {
    /// Serve only trusted buyers and stay off the public listing.
    #[serde(default)]
    pub enabled: bool,
    /// Extra buyer peer-ids allowed to hire this node (your other machines).
    /// Identities on this machine are always allowed.
    #[serde(default)]
    pub buyers: Vec<String>,
}

impl PrivateConfig {
    /// Add a buyer peer-id; returns false when it was already listed.
    pub fn allow(&mut self, peer_id: &str) -> bool {
        let peer_id = peer_id.trim();
        if self.buyers.iter().any(|b| b == peer_id) {
            return false;
        }
        self.buyers.push(peer_id.to_string());
        true
    }

    /// Remove a buyer peer-id; returns false when it was not listed.
    pub fn deny(&mut self, peer_id: &str) -> bool {
        let before = self.buyers.len();
        self.buyers.retain(|b| b != peer_id.trim());
        self.buyers.len() != before
    }
}

/// Is `s` a well-formed libp2p peer-id?
pub fn valid_peer_id(s: &str) -> bool {
    s.trim().parse::<c0mpute_net::PeerId>().is_ok()
}

/// The resolved policy a dispatch loop checks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PrivatePolicy {
    pub enabled: bool,
    /// Trusted buyer peer-ids: configured buyers + local identities.
    pub trusted: BTreeSet<String>,
}

impl PrivatePolicy {
    /// Public: bid on anything we are capable of.
    pub fn public() -> Self {
        Self::default()
    }

    /// Build from config plus the identities found on this machine.
    pub fn from_config(cfg: &PrivateConfig, local: impl IntoIterator<Item = String>) -> Self {
        let mut trusted: BTreeSet<String> = cfg
            .buyers
            .iter()
            .map(|b| b.trim().to_string())
            .filter(|b| !b.is_empty())
            .collect();
        trusted.extend(local);
        Self {
            enabled: cfg.enabled,
            trusted,
        }
    }

    /// May this worker (`our_peer_id`) bid on `offer`, delivered as `msg`?
    ///
    /// Public mode admits everything. Private mode requires the signed
    /// gossip publisher to be the offer's buyer AND that buyer to be
    /// trusted (or this very node).
    pub fn admits(&self, msg: &GossipMessage, offer: &JobOffer, our_peer_id: &str) -> bool {
        if !self.enabled {
            return true;
        }
        if !crate::buyer::source_matches_claim(msg, &offer.buyer_peer_id) {
            return false;
        }
        offer.buyer_peer_id == our_peer_id || self.trusted.contains(&offer.buyer_peer_id)
    }

    /// Whether to publish the public capability ad.
    pub fn advertises(&self) -> bool {
        !self.enabled
    }
}

static POLICY: RwLock<Option<PrivatePolicy>> = RwLock::new(None);

/// Install the process-wide policy (once at startup; later calls replace it).
pub fn install(policy: PrivatePolicy) {
    if let Ok(mut p) = POLICY.write() {
        *p = Some(policy);
    }
}

/// The installed policy, or public when none was installed.
pub fn current() -> PrivatePolicy {
    POLICY
        .read()
        .ok()
        .and_then(|p| p.clone())
        .unwrap_or_default()
}

/// `C0MPUTE_PRIVATE` truthy?
pub fn env_forces_private() -> bool {
    std::env::var("C0MPUTE_PRIVATE")
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

/// Every libp2p identity stored on this machine: `<config_dir>/identity.key`
/// (the worker) and `<data_dir>/*/identity.key` (buyer/serve identities).
pub fn local_identities() -> BTreeSet<String> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(c) = crate::config::config_dir() {
        dirs.push(c);
    }
    if let Some(d) = crate::config::data_dir() {
        dirs.push(d.clone());
        dirs.extend(subdirs(&d));
    }
    identities_in(&dirs)
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default()
}

/// Peer-ids of the identity keys stored directly in `dirs`.
pub fn identities_in(dirs: &[PathBuf]) -> BTreeSet<String> {
    dirs.iter()
        .filter_map(|d| c0mpute_net::identity::existing_peer_id(d))
        .map(|p| p.to_base58())
        .collect()
}

/// Resolve and install the policy from config (+ env). Returns it so the
/// caller can log what it is running as.
pub fn install_from(cfg: &PrivateConfig) -> PrivatePolicy {
    let mut cfg = cfg.clone();
    if env_forces_private() {
        cfg.enabled = true;
    }
    let policy = if cfg.enabled {
        PrivatePolicy::from_config(&cfg, local_identities())
    } else {
        PrivatePolicy::public()
    };
    install(policy.clone());
    policy
}

#[cfg(test)]
mod tests {
    use super::*;
    use c0mpute_net::PeerId;

    fn offer(buyer: &str) -> JobOffer {
        serde_json::from_value(serde_json::json!({
            "job_id": "j1",
            "workload_type": "tts.synthesize",
            "buyer_peer_id": buyer,
            "buyer_did": null,
            "spec_hash": "h",
            "required_capabilities": [],
            "max_price_usd": 0.01,
            "deadline_unix_ms": u64::MAX,
            "published_at_ms": 0
        }))
        .unwrap()
    }

    fn msg(from: Option<PeerId>) -> GossipMessage {
        GossipMessage {
            topic: "t".into(),
            source: from,
            data: vec![],
        }
    }

    #[test]
    fn public_mode_admits_everyone() {
        let stranger = PeerId::random();
        let p = PrivatePolicy::public();
        assert!(p.admits(&msg(Some(stranger)), &offer(&stranger.to_base58()), "me"));
        assert!(p.advertises());
    }

    #[test]
    fn private_mode_admits_only_trusted_buyers() {
        let mine = PeerId::random();
        let stranger = PeerId::random();
        let cfg = PrivateConfig {
            enabled: true,
            buyers: vec![mine.to_base58()],
        };
        let p = PrivatePolicy::from_config(&cfg, []);
        assert!(p.admits(&msg(Some(mine)), &offer(&mine.to_base58()), "worker"));
        assert!(!p.admits(
            &msg(Some(stranger)),
            &offer(&stranger.to_base58()),
            "worker"
        ));
        assert!(!p.advertises());
    }

    #[test]
    fn a_stranger_cannot_claim_a_trusted_buyer_id() {
        let mine = PeerId::random();
        let stranger = PeerId::random();
        let cfg = PrivateConfig {
            enabled: true,
            buyers: vec![mine.to_base58()],
        };
        let p = PrivatePolicy::from_config(&cfg, []);
        // Offer body names my peer-id, but the signed publisher is the stranger.
        assert!(!p.admits(&msg(Some(stranger)), &offer(&mine.to_base58()), "worker"));
        // Unsigned / anonymous messages never pass private mode.
        assert!(!p.admits(&msg(None), &offer(&mine.to_base58()), "worker"));
    }

    #[test]
    fn own_and_local_identities_are_trusted() {
        let me = PeerId::random();
        let local_buyer = PeerId::random();
        let cfg = PrivateConfig {
            enabled: true,
            buyers: vec![],
        };
        let p = PrivatePolicy::from_config(&cfg, [local_buyer.to_base58()]);
        assert!(p.admits(&msg(Some(me)), &offer(&me.to_base58()), &me.to_base58()));
        assert!(p.admits(
            &msg(Some(local_buyer)),
            &offer(&local_buyer.to_base58()),
            "worker"
        ));
    }

    #[test]
    fn identities_are_read_from_key_files_without_creating_any() {
        let dir = tempfile::tempdir().unwrap();
        let with_key = dir.path().join("buyer");
        let empty = dir.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let kp = c0mpute_net::identity::load_or_create(&with_key).unwrap();
        let found = identities_in(&[with_key.clone(), empty.clone()]);
        assert_eq!(
            found,
            [kp.public().to_peer_id().to_base58()].into_iter().collect()
        );
        assert!(!empty.join("identity.key").exists());
    }

    #[test]
    fn allow_and_deny_edit_the_list() {
        let mut c = PrivateConfig::default();
        assert!(c.allow(" a "));
        assert!(!c.allow("a"));
        assert!(c.deny("a"));
        assert!(!c.deny("a"));
        assert!(valid_peer_id(&PeerId::random().to_base58()));
        assert!(!valid_peer_id("not-a-peer"));
    }

    #[test]
    fn config_section_is_optional_and_parses() {
        let c: crate::config::Config = toml::from_str("").unwrap();
        assert_eq!(c.private, PrivateConfig::default());
        let c: crate::config::Config =
            toml::from_str("[private]\nenabled = true\nbuyers = [\"x\"]\n").unwrap();
        assert!(c.private.enabled);
        assert_eq!(c.private.buyers, vec!["x".to_string()]);
    }
}
