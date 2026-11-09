//! Authenticated HTTP bridge from applications to the c0mpute P2P speech jobs.
//!
//! Run this on a small buyer node near the application. Synthesis and
//! transcription execute on workers that advertise the requested CPU model.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use c0mpute_net::{Libp2pNetwork, Network};
use c0mpute_store::ChunkStore;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tower_http::trace::TraceLayer;

use crate::{Config, JobAuction, Supervisor, run_auction};

const MAX_TTS_CHARS: usize = 6_000;
const MAX_CONCURRENT_SPEECH_JOBS: usize = 6;
const MAX_AUDIO_BYTES: usize = 25 * 1024 * 1024;
const MAX_WAV_BYTES: usize = 20 * 1024 * 1024;
const MAX_SPEECH_WORKER_ATTEMPTS: usize = 3;
const MAX_CONCURRENT_IMAGE_JOBS: usize = 4;
/// A healthy CPU worker draws an image in ~2 minutes. One that has not
/// answered in 6 is benched and the job goes to the next worker.
const IMAGE_RECEIPT_TIMEOUT: Duration = Duration::from_secs(6 * 60);
/// How long an image request waits for a busy worker to free up.
const IMAGE_QUEUE_WAIT: Duration = Duration::from_secs(4 * 60);
/// Songs in flight through one gateway; each holds a whole worker for minutes.
const MAX_CONCURRENT_MUSIC_JOBS: usize = 4;
/// How long to keep re-auctioning a song while every music worker is busy.
const MUSIC_QUEUE_WAIT: Duration = Duration::from_secs(3 * 60);
const MAX_CONCURRENT_OCR_JOBS: usize = 4;
const MAX_CONCURRENT_EMBED_JOBS: usize = 8;

#[derive(Clone)]
struct GatewayState {
    net: Arc<Libp2pNetwork>,
    store: ChunkStore,
    token: Arc<str>,
    max_price_usd: f64,
    tts_slots: Arc<Semaphore>,
    stt_slots: Arc<Semaphore>,
    /// Image jobs in flight from this gateway (each holds a worker ~2 min).
    image_slots: Arc<Semaphore>,
    music_slots: Arc<Semaphore>,
    /// OCR jobs in flight (each holds an input of up to 32 MiB here).
    ocr_slots: Arc<Semaphore>,
    /// Embedding jobs in flight.
    embed_slots: Arc<Semaphore>,
    /// Warm, pinned sessions this node is holding (see `speech_session`).
    sessions: crate::speech_session::SessionStore,
    /// Workers that won and did not deliver, benched for a while.
    penalties: crate::penalty_box::PenaltyBox,
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
    pocket_voice: &'static str,
    pocket_voice_gender: &'static str,
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

