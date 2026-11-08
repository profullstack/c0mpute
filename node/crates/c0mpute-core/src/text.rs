//! Infernet text fine-tunes on c0mpute: workloads `infernet.text.train` and
//! `infernet.text.infer` (answers from a trained adapter, further down).
//!
//! A buyer has a job file (`@infernetprotocol/training` job config, `kind:
//! "lora"`) and a JSONL corpus. `c0mpute infernet text train` serves the
//! corpus from a token-gated transfer server, auctions the job to workers that
//! advertise `infernet:text:train`, and shows the winner's training live: the
//! worker relays every training-protocol v1 event (`meta`, `step`, `eval`,
//! `checkpoint`, `done`) to the transfer server as it happens. The worker then
//! uploads the LoRA adapter, and the buyer keeps it only if its sha256 matches
//! the receipt's `output_hash`.
//!
//! The training itself is Infernet's (`infernet model train --job <job.json>
//! --json`, the `peft` backend). c0mpute only moves the data, the progress and
//! the result, over the same offer → bid → accept → receipt protocol as every
//! other workload. One job per node at a time.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use c0mpute_net::Libp2pNetwork;
use c0mpute_net::topics::JobStatus;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{Semaphore, mpsc};
use tracing::{info, warn};

use crate::buyer::JobAuction;
use crate::runner::{AcceptedJob, publish_receipt};
use crate::temporal::client::auction_with_mesh_retry;
use crate::temporal::http::{SlotEvent, TransferServer};
use crate::temporal::node::{NetOptions, spawn_node};

pub const TRAIN_WORKLOAD: &str = "infernet.text.train";
pub const KINDS: [&str; 2] = ["lora", "peft"];
/// What the worker uploads; the buyer accepts nothing else.
pub const ARTIFACTS: [&str; 3] = ["adapter_model.safetensors", "adapter_config.json", "manifest.json"];
const MAX_DATASET: u64 = 512 << 20;
const SLOT: &str = "adapter";

pub fn tag(workload: &str) -> String {
    workload.replace('.', ":")
}

/// `JobOffer.spec_inline` for `infernet.text.train`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TextTrainSpec {
    /// The job config. The worker rewrites `dataset.url` and
    /// `output.destination_url` to local paths; everything else is Infernet's.
    pub job: Value,
    pub dataset_url: String,
    /// `sha256:<hex>` of the corpus; the worker refuses a mismatch.
    pub dataset_hash: String,
    pub artifact_upload_url: String,
    /// Where the worker posts each training event as it happens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events_url: Option<String>,
}

fn is_http(u: &str) -> bool {
    u.starts_with("http://") || u.starts_with("https://")
}

impl TextTrainSpec {
    pub fn validate(&self) -> Result<()> {
        let kind = self.job.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        if !KINDS.contains(&kind) {
            bail!("job.kind must be one of {KINDS:?}, got {kind:?}");
        }
        let base = self.job.get("base_model").and_then(|v| v.as_str()).unwrap_or("");
        // A Hugging Face repo id (org/name): never a path or a URL on the worker.
        if base.is_empty() || base.len() > 200 || base.starts_with('/') || base.contains("..") || base.contains("://") {
            bail!("job.base_model must be a model repo id like org/name");
        }
        if !crate::temporal::is_sha256_ref(&self.dataset_hash) {
            bail!("dataset_hash must be sha256:<64 lowercase hex>");
        }
        if !is_http(&self.dataset_url) || !is_http(&self.artifact_upload_url) || self.events_url.as_deref().is_some_and(|u| !is_http(u)) {
            bail!("dataset, upload and events URLs must be http(s)");
        }
        Ok(())
    }
}

pub fn sha256_hex(b: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(b)))
}

// ── worker ──────────────────────────────────────────────────────────────

/// Is the peft runtime installed (`infernet model runtime status --peft --json`)?
pub async fn probe(infernet: &Path) -> Result<Value> {
    let out = tokio::time::timeout(
        Duration::from_secs(120),
        tokio::process::Command::new(crate::live_bin(infernet)).args(["model", "runtime", "status", "--peft", "--json"]).stdin(std::process::Stdio::null()).output(),
    )
    .await
    .context("infernet model runtime status --peft timed out")?
    .with_context(|| format!("run {}", infernet.display()))?;
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or(json!({"installed": false}));
    if v["installed"] != json!(true) {
        bail!("the peft runtime is not installed: run `infernet model runtime setup --peft --yes` (needs Infernet >= 0.1.79)");
    }
    Ok(v)
}

/// Total RAM in MB from /proc/meminfo (0 where unavailable: no RAM tags).
pub fn local_ram_mb() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|m| m.lines().find(|l| l.starts_with("MemTotal:")).and_then(|l| l.split_whitespace().nth(1)).and_then(|kb| kb.parse::<u64>().ok()))
        .map(|kb| kb / 1024)
        .unwrap_or(0)
}

/// Tags a text-training worker advertises.
pub fn worker_tags(runtime: &Value, ram_mb: u64) -> Vec<String> {
    let mut t = vec![tag(TRAIN_WORKLOAD), "infernet:text:runtime:peft".into(), "infernet:text:device:cpu".into()];
    if runtime["cuda"] == json!(true) {
        t.push("infernet:text:device:cuda".into());
    }
    for gb in crate::temporal::caps::RAM_BUCKETS_GB {
        if ram_mb >= gb * 1024 {
            t.push(format!("c0mpute:ram:{gb}gb+"));
        }
    }
    t
}

