//! Status-aggregator mode (DIP-0014).
//!
//! A **read-only observer** node. It boots a normal c0mpute libp2p node
//! (Kad-DHT + gossipsub) but advertises no roles — it never poses as a
//! worker. It then:
//!
//!   1. Runs the capability [`Registry`] against `c0mpute/cap/v1` to count
//!      workers online, by role, and by capability tag. A worker re-publishes
//!      its capability ad every 60s; ads older than `MAX_AD_AGE` (5min) are
//!      evicted, so the ad doubles as a liveness signal.
//!   2. Subscribes to the per-workload job topics (`c0mpute/jobs/<type>`) and
//!      correlates `JobAccept` → `JobReceipt` by `job_id` to count jobs in
//!      flight, jobs completed in the last 24h, and average latency.
//!   3. Periodically walks the DHT keyspace with `kad_find_node(random_key)`
//!      (bitmagnet-style) to warm the routing table + gossipsub mesh, so we
//!      hear from peers we'd otherwise miss.
//!   4. Serves the aggregate JSON over HTTP at `GET /` (plus `/healthz`).
//!
//! **No private data is ever exposed** — only aggregate counts. See
//! `dips/0014-status-aggregator.md` for the privacy model and the frozen
//! JSON contract. The output shape matches `apps/web/src/lib/status.ts`
//! (`StatusPayload` minus `source`, which the web layer overwrites).
//!
//! State is in-memory only. On restart the 24h counters reconstruct from the
//! live network within a few minutes — acceptable for a real-time status
//! surface (there is no database, per DIP-0011).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use axum::{Json, Router, extract::State, routing::get};
use c0mpute_net::topics::{HEARTBEAT_TOPIC, JobAccept, JobReceipt, JobStatus, job_topic};
use c0mpute_net::{GossipMessage, Libp2pNetwork, Multiaddr, NetworkConfig, bootstrap};
use serde::Serialize;
use tokio::sync::{RwLock, broadcast};
use tracing::{debug, info, warn};

use crate::capabilities::Registry;
use crate::config;

/// Completed jobs are counted over a rolling 24h window.
const COMPLETED_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

/// A job we've seen accepted but not yet receipted is dropped after this
/// long, so a lost/never-arriving receipt doesn't leak an in-flight entry
/// forever.
const MAX_IN_FLIGHT_AGE: Duration = Duration::from_secs(60 * 60);

/// How often to run a random-key DHT walk to warm discovery.
const DHT_CRAWL_INTERVAL: Duration = Duration::from_secs(30);

/// Workload job topics we subscribe to. Gossipsub can't wildcard, so we
/// enumerate the known workload types. `ffmpeg.transcode` is emitted by the
/// worker daemon; `infernet.inference` by workers serving their
/// local OpenAI-compatible runtime. Both always appear in the output (as zeros when idle) to mirror
/// the page's stub shape.
// The temporal workloads (DIP 0026) are listed so seeds running the
// aggregator also relay them for buyers that only know the seeds; their
// session calls are sealed, so the aggregator sees only ciphertext.
const KNOWN_WORKLOADS: &[&str] = &[
    "ffmpeg.transcode",
    "infernet.inference",
    "infernet.temporal.train",
    "infernet.temporal.infer",
    // DIP 0030: webhook workflow steps (meshhook via `c0mpute jobs serve`).
    "workflow.step",
    // CPU speech and images, plus GPU video. Seeds must be in these topics'
    // meshes too: a buyer gateway that only knows the seeds otherwise gets
    // NoPeersSubscribedToTopic and never reaches a remote worker.
    "tts.synthesize",
    "stt.transcribe",
    "diffusion.txt2img",
    "music.txt2music",
    "video.txt2vid",
    "video.img2vid",
];

/// Roles we always surface (even at zero) so the "workers by role" table
/// stays populated. Extra roles observed on the wire are added dynamically.
const KNOWN_ROLES: &[&str] = &["storage", "transcode", "gateway", "verifier"];

/// Capability tags we always surface (even at zero) to keep the "capability
/// tags" table populated. Any other advertised tag is added dynamically.
const KNOWN_TAGS: &[&str] = &[
    "c0mpute:role:storage",
    "c0mpute:role:transcode",
    "c0mpute:role:gateway",
    "c0mpute:role:verifier",
    "c0mpute:gpu:nvidia",
    "c0mpute:gpu:amd",
    "c0mpute:gpu:apple",
    "c0mpute:cpu",
];

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ────────────────────────────────────────────────────────────────────────
// JSON contract (mirrors apps/web/src/lib/status.ts)
// ────────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct StatusPayload {
    ok: bool,
    generated_at: String,
    network: NetworkStatus,
    /// Always `"aggregator"` here. The web layer overwrites this field, but
    /// direct consumers (`curl`, community operators) rely on it.
    source: &'static str,
}

