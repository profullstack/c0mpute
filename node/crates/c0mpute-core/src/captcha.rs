//! `captcha.solve`: read a captcha on a worker's CPU.
//!
//! The engine is ddddocr (MIT, ONNX Runtime, CPU), whose models ship inside
//! its wheel. Like embeddings, the node provisions a per-user Python
//! virtualenv under `<data_dir>/captcha` in the background on worker start and
//! advertises `c0mpute:role:captcha` only once a warmup run answers.
//!
//! Kinds a worker solves:
//! - `image`: the text in a picture (letters/digits), optionally limited to a
//!   charset, with a confidence.
//! - `slider`: where a puzzle piece fits in its background (centre x, y).
//! - `gap`: where a backdrop with a hole differs from the clean backdrop.
//! - `detect`: bounding boxes of the characters in a click-the-text captcha.
//!
//! Two more kinds never reach a worker's ddddocr: `audio` captchas run the
//! existing `stt.transcribe` workload and [`normalize_spoken`] turns the
//! transcript into the answer, and `math` questions ("What is 3 + 4?") are
//! answered locally by [`solve_math`]. Token captchas (reCAPTCHA, hCaptcha,
//! Turnstile) are not solved: they need a human-like browser session, not a
//! model, and [`unsupported_kind_message`] says so.
//!
//! Images are small, so they travel inline in the spec (base64), capped at
//! [`MAX_IMAGE_BYTES`] each; a spec over the inline gossip limit is stored and
//! fetched by hash like an embeddings spec.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use c0mpute_net::Libp2pNetwork;
use c0mpute_net::topics::JobStatus;
use c0mpute_store::ChunkStore;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::runner::{AcceptedJob, WorkerLoad, publish_receipt};

pub const WORKLOAD: &str = "captcha.solve";
/// Capability tag: this node has a working ddddocr runtime.
pub const ROLE_TAG: &str = "c0mpute:role:captcha";
/// Largest image (decoded) a job may carry. Captchas are a few KiB; two
/// images at this cap stay under [`crate::workflow::MAX_SPEC_BYTES`] as base64.
pub const MAX_IMAGE_BYTES: usize = 256 * 1024;
/// Largest audio captcha the gateway accepts (they are a few seconds long).
pub const MAX_AUDIO_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_RESULT_BYTES: usize = 64 * 1024;
pub const MAX_CHARSET_CHARS: usize = 256;
pub const MAX_QUESTION_CHARS: usize = 256;
/// Kinds a worker's ddddocr solves.
pub const WORKER_KINDS: &[&str] = &["image", "slider", "gap", "detect"];
/// Kinds the gateway/CLI solves without ddddocr.
pub const LOCAL_KINDS: &[&str] = &["audio", "math"];
const DDDDOCR: &str = "ddddocr==1.6.1";
const JOB_TIMEOUT: Duration = Duration::from_secs(60);
const INSTALL_RETRY_BACKOFF_SECS: u64 = 6 * 60 * 60;
const UV_PYTHON: &str = "3.12";
const PY_MIN: (u32, u32) = (3, 9);
const PYTHONS: &[&str] = &[
    "python3.13",
    "python3.12",
    "python3.11",
    "python3.10",
    "python3.9",
];
const WORKER: &str = include_str!("captcha_worker.py");

/// Named charsets a buyer may ask for instead of spelling one out.
pub fn charset_preset(name: &str) -> Option<&'static str> {
    Some(match name {
        "digits" | "numeric" => "0123456789",
        "lower" => "abcdefghijklmnopqrstuvwxyz",
        "upper" => "ABCDEFGHIJKLMNOPQRSTUVWXYZ",
        "letters" | "alpha" => "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ",
        "alnum" | "alphanumeric" => {
            "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ"
        }
        "lower_digits" => "0123456789abcdefghijklmnopqrstuvwxyz",
        "upper_digits" => "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ",
        _ => return None,
    })
}

