//! `c0mpute jobs serve`: an authenticated HTTP bridge from an application
//! (meshhook) to generic c0mpute jobs. DIP 0030.
//!
//! `POST /v1/jobs` runs one auction for the named workload, waits for the
//! winner's receipt, fetches the result by the receipt's hash from the
//! winner, checks the bytes against that hash, and answers with the result.
//! Accepts `workflow.step`, `crawl.fetch`, `ocr.extract` (input by hash),
//! `embed.text`, `captcha.solve` (images inline), `blender.render-frame` (input pieces by hash; the bridge
//! fetches and checks the frames and returns them base64-encoded) and
//! `infernet.inference` (chat completions), and `container.run` (DIP 0033:
//! upload the context with `POST /v1/blobs`, name it as `context_blob` /
//! `input_blob`, download the image and outputs from `GET /v1/blobs/{id}`);
//! [`Workload`] is where more go.
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
use c0mpute_net::Libp2pNetwork;
use c0mpute_store::ChunkStore;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tower_http::trace::TraceLayer;

use crate::buyer::AuctionFailure;
use crate::gateway_util::{BearerCheck, FetchError, check_bearer, fetch_result};
use crate::{Config, JobAuction, Supervisor, run_auction, workflow};

pub const DEFAULT_BIND: &str = "127.0.0.1:8788";
pub const DEFAULT_TIMEOUT_MS: u64 = 60_000;
pub const MIN_TIMEOUT_MS: u64 = 1_000;
/// 15 min. Every live inference worker is a CPU node: an agent-sized chat turn
/// (1k+ tokens on a 7B-9B model) takes 3-5 min, and at 300 s the bridge threw
/// away answers that landed seconds later. The requester picks its own budget
/// under this ceiling.
pub const MAX_TIMEOUT_MS: u64 = 900_000;
pub const DEFAULT_BID_WINDOW: Duration = Duration::from_secs(2);
/// Jobs in flight through one bridge; more wait for a slot (inside their
/// own timeout).
pub const MAX_CONCURRENT_JOBS: usize = 32;
/// Request envelope around a spec of at most [`workflow::MAX_SPEC_BYTES`].
const MAX_REQUEST_BYTES: usize = workflow::MAX_SPEC_BYTES + 64 * 1024;
const MAX_ALLOW_PEERS: usize = 64;

/// Workloads the bridge runs. Each knows its required capability tags, how
/// big a result it accepts, and how to check the spec and the result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Workload {
    WorkflowStep,
    CrawlFetch,
    OcrExtract,
    EmbedText,
    CaptchaSolve,
    RenderFrame,
    Inference,
}

impl Workload {
    const ALL: &'static [Workload] = &[
        Workload::WorkflowStep,
        Workload::CrawlFetch,
        Workload::OcrExtract,
        Workload::EmbedText,
        Workload::CaptchaSolve,
        Workload::RenderFrame,
        Workload::Inference,
    ];

    fn parse(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|w| w.name() == name)
    }
    fn name(self) -> &'static str {
        match self {
            Workload::WorkflowStep => workflow::WORKLOAD,
            Workload::CrawlFetch => crate::crawl::WORKLOAD,
            Workload::OcrExtract => crate::ocr::WORKLOAD,
            Workload::EmbedText => crate::embed::WORKLOAD,
            Workload::CaptchaSolve => crate::captcha::WORKLOAD,
            Workload::RenderFrame => crate::render::WORKLOAD,
            Workload::Inference => crate::inference::WORKLOAD,
        }
    }
    /// Reject a spec no worker could run before auctioning it. `workflow.step`
    /// specs are checked by the worker (they are a small language of their own).
    fn check_spec(self, spec: &Value) -> Result<(), String> {
        match self {
            Workload::WorkflowStep => Ok(()),
            Workload::CrawlFetch => crate::crawl::parse_spec(spec).map(|_| ()),
            Workload::OcrExtract => crate::ocr::parse_spec(spec).map(|_| ()),
            Workload::EmbedText => crate::embed::parse_spec(spec).map(|_| ()),
            Workload::CaptchaSolve => crate::captcha::parse_spec(spec).map(|_| ()),
            Workload::RenderFrame => crate::render::parse_spec(spec).map(|_| ()),
            Workload::Inference => crate::inference::parse_spec(spec).map(|_| ()),
        }
    }
    fn required_capabilities(self, spec: &Value) -> Vec<String> {
        match self {
            Workload::WorkflowStep => vec![workflow::ROLE_TAG.to_string()],
            Workload::CrawlFetch => vec![crate::crawl::ROLE_TAG.to_string()],
            Workload::OcrExtract => vec![crate::ocr::ROLE_TAG.to_string()],
            Workload::EmbedText => {
                let model = crate::embed::parse_spec(spec)
                    .map(|(_, m)| m)
                    .unwrap_or_else(|_| crate::embed::default_model());
                vec![
                    crate::embed::ROLE_TAG.to_string(),
                    crate::embed::model_tag(model),
                ]
            }
            Workload::CaptchaSolve => vec![crate::captcha::ROLE_TAG.to_string()],
            Workload::RenderFrame => crate::render::required_capabilities_for(spec),
            Workload::Inference => crate::inference::required_capabilities(spec),
        }
    }
    fn max_result_bytes(self) -> usize {
        match self {
            Workload::WorkflowStep | Workload::CrawlFetch => workflow::MAX_RESULT_BYTES,
            Workload::OcrExtract => crate::ocr::MAX_RESULT_BYTES,
            Workload::EmbedText => crate::embed::MAX_RESULT_BYTES,
            Workload::CaptchaSolve => crate::captcha::MAX_RESULT_BYTES,
            Workload::RenderFrame => crate::render::MAX_RESULT_BYTES,
            Workload::Inference => crate::inference::MAX_RESULT_BYTES,
        }
    }
    fn valid_result(self, spec: &Value, value: &Value) -> bool {
        match self {
            Workload::WorkflowStep | Workload::CrawlFetch => {
                value.get("v").and_then(Value::as_u64) == Some(1)
                    && value.get("ok").is_some_and(Value::is_boolean)
            }
            Workload::OcrExtract => crate::ocr::valid_result(value),
            Workload::EmbedText => crate::embed::parse_spec(spec).is_ok_and(|(s, model)| {
                crate::embed::validate_result(value, s.texts.len(), model).is_ok()
            }),
            Workload::CaptchaSolve => crate::captcha::parse_spec(spec)
                .is_ok_and(|parsed| crate::captcha::valid_result(&parsed.kind, value)),
            Workload::RenderFrame => crate::render::parse_spec(spec)
                .is_ok_and(|parsed| crate::render::valid_result(&parsed, value)),
            Workload::Inference => crate::inference::valid_result(value),
        }
    }
}