async fn download(url: &str, dst: &Path, want: &str) -> Result<()> {
    let resp = reqwest::get(url).await?.error_for_status()?;
    if resp.content_length().is_some_and(|n| n > MAX_DATASET) {
        bail!("dataset larger than {} MiB", MAX_DATASET >> 20);
    }
    let bytes = resp.bytes().await?;
    if bytes.len() as u64 > MAX_DATASET {
        bail!("dataset larger than {} MiB", MAX_DATASET >> 20);
    }
    let got = sha256_hex(&bytes);
    if got != want {
        bail!("dataset hash {got} differs from the offer's {want}");
    }
    tokio::fs::write(dst, &bytes).await?;
    Ok(())
}

/// Run one accepted job: fetch the corpus, run `infernet model train --job
/// --json`, relay every event, return the adapter dir and its weights hash.
pub async fn run_train(infernet: &Path, job_dir: &Path, spec: &TextTrainSpec) -> Result<(PathBuf, String)> {
    spec.validate()?;
    tokio::fs::create_dir_all(job_dir).await?;
    let dataset = job_dir.join("dataset.jsonl");
    download(&spec.dataset_url, &dataset, &spec.dataset_hash).await.context("fetch dataset")?;
    let mut job = spec.job.clone();
    job["dataset"] = json!({"url": "dataset.jsonl", "sha256": spec.dataset_hash.trim_start_matches("sha256:")});
    job["output"] = json!({"destination_url": "adapter"});
    let job_path = job_dir.join("job.json");
    tokio::fs::write(&job_path, serde_json::to_vec_pretty(&job)?).await?;
    let timeout_s = job["limits"]["timeout_s"].as_u64().unwrap_or(4 * 3600).clamp(60, 7 * 86_400);

    let mut command = tokio::process::Command::new(crate::live_bin(infernet));
    command
        .args(["model", "train", "--json", "--job"])
        .arg(&job_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    crate::resources::apply_job_limits(&mut command);
    let mut child = command.spawn().context("spawn infernet model train")?;
    let stdout = child.stdout.take().context("no stdout")?;
    let stderr = child.stderr.take().context("no stderr")?;
    let err_tail = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        let mut last = String::new();
        while let Ok(Some(l)) = lines.next_line().await {
            if !l.trim().is_empty() {
                last = l;
            }
        }
        last
    });

    // Events go to the buyer from their own task so a slow link never stalls training.
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<Value>();
    let relay = spec.events_url.clone().map(|url| {
        tokio::spawn(async move {
            let c = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().unwrap_or_default();
            while let Some(ev) = ev_rx.recv().await {
                if let Err(e) = c.post(&url).json(&ev).send().await {
                    warn!(err = %e, "text: could not relay a training event");
                }
            }
        })
    });

    let read = async {
        let mut lines = BufReader::new(stdout).lines();
        let mut checkpoint: Option<Value> = None;
        let mut done: Option<Value> = None;
        while let Some(line) = lines.next_line().await? {
            let Ok(ev) = serde_json::from_str::<Value>(line.trim()) else { continue };
            if ev["v"] != json!(1) {
                continue;
            }
            match ev["type"].as_str() {
                Some("checkpoint") => checkpoint = Some(ev.clone()),
                Some("done") => done = Some(ev.clone()),
                _ => {}
            }
            if spec.events_url.is_some() {
                let _ = ev_tx.send(ev);
            }
        }
        Ok::<_, anyhow::Error>((checkpoint, done))
    };
    let (checkpoint, done) = tokio::time::timeout(Duration::from_secs(timeout_s), read)
        .await
        .map_err(|_| anyhow::anyhow!("E_TIMEOUT: training exceeded {timeout_s}s"))??;
    let status = child.wait().await?;
    drop(ev_tx);
    if let Some(r) = relay {
        let _ = tokio::time::timeout(Duration::from_secs(30), r).await;
    }
    let last_err = err_tail.await.unwrap_or_default();
    if done.as_ref().and_then(|d| d["reason"].as_str()) != Some("complete") || !status.success() {
        bail!("infernet model train did not complete ({}): {last_err}", done.and_then(|d| d["reason"].as_str().map(String::from)).unwrap_or_else(|| format!("exit {status}")));
    }
    let checkpoint = checkpoint.context("no checkpoint event")?;
    let dir = job_dir.join("adapter");
    let actual = crate::temporal::worker::file_sha256(&dir.join("adapter_model.safetensors"))?;
    if checkpoint["model"].as_str() != Some(actual.as_str()) {
        bail!("adapter hash {actual} differs from the checkpoint's {}", checkpoint["model"]);
    }
    Ok((dir, actual))
}

async fn upload(base: &str, dir: &Path) -> Result<()> {
    let c = reqwest::Client::new();
    for f in ARTIFACTS {
        let p = dir.join(f);
        if p.exists() {
            c.put(format!("{base}/{f}")).body(tokio::fs::read(&p).await?).send().await?.error_for_status()?;
        }
    }
    Ok(())
}