/// Resolve a charset (preset name or literal characters) to the literal set
/// ddddocr is restricted to: deduplicated, printable, at most
/// [`MAX_CHARSET_CHARS`].
pub fn resolve_charset(raw: &str) -> std::result::Result<String, String> {
    let literal = charset_preset(raw).unwrap_or(raw);
    let mut out = String::new();
    for c in literal.chars() {
        if c.is_control() || c.is_whitespace() {
            return Err("charset may not hold whitespace or control characters".into());
        }
        if !out.contains(c) {
            out.push(c);
        }
    }
    if out.is_empty() {
        return Err("charset is empty".into());
    }
    if out.chars().count() > MAX_CHARSET_CHARS {
        return Err(format!(
            "charset holds more than {MAX_CHARSET_CHARS} characters"
        ));
    }
    Ok(out)
}

/// The message for a kind this plugin does not solve.
pub fn unsupported_kind_message(kind: &str) -> String {
    match kind {
        "recaptcha" | "recaptcha_v2" | "recaptcha_v3" | "hcaptcha" | "turnstile" | "funcaptcha"
        | "arkose" | "token" => format!(
            "{kind} is a token captcha: it is passed by a browser session, not read from a \
             picture, and c0mpute does not solve it. Supported: image, slider, gap, detect, \
             audio, math"
        ),
        _ => format!(
            "unknown captcha type {kind:?}; supported: image, slider, gap, detect, audio, math"
        ),
    }
}

/// What a buyer asks a worker for.
#[derive(Debug, Clone, Deserialize)]
pub struct CaptchaSpec {
    pub v: u32,
    pub kind: String,
    /// Base64 image: the captcha (`image`, `detect`), the puzzle piece
    /// (`slider`) or the backdrop with the gap (`gap`).
    pub image: String,
    /// Base64 image: the background (`slider`) or clean backdrop (`gap`).
    #[serde(default)]
    pub background: Option<String>,
    /// `image` only: preset name or literal characters.
    #[serde(default)]
    pub charset: Option<String>,
}

fn decode_image(field: &str, b64: &str) -> std::result::Result<Vec<u8>, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|_| format!("{field} is not valid base64"))?;
    if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "{field} must be between 1 byte and {} KiB",
            MAX_IMAGE_BYTES / 1024
        ));
    }
    if detect_image_format(&bytes).is_none() {
        return Err(format!(
            "{field} is not a PNG, JPEG, GIF, WebP or BMP image"
        ));
    }
    Ok(bytes)
}

/// Identify an image by its magic bytes.
pub fn detect_image_format(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else if bytes.starts_with(b"BM") {
        Some("bmp")
    } else {
        None
    }
}

/// Strip a `data:image/...;base64,` prefix if there is one.
pub fn strip_data_url(value: &str) -> &str {
    let v = value.trim();
    match v.strip_prefix("data:") {
        Some(rest) => rest.split_once(',').map_or(v, |(_, b64)| b64),
        None => v,
    }
}

pub fn parse_spec(spec: &Value) -> std::result::Result<CaptchaSpec, String> {
    let mut s: CaptchaSpec = serde_json::from_value(spec.clone())
        .map_err(|e| format!("invalid captcha.solve spec: {e}"))?;
    if s.v != 1 {
        return Err(format!("unsupported captcha.solve spec version {}", s.v));
    }
    if !WORKER_KINDS.contains(&s.kind.as_str()) {
        return Err(unsupported_kind_message(&s.kind));
    }
    decode_image("image", &s.image)?;
    match (s.kind.as_str(), s.background.as_deref()) {
        ("slider" | "gap", Some(bg)) => {
            decode_image("background", bg)?;
        }
        ("slider" | "gap", None) => {
            return Err(format!("{} needs a background image", s.kind));
        }
        (_, Some(_)) => return Err(format!("{} takes no background image", s.kind)),
        _ => {}
    }
    if let Some(raw) = s.charset.as_deref() {
        if s.kind != "image" {
            return Err("charset applies to image captchas only".into());
        }
        s.charset = Some(resolve_charset(raw)?);
    }
    let size = serde_json::to_vec(spec)
        .map(|b| b.len())
        .unwrap_or(usize::MAX);
    if size > crate::workflow::MAX_SPEC_BYTES {
        return Err(format!(
            "spec is over the {}-byte limit",
            crate::workflow::MAX_SPEC_BYTES
        ));
    }
    Ok(s)
}