/// Bridge state. Build with [`JobsBridge::new`] and mount [`router`]; the
/// CLI does both in [`serve`].
#[derive(Clone)]
pub struct JobsBridge {
    net: Arc<Libp2pNetwork>,
    // Specs used to be served from here by hash; sealed specs (DIP-0032)
    // go point to point instead. Kept for the bridge's own inputs.
    store: ChunkStore,
    token: Arc<str>,
    max_price_usd: f64,
    allow_peers: Arc<Vec<String>>,
    bid_window: Duration,
    slots: Arc<Semaphore>,
    /// Workers that won and then failed or went silent, benched for this
    /// bridge's auctions (30 min, doubling to 6 h; a delivery clears it).
    /// Benched workers still win when nobody else bids. Persisted: the bridge
    /// restarts on every auto-update.
    penalties: crate::penalty_box::PenaltyBox,
    /// Whose turn it is to auction, per queue (workload + tags): oldest
    /// waiting job first, requeued jobs keep their first enqueue time.
    turns: crate::buyer_queue::AuctionTurns,
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
            penalties: crate::penalty_box::PenaltyBox::persistent(
                crate::config::data_dir()
                    .unwrap_or_else(|| std::path::PathBuf::from("./c0mpute-data"))
                    .join("jobs-bridge-penalties.json"),
            ),
            turns: crate::buyer_queue::AuctionTurns::default(),
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
        w.check_spec(&spec).map_err(anyhow::Error::msg)?;
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
    submit_once_with_input(workload, None, |_| spec, max_price_usd, timeout).await
}

/// [`submit_once`] for a job that reads an input blob (`c0mpute ocr`): the
/// buyer stores and announces `input` first, as
/// [`crate::pieces::INPUT_PIECE_BYTES`] pieces, then `build_spec` gets the
/// stored pieces and this buyer's peer id (the provider the worker fetches
/// from). [`crate::pieces::Pieces::spec_fields`] turns the pieces into spec
/// fields.
pub async fn submit_once_with_input(
    workload: &str,
    input: Option<Vec<u8>>,
    build_spec: impl FnOnce(Option<(crate::pieces::Pieces, String)>) -> Value,
    max_price_usd: f64,
    timeout: Duration,
) -> Result<Value> {
    let supervisor = Supervisor::boot_buyer(Config::default()).await?;
    let blob = match input {
        Some(bytes) => {
            let pieces = crate::pieces::put_bytes_pieces(
                &supervisor.libp2p,
                &supervisor.store,
                &bytes,
                crate::pieces::INPUT_PIECE_BYTES,
            )
            .await
            .context("store job input")?;
            Some((pieces, supervisor.libp2p.peer_id().to_base58()))
        }
        None => None,
    };
    let spec = build_spec(blob);
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
        .route(
            "/v1/blobs",
            post(upload_blob).layer(DefaultBodyLimit::max(crate::container_run::MAX_INPUT_BYTES as usize)),
        )
        .route("/v1/blobs/{id}", get(download_blob))
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
    let local = listener.local_addr().unwrap_or(bind);
    axum::serve(listener, c0mpute_gateway::host_guard::guard_loopback(router(state), local)).await?;
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
    let mut workloads: Vec<&str> = Workload::ALL.iter().map(|w| w.name()).collect();
    workloads.push(crate::container_run::WORKLOAD);
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
    /// Answer as NDJSON: `queued` / `running` events while the job waits and
    /// runs, then one `result` (the usual body) or `error` line. Also chosen
    /// by `Accept: application/x-ndjson`.
    #[serde(default)]
    stream: bool,
}

