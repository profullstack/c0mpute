//! `c0mpute jobs serve`: an authenticated HTTP bridge from an application
//! (meshhook) to generic c0mpute jobs. DIP 0030.
//!
//! `POST /v1/jobs` runs one auction for the named workload, waits for the
//! winner's receipt, fetches the result by the receipt's hash from the
//! winner, checks the bytes against that hash, and answers with the result.
//! v1 accepts only `workflow.step`; [`Workload`] is where more go.
//!
//! Same hardening as `c0mpute speech serve`: a required bearer token
//! compared in constant time, loopback bind by default, bounded bodies.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use c0mpute_net::topics::JobStatus;
use c0mpute_net::{Libp2pNetwork, Network};
use c0mpute_store::ChunkStore;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tower_http::trace::TraceLayer;

use crate::buyer::AuctionFailure;
use crate::gateway_util::{BearerCheck, FetchError, check_bearer, fetch_verified};
use crate::{Config, JobAuction, Supervisor, run_auction, workflow};

pub const DEFAULT_BIND: &str = "127.0.0.1:8788";
pub const DEFAULT_TIMEOUT_MS: u64 = 60_000;
pub const MIN_TIMEOUT_MS: u64 = 1_000;
pub const MAX_TIMEOUT_MS: u64 = 300_000;
pub const DEFAULT_BID_WINDOW: Duration = Duration::from_secs(2);
/// Jobs in flight through one bridge; more wait for a slot (inside their
/// own timeout).
pub const MAX_CONCURRENT_JOBS: usize = 32;
/// Request envelope around a spec of at most [`workflow::MAX_SPEC_BYTES`].
const MAX_REQUEST_BYTES: usize = workflow::MAX_SPEC_BYTES + 64 * 1024;
const MAX_ALLOW_PEERS: usize = 64;

/// Workloads the bridge runs. Each knows its required capability tags and
/// how big a result it accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Workload {
    WorkflowStep,
    CrawlFetch,
}

impl Workload {
    const ALL: &'static [Workload] = &[Workload::WorkflowStep, Workload::CrawlFetch];

    fn parse(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|w| w.name() == name)
    }
    fn name(self) -> &'static str {
        match self {
            Workload::WorkflowStep => workflow::WORKLOAD,
            Workload::CrawlFetch => crate::crawl::WORKLOAD,
        }
    }
    fn required_capabilities(self) -> Vec<String> {
        match self {
            Workload::WorkflowStep => vec![workflow::ROLE_TAG.to_string()],
            Workload::CrawlFetch => vec![crate::crawl::ROLE_TAG.to_string()],
        }
    }
    fn max_result_bytes(self) -> usize {
        match self {
            Workload::WorkflowStep | Workload::CrawlFetch => workflow::MAX_RESULT_BYTES,
        }
    }
    fn valid_result(self, value: &Value) -> bool {
        match self {
            Workload::WorkflowStep | Workload::CrawlFetch => {
                value.get("v").and_then(Value::as_u64) == Some(1)
                    && value.get("ok").is_some_and(Value::is_boolean)
            }
        }
    }
}

/// Bridge state. Build with [`JobsBridge::new`] and mount [`router`]; the
/// CLI does both in [`serve`].
#[derive(Clone)]
pub struct JobsBridge {
    net: Arc<Libp2pNetwork>,
    store: ChunkStore,
    token: Arc<str>,
    max_price_usd: f64,
    allow_peers: Arc<Vec<String>>,
    bid_window: Duration,
    slots: Arc<Semaphore>,
}

impl JobsBridge {
    /// `token` must be 24 to 256 characters; `max_price_usd` finite and >= 0.
    /// `allow_peers` (may be empty) is the global bidder allowlist.
    pub fn new(
        net: Arc<Libp2pNetwork>,
        store: ChunkStore,
        token: String,
        max_price_usd: f64,
        allow_peers: Vec<String>,
    ) -> Result<Self> {
        if token.len() < 24 || token.len() > 256 {
            bail!("C0MPUTE_JOBS_TOKEN must be between 24 and 256 characters");
        }
        if !max_price_usd.is_finite() || max_price_usd < 0.0 {
            bail!("maximum job price must be a non-negative number");
        }
        Ok(Self {
            net,
            store,
            token: Arc::from(token),
            max_price_usd,
            allow_peers: Arc::new(allow_peers),
            bid_window: DEFAULT_BID_WINDOW,
            slots: Arc::new(Semaphore::new(MAX_CONCURRENT_JOBS)),
        })
    }

