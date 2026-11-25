//! DIP-0036 operator policy at the gateway: refuse hosted files that a
//! labeler this operator subscribes to has labelled.
//!
//! ```toml
//! [moderation]
//! labelers = [
//!   { url = "https://threatcrush.com", did = "did:web:threatcrush.com",
//!     public_key = "IbytqclmwUoFutZJHl526ZFWmg_TAVf7K9LNYuYGr4M",
//!     token_env = "C0MPUTE_LABELER_TOKEN", appeal = "support@threatcrush.com" },
//! ]
//! [moderation.actions]
//! malware = "refuse"
//! phishing = "refuse"
//! cryptominer = "refuse"
//! ```
//!
//! Off by default: with no labelers the layer is never installed. One
//! operator's choice binds only that operator's node (DIP-0036 L1).
//!
//! The same table is a hosting worker's policy (DIP-0036 step 4): a node with
//! the hosting role does not hold a copy of a site that has a file with a
//! `refuse` label, and lets go of one it holds (`hosting::policy`). `warn`
//! is a gateway action; a worker acts on `refuse` only. `refuse_replicas =
//! false` under `[moderation]` keeps the policy to what the gateway serves.
//!
//! How a response is judged:
//! - only successful GET/HEAD responses of web content (HTML, JS, SVG) up to
//!   `max_scan_bytes` with a known length are looked at; everything else
//!   streams through untouched
//! - the body's SHA-256 is the subject. A cached decision answers at once; a
//!   stale one answers at once and refreshes in the background; a miss asks
//!   the labelers, waiting at most `timeout_ms`, then fails open
//! - concurrent misses for one file share one labeler call
//! - a label counts only if it verifies against the pinned key, comes from the
//!   pinned DID, is positive and unexpired, and names *this* file's SHA-256
//!
//! With a scan token (`token_env`) the gateway sends the file to the labeler's
//! `/scan` the first time it sees it; afterwards, and without a token, it asks
//! the cheap public `/query`. The gateway judges the bytes it actually serves:
//! hosting rewrites meta tags in HTML per host, so a rewritten page has its own
//! SHA-256 and only a gateway with a scan token sees labels for it; `/query`
//! alone covers files served unchanged (JS, SVG, HTML without meta URLs).

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use futures::future::{BoxFuture, FutureExt, Shared};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Refuse,
    Warn,
    Ignore,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LabelerSource {
    /// Base URL, e.g. `https://threatcrush.com`.
    pub url: String,
    /// DID every label must carry in `src`.
    pub did: String,
    /// Raw ed25519 public key, base64url. Pinned: never fetched.
    pub public_key: String,
    /// Environment variable holding a `/scan` token. Empty: query only.
    #[serde(default)]
    pub token_env: String,
    /// Shown on the refusal page.
    #[serde(default)]
    pub appeal: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ModerationConfig {
    #[serde(default)]
    pub labelers: Vec<LabelerSource>,
    /// Label value -> action. Values not listed are ignored.
    #[serde(default)]
    pub actions: BTreeMap<String, Action>,
    #[serde(default = "default_max_scan_bytes")]
    pub max_scan_bytes: usize,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// How long a decision is trusted before it is refreshed in the background.
    #[serde(default = "default_recheck_secs")]
    pub recheck_secs: u64,
    /// On a node with the hosting role: also refuse to hold copies of sites
    /// that have a file with a `refuse` label (DIP-0036 step 4). On by
    /// default; `false` keeps the policy to what the gateway serves.
    #[serde(default = "yes")]
    pub refuse_replicas: bool,
}

fn yes() -> bool {
    true
}

fn default_max_scan_bytes() -> usize {
    2 * 1024 * 1024
}
fn default_timeout_ms() -> u64 {
    2500
}
fn default_recheck_secs() -> u64 {
    3600
}

impl Default for ModerationConfig {
    fn default() -> Self {
        Self {
            labelers: Vec::new(),
            actions: BTreeMap::new(),
            max_scan_bytes: default_max_scan_bytes(),
            timeout_ms: default_timeout_ms(),
            recheck_secs: default_recheck_secs(),
            refuse_replicas: true,
        }
    }
}

// ── labels ──────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Deserialize)]
pub struct Label {
    pub src: String,
    pub uri: String,
    pub val: String,
    pub neg: bool,
    pub cts: String,
    pub exp: String,
    #[serde(default)]
    pub evidence: Option<String>,
    pub sig: String,
}

fn printable(s: &str) -> bool {
    !s.is_empty() && s.len() <= 600 && s.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

/// `YYYY-MM-DDTHH:MM:SSZ` to unix seconds. Anything else is None.
pub fn parse_rfc3339_utc(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
        || b[19] != b'Z'
    {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let t = &s[r];
        if !t.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        t.parse().ok()
    };
    let (y, mo, d, h, mi, se) = (
        num(0..4)?,
        num(5..7)?,
        num(8..10)?,
        num(11..13)?,
        num(14..16)?,
        num(17..19)?,
    );
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    // days from civil (Howard Hinnant)
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + se)
}

/// The exact bytes a DIP-0036 v1 label signature covers. None when a field is unsafe.
pub fn canonical(l: &Label) -> Option<String> {
    if !printable(&l.src) || !printable(&l.uri) || !printable(&l.val) {
        return None;
    }
    parse_rfc3339_utc(&l.cts)?;
    parse_rfc3339_utc(&l.exp)?;
    Some(format!(
        "c0mpute-label-v1\n{}\n{}\n{}\n{}\n{}\n{}",
        l.src,
        l.uri,
        l.val,
        if l.neg { "1" } else { "0" },
        l.cts,
        l.exp
    ))
}

pub fn verify_label(l: &Label, public_key_b64url: &str) -> bool {
    let Some(msg) = canonical(l) else {
        return false;
    };
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let (Ok(key), Ok(sig)) = (
        b64.decode(public_key_b64url.trim_end_matches('=')),
        b64.decode(l.sig.trim_end_matches('=')),
    ) else {
        return false;
    };
    if key.len() != 32 || sig.len() != 64 {
        return false;
    }
    let Ok(public) = c0mpute_net::libp2p::identity::ed25519::PublicKey::try_from_bytes(&key) else {
        return false;
    };
    public.verify(msg.as_bytes(), &sig)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub labeler: String,
    pub val: String,
    pub action: Action,
    pub evidence: Option<String>,
    pub appeal: String,
    /// Unix seconds the label expires.
    pub exp: i64,
}

/// Labels that count for this file under this policy.
pub fn accepted(
    labels: &[Label],
    src: &LabelerSource,
    actions: &BTreeMap<String, Action>,
    sha256_hex: &str,
    now_unix: i64,
) -> Vec<Hit> {
    let want = format!("sha256:{sha256_hex}");
    let mut out: Vec<Hit> = Vec::new();
    for l in labels {
        if l.neg || l.src != src.did || l.uri != want {
            continue;
        }
        let action = match actions.get(&l.val) {
            Some(a @ (Action::Refuse | Action::Warn)) => *a,
            _ => continue,
        };
        let Some(exp) = parse_rfc3339_utc(&l.exp) else {
            continue;
        };
        if exp <= now_unix || !verify_label(l, &src.public_key) {
            continue;
        }
        if !out.iter().any(|h| h.val == l.val && h.labeler == src.did) {
            out.push(Hit {
                labeler: src.did.clone(),
                val: l.val.clone(),
                action,
                evidence: l.evidence.as_ref().map(|e| e.chars().take(200).collect()),
                appeal: src.appeal.clone(),
                exp,
            });
        }
    }
    out
}

// ── talking to labelers ─────────────────────────────────────────────────────

/// What the gateway asks a labeler. A trait so tests need no network.
pub trait LabelerClient: Send + Sync {
    /// Scan one file (needs a token). Returns the labels in the answer.
    fn scan(
        &self,
        src: &LabelerSource,
        token: &str,
        name: &str,
        bytes: &[u8],
    ) -> BoxFuture<'static, Result<Vec<Label>, String>>;
    /// Labels in force for one SHA-256.
    fn query(
        &self,
        src: &LabelerSource,
        sha256_hex: &str,
    ) -> BoxFuture<'static, Result<Vec<Label>, String>>;
    /// Labels in force for several SHA-256s, in one call (at most
    /// [`QUERY_BATCH`]).
    fn query_many(
        &self,
        src: &LabelerSource,
        sha256_hex: &[String],
    ) -> BoxFuture<'static, Result<Vec<Label>, String>>;
}

/// Subjects per `/query` call: well inside the labeler's 64 KiB body cap.
pub const QUERY_BATCH: usize = 50;

#[derive(Deserialize)]
struct LabelsBody {
    #[serde(default)]
    labels: Vec<serde_json::Value>,
}

/// Junk entries are dropped one by one; one bad label never hides the others.
fn parse_labels(v: LabelsBody) -> Vec<Label> {
    v.labels
        .into_iter()
        .filter_map(|x| serde_json::from_value(x).ok())
        .collect()
}

pub struct HttpLabeler {
    client: reqwest::Client,
}

impl HttpLabeler {
    pub fn new(timeout: Duration) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(timeout)
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(concat!(
                    "c0mpute-gateway/",
                    env!("CARGO_PKG_VERSION"),
                    " moderation (+https://c0mpute.com)"
                ))
                .build()
                .unwrap_or_default(),
        }
    }

    fn post(
        &self,
        url: String,
        token: Option<String>,
        body: serde_json::Value,
    ) -> BoxFuture<'static, Result<Vec<Label>, String>> {
        let client = self.client.clone();
        async move {
            let mut req = client.post(&url).json(&body);
            if let Some(t) = token {
                req = req.bearer_auth(t);
            }
            let res = req
                .send()
                .await
                .map_err(|e| format!("labeler unreachable: {e}"))?;
            if !res.status().is_success() {
                return Err(format!("labeler answered {}", res.status()));
            }
            // Answers are small; refuse a runaway body.
            let bytes = res
                .bytes()
                .await
                .map_err(|e| format!("labeler body: {e}"))?;
            if bytes.len() > 1024 * 1024 {
                return Err("labeler answer too large".into());
            }
            let parsed: LabelsBody = serde_json::from_slice(&bytes)
                .map_err(|_| "labeler answered non-JSON".to_string())?;
            Ok(parse_labels(parsed))
        }
        .boxed()
    }
}

