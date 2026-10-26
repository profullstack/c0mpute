//! Buyer side: submit training to a (trusted) worker and drive inference
//! sessions that stay pinned to the worker that opened them.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use c0mpute_net::topics::{JobStatus, job_topic};
use c0mpute_net::{GossipMessage, Libp2pNetwork};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;
use tokio::time::Instant;

use super::caps::{Requirements, required_tags};
use super::crypto;
use super::http::TransferServer;
use super::{INFER_WORKLOAD, SCHEMA_VERSION, SessionCall, SessionReady, SessionReply, SessionSpec, TRAIN_WORKLOAD, TrainSpec, select_family};
use crate::buyer::{JobAuction, run_auction};

/// RFC 3339 UTC timestamp without pulling in a date crate.
pub fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // civil_from_days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

fn sha256_bytes(b: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(b)))
}

pub struct TrainRequest {
    pub dataset_path: PathBuf,
    pub output: PathBuf,
    pub families: Vec<String>,
    pub workers: Vec<String>,
    pub max_price_usd: f64,
    pub requirements: Requirements,
    pub public_url: Option<String>,
    pub bind: std::net::SocketAddr,
    pub timeout_s: u64,
    pub retries: u32,
    pub hyperparameters: Value,
    pub bid_window: Duration,
}

/// What the buyer is about to send, for the consent preview.
#[derive(Debug, Serialize)]
pub struct TrainPreview {
    pub dataset_hash: String,
    pub episodes: usize,
    pub bytes: usize,
    pub families: Vec<String>,
    pub workers: Vec<String>,
    pub max_price_usd: f64,
    pub note: &'static str,
}

pub const PRIVACY_NOTE: &str = "The winning worker receives the redacted dataset in plaintext to train on; transport security does not hide it from that worker. Only pick workers you trust.";

pub fn read_dataset(p: &Path) -> Result<(Vec<u8>, String, usize)> {
    let bytes = std::fs::read(p).with_context(|| format!("read {}", p.display()))?;
    let v: Value = serde_json::from_slice(&bytes).context("dataset is not JSON")?;
    if v["kind"] != "dataset_bundle" || v["schema_version"] != SCHEMA_VERSION {
        bail!("{} is not a liquid-agent/v1 dataset_bundle", p.display());
    }
    let hash = v["manifest"]["dataset_hash"].as_str().context("manifest.dataset_hash missing")?.to_string();
    let n = v["episodes"].as_array().map(|a| a.len()).unwrap_or(0);
    Ok((bytes, hash, n))
}

pub fn preview(req: &TrainRequest) -> Result<TrainPreview> {
    let (bytes, dataset_hash, episodes) = read_dataset(&req.dataset_path)?;
    Ok(TrainPreview {
        dataset_hash,
        episodes,
        bytes: bytes.len(),
        families: req.families.clone(),
        workers: req.workers.clone(),
        max_price_usd: req.max_price_usd,
        note: PRIVACY_NOTE,
    })
}

pub fn train_job(family: &str, dataset_hash: &str, req: &TrainRequest) -> Value {
    let mut hp = json!({"hidden_units": 64, "epochs": 40, "learning_rate": 0.005, "seed": 0});
    if let (Some(h), Some(o)) = (hp.as_object_mut(), req.hyperparameters.as_object()) {
        for (k, v) in o {
            h.insert(k.clone(), v.clone());
        }
    }
    let idem = blake3::hash(format!("{dataset_hash}|{family}|{hp}").as_bytes()).to_hex().to_string();
    let mut resources = json!({"device": if req.requirements.gpu { "cuda" } else { "auto" }, "gpu_required": req.requirements.gpu});
    if let Some(gb) = req.requirements.min_ram_gb {
        resources["min_ram_mb"] = json!(gb * 1024);
    }
    json!({
        "schema_version": SCHEMA_VERSION,
        "kind": "train_job",
        "job_id": format!("tj_{}", &uuid::Uuid::new_v4().simple().to_string()[..16]),
        "idempotency_key": &idem[..32],
        "created_at": now_rfc3339(),
        "dataset_hash": dataset_hash,
        "dataset_ref": "c0mpute-transfer",
        "model_type": family,
        "hyperparameters": hp,
        "resources": resources,
        "limits": {"timeout_s": req.timeout_s, "max_retries": req.retries.min(5), "max_budget_usd": req.max_price_usd},
        "privacy": {"mode": "trusted_worker", "upload_approved": true, "worker_allowlist": req.workers},
        "retention": {"artifact_days": 30, "delete_dataset_after": true}
    })
}

