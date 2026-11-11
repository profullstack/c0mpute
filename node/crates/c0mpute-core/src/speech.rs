//! CPU speech inference runtimes used by the c0mpute `tts.synthesize` and
//! `stt.transcribe` workloads. Models are installed per user under the normal
//! c0mpute data directory; no root access or provider key is required.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::warn;

const WORKER: &str = include_str!("speech_worker.py");
/// The same worker source, for the warm-session path in `speech_session`.
pub(crate) const WORKER_SOURCE: &str = WORKER;
/// Full-precision Kokoro. The int8 build we shipped first is audibly worse
/// (fuzzy, ~7 dB less air above 7 kHz on am_michael; Anthony 2026-11-11: "not
/// crisp and clean like it should be") and on ONNX Runtime CPU it is also
/// SLOWER (rtf 4.0-4.3 vs 2.3-2.6 measured side by side), so it bought nothing.
pub(crate) const KOKORO_MODEL_FILE: &str = "kokoro-v1.0.onnx";
const KOKORO_MODEL_URL: &str = "https://github.com/thewh1teagle/kokoro-onnx/releases/download/model-files-v1.1/kokoro-v1.0.onnx";
const KOKORO_MODEL_SHA256: &str =
    "beb0d1848dee9a49da392cc3df26958d46cfa35d321edf434f52949153f0df3a";
/// What older nodes provisioned. Still served (the worker prefers the full
/// model when both exist) so a node mid-upgrade never drops out of TTS.
pub(crate) const KOKORO_LEGACY_MODEL_FILE: &str = "kokoro-v1.0.int8.onnx";
const KOKORO_VOICES_URL: &str = "https://github.com/thewh1teagle/kokoro-onnx/releases/download/model-files-v1.1/voices-v1.0.bin";
const KOKORO_VOICES_SHA256: &str =
    "bca610b8308e8d99f32e6fe4197e7ec01679264efed0cac9140fe9c29f1fbf7d";
const PIP_BOOTSTRAP_URL: &str = "https://files.pythonhosted.org/packages/29/a2/d40fb2460e883eca5199c62cfc2463fd261f760556ae6290f88488c362c0/pip-25.1.1-py3-none-any.whl";
const PIP_BOOTSTRAP_SHA256: &str =
    "2913a38a2abf4ea6b64ab507bd9e967f3b53dc1ede74b01b0931e1ce548751af";
const PIP_BOOTSTRAP_WHEEL: &str = "pip-25.1.1-py3-none-any.whl";
/// Interpreters kokoro-onnx 0.6.1 has wheels for, newest first. Checked
/// before the box's default `python3`; `C0MPUTE_PYTHON` overrides both.
///
/// The `kokoro-onnx==0.6.1` pin below is not negotiable downwards: every
/// release from 0.4.8 on declares `Requires-Python <3.14`, so on a 3.14
/// interpreter pip falls back to 0.4.7, and 0.4.7 feeds the
/// `model-files-v1.1` graph int32 inputs where it wants float — synthesis
/// then dies with `INVALID_ARGUMENT`. Matching the library to the model
/// matters more than matching it to the newest Python, so when the box's
/// Python is too new we find Kokoro an older interpreter instead.
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
const KOKORO_VOICES: &[&str] = &[
    "af_sarah",
    "af_bella",
    "af_nicole",
    "am_onyx",
    "am_michael",
    "am_echo",
];
// Use the tiny English checkpoint so CPU workers can load Pocket TTS without
// carrying the full-size model. These preset voices use permissive licenses.
const POCKET_VOICES: &[&str] = &[
    "alba",
    "anna",
    "caro_davy",
    "marius",
    "javert",
    "bill_boerst",
    "peter_yearsley",
    "stuart_bell",
];
const POCKET_LANGUAGE: &str = "english_2m_2026-10";

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
/// provisioning Kokoro never disturbs an existing Piper, Pocket or Whisper
/// install.
fn kokoro_venv_dir() -> PathBuf {
    speech_root().join("venv-kokoro")
}

pub(crate) fn venv_python(dir: &Path) -> PathBuf {
    #[cfg(windows)]
    let candidate = dir.join("Scripts/python.exe");
    #[cfg(not(windows))]
    let candidate = dir.join("bin/python");
    candidate
}