#[derive(Serialize)]
struct NetworkStatus {
    workers_online: u64,
    workers_with_role: BTreeMap<String, u64>,
    workers_with_tag: BTreeMap<String, u64>,
    jobs_in_flight: u64,
    jobs_completed_24h: u64,
    avg_job_latency_seconds: Option<f64>,
    workload_types: BTreeMap<String, WorkloadStats>,
    /// `c0mpute bench` scores across online workers (see c0mpute-bench).
    bench: BenchSummary,
    /// Canary auctions over the last hour, per workload ([`crate::canary`]):
    /// how many probes got a bid, overall and while a capable node was
    /// advertised.
    auction_canary: BTreeMap<String, crate::auction_stats::Stats>,
}

#[derive(Serialize, Default, Debug, PartialEq)]
pub(crate) struct BenchSummary {
    /// Workers whose ad carries a score.
    workers_benchmarked: u64,
    /// Sum of scores — the network's total measured capacity in
    /// reference-core thousandths.
    total_score: u64,
    median_score: Option<u32>,
    max_score: Option<u32>,
}

impl BenchSummary {
    pub(crate) fn from_scores(mut scores: Vec<u32>) -> Self {
        if scores.is_empty() {
            return Self::default();
        }
        scores.sort_unstable();
        Self {
            workers_benchmarked: scores.len() as u64,
            total_score: scores.iter().map(|&s| s as u64).sum(),
            median_score: Some(scores[scores.len() / 2]),
            max_score: scores.last().copied(),
        }
    }
}

#[derive(Serialize)]
struct WorkloadStats {
    jobs_in_flight: u64,
    jobs_completed_24h: u64,
    avg_latency_seconds: Option<f64>,
}

// ────────────────────────────────────────────────────────────────────────
// Job tracking
// ────────────────────────────────────────────────────────────────────────

struct InFlight {
    workload_type: String,
    accepted_at_ms: u64,
}

struct Completed {
    workload_type: String,
    completed_at_ms: u64,
    latency_s: Option<f64>,
}

/// In-memory job state, correlated by `job_id`.
#[derive(Default)]
struct JobState {
    in_flight: RwLock<HashMap<String, InFlight>>,
    completed: RwLock<VecDeque<Completed>>,
}

impl JobState {
    /// A buyer awarded a job — start tracking it as in-flight.
    async fn on_accept(&self, a: JobAccept) {
        self.in_flight.write().await.insert(
            a.job_id,
            InFlight {
                workload_type: a.workload_type,
                accepted_at_ms: a.published_at_ms,
            },
        );
    }

    /// A worker published a completion receipt. Any terminal receipt clears
    /// the in-flight entry; only `Completed` counts toward the 24h tally.
    async fn on_receipt(&self, r: JobReceipt) {
        let started = self.in_flight.write().await.remove(&r.job_id);
        if r.status != JobStatus::Completed {
            return;
        }
        // Latency needs the accept timestamp; if we never saw the accept
        // (aggregator started mid-job) we still count the completion but
        // can't attribute a latency or an exact workload type.
        let latency_s = started.as_ref().and_then(|inf| {
            r.completed_at_ms
                .checked_sub(inf.accepted_at_ms)
                .map(|d| d as f64 / 1000.0)
        });
        let workload_type = started
            .map(|inf| inf.workload_type)
            .unwrap_or_else(|| "unknown".to_string());
        self.completed.write().await.push_back(Completed {
            workload_type,
            completed_at_ms: r.completed_at_ms,
            latency_s,
        });
    }

    /// Drop stale in-flight entries and completions outside the 24h window.
    async fn evict(&self, now: u64) {
        let max_age = MAX_IN_FLIGHT_AGE.as_millis() as u64;
        self.in_flight
            .write()
            .await
            .retain(|_, inf| now.saturating_sub(inf.accepted_at_ms) <= max_age);

        let cutoff = now.saturating_sub(COMPLETED_WINDOW.as_millis() as u64);
        let mut completed = self.completed.write().await;
        while completed
            .front()
            .is_some_and(|c| c.completed_at_ms < cutoff)
        {
            completed.pop_front();
        }
    }
}

// ────────────────────────────────────────────────────────────────────────
// HTTP surface
// ────────────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct AppState {
    registry: Registry,
    jobs: Arc<JobState>,
}

