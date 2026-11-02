//! `workflow.step`: one step of a webhook workflow (meshhook) run on a
//! c0mpute worker. DIP 0030.
//!
//! Two kinds, both pure CPU or a guarded outbound fetch; no code from the
//! network ever runs:
//!
//! - `transform`: apply a JMESPath expression to `input`.
//! - `http_call`: make one HTTP request the requester has fully resolved.
//!   http/https only; every resolved address must be public; the connection
//!   is pinned to the vetted addresses (no DNS rebinding); at most 5
//!   redirects, each hop re-vetted; 1 MiB response cap; 15 s default and
//!   30 s maximum timeout.
//!
//! Spec (the auction's `spec_inline`):
//! `{ "v": 1, "kind": "transform" | "http_call", "config": {..}, "input": <any> }`
//!
//! Result (stored in the worker's chunk store, named by the receipt's
//! `output_hash`, fetched by the buyer and checked against that hash):
//! `{ "v": 1, "ok": true, "output": <any>, "duration_ms": 12 }` or
//! `{ "v": 1, "ok": false, "error": "message", "duration_ms": 12 }`.
//!
//! A step that cannot run (bad spec, refused URL, transport error) still
//! completes with an `ok: false` result: the requester decides what to do.
//! A receipt marked `Failed` means the worker itself broke (could not fetch
//! a large spec, could not store the result).

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use c0mpute_net::topics::JobStatus;
use c0mpute_net::{Libp2pNetwork, Network};
use c0mpute_proto::{ChunkRequest, Hash};
use c0mpute_store::ChunkStore;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::net_guard;
use crate::runner::{AcceptedJob, WorkerLoad, publish_receipt};

/// Workload name and gossip topic suffix (`c0mpute/jobs/workflow.step`).
pub const WORKLOAD: &str = "workflow.step";
/// Capability tag a worker with the role advertises; buyers require it.
pub const ROLE_TAG: &str = "c0mpute:role:workflow-step";

/// Largest spec accepted, serialized.
pub const MAX_SPEC_BYTES: usize = 1024 * 1024;
/// Specs larger than this do not ride inline in the gossip offer (gossipsub
/// caps a message at 64 KiB). The buyer stores the spec as a chunk and the
/// winning worker fetches it from the buyer by hash.
pub const MAX_INLINE_SPEC_BYTES: usize = 32 * 1024;
/// Largest `http_call` response body read.
pub const MAX_RESPONSE_BODY_BYTES: usize = 1024 * 1024;
/// Largest result blob a worker stores or a buyer accepts.
pub const MAX_RESULT_BYTES: usize = 4 * 1024 * 1024;
/// Longest JMESPath expression accepted.
pub const MAX_EXPRESSION_BYTES: usize = 16 * 1024;
/// Deepest bracket nesting accepted in an expression. The
/// JMESPath parser recurses per level; this keeps a hostile expression from
/// exhausting the evaluating thread's stack.
pub const MAX_EXPRESSION_DEPTH: usize = 64;
/// Most operators (`.`, `|`, `&&`, `||`, `!`, comparisons, brackets) an
/// expression may hold, outside literals. Same reason as the depth cap.
pub const MAX_EXPRESSION_OPERATORS: usize = 512;
/// Wall-clock budget for one transform.
pub const TRANSFORM_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_HTTP_TIMEOUT_MS: u64 = 15_000;
pub const MAX_HTTP_TIMEOUT_MS: u64 = 30_000;
pub const MAX_REDIRECTS: usize = 5;
/// Steps a worker runs at once. They are light (a JMESPath search or a
/// network wait), so they draw on their own slots, not the node's CPU slots.
pub const MAX_CONCURRENT_STEPS: usize = 16;

const ALLOWED_METHODS: &[&str] = &["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];
/// Request headers a spec may not set: they control framing, the target
/// host or the connection, which the worker owns.
const FORBIDDEN_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "upgrade",
    "te",
    "trailer",
    "proxy-authorization",
    "proxy-connection",
];
/// Credentials dropped when a redirect leaves the original origin.
const CROSS_ORIGIN_STRIP: &[&str] = &["authorization", "cookie", "proxy-authorization"];

/// A parsed, validated step.
#[derive(Debug, Clone)]
pub enum Step {
    Transform { expression: String, input: Value },
    HttpCall(HttpCallConfig),
}

