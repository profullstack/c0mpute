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
/// A song holds a worker ~2.5 min on CPU, so this waits behind two or three;
/// Infernet gives the whole call 19 min.
const MUSIC_QUEUE_WAIT: Duration = Duration::from_secs(8 * 60);
/// How long a video request keeps re-auctioning while nobody bids.
const VIDEO_QUEUE_WAIT: Duration = Duration::from_secs(60);
/// How long a speech, OCR or embedding request keeps re-auctioning while
/// nobody bids (a worker restarting into an update, a bid lost in transit).
const QUICK_QUEUE_WAIT: Duration = Duration::from_secs(20);
const MAX_CONCURRENT_OCR_JOBS: usize = 4;
const MAX_CONCURRENT_EMBED_JOBS: usize = 8;
const MAX_CONCURRENT_CAPTCHA_JOBS: usize = 16;

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
    /// Captcha jobs in flight (each a few KiB and a second or two of CPU).
    captcha_slots: Arc<Semaphore>,
    /// Warm, pinned sessions this node is holding (see `speech_session`).
    sessions: crate::speech_session::SessionStore,
    /// Workers that won and did not deliver, benched for a while.
    penalties: crate::penalty_box::PenaltyBox,
    /// Live stages of long requests (songs), by the caller's x-request-id.
    progress: crate::job_progress::ProgressBoard,
    /// Finished tracked requests, collectable at GET /v1/result/{id}.
    results: crate::job_progress::ResultStore,
    /// Unfinished tracked requests, re-run after a gateway restart.
    pending: crate::job_progress::PendingStore,
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
    let penalties = crate::penalty_box::PenaltyBox::persistent(
        crate::config::data_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("./c0mpute-data"))
            .join("gateway-penalties.json"),
    );
    // The served gateway keeps tracked jobs on disk, so a restart (an
    // auto-update) resumes them; the loopback tests use `router` in memory.
    let data_dir = crate::config::data_dir().unwrap_or_else(|| std::path::PathBuf::from("./c0mpute-data"));
    let app = router_with(supervisor.libp2p, supervisor.store, token, max_price_usd, penalties, Some(data_dir));
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("bind c0mpute speech gateway at {bind}"))?;
    tracing::info!(%bind, "c0mpute speech gateway listening");
    axum::serve(listener, app).await?;
    Ok(())
}

/// The gateway's routes over an already booted buyer node. `serve` uses it;
/// so do the loopback end-to-end tests, with their own node and listener.
pub fn router(
    net: Arc<Libp2pNetwork>,
    store: ChunkStore,
    token: String,
    max_price_usd: f64,
    penalties: crate::penalty_box::PenaltyBox,
) -> Router {
    router_with(net, store, token, max_price_usd, penalties, None)
}

/// `router`, optionally keeping tracked jobs and results under `persist`.
pub fn router_with(
    net: Arc<Libp2pNetwork>,
    store: ChunkStore,
    token: String,
    max_price_usd: f64,
    penalties: crate::penalty_box::PenaltyBox,
    persist: Option<std::path::PathBuf>,
) -> Router {
    let (results, pending) = match &persist {
        Some(dir) => (
            crate::job_progress::ResultStore::persistent(dir.join("gateway-results")),
            crate::job_progress::PendingStore::persistent(dir.join("gateway-jobs")),
        ),
        None => Default::default(),
    };
    let state = GatewayState {
        net,
        store,
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
        captcha_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_CAPTCHA_JOBS)),
        sessions: crate::speech_session::SessionStore::new(),
        progress: crate::job_progress::ProgressBoard::default(),
        results,
        pending,
        penalties,
    };
    resume_pending(&state);
    Router::new()
        .route("/healthz", get(healthz))
        // Per-workload auction outcomes over the last hour (no secrets).
        .route("/v1/auctions", get(auction_stats))
        .route("/v1/voices", get(voices))
        .route("/v1/tts", post(synthesize))
        .route("/v1/stt", post(transcribe))
        .route("/v1/images", post(generate_image))
        .route("/v1/music", post(generate_music))
        .route("/v1/progress/{request}", get(job_progress))
        .route("/v1/result/{request}", get(job_result))
        .route("/v1/videos", post(generate_video))
        // OCR takes a 32 MiB document, above the router-wide audio limit.
        .route(
            "/v1/ocr",
            post(ocr_extract).layer(DefaultBodyLimit::max(crate::ocr::MAX_INPUT_BYTES)),
        )
        .route("/v1/embeddings", post(embeddings))
        .route("/v1/captcha", post(captcha_solve))
        // Warm, pinned sessions: open once per call, then say a sentence at a
        // time with no auction and no model reload in the way.
        .route("/v1/sessions", post(open_session).get(list_sessions))
        .route("/v1/sessions/{session}/say", post(say_on_session))
        .route("/v1/sessions/{session}", delete(close_session))
        .layer(DefaultBodyLimit::max(MAX_AUDIO_BYTES))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn auction_stats() -> Json<serde_json::Value> {
    Json(crate::auction_stats::snapshot_json())
}

