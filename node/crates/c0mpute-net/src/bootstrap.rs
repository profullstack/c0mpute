//! Best-effort, account-free libp2p bootstrap.
//!
//! Standard startup dials known libp2p peers but does not send an HTTPS
//! check-in to c0mpute.com. Independent peers can replace *all* built-in
//! contacts with C0MPUTE_BOOTSTRAP_MODE=community. Peer identities are
//! authenticated by libp2p; operator-provided addresses require a /p2p ID.
//! See docs/protocol/scallion-bootstrap.md.

use std::time::Duration;

use anyhow::{Result, bail};
use libp2p::{Multiaddr, PeerId, multiaddr::Protocol};
use serde::Deserialize;
use tracing::{debug, warn};

pub const DEFAULT_BOOTSTRAP_URL: &str = "https://c0mpute.com/bootstrap.json";
pub const DEFAULT_SEEDS: &[&str] = &[
    "/ip4/23.95.228.174/tcp/46337/p2p/12D3KooWADm5FfNxzPRanv1vacpPD7eh1bWv6pqmDCGE69Zof2n6",
    "/ip4/152.53.47.37/tcp/46339/p2p/12D3KooWN99F7k4Fj6SdUqZ2VG4ZBLuw1e1ftwdPDBx3GsiDXzqv",
];

pub fn default_seeds() -> Vec<Multiaddr> {
    DEFAULT_SEEDS.iter().filter_map(|s| s.parse().ok()).collect()
}

#[derive(Clone, Debug, Deserialize)]
pub struct BootstrapFile {
    pub version: u32,
    #[serde(default)]
    pub protocol_id: Option<String>,
    pub peers: Vec<BootstrapPeer>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct BootstrapPeer {
    pub id: String,
    pub addrs: Vec<String>,
    #[serde(default)]
    pub operator: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
}

const KAD_PROTOCOL: &str = "/c0mpute/kad/1.0.0";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Built-in direct libp2p contacts plus user-provided peers.
    Public,
    /// Only user-provided peers; zero traffic to the built-in seeds.
    Community,
    /// No configured WAN bootstrap; mDNS remains enabled separately.
    Offline,
}

impl Mode {
    fn parse(raw: Option<&str>) -> Self {
        match raw.unwrap_or("public").trim().to_ascii_lowercase().as_str() {
            "" | "public" | "default" => Self::Public,
            "community" | "custom" => Self::Community,
            "offline" | "none" => Self::Offline,
            unknown => {
                // Avoid silently dialing built-in seeds when someone typed
                // an unrecognized privacy-sensitive mode.
                warn!(mode = unknown, "unknown bootstrap mode; using community-only contacts");
                Self::Community
            }
        }
    }
}

fn merge_unique(dest: &mut Vec<Multiaddr>, addrs: impl IntoIterator<Item = Multiaddr>) {
    for a in addrs {
        if !dest.contains(&a) {
            dest.push(a);
        }
    }
}

/// Operator-provided peers: complete, authenticated libp2p Multiaddrs.
/// Comments and empty lines are allowed in the optional text file.
fn parse_peers(text: &str) -> Vec<Multiaddr> {
    let mut out = Vec::new();
    for raw in text.split([',', '\n', '\r']) {
        let s = raw.split('#').next().unwrap_or("").trim();
        if s.is_empty() {
            continue;
        }
        match s.parse::<Multiaddr>() {
            Ok(a) if matches!(a.iter().last(), Some(Protocol::P2p(_))) => {
                merge_unique(&mut out, [a]);
            }
            _ => warn!(addr = s, "invalid bootstrap contact (requires trailing /p2p/<peer-id>)"),
        }
    }
    out
}

fn custom_contacts() -> Vec<Multiaddr> {
    let mut all = std::env::var("C0MPUTE_BOOTSTRAP_PEERS")
        .ok()
        .map(|s| parse_peers(&s))
        .unwrap_or_default();
    if let Ok(file) = std::env::var("C0MPUTE_BOOTSTRAP_FILE") {
        if !file.trim().is_empty() {
            match std::fs::read_to_string(file.trim()) {
                Ok(text) => merge_unique(&mut all, parse_peers(&text)),
                Err(e) => warn!(path = file, err = %e, "unable to read optional bootstrap file"),
            }
        }
    }
    all
}

struct Plan {
    peers: Vec<Multiaddr>,
    feed: Option<String>,
}

fn plan(mode: Mode, defaults: Vec<Multiaddr>, custom: Vec<Multiaddr>, feed: Option<&str>) -> Plan {
    if mode == Mode::Offline {
        return Plan { peers: vec![], feed: None };
    }
    let mut peers = if mode == Mode::Public { defaults } else { vec![] };
    merge_unique(&mut peers, custom);
    let feed = feed.and_then(|input| {
        let input = input.trim();
        if input.starts_with("https://") && input.len() > "https://".len() {
            Some(input.to_string())
        } else {
            if !input.is_empty() {
                warn!(url = input, "bootstrap feed must use HTTPS, ignoring");
            }
            None
        }
    });
    Plan { peers, feed }
}

