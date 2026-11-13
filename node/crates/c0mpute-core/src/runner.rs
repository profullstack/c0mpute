//! Workload runners — the worker-side counterpart to `dispatch::run_worker_subscriber`.
//!
//! For each enabled role, the supervisor spawns:
//!
//!   1. The dispatch task (subscribes to `c0mpute/jobs/<workload>`,
//!      bids on offers we're capable of, watches for accepts naming us,
//!      and forwards accepted jobs to the runner channel).
//!   2. A runner task (this module) — receives accepted jobs, executes
//!      the workload via the in-process workload code (e.g.
//!      `c0mpute_transcode::transcode`), and publishes a `JobReceipt`.
//!
//! The receipt is published on the same job topic the offer was on, so
//! the buyer (which is still subscribed) sees it.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use c0mpute_net::Libp2pNetwork;
use c0mpute_net::topics::{JobAccept, JobReceipt, JobStatus, job_topic};
use c0mpute_proto::TranscodeSpec;
use c0mpute_store::ChunkStore;
use tokio::fs;
use tokio::sync::{Semaphore, mpsc};
use tracing::{info, warn};

/// Job handed off from the dispatch loop to a per-workload runner.
#[derive(Clone, Debug)]
pub struct AcceptedJob {
    pub accept: JobAccept,
}

/// A workload's share of the node's compute, shared between its runner and
/// its dispatch loop so the worker only bids for work it can take now and
/// quotes an ETA from how long its jobs actually run.
///
/// `compute_slots` is node-wide (every workload draws from it); the job
/// duration average is per workload, since a transcode and a TTS line take
/// very different times.
#[derive(Clone, Debug)]
pub struct WorkerLoad {
    compute_slots: Arc<Semaphore>,
    total_slots: usize,
    /// Moving average of recent completed jobs' wall time in ms; 0 = none.
    recent_job_ms: Arc<AtomicU64>,
    /// Where the average is kept across restarts (see [`Self::with_history`]).
    history: Option<Arc<std::path::PathBuf>>,
}

impl WorkerLoad {
    pub fn new(compute_slots: Arc<Semaphore>, total_slots: usize) -> Self {
        Self {
            compute_slots,
            total_slots: total_slots.max(1),
            recent_job_ms: Arc::new(AtomicU64::new(0)),
            history: None,
        }
    }

    /// Keep the recent-job average in `path` and start from what it holds.
    /// Workers restart on every release; without this each restart forgot
    /// its job times and quoted the 60 s placeholder ETA, so a buyer waited
    /// 6 min on a node that needs 10 (bonita, 2026-11-09: images done in
    /// 382-623 s, after the buyer had given up).
    pub fn with_history(mut self, path: std::path::PathBuf) -> Self {
        if let Some(ms) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|ms| *ms > 0)
        {
            self.recent_job_ms.store(ms, Ordering::Relaxed);
        }
        self.history = Some(Arc::new(path));
        self
    }

    pub fn compute_slots(&self) -> Arc<Semaphore> {
        self.compute_slots.clone()
    }

    /// Compute slots not held by a running job right now.
    pub fn free_slots(&self) -> usize {
        self.compute_slots.available_permits()
    }

    pub fn total_slots(&self) -> usize {
        self.total_slots
    }

    /// Average wall time of recent completed jobs; `None` before the first.
    pub fn recent_job_duration(&self) -> Option<Duration> {
        match self.recent_job_ms.load(Ordering::Relaxed) {
            0 => None,
            ms => Some(Duration::from_millis(ms)),
        }
    }

    /// Fold a completed job's wall time into the average (weight 0.3 on the
    /// newest, so one outlier does not swing the quote).
    pub fn record_job(&self, took: Duration) {
        let ms = (took.as_millis() as u64).max(1);
        let _ = self
            .recent_job_ms
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
                Some(if old == 0 {
                    ms
                } else {
                    (old * 7 + ms * 3) / 10
                })
            });
        if let Some(path) = self.history.as_deref() {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(path, self.recent_job_ms.load(Ordering::Relaxed).to_string());
        }
    }
}

