//! `embed.text`: CPU text embeddings for buyers.
//!
//! The runtime follows the speech pattern rather than linking an ML library
//! into the binary (which would break the cross-compiled releases): the node
//! provisions a per-user Python virtualenv with `fastembed` (ONNX Runtime,
//! CPU) under `<data_dir>/embed`, downloads the model once, and runs a small
//! embedded script per job. Provisioning runs in the background on worker
//! start; the role is advertised only once a model answers.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use c0mpute_net::topics::JobStatus;
use c0mpute_net::Libp2pNetwork;
use c0mpute_store::ChunkStore;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::runner::{AcceptedJob, WorkerLoad, publish_receipt};

pub const WORKLOAD: &str = "embed.text";
/// Capability tag: this node has a working embedding runtime.
pub const ROLE_TAG: &str = "c0mpute:role:embed";
/// Prefix of the per-model tag (`c0mpute:embed:model:<slug>`).
pub const MODEL_TAG_PREFIX: &str = "c0mpute:embed:model:";
pub const MAX_TEXTS: usize = 256;
pub const MAX_TEXT_CHARS: usize = 8_192;
/// Total text in one job; the serialized spec must also fit
/// [`crate::workflow::MAX_SPEC_BYTES`].
pub const MAX_TOTAL_BYTES: usize = 1024 * 1024;
/// 256 vectors x 768 dims as JSON is ~3 MB; leave room.
pub const MAX_RESULT_BYTES: usize = 16 * 1024 * 1024;
const FASTEMBED: &str = "fastembed==0.9.1";
const JOB_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const INSTALL_RETRY_BACKOFF_SECS: u64 = 6 * 60 * 60;
/// What `uv` is asked for when the box has no usable interpreter.
const UV_PYTHON: &str = "3.12";
const PY_MIN: (u32, u32) = (3, 10);
const PYTHONS: &[&str] = &["python3.13", "python3.12", "python3.11", "python3.10"];
const WORKER: &str = include_str!("embed_worker.py");

/// A model this workload can serve.
#[derive(Debug, PartialEq, Eq)]
pub struct Model {
    /// fastembed / Hugging Face id.
    pub id: &'static str,
    /// Short name used in tags and the CLI.
    pub slug: &'static str,
    pub dims: usize,
    /// Provisioned on every node by default.
    pub default_install: bool,
}

pub const MODELS: &[Model] = &[
    Model {
        id: "BAAI/bge-small-en-v1.5",
        slug: "bge-small-en-v1.5",
        dims: 384,
        default_install: true,
    },
    Model {
        id: "nomic-ai/nomic-embed-text-v1.5",
        slug: "nomic-embed-text-v1.5",
        dims: 768,
        default_install: false,
    },
];

pub fn default_model() -> &'static Model {
    &MODELS[0]
}

/// Look a model up by id or slug (case-insensitive); `None` means default.
pub fn resolve_model(name: Option<&str>) -> Option<&'static Model> {
    match name.map(str::trim).filter(|n| !n.is_empty()) {
        None => Some(default_model()),
        Some(n) => MODELS
            .iter()
            .find(|m| m.id.eq_ignore_ascii_case(n) || m.slug.eq_ignore_ascii_case(n)),
    }
}

pub fn model_tag(model: &Model) -> String {
    format!("{MODEL_TAG_PREFIX}{}", model.slug)
}

/// What a buyer asks for.
#[derive(Debug, Clone, Deserialize)]
pub struct EmbedSpec {
    pub v: u32,
    #[serde(default)]
    pub model: Option<String>,
    pub texts: Vec<String>,
}

