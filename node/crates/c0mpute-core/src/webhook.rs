//! Paid, end-to-end encrypted webhook delivery on c0mpute: workload
//! `webhook.deliver` (DIP-0028).
//!
//! A buyer has a webhook to send (URL, method, headers, body, optional
//! signing secret). `c0mpute webhook send` auctions one delivery to workers
//! that advertise `webhook:deliver`; the winner sends it from its own IP and
//! returns what the target answered.
//!
//! Gossip topics are readable by every subscriber, so nothing sensitive goes
//! in the offer. The offer carries only the buyer's fresh X25519 key and the
//! retry budget. After the accept:
//!
//! ```text
//! worker → webhook_ready    its fresh X25519 key (gossip-signed by the worker)
//! buyer  → webhook_request  URL/headers/body/secret, sealed to that key
//! worker → webhook_result   status/latency/attempts/response, sealed back
//! worker → receipt          output_hash = sha256 of the sealed result
//! ```
//!
//! Keys come from X25519 → HKDF-SHA256 → AES-256-GCM (`temporal::crypto`,
//! the same stack sealed temporal sessions use), with one key per direction
//! bound to the job id. The buyer only seals to the bidder it accepted, and
//! only on a `webhook_ready` whose gossip source is that bidder; the worker
//! only opens a request whose gossip source is the offer's buyer. Relays,
//! the market and every other node see ciphertext and the retry budget.
//!
//! The worker delivers through an SSRF guard: http(s) only, the host is
//! resolved once, every address must be public (no loopback, private,
//! link-local/metadata, CGNAT, multicast, NAT64...), the connection is pinned
//! to those addresses so DNS cannot rebind, and redirects are never followed
//! (a 3xx is returned to the buyer as the answer). Every attempt has a
//! timeout and the response body is read only up to a cap.
//!
//! Delivery is paid per job (one job = one webhook, all its retries) through
//! the normal offer → bid → accept → receipt market. A worker never bids
//! below its price, which cannot be zero.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use c0mpute_net::topics::{JobAccept, JobStatus, job_topic};
use c0mpute_net::{GossipMessage, Libp2pNetwork};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{Semaphore, broadcast, mpsc};
use tracing::{debug, info, warn};
use x25519_dalek::StaticSecret;

use crate::buyer::JobAuction;
use crate::dispatch::OfferFilter;
use crate::pricing::{PricingConfig, PricingMode};
use crate::runner::{AcceptedJob, WorkerLoad, publish_receipt};
use crate::temporal::client::auction_with_mesh_retry;
use crate::temporal::crypto;

pub const WORKLOAD: &str = "webhook.deliver";
/// Capability tag a webhook worker advertises.
pub const TAG: &str = "webhook:deliver";

/// Default price of one delivery (the webhook and all its retries), USD.
/// Operators change it with `c0mpute webhook serve --price`.
pub const DEFAULT_PRICE_USD: f64 = 0.001;
/// Default buyer cap per delivery, USD (`c0mpute webhook send --max-price`).
pub const DEFAULT_MAX_PRICE_USD: f64 = 0.002;
/// No worker bids under this: there is no free delivery.
pub const MIN_PRICE_USD: f64 = 0.0001;

pub const MAX_ATTEMPTS: u32 = 5;
pub const MIN_TIMEOUT_MS: u32 = 1_000;
pub const MAX_TIMEOUT_MS: u32 = 30_000;
pub const MIN_BACKOFF_MS: u32 = 100;
pub const MAX_BACKOFF_MS: u32 = 10_000;
/// Backoff doubles per retry up to this.
const BACKOFF_CEIL_MS: u64 = 30_000;

pub const MAX_URL: usize = 2_048;
pub const MAX_BODY: usize = 32 * 1024;
pub const MAX_HEADERS: usize = 32;
pub const MAX_HEADER_BYTES: usize = 8 * 1024;
pub const MAX_SECRET: usize = 256;
/// Response bytes kept in the result (the rest is not read).
pub const MAX_RESPONSE_BODY: usize = 16 * 1024;
const MAX_RESPONSE_HEADERS: usize = 24;
const MAX_RESPONSE_HEADER_VALUE: usize = 256;
/// A sealed request or result must fit one gossipsub message (64 KiB).
pub const MAX_SEALED: usize = 60_000;

/// How long the worker waits for the buyer's sealed request after the accept.
const REQUEST_WAIT: Duration = Duration::from_secs(60);
/// How long the worker answers repeat requests with its result.
const RESULT_LINGER: Duration = Duration::from_secs(30);
const REPUBLISH: Duration = Duration::from_secs(2);

pub const METHODS: [&str; 5] = ["POST", "PUT", "PATCH", "DELETE", "GET"];
/// Headers the buyer may not set: the HTTP client owns them, or (the
/// `webhook-*` ones) the signature does.
const RESERVED_HEADERS: [&str; 12] = [
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "upgrade",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "webhook-id",
    "webhook-timestamp",
    "webhook-signature",
];

// ── public job spec (rides in the offer, readable by everyone) ──────────

/// `JobOffer.spec_inline` for `webhook.deliver`. Nothing sensitive: the
/// buyer's per-job key and the retry budget a worker commits to.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WebhookSpec {
    pub v: u8,
    /// Buyer's fresh X25519 public key (hex), one per job.
    pub client_pubkey: String,
    pub max_attempts: u32,
    /// Per attempt.
    pub timeout_ms: u32,
    /// First retry waits this long; each later one doubles it.
    pub backoff_ms: u32,
}

impl WebhookSpec {
    pub fn validate(&self) -> Result<()> {
        if self.v != 1 {
            bail!("unsupported webhook spec version {}", self.v);
        }
        if self.client_pubkey.len() != 64 || hex::decode(&self.client_pubkey).is_err() {
            bail!("client_pubkey must be 32 bytes hex");
        }
        if !(1..=MAX_ATTEMPTS).contains(&self.max_attempts) {
            bail!("max_attempts must be 1..={MAX_ATTEMPTS}");
        }
        if !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&self.timeout_ms) {
            bail!("timeout_ms must be {MIN_TIMEOUT_MS}..={MAX_TIMEOUT_MS}");
        }
        if !(MIN_BACKOFF_MS..=MAX_BACKOFF_MS).contains(&self.backoff_ms) {
            bail!("backoff_ms must be {MIN_BACKOFF_MS}..={MAX_BACKOFF_MS}");
        }
        Ok(())
    }

    /// Worst-case seconds the worker spends on this delivery.
    pub fn budget_secs(&self) -> u64 {
        let tries = u64::from(self.max_attempts);
        let waits: u64 = (1..tries).map(|n| backoff(self.backoff_ms, n as u32).as_millis() as u64).sum();
        (tries * u64::from(self.timeout_ms) + waits).div_ceil(1000)
    }
}

