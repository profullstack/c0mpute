//! Charge bots for the files this gateway serves (`x402` plugin).
//!
//! Off by default. An operator who turns it on sells day passes to AI
//! training crawlers (GPTBot, ClaudeBot, CCBot, Bytespider, ...) that fetch
//! hosted sites through this gateway: `/s/<site>/...`, custom domains, and
//! optionally `/storage/v1` reads. The crawler gets HTTP 402 with an x402 v2
//! offer, pays USDC through CoinPay (the operator's own CoinPay key, money
//! straight to the operator's `pay_to` wallet), and gets back a signed pass
//! (`x-crawl-pass` header or `Authorization: Bearer`) good for the days it
//! paid for. People, retrieval agents (ChatGPT-User, Claude-User,
//! PerplexityBot, ...) and the network's own traffic are never charged.
//!
//! The protocol is the `x402-gateway` crate (profullstack/x402-ports, a port
//! of `@profullstack/x402-gateway`), so a c0mpute gateway answers exactly
//! like every Profullstack site and `x402 pay <url>` works against it.
//!
//! ```toml
//! [x402]
//! enabled = true
//! pay_to = "0x..."                      # your EVM wallet (USDC on Base/Polygon/Ethereum)
//! api_key_env = "C0MPUTE_X402_COINPAY_KEY"  # env var holding a CoinPay key (x402 scope) gitleaks:allow
//! price_cents = 100                     # per day
//! ```
//!
//! A pass is signed with a secret kept in `<data_dir>/x402/secret`, so it is
//! good at the gateway that sold it, across restarts. Sales are appended to
//! `<data_dir>/x402/sales.jsonl`; the money itself is in your CoinPay account.

use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

use axum::{
    body::Body,
    extract::{Request, State},
    http::HeaderMap,
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use x402_gateway::{Gateway, Options, Sale, Transport};

/// `[x402]` in config.toml.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct X402Config {
    /// Charge training crawlers for what this gateway serves. Default false.
    #[serde(default)]
    pub enabled: bool,
    /// Your EVM address. One address takes USDC on Base, Polygon and Ethereum.
    #[serde(default)]
    pub pay_to: String,
    /// Environment variable holding a CoinPay API key that may verify and
    /// settle x402 payments. The key never goes in config.toml.
    #[serde(default = "default_key_env")]
    pub api_key_env: String,
    #[serde(default = "default_coinpay_url")]
    pub coinpay_url: String,
    /// Price of one day, in cents.
    #[serde(default = "default_price_cents")]
    pub price_cents: f64,
    #[serde(default = "default_pass_minutes")]
    pub pass_minutes: i64,
    /// Most days one payment can buy.
    #[serde(default = "default_max_days")]
    pub max_days: i64,
    /// Where the sale (offer, sales page, pass) answers. Namespaced so it
    /// never shadows a path on a hosted site's custom domain.
    #[serde(default = "default_path")]
    pub path: String,
    /// The public origin crawlers are told to buy at. Default
    /// `[gateway] announce`, else `http://<bind>`.
    #[serde(default)]
    pub site_url: Option<String>,
    /// Also charge browsers that send no Sec-Fetch headers (scrapers posing
    /// as Chrome). Off by default: it catches real but unusual clients too.
    #[serde(default)]
    pub charge_spoofed_browsers: bool,
    /// Also gate `/storage/v1` object reads, not only hosted sites.
    #[serde(default)]
    pub gate_storage: bool,
    /// Site ids this gateway never charges for (`/s/<id>/`).
    #[serde(default)]
    pub exclude_sites: Vec<String>,
    /// Hosts (custom domains) this gateway never charges for.
    #[serde(default)]
    pub exclude_hosts: Vec<String>,
    /// Shown on the sales page.
    #[serde(default)]
    pub contact: String,
}

fn default_key_env() -> String {
    "C0MPUTE_X402_COINPAY_KEY".into()
}
fn default_coinpay_url() -> String {
    "https://coinpayportal.com".into()
}
fn default_price_cents() -> f64 {
    100.0
}
fn default_pass_minutes() -> i64 {
    1440
}
fn default_max_days() -> i64 {
    30
}
fn default_path() -> String {
    "/_x402/crawl".into()
}

