//! `blender.render-frame`: render frames of a `.blend` scene on workers'
//! CPUs (and GPUs where Blender finds one), SheepIt-style.
//!
//! The engine is the operator's own Blender (`blender` on PATH, from the OS
//! package manager or blender.org). Nothing is installed by the node: a node
//! advertises `c0mpute:role:render` only when `blender --version` answers,
//! and probes once, at start, which Cycles GPU backends (CUDA, OptiX, HIP,
//! Metal, oneAPI) Blender can see.
//!
//! The `.blend` travels like an `ocr.extract` input, by hash, except that it
//! can be far bigger than one network message: the buyer splits it into
//! [`PIECE_BYTES`] pieces, stores and announces each, and the spec names the
//! whole-file hash plus the piece hashes. A worker fetches the pieces once,
//! checks every piece and the whole file, and keeps the file in a cache keyed
//! by its hash, so the other frames of the same job it wins start at once.
//!
//! Each rendered frame goes back the same way: the result JSON lists every
//! frame with its size, dimensions, whole hash and piece hashes, and the
//! buyer fetches and checks them ([`fetch_frames`]).
//!
//! Safety: Blender always runs with `--disable-autoexec` before the file is
//! loaded, so Python embedded in the `.blend` (drivers, registered text
//! blocks, app handlers) never runs. The only Python is the one-line settings
//! expression this module builds from validated integers, and the constant
//! [`SANITIZE_EXPR`], which cuts every file reference that leaves the job
//! directory before the render starts. Where bubblewrap works (Linux),
//! Blender also runs inside it, with no network and nothing of the host but
//! the system directories and the job directory ([`Sandbox`]).

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use c0mpute_net::topics::{JobOffer, JobStatus};
use c0mpute_net::Libp2pNetwork;
use c0mpute_proto::Hash;
use tokio::io::AsyncReadExt;
use c0mpute_store::ChunkStore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::{Semaphore, mpsc};
use tracing::{info, warn};

use crate::runner::{AcceptedJob, WorkerLoad, cleanup_job_dir, publish_receipt, safe_job_dir};

pub const WORKLOAD: &str = "blender.render-frame";
/// Capability tag: this node has a working `blender`.
pub const ROLE_TAG: &str = "c0mpute:role:render";
/// Any GPU backend Cycles can use was found.
pub const GPU_TAG: &str = "c0mpute:render:gpu";
/// Largest `.blend` a buyer may send.
pub const MAX_INPUT_BYTES: u64 = 512 * 1024 * 1024;
/// Input and output files move in pieces of this size (one network message
/// carries at most 10 MiB; see [`crate::pieces`]).
pub const PIECE_BYTES: usize = 4 * 1024 * 1024;
/// Most pieces a `.blend` may have (`MAX_INPUT_BYTES / PIECE_BYTES`).
pub const MAX_PIECES: usize = (MAX_INPUT_BYTES as usize).div_ceil(PIECE_BYTES);
/// Most frames one job renders (a small contiguous batch).
pub const MAX_FRAMES_PER_JOB: u32 = 10;
/// Blender's highest frame number.
pub const MAX_FRAME: u32 = 1_048_574;
/// Largest single rendered frame a worker returns.
pub const MAX_FRAME_BYTES: u64 = 512 * 1024 * 1024;
/// Cap on the result JSON (frame metadata only; the images are separate).
pub const MAX_RESULT_BYTES: usize = 1024 * 1024;
/// Per-frame render timeout when the spec names none, and the most it may ask.
pub const DEFAULT_FRAME_TIMEOUT_SECS: u64 = 1800;
pub const MAX_FRAME_TIMEOUT_SECS: u64 = 3 * 3600;
/// Upper bounds on the optional render settings.
pub const MAX_RESOLUTION_PCT: u32 = 200;
pub const MAX_SAMPLES: u32 = 16_384;
/// How much `.blend` cache a worker keeps (oldest files go first).
const CACHE_MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_secs(90);

// ─── config ──────────────────────────────────────────────────────────────

/// `[render]` in config.toml.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RenderConfig {
    /// `"auto"` (the default): run Blender inside bubblewrap where it works,
    /// otherwise with scene sanitisation only. `"off"`: never use
    /// bubblewrap. `"require"`: serve no render that cannot be sandboxed.
    /// `C0MPUTE_RENDER_SANDBOX` wins over the file.
    pub sandbox: String,
}

impl Default for RenderConfig {
    fn default() -> Self {
        Self { sandbox: "auto".into() }
    }
}

impl RenderConfig {
    /// The mode `sandbox` (or `env`, when set) names. A value that is not
    /// one of the three is read as `auto`, so a typo never stops a node.
    pub fn mode_with(&self, env: Option<String>) -> SandboxMode {
        let value = env.filter(|v| !v.trim().is_empty()).unwrap_or_else(|| self.sandbox.clone());
        SandboxMode::parse(&value).unwrap_or_else(|| {
            warn!(value = %value, "render: sandbox must be auto, off or require; using auto");
            SandboxMode::Auto
        })
    }
}

/// Whether Blender runs inside bubblewrap (`[render] sandbox`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxMode {
    #[default]
    Auto,
    Off,
    Require,
}

impl SandboxMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "off" => Some(Self::Off),
            "require" => Some(Self::Require),
            _ => None,
        }
    }
}

/// The node's `[render] sandbox`, from its config file and the environment.
/// The render role is started without a config handle, so this reads the
/// file itself, as the speech gateway does for `[adult]`.
fn sandbox_mode() -> SandboxMode {
    let file = crate::config::Config::load_raw(&crate::config::default_config_path()).unwrap_or_default();
    file.render.mode_with(std::env::var("C0MPUTE_RENDER_SANDBOX").ok())
}

// ─── spec ────────────────────────────────────────────────────────────────

/// Render engine. `EEVEE` is mapped to the identifier the worker's Blender
/// uses (`BLENDER_EEVEE_NEXT` on 4.2 to 4.x, `BLENDER_EEVEE` otherwise).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Cycles,
    Eevee,
    Workbench,
}

impl Engine {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_uppercase().as_str() {
            "CYCLES" => Some(Self::Cycles),
            "EEVEE" | "BLENDER_EEVEE" | "BLENDER_EEVEE_NEXT" => Some(Self::Eevee),
            "WORKBENCH" | "BLENDER_WORKBENCH" => Some(Self::Workbench),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Cycles => "CYCLES",
            Self::Eevee => "EEVEE",
            Self::Workbench => "WORKBENCH",
        }
    }
    /// The `--engine` value for a Blender of this version.
    pub fn blender_id(self, version: (u32, u32, u32)) -> &'static str {
        match self {
            Self::Cycles => "CYCLES",
            Self::Workbench => "BLENDER_WORKBENCH",
            Self::Eevee if version.0 == 4 && version.1 >= 2 => "BLENDER_EEVEE_NEXT",
            Self::Eevee => "BLENDER_EEVEE",
        }
    }
    /// EEVEE and Workbench rasterize with OpenGL/Vulkan: only GPU boxes bid.
    pub fn needs_gpu(self) -> bool {
        !matches!(self, Self::Cycles)
    }
    pub fn tag(self) -> String {
        format!("c0mpute:render:engine:{}", self.name().to_ascii_lowercase())
    }
}

/// Output image format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Png,
    Jpeg,
    Exr,
}

impl Format {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_uppercase().as_str() {
            "PNG" => Some(Self::Png),
            "JPEG" | "JPG" => Some(Self::Jpeg),
            "EXR" | "OPEN_EXR" | "OPENEXR" => Some(Self::Exr),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
            Self::Exr => "EXR",
        }
    }
    /// The `--render-format` value.
    pub fn blender_id(self) -> &'static str {
        match self {
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
            Self::Exr => "OPEN_EXR",
        }
    }
    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Exr => "exr",
        }
    }
}

/// Where Cycles renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Device {
    /// The GPU when the worker has one, else the CPU.
    Auto,
    Cpu,
    /// Only workers with a GPU bid.
    Gpu,
}

impl Device {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_uppercase().as_str() {
            "AUTO" => Some(Self::Auto),
            "CPU" => Some(Self::Cpu),
            "GPU" => Some(Self::Gpu),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "AUTO",
            Self::Cpu => "CPU",
            Self::Gpu => "GPU",
        }
    }
}

/// What a buyer asks for, as it travels.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RenderSpec {
    pub v: u32,
    /// Hash of the whole `.blend`.
    pub input_hash: String,
    pub input_size: u64,
    /// Hashes of its [`PIECE_BYTES`] pieces, in order.
    pub input_pieces: Vec<String>,
    /// Peer that holds the pieces (normally the buyer). Without it the
    /// worker looks each piece up on the DHT.
    #[serde(default)]
    pub input_provider_peer_id: Option<String>,
    /// Hex key the stored `.blend` is sealed under (DIP-0032); rides only
    /// inside the sealed spec. Absent: the stored bytes are plaintext.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_key: Option<String>,
    pub frame_start: u32,
    pub frame_end: u32,
    #[serde(default = "default_engine")]
    pub engine: String,
    #[serde(default = "default_device")]
    pub device: String,
    #[serde(default = "default_format")]
    pub format: String,
    /// Scale the scene's resolution (1 to 200 percent).
    #[serde(default)]
    pub resolution_pct: Option<u32>,
    /// Override the scene's sample count (Cycles samples or EEVEE TAA samples).
    #[serde(default)]
    pub samples: Option<u32>,
    /// Hard limit per frame; the job's limit is this times its frame count.
    #[serde(default)]
    pub frame_timeout_secs: Option<u64>,
}

fn default_engine() -> String {
    "CYCLES".into()
}
fn default_device() -> String {
    "AUTO".into()
}
fn default_format() -> String {
    "PNG".into()
}

/// A checked spec.
#[derive(Debug, Clone)]
pub struct Parsed {
    pub spec: RenderSpec,
    pub input_hash: Hash,
    pub pieces: Vec<Hash>,
    pub engine: Engine,
    pub device: Device,
    pub format: Format,
}

impl Parsed {
    pub fn frames(&self) -> std::ops::RangeInclusive<u32> {
        self.spec.frame_start..=self.spec.frame_end
    }
    pub fn frame_count(&self) -> u32 {
        self.spec.frame_end - self.spec.frame_start + 1
    }
    pub fn frame_timeout(&self) -> Duration {
        Duration::from_secs(
            self.spec
                .frame_timeout_secs
                .unwrap_or(DEFAULT_FRAME_TIMEOUT_SECS),
        )
    }
}

pub fn parse_spec(spec: &Value) -> std::result::Result<Parsed, String> {
    let s: RenderSpec = serde_json::from_value(spec.clone())
        .map_err(|e| format!("invalid {WORKLOAD} spec: {e}"))?;
    if s.v != 1 {
        return Err(format!("unsupported {WORKLOAD} spec version {}", s.v));
    }
    let input_hash = Hash::from_hex(&s.input_hash)
        .map_err(|_| "input_hash must be a hex content hash".to_string())?;
    // A sealed .blend is a little bigger than the plaintext limit.
    let max_input = if s.input_key.is_some() { crate::job_e2e::sealed_len(MAX_INPUT_BYTES) } else { MAX_INPUT_BYTES };
    if s.input_size == 0 || s.input_size > max_input {
        return Err("input_size must be between 1 byte and 512 MiB".into());
    }
    if s.input_key.as_deref().is_some_and(|k| crate::job_e2e::key_from_hex(k).is_err()) {
        return Err("input_key must be 32 bytes of hex".into());
    }
    let expected = piece_count(s.input_size);
    if s.input_pieces.len() != expected {
        return Err(format!(
            "input_pieces must list {expected} pieces of {PIECE_BYTES} bytes for {} bytes",
            s.input_size
        ));
    }
    let pieces = s
        .input_pieces
        .iter()
        .map(|h| Hash::from_hex(h))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| "input_pieces must be hex content hashes".to_string())?;
    if let Some(peer) = s.input_provider_peer_id.as_deref() {
        peer.parse::<c0mpute_net::PeerId>()
            .map_err(|_| "input_provider_peer_id is not a peer id".to_string())?;
    }
    if s.frame_end < s.frame_start {
        return Err("frame_end must not be before frame_start".into());
    }
    if s.frame_end > MAX_FRAME {
        return Err(format!("frames must be between 0 and {MAX_FRAME}"));
    }
    if s.frame_end - s.frame_start + 1 > MAX_FRAMES_PER_JOB {
        return Err(format!(
            "one job renders at most {MAX_FRAMES_PER_JOB} frames"
        ));
    }
    let engine = Engine::parse(&s.engine).ok_or_else(|| {
        "engine must be CYCLES, EEVEE (BLENDER_EEVEE_NEXT) or WORKBENCH".to_string()
    })?;
    let device =
        Device::parse(&s.device).ok_or_else(|| "device must be AUTO, CPU or GPU".to_string())?;
    if device == Device::Cpu && engine.needs_gpu() {
        return Err(format!(
            "{} renders on a GPU; use CYCLES for CPU",
            engine.name()
        ));
    }
    let format =
        Format::parse(&s.format).ok_or_else(|| "format must be PNG, JPEG or EXR".to_string())?;
    if let Some(p) = s.resolution_pct
        && !(1..=MAX_RESOLUTION_PCT).contains(&p)
    {
        return Err(format!("resolution_pct must be 1 to {MAX_RESOLUTION_PCT}"));
    }
    if let Some(n) = s.samples
        && !(1..=MAX_SAMPLES).contains(&n)
    {
        return Err(format!("samples must be 1 to {MAX_SAMPLES}"));
    }
    if let Some(t) = s.frame_timeout_secs
        && !(10..=MAX_FRAME_TIMEOUT_SECS).contains(&t)
    {
        return Err(format!(
            "frame_timeout_secs must be 10 to {MAX_FRAME_TIMEOUT_SECS}"
        ));
    }
    let mut spec = s;
    spec.engine = engine.name().into();
    spec.device = device.name().into();
    spec.format = format.name().into();
    Ok(Parsed {
        spec,
        input_hash,
        pieces,
        engine,
        device,
        format,
    })
}

