//! OpenDisk: every storage node describes its own disk at
//! `/.well-known/opendisk.json` on its gateway, and lists itself on an OpenDisk
//! marketplace (d1sks) with a signed heartbeat, NAT or not.
//!
//! There is no central registry in this path. A node states its own name,
//! identity key, free space and price, signs that with its libp2p (ed25519)
//! identity, and pushes it to d1sks every [`HEARTBEAT_EVERY`]. d1sks checks
//! that the key is the peer id and that the signature holds, and lets the
//! listing expire [`LISTING_TTL`] after the last heartbeat. A node with a
//! public https origin says so in the same message; one behind NAT is listed
//! as reachable through c0mpute (holders and gateways talk over libp2p).

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use tracing::{info, warn};

use crate::config::{Config, StorageConfig};

/// What a node asks per GiB-month when the operator has not set a price.
/// Below the big object stores, since the data is erasure-coded across peers.
pub const DEFAULT_PRICE_USD_PER_GIB_MONTH: f64 = 0.004;

/// The OpenDisk marketplace nodes list themselves on.
pub const DEFAULT_D1SKS_URL: &str = "https://d1sks.pwa.pit.world";

/// The descriptor path the OpenDisk spec fixes.
pub const WELL_KNOWN_PATH: &str = "/.well-known/opendisk.json";

/// How often a node re-sends its signed listing.
pub const HEARTBEAT_EVERY: Duration = Duration::from_secs(5 * 60);

/// How long a listing stays up after its last heartbeat (three missed beats,
/// plus slack).
pub const LISTING_TTL: Duration = Duration::from_secs(20 * 60);

/// Where d1sks takes signed listings.
pub const ANNOUNCE_PATH: &str = "/api/v1/disks/announce";

/// The `type` of a signed listing payload.
pub const ANNOUNCE_TYPE: &str = "c0mpute.disk.announce/1";

/// Multiaddrs sent per listing.
pub const MAX_LISTED_ADDRS: usize = 8;

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// Free and total bytes this node offers for storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capacity {
    pub free_bytes: u64,
    pub total_bytes: u64,
}

/// The filesystem under `root`: (available, total). Picks the deepest mount
/// that contains the root; falls back to the largest disk.
pub(crate) fn disk_for(root: &Path) -> (u64, u64) {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let mut best: Option<(usize, u64, u64)> = None;
    let mut largest = (0u64, 0u64);
    for disk in disks.list() {
        if disk.total_space() > largest.1 {
            largest = (disk.available_space(), disk.total_space());
        }
        let mount = disk.mount_point();
        if root.starts_with(mount) {
            let depth = mount.components().count();
            if best.is_none_or(|(d, _, _)| depth > d) {
                best = Some((depth, disk.available_space(), disk.total_space()));
            }
        }
    }
    best.map(|(_, free, total)| (free, total)).unwrap_or(largest)
}

/// What this node offers: the operator's cap when set (minus what is already
/// held), never more than the disk really has free.
pub fn capacity(storage: &StorageConfig, used_bytes: u64) -> Capacity {
    let (disk_free, disk_total) = disk_for(&storage.root);
    offered(storage.cap_bytes, used_bytes, disk_free, disk_total)
}

fn offered(cap: Option<u64>, used: u64, disk_free: u64, disk_total: u64) -> Capacity {
    match cap {
        Some(cap) => Capacity {
            free_bytes: cap.saturating_sub(used).min(disk_free),
            total_bytes: cap,
        },
        None => Capacity {
            free_bytes: disk_free,
            total_bytes: disk_total,
        },
    }
}

fn gib(bytes: u64) -> f64 {
    // Two decimals is plenty for a listing and keeps the JSON stable.
    ((bytes as f64 / GIB) * 100.0).floor() / 100.0
}

/// The price to publish: the operator's, or the default when it is not a
/// usable number.
pub fn price(storage: &StorageConfig) -> f64 {
    let p = storage.price_usd_per_gib_month;
    if p.is_finite() && p >= 0.0 {
        p
    } else {
        DEFAULT_PRICE_USD_PER_GIB_MONTH
    }
}

