//! Storage HTTP API (CIP-002).
//!
//! Turns the `c0mpute-store` engine into a service:
//!
//! ```text
//! PUT    /storage/v1/objects/{hash}     store an object
//! GET    /storage/v1/objects/{hash}     reconstruct and stream it back
//! HEAD   /storage/v1/objects/{hash}     existence + length, no body
//! DELETE /storage/v1/objects/{hash}     drop manifest + shards
//! GET    /storage/v1/shards/{hash}      serve one shard (peer fetch, repair)
//! PUT    /storage/v1/shards/{hash}      accept one shard (peer placement)
//! HEAD   /storage/v1/shards/{hash}      do you hold it?
//! GET    /storage/v1/manifests/{hash}   the manifest as JSON
//! GET    /storage/v1/status             disk budget and usage
//! ```
//!
//! Single-node: every shard lands on the local disk and `host_hint` stays
//! `None`. Cross-node placement is CIP-003.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use c0mpute_proto::Hash;
use c0mpute_store::{ObjectManifest, Storage, Tier};
use futures::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::auth::{AUTH_HEADER, AllowAll, AuthError, AuthRequest, Authorizer};

/// Header selecting the redundancy tier on a write.
pub const TIER_HEADER: &str = "x-c0mpute-tier";
/// Headers a peer sends when placing a single shard.
pub const SHARD_OBJECT_HEADER: &str = "x-c0mpute-object";
pub const SHARD_INDEX_HEADER: &str = "x-c0mpute-shard-index";

/// Default ceiling on a single object. Blocks keep memory bounded, but an
/// unbounded object still means an unbounded manifest.
pub const DEFAULT_MAX_OBJECT_BYTES: u64 = 1 << 40; // 1 TiB

#[derive(Clone, Debug)]
pub struct Limits {
    pub max_object_bytes: u64,
    /// Total bytes this node will hold, if capped.
    pub disk_budget_bytes: Option<u64>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_object_bytes: DEFAULT_MAX_OBJECT_BYTES,
            disk_budget_bytes: None,
        }
    }
}

/// Tracks how much of the operator's committed disk is in use.
///
/// Seeded by walking the shard directory once at startup — O(files), a few
/// seconds for a large store, and the alternative is trusting a counter that
/// drifts across restarts.
#[derive(Debug)]
pub struct DiskBudget {
    limit: Option<u64>,
    used: AtomicU64,
}

impl DiskBudget {
    pub fn new(limit: Option<u64>, used: u64) -> Self {
        Self {
            limit,
            used: AtomicU64::new(used),
        }
    }

    /// Measure current usage by walking the store's shard directory.
    pub fn scan(root: &std::path::Path) -> u64 {
        fn walk(dir: &std::path::Path, total: &mut u64) {
            let Ok(rd) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in rd.flatten() {
                let path = entry.path();
                match entry.file_type() {
                    Ok(t) if t.is_dir() => walk(&path, total),
                    Ok(t) if t.is_file() => {
                        if let Ok(md) = entry.metadata() {
                            *total += md.len();
                        }
                    }
                    _ => {}
                }
            }
        }
        let mut total = 0;
        walk(&root.join("shards"), &mut total);
        total
    }

    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }

    pub fn limit(&self) -> Option<u64> {
        self.limit
    }

    /// True if `bytes` more would fit. Advisory: the write is charged after
    /// the fact, so concurrent writes can overshoot slightly.
    pub fn would_fit(&self, bytes: u64) -> bool {
        match self.limit {
            None => true,
            Some(limit) => self.used().saturating_add(bytes) <= limit,
        }
    }

    pub fn charge(&self, bytes: u64) {
        self.used.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn release(&self, bytes: u64) {
        // saturating: a double-release must not wrap to a huge number.
        let _ = self
            .used
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |u| {
                Some(u.saturating_sub(bytes))
            });
    }
}

#[derive(Clone)]
pub struct StorageApiState {
    pub storage: Storage,
    pub auth: Arc<dyn Authorizer>,
    pub limits: Limits,
    pub budget: Arc<DiskBudget>,
    /// Object hashes with a PUT in flight, so a duplicate concurrent write
    /// gets a 409 instead of two writers racing on the same manifest.
    inflight: Arc<Mutex<HashSet<Hash>>>,
}