impl Default for X402Config {
    fn default() -> Self {
        Self {
            enabled: false,
            pay_to: String::new(),
            api_key_env: default_key_env(),
            coinpay_url: default_coinpay_url(),
            price_cents: default_price_cents(),
            pass_minutes: default_pass_minutes(),
            max_days: default_max_days(),
            path: default_path(),
            site_url: None,
            charge_spoofed_browsers: false,
            gate_storage: false,
            exclude_sites: Vec::new(),
            exclude_hosts: Vec::new(),
            contact: String::new(),
        }
    }
}

/// Paths the gate never charges on: the network's own hosting API (edges and
/// other gateways read chunks through it) and the private loader (ciphertext
/// only). The gate is layered only on the hosting and storage routers, so the
/// node's other APIs never reach it.
const OPEN_PATHS: &[&str] = &["/healthz", "/hosting/v1/", "/p/"];

/// The gate in front of hosted files.
pub struct HostingGate {
    gw: Gateway,
    exclude_sites: Vec<String>,
    exclude_hosts: Vec<String>,
    pub gate_storage: bool,
    pub path: String,
}

impl HostingGate {
    /// Build the gate from config, or None when it is off or incomplete
    /// (incomplete is logged: a gate that cannot settle would only annoy).
    pub fn from_config(cfg: &crate::config::Config) -> Option<Arc<HostingGate>> {
        let x = &cfg.x402;
        if !x.enabled {
            return None;
        }
        if cfg.private.enabled {
            warn!("x402: private mode serves nothing public; gate off");
            return None;
        }
        let key = std::env::var(&x.api_key_env).unwrap_or_default();
        if key.trim().is_empty() || x.pay_to.trim().is_empty() {
            warn!(
                env = %x.api_key_env,
                "x402: enabled but pay_to or the CoinPay key is missing; gate off"
            );
            return None;
        }
        let dir = crate::config::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("x402");
        let secret = match load_or_create_secret(&dir) {
            Ok(s) => s,
            Err(e) => {
                warn!(err = %e, "x402: cannot keep a pass secret; gate off");
                return None;
            }
        };
        let site_url = x
            .site_url
            .clone()
            .or_else(|| cfg.gateway.announce.clone())
            .unwrap_or_else(|| format!("http://{}", cfg.gateway.bind));
        let sales = dir.join("sales.jsonl");
        let opts = Options {
            site_url: site_url.clone(),
            site_name: Some(format!("c0mpute gateway {}", host_of(&site_url))),
            coinpay_api_key: key.trim().to_string(),
            coinpay_base_url: x.coinpay_url.clone(),
            pay_to: x.pay_to.trim().to_string(),
            price_cents: x.price_cents,
            pass_minutes: x.pass_minutes,
            max_days: x.max_days,
            path: x.path.clone(),
            open_paths: open_paths(x.gate_storage),
            charge_spoofed_browsers: x.charge_spoofed_browsers,
            secret,
            contact: x.contact.clone(),
            on_sale: Some(Arc::new(move |s: Sale| record_sale(&sales, &s))),
            transport: Some(Box::new(ReqwestTransport::new())),
            ..Options::default()
        };
        let gw = match Gateway::new(opts) {
            Ok(g) => g,
            Err(e) => {
                warn!(err = %e, "x402: gateway config rejected; gate off");
                return None;
            }
        };
        info!(
            site_url = %site_url,
            pay_to = %x.pay_to,
            price_cents = x.price_cents,
            storage = x.gate_storage,
            "x402: charging training crawlers for hosted files"
        );
        Some(Arc::new(HostingGate {
            gw,
            exclude_sites: x.exclude_sites.clone(),
            exclude_hosts: x.exclude_hosts.iter().map(|h| h.to_ascii_lowercase()).collect(),
            gate_storage: x.gate_storage,
            path: x.path.clone(),
        }))
    }

    /// Requests this operator chose never to charge for.
    fn excluded(&self, path: &str, h: &HeaderMap) -> bool {
        if !self.exclude_hosts.is_empty() {
            let host = h
                .get("host")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .split(':')
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            if self.exclude_hosts.iter().any(|x| *x == host) {
                return true;
            }
        }
        if let Some(rest) = path.strip_prefix("/s/") {
            let site = rest.split('/').next().unwrap_or("");
            if self.exclude_sites.iter().any(|x| x == site) {
                return true;
            }
        }
        false
    }