    let supervisor = Supervisor::boot_buyer(Config::default()).await?;
    let state = GatewayState {
        net: supervisor.libp2p,
        store: supervisor.store,
        token: Arc::from(token),
        max_price_usd,
        // Let one multi-chunk readback fan out across up to six network
        // workers instead of serializing most of its synthesis jobs here.
        tts_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_SPEECH_JOBS)),
        stt_slots: Arc::new(Semaphore::new(2)),
        image_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_IMAGE_JOBS)),
        music_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_MUSIC_JOBS)),
        ocr_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_OCR_JOBS)),
        embed_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_EMBED_JOBS)),
        sessions: crate::speech_session::SessionStore::new(),
        penalties: crate::penalty_box::PenaltyBox::persistent(
            crate::config::data_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("./c0mpute-data"))
                .join("gateway-penalties.json"),
        ),
    };
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/voices", get(voices))
        .route("/v1/tts", post(synthesize))
        .route("/v1/stt", post(transcribe))
        .route("/v1/images", post(generate_image))
        .route("/v1/music", post(generate_music))
        .route("/v1/videos", post(generate_video))
        // OCR takes a 32 MiB document, above the router-wide audio limit.
        .route(
            "/v1/ocr",
            post(ocr_extract).layer(DefaultBodyLimit::max(crate::ocr::MAX_INPUT_BYTES)),
        )
        .route("/v1/embeddings", post(embeddings))
        // Warm, pinned sessions: open once per call, then say a sentence at a
        // time with no auction and no model reload in the way.
        .route("/v1/sessions", post(open_session).get(list_sessions))
        .route("/v1/sessions/{session}/say", post(say_on_session))
        .route("/v1/sessions/{session}", delete(close_session))
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
                        id: "af_nicole",
                        label: "Nicole",
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
                    VoiceItem {
                        id: "am_echo",
                        label: "Echo",
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
            VoiceEngine {
                id: "pocket",
                voices: vec![
                    VoiceItem {
                        id: "alba",
                        label: "Alba · CC-BY-4.0",
                        presentation: "feminine",
                    },
                    VoiceItem {
                        id: "anna",
                        label: "Anna · CC-BY-4.0",
                        presentation: "feminine",
                    },
                    VoiceItem {
                        id: "caro_davy",
                        label: "Caro Davy",
                        presentation: "feminine",
                    },
                    VoiceItem {
                        id: "marius",
                        label: "Marius",
                        presentation: "masculine",
                    },
                    VoiceItem {
                        id: "javert",
                        label: "Javert",
                        presentation: "masculine",
                    },
                    VoiceItem {
                        id: "bill_boerst",
                        label: "Bill Boerst",
                        presentation: "masculine",
                    },
                    VoiceItem {
                        id: "peter_yearsley",
                        label: "Peter Yearsley",
                        presentation: "masculine",
                    },
                    VoiceItem {
                        id: "stuart_bell",
                        label: "Stuart Bell",
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
                pocket_voice: p.pocket_voice,
                pocket_voice_gender: p.pocket_voice_gender,
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
    let (outcome, bytes) = run_speech_job(
        &state,
        "tts.synthesize",
        spec,
        vec![
            "c0mpute:role:tts".to_string(),
            format!("c0mpute:tts:engine:{}", input.engine),
        ],
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
    // One network message carries ~5 MiB at most: store the audio in pieces.
    let audio = crate::pieces::put_bytes_pieces(
        &state.net,
        &state.store,
        &body,
        crate::pieces::INPUT_PIECE_BYTES,
    )
    .await
    .map_err(gateway_failure)?;
    let mut spec = serde_json::json!({
        "audio_provider_peer_id": state.net.peer_id().to_string(),
        "audio_format": format,
        "model": query.model,
        "language": query.language,
    });
    spec.as_object_mut()
        .expect("spec is an object")
        .extend(audio.spec_fields("audio"));
    let (outcome, bytes) = run_speech_job(
        &state,
        "stt.transcribe",
        spec,
        vec![
            "c0mpute:role:stt".to_string(),
            "c0mpute:stt:engine:faster-whisper".to_string(),
        ],
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
    excluded_bidders: &[String],
) -> Result<crate::AuctionOutcome, (ApiError, Option<String>)> {
    // Learn the winner as soon as it is chosen, so a winner that then goes
    // silent can be benched and the job re-auctioned.
    let (winner_tx, mut winner_rx) = tokio::sync::mpsc::unbounded_channel();
    let auction = JobAuction::new(workload, spec)
        .with_accept_notify(winner_tx)
        .with_required_capabilities(required)
        .with_excluded_bidders(excluded_bidders.to_vec())
        .with_max_price_usd(state.max_price_usd)
        .with_bid_window(Duration::from_secs(4))
        .with_deadline(if is_video(workload) {
            crate::video::JOB_TIMEOUT + Duration::from_secs(5 * 60)
        } else if workload == crate::diffusion::WORKLOAD_TXT2IMG {
            IMAGE_RECEIPT_TIMEOUT + Duration::from_secs(60)
        } else if workload == crate::music::WORKLOAD_TXT2MUSIC {
            crate::music::JOB_TIMEOUT + Duration::from_secs(60)
        } else {
            Duration::from_secs(20 * 60)
        })
        .with_receipt_timeout(if is_video(workload) {
            crate::video::JOB_TIMEOUT
        } else if workload == crate::diffusion::WORKLOAD_TXT2IMG {
            IMAGE_RECEIPT_TIMEOUT
        } else if workload == crate::music::WORKLOAD_TXT2MUSIC {
            crate::music::JOB_TIMEOUT
        } else {
            Duration::from_secs(15 * 60)
        });
    run_auction(state.net.clone(), auction)
        .await
        .map_err(|error| {
            let winner = winner_rx.try_recv().ok().map(|(_, bid)| bid.bidder_peer_id);
            tracing::warn!(workload, err = %error, winner = ?winner, "speech auction failed");
            (
                ApiError(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "no c0mpute worker completed the speech job".into(),
                ),
                winner,
            )
        })
}

/// Run a speech job and retrieve its output. If the worker completed the job
/// but cannot be reached to return the result, re-auction the same work on a
/// different peer. Speech jobs are idempotent and the configured bid ceiling
/// still applies on each attempt.
async fn run_speech_job(
    state: &GatewayState,
    workload: &str,
    spec: serde_json::Value,
    required: Vec<String>,
    max_bytes: usize,
) -> ApiResult<(crate::AuctionOutcome, Vec<u8>)> {
    run_checked_job(state, workload, spec, required, max_bytes, &|bytes| {
        valid_speech_result(workload, bytes)
    })
    .await
}

/// [`run_speech_job`] with the caller's own result check (embeddings need
/// the request to know how many vectors are due). A result that fails it
/// strikes the worker and the job goes to the next one.
async fn run_checked_job(
    state: &GatewayState,
    workload: &str,
    spec: serde_json::Value,
    required: Vec<String>,
    max_bytes: usize,
    valid: &(dyn Fn(&[u8]) -> bool + Send + Sync),
) -> ApiResult<(crate::AuctionOutcome, Vec<u8>)> {
    // Workers benched for not delivering earlier stay out of the auction.
    let mut excluded_bidders = state.penalties.benched();
    let mut last_error = None;
    let started = std::time::Instant::now();
    let mut attempt = 0;

    while attempt < MAX_SPEECH_WORKER_ATTEMPTS {
        attempt += 1;
        let outcome = match run_speech_auction(
            state,
            workload,
            spec.clone(),
            required.clone(),
            &excluded_bidders,
        )
        .await
        {
            Ok(outcome) => outcome,
            // A winner was chosen and then never answered: bench it and try
            // the next worker instead of failing the caller.
            Err((error, Some(winner))) => {
                let bench = state.penalties.strike(&winner);
                tracing::warn!(workload, attempt, worker = %winner, bench_secs = bench.as_secs(),
                    "winner never delivered; benched, trying another worker");
                excluded_bidders.push(winner);
                last_error = Some(error);
                continue;
            }
            // Nobody bid. For images that usually means every worker is busy
            // with one (~2 min each): wait and re-auction instead of failing,
            // without spending one of the real attempts.
            Err((error, None))
                if (workload == crate::diffusion::WORKLOAD_TXT2IMG && started.elapsed() < IMAGE_QUEUE_WAIT)
                    || (workload == crate::music::WORKLOAD_TXT2MUSIC && started.elapsed() < MUSIC_QUEUE_WAIT) =>
            {
                tracing::info!(workload, waited_secs = started.elapsed().as_secs(), "no free image worker yet; waiting");
                attempt -= 1;
                last_error.get_or_insert(error);
                tokio::time::sleep(Duration::from_secs(15)).await;
                continue;
            }
            Err((error, None)) => return Err(last_error.unwrap_or(error)),
        };
        let worker = outcome.accepted_bid.bidder_peer_id.clone();
        let result = if outcome.receipt.status != c0mpute_net::topics::JobStatus::Completed {
            Err(ApiError(
                StatusCode::BAD_GATEWAY,
                format!("c0mpute worker failed the {workload} job"),
            ))
        } else if let Some(output_hash) = outcome.receipt.output_hash.as_deref() {
            match fetch_result(state, output_hash, &worker, max_bytes).await {
                Ok(bytes) if valid(&bytes) => Ok(bytes),
                Ok(_) => Err(ApiError(
                    StatusCode::BAD_GATEWAY,
                    "worker returned an invalid speech result".into(),
                )),
                Err(error) => Err(error),
            }
        } else {
            Err(ApiError(
                StatusCode::BAD_GATEWAY,
                "worker returned no speech result".into(),
            ))
        };

        match result {
            Ok(bytes) => {
                state.penalties.clear(&worker);
                return Ok((outcome, bytes));
            }
            Err(error) => {
                state.penalties.strike(&worker);
                tracing::warn!(
                    workload,
                    attempt,
                    worker = %worker,
                    err = %error.1,
                    "speech worker failed to return a usable result; trying another worker"
                );
                excluded_bidders.push(worker);
                last_error = Some(error);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "no c0mpute worker completed the speech job".into(),
        )
    }))
}

fn is_video(workload: &str) -> bool {
    workload == crate::video::WORKLOAD_TXT2VID || workload == crate::video::WORKLOAD_IMG2VID
}

fn valid_speech_result(workload: &str, bytes: &[u8]) -> bool {
    match workload {
        w if is_video(w) => crate::video::is_mp4(bytes),
        "tts.synthesize" => bytes.len() >= 44 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WAVE",
        crate::diffusion::WORKLOAD_TXT2IMG => crate::diffusion::is_png(bytes),
        crate::music::WORKLOAD_TXT2MUSIC => crate::music::audio_content_type(bytes).is_some(),
        "stt.transcribe" => {
            serde_json::from_slice::<serde_json::Value>(bytes).is_ok_and(|value| value.is_object())
        }
        crate::ocr::WORKLOAD => serde_json::from_slice::<serde_json::Value>(bytes)
            .is_ok_and(|value| crate::ocr::valid_result(&value)),
        _ => true,
    }
}

/// How long a result may take to arrive. The receipt's chunk gets at most
/// 20 s of it (a worker that never answers fails as fast as before); the
/// rest is for the 1 MiB pieces of a large result (a 20 MiB WAV, a
/// hundreds-of-MB video), about 3 s per MiB of the cap, at most 10 min.
fn result_fetch_budget(max_bytes: usize) -> Duration {
    let mib = (max_bytes / (1024 * 1024)) as u64;
    Duration::from_secs((20 + 3 * mib).min(10 * 60))
}

async fn fetch_result(
    state: &GatewayState,
    hash: &str,
    provider_peer_id: &str,
    max_bytes: usize,
) -> ApiResult<Vec<u8>> {
    use crate::gateway_util::FetchError;
    match crate::gateway_util::fetch_result(
        &state.net,
        hash,
        provider_peer_id,
        max_bytes,
        result_fetch_budget(max_bytes),
    )
    .await
    {
        Ok(bytes) => Ok(bytes),
        Err(FetchError::Invalid(message)) => Err(ApiError(StatusCode::BAD_GATEWAY, message.into())),
        Err(FetchError::Timeout(last_error)) => {
            tracing::warn!(err = %last_error, "timed out fetching c0mpute speech result");
            Err(ApiError(
                StatusCode::GATEWAY_TIMEOUT,
                "timed out fetching the c0mpute result".into(),
            ))
        }
    }
}

// ---- warm, pinned sessions ----------------------------------------------
//
// The one-shot /v1/tts path pays a 4 second bid window and a Kokoro model load
// on every request. These endpoints pay both once per call instead: `open`
// pins a warm worker on this node, `say` speaks one sentence on it, `close`
// lets the model go. See `speech_session` for the measured numbers, and for
// why a caller should send a sentence at a time rather than a paragraph.

#[derive(Debug, Deserialize)]
struct OpenSessionRequest {
    /// A kokoro voice this node serves; defaults to riotcoder's.
    #[serde(default = "default_session_voice")]
    voice: String,
}

fn default_session_voice() -> String {
    "am_michael".into()
}

#[derive(Debug, Deserialize)]
struct SayRequest {
    text: String,
    #[serde(default = "default_locale")]
    locale: String,
    #[serde(default = "default_speed")]
    speed: f32,
}

async fn open_session(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(input): Json<OpenSessionRequest>,
) -> ApiResult<Response> {
    authenticate(&state, &headers)?;
    let opened = state
        .sessions
        .open(&input.voice)
        .await
        .map_err(|error| ApiError(StatusCode::SERVICE_UNAVAILABLE, format!("{error:#}")))?;
    Ok((StatusCode::CREATED, Json(opened)).into_response())
}

async fn list_sessions(
    State(state): State<GatewayState>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    authenticate(&state, &headers)?;
    let sessions = state.sessions.list().await;
    Ok(Json(serde_json::json!({ "sessions": sessions })).into_response())
}

async fn say_on_session(
    State(state): State<GatewayState>,
    Path(session): Path<String>,
    headers: HeaderMap,
    Json(input): Json<SayRequest>,
) -> ApiResult<Response> {
    authenticate(&state, &headers)?;
    if input.text.trim().is_empty() || input.text.chars().count() > MAX_TTS_CHARS {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("text must contain 1 to {MAX_TTS_CHARS} characters"),
        ));
    }
    if !(0.65..=1.5).contains(&input.speed) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "speed must be between 0.65 and 1.5".into(),
        ));
    }

    let spoken = state
        .sessions
        .say(&session, input.text.trim(), &input.locale, input.speed)
        .await
        .map_err(|error| {
            let message = format!("{error:#}");
            let status = if message.contains("no warm speech session") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_GATEWAY
            };
            ApiError(status, message)
        })?;

    let wav = tokio::fs::read(&spoken.wav).await.map_err(|error| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("could not read the synthesized audio: {error}"),
        )
    })?;
    // The caller is about to play this down a phone line, so the timings ride
    // in headers: it can watch its realtime factor without parsing the body.
    let mut response = (StatusCode::OK, wav).into_response();
    let out = response.headers_mut();
    out.insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/wav"));
    if let Ok(value) = HeaderValue::from_str(&spoken.synth_ms.to_string()) {
        out.insert("x-c0mpute-synth-ms", value);
    }
    if let Ok(value) = HeaderValue::from_str(&format!("{:.3}", spoken.seconds)) {
        out.insert("x-c0mpute-audio-seconds", value);
    }
    Ok(response)
}