impl StorageApiState {
    pub fn new(storage: Storage, auth: Arc<dyn Authorizer>, limits: Limits) -> Self {
        let used = DiskBudget::scan(storage.chunk_store().root());
        let budget = Arc::new(DiskBudget::new(limits.disk_budget_bytes, used));
        Self {
            storage,
            auth,
            limits,
            budget,
            inflight: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Local single-operator node: no auth, no disk cap.
    pub fn local(storage: Storage) -> Self {
        Self::new(storage, Arc::new(AllowAll), Limits::default())
    }
}

pub fn router(state: StorageApiState) -> Router {
    Router::new()
        .route(
            "/storage/v1/objects/{hash}",
            get(get_object)
                .head(head_object)
                .put(put_object)
                .delete(delete_object),
        )
        .route(
            "/storage/v1/shards/{hash}",
            get(get_shard).head(head_shard).put(put_shard),
        )
        .route("/storage/v1/manifests/{hash}", get(get_manifest))
        .route("/storage/v1/status", get(status))
        .with_state(state)
}

// --------------------------------------------------------------------- errors

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    BadRequest(String),
    #[error("unauthorized: {0}")]
    Unauthorized(AuthError),
    #[error("not found")]
    NotFound,
    #[error("a write for this object is already in flight")]
    Conflict,
    #[error("object exceeds the {0} byte limit")]
    TooLarge(u64),
    #[error("{0}")]
    Unprocessable(String),
    #[error("disk budget exhausted")]
    OutOfSpace,
    #[error("{0}")]
    Internal(String),
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
    code: u16,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self {
            ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            ApiError::NotFound => StatusCode::NOT_FOUND,
            ApiError::Conflict => StatusCode::CONFLICT,
            ApiError::TooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            ApiError::Unprocessable(_) => StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::OutOfSpace => StatusCode::INSUFFICIENT_STORAGE,
            ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let body = ErrorBody {
            error: self.to_string(),
            code: status.as_u16(),
        };
        (status, axum::Json(body)).into_response()
    }
}

type ApiResult<T> = Result<T, ApiError>;

// ---------------------------------------------------------------- helpers

/// Accept `blake3:<hex>` or a bare hex string.
fn parse_hash(raw: &str) -> ApiResult<Hash> {
    let hex = raw.strip_prefix("blake3:").unwrap_or(raw);
    Hash::from_hex(hex).map_err(|_| ApiError::BadRequest(format!("invalid hash `{raw}`")))
}

fn parse_tier(headers: &HeaderMap) -> ApiResult<Tier> {
    match headers.get(TIER_HEADER) {
        None => Ok(Tier::default()),
        Some(v) => v
            .to_str()
            .ok()
            .and_then(|s| s.parse::<Tier>().ok())
            .ok_or_else(|| ApiError::BadRequest("invalid tier".into())),
    }
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

fn authorize(
    state: &StorageApiState,
    headers: &HeaderMap,
    method: &str,
    path: &str,
    body_hash: &str,
) -> ApiResult<()> {
    let req = AuthRequest {
        method,
        path,
        body_hash_hex: body_hash,
        header: header_str(headers, AUTH_HEADER),
    };
    state
        .auth
        .authorize(&req)
        .map(|_| ())
        .map_err(ApiError::Unauthorized)
}

/// A single HTTP byte range. Only the common `bytes=a-b` / `bytes=a-` forms
/// are supported; multipart ranges are not.
fn parse_range(raw: &str, total: u64) -> Option<(u64, u64)> {
    let spec = raw.strip_prefix("bytes=")?.trim();
    if spec.contains(',') {
        return None;
    }
    let (start_s, end_s) = spec.split_once('-')?;
    if start_s.is_empty() {
        // `bytes=-N` — the final N bytes.
        let n: u64 = end_s.parse().ok()?;
        let n = n.min(total);
        return Some((total.saturating_sub(n), n));
    }
    let start: u64 = start_s.parse().ok()?;
    if start >= total {
        return None;
    }
    let end = if end_s.is_empty() {
        total - 1
    } else {
        end_s.parse::<u64>().ok()?.min(total - 1)
    };
    if end < start {
        return None;
    }
    Some((start, end - start + 1))
}

// ---------------------------------------------------------------- objects

#[derive(Deserialize)]
pub struct PutQuery {
    /// Optional tier, as an alternative to the header.
    pub tier: Option<String>,
}

async fn put_object(
    State(state): State<StorageApiState>,
    Path(raw_hash): Path<String>,
    Query(q): Query<PutQuery>,
    headers: HeaderMap,
    req: Request,
) -> ApiResult<Response> {
    let object_hash = parse_hash(&raw_hash)?;
    let path = format!("/storage/v1/objects/{raw_hash}");
    authorize(&state, &headers, "PUT", &path, &object_hash.to_hex())?;

    let tier = match q.tier {
        Some(t) => t
            .parse::<Tier>()
            .map_err(|e| ApiError::BadRequest(e.to_string()))?,
        None => parse_tier(&headers)?,
    };

    let len: u64 = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| ApiError::BadRequest("Content-Length is required".into()))?;

    if len > state.limits.max_object_bytes {
        return Err(ApiError::TooLarge(state.limits.max_object_bytes));
    }
    // Charge the expanded size — the tier decides how much disk this costs.
    let cost = (len as f64 * tier.expansion()).ceil() as u64;
    if !state.budget.would_fit(cost) {
        return Err(ApiError::OutOfSpace);
    }

    // Idempotent: an object we already hold is not rewritten.
    if state.storage.has(&object_hash).await {
        let manifest = state
            .storage
            .read_manifest(&object_hash)
            .await
            .map_err(|e| ApiError::Internal(format!("{e:#}")))?;
        return Ok((StatusCode::OK, axum::Json(manifest)).into_response());
    }

    // Single-flight per object hash.
    {
        let mut inflight = state.inflight.lock().await;
        if !inflight.insert(object_hash) {
            return Err(ApiError::Conflict);
        }
    }
    let _guard = InflightGuard {
        state: state.clone(),
        hash: object_hash,
    };

    let stream = req
        .into_body()
        .into_data_stream()
        .map_err(|e| anyhow::anyhow!("request body: {e}"));

    let manifest = state
        .storage
        .put_stream(stream, Some(object_hash), tier, Some(len))
        .await
        .map_err(|e| {
            let msg = format!("{e:#}");
            if msg.contains("integrity failure") {
                ApiError::Unprocessable(msg)
            } else {
                ApiError::Internal(msg)
            }
        })?;

    state.budget.charge(cost);
    info!(object_hash = %object_hash, %tier, bytes = len, "stored object");
    Ok((StatusCode::CREATED, axum::Json(manifest)).into_response())
}

/// Clears the in-flight marker however the handler exits.
struct InflightGuard {
    state: StorageApiState,
    hash: Hash,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        let state = self.state.clone();
        let hash = self.hash;
        tokio::spawn(async move {
            state.inflight.lock().await.remove(&hash);
        });
    }
}