/// Wait before retry `n` (1 = the first retry).
fn backoff(base_ms: u32, n: u32) -> Duration {
    let ms = u64::from(base_ms).saturating_mul(1u64 << (n.saturating_sub(1)).min(16));
    Duration::from_millis(ms.min(BACKOFF_CEIL_MS))
}

// ── sealed request and result ───────────────────────────────────────────

/// What the buyer seals to the winning worker.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WebhookCall {
    pub url: String,
    #[serde(default = "default_method")]
    pub method: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub body: String,
    /// Standard Webhooks signing secret (`whsec_<base64>`, or raw bytes).
    /// When set the worker adds `webhook-id`, `webhook-timestamp` and
    /// `webhook-signature`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    /// `webhook-id`; defaults to `msg_<job id>`. Kept across retries so the
    /// receiver can de-duplicate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

fn default_method() -> String {
    "POST".into()
}

impl WebhookCall {
    pub fn validate(&self) -> Result<()> {
        if self.url.len() > MAX_URL {
            bail!("url longer than {MAX_URL} bytes");
        }
        let u = url::Url::parse(&self.url).context("invalid url")?;
        if !matches!(u.scheme(), "http" | "https") {
            bail!("url must be http(s)");
        }
        if !u.username().is_empty() || u.password().is_some() {
            bail!("url must not carry credentials; send them as a header");
        }
        if !METHODS.contains(&self.method.as_str()) {
            bail!("method must be one of {METHODS:?}");
        }
        if self.body.len() > MAX_BODY {
            bail!("body larger than {MAX_BODY} bytes");
        }
        if self.headers.len() > MAX_HEADERS {
            bail!("more than {MAX_HEADERS} headers");
        }
        let mut bytes = 0;
        for (k, v) in &self.headers {
            bytes += k.len() + v.len();
            let name = reqwest::header::HeaderName::from_bytes(k.as_bytes()).with_context(|| format!("bad header name {k:?}"))?;
            reqwest::header::HeaderValue::from_str(v).with_context(|| format!("bad value for header {k}"))?;
            if RESERVED_HEADERS.contains(&name.as_str()) {
                bail!("header {k} is set by the worker");
            }
        }
        if bytes > MAX_HEADER_BYTES {
            bail!("headers larger than {MAX_HEADER_BYTES} bytes");
        }
        if self.secret.as_deref().is_some_and(|s| s.is_empty() || s.len() > MAX_SECRET) {
            bail!("secret must be 1..={MAX_SECRET} bytes");
        }
        if let Some(id) = &self.id {
            if id.is_empty() || id.len() > 128 || !id.bytes().all(|b| b.is_ascii_graphic()) {
                bail!("id must be 1..=128 printable ASCII characters");
            }
        }
        Ok(())
    }
}

/// One delivery attempt.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Attempt {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub latency_ms: u64,
}

/// What the worker seals back to the buyer.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct DeliveryResult {
    /// The target answered 2xx.
    pub delivered: bool,
    /// Status of the last answer, if the target answered at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    pub attempts: u32,
    /// Latency of the last attempt.
    pub latency_ms: u64,
    pub total_ms: u64,
    #[serde(default)]
    pub response_headers: Vec<(String, String)>,
    #[serde(default)]
    pub response_body: String,
    /// The response body was longer than what is kept here.
    #[serde(default)]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_id: Option<String>,
    /// Why nothing was sent (refused URL, bad request) or the last error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub log: Vec<Attempt>,
}

impl DeliveryResult {
    /// The target answered (any status): the worker did the paid work.
    pub fn answered(&self) -> bool {
        self.status.is_some()
    }

    fn refused(e: impl std::fmt::Display) -> Self {
        Self { error: Some(format!("refused: {e}")), ..Default::default() }
    }
}

// ── gossip messages around the auction ─────────────────────────────────

/// Worker → buyer after the accept: the worker's half of the key exchange.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WebhookReady {
    pub t: String, // "webhook_ready"
    pub job_id: String,
    pub worker_peer_id: String,
    /// Echo of the offer's key, so a buyer can match it before it knows the job id.
    pub client_pubkey: String,
    pub worker_pubkey: String,
}

/// Buyer → worker: the sealed [`WebhookCall`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WebhookSealed {
    pub t: String, // "webhook_request"
    pub job_id: String,
    pub worker_peer_id: String,
    pub sealed: String,
}

/// Worker → buyer: the sealed [`DeliveryResult`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WebhookSealedResult {
    pub t: String, // "webhook_result"
    pub job_id: String,
    pub worker_peer_id: String,
    pub sealed: String,
}

/// One key per direction, bound to the job: a sealed result can never be
/// replayed as a request, nor a message moved to another job.
pub fn request_key(secret: &StaticSecret, peer_pub_hex: &str, job_id: &str) -> Result<[u8; 32]> {
    crypto::session_key(secret, peer_pub_hex, &format!("c0mpute.webhook/v1/{job_id}/request"))
}

pub fn result_key(secret: &StaticSecret, peer_pub_hex: &str, job_id: &str) -> Result<[u8; 32]> {
    crypto::session_key(secret, peer_pub_hex, &format!("c0mpute.webhook/v1/{job_id}/result"))
}

pub fn seal_call(key: &[u8; 32], call: &WebhookCall) -> Result<String> {
    let sealed = crypto::seal(key, &serde_json::to_vec(call)?)?;
    if sealed.len() > MAX_SEALED {
        bail!("sealed request is {} bytes; one delivery carries at most {MAX_SEALED} (shrink the body or headers)", sealed.len());
    }
    Ok(sealed)
}

pub fn open_call(key: &[u8; 32], sealed: &str) -> Result<WebhookCall> {
    if sealed.len() > MAX_SEALED {
        bail!("sealed request too large");
    }
    Ok(serde_json::from_slice(&crypto::open(key, sealed)?)?)
}

/// Seal a result, trimming the response body until it fits one message.
pub fn seal_result(key: &[u8; 32], result: &DeliveryResult) -> Result<String> {
    let mut r = result.clone();
    loop {
        let sealed = crypto::seal(key, &serde_json::to_vec(&r)?)?;
        if sealed.len() <= MAX_SEALED {
            return Ok(sealed);
        }
        if r.response_body.is_empty() && r.response_headers.is_empty() {
            bail!("result does not fit one message");
        }
        let keep = floor_char_boundary(&r.response_body, r.response_body.len() / 2);
        if keep == 0 {
            r.response_body.clear();
            r.response_headers.clear();
        } else {
            r.response_body.truncate(keep);
        }
        r.truncated = true;
    }
}

pub fn open_result(key: &[u8; 32], sealed: &str) -> Result<DeliveryResult> {
    Ok(serde_json::from_slice(&crypto::open(key, sealed)?)?)
}

pub fn sha256_hex(b: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(b)))
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn unix_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ── Standard Webhooks signature ─────────────────────────────────────────

