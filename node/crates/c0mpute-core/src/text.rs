//! Infernet text fine-tunes on c0mpute: workload `infernet.text.train`.
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
        tokio::process::Command::new(infernet).args(["model", "runtime", "status", "--peft", "--json"]).stdin(std::process::Stdio::null()).output(),
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

    let mut command = tokio::process::Command::new(infernet);
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

pub struct Serving {
    pub net: Arc<Libp2pNetwork>,
    pub tags: Vec<String>,
    pub runtime: Value,
}

/// `c0mpute infernet text serve`: advertise and take `infernet.text.train` jobs.
pub async fn serve(net_opts: &NetOptions, infernet: PathBuf, cache_root: PathBuf) -> Result<Serving> {
    let runtime = probe(&infernet).await?;
    let ram_mb = local_ram_mb();
    let tags = worker_tags(&runtime, ram_mb);
    let net = spawn_node(net_opts).await?;
    let hardware = json!({"version": env!("CARGO_PKG_VERSION"), "text_runtime": "peft", "ram_mb": ram_mb, "cuda": runtime["cuda"]});
    tokio::spawn(crate::capabilities::advertise_loop(net.clone(), tags.clone(), hardware, crate::capabilities::local_bench_score(), crate::capabilities::DEFAULT_ADVERTISE_INTERVAL));
    let busy = Arc::new(AtomicBool::new(false));
    let slots = Arc::new(Semaphore::new(1));
    let tx = spawn_train_runner(net.clone(), cache_root, infernet, busy.clone(), slots);
    crate::dispatch::run_worker_subscriber_filtered(net.clone(), TRAIN_WORKLOAD.into(), tags.clone(), Some(tx), Some(train_offer_filter(busy)));
    info!(peer = %net.peer_id().to_base58(), ?tags, "text: serving");
    tokio::time::sleep(Duration::from_millis(300)).await;
    Ok(Serving { net, tags, runtime })
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

#[cfg(test)]
mod tests {
    use super::*;

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