/// Remove a job's scratch directory once its result is stored (or the job
/// failed). The stored result lives in the chunk store, not here.
pub(crate) async fn cleanup_job_dir(cache_root: &std::path::Path, job_id: &str) {
    if let Ok(dir) = safe_job_dir(cache_root, job_id) {
        if let Err(e) = fs::remove_dir_all(&dir).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!(%job_id, err = %e, "could not remove job directory");
            }
        }
    }
}

/// Phase-1 inline schema for an `ffmpeg.transcode` workload. Lives in
/// `JobOffer.spec_inline` (and again in `JobAccept.spec_inline`).
#[derive(serde::Deserialize, serde::Serialize, Clone, Debug)]
pub struct TranscodeJobInline {
    /// HTTP URL the worker should download the input from.
    pub input_url: String,
    /// Human-readable preset (informational only — the actual ffmpeg
    /// args are derived from `spec`).
    pub preset: String,
    /// Concrete TranscodeSpec the worker passes to ffmpeg.
    pub spec: TranscodeSpec,
}

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug)]
pub struct TtsJobInline {
    pub text: String,
    pub engine: String,
    pub voice: String,
    #[serde(default = "default_locale")]
    pub locale: String,
    #[serde(default = "default_speed")]
    pub speed: f32,
    #[serde(default)]
    pub persona: String,
    #[serde(default)]
    pub exaggeration: Option<f32>,
}

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug)]
pub struct SttJobInline {
    /// Content hash of the whole audio, already announced by the buyer (as
    /// one chunk, or as the pieces below).
    pub audio_hash: String,
    /// Audio size in bytes; present with `audio_pieces`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_size: Option<u64>,
    /// Hashes of the audio's [`crate::pieces::INPUT_PIECE_BYTES`] pieces, in
    /// order. Absent: `audio_hash` is a single chunk (older buyers, or audio
    /// that fits in one piece).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_pieces: Option<Vec<String>>,
    /// Peer that owns the input audio, supplied by the buyer.
    #[serde(default)]
    pub audio_provider_peer_id: Option<String>,
    /// Hex key the stored audio is sealed under (DIP-0032); rides only inside
    /// the sealed spec. Absent: the stored bytes are plaintext.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_key: Option<String>,
    #[serde(default = "default_stt_model")]
    pub model: String,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default = "default_audio_format")]
    pub audio_format: String,
}

fn default_locale() -> String {
    "en-US".to_string()
}
fn default_speed() -> f32 {
    1.0
}
fn default_stt_model() -> String {
    "base".to_string()
}
fn default_audio_format() -> String {
    "webm".to_string()
}

pub fn spawn_tts_runner(
    net: Arc<Libp2pNetwork>,
    store: ChunkStore,
    cache_root: PathBuf,
    load: WorkerLoad,
) -> mpsc::Sender<AcceptedJob> {
    let (tx, mut rx) = mpsc::channel::<AcceptedJob>(8);
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let Ok(_compute_slot) = load.compute_slots().acquire_owned().await else {
                return;
            };
            let started = Instant::now();
            let job_id = job.accept.job_id.clone();
            let result = crate::job_control::cancellable(&job.accept.job_id, run_tts_job(&net, &store, &cache_root, &job)).await;
            cleanup_job_dir(&cache_root, &job_id).await;
            match result {
                Ok(hash) => {
                    load.record_job(started.elapsed());
                    info!(%job_id, %hash, "tts runner: job complete");
                    publish_receipt(&net, &job.accept, Some(hash), JobStatus::Completed).await;
                }
                Err(error) => {
                    warn!(%job_id, err = %error, "tts runner: job failed");
                    publish_receipt(&net, &job.accept, None, JobStatus::Failed).await;
                }
            }
        }
    });
    tx
}

pub fn spawn_stt_runner(
    net: Arc<Libp2pNetwork>,
    store: ChunkStore,
    cache_root: PathBuf,
    load: WorkerLoad,
) -> mpsc::Sender<AcceptedJob> {
    let (tx, mut rx) = mpsc::channel::<AcceptedJob>(8);
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let Ok(_compute_slot) = load.compute_slots().acquire_owned().await else {
                return;
            };
            let started = Instant::now();
            let job_id = job.accept.job_id.clone();
            let result = crate::job_control::cancellable(&job.accept.job_id, run_stt_job(&net, &store, &cache_root, &job)).await;
            cleanup_job_dir(&cache_root, &job_id).await;
            match result {
                Ok(hash) => {
                    load.record_job(started.elapsed());
                    info!(%job_id, %hash, "stt runner: job complete");
                    publish_receipt(&net, &job.accept, Some(hash), JobStatus::Completed).await;
                }
                Err(error) => {
                    warn!(%job_id, err = %error, "stt runner: job failed");
                    publish_receipt(&net, &job.accept, None, JobStatus::Failed).await;
                }
            }
        }
    });
    tx
}

