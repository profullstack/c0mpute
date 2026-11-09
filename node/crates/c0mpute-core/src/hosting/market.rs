//! Picking paid hosts from the d1sks OpenDisk marketplace.
//!
//! d1sks is discovery only. `c0mpute hosting publish --disks …` asks it once
//! for disks that are c0mpute hosting nodes, ranks them here, and writes the
//! chosen peer ids and the agreed price into the signed site record. From
//! then on placement, proofs, serving and settlement are peer to peer; if
//! d1sks is down the site publishes without preferred hosts and the network
//! keeps its usual three free copies.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use super::site::{HostingTerms, MAX_PREFERRED_HOSTS};
use crate::payments::{Micros, parse_usd};

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// A listing whose last signed heartbeat is older than this ranks last.
pub const FRESH_SECS: u64 = 15 * 60;

/// What to ask d1sks for. `--disks auto` is the empty query; otherwise a
/// d1sks-style query string such as `country=DE,FI&max_price=0.01&min_gib=5`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiskQuery {
    /// ISO 3166-1 alpha-2 codes, upper case; empty = anywhere.
    pub countries: Vec<String>,
    /// Highest hosting price accepted, micro-USD per GiB-month.
    pub max_price: Option<Micros>,
    /// Least free space a disk must report, GiB.
    pub min_gib: Option<u64>,
    /// Least d1sks standing (0-100).
    pub min_standing: Option<u32>,
}

impl DiskQuery {
    /// Parse `auto` or a query string. Unknown keys are an error, so a typo
    /// does not silently widen the search.
    pub fn parse(spec: &str) -> Result<Self> {
        let spec = spec.trim().trim_start_matches('?');
        let mut q = DiskQuery::default();
        if spec.is_empty() || spec == "auto" {
            return Ok(q);
        }
        for pair in spec.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').with_context(|| format!("{pair:?} is not key=value"))?;
            let v = v.trim();
            match k.trim() {
                "country" => q.add_countries(v)?,
                "max_price" => q.max_price = Some(parse_usd(v).with_context(|| format!("max_price {v:?}"))?),
                "min_gib" => q.min_gib = Some(v.parse().with_context(|| format!("min_gib {v:?}"))?),
                "min_standing" => {
                    let s: u32 = v.parse().with_context(|| format!("min_standing {v:?}"))?;
                    if s > 100 {
                        bail!("min_standing is 0-100");
                    }
                    q.min_standing = Some(s);
                }
                other => bail!("unknown d1sks query key {other:?} (country, max_price, min_gib, min_standing)"),
            }
        }
        Ok(q)
    }

    /// Add comma-separated country codes.
    pub fn add_countries(&mut self, list: &str) -> Result<()> {
        for c in list.split(',').map(|c| c.trim()).filter(|c| !c.is_empty()) {
            let c = c.to_ascii_uppercase();
            if c.len() != 2 || !c.chars().all(|ch| ch.is_ascii_uppercase()) {
                bail!("{c:?} is not an ISO country code");
            }
            if !self.countries.contains(&c) {
                self.countries.push(c);
            }
        }
        Ok(())
    }

    /// The d1sks listing URL for this query. `hosting=1` asks d1sks to list
    /// only c0mpute hosting nodes; the answer is filtered here again anyway,
    /// so an older d1sks that ignores the parameter still works.
    pub fn url(&self, base: &str) -> String {
        let mut u = format!("{}/api/v1/disks?hosting=1&sort=price&limit=200", base.trim_end_matches('/'));
        if !self.countries.is_empty() {
            u.push_str(&format!("&country={}", self.countries.join(",")));
        }
        if let Some(p) = self.max_price {
            u.push_str(&format!("&max_price={}", crate::payments::format_usd(p)));
        }
        if let Some(g) = self.min_gib {
            u.push_str(&format!("&min_gib={g}"));
        }
        if let Some(s) = self.min_standing {
            u.push_str(&format!("&min_standing={s}"));
        }
        u
    }
}

/// A c0mpute hosting node as d1sks lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct HostDisk {
    pub peer_id: String,
    pub name: String,
    /// Hosting price, micro-USD per GiB-month.
    pub price: Micros,
    /// Free bytes it offers for hosting.
    pub free_bytes: u64,
    /// d1sks standing 0-100, when it has one.
    pub standing: Option<f64>,
    pub countries: Vec<String>,
    /// How d1sks says the disk is reached: `https`, `direct` or `c0mpute`
    /// (NAT; reached over connections it opens). `None` from older listings.
    pub reachable: Option<String>,
    /// Seconds since the disk's last signed heartbeat, when it pushes them.
    pub heartbeat_age_secs: Option<u64>,
    /// Seen announcing itself as a hosting node on the live hosting gossip
    /// (set by [`mark_live`], never by d1sks).
    pub live: bool,
}

