//! Worker side: one training job per node at a time, and inference
//! sessions pinned to the node that holds their hidden state.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use c0mpute_net::topics::{JobAccept, JobStatus, job_topic};
use c0mpute_net::{GossipMessage, Libp2pNetwork};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, broadcast, mpsc};
use tracing::{info, warn};

use super::crypto;
use super::http::ARTIFACTS;
use super::runtime::{RuntimeError, RuntimeProc};
use super::{INFER_WORKLOAD, SessionCall, SessionReady, SessionReply, SessionSpec, TrainSpec};
use crate::buyer::source_matches_claim;
use crate::runner::{AcceptedJob, publish_receipt};

const MAX_DATASET: u64 = 512 << 20;

pub fn file_sha256(p: &Path) -> Result<String> {
    let mut h = Sha256::new();
    let mut f = std::fs::File::open(p).with_context(|| format!("open {}", p.display()))?;
    std::io::copy(&mut f, &mut h)?;
    Ok(format!("sha256:{}", hex::encode(h.finalize())))
}

/// Content-addressed local model store: `<root>/<hex>/{manifest.json,model.safetensors}`.
#[derive(Clone, Debug)]
pub struct ModelStore {
    pub root: PathBuf,
}

impl ModelStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn dir_for(&self, model_hash: &str) -> PathBuf {
        self.root.join(model_hash.trim_start_matches("sha256:"))
    }

    /// Copy a model directory in after checking its weights hash. Returns the hash.
    pub fn import(&self, src: &Path) -> Result<String> {
        let manifest: Value = serde_json::from_slice(&std::fs::read(src.join("manifest.json"))?)?;
        let want = manifest["weights"]["weights_hash"].as_str().context("manifest lacks weights.weights_hash")?.to_string();
        let file = manifest["weights"]["file"].as_str().unwrap_or("model.safetensors");
        let got = file_sha256(&src.join(file))?;
        if got != want {
            bail!("weights {got} do not match the manifest's {want}");
        }
        let dst = self.dir_for(&want);
        if dst != src {
            std::fs::create_dir_all(&dst)?;
            for f in ARTIFACTS {
                if src.join(f).exists() {
                    std::fs::copy(src.join(f), dst.join(f))?;
                }
            }
        }
        Ok(want)
    }

    /// Resolve `sha256:<hex>` or a directory path to a verified model dir.
    pub fn resolve(&self, model: &str) -> Result<(PathBuf, String)> {
        let dir = if model.starts_with("sha256:") { self.dir_for(model) } else { PathBuf::from(model) };
        let hash = self.import(&dir)?;
        if model.starts_with("sha256:") && hash != model {
            bail!("stored model {hash} is not {model}");
        }
        Ok((self.dir_for(&hash), hash))
    }
}

// ── training ────────────────────────────────────────────────────────────

async fn download(url: &str, dst: &Path) -> Result<()> {
    let resp = reqwest::get(url).await?.error_for_status()?;
    if resp.content_length().is_some_and(|n| n > MAX_DATASET) {
        bail!("dataset larger than {} MiB", MAX_DATASET >> 20);
    }
    let bytes = resp.bytes().await?;
    if bytes.len() as u64 > MAX_DATASET {
        bail!("dataset larger than {} MiB", MAX_DATASET >> 20);
    }
    tokio::fs::write(dst, &bytes).await?;
    Ok(())
}

/// Run `infernet model train` for one accepted job; returns the model dir and hash.
pub async fn run_train(infernet: &Path, job_dir: &Path, spec: &TrainSpec) -> Result<(PathBuf, String)> {
    spec.validate()?;
    tokio::fs::create_dir_all(job_dir).await?;
    let dataset = job_dir.join("dataset.json");
    download(&spec.dataset_url, &dataset).await.context("fetch dataset")?;
    let job_path = job_dir.join("job.json");
    tokio::fs::write(&job_path, serde_json::to_vec_pretty(&spec.train_job)?).await?;
    let out_dir = job_dir.join("model");
    let timeout_s = spec.train_job["limits"]["timeout_s"].as_u64().unwrap_or(3600).clamp(1, 7 * 86_400);
    let child = tokio::process::Command::new(infernet)
        .args(["model", "train", "--json", "--dataset"])
        .arg(&dataset)
        .arg("--output")
        .arg(&out_dir)
        .arg("--job")
        .arg(&job_path)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(Duration::from_secs(timeout_s), child)
        .await
        .map_err(|_| anyhow::anyhow!("E_TIMEOUT: training exceeded {timeout_s}s"))??;
    if !out.status.success() {
        bail!("infernet model train failed: {}", String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("").trim());
    }
    let res: Value = serde_json::from_slice(&out.stdout).context("parse infernet model train --json")?;
    let r0 = res["results"].get(0).context("no results from infernet model train")?;
    let model_dir = PathBuf::from(r0["model_dir"].as_str().context("result lacks model_dir")?);
    let claimed = r0["model_hash"].as_str().unwrap_or("").to_string();
    let actual = file_sha256(&model_dir.join("model.safetensors"))?;
    if actual != claimed {
        bail!("trained weights hash {actual} differs from infernet's {claimed}");
    }
    Ok((model_dir, actual))
}

