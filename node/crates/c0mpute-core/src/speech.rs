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

pub fn tts_voice_available(engine: &str, voice: &str) -> bool {
    if !python_path().is_file() {
        return false;
    }
    let root = speech_root();
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
        "kokoro" => {
            KOKORO_VOICES.contains(&voice)
                && root.join(".kokoro-ready").is_file()
                && root.join("models/kokoro/kokoro-v1.0.int8.onnx").is_file()
                && root.join("models/kokoro/voices-v1.0.bin").is_file()
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
    ensure_venv().await?;
    let root = speech_root();
    tokio::fs::create_dir_all(root.join("models/piper")).await?;
    tokio::fs::create_dir_all(root.join("models/kokoro")).await?;
    if engine == "piper" || engine == "all" {
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
        pip_install(&["kokoro-onnx==0.6.1", "soundfile==0.13.1"]).await?;
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
        tokio::fs::write(root.join(".kokoro-ready"), b"kokoro-onnx 0.6.1 int8\n").await?;
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
    let python = std::env::var_os("C0MPUTE_PYTHON").unwrap_or_else(|| "python3".into());
    let mut command = Command::new(python);
    command.args(["-m", "venv"]).arg(root.join("venv"));
    run_checked(command, "create the isolated speech Python environment").await
}

async fn pip_install(packages: &[&str]) -> Result<()> {
    let mut command = Command::new(python_path());
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
    let mut command = Command::new(python_path());
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
