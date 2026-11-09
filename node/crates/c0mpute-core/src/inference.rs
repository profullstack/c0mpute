//! `infernet.inference`: chat completions from open models, peer to peer,
//! served by the worker's own OpenAI-compatible runtime.
//!
//! A buyer (`c0mpute chat --p2p`, or the `c0mpute jobs serve` bridge) auctions
//! a spec of chat messages on `c0mpute/jobs/infernet.inference`. A worker
//! answers it from the local runtime it already runs (Ollama on
//! `127.0.0.1:11434` by default, or any OpenAI-compatible server such as
//! llama.cpp's `llama-server`, set with `C0MPUTE_INFERENCE_URL`), stores the
//! answer as a chunk and publishes a receipt with its hash; the buyer fetches
//! it from the worker and checks it. No c0mpute.com, no `infernet` binary on
//! either side.
//!
//! Workers advertise `infernet:inference` plus one
//! `infernet:inference:model:<id>` tag per model the runtime lists at
//! `/v1/models`, and only while it answers; the list is refreshed every
//! minute. The runtime URL is operator configuration, never from a spec, so a
//! buyer cannot point a worker at another address. `C0MPUTE_INFERENCE_URL=off`
//! turns the role off.
//!
//! Answers are one-shot (no token streaming across the network yet).

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use c0mpute_net::topics::JobStatus;
use c0mpute_net::Libp2pNetwork;
use c0mpute_store::ChunkStore;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::runner::{AcceptedJob, WorkerLoad, publish_receipt};

pub const WORKLOAD: &str = "infernet.inference";
/// Capability tag: this node answers chat completions.
pub const ROLE_TAG: &str = "infernet:inference";
pub const DEFAULT_RUNTIME_URL: &str = "http://127.0.0.1:11434";
pub const RUNTIME_URL_ENV: &str = "C0MPUTE_INFERENCE_URL";
/// Concurrent answers per node: the local runtime is usually one GPU or a CPU.
pub const MAX_CONCURRENT: usize = 2;
pub const MAX_MESSAGES: usize = 64;
pub const MAX_PROMPT_BYTES: usize = 64 * 1024;
pub const MAX_TOKENS: u32 = 4096;
pub const DEFAULT_MAX_TOKENS: u32 = 512;
pub const MAX_RESULT_BYTES: usize = 256 * 1024;
const GENERATION_TIMEOUT: Duration = Duration::from_secs(300);
const REFRESH_EVERY: Duration = Duration::from_secs(60);

pub fn model_tag(model: &str) -> String {
    format!("{ROLE_TAG}:model:{model}")
}

/// What a buyer asks for.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ChatSpec {
    pub v: u32,
    /// A model id the worker's runtime lists; None means any.
    #[serde(default)]
    pub model: Option<String>,
    pub messages: Vec<Value>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
}

fn valid_model_id(m: &str) -> bool {
    !m.is_empty() && m.len() <= 128 && m.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '/' | '@'))
}

pub fn parse_spec(spec: &Value) -> std::result::Result<ChatSpec, String> {
    let s: ChatSpec = serde_json::from_value(spec.clone()).map_err(|e| format!("invalid {WORKLOAD} spec: {e}"))?;
    if s.v != 1 {
        return Err(format!("unsupported {WORKLOAD} spec version {}", s.v));
    }
    if s.model.as_deref().is_some_and(|m| !valid_model_id(m)) {
        return Err("model must be 1-128 characters of letters, digits and -_.:/@".into());
    }
    if s.messages.is_empty() || s.messages.len() > MAX_MESSAGES {
        return Err(format!("messages must hold 1..={MAX_MESSAGES} entries"));
    }
    let mut bytes = 0;
    for m in &s.messages {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("");
        if !["system", "user", "assistant"].contains(&role) {
            return Err("message role must be system, user or assistant".into());
        }
        bytes += m.get("content").and_then(Value::as_str).ok_or("message content must be a string")?.len();
    }
    if bytes > MAX_PROMPT_BYTES {
        return Err(format!("messages exceed {MAX_PROMPT_BYTES} bytes"));
    }
    if s.messages.last().and_then(|m| m.get("role")).and_then(Value::as_str) == Some("assistant") {
        return Err("the last message must be the user's (or system's), not the assistant's".into());
    }
    if s.max_tokens.is_some_and(|t| !(1..=MAX_TOKENS).contains(&t)) {
        return Err(format!("max_tokens must be 1..={MAX_TOKENS}"));
    }
    if s.temperature.is_some_and(|t| !(0.0..=2.0).contains(&t)) {
        return Err("temperature must be 0..=2".into());
    }
    Ok(s)
}

