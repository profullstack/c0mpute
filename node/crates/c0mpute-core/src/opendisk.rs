//! OpenDisk: every storage node describes its own disk at
//! `/.well-known/opendisk.json` on its gateway, and lists itself on an OpenDisk
//! marketplace (d1sks) when it has a public https origin.
//!
//! There is no central registry in this path. A node states its own name,
//! identity key, free space and price; the marketplace only reads what the
//! node serves, and re-reads it every hour. A node behind NAT still serves the
//! descriptor locally, it just has no origin a marketplace can reach yet.

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

/// How often a node re-submits itself, so a marketplace that lost it (or a
/// node whose origin changed) catches up without anyone stepping in.
pub const REREGISTER_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// Free and total bytes this node offers for storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capacity {
    pub free_bytes: u64,
    pub total_bytes: u64,
}

/// The filesystem under `root`: (available, total). Picks the deepest mount
/// that contains the root; falls back to the largest disk.
fn disk_for(root: &Path) -> (u64, u64) {
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
            "encrypted_only": false,
        },
        "updated": chrono_now(),
        // Unknown to OpenDisk, kept by readers: how to reach this disk on
        // the c0mpute network itself.
        "c0mpute": {
            "peer_id": peer_id,
            "version": env!("CARGO_PKG_VERSION"),
            "storage_api": origin.map(|o| format!("{}/storage/v1", o.trim_end_matches('/'))),
            "redundancy": "reed-solomon 10/14",
        },
    });
    if let Some(k) = key_hex {
        doc["key"] = Value::String(format!("ed25519:{k}"));
    }
    doc
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

/// Submit `origin` to the marketplace. Returns the marketplace's answer.
pub async fn register(d1sks_url: &str, origin: &str) -> anyhow::Result<Value> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("c0mpute/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let res = client
        .post(format!("{}/api/v1/disks", d1sks_url.trim_end_matches('/')))
        .json(&json!({ "url": origin }))
        .send()
        .await?;
    let status = res.status();
    let body: Value = res.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        anyhow::bail!("d1sks answered {status}: {body}");
    }
    Ok(body)
}

/// Keep this node listed: register now, then once a day. A no-op when the
/// operator turned OpenDisk off or the gateway has no public https origin.
pub fn spawn_self_listing(config: &Config) {
    if !config.storage.opendisk {
        return;
    }
    if config.private.enabled {
        info!("opendisk: private mode, not listing this disk on d1sks");
        return;
    }
    let Some(origin) = public_origin(config) else {
        info!(
            "opendisk: descriptor served at {WELL_KNOWN_PATH}; not listing on d1sks \
             (set gateway.announce to this node's public https origin to list it)"
        );
        return;
    };
    let market = config.storage.d1sks_url.clone();
    tokio::spawn(async move {
        // Let the gateway bind before the marketplace fetches from it.
        tokio::time::sleep(Duration::from_secs(15)).await;
        loop {
            match register(&market, &origin).await {
                Ok(_) => info!(%origin, %market, "opendisk: listed on d1sks"),
                Err(e) => warn!(%origin, %market, err = %e, "opendisk: d1sks listing failed"),
            }
            tokio::time::sleep(REREGISTER_EVERY).await;
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
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_765), (2026, 11, 8));
    }
}