/// The OpenDisk 0.1 descriptor for this node.
///
/// `key_hex` is the node's ed25519 identity (from its peer id); `origin` is the
/// public origin the gateway announces, if any.
pub fn descriptor(
    config: &Config,
    peer_id: &str,
    key_hex: Option<&str>,
    origin: Option<&str>,
    cap: Capacity,
) -> Value {
    let short: String = peer_id
        .chars()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let name = match crate::capabilities::node_name() {
        Some(n) => format!("{n} · c0mpute node {short}"),
        None => format!("c0mpute node {short}"),
    };
    let hosts = config.roles.contains(&c0mpute_proto::Role::Hosting);
    let roles: Vec<Value> = config
        .roles
        .iter()
        .filter_map(|r| serde_json::to_value(r).ok())
        .collect();
    let mut doc = json!({
        "opendisk": "0.1",
        "name": name,
        "web": "https://c0mpute.com/nodes",
        "capacity": {
            "free_gib": gib(cap.free_bytes),
            "total_gib": gib(cap.total_bytes),
        },
        "price": {
            "currency": "USD",
            "per_gib_month": price(&config.storage),
        },
        "accepts": {
            "visibility": ["public", "private"],
            // Private objects arrive already encrypted by their owner
            // (DIP-0012 `private` tier): this node stores and serves
            // ciphertext it has no key for. Public sites stay plaintext.
            "encrypted": true,
            "encrypted_only": false,
            "client_side_encryption": "c0mpute-private-v1",
            // A c0mpute hosting node holds static sites for the network and
            // can be picked as a paid (preferred) host by
            // `c0mpute hosting publish --disks auto`.
            "hosting": hosts,
        },
        "updated": chrono_now(),
        // Unknown to OpenDisk, kept by readers: how to reach this disk on
        // the c0mpute network itself.
        "c0mpute": {
            "peer_id": peer_id,
            "version": env!("CARGO_PKG_VERSION"),
            "storage_api": origin.map(|o| format!("{}/storage/v1", o.trim_end_matches('/'))),
            "redundancy": "reed-solomon 10/14",
            "private_storage": {
                "format": "c0mpute-private-v1",
                "cipher": "AES-256-GCM, X25519 key wrap, client-held keys",
                "operator_sees": "ciphertext, padded size, access timing",
                "docs": "https://c0mpute.com/getting-started#private-storage",
            },
            "roles": roles,
        },
    });
    if hosts {
        doc["c0mpute"]["hosting"] = json!({
            "price_per_gib_month": config.hosting.price_micros() as f64 / crate::payments::MICROS_PER_USD as f64,
            "max_gib": gib(hosting_budget(config)),
            "publish": "c0mpute hosting publish <dir> --name <site> --disks auto",
        });
    }
    if let Some(k) = key_hex {
        doc["key"] = Value::String(format!("ed25519:{k}"));
    }
    doc
}

/// The hosting budget to advertise: `[hosting] max_bytes`, or the automatic
/// budget of the storage disk when it is 0.
fn hosting_budget(config: &Config) -> u64 {
    if config.hosting.max_bytes > 0 {
        return config.hosting.max_bytes;
    }
    let (free, total) = disk_for(&config.storage.root);
    crate::hosting::auto_budget(0, free, total)
}

fn chrono_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days as i64);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The origin a marketplace can fetch from: the gateway's announced address,
/// only when it is https (OpenDisk readers fetch https only).
pub fn public_origin(config: &Config) -> Option<String> {
    let announce = config.gateway.announce.as_deref()?.trim();
    let url = reqwest::Url::parse(announce).ok()?;
    if url.scheme() != "https" || url.host_str().is_none() {
        return None;
    }
    Some(url.origin().ascii_serialization())
}

/// How a marketplace (or a publisher) can reach this disk: `https` (a public
/// https origin), `direct` (a public IP multiaddr), or `c0mpute` (only over
/// connections the node opens itself; still a full holder, since hosting and
/// storage move over libp2p).
pub fn reachability(origin: Option<&str>, addrs: &[String]) -> &'static str {
    if origin.is_some() {
        "https"
    } else if addrs.iter().any(|a| is_public_multiaddr(a)) {
        "direct"
    } else {
        "c0mpute"
    }
}