async fn close_session(
    State(state): State<GatewayState>,
    Path(session): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    authenticate(&state, &headers)?;
    let closed = state.sessions.close(&session).await;
    Ok(Json(serde_json::json!({ "closed": closed })).into_response())
}

fn authenticate(state: &GatewayState, headers: &HeaderMap) -> ApiResult<()> {
    use crate::gateway_util::{BearerCheck, check_bearer};
    match check_bearer(headers, &state.token) {
        BearerCheck::Ok => Ok(()),
        BearerCheck::Missing => Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "bearer token required".into(),
        )),
        BearerCheck::Invalid => Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "invalid bearer token".into(),
        )),
    }
}

fn valid_tts_voice(engine: &str, voice: &str) -> bool {
    match engine {
        "kokoro" => [
            "af_sarah",
            "af_bella",
            "af_nicole",
            "am_onyx",
            "am_michael",
            "am_echo",
        ]
        .contains(&voice),
        "piper" => ["en_US-joe-medium", "en_US-norman-medium"].contains(&voice),
        "pocket" => [
            "alba",
            "anna",
            "caro_davy",
            "marius",
            "javert",
            "bill_boerst",
            "peter_yearsley",
            "stuart_bell",
        ]
        .contains(&voice),
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

/// `POST /v1/images`: CPU txt2img on a c0mpute worker (`diffusion.txt2img`).
/// Body: `{prompt, width?, height?, steps?, seed?}` (see `diffusion::Txt2ImgSpec`).
/// Returns `image/png`. Expect ~90-150 s.
/// `POST /v1/videos`: a video job (Wan 2.2 / LTX-2.3) as a JSON body of
/// [`crate::video::VideoJobInline`] fields. Send `image_base64` for
/// image-to-video; this gateway stores and serves the image to the worker
/// itself. Answers `video/mp4`.
#[derive(Debug, Deserialize)]
struct VideoRequest {
    #[serde(flatten)]
    job: crate::video::VideoJobInline,
    #[serde(default)]
    image_base64: Option<String>,
}

async fn generate_video(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(input): Json<VideoRequest>,
) -> ApiResult<Response> {
    use base64::Engine;
    authenticate(&state, &headers)?;
    let mut job = input.job;
    let workload = match input.image_base64.as_deref() {
        Some(b64) => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64.trim())
                .map_err(|_| ApiError(StatusCode::BAD_REQUEST, "image_base64 is not base64".into()))?;
            if bytes.is_empty() || bytes.len() > crate::video::MAX_IMAGE_BYTES {
                return Err(ApiError(
                    StatusCode::BAD_REQUEST,
                    "image must be 1 byte to 32 MiB".into(),
                ));
            }
            let image = crate::pieces::put_bytes_pieces(
                &state.net,
                &state.store,
                &bytes,
                crate::pieces::INPUT_PIECE_BYTES,
            )
            .await
            .map_err(gateway_failure)?;
            job.image_hash = Some(image.hash.clone());
            if image.pieces.len() > 1 {
                job.image_size = Some(image.size);
                job.image_pieces = Some(image.pieces);
            }
            job.image_provider_peer_id = Some(state.net.peer_id().to_base58());
            crate::video::WORKLOAD_IMG2VID
        }
        None => crate::video::WORKLOAD_TXT2VID,
    };
    job.validate(workload)
        .map_err(|message| ApiError(StatusCode::BAD_REQUEST, message))?;
    let _slot = state
        .image_slots
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| {
            ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "video queue is unavailable".into(),
            )
        })?;
    let spec = serde_json::to_value(&job).map_err(|error| gateway_failure(error.into()))?;
    let (outcome, bytes) = run_speech_job(
        &state,
        workload,
        spec,
        vec![crate::video::ROLE_TAG.to_string()],
        crate::video::MAX_RESULT_BYTES,
    )
    .await?;
    let mut response = (StatusCode::OK, bytes).into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("video/mp4"));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    set_job_headers(
        response.headers_mut(),
        &outcome.job_id,
        &outcome.accepted_bid.bidder_peer_id,
    );
    Ok(response)
}