impl LabelerClient for HttpLabeler {
    fn scan(
        &self,
        src: &LabelerSource,
        token: &str,
        name: &str,
        bytes: &[u8],
    ) -> BoxFuture<'static, Result<Vec<Label>, String>> {
        let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
        let body = serde_json::json!({ "files": [{ "path": name, "content_b64": b64 }] });
        self.post(
            format!("{}/api/v1/labels/scan", src.url.trim_end_matches('/')),
            Some(token.to_string()),
            body,
        )
    }
    fn query(
        &self,
        src: &LabelerSource,
        sha256_hex: &str,
    ) -> BoxFuture<'static, Result<Vec<Label>, String>> {
        self.query_many(src, &[sha256_hex.to_string()])
    }
    fn query_many(
        &self,
        src: &LabelerSource,
        sha256_hex: &[String],
    ) -> BoxFuture<'static, Result<Vec<Label>, String>> {
        let uris: Vec<String> = sha256_hex.iter().map(|h| format!("sha256:{h}")).collect();
        let body = serde_json::json!({ "uris": uris });
        self.post(
            format!("{}/api/v1/labels/query", src.url.trim_end_matches('/')),
            None,
            body,
        )
    }
}

// ── the gate ────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub enum Decision {
    Clean,
    Flagged(Vec<Hit>),
}

#[derive(Clone)]
struct Entry {
    decision: Decision,
    /// Trusted until; after this it is served but refreshed.
    fresh_until: Instant,
    /// Whether some labeler has already scanned it (then refresh with /query).
    scanned: bool,
}

const MAX_ENTRIES: usize = 100_000;