pub fn piece_count(size: u64) -> usize {
    crate::pieces::piece_count(size, PIECE_BYTES)
}

/// The tags a worker must advertise to win this job.
pub fn required_capabilities(parsed: &Parsed) -> Vec<String> {
    let mut tags = vec![ROLE_TAG.to_string(), parsed.engine.tag()];
    if parsed.device == Device::Gpu {
        tags.push(GPU_TAG.to_string());
    }
    tags
}

/// [`required_capabilities`] from a raw spec; just the role tag if it does
/// not parse (the spec check rejects it first anyway).
pub fn required_capabilities_for(spec: &Value) -> Vec<String> {
    parse_spec(spec)
        .map(|p| required_capabilities(&p))
        .unwrap_or_else(|_| vec![ROLE_TAG.to_string()])
}

/// Parse a frame list like `1-120`, `7`, or `1-10,20,30-32` into sorted,
/// de-duplicated frame numbers. At most `max_frames` frames.
pub fn parse_frames(text: &str, max_frames: usize) -> std::result::Result<Vec<u32>, String> {
    let mut frames = Vec::new();
    for part in text.split(',').map(str::trim) {
        if part.is_empty() {
            return Err("empty frame range".into());
        }
        let (a, b) = match part.split_once('-') {
            Some((a, b)) => (a.trim(), b.trim()),
            None => (part, part),
        };
        let a: u32 = a.parse().map_err(|_| format!("bad frame number {a:?}"))?;
        let b: u32 = b.parse().map_err(|_| format!("bad frame number {b:?}"))?;
        if b < a {
            return Err(format!("frame range {part} runs backwards"));
        }
        if b > MAX_FRAME {
            return Err(format!("frames must be between 0 and {MAX_FRAME}"));
        }
        if frames.len() + (b - a + 1) as usize > max_frames {
            return Err(format!("at most {max_frames} frames per render"));
        }
        frames.extend(a..=b);
    }
    frames.sort_unstable();
    frames.dedup();
    Ok(frames)
}

/// Group sorted frames into contiguous batches of at most `batch` frames.
pub fn batch_frames(frames: &[u32], batch: u32) -> Vec<(u32, u32)> {
    let batch = batch.clamp(1, MAX_FRAMES_PER_JOB);
    let mut out: Vec<(u32, u32)> = Vec::new();
    for &f in frames {
        match out.last_mut() {
            Some((start, end)) if f == *end + 1 && f - *start < batch => *end = f,
            _ => out.push((f, f)),
        }
    }
    out
}

// ─── images ──────────────────────────────────────────────────────────────

/// What an image file's header says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageInfo {
    pub format: Format,
    pub width: u32,
    pub height: u32,
}

/// Identify a PNG, JPEG or OpenEXR image by its magic bytes and read its
/// dimensions from the header. `None` for anything else, a truncated header,
/// or a zero dimension.
pub fn inspect_image(bytes: &[u8]) -> Option<ImageInfo> {
    let info = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        if bytes.len() < 24 || &bytes[12..16] != b"IHDR" {
            return None;
        }
        ImageInfo {
            format: Format::Png,
            width: u32::from_be_bytes(bytes[16..20].try_into().ok()?),
            height: u32::from_be_bytes(bytes[20..24].try_into().ok()?),
        }
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        let (width, height) = jpeg_dimensions(bytes)?;
        ImageInfo {
            format: Format::Jpeg,
            width,
            height,
        }
    } else if bytes.starts_with(&[0x76, 0x2f, 0x31, 0x01]) {
        let (width, height) = exr_dimensions(bytes)?;
        ImageInfo {
            format: Format::Exr,
            width,
            height,
        }
    } else {
        return None;
    };
    (info.width > 0 && info.height > 0).then_some(info)
}

/// Walk JPEG segments to the first start-of-frame marker.
fn jpeg_dimensions(b: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    while i + 4 <= b.len() {
        if b[i] != 0xFF {
            return None;
        }
        let marker = b[i + 1];
        if marker == 0xFF {
            i += 1; // fill byte
            continue;
        }
        if marker == 0x01 || (0xD0..=0xD8).contains(&marker) {
            i += 2;
            continue;
        }
        let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
        if len < 2 {
            return None;
        }
        let sof = (0xC0..=0xCF).contains(&marker) && ![0xC4, 0xC8, 0xCC].contains(&marker);
        if sof {
            if i + 9 > b.len() {
                return None;
            }
            let h = u16::from_be_bytes([b[i + 5], b[i + 6]]) as u32;
            let w = u16::from_be_bytes([b[i + 7], b[i + 8]]) as u32;
            return Some((w, h));
        }
        i += 2 + len;
    }
    None
}

/// Read `dataWindow` (box2i) from an OpenEXR header.
fn exr_dimensions(b: &[u8]) -> Option<(u32, u32)> {
    let mut i = 8; // magic + version
    for _ in 0..256 {
        let name_end = i + b.get(i..)?.iter().position(|&c| c == 0)?;
        if name_end == i {
            return None; // end of header without a dataWindow
        }
        let name = &b[i..name_end];
        let type_start = name_end + 1;
        let type_end = type_start + b.get(type_start..)?.iter().position(|&c| c == 0)?;
        let size_at = type_end + 1;
        let size = i32::from_le_bytes(b.get(size_at..size_at + 4)?.try_into().ok()?);
        let size = usize::try_from(size).ok()?;
        let value = b.get(size_at + 4..size_at + 4 + size)?;
        if name == b"dataWindow" && &b[type_start..type_end] == b"box2i" && size == 16 {
            let at = |k: usize| i32::from_le_bytes(value[k..k + 4].try_into().unwrap());
            let (xmin, ymin, xmax, ymax) = (at(0), at(4), at(8), at(12));
            let w = u32::try_from(i64::from(xmax) - i64::from(xmin) + 1).ok()?;
            let h = u32::try_from(i64::from(ymax) - i64::from(ymin) + 1).ok()?;
            return Some((w, h));
        }
        i = size_at + 4 + size;
    }
    None
}

// ─── result ──────────────────────────────────────────────────────────────

/// One rendered frame in a worker's result.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct FrameMeta {
    pub frame: u32,
    pub format: String,
    pub width: u32,
    pub height: u32,
    pub size: u64,
    pub hash: String,
    pub pieces: Vec<String>,
}

/// A buyer's check on a worker's result JSON: `v == 1`, `ok`, and one
/// well-formed frame entry per requested frame, in order, in the requested
/// format.
pub fn valid_result(parsed: &Parsed, value: &Value) -> bool {
    result_frames(parsed, value).is_ok()
}

pub fn result_frames(parsed: &Parsed, value: &Value) -> Result<Vec<FrameMeta>> {
    if value.get("v").and_then(Value::as_u64) != Some(1)
        || value.get("ok").and_then(Value::as_bool) != Some(true)
    {
        bail!("result is not a v1 ok render result");
    }
    let frames: Vec<FrameMeta> = serde_json::from_value(
        value
            .get("frames")
            .cloned()
            .context("result has no frames")?,
    )
    .context("result frames are malformed")?;
    if frames.len() != parsed.frame_count() as usize {
        bail!(
            "result has {} frames, expected {}",
            frames.len(),
            parsed.frame_count()
        );
    }
    for (meta, expected) in frames.iter().zip(parsed.frames()) {
        if meta.frame != expected {
            bail!("result frame {} where {expected} was due", meta.frame);
        }
        if meta.format != parsed.format.name() {
            bail!(
                "frame {} is {}, not {}",
                meta.frame,
                meta.format,
                parsed.format.name()
            );
        }
        // `size` is the sealed frame's: a little over the plaintext cap.
        if meta.size == 0 || meta.size > crate::job_e2e::sealed_len(MAX_FRAME_BYTES) || meta.width == 0 || meta.height == 0 {
            bail!("frame {} has an invalid size or dimensions", meta.frame);
        }
        if meta.pieces.len() != piece_count(meta.size) {
            bail!("frame {} lists the wrong number of pieces", meta.frame);
        }
        if Hash::from_hex(&meta.hash).is_err()
            || meta.pieces.iter().any(|p| Hash::from_hex(p).is_err())
        {
            bail!("frame {} has an invalid hash", meta.frame);
        }
    }
    Ok(frames)
}

/// Check a fetched frame's bytes against its metadata: size, whole hash,
/// image magic, and the dimensions in its header.
pub fn verify_frame(meta: &FrameMeta, bytes: &[u8]) -> Result<ImageInfo> {
    if bytes.len() as u64 != meta.size {
        bail!(
            "frame {} is {} bytes, not {}",
            meta.frame,
            bytes.len(),
            meta.size
        );
    }
    if Hash::of(bytes).to_hex() != meta.hash {
        bail!("frame {} does not match its hash", meta.frame);
    }
    check_frame_image(meta, bytes)
}

/// The image checks of [`verify_frame`]: a PNG, JPEG or EXR whose header
/// matches the result's format and dimensions.
pub fn check_frame_image(meta: &FrameMeta, bytes: &[u8]) -> Result<ImageInfo> {
    let info = inspect_image(bytes)
        .with_context(|| format!("frame {} is not a valid PNG, JPEG or EXR", meta.frame))?;
    if info.format.name() != meta.format || info.width != meta.width || info.height != meta.height {
        bail!(
            "frame {} header says {} {}x{}, result says {} {}x{}",
            meta.frame,
            info.format.name(),
            info.width,
            info.height,
            meta.format,
            meta.width,
            meta.height
        );
    }
    Ok(info)
}

// ─── local runtime probe ─────────────────────────────────────────────────

/// What this box's Blender can do. Probed once per process; the role tag, the
/// job subscription and the bid filter all read the same answer.
#[derive(Debug, Clone, Default)]
pub struct Runtime {
    /// `blender --version`'s first line, e.g. `Blender 4.2.3 LTS`.
    pub version_line: Option<String>,
    pub version: (u32, u32, u32),
    /// Cycles GPU backends with at least one device, best first.
    pub gpu_backends: Vec<String>,
    /// The bubblewrap sandbox Blender runs in, where one works.
    pub sandbox: Option<Sandbox>,
    pub sandbox_mode: SandboxMode,
}

/// Preference order when several backends see devices.
const GPU_BACKENDS: &[&str] = &["OPTIX", "CUDA", "HIP", "METAL", "ONEAPI"];

impl Runtime {
    /// Blender answers and, with `sandbox = "require"`, the sandbox works.
    pub fn ready(&self) -> bool {
        self.version_line.is_some()
            && (self.sandbox_mode != SandboxMode::Require || self.sandbox.is_some())
    }
    pub fn gpu(&self) -> Option<&str> {
        self.gpu_backends.first().map(String::as_str)
    }
    /// Outside `require` every job may run; with it, only what the sandbox
    /// was seen to do (`can`).
    fn sandbox_allows(&self, can: impl Fn(&Sandbox) -> bool) -> bool {
        self.sandbox_mode != SandboxMode::Require || self.sandbox.as_ref().is_some_and(can)
    }
    pub fn can_serve(&self, parsed: &Parsed) -> bool {
        self.can_serve_engine(parsed.engine, parsed.device)
    }
    pub fn can_serve_engine(&self, engine: Engine, device: Device) -> bool {
        self.ready()
            && match (engine.needs_gpu(), device) {
                (true, _) => self.gpu().is_some() && self.sandbox_allows(|s| s.raster),
                (false, Device::Gpu) => self.gpu().is_some() && self.sandbox_allows(|s| s.cycles_gpu),
                (false, _) => true,
            }
    }
    /// The `--cycles-device` to use for this job, if any.
    pub fn cycles_device(&self, parsed: &Parsed) -> Option<String> {
        if parsed.engine != Engine::Cycles || parsed.device == Device::Cpu {
            return None;
        }
        // `require` with a sandbox that cannot reach the GPU: AUTO means CPU.
        if !self.sandbox_allows(|s| s.cycles_gpu) {
            return None;
        }
        self.gpu().map(str::to_string)
    }
    /// The sandbox this job's Blender runs in. `None` runs it directly:
    /// there is no sandbox, or (outside `require`) the job needs a GPU the
    /// sandbox was not seen to reach.
    pub fn sandbox_for(&self, parsed: &Parsed) -> Option<&Sandbox> {
        let sandbox = self.sandbox.as_ref()?;
        let usable = if parsed.engine.needs_gpu() {
            sandbox.raster
        } else if self.cycles_device(parsed).is_some() {
            sandbox.cycles_gpu
        } else {
            true
        };
        usable.then_some(sandbox)
    }
    pub fn tags(&self) -> Vec<String> {
        if !self.ready() {
            return Vec::new();
        }
        let mut tags = vec![ROLE_TAG.to_string(), Engine::Cycles.tag()];
        if self.gpu().is_some() {
            let cycles = self.sandbox_allows(|s| s.cycles_gpu);
            if cycles {
                tags.push(GPU_TAG.to_string());
            }
            if self.sandbox_allows(|s| s.raster) {
                tags.push(Engine::Eevee.tag());
                tags.push(Engine::Workbench.tag());
            }
            if cycles {
                tags.extend(
                    self.gpu_backends
                        .iter()
                        .map(|b| format!("c0mpute:render:device:{}", b.to_ascii_lowercase())),
                );
            }
        }
        tags
    }
}

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// Probe (once; blocks for a few seconds the first time) and return.
pub fn runtime() -> &'static Runtime {
    RUNTIME.get_or_init(probe)
}

/// The probe's answer if it has run; never blocks. The advertise loop reads
/// this so a slow Blender start never stalls a heartbeat.
pub fn probed() -> Option<&'static Runtime> {
    RUNTIME.get()
}

