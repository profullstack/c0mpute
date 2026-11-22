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
//!   5. Follows live streams' signed records (`c0mpute/live/v1`) and files
//!      each relay's signed per-minute receipt (`c0mpute/jobs/live.relay`)
//!      as a credited `live.relay` job, bounded by what the stream could
//!      have carried (`crate::live::meter`).
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
    // DIP 0033: builds and container jobs for trusted buyers.
    "container.run",
    // DIP 0019: live-stream relays. Not an auction: each relay gossips a
    // signed receipt per stream per minute here (`live::meter`), credited to
    // it as one job.
    crate::live::meter::WORKLOAD,
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
    /// The winning bidder's full peer id, from the accept.
    worker: String,
    /// The price the buyer agreed to, in dollars (0 for free jobs).
    price_usd: f64,
}

struct Completed {
    job_id: String,
    workload_type: String,
    completed_at_ms: u64,
    /// When this aggregator filed the receipt (its own clock). Never decreases
    /// along the queue, so `GET /jobs?since=` can page through it without
    /// missing a receipt that arrived late with an older `completed_at_ms`.
    seen_at_ms: u64,
    latency_s: Option<f64>,
    /// Who ran it and for how much, when we saw the accept and the receipt
    /// came from the accepted winner. `None` otherwise: the job still counts
    /// network-wide but is credited to no node.
    credit: Option<(String, f64)>,
}

/// Highest agreed price we credit to a node for one job. Anyone can gossip an
/// accept, so a forged one must not put a fantasy number on the board.
const MAX_CREDITED_PRICE_USD: f64 = 1_000.0;

/// An accept's price as we credit it: finite, non-negative, capped.
fn credited_price(usd: f64) -> f64 {
    if usd.is_finite() && usd > 0.0 { usd.min(MAX_CREDITED_PRICE_USD) } else { 0.0 }
}

/// Jobs tracked at once. Accepts and receipts come from any peer on the
/// topic, so both tables are bounded.
const MAX_IN_FLIGHT: usize = 20_000;
const MAX_COMPLETED: usize = 200_000;

/// In-memory job state, correlated by `job_id`.
#[derive(Default)]
struct JobState {
    in_flight: RwLock<HashMap<String, InFlight>>,
    completed: RwLock<VecDeque<Completed>>,
    /// Offers per model and each worker's last accept/receipt, for
    /// `GET /autoscale` ([`crate::autoscale_signal`]).
    demand: RwLock<crate::autoscale_signal::Demand>,
    /// Live streams seen (from their signed records) and relay receipts
    /// already credited.
    live: RwLock<crate::live::meter::Book>,
}

impl JobState {
    /// A buyer awarded a job — start tracking it as in-flight.
    async fn on_accept(&self, a: JobAccept) {
        let mut in_flight = self.in_flight.write().await;
        // Anyone can gossip an accept: a full table takes no new jobs until
        // eviction makes room.
        if in_flight.len() >= MAX_IN_FLIGHT && !in_flight.contains_key(&a.job_id) {
            return;
        }
        in_flight.insert(
            a.job_id,
            InFlight {
                workload_type: a.workload_type,
                // Never ahead of our clock, or it would never age out.
                accepted_at_ms: a.published_at_ms.min(now_ms()),
                worker: a.winning_bidder_peer_id,
                price_usd: credited_price(a.agreed_price_usd),
            },
        );
    }

    /// A worker published a completion receipt. Any terminal receipt clears
    /// the in-flight entry; only `Completed` counts toward the 24h tally.
    async fn on_receipt(&self, r: JobReceipt) {
        // A twin saying the job is not its own: the awarded one still runs it.
        if r.fault.is_some() {
            return;
        }
        let started = self.in_flight.write().await.remove(&r.job_id);
        if r.status != JobStatus::Completed {
            return;
        }
        // Latency needs the accept timestamp; if we never saw the accept
        // (aggregator started mid-job) we still count the completion but
        // can't attribute a latency or an exact workload type.
        let latency_s = started.as_ref().and_then(|inf| {
            r.completed_at_ms
                .min(now_ms())
                .checked_sub(inf.accepted_at_ms)
                .map(|d| d as f64 / 1000.0)
        });
        // Credit the node only when the receipt comes from the peer the buyer
        // accepted; a receipt from anyone else proves nothing about who ran it.
        let credit = started
            .as_ref()
            .filter(|inf| inf.worker == r.worker_peer_id)
            .map(|inf| (inf.worker.clone(), inf.price_usd));
        let workload_type = started
            .map(|inf| inf.workload_type)
            .unwrap_or_else(|| "unknown".to_string());
        let mut completed = self.completed.write().await;
        if completed.len() >= MAX_COMPLETED {
            completed.pop_front();
        }
        let seen_at_ms = completed.back().map_or(0, |c| c.seen_at_ms).max(now_ms());
        completed.push_back(Completed {
            job_id: r.job_id,
            workload_type,
            // Filed in arrival order at no later than now: a far-future time
            // at the front would block eviction of everything behind it.
            completed_at_ms: r.completed_at_ms.min(now_ms()),
            seen_at_ms,
            latency_s,
            credit,
        });
    }

