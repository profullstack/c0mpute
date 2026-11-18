//! `c0mpute autoscale`: stop a RENTED node while nobody needs it, start it as
//! soon as somebody does.
//!
//! Opt-in, for nodes you rent by the hour (Vast.ai, SaladCloud; Thunder
//! Compute is status-only because it has no native stop). A box you own saves
//! nothing by stopping, so leave it out. Runs on any always-on machine you own,
//! never on the rented box itself (a stopped box cannot start itself). The
//! provider API key stays on that machine: it is read from a file or an
//! environment variable there and is only ever sent to the provider.
//!
//! The network supplies both signals, through c0mpute.com's
//! `GET /api/v1/autoscale/signal` (the status aggregators' `GET /autoscale`):
//!
//! - **demand**: auctions on the mesh for the node's models (waiting, in the
//!   last two minutes, found no taker);
//! - **activity**: when the node last won and finished a job, jobs in flight,
//!   whether it is online and which models its ad says it serves.
//!
//! Policy per node (defaults): stop after `idle_minutes` (20) with no job for
//! the node, no auction for its models in the last two minutes, nothing in
//! flight, `busy_cmd` (if set) not busy, and at least `min_run_minutes` (15)
//! since it started. Start at once when an auction for one of its models is
//! waiting or was published in the last two minutes. A start that is not
//! serving within `boot_timeout_minutes` (8) is an error, or on Vast with
//! `[node.replace]`, rents a replacement within the price cap, waits for it to
//! serve the same node id, then destroys the dead instance.
//!
//! ```text
//! c0mpute autoscale init                 # example ~/.config/c0mpute/autoscale.toml (0600)
//! c0mpute autoscale run                  # the loop (a systemd --user service)
//! c0mpute autoscale once [--dry-run]     # one pass, print what it would do
//! c0mpute autoscale status [--json]
//! c0mpute autoscale start|stop <name>    # by hand (the loop still applies its policy after)
//! c0mpute autoscale reset <name>         # clear an error so the loop acts on it again
//! ```

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::ai_chat::{base_url, client};

// ─── config ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Send each node's state to your c0mpute.com dashboard (needs
    /// `C0MPUTE_API_KEY` or `c0mpute chat login`). Never sends provider keys.
    #[serde(default = "yes")]
    pub report: bool,
    /// c0mpute site (default https://c0mpute.com or $C0MPUTE_URL).
    #[serde(default)]
    pub site: Option<String>,
    /// Seconds between passes.
    #[serde(default = "default_interval")]
    pub interval_s: u64,
    /// Also append every transition here (journald gets them on stdout).
    #[serde(default)]
    pub log_file: Option<String>,
    #[serde(default, rename = "node")]
    pub nodes: Vec<NodeCfg>,
}

fn yes() -> bool {
    true
}
fn default_interval() -> u64 {
    60
}
fn d_idle() -> u64 {
    20
}
fn d_min_run() -> u64 {
    15
}
fn d_boot() -> u64 {
    8
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NodeCfg {
    pub name: String,
    /// vast | salad | thunder
    pub provider: String,
    /// The provider's instance id (Salad: the container group name).
    pub instance: String,
    /// The c0mpute node: its peer id or the 8-character id on /nodes.
    pub node: String,
    /// Models it serves; demand for these starts it.
    pub models: Vec<String>,
    #[serde(default)]
    pub api_key_file: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default = "d_idle")]
    pub idle_minutes: u64,
    #[serde(default = "d_min_run")]
    pub min_run_minutes: u64,
    #[serde(default = "d_boot")]
    pub boot_timeout_minutes: u64,
    /// Shell command: exit 1 means idle; exit 0 (busy) or anything else (the
    /// check itself failed) keeps the node running.
    #[serde(default)]
    pub busy_cmd: Option<String>,
    /// Shell command run right before a stop (e.g. stop the worker so it
    /// takes no new job while the provider powers down).
    #[serde(default)]
    pub pre_stop_cmd: Option<String>,
    /// $/h while running / stopped, when the provider does not say.
    #[serde(default)]
    pub usd_per_hour: Option<f64>,
    #[serde(default)]
    pub stopped_usd_per_hour: Option<f64>,
    /// SaladCloud organization and project.
    #[serde(default)]
    pub org: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Watch and log what it would do, never call start/stop/rent (a node
    /// you want up 24/7 but still want the numbers for).
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub replace: Option<ReplaceCfg>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReplaceCfg {
    /// Hard price cap per hour for a replacement (all-in, as Vast quotes it).
    pub max_usd_per_hour: f64,
    pub gpus: Vec<String>,
    #[serde(default = "d_ram")]
    pub min_gpu_ram_gb: f64,
    #[serde(default = "d_rel")]
    pub min_reliability: f64,
    #[serde(default = "d_disk")]
    pub disk_gb: f64,
    pub image: String,
    /// The boot script (Vast "onstart"); it gets the identity in
    /// $C0MPUTE_IDENTITY_KEY_B64 when `identity_file` is set.
    pub onstart_file: String,
    /// The node's identity.key, so the replacement keeps the node id.
    #[serde(default)]
    pub identity_file: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
}
fn d_ram() -> f64 {
    12.0
}
fn d_rel() -> f64 {
    0.98
}
fn d_disk() -> f64 {
    60.0
}

pub fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(rest),
        None => PathBuf::from(p),
    }
}

pub fn default_config_path() -> PathBuf {
    c0mpute_core::config::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("autoscale.toml")
}

fn state_path() -> PathBuf {
    c0mpute_core::config::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("autoscale-state.json")
}

pub fn parse_config(text: &str) -> Result<Config> {
    let cfg: Config = toml::from_str(text).context("autoscale.toml")?;
    let mut names = std::collections::HashSet::new();
    for n in &cfg.nodes {
        if !names.insert(n.name.clone()) {
            bail!("node {:?} is listed twice", n.name);
        }
        if !["vast", "salad", "thunder"].contains(&n.provider.as_str()) {
            bail!("node {:?}: provider must be vast, salad or thunder", n.name);
        }
        if n.models.is_empty() {
            bail!("node {:?}: list the models it serves (demand for them starts it)", n.name);
        }
        if !c0mpute_core::autoscale_signal::valid_node_id(&n.node) {
            bail!("node {:?}: node must be its peer id or the 8-character id on /nodes", n.name);
        }
        if n.provider == "salad" && (n.org.is_none() || n.project.is_none()) {
            bail!("node {:?}: salad needs org and project", n.name);
        }
        if let Some(r) = &n.replace {
            if n.provider != "vast" {
                bail!("node {:?}: [node.replace] is Vast only", n.name);
            }
            if !(r.max_usd_per_hour.is_finite() && r.max_usd_per_hour > 0.0) {
                bail!("node {:?}: replace.max_usd_per_hour must be a positive price cap", n.name);
            }
            if r.gpus.is_empty() {
                bail!("node {:?}: replace.gpus is empty", n.name);
            }
        }
    }
    Ok(cfg)
}