pub fn spawn_train_runner(net: Arc<Libp2pNetwork>, cache_root: PathBuf, infernet: PathBuf, busy: Arc<AtomicBool>, slots: Arc<Semaphore>) -> mpsc::Sender<AcceptedJob> {
    let (tx, mut rx) = mpsc::channel::<AcceptedJob>(8);
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let Ok(_slot) = slots.clone().acquire_owned().await else { return };
            let accept = job.accept.clone();
            busy.store(true, Ordering::Relaxed);
            let result: Result<String> = async {
                let spec: TextTrainSpec = serde_json::from_value(accept.spec_inline.clone().context("accept lacks spec_inline")?)?;
                let job_dir = cache_root.join("text").join("jobs").join(&accept.job_id);
                let (dir, hash) = run_train(&infernet, &job_dir, &spec).await?;
                upload(&spec.artifact_upload_url, &dir).await.context("upload adapter")?;
                Ok(hash)
            }
            .await;
            busy.store(false, Ordering::Relaxed);
            match result {
                Ok(hash) => {
                    info!(job_id = %accept.job_id, adapter = %hash, "text: training complete");
                    publish_receipt(&net, &accept, Some(hash), JobStatus::Completed).await;
                }
                Err(e) => {
                    warn!(job_id = %accept.job_id, err = %format!("{e:#}"), "text: training failed");
                    publish_receipt(&net, &accept, None, JobStatus::Failed).await;
                }
            }
        }
    });
    tx
}

/// Bid only on valid text-train offers, and only while idle.
pub fn train_offer_filter(busy: Arc<AtomicBool>) -> crate::dispatch::OfferFilter {
    Arc::new(move |offer| {
        !busy.load(Ordering::Relaxed)
            && offer
                .spec_inline
                .as_ref()
                .and_then(|v| serde_json::from_value::<TextTrainSpec>(v.clone()).ok())
                .is_some_and(|s| s.validate().is_ok())
    })
}

// ── inference (infernet.text.infer) ─────────────────────────────────────

pub const INFER_WORKLOAD: &str = "infernet.text.infer";
/// The adapter files a chat request carries (bundled, base64) and the worker caches.
pub const ADAPTER_FILES: [&str; 3] = ["adapter_model.safetensors", "adapter_config.json", "manifest.json"];
/// What the worker uploads back.
pub const ANSWER_ARTIFACTS: [&str; 1] = ["answer.json"];
const MAX_BUNDLE: u64 = 128 << 20;
const MAX_MESSAGES: usize = 24;
const MAX_PROMPT_BYTES: usize = 32 * 1024;
const ANSWER_SLOT: &str = "answer";

/// `JobOffer.spec_inline` for `infernet.text.infer`: one answer from a base
/// model + LoRA adapter. The adapter travels with the request (a bundle on
/// the buyer's transfer server) and is cached by hash, so any worker can
/// serve any adapter; the buyer does retrieval, the worker only generates.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TextInferSpec {
    /// `sha256:` of adapter_model.safetensors.
    pub adapter_hash: String,
    pub base_model: String,
    pub messages: Vec<Value>,
    pub max_new_tokens: u32,
    /// `{"files": {"<name>": "<base64>"}}` for the adapter, fetched only on a cache miss.
    pub adapter_url: String,
    /// Base URL the worker PUTs `answer.json` to.
    pub answer_upload_url: String,
}

fn valid_repo_id(base: &str) -> bool {
    !(base.is_empty() || base.len() > 200 || base.starts_with('/') || base.contains("..") || base.contains("://"))
}

impl TextInferSpec {
    pub fn validate(&self) -> Result<()> {
        if !crate::temporal::is_sha256_ref(&self.adapter_hash) {
            bail!("adapter_hash must be sha256:<64 lowercase hex>");
        }
        if !valid_repo_id(&self.base_model) {
            bail!("base_model must be a model repo id like org/name");
        }
        if self.messages.is_empty() || self.messages.len() > MAX_MESSAGES {
            bail!("messages must hold 1..={MAX_MESSAGES} entries");
        }
        let mut bytes = 0;
        for m in &self.messages {
            let role = m["role"].as_str().unwrap_or("");
            if !["system", "user", "assistant"].contains(&role) {
                bail!("message role must be system, user or assistant");
            }
            bytes += m["content"].as_str().context("message content must be a string")?.len();
        }
        if bytes > MAX_PROMPT_BYTES {
            bail!("messages exceed {MAX_PROMPT_BYTES} bytes");
        }
        if !(1..=1024).contains(&self.max_new_tokens) {
            bail!("max_new_tokens must be 1..=1024");
        }
        if !is_http(&self.adapter_url) || !is_http(&self.answer_upload_url) {
            bail!("adapter and answer URLs must be http(s)");
        }
        Ok(())
    }
}