    /// The gate on parts. None = carry on to the file.
    pub fn check(&self, path: &str, query: Option<&str>, h: &HeaderMap) -> Option<Response<String>> {
        if path != self.path && self.excluded(path, h) {
            return None;
        }
        self.gw.handle_parts(path, query, h)
    }

    fn needs_io(&self, h: &HeaderMap) -> bool {
        self.gw.needs_io(h)
    }
}

fn open_paths(gate_storage: bool) -> Vec<String> {
    let mut v: Vec<String> = OPEN_PATHS.iter().map(|s| s.to_string()).collect();
    if !gate_storage {
        v.push("/storage/".into());
    }
    v
}

fn host_of(url: &str) -> &str {
    url.split("//").nth(1).unwrap_or(url).split('/').next().unwrap_or(url)
}

/// axum middleware: `router.layer(from_fn_with_state(gate, x402::gate))`.
pub async fn gate(State(g): State<Arc<HostingGate>>, req: Request, next: Next) -> Response {
    let (parts, body) = req.into_parts();
    let path = parts.uri.path().to_string();
    let query = parts.uri.query().map(String::from);
    let answer = if g.needs_io(&parts.headers) {
        // A payment proof: CoinPay verify + settle is blocking I/O.
        let g2 = g.clone();
        let headers = parts.headers.clone();
        tokio::task::spawn_blocking(move || g2.check(&path, query.as_deref(), &headers))
            .await
            .unwrap_or(None)
    } else {
        g.check(&path, query.as_deref(), &parts.headers)
    };
    match answer {
        Some(r) => r.map(Body::from).into_response(),
        None => next.run(Request::from_parts(parts, body)).await,
    }
}

/// The sales path as its own route, so it answers on every host this gateway
/// serves even where no gated router would see it.
pub fn sales_router(g: Arc<HostingGate>) -> axum::Router {
    let path = g.path.clone();
    axum::Router::new()
        .route(&path, axum::routing::any(sell))
        .with_state(g)
}

async fn sell(State(g): State<Arc<HostingGate>>, req: Request) -> Response {
    let (parts, _) = req.into_parts();
    let path = parts.uri.path().to_string();
    let query = parts.uri.query().map(String::from);
    let headers = parts.headers;
    let g2 = g.clone();
    let r = tokio::task::spawn_blocking(move || g2.check(&path, query.as_deref(), &headers))
        .await
        .ok()
        .flatten();
    match r {
        Some(r) => r.map(Body::from).into_response(),
        None => (axum::http::StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

/// CoinPay over the node's own HTTP stack. Called from a blocking thread
/// (spawn_blocking), so it can wait on the runtime it came from.
struct ReqwestTransport {
    client: reqwest::Client,
    rt: Option<tokio::runtime::Handle>,
}

impl ReqwestTransport {
    fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                .user_agent(concat!("c0mpute/", env!("CARGO_PKG_VERSION"), " x402"))
                .build()
                .unwrap_or_default(),
            rt: tokio::runtime::Handle::try_current().ok(),
        }
    }
}

impl Transport for ReqwestTransport {
    fn post(&self, url: &str, api_key: &str, body: &str) -> (u16, String) {
        let Some(rt) = self.rt.clone().or_else(|| tokio::runtime::Handle::try_current().ok()) else {
            return (0, String::new());
        };
        let req = self
            .client
            .post(url)
            .header("content-type", "application/json")
            .header("x-api-key", api_key)
            .body(body.to_string());
        rt.block_on(async move {
            match req.send().await {
                Ok(r) => {
                    let code = r.status().as_u16();
                    (code, r.text().await.unwrap_or_default())
                }
                Err(_) => (0, String::new()),
            }
        })
    }
}

/// A 32-byte random secret, hex, created once with mode 0600.
fn load_or_create_secret(dir: &Path) -> std::io::Result<String> {
    let path = dir.join("secret");
    if let Ok(s) = std::fs::read_to_string(&path) {
        let s = s.trim().to_string();
        if s.len() >= 32 {
            return Ok(s);
        }
    }
    std::fs::create_dir_all(dir)?;
    let mut bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut bytes);
    let s = hex::encode(bytes);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(&path)?.write_all(s.as_bytes())?;
    Ok(s)
}