/// `POST /v1/music`: a song on a c0mpute worker (`music.txt2music`, ACE-Step
/// 1.5). Body: `{prompt, lyrics?, duration?, steps?, seed?}` (see
/// `music::Txt2MusicSpec`); empty lyrics = instrumental. Answers `audio/mpeg`
/// (or `audio/flac` from a worker without ffmpeg). Expect minutes on CPU.
async fn generate_music(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(input): Json<crate::music::Txt2MusicSpec>,
) -> ApiResult<Response> {
    authenticate(&state, &headers)?;
    input
        .validate()
        .map_err(|message| ApiError(StatusCode::BAD_REQUEST, message))?;
    let _slot = state
        .music_slots
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| ApiError(StatusCode::SERVICE_UNAVAILABLE, "music queue is unavailable".into()))?;
    let spec = serde_json::to_value(&input).map_err(|error| gateway_failure(error.into()))?;
    let (outcome, bytes) = run_speech_job(
        &state,
        crate::music::WORKLOAD_TXT2MUSIC,
        spec,
        vec![
            crate::music::ROLE_TAG.to_string(),
            crate::music::MODEL_TAG.to_string(),
        ],
        crate::music::MAX_RESULT_BYTES,
    )
    .await?;
    let content_type = crate::music::audio_content_type(&bytes).unwrap_or("audio/mpeg");
    let mut response = (StatusCode::OK, bytes).into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    set_job_headers(
        response.headers_mut(),
        &outcome.job_id,
        &outcome.accepted_bid.bidder_peer_id,
    );
    Ok(response)
}

