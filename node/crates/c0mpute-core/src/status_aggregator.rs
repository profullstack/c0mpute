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
/// worker daemon; `infernet.inference` is emitted by the infernet peer
/// binary. Both always appear in the output (as zeros when idle) to mirror
/// the page's stub shape.
const KNOWN_WORKLOADS: &[&str] = &["ffmpeg.transcode", "infernet.inference"];

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
        },
        source: "aggregator",
    }
}

async fn status_handler(State(state): State<AppState>) -> Json<StatusPayload> {
    Json(build_payload(&state).await)
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
        bootstrap::fetch_or_fallback(bootstrap::DEFAULT_BOOTSTRAP_URL, vec![]).await;
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

    let state = AppState { registry, jobs };
    let app = Router::new()
        .route("/", get(status_handler))
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