    /// A relay's signed receipt for one stream and one minute, already
    /// checked and bounded by the live book: file it as a completed
    /// `live.relay` job credited to that relay.
    async fn on_live_credit(&self, c: crate::live::meter::Credit) {
        let mut completed = self.completed.write().await;
        if completed.len() >= MAX_COMPLETED {
            completed.pop_front();
        }
        let seen_at_ms = completed.back().map_or(0, |c| c.seen_at_ms).max(now_ms());
        completed.push_back(Completed {
            job_id: c.job_id,
            workload_type: crate::live::meter::WORKLOAD.to_string(),
            completed_at_ms: c.completed_at_ms.min(now_ms()),
            seen_at_ms,
            // Relaying is not time a node was busy.
            latency_s: None,
            credit: Some((c.peer, credited_price(c.paid_usd))),
        });
    }

    /// A message on the live record topic or the relay receipt topic.
    async fn on_live_message(&self, topic: &str, source: Option<String>, data: &[u8]) {
        if topic == crate::live::TOPIC {
            if let Ok(crate::live::LiveMsg::Record { record }) = serde_json::from_slice::<crate::live::LiveMsg>(data) {
                self.live.write().await.on_record(&record, now_ms());
            }
            return;
        }
        let Some(receipt) = crate::live::meter::parse_receipt(data) else {
            return;
        };
        // The gossip message must come from the relay the receipt names.
        if source.is_some_and(|s| s != receipt.peer) {
            return;
        }
        let credit = self.live.write().await.on_receipt(&receipt, now_ms());
        if let Some(c) = credit {
            debug!(job_id = %c.job_id, bytes = c.bytes, usd = c.paid_usd, "aggregator: live relay receipt");
            self.on_live_credit(c).await;
        }
    }