async fn run_tts_job(
    net: &Libp2pNetwork,
    store: &ChunkStore,
    cache_root: &std::path::Path,
    job: &AcceptedJob,
) -> Result<String> {
    let spec = job
        .accept
        .spec_inline
        .as_ref()
        .context("accept missing spec_inline")?;
    let inline: TtsJobInline = serde_json::from_value(spec.clone()).context("decode TTS job")?;
    if inline.text.trim().is_empty() || inline.text.chars().count() > 6000 {
        bail!("TTS text must contain 1 to 6000 characters");
    }
    if !(0.65..=1.5).contains(&inline.speed)
        || !crate::speech::tts_voice_available(&inline.engine, &inline.voice)
    {
        bail!("this worker does not have the requested TTS voice installed");
    }
    let job_dir = safe_job_dir(cache_root, &job.accept.job_id)?;
    fs::create_dir_all(&job_dir).await?;
    let output_path = job_dir.join("speech.wav");
    if inline.engine == "pocket" || inline.engine == "chatterbox" {
        // Model load dominates a one-shot job (Chatterbox ~10-30 s), so these
        // engines speak through a warm worker this node keeps between jobs.
        let spoken = warm_say(&inline).await?;
        fs::copy(&spoken, &output_path).await?;
        let _ = fs::remove_file(&spoken).await;
    } else {
        let request = serde_json::json!({
            "text": inline.text,
            "engine": inline.engine,
            "voice": inline.voice,
            "locale": inline.locale,
            "speed": inline.speed,
        });
        crate::speech::synthesize(&request, &output_path).await?;
    }
    let bytes = fs::read(&output_path).await?;
    if bytes.len() < 44
        || bytes.len() > 20 * 1024 * 1024
        || &bytes[..4] != b"RIFF"
        || &bytes[8..12] != b"WAVE"
    {
        bail!("TTS output is not a valid WAV under the 20 MiB result limit");
    }
    crate::pieces::publish_result_bytes(net, store, &job.accept, &bytes).await
}