async fn submit(State(state): State<JobsBridge>, headers: HeaderMap, body: Bytes) -> Response {
    let wants_stream = headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("application/x-ndjson"))
        || serde_json::from_slice::<Value>(&body).ok().and_then(|v| v.get("stream").and_then(Value::as_bool)) == Some(true);
    if !wants_stream {
        return match submit_inner(&state, &headers, &body, None).await {
            Ok(v) => (StatusCode::OK, Json(v)).into_response(),
            Err(e) => e.into_response(),
        };
    }
    // Streaming: refuse bad requests with a plain status, then stream.
    match check_bearer(&headers, &state.token) {
        BearerCheck::Ok => {}
        _ => return ApiError(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" })).into_response(),
    }
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let done = tx.clone();
    tokio::spawn(async move {
        let line = match submit_inner(&state, &headers, &body, Some(tx)).await {
            Ok(v) => json!({"event": "result", "status": 200, "body": v}),
            Err(ApiError(status, body)) => json!({"event": "error", "status": status.as_u16(), "body": body}),
        };
        let _ = done.send(line);
    });
    // One JSON object per line; the stream ends after the final line.
    let lines = futures::stream::unfold((rx, false), |(mut rx, ended)| async move {
        if ended {
            return None;
        }
        let v = rx.recv().await?;
        let last = matches!(v.get("event").and_then(Value::as_str), Some("result" | "error"));
        let mut line = serde_json::to_vec(&v).unwrap_or_default();
        line.push(b'\n');
        Some((Ok::<_, std::convert::Infallible>(Bytes::from(line)), (rx, last)))
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "application/x-ndjson")
        .header(axum::http::header::CACHE_CONTROL, "no-cache")
        .body(axum::body::Body::from_stream(lines))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

async fn submit_inner(
    state: &JobsBridge,
    headers: &HeaderMap,
    body: &[u8],
    progress: Option<Progress>,
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
    // Read by `submit` (it picks the response form before parsing).
    let _ = req.stream;
    if req.workload == crate::container_run::WORKLOAD {
        return run_container_job(state, req).await;
    }
    let workload = Workload::parse(&req.workload).ok_or_else(|| {
        ApiError(
            StatusCode::BAD_REQUEST,
            json!({ "error": "unknown_workload", "detail": format!("unsupported workload {:?}", req.workload) }),
        )
    })?;
    if !req.spec.is_object() {
        return Err(bad_request("spec must be a JSON object"));
    }
    workload.check_spec(&req.spec).map_err(bad_request)?;
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
    let timeout = request_timeout(req.timeout_ms);

    match tokio::time::timeout(
        timeout,
        run_job_with_progress(
            state, workload, req.spec, spec_bytes, max_price, allowed, timeout, progress,
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

/// The request's `timeout_ms`, defaulted and clamped to the bridge's range.
fn request_timeout(timeout_ms: Option<u64>) -> Duration {
    Duration::from_millis(
        timeout_ms
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS),
    )
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

/// Events a streaming caller sees while its job waits and runs (NDJSON, one
/// object per line): `queued` (position in the queue, why it waits),
/// `running` (the worker and its ETA), then one `result` or `error`.
pub type Progress = tokio::sync::mpsc::UnboundedSender<Value>;

fn emit(progress: Option<&Progress>, event: Value) {
    if let Some(p) = progress {
        let _ = p.send(event);
    }
}

/// What a waiting job is waiting for, in words a client can show. Any
/// capable node takes it, GPU or CPU, whichever frees a slot first (bids are
/// ranked by ETA, so a free GPU wins over a free CPU node).
fn waiting_for(workload: Workload) -> &'static str {
    match workload {
        Workload::Inference => "waiting for a free inference slot (GPU or CPU)",
        _ => "waiting for a free worker slot",
    }
}

/// Why one round did not answer the job, and whether it goes back in line.
#[derive(Debug)]
enum Round {
    /// Nobody bid (every capable worker full), or the winner turned it away,
    /// failed it, went silent or its runtime was busy: back to the head of the
    /// queue (it keeps its enqueue time), re-auctioned when a slot frees.
    Requeue(String),
    /// A final answer for the caller.
    Done(Result<Value, ApiError>),
}

/// An inference answer that failed because the worker's runtime was busy or
/// slow to start, not because of the request: worth another worker.
pub fn runtime_busy_answer(result: &Value) -> bool {
    if result.get("ok").and_then(Value::as_bool) != Some(false) {
        return false;
    }
    let e = result.get("error").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
    ["timed out", "timeout", "reach the local runtime", "503", "busy", "overloaded", "no model is served", "not served here", "connection refused"]
        .iter()
        .any(|k| e.contains(k))
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
    run_job_with_progress(state, workload, spec, spec_bytes, max_price, allowed, timeout, None).await
}

/// Run one job until it is answered or its own deadline passes. A round that
/// gets no bid, or whose worker turns the job away, puts it back at the head
/// of its queue (FIFO among waiting jobs by first enqueue time, see
/// [`crate::buyer_queue::AuctionTurns`]); it is re-auctioned as soon as a
/// capable worker announces itself (workers re-advertise when a job ends and
/// frees a slot) or after a short backoff.
#[allow(clippy::too_many_arguments)]
async fn run_job_with_progress(
    state: &JobsBridge,
    workload: Workload,
    spec: Value,
    spec_bytes: Vec<u8>,
    max_price: f64,
    allowed: Vec<String>,
    timeout: Duration,
    progress: Option<Progress>,
) -> Result<Value, ApiError> {
    let started = tokio::time::Instant::now();
    let deadline = started + timeout;
    let _slot = state.slots.clone().acquire_owned().await.map_err(|_| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({ "error": "no_bids", "detail": "bridge is shutting down" }),
        )
    })?;
    let _ = spec_bytes;

    // Routing: a plain-text prompt for a standard model also accepts smaller
    // standard models, so a free worker answers it instead of the job
    // queuing on a working one (inference::with_alternatives). Agent tool
    // loops, JSON output and big prompts keep exactly the model they asked for.
    let widened = if matches!(workload, Workload::Inference) {
        crate::inference::with_alternatives(&spec)
    } else {
        None
    };
    let exact_spec = widened.as_ref().map(|_| spec.clone());
    let spec = widened.unwrap_or(spec);
    let required = workload.required_capabilities(&spec);

    let queued_at_ms = crate::job_progress::now_ms_pub();
    let key = uuid::Uuid::new_v4().to_string();
    let queue = crate::buyer_queue::AuctionTurns::queue_of(workload.name(), &required);
    let mut rounds = 0u32;
    let mut last_reason = String::new();
    // Held through no-bid rounds (so a freed slot goes to this, the oldest
    // job, not a newer one); handed on when a worker accepts.
    let mut turn: Option<crate::buyer_queue::AuctionTurn> = None;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return Err(gave_up(workload, &spec, &required, &last_reason, timeout));
        }
        // Our turn: the oldest waiting job of this queue auctions first. A job
        // whose worker turned it away re-enters with its first enqueue time,
        // so it is next.
        if turn.is_none() {
            turn = {
            let waiting = state.turns.acquire(&queue, &key, queued_at_ms);
            tokio::pin!(waiting);
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            loop {
                tokio::select! {
                    t = &mut waiting => break Some(t),
                    _ = tokio::time::sleep_until(deadline) => break None,
                    _ = tick.tick(), if rounds > 0 || state.turns.waiting(&queue) > 0 => {
                        let (position, length) = state.turns.position(&queue, &key).unwrap_or((1, 1));
                        emit(progress.as_ref(), json!({"event": "queued", "position": position, "queue_length": length,
                            "status": waiting_for(workload), "waited_ms": started.elapsed().as_millis() as u64}));
                    }
                }
            }
            };
            if turn.is_none() {
                return Err(gave_up(workload, &spec, &required, &last_reason, timeout));
            }
        }
        match run_round(state, workload, &spec, exact_spec.as_ref(), &required, max_price, &allowed, deadline, started, &mut turn, progress.as_ref()).await {
            Round::Done(r) => return r,
            Round::Requeue(reason) => {
                rounds += 1;
                let left = deadline.saturating_duration_since(tokio::time::Instant::now());
                tracing::info!(workload = workload.name(), rounds, waited_secs = started.elapsed().as_secs(), left_secs = left.as_secs(),
                    reason = %reason, "jobs bridge: no free worker; job back at the head of its queue");
                let (position, length) = state.turns.position(&queue, &key).unwrap_or((1, state.turns.waiting(&queue).max(1)));
                emit(progress.as_ref(), json!({"event": "queued", "position": position, "queue_length": length,
                    "status": waiting_for(workload), "reason": reason, "waited_ms": started.elapsed().as_millis() as u64}));
                last_reason = reason;
                // Wake on a capable worker's announcement (a slot freed), else
                // after the backoff; never past the deadline.
                let pause = requeue_pause(rounds, left);
                crate::speech_gateway::wait_for_capable(&state.net, &required, pause, Duration::from_millis(250)).await;
            }
        }
    }
}