/// How long a bid window an auction for `workload` gets. Speech answers in
/// seconds and sits in front of a caller; images, songs and clips take
/// minutes, so a slightly longer window that catches a slow bid is cheap.
fn bid_window_for(workload: &str) -> Duration {
    if is_video(workload)
        || workload == crate::diffusion::WORKLOAD_TXT2IMG
        || workload == crate::music::WORKLOAD_TXT2MUSIC
    {
        Duration::from_secs(6)
    } else {
        Duration::from_secs(4)
    }
}

/// How long a request for `workload` keeps re-auctioning while no worker
/// bids, before the caller is told none is available.
fn no_bid_wait_for(workload: &str) -> Duration {
    if workload == crate::diffusion::WORKLOAD_TXT2IMG {
        IMAGE_QUEUE_WAIT
    } else if workload == crate::music::WORKLOAD_TXT2MUSIC {
        MUSIC_QUEUE_WAIT
    } else if is_video(workload) {
        VIDEO_QUEUE_WAIT
    } else {
        QUICK_QUEUE_WAIT
    }
}

/// Pause before the `round`-th re-auction (0-based) of a request nobody bid
/// on: 1, 2, 4, 8 s, then every 15 s, never past what is left of `budget`.
fn no_bid_backoff(round: u32, left: Duration) -> Duration {
    Duration::from_secs(1u64 << round.min(4))
        .min(Duration::from_secs(15))
        .min(left)
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
    fallback_bidders: &[String],
) -> Result<crate::AuctionOutcome, (ApiError, Option<String>)> {
    // Learn the winner as soon as it is chosen, so a winner that then goes
    // silent can be benched and the job re-auctioned.
    let (winner_tx, mut winner_rx) = tokio::sync::mpsc::unbounded_channel();
    let auction = JobAuction::new(workload, spec)
        .with_accept_notify(winner_tx)
        .with_required_capabilities(required)
        .with_excluded_bidders(excluded_bidders.to_vec())
        .with_fallback_bidders(fallback_bidders.to_vec())
        .with_max_price_usd(state.max_price_usd)
        .with_bid_window(bid_window_for(workload))
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
    // Read the winner as soon as it is chosen (not only on failure), so a
    // caller polling /v1/progress sees which node is working and its ETA.
    let auction_run = run_auction(state.net.clone(), auction);
    tokio::pin!(auction_run);
    let mut winner_seen: Option<String> = None;
    let result = loop {
        tokio::select! {
            result = &mut auction_run => break result,
            Some((job_id, bid)) = winner_rx.recv() => {
                crate::job_progress::set_stage("rendering");
                crate::job_progress::report(|p| {
                    p.worker = Some(bid.bidder_peer_id.clone());
                    p.job_id = Some(job_id);
                    p.eta_s = Some(bid.eta_seconds);
                });
                winner_seen = Some(bid.bidder_peer_id);
            }
        }
    };
    result
        .map_err(|error| {
            let winner = winner_seen
                .or_else(|| winner_rx.try_recv().ok().map(|(_, bid)| bid.bidder_peer_id));
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
    // Workers that failed this request are out; workers benched for earlier
    // failures are only a fallback when nobody else bids.
    let mut excluded_bidders: Vec<String> = Vec::new();
    let mut last_error = None;
    let started = std::time::Instant::now();
    let mut attempt = 0;
    let mut no_bid_rounds = 0u32;
    // An update waits for this request instead of killing it mid-flight.
    let _long_job = c0mpute_update::LongJob::start();

    while attempt < MAX_SPEECH_WORKER_ATTEMPTS {
        attempt += 1;
        crate::job_progress::report(|p| {
            p.attempt = attempt as u32;
            p.worker = None;
            p.eta_s = None;
        });
        crate::job_progress::report(|p| {
            if p.stage != "queued" {
                p.stage = "finding_worker";
            }
        });
        let outcome = match run_speech_auction(
            state,
            workload,
            spec.clone(),
            required.clone(),
            &excluded_bidders,
            &state
                .penalties
                .benched()
                .into_iter()
                .filter(|p| !excluded_bidders.contains(p))
                .collect::<Vec<_>>(),
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
            // Nobody bid: every worker busy (an image holds one ~2 min), one
            // restarting into an update, or a bid lost in transit. Re-auction
            // with backoff instead of failing, without spending one of the
            // real attempts, for as long as this workload is worth waiting.
            Err((error, None)) if started.elapsed() < no_bid_wait_for(workload) => {
                let pause = no_bid_backoff(
                    no_bid_rounds,
                    no_bid_wait_for(workload).saturating_sub(started.elapsed()),
                );
                no_bid_rounds += 1;
                tracing::info!(workload, waited_secs = started.elapsed().as_secs(), round = no_bid_rounds,
                    "no free worker yet; waiting");
                crate::job_progress::set_stage("queued");
                attempt -= 1;
                last_error.get_or_insert(error);
                tokio::time::sleep(pause).await;
                continue;
            }
            Err((error, None)) => return Err(last_error.unwrap_or(error)),
        };
        let worker = outcome.accepted_bid.bidder_peer_id.clone();
        crate::job_progress::set_stage("fetching_result");
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
                let why = error.1.clone();
                crate::job_progress::set_stage("retrying");
                crate::job_progress::report(|p| p.detail = Some(why));
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

/// The small Wan model is served on CPU, whose workers take at most
/// `video_cpu::MAX_FRAMES` frames, `MAX_STEPS` steps and 480x272. A request
/// with the GPU-sized defaults (81 frames, 30 steps, 1280x704) was refused by
/// every CPU node's offer filter, so it never got a bid. Clip it to what they
/// take, keeping the orientation, as songs are clipped to their model.
fn clip_for_cpu_video(job: &mut crate::video::VideoJobInline, workload: &str) {
    if workload != crate::video::WORKLOAD_TXT2VID || job.model != crate::video_cpu::MODEL_ID {
        return;
    }
    job.frames = job.frames.min(crate::video_cpu::MAX_FRAMES);
    job.steps = job.steps.min(crate::video_cpu::MAX_STEPS);
    if job.width.saturating_mul(job.height) > crate::video_cpu::MAX_PIXELS {
        (job.width, job.height) = if job.height > job.width { (272, 480) } else { (480, 272) };
    }
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
/// `POST /v1/videos`: a video job (Wan 2.2 / 2.1, LTX-2.3) as a JSON body of
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
    Json(mut raw): Json<serde_json::Value>,
) -> ApiResult<Response> {
    use base64::Engine;
    authenticate(&state, &headers)?;
    // A buyer who names only the model gets that model's own defaults (the
    // small Wan 2.1 model is 33 frames of 480x272, not 81 of 1280x704).
    crate::video::apply_model_defaults(&mut raw);
    let input: VideoRequest = serde_json::from_value(raw)
        .map_err(|error| ApiError(StatusCode::BAD_REQUEST, format!("invalid video request: {error}")))?;
    let mut job = input.job;
    if crate::video::manifest_model(&job.model).is_none() {
        let known: Vec<&str> = crate::video::manifest_models().iter().map(|m| m.id.as_str()).collect();
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("unknown video model {}; have {}", job.model, known.join(", ")),
        ));
    }
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
    clip_for_cpu_video(&mut job, workload);
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
        // The model tag: only a node that can run this model (a big enough
        // card, or CPU video for the small one) is eligible.
        vec![crate::video::ROLE_TAG.to_string(), crate::video::model_tag(&job.model)],
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
    let slot = state
        .music_slots
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| ApiError(StatusCode::SERVICE_UNAVAILABLE, "music queue is unavailable".into()))?;
    let spec = serde_json::to_value(input.clip_for_model())
        .map_err(|error| gateway_failure(error.into()))?;
    let done = run_tracked(
        &state,
        request_id(&headers),
        slot,
        crate::music::WORKLOAD_TXT2MUSIC,
        spec,
        vec![
            crate::music::ROLE_TAG.to_string(),
            crate::music::MODEL_TAG.to_string(),
        ],
        crate::music::MAX_RESULT_BYTES,
        |bytes| crate::music::audio_content_type(bytes).unwrap_or("audio/mpeg"),
    )
    .await?;
    Ok(stored_response(done))
}

/// The caller's `x-request-id`, when it is short and URL-safe.
fn request_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|k| crate::job_progress::valid_key(k))
        .map(str::to_string)
}

