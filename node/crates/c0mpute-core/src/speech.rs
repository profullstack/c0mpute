//! CPU speech inference runtimes used by the c0mpute `tts.synthesize` and
//! `stt.transcribe` workloads. Models are installed per user under the normal
//! c0mpute data directory; no root access or provider key is required.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

const WORKER: &str = include_str!("speech_worker.py");
const KOKORO_MODEL_URL: &str = "https://github.com/thewh1teagle/kokoro-onnx/releases/download/model-files-v1.1/kokoro-v1.0.int8.onnx";
const KOKORO_MODEL_SHA256: &str =
    "ae315a79b623f244700e4afb9246c46a26066782e049ba174bf3ba433970ee9c";
const KOKORO_VOICES_URL: &str = "https://github.com/thewh1teagle/kokoro-onnx/releases/download/model-files-v1.1/voices-v1.0.bin";
const KOKORO_VOICES_SHA256: &str =
    "bca610b8308e8d99f32e6fe4197e7ec01679264efed0cac9140fe9c29f1fbf7d";
const KOKORO_PIP: &[&str] = &["kokoro-onnx==0.6.1", "soundfile==0.13.1"];
/// Interpreters kokoro-onnx 0.6.1 has wheels for, newest first. Checked
/// before the box's default `python3`; `C0MPUTE_PYTHON` overrides both.
///
/// The pin above is not negotiable downwards: every kokoro-onnx from 0.4.8 on
/// declares `Requires-Python <3.14`, so on a 3.14 interpreter pip falls back
/// to 0.4.7, and 0.4.7 feeds int32 inputs to the `model-files-v1.1` graph,
/// which wants float — synthesis then fails at inference time with
/// `INVALID_ARGUMENT`. Matching the library to the model matters more than
/// matching it to the newest Python, so we find an interpreter instead.
const KOKORO_PYTHONS: &[&str] = &["python3.13", "python3.12", "python3.11", "python3.10"];
/// Inclusive floor and exclusive ceiling of the interpreters Kokoro runs on.
const KOKORO_PY_MIN: (u32, u32) = (3, 10);
const KOKORO_PY_MAX_EXCLUSIVE: (u32, u32) = (3, 14);
/// What `uv` is asked for when it has to supply the interpreter itself.
const KOKORO_UV_PYTHON: &str = "3.12";
/// How long to wait before retrying a failed auto-provision, so a restart
/// loop cannot hammer PyPI.
const INSTALL_RETRY_BACKOFF_SECS: u64 = 6 * 60 * 60;
const PIPER_VOICES: &[&str] = &["en_US-joe-medium", "en_US-norman-medium"];
const KOKORO_VOICES: &[&str] = &["af_sarah", "af_bella", "am_onyx", "am_michael"];

pub fn speech_root() -> PathBuf {
    crate::config::data_dir()
        .unwrap_or_else(|| PathBuf::from("./c0mpute-data"))
        .join("speech")
}

pub fn python_path() -> PathBuf {
    #[cfg(windows)]
    let candidate = speech_root().join("venv/Scripts/python.exe");
    #[cfg(not(windows))]
    let candidate = speech_root().join("venv/bin/python");
    candidate
}

/// Kokoro's own virtualenv, used only when the shared speech venv runs a
/// Python kokoro-onnx has no wheels for (3.14+). Keeping it separate means
/// provisioning Kokoro never disturbs an existing Piper or Whisper install.
fn kokoro_venv_dir() -> PathBuf {
    speech_root().join("venv-kokoro")
}

fn venv_python(dir: &Path) -> PathBuf {
    #[cfg(windows)]
    let candidate = dir.join("Scripts/python.exe");
    #[cfg(not(windows))]
    let candidate = dir.join("bin/python");
    candidate
}