/// Fallback pause between rounds when no worker announces a free slot:
/// 2, 4, 8, then 10 s, never past the deadline.
fn requeue_pause(rounds: u32, left: Duration) -> Duration {
    Duration::from_secs(2u64 << rounds.saturating_sub(1).min(3)).min(Duration::from_secs(10)).min(left)
}

fn gave_up(workload: Workload, spec: &Value, required: &[String], last_reason: &str, waited: Duration) -> ApiError {
    let model = crate::buyer_queue::model_of(spec, required);
    let detail = crate::buyer_queue::expired_message(workload.name(), model.as_deref(), waited);
    if last_reason.is_empty() {
        ApiError(StatusCode::SERVICE_UNAVAILABLE, json!({ "error": "no_bids", "detail": detail }))
    } else {
        ApiError(StatusCode::SERVICE_UNAVAILABLE, json!({ "error": "no_bids", "detail": format!("{detail} ({last_reason})") }))
    }
}

/// One auction of the job (the widened spec, then the exact one when nobody
/// that understands alternatives bid), holding the queue's turn until a
/// worker accepts.
#[allow(clippy::too_many_arguments)]
async fn run_round(
    state: &JobsBridge,
    workload: Workload,
    spec: &Value,
    exact_spec: Option<&Value>,
    required: &[String],
    max_price: f64,
    allowed: &[String],
    deadline: tokio::time::Instant,
    started: tokio::time::Instant,
    turn: &mut Option<crate::buyer_queue::AuctionTurn>,
    progress: Option<&Progress>,
) -> Round {
    // The awarded job, so a winner that goes silent is told to stop before
    // the job is auctioned again.
    let in_flight: Arc<std::sync::Mutex<Option<crate::job_e2e::InFlightJob>>> = Arc::default();
    let make = |spec: &Value, required: Vec<String>| {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let record = in_flight.clone();
        JobAuction::new(workload.name(), spec.clone())
            .with_on_accept(Arc::new(move |job| {
                if let Ok(mut j) = record.lock() {
                    *j = Some(job.clone());
                }
            }))
            // A failed or silent winner's job comes back here and goes
            // through the queue again (front, by its enqueue time), never
            // straight to another bidder past the jobs waiting ahead.
            .with_reawards(false)
            .with_required_capabilities(required)
            .with_allowed_bidders(allowed.to_vec())
            .with_fallback_bidders(state.penalties.benched())
            .with_max_price_usd(max_price)
            .with_bid_window(state.bid_window)
            .with_deadline(remaining)
            .with_receipt_timeout(remaining)
    };
    let mut result = auction_releasing_turn(state, make(spec, required.to_vec()), turn, progress).await;
    if let (Err(error), Some(exact)) = (&result, exact_spec)
        && !matches!(error.downcast_ref::<AuctionFailure>(), Some(AuctionFailure::NoReceipt(_)))
    {
        // Nobody that understands alternatives bid (workers older than
        // 0.3.71): ask again for exactly the model the caller named.
        tracing::info!(err = %error, "jobs bridge: no bids on the widened inference offer, retrying with the exact model");
        result = auction_releasing_turn(state, make(exact, workload.required_capabilities(exact)), turn, progress).await;
    }
    let outcome = match result {
        Ok(o) => o,
        Err(error) => {
            if let Some(job) = in_flight.lock().ok().and_then(|mut j| j.take()) {
                let net = state.net.clone();
                tokio::spawn(async move { crate::buyer::cancel_job(&net, &job).await });
            }
            return Round::Requeue(match error.downcast_ref::<AuctionFailure>() {
                Some(AuctionFailure::NoReceipt(_)) => "the worker did not answer in time".to_string(),
                _ => "every capable worker is busy".to_string(),
            });
        }
    };

    let worker = outcome.accepted_bid.bidder_peer_id.clone();
    // A worker that won and failed (or went silent) loses its turn for a
    // while; one that delivered is in good standing again. Before this the
    // bridge never benched anyone: a node that could not take jobs kept
    // winning and cost each one a ~50 s re-award (sergey-234, 2026-11-15).
    for failed in &outcome.failed_workers {
        let bench = state.penalties.strike(failed);
        tracing::info!(worker = %failed, bench_secs = bench.as_secs(), "jobs bridge: worker failed a job, benched");
    }
    if outcome.receipt.status != JobStatus::Completed {
        // Turned away or failed on the worker: another round, not an error.
        return Round::Requeue(format!("worker {worker} turned the job away"));
    }
    state.penalties.clear(&worker);
    let worker_failed = |detail: &str| {
        Round::Done(Err(ApiError(
            StatusCode::BAD_GATEWAY,
            json!({ "error": "worker_failed", "detail": detail, "job_id": outcome.job_id, "worker": worker }),
        )))
    };
    let Some(output_hash) = outcome.receipt.output_hash.clone() else {
        return worker_failed("worker returned no output hash");
    };
    let fetch_budget = deadline.saturating_duration_since(tokio::time::Instant::now()).min(Duration::from_secs(10 * 60));
    let bytes = match fetch_result(&state.net, &outcome, workload.max_result_bytes(), fetch_budget).await {
        Ok(bytes) => bytes,
        Err(FetchError::Invalid(detail)) => return worker_failed(detail),
        Err(FetchError::Timeout(detail)) => {
            tracing::warn!(err = %detail, "jobs bridge: timed out fetching result");
            return Round::Done(Err(ApiError(StatusCode::GATEWAY_TIMEOUT, json!({ "error": "timeout" }))));
        }
    };
    let Ok(result) = serde_json::from_slice::<Value>(&bytes) else {
        return worker_failed("worker result is not JSON");
    };
    if !workload.valid_result(spec, &result) {
        return worker_failed("worker result does not match the result schema");
    }
    // The worker's runtime was busy or timed out: that is capacity, not the
    // request. Back in line (unless the deadline is all but spent).
    if workload == Workload::Inference
        && runtime_busy_answer(&result)
        && deadline.saturating_duration_since(tokio::time::Instant::now()) > Duration::from_secs(5)
    {
        let e = result.get("error").and_then(Value::as_str).unwrap_or("").chars().take(160).collect::<String>();
        return Round::Requeue(format!("worker {worker} runtime busy: {e}"));
    }
    let result = if workload == Workload::RenderFrame {
        let budget = deadline.saturating_duration_since(tokio::time::Instant::now());
        match attach_render_frames(state, spec, result, &worker, &outcome.result_key, budget).await {
            Ok(r) => r,
            Err(error) => return worker_failed(&format!("{error:#}")),
        }
    } else {
        result
    };
    let _ = started;
    Round::Done(Ok(json!({
        "job_id": outcome.job_id,
        "worker": worker,
        "price": outcome.accepted_bid.price_usd,
        "output_hash": output_hash,
        "result": result,
    })))
}