/// One warm session per voice, held for the life of the worker (idle ones
/// are reaped by the store). Sessions on one engine share a single model.
async fn warm_say(inline: &TtsJobInline) -> Result<std::path::PathBuf> {
    use std::collections::HashMap;
    use std::sync::OnceLock;
    static STORE: OnceLock<crate::speech_session::SessionStore> = OnceLock::new();
    static IDS: OnceLock<tokio::sync::Mutex<HashMap<String, String>>> = OnceLock::new();
    let store = STORE.get_or_init(crate::speech_session::SessionStore::new);
    let ids = IDS.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()));
    for attempt in 0..2 {
        let id = {
            let mut ids = ids.lock().await;
            match ids.get(&inline.voice) {
                Some(id) => id.clone(),
                None => {
                    let opened = store.open(&inline.voice).await?;
                    ids.insert(inline.voice.clone(), opened.session.clone());
                    opened.session
                }
            }
        };
        match store
            .say(&id, &inline.text, &inline.locale, inline.speed, inline.exaggeration)
            .await
        {
            Ok(spoken) => return Ok(spoken.wav),
            // The store drops a broken session; forget it and open a new one once.
            Err(error) if attempt == 0 => {
                warn!(voice = %inline.voice, err = %error, "warm TTS session failed; reopening");
                ids.lock().await.remove(&inline.voice);
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("the loop returns on its second attempt")
}

/// Largest audio input a worker accepts.
pub const MAX_STT_AUDIO_BYTES: usize = 100 * 1024 * 1024;

async fn run_stt_job(
    net: &Arc<Libp2pNetwork>,
    store: &ChunkStore,
    cache_root: &std::path::Path,
    job: &AcceptedJob,
) -> Result<String> {
    let spec = job
        .accept
        .spec_inline
        .as_ref()
        .context("accept missing spec_inline")?;
    let inline: SttJobInline = serde_json::from_value(spec.clone()).context("decode STT job")?;
    if inline.model != "base" {
        bail!("this worker currently supports the multilingual base STT model");
    }
    if inline.language.as_ref().is_some_and(|lang| {
        lang.len() > 16 || !lang.chars().all(|c| c.is_ascii_alphabetic() || c == '-')
    }) {
        bail!("language must be a short BCP-47 tag");
    }
    if !["webm", "wav", "ogg", "m4a", "mp3"].contains(&inline.audio_format.as_str()) {
        bail!("audio format must be webm, wav, ogg, m4a, or mp3");
    }
    if !crate::speech::stt_ready() {
        bail!("faster-whisper is not installed; run `c0mpute stt setup`");
    }
    let key = crate::pieces::input_key(inline.audio_key.as_deref())?;
    let input = crate::pieces::InputRef::parse(
        "audio",
        &inline.audio_hash,
        inline.audio_size,
        inline.audio_pieces.as_deref(),
        crate::pieces::input_limit(MAX_STT_AUDIO_BYTES, key.is_some()),
    )
    .map_err(anyhow::Error::msg)?;
    let provider = inline
        .audio_provider_peer_id
        .as_deref()
        .map(str::parse)
        .transpose()
        .context("invalid audio provider peer id")?;
    let audio = input
        .fetch_opened(net, provider, MAX_STT_AUDIO_BYTES, key.as_ref())
        .await
        .context("fetch the audio input (maximum 100 MiB)")?;
    let job_dir = safe_job_dir(cache_root, &job.accept.job_id)?;
    fs::create_dir_all(&job_dir).await?;
    let audio_path = job_dir.join(format!("input-audio.{}", inline.audio_format));
    let output_path = job_dir.join("transcript.json");
    fs::write(&audio_path, &audio).await?;
    let request = serde_json::json!({ "model": inline.model, "language": inline.language });
    crate::speech::transcribe(&request, &audio_path, &output_path).await?;
    let bytes = fs::read(&output_path).await?;
    if bytes.is_empty() || bytes.len() > 10 * 1024 * 1024 {
        bail!("STT result is empty or exceeds the 10 MiB result limit");
    }
    crate::pieces::publish_result_bytes(net, store, &job.accept, &bytes).await
}

pub(crate) fn safe_job_dir(cache_root: &std::path::Path, job_id: &str) -> Result<PathBuf> {
    job_subdir(&cache_root.join("jobs"), job_id)
}

/// `parent/<job id>`, for a job id that is a UUID. The id is the buyer's
/// word: joined raw, an absolute one replaces the whole path and `..` climbs
/// out of it, and runners remove their job directory when they finish.
pub(crate) fn job_subdir(parent: &std::path::Path, job_id: &str) -> Result<PathBuf> {
    let uuid = uuid::Uuid::parse_str(job_id).context("job id must be a UUID")?;
    Ok(parent.join(uuid.to_string()))
}

/// Whether `job_id` is what every buyer mints: a UUID in its canonical
/// (lowercase, hyphenated) form. Checked once where offers and accepts come
/// in, so no runner ever sees anything else.
pub(crate) fn valid_job_id(job_id: &str) -> bool {
    uuid::Uuid::parse_str(job_id).is_ok_and(|u| u.to_string() == job_id)
}

/// Spawn the transcode runner. Returns the sender; clone + give to the
/// dispatch loop so it can deliver `AcceptedJob`s.
pub fn spawn_transcode_runner(
    net: Arc<Libp2pNetwork>,
    store: ChunkStore,
    cache_root: PathBuf,
    ffmpeg_bin: PathBuf,
    load: WorkerLoad,
) -> mpsc::Sender<AcceptedJob> {
    let (tx, mut rx) = mpsc::channel::<AcceptedJob>(16);
    tokio::spawn(async move {
        let caps_result = c0mpute_transcode::probe_capabilities(&ffmpeg_bin).await;
        let caps = match caps_result {
            Ok(c) => c,
            Err(e) => {
                warn!(err = %e, "transcode runner: ffmpeg probe failed; runner exiting");
                return;
            }
        };
        info!(?caps.encoders, "transcode runner ready");
        while let Some(job) = rx.recv().await {
            let Ok(_compute_slot) = load.compute_slots().acquire_owned().await else {
                return;
            };
            let started = Instant::now();
            let job_id = job.accept.job_id.clone();
            let buyer_peer_id = job.accept.buyer_peer_id.clone();
            let result = crate::job_control::cancellable(
                &job_id,
                run_transcode_job(&net, &store, &cache_root, &ffmpeg_bin, &caps, &job),
            )
            .await;
            // The output now lives in the chunk store; the input, the
            // ffmpeg output file and the directory go, success or not.
            cleanup_job_dir(&cache_root, &job_id).await;
            match result {
                Ok(output_hash) => {
                    load.record_job(started.elapsed());
                    info!(
                        %job_id,
                        hash = %output_hash,
                        "transcode runner: job complete, output stored and announced"
                    );
                    publish_receipt(&net, &job.accept, Some(output_hash), JobStatus::Completed)
                        .await;
                }
                Err(e) => {
                    warn!(%job_id, %buyer_peer_id, err = %e, "transcode runner: job failed");
                    publish_receipt(&net, &job.accept, None, JobStatus::Failed).await;
                }
            }
        }
    });
    tx
}

async fn run_transcode_job(
    net: &Libp2pNetwork,
    store: &ChunkStore,
    cache_root: &std::path::Path,
    ffmpeg_bin: &std::path::Path,
    caps: &c0mpute_transcode::Capabilities,
    job: &AcceptedJob,
) -> Result<String> {
    let spec_inline = job
        .accept
        .spec_inline
        .as_ref()
        .context("accept missing spec_inline (Phase 1 requires inline)")?;
    let inline: TranscodeJobInline = serde_json::from_value(spec_inline.clone())
        .context("decode spec_inline as TranscodeJobInline")?;
    validate_transcode_spec(&inline.spec)?;

    let job_dir = safe_job_dir(cache_root, &job.accept.job_id)?;
    fs::create_dir_all(&job_dir).await?;
    let input_path = job_dir.join("input.bin");
    let output_path = job_dir.join("output.mp4");

    download_to_file(&inline.input_url, &input_path).await?;
    let input_bytes = fs::metadata(&input_path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    info!(bytes = input_bytes, path = %input_path.display(), "input downloaded");

    let budget = Duration::from_secs(15 * 60);
    let cuda_devices = crate::resources::cuda_devices_per_job();
    c0mpute_transcode::transcode_with_limits(
        ffmpeg_bin,
        &input_path,
        &output_path,
        &inline.spec,
        caps,
        budget,
        crate::resources::cpu_threads_per_job(),
        cuda_devices.as_deref(),
    )
    .await
    .context("ffmpeg")?;

    // The input is no longer needed once the output exists.
    let _ = fs::remove_file(&input_path).await;
    // Streamed from disk as pieces (a manifest is what the receipt names);
    // the output is never in memory whole.
    crate::pieces::publish_result_file(net, store, &job.accept, &output_path, MAX_OUTPUT_BYTES)
        .await
        .context("publish transcode output")
}

/// Largest transcode output a worker stores and serves. Over the cap the
/// job fails with a clear error rather than filling the disk.
pub const MAX_OUTPUT_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Largest input a transcode job may download.
const MAX_INPUT_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Whole-download budget, separate from the ffmpeg budget.
const INPUT_FETCH_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// A transcode spec arrives from an untrusted peer. Bound every number
/// and refuse raw ffmpeg arguments: appended flags could add inputs or
/// outputs (`-i /etc/passwd`, a second output path) and read or write
/// the operator's files.
pub(crate) fn validate_transcode_spec(spec: &TranscodeSpec) -> Result<()> {
    if !spec.extra_ffmpeg_args.is_empty() {
        bail!("extra_ffmpeg_args are not accepted from the network");
    }
    if !(16..=7680).contains(&spec.width) || !(16..=4320).contains(&spec.height) {
        bail!("resolution {}x{} is out of range", spec.width, spec.height);
    }
    if !(32_000..=200_000_000).contains(&spec.bitrate_bps) {
        bail!("bitrate {} is out of range", spec.bitrate_bps);
    }
    if !(1..=1000).contains(&spec.keyframe_interval) {
        bail!(
            "keyframe interval {} is out of range",
            spec.keyframe_interval
        );
    }
    Ok(())
}

async fn download_to_file(url: &str, path: &std::path::Path) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    // Resolve once, require every address to be public, then pin the
    // request to those addresses so a second lookup cannot rebind to a
    // private one. Redirects are off: each hop would need the same check.
    let target = crate::net_guard::vet_url(url, crate::net_guard::is_public_ip)
        .await
        .context("input_url")?;
    let mut builder = reqwest::Client::builder();
    if let Some(host) = &target.pin_host {
        builder = builder.resolve_to_addrs(host, &target.addrs);
    }
    let client = builder
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .timeout(INPUT_FETCH_TIMEOUT)
        .build()
        .context("build input http client")?;

    let mut resp = client.get(url).send().await.context("GET input_url")?;
    if !resp.status().is_success() {
        bail!("input fetch returned {}", resp.status());
    }
    if resp.content_length().is_some_and(|n| n > MAX_INPUT_BYTES) {
        bail!("input exceeds the {MAX_INPUT_BYTES}-byte limit");
    }
    let mut file = fs::File::create(path).await?;
    let mut written: u64 = 0;
    while let Some(chunk) = resp.chunk().await.context("read input_url body")? {
        written += chunk.len() as u64;
        if written > MAX_INPUT_BYTES {
            drop(file);
            let _ = fs::remove_file(path).await;
            bail!("input exceeds the {MAX_INPUT_BYTES}-byte limit");
        }
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    Ok(())
}

/// Syntactic SSRF check (scheme, `localhost`, private IP literals) for
/// callers that resolve and pin addresses themselves (`webhook.deliver`).
/// [`crate::net_guard::vet_url`] does the full resolve-and-vet.
pub(crate) fn validate_url(url: &str) -> Result<()> {
    let parsed = url::Url::parse(url).context("invalid URL")?;
    match parsed.scheme() {
        "http" | "https" => {}
        scheme => bail!("disallowed URL scheme: {}", scheme),
    }
    match parsed.host().context("URL missing host")? {
        url::Host::Domain(host) => {
            let host = host.to_ascii_lowercase();
            let host = host.trim_end_matches('.');
            if host == "localhost" || host.ends_with(".localhost") {
                bail!("localhost URLs are not allowed");
            }
        }
        url::Host::Ipv4(ip) => {
            if !is_public_ip(ip.into()) {
                bail!("private or internal IP addresses are not allowed");
            }
        }
        url::Host::Ipv6(ip) => {
            if !is_public_ip(ip.into()) {
                bail!("private or internal IP addresses are not allowed");
            }
        }
    }
    Ok(())
}

/// Publicly routable? See [`crate::net_guard::is_public_ip`].
pub(crate) fn is_public_ip(ip: std::net::IpAddr) -> bool {
    crate::net_guard::is_public_ip(ip)
}

/// Before telling a buyer its result is ready, open a connection to the
/// buyer. A worker behind NAT cannot be dialed, but it can dial out; the
/// buyer's fetch of the result then uses this connection instead of failing
/// with "Failed to dial the requested peer". Best effort, bounded to 5 s.
async fn reach_buyer(net: &Libp2pNetwork, buyer_peer_id: &str) {
    let Ok(buyer) = buyer_peer_id.parse::<c0mpute_net::PeerId>() else {
        return;
    };
    if net.is_connected(buyer).await {
        return;
    }
    if let Err(e) = net.connect(buyer).await {
        tracing::debug!(%buyer, err = %e, "could not dial buyer before receipt");
        return;
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if net.is_connected(buyer).await {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

pub async fn publish_receipt(
    net: &Libp2pNetwork,
    accept: &JobAccept,
    output_hash: Option<String>,
    status: JobStatus,
) {
    crate::activity::job_finished(&accept.job_id);
    crate::activity::note_outcome(&accept.workload_type, status == JobStatus::Completed);
    // Its buyer cancelled it: nobody is waiting for a receipt.
    if crate::job_control::cancelled(&accept.job_id) {
        return;
    }
    let receipt = receipt_for(net, accept, output_hash, status, None);
    // Kept for a buyer that restarts before it hears this (`job_control`).
    crate::job_control::done(&accept.job_id, &receipt);
    // Reach back to the buyer first: it can fetch the result only over a
    // connection, and it usually cannot dial our random, firewalled port.
    if receipt.output_hash.is_some() {
        let addrs =
            crate::reachability::dialable_buyer_addrs(&accept.buyer_addrs, &accept.buyer_peer_id);
        if !addrs.is_empty() {
            for addr in addrs {
                if let Err(e) = net.dial(addr).await {
                    tracing::debug!(err = %e, "dial back to buyer failed");
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        } else {
            // The buyer advertised no address: dial it by peer id, using
            // whatever addresses Kad and identify have learned for it.
            reach_buyer(net, &accept.buyer_peer_id).await;
        }
    }
    send_receipt(net, accept, &receipt).await;
}

fn receipt_for(
    net: &Libp2pNetwork,
    accept: &JobAccept,
    output_hash: Option<String>,
    status: JobStatus,
    fault: Option<&str>,
) -> JobReceipt {
    JobReceipt {
        job_id: accept.job_id.clone(),
        worker_peer_id: net.peer_id().to_base58(),
        worker_did: None,
        buyer_peer_id: accept.buyer_peer_id.clone(),
        output_hash,
        status,
        completed_at_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        signature: None,
        external_ref: accept.external_ref.clone(),
        instance: Some(net.instance_id().to_string()),
        fault: fault.map(str::to_string),
    }
}

/// Tell the buyer that `accept` went to another process running this
/// identity (a twin), not to this one: a Failed receipt marked
/// [`c0mpute_net::topics::FAULT_IDENTITY_TWIN`]. Buyers that know the mark
/// re-award the job and do not count it against the node; this process's
/// own failure backoff is not touched either.
pub async fn publish_twin_fault(net: &Libp2pNetwork, accept: &JobAccept) {
    crate::activity::job_finished(&accept.job_id);
    crate::job_control::forget(&accept.job_id);
    let receipt = receipt_for(
        net,
        accept,
        None,
        JobStatus::Failed,
        Some(c0mpute_net::topics::FAULT_IDENTITY_TWIN),
    );
    send_receipt(net, accept, &receipt).await;
}

async fn send_receipt(net: &Libp2pNetwork, accept: &JobAccept, receipt: &JobReceipt) {
    let topic = job_topic(&accept.workload_type);
    if let Ok(payload) = serde_json::to_vec(receipt) {
        // Straight to the buyer as well as on the topic (the aggregator and
        // old buyers read it there): a receipt lost on the mesh left the
        // buyer waiting out its whole receipt timeout for a finished job.
        let direct = async {
            match accept.buyer_peer_id.parse::<c0mpute_net::PeerId>() {
                Ok(buyer) => {
                    net.send_direct(buyer, &topic, &payload, std::time::Duration::from_secs(10))
                        .await
                }
                Err(e) => Err(anyhow::anyhow!("buyer peer id: {e}")),
            }
        };
        let (direct, gossip) = tokio::join!(direct, net.publish(&topic, payload.clone()));
        match (direct, gossip) {
            (Ok(()), _) | (_, Ok(())) => {}
            (Err(d), Err(g)) => {
                warn!(job_id = %accept.job_id, direct_err = %d, gossip_err = %g, "receipt not delivered on either path")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use c0mpute_proto::Codec;

    fn spec() -> TranscodeSpec {
        TranscodeSpec {
            codec: Codec::H264,
            bitrate_bps: 2_000_000,
            width: 1280,
            height: 720,
            keyframe_interval: 60,
            hardware_pref: None,
            extra_ffmpeg_args: vec![],
        }
    }

    #[test]
    fn normal_spec_passes() {
        assert!(validate_transcode_spec(&spec()).is_ok());
    }

    #[test]
    fn extra_ffmpeg_args_are_refused() {
        let mut s = spec();
        s.extra_ffmpeg_args = vec!["-i".into(), "/etc/passwd".into()];
        assert!(validate_transcode_spec(&s).is_err());
    }

    #[test]
    fn absurd_dimensions_are_refused() {
        let mut s = spec();
        s.width = 100_000;
        assert!(validate_transcode_spec(&s).is_err());
    }

    #[test]
    fn job_dir_rejects_traversal() {
        let root = std::path::Path::new("/cache");
        assert!(safe_job_dir(root, "../../home/op/.ssh").is_err());
        assert!(safe_job_dir(root, "6f1c3e2a-8a39-4a0e-9a59-2c3b1f0e7d11").is_ok());
        // The text and temporal runners keep their own layout under the same rule.
        let text = root.join("text").join("jobs");
        assert!(job_subdir(&text, "/home/op").is_err());
        assert!(job_subdir(&text, "../../../..").is_err());
        assert_eq!(
            job_subdir(&text, "6f1c3e2a-8a39-4a0e-9a59-2c3b1f0e7d11").unwrap(),
            text.join("6f1c3e2a-8a39-4a0e-9a59-2c3b1f0e7d11")
        );
        // Offers and accepts carry the canonical form only.
        assert!(valid_job_id("6f1c3e2a-8a39-4a0e-9a59-2c3b1f0e7d11"));
        assert!(valid_job_id(&uuid::Uuid::new_v4().to_string()));
        for bad in ["", "j", "/home/op", "../x", "6F1C3E2A-8A39-4A0E-9A59-2C3B1F0E7D11", "6f1c3e2a8a394a0e9a592c3b1f0e7d11", "{6f1c3e2a-8a39-4a0e-9a59-2c3b1f0e7d11}"] {
            assert!(!valid_job_id(bad), "{bad}");
        }
    }

    #[test]
    fn non_public_ips_are_rejected() {
        for ip in [
            "0.1.2.3",
            "224.0.0.1",
            "240.0.0.1",
            "127.0.0.1",
            "10.0.0.1",
            "169.254.169.254",
        ] {
            assert!(!crate::net_guard::is_public_ip(ip.parse().unwrap()), "{ip}");
        }
        assert!(crate::net_guard::is_public_ip("1.1.1.1".parse().unwrap()));
    }

    fn scratch(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("c0-runner-{name}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn job_dir_is_removed_after_the_job() {
        let root = scratch("cleanup");
        let id = "6f1c3e2a-8a39-4a0e-9a59-2c3b1f0e7d11";
        let dir = safe_job_dir(&root, id).unwrap();
        fs::create_dir_all(&dir).await.unwrap();
        fs::write(dir.join("input.bin"), b"x").await.unwrap();
        cleanup_job_dir(&root, id).await;
        assert!(!dir.exists());
        // Idempotent, and a bad id is a no-op rather than a traversal.
        cleanup_job_dir(&root, id).await;
        cleanup_job_dir(&root, "../../etc").await;
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn job_times_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("job-times").join("diffusion.txt2img.ms");
        let load = WorkerLoad::new(Arc::new(Semaphore::new(1)), 1).with_history(path.clone());
        assert_eq!(load.recent_job_duration(), None);
        load.record_job(Duration::from_secs(600));
        // A new process (an update re-exec) starts from what was measured.
        let again = WorkerLoad::new(Arc::new(Semaphore::new(1)), 1).with_history(path);
        assert_eq!(again.recent_job_duration(), Some(Duration::from_secs(600)));
    }

    #[test]
    fn worker_load_tracks_recent_job_time() {
        let load = WorkerLoad::new(Arc::new(Semaphore::new(2)), 2);
        assert_eq!(load.recent_job_duration(), None);
        load.record_job(Duration::from_secs(10));
        assert_eq!(load.recent_job_duration(), Some(Duration::from_secs(10)));
        load.record_job(Duration::from_secs(20));
        assert_eq!(load.recent_job_duration(), Some(Duration::from_secs(13)));
        assert_eq!(load.free_slots(), 2);
        let _held = load.compute_slots().try_acquire_owned().unwrap();
        assert_eq!(load.free_slots(), 1);
    }

    #[tokio::test]
    async fn hostname_resolving_to_loopback_is_refused() {
        let dir = std::env::temp_dir().join(format!("c0-dl-{}", std::process::id()));
        let err = download_to_file("http://localtest.me:9/x", &dir)
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("non-public") || msg.contains("resolve"),
            "{msg}"
        );
    }
}