/// The interpreter a completed install recorded in `.kokoro-ready`.
///
/// The readiness marker names it so that checking "can this node speak?" — a
/// question asked on every capability advertise tick — stays a small file read
/// rather than spawning a Python to ask its version.
pub fn kokoro_python_installed() -> Option<PathBuf> {
    let marker = std::fs::read_to_string(speech_root().join(".kokoro-ready")).ok()?;
    marker
        .lines()
        .find_map(|line| line.strip_prefix("python="))
        .map(PathBuf::from)
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

fn python_fits_kokoro(version: (u32, u32)) -> bool {
    version >= KOKORO_PY_MIN && version < KOKORO_PY_MAX_EXCLUSIVE
}

/// `(major, minor)` of an interpreter, or None when it cannot be run.
pub(crate) fn python_version(python: &Path) -> Option<(u32, u32)> {
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
    // Kokoro may run from its own venv, so it is the one engine whose
    // readiness does not imply the shared one exists.
    if engine == "kokoro" {
        if !KOKORO_VOICES.contains(&voice) {
            return false;
        }
        let Some(python) = kokoro_python_installed() else {
            return false;
        };
        return python.is_file()
            && (kokoro_full_model_present()
                || root.join("models/kokoro").join(KOKORO_LEGACY_MODEL_FILE).is_file())
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
        "pocket" => POCKET_VOICES.contains(&voice) && root.join(".pocket-ready").is_file(),
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
    if POCKET_VOICES
        .iter()
        .any(|voice| tts_voice_available("pocket", voice))
    {
        engines.push("pocket");
    }
    engines
}

fn piper_ready() -> bool {
    PIPER_VOICES
        .iter()
        .all(|voice| tts_voice_available("piper", voice))
}

fn kokoro_ready() -> bool {
    KOKORO_VOICES
        .iter()
        .all(|voice| tts_voice_available("kokoro", voice))
        && kokoro_full_model_present()
}

/// True once the full-precision model is on disk (see [`KOKORO_MODEL_FILE`]).
pub fn kokoro_full_model_present() -> bool {
    speech_root()
        .join("models/kokoro")
        .join(KOKORO_MODEL_FILE)
        .is_file()
}

fn pocket_ready() -> bool {
    POCKET_VOICES
        .iter()
        .all(|voice| tts_voice_available("pocket", voice))
}

pub async fn setup_tts(engine: &str) -> Result<()> {
    if engine != "piper" && engine != "kokoro" && engine != "pocket" && engine != "all" {
        bail!("engine must be piper, kokoro, pocket, or all");
    }
    let install_piper = (engine == "piper" || engine == "all") && !piper_ready();
    let install_kokoro = (engine == "kokoro" || engine == "all") && !kokoro_ready();
    let install_pocket = (engine == "pocket" || engine == "all") && !pocket_ready();
    if !install_piper && !install_kokoro && !install_pocket {
        return Ok(());
    }

    ensure_venv().await?;
    let root = speech_root();
    if install_piper {
        tokio::fs::create_dir_all(root.join("models/piper")).await?;
        pip_install(&["piper-tts==1.8.0"]).await?;
        let mut command = Command::new(python_path());
        command
            .args(["-m", "piper.download_voices", "--data-dir"])
            .arg(root.join("models/piper"));
        command.args(PIPER_VOICES);
        run_checked(command, "download Piper voices").await?;
        tokio::fs::write(root.join(".piper-ready"), b"piper-tts 1.8.0\n").await?;
    }
    if install_kokoro {
        tokio::fs::create_dir_all(root.join("models/kokoro")).await?;
        let kokoro_setup: Result<()> = async {
            let python = ensure_kokoro_venv().await?;
            pip_install_with(&python, &["kokoro-onnx==0.6.1", "soundfile==0.13.1"]).await?;
            download_verified(
                KOKORO_MODEL_URL,
                &root.join("models/kokoro").join(KOKORO_MODEL_FILE),
                KOKORO_MODEL_SHA256,
            )
            .await?;
            download_verified(
                KOKORO_VOICES_URL,
                &root.join("models/kokoro/voices-v1.0.bin"),
                KOKORO_VOICES_SHA256,
            )
            .await?;
            // The marker names the interpreter so readiness checks never have
            // to re-derive it (see `kokoro_python_installed`).
            tokio::fs::write(
                root.join(".kokoro-ready"),
                format!("kokoro-onnx 0.6.1 fp32\npython={}\n", python.display()),
            )
            .await?;
            Ok(())
        }
        .await;
        if let Err(error) = kokoro_setup {
            if engine == "all" && !available_tts_engines().is_empty() {
                warn!(err = %error, "Kokoro unavailable; continuing with the installed TTS engine");
            } else {
                return Err(error);
            }
        }
    }
    if install_pocket {
        pip_install_pocket().await?;
        let cache = root.join("cache/huggingface");
        tokio::fs::create_dir_all(&cache).await?;
        let warmup = format!(
            "from pocket_tts import TTSModel; model = TTSModel.load_model(language={POCKET_LANGUAGE:?}); "
        ) + &format!(
            "[model.get_state_for_audio_prompt(voice) for voice in {POCKET_VOICES:?}]"
        );
        let mut command = Command::new(python_path());
        command.arg("-c").arg(warmup).env("HF_HOME", &cache);
        run_checked(
            command,
            "download the Pocket TTS CPU model and preset voices",
        )
        .await?;
        tokio::fs::write(
            root.join(".pocket-ready"),
            format!("pocket-tts 4.0.1 {POCKET_LANGUAGE}\n"),
        )
        .await?;
    }
    Ok(())
}

async fn pip_install_pocket() -> Result<()> {
    let mut command = Command::new(python_path());
    command.args(["-m", "pip", "install", "--disable-pip-version-check"]);
    // On Linux, PyPI's default torch wheel includes CUDA libraries. Pocket is a
    // CPU workload, so prefer the smaller official CPU wheel index.
    #[cfg(target_os = "linux")]
    command.args(["--extra-index-url", "https://download.pytorch.org/whl/cpu"]);
    command.arg("pocket-tts==4.0.1");
    // Torch is the biggest wheel we install; a small /tmp fails hardest here.
    if let Some(tmp) = speech_tmpdir().await {
        command.env("TMPDIR", tmp);
    }
    run_checked(command, "install the Pocket TTS CPU runtime").await
}

pub async fn setup_stt() -> Result<()> {
    if stt_ready() {
        return Ok(());
    }
    ensure_venv().await?;
    // faster-whisper 1.2.1 still passes `metadata_errors` to av.open;
    // PyAV 19 removed that keyword, so keep the runtime on the compatible API.
    pip_install(&["faster-whisper==1.2.1", "av<19"]).await?;
    let root = speech_root();
    tokio::fs::create_dir_all(root.join("models/whisper")).await?;
    let code = "from faster_whisper import WhisperModel; import sys; WhisperModel('small', device='cpu', compute_type='int8', download_root=sys.argv[1])";
    let mut command = Command::new(python_path());
    command.arg("-c").arg(code).arg(root.join("models/whisper"));
    run_checked(
        command,
        "download the multilingual faster-whisper small model",
    )
    .await?;
    tokio::fs::write(
        root.join(".stt-ready"),
        b"faster-whisper 1.2.1 small multilingual\n",
    )
    .await?;
    Ok(())
}

async fn ensure_venv() -> Result<()> {
    let root = speech_root();
    tokio::fs::create_dir_all(&root).await?;
    let venv = root.join("venv");
    if python_path().is_file() {
        let mut check_pip = Command::new(python_path());
        check_pip
            .args(["-m", "pip", "--version"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if check_pip
            .status()
            .await
            .is_ok_and(|status| status.success())
        {
            return Ok(());
        }

        // A venv can survive a partial install with its interpreter present
        // but pip missing. Repair it in place when ensurepip is available;
        // otherwise rebuild it and clear readiness markers so packages are
        // installed again against the new environment.
        let mut ensure_pip = Command::new(python_path());
        ensure_pip
            .args(["-m", "ensurepip", "--upgrade"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if ensure_pip
            .status()
            .await
            .is_ok_and(|status| status.success())
        {
            return Ok(());
        }

        tokio::fs::remove_dir_all(&venv).await?;
        for marker in [
            ".piper-ready",
            ".kokoro-ready",
            ".pocket-ready",
            ".stt-ready",
        ] {
            let _ = tokio::fs::remove_file(root.join(marker)).await;
        }
    }

    let python = std::env::var_os("C0MPUTE_PYTHON").unwrap_or_else(|| "python3".into());
    let mut command = Command::new(python);
    command.args(["-m", "venv"]).arg(&venv);
    if run_checked(command, "create the isolated speech Python environment")
        .await
        .is_err()
    {
        // Debian and Ubuntu split ensurepip into a separate system package.
        // The stdlib venv module can still create the isolated environment
        // without pip, after which we install a hash-pinned pip wheel locally.
        let _ = tokio::fs::remove_dir_all(&venv).await;
        let python = std::env::var_os("C0MPUTE_PYTHON").unwrap_or_else(|| "python3".into());
        let mut command = Command::new(python);
        command.args(["-m", "venv", "--without-pip"]).arg(&venv);
        run_checked(
            command,
            "create the isolated speech Python environment without system pip",
        )
        .await?;
    }

    if !python_has_pip().await {
        bootstrap_pip().await?;
    }
    if !python_has_pip().await {
        bail!("could not install pip into the speech Python environment");
    }
    Ok(())
}

async fn python_has_pip() -> bool {
    python_has_pip_at(&python_path()).await
}

pub(crate) async fn python_has_pip_at(python: &Path) -> bool {
    let mut check = Command::new(python);
    check
        .args(["-m", "pip", "--version"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    check.status().await.is_ok_and(|status| status.success())
}

async fn bootstrap_pip() -> Result<()> {
    let response = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?
        .get(PIP_BOOTSTRAP_URL)
        .send()
        .await?
        .error_for_status()?;
    let wheel = response.bytes().await?;
    if hex::encode(Sha256::digest(&wheel)) != PIP_BOOTSTRAP_SHA256 {
        bail!("SHA-256 check failed for the pinned pip bootstrap wheel");
    }

    let wheel_path = speech_root().join(PIP_BOOTSTRAP_WHEEL);
    tokio::fs::write(&wheel_path, &wheel).await?;
    let mut locate_site_packages = Command::new(python_path());
    locate_site_packages
        .args([
            "-c",
            "import sysconfig; print(sysconfig.get_paths()['purelib'])",
        ])
        .stdin(Stdio::null());
    let output = locate_site_packages.output().await?;
    if !output.status.success() {
        let _ = tokio::fs::remove_file(&wheel_path).await;
        bail!("could not locate the speech environment's site-packages directory");
    }
    let site_packages = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if site_packages.is_empty() {
        let _ = tokio::fs::remove_file(&wheel_path).await;
        bail!("the speech environment returned an empty site-packages path");
    }
    tokio::fs::create_dir_all(&site_packages).await?;

    let extract_wheel = "import sys, zipfile; zipfile.ZipFile(sys.argv[1]).extractall(sys.argv[2])";
    let mut install = Command::new(python_path());
    install
        .args(["-c", extract_wheel])
        .arg(&wheel_path)
        .arg(&site_packages);
    let result = run_checked(install, "bootstrap pip into the speech Python environment").await;
    let _ = tokio::fs::remove_file(&wheel_path).await;
    result
}

/// The interpreter Kokoro will run under, creating its environment if needed.
///
/// One shared speech venv is the normal case. A box whose only Python is 3.14+
/// gets Kokoro its own venv instead — built from an older interpreter when the
/// box has one, else by `uv`, which fetches a managed CPython itself. Piper,
/// Pocket and Whisper keep the shared venv either way.
async fn ensure_kokoro_venv() -> Result<PathBuf> {
    let existing = kokoro_python();
    if existing.is_file() && python_version(&existing).is_some_and(python_fits_kokoro) {
        return Ok(existing);
    }
    tokio::fs::create_dir_all(speech_root()).await?;
    let dir = kokoro_venv_dir();
    if let Some(interpreter) = kokoro_interpreter() {
        let mut command = Command::new(interpreter);
        command.args(["-m", "venv"]).arg(&dir);
        run_checked(command, "create the Kokoro Python environment").await?;
        let python = venv_python(&dir);
        if !python_has_pip_at(&python).await {
            bail!(
                "the Kokoro Python environment has no pip; install python3-venv \
                 (Debian/Ubuntu: apt install python3-venv) or install uv"
            );
        }
        return Ok(python);
    }
    if let Some(uv) = find_uv() {
        // `--seed` puts pip in the venv, so the install path below is the same
        // one every other engine uses.
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

pub(crate) fn on_path(program: &str) -> bool {
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
pub(crate) fn find_uv() -> Option<PathBuf> {
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
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Free bytes on the filesystem holding the speech directory. Falls back to
/// the largest disk when no mount point matches, since an unusual layout
/// should not block provisioning.
fn free_bytes_for_speech_root() -> u64 {
    free_bytes_for(&speech_root())
}

/// Free bytes on the filesystem holding `root` (the deepest matching mount),
/// or on the largest disk when none matches.
pub(crate) fn free_bytes_for(root: &Path) -> u64 {
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
    let serving = available_tts_engines().contains(&"kokoro");
    if serving && kokoro_full_model_present() {
        return true;
    }
    // A node that still has only the int8 model keeps serving it; the upgrade
    // below is best effort and never takes TTS away.
    if !config.auto_install_enabled() {
        tracing::debug!("kokoro auto-provisioning is off; not installing");
        return serving;
    }
    let free = free_bytes_for_speech_root();
    let required = config.min_free_disk_gb.saturating_mul(1024 * 1024 * 1024);
    if free < required {
        warn!(
            free_mb = free / 1024 / 1024,
            required_mb = required / 1024 / 1024,
            "skipping kokoro install: not enough free disk"
        );
        return serving;
    }
    if in_install_backoff() {
        tracing::debug!("kokoro install failed recently; waiting out the backoff");
        return serving;
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
            warn!(
                err = %error,
                serving,
                "could not install the full Kokoro model; serving whatever is already installed. \
                 Turn provisioning off with `c0mpute tts auto off`."
            );
            serving
        }
    }
}

async fn pip_install(packages: &[&str]) -> Result<()> {
    pip_install_with(&python_path(), packages).await
}

async fn pip_install_with(python: &Path, packages: &[&str]) -> Result<()> {
    let mut command = Command::new(python);
    command
        .args(["-m", "pip", "install", "--disable-pip-version-check"])
        .args(packages);
    if let Some(tmp) = speech_tmpdir().await {
        command.env("TMPDIR", tmp);
    }
    run_checked(command, "install speech runtime packages").await
}

/// A scratch directory beside the models, used instead of `/tmp`.
///
/// Docker's default `/tmp` is a 64 MB tmpfs mounted `noexec`, and that breaks
/// Kokoro twice over — both observed on dev2's speech gateway container on
/// 2026-11-05:
///
/// - **Installing**: onnxruntime needs more than 64 MB unpacked, so pip dies
///   with `OSError: [Errno 28] No space left on device` while the volume
///   holding the models has tens of gigabytes free.
/// - **Speaking**: `espeakng-loader` extracts `libespeak-ng.so` to a temp dir
///   and `dlopen`s it, which `noexec` refuses —
///   `failed to map segment from shared object`. The install succeeds and the
///   model loads; the failure only appears at the first word.
///
/// The speech root is on the same volume as the models, so it is sized for
/// them and mounted normally. None means "leave the default alone": if we
/// cannot create the directory, the default is still better than failing.
pub(crate) async fn speech_tmpdir() -> Option<PathBuf> {
    let dir = speech_root().join("tmp");
    tokio::fs::create_dir_all(&dir).await.ok()?;
    Some(dir)
}

pub(crate) async fn run_checked(mut command: Command, task: &str) -> Result<()> {
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
    crate::resources::apply_job_limits(&mut command);
    command.env("HF_HOME", speech_root().join("cache/huggingface"));
    // espeak-ng is extracted to a temp dir and dlopen'd, so the temp dir has
    // to allow execution — Docker's default /tmp does not. See speech_tmpdir.
    if let Some(tmp) = speech_tmpdir().await {
        command.env("TMPDIR", tmp);
    }
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