/// The adapter files as the bundle a chat request serves.
pub fn adapter_bundle(dir: &Path) -> Result<(Vec<u8>, String, Value)> {
    use base64::Engine;
    let mut files = serde_json::Map::new();
    for f in ADAPTER_FILES {
        let p = dir.join(f);
        if p.exists() {
            files.insert(f.into(), json!(base64::engine::general_purpose::STANDARD.encode(std::fs::read(&p)?)));
        }
    }
    let weights = std::fs::read(dir.join("adapter_model.safetensors")).with_context(|| format!("{} has no adapter_model.safetensors", dir.display()))?;
    let manifest: Value = std::fs::read(dir.join("manifest.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(json!({}));
    Ok((serde_json::to_vec(&json!({"files": files}))?, sha256_hex(&weights), manifest))
}

/// Fetch, verify and unpack an adapter bundle into `dir` (a cache entry).
async fn fetch_adapter(url: &str, want: &str, dir: &Path) -> Result<()> {
    use base64::Engine;
    let resp = reqwest::get(url).await?.error_for_status()?;
    if resp.content_length().is_some_and(|n| n > MAX_BUNDLE) {
        bail!("adapter bundle larger than {} MiB", MAX_BUNDLE >> 20);
    }
    let bytes = resp.bytes().await?;
    if bytes.len() as u64 > MAX_BUNDLE {
        bail!("adapter bundle larger than {} MiB", MAX_BUNDLE >> 20);
    }
    let v: Value = serde_json::from_slice(&bytes).context("adapter bundle is not JSON")?;
    let tmp = dir.with_extension("partial");
    let _ = tokio::fs::remove_dir_all(&tmp).await;
    tokio::fs::create_dir_all(&tmp).await?;
    for f in ADAPTER_FILES {
        if let Some(b64) = v["files"][f].as_str() {
            tokio::fs::write(tmp.join(f), base64::engine::general_purpose::STANDARD.decode(b64).context("bad base64 in adapter bundle")?).await?;
        }
    }
    let got = crate::temporal::worker::file_sha256(&tmp.join("adapter_model.safetensors")).context("bundle lacks adapter_model.safetensors")?;
    if got != want {
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        bail!("adapter {got} differs from the offer's {want}");
    }
    let _ = tokio::fs::remove_dir_all(dir).await;
    tokio::fs::rename(&tmp, dir).await?;
    Ok(())
}

/// Answer one request: adapter from the cache (or fetched), then `infernet model generate`.
pub async fn run_infer(infernet: &Path, cache_root: &Path, job_id: &str, spec: &TextInferSpec) -> Result<Vec<u8>> {
    spec.validate()?;
    let adapter = cache_root.join("text").join("adapters").join(spec.adapter_hash.trim_start_matches("sha256:"));
    let cached = crate::temporal::worker::file_sha256(&adapter.join("adapter_model.safetensors")).ok();
    if cached.as_deref() != Some(spec.adapter_hash.as_str()) {
        fetch_adapter(&spec.adapter_url, &spec.adapter_hash, &adapter).await.context("fetch adapter")?;
    }
    let dir = cache_root.join("text").join("infer").join(job_id);
    tokio::fs::create_dir_all(&dir).await?;
    let req = dir.join("request.json");
    tokio::fs::write(&req, serde_json::to_vec(&json!({"adapter_dir": adapter, "base_model": spec.base_model, "messages": spec.messages, "max_new_tokens": spec.max_new_tokens}))?).await?;
    let mut command = tokio::process::Command::new(crate::live_bin(infernet));
    command.args(["model", "generate", "--json", "--input"]).arg(&req).stdin(std::process::Stdio::null()).kill_on_drop(true);
    crate::resources::apply_job_limits(&mut command);
    let out = tokio::time::timeout(Duration::from_secs(900), command.output()).await.map_err(|_| anyhow::anyhow!("E_TIMEOUT: generation exceeded 900s"))??;
    let _ = tokio::fs::remove_dir_all(&dir).await;
    let line = String::from_utf8_lossy(&out.stdout).lines().rev().find(|l| l.trim_start().starts_with('{')).unwrap_or("").to_string();
    let ans: Value = serde_json::from_str(&line).unwrap_or(json!({}));
    if !out.status.success() || ans["type"] != json!("answer") {
        bail!("infernet model generate failed: {}", ans["message"].as_str().unwrap_or(String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("")));
    }
    if ans["adapter"].as_str() != Some(spec.adapter_hash.as_str()) {
        bail!("generate used adapter {} instead of {}", ans["adapter"], spec.adapter_hash);
    }
    Ok(serde_json::to_vec(&ans)?)
}

pub fn spawn_infer_runner(net: Arc<Libp2pNetwork>, cache_root: PathBuf, infernet: PathBuf, busy: Arc<AtomicBool>) -> mpsc::Sender<AcceptedJob> {
    let (tx, mut rx) = mpsc::channel::<AcceptedJob>(16);
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let accept = job.accept.clone();
            busy.store(true, Ordering::Relaxed);
            let result: Result<String> = async {
                let spec: TextInferSpec = serde_json::from_value(accept.spec_inline.clone().context("accept lacks spec_inline")?)?;
                let answer = run_infer(&infernet, &cache_root, &accept.job_id, &spec).await?;
                reqwest::Client::new().put(format!("{}/answer.json", spec.answer_upload_url)).body(answer.clone()).send().await?.error_for_status()?;
                Ok(sha256_hex(&answer))
            }
            .await;
            busy.store(false, Ordering::Relaxed);
            match result {
                Ok(hash) => {
                    info!(job_id = %accept.job_id, "text: answered");
                    publish_receipt(&net, &accept, Some(hash), JobStatus::Completed).await;
                }
                Err(e) => {
                    warn!(job_id = %accept.job_id, err = %format!("{e:#}"), "text: answer failed");
                    publish_receipt(&net, &accept, None, JobStatus::Failed).await;
                }
            }
        }
    });
    tx
}