async fn get_object(
    State(state): State<StorageApiState>,
    Path(raw_hash): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let object_hash = parse_hash(&raw_hash)?;
    let manifest = load_manifest(&state, &object_hash).await?;
    let total = manifest.original_len;

    if let Some(range_raw) = header_str(&headers, header::RANGE.as_str()) {
        let Some((offset, len)) = parse_range(range_raw, total) else {
            return Ok((
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{total}"))],
            )
                .into_response());
        };
        let bytes = state
            .storage
            .get_range_with(&manifest, offset, len)
            .await
            .map_err(|e| ApiError::Internal(format!("{e:#}")))?;
        let end = offset + bytes.len() as u64 - 1;
        return Ok((
            StatusCode::PARTIAL_CONTENT,
            [
                (header::CONTENT_TYPE, "application/octet-stream".to_string()),
                (
                    header::CONTENT_RANGE,
                    format!("bytes {offset}-{end}/{total}"),
                ),
                (header::ACCEPT_RANGES, "bytes".to_string()),
            ],
            bytes,
        )
            .into_response());
    }

    // Whole object: stream block by block so memory stays bounded.
    let stream = state.storage.read_stream(manifest);
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::CONTENT_LENGTH, total.to_string()),
            (header::ACCEPT_RANGES, "bytes".to_string()),
        ],
        Body::from_stream(stream),
    )
        .into_response())
}

async fn head_object(
    State(state): State<StorageApiState>,
    Path(raw_hash): Path<String>,
) -> ApiResult<Response> {
    let object_hash = parse_hash(&raw_hash)?;
    let manifest = load_manifest(&state, &object_hash).await?;
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::CONTENT_LENGTH, manifest.original_len.to_string()),
            (header::ACCEPT_RANGES, "bytes".to_string()),
            (
                header::HeaderName::from_static("x-c0mpute-tier"),
                manifest.tier.to_string(),
            ),
        ],
    )
        .into_response())
}