    pub fn with_bid_window(mut self, window: Duration) -> Self {
        self.bid_window = window;
        self
    }

    /// Run one job without the HTTP layer (for one-shot CLI submits such as
    /// `c0mpute crawl fetch`). Returns the same body `POST /v1/jobs` would.
    pub async fn submit_once(&self, workload: &str, spec: Value, timeout: Duration) -> Result<Value> {
        let w = Workload::parse(workload)
            .with_context(|| format!("unsupported workload {workload:?}"))?;
        let spec_bytes = serde_json::to_vec(&spec)?;
        run_job(
            self,
            w,
            spec,
            spec_bytes,
            self.max_price_usd,
            (*self.allow_peers).clone(),
            timeout,
        )
        .await
        .map_err(|ApiError(status, body)| anyhow::anyhow!("{status}: {body}"))
    }
}

/// Boot a buyer and run one job: what `c0mpute crawl fetch` uses.
pub async fn submit_once(workload: &str, spec: Value, max_price_usd: f64, timeout: Duration) -> Result<Value> {
    let supervisor = Supervisor::boot_buyer(Config::default()).await?;
    // Any token: the HTTP layer is not mounted.
    let token: String = std::iter::repeat_n('x', 32).collect();
    let bridge = JobsBridge::new(supervisor.libp2p, supervisor.store, token, max_price_usd, Vec::new())?;
    // Give the swarm a moment to reach peers before the auction.
    tokio::time::sleep(Duration::from_secs(5)).await;
    bridge.submit_once(workload, spec, timeout).await
}

/// Parse `C0MPUTE_JOBS_ALLOW_PEERS` (comma-separated peer ids).
pub fn parse_allow_peers(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn router(state: JobsBridge) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/jobs", post(submit))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Boot a buyer node and serve the bridge until the process exits.
pub async fn serve(
    bind: std::net::SocketAddr,
    token: String,
    max_price_usd: f64,
    allow_peers: Vec<String>,
    bid_window: Duration,
) -> Result<()> {
    // Validate before the network comes up so a bad token fails fast.
    if token.len() < 24 || token.len() > 256 {
        bail!("C0MPUTE_JOBS_TOKEN must be between 24 and 256 characters");
    }
    let supervisor = Supervisor::boot_buyer(Config::default()).await?;
    let state = JobsBridge::new(
        supervisor.libp2p,
        supervisor.store,
        token,
        max_price_usd,
        allow_peers,
    )?
    .with_bid_window(bid_window);
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("bind c0mpute jobs bridge at {bind}"))?;
    tracing::info!(%bind, "c0mpute jobs bridge listening");
    axum::serve(listener, router(state)).await?;
    Ok(())
}

#[derive(Debug)]
struct ApiError(StatusCode, Value);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(self.1)).into_response()
    }
}

fn bad_request(detail: impl Into<String>) -> ApiError {
    ApiError(
        StatusCode::BAD_REQUEST,
        json!({ "error": "bad_request", "detail": detail.into() }),
    )
}

async fn healthz(State(state): State<JobsBridge>) -> Json<Value> {
    let workloads: Vec<&str> = Workload::ALL.iter().map(|w| w.name()).collect();
    Json(json!({
        "ok": true,
        "peer_id": state.net.peer_id().to_base58(),
        "workloads": workloads,
    }))
}

