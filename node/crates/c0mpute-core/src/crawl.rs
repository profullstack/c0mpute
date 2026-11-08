//! `crawl.fetch`: fetch one public web page for a buyer, from a worker's
//! own connection. Static fetches only (no JS rendering).
//!
//! The HTTP side is the guarded client `workflow.step` already uses: public
//! addresses only (re-checked on every redirect), capped body, wall-clock
//! timeout, no hop-by-hop or credential headers. On top of that a crawl
//! honours the site's robots.txt unless the buyer says it owns the site.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use c0mpute_net::topics::JobStatus;
use c0mpute_net::{Libp2pNetwork, Network};
use c0mpute_store::ChunkStore;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::net_guard;
use crate::runner::{AcceptedJob, WorkerLoad, publish_receipt};
use crate::workflow::{self, HttpCallConfig};

pub const WORKLOAD: &str = "crawl.fetch";
/// Capability tag: this node fetches pages for buyers.
pub const ROLE_TAG: &str = "c0mpute:role:crawl";
/// User agent robots.txt rules are matched against (besides `*`).
pub const ROBOTS_AGENT: &str = "c0mpute";
/// Concurrent fetches per node. Network-bound, so its own slots.
pub const MAX_CONCURRENT_FETCHES: usize = 8;

/// What a buyer asks for.
#[derive(Debug, Clone, Deserialize)]
pub struct FetchSpec {
    pub v: u32,
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Skip the robots.txt check. For a buyer fetching its own site.
    #[serde(default)]
    pub ignore_robots: bool,
}

pub fn parse_spec(spec: &Value) -> std::result::Result<FetchSpec, String> {
    let s: FetchSpec =
        serde_json::from_value(spec.clone()).map_err(|e| format!("invalid crawl.fetch spec: {e}"))?;
    if s.v != 1 {
        return Err(format!("unsupported crawl.fetch spec version {}", s.v));
    }
    let url = reqwest::Url::parse(&s.url).map_err(|e| format!("invalid url: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("url must be http or https".into());
    }
    Ok(s)
}

/// Run a fetch and return the result blob.
pub async fn execute(spec: &Value) -> Value {
    execute_with(spec, net_guard::is_public_ip).await
}

pub async fn execute_with(spec: &Value, allow: fn(IpAddr) -> bool) -> Value {
    let started = Instant::now();
    let outcome = run(spec, allow).await;
    let duration_ms = started.elapsed().as_millis() as u64;
    let blob = match outcome {
        Ok(page) => json!({ "v": 1, "ok": true, "output": page, "duration_ms": duration_ms }),
        Err(error) => json!({ "v": 1, "ok": false, "error": error, "duration_ms": duration_ms }),
    };
    let size = serde_json::to_vec(&blob).map(|b| b.len()).unwrap_or(usize::MAX);
    if size > workflow::MAX_RESULT_BYTES {
        return json!({
            "v": 1,
            "ok": false,
            "error": format!("result is over the {}-byte limit", workflow::MAX_RESULT_BYTES),
            "duration_ms": duration_ms,
        });
    }
    blob
}

async fn run(spec: &Value, allow: fn(IpAddr) -> bool) -> std::result::Result<Value, String> {
    let spec = parse_spec(spec)?;
    let url = reqwest::Url::parse(&spec.url).map_err(|e| e.to_string())?;
    if !spec.ignore_robots && !robots_allows(&url, allow).await {
        return Err(format!("robots.txt disallows {}", url.path()));
    }
    let page = workflow::http_call(
        HttpCallConfig {
            url: spec.url.clone(),
            method: "GET".into(),
            headers: spec.headers,
            body: None,
            timeout_ms: spec.timeout_ms,
        },
        allow,
    )
    .await
    .map_err(|e| format!("{e:#}"))?;
    let mut page = page;
    page["url"] = Value::String(spec.url);
    Ok(page)
}

/// Fetch and apply the site's robots.txt. A missing or unreadable file
/// allows everything, as the robots convention says.
async fn robots_allows(url: &reqwest::Url, allow: fn(IpAddr) -> bool) -> bool {
    let Ok(robots_url) = url.join("/robots.txt") else {
        return true;
    };
    let res = workflow::http_call(
        HttpCallConfig {
            url: robots_url.to_string(),
            method: "GET".into(),
            headers: BTreeMap::new(),
            body: None,
            timeout_ms: Some(5_000),
        },
        allow,
    )
    .await;
    let Ok(res) = res else { return true };
    if res["status"].as_u64() != Some(200) {
        return true;
    }
    let text = res["body"].as_str().unwrap_or("");
    let path = match url.query() {
        Some(q) => format!("{}?{q}", url.path()),
        None => url.path().to_string(),
    };
    robots_permits(text, ROBOTS_AGENT, &path)
}

/// Longest-match robots.txt evaluation for `agent` (falling back to `*`).
/// `Allow` wins a tie, as Google and RFC 9309 specify.
pub fn robots_permits(robots: &str, agent: &str, path: &str) -> bool {
    let agent = agent.to_ascii_lowercase();
    // (agents in this group, rules)
    let mut groups: Vec<(Vec<String>, Vec<(bool, String)>)> = Vec::new();
    let mut in_rules = false;
    for raw in robots.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim().to_string();
        match key.as_str() {
            "user-agent" => {
                if in_rules || groups.is_empty() {
                    groups.push((Vec::new(), Vec::new()));
                    in_rules = false;
                }
                if let Some(g) = groups.last_mut() {
                    g.0.push(value.to_ascii_lowercase());
                }
            }
            "allow" | "disallow" => {
                in_rules = true;
                if let Some(g) = groups.last_mut() {
                    g.1.push((key == "allow", value));
                }
            }
            _ => {}
        }
    }
    let specific: Vec<&(bool, String)> = groups
        .iter()
        .filter(|(agents, _)| agents.iter().any(|a| a != "*" && agent.contains(a.as_str())))
        .flat_map(|(_, rules)| rules)
        .collect();
    let rules = if specific.is_empty() {
        groups
            .iter()
            .filter(|(agents, _)| agents.iter().any(|a| a == "*"))
            .flat_map(|(_, rules)| rules)
            .collect()
    } else {
        specific
    };
    let mut best: Option<(usize, bool)> = None;
    for (is_allow, pattern) in rules {
        if pattern.is_empty() {
            continue; // "Disallow:" with no value allows everything
        }
        if let Some(len) = pattern_match(pattern, path) {
            let better = match best {
                None => true,
                Some((l, a)) => len > l || (len == l && *is_allow && !a),
            };
            if better {
                best = Some((len, *is_allow));
            }
        }
    }
    best.map(|(_, allow)| allow).unwrap_or(true)
}

