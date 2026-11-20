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
//! A node with no runtime of its own gets one: see `inference_auto`, which
//! installs a prebuilt llama.cpp `llama-server` and a small Qwen model and
//! serves it on `127.0.0.1:11439` (`C0MPUTE_INFERENCE_AUTO=0` opts out).
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
// Concurrent answers per node follow the runtime's real capacity: see
// `inference_slots` (detected at start, re-read every minute).
pub const MAX_MESSAGES: usize = 64;
/// Largest prompt (all messages) a chat carries: the house 9B serves 32k
/// tokens of context (~128 KB of text) per answer, and agent turns reach
/// 13k+ tokens. Specs above [`LEGACY_PROMPT_BYTES`] ride sealed point to
/// point like any other (never gossip) and need [`BIG_PROMPT_TAG`].
pub const MAX_PROMPT_BYTES: usize = 256 * 1024;
/// The limit before 0.3.86. A worker older than that refuses a bigger spec
/// once it has won it, so such offers require [`BIG_PROMPT_TAG`].
pub const LEGACY_PROMPT_BYTES: usize = 64 * 1024;
/// Workers that take prompts up to [`MAX_PROMPT_BYTES`] (0.3.86+).
pub const BIG_PROMPT_TAG: &str = "infernet:inference:prompt-256k";
pub const MAX_TOKENS: u32 = 4096;
pub const DEFAULT_MAX_TOKENS: u32 = 512;
pub const MAX_RESULT_BYTES: usize = 256 * 1024;
/// One answer, from sending it to the runtime to its last byte. The clock
/// starts only once the job holds a slot, and slots never outnumber what the
/// runtime runs in parallel, so a job is not timed while it waits its turn.
const GENERATION_TIMEOUT: Duration = Duration::from_secs(300);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Longest answer (runtime queue + generation, estimated) a node bids for:
/// 90% of [`GENERATION_TIMEOUT`], so an estimate near the limit does not bid
/// and time out (vienna bid 301 s on a 9B chat and timed out at 300 s,
/// 2026-11-18).
pub const MAX_BID_RUNTIME_SECS: u64 = GENERATION_TIMEOUT.as_secs() * 9 / 10;

/// Runtime slots busy with requests that are not this worker's (another
/// local user of the same Ollama), from the last probe.
static EXTERNAL_BUSY: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub fn external_busy() -> usize {
    EXTERNAL_BUSY.load(std::sync::atomic::Ordering::Relaxed)
}

/// Seconds a new answer waits inside the runtime behind other local users:
/// each full round of the runtime's `slots` takes about `per_job` seconds.
pub fn local_queue_wait_secs(external: usize, slots: usize, per_job: f64) -> u64 {
    let slots = slots.max(1);
    if external < slots {
        return 0;
    }
    ((external - slots) / slots + 1) as u64 * per_job.ceil() as u64
}

/// `(eta to add for the runtime's own queue, whether to bid at all)` for an
/// answer here: the runtime part (local queue + one answer) must fit
/// [`MAX_BID_RUNTIME_SECS`].
pub fn runtime_eta(external: usize, slots: usize, per_job: f64) -> (u64, bool) {
    let wait = local_queue_wait_secs(external, slots, per_job);
    (wait, wait + per_job.ceil() as u64 <= MAX_BID_RUNTIME_SECS)
}

