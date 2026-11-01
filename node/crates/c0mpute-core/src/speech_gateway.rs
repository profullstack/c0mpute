//! Authenticated HTTP bridge from applications to the c0mpute P2P speech jobs.
//!
//! Run this on a small buyer node near the application. Synthesis and
//! transcription execute on workers that advertise the requested CPU model.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use c0mpute_net::{Libp2pNetwork, Network};
use c0mpute_proto::{ChunkRequest, Hash};
use c0mpute_store::ChunkStore;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tower_http::trace::TraceLayer;

use crate::{Config, JobAuction, Supervisor, run_auction};

const MAX_TTS_CHARS: usize = 6_000;
const MAX_CONCURRENT_SPEECH_JOBS: usize = 6;
const MAX_AUDIO_BYTES: usize = 25 * 1024 * 1024;
const MAX_WAV_BYTES: usize = 20 * 1024 * 1024;

#[derive(Clone)]
struct GatewayState {
    net: Arc<Libp2pNetwork>,
    store: ChunkStore,
    token: Arc<str>,
    max_price_usd: f64,
    tts_slots: Arc<Semaphore>,
    stt_slots: Arc<Semaphore>,
}

#[derive(Debug)]
struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

type ApiResult<T> = std::result::Result<T, ApiError>;

#[derive(Debug, Deserialize)]
struct TtsRequest {
    text: String,
    engine: String,
    voice: String,
    #[serde(default = "default_locale")]
    locale: String,
    #[serde(default = "default_speed")]
    speed: f32,
    #[serde(default)]
    persona: String,
}

#[derive(Debug, Deserialize)]
struct SttQuery {
    #[serde(default = "default_stt_model")]
    model: String,
    language: Option<String>,
}

#[derive(Debug, Serialize)]
struct VoiceCatalog {
    engines: Vec<VoiceEngine>,
    agent_profiles: Vec<AgentVoice>,
}

#[derive(Debug, Serialize)]
struct VoiceEngine {
    id: &'static str,
    voices: Vec<VoiceItem>,
}

#[derive(Debug, Serialize)]
struct VoiceItem {
    id: &'static str,
    label: &'static str,
    presentation: &'static str,
}

#[derive(Debug, Serialize)]
struct AgentVoice {
    slug: &'static str,
    persona: &'static str,
    engine: &'static str,
    voice: &'static str,
    language: &'static str,
    locale: &'static str,
    accent: &'static str,
    voice_gender: &'static str,
    openprofile: &'static str,
}

/// Start the buyer-side HTTP bridge. The token is required and never exposed
/// by any response; callers should keep this listener behind HTTPS or a
/// private network and configure the matching server-side application secret.
pub async fn serve(bind: std::net::SocketAddr, token: String, max_price_usd: f64) -> Result<()> {
    if token.len() < 24 || token.len() > 256 {
        bail!("C0MPUTE_SPEECH_TOKEN must be between 24 and 256 characters");
    }
    if !max_price_usd.is_finite() || max_price_usd < 0.0 {
        bail!("maximum job price must be a non-negative number");
    }

    let supervisor = Supervisor::boot(Config::default()).await?;
    let state = GatewayState {
        net: supervisor.libp2p,
        store: supervisor.store,
        token: Arc::from(token),
        max_price_usd,
        // Let one multi-chunk readback fan out across up to six network
        // workers instead of serializing most of its synthesis jobs here.
        tts_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_SPEECH_JOBS)),
        stt_slots: Arc::new(Semaphore::new(2)),
    };
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/voices", get(voices))
        .route("/v1/tts", post(synthesize))
        .route("/v1/stt", post(transcribe))
        .layer(DefaultBodyLimit::max(MAX_AUDIO_BYTES))
        .layer(TraceLayer::new_for_http())
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("bind c0mpute speech gateway at {bind}"))?;
    tracing::info!(%bind, "c0mpute speech gateway listening");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn healthz() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "service": "c0mpute-speech", "status": "ok" }))
}

async fn voices() -> Json<VoiceCatalog> {
    Json(VoiceCatalog {
        engines: vec![
            VoiceEngine {
                id: "kokoro",
                voices: vec![
                    VoiceItem {
                        id: "af_sarah",
                        label: "Sarah",
                        presentation: "feminine",
                    },
                    VoiceItem {
                        id: "af_bella",
                        label: "Bella",
                        presentation: "feminine",
                    },
                    VoiceItem {
                        id: "am_onyx",
                        label: "Onyx",
                        presentation: "masculine",
                    },
                    VoiceItem {
                        id: "am_michael",
                        label: "Michael",
                        presentation: "masculine",
                    },
                ],
            },
            VoiceEngine {
                id: "piper",
                voices: vec![
                    VoiceItem {
                        id: "en_US-joe-medium",
                        label: "Joe",
                        presentation: "masculine",
                    },
                    VoiceItem {
                        id: "en_US-norman-medium",
                        label: "Norman",
                        presentation: "masculine",
                    },
                ],
            },
        ],
        agent_profiles: crate::speech_profiles::AGENTS
            .iter()
            .map(|p| AgentVoice {
                slug: p.slug,
                persona: p.persona,
                engine: p.engine,
                voice: p.voice,
                language: p.language,
                locale: p.locale,
                accent: p.accent,
                voice_gender: p.voice_gender,
                openprofile: p.openprofile,
            })
            .collect(),
    })
}