/// Run `auction`, handing the queue's turn on as soon as a worker accepts
/// (not when its answer arrives), so the next waiting job auctions while
/// this one runs.
async fn auction_releasing_turn(
    state: &JobsBridge,
    auction: JobAuction,
    turn: &mut Option<crate::buyer_queue::AuctionTurn>,
    progress: Option<&Progress>,
) -> Result<crate::AuctionOutcome> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let run = run_auction(state.net.clone(), auction.with_accept_notify(tx));
    tokio::pin!(run);
    loop {
        tokio::select! {
            r = &mut run => return r,
            Some((job_id, bid)) = rx.recv() => {
                turn.take();
                emit(progress, json!({"event": "running", "job_id": job_id, "worker": bid.bidder_peer_id, "eta_s": bid.eta_seconds}));
            }
        }
    }
}

// ─── container.run over HTTP (DIP 0033) ──────────────────────────────────

/// How long uploaded and downloaded blobs stay on the bridge.
const BLOB_TTL: Duration = Duration::from_secs(3600);

fn blob_dir() -> std::path::PathBuf {
    crate::config::data_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("./c0mpute-data"))
        .join("jobs-bridge")
        .join("blobs")
}

fn valid_blob_id(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok_and(|u| u.to_string() == id)
}

/// Drop blobs older than [`BLOB_TTL`].
async fn sweep_blobs(dir: &std::path::Path) {
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else { return };
    while let Ok(Some(e)) = rd.next_entry().await {
        let old = e
            .metadata()
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > BLOB_TTL);
        if old {
            let path = e.path();
            let _ = if path.is_dir() { tokio::fs::remove_dir_all(&path).await } else { tokio::fs::remove_file(&path).await };
        }
    }
}

