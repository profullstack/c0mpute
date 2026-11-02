//! Outbound-request guard shared by every workload that fetches a URL a peer
//! handed us (`ffmpeg.transcode` inputs, `workflow.step` `http_call`).
//!
//! A worker runs on someone's home box or cloud VM. A URL from the network
//! must never reach that box's loopback services, its LAN, the cloud metadata
//! endpoint, or anything else that is not the public internet. The check is
//! done on the resolved addresses (every one of them), and the caller then
//! pins the connection to exactly those addresses so a second DNS answer
//! cannot rebind the host to a private address.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use anyhow::{Context, Result, bail};

/// Is this address publicly routable? Refuses loopback, RFC 1918, link-local
/// (which includes 169.254.169.254, the cloud metadata endpoint), CGNAT,
/// multicast, broadcast, unspecified, documentation, benchmarking and
/// reserved space, IPv6 ULA/link-local/multicast, and IPv6 forms that embed
/// an IPv4 address: mapped, compatible and NAT64 always, 6to4 when the
/// embedded IPv4 address is not itself public.
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] != 0                         // 0.0.0.0/8 "this network"
        && !ip.is_multicast()         // 224.0.0.0/4
        && o[0] < 240                 // 240.0.0.0/4 reserved + broadcast
        && !ip.is_loopback()          // 127.0.0.0/8
        && !ip.is_private()           // 10/8, 172.16/12, 192.168/16
        && !ip.is_link_local()        // 169.254/16 (cloud metadata)
        && !ip.is_broadcast()
        && !ip.is_unspecified()
        && !ip.is_documentation()     // 192.0.2/24, 198.51.100/24, 203.0.113/24
        && !(o[0] == 100 && (o[1] & 0xC0) == 64) // 100.64/10 CGNAT
        && !(o[0] == 192 && o[1] == 0 && o[2] == 0) // 192.0.0/24 IETF protocol
        && !(o[0] == 198 && (o[1] & 0xFE) == 18) // 198.18/15 benchmarking
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return false;
    }
    // fc00::/7 unique local, fe80::/10 link-local, fec0::/10 site-local.
    if (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80 || (s[0] & 0xffc0) == 0xfec0 {
        return false;
    }
    // ::ffff:a.b.c.d (mapped) and ::a.b.c.d (deprecated compatible): refuse
    // outright, a public site has no reason to publish these.
    if s[0..5] == [0, 0, 0, 0, 0] && (s[5] == 0xffff || s[5] == 0) {
        return false;
    }
    // 64:ff9b::/96 and 64:ff9b:1::/48 NAT64: a translator can reach any
    // IPv4 address, including private ones on its own side. Refuse outright.
    if s[0] == 0x64 && s[1] == 0xff9b {
        return false;
    }
    // 2002::/16 6to4 — the IPv4 address lives in segments 1 and 2.
    if s[0] == 0x2002 {
        return is_public_v4(Ipv4Addr::new(
            (s[1] >> 8) as u8,
            s[1] as u8,
            (s[2] >> 8) as u8,
            s[2] as u8,
        ));
    }
    // 2001:db8::/32 documentation.
    !(s[0] == 0x2001 && s[1] == 0x0db8)
}

/// A URL that passed [`vet_url`]: the host to pin, and the addresses the
/// connection may use (every one of them public).
#[derive(Clone, Debug)]
pub struct VettedTarget {
    pub url: url::Url,
    /// Host name to pin with `resolve_to_addrs`. `None` for IP literals,
    /// which need no DNS override.
    pub pin_host: Option<String>,
    pub addrs: Vec<SocketAddr>,
}

/// Parse `raw`, require http/https, resolve the host and require every
/// address to pass `allow` (normally [`is_public_ip`]). The returned
/// addresses must be pinned on the client (`resolve_to_addrs`) so the request
/// cannot be rebound to another address after the check.
pub async fn vet_url(raw: &str, allow: fn(IpAddr) -> bool) -> Result<VettedTarget> {
    let url = url::Url::parse(raw).context("invalid URL")?;
    vet_parsed(url, allow).await
}

pub async fn vet_parsed(url: url::Url, allow: fn(IpAddr) -> bool) -> Result<VettedTarget> {
    match url.scheme() {
        "http" | "https" => {}
        other => bail!("disallowed URL scheme: {other}"),
    }
    let port = url.port_or_known_default().context("URL missing port")?;
    let host = url.host().context("URL missing host")?;
    let (pin_host, addrs) = match host {
        url::Host::Ipv4(ip) => (None, vec![SocketAddr::new(IpAddr::V4(ip), port)]),
        url::Host::Ipv6(ip) => (None, vec![SocketAddr::new(IpAddr::V6(ip), port)]),
        url::Host::Domain(name) => {
            let lower = name.to_ascii_lowercase();
            let bare = lower.trim_end_matches('.');
            if bare == "localhost" || bare.ends_with(".localhost") {
                bail!("localhost URLs are not allowed");
            }
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((name, port))
                .await
                .with_context(|| format!("resolve {name}"))?
                .collect();
            if addrs.is_empty() {
                bail!("{name} did not resolve");
            }
            (Some(name.to_string()), addrs)
        }
    };
    if let Some(bad) = addrs.iter().find(|a| !allow(a.ip())) {
        bail!("URL resolves to a non-public address ({})", bad.ip());
    }
    Ok(VettedTarget {
        url,
        pin_host,
        addrs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_public_addresses_are_refused() {
        for ip in [
            "0.1.2.3",
            "127.0.0.1",
            "10.0.0.1",
            "172.16.5.4",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "198.18.0.1",
            "192.0.0.8",
            "::1",
            "::",
            "fd00::1",
            "fe80::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:8.8.8.8",
            "64:ff9b::a00:1",
            "64:ff9b::808:808",
            "64:ff9b:1::1",
            "2002:a00:1::",
            "2001:db8::1",
        ] {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip} should be refused");
        }
        for ip in [
            "1.1.1.1",
            "8.8.8.8",
            "2606:4700:4700::1111",
            "2002:808:808::",
        ] {
            assert!(is_public_ip(ip.parse().unwrap()), "{ip} should pass");
        }
    }

    #[tokio::test]
    async fn literal_and_named_private_hosts_are_refused() {
        for url in [
            "http://127.0.0.1/",
            "http://0x7f000001/",
            "http://10.1.2.3:8080/x",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]/",
            "http://[fd12::1]/",
            "http://localhost/",
            "http://api.localhost./",
            "ftp://example.com/",
            "file:///etc/passwd",
        ] {
            assert!(vet_url(url, is_public_ip).await.is_err(), "{url}");
        }
    }
}