/// Train every family on remote worker(s), one single-node job each, and
/// verify the artifacts against the receipts. Output matches
/// `infernet model train --json` plus `receipts`.
pub async fn train(net: Arc<Libp2pNetwork>, req: &TrainRequest) -> Result<Value> {
    let (bytes, dataset_hash, _) = read_dataset(&req.dataset_path)?;
    let server = TransferServer::start(req.bind, req.public_url.clone(), bytes).await?;
    let tags = required_tags(TRAIN_WORKLOAD, &req.requirements);
    let mut results = Vec::new();
    let mut receipts = Vec::new();
    for family in &req.families {
        let job = train_job(family, &dataset_hash, req);
        let spec = TrainSpec {
            train_job: job,
            dataset_url: server.dataset_url(),
            dataset_hash: dataset_hash.clone(),
            artifact_upload_url: Some(server.upload_url(family)),
        };
        spec.validate()?;
        let mut last_err = None;
        let mut outcome = None;
        for attempt in 0..=req.retries.min(5) {
            let auction = JobAuction::new(TRAIN_WORKLOAD, serde_json::to_value(&spec)?)
                .with_required_capabilities(tags.clone())
                .with_allowed_bidders(req.workers.clone())
                .with_max_price_usd(req.max_price_usd)
                .with_bid_window(req.bid_window)
                .with_deadline(Duration::from_secs(req.timeout_s + 600))
                .with_receipt_timeout(Duration::from_secs(req.timeout_s + 300));
            match run_auction(net.clone(), auction).await {
                Ok(o) if o.receipt.status == JobStatus::Completed => {
                    outcome = Some(o);
                    break;
                }
                Ok(o) => last_err = Some(anyhow::anyhow!("worker {} reported {:?} (attempt {})", o.accepted_bid.bidder_peer_id, o.receipt.status, attempt + 1)),
                Err(e) => last_err = Some(e.context(format!("attempt {}", attempt + 1))),
            }
        }
        let Some(o) = outcome else {
            return Err(last_err.unwrap_or_else(|| anyhow::anyhow!("training {family} failed")));
        };
        let files = server.take(family).await;
        let weights = files.get("model.safetensors").context("worker did not upload model.safetensors")?;
        let manifest_bytes = files.get("manifest.json").context("worker did not upload manifest.json")?;
        let manifest: Value = serde_json::from_slice(manifest_bytes)?;
        let got = sha256_bytes(weights);
        let claimed = o.receipt.output_hash.clone().unwrap_or_default();
        if got != claimed || manifest["weights"]["weights_hash"].as_str() != Some(got.as_str()) {
            bail!("artifact for {family} does not match the receipt: weights {got}, receipt {claimed}");
        }
        if manifest["training"]["dataset_hash"].as_str() != Some(dataset_hash.as_str()) {
            bail!("model for {family} was trained on a different dataset");
        }
        let dir = req.output.join(family);
        std::fs::create_dir_all(&dir)?;
        for (name, data) in &files {
            std::fs::write(dir.join(name), data)?;
        }
        let eval: Value = files.get("eval.json").and_then(|b| serde_json::from_slice(b).ok()).unwrap_or(json!({}));
        receipts.push(json!({
            "job_id": o.receipt.job_id, "worker_peer_id": o.receipt.worker_peer_id,
            "output_hash": o.receipt.output_hash, "status": "completed", "price_usd": o.accepted_bid.price_usd
        }));
        results.push(json!({"family": family, "model_dir": dir, "model_hash": got, "eval": eval, "worker_peer_id": o.receipt.worker_peer_id}));
    }
    let scores: Vec<(String, f64)> = results
        .iter()
        .map(|r| (r["family"].as_str().unwrap_or("").to_string(), r["eval"]["metrics"]["episode_success"].as_f64().unwrap_or(0.0)))
        .collect();
    Ok(json!({"dataset_hash": dataset_hash, "results": results, "selected": select_family(&scores), "receipts": receipts}))
}