/// Run a long job (song, image). With a request id it runs DETACHED from the
/// HTTP request: if the caller's connection drops (Infernet redeploying
/// mid-song cut every song in flight, 2026-11-09), the job carries on, holds
/// its queue slot to the end, and its result waits at GET /v1/result/{id}.
/// Progress is recorded under the same id (see `job_progress`).
#[allow(clippy::too_many_arguments)]
async fn run_tracked(
    state: &GatewayState,
    request_id: Option<String>,
    slot: tokio::sync::OwnedSemaphorePermit,
    workload: &'static str,
    spec: serde_json::Value,
    required: Vec<String>,
    max_bytes: usize,
    content_type: fn(&[u8]) -> &'static str,
) -> ApiResult<crate::job_progress::Stored> {
    use crate::job_progress::Stored;
    let Some(key) = request_id else {
        let (outcome, bytes) = run_speech_job(state, workload, spec, required, max_bytes).await?;
        drop(slot);
        return Ok(Stored::Done {
            content_type: content_type(&bytes),
            bytes: Arc::new(bytes),
            job_id: outcome.job_id,
            worker: outcome.accepted_bid.bidder_peer_id,
        });
    };
    state.progress.start(&key);
    // On disk until it finishes, so a gateway restart can run it again.
    state.pending.save(
        &key,
        &crate::job_progress::PendingJob {
            workload: workload.to_string(),
            spec: spec.clone(),
            required: required.clone(),
            max_bytes,
            started_at_ms: crate::job_progress::now_ms_pub(),
        },
    );
    let st = state.clone();
    let task_key = key.clone();
    let task = tokio::spawn(crate::job_progress::CURRENT.scope(
        (state.progress.clone(), key.clone()),
        async move {
            let _slot = slot;
            // A gateway auto-update waits for tracked jobs (a caller is
            // waiting on them) instead of cutting them off.
            let _long = c0mpute_update::LongJob::start();
            let result = run_speech_job(&st, workload, spec, required, max_bytes).await;
            crate::job_progress::set_stage(if result.is_ok() { "done" } else { "failed" });
            let stored = match result {
                Ok((outcome, bytes)) => Stored::Done {
                    content_type: content_type(&bytes),
                    bytes: Arc::new(bytes),
                    job_id: outcome.job_id,
                    worker: outcome.accepted_bid.bidder_peer_id,
                },
                Err(ApiError(status, message)) => Stored::Failed { status: status.as_u16(), message },
            };
            st.results.put(&task_key, stored.clone());
            st.pending.remove(&task_key);
            stored
        },
    ));
    match task.await {
        Ok(Stored::Failed { status, message }) => Err(ApiError(
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            message,
        )),
        Ok(done) => Ok(done),
        Err(error) => Err(ApiError(StatusCode::BAD_GATEWAY, format!("job task failed: {error}"))),
    }
}