fn number(value: &Value) -> bool {
    value.as_f64().is_some_and(f64::is_finite)
}

/// The buyer's (and the worker's own) check on a result for `kind`.
pub fn valid_result(kind: &str, value: &Value) -> bool {
    if !value.is_object()
        || value.get("v").and_then(Value::as_u64) != Some(1)
        || value.get("kind").and_then(Value::as_str) != Some(kind)
    {
        return false;
    }
    match kind {
        "image" | "audio" | "math" => value
            .get("text")
            .and_then(Value::as_str)
            .is_some_and(|t| t.chars().count() <= 256),
        "slider" | "gap" => {
            value.get("x").is_some_and(number) && value.get("y").is_some_and(number)
        }
        "detect" => value
            .get("boxes")
            .and_then(Value::as_array)
            .is_some_and(|boxes| {
                boxes.len() <= 256
                    && boxes.iter().all(|b| {
                        b.as_array()
                            .is_some_and(|b| b.len() == 4 && b.iter().all(number))
                    })
            }),
        _ => false,
    }
}

// ─── answers computed without a model ────────────────────────────────────

const NUMBER_WORDS: &[(&str, &str)] = &[
    ("zero", "0"),
    ("oh", "0"),
    ("one", "1"),
    ("won", "1"),
    ("two", "2"),
    ("to", "2"),
    ("too", "2"),
    ("three", "3"),
    ("four", "4"),
    ("for", "4"),
    ("five", "5"),
    ("six", "6"),
    ("seven", "7"),
    ("eight", "8"),
    ("ate", "8"),
    ("nine", "9"),
];

/// Turn an audio captcha's transcript into its answer: spoken digit words
/// become digits, NATO/spelled letters become letters, filler is dropped.
/// `"Seven, four, two. One"` -> `"7421"`; `"alpha bravo 3"` -> `"ab3"`.
pub fn normalize_spoken(transcript: &str) -> String {
    const NATO: &[(&str, char)] = &[
        ("alpha", 'a'),
        ("alfa", 'a'),
        ("bravo", 'b'),
        ("charlie", 'c'),
        ("delta", 'd'),
        ("echo", 'e'),
        ("foxtrot", 'f'),
        ("golf", 'g'),
        ("hotel", 'h'),
        ("india", 'i'),
        ("juliet", 'j'),
        ("juliett", 'j'),
        ("kilo", 'k'),
        ("lima", 'l'),
        ("mike", 'm'),
        ("november", 'n'),
        ("oscar", 'o'),
        ("papa", 'p'),
        ("quebec", 'q'),
        ("romeo", 'r'),
        ("sierra", 's'),
        ("tango", 't'),
        ("uniform", 'u'),
        ("victor", 'v'),
        ("whiskey", 'w'),
        ("xray", 'x'),
        ("x-ray", 'x'),
        ("yankee", 'y'),
        ("zulu", 'z'),
    ];
    const FILLER: &[&str] = &[
        "the",
        "is",
        "are",
        "code",
        "your",
        "please",
        "type",
        "enter",
        "following",
        "characters",
        "digits",
        "numbers",
        "letters",
        "and",
        "then",
        "a",
        "an",
    ];
    let lower = transcript.to_lowercase();
    let mut out = String::new();
    for word in lower.split(|c: char| !(c.is_alphanumeric() || c == '-')) {
        let word = word.trim_matches('-');
        if word.is_empty() {
            continue;
        }
        if let Some((_, d)) = NUMBER_WORDS.iter().find(|(w, _)| *w == word) {
            out.push_str(d);
        } else if let Some((_, c)) = NATO.iter().find(|(w, _)| *w == word) {
            out.push(*c);
        } else if FILLER.contains(&word) {
            continue;
        } else if word.chars().all(|c| c.is_ascii_alphanumeric()) && word.len() <= 2 {
            // Already a digit or letter ("7", "b", "42").
            out.push_str(word);
        } else if word.chars().all(|c| c.is_ascii_digit()) {
            out.push_str(word);
        }
    }
    out
}