#[derive(Debug, Deserialize)]
struct JobRequest {
    workload: String,
    spec: Value,
    #[serde(default)]
    max_price: Option<f64>,
    #[serde(default)]
    allow_peers: Option<Vec<String>>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

async fn submit(State(state): State<JobsBridge>, headers: HeaderMap, body: Bytes) -> Response {
    match submit_inner(&state, &headers, &body).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn submit_inner(
    state: &JobsBridge,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Value, ApiError> {
    match check_bearer(headers, &state.token) {
        BearerCheck::Ok => {}
        BearerCheck::Missing | BearerCheck::Invalid => {
            return Err(ApiError(
                StatusCode::UNAUTHORIZED,
                json!({ "error": "unauthorized" }),
            ));
        }
    }
    let req: JobRequest =
        serde_json::from_slice(body).map_err(|e| bad_request(format!("invalid request: {e}")))?;
    let workload = Workload::parse(&req.workload).ok_or_else(|| {
        ApiError(
            StatusCode::BAD_REQUEST,
            json!({ "error": "unknown_workload", "detail": format!("unsupported workload {:?}", req.workload) }),
        )
    })?;
    if !req.spec.is_object() {
        return Err(bad_request("spec must be a JSON object"));
    }
    let spec_bytes = serde_json::to_vec(&req.spec).map_err(|e| bad_request(e.to_string()))?;
    if spec_bytes.len() > workflow::MAX_SPEC_BYTES {
        return Err(bad_request(format!(
            "spec is over the {}-byte limit",
            workflow::MAX_SPEC_BYTES
        )));
    }
    let max_price = match req.max_price {
        None => state.max_price_usd,
        Some(p) if p.is_finite() && p >= 0.0 => p.min(state.max_price_usd),
        Some(_) => return Err(bad_request("max_price must be a non-negative number")),
    };
    let allowed = effective_allow_peers(&state.allow_peers, req.allow_peers.as_deref())?;
    let timeout_ms = req
        .timeout_ms
        .unwrap_or(DEFAULT_TIMEOUT_MS)
        .clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS);
    let timeout = Duration::from_millis(timeout_ms);

    match tokio::time::timeout(
        timeout,
        run_job(
            state, workload, req.spec, spec_bytes, max_price, allowed, timeout,
        ),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(ApiError(
            StatusCode::GATEWAY_TIMEOUT,
            json!({ "error": "timeout" }),
        )),
    }
}

/// The request's `allow_peers` narrows the bridge's global list; it may not
/// widen it.
fn effective_allow_peers(
    global: &[String],
    requested: Option<&[String]>,
) -> Result<Vec<String>, ApiError> {
    let requested = requested.unwrap_or(&[]);
    if requested.len() > MAX_ALLOW_PEERS {
        return Err(bad_request(format!(
            "allow_peers holds at most {MAX_ALLOW_PEERS} peer ids"
        )));
    }
    if let Some(bad) = requested
        .iter()
        .find(|p| p.parse::<c0mpute_net::PeerId>().is_err())
    {
        return Err(bad_request(format!(
            "invalid peer id {bad:?} in allow_peers"
        )));
    }
    if requested.is_empty() {
        return Ok(global.to_vec());
    }
    if !global.is_empty()
        && let Some(outside) = requested.iter().find(|p| !global.contains(p))
    {
        return Err(bad_request(format!(
            "peer {outside} is outside the bridge's C0MPUTE_JOBS_ALLOW_PEERS"
        )));
    }
    Ok(requested.to_vec())
}

async fn run_job(
    state: &JobsBridge,
    workload: Workload,
    spec: Value,
    spec_bytes: Vec<u8>,
    max_price: f64,
    allowed: Vec<String>,
    timeout: Duration,
) -> Result<Value, ApiError> {
    let started = tokio::time::Instant::now();
    let _slot = state.slots.clone().acquire_owned().await.map_err(|_| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({ "error": "no_bids", "detail": "bridge is shutting down" }),
        )
    })?;

    // Small specs ride in the gossip offer; large ones are served from this
    // node's store and fetched by the winner by hash.
    let spec_inline = if spec_bytes.len() <= workflow::MAX_INLINE_SPEC_BYTES {
        spec
    } else {
        let hash = state.store.put(&spec_bytes).await.map_err(internal)?;
        state.net.announce(&hash).await.map_err(internal)?;
        workflow::spec_ref(&hash, spec_bytes.len())
    };