/// Run `cmd` to completion or kill it at `timeout`; stdout on success.
fn run_with_deadline(mut cmd: std::process::Command, timeout: Duration) -> Option<String> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut stdout, &mut s);
        s
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let out = reader.join().ok()?;
                return status.success().then_some(out);
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Lists the Cycles backends with devices as `C0DEV:<TYPE>` lines.
const GPU_PROBE_EXPR: &str = "import bpy\n\
p = bpy.context.preferences.addons['cycles'].preferences\n\
for t in ('OPTIX', 'CUDA', 'HIP', 'METAL', 'ONEAPI'):\n\
    try:\n\
        p.compute_device_type = t\n\
        (getattr(p, 'refresh_devices', None) or p.get_devices)()\n\
        if any(d.type == t for d in p.devices):\n\
            print('C0DEV:' + t)\n\
    except Exception:\n\
        pass\n";

fn probe() -> Runtime {
    let mut version_cmd = std::process::Command::new("blender");
    version_cmd.args(["--factory-startup", "--version"]);
    let Some(out) = run_with_deadline(version_cmd, PROBE_TIMEOUT) else {
        return Runtime::default();
    };
    let Some((line, version)) = parse_version(&out) else {
        return Runtime::default();
    };
    let gpu_backends = if std::env::var("C0MPUTE_RENDER_GPU").is_ok_and(|v| v == "0") {
        Vec::new()
    } else {
        let mut cmd = std::process::Command::new("blender");
        cmd.args([
            "--background",
            "--factory-startup",
            "--disable-autoexec",
            "--python-expr",
            GPU_PROBE_EXPR,
        ]);
        run_with_deadline(cmd, PROBE_TIMEOUT)
            .map(|out| parse_gpu_probe(&out))
            .unwrap_or_default()
    };
    let mut rt = Runtime {
        version_line: Some(line),
        version,
        gpu_backends,
        sandbox: None,
        sandbox_mode: sandbox_mode(),
    };
    info!(blender = ?rt.version_line, gpu = ?rt.gpu_backends, "render: blender found");
    if rt.sandbox_mode != SandboxMode::Off {
        rt.sandbox = probe_sandbox(&rt.gpu_backends);
    }
    // Said once: the probe runs once per process.
    match (&rt.sandbox, rt.sandbox_mode) {
        (Some(sb), _) => info!(
            bwrap = %sb.bwrap.display(),
            cycles_gpu = sb.cycles_gpu,
            raster = sb.raster,
            "render: blender runs inside bubblewrap (no network, only the job directory writable)"
        ),
        (None, SandboxMode::Off) => info!("render: [render] sandbox is off; renders run with scene sanitisation only"),
        (None, SandboxMode::Auto) => info!("render: bubblewrap does not work on this machine; renders run with scene sanitisation only"),
        (None, SandboxMode::Require) => warn!("render: [render] sandbox is \"require\" but bubblewrap does not work on this machine; not serving renders"),
    }
    rt
}

/// `Blender 4.2.3 LTS` (possibly after warnings) → the line and (4, 2, 3).
pub fn parse_version(text: &str) -> Option<(String, (u32, u32, u32))> {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("Blender "))?;
    let number = line.split_whitespace().nth(1)?;
    let mut parts = number.split('.').map(|p| {
        p.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse::<u32>()
            .ok()
    });
    let major = parts.next()??;
    let minor = parts.next().flatten().unwrap_or(0);
    let patch = parts.next().flatten().unwrap_or(0);
    Some((line.to_string(), (major, minor, patch)))
}

pub fn parse_gpu_probe(text: &str) -> Vec<String> {
    let found: Vec<&str> = text
        .lines()
        .filter_map(|l| l.trim().strip_prefix("C0DEV:"))
        .collect();
    GPU_BACKENDS
        .iter()
        .filter(|b| found.contains(b))
        .map(|b| b.to_string())
        .collect()
}

// ─── the sandbox ─────────────────────────────────────────────────────────

/// One thing of the host the sandbox shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mount {
    /// A file or directory, read-only, at its own path.
    RoBind(PathBuf),
    /// A device node (or a directory of them), usable, at its own path.
    DevBind(PathBuf),
    /// A symlink the host has (`/bin` to `usr/bin`), made again.
    Symlink { target: PathBuf, link: PathBuf },
}

/// How Blender is confined on this machine, found once by the probe: a
/// bubblewrap (`bwrap`) command line that was seen to start this Blender.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sandbox {
    pub bwrap: PathBuf,
    /// The Blender binary with symlinks resolved; run by this path inside.
    pub blender: PathBuf,
    pub mounts: Vec<Mount>,
    /// The operator's home directory: an empty tmpfs inside.
    pub home: Option<PathBuf>,
    /// The worker is root, so bubblewrap is told to drop every capability
    /// (it keeps them for uid 0 otherwise).
    pub as_root: bool,
    /// Cycles found the same GPU backends inside the sandbox as outside.
    pub cycles_gpu: bool,
    /// A Workbench frame rendered inside the sandbox (EEVEE and Workbench
    /// need a GPU context, which a missing display socket can take away).
    pub raster: bool,
}

/// The `bwrap` arguments that come before the Blender binary: no network,
/// new pid, ipc and hostname namespaces, an empty root with a private
/// `/tmp` and home, the host's `mounts`, and `job_dir` (absolute) as the
/// only writable part of the host. `blend`, the job's input inside it, is
/// read-only again: it can be a hard link to the node's cached copy. Pure,
/// so it is tested without bwrap.
pub fn bwrap_args(sandbox: &Sandbox, job_dir: &Path, blend: Option<&Path>) -> Vec<OsString> {
    let mut a: Vec<OsString> = [
        "--unshare-net",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
        "--hostname",
        "c0mpute-render",
        "--die-with-parent",
        "--new-session",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    if sandbox.as_root {
        a.extend(["--cap-drop", "ALL"].map(OsString::from));
    }
    a.extend(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"].map(OsString::from));
    // The tmpfs comes before the binds: a Blender or a job directory under
    // the home (or under /tmp) is then mounted on top of it.
    if let Some(home) = &sandbox.home {
        a.push("--tmpfs".into());
        a.push(home.into());
    }
    for mount in &sandbox.mounts {
        let (flag, from, to) = match mount {
            Mount::RoBind(p) => ("--ro-bind", p, p),
            Mount::DevBind(p) => ("--dev-bind", p, p),
            Mount::Symlink { target, link } => ("--symlink", target, link),
        };
        a.push(flag.into());
        a.push(from.into());
        a.push(to.into());
    }
    a.push("--bind".into());
    a.push(job_dir.into());
    a.push(job_dir.into());
    if let Some(blend) = blend {
        a.push("--ro-bind".into());
        a.push(blend.into());
        a.push(blend.into());
    }
    a.push("--setenv".into());
    a.push("HOME".into());
    a.push(sandbox.home.clone().unwrap_or_else(|| PathBuf::from("/tmp")).into());
    a.push("--chdir".into());
    a.push(job_dir.into());
    a.push("--".into());
    a
}

/// Is `path` under something already bound read-only?
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn covered(mounts: &[Mount], path: &Path) -> bool {
    mounts.iter().any(|m| matches!(m, Mount::RoBind(root) if path.starts_with(root)))
}

/// May a directory outside the system directories be shown read-only? Not
/// the root, not a top-level directory, and nothing that holds the home.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn shareable(dir: &Path, home: Option<&Path>) -> bool {
    dir.is_absolute()
        && dir.components().filter(|c| matches!(c, std::path::Component::Normal(_))).count() >= 2
        && !home.is_some_and(|h| h.starts_with(dir))
}

/// Sets a 16x16 frame for the sandbox's Workbench test render.
#[cfg(target_os = "linux")]
const RASTER_PROBE_EXPR: &str = "import bpy\n\
s = bpy.context.scene\n\
s.render.resolution_x = 16\n\
s.render.resolution_y = 16\n\
s.render.resolution_percentage = 100\n";

/// What `/etc` holds that a dynamically linked, GPU-using Blender reads.
/// Never the whole directory: a root worker can read all of it.
#[cfg(target_os = "linux")]
const ETC_ENTRIES: &[&str] = &["ld.so.cache", "ld.so.conf", "ld.so.conf.d", "alternatives", "OpenCL", "vulkan", "glvnd", "egl"];

#[cfg(target_os = "linux")]
fn find_on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).map(|dir| dir.join(name)).find(|p| p.is_file())
}

/// Show `path` read-only where it really lives, keeping the symlink when it
/// is one (`/bin`, `/opt/rocm`).
#[cfg(target_os = "linux")]
fn add_ro(mounts: &mut Vec<Mount>, path: &Path) {
    let (Ok(meta), Ok(real)) = (std::fs::symlink_metadata(path), path.canonicalize()) else {
        return;
    };
    if meta.file_type().is_symlink()
        && let Ok(target) = std::fs::read_link(path)
    {
        mounts.push(Mount::Symlink { target, link: path.to_path_buf() });
    }
    if !covered(mounts, &real) {
        mounts.push(Mount::RoBind(real));
    }
}

/// What of this host Blender needs: the system directories, the loader and
/// GPU vendor files under `/etc`, `/sys` and the GPU device nodes, and the
/// directory Blender is installed in when that is not a system directory (a
/// blender.org tarball under `/opt` or the operator's home). `None` when
/// showing that directory would show the home itself.
#[cfg(target_os = "linux")]
fn host_mounts(blender: &Path, home: Option<&Path>) -> Option<Vec<Mount>> {
    let mut mounts = Vec::new();
    for dir in ["/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/libx32", "/sys", "/opt/rocm"] {
        add_ro(&mut mounts, Path::new(dir));
    }
    for name in ETC_ENTRIES {
        add_ro(&mut mounts, &Path::new("/etc").join(name));
    }
    // Driver libraries the operator points the loader at (a CUDA or ROCm
    // install outside the system directories).
    if let Some(paths) = std::env::var_os("LD_LIBRARY_PATH") {
        for dir in std::env::split_paths(&paths) {
            if let Ok(real) = dir.canonicalize()
                && real.is_dir()
                && !covered(&mounts, &real)
                && shareable(&real, home)
            {
                mounts.push(Mount::RoBind(real));
            }
        }
    }
    let install = blender.parent()?;
    if !covered(&mounts, install) {
        if !shareable(install, home) {
            return None;
        }
        mounts.push(Mount::RoBind(install.to_path_buf()));
    }
    for dev in ["/dev/dri", "/dev/kfd"] {
        if Path::new(dev).exists() {
            mounts.push(Mount::DevBind(dev.into()));
        }
    }
    let mut nvidia: Vec<PathBuf> = std::fs::read_dir("/dev")
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("nvidia"))
        .map(|e| e.path())
        .collect();
    nvidia.sort();
    mounts.extend(nvidia.into_iter().map(Mount::DevBind));
    Some(mounts)
}