impl HostDisk {
    /// Ranking tier, lower first: live on the hosting gossip; then fresh and
    /// publicly reachable (or an origin-polled listing); then fresh behind
    /// NAT; then a listing whose heartbeat has gone stale.
    pub fn tier(&self) -> u8 {
        if self.live {
            0
        } else if self.heartbeat_age_secs.is_some_and(|a| a > FRESH_SECS) {
            3
        } else if self.reachable.as_deref() == Some("c0mpute") {
            2
        } else {
            1
        }
    }
}

/// Mark the disks whose peer ids announced themselves as hosting nodes on
/// the live hosting gossip (`Hosting::live_hosts`).
pub fn mark_live(disks: &mut [HostDisk], live: &[String]) {
    for d in disks.iter_mut() {
        d.live = live.iter().any(|p| p == &d.peer_id);
    }
}

fn usd_micros(v: &Value) -> Option<Micros> {
    let f = v.as_f64()?;
    if !f.is_finite() || f < 0.0 {
        return None;
    }
    Some((f * crate::payments::MICROS_PER_USD as f64).round() as Micros)
}

/// The c0mpute hosting nodes in a d1sks `/api/v1/disks` answer. A disk
/// counts when its descriptor (`data`) carries a valid `c0mpute.peer_id` and
/// says it hosts (`accepts.hosting: true` or `"hosting"` in `c0mpute.roles`).
pub fn parse_disks(body: &Value) -> Vec<HostDisk> {
    let Some(rows) = body["disks"].as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for row in rows {
        if row["status"].as_str().is_some_and(|s| s != "active") {
            continue;
        }
        let data = &row["data"];
        let c = &data["c0mpute"];
        let Some(peer_id) = c["peer_id"].as_str() else {
            continue;
        };
        if peer_id.len() > 64 || peer_id.parse::<c0mpute_net::PeerId>().is_err() {
            continue;
        }
        let roles_host = c["roles"]
            .as_array()
            .is_some_and(|r| r.iter().any(|x| x.as_str() == Some("hosting")));
        if !(data["accepts"]["hosting"].as_bool() == Some(true) || roles_host) {
            continue;
        }
        let price = usd_micros(&c["hosting"]["price_per_gib_month"])
            .or_else(|| usd_micros(&row["per_gib_month"]))
            .or_else(|| usd_micros(&data["price"]["per_gib_month"]));
        let Some(price) = price else {
            continue;
        };
        let disk_free = row["free_gib"]
            .as_f64()
            .or_else(|| data["capacity"]["free_gib"].as_f64())
            .unwrap_or(0.0);
        let free_gib = match c["hosting"]["max_gib"].as_f64() {
            Some(m) => disk_free.min(m),
            None => disk_free,
        };
        let countries = row["countries"]
            .as_array()
            .or_else(|| data["location"]["countries"].as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_ascii_uppercase()))
                    .collect()
            })
            .unwrap_or_default();
        out.push(HostDisk {
            peer_id: peer_id.to_string(),
            name: row["name"].as_str().or(data["name"].as_str()).unwrap_or("").to_string(),
            price,
            free_bytes: (free_gib.max(0.0) * GIB) as u64,
            standing: row["standing"].as_f64().or_else(|| data["record"]["standing"].as_f64()),
            countries,
            reachable: row["reachable"]
                .as_str()
                .or_else(|| c["reachable"].as_str())
                .filter(|r| matches!(*r, "https" | "direct" | "c0mpute"))
                .map(str::to_string),
            heartbeat_age_secs: row["heartbeat_age_s"].as_f64().filter(|a| *a >= 0.0).map(|a| a as u64),
            live: false,
        });
    }
    out
}

/// Disks that fit the query and the site, best first: by [`HostDisk::tier`]
/// (live on gossip, fresh and reachable, fresh behind NAT, stale), then
/// cheapest, then highest standing (unrated last), then most free space,
/// then peer id, so the same listing always gives the same order.
pub fn rank(disks: &[HostDisk], q: &DiskQuery, site_bytes: u64) -> Vec<HostDisk> {
    let mut v: Vec<HostDisk> = disks
        .iter()
        .filter(|d| q.max_price.is_none_or(|m| d.price <= m))
        .filter(|d| d.free_bytes >= site_bytes)
        .filter(|d| q.min_gib.is_none_or(|g| d.free_bytes as f64 >= g as f64 * GIB))
        .filter(|d| q.min_standing.is_none_or(|s| d.standing.is_some_and(|x| x >= s as f64)))
        .filter(|d| q.countries.is_empty() || d.countries.iter().any(|c| q.countries.contains(c)))
        .cloned()
        .collect();
    v.sort_by(|a, b| {
        a.tier()
            .cmp(&b.tier())
            .then(a.price.cmp(&b.price))
            .then_with(|| {
                let (x, y) = (a.standing.unwrap_or(-1.0), b.standing.unwrap_or(-1.0));
                y.total_cmp(&x)
            })
            .then(b.free_bytes.cmp(&a.free_bytes))
            .then(a.peer_id.cmp(&b.peer_id))
    });
    let mut seen = std::collections::HashSet::new();
    v.retain(|d| seen.insert(d.peer_id.clone()));
    v
}