pub fn infer_offer_filter(busy: Arc<AtomicBool>) -> crate::dispatch::OfferFilter {
    Arc::new(move |offer| {
        !busy.load(Ordering::Relaxed)
            && offer
                .spec_inline
                .as_ref()
                .and_then(|v| serde_json::from_value::<TextInferSpec>(v.clone()).ok())
                .is_some_and(|s| s.validate().is_ok())
    })
}

/// Does the local Infernet have `model generate` (>= 0.1.80)? It answers a
/// bare call with a usage error naming `--adapter`; older ones do not know it.
pub async fn supports_generate(infernet: &Path) -> bool {
    let Ok(Ok(out)) = tokio::time::timeout(Duration::from_secs(60), tokio::process::Command::new(crate::live_bin(infernet)).args(["model", "generate"]).stdin(std::process::Stdio::null()).output()).await else {
        return false;
    };
    String::from_utf8_lossy(&out.stderr).contains("--adapter")
}

pub struct Serving {
    pub net: Arc<Libp2pNetwork>,
    pub tags: Vec<String>,
    pub runtime: Value,
    pub infer: bool,
}

/// `c0mpute infernet text serve`: advertise and take `infernet.text.train`
/// jobs, and `infernet.text.infer` requests when the local Infernet can.
pub async fn serve(net_opts: &NetOptions, infernet: PathBuf, cache_root: PathBuf) -> Result<Serving> {
    let runtime = probe(&infernet).await?;
    let infer = supports_generate(&infernet).await;
    let ram_mb = local_ram_mb();
    let mut tags = worker_tags(&runtime, ram_mb);
    if infer {
        tags.push(tag(INFER_WORKLOAD));
    }
    let net = spawn_node(net_opts).await?;
    let hardware = json!({"version": env!("CARGO_PKG_VERSION"), "text_runtime": "peft", "ram_mb": ram_mb, "cuda": runtime["cuda"]});
    tokio::spawn(crate::capabilities::advertise_loop(net.clone(), tags.clone(), hardware, crate::capabilities::local_bench_score(), crate::capabilities::DEFAULT_ADVERTISE_INTERVAL));
    let busy = Arc::new(AtomicBool::new(false));
    let slots = Arc::new(Semaphore::new(1));
    let tx = spawn_train_runner(net.clone(), cache_root.clone(), infernet.clone(), busy.clone(), slots);
    crate::dispatch::run_worker_subscriber_filtered(net.clone(), TRAIN_WORKLOAD.into(), tags.clone(), Some(tx), Some(train_offer_filter(busy)));
    if infer {
        let ibusy = Arc::new(AtomicBool::new(false));
        let itx = spawn_infer_runner(net.clone(), cache_root, infernet, ibusy.clone());
        crate::dispatch::run_worker_subscriber_filtered(net.clone(), INFER_WORKLOAD.into(), tags.clone(), Some(itx), Some(infer_offer_filter(ibusy)));
    }
    info!(peer = %net.peer_id().to_base58(), ?tags, "text: serving");
    tokio::time::sleep(Duration::from_millis(300)).await;
    Ok(Serving { net, tags, runtime, infer })
}

// ── buyer ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct TrainRequest {
    pub job_path: PathBuf,
    /// Corpus; defaults to `job.dataset.url` resolved against the job file.
    pub dataset_path: Option<PathBuf>,
    pub output: PathBuf,
    pub workers: Vec<String>,
    pub max_price_usd: f64,
    pub gpu: bool,
    pub min_ram_gb: Option<u64>,
    pub public_url: Option<String>,
    pub bind: SocketAddr,
    pub timeout_s: u64,
    pub retries: u32,
    pub bid_window: Duration,
}

#[derive(Clone, Debug, Serialize)]
pub struct Preview {
    pub name: String,
    pub base_model: String,
    pub dataset: String,
    pub dataset_hash: String,
    pub documents: usize,
    pub bytes: usize,
    pub max_price_usd: f64,
    pub workers: Vec<String>,
}

/// The job and corpus the buyer is about to send.
pub fn load(req: &TrainRequest) -> Result<(Value, Vec<u8>, Preview)> {
    let job: Value = serde_json::from_slice(&std::fs::read(&req.job_path).with_context(|| format!("read {}", req.job_path.display()))?)?;
    let dataset = match &req.dataset_path {
        Some(p) => p.clone(),
        None => {
            let u = job["dataset"]["url"].as_str().context("job.dataset.url is missing; pass --dataset")?;
            if is_http(u) {
                bail!("job.dataset.url is a URL; pass the local corpus with --dataset");
            }
            req.job_path.parent().unwrap_or(Path::new(".")).join(u.trim_start_matches("file://"))
        }
    };
    let bytes = std::fs::read(&dataset).with_context(|| format!("read {}", dataset.display()))?;
    let documents = bytes.split(|b| *b == b'\n').filter(|l| !l.iter().all(u8::is_ascii_whitespace)).count();
    let p = Preview {
        name: job["name"].as_str().unwrap_or("adapter").to_string(),
        base_model: job["base_model"].as_str().unwrap_or("").to_string(),
        dataset: dataset.display().to_string(),
        dataset_hash: sha256_hex(&bytes),
        documents,
        bytes: bytes.len(),
        max_price_usd: req.max_price_usd,
        workers: req.workers.clone(),
    };
    Ok((job, bytes, p))
}