async fn generate_image(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(input): Json<crate::diffusion::Txt2ImgSpec>,
) -> ApiResult<Response> {
    authenticate(&state, &headers)?;
    input
        .validate()
        .map_err(|message| ApiError(StatusCode::BAD_REQUEST, message))?;
    let _slot = state
        .image_slots
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| {
            ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "image queue is unavailable".into(),
            )
        })?;
    let spec = serde_json::to_value(&input).map_err(|error| gateway_failure(error.into()))?;
    let (outcome, bytes) = run_speech_job(
        &state,
        crate::diffusion::WORKLOAD_TXT2IMG,
        spec,
        vec![
            crate::diffusion::ROLE_TAG.to_string(),
            crate::diffusion::MODEL_TAG.to_string(),
        ],
        crate::diffusion::MAX_IMAGE_BYTES,
    )
    .await?;
    let mut response = (StatusCode::OK, bytes).into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    set_job_headers(
        response.headers_mut(),
        &outcome.job_id,
        &outcome.accepted_bid.bidder_peer_id,
    );
    Ok(response)
}

#[derive(Debug, Deserialize)]
struct OcrQuery {
    #[serde(default = "default_ocr_lang")]
    lang: String,
}

fn default_ocr_lang() -> String {
    crate::ocr::DEFAULT_LANG.into()
}