/// Answer a text/math captcha such as "What is 3 + 4?", "12 minus five" or
/// "2 x 6 =". Integers only, left to right with * and / before + and -.
/// `None` when the question holds no arithmetic.
pub fn solve_math(question: &str) -> Option<String> {
    let lower = question.to_lowercase();
    let mut tokens: Vec<String> = Vec::new();
    let mut number = String::new();
    let flush = |number: &mut String, tokens: &mut Vec<String>| {
        if !number.is_empty() {
            tokens.push(std::mem::take(number));
        }
    };
    for word in lower.split_whitespace() {
        let word = word.trim_matches(|c: char| matches!(c, '?' | '=' | ',' | '.' | '!' | ':'));
        let op = match word {
            "plus" | "add" | "+" => Some("+"),
            "minus" | "subtract" | "less" | "-" | "−" => Some("-"),
            "times" | "multiplied" | "x" | "*" | "×" => Some("*"),
            "divided" | "over" | "/" | "÷" => Some("/"),
            _ => None,
        };
        if let Some(op) = op {
            flush(&mut number, &mut tokens);
            tokens.push(op.into());
            continue;
        }
        if let Some((_, d)) = NUMBER_WORDS.iter().find(|(w, _)| *w == word) {
            if matches!(word, "to" | "too" | "for" | "won" | "ate" | "oh") {
                continue; // too ambiguous inside a sentence
            }
            flush(&mut number, &mut tokens);
            tokens.push((*d).to_string());
            continue;
        }
        if let Some(n) = match word {
            "ten" => Some("10"),
            "eleven" => Some("11"),
            "twelve" => Some("12"),
            "twenty" => Some("20"),
            _ => None,
        } {
            flush(&mut number, &mut tokens);
            tokens.push(n.into());
            continue;
        }
        // Inline expressions like "3+4" or "12x3".
        for c in word.chars() {
            match c {
                '0'..='9' => number.push(c),
                '+' | '-' | '*' | '/' | 'x' | '×' | '÷' | '−' => {
                    let op = match c {
                        '+' => "+",
                        '-' | '−' => "-",
                        '/' | '÷' => "/",
                        _ => "*",
                    };
                    // An operator needs a number before it ("x" in a word
                    // like "box" is just a letter).
                    if number.is_empty() && tokens.last().is_none_or(|t| t.parse::<i64>().is_err())
                    {
                        continue;
                    }
                    flush(&mut number, &mut tokens);
                    tokens.push(op.to_string());
                }
                _ => flush(&mut number, &mut tokens),
            }
        }
        flush(&mut number, &mut tokens);
    }
    // Keep the longest number-op-number... run.
    let mut best: Vec<String> = Vec::new();
    let mut run: Vec<String> = Vec::new();
    for t in tokens {
        let is_num = t.parse::<i64>().is_ok();
        let expect_num = run.len().is_multiple_of(2);
        if is_num == expect_num {
            run.push(t);
        } else if is_num {
            run = vec![t];
        } else {
            if run.len() > best.len() {
                best = run.clone();
            }
            run.clear();
        }
        if !run.len().is_multiple_of(2) && run.len() > best.len() {
            best = run.clone();
        }
    }
    if best.len() < 3 {
        return None;
    }
    eval(&best).map(|n| n.to_string())
}

fn eval(tokens: &[String]) -> Option<i64> {
    // First pass: * and /.
    let mut terms: Vec<i64> = vec![tokens[0].parse().ok()?];
    let mut ops: Vec<&str> = Vec::new();
    let mut i = 1;
    while i + 1 < tokens.len() {
        let op = tokens[i].as_str();
        let n: i64 = tokens[i + 1].parse().ok()?;
        match op {
            "*" => *terms.last_mut()? = terms.last()?.checked_mul(n)?,
            "/" => {
                if n == 0 {
                    return None;
                }
                *terms.last_mut()? = terms.last()?.checked_div(n)?;
            }
            _ => {
                ops.push(op);
                terms.push(n);
            }
        }
        i += 2;
    }
    let mut total = terms[0];
    for (op, n) in ops.iter().zip(&terms[1..]) {
        total = match *op {
            "+" => total.checked_add(*n)?,
            "-" => total.checked_sub(*n)?,
            _ => return None,
        };
    }
    Some(total)
}