type Pending = Shared<BoxFuture<'static, Option<(Decision, bool)>>>;

pub struct Gate {
    cfg: ModerationConfig,
    tokens: Vec<String>,
    client: Arc<dyn LabelerClient>,
    cache: Mutex<HashMap<[u8; 32], Entry>>,
    inflight: Mutex<HashMap<[u8; 32], Pending>>,
}

impl Gate {
    /// None when no labeler is configured or no action refuses or warns.
    pub fn from_config(config: &crate::config::Config) -> Option<Arc<Self>> {
        let cfg = config.moderation.clone();
        let client = Arc::new(HttpLabeler::new(Duration::from_millis(
            cfg.timeout_ms.max(500) * 4,
        )));
        Self::new(cfg, client)
    }

    pub fn new(cfg: ModerationConfig, client: Arc<dyn LabelerClient>) -> Option<Arc<Self>> {
        if cfg.labelers.is_empty() || !cfg.actions.values().any(|a| *a != Action::Ignore) {
            return None;
        }
        let tokens = cfg
            .labelers
            .iter()
            .map(|l| {
                if l.token_env.is_empty() {
                    String::new()
                } else {
                    std::env::var(&l.token_env).unwrap_or_default()
                }
            })
            .collect();
        tracing::info!(
            labelers = cfg.labelers.len(),
            "moderation: this node honours DIP-0036 labels"
        );
        Some(Arc::new(Self {
            cfg,
            tokens,
            client,
            cache: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashMap::new()),
        }))
    }

    fn remember(&self, key: [u8; 32], decision: Decision, scanned: bool) {
        let now = Instant::now();
        let now_unix = unix_now();
        let mut ttl = Duration::from_secs(self.cfg.recheck_secs.max(30));
        if let Decision::Flagged(hits) = &decision {
            // Re-ask no later than the earliest label expiry.
            if let Some(min) = hits.iter().map(|h| h.exp).min() {
                ttl = ttl.min(Duration::from_secs((min - now_unix).max(1) as u64));
            }
        }
        let mut cache = self.cache.lock().unwrap();
        if cache.len() >= MAX_ENTRIES {
            cache.retain(|_, e| e.fresh_until > now);
            if cache.len() >= MAX_ENTRIES {
                cache.clear();
            }
        }
        cache.insert(
            key,
            Entry {
                decision,
                fresh_until: now + ttl,
                scanned,
            },
        );
    }

    /// Ask every labeler; None when none could answer (fail open, not cached as clean).
    async fn ask(
        self: Arc<Self>,
        sha_hex: String,
        name: String,
        bytes: Option<Arc<Vec<u8>>>,
    ) -> Option<(Decision, bool)> {
        let mut hits = Vec::new();
        let mut answered = false;
        let mut scanned = false;
        for (i, src) in self.cfg.labelers.iter().enumerate() {
            let token = &self.tokens[i];
            let res = match (&bytes, token.is_empty()) {
                (Some(b), false) => {
                    scanned = true;
                    self.client.scan(src, token, &name, b).await
                }
                _ => self.client.query(src, &sha_hex).await,
            };
            match res {
                Ok(labels) => {
                    answered = true;
                    hits.extend(accepted(
                        &labels,
                        src,
                        &self.cfg.actions,
                        &sha_hex,
                        unix_now(),
                    ));
                }
                Err(e) => {
                    tracing::debug!(labeler = %src.did, error = %e, "moderation: labeler did not answer")
                }
            }
        }
        if !answered {
            return None;
        }
        Some((
            if hits.is_empty() {
                Decision::Clean
            } else {
                Decision::Flagged(hits)
            },
            scanned,
        ))
    }

    /// One shared labeler call per file at a time.
    fn pending(
        self: &Arc<Self>,
        key: [u8; 32],
        sha_hex: &str,
        name: &str,
        bytes: Option<Arc<Vec<u8>>>,
    ) -> Pending {
        let mut inflight = self.inflight.lock().unwrap();
        if let Some(p) = inflight.get(&key) {
            return p.clone();
        }
        let me = self.clone();
        let (sha, nm) = (sha_hex.to_string(), name.to_string());
        let fut = async move {
            let out = me.clone().ask(sha, nm, bytes).await;
            if let Some((d, scanned)) = &out {
                me.remember(key, d.clone(), *scanned);
            } else {
                // Nobody answered: do not hammer them, but do not call it clean for long.
                me.remember(key, Decision::Clean, false);
                if let Some(e) = me.cache.lock().unwrap().get_mut(&key) {
                    e.fresh_until = Instant::now() + Duration::from_secs(60);
                }
            }
            me.inflight.lock().unwrap().remove(&key);
            out
        }
        .boxed()
        .shared();
        inflight.insert(key, fut.clone());
        fut
    }

    /// The decision for a body, waiting at most `timeout_ms` on a miss.
    pub async fn decide(self: &Arc<Self>, name: &str, bytes: &[u8]) -> Decision {
        let digest: [u8; 32] = Sha256::digest(bytes).into();
        let sha_hex = hex::encode(digest);
        let cached = self.cache.lock().unwrap().get(&digest).cloned();
        match cached {
            Some(e) if e.fresh_until > Instant::now() => e.decision,
            Some(e) => {
                // Stale: answer now, refresh behind the response.
                let p = self.pending(
                    digest,
                    &sha_hex,
                    name,
                    if e.scanned {
                        None
                    } else {
                        Some(Arc::new(bytes.to_vec()))
                    },
                );
                tokio::spawn(async move {
                    let _ = p.await;
                });
                e.decision
            }
            None => {
                let p = self.pending(digest, &sha_hex, name, Some(Arc::new(bytes.to_vec())));
                match tokio::time::timeout(Duration::from_millis(self.cfg.timeout_ms), p.clone())
                    .await
                {
                    Ok(Some((d, _))) => d,
                    Ok(None) => Decision::Clean,
                    Err(_) => {
                        // Too slow: serve now (fail open); the call finishes and caches for the next visitor.
                        tokio::spawn(async move {
                            let _ = p.await;
                        });
                        Decision::Clean
                    }
                }
            }
        }
    }
}

