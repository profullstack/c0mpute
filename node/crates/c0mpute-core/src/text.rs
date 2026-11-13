//! Infernet text fine-tunes on c0mpute: workloads `infernet.text.train` and
//! `infernet.text.infer` (answers from a trained adapter, further down).
//!
//! A buyer has a job file (`@infernetprotocol/training` job config, `kind:
//! "lora"`) and a JSONL corpus. `c0mpute infernet text train` (and its front
//! door `c0mpute train lora`) stores the corpus as content-addressed pieces in
//! its own chunk store, auctions the job to workers that advertise
//! `infernet:text:train`, and shows the winner's training live: the worker
//! pulls the pieces over libp2p and publishes every training-protocol v1
//! event (`meta`, `step`, `eval`, `checkpoint`, `done`) on the job topic as it
//! happens. The worker then stores the LoRA adapter as pieces behind an
//! [`AdapterManifest`] whose hash its receipt carries; the buyer fetches it
//! over libp2p and keeps it only if the weights' sha256 matches. Peer to peer
//! end to end: neither side needs a reachable HTTP address.
//!
//! Workers before the pieces transport take the corpus and return the adapter
//! over HTTP from the buyer's transfer server; buyers opt into serving them
//! with `--legacy-http` (see [`TextTrainSpec`]).
//!
//! The training itself is Infernet's (`infernet model train --job <job.json>
//! --json`, the `peft` backend); c0mpute hands it local file paths. c0mpute
//! only moves the data, the progress and the result, over the same offer →
//! bid → accept → receipt protocol as every other workload. One job per node
//! at a time.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use c0mpute_net::{Libp2pNetwork, Network, PeerId};
use c0mpute_store::ChunkStore;
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
use crate::temporal::node::{NetOptions, spawn_node_with_store};

pub const TRAIN_WORKLOAD: &str = "infernet.text.train";
pub const KINDS: [&str; 2] = ["lora", "peft"];
/// What the worker uploads; the buyer accepts nothing else.
pub const ARTIFACTS: [&str; 3] = ["adapter_model.safetensors", "adapter_config.json", "manifest.json"];
const MAX_DATASET: u64 = 512 << 20;
const SLOT: &str = "adapter";

pub fn tag(workload: &str) -> String {
    workload.replace('.', ":")
}

/// Capability tag: this worker's runner trains `{"messages": [...]}` records
/// as whole multi-turn conversations (Infernet's peft runner reports
/// `formats: ["text", "messages"]`). Buyers whose corpus has conversations
/// require it, so an older runner (which would drop them) never wins.
pub const MESSAGES_FORMAT_TAG: &str = "infernet:text:format:messages";
/// Largest adapter file a buyer accepts as pieces.
pub const MAX_ADAPTER_FILE_BYTES: usize = 2 << 30;
/// How long a worker keeps a finished adapter's pieces for the buyer to fetch.
const ADAPTER_PIECES_TTL: Duration = Duration::from_secs(6 * 3600);
/// Gossip message type a worker publishes each training event as.
pub const EVENT_MSG: &str = "text_train_event";
/// At most one `step` event per this interval goes on the wire.
const STEP_EVENT_INTERVAL: Duration = Duration::from_millis(250);
/// How often a training worker publishes a `heartbeat` event for its run, so
/// the buyer can tell a quiet stretch (loading, eval) from a dead worker.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
/// Silence after which a buyer that has heard a heartbeat on its run gives
/// the worker up (three missed beats plus gossip slack) and re-auctions.
pub const HEARTBEAT_SILENCE: Duration = Duration::from_secs(120);

/// Jobs a text worker is running now, by job id, with each one's run id, so a
/// shutdown can fail them out loud instead of leaving the buyer waiting.
pub type InFlight = Arc<std::sync::Mutex<HashMap<String, (crate::runner::AcceptedJob, Option<String>)>>>;

/// Aborts the task when dropped, so an early return never leaks it.
struct AbortOnDrop(tokio::task::JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// `JobOffer.spec_inline` for `infernet.text.train`.
///
/// Two transports, both checked against the same `dataset_hash` and adapter
/// hash:
///
/// - **pieces (peer to peer, this version on)**: the buyer stores the corpus
///   as [`crate::pieces::INPUT_PIECE_BYTES`] pieces and names them in
///   `corpus_hash` / `corpus_size` / `corpus_pieces`; the worker pulls them
///   over libp2p. With `adapter_pieces` the worker stores the adapter files as
///   pieces too and its receipt's `output_hash` is the content hash of an
///   [`AdapterManifest`] chunk. With `run_id` it publishes every training
///   event on the job topic. Nothing needs to be dialable over HTTP.
/// - **HTTP (workers before this version)**: `dataset_url`,
///   `artifact_upload_url` and `events_url` on the buyer's transfer server;
///   the receipt's `output_hash` is `sha256:` of the adapter weights. A buyer
///   fills these only with `--legacy-http`. Older workers cannot parse a spec
///   without `dataset_url`, so they never bid on a pieces-only job.
///
/// A worker that understands both prefers pieces.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TextTrainSpec {
    /// The job config. The worker rewrites `dataset.url` and
    /// `output.destination_url` to local paths; everything else is Infernet's.
    pub job: Value,
    /// `sha256:<hex>` of the corpus; the worker refuses a mismatch.
    pub dataset_hash: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub dataset_url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub artifact_upload_url: String,
    /// Where the worker posts each training event as it happens (HTTP).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events_url: Option<String>,
    /// Content hash of the corpus as stored pieces (see [`crate::pieces::InputRef`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corpus_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corpus_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corpus_pieces: Option<Vec<String>>,
    /// Hex key the stored corpus is sealed under (DIP-0032); rides only inside
    /// the sealed spec. Absent: the stored bytes are plaintext.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corpus_key: Option<String>,
    /// Return the adapter as pieces behind an [`AdapterManifest`].
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub adapter_pieces: bool,
    /// Publish training events on the job topic tagged with this id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

/// What a worker's receipt points at when the adapter travels as pieces.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AdapterManifest {
    pub v: u32,
    /// `sha256:` of adapter_model.safetensors (the checkpoint's `model`).
    pub adapter_sha256: String,
    /// Each of [`ARTIFACTS`] the worker produced.
    pub files: std::collections::BTreeMap<String, ManifestFile>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ManifestFile {
    pub hash: String,
    pub size: u64,
    pub pieces: Vec<String>,
}

/// A training event as published on the job topic.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct EventMsg {
    pub t: String,
    pub run_id: String,
    pub event: Value,
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
        let urls = [Some(self.dataset_url.as_str()), Some(self.artifact_upload_url.as_str()), self.events_url.as_deref()];
        if urls.into_iter().flatten().any(|u| !u.is_empty() && !is_http(u)) {
            bail!("dataset, upload and events URLs must be http(s)");
        }
        if self.corpus_ref().transpose()?.is_none() && self.dataset_url.is_empty() {
            bail!("the spec names no corpus: corpus pieces or a dataset URL");
        }
        if !self.adapter_pieces && self.artifact_upload_url.is_empty() {
            bail!("the spec names no way back for the adapter: adapter_pieces or an upload URL");
        }
        if self.run_id.as_ref().is_some_and(|r| r.is_empty() || r.len() > 64 || !r.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')) {
            bail!("run_id must be 1-64 letters, digits or '-'");
        }
        Ok(())
    }

    /// The corpus as pieces, when the spec names them.
    pub fn corpus_ref(&self) -> Option<Result<crate::pieces::InputRef>> {
        let hash = self.corpus_hash.as_deref()?;
        Some(
            crate::pieces::InputRef::parse("corpus", hash, self.corpus_size, self.corpus_pieces.as_deref(), crate::pieces::input_limit(MAX_DATASET as usize, self.corpus_key.is_some()))
                .map_err(anyhow::Error::msg),
        )
    }
}

/// Capability tags a worker needs to train `corpus`.
pub fn required_tags(corpus: &[u8]) -> Vec<String> {
    let mut t = vec![tag(TRAIN_WORKLOAD)];
    if corpus_has_conversations(corpus) {
        t.push(MESSAGES_FORMAT_TAG.into());
    }
    t
}