/// The signing key: `whsec_<base64>` decodes to bytes (Standard Webhooks);
/// anything else is used as raw bytes.
fn signing_key(secret: &str) -> Vec<u8> {
    secret.strip_prefix("whsec_").and_then(|b| B64.decode(b).ok()).unwrap_or_else(|| secret.as_bytes().to_vec())
}

/// `webhook-signature` value: `v1,<base64 HMAC-SHA256(id.timestamp.body)>`.
pub fn sign(secret: &str, id: &str, timestamp: u64, body: &[u8]) -> String {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&signing_key(secret)).expect("hmac accepts any key length");
    mac.update(id.as_bytes());
    mac.update(b".");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    format!("v1,{}", B64.encode(mac.finalize().into_bytes()))
}

// ── delivery (worker) ───────────────────────────────────────────────────

/// Delivery knobs. `allow_private` exists for this crate's tests (a local
/// target); nothing on the network or the CLI can set it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Guard {
    pub(crate) allow_private: bool,
}

/// Resolve the URL's host once and require every address to be public.
async fn resolve_public(u: &url::Url, guard: Guard) -> Result<(String, Vec<std::net::SocketAddr>)> {
    if !guard.allow_private {
        crate::runner::validate_url(u.as_str())?;
    }
    let host = u.host_str().context("url has no host")?.to_string();
    let port = u.port_or_known_default().context("url has no port")?;
    let lookup = host.trim_start_matches('[').trim_end_matches(']').to_string();
    let addrs: Vec<std::net::SocketAddr> = tokio::time::timeout(Duration::from_secs(10), tokio::net::lookup_host((lookup.as_str(), port)))
        .await
        .map_err(|_| anyhow!("resolving {host} timed out"))?
        .with_context(|| format!("resolve {host}"))?
        .collect();
    if addrs.is_empty() {
        bail!("{host} did not resolve");
    }
    if !guard.allow_private {
        if let Some(bad) = addrs.iter().find(|a| !crate::runner::is_public_ip(a.ip())) {
            bail!("{host} resolves to a non-public address ({})", bad.ip());
        }
    }
    Ok((host, addrs))
}

/// Retry on no answer, 408, 425, 429 and 5xx. Never on 2xx, 3xx or other 4xx.
fn retryable(status: Option<u16>) -> bool {
    match status {
        None => true,
        Some(s) => s == 408 || s == 425 || s == 429 || s >= 500,
    }
}

/// Send `call` with up to `spec.max_attempts` tries. Never fails: a refused
/// or unreachable target is reported in the result.
pub async fn deliver(call: &WebhookCall, spec: &WebhookSpec, default_id: &str, guard: Guard) -> DeliveryResult {
    if let Err(e) = call.validate().and_then(|_| spec.validate()) {
        return DeliveryResult::refused(e);
    }
    let u = match url::Url::parse(&call.url) {
        Ok(u) => u,
        Err(e) => return DeliveryResult::refused(e),
    };
    let id = call.id.clone().unwrap_or_else(|| format!("msg_{default_id}"));
    let started = Instant::now();
    let mut out = DeliveryResult { webhook_id: call.secret.as_ref().map(|_| id.clone()), ..Default::default() };
    for n in 1..=spec.max_attempts {
        if n > 1 {
            tokio::time::sleep(backoff(spec.backoff_ms, n - 1)).await;
        }
        let t0 = Instant::now();
        // Resolve and check on every attempt: a retry must not reach an
        // address the first check never saw.
        let attempt = match resolve_public(&u, guard).await {
            Err(e) => {
                // A refusal is final; retrying cannot make the address public.
                out.attempts = n;
                out.error = Some(format!("refused: {e:#}"));
                out.log.push(Attempt { status: None, error: out.error.clone(), latency_ms: 0 });
                break;
            }
            Ok((host, addrs)) => send_once(call, spec, &u, &host, &addrs, &id).await,
        };
        let latency_ms = t0.elapsed().as_millis() as u64;
        out.attempts = n;
        out.latency_ms = latency_ms;
        match attempt {
            Ok(r) => {
                out.status = Some(r.status);
                out.delivered = (200..300).contains(&r.status);
                out.response_headers = r.headers;
                out.response_body = r.body;
                out.truncated = r.truncated;
                out.error = None;
                out.log.push(Attempt { status: Some(r.status), error: None, latency_ms });
            }
            Err(e) => {
                let msg = format!("{e:#}");
                out.error = Some(msg.clone());
                out.log.push(Attempt { status: None, error: Some(msg), latency_ms });
            }
        }
        if !retryable(out.log.last().and_then(|a| a.status)) {
            break;
        }
    }
    out.total_ms = started.elapsed().as_millis() as u64;
    out
}

struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
    truncated: bool,
}