    /// Drop stale in-flight entries and completions outside the 24h window.
    async fn evict(&self, now: u64) {
        self.live.write().await.evict(now);
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
    /// Last probe result per announced gateway origin (`/healthz` answered).
    gateways: GatewayProbes,
}

/// Announced gateways the aggregator could reach on its last probe. A node's
/// ad says where its gateway is; the board publishes it only once the
/// aggregator has fetched `<origin>/healthz` itself, so an unreachable or
/// made-up address never reaches clients (the Moshpit edge, d1sks).
type GatewayProbes = Arc<tokio::sync::RwLock<std::collections::HashMap<String, bool>>>;

const GATEWAY_PROBE_EVERY: Duration = Duration::from_secs(120);

/// Drop every `gateway` the last probe did not reach.
fn keep_reachable_gateways(payload: &mut NodesPayload, probes: &std::collections::HashMap<String, bool>) {
    for n in &mut payload.nodes {
        if n.gateway.as_ref().is_some_and(|g| probes.get(g) != Some(&true)) {
            n.gateway = None;
        }
    }
}

async fn run_gateway_probes(registry: Registry, probes: GatewayProbes) {
    let Ok(http) = reqwest::Client::builder()
        .timeout(Duration::from_secs(6))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    else {
        return;
    };
    loop {
        let origins: std::collections::BTreeSet<String> = registry
            .list()
            .await
            .into_iter()
            .filter_map(|(_, ad)| {
                ad.hardware
                    .get("gateway")
                    .and_then(|v| v.as_str())
                    .and_then(crate::capabilities::gateway_origin)
            })
            .collect();
        let mut set = tokio::task::JoinSet::new();
        for origin in origins {
            let http = http.clone();
            set.spawn(async move {
                let ok = crate::gateway_announce::answers(&http, &origin).await;
                (origin, ok)
            });
        }
        let mut seen = std::collections::HashMap::new();
        while let Some(Ok((origin, ok))) = set.join_next().await {
            seen.insert(origin, ok);
        }
        let reachable = seen.values().filter(|ok| **ok).count();
        debug!(probed = seen.len(), reachable, "aggregator: gateway probes");
        *probes.write().await = seen;
        tokio::time::sleep(GATEWAY_PROBE_EVERY).await;
    }
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
    /// The email its operator offers for questions (`c0mpute contact`), if
    /// any. Public by the operator's choice; re-validated to a plain address.
    #[serde(skip_serializing_if = "Option::is_none")]
    contact: Option<String>,
    /// The node's secure-chat address (`c0mpute dm send <node>` resolves the
    /// node id to it). Re-validated to the address format.
    #[serde(skip_serializing_if = "Option::is_none")]
    dm: Option<String>,
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
    /// One identity running as more than one process (a copied identity dir,
    /// or two services on one box): jobs could split between them. Present
    /// only when the node reports a twin itself or this aggregator heard ads
    /// from two instances of it.
    #[serde(skip_serializing_if = "Option::is_none")]
    duplicate: Option<Duplicate>,
    /// Jobs this node completed in the last 24h, by type, with run time and
    /// what buyers paid. Absent when it completed none.
    #[serde(skip_serializing_if = "Option::is_none")]
    jobs_24h: Option<NodeJobs>,
}

#[derive(Serialize, Debug, PartialEq, Clone, Default)]
struct Duplicate {
    /// Distinct processes (instance ids) advertising this peer id lately.
    instances: u64,
    /// The node itself reports a twin in its ad.
    reported: bool,
    /// The node reports it is the older twin and has stopped bidding.
    yielding: bool,
}

/// The `duplicate` a node's own ad reports (`hardware.twin`).
fn reported_duplicate(hw: &serde_json::Value) -> Option<Duplicate> {
    let twin = hw.get("twin").filter(|t| t.is_object())?;
    Some(Duplicate {
        instances: 0,
        reported: true,
        yielding: twin.get("yielding").and_then(|v| v.as_bool()).unwrap_or(false),
    })
}

/// One workload type's share of a node's last 24h on the board.
#[derive(Serialize, Debug, PartialEq, Clone, Default)]
struct NodeWorkloadJobs {
    /// Completed jobs.
    jobs: u64,
    /// Accept-to-receipt time summed over those jobs, in seconds.
    busy_s: f64,
    /// What buyers agreed to pay for them, in dollars, before any fee.
    paid_usd: f64,
}

/// A node's completed jobs over the last 24h, as this aggregator heard them
/// on the job topics: totals plus a breakdown by workload type. In-memory, so
/// an aggregator restart starts the window over.
#[derive(Serialize, Debug, PartialEq, Clone, Default)]
struct NodeJobs {
    jobs: u64,
    busy_s: f64,
    paid_usd: f64,
    by_type: BTreeMap<String, NodeWorkloadJobs>,
}

/// Fold the 24h completions into per-node stats keyed by short id (the board's
/// row id).
fn node_job_stats<'a>(completed: impl IntoIterator<Item = &'a Completed>) -> HashMap<String, NodeJobs> {
    let mut out: HashMap<String, NodeJobs> = HashMap::new();
    for c in completed {
        let Some((worker, price)) = &c.credit else { continue };
        let busy = c.latency_s.unwrap_or(0.0).max(0.0);
        let n = out.entry(short_id(worker)).or_default();
        n.jobs += 1;
        n.busy_s += busy;
        n.paid_usd += price;
        let t = n.by_type.entry(c.workload_type.clone()).or_default();
        t.jobs += 1;
        t.busy_s += busy;
        t.paid_usd += price;
    }
    out
}

/// Put each node's 24h job stats on its row.
fn apply_job_stats(payload: &mut NodesPayload, stats: &HashMap<String, NodeJobs>) {
    for n in &mut payload.nodes {
        n.jobs_24h = stats.get(&n.id).cloned();
    }
}

/// Add what the aggregator saw itself: ads from more than one instance.
fn apply_instance_counts(payload: &mut NodesPayload, instances: &HashMap<String, usize>) {
    for n in &mut payload.nodes {
        let count = instances.get(&n.id).copied().unwrap_or(0) as u64;
        if count > 1 || n.duplicate.is_some() {
            let d = n.duplicate.get_or_insert_with(Duplicate::default);
            d.instances = count.max(d.instances);
        }
    }
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
            contact: ad
                .hardware
                .get("contact")
                .and_then(|v| v.as_str())
                .and_then(crate::capabilities::sanitize_contact),
            dm: ad
                .hardware
                .get("dm")
                .and_then(|v| v.as_str())
                .filter(|a| a.len() <= 128)
                .and_then(c0mpute_secure_chat::identity::normalize_query),
            pricing: listing(&ad.hardware),
            storage: storage_offer(&ad.hardware),
            gateway: ad
                .hardware
                .get("gateway")
                .and_then(|v| v.as_str())
                .and_then(crate::capabilities::gateway_origin),
            donates: donates(&ad.tags),
            last_seen_s: now.saturating_sub(ad.published_at_ms) / 1000,
            duplicate: reported_duplicate(&ad.hardware),
            jobs_24h: None,
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
    let mut payload = build_nodes(&ads, now_ms());
    let instances: HashMap<String, usize> = state
        .registry
        .instance_counts()
        .await
        .into_iter()
        .map(|(p, n)| (short_id(&p.to_base58()), n))
        .collect();
    apply_instance_counts(&mut payload, &instances);
    state.jobs.evict(now_ms()).await;
    let stats = node_job_stats(state.jobs.completed.read().await.iter());
    apply_job_stats(&mut payload, &stats);
    keep_reachable_gateways(&mut payload, &*state.gateways.read().await);
    Json(payload)
}