/// Tags a worker must carry for `spec`.
pub fn required_capabilities(spec: &Value) -> Vec<String> {
    let mut t = vec![ROLE_TAG.to_string()];
    if let Ok(ChatSpec { model: Some(m), .. }) = parse_spec(spec) {
        t.push(model_tag(&m));
    }
    t
}

/// A result the buyer accepts: `{v:1, ok:true, text, model}` or `{v:1, ok:false, error}`.
pub fn valid_result(value: &Value) -> bool {
    value.get("v").and_then(Value::as_u64) == Some(1)
        && match value.get("ok").and_then(Value::as_bool) {
            Some(true) => value.get("text").is_some_and(Value::is_string) && value.get("model").is_some_and(Value::is_string),
            Some(false) => value.get("error").is_some_and(Value::is_string),
            None => false,
        }
}

// ─── the local runtime ────────────────────────────────────────────────────

/// The operator's runtime base URL, or None when the role is turned off.
pub fn runtime_url() -> Option<String> {
    match std::env::var(RUNTIME_URL_ENV) {
        Ok(v) if matches!(v.trim().to_ascii_lowercase().as_str(), "off" | "0" | "false" | "no" | "") => None,
        Ok(v) => Some(v.trim().trim_end_matches('/').to_string()),
        Err(_) => Some(DEFAULT_RUNTIME_URL.to_string()),
    }
}

static READY: RwLock<Vec<String>> = RwLock::new(Vec::new());

/// Models the runtime listed at the last probe (empty: not ready, advertise nothing).
pub fn ready_models() -> Vec<String> {
    READY.read().map(|m| m.clone()).unwrap_or_default()
}

fn set_ready(models: Vec<String>) {
    if let Ok(mut m) = READY.write() {
        *m = models;
    }
}

/// Model ids at `{base}/v1/models` (OpenAI shape, as Ollama and llama-server serve it).
pub async fn list_models(base: &str) -> Result<Vec<String>> {
    let c = reqwest::Client::builder().timeout(Duration::from_secs(5)).build()?;
    let v: Value = c.get(format!("{base}/v1/models")).send().await?.error_for_status()?.json().await?;
    let mut models: Vec<String> = v["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["id"].as_str())
        .filter(|m| valid_model_id(m))
        // Embedding-only models cannot chat.
        .filter(|m| !m.contains("embed"))
        .map(String::from)
        .collect();
    models.sort();
    models.dedup();
    Ok(models)
}

/// Probe once now, then every minute, keeping [`ready_models`] current.
pub async fn refresh_loop(base: String) {
    let mut last: Option<Vec<String>> = None;
    loop {
        let models = list_models(&base).await.unwrap_or_default();
        if last.as_ref() != Some(&models) {
            if models.is_empty() {
                info!(runtime = %base, "inference: no OpenAI-compatible runtime with chat models; not advertising {WORKLOAD}");
            } else {
                info!(runtime = %base, ?models, "inference: serving {WORKLOAD}");
            }
            last = Some(models.clone());
        }
        set_ready(models);
        tokio::time::sleep(REFRESH_EVERY).await;
    }
}

/// Answer one spec from the runtime at `base`. Never errors: a failure is an
/// `ok: false` result the buyer can read.
pub async fn execute(base: &str, spec: &Value, ready: &[String]) -> Value {
    let started = Instant::now();
    let blob = match run(base, spec, ready).await {
        Ok(v) => v,
        Err(e) => json!({"v": 1, "ok": false, "error": format!("{e:#}")}),
    };
    let mut blob = blob;
    blob["duration_ms"] = json!(started.elapsed().as_millis() as u64);
    if serde_json::to_vec(&blob).map(|b| b.len()).unwrap_or(usize::MAX) > MAX_RESULT_BYTES {
        return json!({"v": 1, "ok": false, "error": format!("answer is over {MAX_RESULT_BYTES} bytes")});
    }
    blob
}

async fn run(base: &str, spec: &Value, ready: &[String]) -> Result<Value> {
    let s = parse_spec(spec).map_err(anyhow::Error::msg)?;
    let model = match &s.model {
        Some(m) if ready.iter().any(|r| r == m) => m.clone(),
        Some(m) => anyhow::bail!("model {m} is not served here"),
        None => ready.first().cloned().context("no model is served here")?,
    };
    let mut body = json!({
        "model": model,
        "messages": s.messages,
        "max_tokens": s.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        "stream": false,
    });
    if let Some(t) = s.temperature {
        body["temperature"] = json!(t);
    }
    let c = reqwest::Client::builder().timeout(GENERATION_TIMEOUT).build()?;
    let resp = c.post(format!("{base}/v1/chat/completions")).json(&body).send().await.context("reach the local runtime")?;
    let status = resp.status();
    let v: Value = resp.json().await.context("runtime answer is not JSON")?;
    if !status.is_success() {
        anyhow::bail!("runtime answered {status}: {}", v["error"]["message"].as_str().or(v["error"].as_str()).unwrap_or("?"));
    }
    let choice = &v["choices"][0];
    let text = choice["message"]["content"].as_str().context("runtime answer has no choices[0].message.content")?;
    Ok(json!({
        "v": 1, "ok": true, "model": model, "text": text,
        "finish_reason": choice["finish_reason"], "usage": v["usage"],
    }))
}

// ─── worker runner ────────────────────────────────────────────────────────

pub fn worker_load() -> WorkerLoad {
    WorkerLoad::new(Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT)), MAX_CONCURRENT)
}

