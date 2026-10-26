//! Infernet temporal policies on c0mpute: workloads `infernet.temporal.train`
//! and `infernet.temporal.infer` (CfC/LTC liquid networks, GRU baseline).
//!
//! c0mpute stays the infrastructure layer. The model code lives in Infernet
//! (`infernet model train|serve`, which runs the isolated infernet-liquid
//! Python runtime); this module only:
//!
//!   * advertises what the local Infernet can actually execute (`caps`),
//!   * turns buyer requirements into the capability tags the existing
//!     offer filter matches (runtime, CPU/GPU, RAM bucket, model hash),
//!   * runs one training job per node (`worker::spawn_train_runner`), and
//!   * keeps each inference session on the worker that holds its hidden
//!     state (`worker::InferServer`), with predict calls end-to-end encrypted
//!     between buyer and that worker over the job topic.
//!
//! Everything rides the existing offer → bid → accept → receipt protocol.
//! The receipt's `output_hash` is the model hash (`sha256:` of the
//! safetensors file) for training and for a session open.

pub mod caps;
pub mod client;
pub mod crypto;
pub mod http;
pub mod node;
pub mod runtime;
pub mod worker;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

pub const TRAIN_WORKLOAD: &str = "infernet.temporal.train";
pub const INFER_WORKLOAD: &str = "infernet.temporal.infer";
pub const SCHEMA_VERSION: &str = "liquid-agent/v1";
pub const FAMILIES: [&str; 3] = ["cfc", "ltc", "gru"];

/// `JobOffer.spec_inline` for `infernet.temporal.train`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TrainSpec {
    /// A liquid-agent/v1 `TrainJobV1`. Infernet validates it fully; the
    /// worker checks the fields that decide whether to bid at all.
    pub train_job: serde_json::Value,
    /// Where the worker fetches the `DatasetBundleV1` (http or https).
    pub dataset_url: String,
    /// Must equal `train_job.dataset_hash`; Infernet re-derives it from the
    /// bundle and refuses a mismatch.
    pub dataset_hash: String,
    /// Base URL the worker PUTs `manifest.json`, `model.safetensors` and
    /// `eval.json` to. Without it the artifacts stay on the worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_upload_url: Option<String>,
}

pub fn is_sha256_ref(s: &str) -> bool {
    s.strip_prefix("sha256:")
        .is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()))
}

fn is_http(u: &str) -> bool {
    u.starts_with("http://") || u.starts_with("https://")
}

impl TrainSpec {
    pub fn family(&self) -> &str {
        self.train_job.get("model_type").and_then(|v| v.as_str()).unwrap_or("")
    }

    pub fn idempotency_key(&self) -> &str {
        self.train_job.get("idempotency_key").and_then(|v| v.as_str()).unwrap_or("")
    }

    /// The checks a worker runs before bidding. A remote job without the
    /// user's upload approval is never accepted, whatever the buyer claims.
    pub fn validate(&self) -> Result<()> {
        let j = &self.train_job;
        if j.get("schema_version").and_then(|v| v.as_str()) != Some(SCHEMA_VERSION)
            || j.get("kind").and_then(|v| v.as_str()) != Some("train_job")
        {
            bail!("train_job is not a liquid-agent/v1 train_job");
        }
        if !FAMILIES.contains(&self.family()) {
            bail!("unknown model_type {:?}", self.family());
        }
        if !is_sha256_ref(&self.dataset_hash) {
            bail!("dataset_hash must be sha256:<64 hex>");
        }
        if j.get("dataset_hash").and_then(|v| v.as_str()) != Some(self.dataset_hash.as_str()) {
            bail!("train_job.dataset_hash differs from the offer's dataset_hash");
        }
        let privacy = j.get("privacy").cloned().unwrap_or_default();
        if privacy.get("mode").and_then(|v| v.as_str()) != Some("trusted_worker")
            || privacy.get("upload_approved").and_then(|v| v.as_bool()) != Some(true)
        {
            bail!("remote training needs privacy.mode=trusted_worker and upload_approved=true");
        }
        if self.idempotency_key().len() < 8 {
            bail!("train_job.idempotency_key is required");
        }
        if !is_http(&self.dataset_url) {
            bail!("dataset_url must be http(s)");
        }
        if let Some(u) = &self.artifact_upload_url {
            if !is_http(u) {
                bail!("artifact_upload_url must be http(s)");
            }
        }
        Ok(())
    }
}

/// `JobOffer.spec_inline` for `infernet.temporal.infer`: open one session.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SessionSpec {
    pub model_hash: String,
    pub session_id: String,
    pub ttl_s: u64,
    /// Opaque tenant id; sessions never share state across scopes.
    pub account_scope: String,
    /// Buyer's X25519 public key (hex) for the session's call encryption.
    pub client_pubkey: String,
}