/// `POST /v1/ocr?lang=eng`: the raw image (PNG, JPEG, TIFF, WebP) or PDF as
/// the body. Answers the worker's JSON: `{v, ok, text, pages:[{page, text}],
/// engine, lang}` plus `job_id` and `worker_peer_id`.
async fn ocr_extract(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<OcrQuery>,
    body: Bytes,
) -> ApiResult<Response> {
    authenticate(&state, &headers)?;
    if body.is_empty() || body.len() > crate::ocr::MAX_INPUT_BYTES {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "document must be between 1 byte and 32 MiB".into(),
        ));
    }
    if !crate::ocr::valid_lang(&query.lang) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "lang must match [a-z_+]{3,32}, e.g. eng or eng+deu".into(),
        ));
    }
    let Some(format) = crate::ocr::detect_format(&body) else {
        return Err(ApiError(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "send a PNG, JPEG, TIFF, WebP image or a PDF".into(),
        ));
    };
    let _slot = state.ocr_slots.clone().acquire_owned().await.map_err(|_| {
        ApiError(StatusCode::SERVICE_UNAVAILABLE, "OCR queue is unavailable".into())
    })?;
    // One network message carries ~5 MiB at most: store the document in pieces.
    let input = crate::pieces::put_bytes_pieces(
        &state.net,
        &state.store,
        &body,
        crate::pieces::INPUT_PIECE_BYTES,
    )
    .await
    .map_err(gateway_failure)?;
    let mut spec = serde_json::json!({
        "v": 1,
        "input_provider_peer_id": state.net.peer_id().to_base58(),
        "format": format,
        "lang": query.lang,
    });
    spec.as_object_mut()
        .expect("spec is an object")
        .extend(input.spec_fields("input"));
    let (outcome, bytes) = run_speech_job(
        &state,
        crate::ocr::WORKLOAD,
        spec,
        vec![crate::ocr::ROLE_TAG.to_string()],
        crate::ocr::MAX_RESULT_BYTES,
    )
    .await?;
    json_job_response(&bytes, &outcome)
}