async fn build_payload(state: &AppState) -> StatusPayload {
    let now = now_ms();
    state.jobs.evict(now).await;

    // Workers, roles, tags — from the capability registry.
    let ads = state.registry.list().await;
    let workers_online = ads.len() as u64;

    let mut workers_with_role: BTreeMap<String, u64> =
        KNOWN_ROLES.iter().map(|r| (r.to_string(), 0)).collect();
    let mut workers_with_tag: BTreeMap<String, u64> =
        KNOWN_TAGS.iter().map(|t| (t.to_string(), 0)).collect();
    for (_peer, ad) in &ads {
        for tag in &ad.tags {
            *workers_with_tag.entry(tag.clone()).or_insert(0) += 1;
            if let Some(role) = tag.strip_prefix("c0mpute:role:") {
                *workers_with_role.entry(role.to_string()).or_insert(0) += 1;
            }
        }
    }
    let bench =
        BenchSummary::from_scores(ads.iter().filter_map(|(_, ad)| ad.bench_score).collect());

    // Jobs — from the correlated job state.
    let mut workload_types: BTreeMap<String, (u64, u64, f64, u64)> = KNOWN_WORKLOADS
        .iter()
        .map(|w| (w.to_string(), (0, 0, 0.0, 0)))
        .collect();

    let in_flight = state.jobs.in_flight.read().await;
    let jobs_in_flight = in_flight.len() as u64;
    for inf in in_flight.values() {
        workload_types
            .entry(inf.workload_type.clone())
            .or_insert((0, 0, 0.0, 0))
            .0 += 1;
    }
    drop(in_flight);

    let completed = state.jobs.completed.read().await;
    let jobs_completed_24h = completed.len() as u64;
    let mut lat_sum = 0.0;
    let mut lat_n: u64 = 0;
    for c in completed.iter() {
        let e = workload_types
            .entry(c.workload_type.clone())
            .or_insert((0, 0, 0.0, 0));
        e.1 += 1;
        if let Some(l) = c.latency_s {
            e.2 += l;
            e.3 += 1;
            lat_sum += l;
            lat_n += 1;
        }
    }
    drop(completed);

    let workload_types = workload_types
        .into_iter()
        .map(|(k, (inflight, done, lsum, ln))| {
            (
                k,
                WorkloadStats {
                    jobs_in_flight: inflight,
                    jobs_completed_24h: done,
                    avg_latency_seconds: (ln > 0).then(|| lsum / ln as f64),
                },
            )
        })
        .collect();

    let avg_job_latency_seconds = (lat_n > 0).then(|| lat_sum / lat_n as f64);

    StatusPayload {
        ok: true,
        generated_at: iso8601_utc(now),
        network: NetworkStatus {
            workers_online,
            workers_with_role,
            workers_with_tag,
            jobs_in_flight,
            jobs_completed_24h,
            avg_job_latency_seconds,
            workload_types,
            bench,
            auction_canary: crate::auction_stats::snapshot(crate::auction_stats::Kind::Probe),
        },
        source: "aggregator",
    }
}

#[cfg(test)]
mod bench_summary_tests {
    use super::BenchSummary;

    #[test]
    fn empty_network_has_no_scores() {
        assert_eq!(BenchSummary::from_scores(vec![]), BenchSummary::default());
    }

    #[test]
    fn summary_counts_sums_and_picks_median_and_max() {
        let s = BenchSummary::from_scores(vec![4000, 900, 12_000]);
        assert_eq!(s.workers_benchmarked, 3);
        assert_eq!(s.total_score, 16_900);
        assert_eq!(s.median_score, Some(4000));
        assert_eq!(s.max_score, Some(12_000));
    }
}

async fn status_handler(State(state): State<AppState>) -> Json<StatusPayload> {
    Json(build_payload(&state).await)
}

/// One row of the public node board (`GET /nodes`).
///
/// Built only from fields of the capability ad, which every worker already
/// gossips in the clear: a shortened peer id, hardware class, core count,
/// benchmark score/details, version and optional referral handle. No address,
/// hostname, region or CPU model string: the exact model ("AMD Ryzen 9
/// 7950X3D 16-Core Processor") fingerprints an operator's machine, so the
/// public board never carries it (rule since 2026-10-21).
#[derive(Serialize, Debug, PartialEq)]
struct NodeRow {
    id: String,
    class: String,
    cores: Option<u64>,
    ram_bytes: Option<u64>,
    bench_score: Option<u32>,
    bench_at: Option<String>,
    benchmarks: Option<BTreeMap<String, WorkloadBenchmark>>,
    version: Option<String>,
    #[serde(rename = "ref")]
    referral: Option<String>,
    /// The operator's nickname (`c0mpute name`), if the node sets one.
    name: Option<String>,
    /// The operator's listing (`mode`, `min_usd`, `fixed_usd`), if the node
    /// advertises one. Re-validated here: only known fields, sane numbers.
    pricing: Option<serde_json::Value>,
    /// What the node offers as storage (`free_bytes`, `total_bytes`,
    /// `per_gib_month`), when it runs the storage role. Numbers only.
    storage: Option<serde_json::Value>,
    /// The public origin of the node's gateway (`http://host:7777`), only
    /// when its operator announced one. Opt-in, so it does not break the rule
    /// above: the operator published this address themselves. Re-validated to
    /// a bare http(s) origin. Clients that are not libp2p peers use it to find
    /// gateways that serve `/s/<site>/`.
    gateway: Option<String>,
    /// Projects this node donates idle compute to (`crowdgpt`), from its
    /// `c0mpute:donate:<project>` tags; only known projects are passed on.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    donates: Vec<String>,
    last_seen_s: u64,
}