impl SessionSpec {
    pub fn validate(&self) -> Result<()> {
        if !is_sha256_ref(&self.model_hash) {
            bail!("model_hash must be sha256:<64 hex>");
        }
        if self.session_id.is_empty()
            || self.session_id.len() > 64
            || !self.session_id.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        {
            bail!("session_id must be 1-64 of [A-Za-z0-9_.-]");
        }
        if !(1..=86_400).contains(&self.ttl_s) {
            bail!("ttl_s must be 1..86400");
        }
        if self.client_pubkey.len() != 64 || hex::decode(&self.client_pubkey).is_err() {
            bail!("client_pubkey must be 32 bytes hex");
        }
        Ok(())
    }
}

/// Worker → buyer after a session opens: the worker's half of the key exchange.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SessionReady {
    pub t: String, // "temporal_ready"
    pub job_id: String,
    pub session_id: String,
    pub worker_peer_id: String,
    pub worker_pubkey: String,
    pub model_hash: String,
}

/// Buyer → worker: one encrypted runtime op (`predict`, `reset`,
/// `close_session`, `checkpoint`) for a session that worker holds.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SessionCall {
    pub t: String, // "temporal_call"
    pub session_id: String,
    pub worker_peer_id: String,
    pub call_id: String,
    pub sealed: String,
}

/// Worker → buyer: the encrypted runtime response.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SessionReply {
    pub t: String, // "temporal_reply"
    pub session_id: String,
    pub call_id: String,
    pub sealed: String,
}

/// The liquid-agent rule: a liquid family is chosen only when it beats the
/// GRU baseline on the predeclared metric (`episode_success`).
pub fn select_family(results: &[(String, f64)]) -> Option<String> {
    let baseline = results.iter().find(|(f, _)| f == "gru").or(results.first())?;
    results
        .iter()
        .filter(|(f, s)| f != &baseline.0 && *s > baseline.1)
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .or(Some(baseline))
        .map(|(f, _)| f.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec() -> TrainSpec {
        let h = format!("sha256:{}", "a".repeat(64));
        TrainSpec {
            train_job: json!({
                "schema_version": SCHEMA_VERSION, "kind": "train_job", "job_id": "tj_test_0001",
                "idempotency_key": "idem-0001-abc", "dataset_hash": h, "model_type": "cfc",
                "privacy": {"mode": "trusted_worker", "upload_approved": true}
            }),
            dataset_url: "http://127.0.0.1:1/d/x/dataset.json".into(),
            dataset_hash: h,
            artifact_upload_url: None,
        }
    }

    #[test]
    fn train_spec_requires_upload_approval_and_matching_hash() {
        assert!(spec().validate().is_ok());
        let mut s = spec();
        s.train_job["privacy"]["upload_approved"] = json!(false);
        assert!(s.validate().unwrap_err().to_string().contains("upload_approved"));
        let mut s = spec();
        s.train_job["privacy"]["mode"] = json!("local_only");
        assert!(s.validate().is_err());
        let mut s = spec();
        s.dataset_hash = format!("sha256:{}", "b".repeat(64));
        assert!(s.validate().unwrap_err().to_string().contains("differs"));
        let mut s = spec();
        s.train_job["model_type"] = json!("transformer");
        assert!(s.validate().is_err());
        let mut s = spec();
        s.dataset_url = "file:///etc/passwd".into();
        assert!(s.validate().is_err());
    }

    #[test]
    fn session_spec_validation() {
        let ok = SessionSpec {
            model_hash: format!("sha256:{}", "c".repeat(64)),
            session_id: "s-1".into(),
            ttl_s: 600,
            account_scope: "acct".into(),
            client_pubkey: "00".repeat(32),
        };
        assert!(ok.validate().is_ok());
        let mut bad = ok.clone();
        bad.session_id = "a/b".into();
        assert!(bad.validate().is_err());
        let mut bad = ok.clone();
        bad.model_hash = "md5:1".into();
        assert!(bad.validate().is_err());
        let mut bad = ok;
        bad.ttl_s = 0;
        assert!(bad.validate().is_err());
    }

    #[test]
    fn liquid_wins_only_by_beating_gru() {
        let r = |v: &[(&str, f64)]| select_family(&v.iter().map(|(f, s)| (f.to_string(), *s)).collect::<Vec<_>>());
        assert_eq!(r(&[("cfc", 0.9), ("gru", 0.9)]).as_deref(), Some("gru"));
        assert_eq!(r(&[("cfc", 0.95), ("gru", 0.9)]).as_deref(), Some("cfc"));
        assert_eq!(r(&[("cfc", 0.8), ("gru", 0.9), ("ltc", 0.92)]).as_deref(), Some("ltc"));
        assert_eq!(r(&[("cfc", 0.8)]).as_deref(), Some("cfc"));
    }
}