/// Apply a buyer's `case` request to a text answer.
pub fn apply_case(text: &str, case: Option<&str>) -> String {
    match case {
        Some("lower") => text.to_lowercase(),
        Some("upper") => text.to_uppercase(),
        _ => text.to_string(),
    }
}

// ─── runtime: paths and readiness ────────────────────────────────────────

pub fn captcha_root() -> PathBuf {
    crate::config::data_dir()
        .unwrap_or_else(|| PathBuf::from("./c0mpute-data"))
        .join("captcha")
}

fn venv_dir() -> PathBuf {
    captcha_root().join("venv")
}

pub fn python_path() -> PathBuf {
    crate::speech::venv_python(&venv_dir())
}

fn ready_marker() -> PathBuf {
    captcha_root().join(".ready")
}

fn retry_marker() -> PathBuf {
    captcha_root().join(".retry-after")
}

/// Cheap enough for every capability tick: two file checks.
pub fn ready() -> bool {
    python_path().is_file() && ready_marker().is_file()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn in_install_backoff() -> bool {
    std::fs::read_to_string(retry_marker())
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .is_some_and(|retry_after| now_secs() < retry_after)
}

/// Make sure ddddocr is installed, installing it if missing. Idempotent and
/// never fatal: a node that cannot provision just does not advertise
/// `captcha`.
pub async fn ensure(config: &crate::config::CaptchaConfig) -> bool {
    if ready() {
        return true;
    }
    if !config.auto_install_enabled() || in_install_backoff() {
        return false;
    }
    let free = crate::speech::free_bytes_for(&captcha_root());
    let required = config.min_free_disk_gb.saturating_mul(1024 * 1024 * 1024);
    if free < required {
        warn!(
            free_mb = free / 1024 / 1024,
            required_mb = required / 1024 / 1024,
            "skipping captcha install: not enough free disk"
        );
        return false;
    }
    info!("installing ddddocr so this node can serve captcha.solve");
    match setup().await {
        Ok(version) => {
            let _ = tokio::fs::remove_file(retry_marker()).await;
            info!(root = %captcha_root().display(), %version, "captcha solver ready");
            true
        }
        Err(error) => {
            let _ = tokio::fs::create_dir_all(captcha_root()).await;
            let retry_after = now_secs().saturating_add(INSTALL_RETRY_BACKOFF_SECS);
            let _ = tokio::fs::write(retry_marker(), retry_after.to_string()).await;
            warn!(
                err = %format!("{error:#}"),
                "could not install the captcha runtime; this node will not serve captcha.solve \
                 (opt out with C0MPUTE_CAPTCHA_AUTO=0)"
            );
            false
        }
    }
}

/// Install ddddocr and verify it loads. `c0mpute captcha setup`. Returns the
/// engine version.
pub async fn setup() -> Result<String> {
    let python = ensure_venv().await?;
    if !ready() {
        let mut install = Command::new(&python);
        install.args([
            "-m",
            "pip",
            "install",
            "--disable-pip-version-check",
            DDDDOCR,
        ]);
        install.env("TMPDIR", tmpdir().await?);
        crate::speech::run_checked(install, "install ddddocr").await?;
    }
    let out = run_worker("warmup", &json!({}), Duration::from_secs(5 * 60))
        .await
        .context("load ddddocr")?;
    let version = out
        .get("engine_version")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    tokio::fs::write(ready_marker(), format!("{DDDDOCR} {version}\n")).await?;
    Ok(version)
}

async fn tmpdir() -> Result<PathBuf> {
    // Not /tmp: a container's /tmp is often a tiny noexec tmpfs, and the
    // onnxruntime + opencv wheels are bigger than that.
    let dir = captcha_root().join("tmp");
    tokio::fs::create_dir_all(&dir).await?;
    Ok(dir)
}

fn interpreter() -> Option<std::ffi::OsString> {
    let fits = |p: &Path| crate::speech::python_version(p).is_some_and(|v| v >= PY_MIN);
    if let Some(python) = std::env::var_os("C0MPUTE_PYTHON")
        && fits(Path::new(&python))
    {
        return Some(python);
    }
    if fits(Path::new("python3")) {
        return Some("python3".into());
    }
    PYTHONS
        .iter()
        .find(|name| crate::speech::on_path(name) && fits(Path::new(name)))
        .map(|name| (*name).into())
}

async fn ensure_venv() -> Result<PathBuf> {
    let python = python_path();
    if python.is_file() && crate::speech::python_has_pip_at(&python).await {
        return Ok(python);
    }
    let dir = venv_dir();
    if dir.exists() {
        // A half-built venv: start over, and forget it was ever ready.
        tokio::fs::remove_dir_all(&dir).await?;
        let _ = tokio::fs::remove_file(ready_marker()).await;
    }
    tokio::fs::create_dir_all(captcha_root()).await?;
    if let Some(interp) = interpreter() {
        let mut command = Command::new(interp);
        command.args(["-m", "venv"]).arg(&dir);
        if crate::speech::run_checked(command, "create the captcha Python environment")
            .await
            .is_ok()
            && crate::speech::python_has_pip_at(&python).await
        {
            return Ok(python);
        }
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
    if let Some(uv) = crate::speech::find_uv() {
        let mut command = Command::new(uv);
        command
            .args(["venv", "--seed", "--python", UV_PYTHON])
            .arg(&dir);
        crate::speech::run_checked(command, "create the captcha Python environment with uv")
            .await?;
        return Ok(python);
    }
    bail!(
        "the captcha solver needs Python 3.9+ with venv support (Debian/Ubuntu: apt install \
         python3-venv), or uv (curl -LsSf https://astral.sh/uv/install.sh | sh), or \
         C0MPUTE_PYTHON pointing at one"
    )
}

async fn run_worker(action: &str, request: &Value, timeout: Duration) -> Result<Value> {
    let mut command = Command::new(python_path());
    command.arg("-c").arg(WORKER).arg(action);
    crate::resources::apply_job_limits(&mut command);
    command
        .env("TMPDIR", tmpdir().await?)
        .env(
            "C0MPUTE_JOB_CPU_THREADS",
            crate::resources::cpu_threads_per_job().to_string(),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .context("start the captcha runtime; run `c0mpute captcha setup`")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&serde_json::to_vec(request)?).await?;
    }
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .context("captcha runtime timed out")??;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "captcha runtime failed: {}",
            stderr.lines().last().unwrap_or("runtime error")
        );
    }
    if output.stdout.len() > MAX_RESULT_BYTES {
        bail!("captcha output is over the {MAX_RESULT_BYTES}-byte limit");
    }
    serde_json::from_slice(&output.stdout).context("captcha runtime printed invalid JSON")
}

