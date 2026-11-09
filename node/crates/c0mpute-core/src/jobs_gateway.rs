//! `c0mpute jobs serve`: an authenticated HTTP bridge from an application
//! (meshhook) to generic c0mpute jobs. DIP 0030.
//!
//! `POST /v1/jobs` runs one auction for the named workload, waits for the
//! winner's receipt, fetches the result by the receipt's hash from the
//! winner, checks the bytes against that hash, and answers with the result.
//! Accepts `workflow.step`, `crawl.fetch`, `ocr.extract` (input by hash),
//! `embed.text`, `blender.render-frame` (input pieces by hash; the bridge
//! fetches and checks the frames and returns them base64-encoded) and
//! `infernet.inference` (chat completions);
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

/// Workloads the bridge runs. Each knows its required capability tags, how
/// big a result it accepts, and how to check the spec and the result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Workload {
    WorkflowStep,
    CrawlFetch,
    OcrExtract,
    EmbedText,
    RenderFrame,
    Inference,
}

impl Workload {
    const ALL: &'static [Workload] = &[
        Workload::WorkflowStep,
        Workload::CrawlFetch,
        Workload::OcrExtract,
        Workload::EmbedText,
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
            Workload::RenderFrame => crate::render::required_capabilities_for(spec),
            Workload::Inference => crate::inference::required_capabilities(spec),
        }
    }
    fn max_result_bytes(self) -> usize {
        match self {
            Workload::WorkflowStep | Workload::CrawlFetch => workflow::MAX_RESULT_BYTES,
            Workload::OcrExtract => crate::ocr::MAX_RESULT_BYTES,
            Workload::EmbedText => crate::embed::MAX_RESULT_BYTES,
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

    let required = workload.required_capabilities(&spec);
    // Kept for checking the result (embeddings: one vector per text).
    let spec_for_check = spec.clone();
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
        .with_required_capabilities(required)
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
    if !workload.valid_result(&spec_for_check, &result) {
        return Err(worker_failed(
            "worker result does not match the result schema",
        ));
    }
    let result = if workload == Workload::RenderFrame {
        let budget = timeout.saturating_sub(started.elapsed());
        attach_render_frames(state, &spec_for_check, result, &worker, budget)
            .await
            .map_err(|error| worker_failed(&format!("{error:#}")))?
    } else {
        result
    };
    Ok(json!({
        "job_id": outcome.job_id,
        "worker": worker,
        "price": outcome.accepted_bid.price_usd,
        "output_hash": output_hash,
        "result": result,
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
    budget: Duration,
) -> Result<Value> {
    use base64::Engine;
    let parsed = crate::render::parse_spec(spec).map_err(anyhow::Error::msg)?;
    let metas = crate::render::result_frames(&parsed, &result)?;
    let total: u64 = metas.iter().map(|m| m.size).sum();
    if total > MAX_INLINE_FRAME_BYTES {
        bail!("frames total {total} bytes, over the bridge's {MAX_INLINE_FRAME_BYTES}-byte limit; use `c0mpute render`");
    }
    let frames = crate::render::fetch_frames(&state.net, worker, &metas, budget).await?;
    if let Some(list) = result.get_mut("frames").and_then(Value::as_array_mut) {
        for (entry, (_, bytes)) in list.iter_mut().zip(frames) {
            entry["data_base64"] =
                Value::String(base64::engine::general_purpose::STANDARD.encode(bytes));
        }
    }
    Ok(result)
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
