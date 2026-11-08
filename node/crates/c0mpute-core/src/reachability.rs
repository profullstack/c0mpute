//! Getting results back from workers that cannot be dialed.
//!
//! Workers listen on a random port, usually behind NAT or a firewall, and
//! c0mpute has no relay, so a buyer often cannot open a connection to the
//! worker that holds its result. A buyer gateway, on the other hand, is one
//! machine we control: it listens on a fixed open port (`C0MPUTE_P2P_LISTEN`)
//! and advertises its public address (`C0MPUTE_P2P_EXTERNAL`) in every
//! `JobAccept`. The winning worker dials that address before publishing its
//! receipt, and the buyer's result fetch reuses the connection.

use c0mpute_net::Multiaddr;
use c0mpute_net::Protocol;

/// At most this many buyer addresses are dialed, whatever the accept says.
pub const MAX_BUYER_ADDRS: usize = 4;

fn parse_list(raw: Option<&str>) -> Vec<Multiaddr> {
    raw.unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse::<Multiaddr>().ok())
        .collect()
}

/// `C0MPUTE_P2P_LISTEN`: comma-separated multiaddrs to listen on.
pub fn listen_addrs(raw: Option<&str>) -> Vec<Multiaddr> {
    parse_list(raw)
}

/// `C0MPUTE_P2P_EXTERNAL` as the strings a buyer puts in its accepts, each
/// ending in `/p2p/<buyer_peer_id>`.
pub fn buyer_external_addrs(raw: Option<&str>, buyer_peer_id: &str) -> Vec<String> {
    parse_list(raw)
        .into_iter()
        .filter_map(|addr| {
            let has_peer = addr.iter().any(|p| matches!(p, Protocol::P2p(_)));
            let s = addr.to_string();
            if has_peer {
                s.ends_with(&format!("/p2p/{buyer_peer_id}")).then_some(s)
            } else {
                Some(format!("{s}/p2p/{buyer_peer_id}"))
            }
        })
        .take(MAX_BUYER_ADDRS)
        .collect()
}

/// The buyer addresses a worker may dial: public IPv4/IPv6 over TCP, ending
/// in the accept's own buyer peer id, at most [`MAX_BUYER_ADDRS`]. Anything
/// else is dropped, so an accept cannot make a worker dial a private network
/// or a third party.
pub fn dialable_buyer_addrs(addrs: &[String], buyer_peer_id: &str) -> Vec<Multiaddr> {
    addrs
        .iter()
        .filter_map(|s| s.parse::<Multiaddr>().ok())
        .filter(|addr| {
            let mut ip_ok = false;
            let mut tcp = false;
            let mut peer_ok = false;
            for p in addr.iter() {
                match p {
                    Protocol::Ip4(ip) => ip_ok = crate::net_guard::is_public_ip(ip.into()),
                    Protocol::Ip6(ip) => ip_ok = crate::net_guard::is_public_ip(ip.into()),
                    Protocol::Tcp(_) => tcp = true,
                    Protocol::P2p(peer) => peer_ok = peer.to_base58() == buyer_peer_id,
                    _ => {}
                }
            }
            ip_ok && tcp && peer_ok
        })
        .take(MAX_BUYER_ADDRS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PEER: &str = "12D3KooWADm5FfNxzPRanv1vacpPD7eh1bWv6pqmDCGE69Zof2n6";
    const OTHER: &str = "12D3KooWN99F7k4Fj6SdUqZ2VG4ZBLuw1e1ftwdPDBx3GsiDXzqv";

    #[test]
    fn external_addrs_get_the_buyer_peer_id() {
        let a = buyer_external_addrs(Some("/ip4/23.95.228.174/tcp/46341"), PEER);
        assert_eq!(a, vec![format!("/ip4/23.95.228.174/tcp/46341/p2p/{PEER}")]);
        let wrong = buyer_external_addrs(Some(&format!("/ip4/1.2.3.4/tcp/1/p2p/{OTHER}")), PEER);
        assert!(wrong.is_empty(), "an address for another peer is dropped");
        assert!(buyer_external_addrs(None, PEER).is_empty());
        assert!(buyer_external_addrs(Some("not a multiaddr"), PEER).is_empty());
    }

    #[test]
    fn workers_only_dial_public_tcp_addrs_of_the_buyer() {
        let good = format!("/ip4/23.95.228.174/tcp/46341/p2p/{PEER}");
        let private = format!("/ip4/10.0.0.5/tcp/46341/p2p/{PEER}");
        let loopback = format!("/ip4/127.0.0.1/tcp/46341/p2p/{PEER}");
        let other_peer = format!("/ip4/23.95.228.174/tcp/46341/p2p/{OTHER}");
        let no_peer = "/ip4/23.95.228.174/tcp/46341".to_string();
        let addrs = vec![good.clone(), private, loopback, other_peer, no_peer];
        let ok = dialable_buyer_addrs(&addrs, PEER);
        assert_eq!(ok.len(), 1);
        assert_eq!(ok[0].to_string(), good);
    }

    #[test]
    fn at_most_four_addrs() {
        let addrs: Vec<String> = (1..10)
            .map(|i| format!("/ip4/23.95.228.{i}/tcp/46341/p2p/{PEER}"))
            .collect();
        assert_eq!(dialable_buyer_addrs(&addrs, PEER).len(), MAX_BUYER_ADDRS);
    }

    #[test]
    fn listen_list_parses() {
        assert_eq!(
            listen_addrs(Some("/ip4/0.0.0.0/tcp/46341, /ip6/::/tcp/46341")).len(),
            2
        );
        assert!(listen_addrs(Some("")).is_empty());
    }
}