impl Gate {
    /// How long a decision is trusted before it is asked for again.
    pub fn recheck(&self) -> Duration {
        Duration::from_secs(self.cfg.recheck_secs.max(30))
    }

    /// Whether the policy also covers the copies a hosting node holds
    /// (`[moderation] refuse_replicas`, on unless turned off) and some
    /// action refuses: `warn` alone is a gateway matter.
    pub fn refuses_replicas(&self) -> bool {
        self.cfg.refuse_replicas && self.cfg.actions.values().any(|a| *a == Action::Refuse)
    }

    /// The largest file sent to a labeler's `/scan`.
    pub fn max_scan_bytes(&self) -> usize {
        self.cfg.max_scan_bytes
    }

    /// Whether some labeler has a scan token (files can be sent, not only
    /// asked about by hash).
    pub fn scans(&self) -> bool {
        self.tokens.iter().any(|t| !t.is_empty())
    }

    /// [`Self::decide`] for a caller with no visitor waiting (a hosting
    /// worker checking a copy): on a miss it waits for the labelers' answer
    /// for as long as the client's own timeout allows, instead of serving
    /// first and learning later. Still fails open when nobody answers.
    pub async fn decide_patiently(self: &Arc<Self>, name: &str, bytes: &[u8]) -> Decision {
        let digest: [u8; 32] = Sha256::digest(bytes).into();
        let cached = self.cache.lock().unwrap().get(&digest).cloned();
        let scanned = match cached {
            Some(e) if e.fresh_until > Instant::now() => return e.decision,
            Some(e) => e.scanned,
            None => false,
        };
        let body = if scanned { None } else { Some(Arc::new(bytes.to_vec())) };
        match self.pending(digest, &hex::encode(digest), name, body).await {
            Some((d, _)) => d,
            None => Decision::Clean,
        }
    }

    /// Decisions for files known by SHA-256 only, in the order given. What a
    /// hosting worker asks: it has every file of a site, and the labelers'
    /// `/query` takes hashes in batches. A fresh cached decision answers;
    /// the rest are asked, [`QUERY_BATCH`] at a time. Fails open: a file no
    /// labeler answered for keeps its last decision while a label of it is
    /// still unexpired, and is otherwise clean (and not remembered as such).
    pub async fn decide_hashes(self: &Arc<Self>, digests: &[[u8; 32]]) -> Vec<Decision> {
        let now = Instant::now();
        // (decision, fresh, scanned) of what is cached.
        let known: HashMap<[u8; 32], (Decision, bool, bool)> = {
            let cache = self.cache.lock().unwrap();
            digests
                .iter()
                .filter_map(|d| {
                    cache
                        .get(d)
                        .map(|e| (*d, (e.decision.clone(), e.fresh_until > now, e.scanned)))
                })
                .collect()
        };
        let mut ask: Vec<[u8; 32]> = digests
            .iter()
            .filter(|d| !known.get(*d).is_some_and(|k| k.1))
            .copied()
            .collect();
        ask.sort();
        ask.dedup();
        let mut found: HashMap<[u8; 32], Decision> = HashMap::new();
        for batch in ask.chunks(QUERY_BATCH) {
            let hexes: Vec<String> = batch.iter().map(hex::encode).collect();
            let mut hits: HashMap<[u8; 32], Vec<Hit>> = HashMap::new();
            let mut answered = false;
            for src in &self.cfg.labelers {
                match self.client.query_many(src, &hexes).await {
                    Ok(labels) => {
                        answered = true;
                        for (d, h) in batch.iter().zip(&hexes) {
                            let got = accepted(&labels, src, &self.cfg.actions, h, unix_now());
                            if !got.is_empty() {
                                hits.entry(*d).or_default().extend(got);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::debug!(labeler = %src.did, error = %e, "moderation: labeler did not answer")
                    }
                }
            }
            if !answered {
                continue;
            }
            for d in batch {
                let decision = match hits.remove(d) {
                    Some(h) => Decision::Flagged(h),
                    None => Decision::Clean,
                };
                // Keep what the gateway knows: a file a labeler scanned is
                // refreshed by /query there too.
                self.remember(*d, decision.clone(), known.get(d).is_some_and(|k| k.2));
                found.insert(*d, decision);
            }
        }
        let now_unix = unix_now();
        digests
            .iter()
            .map(|d| {
                if let Some(f) = found.get(d) {
                    return f.clone();
                }
                match known.get(d) {
                    Some((decision, true, _)) => decision.clone(),
                    // Stale and nobody answered: the labels still in force.
                    Some((Decision::Flagged(hits), false, _)) => {
                        let live: Vec<Hit> = hits.iter().filter(|h| h.exp > now_unix).cloned().collect();
                        if live.is_empty() {
                            Decision::Clean
                        } else {
                            Decision::Flagged(live)
                        }
                    }
                    _ => Decision::Clean,
                }
            })
            .collect()
    }
}

/// The first hit of a decision that this operator's policy refuses.
pub fn refusing(d: &Decision) -> Option<&Hit> {
    match d {
        Decision::Flagged(hits) => hits.iter().find(|h| h.action == Action::Refuse),
        Decision::Clean => None,
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Content types worth judging, and the file name the labeler's rules expect for each.
pub(crate) fn judged_name(content_type: &str) -> Option<&'static str> {
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match ct.as_str() {
        "text/html" | "application/xhtml+xml" => Some("index.html"),
        "text/javascript"
        | "application/javascript"
        | "application/x-javascript"
        | "text/ecmascript"
        | "application/ecmascript" => Some("script.js"),
        "image/svg+xml" => Some("image.svg"),
        _ => None,
    }
}

const CONTINUE_PARAM: &str = "c0mpute-moderation=continue";

/// axum middleware: `router.layer(from_fn_with_state(gate, moderation::gate))`.
pub async fn gate(State(g): State<Arc<Gate>>, req: Request, next: Next) -> Response {
    if req.method() != Method::GET && req.method() != Method::HEAD {
        return next.run(req).await;
    }
    let continued = req
        .uri()
        .query()
        .is_some_and(|q| q.split('&').any(|p| p == CONTINUE_PARAM));
    let path = req.uri().path().to_string();
    let res = next.run(req).await;
    if res.status() != StatusCode::OK {
        return res;
    }
    let Some(name) = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(judged_name)
    else {
        return res;
    };
    // Only a known length we are willing to buffer; anything else streams through.
    // The header, or the body's exact size: hosting's meta rewrite drops the
    // header on HTML it rewrote, but the rewritten body knows its length.
    let len = res
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        .or_else(|| {
            axum::body::HttpBody::size_hint(res.body())
                .exact()
                .and_then(|n| usize::try_from(n).ok())
        });
    let Some(len) = len.filter(|n| *n > 0 && *n <= g.cfg.max_scan_bytes) else {
        return res;
    };
    let (parts, body) = res.into_parts();
    let bytes = match axum::body::to_bytes(body, len.saturating_add(1)).await {
        Ok(b) => b,
        // The body broke mid-stream; the client would have seen that anyway.
        Err(_) => return (StatusCode::BAD_GATEWAY, "upstream body error").into_response(),
    };
    match g.decide(name, &bytes).await {
        Decision::Clean => Response::from_parts(parts, Body::from(bytes)),
        Decision::Flagged(hits) => {
            let refuse = hits.iter().any(|h| h.action == Action::Refuse);
            if !refuse && continued {
                return Response::from_parts(parts, Body::from(bytes));
            }
            tracing::info!(path = %path, labels = ?hits.iter().map(|h| h.val.as_str()).collect::<Vec<_>>(), refuse, "moderation: blocked a labelled file");
            refusal(&hits, refuse, &path)
        }
    }
}

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            c if c.is_control() => {}
            c => o.push(c),
        }
    }
    o
}

