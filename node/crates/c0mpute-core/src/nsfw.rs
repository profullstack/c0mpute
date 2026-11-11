//! The NSFW output classifier for SFW (non-adult) image and video jobs.
//!
//! Model: `AdamCodd/vit-base-nsfw-detector` (Apache-2.0), the int8 ONNX
//! export (`onnx/model_quantized.onnx`, 88.5 MB), pinned by commit and
//! SHA-256 like the other house weights. It runs on CPU through ONNX Runtime
//! in a per-user virtualenv under `<data_dir>/nsfw` (the same shape as the
//! captcha runtime), in well under a second per image.
//!
//! A worker checks every SFW image, and evenly spaced frames of every SFW
//! clip, before it publishes the result. An explicit output is discarded and
//! the job fails with a policy reason. The check fails closed: a worker that
//! cannot load the classifier fails SFW jobs rather than return unchecked
//! output. Adult jobs (opt-in on both sides) skip it.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::process::Command;
use tracing::{info, warn};

use crate::content_policy::Refusal;

pub const MODEL_REPO: &str = "AdamCodd/vit-base-nsfw-detector";
pub const MODEL_COMMIT: &str = "8587de998f441aac03fdd57a85d2e4cb808c7d64";
pub const MODEL_FILE: &str = "onnx/model_quantized.onnx";
pub const MODEL_SHA256: &str = "432763a6899ebc418c55f784b98f90565e5fc694c778d2ffcb0294b12f6a7404";
pub const MODEL_BYTES: u64 = 88_500_985;
/// An image at or over this probability of `nsfw` is withheld.
pub const THRESHOLD: f32 = 0.80;
/// Frames of a clip that get classified.
pub const VIDEO_SAMPLES: usize = 6;
// Ranges, not pins: wheels per Python version differ (3.14 boxes need
// onnxruntime >= 1.24, 3.10 boxes need an older one). The MODEL is pinned.
const PACKAGES: &[&str] = &["onnxruntime>=1.20,<2", "numpy>=1.26,<3", "pillow>=10,<13"];
const PY_MIN: (u32, u32) = (3, 10);
const PYTHONS: &[&str] = &["python3.14", "python3.13", "python3.12", "python3.11", "python3.10"];
const UV_PYTHON: &str = "3.12";
const WORKER: &str = include_str!("nsfw_worker.py");
const CLASSIFY_TIMEOUT: Duration = Duration::from_secs(3 * 60);

pub fn model_url() -> String {
    format!("https://huggingface.co/{MODEL_REPO}/resolve/{MODEL_COMMIT}/{MODEL_FILE}")
}

pub fn root() -> PathBuf {
    crate::config::data_dir()
        .unwrap_or_else(|| PathBuf::from("./c0mpute-data"))
        .join("nsfw")
}

fn venv_dir() -> PathBuf {
    root().join("venv")
}

fn python_path() -> PathBuf {
    crate::speech::venv_python(&venv_dir())
}

fn model_path() -> PathBuf {
    root().join(format!("model-{}.onnx", &MODEL_SHA256[..16]))
}

fn ready_marker() -> PathBuf {
    root().join(".ready")
}

fn marker_text() -> String {
    format!("{} {}\n", MODEL_SHA256, PACKAGES.join(" "))
}

/// Installed and self-tested with the pinned model and packages.
pub fn ready() -> bool {
    python_path().is_file()
        && model_path().is_file()
        && std::fs::read_to_string(ready_marker()).is_ok_and(|m| m == marker_text())
}

/// Install (once per process at a time) and self-test the classifier.
pub async fn ensure() -> Result<()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _guard = LOCK.lock().await;
    if ready() {
        return Ok(());
    }
    info!(model = MODEL_REPO, "installing the NSFW output classifier (SFW jobs need it)");
    tokio::fs::create_dir_all(root()).await?;
    let python = ensure_venv().await?;
    let mut install = Command::new(&python);
    install
        .args(["-m", "pip", "install", "--disable-pip-version-check", "--quiet"])
        .args(PACKAGES)
        .env("TMPDIR", tmpdir().await?);
    crate::speech::run_checked(install, "install the NSFW classifier runtime").await?;
    crate::diffusion::download_verified_streaming(&model_url(), &model_path(), MODEL_SHA256)
        .await
        .context("download the NSFW classifier model")?;
    let scores = run(&["--selftest".into()]).await.context("self-test the NSFW classifier")?;
    if scores.len() != 1 || !scores[0].is_finite() {
        bail!("NSFW classifier self-test printed {scores:?}");
    }
    tokio::fs::write(ready_marker(), marker_text()).await?;
    info!(selftest_score = scores[0], "NSFW output classifier ready");
    Ok(())
}