/// Decode a versioned peer feed with strict /p2p identity matching.
fn feed_addrs(input: BootstrapFile) -> Result<Vec<Multiaddr>> {
    if input.version != 1 {
        bail!("unsupported bootstrap feed version {}", input.version);
    }
    if let Some(id) = &input.protocol_id {
        if id != KAD_PROTOCOL {
            bail!("bootstrap protocol mismatch: {id}");
        }
    }

    let mut output = Vec::new();
    for peer in input.peers {
        let id: PeerId = match peer.id.parse() {
            Ok(id) => id,
            Err(e) => {
                debug!(peer_id = peer.id, err = %e, "skipping malformed peer id");
                continue;
            }
        };
        for raw in peer.addrs {
            let Ok(addr) = raw.parse::<Multiaddr>() else {
                debug!(addr = raw, "skipping malformed peer address");
                continue;
            };
            match addr.iter().last() {
                Some(Protocol::P2p(actual)) if actual == id => merge_unique(&mut output, [addr]),
                Some(Protocol::P2p(_)) => warn!(addr = raw, "bootstrap address peer id mismatch"),
                _ => merge_unique(&mut output, [addr.with(Protocol::P2p(id))]),
            }
        }
    }
    Ok(output)
}

/// Optional HTTPS seed-directory retrieval. The directory is not an
/// authority: all returned contacts are still authenticated as libp2p peers.
pub async fn fetch(url: &str) -> Result<Vec<Multiaddr>> {
    let body = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    feed_addrs(serde_json::from_str(&body)?)
}

/// API-compatible bootstrap helper, now offline-first with respect to HTTP.
///
/// * Default: dial built-in authenticated libp2p seed contacts, without HTTP.
/// * COMMUNITY: use only C0MPUTE_BOOTSTRAP_PEERS / C0MPUTE_BOOTSTRAP_FILE.
/// * OFFLINE: skip all bootstrap contacts (LAN mDNS is unaffected).
/// * C0MPUTE_BOOTSTRAP_URL=https://…: explicitly opt in to a third-party feed.
/// * C0MPUTE_BOOTSTRAP_FETCH=1: opt in to the historic default HTTPS list.
/// Failure of the optional feed never stops startup.
pub async fn fetch_or_fallback(url: &str, fallback: Vec<Multiaddr>) -> Vec<Multiaddr> {
    let mode = Mode::parse(std::env::var("C0MPUTE_BOOTSTRAP_MODE").ok().as_deref());
    if mode == Mode::Offline {
        return vec![];
    }
    let configured_feed = std::env::var("C0MPUTE_BOOTSTRAP_URL")
        .ok()
        .filter(|x| !x.trim().is_empty());
    let default_feed = mode == Mode::Public
        && std::env::var("C0MPUTE_BOOTSTRAP_FETCH")
            .is_ok_and(|s| s == "1" || s.eq_ignore_ascii_case("true"));
    let feed = configured_feed.or_else(|| default_feed.then(|| url.to_string()));
    let mut picked = plan(mode, fallback, custom_contacts(), feed.as_deref());

    if let Some(ref feed) = picked.feed {
        match fetch(feed).await {
            Ok(remote) => merge_unique(&mut picked.peers, remote),
            Err(e) => warn!(url = feed, err = %e, "optional bootstrap feed unavailable"),
        }
    }
    picked.peers
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(id: PeerId) -> Multiaddr {
        format!("/ip4/127.0.0.1/tcp/4444/p2p/{id}").parse().unwrap()
    }

    #[test]
    fn default_seeds_are_parseable() {
        assert_eq!(default_seeds().len(), DEFAULT_SEEDS.len());
    }

    #[test]
    fn default_never_requests_https() {
        let x = plan(Mode::Public, default_seeds(), vec![], None);
        assert_eq!(x.peers, default_seeds());
        assert!(x.feed.is_none());
    }

    #[test]
    fn community_does_not_contact_builtin_operator() {
        let custom = address(PeerId::random());
        let x = plan(Mode::Community, default_seeds(), vec![custom.clone(), custom.clone()], None);
        assert_eq!(x.peers, vec![custom]);
        assert!(x.feed.is_none());
    }

    #[test]
    fn offline_skips_all_contacts_including_remote_feeds() {
        let x = plan(Mode::Offline, default_seeds(), vec![address(PeerId::random())],
            Some("https://example.net/seeds.json"));
        assert!(x.peers.is_empty());
        assert!(x.feed.is_none());
    }

    #[test]
    fn invalid_user_contacts_are_not_trusted() {
        let id = PeerId::random();
        let good = address(id);
        let text = format!("# comment\n{good}\n/ip4/127.0.0.1/tcp/4444,garbage,{good}\n");
        assert_eq!(parse_peers(&text), vec![good]);
    }

    #[test]
    fn non_tls_feed_is_rejected() {
        assert!(plan(Mode::Community, vec![], vec![], Some("http://host/seeds")).feed.is_none());
        assert_eq!(plan(Mode::Community, vec![], vec![], Some("https://host/seeds")).feed.as_deref(),
            Some("https://host/seeds"));
    }

    #[test]
    fn feed_rejects_conflicting_peer_id() {
        let id = PeerId::random();
        let other = PeerId::random();
        let good = address(id);
        let file = BootstrapFile {
            version: 1,
            protocol_id: Some(KAD_PROTOCOL.into()),
            peers: vec![BootstrapPeer {
                id: id.to_string(),
                addrs: vec![address(other).to_string(), good.to_string(),
                    "/ip4/127.0.0.1/tcp/4444".into()],
                operator: None,
                region: None,
            }],
        };
        assert_eq!(feed_addrs(file).unwrap(), vec![good]);
    }

    #[test]
    fn version_and_protocol_mismatch_fail_closed() {
        let file = |version, protocol_id| BootstrapFile { version, protocol_id, peers: vec![] };
        assert!(feed_addrs(file(2, None)).is_err());
        assert!(feed_addrs(file(1, Some("/other/kad/v1".into()))).is_err());
    }
}