async fn send_once(call: &WebhookCall, spec: &WebhookSpec, u: &url::Url, host: &str, addrs: &[std::net::SocketAddr], id: &str) -> Result<Answer> {
    let timeout = Duration::from_millis(u64::from(spec.timeout_ms));
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(timeout.min(Duration::from_secs(10)))
        .timeout(timeout)
        .resolve_to_addrs(host, addrs)
        .user_agent(concat!("c0mpute-webhook/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build http client")?;
    let method = reqwest::Method::from_bytes(call.method.as_bytes()).context("method")?;
    let mut req = client.request(method, u.clone());
    let mut has_type = false;
    for (k, v) in &call.headers {
        has_type |= k.eq_ignore_ascii_case("content-type");
        req = req.header(k.as_str(), v.as_str());
    }
    if !call.body.is_empty() && !has_type {
        req = req.header("content-type", "application/json");
    }
    if let Some(secret) = &call.secret {
        let ts = unix_secs();
        req = req
            .header("webhook-id", id)
            .header("webhook-timestamp", ts.to_string())
            .header("webhook-signature", sign(secret, id, ts, call.body.as_bytes()));
    }
    if !call.body.is_empty() {
        req = req.body(call.body.clone());
    }
    let mut resp = tokio::time::timeout(timeout, req.send()).await.map_err(|_| anyhow!("timed out after {}ms", spec.timeout_ms))??;
    let status = resp.status().as_u16();
    let headers = resp
        .headers()
        .iter()
        .take(MAX_RESPONSE_HEADERS)
        .map(|(k, v)| {
            let v = String::from_utf8_lossy(v.as_bytes());
            let keep = floor_char_boundary(&v, MAX_RESPONSE_HEADER_VALUE);
            (k.as_str().to_string(), v[..keep].to_string())
        })
        .collect();
    // Read the body only up to the cap; a huge or endless body is not drained.
    let mut body = Vec::new();
    let mut truncated = false;
    let read = async {
        while let Some(chunk) = resp.chunk().await? {
            let room = MAX_RESPONSE_BODY - body.len();
            if chunk.len() > room {
                body.extend_from_slice(&chunk[..room]);
                truncated = true;
                break;
            }
            body.extend_from_slice(&chunk);
        }
        Ok::<_, reqwest::Error>(())
    };
    // A slow body after the status line still counts against the attempt.
    if tokio::time::timeout(timeout, read).await.is_err() {
        truncated = true;
    }
    Ok(Answer { status, headers, body: String::from_utf8_lossy(&body).into_owned(), truncated })
}

// ── worker ──────────────────────────────────────────────────────────────

/// A worker's price must be a real number of at least [`MIN_PRICE_USD`].
pub fn validate_price(price_usd: f64) -> Result<()> {
    if !price_usd.is_finite() || price_usd < MIN_PRICE_USD {
        bail!("webhook delivery is a paid workload: --price must be at least ${MIN_PRICE_USD}");
    }
    Ok(())
}

/// The listing every webhook bid is priced with: a fixed price per delivery.
pub fn pricing(price_usd: f64) -> PricingConfig {
    PricingConfig { mode: PricingMode::Fixed, min_usd: price_usd, fixed_usd: Some(price_usd) }
}

/// Bid only on valid webhook offers whose cap covers our price.
pub fn offer_filter(price_usd: f64) -> OfferFilter {
    Arc::new(move |offer| {
        offer.max_price_usd.is_finite()
            && offer.max_price_usd >= price_usd
            && offer
                .spec_inline
                .as_ref()
                .and_then(|v| serde_json::from_value::<WebhookSpec>(v.clone()).ok())
                .is_some_and(|s| {
                    let deadline_ok = offer.deadline_unix_ms.saturating_sub(offer.published_at_ms) / 1000 >= s.budget_secs();
                    s.validate().is_ok() && deadline_ok
                })
    })
}

fn from(msg: &GossipMessage, peer: &str) -> bool {
    msg.source.map(|p| p.to_base58()).as_deref() == Some(peer)
}

/// Run one accepted job: key exchange, delivery, sealed result, receipt.
async fn run_job(net: Arc<Libp2pNetwork>, accept: JobAccept, guard: Guard) -> Result<()> {
    let topic = job_topic(WORKLOAD);
    let me = net.peer_id().to_base58();
    let mut rx = net.messages();
    let spec: WebhookSpec = serde_json::from_value(accept.spec_inline.clone().context("accept lacks spec_inline")?)?;
    spec.validate()?;
    let kp = crypto::keypair();
    let req_key = request_key(&kp.secret, &spec.client_pubkey, &accept.job_id)?;
    let res_key = result_key(&kp.secret, &spec.client_pubkey, &accept.job_id)?;
    let ready = serde_json::to_vec(&WebhookReady {
        t: "webhook_ready".into(),
        job_id: accept.job_id.clone(),
        worker_peer_id: me.clone(),
        client_pubkey: spec.client_pubkey.clone(),
        worker_pubkey: kp.public_hex.clone(),
    })?;

    // Announce our key until the buyer's sealed request arrives.
    let deadline = Instant::now() + REQUEST_WAIT;
    let mut next = Instant::now();
    let call = loop {
        if Instant::now() >= deadline {
            bail!("the buyer never sent the sealed request");
        }
        if Instant::now() >= next {
            let _ = net.publish(&topic, ready.clone()).await;
            next = Instant::now() + REPUBLISH;
        }
        let wait = next.min(deadline).saturating_duration_since(Instant::now());
        let msg = match tokio::time::timeout(wait, rx.recv()).await {
            Ok(Ok(m)) => m,
            Ok(Err(broadcast::error::RecvError::Closed)) => bail!("network closed"),
            _ => continue,
        };
        if msg.topic != topic {
            continue;
        }
        let Ok(r) = serde_json::from_slice::<WebhookSealed>(&msg.data) else { continue };
        // Only the buyer that won us may hand us the request.
        if r.t != "webhook_request" || r.job_id != accept.job_id || r.worker_peer_id != me || !from(&msg, &accept.buyer_peer_id) {
            continue;
        }
        match open_call(&req_key, &r.sealed) {
            Ok(c) => break c,
            Err(e) => debug!(job_id = %accept.job_id, err = %e, "webhook: unreadable request ignored"),
        }
    };

    let result = deliver(&call, &spec, &accept.job_id, guard).await;
    drop(call);
    let sealed = seal_result(&res_key, &result)?;
    let hash = sha256_hex(sealed.as_bytes());
    let msg = serde_json::to_vec(&WebhookSealedResult { t: "webhook_result".into(), job_id: accept.job_id.clone(), worker_peer_id: me.clone(), sealed })?;
    let _ = net.publish(&topic, msg.clone()).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let status = if result.answered() { JobStatus::Completed } else { JobStatus::Failed };
    info!(job_id = %accept.job_id, status = ?result.status, attempts = result.attempts, delivered = result.delivered, "webhook: delivery done");
    publish_receipt(&net, &accept, Some(hash), status).await;

    // The buyer republishes its request until it has the result; answer
    // repeats for a while in case the first copy was lost. Off the delivery
    // slot: it is only a cached reply.
    tokio::spawn(async move {
        let linger = Instant::now() + RESULT_LINGER;
        loop {
            let m = match tokio::time::timeout(linger.saturating_duration_since(Instant::now()), rx.recv()).await {
                Ok(Ok(m)) => m,
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                _ => return,
            };
            if m.topic != topic || !from(&m, &accept.buyer_peer_id) {
                continue;
            }
            if let Ok(r) = serde_json::from_slice::<WebhookSealed>(&m.data) {
                if r.t == "webhook_request" && r.job_id == accept.job_id && r.worker_peer_id == me {
                    let _ = net.publish(&topic, msg.clone()).await;
                }
            }
        }
    });
    Ok(())
}

/// Runner for `webhook.deliver`: each accepted job runs in its own task on
/// one of the node's delivery slots.
pub fn spawn_runner(net: Arc<Libp2pNetwork>, load: WorkerLoad, guard: Guard) -> mpsc::Sender<AcceptedJob> {
    let (tx, mut rx) = mpsc::channel::<AcceptedJob>(64);
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let Ok(slot) = load.compute_slots().acquire_owned().await else { return };
            let net = net.clone();
            let load = load.clone();
            tokio::spawn(async move {
                let t0 = Instant::now();
                let accept = job.accept;
                if let Err(e) = run_job(net.clone(), accept.clone(), guard).await {
                    warn!(job_id = %accept.job_id, err = %format!("{e:#}"), "webhook: job failed");
                    publish_receipt(&net, &accept, None, JobStatus::Failed).await;
                }
                load.record_job(t0.elapsed());
                drop(slot);
            });
        }
    });
    tx
}

pub struct Serving {
    pub net: Arc<Libp2pNetwork>,
    pub tags: Vec<String>,
    pub price_usd: f64,
}

