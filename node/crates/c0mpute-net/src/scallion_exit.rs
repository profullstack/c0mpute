//! Scallion Network: explicit exit consent and destination checks.
//!
//! This is a policy primitive for a future, audited Scallion exit service.
//! It does NOT open sockets, resolve DNS, or act as a network proxy.
//! Callers must connect to an already validated IP, never resolve a second
//! time after approval. See docs/protocol/scallion-network.md.
//!
//! Exit capability is off by default: ordinary c0mpute workers must never
//! become unrestricted Internet exit proxies just because they serve jobs.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use anyhow::{Result, bail, ensure};

/// Deny-all unless an operator explicitly chooses permitted destination
/// ports. A service-level policy may further restrict destinations and
/// enforce applicable operator obligations.
#[derive(Clone, Debug)]
pub struct ExitPolicy {
    enabled: bool,
    allowed_ports: BTreeSet<u16>,
    max_resolved_ips: usize,
}

impl Default for ExitPolicy {
    fn default() -> Self {
        Self::disabled()
    }
}

impl ExitPolicy {
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            allowed_ports: BTreeSet::new(),
            max_resolved_ips: 32,
        }
    }

    /// Explicit opt-in. Ports must still be supplied, and port zero cannot
    /// be enabled. Production exits should additionally enforce per-client
    /// circuit caps, a consent UX, and abuse handling.
    pub fn opt_in(ports: impl IntoIterator<Item = u16>) -> Result<Self> {
        let ports: BTreeSet<_> = ports.into_iter().collect();
        ensure!(!ports.is_empty(), "exit requires at least one allowed port");
        ensure!(!ports.contains(&0), "port zero cannot be an exit destination");
        Ok(Self {
            enabled: true,
            allowed_ports: ports,
            max_resolved_ips: 32,
        })
    }

    /// Validate a requested hostname/port with EVERY DNS answer. If any
    /// answer is private or reserved, fail closed rather than selecting a
    /// convenient public one. Store the vetted IPs for a pinned connection,
    /// to prevent DNS rebinding on a later socket dial.
    ///
    /// The caller must provide the DNS results from an independent resolver
    /// with a bounded timeout. Never do a second name-based connect.
    pub fn approve(
        &self,
        host: &str,
        port: u16,
        resolved: &[IpAddr],
    ) -> Result<ApprovedDestination> {
        ensure!(self.enabled, "Scallion exit is not enabled by the operator");
        ensure!(self.allowed_ports.contains(&port), "exit destination port is not allowed");
        let host = normalize_host(host)?;

        ensure!(!resolved.is_empty(), "exit destination has no resolved addresses");
        ensure!(
            resolved.len() <= self.max_resolved_ips,
            "too many resolved exit addresses"
        );
        let literal = host.parse::<IpAddr>().ok();
        if let Some(ip) = literal {
            ensure!(
                resolved.iter().all(|candidate| *candidate == ip),
                "IP literal resolved to a different destination"
            );
        }
        for ip in resolved {
            ensure!(public_ip(*ip), "private or special-use exit address is blocked");
        }

        let mut ips = resolved.to_vec();
        ips.sort();
        ips.dedup();
        Ok(ApprovedDestination { host, port, ips })
    }
}

/// A destination that passed the operator's port and public-address
/// checks. Fields are private so downstream code cannot directly construct
/// a fake approval.
#[derive(Clone, Debug)]
pub struct ApprovedDestination {
    host: String,
    port: u16,
    ips: Vec<IpAddr>,
}

impl ApprovedDestination {
    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Only connect to one of these vetted addresses, at the vetted port.
    /// Keep the original hostname for TLS SNI / certificate verification.
    pub fn socket_addrs(&self) -> Vec<SocketAddr> {
        self.ips.iter().map(|ip| SocketAddr::new(*ip, self.port)).collect()
    }

    /// Call after connecting to the socket, verifying peer_addr() matches
    /// the approved IP/port. This rejects a silently redirected connection.
    pub fn check_connected(&self, peer: SocketAddr) -> Result<()> {
        ensure!(peer.port() == self.port && self.ips.contains(&peer.ip()), "exit connection left the approved destination set");
        Ok(())
    }
}

fn normalize_host(raw: &str) -> Result<String> {
    ensure!(!raw.is_empty() && raw.len() <= 253, "invalid hostname length");
    ensure!(raw == raw.trim(), "hostname must not contain surrounding whitespace");
    ensure!(!raw.starts_with('[') && !raw.ends_with(']'), "bracketed host literals are not supported");
    ensure!(!raw.chars().any(|c| c.is_control() || c == '/' || c == '\\' || c == '@' || c == '%' || c == ':'), "invalid hostname character");
    let name = raw.trim_end_matches('.').to_ascii_lowercase();
    ensure!(!name.is_empty() && !raw.ends_with(".."), "invalid hostname");

    if let Ok(ip) = name.parse::<IpAddr>() {
        ensure!(public_ip(ip), "private or reserved IP literal is blocked");
        return Ok(name);
    }

    // Require real FQDN syntax, avoiding localhost, search-domain,
    // integer/octal/hex IP shortcuts and other resolver-specific spellings.
    ensure!(name.is_ascii(), "IDNs must be provided as validated ASCII punycode");
    let labels: Vec<_> = name.split('.').collect();
    ensure!(labels.len() >= 2, "single-label local destinations are blocked");
    ensure!(labels.iter().all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
            && label.as_bytes().last().is_some_and(u8::is_ascii_alphanumeric)
            && label.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
    }), "invalid domain name");
    let tld = labels.last().copied().unwrap_or_default();
    ensure!(tld.len() >= 2 && tld.bytes().any(|c| c.is_ascii_alphabetic()), "numeric or invalid TLD");
    ensure!(!["local", "localhost", "internal", "home", "lan", "onion", "invalid", "test"].contains(&tld),
        "local or non-public DNS suffix is blocked");
    ensure!(!name.starts_with("0x"), "ambiguous numeric destination is blocked");
    Ok(name)
}