/// Known donation projects a node's tags name.
fn donates(tags: &[String]) -> Vec<String> {
    const KNOWN: [&str; 1] = ["crowdgpt"];
    KNOWN.iter().filter(|p| tags.iter().any(|t| t.strip_prefix("c0mpute:donate:") == Some(**p))).map(|p| p.to_string()).collect()
}

#[derive(Serialize, Debug, PartialEq)]
struct WorkloadBenchmark {
    params: String,
    samples: Vec<BenchmarkSample>,
}

#[derive(Serialize, Debug, PartialEq)]
struct BenchmarkSample {
    threads: u64,
    duration_us: u64,
    speedup: f64,
    throughput: f64,
}

/// Parse a bounded, allowlisted subset of an operator's full bench report.
/// This keeps malformed or unexpectedly large capability ads off the page.
fn benchmark_workloads(hw: &serde_json::Value) -> Option<BTreeMap<String, WorkloadBenchmark>> {
    let workloads = hw.get("benchmark")?.get("workloads")?.as_object()?;
    let mut parsed = BTreeMap::new();
    for name in c0mpute_bench::WORKLOADS {
        let Some(workload) = workloads.get(name) else {
            continue;
        };
        let params: String = workload.get("params")?.as_str()?.chars().take(80).collect();
        let Some(samples) = workload.get("samples")?.as_array() else {
            continue;
        };
        let samples: Vec<BenchmarkSample> = samples
            .iter()
            .take(16)
            .filter_map(|sample| {
                let threads = sample.get("threads")?.as_u64()?;
                let duration_us = sample.get("duration_us")?.as_u64()?;
                let speedup = sample.get("speedup")?.as_f64()?;
                let throughput = sample.get("throughput")?.as_f64()?;
                (threads > 0
                    && threads <= 4096
                    && duration_us > 0
                    && speedup.is_finite()
                    && speedup > 0.0
                    && throughput.is_finite()
                    && throughput > 0.0)
                    .then_some(BenchmarkSample {
                        threads,
                        duration_us,
                        speedup,
                        throughput,
                    })
            })
            .collect();
        if !samples.is_empty() {
            parsed.insert(name.to_string(), WorkloadBenchmark { params, samples });
        }
    }
    (!parsed.is_empty()).then_some(parsed)
}

/// The ad's pricing listing, rebuilt from validated fields only.
fn listing(hw: &serde_json::Value) -> Option<serde_json::Value> {
    let v = hw.get("pricing")?;
    let p: crate::pricing::PricingConfig = serde_json::from_value(v.clone()).ok()?;
    let cap = |x: f64| x <= 1_000_000.0;
    (p.validate().is_ok() && cap(p.min_usd) && p.fixed_usd.is_none_or(cap)).then(|| p.listing())
}

#[derive(Serialize, Debug, PartialEq)]
struct ReferrerRow {
    #[serde(rename = "ref")]
    referral: String,
    nodes: u64,
}

#[derive(Serialize, Debug, PartialEq)]
struct OperatorRow {
    name: String,
    nodes: u64,
}

#[derive(Serialize)]
struct NodesPayload {
    generated_at: String,
    workers_online: u64,
    nodes: Vec<NodeRow>,
    operators: Vec<OperatorRow>,
    referrers: Vec<ReferrerRow>,
}

/// Coarse class from the ad's tags: the first GPU vendor, else `cpu`.
fn node_class(tags: &[String]) -> String {
    for v in ["nvidia", "amd", "apple"] {
        if tags.iter().any(|t| t == &format!("c0mpute:gpu:{v}")) {
            return format!("gpu:{v}");
        }
    }
    "cpu".to_string()
}

/// The last 8 characters of the base58 peer id; enough to tell rows apart.
fn short_id(peer_id: &str) -> String {
    let n = peer_id.len();
    peer_id[n.saturating_sub(8)..].to_string()
}

/// The storage offer a node advertises, re-validated: only the three known
/// fields, finite and non-negative, so a node cannot inject anything else.
fn storage_offer(hardware: &serde_json::Value) -> Option<serde_json::Value> {
    let s = hardware.get("storage")?;
    let free = s.get("free_bytes")?.as_u64()?;
    let total = s.get("total_bytes")?.as_u64()?;
    let price = s.get("per_gib_month")?.as_f64()?;
    if !price.is_finite() || price < 0.0 || price > 1_000.0 {
        return None;
    }
    Some(serde_json::json!({
        "free_bytes": free.min(total),
        "total_bytes": total,
        "per_gib_month": price,
    }))
}