/// `c0mpute webhook serve`: advertise `webhook:deliver` and deliver paid,
/// sealed webhooks, `concurrency` at a time.
pub async fn serve(net_opts: &crate::temporal::node::NetOptions, price_usd: f64, concurrency: usize) -> Result<Serving> {
    validate_price(price_usd)?;
    let concurrency = concurrency.clamp(1, 256);
    let net = crate::temporal::node::spawn_node(net_opts).await?;
    let tags = vec![TAG.to_string()];
    let listing = pricing(price_usd);
    let hardware = serde_json::json!({"version": env!("CARGO_PKG_VERSION"), "webhook": {"concurrency": concurrency}, "pricing": listing.listing()});
    tokio::spawn(crate::capabilities::advertise_loop(net.clone(), tags.clone(), hardware, crate::capabilities::local_bench_score(), crate::capabilities::DEFAULT_ADVERTISE_INTERVAL));
    let load = WorkerLoad::new(Arc::new(Semaphore::new(concurrency)), concurrency);
    let tx = spawn_runner(net.clone(), load.clone(), Guard::default());
    crate::dispatch::run_worker_subscriber_with_load(net.clone(), WORKLOAD.into(), tags.clone(), Some(tx), Some(offer_filter(price_usd)), Some(listing), Some(load));
    info!(peer = %net.peer_id().to_base58(), price_usd, concurrency, "webhook: serving");
    tokio::time::sleep(Duration::from_millis(300)).await;
    Ok(Serving { net, tags, price_usd })
}

// ── buyer ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct SendRequest {
    pub call: WebhookCall,
    pub max_attempts: u32,
    pub timeout_ms: u32,
    pub backoff_ms: u32,
    pub workers: Vec<String>,
    pub max_price_usd: f64,
    pub bid_window: Duration,
    /// Billing id (e.g. a CoinPay invoice) carried on offer, accept, receipt.
    pub external_ref: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SendOutcome {
    pub result: DeliveryResult,
    pub job_id: String,
    pub worker_peer_id: String,
    pub price_usd: f64,
    pub output_hash: String,
    pub receipt_status: JobStatus,
}

/// Auction one delivery, seal the request to the winner, and return the
/// opened result once it matches the receipt's `output_hash`.
pub async fn send(net: Arc<Libp2pNetwork>, req: &SendRequest) -> Result<SendOutcome> {
    req.call.validate()?;
    if !req.max_price_usd.is_finite() || req.max_price_usd < MIN_PRICE_USD {
        bail!("--max-price must be at least ${MIN_PRICE_USD}: webhook delivery is paid");
    }
    let kp = crypto::keypair();
    let spec = WebhookSpec { v: 1, client_pubkey: kp.public_hex.clone(), max_attempts: req.max_attempts, timeout_ms: req.timeout_ms, backoff_ms: req.backoff_ms };
    spec.validate()?;
    // Fail before the auction if the request cannot fit one message.
    seal_call(&[0u8; 32], &req.call)?;

    let topic = job_topic(WORKLOAD);
    net.subscribe(&topic).await?;
    let mut rx = net.messages();
    let (acc_tx, mut acc_rx) = mpsc::unbounded_channel();
    let budget = spec.budget_secs();
    let spec_v = serde_json::to_value(&spec)?;
    let make = || {
        JobAuction::new(WORKLOAD, spec_v.clone())
            .with_required_capabilities(vec![TAG.into()])
            .with_allowed_bidders(req.workers.clone())
            .with_max_price_usd(req.max_price_usd)
            .with_bid_window(req.bid_window)
            .with_external_ref(req.external_ref.clone())
            .with_deadline(Duration::from_secs(budget + 180))
            .with_receipt_timeout(Duration::from_secs(budget + REQUEST_WAIT.as_secs() + 30))
            .with_accept_notify(acc_tx.clone())
    };
    let auction = auction_with_mesh_retry(net.clone(), make);
    tokio::pin!(auction);

    // job_id → winner, ready-but-unmatched keys, sealed results.
    let mut winners: HashMap<String, String> = HashMap::new();
    let mut readies: HashMap<String, WebhookReady> = HashMap::new();
    let mut keys: HashMap<String, Exchange> = HashMap::new();
    let mut results: HashMap<String, String> = HashMap::new();
    let mut outcome = None;
    let mut grace: Option<Instant> = None;
    let mut tick = tokio::time::interval(REPUBLISH);

    loop {
        if let Some(o) = &outcome {
            let o: &crate::buyer::AuctionOutcome = o;
            if results.contains_key(&o.job_id) || grace.is_some_and(|g| Instant::now() >= g) {
                break;
            }
        }
        tokio::select! {
            r = &mut auction, if outcome.is_none() => {
                outcome = Some(r?);
                grace = Some(Instant::now() + Duration::from_secs(10));
            }
            Some((job_id, bid)) = acc_rx.recv() => {
                winners.insert(job_id.clone(), bid.bidder_peer_id.clone());
                if let Some(ready) = readies.remove(&job_id) {
                    if ready.worker_peer_id == bid.bidder_peer_id {
                        key_up(&net, &topic, &kp.secret, &req.call, ready, &mut keys).await?;
                    }
                }
            }
            m = rx.recv() => {
                let msg = match m {
                    Ok(m) => m,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => bail!("network closed"),
                };
                if msg.topic != topic {
                    continue;
                }
                if let Ok(r) = serde_json::from_slice::<WebhookReady>(&msg.data) {
                    // Seal only to the bidder we accepted, on a key it signed.
                    if r.t != "webhook_ready" || r.client_pubkey != kp.public_hex || !from(&msg, &r.worker_peer_id) || keys.contains_key(&r.job_id) {
                        continue;
                    }
                    match winners.get(&r.job_id) {
                        Some(w) if *w == r.worker_peer_id => key_up(&net, &topic, &kp.secret, &req.call, r, &mut keys).await?,
                        Some(_) => warn!(job_id = %r.job_id, "webhook: key from a worker we did not accept ignored"),
                        None if readies.len() < 64 => { readies.insert(r.job_id.clone(), r); }
                        None => {}
                    }
                } else if let Ok(r) = serde_json::from_slice::<WebhookSealedResult>(&msg.data) {
                    if r.t == "webhook_result" && winners.get(&r.job_id) == Some(&r.worker_peer_id) && from(&msg, &r.worker_peer_id) && r.sealed.len() <= MAX_SEALED {
                        results.entry(r.job_id).or_insert(r.sealed);
                    }
                }
            }
            _ = tick.tick() => {
                // Republish requests still waiting on a result (a fresh mesh drops messages).
                for (job_id, x) in &keys {
                    if !results.contains_key(job_id) {
                        let _ = net.publish(&topic, x.payload.clone()).await;
                    }
                }
            }
        }
    }

    let o = outcome.expect("loop exits with an outcome");
    let worker = o.accepted_bid.bidder_peer_id.clone();
    let sealed = results.remove(&o.job_id).with_context(|| format!("worker {worker} sent a receipt ({:?}) but no result", o.receipt.status))?;
    let got = sha256_hex(sealed.as_bytes());
    let claimed = o.receipt.output_hash.clone().unwrap_or_default();
    if got != claimed {
        bail!("result {got} does not match the receipt's {claimed}");
    }
    let x = keys.get(&o.job_id).context("no key exchange for the accepted job")?;
    let result = open_result(&result_key(&kp.secret, &x.worker_pubkey, &o.job_id)?, &sealed)?;
    Ok(SendOutcome { result, job_id: o.job_id, worker_peer_id: worker, price_usd: o.accepted_bid.price_usd, output_hash: got, receipt_status: o.receipt.status })
}