/// Solve a parsed spec on this box (the runtime must be ready) and build the
/// result blob.
pub async fn solve(spec: &CaptchaSpec) -> Result<Value> {
    if !ready() {
        bail!("the captcha runtime is not installed on this node");
    }
    let started = Instant::now();
    let mut request = json!({ "kind": spec.kind, "image": strip_data_url(&spec.image) });
    if let Some(bg) = spec.background.as_deref() {
        request["background"] = json!(strip_data_url(bg));
    }
    if let Some(charset) = spec.charset.as_deref() {
        request["charset"] = json!(charset);
    }
    let mut result = run_worker("solve", &request, JOB_TIMEOUT).await?;
    let obj = result
        .as_object_mut()
        .context("captcha runtime printed a non-object")?;
    obj.insert("v".into(), json!(1));
    obj.insert("ok".into(), json!(true));
    obj.insert("engine".into(), json!("ddddocr"));
    obj.insert(
        "duration_ms".into(),
        json!(started.elapsed().as_millis() as u64),
    );
    if !valid_result(&spec.kind, &result) {
        bail!("captcha runtime answered the wrong shape for {}", spec.kind);
    }
    Ok(result)
}

// ─── worker runner ───────────────────────────────────────────────────────

pub fn spawn_captcha_runner(
    net: Arc<Libp2pNetwork>,
    store: ChunkStore,
    load: WorkerLoad,
) -> mpsc::Sender<AcceptedJob> {
    let (tx, mut rx) = mpsc::channel::<AcceptedJob>(32);
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let Ok(slot) = load.compute_slots().acquire_owned().await else {
                return;
            };
            let net = net.clone();
            let store = store.clone();
            let load = load.clone();
            tokio::spawn(async move {
                let _slot = slot;
                let started = Instant::now();
                let job_id = job.accept.job_id.clone();
                match crate::job_control::cancellable(&job.accept.job_id, run_captcha_job(&net, &store, &job)).await {
                    Ok(hash) => {
                        load.record_job(started.elapsed());
                        info!(%job_id, %hash, "captcha runner: job complete");
                        publish_receipt(&net, &job.accept, Some(hash), JobStatus::Completed).await;
                    }
                    Err(error) => {
                        warn!(%job_id, err = %error, "captcha runner: job failed");
                        publish_receipt(&net, &job.accept, None, JobStatus::Failed).await;
                    }
                }
            });
        }
    });
    tx
}