async fn upload(base: &str, model_dir: &Path) -> Result<()> {
    let c = reqwest::Client::new();
    for f in ARTIFACTS {
        let p = model_dir.join(f);
        if !p.exists() {
            continue;
        }
        c.put(format!("{base}/{f}")).body(tokio::fs::read(&p).await?).send().await?.error_for_status()?;
    }
    Ok(())
}

/// Runner for `infernet.temporal.train`. Jobs run one at a time (single
/// node, no multi-node training in v1). A repeated idempotency key reuses
/// the finished model instead of training again.
pub fn spawn_train_runner(
    net: Arc<Libp2pNetwork>,
    cache_root: PathBuf,
    infernet: PathBuf,
    store: ModelStore,
    busy: Arc<std::sync::atomic::AtomicBool>,
) -> mpsc::Sender<AcceptedJob> {
    let (tx, mut rx) = mpsc::channel::<AcceptedJob>(8);
    tokio::spawn(async move {
        let mut done: HashMap<String, (PathBuf, String)> = HashMap::new();
        while let Some(job) = rx.recv().await {
            let accept = job.accept.clone();
            busy.store(true, std::sync::atomic::Ordering::Relaxed);
            let result: Result<String> = async {
                let spec: TrainSpec = serde_json::from_value(accept.spec_inline.clone().context("accept lacks spec_inline")?)?;
                spec.validate()?;
                let key = spec.idempotency_key().to_string();
                let (dir, hash) = match done.get(&key) {
                    Some(hit) => hit.clone(),
                    None => {
                        let job_dir = cache_root.join("temporal").join("jobs").join(&accept.job_id);
                        let r = run_train(&infernet, &job_dir, &spec).await?;
                        done.insert(key, r.clone());
                        r
                    }
                };
                if let Err(e) = store.import(&dir) {
                    warn!(err = %e, "temporal: could not keep the model in the local store");
                }
                if let Some(base) = &spec.artifact_upload_url {
                    upload(base, &dir).await.context("upload artifacts")?;
                }
                Ok(hash)
            }
            .await;
            busy.store(false, std::sync::atomic::Ordering::Relaxed);
            match result {
                Ok(hash) => {
                    info!(job_id = %accept.job_id, model = %hash, "temporal: training complete");
                    publish_receipt(&net, &accept, Some(hash), JobStatus::Completed).await;
                }
                Err(e) => {
                    warn!(job_id = %accept.job_id, err = %e, "temporal: training failed");
                    publish_receipt(&net, &accept, None, JobStatus::Failed).await;
                }
            }
        }
    });
    tx
}

// ── inference ───────────────────────────────────────────────────────────

struct Held {
    owner: String,
    key: [u8; 32],
    runtime_sid: String,
}

/// Holds the runtime child and every session this node owns.
pub struct InferServer {
    pub runtime: Arc<RuntimeProc>,
    pub models: Vec<String>,
    sessions: Mutex<HashMap<String, Held>>,
    /// Replies by call id: a republished call gets the first answer back
    /// instead of running twice (exactly-once per call id).
    replies: Mutex<(HashMap<String, Option<String>>, std::collections::VecDeque<String>)>,
}

const REPLY_CACHE: usize = 1024;

fn held_key(owner: &str, sid: &str) -> String {
    format!("{owner}:{sid}")
}