// ── sessions ────────────────────────────────────────────────────────────

/// Local record of an open remote session (kept 0600 in the data dir).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionFile {
    pub session_id: String,
    pub model_hash: String,
    pub worker_peer_id: String,
    pub worker_pubkey: String,
    pub client_secret: String,
    pub account_scope: String,
    pub next_seq: i64,
    pub job_id: String,
}

impl SessionFile {
    pub fn path(dir: &Path, session_id: &str) -> PathBuf {
        dir.join(format!("{session_id}.json"))
    }

    pub fn load(dir: &Path, session_id: &str) -> Result<Option<Self>> {
        let p = Self::path(dir, session_id);
        if !p.exists() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_slice(&std::fs::read(p)?)?))
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        let p = Self::path(dir, &self.session_id);
        std::fs::write(&p, serde_json::to_vec_pretty(self)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    fn key(&self) -> Result<[u8; 32]> {
        crypto::session_key(&crypto::secret_from_hex(&self.client_secret)?, &self.worker_pubkey, &self.session_id)
    }
}

pub struct OpenRequest {
    pub model_hash: String,
    pub session_id: String,
    pub ttl_s: u64,
    pub account_scope: String,
    pub workers: Vec<String>,
    pub max_price_usd: f64,
    pub bid_window: Duration,
}

/// Auction a session open; the winner holds the session from then on.
pub async fn open_session(net: Arc<Libp2pNetwork>, req: &OpenRequest) -> Result<SessionFile> {
    let kp = crypto::keypair();
    let spec = SessionSpec {
        model_hash: req.model_hash.clone(),
        session_id: req.session_id.clone(),
        ttl_s: req.ttl_s,
        account_scope: req.account_scope.clone(),
        client_pubkey: kp.public_hex.clone(),
    };
    spec.validate()?;
    let mut rx = net.messages();
    let tags = required_tags(INFER_WORKLOAD, &Requirements { model_hash: Some(req.model_hash.clone()), ..Default::default() });
    let auction = JobAuction::new(INFER_WORKLOAD, serde_json::to_value(&spec)?)
        .with_required_capabilities(tags)
        .with_allowed_bidders(req.workers.clone())
        .with_max_price_usd(req.max_price_usd)
        .with_bid_window(req.bid_window)
        .with_receipt_timeout(Duration::from_secs(60));
    let o = run_auction(net.clone(), auction).await?;
    if o.receipt.status != JobStatus::Completed || o.receipt.output_hash.as_deref() != Some(req.model_hash.as_str()) {
        bail!("worker {} could not open the session ({:?})", o.accepted_bid.bidder_peer_id, o.receipt.status);
    }
    let topic = job_topic(INFER_WORKLOAD);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let msg = match tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), rx.recv()).await {
            Ok(Ok(m)) => m,
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            _ => break,
        };
        if msg.topic != topic {
            continue;
        }
        if let Ok(r) = serde_json::from_slice::<SessionReady>(&msg.data) {
            if r.t == "temporal_ready" && r.job_id == o.job_id && r.worker_peer_id == o.accepted_bid.bidder_peer_id && from(&msg, &r.worker_peer_id) {
                return Ok(SessionFile {
                    session_id: req.session_id.clone(),
                    model_hash: req.model_hash.clone(),
                    worker_peer_id: r.worker_peer_id,
                    worker_pubkey: r.worker_pubkey,
                    client_secret: hex::encode(kp.secret.to_bytes()),
                    account_scope: req.account_scope.clone(),
                    next_seq: 0,
                    job_id: o.job_id,
                });
            }
        }
    }
    bail!("worker opened the session but its key never arrived")
}

fn from(msg: &GossipMessage, peer: &str) -> bool {
    msg.source.map(|p| p.to_base58()).as_deref() == Some(peer)
}