/// A multiaddr on a public IP (or a DNS name).
pub fn is_public_multiaddr(addr: &str) -> bool {
    use c0mpute_net::Protocol;
    let Ok(ma) = addr.parse::<c0mpute_net::Multiaddr>() else {
        return false;
    };
    ma.iter().any(|p| match p {
        Protocol::Ip4(ip) => {
            let o = ip.octets();
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.is_documentation()
                || (o[0] == 100 && (o[1] & 0xc0) == 64))
        }
        Protocol::Ip6(ip) => {
            let seg = ip.segments();
            !(ip.is_loopback()
                || ip.is_unspecified()
                || (seg[0] & 0xfe00) == 0xfc00
                || (seg[0] & 0xffc0) == 0xfe80)
        }
        Protocol::Dns(_) | Protocol::Dns4(_) | Protocol::Dns6(_) => true,
        _ => false,
    })
}

/// The addresses worth listing: no wildcard binds, at most [`MAX_LISTED_ADDRS`],
/// public ones first.
pub fn listable_addrs(addrs: Vec<String>) -> Vec<String> {
    let mut v: Vec<String> = addrs
        .into_iter()
        .filter(|a| !a.contains("/ip4/0.0.0.0") && !a.contains("/ip6/::/") && a.len() <= 200)
        .collect();
    v.sort_by_key(|a| !is_public_multiaddr(a));
    v.dedup();
    v.truncate(MAX_LISTED_ADDRS);
    v
}