#[derive(Debug, Clone, Deserialize)]
pub struct HttpCallConfig {
    pub url: String,
    #[serde(default = "default_method")]
    pub method: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body: Option<Value>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

fn default_method() -> String {
    "GET".into()
}

/// Parse and validate a spec. `Err` carries the message for an `ok: false`
/// result.
pub fn parse_spec(spec: &Value) -> std::result::Result<Step, String> {
    let size = serde_json::to_vec(spec)
        .map(|b| b.len())
        .unwrap_or(usize::MAX);
    if size > MAX_SPEC_BYTES {
        return Err(format!(
            "spec is {size} bytes, over the {MAX_SPEC_BYTES}-byte limit"
        ));
    }
    let obj = spec.as_object().ok_or("spec must be a JSON object")?;
    match obj.get("v") {
        Some(v) if v.as_u64() == Some(1) => {}
        Some(v) => return Err(format!("unsupported spec version {v} (expected 1)")),
        None => return Err("spec is missing \"v\"".into()),
    }
    let kind = obj
        .get("kind")
        .and_then(Value::as_str)
        .ok_or("spec is missing \"kind\"")?;
    let config = obj.get("config").cloned().unwrap_or(Value::Null);
    let input = obj.get("input").cloned().unwrap_or(Value::Null);
    match kind {
        "transform" => {
            let expression = config
                .get("expression")
                .and_then(Value::as_str)
                .ok_or("transform needs config.expression (a JMESPath string)")?
                .to_string();
            check_expression(&expression)?;
            Ok(Step::Transform { expression, input })
        }
        "http_call" => {
            if !config.is_object() {
                return Err("http_call needs a config object".into());
            }
            let cfg: HttpCallConfig = serde_json::from_value(config)
                .map_err(|e| format!("invalid http_call config: {e}"))?;
            let method = cfg.method.to_ascii_uppercase();
            if !ALLOWED_METHODS.contains(&method.as_str()) {
                return Err(format!("unsupported HTTP method {}", cfg.method));
            }
            for (name, value) in &cfg.headers {
                let lower = name.to_ascii_lowercase();
                if FORBIDDEN_HEADERS.contains(&lower.as_str()) {
                    return Err(format!("header {name} may not be set"));
                }
                reqwest::header::HeaderName::from_bytes(name.as_bytes())
                    .map_err(|_| format!("invalid header name {name:?}"))?;
                reqwest::header::HeaderValue::from_str(value)
                    .map_err(|_| format!("invalid value for header {name}"))?;
            }
            Ok(Step::HttpCall(HttpCallConfig { method, ..cfg }))
        }
        other => Err(format!("unknown kind {other:?}")),
    }
}

fn check_expression(expression: &str) -> std::result::Result<(), String> {
    if expression.len() > MAX_EXPRESSION_BYTES {
        return Err(format!(
            "expression is over the {MAX_EXPRESSION_BYTES}-byte limit"
        ));
    }
    // The parser and interpreter recurse once per operator along a chain
    // (`a.b.c`, `a | b`, `!!x` all build left- or right-deep trees), so bound
    // both bracket nesting and the operator count. String and JSON literals
    // are skipped.
    let mut depth = 0usize;
    let mut max_depth = 0usize;
    let mut operators = 0usize;
    let mut chars = expression.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' | '`' => {
                let mut escaped = false;
                for d in chars.by_ref() {
                    if escaped {
                        escaped = false;
                    } else if d == '\\' {
                        escaped = true;
                    } else if d == c {
                        break;
                    }
                }
            }
            '(' | '[' | '{' => {
                depth += 1;
                max_depth = max_depth.max(depth);
                operators += 1;
            }
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            '.' | '|' | '&' | '!' | '<' | '>' | '=' => operators += 1,
            _ => {}
        }
    }
    if max_depth > MAX_EXPRESSION_DEPTH {
        return Err(format!(
            "expression nests deeper than {MAX_EXPRESSION_DEPTH} levels"
        ));
    }
    if operators > MAX_EXPRESSION_OPERATORS {
        return Err(format!(
            "expression is too complex (over {MAX_EXPRESSION_OPERATORS} operators)"
        ));
    }
    Ok(())
}

/// Run a spec end to end and return the result blob.
pub async fn execute(spec: &Value) -> Value {
    execute_with(spec, net_guard::is_public_ip).await
}

/// [`execute`] with a custom address policy (tests allow loopback).
pub async fn execute_with(spec: &Value, allow: fn(IpAddr) -> bool) -> Value {
    let started = Instant::now();
    let outcome = match parse_spec(spec) {
        Err(e) => Err(e),
        Ok(Step::Transform { expression, input }) => transform(expression, input).await,
        Ok(Step::HttpCall(cfg)) => http_call(cfg, allow).await.map_err(|e| format!("{e:#}")),
    };
    let duration_ms = started.elapsed().as_millis() as u64;
    let blob = match outcome {
        Ok(output) => json!({ "v": 1, "ok": true, "output": output, "duration_ms": duration_ms }),
        Err(error) => json!({ "v": 1, "ok": false, "error": error, "duration_ms": duration_ms }),
    };
    let size = serde_json::to_vec(&blob)
        .map(|b| b.len())
        .unwrap_or(usize::MAX);
    if size > MAX_RESULT_BYTES {
        return json!({
            "v": 1,
            "ok": false,
            "error": format!("result is over the {MAX_RESULT_BYTES}-byte limit"),
            "duration_ms": duration_ms,
        });
    }
    blob
}

/// Apply a JMESPath expression. Runs on its own thread with a large stack
/// (the parser and interpreter recurse) and a wall-clock budget; output is
/// serialized through a capped writer so a fan-out expression cannot
/// allocate without bound.
pub async fn transform(expression: String, input: Value) -> std::result::Result<Value, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("workflow-transform".into())
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            let _ = tx.send(transform_blocking(&expression, input));
        })
        .map_err(|e| format!("could not start transform: {e}"))?;
    match tokio::time::timeout(TRANSFORM_TIMEOUT, rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("transform aborted".into()),
        Err(_) => Err(format!(
            "transform exceeded {}s",
            TRANSFORM_TIMEOUT.as_secs()
        )),
    }
}

fn transform_blocking(expression: &str, input: Value) -> std::result::Result<Value, String> {
    let compiled =
        jmespath::compile(expression).map_err(|e| format!("invalid JMESPath expression: {e}"))?;
    let found = compiled
        .search(input)
        .map_err(|e| format!("JMESPath evaluation failed: {e}"))?;
    let mut out = CappedWriter::new(MAX_RESULT_BYTES);
    serde_json::to_writer(&mut out, &*found).map_err(|_| {
        if out.overflowed {
            format!("transform output is over the {MAX_RESULT_BYTES}-byte limit")
        } else {
            "could not serialize transform output".to_string()
        }
    })?;
    serde_json::from_slice(&out.buf).map_err(|e| format!("transform output is not JSON: {e}"))
}

struct CappedWriter {
    buf: Vec<u8>,
    cap: usize,
    overflowed: bool,
}

impl CappedWriter {
    fn new(cap: usize) -> Self {
        Self {
            buf: Vec::new(),
            cap,
            overflowed: false,
        }
    }
}

impl std::io::Write for CappedWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if self.buf.len() + data.len() > self.cap {
            self.overflowed = true;
            return Err(std::io::Error::other("output cap reached"));
        }
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Make the request. `Err` is a transport-level failure (refused URL, DNS,
/// connect, timeout, over-cap body, too many redirects); any HTTP status,
/// 2xx or not, is `Ok`.
pub async fn http_call(cfg: HttpCallConfig, allow: fn(IpAddr) -> bool) -> Result<Value> {
    use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, LOCATION, USER_AGENT};

    let timeout_ms = cfg
        .timeout_ms
        .unwrap_or(DEFAULT_HTTP_TIMEOUT_MS)
        .clamp(1, MAX_HTTP_TIMEOUT_MS);
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);

    let mut headers = HeaderMap::new();
    for (name, value) in &cfg.headers {
        headers.insert(
            HeaderName::from_bytes(name.as_bytes())?,
            HeaderValue::from_str(value)?,
        );
    }
    if !headers.contains_key(USER_AGENT) {
        headers.insert(
            USER_AGENT,
            HeaderValue::from_static(concat!("c0mpute-workflow-step/", env!("CARGO_PKG_VERSION"))),
        );
    }
    let mut body: Option<Vec<u8>> = match &cfg.body {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone().into_bytes()),
        Some(other) => {
            if !headers.contains_key(CONTENT_TYPE) {
                headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
            }
            Some(serde_json::to_vec(other)?)
        }
    };
    let mut method = reqwest::Method::from_bytes(cfg.method.as_bytes())?;
    let mut url = url::Url::parse(&cfg.url).context("invalid URL")?;

    let mut hops = 0usize;
    let response = loop {
        let target = tokio::time::timeout_at(deadline, net_guard::vet_parsed(url.clone(), allow))
            .await
            .map_err(|_| {
                anyhow::anyhow!("timed out resolving {}", url.host_str().unwrap_or(""))
            })??;
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            bail!("timed out after {timeout_ms} ms");
        }
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(remaining.min(Duration::from_secs(10)))
            .timeout(remaining);
        if let Some(host) = &target.pin_host {
            builder = builder.resolve_to_addrs(host, &target.addrs);
        }
        let client = builder.build().context("build http client")?;
        let mut request = client
            .request(method.clone(), url.clone())
            .headers(headers.clone());
        if let Some(b) = &body {
            request = request.body(b.clone());
        }
        let response = request.send().await.map_err(|e| {
            if e.is_timeout() {
                anyhow::anyhow!("timed out after {timeout_ms} ms")
            } else {
                anyhow::anyhow!("request failed: {e}")
            }
        })?;
        let status = response.status().as_u16();
        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let (true, Some(location)) = (matches!(status, 301 | 302 | 303 | 307 | 308), location)
        else {
            break response;
        };
        if hops >= MAX_REDIRECTS {
            bail!("too many redirects (more than {MAX_REDIRECTS})");
        }
        hops += 1;
        let next = url
            .join(&location)
            .with_context(|| format!("invalid redirect location {location:?}"))?;
        if next.origin() != url.origin() {
            for name in CROSS_ORIGIN_STRIP {
                headers.remove(*name);
            }
        }
        // Same method rewriting as browsers and reqwest's own policy.
        if status == 303 && method != reqwest::Method::HEAD
            || matches!(status, 301 | 302) && method == reqwest::Method::POST
        {
            method = reqwest::Method::GET;
            body = None;
            headers.remove(CONTENT_TYPE);
        }
        url = next;
    };

    let status = response.status().as_u16();
    let mut out_headers = serde_json::Map::new();
    for (name, value) in response.headers() {
        let value = String::from_utf8_lossy(value.as_bytes()).into_owned();
        let key = name.as_str().to_ascii_lowercase();
        match out_headers.get_mut(&key) {
            Some(Value::String(existing)) => {
                existing.push_str(", ");
                existing.push_str(&value);
            }
            _ => {
                out_headers.insert(key, Value::String(value));
            }
        }
    }
    let is_json = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| {
            let mime = ct
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            mime == "application/json" || mime.ends_with("+json")
        })
        .unwrap_or(false);
    let bytes = read_capped(response, deadline, timeout_ms).await?;
    let body = if is_json {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    } else {
        Value::String(String::from_utf8_lossy(&bytes).into_owned())
    };
    Ok(json!({ "status": status, "headers": out_headers, "body": body }))
}

async fn read_capped(
    mut response: reqwest::Response,
    deadline: tokio::time::Instant,
    timeout_ms: u64,
) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|n| n > MAX_RESPONSE_BODY_BYTES as u64)
    {
        bail!("response body is over the {MAX_RESPONSE_BODY_BYTES}-byte limit");
    }
    let mut buf = Vec::new();
    loop {
        let chunk = tokio::time::timeout_at(deadline, response.chunk())
            .await
            .map_err(|_| anyhow::anyhow!("timed out after {timeout_ms} ms reading the body"))?
            .context("read response body")?;
        let Some(chunk) = chunk else { break };
        if buf.len() + chunk.len() > MAX_RESPONSE_BODY_BYTES {
            bail!("response body is over the {MAX_RESPONSE_BODY_BYTES}-byte limit");
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

// ─── large specs: carried by hash ─────────────────────────────────────────

/// What rides in `spec_inline` when the spec is too big for a gossip message.
/// The worker fetches the spec from the buyer (the offer's publisher) by hash.
pub fn spec_ref(hash: &Hash, bytes: usize) -> Value {
    json!({ "spec_ref": { "hash": hash.to_hex(), "bytes": bytes } })
}

/// Resolve `spec_inline` to the real spec: itself, or the chunk its
/// `spec_ref` names, fetched from the buyer and checked against the hash.
async fn resolve_spec(net: &Libp2pNetwork, job: &AcceptedJob) -> Result<Value> {
    let spec = job
        .accept
        .spec_inline
        .clone()
        .context("accept missing spec_inline")?;
    let Some(r) = spec.get("spec_ref") else {
        return Ok(spec);
    };
    let hash = r
        .get("hash")
        .and_then(Value::as_str)
        .context("spec_ref.hash missing")?;
    let hash = Hash::from_hex(hash).context("invalid spec_ref.hash")?;
    let provider: c0mpute_net::PeerId = job
        .accept
        .buyer_peer_id
        .parse()
        .context("invalid buyer peer id")?;
    let request = ChunkRequest {
        chunk_hash: hash,
        shard_index: None,
    };
    let bytes = tokio::time::timeout(
        Duration::from_secs(20),
        net.fetch_from_peer(provider, &request),
    )
    .await
    .context("timed out fetching the spec from the buyer")??;
    if Hash::of(&bytes) != hash || bytes.len() > MAX_SPEC_BYTES {
        bail!("spec from the buyer does not match its hash or is over the limit");
    }
    serde_json::from_slice(&bytes).context("spec from the buyer is not JSON")
}

// ─── worker runner ────────────────────────────────────────────────────────

/// The load a `workflow.step` subscriber uses: its own slots, so light steps
/// neither wait on nor block transcodes holding the node's CPU slots.
pub fn worker_load() -> WorkerLoad {
    WorkerLoad::new(
        Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_STEPS)),
        MAX_CONCURRENT_STEPS,
    )
}

/// Spawn the runner; hand the sender to the dispatch loop. Steps run
/// concurrently up to the load's slots.
pub fn spawn_workflow_runner(
    net: Arc<Libp2pNetwork>,
    store: ChunkStore,
    load: WorkerLoad,
) -> mpsc::Sender<AcceptedJob> {
    let (tx, mut rx) = mpsc::channel::<AcceptedJob>(MAX_CONCURRENT_STEPS);
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
                match run_step_job(&net, &store, &job).await {
                    Ok(hash) => {
                        load.record_job(started.elapsed());
                        info!(%job_id, %hash, "workflow runner: step complete");
                        publish_receipt(&net, &job.accept, Some(hash), JobStatus::Completed).await;
                    }
                    Err(error) => {
                        warn!(%job_id, err = %error, "workflow runner: step failed");
                        publish_receipt(&net, &job.accept, None, JobStatus::Failed).await;
                    }
                }
            });
        }
    });
    tx
}

async fn run_step_job(
    net: &Libp2pNetwork,
    store: &ChunkStore,
    job: &AcceptedJob,
) -> Result<String> {
    let spec = resolve_spec(net, job).await?;
    let result = execute(&spec).await;
    let bytes = serde_json::to_vec(&result)?;
    let hash = store.put(&bytes).await.context("store step result")?;
    net.announce(&hash).await.context("announce step result")?;
    Ok(hash.to_hex())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loopback_ok(ip: IpAddr) -> bool {
        ip == IpAddr::from([127, 0, 0, 1]) || net_guard::is_public_ip(ip)
    }

    #[test]
    fn spec_parsing() {
        assert!(matches!(
            parse_spec(&json!({"v":1,"kind":"transform","config":{"expression":"a"},"input":{}})),
            Ok(Step::Transform { .. })
        ));
        let err = |v: Value| parse_spec(&v).unwrap_err();
        assert!(
            err(json!({"v":2,"kind":"transform","config":{"expression":"a"}})).contains("version")
        );
        assert!(err(json!({"kind":"transform"})).contains("\"v\""));
        assert!(err(json!({"v":1,"kind":"exec","config":{}})).contains("unknown kind"));
        assert!(err(json!({"v":1,"kind":"transform","config":{}})).contains("expression"));
        assert!(err(json!([1, 2])).contains("object"));
        assert!(
            err(
                json!({"v":1,"kind":"http_call","config":{"url":"https://x.test","method":"TRACE"}})
            )
            .contains("method")
        );
        assert!(err(json!({"v":1,"kind":"http_call","config":{"url":"https://x.test","headers":{"Host":"evil"}}})).contains("Host"));
        assert!(err(json!({"v":1,"kind":"http_call","config":{"url":"https://x.test","headers":{"x":1}}})).contains("invalid http_call config"));
        let big = "x".repeat(MAX_SPEC_BYTES);
        assert!(
            err(json!({"v":1,"kind":"transform","config":{"expression":"a"},"input":big}))
                .contains("limit")
        );
        let Ok(Step::HttpCall(cfg)) = parse_spec(
            &json!({"v":1,"kind":"http_call","config":{"url":"https://x.test","method":"post"}}),
        ) else {
            panic!("http_call parses");
        };
        assert_eq!(cfg.method, "POST");
    }

    #[test]
    fn deep_expressions_are_refused() {
        let deep = format!("{}a{}", "(".repeat(200), ")".repeat(200));
        assert!(check_expression(&deep).unwrap_err().contains("deeper"));
        assert!(
            check_expression(&"!".repeat(600))
                .unwrap_err()
                .contains("complex")
        );
        assert!(check_expression("people[?age > `20`].{name: name, tags: tags[0]}").is_ok());
        assert!(check_expression(&"x".repeat(MAX_EXPRESSION_BYTES + 1)).is_err());
    }

    #[tokio::test]
    async fn transform_applies_jmespath() {
        let r = execute(&json!({
            "v": 1, "kind": "transform",
            "config": {"expression": "people[?age > `20`].name"},
            "input": {"people": [{"name": "a", "age": 30}, {"name": "b", "age": 10}]}
        }))
        .await;
        assert_eq!(r["v"], 1);
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["output"], json!(["a"]));
        assert!(r["duration_ms"].is_u64());

        let r = execute(&json!({"v":1,"kind":"transform","config":{"expression":"{n: length(@)}"},"input":[1,2,3]})).await;
        assert_eq!(r["output"], json!({"n": 3}));

        let r =
            execute(&json!({"v":1,"kind":"transform","config":{"expression":"a[["},"input":{}}))
                .await;
        assert_eq!(r["ok"], false);
        assert!(r["error"].as_str().unwrap().contains("JMESPath"));

        let r = execute(&json!({"v":9,"kind":"transform"})).await;
        assert_eq!(r["ok"], false);
    }

    #[tokio::test]
    async fn long_chains_do_not_overflow_the_stack() {
        // Over the operator cap: refused before parsing.
        for expr in [vec!["a"; 4000].join("."), vec!["a"; 4000].join(" | ")] {
            let r =
                execute(&json!({"v":1,"kind":"transform","config":{"expression":expr},"input":{}}))
                    .await;
            assert!(r["error"].as_str().unwrap().contains("too complex"), "{r}");
        }
        // At the cap: runs on the transform thread without overflowing.
        let chain = vec!["a"; MAX_EXPRESSION_OPERATORS + 1].join(".");
        assert!(check_expression(&chain).is_ok());
        assert_eq!(transform(chain, json!({})).await.unwrap(), Value::Null);
        let pipes = vec!["a"; MAX_EXPRESSION_OPERATORS + 1].join(" | ");
        assert_eq!(transform(pipes, json!({})).await.unwrap(), Value::Null);
        let nots = format!("{}a", "!".repeat(MAX_EXPRESSION_OPERATORS));
        assert_eq!(
            transform(nots, json!({"a": true})).await.unwrap(),
            json!(true)
        );
        let nested = format!(
            "{}a{}",
            "(".repeat(MAX_EXPRESSION_DEPTH),
            ")".repeat(MAX_EXPRESSION_DEPTH)
        );
        assert_eq!(transform(nested, json!({"a": 1})).await.unwrap(), json!(1));
        // Brackets and dots inside literals do not count.
        assert!(check_expression(&format!("`{}`", "[".repeat(200))).is_ok());
    }

    #[tokio::test]
    async fn fan_out_transform_is_capped() {
        // Each pipe doubles the output; 2^30 copies must hit the cap, fast.
        let expr = std::iter::once("@".to_string())
            .chain(std::iter::repeat_n("[@, @]".to_string(), 30))
            .collect::<Vec<_>>()
            .join(" | ");
        let r = transform(expr, json!("x".repeat(64))).await;
        assert!(r.unwrap_err().contains("limit"));
    }

    #[tokio::test]
    async fn ssrf_targets_are_refused() {
        for url in [
            "http://127.0.0.1:9/",
            "http://localhost:9/",
            "http://10.0.0.5/",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]:9/",
            "http://100.64.1.1/",
            "file:///etc/passwd",
        ] {
            let r = execute(&json!({"v":1,"kind":"http_call","config":{"url":url,"method":"GET"}}))
                .await;
            assert_eq!(r["ok"], false, "{url}: {r}");
        }
    }

    async fn serve(app: axum::Router) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.ok() });
        addr
    }

    #[tokio::test]
    async fn http_call_round_trip_and_guards() {
        use axum::http::{HeaderMap, StatusCode, header};
        use axum::routing::{get, post};
        let app = axum::Router::new()
            .route(
                "/json",
                post(|headers: HeaderMap, body: String| async move {
                    let auth = headers
                        .get(header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    (
                        StatusCode::CREATED,
                        [(header::CONTENT_TYPE, "application/json")],
                        json!({"echo": body, "auth": auth}).to_string(),
                    )
                }),
            )
            .route("/text", get(|| async { (StatusCode::NOT_FOUND, "nope") }))
            .route(
                "/to-metadata",
                get(|| async {
                    (
                        StatusCode::FOUND,
                        [(header::LOCATION, "http://169.254.169.254/latest/meta-data/")],
                    )
                }),
            )
            .route(
                "/to-text",
                get(|| async { (StatusCode::FOUND, [(header::LOCATION, "/text")]) }),
            )
            .route(
                "/loop",
                get(|| async { (StatusCode::FOUND, [(header::LOCATION, "/loop")]) }),
            )
            .route(
                "/big",
                get(|| async { "x".repeat(MAX_RESPONSE_BODY_BYTES + 1) }),
            )
            .route(
                "/slow",
                get(|| async {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    "late"
                }),
            );
        let addr = serve(app).await;
        let call = |config: Value| async move {
            execute_with(
                &json!({"v":1,"kind":"http_call","config":config}),
                loopback_ok,
            )
            .await
        };

        let r = call(
            json!({"url": format!("http://{addr}/json"), "method": "POST",
            "headers": {"Authorization": "Bearer t"}, "body": {"a": 1}}),
        )
        .await;
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["output"]["status"], 201);
        assert_eq!(r["output"]["headers"]["content-type"], "application/json");
        assert_eq!(r["output"]["body"]["echo"], "{\"a\":1}");
        assert_eq!(r["output"]["body"]["auth"], "Bearer t");

        // Non-2xx is still ok:true; non-JSON body is text.
        let r = call(json!({"url": format!("http://{addr}/text"), "method": "GET"})).await;
        assert_eq!(r["ok"], true);
        assert_eq!(r["output"]["status"], 404);
        assert_eq!(r["output"]["body"], "nope");

        // Same-origin redirect is followed.
        let r = call(json!({"url": format!("http://{addr}/to-text"), "method": "GET"})).await;
        assert_eq!(r["output"]["status"], 404, "{r}");

        // A redirect to a private address is re-vetted and refused.
        let r = call(json!({"url": format!("http://{addr}/to-metadata"), "method": "GET"})).await;
        assert_eq!(r["ok"], false);
        assert!(r["error"].as_str().unwrap().contains("non-public"), "{r}");

        let r = call(json!({"url": format!("http://{addr}/loop"), "method": "GET"})).await;
        assert!(
            r["error"].as_str().unwrap().contains("too many redirects"),
            "{r}"
        );

        let r = call(json!({"url": format!("http://{addr}/big"), "method": "GET"})).await;
        assert_eq!(r["ok"], false);
        assert!(r["error"].as_str().unwrap().contains("limit"), "{r}");

        let r =
            call(json!({"url": format!("http://{addr}/slow"), "method": "GET", "timeout_ms": 300}))
                .await;
        assert_eq!(r["ok"], false);
        assert!(r["error"].as_str().unwrap().contains("timed out"), "{r}");
    }
}