fn unauthorized() -> ApiError {
    ApiError(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }))
}

/// `POST /v1/blobs`: store a build context or input tarball (the raw body,
/// up to 2 GiB) for a later `container.run`; answers `{blob, size}`.
async fn upload_blob(State(state): State<JobsBridge>, headers: HeaderMap, body: axum::body::Body) -> Response {
    use futures::TryStreamExt;
    use tokio::io::AsyncWriteExt;
    if !matches!(check_bearer(&headers, &state.token), BearerCheck::Ok) {
        return unauthorized().into_response();
    }
    let dir = blob_dir();
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        return ApiError(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": "storage", "detail": e.to_string()})).into_response();
    }
    sweep_blobs(&dir).await;
    let id = uuid::Uuid::new_v4().to_string();
    let path = dir.join(&id);
    let result: Result<u64> = async {
        let mut file = tokio::fs::File::create(&path).await?;
        let mut stream = body.into_data_stream();
        let mut size = 0u64;
        while let Some(chunk) = stream.try_next().await.map_err(|e| anyhow::anyhow!("request body: {e}"))? {
            size += chunk.len() as u64;
            if size > crate::container_run::MAX_INPUT_BYTES {
                bail!("blob is over the {}-byte limit", crate::container_run::MAX_INPUT_BYTES);
            }
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        if size == 0 {
            bail!("empty body");
        }
        Ok(size)
    }
    .await;
    match result {
        Ok(size) => (StatusCode::CREATED, Json(json!({ "blob": id, "size": size }))).into_response(),
        Err(e) => {
            let _ = tokio::fs::remove_file(&path).await;
            bad_request(format!("{e:#}")).into_response()
        }
    }
}

/// `GET /v1/blobs/{id}`: an uploaded blob, or an image / output a
/// `container.run` job returned (listed in its response).
async fn download_blob(
    State(state): State<JobsBridge>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    if !matches!(check_bearer(&headers, &state.token), BearerCheck::Ok) {
        return unauthorized().into_response();
    }
    if !valid_blob_id(&id) {
        return bad_request("unknown blob").into_response();
    }
    let path = blob_dir().join(&id);
    let Ok(file) = tokio::fs::File::open(&path).await else {
        return ApiError(StatusCode::NOT_FOUND, json!({"error": "not_found"})).into_response();
    };
    let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    let stream = futures::stream::unfold(file, |mut f| async move {
        use tokio::io::AsyncReadExt;
        let mut buf = vec![0u8; 256 * 1024];
        match f.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok::<_, std::io::Error>(Bytes::from(buf)), f))
            }
            Err(e) => Some((Err(e), f)),
        }
    });
    (
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, "application/gzip".to_string()),
            (axum::http::header::CONTENT_LENGTH, len.to_string()),
        ],
        axum::body::Body::from_stream(stream),
    )
        .into_response()
}

/// A `container.run` request: the container.run spec with blobs named by
/// upload id (`context_blob`, `input_blob`) instead of piece hashes.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContainerHttpSpec {
    #[serde(default)]
    context_blob: Option<String>,
    #[serde(default)]
    input_blob: Option<String>,
    #[serde(default)]
    build: Option<crate::container_run::BuildSpec>,
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    cmd: Vec<String>,
    #[serde(default)]
    env: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    workdir: Option<String>,
    #[serde(default)]
    outputs: Vec<String>,
    #[serde(default)]
    return_image: bool,
    #[serde(default)]
    cpus: Option<f64>,
    #[serde(default)]
    memory_mb: Option<u64>,
    #[serde(default)]
    disk_mb: Option<u64>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    network: Option<bool>,
    /// Accept any builder when neither the request nor the bridge names any.
    #[serde(default)]
    any_worker: bool,
}