    let remaining = timeout.saturating_sub(started.elapsed());
    let auction = JobAuction::new(workload.name(), spec_inline)
        .with_required_capabilities(workload.required_capabilities())
        .with_allowed_bidders(allowed)
        .with_max_price_usd(max_price)
        .with_bid_window(state.bid_window)
        .with_deadline(remaining)
        .with_receipt_timeout(remaining);
    let outcome = run_auction(state.net.clone(), auction)
        .await
        .map_err(|error| match error.downcast_ref::<AuctionFailure>() {
            Some(AuctionFailure::NoReceipt(_)) => {
                ApiError(StatusCode::GATEWAY_TIMEOUT, json!({ "error": "timeout" }))
            }
            // No bids, or nobody on the topic to publish to.
            _ => ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                json!({ "error": "no_bids", "detail": error.to_string() }),
            ),
        })?;

    let worker = outcome.accepted_bid.bidder_peer_id.clone();
    let worker_failed = |detail: &str| {
        ApiError(
            StatusCode::BAD_GATEWAY,
            json!({ "error": "worker_failed", "detail": detail, "job_id": outcome.job_id, "worker": worker }),
        )
    };
    if outcome.receipt.status != JobStatus::Completed {
        return Err(worker_failed("worker reported the job failed"));
    }
    let Some(output_hash) = outcome.receipt.output_hash.clone() else {
        return Err(worker_failed("worker returned no output hash"));
    };
    let fetch_budget = timeout
        .saturating_sub(started.elapsed())
        .min(Duration::from_secs(20));
    let bytes = match fetch_verified(
        &state.net,
        &output_hash,
        &worker,
        workload.max_result_bytes(),
        fetch_budget,
    )
    .await
    {
        Ok(bytes) => bytes,
        Err(FetchError::Invalid(detail)) => return Err(worker_failed(detail)),
        Err(FetchError::Timeout(detail)) => {
            tracing::warn!(err = %detail, "jobs bridge: timed out fetching result");
            return Err(ApiError(
                StatusCode::GATEWAY_TIMEOUT,
                json!({ "error": "timeout" }),
            ));
        }
    };
    let result: Value =
        serde_json::from_slice(&bytes).map_err(|_| worker_failed("worker result is not JSON"))?;
    if !workload.valid_result(&result) {
        return Err(worker_failed(
            "worker result does not match the result schema",
        ));
    }
    Ok(json!({
        "job_id": outcome.job_id,
        "worker": worker,
        "price": outcome.accepted_bid.price_usd,
        "output_hash": output_hash,
        "result": result,
    }))
}

fn internal(error: anyhow::Error) -> ApiError {
    tracing::warn!(err = %error, "jobs bridge operation failed");
    ApiError(
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({ "error": "internal", "detail": "jobs bridge operation failed" }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workloads() {
        assert_eq!(
            Workload::parse("workflow.step"),
            Some(Workload::WorkflowStep)
        );
        assert_eq!(Workload::parse("ffmpeg.transcode"), None);
        assert!(Workload::WorkflowStep.valid_result(&json!({"v":1,"ok":true,"output":1})));
        assert!(!Workload::WorkflowStep.valid_result(&json!({"v":2,"ok":true})));
        assert!(!Workload::WorkflowStep.valid_result(&json!({"v":1})));
    }

    #[test]
    fn allow_peers_narrow_never_widen() {
        let peer_a = c0mpute_net::PeerId::random().to_base58();
        let peer_b = c0mpute_net::PeerId::random().to_base58();
        let global = vec![peer_a.clone()];
        assert_eq!(effective_allow_peers(&global, None).unwrap(), global);
        assert_eq!(
            effective_allow_peers(&[], None).unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(
            effective_allow_peers(&[], Some(std::slice::from_ref(&peer_b))).unwrap(),
            vec![peer_b.clone()]
        );
        assert!(effective_allow_peers(&global, Some(std::slice::from_ref(&peer_b))).is_err());
        assert!(effective_allow_peers(&[], Some(&["not-a-peer".to_string()])).is_err());
    }

    #[test]
    fn allow_peers_env_parsing() {
        assert_eq!(parse_allow_peers(" a, ,b ,"), vec!["a", "b"]);
        assert!(parse_allow_peers("").is_empty());
    }
}