/// Find out, once, whether bubblewrap can run this machine's Blender: build
/// the sandbox and start Blender inside it. A machine without `bwrap`, with
/// unprivileged user namespaces switched off (Ubuntu 24.04's AppArmor
/// default without a bwrap profile), or with a Blender the sandbox cannot
/// start (a snap, a wrapper script around an install elsewhere) gets `None`.
/// With a GPU, also checks that Cycles and Workbench still reach it inside.
#[cfg(target_os = "linux")]
fn probe_sandbox(gpu_backends: &[String]) -> Option<Sandbox> {
    let bwrap = find_on_path("bwrap")?;
    let blender = find_on_path("blender")?.canonicalize().ok()?;
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|h| h.is_absolute() && h.parent().is_some());
    let mounts = host_mounts(&blender, home.as_deref())?;
    let mut sandbox = Sandbox {
        bwrap,
        blender,
        mounts,
        home,
        as_root: crate::container_run::is_root(),
        cycles_gpu: false,
        raster: false,
    };
    let dir = std::env::temp_dir().join(format!("c0mpute-render-probe-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).ok()?;
    let run = |sandbox: &Sandbox, args: &[&std::ffi::OsStr]| {
        let mut cmd = std::process::Command::new(&sandbox.bwrap);
        cmd.args(bwrap_args(sandbox, &dir, None)).arg(&sandbox.blender).args(args);
        run_with_deadline(cmd, PROBE_TIMEOUT)
    };
    let os = |s: &'static str| std::ffi::OsStr::new(s);
    let works = run(&sandbox, &[os("--factory-startup"), os("--version")])
        .is_some_and(|out| parse_version(&out).is_some());
    if works && !gpu_backends.is_empty() {
        sandbox.cycles_gpu = run(
            &sandbox,
            &[os("--background"), os("--factory-startup"), os("--disable-autoexec"), os("--python-expr"), os(GPU_PROBE_EXPR)],
        )
        .is_some_and(|out| parse_gpu_probe(&out) == gpu_backends);
        let frame = dir.join("probe_####");
        sandbox.raster = run(
            &sandbox,
            &[
                os("--background"),
                os("--factory-startup"),
                os("--disable-autoexec"),
                os("--python-expr"),
                os(RASTER_PROBE_EXPR),
                os("--engine"),
                os("BLENDER_WORKBENCH"),
                os("--render-output"),
                frame.as_os_str(),
                os("--render-format"),
                os("PNG"),
                os("--render-frame"),
                os("1"),
            ],
        )
        .is_some()
            && dir.join("probe_0001.png").is_file();
    }
    let _ = std::fs::remove_dir_all(&dir);
    works.then_some(sandbox)
}

/// Bubblewrap is Linux only: everywhere else renders run with scene
/// sanitisation only.
#[cfg(not(target_os = "linux"))]
fn probe_sandbox(_gpu_backends: &[String]) -> Option<Sandbox> {
    None
}

/// Bid filter: skip offers this box cannot render (a GPU engine or device
/// without a GPU, or a spec it cannot read).
pub fn offer_filter() -> crate::dispatch::OfferFilter {
    Arc::new(|offer: &JobOffer| match crate::job_e2e::bid_spec(offer) {
        // The .blend file stays sealed: judge the engine and device.
        Some(crate::job_e2e::BidSpec::Public(v)) => {
            let field = |k: &str, d: &str| v.get(k).and_then(Value::as_str).unwrap_or(d).to_string();
            match (Engine::parse(&field("engine", &default_engine())), Device::parse(&field("device", &default_device()))) {
                (Some(engine), Some(device)) => runtime().can_serve_engine(engine, device),
                _ => false,
            }
        }
        Some(crate::job_e2e::BidSpec::Full(spec)) => parse_spec(spec).is_ok_and(|parsed| runtime().can_serve(&parsed)),
        None => false,
    })
}

// ─── the blender command ─────────────────────────────────────────────────

/// The one line of Python the render runs (after the file loads, before the
/// render): only settings from validated integers.
pub fn settings_expr(parsed: &Parsed, cycles_device: Option<&str>) -> String {
    let mut lines = vec![
        "import bpy".to_string(),
        "s = bpy.context.scene".to_string(),
    ];
    if let Some(p) = parsed.spec.resolution_pct {
        lines.push(format!("s.render.resolution_percentage = {p}"));
    }
    match parsed.engine {
        Engine::Cycles => {
            let device = if cycles_device.is_some() {
                "GPU"
            } else {
                "CPU"
            };
            lines.push(format!("s.cycles.device = '{device}'"));
            // Distro builds (Debian's among them) ship without
            // OpenImageDenoise, and a scene with denoising on then fails
            // with "Build without OpenImageDenoiser". Render it undenoised.
            lines.push(
                "if 'OPENIMAGEDENOISE' not in [i.identifier for i in \
                 s.cycles.bl_rna.properties['denoiser'].enum_items] \
                 and s.cycles.denoiser != 'OPTIX': s.cycles.use_denoising = False"
                    .to_string(),
            );
            if let Some(n) = parsed.spec.samples {
                lines.push(format!("s.cycles.samples = {n}"));
            }
        }
        Engine::Eevee => {
            if let Some(n) = parsed.spec.samples {
                lines.push(format!("s.eevee.taa_render_samples = {n}"));
            }
        }
        Engine::Workbench => {}
    }
    lines.join("\n")
}

/// The job directory and its output directory, handed to [`SANITIZE_EXPR`]
/// in the environment so that no path is ever written into the script.
pub const JOB_DIR_ENV: &str = "C0MPUTE_RENDER_JOB_DIR";
pub const OUT_DIR_ENV: &str = "C0MPUTE_RENDER_OUT_DIR";
/// Blender's exit code when [`SANITIZE_EXPR`] could not finish: the scene is
/// not rendered at all rather than rendered as the buyer saved it.
pub const SANITIZE_EXIT: i32 = 97;

/// Python run after the file loads and before the render, on every OS: it
/// leaves the scene able to read and write inside the job directory only.
/// A constant: nothing from the job is formatted into it (the two
/// directories come from the environment), so a buyer cannot reach it.
///
/// A `.blend` names files by path: image textures, movie clips, sounds,
/// fonts, linked libraries, caches, sequencer strips. Blender reads them
/// from the worker's disk when it renders, so a scene that names
/// `/home/<user>/Pictures/x.png` would return that picture inside a frame.
/// Every such path that is absolute or leaves the job directory is emptied
/// (the scene then renders with Blender's pink "missing" texture, the
/// built-in font, or without the strip or cache); data packed into the
/// `.blend` is untouched. Linked libraries are loaded by Blender while the
/// file opens, before this runs, so what came from them is deleted instead.
/// Compositor File Output nodes are removed, Open Shading Language is turned
/// off (a compiled shader may open any image by name), and every scene's
/// output path is set to the node's own output directory.
///
/// Written for Blender 3.x to 5.x: attributes a version lacks are skipped
/// (`getattr`), and any other failure exits with [`SANITIZE_EXIT`].
pub const SANITIZE_EXPR: &str = r#"import bpy, os, sys, traceback

def _c0_sanitize():
    D = bpy.data
    job_dir = os.environ.get('C0MPUTE_RENDER_JOB_DIR')
    out_dir = os.environ.get('C0MPUTE_RENDER_OUT_DIR')
    if not job_dir or not out_dir:
        raise RuntimeError('the job and output directories are not set')
    root = os.path.normcase(os.path.realpath(job_dir))
    cut = [0]

    def inside(path, lib=None):
        try:
            full = os.path.normcase(os.path.realpath(bpy.path.abspath(path, library=lib)))
            return os.path.commonpath([root, full]) == root
        except Exception:
            return False

    def drop(owner, attr, lib=None, to=''):
        path = getattr(owner, attr, None)
        if not isinstance(path, str) or path in ('', to) or inside(path, lib):
            return False
        setattr(owner, attr, to)
        cut[0] += 1
        return True

    def outside_libs():
        return [lib for lib in D.libraries if not inside(lib.filepath)]

    bad = outside_libs()
    if bad:
        def from_bad(i):
            if i.library in bad:
                return True
            ref = getattr(getattr(i, 'override_library', None), 'reference', None)
            return ref is not None and ref.library in bad
        D.batch_remove([i for i in D.user_map() if from_bad(i)])
        cut[0] += len(bad)
        for lib in outside_libs():
            D.libraries.remove(lib)
        if outside_libs():
            raise RuntimeError('a linked library could not be removed')

    for im in D.images:
        packed = im.packed_file is not None or len(getattr(im, 'packed_files', ())) > 0
        if packed and im.source in ('FILE', 'TILED'):
            continue
        drop(im, 'filepath', im.library)
    for name, keep_packed in (('movieclips', False), ('sounds', True), ('volumes', True), ('cache_files', False)):
        for item in getattr(D, name, ()):
            if keep_packed and getattr(item, 'packed_file', None) is not None:
                continue
            drop(item, 'filepath', item.library)
    for font in D.fonts:
        if font.packed_file is None:
            drop(font, 'filepath', font.library, '<builtin>')

    for ob in D.objects:
        for mod in ob.modifiers:
            domain = getattr(mod, 'domain_settings', None)
            if domain is not None:
                drop(domain, 'cache_directory', ob.library, os.path.join(job_dir, 'fluid_cache'))
            drop(mod, 'filepath', ob.library)
            drop(mod, 'bake_directory', ob.library)
            for bake in getattr(mod, 'bakes', ()):
                drop(bake, 'directory', ob.library)
            caches = [getattr(mod, 'point_cache', None)]
            for surface in getattr(getattr(mod, 'canvas_settings', None), 'canvas_surfaces', ()):
                caches.append(getattr(surface, 'point_cache', None))
            for cache in caches:
                if cache is not None:
                    drop(cache, 'filepath', ob.library)
        for psys in ob.particle_systems:
            cache = getattr(psys, 'point_cache', None)
            if cache is not None:
                drop(cache, 'filepath', ob.library)

    trees = list(D.node_groups)
    for name in ('materials', 'worlds', 'lights', 'textures', 'linestyles', 'scenes'):
        for item in getattr(D, name, ()):
            tree = getattr(item, 'node_tree', None)
            if tree is not None:
                trees.append(tree)
    for tree in trees:
        for node in list(tree.nodes):
            if node.type == 'OUTPUT_FILE':
                try:
                    tree.nodes.remove(node)
                except Exception:
                    node.mute = True
                    for attr in ('base_path', 'directory'):
                        if hasattr(node, attr):
                            setattr(node, attr, out_dir)
                cut[0] += 1
            else:
                drop(node, 'filepath', tree.library)

    for sc in D.scenes:
        r = sc.render
        r.filepath = os.path.join(out_dir, 'frame_####')
        r.use_file_extension = True
        for attr in ('use_render_cache', 'use_stamp_hostname', 'use_stamp_filename'):
            if hasattr(r, attr):
                setattr(r, attr, False)
        cycles = getattr(sc, 'cycles', None)
        if cycles is not None and hasattr(cycles, 'shading_system'):
            cycles.shading_system = False
        world = getattr(sc, 'rigidbody_world', None)
        if world is not None and getattr(world, 'point_cache', None) is not None:
            drop(world.point_cache, 'filepath', sc.library)
        ed = sc.sequence_editor
        if ed is None:
            continue
        if drop(ed, 'proxy_dir', sc.library) and hasattr(ed, 'proxy_storage'):
            ed.proxy_storage = 'PER_STRIP'
        strips = getattr(ed, 'strips_all', None)
        if strips is None:
            strips = getattr(ed, 'sequences_all', ())
        for strip in strips:
            hit = drop(strip, 'filepath', sc.library)
            hit = drop(strip, 'directory', sc.library) or hit
            if hit:
                strip.mute = True
            proxy = getattr(strip, 'proxy', None)
            if proxy is not None:
                hit = drop(proxy, 'filepath', sc.library)
                hit = drop(proxy, 'directory', sc.library) or hit
                if hit and hasattr(strip, 'use_proxy'):
                    strip.use_proxy = False

    left = [p for p in bpy.utils.blend_paths(absolute=True, packed=False, local=False) if not inside(p)]
    print('c0mpute: scene sanitised: %d outside references cut, %d path fields a render does not read left as they were' % (cut[0], len(left)))

try:
    _c0_sanitize()
except BaseException:
    traceback.print_exc()
    sys.stdout.flush()
    sys.stderr.flush()
    os._exit(97)
"#;

/// The full `blender` argument list for one job. `out_dir` must not contain
/// `#` (Blender's frame-number placeholder). The `.blend` path is one
/// argument (no shell is involved), and `--disable-autoexec` comes before it
/// so nothing in the file runs.
pub fn blender_args(
    blend: &Path,
    out_dir: &Path,
    parsed: &Parsed,
    version: (u32, u32, u32),
    cycles_device: Option<&str>,
    threads: usize,
) -> Result<Vec<OsString>> {
    if out_dir.to_string_lossy().contains('#') {
        bail!("render output directory must not contain '#'");
    }
    let mut args: Vec<OsString> = ["--background", "--factory-startup", "--disable-autoexec"]
        .into_iter()
        .map(OsString::from)
        .collect();
    args.push(blend.as_os_str().to_owned());
    args.push("--python-expr".into());
    args.push(settings_expr(parsed, cycles_device).into());
    args.push("--python-expr".into());
    args.push(SANITIZE_EXPR.into());
    args.push("--engine".into());
    args.push(parsed.engine.blender_id(version).into());
    args.push("--render-output".into());
    args.push(out_dir.join("frame_####").into_os_string());
    args.push("--render-format".into());
    args.push(parsed.format.blender_id().into());
    args.push("--threads".into());
    args.push(threads.max(1).to_string().into());
    args.push("--frame-start".into());
    args.push(parsed.spec.frame_start.to_string().into());
    args.push("--frame-end".into());
    args.push(parsed.spec.frame_end.to_string().into());
    args.push("--render-anim".into());
    if let Some(device) = cycles_device {
        args.push("--".into());
        args.push("--cycles-device".into());
        args.push(device.into());
    }
    Ok(args)
}

/// `frame_0007.png` → 7 (any digit count, the given extension).
pub fn frame_number(name: &str, extension: &str) -> Option<u32> {
    let digits = name
        .strip_prefix("frame_")?
        .strip_suffix(extension)?
        .strip_suffix('.')?;
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// What one rendered frame is, before it is stored.
#[derive(Debug, Clone)]
pub struct Rendered {
    pub frame: u32,
    pub path: PathBuf,
    pub info: ImageInfo,
    pub size: u64,
}

/// Render `parsed`'s frames of `blend` into `job_dir/out` and check each
/// output. Needs a ready [`runtime`].
pub async fn render(blend: &Path, parsed: &Parsed, job_dir: &Path) -> Result<Vec<Rendered>> {
    let rt = runtime();
    if rt.version_line.is_none() {
        bail!("blender is not installed on this node");
    }
    if !rt.ready() {
        bail!("this node renders only inside a sandbox, and bubblewrap does not work here");
    }
    if !rt.can_serve(parsed) {
        bail!(
            "this node cannot render {} on {}",
            parsed.engine.name(),
            parsed.device.name()
        );
    }
    tokio::fs::create_dir_all(job_dir.join("out")).await?;
    let sandbox = rt.sandbox_for(parsed);
    // Bubblewrap binds the job directory by its real path. Without it the
    // path only has to be absolute: the sanitiser reads it after Blender has
    // changed into the job directory.
    let job_dir = &match sandbox {
        Some(_) => tokio::fs::canonicalize(job_dir).await?,
        None => std::path::absolute(job_dir)?,
    };
    let out_dir = job_dir.join("out");
    let blend = &stage_input(blend, job_dir).await?;
    let cycles_device = rt.cycles_device(parsed);
    let mut command = match sandbox {
        Some(sb) => {
            let mut command = Command::new(&sb.bwrap);
            command.args(bwrap_args(sb, job_dir, Some(blend.as_path()))).arg(&sb.blender);
            command
        }
        None => Command::new("blender"),
    };
    // Environment only, so the limits reach Blender through bubblewrap too.
    let threads = crate::resources::apply_job_limits(&mut command);
    command.env(JOB_DIR_ENV, job_dir).env(OUT_DIR_ENV, &out_dir);
    command.args(blender_args(
        blend,
        &out_dir,
        parsed,
        rt.version,
        cycles_device.as_deref(),
        threads,
    )?);
    // Cycles prints a progress line per tile: keep it on disk, not in memory.
    let log_path = job_dir.join("blender.log");
    let log = std::fs::File::create(&log_path)?;
    command
        .current_dir(job_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .kill_on_drop(true);
    let timeout = parsed.frame_timeout() * parsed.frame_count();
    let mut child = command.spawn().context("start blender")?;
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(status) => status?,
        Err(_) => {
            let _ = child.kill().await;
            bail!("blender timed out after {}s", timeout.as_secs());
        }
    };
    if status.code() == Some(SANITIZE_EXIT) {
        let tail = log_tail(&log_path).await;
        bail!("the scene could not be made safe to render, so it was not rendered: {tail}");
    }
    let ext = parsed.format.extension();
    let mut found: HashMap<u32, PathBuf> = HashMap::new();
    let mut entries = tokio::fs::read_dir(&out_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(n) = frame_number(&name, ext) {
            found.insert(n, entry.path());
        }
    }
    let mut frames = Vec::new();
    for frame in parsed.frames() {
        let Some(path) = found.remove(&frame) else {
            let tail = log_tail(&log_path).await;
            bail!("blender ({status}) rendered no frame {frame}: {tail}");
        };
        let size = tokio::fs::metadata(&path).await?.len();
        if size == 0 || size > MAX_FRAME_BYTES {
            bail!("frame {frame} is {size} bytes (limit {MAX_FRAME_BYTES})");
        }
        let mut head = vec![0u8; size.min(256 * 1024) as usize];
        tokio::fs::File::open(&path)
            .await?
            .read_exact(&mut head)
            .await?;
        let info = inspect_image(&head)
            .with_context(|| format!("frame {frame} is not a readable {}", parsed.format.name()))?;
        if info.format != parsed.format {
            bail!("frame {frame} came out as {}", info.format.name());
        }
        frames.push(Rendered {
            frame,
            path,
            info,
            size,
        });
    }
    Ok(frames)
}

/// The `.blend` as this job's own file, `<job_dir>/input.blend`: a hard link
/// to the cached copy (a copy where the filesystem cannot link). Blender
/// resolves `//relative` paths against the file's directory, and the cache
/// directory holds other buyers' scenes; the job directory holds only this
/// job's.
async fn stage_input(blend: &Path, job_dir: &Path) -> Result<PathBuf> {
    // Already the job's own (an opened sealed input), however it is spelled.
    let real = tokio::fs::canonicalize(blend).await.context("find the .blend")?;
    if real.parent() == tokio::fs::canonicalize(job_dir).await.ok().as_deref()
        && let Some(name) = blend.file_name()
    {
        return Ok(job_dir.join(name));
    }
    let staged = job_dir.join("input.blend");
    let _ = tokio::fs::remove_file(&staged).await;
    if tokio::fs::hard_link(blend, &staged).await.is_err() {
        tokio::fs::copy(blend, &staged).await.context("copy the .blend into the job directory")?;
    }
    Ok(staged)
}

async fn log_tail(path: &Path) -> String {
    let text = tokio::fs::read_to_string(path).await.unwrap_or_default();
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(3)..].join(" | ")
}

// ─── moving files in pieces ──────────────────────────────────────────────

pub use crate::pieces::Pieces;

/// Store `path` as [`PIECE_BYTES`] pieces and announce each
/// ([`crate::pieces::put_file_pieces`]).
pub async fn put_file_pieces(
    net: &Libp2pNetwork,
    store: &ChunkStore,
    path: &Path,
    max_bytes: u64,
) -> Result<Pieces> {
    crate::pieces::put_file_pieces(net, store, path, max_bytes, PIECE_BYTES).await
}

/// One lock per input hash, so two frames of the same job on one worker
/// fetch the `.blend` once.
fn input_lock(hash: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    locks.retain(|_, l| Arc::strong_count(l) > 1);
    locks.entry(hash.to_string()).or_default().clone()
}

/// The cached `.blend` for this spec, fetching it first if needed.
pub async fn ensure_input(
    net: &Arc<Libp2pNetwork>,
    cache_dir: &Path,
    parsed: &Parsed,
) -> Result<PathBuf> {
    let hex = parsed.input_hash.to_hex();
    let path = cache_dir.join(format!("{hex}.blend"));
    let lock = input_lock(&hex);
    let _guard = lock.lock().await;
    if let Ok(meta) = tokio::fs::metadata(&path).await
        && meta.len() == parsed.spec.input_size
    {
        // Only verified files are ever renamed into place; mark it fresh.
        if let Ok(f) = std::fs::File::options().append(true).open(&path) {
            let _ = f.set_modified(std::time::SystemTime::now());
        }
        return Ok(path);
    }
    tokio::fs::create_dir_all(cache_dir).await?;
    let provider = parsed
        .spec
        .input_provider_peer_id
        .as_deref()
        .map(|p| p.parse())
        .transpose()
        .context("invalid input provider peer id")?;
    crate::pieces::fetch_pieces_to_file(
        net,
        provider,
        &parsed.pieces,
        parsed.spec.input_size,
        parsed.input_hash,
        PIECE_BYTES,
        &path,
    )
    .await
    .context("fetch the .blend")?;
    evict_cache(cache_dir, &path, CACHE_MAX_BYTES).await;
    Ok(path)
}

/// Delete the oldest cached files until the cache fits in `max` (never `keep`).
async fn evict_cache(dir: &Path, keep: &Path, max: u64) {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return;
    };
    let mut files = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        if let Ok(meta) = entry.metadata().await
            && meta.is_file()
        {
            let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
            files.push((modified, meta.len(), entry.path()));
        }
    }
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    files.sort();
    for (_, len, path) in files {
        if total <= max {
            break;
        }
        if path != keep && tokio::fs::remove_file(&path).await.is_ok() {
            total -= len;
        }
    }
}

