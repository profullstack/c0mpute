//! Your c0mpute account from the terminal, and running a fleet of nodes.
//!
//! ```text
//! c0mpute login                      # device sign-in (approve in a browser: CoinPay, passkey or password)
//! c0mpute whoami | logout
//! c0mpute node link [--token c0e_...] [--label NAME] [--tags a,b]
//! c0mpute node unlink
//! c0mpute fleet ls|show|pause|resume|update|restart|tag|untag|set-tags|rename|unlink|earnings|audit
//! c0mpute fleet token create|ls|revoke
//! ```
//!
//! The account key is the one `c0mpute chat login` always stored
//! (`~/.c0mpute/chat.json`, 0600), so both commands share one sign-in. A linked
//! node keeps `<config dir>/account.json` (0600): the account it is linked to
//! and the c0mpute.com fleet key it pinned, which every fleet command must be
//! signed with (see `fleet_agent`).

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::ai_chat::{api_error, api_key, base_url, client, require_key};

// ── the link record ──────────────────────────────────────────────────────────

/// What a node remembers about the account it is linked to.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct LinkRecord {
    pub site: String,
    pub account_id: String,
    pub peer_id: String,
    /// c0mpute.com's fleet signing key (raw ed25519, base64), pinned at link time.
    pub fleet_key: String,
    pub linked_at: u64,
}

pub fn link_path() -> Option<PathBuf> {
    c0mpute_core::config::config_dir().map(|d| d.join("account.json"))
}

pub fn load_link() -> Option<LinkRecord> {
    let p = link_path()?;
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

fn save_private(path: &std::path::Path, body: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
        f.write_all(body.as_bytes())?;
    }
    #[cfg(not(unix))]
    std::fs::write(path, body)?;
    Ok(())
}

fn save_link(rec: &LinkRecord) -> Result<PathBuf> {
    let p = link_path().ok_or_else(|| anyhow!("no config dir"))?;
    save_private(&p, &serde_json::to_string_pretty(rec)?)?;
    Ok(p)
}

/// The exact text the node signs to link. Must match `linkMessage` in
/// apps/web/src/lib/accounts/peer.ts.
// The format the site produces; the node checks it with `check_link_message`.
#[allow(dead_code)]
pub fn link_message(site: &str, account_id: &str, peer_id: &str, challenge: &str) -> String {
    format!("c0mpute node link v1\nsite: {site}\naccount: {account_id}\nnode: {peer_id}\nchallenge: {challenge}")
}

/// Refuse to sign anything but a link message for this node and this site:
/// a server cannot use `node link` to get some other statement signed.
pub fn check_link_message(msg: &str, site: &str, peer_id: &str) -> Result<()> {
    let lines: Vec<&str> = msg.split('\n').collect();
    let ok = lines.len() == 5
        && lines[0] == "c0mpute node link v1"
        && lines[1] == format!("site: {site}")
        && lines[2].starts_with("account: u_")
        && lines[3] == format!("node: {peer_id}")
        && lines[4].starts_with("challenge: ");
    if !ok {
        bail!("the server asked this node to sign something that is not a link for {peer_id} on {site}; refusing");
    }
    Ok(())
}

fn now_s() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn node_keypair() -> Result<c0mpute_net::libp2p::identity::Keypair> {
    let dir = c0mpute_core::config::config_dir().ok_or_else(|| anyhow!("no config dir"))?;
    c0mpute_net::identity::load_or_create(&dir)
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_default()
}

/// What the node says about itself in the fleet view (never trusted for money).
fn node_info() -> Value {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    sys.refresh_cpu_all();
    let name = c0mpute_core::config::config_dir()
        .and_then(|d| std::fs::read_to_string(d.join("name")).ok())
        .or_else(|| std::env::var("HOME").ok().and_then(|h| std::fs::read_to_string(PathBuf::from(h).join(".c0mpute/name")).ok()))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    json!({
        "hostname": hostname(),
        "version": env!("CARGO_PKG_VERSION"),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "cores": sys.cpus().len(),
        "ram_bytes": sys.total_memory(),
        "name": name,
    })
}

// ── login / whoami ───────────────────────────────────────────────────────────