/// The first `copies` of a ranking (at most [`MAX_PREFERRED_HOSTS`]), and
/// the terms to sign: the agreed price is the highest ask among them, so
/// every chosen host gets at least what it asks.
pub fn choose(ranked: &[HostDisk], copies: usize, payer: Option<String>) -> Option<(Vec<HostDisk>, HostingTerms)> {
    let n = copies.clamp(1, MAX_PREFERRED_HOSTS);
    let chosen: Vec<HostDisk> = ranked.iter().take(n).cloned().collect();
    if chosen.is_empty() {
        return None;
    }
    let price = chosen.iter().map(|d| d.price).max().unwrap_or(0);
    let terms = HostingTerms {
        preferred_hosts: chosen.iter().map(|d| d.peer_id.clone()).collect(),
        price_per_gib_month: price,
        payer,
    };
    Some((chosen, terms))
}

/// Ask d1sks for hosting disks matching `q`.
pub async fn query(d1sks: &str, q: &DiskQuery) -> Result<Vec<HostDisk>> {
    let url = q.url(d1sks);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent(concat!("c0mpute/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let res = client.get(&url).send().await.with_context(|| format!("GET {url}"))?;
    if !res.status().is_success() {
        bail!("{url} answered {}", res.status());
    }
    let body: Value = res.json().await.with_context(|| format!("{url} is not JSON"))?;
    Ok(parse_disks(&body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pid(seed: u8) -> String {
        let kp = c0mpute_net::libp2p::identity::Keypair::ed25519_from_bytes([seed; 32]).unwrap();
        kp.public().to_peer_id().to_base58()
    }

    /// A d1sks `/api/v1/disks` answer: rows as `src/disks.js` returns them,
    /// with the raw descriptor in `data`.
    fn fixture() -> Value {
        serde_json::json!({
            "disks": [
                {   // cheapest hosting node, DE
                    "name": "a", "status": "active", "free_gib": 100.0, "per_gib_month": 0.004,
                    "countries": ["DE"], "standing": 90,
                    "data": { "accepts": { "hosting": true },
                              "c0mpute": { "peer_id": pid(1), "roles": ["storage", "hosting"],
                                           "hosting": { "price_per_gib_month": 0.003, "max_gib": 5.0 } } }
                },
                {   // same price, lower standing, FI
                    "name": "b", "status": "active", "free_gib": 50.0, "per_gib_month": 0.003,
                    "countries": ["FI"], "standing": 40,
                    "data": { "c0mpute": { "peer_id": pid(2), "roles": ["hosting"] } }
                },
                {   // same price, unrated, US
                    "name": "c", "status": "active", "free_gib": 500.0, "per_gib_month": 0.003,
                    "countries": ["US"], "standing": null,
                    "data": { "accepts": { "hosting": true }, "c0mpute": { "peer_id": pid(3) } }
                },
                {   // pricey, DE
                    "name": "d", "status": "active", "free_gib": 900.0, "per_gib_month": 0.02,
                    "countries": ["DE"], "standing": 99,
                    "data": { "accepts": { "hosting": true }, "c0mpute": { "peer_id": pid(4) } }
                },
                {   // storage only: not a hosting node
                    "name": "e", "status": "active", "free_gib": 900.0, "per_gib_month": 0.001,
                    "countries": ["DE"],
                    "data": { "c0mpute": { "peer_id": pid(5), "roles": ["storage"] } }
                },
                {   // not a c0mpute node at all
                    "name": "f", "status": "active", "free_gib": 900.0, "per_gib_month": 0.001,
                    "data": { "accepts": { "hosting": true } }
                },
                {   // bad peer id
                    "name": "g", "status": "active", "free_gib": 900.0, "per_gib_month": 0.001,
                    "data": { "accepts": { "hosting": true }, "c0mpute": { "peer_id": "nope" } }
                },
                {   // gone
                    "name": "h", "status": "gone", "free_gib": 900.0, "per_gib_month": 0.001,
                    "data": { "accepts": { "hosting": true }, "c0mpute": { "peer_id": pid(8) } }
                },
                {   // tiny disk, FI
                    "name": "i", "status": "active", "free_gib": 0.0001, "per_gib_month": 0.001,
                    "countries": ["FI"],
                    "data": { "accepts": { "hosting": true }, "c0mpute": { "peer_id": pid(9) } }
                }
            ],
            "total": 9
        })
    }

    #[test]
    fn query_strings_parse_strictly() {
        assert_eq!(DiskQuery::parse("auto").unwrap(), DiskQuery::default());
        assert_eq!(DiskQuery::parse("").unwrap(), DiskQuery::default());
        let q = DiskQuery::parse("country=de, fi&max_price=0.01&min_gib=5&min_standing=50").unwrap();
        assert_eq!(q.countries, vec!["DE", "FI"]);
        assert_eq!(q.max_price, Some(10_000));
        assert_eq!(q.min_gib, Some(5));
        assert_eq!(q.min_standing, Some(50));
        assert_eq!(
            q.url("https://d1sks.example/"),
            "https://d1sks.example/api/v1/disks?hosting=1&sort=price&limit=200&country=DE,FI&max_price=0.01&min_gib=5&min_standing=50"
        );
        assert!(DiskQuery::parse("country=Germany").is_err());
        assert!(DiskQuery::parse("max_price=cheap").is_err());
        assert!(DiskQuery::parse("colour=blue").is_err(), "unknown keys are refused");
        assert!(DiskQuery::parse("min_standing=101").is_err());
        assert!(DiskQuery::parse("country").is_err());
    }

    #[test]
    fn only_c0mpute_hosting_nodes_are_parsed() {
        let disks = parse_disks(&fixture());
        let names: Vec<&str> = disks.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c", "d", "i"]);
        let a = &disks[0];
        assert_eq!(a.peer_id, pid(1));
        assert_eq!(a.price, 3_000, "the hosting price wins over the storage price");
        assert_eq!(a.free_bytes, 5 * 1024 * 1024 * 1024, "capped at hosting max_gib");
        assert_eq!(a.standing, Some(90.0));
        assert!(parse_disks(&serde_json::json!({"oops": 1})).is_empty());
    }

    #[test]
    fn ranking_is_price_then_standing_then_space() {
        let disks = parse_disks(&fixture());
        let site = 1024 * 1024;
        let order: Vec<String> = rank(&disks, &DiskQuery::default(), site)
            .into_iter()
            .map(|d| d.name)
            .collect();
        // i is too small for the site; a/b/c tie on price, ranked by standing
        // (unrated last); d is the most expensive.
        assert_eq!(order, vec!["a", "b", "c", "d"]);

        let q = DiskQuery::parse("country=DE,FI&max_price=0.01").unwrap();
        let order: Vec<String> = rank(&disks, &q, site).into_iter().map(|d| d.name).collect();
        assert_eq!(order, vec!["a", "b"]);

        let q = DiskQuery::parse("min_standing=50").unwrap();
        let order: Vec<String> = rank(&disks, &q, site).into_iter().map(|d| d.name).collect();
        assert_eq!(order, vec!["a", "d"]);
    }

    #[test]
    fn live_and_fresh_reachable_disks_rank_first() {
        let mut body = fixture();
        let rows = body["disks"].as_array_mut().unwrap();
        // a: cheapest but its heartbeat went stale; b: behind NAT, fresh;
        // c: fresh and directly reachable; d: priciest but live on gossip.
        rows[0]["heartbeat_age_s"] = serde_json::json!(3600);
        rows[0]["reachable"] = serde_json::json!("direct");
        rows[1]["heartbeat_age_s"] = serde_json::json!(60);
        rows[1]["reachable"] = serde_json::json!("c0mpute");
        rows[2]["heartbeat_age_s"] = serde_json::json!(30);
        rows[2]["reachable"] = serde_json::json!("direct");
        let mut disks = parse_disks(&body);
        assert_eq!(disks[1].reachable.as_deref(), Some("c0mpute"));
        assert_eq!(disks[0].heartbeat_age_secs, Some(3600));
        mark_live(&mut disks, &[pid(4)]);
        let order: Vec<String> = rank(&disks, &DiskQuery::default(), 1024)
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(order, vec!["d", "i", "c", "b", "a"]);
    }

    #[test]
    fn choosing_agrees_on_the_highest_ask() {
        let disks = parse_disks(&fixture());
        let ranked = rank(&disks, &DiskQuery::default(), 1024 * 1024);
        let (chosen, terms) = choose(&ranked, 4, None).unwrap();
        assert_eq!(chosen.len(), 4);
        assert_eq!(terms.preferred_hosts[0], pid(1));
        assert_eq!(terms.price_per_gib_month, 20_000, "d asks $0.02, so all four are offered $0.02");
        let (two, terms) = choose(&ranked, 2, None).unwrap();
        assert_eq!(two.len(), 2);
        assert_eq!(terms.price_per_gib_month, 3_000);
        assert_eq!(choose(&ranked, 99, None).unwrap().0.len(), 4, "capped by what exists");
        assert!(choose(&[], 3, None).is_none());
    }
}
