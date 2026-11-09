//! Every reachable node is a public gateway, without configuration.
//!
//! A node's gateway listens on `gateway.bind` (`0.0.0.0:7777`). When the
//! operator has not set `gateway.announce`, this finds the node's public IPv4
//! (my.ip.pit.world, ipify as a fallback) and checks that the gateway answers
//! on it from the outside path (`http://<ip>:<port>/healthz`). Only then does
//! the capability ad carry `gateway`, so the node board (`/api/nodes`) lists
//! it and clients such as the Moshpit c0mpute edge load-balance to it. A NAT'd
//! node fails the check and announces nothing. Re-checked every ten minutes,
//! so an address that changes or a port that closes drops out on its own.
//!
//! The status aggregator probes every announced gateway again before it
//! publishes one, and clients verify site bytes against the owner's signature
//! (`/hosting/v1/sites/{site}/resolve`), so a gateway is never trusted for
//! content. Opt out with `[gateway] auto_announce = false` or
//! `C0MPUTE_GATEWAY_AUTO=0`; private mode never announces.

use std::net::Ipv4Addr;
use std::sync::RwLock;
use std::time::Duration;

use tracing::{debug, info};

use crate::config::Config;

const RECHECK: Duration = Duration::from_secs(10 * 60);
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(15);
const HTTP_TIMEOUT: Duration = Duration::from_secs(8);
const IP_SOURCES: &[&str] = &["https://my.ip.pit.world/?format=json", "https://api.ipify.org?format=json"];

static AUTO: RwLock<Option<String>> = RwLock::new(None);

/// The gateway origin found and verified by [`spawn`], if any.
pub fn auto_gateway() -> Option<String> {
    AUTO.read().ok().and_then(|g| g.clone())
}

fn set_auto(value: Option<String>) {
    if let Ok(mut g) = AUTO.write()
        && *g != value
    {
        match &value {
            Some(v) => info!(gateway = %v, "gateway: reachable from the internet; announcing it"),
            None => info!("gateway: not reachable from the internet; not announcing one"),
        }
        *g = value;
    }
}

/// Whether this node should look for a public gateway address itself.
pub fn auto_enabled(config: &Config) -> bool {
    let env_off = std::env::var("C0MPUTE_GATEWAY_AUTO").ok().is_some_and(|v| {
        matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off")
    });
    !env_off
        && config.gateway.auto_announce
        && config.gateway.announce.is_none()
        && !config.private.enabled
        && config.roles.contains(&c0mpute_proto::Role::Gateway)
}

/// A public, routable IPv4 address (no private, loopback, CGNAT, link-local,
/// documentation or reserved ranges).
pub fn is_public_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || o[0] == 0
        || (o[0] == 100 && (64..128).contains(&o[1]))
        || o[0] >= 240)
}

/// The port the gateway listens on, from `gateway.bind`.
pub fn bind_port(bind: &str) -> Option<u16> {
    bind.parse::<std::net::SocketAddr>().ok().map(|a| a.port())
}

/// `http://<ip>:<port>` for a public IPv4.
pub fn candidate(ip: Ipv4Addr, port: u16) -> Option<String> {
    is_public_v4(ip).then(|| format!("http://{ip}:{port}"))
}

async fn public_ipv4(http: &reqwest::Client) -> Option<Ipv4Addr> {
    for url in IP_SOURCES {
        let Ok(res) = http.get(*url).send().await else { continue };
        let Ok(v) = res.json::<serde_json::Value>().await else { continue };
        let raw = v.get("ipv4").or_else(|| v.get("ip")).and_then(|x| x.as_str());
        if let Some(ip) = raw.and_then(|s| s.parse::<Ipv4Addr>().ok()) {
            return Some(ip);
        }
    }
    None
}

/// Does `origin/healthz` answer like a c0mpute gateway?
pub async fn answers(http: &reqwest::Client, origin: &str) -> bool {
    match http.get(format!("{origin}/healthz")).send().await {
        Ok(res) if res.status().is_success() => res.text().await.is_ok_and(|b| b.trim() == "ok"),
        _ => false,
    }
}

/// Start the background check (no-op when auto-announce is off).
pub fn spawn(config: &Config) {
    if !auto_enabled(config) {
        return;
    }
    let Some(port) = bind_port(&config.gateway.bind) else {
        return;
    };
    tokio::spawn(async move {
        let Ok(http) = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
        else {
            return;
        };
        tokio::time::sleep(FIRST_CHECK_DELAY).await;
        loop {
            let found = match public_ipv4(&http).await.and_then(|ip| candidate(ip, port)) {
                Some(origin) if answers(&http, &origin).await => Some(origin),
                Some(origin) => {
                    debug!(%origin, "gateway: public address does not answer (NAT or firewall)");
                    None
                }
                None => None,
            };
            set_auto(found);
            tokio::time::sleep(RECHECK).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_public_v4_becomes_a_candidate() {
        let ip = |s: &str| s.parse::<Ipv4Addr>().unwrap();
        assert_eq!(candidate(ip("2.56.246.141"), 7777).as_deref(), Some("http://2.56.246.141:7777"));
        for private in ["10.0.0.1", "192.168.1.5", "172.16.3.4", "127.0.0.1", "100.64.0.1", "169.254.1.1", "0.0.0.0", "203.0.113.7", "240.0.0.1"] {
            assert_eq!(candidate(ip(private), 7777), None, "{private}");
        }
        assert!(is_public_v4(ip("100.128.0.1")));
    }

    #[test]
    fn port_comes_from_the_bind_address() {
        assert_eq!(bind_port("0.0.0.0:7777"), Some(7777));
        assert_eq!(bind_port("[::]:8080"), Some(8080));
        assert_eq!(bind_port("nope"), None);
    }

    #[test]
    fn gates() {
        let mut c = Config::default();
        assert!(auto_enabled(&c));
        c.gateway.announce = Some("https://gw.example".into());
        assert!(!auto_enabled(&c), "an operator's announce wins");
        c.gateway.announce = None;
        c.gateway.auto_announce = false;
        assert!(!auto_enabled(&c));
        c.gateway.auto_announce = true;
        c.private.enabled = true;
        assert!(!auto_enabled(&c), "never in private mode");
        c.private.enabled = false;
        c.roles.retain(|r| *r != c0mpute_proto::Role::Gateway);
        assert!(!auto_enabled(&c), "only gateways");
    }

    /// A real gateway-shaped server on loopback: `answers` accepts it, and a
    /// closed port or a non-gateway does not.
    #[tokio::test]
    async fn answers_only_a_gateway() {
        let app = axum::Router::new()
            .route("/healthz", axum::routing::get(|| async { "ok" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let other = axum::Router::new().route("/healthz", axum::routing::get(|| async { "nginx" }));
        let l2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr2 = l2.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l2, other).await.unwrap() });
        let http = reqwest::Client::builder().timeout(Duration::from_secs(3)).build().unwrap();
        assert!(answers(&http, &format!("http://{addr}")).await);
        assert!(!answers(&http, &format!("http://{addr2}")).await);
        assert!(!answers(&http, "http://127.0.0.1:1").await);
    }
}
