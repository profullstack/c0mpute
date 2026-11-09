//! Custom-domain proof. A gateway serves `example.org` for a site only when
//! `TXT _c0mpute-hosting.example.org` contains `site=<site_id>`: the record
//! naming the domain is signed by the site owner, and the TXT shows the
//! domain owner agrees. Neither alone can hijack a domain.
//!
//! Lookups go over DNS-over-HTTPS to two public resolvers (whichever answers),
//! and are cached: confirmations for 10 minutes, refusals for 2.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;

pub const TXT_PREFIX: &str = "_c0mpute-hosting";
const POSITIVE_TTL: Duration = Duration::from_secs(600);
const NEGATIVE_TTL: Duration = Duration::from_secs(120);

pub const DEFAULT_RESOLVERS: &[&str] = &[
    "https://cloudflare-dns.com/dns-query",
    "https://dns.google/resolve",
];

#[derive(Clone)]
pub struct Verifier {
    resolvers: Arc<Vec<String>>,
    cache: Arc<RwLock<HashMap<(String, String), (bool, Instant)>>>,
    /// Tests: answer from this table instead of the network.
    fixed: Option<Arc<HashMap<String, Vec<String>>>>,
}

impl Default for Verifier {
    fn default() -> Self {
        Self {
            resolvers: Arc::new(DEFAULT_RESOLVERS.iter().map(|s| s.to_string()).collect()),
            cache: Arc::new(RwLock::new(HashMap::new())),
            fixed: None,
        }
    }
}

impl Verifier {
    /// A verifier that answers TXT lookups from `table` (name -> strings).
    pub fn fixed(table: HashMap<String, Vec<String>>) -> Self {
        Self {
            fixed: Some(Arc::new(table)),
            ..Self::default()
        }
    }

    /// Does DNS say `domain` belongs to `site_id`?
    pub async fn confirms(&self, domain: &str, site_id: &str) -> bool {
        let key = (domain.to_string(), site_id.to_string());
        if let Some((ok, at)) = self.cache.read().await.get(&key).copied() {
            let ttl = if ok { POSITIVE_TTL } else { NEGATIVE_TTL };
            if at.elapsed() < ttl {
                return ok;
            }
        }
        let name = format!("{TXT_PREFIX}.{domain}");
        let txts = match &self.fixed {
            Some(t) => t.get(&name).cloned().unwrap_or_default(),
            None => self.lookup_txt(&name).await,
        };
        let ok = txts.iter().any(|t| txt_names_site(t, site_id));
        self.cache.write().await.insert(key, (ok, Instant::now()));
        ok
    }

    async fn lookup_txt(&self, name: &str) -> Vec<String> {
        let Ok(client) = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .user_agent(concat!("c0mpute/", env!("CARGO_PKG_VERSION")))
            .build()
        else {
            return Vec::new();
        };
        for r in self.resolvers.iter() {
            let res = client
                .get(r)
                .query(&[("name", name), ("type", "TXT")])
                .header("accept", "application/dns-json")
                .send()
                .await;
            let Ok(res) = res else { continue };
            let Ok(v) = res.json::<serde_json::Value>().await else {
                continue;
            };
            return parse_doh_txt(&v);
        }
        Vec::new()
    }
}

/// TXT strings from a DNS-over-HTTPS JSON answer (type 16 records).
pub fn parse_doh_txt(v: &serde_json::Value) -> Vec<String> {
    v.get("Answer")
        .and_then(|a| a.as_array())
        .map(|answers| {
            answers
                .iter()
                .filter(|a| a.get("type").and_then(|t| t.as_u64()) == Some(16))
                .filter_map(|a| a.get("data").and_then(|d| d.as_str()))
                // A long TXT arrives as several quoted strings; join them.
                .map(|d| d.split('"').filter(|s| !s.trim().is_empty()).collect::<String>())
                .collect()
        })
        .unwrap_or_default()
}

/// `site=<id>` among the space-separated tokens of a TXT string.
pub fn txt_names_site(txt: &str, site_id: &str) -> bool {
    txt.split_whitespace()
        .any(|tok| tok.strip_prefix("site=") == Some(site_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_doh_answers() {
        let v = serde_json::json!({"Status":0,"Answer":[
            {"name":"_c0mpute-hosting.example.org.","type":16,"TTL":300,"data":"\"site=abc123 v=1\""},
            {"name":"x","type":5,"data":"cname.example."}
        ]});
        let txts = parse_doh_txt(&v);
        assert_eq!(txts, vec!["site=abc123 v=1".to_string()]);
        assert!(txt_names_site(&txts[0], "abc123"));
        assert!(!txt_names_site(&txts[0], "abc12"));
    }

    #[tokio::test]
    async fn fixed_table_confirms_only_the_named_site() {
        let mut t = HashMap::new();
        t.insert("_c0mpute-hosting.blog.example".to_string(), vec!["site=s1".to_string()]);
        let v = Verifier::fixed(t);
        assert!(v.confirms("blog.example", "s1").await);
        assert!(!v.confirms("blog.example", "s2").await);
        assert!(!v.confirms("other.example", "s1").await);
    }
}