/// After a restart: run again every tracked request that had not finished,
/// under the same request id, so `GET /v1/result/{id}` still delivers it.
/// What a resumed request needs: its workload, its queue and its content type.
type ResumeTarget = (&'static str, Arc<Semaphore>, fn(&[u8]) -> &'static str);

fn resume_pending(state: &GatewayState) {
    use crate::job_progress::Stored;
    for (key, job) in state.pending.take_resumable() {
        let (workload, slots, content_type): ResumeTarget =
            match job.workload.as_str() {
                crate::music::WORKLOAD_TXT2MUSIC => (
                    crate::music::WORKLOAD_TXT2MUSIC,
                    state.music_slots.clone(),
                    |b| crate::music::audio_content_type(b).unwrap_or("audio/mpeg"),
                ),
                crate::diffusion::WORKLOAD_TXT2IMG => {
                    (crate::diffusion::WORKLOAD_TXT2IMG, state.image_slots.clone(), |_| "image/png")
                }
                _ => {
                    state.pending.remove(&key);
                    continue;
                }
            };
        tracing::info!(%key, workload, "resuming a tracked request after a restart");
        state.progress.start(&key);
        let st = state.clone();
        let task_key = key.clone();
        tokio::spawn(crate::job_progress::CURRENT.scope((state.progress.clone(), key), async move {
            let Ok(_slot) = slots.acquire_owned().await else { return };
            let _long = c0mpute_update::LongJob::start();
            let result = run_speech_job(&st, workload, job.spec, job.required, job.max_bytes).await;
            crate::job_progress::set_stage(if result.is_ok() { "done" } else { "failed" });
            let stored = match result {
                Ok((outcome, bytes)) => Stored::Done {
                    content_type: content_type(&bytes),
                    bytes: Arc::new(bytes),
                    job_id: outcome.job_id,
                    worker: outcome.accepted_bid.bidder_peer_id,
                },
                Err(ApiError(status, message)) => Stored::Failed { status: status.as_u16(), message },
            };
            st.results.put(&task_key, stored);
            st.pending.remove(&task_key);
        }));
    }
}