/// The key exchange with an accepted worker: its key, and our sealed request
/// (republished until its result arrives).
struct Exchange {
    worker_pubkey: String,
    payload: Vec<u8>,
}

/// Seal the call to an accepted worker's key, publish it, and remember it.
async fn key_up(net: &Libp2pNetwork, topic: &str, secret: &StaticSecret, call: &WebhookCall, ready: WebhookReady, keys: &mut HashMap<String, Exchange>) -> Result<()> {
    let key = request_key(secret, &ready.worker_pubkey, &ready.job_id)?;
    let sealed = seal_call(&key, call)?;
    let payload = serde_json::to_vec(&WebhookSealed { t: "webhook_request".into(), job_id: ready.job_id.clone(), worker_peer_id: ready.worker_peer_id.clone(), sealed })?;
    let _ = net.publish(topic, payload.clone()).await;
    debug!(job_id = %ready.job_id, worker = %ready.worker_peer_id, "webhook: sealed request sent");
    keys.insert(ready.job_id, Exchange { worker_pubkey: ready.worker_pubkey, payload });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::any;

    const LOCAL: Guard = Guard { allow_private: true };

    fn spec(attempts: u32) -> WebhookSpec {
        WebhookSpec { v: 1, client_pubkey: "11".repeat(32), max_attempts: attempts, timeout_ms: 5_000, backoff_ms: 100 }
    }

    fn call(url: &str) -> WebhookCall {
        WebhookCall { url: url.into(), method: "POST".into(), headers: vec![("x-tenant".into(), "acme".into())], body: r#"{"event":"paid"}"#.into(), secret: None, id: None }
    }

    #[derive(Clone, Default)]
    struct Seen {
        hits: Arc<AtomicUsize>,
        other_hits: Arc<AtomicUsize>,
        last: Arc<std::sync::Mutex<Option<(HeaderMap, Vec<u8>)>>>,
    }

    /// A local target: `/ok` 200, `/fail` 503, `/bad` 400, `/redirect` 302
    /// to `/other`, `/big` 100 KiB.
    async fn target() -> (String, Seen) {
        let seen = Seen::default();
        async fn handle(State(s): State<Seen>, uri: axum::http::Uri, headers: HeaderMap, body: axum::body::Bytes) -> (StatusCode, HeaderMap, Vec<u8>) {
            let mut h = HeaderMap::new();
            match uri.path() {
                "/other" => {
                    s.other_hits.fetch_add(1, Ordering::SeqCst);
                    return (StatusCode::OK, h, b"other".to_vec());
                }
                _ => {
                    s.hits.fetch_add(1, Ordering::SeqCst);
                    *s.last.lock().unwrap() = Some((headers, body.to_vec()));
                }
            }
            match uri.path() {
                "/ok" => (StatusCode::OK, h, b"thanks".to_vec()),
                "/fail" => (StatusCode::SERVICE_UNAVAILABLE, h, b"down".to_vec()),
                "/bad" => (StatusCode::BAD_REQUEST, h, b"no".to_vec()),
                "/redirect" => {
                    h.insert("location", "/other".parse().unwrap());
                    (StatusCode::FOUND, h, vec![])
                }
                "/big" => (StatusCode::OK, h, vec![b'a'; 100 * 1024]),
                _ => (StatusCode::NOT_FOUND, h, vec![]),
            }
        }
        let app = Router::new().fallback(any(handle)).with_state(seen.clone());
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (format!("http://{addr}"), seen)
    }

    #[test]
    fn sealed_request_and_result_round_trip() {
        let buyer = crypto::keypair();
        let worker = crypto::keypair();
        let mut c = call("https://example.com/hook");
        c.secret = Some("whsec_c2VjcmV0".into());
        let bk = request_key(&buyer.secret, &worker.public_hex, "job-1").unwrap();
        let wk = request_key(&worker.secret, &buyer.public_hex, "job-1").unwrap();
        let sealed = seal_call(&bk, &c).unwrap();
        assert!(!sealed.contains("example.com") && !sealed.contains("whsec_"), "ciphertext must not leak the call");
        assert_eq!(open_call(&wk, &sealed).unwrap(), c);
        // Another job, another direction, another worker: none can open it.
        assert!(open_call(&request_key(&worker.secret, &buyer.public_hex, "job-2").unwrap(), &sealed).is_err());
        assert!(open_call(&result_key(&worker.secret, &buyer.public_hex, "job-1").unwrap(), &sealed).is_err());
        let eve = crypto::keypair();
        assert!(open_call(&request_key(&eve.secret, &buyer.public_hex, "job-1").unwrap(), &sealed).is_err());

        let r = DeliveryResult { delivered: true, status: Some(200), attempts: 1, response_body: "ok".into(), ..Default::default() };
        let rs = seal_result(&result_key(&worker.secret, &buyer.public_hex, "job-1").unwrap(), &r).unwrap();
        assert_eq!(open_result(&result_key(&buyer.secret, &worker.public_hex, "job-1").unwrap(), &rs).unwrap(), r);
        assert!(open_result(&bk, &rs).is_err(), "a result cannot be opened as a request");
    }

    #[test]
    fn oversized_requests_are_refused_and_results_trimmed_to_fit() {
        let key = [7u8; 32];
        let mut c = call("https://example.com/hook");
        c.body = "x".repeat(MAX_BODY + 1);
        assert!(c.validate().is_err());
        // JSON escaping can blow a body under the cap past one message.
        c.body = "\u{1}".repeat(MAX_BODY / 2);
        assert!(seal_call(&key, &c).is_err());
        let r = DeliveryResult { status: Some(200), response_body: "\u{1}".repeat(MAX_RESPONSE_BODY), ..Default::default() };
        let s = seal_result(&key, &r).unwrap();
        assert!(s.len() <= MAX_SEALED);
        assert!(open_result(&key, &s).unwrap().truncated);
    }

    #[test]
    fn call_validation() {
        assert!(call("https://example.com/h").validate().is_ok());
        for bad in ["ftp://example.com/", "file:///etc/passwd", "https://user:pw@example.com/", "not a url"] {
            assert!(call(bad).validate().is_err(), "{bad}");
        }
        let mut c = call("https://example.com/h");
        c.method = "CONNECT".into();
        assert!(c.validate().is_err());
        for h in ["Host", "content-length", "webhook-signature", "Transfer-Encoding"] {
            let mut c = call("https://example.com/h");
            c.headers = vec![(h.into(), "x".into())];
            assert!(c.validate().is_err(), "{h}");
        }
        let mut c = call("https://example.com/h");
        c.headers = vec![("x-a".into(), "line\r\nInjected: 1".into())];
        assert!(c.validate().is_err(), "CRLF in a header value");
    }

    #[tokio::test]
    async fn private_loopback_and_metadata_targets_are_refused() {
        let (base, seen) = target().await;
        let port = base.rsplit(':').next().unwrap();
        for url in [
            format!("{base}/ok"),
            format!("http://localhost:{port}/ok"),
            format!("http://[::1]:{port}/ok"),
            "http://169.254.169.254/latest/meta-data/".to_string(),
            "http://10.0.0.1/".to_string(),
            "http://100.64.0.1/".to_string(),
            "http://[fd00::1]/".to_string(),
            "http://[::ffff:127.0.0.1]/".to_string(),
            "http://[64:ff9b::a00:1]/".to_string(),
        ] {
            let r = deliver(&call(&url), &spec(3), "j", Guard::default()).await;
            assert!(!r.answered(), "{url} must not be reached");
            assert!(r.error.as_deref().unwrap_or("").starts_with("refused"), "{url}: {:?}", r.error);
            assert_eq!(r.attempts, 1, "{url}: a refusal is not retried");
        }
        assert_eq!(seen.hits.load(Ordering::SeqCst), 0, "nothing reached the local target");
    }

    #[tokio::test]
    async fn redirects_are_returned_not_followed() {
        let (base, seen) = target().await;
        let r = deliver(&call(&format!("{base}/redirect")), &spec(3), "j", LOCAL).await;
        assert_eq!(r.status, Some(302));
        assert!(!r.delivered);
        assert_eq!(r.attempts, 1, "a 3xx is final");
        assert_eq!(seen.other_hits.load(Ordering::SeqCst), 0, "the redirect target was never fetched");
        assert!(r.response_headers.iter().any(|(k, v)| k == "location" && v == "/other"));
    }

    #[tokio::test]
    async fn delivers_with_standard_webhooks_signature() {
        let (base, seen) = target().await;
        let mut c = call(&format!("{base}/ok"));
        c.secret = Some("whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw".into());
        c.id = Some("msg_1".into());
        let r = deliver(&c, &spec(3), "j", LOCAL).await;
        assert!(r.delivered && r.status == Some(200) && r.attempts == 1, "{r:?}");
        assert_eq!(r.response_body, "thanks");
        assert_eq!(r.webhook_id.as_deref(), Some("msg_1"));
        let (h, body) = seen.last.lock().unwrap().clone().unwrap();
        assert_eq!(body, c.body.as_bytes());
        assert_eq!(h["x-tenant"], "acme");
        assert_eq!(h["content-type"], "application/json");
        assert_eq!(h["webhook-id"], "msg_1");
        let ts: u64 = h["webhook-timestamp"].to_str().unwrap().parse().unwrap();
        // Verify the way a receiver does, with an independent HMAC.
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&B64.decode("MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw").unwrap()).unwrap();
        mac.update(format!("msg_1.{ts}.{}", c.body).as_bytes());
        assert_eq!(h["webhook-signature"].to_str().unwrap(), format!("v1,{}", B64.encode(mac.finalize().into_bytes())));
    }

    #[test]
    fn signature_matches_the_standard_webhooks_reference_vector() {
        // From the Standard Webhooks spec / svix reference libraries.
        let sig = sign("whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw", "msg_p5jXN8AQM9LWM0D4loKWxJek", 1614265330, br#"{"test": 2432232314}"#);
        assert_eq!(sig, "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE=");
    }

    #[tokio::test]
    async fn retries_stop_at_the_cap_and_only_on_retryable_answers() {
        let (base, seen) = target().await;
        let r = deliver(&call(&format!("{base}/fail")), &spec(3), "j", LOCAL).await;
        assert_eq!((r.attempts, r.status, r.delivered), (3, Some(503), false));
        assert_eq!(r.log.len(), 3);
        assert_eq!(seen.hits.load(Ordering::SeqCst), 3);

        let r = deliver(&call(&format!("{base}/bad")), &spec(3), "j", LOCAL).await;
        assert_eq!((r.attempts, r.status), (1, Some(400)), "a 4xx is not retried");
        assert_eq!(seen.hits.load(Ordering::SeqCst), 4);

        let r = deliver(&call(&format!("{base}/fail")), &spec(MAX_ATTEMPTS + 1), "j", LOCAL).await;
        assert!(r.error.unwrap().contains("max_attempts"), "over the cap is refused outright");
        assert_eq!(seen.hits.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn response_body_is_capped() {
        let (base, _) = target().await;
        let r = deliver(&call(&format!("{base}/big")), &spec(1), "j", LOCAL).await;
        assert_eq!(r.status, Some(200));
        assert_eq!(r.response_body.len(), MAX_RESPONSE_BODY);
        assert!(r.truncated);
    }

    #[test]
    fn spec_bounds_and_budget() {
        assert!(spec(3).validate().is_ok());
        assert!(spec(0).validate().is_err());
        assert!(spec(MAX_ATTEMPTS + 1).validate().is_err());
        let mut s = spec(1);
        s.timeout_ms = MAX_TIMEOUT_MS + 1;
        assert!(s.validate().is_err());
        let mut s = spec(1);
        s.client_pubkey = "zz".into();
        assert!(s.validate().is_err());
        let s = WebhookSpec { v: 1, client_pubkey: "11".repeat(32), max_attempts: 3, timeout_ms: 10_000, backoff_ms: 1_000 };
        assert_eq!(s.budget_secs(), 30 + 1 + 2);
        assert_eq!(backoff(10_000, 5), Duration::from_millis(BACKOFF_CEIL_MS));
    }

    async fn node(dir: &std::path::Path, boot: Vec<libp2p::Multiaddr>) -> Arc<Libp2pNetwork> {
        let cfg = c0mpute_net::NetworkConfig::for_dir(dir.to_path_buf()).with_listen(vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()]).with_bootstrap(boot);
        Arc::new(Libp2pNetwork::spawn(cfg).await.unwrap())
    }

    async fn dial_addr(net: &Libp2pNetwork) -> libp2p::Multiaddr {
        for _ in 0..50 {
            if let Some(a) = net.listen_addrs().await.ok().and_then(|v| v.into_iter().next()) {
                return a.with(libp2p::multiaddr::Protocol::P2p(net.peer_id()));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("no listen addr");
    }

    /// Buyer, worker and a bystander on loopback: the auction runs, the call
    /// is sealed to the winner, delivered and signed, the result comes back
    /// matching the receipt, and the bystander never sees the URL, headers,
    /// body or secret.
    #[tokio::test]
    async fn end_to_end_sealed_paid_delivery() {
        let (base, seen) = target().await;
        let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
        let worker = node(dirs[0].path(), vec![]).await;
        let w_addr = dial_addr(&worker).await;
        let buyer = node(dirs[1].path(), vec![w_addr.clone()]).await;
        let bystander = node(dirs[2].path(), vec![w_addr]).await;

        let load = WorkerLoad::new(Arc::new(Semaphore::new(4)), 4);
        let tx = spawn_runner(worker.clone(), load.clone(), LOCAL);
        crate::dispatch::run_worker_subscriber_with_load(worker.clone(), WORKLOAD.into(), vec![TAG.into()], Some(tx), Some(offer_filter(DEFAULT_PRICE_USD)), Some(pricing(DEFAULT_PRICE_USD)), Some(load));
        let topic = job_topic(WORKLOAD);
        bystander.subscribe(&topic).await.unwrap();
        let mut spy = bystander.messages();
        let sniffed = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let s2 = sniffed.clone();
        tokio::spawn(async move {
            while let Ok(m) = spy.recv().await {
                s2.lock().unwrap().extend_from_slice(&m.data);
            }
        });
        tokio::time::sleep(Duration::from_secs(2)).await;

        let secret = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";
        let mut c = call(&format!("{base}/ok"));
        c.secret = Some(secret.into());
        c.headers.push(("authorization".into(), "Bearer sk_live_topsecret".into()));
        let req = SendRequest { call: c.clone(), max_attempts: 2, timeout_ms: 5_000, backoff_ms: 100, workers: vec![], max_price_usd: DEFAULT_MAX_PRICE_USD, bid_window: Duration::from_secs(3), external_ref: Some("coinpay:inv_e2e".into()) };
        let out = tokio::time::timeout(Duration::from_secs(90), send(buyer.clone(), &req)).await.expect("send finishes").expect("send succeeds");
        assert_eq!(out.worker_peer_id, worker.peer_id().to_base58());
        assert_eq!(out.receipt_status, JobStatus::Completed);
        assert!(out.result.delivered && out.result.status == Some(200), "{:?}", out.result);
        assert_eq!(out.result.response_body, "thanks");
        assert_eq!(out.price_usd, DEFAULT_PRICE_USD, "paid the worker's price, not zero and not the cap");
        assert_eq!(seen.hits.load(Ordering::SeqCst), 1);
        let (h, _) = seen.last.lock().unwrap().clone().unwrap();
        assert_eq!(h["authorization"], "Bearer sk_live_topsecret");
        assert!(h["webhook-signature"].to_str().unwrap().starts_with("v1,"));

        tokio::time::sleep(Duration::from_millis(500)).await;
        let wire = String::from_utf8_lossy(&sniffed.lock().unwrap()).into_owned();
        assert!(wire.contains("webhook_request") && wire.contains("webhook_result"), "the bystander saw the sealed exchange");
        for leak in [base.as_str(), "sk_live_topsecret", secret, "x-tenant", "\"event\"", "thanks"] {
            assert!(!wire.contains(leak), "plaintext {leak:?} leaked onto gossip");
        }
    }

    fn offer(cap: f64, spec_v: serde_json::Value) -> c0mpute_net::topics::JobOffer {
        c0mpute_net::topics::JobOffer {
            job_id: "j".into(),
            workload_type: WORKLOAD.into(),
            buyer_peer_id: "buyer".into(),
            buyer_did: None,
            spec_hash: String::new(),
            spec_inline: Some(spec_v),
            required_capabilities: vec![TAG.into()],
            max_price_usd: cap,
            deadline_unix_ms: 1_000_000 + 600_000,
            published_at_ms: 1_000_000,
            external_ref: None,
        }
    }

    #[test]
    fn paid_only_no_free_bids_or_under_cap_offers() {
        assert!(validate_price(0.0).is_err());
        assert!(validate_price(f64::NAN).is_err());
        assert!(validate_price(-1.0).is_err());
        assert!(validate_price(DEFAULT_PRICE_USD).is_ok());
        // Our bid is always our price, never the buyer's cap, never zero.
        let p = pricing(DEFAULT_PRICE_USD);
        assert_eq!(p.quote(0.01, 0.0), Some(DEFAULT_PRICE_USD));
        assert_eq!(p.quote(DEFAULT_PRICE_USD / 2.0, 0.0), None, "a cap under our price gets no bid");
        let f = offer_filter(DEFAULT_PRICE_USD);
        let good = serde_json::to_value(spec(3)).unwrap();
        assert!(f(&offer(DEFAULT_MAX_PRICE_USD, good.clone())));
        assert!(!f(&offer(0.0, good.clone())), "a free offer gets no bid");
        assert!(!f(&offer(f64::NAN, good.clone())));
        assert!(!f(&offer(1.0, serde_json::to_value(spec(9)).unwrap())), "an invalid spec gets no bid");
        assert!(!f(&offer(1.0, serde_json::json!({"url": "http://169.254.169.254/"}))), "plaintext calls are not a webhook spec");
        let mut short = offer(1.0, good);
        short.deadline_unix_ms = short.published_at_ms + 1_000;
        assert!(!f(&short), "a deadline shorter than the retry budget gets no bid");
    }

    #[test]
    fn accepts_must_match_the_webhook_offer_we_bid_on() {
        let s = serde_json::to_value(spec(3)).unwrap();
        let bid = crate::dispatch::OpenBid { cap: 0.002, price: 0.001, buyer_peer_id: "buyer".into(), spec_inline: Some(s.clone()), external_ref: Some("coinpay:inv_1".into()), expires_at_ms: u64::MAX / 2 };
        let accept = JobAccept { job_id: "j".into(), buyer_peer_id: "buyer".into(), winning_bidder_peer_id: "us".into(), agreed_price_usd: 0.001, workload_type: WORKLOAD.into(), spec_inline: Some(s), published_at_ms: 0, external_ref: Some("coinpay:inv_1".into()) };
        assert!(bid.matches(&accept, 0).is_ok());
        // A forged accept swapping in another buyer key (to read the call) is refused.
        let mut a = accept.clone();
        let mut other = spec(3);
        other.client_pubkey = "22".repeat(32);
        a.spec_inline = Some(serde_json::to_value(other).unwrap());
        assert!(bid.matches(&a, 0).is_err());
        let mut a = accept.clone();
        a.agreed_price_usd = 0.01;
        assert!(bid.matches(&a, 0).is_err(), "over the cap");
        let mut a = accept;
        a.buyer_peer_id = "mallory".into();
        assert!(bid.matches(&a, 0).is_err(), "only the offer's buyer accepts");
    }
}