async fn run_captcha_job(
    net: &Libp2pNetwork,
    store: &ChunkStore,
    job: &AcceptedJob,
) -> Result<String> {
    let spec = crate::workflow::resolve_spec(net, job).await?;
    let spec = parse_spec(&spec).map_err(anyhow::Error::msg)?;
    let result = solve(&spec).await?;
    let bytes = serde_json::to_vec(&result)?;
    if bytes.len() > MAX_RESULT_BYTES {
        bail!("captcha result is over the {MAX_RESULT_BYTES}-byte limit");
    }
    crate::pieces::publish_result_bytes(net, store, &job.accept, &bytes)
        .await
        .context("publish captcha result")
}

#[cfg(test)]
mod tests {
    use super::*;

    // 1x1 transparent PNG.
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=";

    #[test]
    fn spec_validation() {
        let s = parse_spec(&json!({"v":1,"kind":"image","image":PNG})).unwrap();
        assert_eq!(s.kind, "image");
        let s = parse_spec(&json!({"v":1,"kind":"image","image":PNG,"charset":"digits"})).unwrap();
        assert_eq!(s.charset.as_deref(), Some("0123456789"));
        assert!(
            parse_spec(&json!({"v":2,"kind":"image","image":PNG}))
                .unwrap_err()
                .contains("version")
        );
        assert!(
            parse_spec(&json!({"v":1,"kind":"recaptcha","image":PNG}))
                .unwrap_err()
                .contains("token captcha")
        );
        assert!(
            parse_spec(&json!({"v":1,"kind":"image","image":"bm90IGFuIGltYWdl"}))
                .unwrap_err()
                .contains("not a PNG")
        );
        assert!(
            parse_spec(&json!({"v":1,"kind":"image","image":"%%%"}))
                .unwrap_err()
                .contains("base64")
        );
        assert!(
            parse_spec(&json!({"v":1,"kind":"slider","image":PNG}))
                .unwrap_err()
                .contains("background")
        );
        assert!(parse_spec(&json!({"v":1,"kind":"slider","image":PNG,"background":PNG})).is_ok());
        assert!(parse_spec(&json!({"v":1,"kind":"image","image":PNG,"background":PNG})).is_err());
        assert!(
            parse_spec(
                &json!({"v":1,"kind":"slider","image":PNG,"background":PNG,"charset":"digits"})
            )
            .is_err()
        );
        let big = base64::engine::general_purpose::STANDARD
            .encode([b"\x89PNG\r\n\x1a\n".as_slice(), &vec![0u8; MAX_IMAGE_BYTES]].concat());
        assert!(
            parse_spec(&json!({"v":1,"kind":"image","image":big}))
                .unwrap_err()
                .contains("KiB")
        );
    }

    #[test]
    fn charsets() {
        assert_eq!(resolve_charset("digits").unwrap(), "0123456789");
        assert_eq!(resolve_charset("aabbc").unwrap(), "abc");
        assert!(resolve_charset("a b").is_err());
        assert!(resolve_charset("").is_err());
        let many: String = (0x4e00u32..0x4e00 + 300)
            .filter_map(char::from_u32)
            .collect();
        assert!(resolve_charset(&many).is_err());
    }