/// OpenAI-style embeddings request: `input` is one string or a list.
#[derive(Debug, Deserialize)]
struct EmbeddingsRequest {
    input: EmbeddingsInput,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum EmbeddingsInput {
    One(String),
    Many(Vec<String>),
}

/// `POST /v1/embeddings`: `{input: string | [string], model?}` in the OpenAI
/// shape, answered as `{object: "list", data: [{object: "embedding",
/// embedding, index}], model}`. Runs `embed.text` on a worker.
async fn embeddings(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(input): Json<EmbeddingsRequest>,
) -> ApiResult<Response> {
    authenticate(&state, &headers)?;
    let texts = match input.input {
        EmbeddingsInput::One(text) => vec![text],
        EmbeddingsInput::Many(texts) => texts,
    };
    let spec = serde_json::json!({ "v": 1, "model": input.model, "texts": texts });
    let (parsed, model) = crate::embed::parse_spec(&spec)
        .map_err(|message| ApiError(StatusCode::BAD_REQUEST, message))?;
    let expected = parsed.texts.len();
    let _slot = state.embed_slots.clone().acquire_owned().await.map_err(|_| {
        ApiError(StatusCode::SERVICE_UNAVAILABLE, "embeddings queue is unavailable".into())
    })?;
    // Up to 1 MiB of text: too big for the gossip offer, so the worker
    // fetches it from this node by hash.
    let spec_bytes = serde_json::to_vec(&spec).map_err(|error| gateway_failure(error.into()))?;
    let offer_spec = if spec_bytes.len() <= crate::workflow::MAX_INLINE_SPEC_BYTES {
        spec
    } else {
        let hash = state.store.put(&spec_bytes).await.map_err(gateway_failure)?;
        state.net.announce(&hash).await.map_err(gateway_failure)?;
        crate::workflow::spec_ref(&hash, spec_bytes.len())
    };
    let (outcome, bytes) = run_checked_job(
        &state,
        crate::embed::WORKLOAD,
        offer_spec,
        vec![
            crate::embed::ROLE_TAG.to_string(),
            crate::embed::model_tag(model),
        ],
        crate::embed::MAX_RESULT_BYTES,
        &|bytes| {
            serde_json::from_slice::<serde_json::Value>(bytes)
                .is_ok_and(|v| crate::embed::validate_result(&v, expected, model).is_ok())
        },
    )
    .await?;
    let result: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| {
        ApiError(StatusCode::BAD_GATEWAY, "worker returned invalid embeddings JSON".into())
    })?;
    let data: Vec<serde_json::Value> = result["vectors"]
        .as_array()
        .map(|vectors| {
            vectors
                .iter()
                .enumerate()
                .map(|(index, embedding)| {
                    serde_json::json!({ "object": "embedding", "embedding": embedding, "index": index })
                })
                .collect()
        })
        .unwrap_or_default();
    let mut response = Json(serde_json::json!({
        "object": "list",
        "data": data,
        "model": model.id,
        "dims": model.dims,
        "job_id": outcome.job_id,
        "worker_peer_id": outcome.accepted_bid.bidder_peer_id,
    }))
    .into_response();
    set_job_headers(
        response.headers_mut(),
        &outcome.job_id,
        &outcome.accepted_bid.bidder_peer_id,
    );
    Ok(response)
}