fn build_nodes(ads: &[(String, c0mpute_net::topics::CapabilityAd)], now: u64) -> NodesPayload {
    let mut nodes: Vec<NodeRow> = ads
        .iter()
        .map(|(peer, ad)| NodeRow {
            id: short_id(peer),
            class: node_class(&ad.tags),
            cores: ad.hardware.get("cores").and_then(|v| v.as_u64()),
            ram_bytes: ad.hardware.get("ram_bytes").and_then(|v| v.as_u64()),
            bench_score: ad.bench_score,
            bench_at: ad
                .hardware
                .get("bench_at")
                .and_then(|v| v.as_str())
                .map(|s| s.chars().take(40).collect()),
            benchmarks: benchmark_workloads(&ad.hardware),
            version: ad
                .hardware
                .get("version")
                .and_then(|v| v.as_str())
                .map(|s| s.chars().take(24).collect()),
            referral: ad
                .hardware
                .get("ref")
                .and_then(|v| v.as_str())
                .and_then(crate::capabilities::sanitize_ref),
            name: ad
                .hardware
                .get("name")
                .and_then(|v| v.as_str())
                .and_then(crate::capabilities::sanitize_ref),
            pricing: listing(&ad.hardware),
            storage: storage_offer(&ad.hardware),
            gateway: ad
                .hardware
                .get("gateway")
                .and_then(|v| v.as_str())
                .and_then(crate::capabilities::gateway_origin),
            donates: donates(&ad.tags),
            last_seen_s: now.saturating_sub(ad.published_at_ms) / 1000,
        })
        .collect();
    // Benchmarked nodes first, best score first; then by id for a stable order.
    nodes.sort_by(|a, b| b.bench_score.cmp(&a.bench_score).then(a.id.cmp(&b.id)));

    let mut by_ref: BTreeMap<String, u64> = BTreeMap::new();
    for n in &nodes {
        if let Some(r) = &n.referral {
            *by_ref.entry(r.clone()).or_insert(0) += 1;
        }
    }
    let mut referrers: Vec<ReferrerRow> = by_ref
        .into_iter()
        .map(|(referral, nodes)| ReferrerRow { referral, nodes })
        .collect();
    referrers.sort_by(|a, b| b.nodes.cmp(&a.nodes).then(a.referral.cmp(&b.referral)));

    let mut by_name: BTreeMap<String, u64> = BTreeMap::new();
    for n in &nodes {
        if let Some(name) = &n.name {
            *by_name.entry(name.clone()).or_insert(0) += 1;
        }
    }
    let mut operators: Vec<OperatorRow> = by_name
        .into_iter()
        .map(|(name, nodes)| OperatorRow { name, nodes })
        .collect();
    operators.sort_by(|a, b| b.nodes.cmp(&a.nodes).then(a.name.cmp(&b.name)));

    NodesPayload {
        generated_at: iso8601_utc(now),
        workers_online: nodes.len() as u64,
        nodes,
        operators,
        referrers,
    }
}

async fn nodes_handler(State(state): State<AppState>) -> Json<NodesPayload> {
    let ads: Vec<(String, c0mpute_net::topics::CapabilityAd)> = state
        .registry
        .list()
        .await
        .into_iter()
        .map(|(p, a)| (p.to_base58(), a))
        .collect();
    Json(build_nodes(&ads, now_ms()))
}

async fn healthz() -> &'static str {
    "ok"
}

// ────────────────────────────────────────────────────────────────────────
// Background tasks
// ────────────────────────────────────────────────────────────────────────

/// Subscribe to the job topics and fold `JobAccept`/`JobReceipt` messages
/// into the job state. All four job message types share a topic; we
/// distinguish them by structural JSON parse (their required fields differ).
async fn run_job_tracker(net: Arc<Libp2pNetwork>, jobs: Arc<JobState>) {
    for w in KNOWN_WORKLOADS {
        let topic = job_topic(w);
        if let Err(e) = net.subscribe(&topic).await {
            warn!(err = %e, %topic, "aggregator: job topic subscribe failed");
        } else {
            info!(%topic, "aggregator: subscribed to job topic");
        }
    }
    // Heartbeat is defined but unused today; subscribe defensively so we're
    // already in the mesh if a heartbeat publisher ever ships.
    let _ = net.subscribe(HEARTBEAT_TOPIC).await;

    let mut rx = net.messages();
    loop {
        let msg: GossipMessage = match rx.recv().await {
            Ok(m) => m,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!(skipped = n, "aggregator: job stream lagged");
                continue;
            }
            Err(_) => {
                info!("aggregator: gossip stream closed");
                return;
            }
        };
        if !msg.topic.starts_with("c0mpute/jobs/") {
            continue;
        }
        // Accept before receipt: an accept carries `winning_bidder_peer_id`
        // (absent from every other job message), so it parses unambiguously.
        if let Ok(accept) = serde_json::from_slice::<JobAccept>(&msg.data) {
            debug!(job_id = %accept.job_id, "aggregator: job accepted");
            jobs.on_accept(accept).await;
            continue;
        }
        if let Ok(receipt) = serde_json::from_slice::<JobReceipt>(&msg.data) {
            debug!(job_id = %receipt.job_id, status = ?receipt.status, "aggregator: job receipt");
            jobs.on_receipt(receipt).await;
            continue;
        }
        // JobOffer / JobBid are ignored — they don't mark work as in flight.
    }
}