/// The interpreter Kokoro runs under: the shared speech venv when its Python
/// is in range, otherwise Kokoro's dedicated venv.
///
/// Prefer [`kokoro_python_installed`] on hot paths — this one probes
/// interpreter versions by running them.
pub fn kokoro_python() -> PathBuf {
    if let Some(python) = kokoro_python_installed() {
        return python;
    }
    let shared = python_path();
    if shared.is_file() && python_version(&shared).is_some_and(python_fits_kokoro) {
        return shared;
    }
    venv_python(&kokoro_venv_dir())
}

/// The interpreter a completed install recorded in `.kokoro-ready`.
///
/// The readiness marker names it so that checking "can this node speak?" — a
/// question asked on every capability advertise tick — is one small file read
/// rather than spawning a Python to ask its version.
pub fn kokoro_python_installed() -> Option<PathBuf> {
    let marker = std::fs::read_to_string(speech_root().join(".kokoro-ready")).ok()?;
    marker
        .lines()
        .find_map(|line| line.strip_prefix("python="))
        .map(PathBuf::from)
}

fn python_fits_kokoro(version: (u32, u32)) -> bool {
    version >= KOKORO_PY_MIN && version < KOKORO_PY_MAX_EXCLUSIVE
}

/// `(major, minor)` of an interpreter, or None when it cannot be run.
fn python_version(python: &Path) -> Option<(u32, u32)> {
    let output = std::process::Command::new(python)
        .args([
            "-c",
            "import sys;print(sys.version_info[0], sys.version_info[1])",
        ])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut parts = text.split_whitespace();
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

pub fn tts_voice_available(engine: &str, voice: &str) -> bool {
    let root = speech_root();
    if engine == "kokoro" {
        if !KOKORO_VOICES.contains(&voice) {
            return false;
        }
        let Some(python) = kokoro_python_installed() else {
            return false;
        };
        return python.is_file()
            && root.join("models/kokoro/kokoro-v1.0.int8.onnx").is_file()
            && root.join("models/kokoro/voices-v1.0.bin").is_file();
    }
    if !python_path().is_file() {
        return false;
    }
    match engine {
        "piper" => {
            PIPER_VOICES.contains(&voice)
                && root.join(".piper-ready").is_file()
                && root
                    .join("models/piper")
                    .join(format!("{voice}.onnx"))
                    .is_file()
                && root
                    .join("models/piper")
                    .join(format!("{voice}.onnx.json"))
                    .is_file()
        }
        _ => false,
    }
}

pub fn stt_ready() -> bool {
    python_path().is_file() && speech_root().join(".stt-ready").is_file()
}

pub fn available_tts_engines() -> Vec<&'static str> {
    let mut engines = Vec::new();
    if PIPER_VOICES
        .iter()
        .any(|voice| tts_voice_available("piper", voice))
    {
        engines.push("piper");
    }
    if KOKORO_VOICES
        .iter()
        .any(|voice| tts_voice_available("kokoro", voice))
    {
        engines.push("kokoro");
    }
    engines
}

pub async fn setup_tts(engine: &str) -> Result<()> {
    if engine != "piper" && engine != "kokoro" && engine != "all" {
        bail!("engine must be piper, kokoro, or all");
    }
    let root = speech_root();
    tokio::fs::create_dir_all(root.join("models/piper")).await?;
    tokio::fs::create_dir_all(root.join("models/kokoro")).await?;
    if engine == "piper" || engine == "all" {
        ensure_venv().await?;
        pip_install(&["piper-tts==1.8.0"]).await?;
        let mut command = Command::new(python_path());
        command
            .args(["-m", "piper.download_voices", "--data-dir"])
            .arg(root.join("models/piper"));
        command.args(PIPER_VOICES);
        run_checked(command, "download Piper voices").await?;
        tokio::fs::write(root.join(".piper-ready"), b"piper-tts 1.8.0\n").await?;
    }
    if engine == "kokoro" || engine == "all" {
        let python = ensure_kokoro_venv().await?;
        pip_install_with(&python, KOKORO_PIP).await?;
        download_verified(
            KOKORO_MODEL_URL,
            &root.join("models/kokoro/kokoro-v1.0.int8.onnx"),
            KOKORO_MODEL_SHA256,
        )
        .await?;
        download_verified(
            KOKORO_VOICES_URL,
            &root.join("models/kokoro/voices-v1.0.bin"),
            KOKORO_VOICES_SHA256,
        )
        .await?;
        // The marker names the interpreter so readiness checks never have to
        // re-derive it (see `kokoro_python_installed`).
        tokio::fs::write(
            root.join(".kokoro-ready"),
            format!("kokoro-onnx 0.6.1 int8\npython={}\n", python.display()),
        )
        .await?;
    }
    Ok(())
}