async fn run_container_job(state: &JobsBridge, req: JobRequest) -> Result<Value, ApiError> {
    let spec: ContainerHttpSpec =
        serde_json::from_value(req.spec).map_err(|e| bad_request(format!("invalid container.run spec: {e}")))?;
    let dir = blob_dir();
    let blob = |id: &Option<String>| -> Result<Option<std::path::PathBuf>, ApiError> {
        match id {
            None => Ok(None),
            Some(id) if valid_blob_id(id) && dir.join(id).is_file() => Ok(Some(dir.join(id))),
            Some(id) => Err(bad_request(format!("unknown blob {id:?} (upload it with POST /v1/blobs)"))),
        }
    };
    let builders = effective_allow_peers(&state.allow_peers, req.allow_peers.as_deref())?;
    if builders.is_empty() && !spec.any_worker {
        return Err(bad_request(
            "container.run needs allow_peers (or C0MPUTE_JOBS_ALLOW_PEERS): a builder controls the image it returns; set any_worker to accept anyone",
        ));
    }
    let max_price = match req.max_price {
        None => state.max_price_usd,
        Some(p) if p.is_finite() && p >= 0.0 => p.min(state.max_price_usd),
        Some(_) => return Err(bad_request("max_price must be a non-negative number")),
    };
    let request = crate::container_run::Request {
        context: blob(&spec.context_blob)?,
        input: blob(&spec.input_blob)?,
        build: spec.build,
        image: spec.image,
        cmd: spec.cmd,
        env: spec.env,
        workdir: spec.workdir,
        outputs: spec.outputs,
        return_image: spec.return_image,
        cpus: spec.cpus,
        memory_mb: spec.memory_mb,
        disk_mb: spec.disk_mb,
        timeout_secs: spec.timeout_secs,
        network: spec.network,
        builders,
        any_worker: spec.any_worker,
        max_price_usd: max_price,
    };
    let _slot = state.slots.clone().acquire_owned().await.map_err(|_| {
        ApiError(StatusCode::SERVICE_UNAVAILABLE, json!({ "error": "no_bids", "detail": "bridge is shutting down" }))
    })?;
    let out_dir = dir.join(uuid::Uuid::new_v4().to_string());
    let progress: crate::container_run::Progress = Arc::new(|line: &str| tracing::info!(target: "c0mpute::container", "{line}"));
    let outcome = crate::container_run::submit(state.net.clone(), state.store.clone(), request, &out_dir, progress)
        .await
        .map_err(|e| {
            let detail = format!("{e:#}");
            let status = if detail.contains("no bids") || detail.contains("No bids") || detail.contains("NoPeers") {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::BAD_GATEWAY
            };
            ApiError(status, json!({ "error": if status == StatusCode::SERVICE_UNAVAILABLE { "no_bids" } else { "worker_failed" }, "detail": detail }))
        })?;
    // Expose each fetched artifact as a blob of its own.
    let publish = |path: &std::path::Path| -> Option<String> {
        let id = uuid::Uuid::new_v4().to_string();
        std::fs::rename(path, dir.join(&id)).ok().map(|_| format!("/v1/blobs/{id}"))
    };
    let image_url = outcome.image.as_deref().and_then(publish);
    let outputs: Vec<Value> = outcome
        .outputs
        .iter()
        .map(|(p, f)| json!({ "path": p, "url": publish(f) }))
        .collect();
    let _ = tokio::fs::remove_dir_all(&out_dir).await;
    Ok(json!({
        "job_id": outcome.job_id,
        "worker": outcome.worker,
        "price": outcome.price_usd,
        "result": outcome.result,
        "image_url": image_url,
        "outputs": outputs,
    }))
}

/// Most frame bytes the bridge returns inline in one response.
const MAX_INLINE_FRAME_BYTES: u64 = 64 * 1024 * 1024;