pub fn parse_spec(spec: &Value) -> std::result::Result<(EmbedSpec, &'static Model), String> {
    let s: EmbedSpec = serde_json::from_value(spec.clone())
        .map_err(|e| format!("invalid embed.text spec: {e}"))?;
    if s.v != 1 {
        return Err(format!("unsupported embed.text spec version {}", s.v));
    }
    let model = resolve_model(s.model.as_deref()).ok_or_else(|| {
        let known: Vec<&str> = MODELS.iter().map(|m| m.slug).collect();
        format!("unknown embedding model; known: {}", known.join(", "))
    })?;
    if s.texts.is_empty() || s.texts.len() > MAX_TEXTS {
        return Err(format!("texts must hold 1 to {MAX_TEXTS} strings"));
    }
    if s.texts.iter().any(|t| t.chars().count() > MAX_TEXT_CHARS) {
        return Err(format!(
            "each text must be at most {MAX_TEXT_CHARS} characters"
        ));
    }
    if s.texts.iter().map(String::len).sum::<usize>() > MAX_TOTAL_BYTES {
        return Err(format!("texts total more than {MAX_TOTAL_BYTES} bytes"));
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
    Ok((s, model))
}

/// The buyer's (and the worker's own) check on a result: one finite vector
/// per text, each of the model's dimension.
pub fn validate_result(
    value: &Value,
    texts: usize,
    model: &Model,
) -> std::result::Result<(), String> {
    if value.get("v").and_then(Value::as_u64) != Some(1) {
        return Err("result is not v1".into());
    }
    let dims = value
        .get("dims")
        .and_then(Value::as_u64)
        .ok_or("result has no dims")? as usize;
    if dims != model.dims {
        return Err(format!(
            "dims {dims} does not match {} ({})",
            model.slug, model.dims
        ));
    }
    let vectors = value
        .get("vectors")
        .and_then(Value::as_array)
        .ok_or("result has no vectors")?;
    if vectors.len() != texts {
        return Err(format!("{} vectors for {texts} texts", vectors.len()));
    }
    for vector in vectors {
        let vector = vector.as_array().ok_or("a vector is not an array")?;
        if vector.len() != dims {
            return Err("a vector has the wrong length".into());
        }
        if !vector
            .iter()
            .all(|x| x.as_f64().is_some_and(f64::is_finite))
        {
            return Err("a vector holds a non-finite or non-numeric value".into());
        }
    }
    Ok(())
}

// ─── runtime: paths and readiness ────────────────────────────────────────

pub fn embed_root() -> PathBuf {
    crate::config::data_dir()
        .unwrap_or_else(|| PathBuf::from("./c0mpute-data"))
        .join("embed")
}

fn venv_dir() -> PathBuf {
    embed_root().join("venv")
}

pub fn python_path() -> PathBuf {
    crate::speech::venv_python(&venv_dir())
}

fn cache_dir() -> PathBuf {
    embed_root().join("models")
}

fn ready_marker(model: &Model) -> PathBuf {
    embed_root().join(format!(".ready-{}", model.slug))
}

fn retry_marker() -> PathBuf {
    embed_root().join(".retry-after")
}

/// Models this node can serve right now. Two file checks per model, so it is
/// cheap enough for every capability tick.
pub fn ready_models() -> Vec<&'static Model> {
    if !python_path().is_file() {
        return Vec::new();
    }
    MODELS
        .iter()
        .filter(|m| ready_marker(m).is_file())
        .collect()
}