pub async fn setup_stt() -> Result<()> {
    ensure_venv().await?;
    // faster-whisper 1.2.1 still passes `metadata_errors` to av.open;
    // PyAV 19 removed that keyword, so keep the runtime on the compatible API.
    pip_install(&["faster-whisper==1.2.1", "av<19"]).await?;
    let root = speech_root();
    tokio::fs::create_dir_all(root.join("models/whisper")).await?;
    let code = "from faster_whisper import WhisperModel; import sys; WhisperModel('base', device='cpu', compute_type='int8', download_root=sys.argv[1])";
    let mut command = Command::new(python_path());
    command.arg("-c").arg(code).arg(root.join("models/whisper"));
    run_checked(
        command,
        "download the multilingual faster-whisper base model",
    )
    .await?;
    tokio::fs::write(
        root.join(".stt-ready"),
        b"faster-whisper 1.2.1 base multilingual\n",
    )
    .await?;
    Ok(())
}

async fn ensure_venv() -> Result<()> {
    if python_path().is_file() {
        return Ok(());
    }
    let root = speech_root();
    tokio::fs::create_dir_all(&root).await?;
    create_venv(&select_interpreter(), &root.join("venv")).await
}

async fn create_venv(interpreter: &std::ffi::OsStr, dir: &Path) -> Result<()> {
    let mut command = Command::new(interpreter);
    command.args(["-m", "venv"]).arg(dir);
    run_checked(command, "create the isolated speech Python environment").await
}

/// The interpreter Kokoro will run under, creating its environment if needed.
///
/// One shared speech venv is the normal case. A box whose only Python is 3.14+
/// gets Kokoro its own venv instead — built from an older interpreter when the
/// box has one, else by `uv`, which fetches a managed CPython itself. Piper
/// and Whisper keep the shared venv either way.
async fn ensure_kokoro_venv() -> Result<PathBuf> {
    let existing = kokoro_python();
    if existing.is_file() {
        return Ok(existing);
    }
    tokio::fs::create_dir_all(speech_root()).await?;
    if let Some(interpreter) = kokoro_interpreter() {
        // No shared venv yet: build that one and let every engine share it.
        if !python_path().is_file() {
            create_venv(&interpreter, &speech_root().join("venv")).await?;
            return Ok(python_path());
        }
        let dir = kokoro_venv_dir();
        create_venv(&interpreter, &dir).await?;
        return Ok(venv_python(&dir));
    }
    if let Some(uv) = find_uv() {
        let dir = kokoro_venv_dir();
        let mut command = Command::new(uv);
        command
            .args(["venv", "--seed", "--python", KOKORO_UV_PYTHON])
            .arg(&dir);
        run_checked(command, "create the Kokoro Python environment with uv").await?;
        return Ok(venv_python(&dir));
    }
    bail!(
        "Kokoro needs Python {}.{} to {}.{} and this box has none (default python3 is {}). \
         Install python3.13 (Debian/Ubuntu: apt install python3.13-venv), or install uv \
         (curl -LsSf https://astral.sh/uv/install.sh | sh), or point C0MPUTE_PYTHON at a \
         suitable interpreter.",
        KOKORO_PY_MIN.0,
        KOKORO_PY_MIN.1,
        KOKORO_PY_MAX_EXCLUSIVE.0,
        KOKORO_PY_MAX_EXCLUSIVE.1 - 1,
        python_version(Path::new("python3"))
            .map(|(major, minor)| format!("{major}.{minor}"))
            .unwrap_or_else(|| "missing".to_string())
    )
}