impl InferServer {
    /// Load every model (verifying its hash) into a fresh runtime.
    pub async fn start(infernet: &Path, model_dirs: &[(PathBuf, String)], worker_id: &str, max_sessions: u32) -> Result<Arc<Self>> {
        let rt = RuntimeProc::spawn(infernet, &["--worker".into(), worker_id.into(), "--max-sessions".into(), max_sessions.to_string()]).await?;
        let mut models = Vec::new();
        for (dir, hash) in model_dirs {
            let r = rt
                .request("load", json!({"model_dir": dir, "expected_hash": hash}), Duration::from_secs(300))
                .await
                .map_err(|e| anyhow::anyhow!("load {}: {e}", dir.display()))?;
            models.push(r["model_hash"].as_str().unwrap_or(hash).to_string());
        }
        Ok(Arc::new(Self { runtime: rt, models, sessions: Mutex::new(HashMap::new()), replies: Mutex::new((HashMap::new(), Default::default())) }))
    }

    /// Open a session for `owner` (the buyer's authenticated peer id).
    /// Returns our X25519 public key.
    pub async fn open(&self, owner: &str, spec: &SessionSpec) -> Result<String> {
        spec.validate()?;
        if !self.models.contains(&spec.model_hash) {
            bail!("E_NOT_LOADED: model {} is not served here", spec.model_hash);
        }
        let runtime_sid = format!("{}.{}", &owner[owner.len().saturating_sub(16)..], spec.session_id);
        self.runtime
            .request(
                "open_session",
                json!({"model_hash": spec.model_hash, "session_id": runtime_sid, "ttl_s": spec.ttl_s, "account_scope": spec.account_scope}),
                Duration::from_secs(30),
            )
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let kp = crypto::keypair();
        let key = crypto::session_key(&kp.secret, &spec.client_pubkey, &spec.session_id)?;
        self.sessions.lock().await.insert(held_key(owner, &spec.session_id), Held { owner: owner.into(), key, runtime_sid });
        Ok(kp.public_hex)
    }

    /// Handle one sealed call from `owner`. Returns the sealed reply, or
    /// None when the session is not ours (another worker's, or forged).
    pub async fn call(&self, owner: &str, call: &SessionCall) -> Option<String> {
        let (key, runtime_sid) = {
            let s = self.sessions.lock().await;
            let h = s.get(&held_key(owner, &call.session_id))?;
            if h.owner != owner {
                return None;
            }
            (h.key, h.runtime_sid.clone())
        };
        let cache_key = format!("{owner}:{}:{}", call.session_id, call.call_id);
        {
            let mut c = self.replies.lock().await;
            match c.0.get(&cache_key) {
                Some(Some(hit)) => return Some(hit.clone()),
                Some(None) => return None, // the first copy is still running and will answer
                None => {
                    c.0.insert(cache_key.clone(), None);
                }
            }
        }
        let Ok(plain) = crypto::open(&key, &call.sealed) else {
            self.replies.lock().await.0.remove(&cache_key);
            return None;
        };
        let reply = match self.dispatch(&runtime_sid, &call.session_id, &plain).await {
            Ok(result) => json!({"ok": true, "result": result}),
            Err(e) => json!({"ok": false, "error": e}),
        };
        if serde_json::from_slice::<Value>(&plain).ok().and_then(|v| v["op"].as_str().map(str::to_string)).as_deref() == Some("close_session") {
            self.sessions.lock().await.remove(&held_key(owner, &call.session_id));
        }
        let sealed = crypto::seal(&key, reply.to_string().as_bytes()).ok()?;
        let mut c = self.replies.lock().await;
        c.0.insert(cache_key.clone(), Some(sealed.clone()));
        c.1.push_back(cache_key);
        while c.1.len() > REPLY_CACHE {
            if let Some(old) = c.1.pop_front() {
                c.0.remove(&old);
            }
        }
        Some(sealed)
    }

    async fn dispatch(&self, runtime_sid: &str, client_sid: &str, plain: &[u8]) -> std::result::Result<Value, RuntimeError> {
        let msg: Value = serde_json::from_slice(plain).map_err(|_| RuntimeError::new("E_BAD_REQUEST", "call is not JSON"))?;
        let op = msg["op"].as_str().unwrap_or("");
        let t = Duration::from_secs(30);
        let swap = |mut v: Value| {
            if v.get("session_id").is_some() {
                v["session_id"] = json!(client_sid);
            }
            v
        };
        match op {
            "predict" => {
                let mut req = msg["request"].clone();
                if !req.is_object() {
                    return Err(RuntimeError::new("E_BAD_REQUEST", "predict needs request"));
                }
                req["session_id"] = json!(runtime_sid);
                self.runtime.request("predict", json!({"request": req}), t).await.map(swap)
            }
            "reset" | "close_session" => self.runtime.request(op, json!({"session_id": runtime_sid}), t).await.map(swap),
            _ => Err(RuntimeError::new("E_BAD_REQUEST", format!("op {op} is not allowed over the network"))),
        }
    }