/// One completed job credited to the node that ran it (`GET /jobs`).
#[derive(Serialize, Debug, PartialEq, Clone)]
struct CreditedJob {
    job_id: String,
    /// The board's row id (short peer id) of the node the buyer accepted.
    node: String,
    /// The same node's full libp2p peer id (base58): what a settlement
    /// system credits (`c0mpute:<peer>`).
    peer: String,
    #[serde(rename = "type")]
    workload_type: String,
    completed_at_ms: u64,
    /// When this aggregator filed the receipt; page with `?since=` on it.
    seen_at_ms: u64,
    /// Accept to receipt, in seconds (0 when the accept was not seen).
    busy_s: f64,
    /// What the buyer agreed to pay, in dollars, before the house fee.
    paid_usd: f64,
}

#[derive(Serialize, Debug)]
struct JobsPayload {
    generated_at_ms: u64,
    /// Pass back as `?since=` for the next page (or the next poll).
    next_since: u64,
    /// More credited jobs are waiting after `next_since`.
    more: bool,
    jobs: Vec<CreditedJob>,
}

/// Rows `GET /jobs` returns at most per call, by default and at most.
const JOBS_PAGE_DEFAULT: usize = 2_000;
const JOBS_PAGE_MAX: usize = 5_000;

/// Credited completions filed after `since` (aggregator clock, ms), oldest
/// first, at most about `limit` of them. A page never splits one `seen_at_ms`,
/// so `next_since` is safe to pass back with `>`.
fn credited_jobs_since<'a>(
    completed: impl IntoIterator<Item = &'a Completed>,
    since: u64,
    limit: usize,
) -> (Vec<CreditedJob>, bool) {
    let mut out: Vec<CreditedJob> = Vec::new();
    for c in completed {
        if c.seen_at_ms <= since {
            continue;
        }
        let Some((worker, price)) = &c.credit else { continue };
        if out.len() >= limit && out.last().is_some_and(|l| l.seen_at_ms != c.seen_at_ms) {
            return (out, true);
        }
        out.push(CreditedJob {
            job_id: c.job_id.clone(),
            node: short_id(worker),
            peer: worker.clone(),
            workload_type: c.workload_type.clone(),
            completed_at_ms: c.completed_at_ms,
            seen_at_ms: c.seen_at_ms,
            busy_s: c.latency_s.unwrap_or(0.0).max(0.0),
            paid_usd: *price,
        });
    }
    (out, false)
}

/// `GET /jobs?since=<ms>&limit=<n>`: every completed job this aggregator
/// credited to a node in its 24h window, one row each with its job id, so a
/// site can keep durable per-node history (the window itself is in memory
/// and starts over when this process restarts).
async fn jobs_handler(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
) -> Json<JobsPayload> {
    let since = q.get("since").and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    let limit = q
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(JOBS_PAGE_DEFAULT)
        .clamp(1, JOBS_PAGE_MAX);
    state.jobs.evict(now_ms()).await;
    let (jobs, more) = credited_jobs_since(state.jobs.completed.read().await.iter(), since, limit);
    let next_since = jobs.last().map_or(since, |j| j.seen_at_ms);
    Json(JobsPayload { generated_at_ms: now_ms(), next_since, more, jobs })
}

#[derive(Serialize, Debug)]
struct AutoscalePayload {
    now_ms: u64,
    window_s: u64,
    node: crate::autoscale_signal::NodeActivity,
    models: HashMap<String, crate::autoscale_signal::ModelDemand>,
}