// ─── worker runner ───────────────────────────────────────────────────────

pub fn spawn_render_runner(
    net: Arc<Libp2pNetwork>,
    store: ChunkStore,
    cache_root: PathBuf,
    load: WorkerLoad,
) -> mpsc::Sender<AcceptedJob> {
    let (tx, mut rx) = mpsc::channel::<AcceptedJob>(8);
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let Ok(slot) = load.compute_slots().acquire_owned().await else {
                return;
            };
            let net = net.clone();
            let store = store.clone();
            let cache_root = cache_root.clone();
            let load = load.clone();
            tokio::spawn(async move {
                let _slot = slot;
                let started = Instant::now();
                let job_id = job.accept.job_id.clone();
                let result = crate::job_control::cancellable(&job.accept.job_id, run_render_job(&net, &store, &cache_root, &job)).await;
                cleanup_job_dir(&cache_root, &job_id).await;
                match result {
                    Ok(hash) => {
                        load.record_job(started.elapsed());
                        info!(%job_id, %hash, "render runner: job complete");
                        publish_receipt(&net, &job.accept, Some(hash), JobStatus::Completed).await;
                    }
                    Err(error) => {
                        warn!(%job_id, err = %format!("{error:#}"), "render runner: job failed");
                        publish_receipt(&net, &job.accept, None, JobStatus::Failed).await;
                    }
                }
            });
        }
    });
    tx
}

async fn run_render_job(
    net: &Arc<Libp2pNetwork>,
    store: &ChunkStore,
    cache_root: &Path,
    job: &AcceptedJob,
) -> Result<String> {
    let spec = crate::workflow::resolve_spec(net, job).await?;
    let parsed = parse_spec(&spec).map_err(anyhow::Error::msg)?;
    let cached = ensure_input(net, &cache_root.join("render-inputs"), &parsed).await?;
    let job_dir = safe_job_dir(cache_root, &job.accept.job_id)?;
    tokio::fs::create_dir_all(&job_dir).await?;
    // A sealed .blend stays sealed in the cache; the plaintext lives only in
    // this job's directory, removed when the job ends.
    let blend = match crate::pieces::input_key(parsed.spec.input_key.as_deref())? {
        Some(key) => {
            let plain = job_dir.join("input.blend");
            crate::job_e2e::open_file(&key, &cached, &plain).await.context("the .blend does not open with its key")?;
            plain
        }
        None => cached,
    };
    let frames = render(&blend, &parsed, &job_dir).await?;
    let rt = runtime();
    let mut metas = Vec::with_capacity(frames.len());
    for f in &frames {
        // An e2e job's frames are sealed to the buyer before they are stored.
        let (file, limit) = crate::job_e2e::sealed_result_file(&job.accept, &f.path, MAX_FRAME_BYTES).await?;
        let stored = put_file_pieces(net, store, &file.path, limit).await?;
        metas.push(FrameMeta {
            frame: f.frame,
            format: f.info.format.name().into(),
            width: f.info.width,
            height: f.info.height,
            size: stored.size,
            hash: stored.hash,
            pieces: stored.pieces,
        });
    }
    let result = json!({
        "v": 1,
        "ok": true,
        "engine": parsed.engine.name(),
        "engine_id": parsed.engine.blender_id(rt.version),
        "device": rt.cycles_device(&parsed).unwrap_or_else(|| "CPU".into()),
        "blender": rt.version_line,
        "format": parsed.format.name(),
        "frames": metas,
    });
    let bytes = serde_json::to_vec(&result)?;
    if bytes.len() > MAX_RESULT_BYTES {
        bail!("render result is over the {MAX_RESULT_BYTES}-byte limit");
    }
    crate::pieces::publish_result_bytes(net, store, &job.accept, &bytes)
        .await
        .context("publish render result")
}

// ─── buyer ───────────────────────────────────────────────────────────────

/// Fetch every frame a worker's result lists from that worker and check it
/// ([`verify_frame`]). Returns `(meta, bytes)` in frame order.
pub async fn fetch_frames(
    net: &Arc<Libp2pNetwork>,
    worker: &str,
    frames: &[FrameMeta],
    key: &c0mpute_net::topics::JobKey,
    budget: Duration,
) -> Result<Vec<(FrameMeta, Vec<u8>)>> {
    let started = Instant::now();
    let mut out = Vec::with_capacity(frames.len());
    for meta in frames {
        let mut bytes = Vec::with_capacity(meta.size as usize);
        for piece in &meta.pieces {
            let left = budget.saturating_sub(started.elapsed());
            if left.is_zero() {
                bail!("timed out fetching frame {}", meta.frame);
            }
            let chunk = crate::gateway_util::fetch_verified(
                net,
                piece,
                worker,
                PIECE_BYTES,
                left.min(Duration::from_secs(60)),
            )
            .await
            .map_err(|e| anyhow::anyhow!("frame {} piece: {e:?}", meta.frame))?;
            bytes.extend_from_slice(&chunk);
            if bytes.len() as u64 > meta.size {
                bail!("frame {} is bigger than its result says", meta.frame);
            }
        }
        // `size` and `hash` name the sealed frame; the image is checked once
        // it is opened with the job's result key.
        if bytes.len() as u64 != meta.size || Hash::of(&bytes).to_hex() != meta.hash {
            bail!("frame {} does not match its size or hash", meta.frame);
        }
        let plain = crate::job_e2e::open_blob(key, &bytes)
            .with_context(|| format!("frame {} does not open with this job's key", meta.frame))?;
        check_frame_image(meta, &plain)?;
        out.push((meta.clone(), plain));
    }
    Ok(out)
}

/// A distributed render: what [`render_frames`] needs besides the network.
#[derive(Debug, Clone)]
pub struct RenderJob {
    pub blend: PathBuf,
    pub frames: Vec<u32>,
    /// Frames per job (1 to [`MAX_FRAMES_PER_JOB`]).
    pub batch: u32,
    pub engine: Engine,
    pub device: Device,
    pub format: Format,
    pub resolution_pct: Option<u32>,
    pub samples: Option<u32>,
    pub frame_timeout: Duration,
    pub max_price_usd: f64,
    /// Jobs in flight at once.
    pub concurrency: usize,
    /// Tries per batch, each on a worker that has not failed it.
    pub attempts: usize,
    pub out_dir: PathBuf,
    pub allowed_bidders: Vec<String>,
    pub bid_window: Duration,
}

/// One written frame.
#[derive(Debug, Clone)]
pub struct WrittenFrame {
    pub frame: u32,
    pub path: PathBuf,
    pub worker: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Default)]
pub struct RenderReport {
    pub written: Vec<WrittenFrame>,
    /// Frames no worker delivered, with the last error.
    pub failed: Vec<(u32, String)>,
}

/// The output file for a frame.
pub fn frame_path(out_dir: &Path, frame: u32, format: Format) -> PathBuf {
    out_dir.join(format!("frame_{frame:04}.{}", format.extension()))
}

/// Publish the `.blend`, split the frames into batches, and auction the
/// batches in parallel (bounded by `concurrency`), retrying a failed batch on
/// workers that have not failed it. Every frame is checked (hash, image
/// magic, header dimensions, and the same dimensions as the other frames)
/// before it is written to `out_dir`. Fully peer to peer: each batch is its
/// own auction on the job topic.
pub async fn render_frames(
    net: Arc<Libp2pNetwork>,
    store: &ChunkStore,
    job: RenderJob,
    progress: impl Fn(&str) + Send + Sync + 'static,
) -> Result<RenderReport> {
    if job.frames.is_empty() {
        bail!("no frames to render");
    }
    tokio::fs::create_dir_all(&job.out_dir).await?;
    // Sealed under a fresh key that only the winning workers learn.
    let (input, input_key) =
        crate::pieces::put_file_pieces_sealed(&net, store, &job.blend, MAX_INPUT_BYTES, PIECE_BYTES).await?;
    let input_key = crate::job_e2e::key_to_hex(&input_key);
    progress(&format!(
        "published {} ({} bytes, {} pieces, blake3 {})",
        job.blend.display(),
        input.size,
        input.pieces.len(),
        &input.hash[..16]
    ));
    let provider = net.peer_id().to_base58();
    let batches = batch_frames(&job.frames, job.batch);
    let job = Arc::new(job);
    let progress = Arc::new(progress);
    let dims: Arc<Mutex<Option<(u32, u32)>>> = Arc::default();
    let slots = Arc::new(Semaphore::new(job.concurrency.max(1)));
    let mut set = tokio::task::JoinSet::new();
    for (start, end) in batches {
        let spec = json!({
            "v": 1,
            "input_hash": input.hash,
            "input_size": input.size,
            "input_pieces": input.pieces,
            "input_provider_peer_id": provider,
            "input_key": input_key,
            "frame_start": start,
            "frame_end": end,
            "engine": job.engine.name(),
            "device": job.device.name(),
            "format": job.format.name(),
            "resolution_pct": job.resolution_pct,
            "samples": job.samples,
            "frame_timeout_secs": job.frame_timeout.as_secs(),
        });
        let parsed = parse_spec(&spec).map_err(anyhow::Error::msg)?;
        let (net, job, slots, progress, dims) = (
            net.clone(),
            job.clone(),
            slots.clone(),
            progress.clone(),
            dims.clone(),
        );
        set.spawn(async move {
            let _slot = slots.acquire_owned().await;
            let outcome = run_batch(&net, &job, spec, &parsed, &dims, &*progress).await;
            (start, end, outcome)
        });
    }
    let mut report = RenderReport::default();
    while let Some(joined) = set.join_next().await {
        let (start, end, outcome) = joined?;
        match outcome {
            Ok(frames) => report.written.extend(frames),
            Err(error) => {
                let message = format!("{error:#}");
                progress(&format!("frames {start}-{end} failed: {message}"));
                report
                    .failed
                    .extend((start..=end).map(|f| (f, message.clone())));
            }
        }
    }
    report.written.sort_by_key(|f| f.frame);
    report.failed.sort_by_key(|f| f.0);
    Ok(report)
}