/// Bid only on valid specs for a model served right now.
pub fn offer_filter() -> crate::dispatch::OfferFilter {
    Arc::new(|offer| {
        let ready = ready_models();
        if ready.is_empty() {
            return false;
        }
        match crate::job_e2e::bid_spec(offer) {
            // An e2e offer shows only the model; the whole spec is checked
            // again here once it is opened.
            Some(crate::job_e2e::BidSpec::Public(v)) => v.get("model").and_then(Value::as_str).is_none_or(|m| ready.iter().any(|r| r == m)),
            Some(crate::job_e2e::BidSpec::Full(v)) => parse_spec(v).ok().is_some_and(|s| s.model.is_none_or(|m| ready.contains(&m))),
            None => false,
        }
    })
}

pub fn spawn_inference_runner(net: Arc<Libp2pNetwork>, store: ChunkStore, base: String, load: WorkerLoad) -> mpsc::Sender<AcceptedJob> {
    let (tx, mut rx) = mpsc::channel::<AcceptedJob>(MAX_CONCURRENT * 4);
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let Ok(slot) = load.compute_slots().acquire_owned().await else { return };
            let (net, store, base, load) = (net.clone(), store.clone(), base.clone(), load.clone());
            tokio::spawn(async move {
                let _slot = slot;
                let started = Instant::now();
                let job_id = job.accept.job_id.clone();
                let result: Result<String> = crate::job_control::cancellable(&job.accept.job_id, async {
                    let spec = crate::workflow::resolve_spec(&net, &job).await?;
                    let answer = execute(&base, &spec, &ready_models()).await;
                    let bytes = serde_json::to_vec(&answer)?;
                    // Sealed to the buyer (e2e jobs) before it is stored.
                    crate::pieces::publish_result_bytes(&net, &store, &job.accept, &bytes).await.context("store answer")
                })
                .await;
                match result {
                    Ok(hash) => {
                        load.record_job(started.elapsed());
                        info!(%job_id, %hash, "inference: answered");
                        publish_receipt(&net, &job.accept, Some(hash), JobStatus::Completed).await;
                    }
                    Err(e) => {
                        warn!(%job_id, err = %format!("{e:#}"), "inference: failed");
                        publish_receipt(&net, &job.accept, None, JobStatus::Failed).await;
                    }
                }
            });
        }
    });
    tx
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(v: Value) -> std::result::Result<ChatSpec, String> {
        parse_spec(&v)
    }

    #[test]
    fn spec_validation() {
        let ok = json!({"v": 1, "messages": [{"role": "system", "content": "be brief"}, {"role": "user", "content": "hi"}]});
        assert!(spec(ok.clone()).is_ok());
        assert_eq!(required_capabilities(&ok), vec![ROLE_TAG.to_string()]);
        let m = json!({"v": 1, "model": "qwen2.5:0.5b", "messages": [{"role": "user", "content": "hi"}], "max_tokens": 64, "temperature": 0.2});
        assert!(spec(m.clone()).is_ok());
        assert_eq!(required_capabilities(&m), vec![ROLE_TAG.to_string(), "infernet:inference:model:qwen2.5:0.5b".to_string()]);
        assert!(spec(json!({"v": 2, "messages": [{"role": "user", "content": "hi"}]})).unwrap_err().contains("version"));
        assert!(spec(json!({"v": 1, "messages": []})).is_err());
        assert!(spec(json!({"v": 1, "messages": [{"role": "tool", "content": "x"}]})).is_err());
        assert!(spec(json!({"v": 1, "messages": [{"role": "user", "content": 3}]})).is_err());
        assert!(spec(json!({"v": 1, "messages": [{"role": "user", "content": "x".repeat(MAX_PROMPT_BYTES + 1)}]})).is_err());
        assert!(spec(json!({"v": 1, "messages": [{"role": "user", "content": "q"}, {"role": "assistant", "content": "a"}]})).unwrap_err().contains("last message"));
        assert!(spec(json!({"v": 1, "model": "../etc passwd", "messages": [{"role": "user", "content": "q"}]})).is_err());
        assert!(spec(json!({"v": 1, "max_tokens": 0, "messages": [{"role": "user", "content": "q"}]})).is_err());
        assert!(spec(json!({"v": 1, "temperature": 3.0, "messages": [{"role": "user", "content": "q"}]})).is_err());
    }

    #[test]
    fn results() {
        assert!(valid_result(&json!({"v": 1, "ok": true, "text": "hi", "model": "m"})));
        assert!(valid_result(&json!({"v": 1, "ok": false, "error": "no"})));
        assert!(!valid_result(&json!({"v": 1, "ok": true})));
        assert!(!valid_result(&json!({"v": 2, "ok": false, "error": "no"})));
    }

    /// A stand-in OpenAI-compatible runtime on loopback.
    pub(crate) async fn fake_runtime() -> String {
        use axum::{Json, Router, routing::{get, post}};
        let app = Router::new()
            .route("/v1/models", get(|| async { Json(json!({"object": "list", "data": [{"id": "tiny:1b"}, {"id": "nomic-embed-text"}]})) }))
            .route(
                "/v1/chat/completions",
                post(|Json(b): Json<Value>| async move {
                    let last = b["messages"].as_array().and_then(|m| m.last()).and_then(|m| m["content"].as_str()).unwrap_or("").to_string();
                    Json(json!({"choices": [{"message": {"role": "assistant", "content": format!("echo[{}]: {last}", b["model"].as_str().unwrap_or(""))}, "finish_reason": "stop"}],
                                "usage": {"prompt_tokens": 3, "completion_tokens": 2}}))
                }),
            );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn answers_from_an_openai_compatible_runtime() {
        let base = fake_runtime().await;
        let models = list_models(&base).await.unwrap();
        assert_eq!(models, vec!["tiny:1b".to_string()], "embedding models are not chat models");
        let s = json!({"v": 1, "messages": [{"role": "user", "content": "ping"}]});
        let r = execute(&base, &s, &models).await;
        assert!(valid_result(&r), "{r}");
        assert_eq!(r["text"], "echo[tiny:1b]: ping");
        assert_eq!(r["model"], "tiny:1b");
        let wrong = json!({"v": 1, "model": "big:70b", "messages": [{"role": "user", "content": "ping"}]});
        let r = execute(&base, &wrong, &models).await;
        assert_eq!(r["ok"], false);
        assert!(r["error"].as_str().unwrap().contains("not served"));
        // Runtime down: an ok:false answer, not a crash.
        let r = execute("http://127.0.0.1:1", &s, &models).await;
        assert_eq!(r["ok"], false);
        assert!(list_models("http://127.0.0.1:1").await.is_err());
    }
}