/// Does any record of a JSONL corpus carry `messages` (a conversation)?
pub fn corpus_has_conversations(corpus: &[u8]) -> bool {
    corpus.split(|b| *b == b'\n').any(|line| {
        // Cheap pre-check before parsing: most corpora are {title, text}.
        line.windows(10).any(|w| w == b"\"messages\"")
            && serde_json::from_slice::<Value>(line).is_ok_and(|v| v.get("messages").is_some_and(Value::is_array))
    })
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
    // Infernet's runner trains whole conversations from the release that
    // reports it; older ones would silently keep only {title, text} records.
    if runtime["formats"].as_array().is_some_and(|f| f.iter().any(|x| x == "messages")) {
        t.push(MESSAGES_FORMAT_TAG.into());
    }
    for gb in crate::temporal::caps::RAM_BUCKETS_GB {
        if ram_mb >= gb * 1024 {
            t.push(format!("c0mpute:ram:{gb}gb+"));
        }
    }
    t
}

async fn download(url: &str, dst: &Path, want: &str) -> Result<()> {
    let (client, url) = crate::net_guard::job_client(url, None).await?;
    let resp = client.get(url).send().await?.error_for_status()?;
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

/// Fetch the corpus into `dst`: pieces over libp2p (from the buyer first)
/// when the spec names them, else the legacy HTTP URL. Either way the file's
/// sha256 must be the offer's `dataset_hash`.
pub async fn fetch_corpus(net: &Arc<Libp2pNetwork>, buyer: Option<PeerId>, spec: &TextTrainSpec, dst: &Path) -> Result<()> {
    match spec.corpus_ref() {
        Some(r) => {
            let key = crate::pieces::input_key(spec.corpus_key.as_deref())?;
            r?.fetch_to_file_opened(net, buyer, MAX_DATASET as usize, key.as_ref(), dst).await.context("fetch corpus pieces")?;
            let got = crate::temporal::worker::file_sha256(dst)?;
            if got != spec.dataset_hash {
                let _ = tokio::fs::remove_file(dst).await;
                bail!("dataset hash {got} differs from the offer's {}", spec.dataset_hash);
            }
            Ok(())
        }
        None => download(&spec.dataset_url, dst, &spec.dataset_hash).await,
    }
}

/// Run one accepted job: fetch the corpus, run `infernet model train --job
/// --json`, relay every event, return the adapter dir and its weights hash.
/// `buyer` is the peer the corpus pieces are fetched from first.
pub async fn run_train(infernet: &Path, job_dir: &Path, spec: &TextTrainSpec, net: &Arc<Libp2pNetwork>, buyer: Option<PeerId>) -> Result<(PathBuf, String)> {
    spec.validate()?;
    tokio::fs::create_dir_all(job_dir).await?;
    let dataset = job_dir.join("dataset.jsonl");
    fetch_corpus(net, buyer, spec, &dataset).await.context("fetch dataset")?;
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

    // Events go to the buyer from their own task so a slow link never stalls
    // training: on the job topic (p2p, `run_id`) and/or to the legacy HTTP URL.
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<Value>();
    let relaying = spec.events_url.is_some() || spec.run_id.is_some();
    let relay = relaying.then(|| {
        let (url, run_id, net) = (spec.events_url.clone(), spec.run_id.clone(), net.clone());
        tokio::spawn(async move {
            // The hook is the buyer's URL: vetted and pinned once, or not used.
            let hook = match &url {
                Some(u) => match crate::net_guard::job_client(u, Some(Duration::from_secs(10))).await {
                    Ok(hook) => Some(hook),
                    Err(e) => {
                        warn!(err = %format!("{e:#}"), "text: events URL refused; training events are not relayed there");
                        None
                    }
                },
                None => None,
            };
            let topic = c0mpute_net::topics::job_topic(TRAIN_WORKLOAD);
            let mut last_step: Option<std::time::Instant> = None;
            while let Some(ev) = ev_rx.recv().await {
                if let Some(run_id) = &run_id {
                    let step = ev["type"] == "step";
                    if !(step && last_step.is_some_and(|t| t.elapsed() < STEP_EVENT_INTERVAL)) {
                        if step {
                            last_step = Some(std::time::Instant::now());
                        }
                        let msg = EventMsg { t: EVENT_MSG.into(), run_id: run_id.clone(), event: ev.clone() };
                        if let Ok(b) = serde_json::to_vec(&msg)
                            && let Err(e) = net.publish(&topic, b).await
                        {
                            warn!(err = %e, "text: could not publish a training event");
                        }
                    }
                }
                if let Some((c, url)) = &hook
                    && let Err(e) = c.post(url.clone()).json(&ev).send().await
                {
                    warn!(err = %e, "text: could not relay a training event");
                }
            }
        })
    });

    // Heartbeats on the same path as the events, for as long as this call
    // runs: the buyer arms its silence watchdog on the first one.
    let _beat = relaying.then(|| {
        let tx = ev_tx.clone();
        AbortOnDrop(tokio::spawn(async move {
            let mut tick = tokio::time::interval(HEARTBEAT_INTERVAL);
            loop {
                tick.tick().await;
                if tx.send(json!({"v": 1, "type": "heartbeat", "at_ms": unix_ms()})).is_err() {
                    break;
                }
            }
        }))
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
            if relaying {
                let _ = ev_tx.send(ev);
            }
        }
        Ok::<_, anyhow::Error>((checkpoint, done))
    };
    let (checkpoint, done) = tokio::time::timeout(Duration::from_secs(timeout_s), read)
        .await
        .map_err(|_| anyhow::anyhow!("E_TIMEOUT: training exceeded {timeout_s}s"))??;
    let status = child.wait().await?;
    drop(_beat);
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
    for f in ARTIFACTS {
        let p = dir.join(f);
        if p.exists() {
            let (c, url) = crate::net_guard::job_client(&format!("{base}/{f}"), None).await?;
            c.put(url).body(tokio::fs::read(&p).await?).send().await?.error_for_status()?;
        }
    }
    Ok(())
}

/// Store the adapter files as pieces, then the [`AdapterManifest`] naming
/// them; returns the manifest's content hash (what the receipt carries). The
/// pieces are dropped from the store after [`ADAPTER_PIECES_TTL`].
///
/// With `key` (DIP-0032) every file and the manifest are sealed under it
/// first: what is stored, announced and named is ciphertext.
pub async fn store_adapter(net: &Arc<Libp2pNetwork>, store: &ChunkStore, dir: &Path, adapter_sha256: &str, key: Option<&c0mpute_net::topics::JobKey>) -> Result<String> {
    let mut files = std::collections::BTreeMap::new();
    let mut held = Vec::new();
    for f in ARTIFACTS {
        let p = dir.join(f);
        if !p.exists() {
            continue;
        }
        let (src, limit, _sealed) = match key {
            Some(k) => {
                let sealed = std::env::temp_dir().join(format!("c0mpute-adapter-{}.sealed", uuid::Uuid::new_v4()));
                crate::job_e2e::seal_file(k, &p, &sealed).await?;
                (sealed.clone(), crate::job_e2e::sealed_len(MAX_ADAPTER_FILE_BYTES as u64), Some(RemoveOnDrop(sealed)))
            }
            None => (p.clone(), MAX_ADAPTER_FILE_BYTES as u64, None),
        };
        let stored = crate::pieces::put_file_pieces(net, store, &src, limit, crate::pieces::INPUT_PIECE_BYTES).await.with_context(|| format!("store {f}"))?;
        held.extend(stored.pieces.iter().cloned());
        files.insert(f.to_string(), ManifestFile { hash: stored.hash, size: stored.size, pieces: stored.pieces });
    }
    if !files.contains_key("adapter_model.safetensors") {
        bail!("no adapter_model.safetensors to store");
    }
    let mut manifest = serde_json::to_vec(&AdapterManifest { v: 1, adapter_sha256: adapter_sha256.to_string(), files })?;
    if let Some(k) = key {
        manifest = crate::job_e2e::seal_blob(k, &manifest)?;
    }
    let hash = store.put(&manifest).await.context("store adapter manifest")?;
    net.announce(&hash).await.context("announce adapter manifest")?;
    held.push(hash.to_hex());
    let store = store.clone();
    tokio::spawn(async move {
        tokio::time::sleep(ADAPTER_PIECES_TTL).await;
        for h in held {
            if let Ok(h) = c0mpute_proto::Hash::from_hex(&h) {
                let _ = store.delete(&h).await;
            }
        }
    });
    Ok(hash.to_hex())
}

/// Removes a temporary file when dropped.
struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Removes a job directory when dropped, on every exit path.
struct RemoveDirOnDrop(PathBuf);

impl Drop for RemoveDirOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn spawn_train_runner(net: Arc<Libp2pNetwork>, store: ChunkStore, cache_root: PathBuf, infernet: PathBuf, busy: Arc<AtomicBool>, slots: Arc<Semaphore>, in_flight: InFlight) -> mpsc::Sender<AcceptedJob> {
    let (tx, mut rx) = mpsc::channel::<AcceptedJob>(8);
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let Ok(_slot) = slots.clone().acquire_owned().await else { return };
            let accept = job.accept.clone();
            // Never a raw job id in a path (the directory is removed below).
            let Ok(job_dir) = crate::runner::job_subdir(&cache_root.join("text").join("jobs"), &accept.job_id) else {
                warn!(job_id = %accept.job_id, "text: job id is not a UUID; not running it");
                continue;
            };
            busy.store(true, Ordering::Relaxed);
            let run_id = accept.spec_inline.as_ref().and_then(|v| v["run_id"].as_str()).map(String::from);
            in_flight.lock().unwrap_or_else(|e| e.into_inner()).insert(accept.job_id.clone(), (job.clone(), run_id));
            let result: Result<String> = crate::job_control::cancellable(&accept.job_id, async {
                let spec: TextTrainSpec = serde_json::from_value(accept.spec_inline.clone().context("accept lacks spec_inline")?)?;
                let buyer = accept.buyer_peer_id.parse::<PeerId>().ok();
                let (dir, hash) = run_train(&infernet, &job_dir, &spec, &net, buyer).await?;
                if spec.adapter_pieces {
                    // Receipt -> manifest -> pieces, all over libp2p.
                    // Sealed to the buyer for e2e jobs.
                    let key = crate::job_e2e::result_key(&accept)?;
                    return store_adapter(&net, &store, &dir, &hash, key).await;
                }
                upload(&spec.artifact_upload_url, &dir).await.context("upload adapter")?;
                Ok(hash)
            })
            .await;
            let _ = tokio::fs::remove_dir_all(&job_dir).await;
            in_flight.lock().unwrap_or_else(|e| e.into_inner()).remove(&accept.job_id);
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
            && match crate::job_e2e::bid_spec(offer) {
                // The corpus and URLs stay sealed: judge the kind and base
                // model; the opened spec is validated in full before it runs.
                Some(crate::job_e2e::BidSpec::Public(v)) => {
                    let kind = v["job"]["kind"].as_str().unwrap_or("");
                    let base = v["job"]["base_model"].as_str().unwrap_or("");
                    KINDS.contains(&kind) && valid_repo_id(base)
                }
                Some(crate::job_e2e::BidSpec::Full(v)) => {
                    serde_json::from_value::<TextTrainSpec>(v.clone()).ok().is_some_and(|s| s.validate().is_ok())
                }
                None => false,
            }
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
/// model + LoRA adapter. The adapter travels with the request and is cached
/// by hash, so any worker can serve any adapter; the buyer does retrieval,
/// the worker only generates.
///
/// Peer to peer (this version on): the buyer stores the adapter as pieces
/// behind an [`AdapterManifest`] (`adapter_manifest`), the worker fetches it
/// over libp2p on a cache miss, and with `answer_chunk` stores `answer.json`
/// as a chunk whose hash its receipt carries. Workers before this version need
/// `adapter_url` / `answer_upload_url` on the buyer's transfer server (the
/// buyer's `--legacy-http`); they cannot parse a spec without them, so they
/// never bid on a pieces-only request.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TextInferSpec {
    /// `sha256:` of adapter_model.safetensors.
    pub adapter_hash: String,
    pub base_model: String,
    pub messages: Vec<Value>,
    pub max_new_tokens: u32,
    /// `{"files": {"<name>": "<base64>"}}` for the adapter, fetched only on a cache miss.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub adapter_url: String,
    /// Base URL the worker PUTs `answer.json` to.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub answer_upload_url: String,
    /// Content hash of the adapter's [`AdapterManifest`] chunk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter_manifest: Option<String>,
    /// Return `answer.json` as a chunk (receipt `output_hash` = its content hash).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub answer_chunk: bool,
    /// Hex key the adapter pieces and manifest are sealed under (DIP-0032).
    /// With it the worker keeps the adapter only for this job, never in its
    /// cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter_key: Option<String>,
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
        if [&self.adapter_url, &self.answer_upload_url].iter().any(|u| !u.is_empty() && !is_http(u)) {
            bail!("adapter and answer URLs must be http(s)");
        }
        if self.adapter_manifest.as_deref().is_some_and(|h| c0mpute_proto::Hash::from_hex(h).is_err()) {
            bail!("adapter_manifest must be a hex content hash");
        }
        if self.adapter_key.as_deref().is_some_and(|k| crate::job_e2e::key_from_hex(k).is_err()) {
            bail!("adapter_key must be 32 bytes of hex");
        }
        if self.adapter_manifest.is_none() && self.adapter_url.is_empty() {
            bail!("the spec names no adapter: adapter_manifest or adapter_url");
        }
        if !self.answer_chunk && self.answer_upload_url.is_empty() {
            bail!("the spec names no way back for the answer: answer_chunk or an upload URL");
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
    let (client, url) = crate::net_guard::job_client(url, None).await?;
    let resp = client.get(url).send().await?.error_for_status()?;
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
/// `buyer` is the peer adapter pieces are fetched from first.
pub async fn run_infer(infernet: &Path, cache_root: &Path, job_id: &str, spec: &TextInferSpec, net: &Arc<Libp2pNetwork>, buyer: Option<PeerId>) -> Result<Vec<u8>> {
    spec.validate()?;
    // A sealed adapter (DIP-0032) is the buyer's private data: it lives only
    // in this job's directory, removed when the job ends, never in the cache.
    let input_key = spec.adapter_key.as_deref().map(crate::job_e2e::key_from_hex).transpose()?;
    let (adapter, _private_adapter) = match &input_key {
        Some(_) => {
            let d = crate::runner::job_subdir(&cache_root.join("text").join("infer-adapters"), job_id)?;
            (d.clone(), Some(RemoveDirOnDrop(d)))
        }
        None => (cache_root.join("text").join("adapters").join(spec.adapter_hash.trim_start_matches("sha256:")), None),
    };
    let cached = crate::temporal::worker::file_sha256(&adapter.join("adapter_model.safetensors")).ok();
    if cached.as_deref() != Some(spec.adapter_hash.as_str()) {
        match &spec.adapter_manifest {
            Some(m) => {
                let _ = tokio::fs::remove_dir_all(&adapter).await;
                let got = fetch_adapter_pieces(net, buyer, m, &adapter, input_key.as_ref()).await.context("fetch adapter pieces")?;
                if got != spec.adapter_hash {
                    let _ = tokio::fs::remove_dir_all(&adapter).await;
                    bail!("adapter {got} differs from the offer's {}", spec.adapter_hash);
                }
            }
            None => fetch_adapter(&spec.adapter_url, &spec.adapter_hash, &adapter).await.context("fetch adapter")?,
        }
    }
    let dir = crate::runner::job_subdir(&cache_root.join("text").join("infer"), job_id)?;
    tokio::fs::create_dir_all(&dir).await?;
    // The request holds the prompt: removed on every exit, timeouts included.
    let _job_dir = RemoveDirOnDrop(dir.clone());
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

pub fn spawn_infer_runner(net: Arc<Libp2pNetwork>, store: ChunkStore, cache_root: PathBuf, infernet: PathBuf, busy: Arc<AtomicBool>) -> mpsc::Sender<AcceptedJob> {
    let (tx, mut rx) = mpsc::channel::<AcceptedJob>(16);
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let accept = job.accept.clone();
            busy.store(true, Ordering::Relaxed);
            let result: Result<String> = crate::job_control::cancellable(&accept.job_id, async {
                let spec: TextInferSpec = serde_json::from_value(accept.spec_inline.clone().context("accept lacks spec_inline")?)?;
                let buyer = accept.buyer_peer_id.parse::<PeerId>().ok();
                let answer = run_infer(&infernet, &cache_root, &accept.job_id, &spec, &net, buyer).await?;
                if spec.answer_chunk {
                    // Sealed to the buyer for e2e jobs (one chunk either way).
                    let answer = crate::job_e2e::seal_result(&accept, &answer)?.unwrap_or(answer);
                    let hash = store.put(&answer).await.context("store answer")?;
                    net.announce(&hash).await.context("announce answer")?;
                    return Ok(hash.to_hex());
                }
                let (c, url) = crate::net_guard::job_client(&format!("{}/answer.json", spec.answer_upload_url), None).await?;
                c.put(url).body(answer.clone()).send().await?.error_for_status()?;
                Ok(sha256_hex(&answer))
            })
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
            && match crate::job_e2e::bid_spec(offer) {
                Some(crate::job_e2e::BidSpec::Public(v)) => {
                    valid_repo_id(v["base_model"].as_str().unwrap_or(""))
                        && v["max_new_tokens"].as_u64().is_some_and(|n| (1..=1024).contains(&n))
                }
                Some(crate::job_e2e::BidSpec::Full(v)) => {
                    serde_json::from_value::<TextInferSpec>(v.clone()).ok().is_some_and(|s| s.validate().is_ok())
                }
                None => false,
            }
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
    pub in_flight: InFlight,
}

impl Serving {
    /// Shutting down: tell every buyer whose job is still running that it
    /// failed (a `done{reason:"error"}` event on its run, then a Failed
    /// receipt), so it re-auctions now instead of waiting out its timeout.
    /// Returns how many jobs were failed.
    pub async fn fail_in_flight(&self) -> usize {
        let jobs: Vec<_> = self.in_flight.lock().unwrap_or_else(|e| e.into_inner()).drain().map(|(_, v)| v).collect();
        let topic = c0mpute_net::topics::job_topic(TRAIN_WORKLOAD);
        for (job, run_id) in &jobs {
            if let Some(run_id) = run_id {
                for event in [
                    json!({"v": 1, "type": "error", "message": "worker shutting down", "code": "E_WORKER_SHUTDOWN"}),
                    json!({"v": 1, "type": "done", "reason": "error"}),
                ] {
                    let msg = EventMsg { t: EVENT_MSG.into(), run_id: run_id.clone(), event };
                    if let Ok(b) = serde_json::to_vec(&msg) {
                        let _ = self.net.publish(&topic, b).await;
                    }
                }
            }
            warn!(job_id = %job.accept.job_id, "text: shutting down mid-job; reporting it failed");
            publish_receipt(&self.net, &job.accept, None, JobStatus::Failed).await;
        }
        if !jobs.is_empty() {
            // Let gossip carry the receipts before the process exits.
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }
        jobs.len()
    }
}

/// Resolves on Ctrl-C or SIGTERM (what `systemctl restart` sends).
pub async fn shutdown_signal() {
    #[cfg(unix)]
    if let Ok(mut term) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
        return;
    }
    let _ = tokio::signal::ctrl_c().await;
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
    // Adapter pieces live here until the buyer has fetched them.
    let store = ChunkStore::open(cache_root.join("text").join("store")).await?;
    let net = spawn_node_with_store(net_opts, store.clone()).await?;
    let hardware = json!({"version": env!("CARGO_PKG_VERSION"), "text_runtime": "peft", "ram_mb": ram_mb, "cuda": runtime["cuda"]});
    tokio::spawn(crate::capabilities::advertise_loop(net.clone(), tags.clone(), hardware, crate::capabilities::local_bench_score(), crate::capabilities::DEFAULT_ADVERTISE_INTERVAL));
    let busy = Arc::new(AtomicBool::new(false));
    let slots = Arc::new(Semaphore::new(1));
    let in_flight = InFlight::default();
    let tx = spawn_train_runner(net.clone(), store.clone(), cache_root.clone(), infernet.clone(), busy.clone(), slots, in_flight.clone());
    crate::dispatch::run_worker_subscriber_filtered(net.clone(), TRAIN_WORKLOAD.into(), tags.clone(), Some(tx), Some(train_offer_filter(busy)));
    if infer {
        let ibusy = Arc::new(AtomicBool::new(false));
        let itx = spawn_infer_runner(net.clone(), store.clone(), cache_root, infernet, ibusy.clone());
        crate::dispatch::run_worker_subscriber_filtered(net.clone(), INFER_WORKLOAD.into(), tags.clone(), Some(itx), Some(infer_offer_filter(ibusy)));
    }
    info!(peer = %net.peer_id().to_base58(), ?tags, "text: serving");
    tokio::time::sleep(Duration::from_millis(300)).await;
    Ok(Serving { net, tags, runtime, infer, in_flight })
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
    /// Also serve the corpus and take the adapter over HTTP from this machine,
    /// so workers older than the pieces transport can bid (they need to reach
    /// `public_url` / `bind`). Off by default: the job is fully peer to peer.
    pub legacy_http: bool,
    pub public_url: Option<String>,
    pub bind: SocketAddr,
    pub timeout_s: u64,
    pub retries: u32,
    pub bid_window: Duration,
    /// Replicated verification: run the job on this many independent workers
    /// with the same corpus, seed and step count, and keep a result a strict
    /// majority agrees on (see [`crate::train_verify`]). 1 (the default) is
    /// an ordinary single-worker job. Each replica is a separate job at up to
    /// `max_price_usd`, so the cost is up to `replicas` times one job. 0
    /// means "take it from the job file's `replicas`, else 1".
    pub replicas: u32,
    /// Relative weight-delta distance replicas may differ by and still agree.
    pub replica_tolerance: f64,
    /// Where the buyer remembers train workers that disagreed or did not
    /// deliver; `None` = `<data dir>/train-penalties.json`.
    pub penalty_file: Option<PathBuf>,
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
    /// Replicas the job will run (1 = one worker); each costs up to `max_price_usd`.
    pub replicas: u32,
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
        replicas: replica_count(req, &mut job.clone()).unwrap_or(1),
    };
    Ok((job, bytes, p))
}

/// Live progress, one line rewritten in place.
pub fn render(ev: &Value, total: &mut Option<u64>) -> Option<String> {
    let line = render_one(ev, total)?;
    Some(match ev["replica"].as_u64() {
        Some(k) => format!("[replica {}] {line}", k + 1),
        None => line,
    })
}

fn render_one(ev: &Value, total: &mut Option<u64>) -> Option<String> {
    let bar = |step: u64, total: u64| {
        let w = 24usize;
        let filled = ((step.min(total) as f64 / total.max(1) as f64) * w as f64).round() as usize;
        format!("[{}{}]", "#".repeat(filled), "-".repeat(w - filled))
    };
    match ev["type"].as_str()? {
        "replicas" => Some(format!(
            "replicated verification: {} workers, up to ${} each (${} total)",
            ev["replicas"], ev["max_price_usd_each"].as_f64().unwrap_or(0.0), ev["max_price_usd_total"].as_f64().unwrap_or(0.0)
        )),
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

/// Resolves once the worker on `run_id` has heartbeated and then said nothing
/// on its run for `silence`. Never resolves for a worker that predates
/// heartbeats, so those still run to the buyer's timeout as before. Any event
/// on the run counts as a sign of life once armed.
async fn worker_went_silent(mut gossip: tokio::sync::broadcast::Receiver<c0mpute_net::GossipMessage>, run_id: String, silence: Duration) {
    use tokio::sync::broadcast::error::RecvError;
    let topic = c0mpute_net::topics::job_topic(TRAIN_WORKLOAD);
    let mut last: Option<tokio::time::Instant> = None;
    loop {
        let m = match last {
            Some(t) => match tokio::time::timeout_at(t + silence, gossip.recv()).await {
                Ok(m) => m,
                Err(_) => return,
            },
            None => gossip.recv().await,
        };
        match m {
            Ok(m) if m.topic == topic => {
                let Ok(msg) = serde_json::from_slice::<EventMsg>(&m.data) else { continue };
                if msg.t == EVENT_MSG && msg.run_id == run_id && (last.is_some() || msg.event["type"] == "heartbeat") {
                    last = Some(tokio::time::Instant::now());
                }
            }
            Ok(_) => {}
            // Missed messages: it may have spoken, so do not call it dead.
            Err(RecvError::Lagged(_)) => {
                if last.is_some() {
                    last = Some(tokio::time::Instant::now());
                }
            }
            Err(RecvError::Closed) => std::future::pending::<()>().await,
        }
    }
}

fn silent_err() -> anyhow::Error {
    anyhow::anyhow!("E_WORKER_SILENT: the worker stopped heartbeating for {}s (restarted or crashed)", HEARTBEAT_SILENCE.as_secs())
}

/// Auction the job, stream its progress through `on_event`, verify and keep the adapter.
///
/// Peer to peer: the corpus goes out as pieces from `store` (served by `net`),
/// progress arrives on the job topic, and the adapter comes back as pieces
/// named by the manifest the worker's receipt points at. With
/// `req.legacy_http` the buyer also runs the HTTP transfer server, so a worker
/// that predates pieces can win; its adapter then arrives over HTTP and is
/// checked against the receipt exactly as before.
pub async fn train(net: Arc<Libp2pNetwork>, store: &ChunkStore, req: &TrainRequest, mut on_event: impl FnMut(&Value) + Send) -> Result<Value> {
    let (mut job, bytes, preview) = load(req)?;
    let replicas = replica_count(req, &mut job)?;
    if replicas > 1 && req.legacy_http {
        bail!("--replicas needs the peer-to-peer transport; drop --legacy-http");
    }
    // Sealed under a fresh key that only the winning worker learns.
    let (stored, corpus_key) = crate::pieces::put_bytes_pieces_sealed(&net, store, &bytes, crate::pieces::INPUT_PIECE_BYTES).await.context("store the corpus as pieces")?;
    let run_id = uuid::Uuid::new_v4().to_string();
    let mut spec = TextTrainSpec {
        job,
        dataset_hash: preview.dataset_hash.clone(),
        dataset_url: String::new(),
        artifact_upload_url: String::new(),
        events_url: None,
        corpus_hash: Some(stored.hash.clone()),
        corpus_size: (stored.pieces.len() > 1).then_some(stored.size),
        corpus_pieces: (stored.pieces.len() > 1).then(|| stored.pieces.clone()),
        corpus_key: Some(crate::job_e2e::key_to_hex(&corpus_key)),
        adapter_pieces: true,
        run_id: Some(run_id.clone()),
    };
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<SlotEvent>();
    let server = if req.legacy_http {
        let s = TransferServer::start_with(req.bind, req.public_url.clone(), bytes.clone(), &ARTIFACTS, Some(ev_tx)).await?;
        spec.dataset_url = s.dataset_url();
        spec.artifact_upload_url = s.upload_url(SLOT);
        spec.events_url = Some(s.events_url(SLOT));
        Some(s)
    } else {
        drop(ev_tx);
        None
    };
    spec.validate()?;
    let mut tags = required_tags(&bytes);
    drop(bytes);
    if req.gpu {
        tags.push("infernet:text:device:cuda".into());
    }
    if let Some(min) = req.min_ram_gb {
        let b = crate::temporal::caps::RAM_BUCKETS_GB.iter().copied().find(|b| *b >= min).unwrap_or(128);
        tags.push(format!("c0mpute:ram:{b}gb+"));
    }
    if replicas > 1 {
        let result = train_replicated(&net, req, &spec, &tags, replicas, &preview, &mut on_event).await;
        for h in stored.pieces.iter().chain(std::iter::once(&stored.hash)) {
            if let Ok(h) = c0mpute_proto::Hash::from_hex(h) {
                let _ = store.delete(&h).await;
            }
        }
        return result;
    }
    // Training events published on the job topic for this run.
    let topic = c0mpute_net::topics::job_topic(TRAIN_WORKLOAD);
    let mut gossip = net.messages();
    let p2p_event = |m: &c0mpute_net::GossipMessage| -> Option<Value> {
        if m.topic != topic {
            return None;
        }
        let msg: EventMsg = serde_json::from_slice(&m.data).ok()?;
        (msg.t == EVENT_MSG && msg.run_id == run_id && msg.event["type"] != "heartbeat").then_some(msg.event)
    };
    // Heartbeats keep the watchdog fed; they are not progress to show.
    let mut on_event = |ev: &Value| {
        if ev["type"] != "heartbeat" {
            on_event(ev)
        }
    };
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
        // Print progress while the auction waits for the receipt; give the
        // attempt up (and re-auction) if its worker goes silent.
        let auction = auction_with_mesh_retry(net.clone(), make);
        tokio::pin!(auction);
        let silent = worker_went_silent(net.messages(), run_id.clone(), HEARTBEAT_SILENCE);
        tokio::pin!(silent);
        let res = loop {
            tokio::select! {
                r = &mut auction => break r,
                _ = &mut silent => break Err(silent_err()),
                Some((_, ev)) = ev_rx.recv() => on_event(&ev),
                m = gossip.recv() => match m {
                    Ok(m) => if let Some(ev) = p2p_event(&m) { on_event(&ev) },
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => {}
                },
            }
        };
        while let Ok((_, ev)) = ev_rx.try_recv() {
            on_event(&ev);
        }
        while let Ok(m) = gossip.try_recv() {
            if let Some(ev) = p2p_event(&m) {
                on_event(&ev);
            }
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
    let result = async {
        let o = outcome.ok_or_else(|| last_err.unwrap_or_else(|| anyhow::anyhow!("training failed")))?;
        let claimed = o.receipt.output_hash.clone().unwrap_or_default();
        let (got, transport) = if claimed.starts_with("sha256:") {
            // A worker before pieces: it uploaded over HTTP.
            let server = server.as_ref().context("the worker answered with an HTTP upload, but this job has no HTTP transfer server")?;
            let files: HashMap<String, Vec<u8>> = server.take(SLOT).await;
            let weights = files.get("adapter_model.safetensors").context("worker did not upload adapter_model.safetensors")?;
            let got = sha256_hex(weights);
            if got != claimed {
                bail!("adapter {got} does not match the receipt's {claimed}");
            }
            std::fs::create_dir_all(&req.output)?;
            for (name, data) in &files {
                std::fs::write(req.output.join(name), data)?;
            }
            (got, "http")
        } else {
            let worker = o.receipt.worker_peer_id.parse::<PeerId>().ok();
            (fetch_adapter_pieces(&net, worker, &claimed, &req.output, Some(&o.result_key)).await?, "pieces")
        };
        Ok(json!({
            "adapter_dir": req.output, "adapter_hash": got, "base_model": preview.base_model, "dataset_hash": preview.dataset_hash,
            "transport": transport,
            "receipt": {"job_id": o.receipt.job_id, "worker_peer_id": o.receipt.worker_peer_id, "output_hash": o.receipt.output_hash, "price_usd": o.accepted_bid.price_usd}
        }))
    }
    .await;
    // The corpus pieces have done their job (stored whole too when small).
    for h in stored.pieces.iter().chain(std::iter::once(&stored.hash)) {
        if let Ok(h) = c0mpute_proto::Hash::from_hex(h) {
            let _ = store.delete(&h).await;
        }
    }
    result
}

/// How many replicas this job runs: the request's count, else the job file's
/// `replicas` (removed from the job either way: the worker's runner does not
/// read it), else 1. With more than one, the seed is pinned in the job so
/// every replica draws the same shuffle, split and LoRA init.
fn replica_count(req: &TrainRequest, job: &mut Value) -> Result<u32> {
    let from_job = job.as_object_mut().and_then(|o| o.remove("replicas"));
    let n = if req.replicas > 0 {
        req.replicas
    } else {
        match from_job {
            None => 1,
            Some(v) => v.as_u64().filter(|n| *n <= u32::MAX as u64).context("job.replicas must be a whole number")? as u32,
        }
    };
    if !(1..=crate::train_verify::MAX_REPLICAS).contains(&n) {
        bail!("replicas must be 1..={}, got {n}", crate::train_verify::MAX_REPLICAS);
    }
    if n > 1 {
        if !(req.replica_tolerance.is_finite() && req.replica_tolerance > 0.0 && req.replica_tolerance < 2.0) {
            bail!("--replica-tolerance must be in (0, 2)");
        }
        if !job["hyperparams"].is_object() {
            job["hyperparams"] = json!({});
        }
        if job["hyperparams"].get("seed").is_none() {
            job["hyperparams"]["seed"] = json!(0);
        }
    }
    Ok(n)
}

/// One replica's auction: bid phases run one at a time (`gate`), so each
/// replica's winner is known before the next offer goes out and can be
/// excluded from it; the training itself runs concurrently.
async fn replica_attempt(net: &Arc<Libp2pNetwork>, spec_v: &Value, tags: &[String], req: &TrainRequest, excluded_base: &[String], taken: &std::sync::Mutex<Vec<String>>, gate: &Arc<Semaphore>) -> Result<crate::buyer::AuctionOutcome> {
    let permit = gate.clone().acquire_owned().await?;
    let excluded: Vec<String> = excluded_base.iter().cloned().chain(taken.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned()).collect();
    let (tx, mut rx) = mpsc::unbounded_channel::<(String, c0mpute_net::topics::JobBid)>();
    let make = || {
        JobAuction::new(TRAIN_WORKLOAD, spec_v.clone())
            .with_required_capabilities(tags.to_vec())
            .with_allowed_bidders(req.workers.clone())
            .with_excluded_bidders(excluded.clone())
            .with_max_price_usd(req.max_price_usd)
            .with_bid_window(req.bid_window)
            .with_deadline(Duration::from_secs(req.timeout_s + 600))
            .with_receipt_timeout(Duration::from_secs(req.timeout_s + 300))
            .with_accept_notify(tx.clone())
    };
    let take = |peer: &str| {
        let mut t = taken.lock().unwrap_or_else(|e| e.into_inner());
        if !t.iter().any(|p| p == peer) {
            t.push(peer.to_string());
        }
    };
    let auction = auction_with_mesh_retry(net.clone(), make);
    tokio::pin!(auction);
    let silent = worker_went_silent(net.messages(), spec_v["run_id"].as_str().unwrap_or_default().to_string(), HEARTBEAT_SILENCE);
    tokio::pin!(silent);
    let mut permit = Some(permit);
    loop {
        tokio::select! {
            _ = &mut silent => return Err(silent_err()),
            r = &mut auction => {
                if let Ok(o) = &r {
                    take(&o.accepted_bid.bidder_peer_id);
                }
                return r;
            }
            Some((_, bid)) = rx.recv(), if permit.is_some() => {
                take(&bid.bidder_peer_id);
                permit = None;
            }
        }
    }
}

/// One replica slot, with the request's retries (never back to a worker that
/// already won a replica of this job).
async fn replica_slot(net: Arc<Libp2pNetwork>, spec_v: Value, tags: Vec<String>, req: TrainRequest, excluded: Vec<String>, taken: Arc<std::sync::Mutex<Vec<String>>>, gate: Arc<Semaphore>) -> Result<crate::buyer::AuctionOutcome> {
    let mut last_err = None;
    for attempt in 0..=req.retries.min(5) {
        match replica_attempt(&net, &spec_v, &tags, &req, &excluded, &taken, &gate).await {
            Ok(o) if o.receipt.status == JobStatus::Completed => return Ok(o),
            Ok(o) => last_err = Some(anyhow::anyhow!("worker {} reported {:?} (attempt {})", o.accepted_bid.bidder_peer_id, o.receipt.status, attempt + 1)),
            Err(e) => last_err = Some(e.context(format!("attempt {}", attempt + 1))),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("training failed")))
}

/// The buyer's memory of train workers that disagreed or did not deliver.
fn train_penalties(file: Option<&Path>) -> crate::penalty_box::PenaltyBox {
    if let Some(f) = file {
        return crate::penalty_box::PenaltyBox::persistent(f.to_path_buf());
    }
    match crate::config::data_dir() {
        Some(dir) if std::fs::create_dir_all(&dir).is_ok() => crate::penalty_box::PenaltyBox::persistent(dir.join("train-penalties.json")),
        _ => crate::penalty_box::PenaltyBox::new(),
    }
}

/// Run `replicas` copies of the job on distinct workers, fetch every adapter,
/// and keep the one a strict majority agrees on ([`crate::train_verify`]).
async fn train_replicated(net: &Arc<Libp2pNetwork>, req: &TrainRequest, spec: &TextTrainSpec, tags: &[String], replicas: u32, preview: &Preview, on_event: &mut (impl FnMut(&Value) + Send)) -> Result<Value> {
    use crate::train_verify::{self, Replica};
    let penalties = train_penalties(req.penalty_file.as_deref());
    let benched = penalties.benched();
    let taken = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let gate = Arc::new(Semaphore::new(1));
    let run_ids: Vec<String> = (0..replicas).map(|_| uuid::Uuid::new_v4().to_string()).collect();
    let mut slots = Vec::new();
    for run_id in &run_ids {
        let mut s = spec.clone();
        s.run_id = Some(run_id.clone());
        s.validate()?;
        slots.push(tokio::spawn(replica_slot(net.clone(), serde_json::to_value(&s)?, tags.to_vec(), req.clone(), benched.clone(), taken.clone(), gate.clone())));
    }
    on_event(&json!({"type": "replicas", "replicas": replicas, "max_price_usd_each": req.max_price_usd, "max_price_usd_total": req.max_price_usd * replicas as f64}));

    // Relay every replica's events, tagged with its index; keep each one's
    // last eval perplexity for the comparison.
    let topic = c0mpute_net::topics::job_topic(TRAIN_WORKLOAD);
    let mut gossip = net.messages();
    let mut evals: HashMap<usize, (u64, f64)> = HashMap::new();
    let mut relay = |m: &c0mpute_net::GossipMessage, on_event: &mut (dyn FnMut(&Value) + Send)| {
        if m.topic != topic {
            return;
        }
        let Ok(msg) = serde_json::from_slice::<EventMsg>(&m.data) else { return };
        let Some(k) = run_ids.iter().position(|r| *r == msg.run_id).filter(|_| msg.t == EVENT_MSG) else { return };
        let mut ev = msg.event;
        if ev["type"] == "heartbeat" {
            return;
        }
        if ev["type"] == "eval"
            && let Some(p) = ev["metrics"]["perplexity"].as_f64()
        {
            let step = ev["step"].as_u64().unwrap_or(0);
            if evals.get(&k).is_none_or(|(s, _)| step >= *s) {
                evals.insert(k, (step, p));
            }
        }
        ev["replica"] = json!(k);
        on_event(&ev);
    };
    let all = async {
        let mut out = Vec::with_capacity(slots.len());
        for h in slots {
            out.push(h.await.unwrap_or_else(|e| Err(anyhow::anyhow!("replica task failed: {e}"))));
        }
        out
    };
    tokio::pin!(all);
    let outcomes = loop {
        tokio::select! {
            r = &mut all => break r,
            m = gossip.recv() => match m {
                Ok(m) => relay(&m, on_event),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => {}
            },
        }
    };
    while let Ok(m) = gossip.try_recv() {
        relay(&m, on_event);
    }

    // Fetch each delivered adapter into its own directory.
    let scratch = req.output.join(".replicas");
    let _ = std::fs::remove_dir_all(&scratch);
    let mut got: Vec<(crate::buyer::AuctionOutcome, PathBuf, String, Replica)> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    for (k, outcome) in outcomes.into_iter().enumerate() {
        let o = match outcome {
            Ok(o) => o,
            Err(e) => {
                failures.push(format!("replica {k}: {e:#}"));
                continue;
            }
        };
        let worker = o.accepted_bid.bidder_peer_id.clone();
        let dir = scratch.join(k.to_string());
        let claimed = o.receipt.output_hash.clone().unwrap_or_default();
        let fetched = async {
            let hash = fetch_adapter_pieces(net, worker.parse::<PeerId>().ok(), &claimed, &dir, Some(&o.result_key)).await?;
            let r = Replica::from_dir(&worker, &dir, evals.get(&k).map(|(_, p)| *p))?;
            Ok::<_, anyhow::Error>((hash, r))
        }
        .await;
        match fetched {
            Ok((hash, r)) => got.push((o, dir, hash, r)),
            Err(e) => {
                // Won, then could not hand over a usable adapter.
                penalties.strike(&worker);
                failures.push(format!("replica {k} ({worker}): {e:#}"));
            }
        }
    }
    let reps: Vec<Replica> = got.iter().map(|g| g.3.clone()).collect();
    let decision = match train_verify::decide(&reps, replicas as usize, req.replica_tolerance, train_verify::DEFAULT_EVAL_TOLERANCE) {
        Ok(d) => d,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&scratch);
            if failures.is_empty() {
                return Err(e);
            }
            return Err(e.context(format!("not delivered: {}", failures.join("; "))));
        }
    };
    train_verify::apply_penalties(&penalties, &reps, &decision);
    let (o, dir, hash, _) = &got[decision.accepted];
    std::fs::create_dir_all(&req.output)?;
    for name in ARTIFACTS {
        let src = dir.join(name);
        if src.exists() {
            std::fs::copy(&src, req.output.join(name))?;
        }
    }
    let total: f64 = got.iter().map(|g| g.0.accepted_bid.price_usd).sum();
    let workers: Vec<&str> = got.iter().map(|g| g.0.receipt.worker_peer_id.as_str()).collect();
    let report = json!({
        "adapter_dir": req.output, "adapter_hash": hash, "base_model": preview.base_model, "dataset_hash": preview.dataset_hash,
        "transport": "pieces",
        "receipt": {"job_id": o.receipt.job_id, "worker_peer_id": o.receipt.worker_peer_id, "output_hash": o.receipt.output_hash, "price_usd": o.accepted_bid.price_usd},
        "replicas": {
            "requested": replicas,
            "returned": got.len(),
            "workers": workers,
            "accepted": decision.accepted,
            "agreeing": decision.agreeing,
            "outliers": decision.outliers.iter().map(|&i| workers[i]).collect::<Vec<_>>(),
            "distances": decision.distances,
            "tolerance": req.replica_tolerance,
            "not_delivered": failures,
            "price_usd_total": total,
        },
    });
    let _ = std::fs::remove_dir_all(&scratch);
    Ok(report)
}

/// Fetch the [`AdapterManifest`] a receipt names, then every file it lists,
/// into `out`; keep them only if adapter_model.safetensors hashes to the
/// manifest's `adapter_sha256`. Returns that hash.
///
/// With `key` the manifest and files are sealed (see [`store_adapter`]):
/// each is opened after its hash checks.
pub async fn fetch_adapter_pieces(net: &Arc<Libp2pNetwork>, worker: Option<PeerId>, manifest_hash: &str, out: &Path, key: Option<&c0mpute_net::topics::JobKey>) -> Result<String> {
    let mmax = if key.is_some() { crate::job_e2e::sealed_len(1 << 20) as usize } else { 1 << 20 };
    let mref = crate::pieces::InputRef::parse("adapter_manifest", manifest_hash, None, None, mmax).map_err(anyhow::Error::msg)?;
    let mut mbytes = mref.fetch(net, worker, mmax).await.context("fetch the adapter manifest")?;
    if let Some(k) = key {
        mbytes = crate::job_e2e::open_blob(k, &mbytes).context("adapter manifest does not open with the job key")?;
    }
    let manifest: AdapterManifest = serde_json::from_slice(&mbytes).context("adapter manifest is not JSON")?;
    if manifest.v != 1 || !crate::temporal::is_sha256_ref(&manifest.adapter_sha256) {
        bail!("adapter manifest has an unknown version or a bad adapter_sha256");
    }
    if !manifest.files.contains_key("adapter_model.safetensors") || manifest.files.keys().any(|k| !ARTIFACTS.contains(&k.as_str())) {
        bail!("adapter manifest must list adapter_model.safetensors and only {ARTIFACTS:?}");
    }
    let tmp = out.with_extension(format!("partial-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&tmp).await?;
    let fetched: Result<String> = async {
        for (name, f) in &manifest.files {
            let (size, pieces) = if f.pieces.len() > 1 { (Some(f.size), Some(f.pieces.as_slice())) } else { (None, None) };
            let fmax = if key.is_some() { crate::job_e2e::sealed_len(MAX_ADAPTER_FILE_BYTES as u64) as usize } else { MAX_ADAPTER_FILE_BYTES };
            let r = crate::pieces::InputRef::parse("adapter", &f.hash, size, pieces, fmax).map_err(anyhow::Error::msg)?;
            match key {
                Some(k) => {
                    let sealed = tmp.join(format!("{name}.sealed"));
                    r.fetch_to_file(net, worker, fmax, &sealed).await.with_context(|| format!("fetch {name}"))?;
                    let opened = crate::job_e2e::open_file(k, &sealed, &tmp.join(name)).await;
                    let _ = tokio::fs::remove_file(&sealed).await;
                    opened.with_context(|| format!("{name} does not open with the job key"))?;
                }
                None => r.fetch_to_file(net, worker, fmax, &tmp.join(name)).await.with_context(|| format!("fetch {name}"))?,
            }
        }
        let got = crate::temporal::worker::file_sha256(&tmp.join("adapter_model.safetensors"))?;
        if got != manifest.adapter_sha256 {
            bail!("adapter {got} does not match the manifest's {}", manifest.adapter_sha256);
        }
        Ok(got)
    }
    .await;
    match fetched {
        Ok(got) => {
            tokio::fs::create_dir_all(out).await?;
            for name in manifest.files.keys() {
                tokio::fs::rename(tmp.join(name), out.join(name)).await?;
            }
            let _ = tokio::fs::remove_dir_all(&tmp).await;
            Ok(got)
        }
        Err(e) => {
            let _ = tokio::fs::remove_dir_all(&tmp).await;
            Err(e)
        }
    }
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
    /// Also serve workers older than the pieces transport over HTTP.
    pub legacy_http: bool,
    pub public_url: Option<String>,
    pub bind: SocketAddr,
    pub timeout_s: u64,
    pub bid_window: Duration,
}

/// Ask a worker; returns the answer, its sources and the receipt.
///
/// Peer to peer: the adapter goes out as pieces from `store` (the worker
/// fetches them only on a cache miss) and the answer comes back as a chunk
/// named by the receipt. `req.legacy_http` adds the HTTP transfer server for
/// older workers.
pub async fn chat(net: Arc<Libp2pNetwork>, store: &ChunkStore, req: &ChatRequest) -> Result<Value> {
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
    // The adapter is the buyer's model: sealed under a fresh key that only
    // the winning worker learns (inside the sealed spec).
    let adapter_key = crate::job_e2e::random_key();
    let adapter_manifest = store_adapter(&net, store, &req.adapter_dir, &adapter_hash, Some(&adapter_key)).await.context("store the adapter as pieces")?;
    let mut spec = TextInferSpec {
        adapter_hash: adapter_hash.clone(),
        base_model: base_model.clone(),
        messages,
        max_new_tokens: req.max_new_tokens,
        adapter_url: String::new(),
        answer_upload_url: String::new(),
        adapter_manifest: Some(adapter_manifest),
        answer_chunk: true,
        adapter_key: Some(crate::job_e2e::key_to_hex(&adapter_key)),
    };
    let server = if req.legacy_http {
        let s = TransferServer::start_with(req.bind, req.public_url.clone(), bundle, &ANSWER_ARTIFACTS, None).await?;
        spec.adapter_url = s.dataset_url();
        spec.answer_upload_url = s.upload_url(ANSWER_SLOT);
        Some(s)
    } else {
        None
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
    let o = auction_with_mesh_retry(net.clone(), make).await?;
    if o.receipt.status != JobStatus::Completed {
        bail!("worker {} reported {:?}", o.accepted_bid.bidder_peer_id, o.receipt.status);
    }
    let claimed = o.receipt.output_hash.clone().unwrap_or_default();
    let answer = if claimed.starts_with("sha256:") {
        // A worker before pieces: it uploaded over HTTP.
        let server = server.as_ref().context("the worker answered over HTTP, but this request has no transfer server")?;
        let files = server.take(ANSWER_SLOT).await;
        let answer = files.get("answer.json").context("worker did not upload answer.json")?.clone();
        let got = sha256_hex(&answer);
        if got != claimed {
            bail!("answer {got} does not match the receipt's {claimed}");
        }
        answer
    } else {
        // The fetch checks the bytes against the receipt's content hash.
        let max = crate::job_e2e::sealed_len(1 << 20) as usize;
        let r = crate::pieces::InputRef::parse("answer", &claimed, None, None, max).map_err(anyhow::Error::msg)?;
        let sealed = r.fetch(&net, o.receipt.worker_peer_id.parse::<PeerId>().ok(), max).await.context("fetch the answer")?;
        o.open_result(&sealed).context("the answer does not open with this job's key")?
    };
    let ans: Value = serde_json::from_slice(&answer)?;
    if ans["adapter"].as_str().is_some_and(|a| a != adapter_hash) {
        bail!("the worker answered with adapter {} instead of {adapter_hash}", ans["adapter"]);
    }
    Ok(json!({
        "text": ans["text"], "sources": sources, "base_model": base_model, "adapter_hash": adapter_hash,
        "tokens": ans["tokens"], "seconds": ans["seconds"],
        "receipt": {"job_id": o.receipt.job_id, "worker_peer_id": o.receipt.worker_peer_id, "output_hash": o.receipt.output_hash, "price_usd": o.accepted_bid.price_usd}
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gossip(run_id: &str, ty: &str) -> c0mpute_net::GossipMessage {
        let msg = EventMsg { t: EVENT_MSG.into(), run_id: run_id.into(), event: json!({"v": 1, "type": ty}) };
        c0mpute_net::GossipMessage { topic: c0mpute_net::topics::job_topic(TRAIN_WORKLOAD), source: None, data: serde_json::to_vec(&msg).unwrap() }
    }

    #[tokio::test]
    async fn watchdog_arms_on_a_heartbeat_and_fires_on_silence() {
        let silence = Duration::from_millis(300);
        let (tx, rx) = tokio::sync::broadcast::channel(16);
        let w = tokio::spawn(worker_went_silent(rx, "run".into(), silence));
        // Unarmed: a worker that never heartbeats is left to the old timeout,
        // even when it sends other events.
        tx.send(gossip("run", "meta")).unwrap();
        tokio::time::sleep(silence * 2).await;
        assert!(!w.is_finished(), "fired before any heartbeat");
        tx.send(gossip("run", "heartbeat")).unwrap();
        // Armed: steps keep it alive; another run's heartbeats do not.
        for _ in 0..3 {
            tokio::time::sleep(silence / 2).await;
            tx.send(gossip("run", "step")).unwrap();
            tx.send(gossip("other", "heartbeat")).unwrap();
        }
        assert!(!w.is_finished(), "fired while the worker was talking");
        tokio::time::timeout(silence * 3, w).await.expect("fires after the worker goes quiet").unwrap();
    }

    fn infer_spec() -> TextInferSpec {
        TextInferSpec {
            adapter_key: None,
            adapter_hash: format!("sha256:{}", "b".repeat(64)),
            base_model: "HuggingFaceTB/SmolLM2-135M-Instruct".into(),
            messages: vec![json!({"role": "user", "content": "hi"})],
            max_new_tokens: 200,
            adapter_url: "http://127.0.0.1:1/t/x/dataset.json".into(),
            answer_upload_url: "http://127.0.0.1:1/t/x/a/answer".into(),
            adapter_manifest: None,
            answer_chunk: false,
        }
    }

    #[test]
    fn p2p_infer_specs() {
        let s = TextInferSpec {
            adapter_url: String::new(),
            answer_upload_url: String::new(),
            adapter_manifest: Some(c0mpute_proto::Hash::of(b"m").to_hex()),
            answer_chunk: true,
            ..infer_spec()
        };
        s.validate().unwrap();
        let v = serde_json::to_value(&s).unwrap();
        assert!(v.get("adapter_url").is_none());
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct OldInfer {
            adapter_url: String,
            answer_upload_url: String,
        }
        assert!(serde_json::from_value::<OldInfer>(v).is_err(), "old workers never bid on a pieces-only chat");
        assert!(TextInferSpec { adapter_manifest: None, ..s.clone() }.validate().is_err());
        assert!(TextInferSpec { answer_chunk: false, ..s.clone() }.validate().is_err());
        assert!(TextInferSpec { adapter_manifest: Some("zz".into()), ..s }.validate().is_err());
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
            corpus_key: None,
            job: json!({"kind": "lora", "base_model": "HuggingFaceTB/SmolLM2-135M-Instruct"}),
            dataset_url: "http://127.0.0.1:1/t/x/dataset.json".into(),
            dataset_hash: format!("sha256:{}", "a".repeat(64)),
            artifact_upload_url: "http://127.0.0.1:1/t/x/a/adapter".into(),
            events_url: Some("http://127.0.0.1:1/t/x/e/adapter".into()),
            corpus_hash: None,
            corpus_size: None,
            corpus_pieces: None,
            adapter_pieces: false,
            run_id: None,
        }
    }

    fn p2p_spec() -> TextTrainSpec {
        TextTrainSpec {
            corpus_key: None,
            dataset_url: String::new(),
            artifact_upload_url: String::new(),
            events_url: None,
            corpus_hash: Some(c0mpute_proto::Hash::of(b"corpus").to_hex()),
            adapter_pieces: true,
            run_id: Some("0e1f2a3b-run".into()),
            ..spec()
        }
    }

    #[test]
    fn p2p_specs_validate_and_old_workers_cannot_parse_them() {
        let s = p2p_spec();
        s.validate().unwrap();
        let v = serde_json::to_value(&s).unwrap();
        // No HTTP fields on the wire at all.
        assert!(v.get("dataset_url").is_none() && v.get("artifact_upload_url").is_none() && v.get("events_url").is_none());
        // A worker before pieces requires dataset_url, so it never bids.
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct OldSpec {
            job: Value,
            dataset_url: String,
            dataset_hash: String,
            artifact_upload_url: String,
        }
        assert!(serde_json::from_value::<OldSpec>(v.clone()).is_err());
        assert_eq!(serde_json::from_value::<TextTrainSpec>(v).unwrap(), s);
        // A legacy spec (an old buyer) still parses and validates.
        let old = json!({"job": spec().job, "dataset_url": "http://h/d", "dataset_hash": spec().dataset_hash, "artifact_upload_url": "http://h/u"});
        serde_json::from_value::<TextTrainSpec>(old).unwrap().validate().unwrap();

        let mut bad = p2p_spec();
        bad.corpus_hash = None;
        assert!(bad.validate().unwrap_err().to_string().contains("no corpus"));
        let mut bad = p2p_spec();
        bad.adapter_pieces = false;
        assert!(bad.validate().unwrap_err().to_string().contains("way back"));
        let mut bad = p2p_spec();
        bad.corpus_size = Some(3 << 20);
        bad.corpus_pieces = Some(vec![c0mpute_proto::Hash::of(b"x").to_hex()]);
        assert!(bad.validate().is_err(), "3 MiB cannot be one piece");
        let mut bad = p2p_spec();
        bad.run_id = Some("no spaces".into());
        assert!(bad.validate().is_err());
    }

    #[test]
    fn conversations_require_the_messages_format_tag() {
        let text = b"{\"title\":\"a\",\"text\":\"b\"}\n{\"title\":\"c\",\"text\":\"the word \\\"messages\\\" in text\"}\n";
        assert_eq!(required_tags(text), vec!["infernet:text:train".to_string()]);
        let chat = b"{\"title\":\"a\",\"text\":\"b\"}\n{\"messages\":[{\"role\":\"user\",\"content\":\"hi\"},{\"role\":\"assistant\",\"content\":\"yo\"}]}\n";
        assert_eq!(required_tags(chat), vec!["infernet:text:train".to_string(), MESSAGES_FORMAT_TAG.to_string()]);
        let t = worker_tags(&json!({"cuda": false, "formats": ["text", "messages"]}), 0);
        assert!(t.contains(&MESSAGES_FORMAT_TAG.to_string()));
        assert!(!worker_tags(&json!({"cuda": false}), 0).contains(&MESSAGES_FORMAT_TAG.to_string()));
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