/// Live progress, one line rewritten in place.
pub fn render(ev: &Value, total: &mut Option<u64>) -> Option<String> {
    let bar = |step: u64, total: u64| {
        let w = 24usize;
        let filled = ((step.min(total) as f64 / total.max(1) as f64) * w as f64).round() as usize;
        format!("[{}{}]", "#".repeat(filled), "-".repeat(w - filled))
    };
    match ev["type"].as_str()? {
        "meta" => {
            *total = ev["total_steps"].as_u64();
            Some(format!("training {} on {} ({} docs, {} steps)", ev["base_model"].as_str().unwrap_or("?"), ev["device"].as_str().unwrap_or("cpu"), ev["documents"], total.map(|t| t.to_string()).unwrap_or("?".into())))
        }
        "step" => {
            let step = ev["step"].as_u64().unwrap_or(0);
            let t = ev["total_steps"].as_u64().or(*total).unwrap_or(step.max(1));
            Some(format!("{} step {step}/{t}  loss {}  {} tok/s", bar(step, t), ev["loss"], ev["throughput"]))
        }
        "eval" => Some(format!("eval @{}: {}", ev["step"], ev["metrics"])),
        "checkpoint" => Some(format!("adapter {} ({} bytes)", ev["model"].as_str().unwrap_or("?"), ev["size_bytes"])),
        "error" => Some(format!("error: {}", ev["message"].as_str().unwrap_or("?"))),
        "done" => Some(format!("done: {}", ev["reason"].as_str().unwrap_or("?"))),
        _ => None,
    }
}

/// Auction the job, stream its progress through `on_event`, verify and keep the adapter.
pub async fn train(net: Arc<Libp2pNetwork>, req: &TrainRequest, mut on_event: impl FnMut(&Value) + Send) -> Result<Value> {
    let (job, bytes, preview) = load(req)?;
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<SlotEvent>();
    let server = TransferServer::start_with(req.bind, req.public_url.clone(), bytes, &ARTIFACTS, Some(ev_tx)).await?;
    let spec = TextTrainSpec {
        job,
        dataset_url: server.dataset_url(),
        dataset_hash: preview.dataset_hash.clone(),
        artifact_upload_url: server.upload_url(SLOT),
        events_url: Some(server.events_url(SLOT)),
    };
    spec.validate()?;
    let mut tags = vec![tag(TRAIN_WORKLOAD)];
    if req.gpu {
        tags.push("infernet:text:device:cuda".into());
    }
    if let Some(min) = req.min_ram_gb {
        let b = crate::temporal::caps::RAM_BUCKETS_GB.iter().copied().find(|b| *b >= min).unwrap_or(128);
        tags.push(format!("c0mpute:ram:{b}gb+"));
    }
    let spec_v = serde_json::to_value(&spec)?;
    let mut last_err = None;
    let mut outcome = None;
    for attempt in 0..=req.retries.min(5) {
        let make = || {
            JobAuction::new(TRAIN_WORKLOAD, spec_v.clone())
                .with_required_capabilities(tags.clone())
                .with_allowed_bidders(req.workers.clone())
                .with_max_price_usd(req.max_price_usd)
                .with_bid_window(req.bid_window)
                .with_deadline(Duration::from_secs(req.timeout_s + 600))
                .with_receipt_timeout(Duration::from_secs(req.timeout_s + 300))
        };
        // Print progress while the auction waits for the receipt.
        let auction = auction_with_mesh_retry(net.clone(), make);
        tokio::pin!(auction);
        let res = loop {
            tokio::select! {
                r = &mut auction => break r,
                Some((_, ev)) = ev_rx.recv() => on_event(&ev),
            }
        };
        while let Ok((_, ev)) = ev_rx.try_recv() {
            on_event(&ev);
        }
        match res {
            Ok(o) if o.receipt.status == JobStatus::Completed => {
                outcome = Some(o);
                break;
            }
            Ok(o) => last_err = Some(anyhow::anyhow!("worker {} reported {:?} (attempt {})", o.accepted_bid.bidder_peer_id, o.receipt.status, attempt + 1)),
            Err(e) => last_err = Some(e.context(format!("attempt {}", attempt + 1))),
        }
    }
    let o = outcome.ok_or_else(|| last_err.unwrap_or_else(|| anyhow::anyhow!("training failed")))?;
    let files: HashMap<String, Vec<u8>> = server.take(SLOT).await;
    let weights = files.get("adapter_model.safetensors").context("worker did not upload adapter_model.safetensors")?;
    let got = sha256_hex(weights);
    let claimed = o.receipt.output_hash.clone().unwrap_or_default();
    if got != claimed {
        bail!("adapter {got} does not match the receipt's {claimed}");
    }
    std::fs::create_dir_all(&req.output)?;
    for (name, data) in &files {
        std::fs::write(req.output.join(name), data)?;
    }
    Ok(json!({
        "adapter_dir": req.output, "adapter_hash": got, "base_model": preview.base_model, "dataset_hash": preview.dataset_hash,
        "receipt": {"job_id": o.receipt.job_id, "worker_peer_id": o.receipt.worker_peer_id, "output_hash": o.receipt.output_hash, "price_usd": o.accepted_bid.price_usd}
    }))
}