/// Install in the background at worker start so the first SFW job does not
/// wait for it. Failures are logged; `ensure` runs again on the next job.
pub fn warm_up() {
    tokio::spawn(async {
        if let Err(e) = ensure().await {
            warn!(err = %format!("{e:#}"), "NSFW classifier not ready yet; SFW image/video jobs will retry the install");
        }
    });
}

async fn tmpdir() -> Result<PathBuf> {
    let dir = root().join("tmp");
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
        tokio::fs::remove_dir_all(&dir).await?;
    }
    if let Some(interp) = interpreter() {
        let mut command = Command::new(interp);
        command.args(["-m", "venv"]).arg(&dir);
        if crate::speech::run_checked(command, "create the NSFW classifier Python environment")
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
        command.args(["venv", "--seed", "--python", UV_PYTHON]).arg(&dir);
        crate::speech::run_checked(command, "create the NSFW classifier Python environment with uv").await?;
        return Ok(python);
    }
    bail!(
        "the NSFW classifier needs Python 3.10+ with venv support (apt install python3-venv), \
         or uv, or C0MPUTE_PYTHON pointing at one"
    )
}

async fn run(args: &[String]) -> Result<Vec<f32>> {
    let mut command = Command::new(python_path());
    command
        .arg("-c")
        .arg(WORKER)
        .arg(model_path())
        .args(args)
        .env("TMPDIR", tmpdir().await?)
        .env("C0MPUTE_JOB_CPU_THREADS", crate::resources::cpu_threads_per_job().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(CLASSIFY_TIMEOUT, command.output())
        .await
        .context("NSFW classifier timed out")?
        .context("start the NSFW classifier")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("NSFW classifier failed: {}", stderr.lines().last().unwrap_or("error"));
    }
    #[derive(serde::Deserialize)]
    struct Out {
        scores: Vec<f32>,
    }
    let out: Out = serde_json::from_slice(&output.stdout).context("NSFW classifier printed invalid JSON")?;
    Ok(out.scores)
}

static TEST_SCORE: std::sync::OnceLock<f32> = std::sync::OnceLock::new();

/// Integration tests only: score every output `score` instead of running
/// the model (their fake runners write placeholder clips no decoder reads).
/// Code-only, never read from config or the environment.
#[doc(hidden)]
pub fn use_fixed_score_for_tests(score: f32) {
    let _ = TEST_SCORE.set(score);
}

pub(crate) fn fixed_test_score() -> Option<f32> {
    TEST_SCORE.get().copied()
}

/// `nsfw` probability per image file.
pub async fn classify(paths: &[PathBuf]) -> Result<Vec<f32>> {
    if let Some(score) = fixed_test_score() {
        return Ok(vec![score; paths.len()]);
    }
    ensure().await?;
    let args: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
    let scores = run(&args).await?;
    if scores.len() != paths.len() {
        bail!("NSFW classifier scored {} of {} images", scores.len(), paths.len());
    }
    Ok(scores)
}

/// The verdict for a set of scores: explicit when any reaches the threshold.
pub fn explicit(scores: &[f32]) -> bool {
    scores.iter().any(|s| !s.is_finite() || *s >= THRESHOLD)
}

/// Fail an SFW job whose output is explicit; fail closed when the
/// classifier cannot run. Never called for adult jobs.
pub async fn sfw_gate(paths: &[PathBuf], job_id: &str) -> Result<()> {
    let scores = classify(paths)
        .await
        .map_err(|e| anyhow::anyhow!("policy: classifier_unavailable: SFW output could not be checked: {e:#}"))?;
    if explicit(&scores) {
        crate::content_policy::record(Refusal::ExplicitOutput, "worker", Some(job_id));
        bail!(Refusal::ExplicitOutput.job_error());
    }
    Ok(())
}

/// Up to `n` evenly spaced items of `all` (first and last included).
pub fn sample<T: Clone>(all: &[T], n: usize) -> Vec<T> {
    if all.len() <= n {
        return all.to_vec();
    }
    if n < 2 {
        return all.iter().take(n).cloned().collect();
    }
    (0..n).map(|i| all[i * (all.len() - 1) / (n - 1)].clone()).collect()
}

/// Sampled frames of a rendered frame directory (`*.png`, sorted).
pub async fn sample_frames(dir: &Path, n: usize) -> Result<Vec<PathBuf>> {
    let mut frames = Vec::new();
    let mut entries = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "png") {
            frames.push(path);
        }
    }
    frames.sort();
    Ok(sample(&frames, n))
}

