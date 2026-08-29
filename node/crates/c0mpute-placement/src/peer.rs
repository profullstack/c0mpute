//! Storage peers and the failure domains they belong to (CIP-003).

use serde::{Deserialize, Serialize};

/// The unit of correlated failure.
///
/// CIP-001's durability arithmetic treats shard hosts as independent samples.
/// Two peers in the same autonomous system share an operator, a transit
/// provider and often a building, so they are one sample wearing two hats.
/// Grouping by domain is what keeps the arithmetic honest.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FailureDomain {
    /// Best signal: the peer's autonomous system.
    Asn(u32),
    /// Fallback when the ASN is unknown. Weaker — two ASNs can share a
    /// prefix's neighbourhood and one ASN can span many prefixes — but it is
    /// never *wrong*, only coarse.
    IpPrefix(String),
    /// Nothing is known about where this peer sits.
    Unknown,
}

/// A peer that might hold shards.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PeerInfo {
    pub peer_id: String,
    /// Base URL of the peer's storage API, e.g. `http://1.2.3.4:7780`.
    pub endpoint: String,
    /// From `c0mpute_verify::reputation`.
    pub reputation: f32,
    /// Fraction of the last 30 days the peer was reachable.
    pub uptime_30d: f32,
    /// Bytes the peer will still accept.
    pub free_bytes: u64,
    pub rtt_ms: u32,
    pub asn: Option<u32>,
    pub region: Option<String>,
    /// Network prefix (e.g. `"203.0.113"`), used when `asn` is unknown.
    pub ip_prefix: Option<String>,
}

impl PeerInfo {
    /// Which failure domain this peer counts against.
    ///
    /// ASN first, IP prefix second, `Unknown` last. Region is deliberately not
    /// part of the identity: it is far coarser than an ASN, so folding it in
    /// would let two peers in one datacenter look like two domains simply
    /// because their operators labelled them differently.
    pub fn domain(&self) -> FailureDomain {
        match (self.asn, &self.ip_prefix) {
            (Some(asn), _) => FailureDomain::Asn(asn),
            (None, Some(prefix)) => FailureDomain::IpPrefix(prefix.clone()),
            (None, None) => FailureDomain::Unknown,
        }
    }

    /// Derive an IP prefix from a hostname or `host:port`, when it is a
    /// literal IPv4 address. A DNS name tells us nothing without resolving it,
    /// and resolving here would make selection do network I/O.
    pub fn prefix_from_endpoint(endpoint: &str) -> Option<String> {
        let after_scheme = match endpoint.split_once("://") {
            Some((_, rest)) => rest,
            None => endpoint,
        };
        let host_port = after_scheme.split('/').next()?;
        // Only strip a trailing `:port`, never an IPv6 colon.
        let host = match host_port.rsplit_once(':') {
            Some((h, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => h,
            _ => host_port,
        };
        let host = host.trim_start_matches('[').trim_end_matches(']');

        let octets: Vec<&str> = host.split('.').collect();
        if octets.len() == 4 && octets.iter().copied().all(|o| o.parse::<u8>().is_ok()) {
            // /24-equivalent. Coarse on purpose: the point is to catch "these
            // are obviously the same rack", not to model routing.
            return Some(format!("{}.{}.{}", octets[0], octets[1], octets[2]));
        }
        None
    }
}

/// What this node currently believes about its storage peers.
///
/// Populated from gossipsub capability ads today; CIP-006's challenge results
/// will feed `reputation` and `uptime_30d` once those exist. Deliberately a
/// plain snapshot — selection must not do network I/O, or a slow peer lookup
/// becomes a slow write.
#[derive(Clone, Debug, Default)]
pub struct PeerCatalog {
    peers: Vec<PeerInfo>,
}

impl PeerCatalog {
    pub fn new(peers: Vec<PeerInfo>) -> Self {
        Self { peers }
    }

    pub fn peers(&self) -> &[PeerInfo] {
        &self.peers
    }

    pub fn len(&self) -> usize {
        self.peers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    pub fn upsert(&mut self, peer: PeerInfo) {
        match self.peers.iter_mut().find(|p| p.peer_id == peer.peer_id) {
            Some(existing) => *existing = peer,
            None => self.peers.push(peer),
        }
    }

    pub fn remove(&mut self, peer_id: &str) {
        self.peers.retain(|p| p.peer_id != peer_id);
    }

    pub fn get(&self, peer_id: &str) -> Option<&PeerInfo> {
        self.peers.iter().find(|p| p.peer_id == peer_id)
    }

    /// How many distinct failure domains the catalog spans. The headline
    /// number for whether this network can store anything durably at all.
    pub fn domain_count(&self) -> usize {
        self.peers
            .iter()
            .map(|p| p.domain())
            .collect::<std::collections::HashSet<_>>()
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(id: &str) -> PeerInfo {
        PeerInfo {
            peer_id: id.into(),
            endpoint: "http://10.0.0.1:7780".into(),
            reputation: 0.95,
            uptime_30d: 0.995,
            free_bytes: 1 << 30,
            rtt_ms: 20,
            asn: None,
            region: None,
            ip_prefix: None,
        }
    }

    #[test]
    fn domain_prefers_asn_then_prefix_then_unknown() {
        let mut p = peer("a");
        assert_eq!(p.domain(), FailureDomain::Unknown);

        p.ip_prefix = Some("203.0.113".into());
        assert_eq!(p.domain(), FailureDomain::IpPrefix("203.0.113".into()));

        p.asn = Some(64512);
        assert_eq!(p.domain(), FailureDomain::Asn(64512), "ASN should win");
    }

    /// Region is intentionally not part of domain identity — it is coarser
    /// than an ASN and would make one datacenter look like several domains.
    #[test]
    fn region_does_not_affect_domain_identity() {
        let mut a = peer("a");
        let mut b = peer("b");
        a.asn = Some(7);
        b.asn = Some(7);
        a.region = Some("us-east".into());
        b.region = Some("eu-west".into());
        assert_eq!(a.domain(), b.domain());
    }

    #[test]
    fn prefix_from_endpoint_handles_the_shapes_we_see() {
        assert_eq!(
            PeerInfo::prefix_from_endpoint("http://203.0.113.42:7780"),
            Some("203.0.113".into())
        );
        assert_eq!(
            PeerInfo::prefix_from_endpoint("https://198.51.100.7/storage"),
            Some("198.51.100".into())
        );
        // A DNS name tells us nothing without resolving, and selection must
        // not do network I/O.
        assert_eq!(
            PeerInfo::prefix_from_endpoint("http://node.example.com:7780"),
            None
        );
        assert_eq!(PeerInfo::prefix_from_endpoint("http://999.1.1.1"), None);
    }

    #[test]
    fn catalog_upsert_replaces_rather_than_duplicating() {
        let mut c = PeerCatalog::default();
        c.upsert(peer("a"));
        c.upsert(peer("a"));
        assert_eq!(c.len(), 1);

        let mut updated = peer("a");
        updated.free_bytes = 42;
        c.upsert(updated);
        assert_eq!(c.len(), 1);
        assert_eq!(c.get("a").unwrap().free_bytes, 42);
    }

    #[test]
    fn domain_count_is_the_networks_real_capacity_for_durability() {
        let mut c = PeerCatalog::default();
        for i in 0..10 {
            let mut p = peer(&format!("p{i}"));
            p.asn = Some(if i < 6 { 100 } else { 200 });
            c.upsert(p);
        }
        assert_eq!(c.len(), 10);
        assert_eq!(c.domain_count(), 2, "ten peers, two places they can fail");
    }
}
