//! The node's storage-peer registry (CIP-003).
//!
//! Peers are read from `<storage-root>/peers.json`. Gossipsub capability ads
//! will populate this automatically once the storage role advertises itself;
//! until then an operator adds peers explicitly, which is also what makes a
//! deliberate small testnet possible.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use c0mpute_placement::{PeerCatalog, PeerInfo};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PeerFile {
    #[serde(default)]
    pub peers: Vec<PeerInfo>,
}

pub fn peers_path(storage_root: &Path) -> PathBuf {
    storage_root.join("peers.json")
}

pub fn load(storage_root: &Path) -> Result<PeerCatalog> {
    let path = peers_path(storage_root);
    if !path.exists() {
        return Ok(PeerCatalog::default());
    }
    let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let file: PeerFile =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    Ok(PeerCatalog::new(file.peers))
}

pub fn save(storage_root: &Path, catalog: &PeerCatalog) -> Result<()> {
    let path = peers_path(storage_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = PeerFile {
        peers: catalog.peers().to_vec(),
    };
    let json = serde_json::to_vec_pretty(&file)?;
    // Write-then-rename: a half-written peer list would be parsed as a
    // smaller network on the next read, and placement decisions follow from
    // exactly that number.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Build a peer record, filling in what can be inferred.
///
/// A peer with no ASN and a DNS endpoint has an `Unknown` failure domain and
/// is excluded from placement by default — deliberately, since CIP-001's
/// durability figures assume independent hosts. `prefix_from_endpoint`
/// recovers a weak-but-real domain when the endpoint is a literal IP.
pub fn build(
    peer_id: String,
    endpoint: String,
    asn: Option<u32>,
    region: Option<String>,
) -> PeerInfo {
    let ip_prefix = PeerInfo::prefix_from_endpoint(&endpoint);
    PeerInfo {
        peer_id,
        endpoint,
        // Until CIP-006's challenges produce real numbers, a manually added
        // peer is taken at its word. Recorded here rather than hidden so the
        // assumption is visible when reputation starts being measured.
        reputation: 1.0,
        uptime_30d: 1.0,
        free_bytes: u64::MAX,
        rtt_ms: 50,
        asn,
        region,
        ip_prefix,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "c0mpute-peers-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn missing_file_is_an_empty_catalog_not_an_error() {
        let c = load(&tmpdir()).unwrap();
        assert!(c.is_empty());
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tmpdir();
        let mut c = PeerCatalog::default();
        c.upsert(build(
            "a".into(),
            "http://10.0.0.1:7780".into(),
            Some(7),
            None,
        ));
        c.upsert(build(
            "b".into(),
            "http://10.1.0.1:7780".into(),
            Some(8),
            None,
        ));
        save(&dir, &c).unwrap();

        let back = load(&dir).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back.get("a").unwrap().asn, Some(7));
        assert_eq!(back.domain_count(), 2);
    }

    #[test]
    fn infers_an_ip_prefix_when_the_asn_is_unknown() {
        let p = build("a".into(), "http://203.0.113.9:7780".into(), None, None);
        assert_eq!(p.ip_prefix.as_deref(), Some("203.0.113"));
        assert_eq!(
            p.domain(),
            c0mpute_placement::FailureDomain::IpPrefix("203.0.113".into())
        );
    }

    #[test]
    fn a_dns_endpoint_without_an_asn_has_no_domain() {
        let p = build(
            "a".into(),
            "http://node.example.com:7780".into(),
            None,
            None,
        );
        assert_eq!(p.ip_prefix, None);
        assert_eq!(p.domain(), c0mpute_placement::FailureDomain::Unknown);
    }

    #[test]
    fn an_explicit_asn_wins_over_the_inferred_prefix() {
        let p = build(
            "a".into(),
            "http://203.0.113.9:7780".into(),
            Some(64512),
            None,
        );
        assert_eq!(p.domain(), c0mpute_placement::FailureDomain::Asn(64512));
    }
}