    #[test]
    fn data_urls_and_formats() {
        assert_eq!(strip_data_url("data:image/png;base64,QUJD"), "QUJD");
        assert_eq!(strip_data_url(" QUJD "), "QUJD");
        assert_eq!(detect_image_format(b"GIF89a.."), Some("gif"));
        assert_eq!(detect_image_format(b"BM...."), Some("bmp"));
        assert_eq!(detect_image_format(b"%PDF-"), None);
    }

    #[test]
    fn results() {
        assert!(valid_result(
            "image",
            &json!({"v":1,"kind":"image","text":"ab12"})
        ));
        assert!(!valid_result(
            "image",
            &json!({"v":1,"kind":"slider","text":"ab12"})
        ));
        assert!(!valid_result("image", &json!({"v":1,"kind":"image"})));
        assert!(valid_result(
            "slider",
            &json!({"v":1,"kind":"slider","x":115,"y":25})
        ));
        assert!(!valid_result(
            "slider",
            &json!({"v":1,"kind":"slider","x":null,"y":25})
        ));
        assert!(valid_result(
            "detect",
            &json!({"v":1,"kind":"detect","boxes":[[1,2,3,4]]})
        ));
        assert!(!valid_result(
            "detect",
            &json!({"v":1,"kind":"detect","boxes":[[1,2,3]]})
        ));
        assert!(!valid_result(
            "image",
            &json!({"v":2,"kind":"image","text":"x"})
        ));
    }

    #[test]
    fn spoken_answers() {
        assert_eq!(normalize_spoken("Seven, four, two. One."), "7421");
        assert_eq!(normalize_spoken("The code is 5 9 3 8"), "5938");
        assert_eq!(normalize_spoken("alpha bravo three"), "ab3");
        assert_eq!(
            normalize_spoken("Please type the following: x-ray, 4, zulu"),
            "x4z"
        );
        assert_eq!(normalize_spoken("62 41"), "6241");
        assert_eq!(normalize_spoken(""), "");
    }

    #[test]
    fn math_answers() {
        assert_eq!(solve_math("What is 3 + 4?").as_deref(), Some("7"));
        assert_eq!(solve_math("3+4=").as_deref(), Some("7"));
        assert_eq!(solve_math("12 minus five").as_deref(), Some("7"));
        assert_eq!(solve_math("What is two times six?").as_deref(), Some("12"));
        assert_eq!(solve_math("2 x 6 =").as_deref(), Some("12"));
        assert_eq!(solve_math("1 + 2 * 3").as_deref(), Some("7"));
        assert_eq!(solve_math("20 / 4").as_deref(), Some("5"));
        assert_eq!(solve_math("8 - 10").as_deref(), Some("-2"));
        assert_eq!(solve_math("5 / 0"), None);
        assert_eq!(solve_math("Are you human?"), None);
        assert_eq!(solve_math("7"), None);
    }

    #[test]
    fn unsupported() {
        assert!(unsupported_kind_message("hcaptcha").contains("token captcha"));
        assert!(unsupported_kind_message("xyz").contains("unknown"));
    }

    /// Installs ddddocr for real (network, ~150 MB), so it runs only with
    /// `C0MPUTE_TEST_CAPTCHA_RUNTIME=1`; point HOME at a scratch dir.
    #[tokio::test]
    async fn runtime_end_to_end() {
        if std::env::var("C0MPUTE_TEST_CAPTCHA_RUNTIME")
            .ok()
            .as_deref()
            != Some("1")
        {
            eprintln!("skipping: set C0MPUTE_TEST_CAPTCHA_RUNTIME=1 to install and run ddddocr");
            return;
        }
        setup().await.expect("setup");
        assert!(ready());
        let spec =
            parse_spec(&json!({"v":1,"kind":"image","image":PNG,"charset":"digits"})).unwrap();
        let result = solve(&spec).await.expect("solve");
        assert!(valid_result("image", &result), "{result}");
    }
}