/// The signed listing d1sks takes at [`ANNOUNCE_PATH`]:
/// `{ "payload": "<JSON text>", "signature": "<hex ed25519 over the text>" }`.
///
/// The payload binds the marketplace host (so a listing cannot be replayed to
/// another marketplace), the time (d1sks refuses old or reused ones), the
/// peer id and its key, and the OpenDisk descriptor with the c0mpute block
/// (roles, version, addrs, reachability).
pub fn signed_listing(
    config: &Config,
    keypair: &c0mpute_net::libp2p::identity::Keypair,
    market_url: &str,
    origin: Option<&str>,
    addrs: Vec<String>,
    cap: Capacity,
    unix_secs: u64,
) -> anyhow::Result<Value> {
    let peer = keypair.public().to_peer_id();
    let key_hex = c0mpute_net::identity::ed25519_public_hex(&peer)
        .ok_or_else(|| anyhow::anyhow!("this identity is not an ed25519 key"))?;
    let peer_id = peer.to_base58();
    let addrs = listable_addrs(addrs);
    let mut doc = descriptor(config, &peer_id, Some(&key_hex), origin, cap);
    doc["c0mpute"]["addrs"] = json!(addrs);
    doc["c0mpute"]["reachable"] = json!(reachability(origin, &addrs));
    if let Some(c) = config.country() {
        doc["location"] = json!({ "countries": [c] });
    }
    let market_host = reqwest::Url::parse(market_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default();
    let payload = json!({
        "type": ANNOUNCE_TYPE,
        "marketplace": market_host,
        "peer_id": peer_id,
        "key": format!("ed25519:{key_hex}"),
        "ts": unix_secs,
        "expires_in": LISTING_TTL.as_secs(),
        "origin": origin,
        "descriptor": doc,
    });
    let text = serde_json::to_string(&payload)?;
    let sig = keypair
        .sign(text.as_bytes())
        .map_err(|e| anyhow::anyhow!("sign listing: {e}"))?;
    Ok(json!({ "payload": text, "signature": hex::encode(sig) }))
}

/// Push one signed listing to the marketplace. Returns its answer.
pub async fn announce(market_url: &str, body: &Value) -> anyhow::Result<Value> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("c0mpute/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let res = client
        .post(format!("{}{ANNOUNCE_PATH}", market_url.trim_end_matches('/')))
        .json(body)
        .send()
        .await?;
    let status = res.status();
    let answer: Value = res.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        anyhow::bail!("d1sks answered {status}: {answer}");
    }
    Ok(answer)
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Keep this disk listed on d1sks: a signed heartbeat now and every
/// [`HEARTBEAT_EVERY`], NAT or not. A no-op when the node does not list
/// (`Config::lists_on_d1sks`: storage off, OpenDisk off, `list_on_d1sks =
/// false`, `C0MPUTE_D1SKS=0`, or private mode).
pub fn spawn_listing(config: &Config, net: std::sync::Arc<c0mpute_net::Libp2pNetwork>) {
    if !config.lists_on_d1sks() {
        info!("opendisk: not listing this disk on d1sks (storage, listing or private mode says no)");
        return;
    }
    let identity_dir = crate::config::config_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    let keypair = match c0mpute_net::identity::load_or_create(&identity_dir) {
        Ok(k) if k.public().to_peer_id() == net.peer_id() => k,
        Ok(_) => {
            warn!("opendisk: the identity on disk is not this node's peer id; not listing");
            return;
        }
        Err(e) => {
            warn!(err = %e, "opendisk: no identity to sign the d1sks listing with");
            return;
        }
    };
    let config = config.clone();
    tokio::spawn(async move {
        // Let the swarm learn its addresses before the first listing.
        tokio::time::sleep(Duration::from_secs(20)).await;
        let market = config.storage.d1sks_url.clone();
        let origin = public_origin(&config);
        let mut logged_ok = false;
        loop {
            let addrs: Vec<String> = net.advertised_addrs().await.into_iter().map(|a| a.to_string()).collect();
            let cap = capacity(&config.storage, 0);
            match signed_listing(&config, &keypair, &market, origin.as_deref(), addrs, cap, unix_now()) {
                Ok(body) => match announce(&market, &body).await {
                    Ok(_) if !logged_ok => {
                        info!(%market, "opendisk: listed on d1sks (signed heartbeat every {}s)", HEARTBEAT_EVERY.as_secs());
                        logged_ok = true;
                    }
                    Ok(_) => tracing::debug!(%market, "opendisk: d1sks heartbeat"),
                    Err(e) => warn!(%market, err = %e, "opendisk: d1sks listing failed"),
                },
                Err(e) => warn!(err = %e, "opendisk: could not build the d1sks listing"),
            }
            // A little jitter so a fleet restarted together does not beat in step.
            let jitter = Duration::from_secs(unix_now() % 30);
            tokio::time::sleep(HEARTBEAT_EVERY + jitter).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: u64 = 1024 * 1024 * 1024;

    #[test]
    fn uncapped_offers_the_disk() {
        let c = offered(None, 5 * G, 100 * G, 500 * G);
        assert_eq!(c.free_bytes, 100 * G);
        assert_eq!(c.total_bytes, 500 * G);
    }

    #[test]
    fn cap_limits_free_space_and_counts_what_is_held() {
        let c = offered(Some(50 * G), 20 * G, 400 * G, 500 * G);
        assert_eq!(c.free_bytes, 30 * G);
        assert_eq!(c.total_bytes, 50 * G);
    }

    #[test]
    fn never_offers_more_than_the_disk_has() {
        let c = offered(Some(50 * G), 0, 10 * G, 500 * G);
        assert_eq!(c.free_bytes, 10 * G);
    }

    #[test]
    fn descriptor_is_valid_opendisk() {
        let cfg = Config::default();
        let key = "a".repeat(64);
        let d = descriptor(
            &cfg,
            "12D3KooWSFxwveBjrGy4n1Djg3vpQD5aHPCXYCQHZWruffig7P4J",
            Some(&key),
            Some("https://disk.example"),
            Capacity {
                free_bytes: 900 * G,
                total_bytes: 1000 * G,
            },
        );
        assert!(d["name"].as_str().unwrap().contains("ffig7P4J"));
        assert_eq!(d["capacity"]["free_gib"], 900.0);
        assert_eq!(d["price"]["per_gib_month"], DEFAULT_PRICE_USD_PER_GIB_MONTH);
        assert_eq!(d["key"], format!("ed25519:{key}"));
        assert_eq!(d["c0mpute"]["storage_api"], "https://disk.example/storage/v1");
        assert!(d["updated"].as_str().unwrap().ends_with('Z'));
        // Default roles include hosting: the disk says so, for d1sks to filter on.
        assert_eq!(d["accepts"]["hosting"], true);
        assert_eq!(d["accepts"]["encrypted"], true);
        assert_eq!(d["c0mpute"]["private_storage"]["format"], "c0mpute-private-v1");
        assert!(d["c0mpute"]["roles"].as_array().unwrap().iter().any(|r| r == "hosting"));
        assert_eq!(d["c0mpute"]["hosting"]["price_per_gib_month"], 0.004);
        assert_eq!(d["c0mpute"]["hosting"]["max_gib"], 5.0);
    }

    #[test]
    fn a_node_without_the_hosting_role_does_not_offer_hosting() {
        let mut cfg = Config::default();
        cfg.roles.retain(|r| *r != c0mpute_proto::Role::Hosting);
        let d = descriptor(&cfg, "12D3KooWSFxwveBjrGy4n1Djg3vpQD5aHPCXYCQHZWruffig7P4J", None, None, Capacity {
            free_bytes: G,
            total_bytes: G,
        });
        assert_eq!(d["accepts"]["hosting"], false);
        assert!(d["c0mpute"].get("hosting").is_none());
    }

    #[test]
    fn the_descriptor_parses_as_a_marketplace_hosting_disk() {
        // What d1sks stores is the descriptor under `data`; the publisher's
        // ranking reads it back as a hosting node.
        let cfg = Config::default();
        let peer = c0mpute_net::libp2p::identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id()
            .to_base58();
        let d = descriptor(&cfg, &peer, None, Some("https://disk.example"), Capacity {
            free_bytes: 100 * G,
            total_bytes: 200 * G,
        });
        let listing = json!({ "disks": [{ "name": "n", "status": "active", "free_gib": 100.0,
            "per_gib_month": 0.004, "countries": [], "data": d }] });
        let disks = crate::hosting::market::parse_disks(&listing);
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].peer_id, peer);
        assert_eq!(disks[0].price, 4_000);
        assert_eq!(disks[0].free_bytes, 5 * G, "hosting offers its hosting budget, not the whole disk");
    }

    #[test]
    fn only_https_origins_are_listable() {
        let mut cfg = Config::default();
        cfg.gateway.announce = Some("http://1.2.3.4:7777".into());
        assert_eq!(public_origin(&cfg), None);
        cfg.gateway.announce = Some("https://disk.example/some/path".into());
        assert_eq!(public_origin(&cfg).as_deref(), Some("https://disk.example"));
    }

    #[test]
    fn signed_listing_verifies_with_the_peer_key() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let cfg = Config::default();
        let body = signed_listing(
            &cfg,
            &kp,
            "https://d1sks.example/",
            None,
            vec![
                "/ip4/0.0.0.0/tcp/1".into(),
                "/ip4/192.168.1.5/tcp/4001".into(),
            ],
            Capacity { free_bytes: 10 * G, total_bytes: 20 * G },
            1_800_000_000,
        )
        .unwrap();
        let text = body["payload"].as_str().unwrap();
        let sig = hex::decode(body["signature"].as_str().unwrap()).unwrap();
        assert!(kp.public().verify(text.as_bytes(), &sig));
        let p: Value = serde_json::from_str(text).unwrap();
        assert_eq!(p["type"], ANNOUNCE_TYPE);
        assert_eq!(p["marketplace"], "d1sks.example");
        assert_eq!(p["peer_id"], kp.public().to_peer_id().to_base58());
        assert_eq!(p["ts"], 1_800_000_000u64);
        assert_eq!(p["descriptor"]["key"], p["key"]);
        assert_eq!(p["descriptor"]["c0mpute"]["reachable"], "c0mpute");
        assert_eq!(p["descriptor"]["c0mpute"]["addrs"], json!(["/ip4/192.168.1.5/tcp/4001"]));
        assert_eq!(p["descriptor"]["capacity"]["free_gib"], 10.0);
    }

    #[test]
    fn reachability_reads_origin_then_public_addrs() {
        let lan = vec!["/ip4/10.0.0.2/tcp/1".to_string(), "/ip4/100.64.1.1/tcp/1".to_string()];
        assert_eq!(reachability(None, &lan), "c0mpute");
        assert_eq!(reachability(Some("https://x.example"), &lan), "https");
        assert_eq!(reachability(None, &["/ip4/203.0.113.9/tcp/1".to_string()]), "c0mpute", "documentation range");
        assert_eq!(reachability(None, &["/ip4/5.9.10.11/tcp/46341".to_string()]), "direct");
        assert_eq!(reachability(None, &["/ip6/2a01:4f8::1/tcp/1".to_string()]), "direct");
        assert_eq!(reachability(None, &["/ip6/fd00::1/tcp/1".to_string()]), "c0mpute");
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_765), (2026, 11, 8));
    }
}