pub async fn whoami(url_flag: Option<&str>, as_json: bool) -> Result<()> {
    let url = base_url(url_flag);
    let key = require_key().map_err(|_| anyhow!("not signed in: run `c0mpute login`"))?;
    let res = client()?.get(format!("{url}/api/v1/me?nodes=1")).bearer_auth(&key).send().await?;
    let status = res.status();
    let text = res.text().await?;
    if !status.is_success() {
        return Err(api_error(status, &text));
    }
    let v: Value = serde_json::from_str(&text)?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    let name = v["name"].as_str().unwrap_or("(no name)");
    println!("{name}  account {}", v["id"].as_str().unwrap_or("?"));
    match v["did"].as_str() {
        Some(did) => println!("CoinPay: connected  DID {did}"),
        None => println!("CoinPay: not connected (connect it at {url}/account to get paid)"),
    }
    if let Some(b) = v.get("balance").filter(|b| !b.is_null()) {
        println!("balance: ${} spendable", b["spendable_usd"].as_str().unwrap_or("0"));
    }
    let nodes = v["nodes"].as_array().cloned().unwrap_or_default();
    if nodes.is_empty() {
        println!("nodes: none linked (run `c0mpute node link` on a node)");
    } else {
        let t = &v["totals"];
        println!("nodes: {} ({} online, {} verified)", t["nodes"], t["online"], t["verified"]);
        for n in nodes.iter().take(20) {
            println!(
                "  {} {:8} {:18} {} {}",
                if n["online"].as_bool() == Some(true) { "●" } else { "○" },
                n["node"].as_str().unwrap_or(""),
                n["label"].as_str().unwrap_or(""),
                n["version"].as_str().unwrap_or("?"),
                if n["verified"].as_bool() == Some(true) { "" } else { "(unverified)" }
            );
        }
    }
    if let Some(l) = load_link() {
        println!("this node: {} linked to account {}", &l.peer_id[l.peer_id.len().saturating_sub(8)..], l.account_id);
    }
    Ok(())
}

// ── node link ────────────────────────────────────────────────────────────────

#[derive(Subcommand, Debug, Clone)]
pub enum NodeCmd {
    /// Link this node to your account: it signs a one-shot challenge with its
    /// identity key. Uses your `c0mpute login` key, or --token (an enrollment
    /// token from `c0mpute fleet token create`, for headless installs).
    Link(LinkArgs),
    /// Forget the link on this node (stop taking fleet commands). Remove it
    /// from the account with `c0mpute fleet unlink <node>`.
    Unlink,
    /// This node's link: the account and the pinned fleet key.
    Status {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args, Debug, Clone)]
pub struct LinkArgs {
    /// A fleet enrollment token (c0e_...) instead of your login key. Also $C0MPUTE_ENROLL.
    #[arg(long)]
    pub token: Option<String>,
    /// A name for this node in your fleet (default: its nickname or hostname).
    #[arg(long)]
    pub label: Option<String>,
    /// Tags, comma separated (e.g. vast,rack-a).
    #[arg(long)]
    pub tags: Option<String>,
    #[arg(long)]
    pub url: Option<String>,
    #[arg(long)]
    pub json: bool,
}

pub async fn node(cmd: NodeCmd) -> Result<()> {
    match cmd {
        NodeCmd::Link(a) => link(a).await,
        NodeCmd::Unlink => {
            match link_path() {
                Some(p) if p.exists() => {
                    std::fs::remove_file(&p)?;
                    println!("this node no longer takes fleet commands (removed {}). Remove it from the account with `c0mpute fleet unlink <node>`.", p.display());
                }
                _ => println!("this node is not linked"),
            }
            Ok(())
        }
        NodeCmd::Status { json } => {
            let peer = node_keypair()?.public().to_peer_id().to_string();
            match load_link() {
                Some(l) if json => println!("{}", serde_json::to_string_pretty(&l)?),
                Some(l) => println!("node {peer}\nlinked to account {} on {} (since {})\nfleet key {}", l.account_id, l.site, l.linked_at, l.fleet_key),
                None if json => println!("{}", json!({"peer_id": peer, "linked": false})),
                None => println!("node {peer}\nnot linked: run `c0mpute node link`"),
            }
            Ok(())
        }
    }
}