/// Periodically walk the DHT keyspace toward a random key. `kad_find_node`
/// is fire-and-forget: results land in the routing table (and, transitively,
/// warm the gossipsub mesh), surfacing peers we'd never hear from on
/// gossipsub alone. Bitmagnet-style.
async fn run_dht_crawl(net: Arc<Libp2pNetwork>) {
    let mut ticker = tokio::time::interval(DHT_CRAWL_INTERVAL);
    loop {
        ticker.tick().await;
        let key: [u8; 32] = rand::random();
        if let Err(e) = net.kad_find_node(key.to_vec()).await {
            debug!(err = %e, "aggregator: dht crawl find_node failed");
        }
    }
}

// ────────────────────────────────────────────────────────────────────────
// Entry point
// ────────────────────────────────────────────────────────────────────────

/// Boot the observer node and serve the aggregate status JSON on `bind`.
/// Runs until the process is killed.
pub async fn run(bind: SocketAddr) -> Result<()> {
    // Persistent identity (stable peer-id) at ~/.config/c0mpute/identity.key.
    let identity_dir = config::config_dir().unwrap_or_else(|| PathBuf::from("."));

    // Best-effort bootstrap from c0mpute.com/bootstrap.json; mDNS covers LAN.
    let bootstrap_addrs =
        bootstrap::fetch_or_fallback(bootstrap::DEFAULT_BOOTSTRAP_URL, bootstrap::default_seeds())
            .await;
    info!(
        count = bootstrap_addrs.len(),
        "aggregator: bootstrap peers loaded"
    );

    // Observer node: no local chunk source, no roles → never advertises as a
    // worker (the advertise loop is simply not spawned).
    let mut net_cfg = NetworkConfig::for_dir(identity_dir).with_bootstrap(bootstrap_addrs);

    // Seed mode (DIP-0010): when C0MPUTE_P2P_PORT is set, listen on that fixed
    // TCP port instead of a random one, so a Railway TCP proxy (or any stable
    // public address) can front it and the aggregator can be published as a
    // bootstrap seed. Paired with a persistent identity volume for a stable
    // peer-id, this lets the aggregator be the network's first reachable peer —
    // workers that bootstrap to it join the mesh and get counted.
    if let Ok(raw) = std::env::var("C0MPUTE_P2P_PORT") {
        match raw.parse::<u16>() {
            Ok(port) => match format!("/ip4/0.0.0.0/tcp/{port}").parse::<Multiaddr>() {
                Ok(addr) => {
                    net_cfg = net_cfg.with_listen(vec![addr]);
                    info!(port, "aggregator: seed mode — libp2p on fixed p2p port");
                }
                Err(e) => warn!(err = %e, "invalid C0MPUTE_P2P_PORT addr; using random port"),
            },
            Err(e) => warn!(err = %e, raw, "C0MPUTE_P2P_PORT not a u16; using random port"),
        }
    }

    let net: Arc<Libp2pNetwork> = Arc::new(Libp2pNetwork::spawn(net_cfg).await?);
    info!(peer_id = %net.peer_id(), "aggregator: libp2p network up");

    // Worker/role/tag counting via the shared capability registry.
    let registry = Registry::new();
    registry.run(net.clone());

    // Job counting + DHT crawl.
    let jobs = Arc::new(JobState::default());
    tokio::spawn(run_job_tracker(net.clone(), jobs.clone()));
    tokio::spawn(run_dht_crawl(net.clone()));

    // One tiny offer per workload every few minutes, bids counted, nothing
    // accepted: the auction success rate on the status page.
    if let Some(interval) = crate::canary::interval_from_env() {
        tokio::spawn(crate::canary::run(net.clone(), registry.clone(), interval));
    }

    let state = AppState { registry, jobs };
    let app = Router::new()
        .route("/", get(status_handler))
        .route("/nodes", get(nodes_handler))
        .route("/healthz", get(healthz))
        .with_state(state);

    info!(%bind, "aggregator: HTTP status endpoint listening");
    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

// ────────────────────────────────────────────────────────────────────────
// Time formatting (dependency-free RFC-3339 / ISO-8601 UTC)
// ────────────────────────────────────────────────────────────────────────

/// Format unix-ms as an ISO-8601 UTC timestamp, e.g. `2026-07-12T19:42:00Z`.
/// Uses Howard Hinnant's civil-from-days algorithm so we don't pull in chrono.
fn iso8601_utc(unix_ms: u64) -> String {
    let secs = (unix_ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    // days since 1970-01-01 → civil (y, m, d)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn donation_tags_show_only_known_projects() {
        let tags = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(donates(&tags(&["c0mpute:role:tts", "c0mpute:donate:crowdgpt"])), vec!["crowdgpt".to_string()]);
        assert!(donates(&tags(&["c0mpute:donate:<script>", "c0mpute:role:tts"])).is_empty());
    }

    #[test]
    fn node_board_ranks_by_bench_and_counts_referrers() {
        use c0mpute_net::topics::CapabilityAd;
        let mk = |tags: &[&str], score: Option<u32>, hw: serde_json::Value| {
            let mut ad = CapabilityAd::with_score(
                "x".into(),
                tags.iter().map(|t| t.to_string()).collect(),
                hw,
                score,
            );
            ad.published_at_ms = 10_000;
            ad
        };
        let ads = vec![
            (
                "12D3KooWaaaaaaaaAAAAAAAA".to_string(),
                mk(
                    &["c0mpute:cpu"],
                    None,
                    serde_json::json!({"ref": "Chovy", "name": "Profullstack"}),
                ),
            ),
            (
                "12D3KooWbbbbbbbbBBBBBBBB".to_string(),
                mk(
                    &["c0mpute:gpu:nvidia"],
                    Some(9000),
                    serde_json::json!({
                        "cores": 16,
                        "ram_bytes": 68_719_476_736u64,
                        "version": "0.2.32",
                        "ref": "chovy",
                        "name": "profullstack",
                        "gateway": "http://203.0.113.7:7777/",
                        "cpu": "Example CPU",
                        "bench_at": "2026-11-01T12:00:00Z",
                        "benchmark": {"workloads": {"fib": {
                            "params": "n=42",
                            "samples": [{"threads": 1, "duration_us": 1000, "speedup": 1.0, "throughput": 500_000_000.0}]
                        }}}
                    }),
                ),
            ),
            (
                "12D3KooWccccccccCCCCCCCC".to_string(),
                mk(
                    &["c0mpute:cpu"],
                    Some(100),
                    serde_json::json!({"ref": "<bad>", "name": "has space", "gateway": "javascript:alert(1)"}),
                ),
            ),
        ];
        let p = build_nodes(&ads, 40_000);
        assert_eq!(p.workers_online, 3);
        assert_eq!(p.nodes[0].id, "BBBBBBBB");
        assert_eq!(p.nodes[0].class, "gpu:nvidia");
        assert_eq!(p.nodes[0].cores, Some(16));
        assert_eq!(p.nodes[0].ram_bytes, Some(68_719_476_736));
        assert_eq!(p.nodes[0].bench_at.as_deref(), Some("2026-11-01T12:00:00Z"));
        assert_eq!(
            p.nodes[0].benchmarks.as_ref().unwrap()["fib"].samples[0].throughput,
            500_000_000.0
        );
        assert_eq!(p.nodes[0].last_seen_s, 30);
        assert_eq!(p.nodes[2].bench_score, None);
        assert_eq!(p.nodes[1].referral, None, "invalid ref dropped");
        assert_eq!(p.nodes[1].name, None, "invalid name dropped");
        assert_eq!(p.nodes[0].name.as_deref(), Some("profullstack"));
        assert_eq!(
            p.nodes[0].gateway.as_deref(),
            Some("http://203.0.113.7:7777"),
            "an announced gateway is published as a bare origin"
        );
        assert_eq!(p.nodes[1].gateway, None, "an invalid gateway is dropped");
        assert_eq!(p.nodes[2].gateway, None, "no gateway unless announced");
        assert_eq!(
            p.operators,
            vec![OperatorRow {
                name: "profullstack".into(),
                nodes: 2
            }]
        );
        assert_eq!(
            p.referrers,
            vec![ReferrerRow {
                referral: "chovy".into(),
                nodes: 2
            }]
        );
        let json = serde_json::to_string(&p.nodes).unwrap();
        assert!(
            !json.contains("Example CPU"),
            "CPU model must never be public"
        );
        assert!(
            !json.contains("\"cpu\":"),
            "no cpu field on the public board"
        );
    }

    #[test]
    fn node_board_carries_only_a_valid_pricing_listing() {
        let ok = serde_json::json!({"pricing": {"mode": "dynamic", "min_usd": 0.02, "junk": "x"}});
        assert_eq!(
            listing(&ok),
            Some(serde_json::json!({"mode": "dynamic", "min_usd": 0.02}))
        );
        let fixed =
            serde_json::json!({"pricing": {"mode": "fixed", "min_usd": 0.0, "fixed_usd": 0.5}});
        assert_eq!(listing(&fixed).unwrap()["fixed_usd"], 0.5);
        assert_eq!(
            listing(&serde_json::json!({"pricing": {"mode": "fixed"}})),
            None
        );
        assert_eq!(
            listing(&serde_json::json!({"pricing": {"min_usd": -1}})),
            None
        );
        assert_eq!(
            listing(&serde_json::json!({"pricing": {"mode": "auction"}})),
            None
        );
        assert_eq!(listing(&serde_json::json!({})), None);
    }

    #[test]
    fn iso8601_known_epochs() {
        assert_eq!(iso8601_utc(0), "1970-01-01T00:00:00Z");
        // 2026-07-12T00:00:00Z = 1_783_814_400 s
        assert_eq!(iso8601_utc(1_783_814_400_000), "2026-07-12T00:00:00Z");
        // 2000-01-01T00:00:00Z = 946_684_800 s
        assert_eq!(iso8601_utc(946_684_800_000), "2000-01-01T00:00:00Z");
    }

    #[tokio::test]
    async fn job_lifecycle_counts_and_latency() {
        let jobs = JobState::default();
        let now = now_ms();
        jobs.on_accept(JobAccept {
            job_id: "job-1".into(),
            buyer_peer_id: "buyer".into(),
            winning_bidder_peer_id: "worker".into(),
            agreed_price_usd: 1.0,
            workload_type: "ffmpeg.transcode".into(),
            spec_inline: None,
            published_at_ms: now - 5_000,
            external_ref: None,
            buyer_addrs: Vec::new(),
        })
        .await;
        assert_eq!(jobs.in_flight.read().await.len(), 1);

        jobs.on_receipt(JobReceipt {
            job_id: "job-1".into(),
            worker_peer_id: "worker".into(),
            worker_did: None,
            buyer_peer_id: "buyer".into(),
            output_hash: Some("abc".into()),
            status: JobStatus::Completed,
            completed_at_ms: now,
            signature: None,
            external_ref: None,
        })
        .await;
        // Cleared from in-flight, recorded as completed with ~5s latency.
        assert_eq!(jobs.in_flight.read().await.len(), 0);
        let completed = jobs.completed.read().await;
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].latency_s, Some(5.0));
        assert_eq!(completed[0].workload_type, "ffmpeg.transcode");
    }

    #[tokio::test]
    async fn failed_receipt_clears_but_does_not_count() {
        let jobs = JobState::default();
        let now = now_ms();
        jobs.on_accept(JobAccept {
            job_id: "job-2".into(),
            buyer_peer_id: "buyer".into(),
            winning_bidder_peer_id: "worker".into(),
            agreed_price_usd: 1.0,
            workload_type: "ffmpeg.transcode".into(),
            spec_inline: None,
            published_at_ms: now,
            external_ref: None,
            buyer_addrs: Vec::new(),
        })
        .await;
        jobs.on_receipt(JobReceipt {
            job_id: "job-2".into(),
            worker_peer_id: "worker".into(),
            worker_did: None,
            buyer_peer_id: "buyer".into(),
            output_hash: None,
            status: JobStatus::Failed,
            completed_at_ms: now,
            signature: None,
            external_ref: None,
        })
        .await;
        assert_eq!(jobs.in_flight.read().await.len(), 0);
        assert_eq!(jobs.completed.read().await.len(), 0);
    }

    #[tokio::test]
    async fn evict_drops_stale_inflight_and_old_completions() {
        let jobs = JobState::default();
        let now = now_ms();
        // Stale in-flight (2h old) + an old completion (25h old).
        jobs.in_flight.write().await.insert(
            "old".into(),
            InFlight {
                workload_type: "ffmpeg.transcode".into(),
                accepted_at_ms: now - 2 * 60 * 60 * 1000,
            },
        );
        jobs.completed.write().await.push_back(Completed {
            workload_type: "ffmpeg.transcode".into(),
            completed_at_ms: now - 25 * 60 * 60 * 1000,
            latency_s: Some(3.0),
        });
        jobs.evict(now).await;
        assert_eq!(jobs.in_flight.read().await.len(), 0);
        assert_eq!(jobs.completed.read().await.len(), 0);
    }
}

#[cfg(test)]
mod storage_offer_tests {
    use super::storage_offer;
    use serde_json::json;

    #[test]
    fn keeps_only_sane_numbers() {
        let hw = json!({"storage": {"free_bytes": 900, "total_bytes": 1000, "per_gib_month": 0.004, "evil": "<script>"}});
        assert_eq!(
            storage_offer(&hw),
            Some(json!({"free_bytes": 900, "total_bytes": 1000, "per_gib_month": 0.004}))
        );
        let negative = json!({"storage": {"free_bytes": 1, "total_bytes": 2, "per_gib_month": -1.0}});
        assert_eq!(storage_offer(&negative), None);
        assert_eq!(storage_offer(&json!({})), None);
        let over = json!({"storage": {"free_bytes": 5000, "total_bytes": 1000, "per_gib_month": 0.0}});
        assert_eq!(storage_offer(&over).unwrap()["free_bytes"], 1000);
    }
}