    pub async fn session_count(&self) -> usize {
        self.sessions.lock().await.len()
    }
}

/// Runner for `infernet.temporal.infer`: an accepted job opens one session,
/// publishes our key half, and receipts with the model hash.
pub fn spawn_infer_runner(net: Arc<Libp2pNetwork>, server: Arc<InferServer>) -> mpsc::Sender<AcceptedJob> {
    let (tx, mut rx) = mpsc::channel::<AcceptedJob>(32);
    let topic = job_topic(INFER_WORKLOAD);
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let accept: JobAccept = job.accept;
            let spec: Option<SessionSpec> = accept.spec_inline.clone().and_then(|v| serde_json::from_value(v).ok());
            let Some(spec) = spec else {
                publish_receipt(&net, &accept, None, JobStatus::Failed).await;
                continue;
            };
            match server.open(&accept.buyer_peer_id, &spec).await {
                Ok(pubkey) => {
                    let ready = SessionReady {
                        t: "temporal_ready".into(),
                        job_id: accept.job_id.clone(),
                        session_id: spec.session_id.clone(),
                        worker_peer_id: net.peer_id().to_base58(),
                        worker_pubkey: pubkey,
                        model_hash: spec.model_hash.clone(),
                    };
                    if let Ok(b) = serde_json::to_vec(&ready) {
                        let _ = net.publish(&topic, b).await;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    publish_receipt(&net, &accept, Some(spec.model_hash.clone()), JobStatus::Completed).await;
                }
                Err(e) => {
                    warn!(job_id = %accept.job_id, err = %e, "temporal: session open failed");
                    publish_receipt(&net, &accept, None, JobStatus::Failed).await;
                }
            }
        }
    });
    tx
}

/// Answer sealed calls for sessions this node holds. Calls are accepted
/// only from the peer that opened the session (gossipsub-signed source).
pub fn spawn_call_listener(net: Arc<Libp2pNetwork>, server: Arc<InferServer>) {
    let topic = job_topic(INFER_WORKLOAD);
    let me = net.peer_id().to_base58();
    let mut rx: broadcast::Receiver<GossipMessage> = net.messages();
    tokio::spawn(async move {
        loop {
            let msg = match rx.recv().await {
                Ok(m) => m,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return,
            };
            if msg.topic != topic {
                continue;
            }
            let Ok(call) = serde_json::from_slice::<SessionCall>(&msg.data) else { continue };
            if call.t != "temporal_call" || call.worker_peer_id != me {
                continue;
            }
            let Some(src) = msg.source.map(|p| p.to_base58()) else { continue };
            if !source_matches_claim(&msg, &src) {
                continue;
            }
            let (net, server, topic) = (net.clone(), server.clone(), topic.clone());
            tokio::spawn(async move {
                if let Some(sealed) = server.call(&src, &call).await {
                    let reply = SessionReply { t: "temporal_reply".into(), session_id: call.session_id, call_id: call.call_id, sealed };
                    if let Ok(b) = serde_json::to_vec(&reply) {
                        let _ = net.publish(&topic, b).await;
                    }
                }
            });
        }
    });
}

/// Bid only on train offers whose spec passes [`TrainSpec::validate`] and
/// only while no training job is running here (one job per node).
pub fn train_offer_filter(busy: Arc<std::sync::atomic::AtomicBool>) -> crate::dispatch::OfferFilter {
    Arc::new(move |offer| {
        if busy.load(std::sync::atomic::Ordering::Relaxed) {
            return false;
        }
        offer
            .spec_inline
            .as_ref()
            .and_then(|v| serde_json::from_value::<TrainSpec>(v.clone()).ok())
            .is_some_and(|s| s.validate().is_ok())
    })
}

/// Bid only on session offers for models this node serves.
pub fn infer_offer_filter(models: Vec<String>) -> crate::dispatch::OfferFilter {
    Arc::new(move |offer| {
        offer
            .spec_inline
            .as_ref()
            .and_then(|v| serde_json::from_value::<SessionSpec>(v.clone()).ok())
            .is_some_and(|s| s.validate().is_ok() && models.contains(&s.model_hash))
    })
}