/// Extract `n` frames of an encoded clip with ffmpeg into `dir`.
pub async fn extract_frames(clip: &Path, dir: &Path, n: usize) -> Result<Vec<PathBuf>> {
    let ffmpeg = crate::supervisor::which_ffmpeg().context("checking an SFW clip needs ffmpeg on PATH")?;
    tokio::fs::create_dir_all(dir).await?;
    // Every 8th frame, at most n of them: a 33-frame clip gives 5, an
    // 81-frame clip gives n.
    let output = Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(clip)
        .args(["-vf", "select=not(mod(n\\,8))", "-vsync", "vfr", "-frames:v", &n.to_string()])
        .arg(dir.join("check_%03d.png"))
        .stdin(Stdio::null())
        .output()
        .await
        .context("run ffmpeg to sample the clip")?;
    if !output.status.success() {
        bail!("ffmpeg could not sample the clip");
    }
    let frames = sample_frames(dir, n).await?;
    if frames.is_empty() {
        bail!("ffmpeg sampled no frames from the clip");
    }
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_uses_the_threshold_and_fails_closed_on_nan() {
        assert!(!explicit(&[0.01, 0.2, 0.79]));
        assert!(explicit(&[0.01, 0.8]));
        assert!(explicit(&[f32::NAN]));
        assert!(!explicit(&[]));
    }

    #[test]
    fn samples_are_evenly_spaced_and_include_both_ends() {
        let all: Vec<u32> = (0..33).collect();
        assert_eq!(sample(&all, 6), vec![0, 6, 12, 19, 25, 32]);
        assert_eq!(sample(&[1, 2], 6), vec![1, 2]);
    }

    #[test]
    fn the_model_is_pinned() {
        assert_eq!(MODEL_SHA256.len(), 64);
        assert_eq!(MODEL_COMMIT.len(), 40);
        assert!(model_url().contains(MODEL_COMMIT));
    }

    /// Installs onnxruntime and downloads the pinned model (network, ~150 MB),
    /// then classifies a benign generated image. Runs only with
    /// `C0MPUTE_TEST_NSFW_RUNTIME=1`; point HOME at a scratch dir.
    #[tokio::test]
    async fn classifier_passes_a_benign_image() {
        if std::env::var("C0MPUTE_TEST_NSFW_RUNTIME").ok().as_deref() != Some("1") {
            eprintln!("skipping: set C0MPUTE_TEST_NSFW_RUNTIME=1 to install and run the classifier");
            return;
        }
        ensure().await.expect("install");
        assert!(ready());
        // A benign, flat blue PNG written by hand (no image crate here).
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("benign.png");
        std::fs::write(&png, benign_png()).unwrap();
        let scores = classify(std::slice::from_ref(&png)).await.expect("classify");
        assert_eq!(scores.len(), 1);
        assert!(!explicit(&scores), "benign image scored {scores:?}");
        sfw_gate(&[png], "test-job").await.expect("benign image passes the SFW gate");
    }

    /// A 32x32 solid sky-blue PNG (stored, uncompressed deflate blocks).
    fn benign_png() -> Vec<u8> {
        fn crc(data: &[u8]) -> u32 {
            let mut c = 0xffff_ffffu32;
            for b in data {
                c ^= *b as u32;
                for _ in 0..8 {
                    c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
                }
            }
            !c
        }
        fn chunk(out: &mut Vec<u8>, kind: &[u8], data: &[u8]) {
            out.extend((data.len() as u32).to_be_bytes());
            let mut body = kind.to_vec();
            body.extend(data);
            out.extend(&body);
            out.extend(crc(&body).to_be_bytes());
        }
        let (w, h) = (32u32, 32u32);
        let mut raw = Vec::new();
        for _ in 0..h {
            raw.push(0u8);
            for _ in 0..w {
                raw.extend([110u8, 170, 230]);
            }
        }
        // zlib: header, one stored block, adler32.
        let mut z = vec![0x78, 0x01, 0x01];
        z.extend((raw.len() as u16).to_le_bytes());
        z.extend((!(raw.len() as u16)).to_le_bytes());
        z.extend(&raw);
        let (mut a, mut b) = (1u32, 0u32);
        for x in &raw {
            a = (a + *x as u32) % 65521;
            b = (b + a) % 65521;
        }
        z.extend(((b << 16) | a).to_be_bytes());
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend(w.to_be_bytes());
        ihdr.extend(h.to_be_bytes());
        ihdr.extend([8, 2, 0, 0, 0]);
        chunk(&mut png, b"IHDR", &ihdr);
        chunk(&mut png, b"IDAT", &z);
        chunk(&mut png, b"IEND", &[]);
        png
    }
}