/// An interpreter on this box that Kokoro has wheels for, if there is one.
fn kokoro_interpreter() -> Option<std::ffi::OsString> {
    if let Some(python) = std::env::var_os("C0MPUTE_PYTHON")
        && python_version(Path::new(&python)).is_some_and(python_fits_kokoro)
    {
        return Some(python);
    }
    for name in KOKORO_PYTHONS {
        if on_path(name) && python_version(Path::new(name)).is_some_and(python_fits_kokoro) {
            return Some((*name).into());
        }
    }
    python_version(Path::new("python3"))
        .filter(|version| python_fits_kokoro(*version))
        .map(|_| "python3".into())
}

async fn pip_install(packages: &[&str]) -> Result<()> {
    pip_install_with(&python_path(), packages).await
}

async fn pip_install_with(python: &Path, packages: &[&str]) -> Result<()> {
    let mut command = Command::new(python);
    command
        .args(["-m", "pip", "install", "--disable-pip-version-check"])
        .args(packages);
    run_checked(command, "install speech runtime packages").await
}

async fn run_checked(mut command: Command, task: &str) -> Result<()> {
    let status = command
        .stdin(Stdio::null())
        .status()
        .await
        .with_context(|| format!("start {task}; install Python 3 and venv support first"))?;
    if !status.success() {
        bail!("could not {task} (exit {status})");
    }
    Ok(())
}

async fn download_verified(url: &str, path: &Path, expected_sha256: &str) -> Result<()> {
    if path.is_file() {
        let existing = tokio::fs::read(path).await?;
        if hex::encode(Sha256::digest(&existing)) == expected_sha256 {
            return Ok(());
        }
        tokio::fs::remove_file(path).await?;
    }
    let response = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()?
        .get(url)
        .send()
        .await?
        .error_for_status()?;
    let bytes = response.bytes().await?;
    if hex::encode(Sha256::digest(&bytes)) != expected_sha256 {
        bail!("SHA-256 check failed for speech model download");
    }
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let temp = path.with_extension("partial");
    tokio::fs::write(&temp, &bytes).await?;
    tokio::fs::rename(temp, path).await?;
    Ok(())
}

pub async fn synthesize(request: &serde_json::Value, output: &Path) -> Result<()> {
    run_worker("tts", &[output.to_string_lossy().as_ref()], request).await
}

pub async fn transcribe(request: &serde_json::Value, audio: &Path, output: &Path) -> Result<()> {
    run_worker(
        "stt",
        &[
            audio.to_string_lossy().as_ref(),
            output.to_string_lossy().as_ref(),
        ],
        request,
    )
    .await
}

async fn run_worker(action: &str, args: &[&str], request: &serde_json::Value) -> Result<()> {
    // Kokoro may live in its own venv (see `ensure_kokoro_venv`), so pick the
    // interpreter by engine rather than assuming the shared one.
    let python = if action == "tts"
        && request.get("engine").and_then(|engine| engine.as_str()) == Some("kokoro")
    {
        kokoro_python_installed().unwrap_or_else(kokoro_python)
    } else {
        python_path()
    };
    let mut command = Command::new(python);
    command.arg("-c").arg(WORKER).arg(action).arg(speech_root());
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .context("start local speech runtime; run `c0mpute tts setup` or `c0mpute stt setup`")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(serde_json::to_string(request)?.as_bytes())
            .await?;
    }
    let output_result = child.wait_with_output().await?;
    if !output_result.status.success() {
        let detail = String::from_utf8_lossy(&output_result.stderr);
        bail!(
            "speech inference failed: {}",
            detail.lines().last().unwrap_or("runtime error")
        );
    }
    Ok(())
}

/// The interpreter the speech venv is built from: `C0MPUTE_PYTHON` if set,
/// else the newest interpreter Kokoro has wheels for, else plain `python3`
/// (which is right on any box whose default is already in range).
fn select_interpreter() -> std::ffi::OsString {
    if let Some(python) = std::env::var_os("C0MPUTE_PYTHON") {
        return python;
    }
    kokoro_interpreter().unwrap_or_else(|| "python3".into())
}