async fn synthesize(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(input): Json<TtsRequest>,
) -> ApiResult<Response> {
    authenticate(&state, &headers)?;
    if input.text.trim().is_empty() || input.text.chars().count() > MAX_TTS_CHARS {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("text must contain 1 to {MAX_TTS_CHARS} characters"),
        ));
    }
    if !valid_tts_voice(&input.engine, &input.voice) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "unknown TTS engine or voice".into(),
        ));
    }
    if !(0.65..=1.5).contains(&input.speed) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "speed must be between 0.65 and 1.5".into(),
        ));
    }
    if input.locale.len() > 32
        || !input
            .locale
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "locale must be a short BCP-47 tag".into(),
        ));
    }
    if input.persona.len() > 500 {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "persona must be at most 500 characters".into(),
        ));
    }

    let _slot = state.tts_slots.clone().acquire_owned().await.map_err(|_| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "speech queue is unavailable".into(),
        )
    })?;
    let spec = serde_json::json!({
        "text": input.text,
        "engine": input.engine,
        "voice": input.voice,
        "locale": input.locale,
        "speed": input.speed,
        "persona": input.persona,
    });
    let outcome = run_speech_auction(
        &state,
        "tts.synthesize",
        spec,
        vec![
            "c0mpute:role:tts".to_string(),
            format!("c0mpute:tts:engine:{}", input.engine),
        ],
    )
    .await?;
    if outcome.receipt.status != c0mpute_net::topics::JobStatus::Completed {
        return Err(ApiError(
            StatusCode::BAD_GATEWAY,
            "c0mpute worker failed the synthesis job".into(),
        ));
    }
    let output_hash = outcome.receipt.output_hash.as_deref().ok_or_else(|| {
        ApiError(
            StatusCode::BAD_GATEWAY,
            "worker returned no audio result".into(),
        )
    })?;
    let bytes = fetch_result(
        &state,
        output_hash,
        &outcome.accepted_bid.bidder_peer_id,
        MAX_WAV_BYTES,
    )
    .await?;
    if bytes.len() < 44 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(ApiError(
            StatusCode::BAD_GATEWAY,
            "worker returned invalid WAV audio".into(),
        ));
    }
    Ok(audio_response(
        bytes,
        &outcome.job_id,
        &outcome.accepted_bid.bidder_peer_id,
    ))
}

async fn transcribe(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<SttQuery>,
    body: Bytes,
) -> ApiResult<Response> {
    authenticate(&state, &headers)?;
    if body.is_empty() || body.len() > MAX_AUDIO_BYTES {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "audio must be between 1 byte and 25 MiB".into(),
        ));
    }
    if query.model != "base" {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "supported STT model: base".into(),
        ));
    }
    if query
        .language
        .as_ref()
        .is_some_and(|s| s.len() > 16 || !s.chars().all(|c| c.is_ascii_alphabetic() || c == '-'))
    {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "language must be a short BCP-47 tag".into(),
        ));
    }
    let format = audio_format(&headers)?;
    let _slot = state.stt_slots.clone().acquire_owned().await.map_err(|_| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "speech queue is unavailable".into(),
        )
    })?;
    let audio_hash = state.store.put(&body).await.map_err(gateway_failure)?;
    state
        .net
        .announce(&audio_hash)
        .await
        .map_err(gateway_failure)?;
    let spec = serde_json::json!({
        "audio_hash": audio_hash.to_hex(),
        "audio_provider_peer_id": state.net.peer_id().to_string(),
        "audio_format": format,
        "model": query.model,
        "language": query.language,
    });
    let outcome = run_speech_auction(
        &state,
        "stt.transcribe",
        spec,
        vec![
            "c0mpute:role:stt".to_string(),
            "c0mpute:stt:engine:faster-whisper".to_string(),
        ],
    )
    .await?;
    if outcome.receipt.status != c0mpute_net::topics::JobStatus::Completed {
        return Err(ApiError(
            StatusCode::BAD_GATEWAY,
            "c0mpute worker failed the transcription job".into(),
        ));
    }
    let output_hash = outcome.receipt.output_hash.as_deref().ok_or_else(|| {
        ApiError(
            StatusCode::BAD_GATEWAY,
            "worker returned no transcript".into(),
        )
    })?;
    let bytes = fetch_result(
        &state,
        output_hash,
        &outcome.accepted_bid.bidder_peer_id,
        10 * 1024 * 1024,
    )
    .await?;
    let mut payload: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| {
        ApiError(
            StatusCode::BAD_GATEWAY,
            "worker returned invalid transcript JSON".into(),
        )
    })?;
    payload["job_id"] = serde_json::Value::String(outcome.job_id.clone());
    payload["worker_peer_id"] =
        serde_json::Value::String(outcome.accepted_bid.bidder_peer_id.clone());
    let mut response = Json(payload).into_response();
    set_job_headers(
        response.headers_mut(),
        &outcome.job_id,
        &outcome.accepted_bid.bidder_peer_id,
    );
    Ok(response)
}

