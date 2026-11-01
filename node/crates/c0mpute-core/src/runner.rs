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
use std::time::Duration;

use anyhow::{Context, Result, bail};
use c0mpute_net::Libp2pNetwork;
use c0mpute_net::{
    Network,
    topics::{JobAccept, JobReceipt, JobStatus, job_topic},
};
use c0mpute_proto::{ChunkRequest, Hash, TranscodeSpec};
use c0mpute_store::ChunkStore;
use tokio::fs;
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Job handed off from the dispatch loop to a per-workload runner.
#[derive(Clone, Debug)]
pub struct AcceptedJob {
    pub accept: JobAccept,
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
}

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug)]
pub struct SttJobInline {
    /// Content-addressed audio chunk already announced by the buyer.
    pub audio_hash: String,
    /// Peer that owns the input audio, supplied by the buyer.
    #[serde(default)]
    pub audio_provider_peer_id: Option<String>,
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
) -> mpsc::Sender<AcceptedJob> {
    let (tx, mut rx) = mpsc::channel::<AcceptedJob>(8);
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let job_id = job.accept.job_id.clone();
            match run_tts_job(&net, &store, &cache_root, &job).await {
                Ok(hash) => {
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
) -> mpsc::Sender<AcceptedJob> {
    let (tx, mut rx) = mpsc::channel::<AcceptedJob>(8);
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let job_id = job.accept.job_id.clone();
            match run_stt_job(&net, &store, &cache_root, &job).await {
                Ok(hash) => {
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
    let request = serde_json::json!({
        "text": inline.text,
        "engine": inline.engine,
        "voice": inline.voice,
        "locale": inline.locale,
        "speed": inline.speed,
    });
    crate::speech::synthesize(&request, &output_path).await?;
    let bytes = fs::read(&output_path).await?;
    if bytes.len() < 44
        || bytes.len() > 20 * 1024 * 1024
        || &bytes[..4] != b"RIFF"
        || &bytes[8..12] != b"WAVE"
    {
        bail!("TTS output is not a valid WAV under the 20 MiB result limit");
    }
    let hash = store.put(&bytes).await?;
    net.announce(&hash).await?;
    Ok(hash.to_hex())
}

async fn run_stt_job(
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
    let input_hash = Hash::from_hex(&inline.audio_hash).context("invalid audio hash")?;
    let input_request = ChunkRequest {
        chunk_hash: input_hash,
        shard_index: None,
    };
    let audio = if let Some(provider) = inline.audio_provider_peer_id.as_deref() {
        let provider = provider.parse().context("invalid audio provider peer id")?;
        net.fetch_from_peer(provider, &input_request).await?
    } else {
        net.fetch(&input_request).await?
    };
    if Hash::of(&audio) != input_hash || audio.is_empty() || audio.len() > 100 * 1024 * 1024 {
        bail!("audio input hash or size is invalid (maximum 100 MiB)");
    }
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
    let hash = store.put(&bytes).await?;
    net.announce(&hash).await?;
    Ok(hash.to_hex())
}

fn safe_job_dir(cache_root: &std::path::Path, job_id: &str) -> Result<PathBuf> {
    let uuid = uuid::Uuid::parse_str(job_id).context("job id must be a UUID")?;
    Ok(cache_root.join("jobs").join(uuid.to_string()))
}

/// Spawn the transcode runner. Returns the sender; clone + give to the
/// dispatch loop so it can deliver `AcceptedJob`s.
pub fn spawn_transcode_runner(
    net: Arc<Libp2pNetwork>,
    cache_root: PathBuf,
    ffmpeg_bin: PathBuf,
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
            let job_id = job.accept.job_id.clone();
            let buyer_peer_id = job.accept.buyer_peer_id.clone();
            match run_transcode_job(&net, &cache_root, &ffmpeg_bin, &caps, &job).await {
                Ok((output_path, output_hash)) => {
                    info!(
                        %job_id,
                        output = %output_path.display(),
                        hash = %output_hash,
                        "transcode runner: job complete"
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
    _net: &Libp2pNetwork,
    cache_root: &std::path::Path,
    ffmpeg_bin: &std::path::Path,
    caps: &c0mpute_transcode::Capabilities,
    job: &AcceptedJob,
) -> Result<(PathBuf, String)> {
    let spec_inline = job
        .accept
        .spec_inline
        .as_ref()
        .context("accept missing spec_inline (Phase 1 requires inline)")?;
    let inline: TranscodeJobInline = serde_json::from_value(spec_inline.clone())
        .context("decode spec_inline as TranscodeJobInline")?;

    let job_dir = cache_root.join("jobs").join(&job.accept.job_id);
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
    c0mpute_transcode::transcode(
        ffmpeg_bin,
        &input_path,
        &output_path,
        &inline.spec,
        caps,
        budget,
    )
    .await
    .context("ffmpeg")?;

    let output_bytes = fs::read(&output_path).await?;
    let hash = blake3::hash(&output_bytes);
    Ok((output_path, hex::encode(hash.as_bytes())))
}

async fn download_to_file(url: &str, path: &std::path::Path) -> Result<()> {
    validate_url(url)?;
    let resp = reqwest::get(url).await.context("GET input_url")?;
    if !resp.status().is_success() {
        bail!("input fetch returned {}", resp.status());
    }
    let bytes = resp.bytes().await?;
    fs::write(path, &bytes).await?;
    Ok(())
}

/// Validate URL to prevent SSRF attacks.
/// Only allows http/https schemes and rejects private/internal addresses.
fn validate_url(url: &str) -> Result<()> {
    let parsed = url::Url::parse(url).context("invalid URL")?;

    // Only allow http and https schemes
    match parsed.scheme() {
        "http" | "https" => {}
        scheme => bail!("disallowed URL scheme: {}", scheme),
    }

    let host = parsed.host_str().context("URL missing host")?;

    // Reject localhost variants
    if host == "localhost" || host.ends_with(".localhost") {
        bail!("localhost URLs are not allowed");
    }

    // Parse as IP and reject private/internal ranges
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if !is_public_ip(ip) {
            bail!("private or internal IP addresses are not allowed");
        }
    }

    Ok(())
}

/// Check if an IP address is publicly routable (not private, loopback, link-local, etc.)
fn is_public_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ipv4) => {
            !ipv4.is_loopback()           // 127.0.0.0/8
                && !ipv4.is_private()     // 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16
                && !ipv4.is_link_local()  // 169.254.0.0/16 (includes cloud metadata)
                && !ipv4.is_broadcast()   // 255.255.255.255
                && !ipv4.is_unspecified() // 0.0.0.0
                && !ipv4.is_documentation() // 192.0.2.0/24, 198.51.100.0/24, 203.0.113.0/24
                && !is_shared_nat(ipv4) // 100.64.0.0/10 (Carrier-grade NAT)
        }
        std::net::IpAddr::V6(ipv6) => {
            !ipv6.is_loopback()           // ::1
                && !ipv6.is_unspecified() // ::
                && !is_ipv6_private_or_local(&ipv6)
        }
    }
}

fn is_shared_nat(ip: std::net::Ipv4Addr) -> bool {
    // 100.64.0.0/10 - Shared address space (RFC 6598)
    let octets = ip.octets();
    octets[0] == 100 && (octets[1] & 0xC0) == 64
}

fn is_ipv6_private_or_local(ip: &std::net::Ipv6Addr) -> bool {
    let segments = ip.segments();
    // fc00::/7 - Unique local addresses
    (segments[0] & 0xfe00) == 0xfc00
        // fe80::/10 - Link-local addresses
        || (segments[0] & 0xffc0) == 0xfe80
        // ::ffff:0:0/96 - IPv4-mapped addresses (check the mapped IPv4)
        || (segments[0..5] == [0, 0, 0, 0, 0] && segments[5] == 0xffff)
}

pub(crate) async fn publish_receipt(
    net: &Libp2pNetwork,
    accept: &JobAccept,
    output_hash: Option<String>,
    status: JobStatus,
) {
    let receipt = JobReceipt {
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
    };
    let topic = job_topic(&accept.workload_type);
    if let Ok(payload) = serde_json::to_vec(&receipt) {
        if let Err(e) = net.publish(&topic, payload).await {
            warn!(err = %e, "publish receipt");
        }
    }
}