/// `GET /autoscale?node=<peer id or board id>&models=a,b&window_s=1200`: what
/// an operator's autoscaler needs to stop a rented node nobody is using and to
/// start it when somebody is: whether the node is online and busy, when it
/// last took and finished a job, and how many auctions asked for its models
/// (and found no taker) recently. Counts only.
async fn autoscale_handler(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
) -> Result<Json<AutoscalePayload>, (axum::http::StatusCode, &'static str)> {
    use crate::autoscale_signal::{NodeActivity, parse_models, parse_window_ms, valid_node_id};
    let id = q.get("node").map(|s| s.trim().to_string()).unwrap_or_default();
    if !valid_node_id(&id) {
        return Err((axum::http::StatusCode::BAD_REQUEST, "node: a peer id or the 8-character board id"));
    }
    let models = parse_models(q.get("models").map(String::as_str));
    let window_ms = parse_window_ms(q.get("window_s").map(String::as_str));
    let now = now_ms();
    state.jobs.evict(now).await;
    let short = short_id(&id);
    let mut node = NodeActivity { id: short.clone(), ..Default::default() };
    for (peer, ad) in state.registry.list().await {
        let p = peer.to_base58();
        if p == id || short_id(&p) == short {
            let seen = now.saturating_sub(ad.published_at_ms) / 1000;
            node.online = true;
            node.last_seen_s = Some(node.last_seen_s.map_or(seen, |s: u64| s.min(seen)));
            node.peer_id = Some(p);
            for t in &ad.tags {
                if let Some(m) = t.split(":model:").nth(1)
                    && !m.is_empty()
                    && !node.serves.iter().any(|x| x == m)
                {
                    node.serves.push(m.to_string());
                }
            }
        }
    }
    node.in_flight = state
        .jobs
        .in_flight
        .read()
        .await
        .values()
        .filter(|f| f.worker == id || short_id(&f.worker) == short)
        .count() as u64;
    let demand = {
        let mut d = state.jobs.demand.write().await;
        d.evict(now);
        if let Some((peer, accept, receipt)) = d.worker(&id) {
            node.peer_id.get_or_insert(peer);
            node.last_accept_ms = accept;
            node.last_receipt_ms = receipt;
        }
        d.models(&models, window_ms, now)
    };
    Ok(Json(AutoscalePayload { now_ms: now, window_s: window_ms / 1000, node, models: demand }))
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
    // Live streams' signed records: which streams exist, their bitrate and
    // whether they are paid. Relay receipts are checked against these.
    if let Err(e) = net.subscribe(crate::live::TOPIC).await {
        warn!(err = %e, "aggregator: live topic subscribe failed");
    }
    let live_receipts = crate::live::meter::topic();

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
        if msg.topic == crate::live::TOPIC || msg.topic == live_receipts {
            jobs.on_live_message(&msg.topic, msg.source.map(|s| s.to_base58()), &msg.data).await;
            continue;
        }
        if !msg.topic.starts_with("c0mpute/jobs/") {
            continue;
        }
        // Accept before receipt: an accept carries `winning_bidder_peer_id`
        // (absent from every other job message), so it parses unambiguously.
        if let Ok(accept) = serde_json::from_slice::<JobAccept>(&msg.data) {
            debug!(job_id = %accept.job_id, "aggregator: job accepted");
            jobs.demand.write().await.on_accept(&accept.job_id, &accept.winning_bidder_peer_id, now_ms());
            jobs.on_accept(accept).await;
            continue;
        }
        if let Ok(receipt) = serde_json::from_slice::<JobReceipt>(&msg.data) {
            debug!(job_id = %receipt.job_id, status = ?receipt.status, "aggregator: job receipt");
            if receipt.fault.is_none() {
                jobs.demand.write().await.on_receipt(&receipt.worker_peer_id, now_ms());
            }
            jobs.on_receipt(receipt).await;
            continue;
        }
        // An offer is demand (per model) for autoscalers; bids are ignored.
        if let Ok(offer) = serde_json::from_slice::<c0mpute_net::topics::JobOffer>(&msg.data) {
            jobs.demand.write().await.on_offer(&offer, now_ms());
        }
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
    let jobs = Arc::new(JobState {
        live: RwLock::new(crate::live::meter::Book::from_env()),
        ..Default::default()
    });
    tokio::spawn(run_job_tracker(net.clone(), jobs.clone()));
    tokio::spawn(run_dht_crawl(net.clone()));

    // One tiny offer per workload every few minutes, bids counted, nothing
    // accepted: the auction success rate on the status page.
    if let Some(interval) = crate::canary::interval_from_env() {
        tokio::spawn(crate::canary::run(net.clone(), registry.clone(), interval));
    }

    let gateways: GatewayProbes = Default::default();
    tokio::spawn(run_gateway_probes(registry.clone(), gateways.clone()));

    let state = AppState { registry, jobs, gateways };
    let app = Router::new()
        .route("/", get(status_handler))
        .route("/nodes", get(nodes_handler))
        .route("/jobs", get(jobs_handler))
        .route("/autoscale", get(autoscale_handler))
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
                        "contact": "Ops@Example.com",
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
                    serde_json::json!({"ref": "<bad>", "name": "has space", "gateway": "javascript:alert(1)", "contact": "x@y.com\"><script>"}),
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
        assert_eq!(p.nodes[0].contact.as_deref(), Some("ops@example.com"), "a plain contact email passes, lowercased");
        assert_eq!(p.nodes[1].contact, None, "a contact that is not a plain email is dropped");
        assert_eq!(p.nodes[0].name.as_deref(), Some("profullstack"));
        assert_eq!(
            p.nodes[0].gateway.as_deref(),
            Some("http://203.0.113.7:7777"),
            "an announced gateway is published as a bare origin"
        );
        assert_eq!(p.nodes[1].gateway, None, "an invalid gateway is dropped");
        assert_eq!(p.nodes[2].gateway, None, "no gateway unless announced");
        // Published only after the aggregator reached it itself.
        let mut unprobed = build_nodes(&ads, 40_000);
        keep_reachable_gateways(&mut unprobed, &HashMap::new());
        assert_eq!(unprobed.nodes[0].gateway, None, "not probed yet: not published");
        let mut down = build_nodes(&ads, 40_000);
        keep_reachable_gateways(&mut down, &HashMap::from([("http://203.0.113.7:7777".to_string(), false)]));
        assert_eq!(down.nodes[0].gateway, None, "unreachable: not published");
        let mut up = build_nodes(&ads, 40_000);
        keep_reachable_gateways(&mut up, &HashMap::from([("http://203.0.113.7:7777".to_string(), true)]));
        assert_eq!(up.nodes[0].gateway.as_deref(), Some("http://203.0.113.7:7777"));
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
            e2e: None,
            sealed_spec: None,
            result_key: None,
            winning_instance: None,
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
            instance: None,
            fault: None,
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
            e2e: None,
            sealed_spec: None,
            result_key: None,
            winning_instance: None,
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
            instance: None,
            fault: None,
        })
        .await;
        assert_eq!(jobs.in_flight.read().await.len(), 0);
        assert_eq!(jobs.completed.read().await.len(), 0);
    }

    fn accept_for(job: &str, workload: &str) -> JobAccept {
        JobAccept {
            job_id: job.into(),
            buyer_peer_id: "buyer".into(),
            winning_bidder_peer_id: "worker".into(),
            agreed_price_usd: 1.0,
            workload_type: workload.into(),
            spec_inline: None,
            published_at_ms: now_ms(),
            external_ref: None,
            buyer_addrs: Vec::new(),
            e2e: None,
            sealed_spec: None,
            result_key: None,
            winning_instance: None,
        }
    }

    fn receipt_for(job: &str, completed_at_ms: u64) -> JobReceipt {
        JobReceipt {
            job_id: job.into(),
            worker_peer_id: "worker".into(),
            worker_did: None,
            buyer_peer_id: "buyer".into(),
            output_hash: Some("abc".into()),
            status: JobStatus::Completed,
            completed_at_ms,
            signature: None,
            external_ref: None,
            instance: None,
            fault: None,
        }
    }

    #[tokio::test]
    async fn future_dated_jobs_still_age_out() {
        let jobs = JobState::default();
        let now = now_ms();
        let mut a = accept_for("far", "ffmpeg.transcode");
        a.published_at_ms = u64::MAX;
        jobs.on_accept(a).await;
        assert!(jobs.in_flight.read().await["far"].accepted_at_ms <= now_ms());
        jobs.on_receipt(receipt_for("far", u64::MAX)).await;
        jobs.on_receipt(receipt_for("other", now)).await;
        assert!(jobs.completed.read().await.iter().all(|c| c.completed_at_ms <= now_ms()));
        jobs.evict(now_ms() + COMPLETED_WINDOW.as_millis() as u64 + 1).await;
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
                worker: "worker".into(),
                price_usd: 0.0,
            },
        );
        jobs.completed.write().await.push_back(Completed {
            job_id: "old".into(),
            workload_type: "ffmpeg.transcode".into(),
            completed_at_ms: now - 25 * 60 * 60 * 1000,
            seen_at_ms: now - 25 * 60 * 60 * 1000,
            latency_s: Some(3.0),
            credit: None,
        });
        jobs.evict(now).await;
        assert_eq!(jobs.in_flight.read().await.len(), 0);
        assert_eq!(jobs.completed.read().await.len(), 0);
    }

    #[tokio::test]
    async fn completions_credit_the_accepted_worker_by_type() {
        let jobs = JobState::default();
        let now = now_ms();
        let worker = "12D3KooWworkerAAAAAAAAAAAAAAAAworkerXYZ12345";
        let mut a = accept_for("t1", "tts.synthesize");
        a.winning_bidder_peer_id = worker.into();
        a.agreed_price_usd = 0.02;
        a.published_at_ms = now - 4_000;
        jobs.on_accept(a).await;
        let mut a = accept_for("i1", "infernet.inference");
        a.winning_bidder_peer_id = worker.into();
        a.agreed_price_usd = f64::INFINITY; // forged: credited as 0
        a.published_at_ms = now - 2_000;
        jobs.on_accept(a).await;
        // A receipt from a peer the buyer did not accept credits nobody.
        let mut a = accept_for("x1", "tts.synthesize");
        a.winning_bidder_peer_id = worker.into();
        jobs.on_accept(a).await;
        for (id, from) in [("t1", worker), ("i1", worker), ("x1", "someone-else")] {
            let mut r = receipt_for(id, now);
            r.worker_peer_id = from.into();
            jobs.on_receipt(r).await;
        }
        let completed = jobs.completed.read().await;
        assert_eq!(completed.len(), 3, "all three still count network-wide");
        let stats = node_job_stats(completed.iter());
        let n = &stats[&short_id(worker)];
        assert_eq!(n.jobs, 2);
        assert!((n.paid_usd - 0.02).abs() < 1e-9);
        assert!((n.busy_s - 6.0).abs() < 0.5);
        assert_eq!(n.by_type["tts.synthesize"].jobs, 1);
        assert_eq!(n.by_type["infernet.inference"].paid_usd, 0.0);
    }

    #[tokio::test]
    async fn credited_jobs_page_by_seen_at() {
        let jobs = JobState::default();
        let now = now_ms();
        let worker = "12D3KooWworkerAAAAAAAAAAAAAAAAworkerXYZ12345";
        for (i, id) in ["a", "b", "c"].iter().enumerate() {
            let mut a = accept_for(id, "tts.synthesize");
            a.winning_bidder_peer_id = worker.into();
            a.agreed_price_usd = 0.01 * (i as f64 + 1.0);
            a.published_at_ms = now - 3_000;
            jobs.on_accept(a).await;
            let mut r = receipt_for(id, now);
            r.worker_peer_id = worker.into();
            jobs.on_receipt(r).await;
        }
        // Uncredited (receipt from someone else): never listed.
        let mut a = accept_for("x", "tts.synthesize");
        a.winning_bidder_peer_id = worker.into();
        jobs.on_accept(a).await;
        let mut r = receipt_for("x", now);
        r.worker_peer_id = "someone-else".into();
        jobs.on_receipt(r).await;

        let completed = jobs.completed.read().await;
        let seen: Vec<u64> = completed.iter().map(|c| c.seen_at_ms).collect();
        assert!(seen.windows(2).all(|w| w[0] <= w[1]), "seen_at never decreases");
        let (all, more) = credited_jobs_since(completed.iter(), 0, 100);
        assert!(!more);
        assert_eq!(all.iter().map(|j| j.job_id.as_str()).collect::<Vec<_>>(), ["a", "b", "c"]);
        assert_eq!(all[0].node, short_id(worker));
        assert_eq!(all[0].peer, worker, "the full peer id rides along for settlement");
        assert!((all[2].paid_usd - 0.03).abs() < 1e-9);
        assert!(all[0].busy_s >= 2.5);
        // Nothing after the last one seen.
        let (none, _) = credited_jobs_since(completed.iter(), all[2].seen_at_ms, 100);
        assert!(none.is_empty());
        // A page of 1 never splits rows filed in the same millisecond.
        let (page, _) = credited_jobs_since(completed.iter(), 0, 1);
        assert!(page.iter().all(|j| j.seen_at_ms == page[0].seen_at_ms));
        let json = serde_json::to_value(&all[0]).unwrap();
        assert_eq!(json["type"], "tts.synthesize");
        assert_eq!(json["peer"], worker);
    }

    /// A signed live record with one ~`kbps` rendition.
    fn live_record(
        owner: &c0mpute_net::libp2p::identity::Keypair,
        kbps: u64,
        binding: Option<&crate::live::record::Binding>,
    ) -> crate::live::record::LiveRecord {
        use crate::live::record::{LiveBody, LiveRecord, Window, describe_segment};
        let mut w = Window::new("src", None, 6);
        for i in 0..3u8 {
            w.push(describe_segment(&vec![i; (kbps * 500) as usize], 4000, false).0);
        }
        LiveRecord::sign_bound(
            owner,
            LiveBody {
                v: 1,
                stream_id: String::new(),
                name: "show".into(),
                owner: String::new(),
                seq: now_ms(),
                ts_ms: now_ms(),
                ended: false,
                renditions: vec![w.rendition],
            },
            binding,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn live_relay_receipts_become_credited_jobs() {
        use crate::live::meter::{self, RelayReceipt};
        use c0mpute_net::libp2p::identity::Keypair;
        assert!(KNOWN_WORKLOADS.contains(&"live.relay"));
        let jobs = JobState::default();
        let owner = Keypair::generate_ed25519();
        let relay = Keypair::generate_ed25519();
        let peer = relay.public().to_peer_id().to_base58();
        let paid = crate::live::record::Binding {
            account: "acct_1".into(),
            paid: true,
            max_viewers: None,
        };
        let record = live_record(&owner, 2_000, Some(&paid));
        let sid = record.stream_id().to_string();
        let msg = serde_json::to_vec(&crate::live::LiveMsg::Record { record }).unwrap();
        jobs.on_live_message(crate::live::TOPIC, None, &msg).await;

        let topic = meter::topic();
        assert_eq!(topic, "c0mpute/jobs/live.relay");
        let epoch = meter::epoch_of(now_ms()) - 1;
        let receipt = RelayReceipt::sign_with(&relay, &sid, 2_000_000_000, epoch).unwrap();
        let wire = serde_json::to_vec(&receipt).unwrap();
        // Relayed by someone claiming to be another peer: ignored.
        jobs.on_live_message(&topic, Some("12D3KooWsomeoneElse".into()), &wire).await;
        assert!(jobs.completed.read().await.is_empty());
        jobs.on_live_message(&topic, Some(peer.clone()), &wire).await;
        // The same receipt again is one job, not two.
        jobs.on_live_message(&topic, Some(peer.clone()), &wire).await;
        // A receipt for a stream nobody has a record of is not credited.
        let unknown = RelayReceipt::sign_with(&relay, "00000000000000000000", 5, epoch).unwrap();
        jobs.on_live_message(&topic, Some(peer.clone()), &serde_json::to_vec(&unknown).unwrap()).await;

        let completed = jobs.completed.read().await;
        let (rows, more) = credited_jobs_since(completed.iter(), 0, 100);
        assert!(!more);
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        let node = short_id(&peer);
        assert_eq!(row.job_id, format!("live:{sid}:{node}:{epoch}"));
        assert!(row.job_id.len() < 128);
        assert_eq!(row.node, node);
        assert_eq!(row.peer, peer);
        assert_eq!(row.workload_type, "live.relay");
        assert!((row.paid_usd - 0.02).abs() < 1e-9, "2 GB at $0.01 per GB");
        assert_eq!(row.busy_s, 0.0);
        let json = serde_json::to_value(row).unwrap();
        assert_eq!(json["type"], "live.relay");
        assert_eq!(json["peer"], peer);
        // And it shows on the node's 24h stats under its own type.
        let stats = node_job_stats(completed.iter());
        assert!((stats[&node].by_type["live.relay"].paid_usd - 0.02).abs() < 1e-9);
    }

    #[tokio::test]
    async fn an_unpaid_stream_is_credited_at_zero() {
        use crate::live::meter::{self, RelayReceipt};
        use c0mpute_net::libp2p::identity::Keypair;
        let jobs = JobState::default();
        let owner = Keypair::generate_ed25519();
        let relay = Keypair::generate_ed25519();
        let peer = relay.public().to_peer_id().to_base58();
        let record = live_record(&owner, 2_000, None);
        let sid = record.stream_id().to_string();
        let msg = serde_json::to_vec(&crate::live::LiveMsg::Record { record }).unwrap();
        jobs.on_live_message(crate::live::TOPIC, None, &msg).await;
        let receipt = RelayReceipt::sign_with(&relay, &sid, 900_000_000, meter::epoch_of(now_ms())).unwrap();
        jobs.on_live_message(&meter::topic(), Some(peer), &serde_json::to_vec(&receipt).unwrap()).await;
        let completed = jobs.completed.read().await;
        let (rows, _) = credited_jobs_since(completed.iter(), 0, 100);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].paid_usd, 0.0);
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