pub fn model_ready(model: &Model) -> bool {
    python_path().is_file() && ready_marker(model).is_file()
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

/// Make sure the default model is served, installing it if missing.
/// Idempotent and never fatal: a node that cannot provision just does not
/// advertise `embed`. True when at least one model is ready.
pub async fn ensure(config: &crate::config::EmbedConfig) -> bool {
    if model_ready(default_model()) {
        return true;
    }
    if !config.auto_install_enabled() {
        return !ready_models().is_empty();
    }
    let free = crate::speech::free_bytes_for(&embed_root());
    let required = config.min_free_disk_gb.saturating_mul(1024 * 1024 * 1024);
    if free < required {
        warn!(
            free_mb = free / 1024 / 1024,
            required_mb = required / 1024 / 1024,
            "skipping embeddings install: not enough free disk"
        );
        return !ready_models().is_empty();
    }
    if in_install_backoff() {
        tracing::debug!("embeddings install failed recently; waiting out the backoff");
        return !ready_models().is_empty();
    }
    info!("installing fastembed + bge-small-en-v1.5 so this node can serve embed.text");
    match setup(&[default_model()]).await {
        Ok(()) => {
            let _ = tokio::fs::remove_file(retry_marker()).await;
            info!(root = %embed_root().display(), "embeddings ready");
            true
        }
        Err(error) => {
            let _ = tokio::fs::create_dir_all(embed_root()).await;
            let retry_after = now_secs().saturating_add(INSTALL_RETRY_BACKOFF_SECS);
            let _ = tokio::fs::write(retry_marker(), retry_after.to_string()).await;
            warn!(
                err = %format!("{error:#}"),
                "could not install the embeddings runtime; this node will not serve embed.text \
                 (opt out with C0MPUTE_EMBED_AUTO=0)"
            );
            !ready_models().is_empty()
        }
    }
}

/// Install fastembed and download + verify each model. `c0mpute embed setup`.
pub async fn setup(models: &[&'static Model]) -> Result<()> {
    let todo: Vec<&Model> = models.iter().copied().filter(|m| !model_ready(m)).collect();
    if todo.is_empty() {
        return Ok(());
    }
    let python = ensure_venv().await?;
    let mut install = Command::new(&python);
    install.args([
        "-m",
        "pip",
        "install",
        "--disable-pip-version-check",
        FASTEMBED,
    ]);
    install.env("TMPDIR", tmpdir().await?);
    crate::speech::run_checked(install, "install fastembed").await?;
    for model in todo {
        // Downloads the model into the cache and embeds one string, so a model
        // that loads but answers the wrong shape is never marked ready.
        let out = run_worker(
            "warmup",
            &json!({ "model": model.id }),
            false,
            Duration::from_secs(30 * 60),
        )
        .await
        .with_context(|| format!("download and load {}", model.id))?;
        let dims = out.get("dims").and_then(Value::as_u64).unwrap_or(0) as usize;
        if dims != model.dims {
            bail!("{} answered {dims} dims, expected {}", model.id, model.dims);
        }
        tokio::fs::write(ready_marker(model), format!("{FASTEMBED} {}\n", model.id)).await?;
    }
    Ok(())
}

async fn tmpdir() -> Result<PathBuf> {
    // Not /tmp: a container's /tmp is often a tiny noexec tmpfs, and the
    // onnxruntime wheel alone is bigger than that (see speech_tmpdir).
    let dir = embed_root().join("tmp");
    tokio::fs::create_dir_all(&dir).await?;
    Ok(dir)
}

/// An interpreter fastembed supports (3.10+).
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
        // A half-built venv: start over, and forget what it had installed.
        tokio::fs::remove_dir_all(&dir).await?;
        for m in MODELS {
            let _ = tokio::fs::remove_file(ready_marker(m)).await;
        }
    }
    tokio::fs::create_dir_all(embed_root()).await?;
    if let Some(interp) = interpreter() {
        let mut command = Command::new(interp);
        command.args(["-m", "venv"]).arg(&dir);
        if crate::speech::run_checked(command, "create the embeddings Python environment")
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
        crate::speech::run_checked(command, "create the embeddings Python environment with uv")
            .await?;
        return Ok(python);
    }
    bail!(
        "embeddings need Python 3.10+ with venv support (Debian/Ubuntu: apt install python3-venv), \
         or uv (curl -LsSf https://astral.sh/uv/install.sh | sh), or C0MPUTE_PYTHON pointing at one"
    )
}