async fn delete_object(
    State(state): State<StorageApiState>,
    Path(raw_hash): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let object_hash = parse_hash(&raw_hash)?;
    let path = format!("/storage/v1/objects/{raw_hash}");
    authorize(&state, &headers, "DELETE", &path, &object_hash.to_hex())?;

    let manifest = load_manifest(&state, &object_hash).await?;
    let cost = (manifest.original_len as f64 * manifest.tier.expansion()).ceil() as u64;
    state
        .storage
        .delete(&object_hash)
        .await
        .map_err(|e| ApiError::Internal(format!("{e:#}")))?;
    state.budget.release(cost);
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn get_manifest(
    State(state): State<StorageApiState>,
    Path(raw_hash): Path<String>,
) -> ApiResult<Response> {
    let object_hash = parse_hash(&raw_hash)?;
    let manifest = load_manifest(&state, &object_hash).await?;
    Ok(axum::Json(manifest).into_response())
}

async fn load_manifest(state: &StorageApiState, hash: &Hash) -> ApiResult<ObjectManifest> {
    if !state.storage.has(hash).await {
        return Err(ApiError::NotFound);
    }
    state
        .storage
        .read_manifest(hash)
        .await
        .map_err(|e| ApiError::Internal(format!("{e:#}")))
}

// ----------------------------------------------------------------- shards

async fn get_shard(
    State(state): State<StorageApiState>,
    Path(raw_hash): Path<String>,
) -> ApiResult<Response> {
    let hash = parse_hash(&raw_hash)?;
    match state.storage.chunk_store().get(&hash).await {
        Ok(bytes) => Ok((
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/octet-stream")],
            bytes,
        )
            .into_response()),
        Err(e) => {
            // A read error here is either "absent" or "corrupt on disk"; both
            // mean the peer should look elsewhere, but corruption is a fault
            // we want in the logs.
            if state.storage.chunk_store().has(&hash).await {
                warn!(shard = %hash, err = %e, "held shard failed verification");
            }
            Err(ApiError::NotFound)
        }
    }
}

async fn head_shard(
    State(state): State<StorageApiState>,
    Path(raw_hash): Path<String>,
) -> ApiResult<Response> {
    let hash = parse_hash(&raw_hash)?;
    if state.storage.chunk_store().has(&hash).await {
        Ok(StatusCode::OK.into_response())
    } else {
        Err(ApiError::NotFound)
    }
}

/// Accept one shard from a peer (placement, CIP-003; repair, CIP-005).
async fn put_shard(
    State(state): State<StorageApiState>,
    Path(raw_hash): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult<Response> {
    let shard_hash = parse_hash(&raw_hash)?;
    let path = format!("/storage/v1/shards/{raw_hash}");
    authorize(&state, &headers, "PUT", &path, &shard_hash.to_hex())?;

    if !state.budget.would_fit(body.len() as u64) {
        return Err(ApiError::OutOfSpace);
    }

    // Commit-then-verify: a peer cannot make us store bytes under a hash they
    // do not hash to.
    let actual = Hash::of(&body);
    if actual != shard_hash {
        return Err(ApiError::Unprocessable(format!(
            "shard integrity failure: committed to {shard_hash} but body hashes to {actual}"
        )));
    }

    if state.storage.chunk_store().has(&shard_hash).await {
        return Ok(StatusCode::OK.into_response());
    }

    let len = body.len() as u64;
    state
        .storage
        .chunk_store()
        .put(&body)
        .await
        .map_err(|e| ApiError::Internal(format!("{e:#}")))?;
    state.budget.charge(len);
    Ok(StatusCode::CREATED.into_response())
}

// ----------------------------------------------------------------- status

#[derive(Serialize)]
struct StatusBody {
    used_bytes: u64,
    limit_bytes: Option<u64>,
    max_object_bytes: u64,
    default_tier: String,
}

async fn status(State(state): State<StorageApiState>) -> Response {
    axum::Json(StatusBody {
        used_bytes: state.budget.used(),
        limit_bytes: state.budget.limit(),
        max_object_bytes: state.limits.max_object_bytes,
        default_tier: Tier::default().to_string(),
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_hash_forms() {
        let h = Hash::of(b"x");
        assert_eq!(parse_hash(&h.to_hex()).unwrap(), h);
        assert_eq!(parse_hash(&format!("blake3:{}", h.to_hex())).unwrap(), h);
        assert!(parse_hash("nonsense").is_err());
    }

    #[test]
    fn parses_ranges() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 100)));
        assert_eq!(parse_range("bytes=100-", 1000), Some((100, 900)));
        assert_eq!(parse_range("bytes=-50", 1000), Some((950, 50)));
        // Clamped to the object.
        assert_eq!(parse_range("bytes=990-5000", 1000), Some((990, 10)));
        // Unsatisfiable or unsupported.
        assert_eq!(parse_range("bytes=2000-3000", 1000), None);
        assert_eq!(parse_range("bytes=50-10", 1000), None);
        assert_eq!(parse_range("bytes=0-10,20-30", 1000), None);
        assert_eq!(parse_range("items=0-10", 1000), None);
    }

    #[test]
    fn budget_tracks_and_never_wraps() {
        let b = DiskBudget::new(Some(1000), 0);
        assert!(b.would_fit(1000));
        assert!(!b.would_fit(1001));
        b.charge(600);
        assert_eq!(b.used(), 600);
        assert!(!b.would_fit(500));
        b.release(600);
        assert_eq!(b.used(), 0);
        // Over-release must not underflow into a huge number.
        b.release(999_999);
        assert_eq!(b.used(), 0);
    }

    #[test]
    fn unlimited_budget_always_fits() {
        let b = DiskBudget::new(None, u64::MAX / 2);
        assert!(b.would_fit(u64::MAX));
    }
}