async fn link(a: LinkArgs) -> Result<()> {
    let url = base_url(a.url.as_deref());
    let token = a.token.clone().or_else(|| std::env::var("C0MPUTE_ENROLL").ok().filter(|s| !s.is_empty()));
    let bearer = match &token {
        Some(t) if t.starts_with("c0e_") => t.clone(),
        Some(_) => bail!("--token: an enrollment token starts with c0e_ (make one with `c0mpute fleet token create`)"),
        None => api_key().ok_or_else(|| anyhow!("not signed in: run `c0mpute login` first, or pass --token c0e_..."))?,
    };
    let kp = node_keypair()?;
    let peer_id = kp.public().to_peer_id().to_string();
    let http = client()?;
    let res = http
        .post(format!("{url}/api/v1/nodes/link/challenge"))
        .bearer_auth(&bearer)
        .json(&json!({"peer_id": peer_id}))
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .with_context(|| format!("could not reach {url}"))?;
    let status = res.status();
    let text = res.text().await?;
    if !status.is_success() {
        return Err(api_error(status, &text));
    }
    let ch: Value = serde_json::from_str(&text)?;
    let message = ch["message"].as_str().context("no message in the challenge")?;
    check_link_message(message, &url, &peer_id)?;
    let sig = kp.sign(message.as_bytes()).map_err(|e| anyhow!("sign: {e}"))?;
    let tags: Vec<String> = a.tags.as_deref().unwrap_or("").split(',').map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect();
    let res = http
        .post(format!("{url}/api/v1/nodes/link"))
        .bearer_auth(&bearer)
        .json(&json!({
            "challenge_id": ch["challenge_id"],
            "peer_id": peer_id,
            "signature": b64(&sig),
            "info": node_info(),
            "label": a.label,
            "tags": tags,
        }))
        .timeout(Duration::from_secs(20))
        .send()
        .await?;
    let status = res.status();
    let text = res.text().await?;
    if !status.is_success() {
        return Err(api_error(status, &text));
    }
    let v: Value = serde_json::from_str(&text)?;
    let rec = LinkRecord {
        site: url.clone(),
        account_id: v["account_id"].as_str().context("no account_id")?.to_string(),
        peer_id: peer_id.clone(),
        fleet_key: v["fleet_key"].as_str().context("no fleet_key")?.to_string(),
        linked_at: now_s(),
    };
    let path = save_link(&rec)?;
    if a.json {
        println!("{}", serde_json::to_string_pretty(&json!({"linked": true, "node": v["node"], "account_id": rec.account_id, "record": path}))?);
    } else {
        println!(
            "linked node {} ({}) to account {}\nverified with its identity key; it now shows on {url}/dashboard/nodes and takes fleet commands signed by {url}.\nlink saved to {}",
            &peer_id[peer_id.len() - 8..],
            peer_id,
            rec.account_id,
            path.display()
        );
        println!("A running worker picks this up within a minute; no restart needed.");
    }
    Ok(())
}

// ── fleet ────────────────────────────────────────────────────────────────────

#[derive(Args, Debug, Clone, Default)]
pub struct Target {
    /// Node ids (the 8-character id on /nodes, or a full peer id).
    pub nodes: Vec<String>,
    /// Every node with this tag.
    #[arg(long)]
    pub tag: Option<String>,
    /// Every node on the account.
    #[arg(long)]
    pub all: bool,
}