/// Match a robots pattern (`*` wildcard, trailing `$` anchor) against the
/// start of `path`; returns the pattern length on a match.
fn pattern_match(pattern: &str, path: &str) -> Option<usize> {
    let (pat, anchored) = match pattern.strip_suffix('$') {
        Some(p) => (p, true),
        None => (pattern, false),
    };
    let parts: Vec<&str> = pat.split('*').collect();
    let mut pos = 0usize;
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            if !path.starts_with(part) {
                return None;
            }
            pos = part.len();
        } else if part.is_empty() {
            continue;
        } else {
            let found = path[pos..].find(part)?;
            pos += found + part.len();
        }
    }
    if anchored {
        let ends_ok = if pat.ends_with('*') { true } else { pos == path.len() };
        if !ends_ok {
            return None;
        }
    }
    Some(pattern.len())
}

// ─── worker runner ────────────────────────────────────────────────────────

pub fn worker_load() -> WorkerLoad {
    WorkerLoad::new(
        Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_FETCHES)),
        MAX_CONCURRENT_FETCHES,
    )
}

pub fn spawn_crawl_runner(
    net: Arc<Libp2pNetwork>,
    store: ChunkStore,
    load: WorkerLoad,
) -> mpsc::Sender<AcceptedJob> {
    let (tx, mut rx) = mpsc::channel::<AcceptedJob>(MAX_CONCURRENT_FETCHES);
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let Ok(slot) = load.compute_slots().acquire_owned().await else {
                return;
            };
            let net = net.clone();
            let store = store.clone();
            let load = load.clone();
            tokio::spawn(async move {
                let _slot = slot;
                let started = Instant::now();
                let job_id = job.accept.job_id.clone();
                match run_fetch_job(&net, &store, &job).await {
                    Ok(hash) => {
                        load.record_job(started.elapsed());
                        info!(%job_id, %hash, "crawl runner: fetch complete");
                        publish_receipt(&net, &job.accept, Some(hash), JobStatus::Completed).await;
                    }
                    Err(error) => {
                        warn!(%job_id, err = %error, "crawl runner: fetch failed");
                        publish_receipt(&net, &job.accept, None, JobStatus::Failed).await;
                    }
                }
            });
        }
    });
    tx
}

async fn run_fetch_job(net: &Libp2pNetwork, store: &ChunkStore, job: &AcceptedJob) -> Result<String> {
    let spec = workflow::resolve_spec(net, job).await?;
    let result = execute(&spec).await;
    let bytes = serde_json::to_vec(&result)?;
    let hash = store.put(&bytes).await.context("store crawl result")?;
    net.announce(&hash).await.context("announce crawl result")?;
    Ok(hash.to_hex())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_validation() {
        assert!(parse_spec(&json!({"v":1,"url":"https://example.com/"})).is_ok());
        assert!(parse_spec(&json!({"v":2,"url":"https://example.com/"})).unwrap_err().contains("version"));
        assert!(parse_spec(&json!({"v":1,"url":"ftp://example.com/"})).unwrap_err().contains("http"));
        assert!(parse_spec(&json!({"v":1})).is_err());
    }

    #[test]
    fn robots_rules() {
        let robots = "User-agent: *\nDisallow: /private\nAllow: /private/ok\n\nUser-agent: badbot\nDisallow: /\n";
        assert!(robots_permits(robots, ROBOTS_AGENT, "/"));
        assert!(!robots_permits(robots, ROBOTS_AGENT, "/private/x"));
        assert!(robots_permits(robots, ROBOTS_AGENT, "/private/ok/y"));
        assert!(!robots_permits(robots, "badbot", "/anything"));
        assert!(robots_permits("User-agent: *\nDisallow:\n", ROBOTS_AGENT, "/x"));
        assert!(!robots_permits("User-agent: c0mpute\nDisallow: /\n", ROBOTS_AGENT, "/x"));
        assert!(!robots_permits("User-agent: *\nDisallow: /*.pdf$\n", ROBOTS_AGENT, "/a/b.pdf"));
        assert!(robots_permits("User-agent: *\nDisallow: /*.pdf$\n", ROBOTS_AGENT, "/a/b.pdf?x"));
        assert!(robots_permits("", ROBOTS_AGENT, "/x"));
    }

    #[tokio::test]
    async fn refuses_private_addresses() {
        let r = execute(&json!({"v":1,"url":"http://127.0.0.1:1/","ignore_robots":true})).await;
        assert_eq!(r["ok"], false, "{r}");
    }
}