/// Fetch a render job's frames from the worker, check each (hash, image
/// magic, header dimensions), and add them to the result as `data_base64`.
async fn attach_render_frames(
    state: &JobsBridge,
    spec: &Value,
    mut result: Value,
    worker: &str,
    key: &c0mpute_net::topics::JobKey,
    budget: Duration,
) -> Result<Value> {
    use base64::Engine;
    let parsed = crate::render::parse_spec(spec).map_err(anyhow::Error::msg)?;
    let metas = crate::render::result_frames(&parsed, &result)?;
    let total: u64 = metas.iter().map(|m| m.size).sum();
    if total > MAX_INLINE_FRAME_BYTES {
        bail!("frames total {total} bytes, over the bridge's {MAX_INLINE_FRAME_BYTES}-byte limit; use `c0mpute render`");
    }
    let frames = crate::render::fetch_frames(&state.net, worker, &metas, key, budget).await?;
    if let Some(list) = result.get_mut("frames").and_then(Value::as_array_mut) {
        for (entry, (_, bytes)) in list.iter_mut().zip(frames) {
            entry["data_base64"] =
                Value::String(base64::engine::general_purpose::STANDARD.encode(bytes));
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_timeout_allows_a_cpu_agent_turn() {
        assert_eq!(request_timeout(None), Duration::from_secs(60));
        // Infernet asks 420 s for chat on CPU workers; it must not be cut to 300 s.
        assert_eq!(request_timeout(Some(420_000)), Duration::from_secs(420));
        assert_eq!(request_timeout(Some(10_000_000)), Duration::from_secs(15 * 60));
        assert_eq!(request_timeout(Some(5)), Duration::from_secs(1));
    }

    #[test]
    fn busy_runtimes_requeue_and_request_errors_do_not() {
        // The Vast 3060 failure: the worker's Ollama queued the job past the timeout.
        let busy = json!({"v": 1, "ok": false, "error": "reach the local runtime: error sending request for url (http://127.0.0.1:11434/v1/chat/completions): operation timed out"});
        assert!(runtime_busy_answer(&busy));
        assert!(runtime_busy_answer(&json!({"v": 1, "ok": false, "error": "runtime answered 503 Service Unavailable: server busy"})));
        assert!(!runtime_busy_answer(&json!({"v": 1, "ok": false, "error": "messages must hold 1..=64 entries"})));
        assert!(!runtime_busy_answer(&json!({"v": 1, "ok": true, "text": "timed out", "model": "m"})));
    }

    #[test]
    fn requeue_pauses_are_short_and_never_pass_the_deadline() {
        let lots = Duration::from_secs(600);
        let p: Vec<u64> = (1..=6).map(|r| requeue_pause(r, lots).as_secs()).collect();
        assert_eq!(p, vec![2, 4, 8, 10, 10, 10]);
        assert_eq!(requeue_pause(1, Duration::from_millis(300)), Duration::from_millis(300));
    }

    #[test]
    fn workloads() {
        assert_eq!(
            Workload::parse("workflow.step"),
            Some(Workload::WorkflowStep)
        );
        assert_eq!(Workload::parse("ffmpeg.transcode"), None);
        let none = json!({});
        assert!(Workload::WorkflowStep.valid_result(&none, &json!({"v":1,"ok":true,"output":1})));
        assert!(!Workload::WorkflowStep.valid_result(&none, &json!({"v":2,"ok":true})));
        assert!(!Workload::WorkflowStep.valid_result(&none, &json!({"v":1})));
    }

    #[test]
    fn ocr_and_embed_workloads() {
        assert_eq!(Workload::parse("ocr.extract"), Some(Workload::OcrExtract));
        assert_eq!(Workload::parse("embed.text"), Some(Workload::EmbedText));

        let hash = c0mpute_proto::Hash::of(b"page").to_hex();
        let ocr = json!({"v":1,"input_hash":hash,"format":"pdf","lang":"eng"});
        assert!(Workload::OcrExtract.check_spec(&ocr).is_ok());
        assert!(Workload::OcrExtract.check_spec(&json!({"v":1,"input_hash":hash,"format":"bmp"})).is_err());
        assert_eq!(Workload::OcrExtract.required_capabilities(&ocr), vec!["c0mpute:role:ocr"]);
        assert!(Workload::OcrExtract.valid_result(&ocr, &json!({"v":1,"ok":true,"text":"x"})));
        assert!(!Workload::OcrExtract.valid_result(&ocr, &json!({"v":1,"ok":false})));

        let spec = json!({"v":1,"model":"nomic-embed-text-v1.5","texts":["a","b"]});
        assert!(Workload::EmbedText.check_spec(&spec).is_ok());
        assert!(Workload::EmbedText.check_spec(&json!({"v":1,"texts":[]})).is_err());
        assert_eq!(
            Workload::EmbedText.required_capabilities(&spec),
            vec!["c0mpute:role:embed", "c0mpute:embed:model:nomic-embed-text-v1.5"]
        );
        let v = vec![0.5f64; 768];
        let good = json!({"v":1,"ok":true,"dims":768,"vectors":[v.clone(), v.clone()]});
        assert!(Workload::EmbedText.valid_result(&spec, &good));
        let one = json!({"v":1,"ok":true,"dims":768,"vectors":[v]});
        assert!(!Workload::EmbedText.valid_result(&spec, &one));
    }

    #[test]
    fn render_workload() {
        assert_eq!(Workload::parse("blender.render-frame"), Some(Workload::RenderFrame));
        let hash = c0mpute_proto::Hash::of(b"scene").to_hex();
        let spec = json!({"v":1,"input_hash":hash,"input_size":5,"input_pieces":[hash],
            "frame_start":1,"frame_end":1,"device":"GPU"});
        assert!(Workload::RenderFrame.check_spec(&spec).is_ok());
        assert!(Workload::RenderFrame
            .check_spec(&json!({"v":1,"input_hash":hash,"input_size":5,"input_pieces":[hash],
                "frame_start":1,"frame_end":1,"engine":"LUX"}))
            .is_err());
        assert_eq!(
            Workload::RenderFrame.required_capabilities(&spec),
            vec!["c0mpute:role:render", "c0mpute:render:engine:cycles", "c0mpute:render:gpu"]
        );
        let frame = json!({"frame":1,"format":"PNG","width":64,"height":48,"size":5,
            "hash":hash,"pieces":[hash]});
        assert!(Workload::RenderFrame.valid_result(&spec, &json!({"v":1,"ok":true,"frames":[frame]})));
        assert!(!Workload::RenderFrame.valid_result(&spec, &json!({"v":1,"ok":true,"frames":[]})));
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