// ── chat (buyer) ────────────────────────────────────────────────────────

pub const PIT_WORLD_SEARCH: &str = "https://pit.world/api/search";

#[derive(Clone, Debug, Serialize)]
pub struct Source {
    pub title: String,
    pub url: String,
    pub text: String,
}

/// Top web results for the question from pit.world search; empty when unreachable.
pub async fn pit_search(base: &str, query: &str, k: usize) -> Vec<Source> {
    let Ok(mut url) = url::Url::parse(base) else { return vec![] };
    url.query_pairs_mut().append_pair("q", query);
    let Ok(client) = reqwest::Client::builder().timeout(Duration::from_secs(20)).user_agent(concat!("c0mpute/", env!("CARGO_PKG_VERSION"))).build() else {
        return vec![];
    };
    let Ok(resp) = client.get(url).send().await else { return vec![] };
    let Ok(body) = resp.json::<Value>().await else { return vec![] };
    body["results"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| {
            Some(Source { title: r["title"].as_str()?.to_string(), url: r["url"].as_str()?.to_string(), text: r["description"].as_str().unwrap_or("").to_string() })
        })
        .take(k)
        .collect()
}

/// The user turn: the question, with the sources as background when there are any.
pub fn chat_prompt(question: &str, sources: &[Source]) -> String {
    if sources.is_empty() {
        return question.to_string();
    }
    let ctx: Vec<String> = sources.iter().enumerate().map(|(i, s)| format!("[{}] {} ({})\n{}", i + 1, s.title, s.url, s.text.chars().take(700).collect::<String>())).collect();
    format!("Background from the web:\n{}\n\nUsing the background where it helps, answer: {question}", ctx.join("\n\n"))
}

#[derive(Clone, Debug)]
pub struct ChatRequest {
    pub adapter_dir: PathBuf,
    pub question: String,
    /// pit.world search base; None skips retrieval.
    pub search: Option<String>,
    pub max_new_tokens: u32,
    pub workers: Vec<String>,
    pub max_price_usd: f64,
    pub public_url: Option<String>,
    pub bind: SocketAddr,
    pub timeout_s: u64,
    pub bid_window: Duration,
}