async fn run_speech_auction(
    state: &GatewayState,
    workload: &str,
    spec: serde_json::Value,
    required: Vec<String>,
) -> ApiResult<crate::AuctionOutcome> {
    let auction = JobAuction::new(workload, spec)
        .with_required_capabilities(required)
        .with_max_price_usd(state.max_price_usd)
        .with_bid_window(Duration::from_secs(4))
        .with_deadline(Duration::from_secs(20 * 60))
        .with_receipt_timeout(Duration::from_secs(15 * 60));
    run_auction(state.net.clone(), auction)
        .await
        .map_err(|error| {
            tracing::warn!(workload, err = %error, "speech auction failed");
            ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "no c0mpute worker completed the speech job".into(),
            )
        })
}

async fn fetch_result(
    state: &GatewayState,
    hash: &str,
    provider_peer_id: &str,
    max_bytes: usize,
) -> ApiResult<Vec<u8>> {
    let hash = Hash::from_hex(hash).map_err(|_| {
        ApiError(
            StatusCode::BAD_GATEWAY,
            "worker returned an invalid result hash".into(),
        )
    })?;
    let request = ChunkRequest {
        chunk_hash: hash,
        shard_index: None,
    };
    let provider: c0mpute_net::PeerId = provider_peer_id.parse().map_err(|_| {
        ApiError(
            StatusCode::BAD_GATEWAY,
            "worker returned an invalid peer id".into(),
        )
    })?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut last_error = None;
    while tokio::time::Instant::now() < deadline {
        match state.net.fetch_from_peer(provider, &request).await {
            Ok(bytes) if Hash::of(&bytes) == hash && bytes.len() <= max_bytes => return Ok(bytes),
            Ok(_) => {
                return Err(ApiError(
                    StatusCode::BAD_GATEWAY,
                    "worker result hash or size is invalid".into(),
                ));
            }
            Err(error) => {
                last_error = Some(error);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    tracing::warn!(err = ?last_error, "timed out fetching c0mpute speech result");
    Err(ApiError(
        StatusCode::GATEWAY_TIMEOUT,
        "timed out fetching the c0mpute result".into(),
    ))
}

fn authenticate(state: &GatewayState, headers: &HeaderMap) -> ApiResult<()> {
    let supplied = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| ApiError(StatusCode::UNAUTHORIZED, "bearer token required".into()))?;
    if constant_time_eq(supplied.as_bytes(), state.token.as_bytes()) {
        Ok(())
    } else {
        Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "invalid bearer token".into(),
        ))
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut difference = a.len() ^ b.len();
    for index in 0..a.len().max(b.len()) {
        difference |=
            usize::from(a.get(index).copied().unwrap_or(0) ^ b.get(index).copied().unwrap_or(0));
    }
    difference == 0
}

fn valid_tts_voice(engine: &str, voice: &str) -> bool {
    match engine {
        "kokoro" => ["af_sarah", "af_bella", "am_onyx", "am_michael"].contains(&voice),
        "piper" => ["en_US-joe-medium", "en_US-norman-medium"].contains(&voice),
        _ => false,
    }
}

fn audio_format(headers: &HeaderMap) -> ApiResult<&'static str> {
    let raw = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let mime = raw
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match mime.as_str() {
        "audio/webm" | "video/webm" => Ok("webm"),
        "audio/wav" | "audio/x-wav" => Ok("wav"),
        "audio/ogg" => Ok("ogg"),
        "audio/mp4" | "audio/x-m4a" => Ok("m4a"),
        "audio/mpeg" | "audio/mp3" => Ok("mp3"),
        _ => Err(ApiError(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "send WebM, WAV, Ogg, M4A, or MP3 audio".into(),
        )),
    }
}

fn audio_response(bytes: Vec<u8>, job_id: &str, worker: &str) -> Response {
    let mut response = (StatusCode::OK, bytes).into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/wav"));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    set_job_headers(response.headers_mut(), job_id, worker);
    response
}

fn set_job_headers(headers: &mut HeaderMap, job_id: &str, worker: &str) {
    if let Ok(value) = HeaderValue::from_str(job_id) {
        headers.insert("x-c0mpute-job-id", value);
    }
    if let Ok(value) = HeaderValue::from_str(worker) {
        headers.insert("x-c0mpute-worker-peer-id", value);
    }
}

fn gateway_failure(error: anyhow::Error) -> ApiError {
    tracing::warn!(err = %error, "speech gateway operation failed");
    ApiError(
        StatusCode::INTERNAL_SERVER_ERROR,
        "speech gateway operation failed".into(),
    )
}

fn default_locale() -> String {
    "en-US".into()
}
fn default_speed() -> f32 {
    1.0
}
fn default_stt_model() -> String {
    "base".into()
}