/// Probe how busy the runtime is with other clients every few seconds
/// (llama-server or the local Ollama's runners: `/slots`).
pub async fn busy_loop(base: String, load: WorkerLoad) {
    loop {
        let external = match crate::inference_slots::runtime_busy(&base).await {
            Some(busy) => busy.saturating_sub(load.total_slots().saturating_sub(load.free_slots())),
            None => 0,
        };
        let before = EXTERNAL_BUSY.swap(external, std::sync::atomic::Ordering::Relaxed);
        if before != external {
            tracing::debug!(runtime = %base, external, "inference: runtime slots busy with other local clients");
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}
const REFRESH_EVERY: Duration = Duration::from_secs(60);

pub fn model_tag(model: &str) -> String {
    format!("{ROLE_TAG}:model:{model}")
}

/// Workers that understand `accept_models` (0.3.71+) carry this tag; offers
/// that list alternatives require it, so older workers never win a job they
/// would refuse once opened.
pub const ACCEPT_TAG: &str = "infernet:inference:accept";
/// At most this many alternatives on one offer.
pub const MAX_ACCEPT: usize = 8;

/// The network's default chat model: what house runtimes serve first and
/// what the router leads with. A 4B so every 8 GiB CPU node serves it (~10
/// tok/s on 8 cores, no thinking); the 9B ran only on GPUs and 16 GiB boxes,
/// so one rented GPU took every default chat (2026-11-20).
pub const HOUSE_DEFAULT_MODEL: &str = "huihui_ai/qwen3.5-abliterated:4b";

/// The bigger uncensored house model GPU nodes serve. Plain-text chats for
/// the default accept it as an upgrade, so GPUs still compete for them.
pub const HOUSE_GPU_MODEL: &str = "eriktonon/qwen3.8-uncensored:9b";

/// Smaller standard models a plain-text prompt for the key model may take
/// when every worker serving the key model is busy, best first. 0.5b is left
/// out on purpose: it parrots the conversation back.
pub const FALLBACK_LADDER: &[(&str, &[&str])] = &[
    // The house default (every 8 GiB+ CPU node serves it); the GPU 9B takes
    // it as an upgrade. qwen2.5:7b is no key and no alternative: it is not a
    // house model any more.
    (HOUSE_DEFAULT_MODEL, &[HOUSE_GPU_MODEL, "qwen3:4b", "qwen2.5:3b", "qwen2.5:1.5b"]),
    (HOUSE_GPU_MODEL, &[HOUSE_DEFAULT_MODEL, "qwen3:4b", "qwen2.5:3b", "qwen2.5:1.5b"]),
    ("qwen3:4b", &["qwen2.5:3b", "qwen2.5:1.5b"]),
    ("gemma3:4b", &["qwen3:4b", "qwen2.5:3b", "qwen2.5:1.5b"]),
];

/// Largest prompt still routed as "plain text" (bytes over all messages).
pub const PLAIN_TEXT_MAX_BYTES: usize = 16 * 1024;

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
    /// Other models this request takes when no worker serving `model` is
    /// free, best first. The jobs bridge fills it for plain-text prompts.
    #[serde(default)]
    pub accept_models: Vec<String>,
    /// Never substitute: this request needs exactly `model` (agent tool
    /// protocols, JSON output). The bridge then adds no alternatives.
    #[serde(default)]
    pub exact_model: bool,
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
    if s.accept_models.len() > MAX_ACCEPT || s.accept_models.iter().any(|m| !valid_model_id(m)) {
        return Err(format!("accept_models must hold up to {MAX_ACCEPT} valid model ids"));
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

/// Tags a worker must carry for `spec`. An offer that accepts alternatives
/// needs a worker that understands them, not one particular model.
pub fn required_capabilities(spec: &Value) -> Vec<String> {
    let mut t = vec![ROLE_TAG.to_string()];
    let parsed = parse_spec(spec);
    match &parsed {
        Ok(s) if !s.accept_models.is_empty() => t.push(ACCEPT_TAG.to_string()),
        Ok(ChatSpec { model: Some(m), .. }) => t.push(model_tag(m)),
        _ => {}
    }
    if parsed.as_ref().is_ok_and(|s| prompt_bytes(s) > LEGACY_PROMPT_BYTES) {
        t.push(BIG_PROMPT_TAG.to_string());
    }
    t
}

/// Bytes of message content in `s`.
pub fn prompt_bytes(s: &ChatSpec) -> usize {
    s.messages.iter().map(|m| m.get("content").and_then(Value::as_str).map_or(0, str::len)).sum()
}

/// The model to answer `model` / `accept_models` with from `ready`, and
/// whether it is an alternative: the asked-for model when served, else the
/// first served alternative; any served model when nothing was asked for.
pub fn choose_model(model: Option<&str>, accept: &[String], ready: &[String]) -> Option<(String, bool)> {
    match model {
        Some(m) if ready.iter().any(|r| r == m) => Some((m.to_string(), false)),
        Some(_) => accept.iter().find(|a| ready.contains(a)).map(|a| (a.clone(), true)),
        None if accept.is_empty() => ready.first().map(|m| (m.clone(), false)),
        None => accept.iter().find(|a| ready.contains(a)).map(|a| (a.clone(), false)),
    }
}

fn accept_of(v: &Value) -> Vec<String> {
    v.get("accept_models")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).filter(|m| valid_model_id(m)).take(MAX_ACCEPT).map(String::from).collect())
        .unwrap_or_default()
}

/// What this node would answer `offer` with, if anything (see [`choose_model`]).
pub fn bid_model(offer: &c0mpute_net::topics::JobOffer) -> Option<(String, bool)> {
    let ready = ready_models();
    if ready.is_empty() {
        return None;
    }
    match crate::job_e2e::bid_spec(offer)? {
        crate::job_e2e::BidSpec::Public(v) => choose_model(v.get("model").and_then(Value::as_str), &accept_of(v), &ready),
        crate::job_e2e::BidSpec::Full(v) => {
            let s = parse_spec(v).ok()?;
            choose_model(s.model.as_deref(), &s.accept_models, &ready)
        }
    }
}

/// A prompt any capable chat model can answer: no agent tool protocol, no
/// JSON-only output, not huge. Only these get smaller alternatives.
pub fn is_plain_text(s: &ChatSpec) -> bool {
    if s.exact_model {
        return false;
    }
    let mut bytes = 0;
    for m in &s.messages {
        let c = m.get("content").and_then(Value::as_str).unwrap_or("");
        bytes += c.len();
        let lc = c.to_ascii_lowercase();
        // Agent harnesses (moshcode, Claude-Code-style tool loops) and
        // OpenAI tool calling need the model the caller tuned them for.
        if ["<tool", "</tool>", "tool_result", "tool_call", "function_call", "\"tool\":", "<function"].iter().any(|k| lc.contains(k)) {
            return false;
        }
        // Structured output: a smaller model breaks the format more often.
        if lc.contains("json")
            && ["only json", "valid json", "json object", "json format", "in json", "as json", "json only", "json schema", "return json", "respond with json", "output json"]
                .iter()
                .any(|k| lc.contains(k))
        {
            return false;
        }
    }
    bytes <= PLAIN_TEXT_MAX_BYTES
}

/// The jobs bridge's routing step: a plain-text prompt for a standard model
/// also accepts smaller standard models (see [`FALLBACK_LADDER`]), so a free
/// worker can answer it instead of the job queuing on a working one. Returns
/// the widened spec, or None to send it as asked.
pub fn with_alternatives(spec: &Value) -> Option<Value> {
    let s = parse_spec(spec).ok()?;
    if !s.accept_models.is_empty() || !is_plain_text(&s) {
        return None;
    }
    let model = s.model.as_deref()?;
    let (_, ladder) = FALLBACK_LADDER.iter().find(|(k, _)| *k == model)?;
    let mut out = spec.clone();
    out["accept_models"] = json!(ladder.iter().take(MAX_ACCEPT).collect::<Vec<_>>());
    Some(out)
}

/// `spec` without alternatives: what the bridge sends when no worker that
/// understands them bid (an older network).
pub fn without_alternatives(spec: &Value) -> Value {
    let mut out = spec.clone();
    if let Some(o) = out.as_object_mut() {
        o.remove("accept_models");
    }
    out
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

/// True when an [`execute`] result is an answer, not an `ok: false` failure.
pub fn answer_ok(answer: &Value) -> bool {
    answer.get("ok").and_then(Value::as_bool) == Some(true)
}

/// Answer one spec from the runtime at `base`. Never errors: a failure is an
/// `ok: false` result the buyer can read.
/// Qwen 3.5 models (the house default 4B among them) think before answering
/// unless told not to. The 9B house GPU model's image is no-think already.
pub fn thinks_by_default(model: &str) -> bool {
    model.to_ascii_lowercase().contains("qwen3.5")
}

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
    let model = match choose_model(s.model.as_deref(), &s.accept_models, ready) {
        Some((m, _)) => m,
        None => match &s.model {
            Some(m) => anyhow::bail!("model {m} is not served here"),
            None => anyhow::bail!("no model is served here"),
        },
    };
    let mut messages = s.messages.clone();
    if let Some(system) = crate::inference_auto::default_system(base, &model) {
        if !messages.iter().any(|m| m.get("role").and_then(Value::as_str) == Some("system")) {
            messages.insert(0, json!({"role": "system", "content": system}));
        }
    }
    let mut body = json!({
        "model": model,
        "messages": messages,
        "max_tokens": s.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        "stream": false,
    });
    if let Some(t) = s.temperature {
        body["temperature"] = json!(t);
    }
    // Hybrid-thinking models reason before every answer on Ollama: the 4B
    // house default took 272 s for a haiku on a CPU Ollama, 10 s with this.
    // The house llama-server already runs them with --reasoning-budget 0.
    if thinks_by_default(&model) && base.trim_end_matches('/') != crate::inference_auto::auto_url() {
        body["reasoning_effort"] = json!("none");
    }
    let c = reqwest::Client::builder().connect_timeout(CONNECT_TIMEOUT).timeout(GENERATION_TIMEOUT).build()?;
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

/// The inference role's slots: `slots` answers at once (see
/// [`crate::inference_slots::detect`]; [`slots_loop`] keeps it current).
pub fn worker_load(slots: usize) -> WorkerLoad {
    let slots = slots.clamp(1, crate::inference_slots::MAX_SLOTS);
    // The runtime decides how many answers run at once and holds the model's
    // memory itself; each answer reserves only its small share of the node
    // budget, so the other roles see it and inference never waits on them
    // for more than that.
    let (_, cost) = crate::budget::for_workload(WORKLOAD);
    let budget = crate::budget::node();
    tracing::info!(role = "inference", workload = WORKLOAD, slots, cost = %cost, node = ?budget.totals(), "capacity: role slots from the runtime");
    // Job times survive restarts like every other workload's (runner::with_history):
    // without it each update reset every node's ETA to the 60 s placeholder, and
    // the bench score alone picked winners until real jobs finished again.
    WorkerLoad::new(Arc::new(tokio::sync::Semaphore::new(slots)), slots).with_workload(WORKLOAD).with_budget(budget, cost).with_history(
        crate::config::data_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("./c0mpute-data"))
            .join("job-times")
            .join(format!("{WORKLOAD}.ms")),
    )
}

static SLOTS: RwLock<Option<crate::inference_slots::Slots>> = RwLock::new(None);

/// The slot count the worker last decided, and why (None before the first).
pub fn current_slots() -> Option<crate::inference_slots::Slots> {
    SLOTS.read().ok().and_then(|s| s.clone())
}

/// Decide the slot count for `base` and log it (at start, and on a change).
pub async fn detect_slots(base: &str, config: &crate::config::InferenceConfig) -> crate::inference_slots::Slots {
    let slots = crate::inference_slots::detect(base, config).await;
    let changed = current_slots().as_ref() != Some(&slots);
    if changed {
        info!(runtime = %base, slots = slots.n, source = %slots.source, "inference: concurrent answers");
        if let Ok(mut s) = SLOTS.write() {
            *s = Some(slots.clone());
        }
    }
    slots
}

/// Re-read the runtime's capacity every minute and resize `load` to it, so
/// the node never bids for more answers than the runtime runs at once (an
/// operator restarting Ollama with another `OLLAMA_NUM_PARALLEL`, a runner
/// loading after the worker started).
pub async fn slots_loop(base: String, config: crate::config::InferenceConfig, load: WorkerLoad) {
    loop {
        tokio::time::sleep(REFRESH_EVERY).await;
        let slots = detect_slots(&base, &config).await;
        if slots.n != load.total_slots() {
            load.resize(slots.n);
        }
    }
}

/// Bid only on valid specs for a model served right now.
pub fn offer_filter() -> crate::dispatch::OfferFilter {
    Arc::new(|offer| {
        let ready = ready_models();
        // The house runtime yields to songs, images and a busy box.
        if ready.is_empty() || !crate::inference_auto::can_take_job() {
            return false;
        }
        match crate::job_e2e::bid_spec(offer) {
            // An e2e offer shows only the model; the whole spec is checked
            // again here once it is opened.
            Some(crate::job_e2e::BidSpec::Public(v)) => choose_model(v.get("model").and_then(Value::as_str), &accept_of(v), &ready).is_some(),
            Some(crate::job_e2e::BidSpec::Full(v)) => parse_spec(v).ok().is_some_and(|s| choose_model(s.model.as_deref(), &s.accept_models, &ready).is_some()),
            None => false,
        }
    })
}

pub fn spawn_inference_runner(net: Arc<Libp2pNetwork>, store: ChunkStore, base: String, load: WorkerLoad) -> mpsc::Sender<AcceptedJob> {
    let (tx, mut rx) = mpsc::channel::<AcceptedJob>(crate::inference_slots::MAX_SLOTS * 2);
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let Some(slot) = load.acquire().await else { return };
            let (net, store, base, load) = (net.clone(), store.clone(), base.clone(), load.clone());
            tokio::spawn(async move {
                let _slot = slot;
                let started = Instant::now();
                let job_id = job.accept.job_id.clone();
                let mut answered = false;
                let result: Result<String> = crate::job_control::cancellable(&job.accept.job_id, async {
                    let spec = crate::workflow::resolve_spec(&net, &job).await?;
                    let answer = execute(&base, &spec, &ready_models()).await;
                    answered = answer_ok(&answer);
                    let bytes = serde_json::to_vec(&answer)?;
                    // Sealed to the buyer (e2e jobs) before it is stored.
                    crate::pieces::publish_result_bytes(&net, &store, &job.accept, &bytes).await.context("store answer")
                })
                .await;
                match result {
                    Ok(hash) => {
                        // Only a real answer says how fast this box answers. A
                        // timed-out or failed one (`ok: false`) took the whole
                        // generation timeout: folded in, it pushed the average
                        // past MAX_BID_RUNTIME_SECS and the node never bid again
                        // (vienna 2026-11-19: 300431 ms stored, no bids for days).
                        if answered {
                            load.record_job(started.elapsed());
                        }
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

    #[test]
    fn qwen35_models_answer_without_thinking() {
        assert!(thinks_by_default(HOUSE_DEFAULT_MODEL));
        assert!(thinks_by_default("huihui_ai/qwen3.5-abliterated:0.8b"));
        assert!(!thinks_by_default(HOUSE_GPU_MODEL), "the 9B image is no-think");
        assert!(!thinks_by_default("qwen2.5:1.5b"));
    }

    #[test]
    fn only_real_answers_count_toward_job_time() {
        assert!(answer_ok(&json!({"v": 1, "ok": true, "text": "hi"})));
        assert!(!answer_ok(&json!({"v": 1, "ok": false, "error": "timed out"})));
        assert!(!answer_ok(&json!({"v": 1})));
    }

    fn ready(models: &[&str]) -> Vec<String> {
        models.iter().map(|m| m.to_string()).collect()
    }

    #[test]
    fn choose_model_prefers_the_asked_for_model_then_alternatives_in_order() {
        let alts = ready(&["qwen3:4b", "qwen2.5:1.5b"]);
        assert_eq!(choose_model(Some("qwen2.5:7b"), &alts, &ready(&["qwen2.5:7b", "qwen3:4b"])), Some(("qwen2.5:7b".into(), false)));
        assert_eq!(choose_model(Some("qwen2.5:7b"), &alts, &ready(&["qwen2.5:1.5b", "qwen3:4b"])), Some(("qwen3:4b".into(), true)));
        assert_eq!(choose_model(Some("qwen2.5:7b"), &alts, &ready(&["llama3"])), None);
        assert_eq!(choose_model(Some("qwen2.5:7b"), &[], &ready(&["qwen3:4b"])), None, "no alternatives: exact only");
        assert_eq!(choose_model(None, &[], &ready(&["m"])), Some(("m".into(), false)));
    }

    #[test]
    fn plain_text_prompts_get_smaller_alternatives_and_agent_or_json_ones_do_not() {
        let plain = json!({"v": 1, "model": HOUSE_DEFAULT_MODEL, "messages": [{"role": "user", "content": "write a haiku about rain"}]});
        let widened = with_alternatives(&plain).expect("plain text is widened");
        assert_eq!(widened["accept_models"], json!([HOUSE_GPU_MODEL, "qwen3:4b", "qwen2.5:3b", "qwen2.5:1.5b"]));
        assert_eq!(widened["model"], HOUSE_DEFAULT_MODEL, "the asked-for model stays first");
        assert_eq!(required_capabilities(&widened), vec![ROLE_TAG.to_string(), ACCEPT_TAG.to_string()]);
        assert_eq!(required_capabilities(&without_alternatives(&widened)), vec![ROLE_TAG.to_string(), model_tag(HOUSE_DEFAULT_MODEL)]);

        let agent = json!({"v": 1, "model": HOUSE_DEFAULT_MODEL, "messages": [
            {"role": "system", "content": "Call tools like <tool name=\"bash\">ls</tool>"},
            {"role": "user", "content": "fix the test"}]});
        assert!(with_alternatives(&agent).is_none(), "agent tool protocol keeps the exact model");
        let json_mode = json!({"v": 1, "model": HOUSE_DEFAULT_MODEL, "messages": [{"role": "user", "content": "Respond with JSON only: {\"title\": string}"}]});
        assert!(with_alternatives(&json_mode).is_none(), "JSON output keeps the exact model");
        let exact = json!({"v": 1, "model": HOUSE_DEFAULT_MODEL, "exact_model": true, "messages": [{"role": "user", "content": "hi"}]});
        assert!(with_alternatives(&exact).is_none(), "exact_model opts out");
        let big = json!({"v": 1, "model": HOUSE_DEFAULT_MODEL, "messages": [{"role": "user", "content": "x".repeat(PLAIN_TEXT_MAX_BYTES + 1)}]});
        assert!(with_alternatives(&big).is_none(), "long context keeps the exact model");
        let retired = json!({"v": 1, "model": "qwen2.5:7b", "messages": [{"role": "user", "content": "hi"}]});
        assert!(with_alternatives(&retired).is_none(), "qwen2.5:7b is no ladder key any more");
        assert!(FALLBACK_LADDER.iter().all(|(k, alts)| *k != "qwen2.5:7b" && !alts.contains(&"qwen2.5:7b")));
        let unknown = json!({"v": 1, "model": "devstral:24b", "messages": [{"role": "user", "content": "hi"}]});
        assert!(with_alternatives(&unknown).is_none(), "only standard models have a ladder");
        let too_many = json!({"v": 1, "model": "m", "accept_models": ["a","b","c","d","e","f","g","h","i"], "messages": [{"role": "user", "content": "hi"}]});
        assert!(parse_spec(&too_many).is_err());
    }

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
    fn big_prompts_fit_the_9b_context_and_need_new_workers() {
        let big = json!({"v": 1, "model": HOUSE_DEFAULT_MODEL, "messages": [{"role": "user", "content": "x".repeat(100 * 1024)}]});
        assert!(parse_spec(&big).is_ok(), "a ~100 KB agent turn is accepted");
        let req = required_capabilities(&big);
        assert!(req.contains(&BIG_PROMPT_TAG.to_string()), "only 0.3.86+ workers may win it: {req:?}");
        let small = json!({"v": 1, "model": HOUSE_DEFAULT_MODEL, "messages": [{"role": "user", "content": "x".repeat(LEGACY_PROMPT_BYTES)}]});
        assert!(!required_capabilities(&small).contains(&BIG_PROMPT_TAG.to_string()), "old workers still take up to 64 KB");
        let too_big = json!({"v": 1, "messages": [{"role": "user", "content": "x".repeat(MAX_PROMPT_BYTES + 1)}]});
        assert!(parse_spec(&too_big).is_err());
        assert!(MAX_PROMPT_BYTES + 64 * 1024 < crate::workflow::MAX_SPEC_BYTES, "the bridge's spec limit holds it");
        assert!(MAX_PROMPT_BYTES * 2 < crate::job_e2e::MAX_SEALED_SPEC, "the sealed spec holds it");
    }

    #[test]
    fn slow_runtimes_do_not_bid() {
        // vienna, 2026-11-18: a 9B chat at ~301 s on its CPU: no bid.
        assert_eq!(runtime_eta(0, 1, 301.0), (0, false));
        assert_eq!(runtime_eta(0, 1, 200.0), (0, true));
        // Another local user holds the only Ollama slot: wait one answer.
        assert_eq!(runtime_eta(1, 1, 100.0), (100, true));
        assert_eq!(runtime_eta(1, 1, 150.0), (150, false), "150 s wait + 150 s answer is over the limit");
        // Two slots, one taken by someone else: no wait.
        assert_eq!(local_queue_wait_secs(1, 2, 60.0), 0);
        assert_eq!(local_queue_wait_secs(4, 2, 60.0), 120);
        assert!(MAX_BID_RUNTIME_SECS < GENERATION_TIMEOUT.as_secs());
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