fn on_path(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(program).is_file())
}

/// Locate `uv`, including where its installer puts it.
///
/// A daemon started by systemd inherits a minimal PATH that usually omits
/// `~/.local/bin`, which is exactly where `astral.sh/uv/install.sh` lands, so
/// PATH alone would miss an interpreter source the box actually has.
fn find_uv() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("PATH")
        && let Some(found) = std::env::split_paths(&path)
            .map(|dir| dir.join("uv"))
            .find(|candidate| candidate.is_file())
    {
        return Some(found);
    }
    let mut candidates = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join(".local/bin/uv"));
    }
    candidates.push(PathBuf::from("/usr/local/bin/uv"));
    candidates.push(PathBuf::from("/opt/homebrew/bin/uv"));
    candidates.into_iter().find(|candidate| candidate.is_file())
}

fn retry_marker() -> PathBuf {
    speech_root().join(".kokoro-retry-after")
}

/// True when a previous auto-provision failed recently enough that we should
/// leave PyPI alone for now.
fn in_install_backoff() -> bool {
    let Ok(contents) = std::fs::read_to_string(retry_marker()) else {
        return false;
    };
    let Ok(retry_after) = contents.trim().parse::<u64>() else {
        return false;
    };
    now_secs() < retry_after
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Free bytes on the filesystem holding the speech directory. Falls back to
/// the largest disk when no mount point matches (an unusual layout should not
/// block provisioning).
fn free_bytes_for_speech_root() -> u64 {
    let root = speech_root();
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let mut best: Option<(usize, u64)> = None;
    let mut largest = 0u64;
    for disk in disks.list() {
        largest = largest.max(disk.available_space());
        let mount = disk.mount_point();
        if root.starts_with(mount) {
            let depth = mount.components().count();
            if best.is_none() || best.is_some_and(|(best_depth, _)| depth > best_depth) {
                best = Some((depth, disk.available_space()));
            }
        }
    }
    best.map(|(_, free)| free).unwrap_or(largest)
}

/// Make sure this node can synthesize speech with Kokoro, installing it if it
/// is missing. Idempotent, safe to call on every worker start, and never
/// fatal: a node that cannot provision simply does not advertise TTS.
///
/// Returns true when Kokoro is ready to serve.
pub async fn ensure_kokoro(config: &crate::config::SpeechConfig) -> bool {
    if available_tts_engines().contains(&"kokoro") {
        return true;
    }
    if !config.auto_install_enabled() {
        tracing::debug!("kokoro auto-provisioning is off; not installing");
        return false;
    }
    let free = free_bytes_for_speech_root();
    let required = config.min_free_disk_gb.saturating_mul(1024 * 1024 * 1024);
    if free < required {
        tracing::warn!(
            free_mb = free / 1024 / 1024,
            required_mb = required / 1024 / 1024,
            "skipping kokoro install: not enough free disk"
        );
        return false;
    }
    if in_install_backoff() {
        tracing::debug!("kokoro install failed recently; waiting out the backoff");
        return false;
    }
    tracing::info!("installing Kokoro CPU voices so this node can serve tts.synthesize");
    match setup_tts("kokoro").await {
        Ok(()) => {
            let _ = tokio::fs::remove_file(retry_marker()).await;
            tracing::info!(root = %speech_root().display(), "kokoro ready");
            true
        }
        Err(error) => {
            let retry_after = now_secs().saturating_add(INSTALL_RETRY_BACKOFF_SECS);
            if let Some(parent) = retry_marker().parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            let _ = tokio::fs::write(retry_marker(), retry_after.to_string()).await;
            tracing::warn!(
                err = %error,
                "could not install Kokoro; this node will not serve TTS. Needs python3 with venv support (Debian/Ubuntu: apt install python3-venv). Turn provisioning off with `c0mpute tts auto off`."
            );
            false
        }
    }
}