/// 200 with the bytes, their type and the job headers.
fn stored_response(done: crate::job_progress::Stored) -> Response {
    let crate::job_progress::Stored::Done { bytes, content_type, job_id, worker } = done else {
        return (StatusCode::BAD_GATEWAY, "job failed").into_response();
    };
    let mut response = (StatusCode::OK, (*bytes).clone()).into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    set_job_headers(response.headers_mut(), &job_id, &worker);
    response
}

/// `GET /v1/result/{request}`: a tracked request's result (200 with the
/// bytes), its live progress while it runs (202 JSON), or 404. Lets a caller
/// that restarted mid-job collect it.
async fn job_result(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Path(request): Path<String>,
) -> ApiResult<Response> {
    authenticate(&state, &headers)?;
    if !crate::job_progress::valid_key(&request) {
        return Err(ApiError(StatusCode::BAD_REQUEST, "bad request id".into()));
    }
    match state.results.get(&request) {
        Some(crate::job_progress::Stored::Failed { status, message }) => Err(ApiError(
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            message,
        )),
        Some(done) => Ok(stored_response(done)),
        None => {
            let snapshot = state
                .progress
                .snapshot(&request)
                .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "no such request".into()))?;
            let mut response = (StatusCode::ACCEPTED, Json(snapshot)).into_response();
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            Ok(response)
        }
    }
}