async fn run_batch(
    net: &Arc<Libp2pNetwork>,
    job: &RenderJob,
    spec: Value,
    parsed: &Parsed,
    dims: &Mutex<Option<(u32, u32)>>,
    progress: &(dyn Fn(&str) + Send + Sync),
) -> Result<Vec<WrittenFrame>> {
    let mut excluded: Vec<String> = Vec::new();
    let mut last_error = anyhow::anyhow!("no attempt made");
    // Render time plus fetching a big .blend and the frames back.
    let timeout = parsed.frame_timeout() * parsed.frame_count() + Duration::from_secs(300);
    for attempt in 1..=job.attempts.max(1) {
        let auction = crate::JobAuction::new(WORKLOAD, spec.clone())
            .with_required_capabilities(required_capabilities(parsed))
            .with_allowed_bidders(job.allowed_bidders.clone())
            .with_excluded_bidders(excluded.clone())
            .with_max_price_usd(job.max_price_usd)
            .with_bid_window(job.bid_window)
            .with_deadline(timeout)
            .with_receipt_timeout(timeout);
        let outcome = match crate::run_auction(net.clone(), auction).await {
            Ok(outcome) => outcome,
            Err(error) => {
                last_error = error;
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        let worker = outcome.accepted_bid.bidder_peer_id.clone();
        match collect_batch(net, job, parsed, &outcome, dims).await {
            Ok(frames) => {
                for f in &frames {
                    progress(&format!(
                        "frame {} {}x{} from {} -> {}",
                        f.frame,
                        f.width,
                        f.height,
                        short(&worker),
                        f.path.display()
                    ));
                }
                return Ok(frames);
            }
            Err(error) => {
                progress(&format!(
                    "frames {}-{} attempt {attempt} on {} failed: {error:#}",
                    parsed.spec.frame_start,
                    parsed.spec.frame_end,
                    short(&worker)
                ));
                excluded.push(worker);
                last_error = error;
            }
        }
    }
    Err(last_error)
}

fn short(peer: &str) -> &str {
    &peer[peer.len().saturating_sub(8)..]
}

async fn collect_batch(
    net: &Arc<Libp2pNetwork>,
    job: &RenderJob,
    parsed: &Parsed,
    outcome: &crate::AuctionOutcome,
    dims: &Mutex<Option<(u32, u32)>>,
) -> Result<Vec<WrittenFrame>> {
    if outcome.receipt.status != JobStatus::Completed {
        bail!("worker reported the job failed");
    }
    let worker = &outcome.accepted_bid.bidder_peer_id;
    let hash = outcome
        .receipt
        .output_hash
        .as_deref()
        .context("worker returned no output hash")?;
    let _ = hash;
    let bytes = crate::gateway_util::fetch_result(
        net,
        outcome,
        MAX_RESULT_BYTES,
        Duration::from_secs(30),
    )
    .await
    .map_err(|e| anyhow::anyhow!("fetch result: {e:?}"))?;
    let result: Value = serde_json::from_slice(&bytes).context("result is not JSON")?;
    let metas = result_frames(parsed, &result)?;
    let budget =
        Duration::from_secs(120 + 30 * metas.iter().map(|m| m.pieces.len() as u64).sum::<u64>());
    let frames = fetch_frames(net, worker, &metas, &outcome.result_key, budget).await?;
    // Every frame of one render has the same size.
    {
        let mut known = dims.lock().unwrap_or_else(|e| e.into_inner());
        for (meta, _) in &frames {
            match *known {
                None => *known = Some((meta.width, meta.height)),
                Some((w, h)) if (w, h) != (meta.width, meta.height) => bail!(
                    "frame {} is {}x{}, the other frames are {w}x{h}",
                    meta.frame,
                    meta.width,
                    meta.height
                ),
                Some(_) => {}
            }
        }
    }
    let mut written = Vec::with_capacity(frames.len());
    for (meta, bytes) in frames {
        let path = frame_path(&job.out_dir, meta.frame, parsed.format);
        let tmp = path.with_extension("part");
        tokio::fs::write(&tmp, &bytes).await?;
        tokio::fs::rename(&tmp, &path).await?;
        written.push(WrittenFrame {
            frame: meta.frame,
            path,
            worker: worker.clone(),
            width: meta.width,
            height: meta.height,
        });
    }
    Ok(written)
}

/// `ffmpeg` arguments that stitch contiguous frames `first..` into an H.264
/// MP4 (PNG or JPEG frames only).
pub fn mp4_args(
    out_dir: &Path,
    first: u32,
    format: Format,
    fps: u32,
    mp4: &Path,
) -> Result<Vec<OsString>> {
    if format == Format::Exr {
        bail!("--mp4 needs PNG or JPEG frames, not EXR");
    }
    let pattern = out_dir.join(format!("frame_%04d.{}", format.extension()));
    Ok(vec![
        "-y".into(),
        "-loglevel".into(),
        "error".into(),
        "-framerate".into(),
        fps.clamp(1, 240).to_string().into(),
        "-start_number".into(),
        first.to_string().into(),
        "-i".into(),
        pattern.into_os_string(),
        "-c:v".into(),
        "libx264".into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        // yuv420p needs even dimensions.
        "-vf".into(),
        "pad=ceil(iw/2)*2:ceil(ih/2)*2".into(),
        mp4.as_os_str().to_owned(),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash() -> String {
        Hash::of(b"x").to_hex()
    }

    fn spec(extra: Value) -> Value {
        let mut base = json!({
            "v": 1,
            "input_hash": hash(),
            "input_size": 1000,
            "input_pieces": [hash()],
            "frame_start": 1,
            "frame_end": 1,
        });
        for (k, v) in extra.as_object().unwrap() {
            base[k] = v.clone();
        }
        base
    }

    #[test]
    fn spec_validation() {
        let p = parse_spec(&spec(json!({}))).unwrap();
        assert_eq!(p.engine, Engine::Cycles);
        assert_eq!(p.device, Device::Auto);
        assert_eq!(p.format, Format::Png);
        assert_eq!(p.frame_count(), 1);
        assert_eq!(
            p.frame_timeout(),
            Duration::from_secs(DEFAULT_FRAME_TIMEOUT_SECS)
        );

        // Engine aliases normalize.
        let p = parse_spec(&spec(json!({"engine":"blender_eevee_next","format":"jpg"}))).unwrap();
        assert_eq!(p.engine, Engine::Eevee);
        assert_eq!(p.spec.engine, "EEVEE");
        assert_eq!(p.spec.format, "JPEG");
        assert_eq!(
            parse_spec(&spec(json!({"format":"open_exr"})))
                .unwrap()
                .format,
            Format::Exr
        );

        let bad = |extra: Value| parse_spec(&spec(extra)).unwrap_err();
        assert!(bad(json!({"v":2})).contains("version"));
        assert!(bad(json!({"input_hash":"zz"})).contains("input_hash"));
        assert!(bad(json!({"input_size":0})).contains("input_size"));
        assert!(bad(json!({"input_size": MAX_INPUT_BYTES + 1})).contains("input_size"));
        assert!(bad(json!({"input_size": PIECE_BYTES + 1})).contains("pieces"));
        assert!(bad(json!({"input_pieces":["nope"]})).contains("hex"));
        assert!(bad(json!({"input_provider_peer_id":"nope"})).contains("peer"));
        assert!(bad(json!({"frame_start":5,"frame_end":4})).contains("before"));
        assert!(bad(json!({"frame_start":1,"frame_end":11})).contains("at most"));
        assert!(
            bad(json!({"frame_start":MAX_FRAME+1,"frame_end":MAX_FRAME+1})).contains("between")
        );
        assert!(bad(json!({"engine":"LUXCORE"})).contains("engine"));
        assert!(bad(json!({"engine":"EEVEE","device":"CPU"})).contains("GPU"));
        assert!(bad(json!({"device":"TPU"})).contains("device"));
        assert!(bad(json!({"format":"GIF"})).contains("format"));
        assert!(bad(json!({"resolution_pct":0})).contains("resolution"));
        assert!(bad(json!({"resolution_pct":201})).contains("resolution"));
        assert!(bad(json!({"samples":0})).contains("samples"));
        assert!(bad(json!({"samples":MAX_SAMPLES+1})).contains("samples"));
        assert!(bad(json!({"frame_timeout_secs":1})).contains("timeout"));
        // Unknown script-ish fields do not sneak through as numbers.
        assert!(bad(json!({"samples":"16; import os"})).contains("invalid"));

        // Two pieces for just over one piece of input.
        let two = parse_spec(&spec(json!({
            "input_size": PIECE_BYTES + 1,
            "input_pieces": [hash(), hash()],
            "frame_start": 0, "frame_end": 9,
            "input_provider_peer_id": c0mpute_net::PeerId::random().to_base58(),
        })))
        .unwrap();
        assert_eq!(two.frame_count(), 10);
        assert_eq!(MAX_PIECES, 128);
    }

    #[test]
    fn capabilities_per_spec() {
        let p = parse_spec(&spec(json!({}))).unwrap();
        assert_eq!(
            required_capabilities(&p),
            vec![ROLE_TAG, "c0mpute:render:engine:cycles"]
        );
        let p = parse_spec(&spec(json!({"device":"GPU"}))).unwrap();
        assert!(required_capabilities(&p).contains(&GPU_TAG.to_string()));
        let p = parse_spec(&spec(json!({"engine":"EEVEE"}))).unwrap();
        assert!(required_capabilities(&p).contains(&"c0mpute:render:engine:eevee".to_string()));
    }

    #[test]
    fn engine_ids_follow_the_blender_version() {
        assert_eq!(Engine::Eevee.blender_id((3, 4, 1)), "BLENDER_EEVEE");
        assert_eq!(Engine::Eevee.blender_id((4, 1, 0)), "BLENDER_EEVEE");
        assert_eq!(Engine::Eevee.blender_id((4, 2, 3)), "BLENDER_EEVEE_NEXT");
        assert_eq!(Engine::Eevee.blender_id((5, 0, 0)), "BLENDER_EEVEE");
        assert_eq!(Engine::Cycles.blender_id((4, 2, 0)), "CYCLES");
        assert_eq!(Engine::Workbench.blender_id((4, 2, 0)), "BLENDER_WORKBENCH");
    }

    #[test]
    fn frame_ranges() {
        assert_eq!(parse_frames("1-5", 100).unwrap(), vec![1, 2, 3, 4, 5]);
        assert_eq!(parse_frames("7", 100).unwrap(), vec![7]);
        assert_eq!(parse_frames("3, 1-2,2", 100).unwrap(), vec![1, 2, 3]);
        assert!(parse_frames("5-1", 100).is_err());
        assert!(parse_frames("a-3", 100).is_err());
        assert!(parse_frames("1,,2", 100).is_err());
        assert!(parse_frames("1-1000", 100).is_err());
        assert!(parse_frames("-3", 100).is_err());
        assert!(parse_frames(&format!("{}", MAX_FRAME + 1), 100).is_err());

        assert_eq!(
            batch_frames(&[1, 2, 3, 4, 5], 1),
            vec![(1, 1), (2, 2), (3, 3), (4, 4), (5, 5)]
        );
        assert_eq!(
            batch_frames(&[1, 2, 3, 4, 5], 2),
            vec![(1, 2), (3, 4), (5, 5)]
        );
        assert_eq!(
            batch_frames(&[1, 2, 5, 6, 7, 9], 3),
            vec![(1, 2), (5, 7), (9, 9)]
        );
        // Batches never exceed the per-job cap.
        let many: Vec<u32> = (0..25).collect();
        assert!(
            batch_frames(&many, 50)
                .iter()
                .all(|(a, b)| b - a < MAX_FRAMES_PER_JOB)
        );
    }

    #[test]
    fn command_construction() {
        let p = parse_spec(&spec(json!({
            "frame_start": 3, "frame_end": 5, "resolution_pct": 50, "samples": 16, "format": "EXR"
        })))
        .unwrap();
        let blend = Path::new("/cache/a b; rm -rf ~.blend");
        let args = blender_args(blend, Path::new("/jobs/x/out"), &p, (3, 4, 1), None, 4).unwrap();
        let args: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        // Never enable autoexec; disable it before the file loads.
        assert!(!args.iter().any(|a| a == "-y" || a == "--enable-autoexec"));
        let no_exec = args.iter().position(|a| a == "--disable-autoexec").unwrap();
        let file = args
            .iter()
            .position(|a| a == "/cache/a b; rm -rf ~.blend")
            .unwrap();
        assert!(no_exec < file, "{args:?}");
        assert_eq!(args[0], "--background");
        // Engine and output after the file; the range before --render-anim.
        let at = |flag: &str| args.iter().position(|a| a == flag).unwrap();
        assert!(at("--engine") > file && at("--python-expr") > file);
        assert_eq!(args[at("--engine") + 1], "CYCLES");
        assert_eq!(args[at("--render-output") + 1], "/jobs/x/out/frame_####");
        assert_eq!(args[at("--render-format") + 1], "OPEN_EXR");
        assert_eq!(args[at("--threads") + 1], "4");
        assert_eq!(args[at("--frame-start") + 1], "3");
        assert_eq!(args[at("--frame-end") + 1], "5");
        assert_eq!(args.last().unwrap(), "--render-anim");
        let expr = &args[at("--python-expr") + 1];
        assert!(expr.contains("resolution_percentage = 50"));
        assert!(expr.contains("s.cycles.samples = 16"));
        assert!(expr.contains("s.cycles.device = 'CPU'"));
        assert!(!args.contains(&"--cycles-device".to_string()));

        // A GPU job: device args go last, after `--`.
        let gpu = blender_args(blend, Path::new("/o"), &p, (4, 2, 0), Some("OPTIX"), 0).unwrap();
        let gpu: Vec<String> = gpu
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(&gpu[gpu.len() - 3..], ["--", "--cycles-device", "OPTIX"]);
        assert!(gpu.iter().any(|a| a.contains("s.cycles.device = 'GPU'")));
        assert_eq!(
            gpu[gpu.iter().position(|a| a == "--threads").unwrap() + 1],
            "1"
        );

        // EEVEE on 4.2 uses the _NEXT id and TAA samples.
        let e = parse_spec(&spec(json!({"engine":"EEVEE","samples":8}))).unwrap();
        let ea = blender_args(blend, Path::new("/o"), &e, (4, 2, 0), None, 2).unwrap();
        assert!(ea.iter().any(|a| a == "BLENDER_EEVEE_NEXT"));
        assert!(
            ea.iter()
                .any(|a| a.to_string_lossy().contains("taa_render_samples = 8"))
        );

        // '#' in the output dir would be read as a frame placeholder.
        assert!(blender_args(blend, Path::new("/o/#1"), &p, (3, 4, 1), None, 1).is_err());
    }

    #[test]
    fn output_names() {
        assert_eq!(frame_number("frame_0007.png", "png"), Some(7));
        assert_eq!(frame_number("frame_12345.exr", "exr"), Some(12345));
        assert_eq!(frame_number("frame_0007.jpg", "png"), None);
        assert_eq!(frame_number("frame_.png", "png"), None);
        assert_eq!(frame_number("frame_00a1.png", "png"), None);
        assert_eq!(frame_number("blender.log", "png"), None);
        assert_eq!(
            frame_path(Path::new("/o"), 7, Format::Jpeg),
            PathBuf::from("/o/frame_0007.jpg")
        );
        let mp4 = mp4_args(Path::new("/o"), 3, Format::Png, 24, Path::new("/o/a.mp4")).unwrap();
        let mp4: Vec<String> = mp4
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(mp4.windows(2).any(|w| w == ["-start_number", "3"]));
        assert!(mp4.contains(&"/o/frame_%04d.png".to_string()));
        assert!(mp4_args(Path::new("/o"), 1, Format::Exr, 24, Path::new("/o/a.mp4")).is_err());
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        b.extend_from_slice(&w.to_be_bytes());
        b.extend_from_slice(&h.to_be_bytes());
        b.extend_from_slice(&[8, 6, 0, 0, 0]);
        b
    }

    fn jpeg(w: u16, h: u16) -> Vec<u8> {
        let mut b = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, b'J', b'F'];
        b.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
        b.extend_from_slice(&h.to_be_bytes());
        b.extend_from_slice(&w.to_be_bytes());
        b.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
        b
    }

    fn exr(w: i32, h: i32) -> Vec<u8> {
        let mut b = vec![0x76, 0x2f, 0x31, 0x01, 2, 0, 0, 0];
        b.extend_from_slice(b"compression\0compression\0");
        b.extend_from_slice(&1i32.to_le_bytes());
        b.push(0);
        b.extend_from_slice(b"dataWindow\0box2i\0");
        b.extend_from_slice(&16i32.to_le_bytes());
        for v in [0i32, 0, w - 1, h - 1] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.push(0);
        b
    }

    #[test]
    fn image_headers() {
        assert_eq!(
            inspect_image(&png(64, 48)),
            Some(ImageInfo {
                format: Format::Png,
                width: 64,
                height: 48
            })
        );
        assert_eq!(
            inspect_image(&jpeg(1920, 1080)),
            Some(ImageInfo {
                format: Format::Jpeg,
                width: 1920,
                height: 1080
            })
        );
        assert_eq!(
            inspect_image(&exr(320, 240)),
            Some(ImageInfo {
                format: Format::Exr,
                width: 320,
                height: 240
            })
        );
        assert_eq!(inspect_image(&png(0, 48)), None);
        assert_eq!(inspect_image(&png(64, 48)[..20]), None);
        assert_eq!(inspect_image(&jpeg(10, 10)[..12]), None);
        assert_eq!(inspect_image(&exr(10, 10)[..20]), None);
        assert_eq!(inspect_image(b"GIF89a......................"), None);
        assert_eq!(inspect_image(b""), None);
    }

    #[test]
    fn result_and_frame_validation() {
        let p = parse_spec(&spec(json!({"frame_start": 4, "frame_end": 5}))).unwrap();
        let img = png(64, 48);
        let meta = |frame: u32| FrameMeta {
            frame,
            format: "PNG".into(),
            width: 64,
            height: 48,
            size: img.len() as u64,
            hash: Hash::of(&img).to_hex(),
            pieces: vec![Hash::of(&img).to_hex()],
        };
        let good = json!({"v":1,"ok":true,"frames":[meta(4), meta(5)]});
        assert!(valid_result(&p, &good));
        assert!(!valid_result(
            &p,
            &json!({"v":1,"ok":true,"frames":[meta(4)]})
        ));
        assert!(!valid_result(
            &p,
            &json!({"v":1,"ok":true,"frames":[meta(5), meta(4)]})
        ));
        assert!(!valid_result(
            &p,
            &json!({"v":1,"ok":false,"frames":[meta(4), meta(5)]})
        ));
        assert!(!valid_result(
            &p,
            &json!({"v":2,"ok":true,"frames":[meta(4), meta(5)]})
        ));
        let mut jpeg_meta = meta(5);
        jpeg_meta.format = "JPEG".into();
        assert!(!valid_result(
            &p,
            &json!({"v":1,"ok":true,"frames":[meta(4), jpeg_meta]})
        ));
        let mut no_pieces = meta(5);
        no_pieces.pieces.clear();
        assert!(!valid_result(
            &p,
            &json!({"v":1,"ok":true,"frames":[meta(4), no_pieces]})
        ));
        let mut empty = meta(5);
        empty.size = 0;
        assert!(!valid_result(
            &p,
            &json!({"v":1,"ok":true,"frames":[meta(4), empty]})
        ));

        assert_eq!(verify_frame(&meta(4), &img).unwrap().width, 64);
        let mut tampered = img.clone();
        tampered[16] = 1;
        assert!(
            verify_frame(&meta(4), &tampered)
                .unwrap_err()
                .to_string()
                .contains("hash")
        );
        // Honest hash, lying dimensions.
        let mut lying = meta(4);
        lying.width = 128;
        assert!(
            verify_frame(&lying, &img)
                .unwrap_err()
                .to_string()
                .contains("header")
        );
        let gif = b"GIF89a".to_vec();
        let mut gif_meta = meta(4);
        gif_meta.size = gif.len() as u64;
        gif_meta.hash = Hash::of(&gif).to_hex();
        assert!(verify_frame(&gif_meta, &gif).is_err());
        assert!(verify_frame(&meta(4), &img[..10]).is_err());
    }

    #[test]
    fn probe_output_parsing() {
        let (line, v) =
            parse_version("Color management: x\nBlender 4.2.3 LTS\n\tbuild date: 2024\n").unwrap();
        assert_eq!(line, "Blender 4.2.3 LTS");
        assert_eq!(v, (4, 2, 3));
        assert_eq!(parse_version("Blender 3.4.1\n").unwrap().1, (3, 4, 1));
        assert_eq!(parse_version("Blender 5.0\n").unwrap().1, (5, 0, 0));
        assert!(parse_version("command not found").is_none());
        assert_eq!(
            parse_gpu_probe("noise\nC0DEV:CUDA\nC0DEV:OPTIX\n"),
            vec!["OPTIX", "CUDA"]
        );
        assert!(parse_gpu_probe("Blender quit\n").is_empty());
    }

    #[test]
    fn serve_gate_and_tags() {
        let cpu = Runtime {
            version_line: Some("Blender 3.4.1".into()),
            version: (3, 4, 1),
            gpu_backends: vec![],
            sandbox: None,
            sandbox_mode: SandboxMode::Auto,
        };
        let gpu = Runtime {
            gpu_backends: vec!["CUDA".into()],
            ..cpu.clone()
        };
        let p = |extra: Value| parse_spec(&spec(extra)).unwrap();
        assert!(cpu.can_serve(&p(json!({}))));
        assert!(cpu.can_serve(&p(json!({"device":"CPU"}))));
        assert!(!cpu.can_serve(&p(json!({"device":"GPU"}))));
        assert!(!cpu.can_serve(&p(json!({"engine":"EEVEE"}))));
        assert!(gpu.can_serve(&p(json!({"engine":"EEVEE"}))));
        assert!(!Runtime::default().can_serve(&p(json!({}))));
        assert_eq!(cpu.cycles_device(&p(json!({}))), None);
        assert_eq!(gpu.cycles_device(&p(json!({}))).as_deref(), Some("CUDA"));
        assert_eq!(gpu.cycles_device(&p(json!({"device":"CPU"}))), None);
        assert_eq!(cpu.tags(), vec![ROLE_TAG, "c0mpute:render:engine:cycles"]);
        let tags = gpu.tags();
        assert!(tags.contains(&GPU_TAG.to_string()));
        assert!(tags.contains(&"c0mpute:render:device:cuda".to_string()));
        assert!(tags.contains(&"c0mpute:render:engine:eevee".to_string()));
        assert!(Runtime::default().tags().is_empty());
    }

    #[test]
    fn sandbox_setting_parses_and_a_typo_means_auto() {
        use crate::config::Config;
        // A config written before the section existed, and the default.
        let cfg: Config = toml::from_str("roles = []\n").unwrap();
        assert_eq!(cfg.render.mode_with(None), SandboxMode::Auto);
        assert_eq!(Config::default().render.sandbox, "auto");
        let mode = |text: &str| toml::from_str::<Config>(text).unwrap().render.mode_with(None);
        assert_eq!(mode("[render]\nsandbox = \"off\"\n"), SandboxMode::Off);
        assert_eq!(mode("[render]\nsandbox = \"require\"\n"), SandboxMode::Require);
        assert_eq!(mode("[render]\nsandbox = \" Require \"\n"), SandboxMode::Require);
        // A value that is none of the three never stops the node: it is auto.
        assert_eq!(mode("[render]\nsandbox = \"always\"\n"), SandboxMode::Auto);
        // The environment wins when it is set to something.
        let cfg: Config = toml::from_str("[render]\nsandbox = \"require\"\n").unwrap();
        assert_eq!(cfg.render.mode_with(Some("off".into())), SandboxMode::Off);
        assert_eq!(cfg.render.mode_with(Some("  ".into())), SandboxMode::Require);
    }

    fn sandbox(cycles_gpu: bool, raster: bool) -> Sandbox {
        Sandbox {
            bwrap: "/usr/bin/bwrap".into(),
            blender: "/home/op/blender-4.2/blender".into(),
            mounts: vec![
                Mount::RoBind("/usr".into()),
                Mount::Symlink { target: "usr/bin".into(), link: "/bin".into() },
                Mount::RoBind("/etc/ld.so.cache".into()),
                Mount::RoBind("/sys".into()),
                Mount::RoBind("/home/op/blender-4.2".into()),
                Mount::DevBind("/dev/dri".into()),
                Mount::DevBind("/dev/nvidia0".into()),
            ],
            home: Some("/home/op".into()),
            as_root: false,
            cycles_gpu,
            raster,
        }
    }

    #[test]
    fn bwrap_argv_shows_the_system_read_only_and_only_the_job_writable() {
        let job = Path::new("/home/op/.local/share/c0mpute/cache/jobs/j1");
        let blend = job.join("input.blend");
        let a: Vec<String> = bwrap_args(&sandbox(true, true), job, Some(&blend)).iter().map(|x| x.to_string_lossy().into_owned()).collect();
        let at = |w: &[&str]| a.windows(w.len()).position(|x| x == w).unwrap_or_else(|| panic!("{w:?} not in {a:?}"));
        for flag in ["--unshare-net", "--unshare-pid", "--unshare-ipc", "--unshare-uts", "--die-with-parent", "--new-session"] {
            assert!(a.iter().any(|x| x == flag), "{flag}");
        }
        at(&["--proc", "/proc"]);
        at(&["--ro-bind", "/usr", "/usr"]);
        at(&["--symlink", "usr/bin", "/bin"]);
        at(&["--ro-bind", "/sys", "/sys"]);
        at(&["--dev-bind", "/dev/nvidia0", "/dev/nvidia0"]);
        // /dev is made fresh before the GPU nodes are put back in it.
        assert!(at(&["--dev", "/dev"]) < at(&["--dev-bind", "/dev/dri", "/dev/dri"]));
        // The home is an empty tmpfs; Blender's own directory and the job
        // directory, both under it, are mounted on top afterwards.
        let home = at(&["--tmpfs", "/home/op"]);
        assert!(at(&["--tmpfs", "/tmp"]) < home);
        assert!(home < at(&["--ro-bind", "/home/op/blender-4.2", "/home/op/blender-4.2"]));
        let job_s = job.to_str().unwrap();
        assert!(home < at(&["--bind", job_s, job_s]));
        // The input (a hard link to the cached copy) is read-only inside it.
        let blend_s = blend.to_str().unwrap();
        assert!(at(&["--bind", job_s, job_s]) < at(&["--ro-bind", blend_s, blend_s]));
        at(&["--setenv", "HOME", "/home/op"]);
        at(&["--chdir", job_s]);
        assert_eq!(a.last().unwrap(), "--");
        // Exactly one writable bind of the host, and it is the job directory.
        assert_eq!(a.iter().filter(|x| *x == "--bind").count(), 1);
        assert!(!a.iter().any(|x| x == "--share-net" || x == "--dev-bind-try" || x == "/home" || x == "/root" || x == "/etc"));
        // An unprivileged worker gets no capabilities from bwrap; root must drop them.
        assert!(!a.iter().any(|x| x == "--cap-drop"));
        let root = Sandbox { as_root: true, home: None, ..sandbox(false, false) };
        let r: Vec<String> = bwrap_args(&root, Path::new("/var/lib/c0mpute/jobs/j1"), None).iter().map(|x| x.to_string_lossy().into_owned()).collect();
        assert!(r.windows(2).any(|w| w == ["--cap-drop", "ALL"]));
        assert!(r.windows(3).any(|w| w == ["--setenv", "HOME", "/tmp"]), "no home: HOME is the private /tmp");
    }

    #[test]
    fn only_install_directories_that_do_not_hold_the_home_are_shown() {
        let home = Some(Path::new("/home/op"));
        assert!(shareable(Path::new("/opt/blender-4.2"), home));
        assert!(shareable(Path::new("/home/op/blender-4.2"), home));
        assert!(!shareable(Path::new("/home/op"), home), "the home itself");
        assert!(!shareable(Path::new("/home"), home));
        assert!(!shareable(Path::new("/opt"), home), "a top-level directory");
        assert!(!shareable(Path::new("/"), None));
        assert!(!shareable(Path::new("blender"), None));
        let mounts = vec![Mount::RoBind("/usr".into()), Mount::DevBind("/dev/dri".into())];
        assert!(covered(&mounts, Path::new("/usr/bin")));
        assert!(!covered(&mounts, Path::new("/usrlocal")));
        assert!(!covered(&mounts, Path::new("/dev/dri/card0")), "a device bind is not a read-only view");
    }

    #[test]
    fn require_serves_only_what_the_sandbox_was_seen_to_do() {
        let p = |extra: Value| parse_spec(&spec(extra)).unwrap();
        let base = Runtime {
            version_line: Some("Blender 4.2.3 LTS".into()),
            version: (4, 2, 3),
            gpu_backends: vec!["OPTIX".into()],
            sandbox: None,
            sandbox_mode: SandboxMode::Require,
        };
        // No working sandbox: no role, no tags, no bids.
        assert!(!base.ready());
        assert!(base.tags().is_empty());
        assert!(!base.can_serve(&p(json!({"device":"CPU"}))));
        // A sandbox that starts Blender but cannot reach the GPU: CPU Cycles only.
        let cpu_only = Runtime { sandbox: Some(sandbox(false, false)), ..base.clone() };
        assert!(cpu_only.ready());
        assert!(cpu_only.can_serve(&p(json!({}))));
        assert_eq!(cpu_only.cycles_device(&p(json!({}))), None, "AUTO renders on the CPU inside the sandbox");
        assert!(!cpu_only.can_serve(&p(json!({"device":"GPU"}))));
        assert!(!cpu_only.can_serve(&p(json!({"engine":"EEVEE"}))));
        assert_eq!(cpu_only.tags(), vec![ROLE_TAG, "c0mpute:render:engine:cycles"]);
        assert!(cpu_only.sandbox_for(&p(json!({}))).is_some());
        // A sandbox that reaches the GPU serves everything, inside it.
        let full = Runtime { sandbox: Some(sandbox(true, true)), ..base.clone() };
        assert!(full.can_serve(&p(json!({"device":"GPU"}))) && full.can_serve(&p(json!({"engine":"EEVEE"}))));
        assert_eq!(full.cycles_device(&p(json!({}))).as_deref(), Some("OPTIX"));
        assert!(full.sandbox_for(&p(json!({"engine":"EEVEE"}))).is_some());
        assert!(full.tags().contains(&GPU_TAG.to_string()));

        // "auto" keeps today's behaviour: every job runs, and a GPU job the
        // sandbox cannot serve runs outside it.
        let auto = Runtime { sandbox_mode: SandboxMode::Auto, ..cpu_only.clone() };
        assert!(auto.can_serve(&p(json!({"device":"GPU"}))) && auto.can_serve(&p(json!({"engine":"EEVEE"}))));
        assert_eq!(auto.cycles_device(&p(json!({}))).as_deref(), Some("OPTIX"));
        assert!(auto.sandbox_for(&p(json!({}))).is_none(), "GPU Cycles, sandbox without the GPU");
        assert!(auto.sandbox_for(&p(json!({"engine":"EEVEE"}))).is_none());
        assert!(auto.sandbox_for(&p(json!({"device":"CPU"}))).is_some());
        let none = Runtime { sandbox: None, ..auto.clone() };
        assert!(none.ready() && none.sandbox_for(&p(json!({"device":"CPU"}))).is_none());
    }

    #[test]
    fn no_buyer_text_reaches_the_python_blender_runs() {
        // Everything a buyer chooses, at its most hostile: the file's path
        // (its hash names it in practice) and every spec field.
        let p = parse_spec(&spec(json!({
            "frame_start": MAX_FRAME - 1, "frame_end": MAX_FRAME, "resolution_pct": MAX_RESOLUTION_PCT,
            "samples": MAX_SAMPLES, "format": "jpg", "engine": "cycles", "device": "gpu", "frame_timeout_secs": 10,
        })))
        .unwrap();
        let blend = Path::new("/cache/'); import os; os.system('id') #.blend");
        let out = Path::new("/jobs/\"]); __import__('os')/out");
        let args: Vec<String> = blender_args(blend, out, &p, (4, 2, 0), Some("CUDA"), 4)
            .unwrap()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let exprs: Vec<&String> = args.iter().zip(args.iter().skip(1)).filter(|(flag, _)| *flag == "--python-expr").map(|(_, e)| e).collect();
        assert_eq!(exprs.len(), 2, "the settings expression and the sanitiser");
        // The sanitiser is the constant, byte for byte, whatever the job is.
        assert_eq!(exprs[1], SANITIZE_EXPR);
        let other = parse_spec(&spec(json!({"engine": "EEVEE", "samples": 1}))).unwrap();
        let other_args = blender_args(Path::new("/x.blend"), Path::new("/o"), &other, (3, 4, 1), None, 1).unwrap();
        assert!(other_args.iter().any(|a| a.to_str() == Some(SANITIZE_EXPR)));
        // The settings expression holds the validated numbers and fixed words only.
        for needle in ["os.system", "__import__", "/cache", "/jobs", ".blend", "CUDA"] {
            assert!(!exprs[0].contains(needle), "{needle} in {}", exprs[0]);
        }
        assert!(exprs[0].contains(&format!("s.cycles.samples = {MAX_SAMPLES}")));
        // Both run before the render and after the file is loaded.
        let at = |needle: &str| args.iter().position(|a| a == needle).unwrap();
        assert!(at(SANITIZE_EXPR) > at(blend.to_str().unwrap()) && at(SANITIZE_EXPR) < at("--render-anim"));
        // The directories reach the script through the environment, and a
        // script that cannot finish stops Blender instead of rendering.
        assert!(SANITIZE_EXPR.contains(&format!("os.environ.get('{JOB_DIR_ENV}')")));
        assert!(SANITIZE_EXPR.contains(&format!("os.environ.get('{OUT_DIR_ENV}')")));
        assert!(SANITIZE_EXPR.contains(&format!("os._exit({SANITIZE_EXIT})")));
        // Python needs its indentation (a `\` line continuation in a Rust
        // string would strip it), and the script has no format placeholders.
        assert!(SANITIZE_EXPR.contains("\n    D = bpy.data\n") && SANITIZE_EXPR.contains("\n        try:\n"));
        assert!(!SANITIZE_EXPR.contains("{}") && !SANITIZE_EXPR.contains("{0}"));
        for covered in ["D.libraries", "D.images", "'movieclips'", "'sounds'", "'volumes'", "'cache_files'", "D.fonts", "OUTPUT_FILE", "sequence_editor", "shading_system", "blend_paths"] {
            assert!(SANITIZE_EXPR.contains(covered), "{covered}");
        }
    }

    #[tokio::test]
    async fn the_blend_is_staged_into_the_job_directory() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("render-inputs");
        let job = dir.path().join("jobs").join("j1");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::create_dir_all(&job).unwrap();
        std::fs::write(cache.join("abc.blend"), b"BLENDER").unwrap();
        std::fs::write(cache.join("other-buyer.blend"), b"BLENDER2").unwrap();
        let staged = stage_input(&cache.join("abc.blend"), &job).await.unwrap();
        assert_eq!(staged, job.join("input.blend"));
        assert_eq!(std::fs::read(&staged).unwrap(), b"BLENDER");
        // Its directory holds this job's file only, not the other scenes.
        assert_eq!(std::fs::read_dir(&job).unwrap().count(), 1);
        // The cached copy outlives the job directory.
        std::fs::remove_dir_all(&job).unwrap();
        assert!(cache.join("abc.blend").exists());
        // An input already in the job directory (an opened sealed .blend) stays where it is.
        std::fs::create_dir_all(&job).unwrap();
        std::fs::write(job.join("input.blend"), b"PLAIN").unwrap();
        let same = stage_input(&job.join("input.blend"), &job).await.unwrap();
        assert_eq!(same, job.join("input.blend"));
        assert_eq!(std::fs::read(&same).unwrap(), b"PLAIN");
    }

    /// Runs only where `blender` is installed; elsewhere it says so and
    /// passes. Builds a tiny scene with Blender itself and renders two frames.
    #[tokio::test]
    async fn blender_renders_a_generated_scene() {
        let rt = tokio::task::spawn_blocking(runtime).await.unwrap();
        if !rt.ready() {
            eprintln!("skipping: blender is not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let blend = dir.path().join("scene.blend");
        make_test_scene(&blend);
        let p = parse_spec(&spec(json!({
            "input_size": std::fs::metadata(&blend).unwrap().len(),
            "input_pieces": vec![hash(); piece_count(std::fs::metadata(&blend).unwrap().len())],
            "frame_start": 1, "frame_end": 2, "samples": 4, "device": "CPU",
        })))
        .unwrap();
        let frames = render(&blend, &p, &dir.path().join("job")).await.unwrap();
        assert_eq!(frames.len(), 2);
        for (f, n) in frames.iter().zip(1..) {
            assert_eq!(f.frame, n);
            assert_eq!(
                (f.info.format, f.info.width, f.info.height),
                (Format::Png, 64, 48)
            );
        }
        // resolution_pct halves it.
        let half = parse_spec(&spec(json!({
            "input_size": std::fs::metadata(&blend).unwrap().len(),
            "input_pieces": vec![hash(); piece_count(std::fs::metadata(&blend).unwrap().len())],
            "resolution_pct": 50, "samples": 2, "format": "JPEG", "device": "CPU",
        })))
        .unwrap();
        let frames = render(&blend, &half, &dir.path().join("job2"))
            .await
            .unwrap();
        assert_eq!(
            (frames[0].info.format, frames[0].info.width),
            (Format::Jpeg, 32)
        );
    }

    /// Runs only where `blender` is installed. A scene that names an image
    /// outside the job directory by absolute path still renders, with that
    /// reference cut before the render starts.
    #[tokio::test]
    async fn a_scene_naming_a_host_file_renders_without_it() {
        let rt = tokio::task::spawn_blocking(runtime).await.unwrap();
        if !rt.ready() {
            eprintln!("skipping: blender is not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let (blend, secret) = (dir.path().join("scene.blend"), outside.join("secret.png"));
        // Saved once before the image is added: Blender turns absolute paths
        // into relative ones on a file's first save, and this scene must
        // keep naming the image by its absolute path.
        let expr = format!(
            "import bpy\ns = bpy.context.scene\ns.render.engine = 'CYCLES'\n\
             s.render.resolution_x = 64\ns.render.resolution_y = 48\n\
             s.render.resolution_percentage = 100\ns.cycles.samples = 2\n\
             bpy.ops.wm.save_as_mainfile(filepath={blend:?})\n\
             img = bpy.data.images.new('secret', 8, 8)\nimg.filepath_raw = {secret:?}\n\
             img.file_format = 'PNG'\nimg.save()\nbpy.data.images.remove(img)\n\
             ext = bpy.data.images.load({secret:?})\next.filepath = {secret:?}\n\
             ext.use_fake_user = True\ns.render.filepath = {out:?}\n\
             bpy.ops.wm.save_mainfile()\n",
            secret = secret.to_string_lossy(),
            out = outside.join("render_").to_string_lossy(),
            blend = blend.to_string_lossy(),
        );
        let status = std::process::Command::new("blender")
            .args(["--background", "--factory-startup", "--python-expr", &expr])
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success() && blend.exists() && secret.exists(), "could not build the test scene");
        let size = std::fs::metadata(&blend).unwrap().len();
        let p = parse_spec(&spec(json!({
            "input_size": size, "input_pieces": vec![hash(); piece_count(size)],
            "frame_start": 1, "frame_end": 1, "samples": 2, "device": "CPU",
        })))
        .unwrap();
        let job = dir.path().join("job");
        let frames = render(&blend, &p, &job).await.unwrap();
        assert_eq!(frames.len(), 1);
        let log = std::fs::read_to_string(job.join("blender.log")).unwrap();
        assert!(log.contains("scene sanitised: 1 outside references cut"), "{log}");
        // Nothing was written beside the file the scene pointed its output at.
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 1);
    }

    /// Save the default scene (cube, camera, light) at 64x48 to `path`.
    pub(crate) fn make_test_scene(path: &Path) {
        let expr = format!(
            "import bpy\ns = bpy.context.scene\ns.render.engine = 'CYCLES'\n\
             s.render.resolution_x = 64\ns.render.resolution_y = 48\n\
             s.render.resolution_percentage = 100\ns.frame_start = 1\ns.frame_end = 3\n\
             s.cycles.samples = 4\nbpy.ops.wm.save_as_mainfile(filepath={:?})\n",
            path.to_string_lossy()
        );
        let status = std::process::Command::new("blender")
            .args(["--background", "--factory-startup", "--python-expr", &expr])
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(
            status.success() && path.exists(),
            "could not build the test scene"
        );
    }
}