/// One sealed call to the session's worker (republished until answered,
/// since a fresh gossip mesh may drop the first copy).
pub async fn call(net: Arc<Libp2pNetwork>, sf: &SessionFile, body: Value, timeout: Duration) -> Result<Value> {
    let key = sf.key()?;
    let call_id = uuid::Uuid::new_v4().simple().to_string();
    let msg = SessionCall {
        t: "temporal_call".into(),
        session_id: sf.session_id.clone(),
        worker_peer_id: sf.worker_peer_id.clone(),
        call_id: call_id.clone(),
        sealed: crypto::seal(&key, body.to_string().as_bytes())?,
    };
    let topic = job_topic(INFER_WORKLOAD);
    net.subscribe(&topic).await?;
    let mut rx = net.messages();
    let payload = serde_json::to_vec(&msg)?;
    let deadline = Instant::now() + timeout;
    let mut next_send = Instant::now();
    while Instant::now() < deadline {
        if Instant::now() >= next_send {
            let _ = net.publish(&topic, payload.clone()).await;
            next_send = Instant::now() + Duration::from_secs(2);
        }
        let wait = next_send.min(deadline).saturating_duration_since(Instant::now());
        match tokio::time::timeout(wait, rx.recv()).await {
            Ok(Ok(m)) if m.topic == topic => {
                if let Ok(r) = serde_json::from_slice::<SessionReply>(&m.data) {
                    if r.t == "temporal_reply" && r.call_id == call_id && from(&m, &sf.worker_peer_id) {
                        let plain = crypto::open(&key, &r.sealed)?;
                        return Ok(serde_json::from_slice(&plain)?);
                    }
                }
            }
            Ok(Err(broadcast::error::RecvError::Closed)) => bail!("network closed"),
            _ => {}
        }
    }
    bail!("E_TIMEOUT: worker {} did not answer (session lost? reopen it)", sf.worker_peer_id)
}

/// Build a `PredictRequestV1` from a step file: either a full request or
/// `{observation, dt_ms?, prev_action?, task?}`.
pub fn predict_request(step: &Value, sf: &SessionFile) -> Result<Value> {
    if step.get("observation").is_none() {
        bail!("step needs an observation");
    }
    let mut r = json!({
        "schema_version": SCHEMA_VERSION,
        "kind": "predict_request",
        "session_id": sf.session_id,
        "model_hash": sf.model_hash,
        "sequence_index": step.get("sequence_index").and_then(|v| v.as_i64()).unwrap_or(sf.next_seq),
        "dt_ms": step.get("dt_ms").and_then(|v| v.as_u64()).unwrap_or(0),
        "observation": step["observation"],
        "prev_action": step.get("prev_action").cloned().unwrap_or(Value::Null)
    });
    if let Some(t) = step.get("task") {
        r["task"] = t.clone();
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_shape() {
        let t = now_rfc3339();
        assert_eq!(t.len(), 20);
        assert!(t.ends_with('Z') && t.as_bytes()[10] == b'T');
        assert!(t.starts_with("20"));
    }

    #[test]
    fn train_job_is_remote_approved_and_stable_idempotency() {
        let req = TrainRequest {
            dataset_path: "x".into(),
            output: "o".into(),
            families: vec!["cfc".into()],
            workers: vec!["peer1".into()],
            max_price_usd: 0.5,
            requirements: Requirements { min_ram_gb: Some(8), ..Default::default() },
            public_url: None,
            bind: "127.0.0.1:0".parse().unwrap(),
            timeout_s: 600,
            retries: 1,
            hyperparameters: json!({"epochs": 3}),
            bid_window: Duration::from_secs(1),
        };
        let h = format!("sha256:{}", "e".repeat(64));
        let a = train_job("cfc", &h, &req);
        let b = train_job("cfc", &h, &req);
        assert_eq!(a["idempotency_key"], b["idempotency_key"]);
        assert_ne!(a["job_id"], b["job_id"]);
        assert_ne!(train_job("gru", &h, &req)["idempotency_key"], a["idempotency_key"]);
        assert_eq!(a["privacy"]["upload_approved"], true);
        assert_eq!(a["hyperparameters"]["epochs"], 3);
        assert_eq!(a["resources"]["min_ram_mb"], 8192);
        let spec = TrainSpec { train_job: a, dataset_url: "http://h/d".into(), dataset_hash: h, artifact_upload_url: None };
        spec.validate().unwrap();
    }
}