pub const EXAMPLE: &str = r#"# c0mpute autoscale: stop rented nodes nobody needs, start them on demand.
# Runs on an always-on machine you own (not the rented box). Provider keys stay here.
report = true            # show state on c0mpute.com/dashboard/nodes (C0MPUTE_API_KEY)
interval_s = 60
# log_file = "~/autoscale.log"

[[node]]
name = "my-gpu"
provider = "vast"        # vast | salad | thunder (thunder: status only)
instance = "12345678"
node = "abcdEFGH"        # the 8-character id on c0mpute.com/nodes, or the peer id
models = ["eriktonon/qwen3.8-uncensored:9b"]
api_key_file = "~/.config/c0mpute/vast.key"
idle_minutes = 20
min_run_minutes = 15
boot_timeout_minutes = 8
# busy_cmd = "ssh -p 12345 root@ssh1.vast.ai 'test $(nvidia-smi --query-gpu=utilization.gpu --format=csv,noheader,nounits) -gt 10'"
# pre_stop_cmd = "ssh -p 12345 root@ssh1.vast.ai 'pkill -f \"c0mpute worker\"'"

# [node.replace]         # Vast only: a start that never comes up rents a replacement
# max_usd_per_hour = 0.12
# gpus = ["RTX 3060", "RTX 3070", "RTX 4060 Ti"]
# min_gpu_ram_gb = 12
# min_reliability = 0.98
# disk_gb = 60
# image = "nvidia/cuda:12.4.1-runtime-ubuntu22.04"
# onstart_file = "~/.config/c0mpute/vast-onstart.sh"
# identity_file = "~/.config/c0mpute/node-identity.key"
"#;