/// Run the embedded worker script and parse its stdout.
async fn run_worker(
    action: &str,
    request: &Value,
    offline: bool,
    timeout: Duration,
) -> Result<Value> {
    let mut command = Command::new(python_path());
    command.arg("-c").arg(WORKER).arg(cache_dir()).arg(action);
    crate::resources::apply_job_limits(&mut command);
    command
        .env("HF_HOME", embed_root().join("cache/huggingface"))
        .env("TMPDIR", tmpdir().await?)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if offline {
        // Jobs only ever use a model that setup already downloaded.
        command.env("HF_HUB_OFFLINE", "1");
    }
    let mut child = command
        .spawn()
        .context("start the embeddings runtime; run `c0mpute embed setup`")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&serde_json::to_vec(request)?).await?;
    }
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .context("embeddings runtime timed out")??;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "embeddings runtime failed: {}",
            stderr.lines().last().unwrap_or("runtime error")
        );
    }
    if output.stdout.len() > MAX_RESULT_BYTES {
        bail!("embeddings output is over the {MAX_RESULT_BYTES}-byte limit");
    }
    serde_json::from_slice(&output.stdout).context("embeddings runtime printed invalid JSON")
}

/// Embed `texts` with `model` (which must be ready) and build the result blob.
pub async fn embed(model: &'static Model, texts: &[String]) -> Result<Value> {
    if !model_ready(model) {
        bail!("{} is not installed on this node", model.id);
    }
    let started = Instant::now();
    let out = run_worker(
        "embed",
        &json!({ "model": model.id, "texts": texts }),
        true,
        JOB_TIMEOUT,
    )
    .await?;
    let result = json!({
        "v": 1,
        "ok": true,
        "model": model.id,
        "dims": out.get("dims").cloned().unwrap_or(Value::Null),
        "vectors": out.get("vectors").cloned().unwrap_or(Value::Null),
        "duration_ms": started.elapsed().as_millis() as u64,
    });
    validate_result(&result, texts.len(), model).map_err(anyhow::Error::msg)?;
    Ok(result)
}

// ─── worker runner ───────────────────────────────────────────────────────

pub fn spawn_embed_runner(
    net: Arc<Libp2pNetwork>,
    store: ChunkStore,
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
            let load = load.clone();
            tokio::spawn(async move {
                let _slot = slot;
                let started = Instant::now();
                let job_id = job.accept.job_id.clone();
                match crate::job_control::cancellable(&job.accept.job_id, run_embed_job(&net, &store, &job)).await {
                    Ok(hash) => {
                        load.record_job(started.elapsed());
                        info!(%job_id, %hash, "embed runner: job complete");
                        publish_receipt(&net, &job.accept, Some(hash), JobStatus::Completed).await;
                    }
                    Err(error) => {
                        warn!(%job_id, err = %error, "embed runner: job failed");
                        publish_receipt(&net, &job.accept, None, JobStatus::Failed).await;
                    }
                }
            });
        }
    });
    tx
}