fn record_sale(path: &Path, s: &Sale) {
    let line = serde_json::json!({
        "at": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        "payer": s.payer,
        "ref": s.r#ref,
        "user_agent": s.user_agent,
        "days": s.days,
        "price_cents": s.price_cents,
        "total_cents": s.total_cents,
        "currency": s.currency,
        "expires_at": s.expires_at,
    });
    info!(
        payer = ?s.payer,
        days = s.days,
        total_cents = s.total_cents,
        ua = %s.user_agent,
        "x402: sold a crawl pass"
    );
    let res = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| writeln!(f, "{line}"));
    if let Err(e) = res {
        warn!(err = %e, "x402: could not record sale");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate_with(cfg: X402Config) -> HostingGate {
        let gw = Gateway::new(Options {
            site_url: "http://gw.example:7777".into(),
            coinpay_api_key: "k".into(),
            pay_to: "0x0000000000000000000000000000000000000001".into(),
            path: cfg.path.clone(),
            open_paths: open_paths(cfg.gate_storage),
            secret: "s".repeat(64),
            ..Options::default()
        })
        .unwrap();
        HostingGate {
            gw,
            exclude_sites: cfg.exclude_sites,
            exclude_hosts: cfg.exclude_hosts,
            gate_storage: cfg.gate_storage,
            path: cfg.path,
        }
    }

    fn ua(s: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("user-agent", s.parse().unwrap());
        h.insert("host", "gw.example:7777".parse().unwrap());
        h
    }

    const GPTBOT: &str = "Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko; compatible; GPTBot/1.2; +https://openai.com/gptbot)";
    const SITE: &str = "/s/53021d6e8d5a0e1aac48/movie.mp4";

    #[test]
    fn training_crawler_pays_for_hosted_files() {
        let g = gate_with(X402Config::default());
        let r = g.check(SITE, None, &ua(GPTBOT)).expect("gated");
        assert_eq!(r.status(), 402);
    }

    #[test]
    fn people_and_retrieval_agents_pass() {
        let g = gate_with(X402Config::default());
        assert!(g.check(SITE, None, &ua("Mozilla/5.0 (X11; Linux x86_64) Firefox/140.0")).is_none());
        assert!(g.check(SITE, None, &ua("Mozilla/5.0 (compatible; ChatGPT-User/1.0)")).is_none());
    }

    #[test]
    fn network_apis_stay_open() {
        let g = gate_with(X402Config::default());
        for p in ["/healthz", "/hosting/v1/sites", "/p/53021d6e8d5a0e1aac48/", "/storage/v1/objects/x"] {
            assert!(g.check(p, None, &ua(GPTBOT)).is_none(), "{p}");
        }
    }

    #[test]
    fn storage_gated_only_when_asked() {
        let g = gate_with(X402Config { gate_storage: true, ..X402Config::default() });
        assert_eq!(g.check("/storage/v1/objects/x", None, &ua(GPTBOT)).unwrap().status(), 402);
    }

    #[test]
    fn excluded_sites_and_hosts_are_free() {
        let g = gate_with(X402Config {
            exclude_sites: vec!["53021d6e8d5a0e1aac48".into()],
            exclude_hosts: vec!["free.example".into()],
            ..X402Config::default()
        });
        assert!(g.check(SITE, None, &ua(GPTBOT)).is_none());
        let mut h = ua(GPTBOT);
        h.insert("host", "free.example".parse().unwrap());
        assert!(g.check("/index.html", None, &h).is_none());
        // the sales path still answers, excluded or not
        assert_eq!(g.check("/_x402/crawl", None, &h).unwrap().status(), 402);
    }

    #[test]
    fn sales_path_answers_any_client() {
        let g = gate_with(X402Config::default());
        let r = g.check("/_x402/crawl", None, &ua("curl/8.5.0")).expect("sale");
        assert_eq!(r.status(), 402);
    }

    #[test]
    fn secret_persists() {
        let d = tempfile::tempdir().unwrap();
        let a = load_or_create_secret(d.path()).unwrap();
        let b = load_or_create_secret(d.path()).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn config_defaults_off() {
        let c: X402Config = toml::from_str("").unwrap();
        assert!(!c.enabled);
        assert_eq!(c.path, "/_x402/crawl");
        assert_eq!(c.api_key_env, "C0MPUTE_X402_COINPAY_KEY"); // gitleaks:allow
    }
}