/// A worker's JSON result with the job id and worker peer id added.
fn json_job_response(bytes: &[u8], outcome: &crate::AuctionOutcome) -> ApiResult<Response> {
    let mut payload: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| {
        ApiError(StatusCode::BAD_GATEWAY, "worker returned invalid JSON".into())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ocr_results_are_checked() {
        assert!(valid_speech_result(
            crate::ocr::WORKLOAD,
            br#"{"v":1,"ok":true,"text":"hello","pages":[{"page":1,"text":"hello"}]}"#
        ));
        assert!(!valid_speech_result(crate::ocr::WORKLOAD, br#"{"v":1,"ok":true}"#));
        assert!(!valid_speech_result(crate::ocr::WORKLOAD, b"not json"));
    }

    #[test]
    fn embeddings_input_shapes() {
        let one: EmbeddingsRequest = serde_json::from_str(r#"{"input":"hi"}"#).unwrap();
        assert!(matches!(one.input, EmbeddingsInput::One(ref t) if t == "hi"));
        let many: EmbeddingsRequest =
            serde_json::from_str(r#"{"input":["a","b"],"model":"bge-small-en-v1.5"}"#).unwrap();
        assert!(matches!(many.input, EmbeddingsInput::Many(ref t) if t.len() == 2));
        assert!(serde_json::from_str::<EmbeddingsRequest>(r#"{"input":[1,2]}"#).is_err());
    }
}