async fn run_embed_job(
    net: &Libp2pNetwork,
    store: &ChunkStore,
    job: &AcceptedJob,
) -> Result<String> {
    let spec = crate::workflow::resolve_spec(net, job).await?;
    let (spec, model) = parse_spec(&spec).map_err(anyhow::Error::msg)?;
    let result = embed(model, &spec.texts).await?;
    let bytes = serde_json::to_vec(&result)?;
    if bytes.len() > MAX_RESULT_BYTES {
        bail!("embeddings result is over the {MAX_RESULT_BYTES}-byte limit");
    }
    crate::pieces::publish_result_bytes(net, store, &job.accept, &bytes)
        .await
        .context("publish embeddings result")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models() {
        assert_eq!(resolve_model(None).unwrap().dims, 384);
        assert_eq!(resolve_model(Some("")).unwrap().slug, "bge-small-en-v1.5");
        assert_eq!(
            resolve_model(Some("baai/bge-small-en-v1.5")).unwrap().dims,
            384
        );
        assert_eq!(
            resolve_model(Some("nomic-embed-text-v1.5")).unwrap().dims,
            768
        );
        assert!(resolve_model(Some("text-embedding-3-small")).is_none());
        assert_eq!(
            model_tag(default_model()),
            "c0mpute:embed:model:bge-small-en-v1.5"
        );
        assert_eq!(MODELS.iter().filter(|m| m.default_install).count(), 1);
    }

    #[test]
    fn spec_validation() {
        let (s, m) = parse_spec(&json!({"v":1,"texts":["a","b"]})).unwrap();
        assert_eq!(s.texts.len(), 2);
        assert_eq!(m.slug, "bge-small-en-v1.5");
        assert!(
            parse_spec(&json!({"v":2,"texts":["a"]}))
                .unwrap_err()
                .contains("version")
        );
        assert!(parse_spec(&json!({"v":1,"texts":[]})).is_err());
        assert!(
            parse_spec(&json!({"v":1,"texts":["a"],"model":"nope"}))
                .unwrap_err()
                .contains("unknown")
        );
        let many: Vec<String> = (0..=MAX_TEXTS).map(|i| i.to_string()).collect();
        assert!(parse_spec(&json!({"v":1,"texts":many})).is_err());
        assert!(parse_spec(&json!({"v":1,"texts":["x".repeat(MAX_TEXT_CHARS + 1)]})).is_err());
        assert!(parse_spec(&json!({"v":1,"texts":["é".repeat(MAX_TEXT_CHARS)]})).is_ok());
        // 200 texts of 6000 chars = 1.2 MB: over the total.
        let big: Vec<String> = (0..200).map(|_| "y".repeat(6000)).collect();
        assert!(
            parse_spec(&json!({"v":1,"texts":big}))
                .unwrap_err()
                .contains("total")
        );
        assert!(parse_spec(&json!({"v":1})).is_err());
    }

    #[test]
    fn result_validation() {
        let m = &MODELS[0];
        let vec = vec![0.1f64; 384];
        let ok =
            json!({"v":1,"ok":true,"model":m.id,"dims":384,"vectors":[vec.clone(), vec.clone()]});
        assert!(validate_result(&ok, 2, m).is_ok());
        assert!(validate_result(&ok, 3, m).is_err());
        assert!(validate_result(&ok, 2, &MODELS[1]).is_err());
        let short = json!({"v":1,"dims":384,"vectors":[vec![0.1; 383]]});
        assert!(validate_result(&short, 1, m).is_err());
        let mut bad = vec![json!(0.1); 384];
        bad[7] = json!("NaN");
        assert!(validate_result(&json!({"v":1,"dims":384,"vectors":[bad]}), 1, m).is_err());
        assert!(validate_result(&json!({"v":1,"dims":384}), 1, m).is_err());
        assert!(validate_result(&json!({"v":2,"dims":384,"vectors":[]}), 0, m).is_err());
    }

    /// Provisions fastembed and the default model for real (network, ~200 MB),
    /// so it runs only with `C0MPUTE_TEST_EMBED_RUNTIME=1`; point HOME at a
    /// scratch directory when you do.
    #[tokio::test]
    async fn runtime_end_to_end() {
        if std::env::var("C0MPUTE_TEST_EMBED_RUNTIME").ok().as_deref() != Some("1") {
            eprintln!("skipping: set C0MPUTE_TEST_EMBED_RUNTIME=1 to provision and run fastembed");
            return;
        }
        let model = default_model();
        setup(&[model]).await.expect("setup");
        assert!(ready_models().contains(&model));
        let texts = vec![
            "The cat sat on the mat.".to_string(),
            "A cat is sitting on a rug.".to_string(),
            "Quarterly revenue grew eleven percent.".to_string(),
        ];
        let result = embed(model, &texts).await.expect("embed");
        validate_result(&result, 3, model).unwrap();
        let v: Vec<Vec<f64>> = serde_json::from_value(result["vectors"].clone()).unwrap();
        let cos = |a: &[f64], b: &[f64]| {
            let dot: f64 = a.iter().zip(b).map(|(x, y)| x * y).sum();
            let n = |x: &[f64]| x.iter().map(|v| v * v).sum::<f64>().sqrt();
            dot / (n(a) * n(b))
        };
        assert!(
            cos(&v[0], &v[1]) > cos(&v[0], &v[2]),
            "similar sentences should be closer"
        );
    }
}