/// `GET /v1/progress/{request}`: the live stage of a request sent with that
/// `x-request-id` (see `job_progress`). 404 until it starts and 30 min after.
async fn job_progress(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Path(request): Path<String>,
) -> ApiResult<Response> {
    authenticate(&state, &headers)?;
    if !crate::job_progress::valid_key(&request) {
        return Err(ApiError(StatusCode::BAD_REQUEST, "bad request id".into()));
    }
    let snapshot = state
        .progress
        .snapshot(&request)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "no such request".into()))?;
    let mut response = Json(snapshot).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
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
    let slot = state
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
    let done = run_tracked(
        &state,
        request_id(&headers),
        slot,
        crate::diffusion::WORKLOAD_TXT2IMG,
        spec,
        vec![
            crate::diffusion::ROLE_TAG.to_string(),
            crate::diffusion::MODEL_TAG.to_string(),
        ],
        crate::diffusion::MAX_IMAGE_BYTES,
        |_| "image/png",
    )
    .await?;
    Ok(stored_response(done))
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

/// `POST /v1/captcha`: solve one captcha.
///
/// Body: `{type, image?, background?, charset?, case?, length?, audio?,
/// audio_format?, question?}` where `type` is `image` (default), `slider`,
/// `gap`, `detect` (ddddocr on a worker), `audio` (whisper on a worker, then
/// spoken digits/letters normalized) or `math` (answered here). Images and
/// audio are base64 or `data:` URLs. Answers `{v, ok, kind, text | x,y |
/// boxes, confidence?, ...}` plus `job_id` and `worker_peer_id` when a worker
/// ran it. Token captchas (reCAPTCHA, hCaptcha, Turnstile) get a 501.
async fn captcha_solve(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(req): Json<CaptchaRequest>,
) -> ApiResult<Response> {
    use crate::captcha;
    authenticate(&state, &headers)?;
    let kind = match req.kind.as_deref() {
        Some(k) => k.trim().to_ascii_lowercase(),
        None if req.audio.is_some() => "audio".into(),
        None if req.question.is_some() => "math".into(),
        None => "image".into(),
    };
    let case = req.case.as_deref();
    if case.is_some_and(|c| !matches!(c, "lower" | "upper" | "any")) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "case must be lower, upper or any".into(),
        ));
    }
    let finish = |mut payload: serde_json::Value| -> serde_json::Value {
        if let Some(text) = payload.get("text").and_then(|t| t.as_str()) {
            let text = captcha::apply_case(text, case);
            if let Some(len) = req.length {
                payload["length_ok"] = serde_json::json!(text.chars().count() == len);
            }
            payload["text"] = serde_json::json!(text);
        }
        payload
    };
    match kind.as_str() {
        "math" => {
            let question = req.question.as_deref().unwrap_or("");
            if question.is_empty() || question.chars().count() > captcha::MAX_QUESTION_CHARS {
                return Err(ApiError(
                    StatusCode::BAD_REQUEST,
                    "math needs a question of 1 to 256 characters".into(),
                ));
            }
            let Some(answer) = captcha::solve_math(question) else {
                return Err(ApiError(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "no arithmetic found in the question".into(),
                ));
            };
            Ok(Json(finish(serde_json::json!({
                "v": 1, "ok": true, "kind": "math", "text": answer, "engine": "local",
            })))
            .into_response())
        }
        "audio" => {
            use base64::Engine as _;
            let raw = req.audio.as_deref().unwrap_or("");
            let audio = base64::engine::general_purpose::STANDARD
                .decode(captcha::strip_data_url(raw))
                .map_err(|_| ApiError(StatusCode::BAD_REQUEST, "audio is not valid base64".into()))?;
            if audio.is_empty() || audio.len() > captcha::MAX_AUDIO_BYTES {
                return Err(ApiError(
                    StatusCode::BAD_REQUEST,
                    "audio must be between 1 byte and 2 MiB".into(),
                ));
            }
            let format = req
                .audio_format
                .as_deref()
                .and_then(|f| ["wav", "mp3", "ogg", "webm", "m4a"].into_iter().find(|k| *k == f))
                .or_else(|| sniff_audio(&audio))
                .ok_or_else(|| {
                    ApiError(
                        StatusCode::UNSUPPORTED_MEDIA_TYPE,
                        "send WAV, MP3, Ogg, WebM or M4A audio".into(),
                    )
                })?;
            let _slot = state.stt_slots.clone().acquire_owned().await.map_err(|_| {
                ApiError(StatusCode::SERVICE_UNAVAILABLE, "speech queue is unavailable".into())
            })?;
            let pieces = crate::pieces::put_bytes_pieces(
                &state.net,
                &state.store,
                &audio,
                crate::pieces::INPUT_PIECE_BYTES,
            )
            .await
            .map_err(gateway_failure)?;
            let mut spec = serde_json::json!({
                "audio_provider_peer_id": state.net.peer_id().to_string(),
                "audio_format": format,
                "model": "base",
                "language": "en",
            });
            spec.as_object_mut()
                .expect("spec is an object")
                .extend(pieces.spec_fields("audio"));
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
            let transcript: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| {
                ApiError(StatusCode::BAD_GATEWAY, "worker returned invalid transcript JSON".into())
            })?;
            let heard = transcript
                .get("text")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let payload = finish(serde_json::json!({
                "v": 1,
                "ok": true,
                "kind": "audio",
                "text": captcha::normalize_spoken(&heard),
                "transcript": heard,
                "engine": "faster-whisper",
                "job_id": outcome.job_id,
                "worker_peer_id": outcome.accepted_bid.bidder_peer_id,
            }));
            let mut response = Json(payload).into_response();
            set_job_headers(
                response.headers_mut(),
                &outcome.job_id,
                &outcome.accepted_bid.bidder_peer_id,
            );
            Ok(response)
        }
        k if captcha::WORKER_KINDS.contains(&k) => {
            let mut spec = serde_json::json!({
                "v": 1,
                "kind": k,
                "image": captcha::strip_data_url(req.image.as_deref().unwrap_or("")),
            });
            if let Some(bg) = req.background.as_deref() {
                spec["background"] = serde_json::json!(captcha::strip_data_url(bg));
            }
            if let Some(cs) = req.charset.as_deref() {
                spec["charset"] = serde_json::json!(cs);
            }
            let parsed =
                captcha::parse_spec(&spec).map_err(|m| ApiError(StatusCode::BAD_REQUEST, m))?;
            // Hand workers the resolved charset, not the preset name.
            if let Some(cs) = parsed.charset.as_deref() {
                spec["charset"] = serde_json::json!(cs);
            }
            let _slot = state.captcha_slots.clone().acquire_owned().await.map_err(|_| {
                ApiError(StatusCode::SERVICE_UNAVAILABLE, "captcha queue is unavailable".into())
            })?;
            let spec_bytes =
                serde_json::to_vec(&spec).map_err(|error| gateway_failure(error.into()))?;
            // Up to two 256 KiB images: often too big for the gossip offer, so
            // the worker fetches the spec from this node by hash.
            let offer_spec = if spec_bytes.len() <= crate::workflow::MAX_INLINE_SPEC_BYTES {
                spec
            } else {
                let hash = state.store.put(&spec_bytes).await.map_err(gateway_failure)?;
                state.net.announce(&hash).await.map_err(gateway_failure)?;
                crate::workflow::spec_ref(&hash, spec_bytes.len())
            };
            let expected = parsed.kind.clone();
            let (outcome, bytes) = run_checked_job(
                &state,
                captcha::WORKLOAD,
                offer_spec,
                vec![captcha::ROLE_TAG.to_string()],
                captcha::MAX_RESULT_BYTES,
                &move |bytes| {
                    serde_json::from_slice::<serde_json::Value>(bytes)
                        .is_ok_and(|v| captcha::valid_result(&expected, &v))
                },
            )
            .await?;
            let payload: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| {
                ApiError(StatusCode::BAD_GATEWAY, "worker returned invalid JSON".into())
            })?;
            let rewritten =
                serde_json::to_vec(&finish(payload)).map_err(|error| gateway_failure(error.into()))?;
            json_job_response(&rewritten, &outcome)
        }
        other => Err(ApiError(
            StatusCode::NOT_IMPLEMENTED,
            captcha::unsupported_kind_message(other),
        )),
    }
}