fn refusal(hits: &[Hit], refuse: bool, path: &str) -> Response {
    let items: String = hits
        .iter()
        .map(|h| {
            let appeal = if h.appeal.is_empty() {
                String::new()
            } else {
                format!(" Appeal: <code>{}</code>.", esc(&h.appeal))
            };
            let ev = h
                .evidence
                .as_deref()
                .map(|e| format!(" <small>({})</small>", esc(e)))
                .unwrap_or_default();
            format!(
                "<li><b>{}</b> by <code>{}</code>{}.{}</li>",
                esc(&h.val),
                esc(&h.labeler),
                ev,
                appeal
            )
        })
        .collect();
    let (title, lead, more) = if refuse {
        (
            "Blocked by this gateway's content policy",
            "This gateway does not serve this file because a labeler it subscribes to flagged it:",
            String::new(),
        )
    } else {
        let sep = if path.contains('?') { '&' } else { '?' };
        (
            "Warning from this gateway",
            "A labeler this gateway subscribes to flagged this file:",
            format!(
                "<p><a href=\"{}{}{}\" rel=\"nofollow\">I understand the risk, show it anyway</a></p>",
                esc(path),
                sep,
                CONTINUE_PARAM
            ),
        )
    };
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><meta name=\"robots\" content=\"noindex\"><title>{title}</title><style>body{{font:16px/1.5 system-ui,sans-serif;max-width:40rem;margin:3rem auto;padding:0 1rem}}code{{word-break:break-all}}</style></head><body><h1>{title}</h1><p>{lead}</p><ul>{items}</ul>{more}<p>Other c0mpute gateways may still serve it: this is one operator's choice (<a href=\"https://github.com/profullstack/c0mpute/blob/master/dips/v2.x/0036-layered-moderation.md\">DIP-0036</a>).</p></body></html>"
    );
    let vals: Vec<&str> = hits.iter().map(|h| h.val.as_str()).collect();
    let mut res = (
        if refuse {
            StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS
        } else {
            StatusCode::OK
        },
        html,
    )
        .into_response();
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        header::HeaderName::from_static("x-robots-tag"),
        HeaderValue::from_static("noindex"),
    );
    h.insert(
        header::HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'"),
    );
    if let Ok(v) = HeaderValue::from_str(&format!(
        "{}; labels={}",
        if refuse { "refused" } else { "warned" },
        vals.join(",")
    )) {
        h.insert(header::HeaderName::from_static("x-c0mpute-moderation"), v);
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // RFC 8032 test vector 1 secret key; tests sign with libp2p's ed25519.
    const SEED_HEX: &str = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";
    const PUB_B64: &str = "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo";

    fn keypair() -> c0mpute_net::libp2p::identity::ed25519::Keypair {
        let mut seed = hex::decode(SEED_HEX).unwrap();
        let sk =
            c0mpute_net::libp2p::identity::ed25519::SecretKey::try_from_bytes(&mut seed).unwrap();
        c0mpute_net::libp2p::identity::ed25519::Keypair::from(sk)
    }

    fn iso(unix: i64) -> String {
        // inverse of parse_rfc3339_utc for tests (civil from days)
        let days = unix.div_euclid(86_400);
        let secs = unix.rem_euclid(86_400);
        let z = days + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if m <= 2 { y + 1 } else { y };
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            y,
            m,
            d,
            secs / 3600,
            (secs % 3600) / 60,
            secs % 60
        )
    }

    fn sha_hex(b: &[u8]) -> String {
        hex::encode(Sha256::digest(b))
    }

    fn label(sha: &str, val: &str, neg: bool, exp_in: i64) -> Label {
        let now = unix_now();
        let mut l = Label {
            src: "did:web:labeler.test".into(),
            uri: format!("sha256:{sha}"),
            val: val.into(),
            neg,
            cts: iso(now - 10),
            exp: iso(now + exp_in),
            evidence: Some("crushav:test".into()),
            sig: String::new(),
        };
        let sig = keypair().sign(canonical(&l).unwrap().as_bytes());
        l.sig = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig);
        l
    }

    fn source() -> LabelerSource {
        LabelerSource {
            url: "https://labeler.test".into(),
            did: "did:web:labeler.test".into(),
            public_key: PUB_B64.into(),
            token_env: String::new(),
            appeal: "appeals@labeler.test".into(),
        }
    }

    fn actions() -> BTreeMap<String, Action> {
        BTreeMap::from([
            ("malware".into(), Action::Refuse),
            ("phishing".into(), Action::Refuse),
            ("spam".into(), Action::Warn),
        ])
    }

    #[test]
    fn rfc8032_public_key_and_timestamps() {
        let pk = keypair().public().to_bytes();
        assert_eq!(
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(pk),
            PUB_B64
        );
        assert_eq!(parse_rfc3339_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_utc("2026-11-22T00:00:00Z"),
            Some(1_795_305_600)
        );
        assert_eq!(parse_rfc3339_utc("2000-02-29T12:34:56Z"), Some(951_827_696));
        for bad in [
            "2026-11-22",
            "2026-11-22T00:00:00.000Z",
            "2026-13-01T00:00:00Z",
            "2026-11-22T24:00:00Z",
            "2026-11-22T00:00:00+00:00",
            "+026-11-22T00:00:00Z",
        ] {
            assert_eq!(parse_rfc3339_utc(bad), None, "{bad}");
        }
        let now = unix_now();
        assert_eq!(parse_rfc3339_utc(&iso(now)), Some(now));
    }

    #[test]
    fn framing_matches_the_threatcrush_labeler() {
        let l = Label {
            src: "did:web:threatcrush.com".into(),
            uri: format!("sha256:{}", "a".repeat(64)),
            val: "malware".into(),
            neg: false,
            cts: "2026-11-22T00:00:00Z".into(),
            exp: "2026-12-22T00:00:00Z".into(),
            evidence: None,
            sig: String::new(),
        };
        assert_eq!(
            canonical(&l).unwrap(),
            format!(
                "c0mpute-label-v1\ndid:web:threatcrush.com\nsha256:{}\nmalware\n0\n2026-11-22T00:00:00Z\n2026-12-22T00:00:00Z",
                "a".repeat(64)
            )
        );
        let mut bad = l.clone();
        bad.uri.push('\n');
        assert!(canonical(&bad).is_none());
    }

    #[test]
    fn only_valid_labels_for_this_file_count() {
        let sha = sha_hex(b"evil");
        let good = label(&sha, "phishing", false, 3600);
        assert_eq!(
            accepted(&[good.clone()], &source(), &actions(), &sha, unix_now()).len(),
            1
        );
        let other_file = label(&sha_hex(b"someone else"), "phishing", false, 3600);
        let negated = label(&sha, "phishing", true, 3600);
        let expired = label(&sha, "phishing", false, -5);
        let ignored_val = label(&sha, "cryptominer", false, 3600);
        let mut wrong_src = good.clone();
        wrong_src.src = "did:web:evil.example".into();
        let mut tampered = good.clone();
        tampered.val = "malware".into();
        let mut lifted = good.clone();
        lifted.sig = label(&sha, "malware", false, 3600).sig;
        let mut junk_sig = good.clone();
        junk_sig.sig = "!!".into();
        for l in [
            other_file,
            negated,
            expired,
            ignored_val,
            wrong_src,
            tampered,
            lifted,
            junk_sig,
        ] {
            assert!(
                accepted(&[l.clone()], &source(), &actions(), &sha, unix_now()).is_empty(),
                "{l:?}"
            );
        }
        let mut wrong_key = source();
        wrong_key.public_key = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([7u8; 32]);
        assert!(accepted(&[good], &wrong_key, &actions(), &sha, unix_now()).is_empty());
    }

    #[test]
    fn junk_entries_are_dropped_one_by_one() {
        let sha = sha_hex(b"x");
        let good = serde_json::to_value(serde_json::json!({
            "src": "did:web:labeler.test", "uri": format!("sha256:{sha}"), "val": "malware", "neg": false,
            "cts": "2026-11-22T00:00:00Z", "exp": "2026-12-22T00:00:00Z", "sig": "x"
        }))
        .unwrap();
        let body = LabelsBody {
            labels: vec![
                serde_json::json!(null),
                serde_json::json!("x"),
                serde_json::json!({"neg": "no"}),
                good,
            ],
        };
        assert_eq!(parse_labels(body).len(), 1);
    }

    /// A labeler that answers from a fixed list and counts calls.
    struct Fake {
        labels: Mutex<Vec<Label>>,
        fail: bool,
        delay_ms: u64,
        scans: AtomicUsize,
        queries: AtomicUsize,
    }

    impl Fake {
        fn new(labels: Vec<Label>) -> Arc<Self> {
            Arc::new(Self {
                labels: Mutex::new(labels),
                fail: false,
                delay_ms: 0,
                scans: AtomicUsize::new(0),
                queries: AtomicUsize::new(0),
            })
        }
    }

    impl LabelerClient for Fake {
        fn scan(
            &self,
            _: &LabelerSource,
            _: &str,
            _: &str,
            _: &[u8],
        ) -> BoxFuture<'static, Result<Vec<Label>, String>> {
            self.scans.fetch_add(1, Ordering::SeqCst);
            self.answer()
        }
        fn query(
            &self,
            _: &LabelerSource,
            _: &str,
        ) -> BoxFuture<'static, Result<Vec<Label>, String>> {
            self.queries.fetch_add(1, Ordering::SeqCst);
            self.answer()
        }
        fn query_many(
            &self,
            _: &LabelerSource,
            _: &[String],
        ) -> BoxFuture<'static, Result<Vec<Label>, String>> {
            self.queries.fetch_add(1, Ordering::SeqCst);
            self.answer()
        }
    }

    impl Fake {
        fn answer(&self) -> BoxFuture<'static, Result<Vec<Label>, String>> {
            let (labels, fail, delay) = (
                self.labels.lock().unwrap().clone(),
                self.fail,
                self.delay_ms,
            );
            async move {
                if delay > 0 {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
                if fail { Err("down".into()) } else { Ok(labels) }
            }
            .boxed()
        }
    }

    fn cfg() -> ModerationConfig {
        ModerationConfig {
            labelers: vec![source()],
            actions: actions(),
            timeout_ms: 300,
            ..Default::default()
        }
    }

    fn app(g: Arc<Gate>) -> axum::Router {
        axum::Router::new()
            .route(
                "/s/{site}/evil.html",
                get(|| async { ([(header::CONTENT_TYPE, "text/html")], "evil") }),
            )
            .route(
                "/s/{site}/spam.html",
                get(|| async { ([(header::CONTENT_TYPE, "text/html")], "spammy") }),
            )
            .route(
                "/s/{site}/ok.html",
                get(|| async { ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], "fine") }),
            )
            .route(
                "/s/{site}/evil.png",
                get(|| async { ([(header::CONTENT_TYPE, "image/png")], "evil") }),
            )
            .route(
                "/s/{site}/missing",
                get(|| async { (StatusCode::NOT_FOUND, "evil") }),
            )
            .layer(axum::middleware::from_fn_with_state(g, gate))
    }

    async fn fetch(app: &axum::Router, uri: &str) -> (StatusCode, String, Option<String>) {
        use tower::ServiceExt;
        let res = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let mark = res
            .headers()
            .get("x-c0mpute-moderation")
            .map(|v| v.to_str().unwrap().to_string());
        let body = String::from_utf8(
            axum::body::to_bytes(res.into_body(), 1 << 20)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        (status, body, mark)
    }

    #[test]
    fn off_without_labelers_or_refusing_actions() {
        assert!(Gate::new(ModerationConfig::default(), Fake::new(vec![])).is_none());
        let mut c = cfg();
        c.actions = BTreeMap::from([("malware".into(), Action::Ignore)]);
        assert!(Gate::new(c, Fake::new(vec![])).is_none());
    }

    #[tokio::test]
    async fn refuses_a_labelled_file_and_serves_everything_else() {
        let fake = Fake::new(vec![
            label(&sha_hex(b"evil"), "malware", false, 3600),
            label(&sha_hex(b"spammy"), "spam", false, 3600),
        ]);
        let g = Gate::new(cfg(), fake.clone()).unwrap();
        let app = app(g);
        let (s, body, mark) = fetch(&app, "/s/abc/evil.html").await;
        assert_eq!(s, StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS);
        assert!(
            body.contains("malware")
                && body.contains("appeals@labeler.test")
                && !body.contains(">evil<")
        );
        assert_eq!(mark.as_deref(), Some("refused; labels=malware"));
        assert_eq!(fetch(&app, "/s/abc/ok.html").await.1, "fine");
        // Not web content, or not a 200: never judged, never touched.
        assert_eq!(fetch(&app, "/s/abc/evil.png").await.1, "evil");
        assert_eq!(fetch(&app, "/s/abc/missing").await.0, StatusCode::NOT_FOUND);
        // warn: an interstitial with a way through
        let (s, body, _) = fetch(&app, "/s/abc/spam.html").await;
        assert_eq!(s, StatusCode::OK);
        assert!(body.contains("c0mpute-moderation=continue"));
        assert_eq!(
            fetch(&app, "/s/abc/spam.html?c0mpute-moderation=continue")
                .await
                .1,
            "spammy"
        );
        // a refusal has no way through
        assert_eq!(
            fetch(&app, "/s/abc/evil.html?c0mpute-moderation=continue")
                .await
                .0,
            StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS
        );
    }

    #[tokio::test]
    async fn caches_decisions_and_shares_one_call_per_file() {
        let fake = Fake::new(vec![label(&sha_hex(b"evil"), "malware", false, 3600)]);
        let app = app(Gate::new(cfg(), fake.clone()).unwrap());
        let mut handles = Vec::new();
        for _ in 0..20 {
            let a = app.clone();
            handles.push(tokio::spawn(async move {
                fetch(&a, "/s/abc/evil.html").await.0
            }));
        }
        for h in handles {
            assert_eq!(h.await.unwrap(), StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS);
        }
        assert_eq!(
            fake.queries.load(Ordering::SeqCst) + fake.scans.load(Ordering::SeqCst),
            1
        );
        fetch(&app, "/s/abc/evil.html").await;
        assert_eq!(fake.queries.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn fails_open_when_the_labeler_is_down_or_slow() {
        let mut f = Fake {
            labels: Mutex::new(vec![label(&sha_hex(b"evil"), "malware", false, 3600)]),
            fail: true,
            delay_ms: 0,
            scans: AtomicUsize::new(0),
            queries: AtomicUsize::new(0),
        };
        let app1 = app(Gate::new(cfg(), Arc::new(f)).unwrap());
        assert_eq!(fetch(&app1, "/s/abc/evil.html").await.1, "evil");
        f = Fake {
            labels: Mutex::new(vec![label(&sha_hex(b"evil"), "malware", false, 3600)]),
            fail: false,
            delay_ms: 2000,
            scans: AtomicUsize::new(0),
            queries: AtomicUsize::new(0),
        };
        let app2 = app(Gate::new(cfg(), Arc::new(f)).unwrap());
        let t = Instant::now();
        assert_eq!(fetch(&app2, "/s/abc/evil.html").await.1, "evil");
        assert!(
            t.elapsed() < Duration::from_millis(1500),
            "waited {:?}",
            t.elapsed()
        );
    }

    #[tokio::test]
    async fn a_retraction_is_picked_up_on_recheck() {
        let sha = sha_hex(b"evil");
        let fake = Fake::new(vec![label(&sha, "malware", false, 3600)]);
        let mut c = cfg();
        c.recheck_secs = 30;
        let g = Gate::new(c, fake.clone()).unwrap();
        let app = app(g.clone());
        assert_eq!(
            fetch(&app, "/s/abc/evil.html").await.0,
            StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS
        );
        // The labeler retracts; force the entry stale, as an hour passing would.
        *fake.labels.lock().unwrap() = vec![];
        for e in g.cache.lock().unwrap().values_mut() {
            e.fresh_until = Instant::now() - Duration::from_secs(1);
        }
        // Stale: still refused this once while the refresh runs behind it...
        assert_eq!(
            fetch(&app, "/s/abc/evil.html").await.0,
            StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        // ...then served.
        assert_eq!(fetch(&app, "/s/abc/evil.html").await.1, "evil");
    }

    #[tokio::test]
    async fn uses_scan_with_a_token_then_query_on_refresh() {
        // SAFETY: test-only env var with a unique name.
        unsafe { std::env::set_var("C0MPUTE_TEST_LABELER_TOKEN_X", "tok") };
        let mut c = cfg();
        c.labelers[0].token_env = "C0MPUTE_TEST_LABELER_TOKEN_X".into();
        let fake = Fake::new(vec![]);
        let g = Gate::new(c, fake.clone()).unwrap();
        let app = app(g.clone());
        fetch(&app, "/s/abc/ok.html").await;
        assert_eq!(
            (
                fake.scans.load(Ordering::SeqCst),
                fake.queries.load(Ordering::SeqCst)
            ),
            (1, 0)
        );
        for e in g.cache.lock().unwrap().values_mut() {
            e.fresh_until = Instant::now() - Duration::from_secs(1);
        }
        fetch(&app, "/s/abc/ok.html").await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            (
                fake.scans.load(Ordering::SeqCst),
                fake.queries.load(Ordering::SeqCst)
            ),
            (1, 1)
        );
    }

    #[tokio::test]
    async fn hashes_are_asked_in_batches_cached_and_fail_open() {
        let evil: [u8; 32] = Sha256::digest(b"evil").into();
        let spam: [u8; 32] = Sha256::digest(b"spammy").into();
        let fine: [u8; 32] = Sha256::digest(b"fine").into();
        let fake = Fake::new(vec![
            label(&sha_hex(b"evil"), "malware", false, 3600),
            label(&sha_hex(b"spammy"), "spam", false, 3600),
        ]);
        let g = Gate::new(cfg(), fake.clone()).unwrap();
        // 120 distinct files: three batches, one call each.
        let mut many: Vec<[u8; 32]> = (0..117u32)
            .map(|i| Sha256::digest(i.to_le_bytes()).into())
            .collect();
        many.extend([evil, spam, fine]);
        let out = g.decide_hashes(&many).await;
        assert_eq!(out.len(), 120);
        assert_eq!(fake.queries.load(Ordering::SeqCst), 3);
        assert_eq!(refusing(&out[117]).map(|h| h.val.as_str()), Some("malware"));
        // `warn` is a decision, but not one a worker refuses on.
        assert!(matches!(out[118], Decision::Flagged(_)) && refusing(&out[118]).is_none());
        assert_eq!(out[119], Decision::Clean);
        assert!(out[..117].iter().all(|d| *d == Decision::Clean));
        // All cached now: no further call, the same answers, duplicates fine.
        let again = g.decide_hashes(&[evil, evil, fine]).await;
        assert_eq!(fake.queries.load(Ordering::SeqCst), 3);
        assert!(refusing(&again[0]).is_some() && refusing(&again[1]).is_some());
        assert_eq!(again[2], Decision::Clean);
        // The gateway's per-file path reads the same cache.
        assert!(matches!(g.decide("index.html", b"evil").await, Decision::Flagged(_)));
        assert_eq!(
            fake.queries.load(Ordering::SeqCst) + fake.scans.load(Ordering::SeqCst),
            3
        );

        // A labeler that is down: unknown files are clean (fail open)...
        let down = Arc::new(Fake {
            labels: Mutex::new(vec![]),
            fail: true,
            delay_ms: 0,
            scans: AtomicUsize::new(0),
            queries: AtomicUsize::new(0),
        });
        let g2 = Gate::new(cfg(), down).unwrap();
        assert_eq!(g2.decide_hashes(&[evil]).await, vec![Decision::Clean]);
        assert!(
            g2.cache.lock().unwrap().is_empty(),
            "an unanswered question is not remembered as clean"
        );
        // ...but a stale refusal stands while its label is unexpired.
        let hits = accepted(
            &[label(&sha_hex(b"evil"), "malware", false, 3600)],
            &source(),
            &actions(),
            &sha_hex(b"evil"),
            unix_now(),
        );
        g2.remember(evil, Decision::Flagged(hits), false);
        for e in g2.cache.lock().unwrap().values_mut() {
            e.fresh_until = Instant::now() - Duration::from_secs(1);
        }
        assert!(refusing(&g2.decide_hashes(&[evil]).await[0]).is_some());
    }

    #[tokio::test]
    async fn a_retracted_label_clears_a_hash_decision_on_recheck() {
        let evil: [u8; 32] = Sha256::digest(b"evil").into();
        let fake = Fake::new(vec![label(&sha_hex(b"evil"), "malware", false, 3600)]);
        let g = Gate::new(cfg(), fake.clone()).unwrap();
        assert!(refusing(&g.decide_hashes(&[evil]).await[0]).is_some());
        *fake.labels.lock().unwrap() = vec![];
        for e in g.cache.lock().unwrap().values_mut() {
            e.fresh_until = Instant::now() - Duration::from_secs(1);
        }
        assert_eq!(g.decide_hashes(&[evil]).await, vec![Decision::Clean]);
    }

    #[test]
    fn the_refusal_page_escapes_everything() {
        let hits = vec![Hit {
            labeler: "did:<x>".into(),
            val: "malware".into(),
            action: Action::Refuse,
            evidence: Some("<script>alert(1)</script>".into()),
            appeal: "\"a\"@x".into(),
            exp: 0,
        }];
        let res = refusal(&hits, true, "/s/x/\"><script>");
        let csp = res
            .headers()
            .get("content-security-policy")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(csp.contains("default-src 'none'"));
        let body =
            futures::executor::block_on(axum::body::to_bytes(res.into_body(), 1 << 20)).unwrap();
        let s = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            !s.contains("<script>") && s.contains("&lt;script&gt;") && s.contains("&quot;a&quot;")
        );
    }
}