// ─── persisted state ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    #[default]
    Unknown,
    Running,
    Stopped,
    Starting,
    Stopping,
    Replacing,
    Error,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Unknown => "unknown",
            Phase::Running => "running",
            Phase::Stopped => "stopped",
            Phase::Starting => "starting",
            Phase::Stopping => "stopping",
            Phase::Replacing => "replacing",
            Phase::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct NodeState {
    /// The instance in use (differs from the config after a replacement).
    #[serde(default)]
    pub instance: Option<String>,
    #[serde(default)]
    pub phase: Phase,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub since_ms: u64,
    /// When the node last came up (or was first seen running).
    #[serde(default)]
    pub started_at_ms: u64,
    /// When we asked the provider to start it.
    #[serde(default)]
    pub start_issued_ms: u64,
    #[serde(default)]
    pub saved_usd: f64,
    #[serde(default)]
    pub running_usd_h: Option<f64>,
    #[serde(default)]
    pub stopped_usd_h: Option<f64>,
    #[serde(default)]
    pub last_tick_ms: u64,
    #[serde(default)]
    pub last_report_ms: u64,
    /// Seconds from start request to serving, newest last (10 kept).
    #[serde(default)]
    pub cold_starts_s: Vec<u64>,
    #[serde(default)]
    pub replacement: Option<Replacement>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Replacement {
    pub instance: String,
    pub created_ms: u64,
    pub usd_per_hour: f64,
}

fn load_state() -> BTreeMap<String, NodeState> {
    std::fs::read_to_string(state_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_state(s: &BTreeMap<String, NodeState>) -> Result<()> {
    let p = state_path();
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(s)?)?;
    std::fs::rename(tmp, p)?;
    Ok(())
}

// ─── the network signal ───────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
pub struct Signal {
    #[serde(default)]
    pub node: SigNode,
    #[serde(default)]
    pub models: BTreeMap<String, SigModel>,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
pub struct SigNode {
    #[serde(default)]
    pub online: bool,
    #[serde(default)]
    pub last_seen_s: Option<u64>,
    #[serde(default)]
    pub in_flight: u64,
    #[serde(default)]
    pub last_accept_ms: Option<u64>,
    #[serde(default)]
    pub last_receipt_ms: Option<u64>,
    #[serde(default)]
    pub serves: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
pub struct SigModel {
    #[serde(default)]
    pub offers_window: u64,
    #[serde(default)]
    pub offers_2m: u64,
    #[serde(default)]
    pub waiting: u64,
    #[serde(default)]
    pub unserved_window: u64,
}

impl Signal {
    /// Somebody wants one of these models now.
    pub fn demand(&self) -> Option<String> {
        self.models.iter().find_map(|(m, d)| {
            if d.waiting > 0 {
                Some(format!("{} job(s) for {m} waiting with no free node", d.waiting))
            } else if d.offers_2m > 0 {
                Some(format!("{} job(s) for {m} in the last 2 min", d.offers_2m))
            } else {
                None
            }
        })
    }

    /// The node is up on the network and advertises every model.
    pub fn serving(&self, models: &[String]) -> bool {
        self.node.online
            && self.node.last_seen_s.is_some_and(|s| s <= 180)
            && models.iter().all(|m| self.node.serves.iter().any(|s| s == m))
    }

    pub fn last_activity_ms(&self) -> Option<u64> {
        match (self.node.last_accept_ms, self.node.last_receipt_ms) {
            (Some(a), Some(r)) => Some(a.max(r)),
            (a, r) => a.or(r),
        }
    }
}

async fn fetch_signal(http: &reqwest::Client, site: &str, n: &NodeCfg) -> Result<Signal> {
    let window = (n.idle_minutes.max(1) * 60).to_string();
    let r = http
        .get(format!("{site}/api/v1/autoscale/signal"))
        .query(&[("node", n.node.as_str()), ("models", &n.models.join(",")), ("window_s", &window)])
        .timeout(Duration::from_secs(20))
        .send()
        .await?;
    let status = r.status();
    let body = r.text().await?;
    if !status.is_success() {
        bail!("signal HTTP {}: {}", status.as_u16(), body.chars().take(200).collect::<String>());
    }
    Ok(serde_json::from_str(&body)?)
}

// ─── providers ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PState {
    Running,
    Stopped,
    /// On its way up (scheduled, loading, intended running).
    Starting,
    Stopping,
    /// Destroyed, or the provider does not know it.
    Gone,
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PStatus {
    pub state: PState,
    pub running_usd_h: Option<f64>,
    pub stopped_usd_h: Option<f64>,
    pub detail: String,
}

/// Vast.ai: `actual_status` + `intended_status` of `GET /api/v0/instances/{id}/`.
pub fn vast_status(v: &Value) -> PStatus {
    let i = v.get("instances").filter(|x| x.is_object()).unwrap_or(v);
    let s = |k: &str| i.get(k).and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
    let (actual, intended, cur) = (s("actual_status"), s("intended_status"), s("cur_state"));
    let state = if i.is_null() || i.as_object().is_some_and(|o| o.is_empty()) {
        PState::Gone
    } else if actual == "running" && intended != "stopped" {
        PState::Running
    } else if actual == "running" && intended == "stopped" {
        PState::Stopping
    } else if intended == "running" || ["loading", "created", "scheduling"].contains(&actual.as_str()) {
        PState::Starting
    } else if ["exited", "stopped", "offline", "inactive"].contains(&actual.as_str()) || cur == "stopped" || intended == "stopped" {
        PState::Stopped
    } else {
        PState::Unknown
    };
    let f = |p: &str| i.pointer(p).and_then(Value::as_f64).filter(|x| x.is_finite() && *x >= 0.0);
    PStatus {
        state,
        running_usd_h: f("/dph_total"),
        stopped_usd_h: f("/instance/diskHour").or_else(|| f("/storage_total_cost")),
        detail: format!("{actual}/{intended} {}", i.get("status_msg").and_then(Value::as_str).unwrap_or("")).trim().to_string(),
    }
}

/// SaladCloud: `current_state.status` of a container group.
pub fn salad_status(v: &Value) -> PStatus {
    let st = v.pointer("/current_state/status").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
    let state = match st.as_str() {
        "running" => PState::Running,
        "stopped" => PState::Stopped,
        "pending" | "deploying" => PState::Starting,
        "stopping" => PState::Stopping,
        "" => PState::Gone,
        _ => PState::Unknown,
    };
    PStatus { state, running_usd_h: None, stopped_usd_h: Some(0.0), detail: st }
}

pub struct Provider {
    kind: String,
    key: String,
    org: Option<String>,
    project: Option<String>,
    http: reqwest::Client,
}

const VAST: &str = "https://console.vast.ai";
const SALAD: &str = "https://api.salad.com/api/public";
const THUNDER: &str = "https://api.thundercompute.com:8443";

impl Provider {
    pub fn new(n: &NodeCfg, http: reqwest::Client) -> Result<Self> {
        let key = if let Some(f) = &n.api_key_file {
            let p = expand(f);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(m) = std::fs::metadata(&p)
                    && m.permissions().mode() & 0o077 != 0
                {
                    bail!("{}: provider key file must not be readable by others (chmod 600)", p.display());
                }
            }
            let raw = std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
            // A bare key, or KEY=value lines (any *_API_KEY).
            raw.lines()
                .map(str::trim)
                .find_map(|l| match l.split_once('=') {
                    Some((k, v)) if k.trim().ends_with("API_KEY") => Some(v.trim().trim_matches('"').to_string()),
                    None if !l.is_empty() && !l.starts_with('#') => Some(l.to_string()),
                    _ => None,
                })
                .ok_or_else(|| anyhow!("{}: no key", p.display()))?
        } else {
            let var = n.api_key_env.clone().unwrap_or_else(|| format!("{}_API_KEY", n.provider.to_ascii_uppercase()));
            std::env::var(&var).map_err(|_| anyhow!("node {:?}: set api_key_file or ${var}", n.name))?
        };
        Ok(Self { kind: n.provider.clone(), key, org: n.org.clone(), project: n.project.clone(), http })
    }

    async fn call(&self, method: reqwest::Method, url: String, body: Option<Value>) -> Result<Value> {
        let mut rq = self.http.request(method.clone(), &url).timeout(Duration::from_secs(30));
        rq = match self.kind.as_str() {
            "salad" => rq.header("Salad-Api-Key", &self.key),
            _ => rq.bearer_auth(&self.key),
        };
        if let Some(b) = body {
            rq = rq.json(&b);
        }
        let r = rq.send().await?;
        let status = r.status();
        let text = r.text().await.unwrap_or_default();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(Value::Null);
        }
        if !status.is_success() {
            // Never echo the URL query or headers: they can carry the key.
            bail!("{} {} -> HTTP {}: {}", self.kind, method, status.as_u16(), text.chars().take(300).collect::<String>());
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    fn salad_group(&self, name: &str) -> String {
        format!(
            "{SALAD}/organizations/{}/projects/{}/containers/{}",
            self.org.as_deref().unwrap_or(""),
            self.project.as_deref().unwrap_or(""),
            name
        )
    }

    pub async fn status(&self, instance: &str) -> Result<PStatus> {
        match self.kind.as_str() {
            "vast" => Ok(vast_status(&self.call(reqwest::Method::GET, format!("{VAST}/api/v0/instances/{instance}/"), None).await?)),
            "salad" => Ok(salad_status(&self.call(reqwest::Method::GET, self.salad_group(instance), None).await?)),
            "thunder" => {
                let v = self.call(reqwest::Method::GET, format!("{THUNDER}/v1/instances/list"), None).await?;
                let found = v
                    .as_object()
                    .and_then(|o| o.get(instance).cloned())
                    .or_else(|| {
                        v.as_array().and_then(|a| {
                            a.iter().find(|x| x.get("id").and_then(Value::as_str) == Some(instance) || x.get("uuid").and_then(Value::as_str) == Some(instance)).cloned()
                        })
                    })
                    .unwrap_or(Value::Null);
                let st = found.get("status").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
                let state = match st.as_str() {
                    "running" => PState::Running,
                    "" => PState::Gone,
                    "stopped" | "terminated" | "deleted" => PState::Gone,
                    _ => PState::Starting,
                };
                Ok(PStatus { state, running_usd_h: None, stopped_usd_h: None, detail: st })
            }
            k => bail!("unknown provider {k}"),
        }
    }

    pub async fn set_running(&self, instance: &str, on: bool) -> Result<()> {
        match self.kind.as_str() {
            "vast" => {
                let v = self
                    .call(reqwest::Method::PUT, format!("{VAST}/api/v0/instances/{instance}/"), Some(json!({"state": if on { "running" } else { "stopped" }})))
                    .await?;
                if v.get("success").and_then(Value::as_bool) == Some(false) {
                    bail!("vast refused: {}", v.get("msg").or_else(|| v.get("error")).cloned().unwrap_or(v.clone()));
                }
                Ok(())
            }
            "salad" => {
                self.call(reqwest::Method::POST, format!("{}/{}", self.salad_group(instance), if on { "start" } else { "stop" }), None).await?;
                Ok(())
            }
            "thunder" => bail!("Thunder Compute has no native stop/start (snapshot + delete only): status only for now"),
            k => bail!("unknown provider {k}"),
        }
    }

    /// Vast: the cheapest offer that fits `r`, cheapest first.
    pub async fn vast_offers(&self, r: &ReplaceCfg) -> Result<Vec<(u64, f64, String)>> {
        let q = json!({
            "verified": {"eq": true}, "external": {"eq": false}, "rentable": {"eq": true}, "rented": {"eq": false},
            "gpu_name": {"in": r.gpus}, "num_gpus": {"eq": 1},
            "gpu_ram": {"gte": r.min_gpu_ram_gb * 1000.0}, "reliability2": {"gte": r.min_reliability},
            "dph_total": {"lte": r.max_usd_per_hour},
            "order": [["dph_total", "asc"]], "type": "on-demand", "allocated_storage": r.disk_gb, "limit": 10
        });
        let v = self.call(reqwest::Method::POST, format!("{VAST}/api/v0/bundles/"), Some(q)).await?;
        Ok(vast_offer_rows(&v, r.max_usd_per_hour))
    }

    pub async fn vast_create(&self, offer: u64, r: &ReplaceCfg, env: BTreeMap<String, String>, label: &str) -> Result<String> {
        let onstart = std::fs::read_to_string(expand(&r.onstart_file)).with_context(|| format!("read {}", r.onstart_file))?;
        let body = json!({
            "client_id": "me", "image": r.image, "env": env, "price": null, "disk": r.disk_gb,
            "label": label, "onstart": onstart, "runtype": "ssh_direc ssh_proxy", "cancel_unavail": true
        });
        let v = self.call(reqwest::Method::PUT, format!("{VAST}/api/v0/asks/{offer}/"), Some(body)).await?;
        v.get("new_contract")
            .and_then(|c| c.as_u64().map(|n| n.to_string()).or_else(|| c.as_str().map(str::to_string)))
            .ok_or_else(|| anyhow!("vast create gave no instance id: {v}"))
    }

    pub async fn vast_destroy(&self, instance: &str) -> Result<()> {
        self.call(reqwest::Method::DELETE, format!("{VAST}/api/v0/instances/{instance}/"), Some(json!({}))).await?;
        Ok(())
    }
}

/// (offer id, $/h, description), only offers within the cap, cheapest first.
pub fn vast_offer_rows(v: &Value, cap: f64) -> Vec<(u64, f64, String)> {
    let mut rows: Vec<(u64, f64, String)> = v
        .get("offers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|o| {
            let id = o.get("id")?.as_u64()?;
            let p = o.get("dph_total")?.as_f64()?;
            (p.is_finite() && p > 0.0 && p <= cap).then(|| {
                (id, p, format!("{} {} rel {:.3}", o.get("gpu_name").and_then(Value::as_str).unwrap_or("?"), o.get("geolocation").and_then(Value::as_str).unwrap_or("?"), o.get("reliability2").and_then(Value::as_f64).unwrap_or(0.0)))
            })
        })
        .collect();
    rows.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    rows
}

// ─── policy (pure) ────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Nothing,
    Start(String),
    Stop(String),
    /// Mark running (came up, or found running), with the cold start in s.
    Up(Option<u64>),
    /// The provider reports it stopped (by us or by hand).
    Down,
    /// A start that never came up.
    StartTimedOut(String),
}

pub struct Inputs<'a> {
    pub cfg: &'a NodeCfg,
    pub st: &'a NodeState,
    pub provider: PState,
    /// None when the network signal could not be read.
    pub signal: Option<&'a Signal>,
    /// busy_cmd said busy.
    pub busy: bool,
    pub now: u64,
}

pub fn decide(i: &Inputs) -> Action {
    let min = 60_000u64;
    let demand = i.signal.and_then(Signal::demand);
    match i.provider {
        PState::Stopped => {
            if let Some(d) = demand {
                return Action::Start(d);
            }
            // Just asked to start: the provider has not caught up yet.
            if i.st.phase == Phase::Starting && i.now.saturating_sub(i.st.start_issued_ms) <= i.cfg.boot_timeout_minutes * min {
                return Action::Nothing;
            }
            if i.st.phase != Phase::Stopped && i.st.phase != Phase::Error {
                return Action::Down;
            }
            Action::Nothing
        }
        PState::Starting | PState::Running => {
            let serving = i.signal.is_some_and(|s| s.serving(&i.cfg.models));
            if matches!(i.st.phase, Phase::Starting | Phase::Stopped | Phase::Unknown | Phase::Replacing) {
                if i.provider == PState::Running && serving {
                    let cold = (i.st.start_issued_ms > 0 && i.st.phase == Phase::Starting)
                        .then(|| i.now.saturating_sub(i.st.start_issued_ms) / 1000);
                    return Action::Up(cold);
                }
                if i.st.phase == Phase::Starting
                    && i.now.saturating_sub(i.st.start_issued_ms) > i.cfg.boot_timeout_minutes * min
                {
                    return Action::StartTimedOut(format!(
                        "not serving {} min after the start (provider: {:?}, online: {})",
                        i.cfg.boot_timeout_minutes,
                        i.provider,
                        i.signal.is_some_and(|s| s.node.online)
                    ));
                }
                if i.st.phase == Phase::Unknown && i.provider == PState::Running {
                    // Found running but not (yet) on the network: treat as up so
                    // the idle clock and min-run guard apply.
                    return Action::Up(None);
                }
                return Action::Nothing;
            }
            if i.st.phase != Phase::Running || i.provider != PState::Running {
                return Action::Nothing;
            }
            // Running: stop only when everything says idle.
            let Some(sig) = i.signal else { return Action::Nothing };
            if demand.is_some() || i.busy || sig.node.in_flight > 0 {
                return Action::Nothing;
            }
            if i.now.saturating_sub(i.st.started_at_ms) < i.cfg.min_run_minutes * min {
                return Action::Nothing;
            }
            let last = sig.last_activity_ms().unwrap_or(0).max(i.st.started_at_ms);
            let idle = i.now.saturating_sub(last);
            if idle >= i.cfg.idle_minutes * min {
                return Action::Stop(format!(
                    "idle {} min: no job for the node, nothing in flight, no demand for {}",
                    idle / min,
                    i.cfg.models.join(", ")
                ));
            }
            Action::Nothing
        }
        PState::Stopping => Action::Nothing,
        PState::Gone | PState::Unknown => Action::Nothing,
    }
}

// ─── the loop ─────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct AutoscaleArgs {
    #[command(subcommand)]
    pub cmd: AutoscaleCmd,
    /// Config file (default ~/.config/c0mpute/autoscale.toml).
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
pub enum AutoscaleCmd {
    /// Write an example config (0600) if there is none.
    Init,
    /// Run the loop (for a systemd --user service).
    Run {
        #[arg(long)]
        dry_run: bool,
    },
    /// One pass over every node.
    Once {
        #[arg(long)]
        dry_run: bool,
    },
    /// Each node's state, the provider's view and the network signal.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Start a node now.
    Start { name: String },
    /// Stop a node now (the guards do not apply).
    Stop { name: String },
    /// Clear an error so the loop acts on the node again.
    Reset { name: String },
}

struct Ctx {
    cfg: Config,
    site: String,
    http: reqwest::Client,
    dry_run: bool,
    /// Log every pass (`once`), not only transitions.
    verbose: bool,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn stamp() -> String {
    let s = now_ms() / 1000;
    let (d, r) = (s / 86_400, s % 86_400);
    // civil from days (UTC)
    let z = d as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{day:02}T{:02}:{:02}:{:02}Z", r / 3600, (r % 3600) / 60, r % 60)
}

fn log(ctx: &Ctx, name: &str, msg: &str) {
    let line = format!("{} {name}: {msg}", stamp());
    println!("{line}");
    if let Some(f) = &ctx.cfg.log_file {
        let p = expand(f);
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        if let Ok(mut fh) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            let _ = writeln!(fh, "{line}");
        }
    }
}

/// Run an operator hook with `C0MPUTE_AUTOSCALE_{NAME,INSTANCE,NODE}` set, so
/// one script can follow the node across a replacement.
async fn sh(cmd: &str, n: &NodeCfg, instance: &str) -> Option<i32> {
    let fut = tokio::process::Command::new("sh").arg("-c").arg(cmd)
        .env("C0MPUTE_AUTOSCALE_NAME", &n.name).env("C0MPUTE_AUTOSCALE_INSTANCE", instance).env("C0MPUTE_AUTOSCALE_NODE", &n.node).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
    match tokio::time::timeout(Duration::from_secs(60), fut).await {
        Ok(Ok(s)) => s.code(),
        _ => None,
    }
}

async fn report(ctx: &Ctx, n: &NodeCfg, st: &mut NodeState, force: bool) {
    if !ctx.cfg.report || ctx.dry_run {
        return;
    }
    let now = now_ms();
    if !force && now.saturating_sub(st.last_report_ms) < 10 * 60_000 {
        return;
    }
    let Ok(key) = crate::ai_chat::require_key() else { return };
    let body = json!({
        "node": n.node, "provider": n.provider, "instance": st.instance.clone().unwrap_or_else(|| n.instance.clone()),
        "state": st.phase.as_str(), "reason": st.reason.chars().take(300).collect::<String>(), "at_ms": st.since_ms,
        "saved_usd": (st.saved_usd * 10_000.0).round() / 10_000.0,
        "running_usd_per_hour": st.running_usd_h, "stopped_usd_per_hour": st.stopped_usd_h,
        "idle_minutes": n.idle_minutes, "models": n.models,
    });
    let r = ctx.http.post(format!("{}/api/v1/operator/autoscale", ctx.site)).bearer_auth(key).json(&body).timeout(Duration::from_secs(15)).send().await;
    match r {
        Ok(r) if r.status().is_success() => st.last_report_ms = now,
        Ok(r) => eprintln!("autoscale: report for {} -> HTTP {}", n.name, r.status().as_u16()),
        Err(e) => eprintln!("autoscale: report for {}: {e}", n.name),
    }
}

fn transition(ctx: &Ctx, n: &NodeCfg, st: &mut NodeState, phase: Phase, reason: String) {
    log(ctx, &n.name, &format!("{} -> {}: {reason}", st.phase.as_str(), phase.as_str()));
    st.phase = phase;
    st.reason = reason;
    st.since_ms = now_ms();
}

async fn tick(ctx: &Ctx, n: &NodeCfg, st: &mut NodeState) -> Result<()> {
    if n.dry_run && !ctx.dry_run {
        let dry = Ctx { cfg: ctx.cfg.clone(), site: ctx.site.clone(), http: ctx.http.clone(), dry_run: true, verbose: ctx.verbose };
        return Box::pin(tick(&dry, n, st)).await;
    }
    let now = now_ms();
    let instance = st.instance.clone().unwrap_or_else(|| n.instance.clone());
    st.instance = Some(instance.clone());
    let prov = Provider::new(n, ctx.http.clone())?;
    let ps = prov.status(&instance).await?;
    if let Some(r) = ps.running_usd_h.or(n.usd_per_hour) {
        st.running_usd_h = Some(r);
    }
    if let Some(s) = ps.stopped_usd_h.or(n.stopped_usd_per_hour) {
        st.stopped_usd_h = Some(s);
    }
    // Savings accrue while the provider says stopped.
    if st.last_tick_ms > 0 && ps.state == PState::Stopped && !ctx.dry_run {
        let hours = now.saturating_sub(st.last_tick_ms) as f64 / 3_600_000.0;
        let rate = st.running_usd_h.unwrap_or(0.0) - st.stopped_usd_h.unwrap_or(0.0);
        if rate > 0.0 && hours < 1.0 {
            st.saved_usd += rate * hours;
        }
    }
    if !ctx.dry_run {
        st.last_tick_ms = now;
    }

    let signal = match fetch_signal(&ctx.http, &ctx.site, n).await {
        Ok(s) => Some(s),
        Err(e) => {
            log(ctx, &n.name, &format!("no network signal ({e}); holding"));
            None
        }
    };

    // A replacement in progress has its own clock.
    if st.phase == Phase::Replacing {
        return replacing(ctx, n, st, &prov, signal.as_ref()).await;
    }
    if st.phase == Phase::Error {
        report(ctx, n, st, false).await;
        return Ok(());
    }
    if ps.state == PState::Gone {
        transition(ctx, n, st, Phase::Error, format!("instance {instance} is gone at {}", n.provider));
        report(ctx, n, st, true).await;
        return Ok(());
    }

    let busy = match (&n.busy_cmd, ps.state) {
        (Some(c), PState::Running) if st.phase == Phase::Running => sh(c, n, &instance).await != Some(1),
        _ => false,
    };
    let action = decide(&Inputs { cfg: n, st, provider: ps.state, signal: signal.as_ref(), busy, now });
    if ctx.dry_run {
        if ctx.verbose || action != Action::Nothing {
            log(ctx, &n.name, &format!("[dry-run] provider {:?} ({}), phase {}, would: {action:?}", ps.state, ps.detail, st.phase.as_str()));
        }
        return Ok(());
    }
    match action {
        Action::Nothing => {}
        Action::Start(why) => {
            prov.set_running(&instance, true).await?;
            st.start_issued_ms = now;
            transition(ctx, n, st, Phase::Starting, format!("start: {why}"));
            report(ctx, n, st, true).await;
        }
        Action::Stop(why) => {
            if let Some(c) = &n.pre_stop_cmd {
                let ok = sh(c, n, &instance).await;
                log(ctx, &n.name, &format!("pre_stop_cmd: {}", match ok { Some(0) => "ok", Some(_) => "failed", None => "timed out" }));
                // A job could have landed while the worker wound down: let it finish.
                if let Ok(s) = fetch_signal(&ctx.http, &ctx.site, n).await
                    && s.node.in_flight > 0
                {
                    log(ctx, &n.name, "a job is in flight after pre_stop_cmd; stopping on a later pass");
                    return Ok(());
                }
            }
            prov.set_running(&instance, false).await?;
            transition(ctx, n, st, Phase::Stopped, format!("stop: {why}"));
            report(ctx, n, st, true).await;
        }
        Action::Up(cold) => {
            st.started_at_ms = now;
            let why = match cold {
                Some(s) => {
                    st.cold_starts_s.push(s);
                    if st.cold_starts_s.len() > 10 {
                        st.cold_starts_s.remove(0);
                    }
                    format!("serving {} {s} s after the start", n.models.join(", "))
                }
                None => "found running".to_string(),
            };
            transition(ctx, n, st, Phase::Running, why);
            report(ctx, n, st, true).await;
        }
        Action::Down => {
            transition(ctx, n, st, Phase::Stopped, "provider reports it stopped".into());
            report(ctx, n, st, true).await;
        }
        Action::StartTimedOut(why) => {
            if n.replace.is_some() {
                transition(ctx, n, st, Phase::Replacing, format!("{why}; renting a replacement"));
                report(ctx, n, st, true).await;
                return replacing(ctx, n, st, &prov, signal.as_ref()).await;
            }
            transition(ctx, n, st, Phase::Error, why);
            report(ctx, n, st, true).await;
        }
    }
    report(ctx, n, st, false).await;
    Ok(())
}

/// Vast: rent a replacement (once), wait for it to serve the same node, then
/// destroy the instance that never came up. Gives up (and destroys the
/// replacement) after two boot timeouts.
async fn replacing(ctx: &Ctx, n: &NodeCfg, st: &mut NodeState, prov: &Provider, sig: Option<&Signal>) -> Result<()> {
    let Some(r) = &n.replace else {
        transition(ctx, n, st, Phase::Error, "replacing without [node.replace]".into());
        return Ok(());
    };
    let now = now_ms();
    let old = st.instance.clone().unwrap_or_else(|| n.instance.clone());
    match st.replacement.clone() {
        None => {
            // The old one may have come up after all.
            if prov.status(&old).await?.state == PState::Running && sig.is_some_and(|s| s.serving(&n.models)) {
                st.started_at_ms = now;
                transition(ctx, n, st, Phase::Running, "came up late; no replacement needed".into());
                return Ok(());
            }
            let offers = prov.vast_offers(r).await?;
            let Some((offer, price, desc)) = offers.first().cloned() else {
                transition(ctx, n, st, Phase::Error, format!("no offer for {:?} at or under ${}/h", r.gpus, r.max_usd_per_hour));
                report(ctx, n, st, true).await;
                return Ok(());
            };
            if price > r.max_usd_per_hour {
                bail!("offer over the cap");
            }
            let mut env = BTreeMap::new();
            if let Some(f) = &r.identity_file {
                use base64::Engine;
                let k = std::fs::read(expand(f)).with_context(|| format!("read {f}"))?;
                env.insert("C0MPUTE_IDENTITY_KEY_B64".to_string(), base64::engine::general_purpose::STANDARD.encode(k));
            }
            let label = r.label.clone().unwrap_or_else(|| format!("c0mpute-autoscale {}", n.name));
            let id = prov.vast_create(offer, r, env, &label).await?;
            log(ctx, &n.name, &format!("rented replacement {id} (offer {offer}, {desc}, ${price:.4}/h)"));
            st.replacement = Some(Replacement { instance: id, created_ms: now, usd_per_hour: price });
            report(ctx, n, st, true).await;
        }
        Some(rep) => {
            let ps = prov.status(&rep.instance).await?;
            if ps.state == PState::Running && sig.is_some_and(|s| s.serving(&n.models)) {
                log(ctx, &n.name, &format!("replacement {} serves {}; destroying {old}", rep.instance, n.models.join(", ")));
                if let Err(e) = prov.vast_destroy(&old).await {
                    log(ctx, &n.name, &format!("destroy {old} failed: {e:#} (destroy it by hand)"));
                }
                st.instance = Some(rep.instance.clone());
                st.replacement = None;
                st.started_at_ms = now;
                st.running_usd_h = Some(rep.usd_per_hour);
                st.cold_starts_s.push(now.saturating_sub(st.start_issued_ms) / 1000);
                transition(ctx, n, st, Phase::Running, format!("replaced {old} with {}", rep.instance));
                report(ctx, n, st, true).await;
            } else if now.saturating_sub(rep.created_ms) > 2 * n.boot_timeout_minutes * 60_000 {
                log(ctx, &n.name, &format!("replacement {} not serving after {} min; destroying it", rep.instance, 2 * n.boot_timeout_minutes));
                let _ = prov.vast_destroy(&rep.instance).await;
                st.replacement = None;
                transition(ctx, n, st, Phase::Error, "the replacement did not come up either; needs a person (`c0mpute autoscale reset`)".into());
                report(ctx, n, st, true).await;
            }
        }
    }
    Ok(())
}

fn load_config(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {} (run `c0mpute autoscale init`)", path.display()))?;
    parse_config(&text)
}

pub async fn run(args: AutoscaleArgs) -> Result<()> {
    let path = args.config.clone().unwrap_or_else(default_config_path);
    if let AutoscaleCmd::Init = args.cmd {
        if path.exists() {
            println!("{} exists; not touching it", path.display());
            return Ok(());
        }
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?;
            f.write_all(EXAMPLE.as_bytes())?;
        }
        #[cfg(not(unix))]
        std::fs::write(&path, EXAMPLE)?;
        println!("wrote {}: edit it, then `c0mpute autoscale once --dry-run`", path.display());
        return Ok(());
    }
    let cfg = load_config(&path)?;
    let site = base_url(cfg.site.as_deref());
    let http = client()?;
    let mut states = load_state();
    let find = |name: &str| cfg.nodes.iter().find(|n| n.name == name).cloned().ok_or_else(|| anyhow!("no node named {name:?} in {}", path.display()));
    match args.cmd {
        AutoscaleCmd::Init => unreachable!(),
        AutoscaleCmd::Status { json: as_json } => {
            let mut out = Vec::new();
            for n in &cfg.nodes {
                let st = states.get(&n.name).cloned().unwrap_or_default();
                let inst = st.instance.clone().unwrap_or_else(|| n.instance.clone());
                let ps = match Provider::new(n, http.clone()) {
                    Ok(p) => p.status(&inst).await.map(|s| format!("{:?} ({})", s.state, s.detail)).unwrap_or_else(|e| format!("error: {e:#}")),
                    Err(e) => format!("error: {e:#}"),
                };
                let sig = fetch_signal(&http, &site, n).await.ok();
                if as_json {
                    out.push(json!({"name": n.name, "instance": inst, "phase": st.phase.as_str(), "reason": st.reason, "since_ms": st.since_ms,
                        "provider": ps, "saved_usd": st.saved_usd, "cold_starts_s": st.cold_starts_s,
                        "online": sig.as_ref().map(|s| s.node.online), "demand": sig.as_ref().and_then(Signal::demand)}));
                } else {
                    println!("{}  {} {}  phase={} since={}s ago  provider={}  saved=${:.2}  cold_starts={:?}\n  reason: {}\n  network: {}",
                        n.name, n.provider, inst, st.phase.as_str(), now_ms().saturating_sub(st.since_ms) / 1000, ps, st.saved_usd, st.cold_starts_s, st.reason,
                        match &sig { Some(s) => format!("online={} in_flight={} serves={:?} demand={}", s.node.online, s.node.in_flight, s.node.serves, s.demand().unwrap_or_else(|| "none".into())), None => "unavailable".into() });
                }
            }
            if as_json {
                println!("{}", serde_json::to_string_pretty(&out)?);
            }
            Ok(())
        }
        AutoscaleCmd::Reset { name } => {
            let n = find(&name)?;
            let st = states.entry(n.name.clone()).or_default();
            st.phase = Phase::Unknown;
            st.reason = "reset by hand".into();
            st.replacement = None;
            save_state(&states)?;
            println!("{name}: reset");
            Ok(())
        }
        AutoscaleCmd::Start { ref name } | AutoscaleCmd::Stop { ref name } => {
            let on = matches!(args.cmd, AutoscaleCmd::Start { .. });
            let n = find(&name)?;
            let ctx = Ctx { cfg: cfg.clone(), site, http: http.clone(), dry_run: false, verbose: true };
            let st = states.entry(n.name.clone()).or_default();
            let inst = st.instance.clone().unwrap_or_else(|| n.instance.clone());
            Provider::new(&n, http)?.set_running(&inst, on).await?;
            if on {
                st.start_issued_ms = now_ms();
                transition(&ctx, &n, st, Phase::Starting, "start by hand".into());
            } else {
                transition(&ctx, &n, st, Phase::Stopped, "stop by hand".into());
            }
            report(&ctx, &n, st, true).await;
            save_state(&states)?;
            Ok(())
        }
        AutoscaleCmd::Once { dry_run } | AutoscaleCmd::Run { dry_run } => {
            let looping = matches!(args.cmd, AutoscaleCmd::Run { .. });
            let ctx = Ctx { cfg: cfg.clone(), site, http, dry_run, verbose: !looping };
            loop {
                for n in cfg.nodes.iter().filter(|n| n.enabled) {
                    let st = states.entry(n.name.clone()).or_default();
                    if let Err(e) = tick(&ctx, n, st).await {
                        log(&ctx, &n.name, &format!("error: {e:#}"));
                    }
                }
                if !dry_run {
                    save_state(&states)?;
                }
                if !looping {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_secs(cfg.interval_s.max(15))).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node() -> NodeCfg {
        parse_config(
            r#"
[[node]]
name = "g"
provider = "vast"
instance = "1"
node = "sdNT3kCN"
models = ["m:9b"]
"#,
        )
        .unwrap()
        .nodes
        .remove(0)
    }

    fn sig(online: bool, serves: bool, last: Option<u64>, in_flight: u64, waiting: u64, offers_2m: u64) -> Signal {
        let mut s = Signal::default();
        s.node.online = online;
        s.node.last_seen_s = Some(5);
        s.node.in_flight = in_flight;
        s.node.last_receipt_ms = last;
        if serves {
            s.node.serves = vec!["m:9b".into()];
        }
        s.models.insert("m:9b".into(), SigModel { offers_window: offers_2m, offers_2m, waiting, unserved_window: waiting });
        s
    }

    const M: u64 = 60_000;

    #[test]
    fn config_defaults_and_validation() {
        let n = node();
        assert_eq!((n.idle_minutes, n.min_run_minutes, n.boot_timeout_minutes), (20, 15, 8));
        assert!(parse_config(EXAMPLE).is_ok());
        assert!(parse_config("[[node]]\nname='a'\nprovider='aws'\ninstance='1'\nnode='sdNT3kCN'\nmodels=['m']").is_err());
        assert!(parse_config("[[node]]\nname='a'\nprovider='vast'\ninstance='1'\nnode='sdNT3kCN'\nmodels=[]").is_err());
        assert!(parse_config("[[node]]\nname='a'\nprovider='salad'\ninstance='g'\nnode='sdNT3kCN'\nmodels=['m']").is_err());
        assert!(parse_config("[[node]]\nname='a'\nprovider='vast'\ninstance='1'\nnode='sdNT3kCN'\nmodels=['m']\nbogus=1").is_err());
    }

    #[test]
    fn stopped_starts_on_waiting_or_recent_demand_only() {
        let n = node();
        let st = NodeState { phase: Phase::Stopped, ..Default::default() };
        let now = 100 * M;
        let go = |s: &Signal| decide(&Inputs { cfg: &n, st: &st, provider: PState::Stopped, signal: Some(s), busy: false, now });
        assert!(matches!(go(&sig(false, false, None, 0, 1, 0)), Action::Start(_)));
        assert!(matches!(go(&sig(false, false, None, 0, 0, 2)), Action::Start(_)));
        assert_eq!(go(&sig(false, false, None, 0, 0, 0)), Action::Nothing);
        // No signal: never start blind, never stop blind.
        assert_eq!(decide(&Inputs { cfg: &n, st: &st, provider: PState::Stopped, signal: None, busy: false, now }), Action::Nothing);
    }

    #[test]
    fn running_stops_only_when_idle_past_min_run_and_nothing_in_flight() {
        let n = node();
        let now = 1_000 * M;
        let st = NodeState { phase: Phase::Running, started_at_ms: now - 60 * M, ..Default::default() };
        let go = |s: Option<&Signal>, busy: bool, st: &NodeState| decide(&Inputs { cfg: &n, st, provider: PState::Running, signal: s, busy, now });
        let idle = sig(true, true, Some(now - 25 * M), 0, 0, 0);
        assert!(matches!(go(Some(&idle), false, &st), Action::Stop(_)));
        // Recent job.
        assert_eq!(go(Some(&sig(true, true, Some(now - 5 * M), 0, 0, 0)), false, &st), Action::Nothing);
        // In flight.
        assert_eq!(go(Some(&sig(true, true, Some(now - 25 * M), 1, 0, 0)), false, &st), Action::Nothing);
        // Demand.
        assert_eq!(go(Some(&sig(true, true, Some(now - 25 * M), 0, 0, 1)), false, &st), Action::Nothing);
        // busy_cmd.
        assert_eq!(go(Some(&idle), true, &st), Action::Nothing);
        // No signal.
        assert_eq!(go(None, false, &st), Action::Nothing);
        // Min run time after a start.
        let fresh = NodeState { phase: Phase::Running, started_at_ms: now - 10 * M, ..Default::default() };
        assert_eq!(go(Some(&sig(true, true, None, 0, 0, 0)), false, &fresh), Action::Nothing);
        // Started 16 min ago, never got a job: idle counts from the start (16 < 20).
        let st16 = NodeState { phase: Phase::Running, started_at_ms: now - 16 * M, ..Default::default() };
        assert_eq!(go(Some(&sig(true, true, None, 0, 0, 0)), false, &st16), Action::Nothing);
    }

    #[test]
    fn starting_comes_up_when_serving_or_times_out() {
        let n = node();
        let now = 1_000 * M;
        let st = NodeState { phase: Phase::Starting, start_issued_ms: now - 2 * M, ..Default::default() };
        let go = |p: PState, s: &Signal, st: &NodeState| decide(&Inputs { cfg: &n, st, provider: p, signal: Some(s), busy: false, now });
        assert_eq!(go(PState::Running, &sig(true, true, None, 0, 1, 1), &st), Action::Up(Some(120)));
        // Online but the model is not loaded yet.
        assert_eq!(go(PState::Running, &sig(true, false, None, 0, 1, 1), &st), Action::Nothing);
        assert_eq!(go(PState::Starting, &sig(false, false, None, 0, 1, 1), &st), Action::Nothing);
        let late = NodeState { phase: Phase::Starting, start_issued_ms: now - 9 * M, ..Default::default() };
        assert!(matches!(go(PState::Starting, &sig(false, false, None, 0, 1, 1), &late), Action::StartTimedOut(_)));
    }

    #[test]
    fn provider_status_parsing() {
        let v = json!({"instances": {"actual_status": "running", "intended_status": "running", "dph_total": 0.0867, "instance": {"diskHour": 0.0333}}});
        let s = vast_status(&v);
        assert_eq!(s.state, PState::Running);
        assert_eq!(s.running_usd_h, Some(0.0867));
        assert_eq!(s.stopped_usd_h, Some(0.0333));
        assert_eq!(vast_status(&json!({"instances": {"actual_status": "exited", "intended_status": "stopped"}})).state, PState::Stopped);
        assert_eq!(vast_status(&json!({"instances": {"actual_status": "exited", "intended_status": "running"}})).state, PState::Starting);
        assert_eq!(vast_status(&json!({"instances": {"actual_status": "running", "intended_status": "stopped"}})).state, PState::Stopping);
        assert_eq!(vast_status(&Value::Null).state, PState::Gone);
        assert_eq!(salad_status(&json!({"current_state": {"status": "stopped"}})).state, PState::Stopped);
        assert_eq!(salad_status(&json!({"current_state": {"status": "deploying"}})).state, PState::Starting);
    }

    #[test]
    fn offers_over_the_cap_are_never_picked() {
        let v = json!({"offers": [
            {"id": 3, "dph_total": 0.13, "gpu_name": "RTX 3060"},
            {"id": 2, "dph_total": 0.09, "gpu_name": "RTX 3060"},
            {"id": 1, "dph_total": 0.07, "gpu_name": "RTX 3070"}]});
        let rows = vast_offer_rows(&v, 0.12);
        assert_eq!(rows.iter().map(|r| r.0).collect::<Vec<_>>(), vec![1, 2]);
    }
}