/// Ask a worker; returns the answer, its sources and the receipt.
pub async fn chat(net: Arc<Libp2pNetwork>, req: &ChatRequest) -> Result<Value> {
    let (bundle, adapter_hash, manifest) = adapter_bundle(&req.adapter_dir)?;
    let base_model = manifest["base_model"].as_str().context("the adapter's manifest.json has no base_model")?.to_string();
    let sources = match &req.search {
        Some(base) => pit_search(base, &req.question, 3).await,
        None => vec![],
    };
    let mut messages = Vec::new();
    if let Some(system) = manifest["system_prompt"].as_str() {
        messages.push(json!({"role": "system", "content": system}));
    }
    messages.push(json!({"role": "user", "content": chat_prompt(&req.question, &sources)}));
    let server = TransferServer::start_with(req.bind, req.public_url.clone(), bundle, &ANSWER_ARTIFACTS, None).await?;
    let spec = TextInferSpec {
        adapter_hash: adapter_hash.clone(),
        base_model: base_model.clone(),
        messages,
        max_new_tokens: req.max_new_tokens,
        adapter_url: server.dataset_url(),
        answer_upload_url: server.upload_url(ANSWER_SLOT),
    };
    spec.validate()?;
    let spec_v = serde_json::to_value(&spec)?;
    let make = || {
        JobAuction::new(INFER_WORKLOAD, spec_v.clone())
            .with_required_capabilities(vec![tag(INFER_WORKLOAD)])
            .with_allowed_bidders(req.workers.clone())
            .with_max_price_usd(req.max_price_usd)
            .with_bid_window(req.bid_window)
            .with_deadline(Duration::from_secs(req.timeout_s + 120))
            .with_receipt_timeout(Duration::from_secs(req.timeout_s + 60))
    };
    let o = auction_with_mesh_retry(net, make).await?;
    if o.receipt.status != JobStatus::Completed {
        bail!("worker {} reported {:?}", o.accepted_bid.bidder_peer_id, o.receipt.status);
    }
    let files = server.take(ANSWER_SLOT).await;
    let answer = files.get("answer.json").context("worker did not upload answer.json")?;
    let got = sha256_hex(answer);
    if Some(got.as_str()) != o.receipt.output_hash.as_deref() {
        bail!("answer {got} does not match the receipt's {:?}", o.receipt.output_hash);
    }
    let ans: Value = serde_json::from_slice(answer)?;
    Ok(json!({
        "text": ans["text"], "sources": sources, "base_model": base_model, "adapter_hash": adapter_hash,
        "tokens": ans["tokens"], "seconds": ans["seconds"],
        "receipt": {"job_id": o.receipt.job_id, "worker_peer_id": o.receipt.worker_peer_id, "output_hash": o.receipt.output_hash, "price_usd": o.accepted_bid.price_usd}
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn infer_spec() -> TextInferSpec {
        TextInferSpec {
            adapter_hash: format!("sha256:{}", "b".repeat(64)),
            base_model: "HuggingFaceTB/SmolLM2-135M-Instruct".into(),
            messages: vec![json!({"role": "user", "content": "hi"})],
            max_new_tokens: 200,
            adapter_url: "http://127.0.0.1:1/t/x/dataset.json".into(),
            answer_upload_url: "http://127.0.0.1:1/t/x/a/answer".into(),
        }
    }

    #[test]
    fn infer_spec_validation() {
        assert!(infer_spec().validate().is_ok());
        let mut s = infer_spec();
        s.messages = vec![json!({"role": "tool", "content": "x"})];
        assert!(s.validate().is_err());
        let mut s = infer_spec();
        s.messages = vec![json!({"role": "user", "content": "x".repeat(40 * 1024)})];
        assert!(s.validate().is_err());
        let mut s = infer_spec();
        s.max_new_tokens = 0;
        assert!(s.validate().is_err());
        let mut s = infer_spec();
        s.base_model = "../../etc".into();
        assert!(s.validate().is_err());
    }

    #[tokio::test]
    async fn adapter_bundle_round_trips_through_the_cache_fetch() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("adapter_model.safetensors"), b"weights").unwrap();
        std::fs::write(src.path().join("adapter_config.json"), b"{}").unwrap();
        std::fs::write(src.path().join("manifest.json"), br#"{"base_model":"org/m","system_prompt":"be brief"}"#).unwrap();
        let (bundle, hash, manifest) = adapter_bundle(src.path()).unwrap();
        assert_eq!(hash, sha256_hex(b"weights"));
        assert_eq!(manifest["base_model"], "org/m");
        let srv = TransferServer::start("127.0.0.1:0".parse().unwrap(), None, bundle).await.unwrap();
        let cache = tempfile::tempdir().unwrap();
        let dst = cache.path().join("a");
        fetch_adapter(&srv.dataset_url(), &hash, &dst).await.unwrap();
        assert_eq!(std::fs::read(dst.join("adapter_model.safetensors")).unwrap(), b"weights");
        let bad = format!("sha256:{}", "0".repeat(64));
        assert!(fetch_adapter(&srv.dataset_url(), &bad, &cache.path().join("b")).await.is_err());
        assert!(!cache.path().join("b").exists());
    }

    #[test]
    fn prompt_carries_numbered_sources() {
        let s = vec![Source { title: "How to price".into(), url: "https://x/p".into(), text: "value".into() }];
        let p = chat_prompt("Pricing?", &s);
        assert!(p.starts_with("Background from the web:\n[1] How to price (https://x/p)"));
        assert!(p.ends_with("answer: Pricing?"));
        assert_eq!(chat_prompt("Q", &[]), "Q");
    }

    fn spec() -> TextTrainSpec {
        TextTrainSpec {
            job: json!({"kind": "lora", "base_model": "HuggingFaceTB/SmolLM2-135M-Instruct"}),
            dataset_url: "http://127.0.0.1:1/t/x/dataset.json".into(),
            dataset_hash: format!("sha256:{}", "a".repeat(64)),
            artifact_upload_url: "http://127.0.0.1:1/t/x/a/adapter".into(),
            events_url: Some("http://127.0.0.1:1/t/x/e/adapter".into()),
        }
    }

    #[test]
    fn validates_kind_model_hash_and_urls() {
        assert!(spec().validate().is_ok());
        let mut s = spec();
        s.job["kind"] = json!("temporal-policy");
        assert!(s.validate().is_err());
        for bad in ["", "/etc/passwd", "../x", "https://evil/model"] {
            let mut s = spec();
            s.job["base_model"] = json!(bad);
            assert!(s.validate().is_err(), "{bad}");
        }
        let mut s = spec();
        s.dataset_hash = "sha256:ABC".into();
        assert!(s.validate().is_err());
        let mut s = spec();
        s.events_url = Some("file:///tmp/x".into());
        assert!(s.validate().is_err());
    }

    #[test]
    fn tags_and_render() {
        let t = worker_tags(&json!({"cuda": false}), 16 * 1024);
        assert!(t.contains(&"infernet:text:train".to_string()));
        assert!(t.contains(&"c0mpute:ram:16gb+".to_string()));
        assert!(!t.contains(&"infernet:text:device:cuda".to_string()));
        let mut total = None;
        render(&json!({"type": "meta", "base_model": "m", "total_steps": 4, "documents": 9}), &mut total);
        assert_eq!(total, Some(4));
        let line = render(&json!({"type": "step", "step": 2, "loss": 1.5, "throughput": 10}), &mut total).unwrap();
        assert!(line.starts_with("[############------------] step 2/4"), "{line}");
    }

    #[tokio::test]
    async fn worker_refuses_a_dataset_whose_hash_differs() {
        let srv = TransferServer::start("127.0.0.1:0".parse().unwrap(), None, b"{\"text\":\"x\"}\n".to_vec()).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let err = download(&srv.dataset_url(), &dir.path().join("d.jsonl"), &format!("sha256:{}", "0".repeat(64))).await.unwrap_err();
        assert!(format!("{err}").contains("differs"));
        let ok = sha256_hex(b"{\"text\":\"x\"}\n");
        download(&srv.dataset_url(), &dir.path().join("d.jsonl"), &ok).await.unwrap();
    }
}