#[derive(Subcommand, Debug, Clone)]
pub enum FleetCmd {
    /// Every node: status, last seen, version, hardware, jobs, earnings, tags.
    Ls {
        #[arg(long)]
        tag: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// One node in full.
    Show {
        node: String,
        #[arg(long)]
        json: bool,
    },
    /// Stop bidding on new jobs (running jobs finish). Queued; runs on the node's next poll.
    Pause(TargetArgs),
    /// Start bidding again.
    Resume(TargetArgs),
    /// Install the latest release and restart into it.
    Update(TargetArgs),
    /// Restart the worker (finishes running jobs first).
    Restart(TargetArgs),
    /// Chat models to serve: `c0mpute fleet models <node...|--tag T|--all> --set qwen3:4b,llama3:8b` (`--clear`: every model the node has).
    Models(ModelsArgs),
    /// Serve the network's house models: `c0mpute fleet house-models off <node...|--tag T|--all>`.
    HouseModels(ToggleArgs),
    /// Serve uncensored / abliterated models: `c0mpute fleet uncensored off ...`.
    Uncensored(ToggleArgs),
    /// Hold sites for the network (the node restarts after running jobs finish).
    Hosting(ToggleArgs),
    /// Store shards for the network (the node restarts after running jobs finish).
    Storage(ToggleArgs),
    /// Alerts by email (and SMS for Profullstack admin accounts): node offline, earnings drop, update failed, disk low.
    Alerts {
        #[command(subcommand)]
        cmd: AlertsCmd,
    },
    /// Add tags: `c0mpute fleet tag <node...|--tag T|--all> --set vast,home`.
    Tag(TagArgs),
    /// Remove tags.
    Untag(TagArgs),
    /// Replace tags.
    SetTags(TagArgs),
    /// Name a node in your fleet.
    Rename {
        node: String,
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Remove nodes from the account.
    Unlink(TargetArgs),
    /// Earnings per node and in total (24h, 30d, all).
    Earnings {
        #[arg(long)]
        tag: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Who did what to which node, and every command with its outcome.
    Audit {
        #[arg(long, default_value_t = 50)]
        limit: u32,
        #[arg(long)]
        json: bool,
    },
    /// Enrollment tokens: link many boxes without a browser.
    Token {
        #[command(subcommand)]
        cmd: TokenCmd,
    },
}

#[derive(Args, Debug, Clone)]
pub struct TargetArgs {
    #[command(flatten)]
    pub target: Target,
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum OnOff {
    On,
    Off,
    /// Hand the setting back to the node's own config.
    Default,
}

#[derive(Args, Debug, Clone)]
pub struct ToggleArgs {
    /// on, off, or default (the node's own config decides).
    #[arg(value_enum)]
    pub state: OnOff,
    #[command(flatten)]
    pub target: Target,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug, Clone)]
pub struct ModelsArgs {
    #[command(flatten)]
    pub target: Target,
    /// The models, comma separated (exact ids as `/v1/models` lists them).
    #[arg(long = "set", value_name = "MODELS", conflicts_with = "clear")]
    pub models: Option<String>,
    /// Serve every model the node has again.
    #[arg(long)]
    pub clear: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Subcommand, Debug, Clone)]
pub enum AlertsCmd {
    /// Current settings, open alerts and recent notices.
    Get {
        #[arg(long)]
        json: bool,
    },
    /// Change settings: `c0mpute fleet alerts set --email me@example.com --offline-minutes 15 --off earnings`.
    Set(AlertsSetArgs),
    /// Send a test notice to the alert email (and SMS when on).
    Test,
}

#[derive(Args, Debug, Clone, Default)]
pub struct AlertsSetArgs {
    /// Where alerts go (a confirmation link is sent first). `--email none` removes it.
    #[arg(long)]
    pub email: Option<String>,
    /// A node counts as offline after this many minutes without being heard (default 15).
    #[arg(long)]
    pub offline_minutes: Option<u32>,
    /// Alert when the last 24h earned this much less (%) than the trailing 7-day daily average (default 50).
    #[arg(long)]
    pub earnings_drop_pct: Option<u32>,
    /// Ignore earnings drops while the 7-day daily average is under this (USD, default 0.05).
    #[arg(long)]
    pub earnings_min_usd: Option<f64>,
    /// Disk low under this many GB free (0 = off; default 10).
    #[arg(long)]
    pub disk_min_gb: Option<f64>,
    /// Disk low under this % free (0 = off; default 5).
    #[arg(long)]
    pub disk_min_pct: Option<f64>,
    /// After a recovery, a new alert for the same thing waits this long (default 60).
    #[arg(long)]
    pub cooldown_minutes: Option<u32>,
    /// Turn alert kinds on: offline,earnings,update,disk.
    #[arg(long)]
    pub on: Option<String>,
    /// Turn alert kinds off.
    #[arg(long)]
    pub off: Option<String>,
    /// SMS through the fleet alert line (Profullstack admin accounts only).
    #[arg(long, value_enum)]
    pub sms: Option<OnOff>,
    #[arg(long)]
    pub json: bool,
}

/// The JSON body for PUT /api/v1/fleet/alerts.
pub fn alerts_body(a: &AlertsSetArgs) -> Result<Value> {
    let mut b = json!({});
    if let Some(e) = &a.email {
        b["email"] = if e.eq_ignore_ascii_case("none") || e.is_empty() { Value::Null } else { json!(e) };
    }
    let mut num = |k: &str, v: Option<f64>| {
        if let Some(v) = v {
            b[k] = json!(v);
        }
    };
    num("offline_minutes", a.offline_minutes.map(f64::from));
    num("earnings_drop_pct", a.earnings_drop_pct.map(f64::from));
    num("earnings_min_usd", a.earnings_min_usd);
    num("disk_min_gb", a.disk_min_gb);
    num("disk_min_pct", a.disk_min_pct);
    num("cooldown_minutes", a.cooldown_minutes.map(f64::from));
    let kinds = ["offline", "earnings", "update", "disk"];
    let mut k = json!({});
    for (list, on) in [(&a.on, true), (&a.off, false)] {
        for w in list.iter().flat_map(|l| l.split(',')).map(str::trim).filter(|w| !w.is_empty()) {
            if !kinds.contains(&w) {
                bail!("alert kinds: offline, earnings, update, disk (not {w})");
            }
            k[w] = json!(on);
        }
    }
    if k.as_object().is_some_and(|o| !o.is_empty()) {
        b["kinds"] = k;
    }
    match a.sms {
        Some(OnOff::On) => b["sms"] = json!(true),
        Some(OnOff::Off) | Some(OnOff::Default) => b["sms"] = json!(false),
        None => {}
    }
    if b.as_object().is_some_and(|o| o.is_empty()) {
        bail!("nothing to change: see `c0mpute fleet alerts set --help`");
    }
    Ok(b)
}

fn toggle_args(s: OnOff) -> Value {
    match s {
        OnOff::On => json!({"on": true}),
        OnOff::Off => json!({"on": false}),
        OnOff::Default => json!({"on": null}),
    }
}

/// `--set a,b` as a list; `--clear` (or nothing) as an empty list = every model.
pub fn models_args(a: &ModelsArgs) -> Result<Value> {
    if a.models.is_none() && !a.clear {
        bail!("say which models: --set qwen3:4b,llama3:8b (or --clear for every model)");
    }
    let list: Vec<String> = a.models.iter().flat_map(|m| m.split(',')).map(|m| m.trim().to_string()).filter(|m| !m.is_empty()).collect();
    Ok(json!({ "models": list }))
}

fn print_alerts(v: &Value) {
    let s = &v["settings"];
    let email = match (s["email"].as_str(), s["email_verified"].as_bool()) {
        (Some(e), Some(true)) => e.to_string(),
        (Some(e), _) => format!("{e} (not confirmed yet: open the link we emailed)"),
        _ => "none (set one: c0mpute fleet alerts set --email you@example.com)".into(),
    };
    println!("email: {email}");
    if s["sms_available"].as_bool() == Some(true) {
        println!("sms: {}", if s["sms"].as_bool() == Some(true) { "on" } else { "off" });
    }
    let k = &s["kinds"];
    let on = |n: &str| if k[n].as_bool() == Some(false) { "off" } else { "on" };
    println!("offline: {} after {} min", on("offline"), s["offline_minutes"]);
    println!("earnings drop: {} at {}% under the 7-day daily average (ignored under ${}/day)", on("earnings"), s["earnings_drop_pct"], s["earnings_min_usd"]);
    println!("update failed: {}", on("update"));
    println!("disk low: {} under {} GB or {}% free", on("disk"), s["disk_min_gb"], s["disk_min_pct"]);
    println!("cooldown: {} min", s["cooldown_minutes"]);
    let active = v["active"].as_array().cloned().unwrap_or_default();
    println!("open alerts: {}", if active.is_empty() { "none".to_string() } else { active.len().to_string() });
    for a in active {
        println!("  {:9} {:8} since {}  {}", a["kind"].as_str().unwrap_or(""), a["node"].as_str().unwrap_or(""), a["since"].as_str().unwrap_or("").get(..16).unwrap_or(""), a["detail"].as_str().unwrap_or(""));
    }
    for l in v["log"].as_array().cloned().unwrap_or_default().into_iter().take(10) {
        println!("  {}  {:8} {:9} {:8} {}", l["at"].as_str().unwrap_or("").get(..16).unwrap_or(""), l["event"].as_str().unwrap_or(""), l["kind"].as_str().unwrap_or(""), l["node"].as_str().unwrap_or(""), l["detail"].as_str().unwrap_or(""));
    }
}

#[derive(Args, Debug, Clone)]
pub struct TagArgs {
    #[command(flatten)]
    pub target: Target,
    /// The tags, comma separated.
    #[arg(long = "set", value_name = "TAGS")]
    pub tags: String,
    #[arg(long)]
    pub json: bool,
}

#[derive(Subcommand, Debug, Clone)]
pub enum TokenCmd {
    /// A new token (shown once): `--name rack-a --expires 7d [--tags rack-a] [--max-uses 50]`.
    Create {
        #[arg(long, default_value = "fleet")]
        name: String,
        /// 30m, 12h, 7d (at most 366d), or never.
        #[arg(long, default_value = "7d")]
        expires: String,
        #[arg(long)]
        tags: Option<String>,
        #[arg(long)]
        max_uses: Option<u32>,
        #[arg(long)]
        json: bool,
    },
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// Revoke by id or name. Nodes it linked stay linked.
    Revoke { id: String },
}

async fn api(method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value> {
    let url = base_url(None);
    let key = require_key().map_err(|_| anyhow!("not signed in: run `c0mpute login`"))?;
    let mut req = client()?.request(method, format!("{url}{path}")).bearer_auth(key).timeout(Duration::from_secs(30));
    if let Some(b) = body {
        req = req.json(&b);
    }
    let res = req.send().await.with_context(|| format!("could not reach {url}"))?;
    let status = res.status();
    let text = res.text().await?;
    if !status.is_success() && status.as_u16() != 409 {
        return Err(api_error(status, &text));
    }
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if let Some(e) = v.get("error").filter(|e| !e.is_null()) {
        bail!("{}", e["message"].as_str().unwrap_or("request failed"));
    }
    Ok(v)
}

fn usd(v: &Value) -> String {
    format!("${:.4}", v.as_f64().unwrap_or(0.0))
}

fn ago(v: &Value) -> String {
    match v.as_u64() {
        None => "-".into(),
        Some(s) if s < 90 => format!("{s}s"),
        Some(s) if s < 5400 => format!("{}m", s / 60),
        Some(s) => format!("{}h", s / 3600),
    }
}

fn selection(t: &Target) -> Result<Value> {
    if t.all {
        return Ok(json!({"all": true}));
    }
    if let Some(tag) = &t.tag {
        return Ok(json!({"tag": tag}));
    }
    if t.nodes.is_empty() {
        bail!("say which nodes: <node...>, --tag <tag> or --all");
    }
    Ok(json!({"nodes": t.nodes}))
}

async fn action(name: &str, t: &Target, args: Value, as_json: bool) -> Result<()> {
    let mut body = selection(t)?;
    body["action"] = json!(name);
    body["args"] = args;
    let v = api(reqwest::Method::POST, "/api/v1/fleet/actions", Some(body)).await?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    for r in v["results"].as_array().cloned().unwrap_or_default() {
        println!("{} {}  {}", if r["ok"].as_bool() == Some(true) { "ok " } else { "-- " }, r["node"].as_str().unwrap_or(""), r["detail"].as_str().unwrap_or(""));
    }
    if matches!(name, "pause" | "resume" | "update" | "restart" | "models" | "house-models" | "uncensored" | "hosting" | "storage") {
        println!("queued: each node runs it on its next poll (about a minute); follow with `c0mpute fleet audit`.");
    }
    Ok(())
}

fn print_node(n: &Value) {
    let ram = n["ram_bytes"].as_f64().map(|b| format!("{:.0}G", b / 1e9)).unwrap_or_else(|| "-".into());
    println!(
        "{} {:8} {:16} {:8} {:>4} {:9} {:>4}c {:>5} jobs24h {:>4}  24h {:>9}  30d {:>9}  all {:>9}  {}{}{}",
        if n["online"].as_bool() == Some(true) { "●" } else { "○" },
        n["node"].as_str().unwrap_or(""),
        n["label"].as_str().unwrap_or("").chars().take(16).collect::<String>(),
        n["version"].as_str().unwrap_or("-"),
        ago(&n["last_seen_s"]),
        n["class"].as_str().unwrap_or("-"),
        n["cores"].as_u64().map(|c| c.to_string()).unwrap_or_else(|| "-".into()),
        ram,
        n["jobs_24h"].as_u64().unwrap_or(0),
        usd(&n["earned_usd"]["h24"]),
        usd(&n["earned_usd"]["d30"]),
        usd(&n["earned_usd"]["all"]),
        n["tags"].as_array().map(|t| t.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(",")).unwrap_or_default(),
        if n["verified"].as_bool() == Some(true) { "" } else { " (unverified)" },
        if n["paused"].as_bool() == Some(true) { " PAUSED" } else { "" },
    );
    if let Some(line) = settings_line(&n["node_state"]) {
        println!("    {line}");
    }
}

/// What a node reported about its settings, in one line (None before it reported).
pub fn settings_line(st: &Value) -> Option<String> {
    if !st.is_object() {
        return None;
    }
    let e = &st["effective"];
    let onoff = |v: &Value| match v.as_bool() {
        Some(true) => "on",
        Some(false) => "off",
        None => "-",
    };
    let served = e["served_models"].as_array().map(|a| a.len()).unwrap_or(0);
    let allow = st["settings"]["models"].as_array().map(|a| format!(" (allowlist {})", a.len())).unwrap_or_default();
    let disk = st["disk"]["free_bytes"].as_f64().map(|f| format!("  disk {:.0} GB free", f / 1e9)).unwrap_or_default();
    let upd = st["update_error"]["error"].as_str().map(|e| format!("  UPDATE FAILED: {}", e.chars().take(80).collect::<String>())).unwrap_or_default();
    Some(format!(
        "models {served}{allow}  house {}  uncensored {}  hosting {}  storage {}{disk}{upd}",
        onoff(&e["house_models"]),
        onoff(&e["uncensored"]),
        onoff(&e["hosting"]),
        onoff(&e["storage"])
    ))
}

pub async fn fleet(cmd: FleetCmd) -> Result<()> {
    use reqwest::Method;
    match cmd {
        FleetCmd::Ls { tag, json } => {
            let path = match &tag {
                Some(t) => format!("/api/v1/fleet?tag={}", urlencode(t)),
                None => "/api/v1/fleet".into(),
            };
            let v = api(Method::GET, &path, None).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            let t = &v["totals"];
            println!(
                "{} nodes, {} online, {} verified, {} jobs 24h; earned 24h {} 30d {} all {} (buyer-agreed, before the 5% fee)",
                t["nodes"], t["online"], t["verified"], t["jobs_24h"], usd(&t["earned_usd"]["h24"]), usd(&t["earned_usd"]["d30"]), usd(&t["earned_usd"]["all"])
            );
            if v["payouts_ready"].as_bool() != Some(true) {
                println!("payouts: connect CoinPay at {}/account to get paid", base_url(None));
            }
            for n in v["nodes"].as_array().cloned().unwrap_or_default() {
                print_node(&n);
            }
            Ok(())
        }
        FleetCmd::Show { node, json } => {
            let v = api(Method::GET, "/api/v1/fleet", None).await?;
            let key = node.trim().chars().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect::<String>();
            let n = v["nodes"].as_array().and_then(|a| a.iter().find(|n| n["node"].as_str() == Some(key.as_str())).cloned()).ok_or_else(|| anyhow!("not one of your nodes: {node}"))?;
            println!("{}", if json { serde_json::to_string_pretty(&n)? } else { serde_json::to_string_pretty(&n)? });
            Ok(())
        }
        FleetCmd::Pause(a) => action("pause", &a.target, json!({}), a.json).await,
        FleetCmd::Resume(a) => action("resume", &a.target, json!({}), a.json).await,
        FleetCmd::Update(a) => action("update", &a.target, json!({}), a.json).await,
        FleetCmd::Restart(a) => action("restart", &a.target, json!({}), a.json).await,
        FleetCmd::Models(a) => action("models", &a.target, models_args(&a)?, a.json).await,
        FleetCmd::HouseModels(a) => action("house-models", &a.target, toggle_args(a.state), a.json).await,
        FleetCmd::Uncensored(a) => action("uncensored", &a.target, toggle_args(a.state), a.json).await,
        FleetCmd::Hosting(a) => action("hosting", &a.target, toggle_args(a.state), a.json).await,
        FleetCmd::Storage(a) => action("storage", &a.target, toggle_args(a.state), a.json).await,
        FleetCmd::Alerts { cmd } => match cmd {
            AlertsCmd::Get { json } => {
                let v = api(Method::GET, "/api/v1/fleet/alerts", None).await?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&v)?);
                } else {
                    print_alerts(&v);
                }
                Ok(())
            }
            AlertsCmd::Set(a) => {
                let v = api(Method::PUT, "/api/v1/fleet/alerts", Some(alerts_body(&a)?)).await?;
                if a.json {
                    println!("{}", serde_json::to_string_pretty(&v)?);
                } else {
                    if let Some(n) = v["notice"].as_str() {
                        println!("{n}");
                    }
                    print_alerts(&v);
                }
                Ok(())
            }
            AlertsCmd::Test => {
                let v = api(Method::POST, "/api/v1/fleet/alerts", Some(json!({"test": true}))).await?;
                println!("{}", v["detail"].as_str().unwrap_or("sent"));
                Ok(())
            }
        },
        FleetCmd::Unlink(a) => action("unlink", &a.target, json!({}), a.json).await,
        FleetCmd::Tag(a) => action("tag", &a.target, json!({"tags": a.tags}), a.json).await,
        FleetCmd::Untag(a) => action("untag", &a.target, json!({"tags": a.tags}), a.json).await,
        FleetCmd::SetTags(a) => action("set-tags", &a.target, json!({"tags": a.tags}), a.json).await,
        FleetCmd::Rename { node, name, json } => action("rename", &Target { nodes: vec![node], ..Default::default() }, json!({"label": name}), json).await,
        FleetCmd::Earnings { tag, json } => {
            let path = match &tag {
                Some(t) => format!("/api/v1/fleet?tag={}", urlencode(t)),
                None => "/api/v1/fleet".into(),
            };
            let v = api(Method::GET, &path, None).await?;
            let rows: Vec<Value> = v["nodes"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|n| json!({"node": n["node"], "label": n["label"], "earned_usd": n["earned_usd"], "jobs_24h": n["jobs_24h"]}))
                .collect();
            if json {
                println!("{}", serde_json::to_string_pretty(&json!({"totals": v["totals"]["earned_usd"], "payouts_ready": v["payouts_ready"], "nodes": rows}))?);
                return Ok(());
            }
            let t = &v["totals"]["earned_usd"];
            println!("fleet: 24h {}  30d {}  all {}  (buyer-agreed, before the 5% fee)", usd(&t["h24"]), usd(&t["d30"]), usd(&t["all"]));
            println!("{}", if v["payouts_ready"].as_bool() == Some(true) { "payouts: to your CoinPay account".to_string() } else { format!("payouts: not set up; connect CoinPay at {}/account", base_url(None)) });
            for r in rows {
                println!("  {:8} {:18} 24h {:>9}  30d {:>9}  all {:>9}", r["node"].as_str().unwrap_or(""), r["label"].as_str().unwrap_or(""), usd(&r["earned_usd"]["h24"]), usd(&r["earned_usd"]["d30"]), usd(&r["earned_usd"]["all"]));
            }
            Ok(())
        }
        FleetCmd::Audit { limit, json } => {
            let v = api(Method::GET, &format!("/api/v1/fleet/audit?limit={limit}"), None).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            for e in v["log"].as_array().cloned().unwrap_or_default() {
                println!(
                    "{}  {:8}  {:14} by {:8} {}",
                    e["at"].as_str().unwrap_or("").get(..19).unwrap_or(""),
                    e["node"].as_str().unwrap_or("account"),
                    e["action"].as_str().unwrap_or(""),
                    e["actor"].as_str().unwrap_or(""),
                    e["outcome"].as_str().unwrap_or("")
                );
            }
            Ok(())
        }
        FleetCmd::Token { cmd } => match cmd {
            TokenCmd::Create { name, expires, tags, max_uses, json } => {
                let mut body = json!({"name": name, "expires": expires});
                if let Some(t) = tags {
                    body["tags"] = json!(t);
                }
                if let Some(m) = max_uses {
                    body["max_uses"] = json!(m);
                }
                let v = api(Method::POST, "/api/v1/fleet/tokens", Some(body)).await?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&v)?);
                } else {
                    println!("{}\n\nShown once. On each box:\n  {}\nor, already installed:\n  {}", v["token"].as_str().unwrap_or(""), v["install"].as_str().unwrap_or(""), v["link"].as_str().unwrap_or(""));
                    println!("expires {}", v["expires_at"].as_str().unwrap_or("never"));
                }
                Ok(())
            }
            TokenCmd::Ls { json } => {
                let v = api(Method::GET, "/api/v1/fleet/tokens", None).await?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&v)?);
                    return Ok(());
                }
                for t in v["tokens"].as_array().cloned().unwrap_or_default() {
                    println!(
                        "{}  {:12} {}…  used {}{}  {}",
                        t["id"].as_str().unwrap_or(""),
                        t["name"].as_str().unwrap_or(""),
                        t["prefix"].as_str().unwrap_or(""),
                        t["uses"],
                        t["max_uses"].as_u64().map(|m| format!("/{m}")).unwrap_or_default(),
                        if t["revoked_at"].is_string() { "revoked".to_string() } else { t["expires_at"].as_str().map(|e| format!("expires {e}")).unwrap_or_else(|| "no expiry".into()) }
                    );
                }
                Ok(())
            }
            TokenCmd::Revoke { id } => {
                let v = api(Method::DELETE, &format!("/api/v1/fleet/tokens?id={}", urlencode(&id)), None).await?;
                println!("revoked {}", v["revoked"]);
                Ok(())
            }
        },
    }
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_message_matches_the_site() {
        assert_eq!(
            link_message("https://c0mpute.com", "u_1", "12D3KooWx", "abc"),
            "c0mpute node link v1\nsite: https://c0mpute.com\naccount: u_1\nnode: 12D3KooWx\nchallenge: abc"
        );
    }

    #[test]
    fn only_link_messages_for_this_node_and_site_are_signed() {
        let ok = link_message("https://c0mpute.com", "u_1", "12D3KooWx", "abc");
        assert!(check_link_message(&ok, "https://c0mpute.com", "12D3KooWx").is_ok());
        assert!(check_link_message(&ok, "https://evil.example", "12D3KooWx").is_err());
        assert!(check_link_message(&ok, "https://c0mpute.com", "12D3KooWy").is_err());
        assert!(check_link_message("c0mpute node poll v1\nsite: https://c0mpute.com\nnode: 12D3KooWx\nts: 1", "https://c0mpute.com", "12D3KooWx").is_err());
        assert!(check_link_message(&format!("{ok}\nextra: 1"), "https://c0mpute.com", "12D3KooWx").is_err());
    }

    #[test]
    fn settings_and_alert_bodies() {
        let m = ModelsArgs { target: Target::default(), models: Some("qwen3:4b, llama3:8b,".into()), clear: false, json: false };
        assert_eq!(models_args(&m).unwrap(), json!({"models": ["qwen3:4b", "llama3:8b"]}));
        let c = ModelsArgs { target: Target::default(), models: None, clear: true, json: false };
        assert_eq!(models_args(&c).unwrap(), json!({"models": []}));
        let none = ModelsArgs { target: Target::default(), models: None, clear: false, json: false };
        assert!(models_args(&none).is_err());
        assert_eq!(toggle_args(OnOff::Off), json!({"on": false}));
        assert_eq!(toggle_args(OnOff::Default), json!({"on": null}));
        let a = AlertsSetArgs { email: Some("ops@example.com".into()), offline_minutes: Some(10), off: Some("earnings, disk".into()), on: Some("offline".into()), sms: Some(OnOff::On), ..Default::default() };
        assert_eq!(
            alerts_body(&a).unwrap(),
            json!({"email": "ops@example.com", "offline_minutes": 10.0, "kinds": {"offline": true, "earnings": false, "disk": false}, "sms": true})
        );
        assert_eq!(alerts_body(&AlertsSetArgs { email: Some("none".into()), ..Default::default() }).unwrap(), json!({"email": null}));
        assert!(alerts_body(&AlertsSetArgs { on: Some("weather".into()), ..Default::default() }).is_err());
        assert!(alerts_body(&AlertsSetArgs::default()).is_err());
        let st = json!({"effective": {"served_models": ["a", "b"], "house_models": true, "uncensored": false, "hosting": false, "storage": true}, "settings": {"models": ["a", "b"]}, "disk": {"free_bytes": 50e9}});
        assert_eq!(settings_line(&st).unwrap(), "models 2 (allowlist 2)  house on  uncensored off  hosting off  storage on  disk 50 GB free");
        assert_eq!(settings_line(&Value::Null), None);
    }

    #[test]
    fn selections() {
        assert_eq!(selection(&Target { all: true, ..Default::default() }).unwrap(), json!({"all": true}));
        assert_eq!(selection(&Target { tag: Some("vast".into()), ..Default::default() }).unwrap(), json!({"tag": "vast"}));
        assert!(selection(&Target::default()).is_err());
    }
}