/// Conservative public Internet address check. This deliberately refuses
/// special-purpose ranges, including IPv4-mapped IPv6 and transition
/// mechanisms whose embedded addresses could point to private networks.
/// A production exit needs an independently maintained IANA registry
/// implementation and defense against routing changes.
pub fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => public_ipv4(ip),
        IpAddr::V6(ip) => public_ipv6(ip),
    }
}

fn public_ipv4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    let in_cgnat = o[0] == 100 && (o[1] & 0xc0) == 64;
    let in_benchmark = o[0] == 198 && (o[1] == 18 || o[1] == 19);
    let in_testnet = (o[0] == 192 && o[1] == 0 && (o[2] == 0 || o[2] == 2))
        || (o[0] == 198 && o[1] == 51 && o[2] == 100)
        || (o[0] == 203 && o[1] == 0 && o[2] == 113);
    let in_deprecated_relay = o[0] == 192 && o[1] == 88 && o[2] == 99;
    !(
        o[0] == 0
            || o[0] == 10
            || o[0] == 127
            || o[0] == 169 && o[1] == 254
            || o[0] == 172 && (16..=31).contains(&o[1])
            || o[0] == 192 && o[1] == 168
            || o[0] >= 224
            || in_cgnat
            || in_benchmark
            || in_testnet
            || in_deprecated_relay
    )
}

fn public_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return public_ipv4(mapped);
    }
    let segments = ip.segments();
    // 2000::/3 is the current IPv6 global-unicast allocation. Exclude
    // documentation, Teredo and 6to4 addresses (which can embed private
    // IPv4). This rule can be widened only after evaluating new ranges.
    (segments[0] & 0xe000) == 0x2000
        && !(segments[0] == 0x2001 && segments[1] == 0x0db8)
        && !(segments[0] == 0x2001 && segments[1] == 0x0000)
        && segments[0] != 0x2002
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(raw: &str) -> IpAddr {
        raw.parse().unwrap()
    }

    #[test]
    fn exits_are_disabled_by_default() {
        assert!(ExitPolicy::default().approve("example.com", 443, &[ip("8.8.8.8")]).is_err());
        assert!(ExitPolicy::opt_in([]).is_err());
        assert!(ExitPolicy::opt_in([0]).is_err());
    }

    #[test]
    fn requires_explicit_ports_and_public_destinations() {
        let policy = ExitPolicy::opt_in([443]).unwrap();
        assert!(policy.approve("example.com", 22, &[ip("8.8.8.8")]).is_err());
        assert!(policy.approve("example.com", 443, &[ip("127.0.0.1")]).is_err());
        assert!(policy.approve("example.com", 443, &[ip("8.8.8.8"), ip("10.0.0.1")]).is_err());
        assert!(policy.approve("example.com", 443, &[]).is_err());
        assert!(policy.approve("example.com", 443, &vec![ip("8.8.8.8"); 33]).is_err());
    }

    #[test]
    fn rejects_sensitive_ipv4_ipv6_and_mapped_destinations() {
        for raw in [
            "0.1.2.3", "10.0.0.1", "127.0.0.1", "169.254.169.254",
            "172.16.0.1", "192.168.1.1", "100.64.0.1", "198.18.0.1",
            "192.0.2.1", "198.51.100.1", "203.0.113.9", "224.0.0.1",
            "::", "::1", "fc00::1", "fe80::1", "ff02::1",
            "2001:db8::1", "2002:c0a8:0101::1", "::ffff:127.0.0.1",
            "64:ff9b::a00:1"
        ] {
            assert!(!public_ip(ip(raw)), "special-purpose address permitted: {raw}");
        }
        assert!(public_ip(ip("8.8.8.8")));
        assert!(public_ip(ip("1.1.1.1")));
        assert!(public_ip(ip("2001:4860:4860::8888")));
    }

    #[test]
    fn blocks_local_names_and_ambiguous_dns() {
        let policy = ExitPolicy::opt_in([443]).unwrap();
        for host in [
            "localhost", "localhost.", "localhost.local", "metadata.google.internal",
            "a.home", "a.lan", "a.onion", "0x7f000001", "127.1", "2130706433",
            "api..example.com", "-host.example.com", "foo-.example.com",
            "example.com:443", "user@example.com", "foo/example.com",
            "api.example.com..", "api example.com",
        ] {
            assert!(policy.approve(host, 443, &[ip("8.8.8.8")]).is_err(), "unexpectedly allowed: {host}");
        }
        assert!(policy.approve("api.example.com", 443, &[ip("8.8.8.8")]).is_ok());
        assert!(policy.approve("api.example.com.", 443, &[ip("8.8.8.8")]).is_ok());
    }

    #[test]
    fn resolution_is_pinned_to_prevent_dns_rebinding() {
        let policy = ExitPolicy::opt_in([443, 8443]).unwrap();
        let approved = policy.approve("API.Example.COM", 443, &[ip("1.1.1.1"), ip("8.8.8.8")]).unwrap();
        assert_eq!(approved.host(), "api.example.com");
        assert_eq!(approved.socket_addrs().len(), 2);
        approved.check_connected("8.8.8.8:443".parse().unwrap()).unwrap();
        assert!(approved.check_connected("10.0.0.1:443".parse().unwrap()).is_err());
        assert!(approved.check_connected("8.8.8.8:80".parse().unwrap()).is_err());
        assert!(policy.approve("8.8.8.8", 443, &[ip("1.1.1.1")]).is_err());
    }
}