#[derive(Debug, Deserialize)]
struct CaptchaRequest {
    #[serde(default, alias = "type")]
    kind: Option<String>,
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    background: Option<String>,
    #[serde(default)]
    charset: Option<String>,
    #[serde(default)]
    case: Option<String>,
    #[serde(default)]
    length: Option<usize>,
    #[serde(default)]
    audio: Option<String>,
    #[serde(default)]
    audio_format: Option<String>,
    #[serde(default)]
    question: Option<String>,
}

/// Audio container by magic bytes.
fn sniff_audio(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WAVE" {
        Some("wav")
    } else if bytes.starts_with(b"ID3") || (bytes.len() > 1 && bytes[0] == 0xFF && bytes[1] & 0xE0 == 0xE0) {
        Some("mp3")
    } else if bytes.starts_with(b"OggS") {
        Some("ogg")
    } else if bytes.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        Some("webm")
    } else if bytes.len() >= 8 && &bytes[4..8] == b"ftyp" {
        Some("m4a")
    } else {
        None
    }
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
    fn cpu_video_requests_are_clipped_to_what_cpu_nodes_take() {
        let mut job: crate::video::VideoJobInline = serde_json::from_value(serde_json::json!({
            "model": crate::video_cpu::MODEL_ID,
            "prompt": "a cat",
        }))
        .unwrap();
        clip_for_cpu_video(&mut job, crate::video::WORKLOAD_TXT2VID);
        crate::video_cpu::fits(&job, crate::video::WORKLOAD_TXT2VID).unwrap();
        assert_eq!((job.width, job.height), (480, 272));
        // Portrait stays portrait; other models are left alone.
        let mut tall = job.clone();
        (tall.width, tall.height) = (704, 1280);
        clip_for_cpu_video(&mut tall, crate::video::WORKLOAD_TXT2VID);
        assert_eq!((tall.width, tall.height), (272, 480));
        let mut big: crate::video::VideoJobInline = serde_json::from_value(serde_json::json!({
            "model": "wan2.1-t2v-14b",
            "prompt": "a cat",
        }))
        .unwrap();
        let before = (big.frames, big.steps, big.width);
        clip_for_cpu_video(&mut big, crate::video::WORKLOAD_TXT2VID);
        assert_eq!((big.frames, big.steps, big.width), before);
    }

    #[test]
    fn no_bid_backoff_doubles_then_holds_and_respects_the_budget() {
        let lots = Duration::from_secs(600);
        let pauses: Vec<u64> = (0..7).map(|r| no_bid_backoff(r, lots).as_secs()).collect();
        assert_eq!(pauses, vec![1, 2, 4, 8, 15, 15, 15]);
        assert_eq!(no_bid_backoff(6, Duration::from_millis(300)), Duration::from_millis(300));
    }

    #[test]
    fn slow_workloads_get_longer_windows_and_waits() {
        assert_eq!(bid_window_for("tts.synthesize"), Duration::from_secs(4));
        assert_eq!(bid_window_for(crate::diffusion::WORKLOAD_TXT2IMG), Duration::from_secs(6));
        assert_eq!(no_bid_wait_for("tts.synthesize"), QUICK_QUEUE_WAIT);
        assert_eq!(no_bid_wait_for(crate::video::WORKLOAD_TXT2VID), VIDEO_QUEUE_WAIT);
        assert_eq!(no_bid_wait_for(crate::music::WORKLOAD_TXT2MUSIC), MUSIC_QUEUE_WAIT);
    }

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
