//! `gameserver.start` / `gameserver.stop`: rent a Minecraft, Factorio or
//! Valheim server on a worker for a fixed number of hours.
//!
//! A start job is an ordinary auction whose job is short: the winner creates
//! a container from a pinned, well-known image (`docker create`, argv only,
//! never a shell) with memory, CPU and pid limits, a named volume for the
//! world and a published port, optionally restores a world backup the buyer
//! serves by hash, starts it, and returns `address:port` and the expiry. The
//! container then runs until the reservation ends: the worker's reaper stops
//! it at expiry. `gameserver.stop` (bid on only by the worker holding the
//! server, and only with the stop token whose sha256 the start job carried)
//! ends it early. Either way the worker tars the world out of the volume,
//! stores it as 1 MiB content-addressed chunks plus a manifest, and hands the
//! manifest hash back: `start --world <hash>` restores it on any worker.
//!
//! Only opt-in nodes serve it (`--roles ...,gameserver`): it holds a port and
//! RAM for hours. A node advertises it only when `docker info` works and it
//! knows a public address (`[gameserver] public_host`, the gateway's announce
//! URL, or `C0MPUTE_P2P_EXTERNAL`); games appear once their image is pulled.
//! The operator must open the port range in the firewall.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use c0mpute_net::topics::{JobOffer, JobStatus};
use c0mpute_net::{Libp2pNetwork, Network};
use c0mpute_proto::Hash;
use c0mpute_store::ChunkStore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Semaphore, mpsc};
use tracing::{info, warn};

use crate::runner::{AcceptedJob, WorkerLoad, publish_receipt};

pub const WORKLOAD_START: &str = "gameserver.start";
pub const WORKLOAD_STOP: &str = "gameserver.stop";
pub const ROLE_TAG: &str = "c0mpute:role:gameserver";
pub const MAX_HOURS: u32 = 72;
pub const MAX_RAM_GB: u32 = 32;
pub const MAX_CPUS: f64 = 8.0;
/// World backups are stored and moved as chunks of this size.
pub const CHUNK_BYTES: usize = 1024 * 1024;
pub const MAX_WORLD_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MANIFEST_KIND: &str = "c0mpute-gameserver-world";
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
/// Start/stop jobs handled at once (the servers themselves are not jobs).
const JOB_SLOTS: usize = 2;
/// Stopped-server records (and their backup hash) are kept this long.
const KEEP_STOPPED_SECS: u64 = 7 * 86_400;
const REAP_EVERY: Duration = Duration::from_secs(30);

// ── games ───────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Game {
    Minecraft,
    Factorio,
    Valheim,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Proto {
    Tcp,
    Udp,
}

impl Proto {
    fn as_str(self) -> &'static str {
        match self {
            Proto::Tcp => "tcp",
            Proto::Udp => "udp",
        }
    }
}

impl Game {
    pub const ALL: [Game; 3] = [Game::Minecraft, Game::Factorio, Game::Valheim];

    pub fn parse(s: &str) -> Option<Game> {
        Self::ALL.into_iter().find(|g| g.name() == s.trim().to_ascii_lowercase())
    }
    pub fn name(self) -> &'static str {
        match self {
            Game::Minecraft => "minecraft",
            Game::Factorio => "factorio",
            Game::Valheim => "valheim",
        }
    }
    /// Pinned by multi-arch index digest: a tag that moves cannot change what runs.
    pub fn image(self) -> &'static str {
        match self {
            Game::Minecraft => "itzg/minecraft-server:java21@sha256:7b047e11ced52f1cbfe694ed9eb96e998fb374c582158f1af651009c9a975fca",
            Game::Factorio => "factoriotools/factorio:stable-2.1.24@sha256:a0c019b7608975a58f1694de54a2326e13a5aa2d123c18f48e5396181e366e28",
            Game::Valheim => "lloesche/valheim-server:sha-0ab6cdcc16dd@sha256:fde7829b12d39203e155b6209689b70234f8a27d12c4d48baf890cc2ae2c2df3",
        }
    }
    /// Where the image keeps its world; the named volume mounts here.
    pub fn data_dir(self) -> &'static str {
        match self {
            Game::Minecraft => "/data",
            Game::Factorio => "/factorio",
            Game::Valheim => "/config",
        }
    }
    /// Container ports, in order; the host gets a contiguous block of the same length.
    pub fn ports(self) -> &'static [(u16, Proto)] {
        match self {
            Game::Minecraft => &[(25565, Proto::Tcp)],
            Game::Factorio => &[(34197, Proto::Udp)],
            Game::Valheim => &[(2456, Proto::Udp), (2457, Proto::Udp)],
        }
    }
    pub fn min_ram_gb(self) -> u32 {
        match self {
            Game::Minecraft | Game::Factorio => 2,
            Game::Valheim => 4,
        }
    }
    /// CPU architectures the pinned image is published for.
    pub fn supports_arch(self, arch: &str) -> bool {
        match self {
            Game::Minecraft | Game::Factorio => matches!(arch, "x86_64" | "aarch64"),
            Game::Valheim => arch == "x86_64",
        }
    }
    pub fn tag(self) -> String {
        format!("c0mpute:gameserver:game:{}", self.name())
    }
}

// ── specs ───────────────────────────────────────────────────────────────

/// `spec_inline` of a `gameserver.start` offer.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StartSpec {
    pub v: u32,
    pub game: Game,
    pub hours: u32,
    pub ram_gb: u32,
    pub cpus: f64,
    /// Minecraft only: the buyer accepted the Minecraft EULA (`--accept-eula`).
    #[serde(default)]
    pub accept_eula: bool,
    /// `sha256:<hex>` of the stop token only the buyer holds.
    pub stop_hash: String,
    /// Manifest hash (blake3 hex) of a world backup to restore.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub world: Option<String>,
    /// Valheim only (it requires one). Visible to peers on the job topic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn valid_sha256_ref(s: &str) -> bool {
    s.strip_prefix("sha256:").is_some_and(|h| is_hex(h, 64))
}

impl StartSpec {
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.v != 1 {
            return Err(format!("unsupported gameserver spec version {}", self.v));
        }
        if !(1..=MAX_HOURS).contains(&self.hours) {
            return Err(format!("hours must be 1..={MAX_HOURS}"));
        }
        if self.ram_gb < self.game.min_ram_gb() || self.ram_gb > MAX_RAM_GB {
            return Err(format!("{} needs ram {}..={MAX_RAM_GB} GB", self.game.name(), self.game.min_ram_gb()));
        }
        if !(self.cpus.is_finite() && (0.5..=MAX_CPUS).contains(&self.cpus)) {
            return Err(format!("cpus must be 0.5..={MAX_CPUS}"));
        }
        if self.game == Game::Minecraft && !self.accept_eula {
            return Err("minecraft needs the Minecraft EULA accepted (--accept-eula, https://aka.ms/MinecraftEULA)".into());
        }
        if !valid_sha256_ref(&self.stop_hash) {
            return Err("stop_hash must be sha256:<64 lowercase hex>".into());
        }
        if self.world.as_deref().is_some_and(|w| !is_hex(w, 64)) {
            return Err("world must be a 64-hex manifest hash".into());
        }
        match (self.game, &self.password) {
            (Game::Valheim, Some(p)) if (5..=32).contains(&p.len()) && p.chars().all(|c| c.is_ascii_alphanumeric()) => {}
            (Game::Valheim, _) => return Err("valheim needs a password of 5-32 letters or digits".into()),
            (_, Some(_)) => return Err("only valheim takes a password".into()),
            _ => {}
        }
        if let Some(n) = &self.server_name
            && (n.is_empty() || n.len() > 32 || !n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.')))
        {
            return Err("server_name must be 1-32 letters, digits, spaces, '-', '_' or '.'".into());
        }
        Ok(())
    }
}

/// `spec_inline` of a `gameserver.stop` offer.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StopSpec {
    pub v: u32,
    pub id: String,
    pub token: String,
}

pub fn valid_server_id(id: &str) -> bool {
    id.strip_prefix("gs-").is_some_and(|h| is_hex(h, 16))
}

impl StopSpec {
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.v != 1 {
            return Err(format!("unsupported gameserver spec version {}", self.v));
        }
        if !valid_server_id(&self.id) {
            return Err("id must be gs-<16 hex>".into());
        }
        if !is_hex(&self.token, 64) {
            return Err("token must be 64 hex".into());
        }
        Ok(())
    }
}

pub fn sha256_ref(b: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(b)))
}

/// The server's id, derived from the start job's id.
pub fn server_id(job_id: &str) -> String {
    format!("gs-{}", &hex::encode(Sha256::digest(job_id.as_bytes()))[..16])
}

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn expires_at(start: u64, hours: u32) -> u64 {
    start + u64::from(hours) * 3600
}

// ── container ───────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct ContainerPlan {
    pub name: String,
    pub volume: String,
    pub game: Game,
    pub memory_mb: u64,
    pub cpus: f64,
    /// First host port; the game's ports map onto consecutive host ports.
    pub host_port: u16,
    pub env: Vec<(String, String)>,
    pub labels: Vec<(String, String)>,
}

impl ContainerPlan {
    pub fn new(id: &str, spec: &StartSpec, host_port: u16, expires: u64) -> Self {
        let memory_mb = u64::from(spec.ram_gb) * 1024;
        let name = format!("c0mpute-{id}");
        let mut env: Vec<(String, String)> = Vec::new();
        let label = spec.server_name.clone().unwrap_or_else(|| format!("c0mpute {id}"));
        match spec.game {
            Game::Minecraft => {
                env.push(("EULA".into(), if spec.accept_eula { "TRUE" } else { "FALSE" }.into()));
                // JVM heap below the container limit, leaving room for the rest of the JVM.
                env.push(("MEMORY".into(), format!("{}M", memory_mb.saturating_sub(768).max(1024))));
                env.push(("MOTD".into(), label));
            }
            Game::Factorio => {}
            Game::Valheim => {
                env.push(("SERVER_NAME".into(), label));
                env.push(("WORLD_NAME".into(), "c0mpute".into()));
                env.push(("SERVER_PASS".into(), spec.password.clone().unwrap_or_default()));
                env.push(("SERVER_PUBLIC".into(), "false".into()));
                env.push(("SERVER_PORT".into(), "2456".into()));
            }
        }
        let labels = vec![
            ("c0mpute.gameserver".into(), "1".into()),
            ("c0mpute.gameserver.id".into(), id.to_string()),
            ("c0mpute.gameserver.game".into(), spec.game.name().into()),
            ("c0mpute.gameserver.expires".into(), expires.to_string()),
        ];
        Self { volume: name.clone(), name, game: spec.game, memory_mb, cpus: spec.cpus, host_port, env, labels }
    }

    /// `docker create` argv (without the leading `docker`). Every value is its
    /// own argument: nothing passes through a shell.
    pub fn create_argv(&self) -> Vec<String> {
        let mut a: Vec<String> = vec!["create".into(), "--name".into(), self.name.clone()];
        for (k, v) in [
            ("--restart", "no".to_string()),
            ("--memory", format!("{}m", self.memory_mb)),
            ("--memory-swap", format!("{}m", self.memory_mb)),
            ("--cpus", format!("{}", self.cpus)),
            ("--pids-limit", "1024".into()),
            ("--security-opt", "no-new-privileges".into()),
            ("--log-opt", "max-size=10m".into()),
            ("--log-opt", "max-file=2".into()),
            ("--stop-timeout", "60".into()),
        ] {
            a.push(k.into());
            a.push(v);
        }
        for (k, v) in &self.labels {
            a.push("--label".into());
            a.push(format!("{k}={v}"));
        }
        for (i, (port, proto)) in self.game.ports().iter().enumerate() {
            a.push("-p".into());
            a.push(format!("{}:{port}/{}", self.host_port + i as u16, proto.as_str()));
        }
        a.push("-v".into());
        a.push(format!("{}:{}", self.volume, self.game.data_dir()));
        for (k, v) in &self.env {
            a.push("-e".into());
            a.push(format!("{k}={v}"));
        }
        a.push(self.game.image().into());
        a
    }
}

async fn docker(args: &[&str], timeout: Duration) -> Result<String> {
    let out = tokio::time::timeout(timeout, tokio::process::Command::new("docker").args(args).stdin(Stdio::null()).kill_on_drop(true).output())
        .await
        .with_context(|| format!("docker {} timed out", args.first().unwrap_or(&"")))?
        .context("run docker")?;
    if !out.status.success() {
        bail!("docker {}: {}", args.first().unwrap_or(&""), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn docker_sync_ok(args: &[&str]) -> bool {
    std::process::Command::new("docker").args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

// ── public address ──────────────────────────────────────────────────────

fn public_hostname(h: &str) -> Option<String> {
    let h = h.trim().trim_start_matches('[').trim_end_matches(']');
    if h.is_empty() {
        return None;
    }
    if let Ok(ip) = h.parse::<IpAddr>() {
        return crate::net_guard::is_public_ip(ip).then(|| ip.to_string());
    }
    let lower = h.to_ascii_lowercase();
    let dns_ok = lower.contains('.')
        && !lower.ends_with(".local")
        && !lower.ends_with(".localhost")
        && !lower.ends_with(".internal")
        && lower.split('.').all(|l| !l.is_empty() && l.len() <= 63 && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    dns_ok.then_some(lower)
}

/// The address players use: `[gameserver] public_host`, else the gateway's
/// announce URL host, else the first `C0MPUTE_P2P_EXTERNAL` address. Private,
/// loopback and `.local` names never count.
pub fn public_host(configured: Option<&str>, gateway_announce: Option<&str>, p2p_external: Option<&str>) -> Option<String> {
    if let Some(h) = configured.and_then(public_hostname) {
        return Some(h);
    }
    if let Some(h) = gateway_announce.and_then(|u| url::Url::parse(u).ok()).and_then(|u| u.host_str().map(str::to_string)).and_then(|h| public_hostname(&h)) {
        return Some(h);
    }
    for addr in p2p_external.unwrap_or("").split(',') {
        let mut parts = addr.trim().split('/').filter(|s| !s.is_empty());
        while let Some(p) = parts.next() {
            if matches!(p, "ip4" | "ip6" | "dns" | "dns4" | "dns6")
                && let Some(h) = parts.next().and_then(public_hostname)
            {
                return Some(h);
            }
        }
    }
    None
}

fn host_port_str(host: &str, port: u16) -> String {
    if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") }
}

// ── readiness ───────────────────────────────────────────────────────────

/// Why this node is or is not serving game servers, probed at most once a minute.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Readiness {
    pub docker: bool,
    pub host: Option<String>,
    /// Games whose pinned image is present and runs on this CPU.
    pub games: Vec<Game>,
}

impl Readiness {
    pub fn ready(&self) -> bool {
        self.docker && self.host.is_some() && !self.games.is_empty()
    }
    pub fn tags(&self) -> Vec<String> {
        if !self.ready() {
            return Vec::new();
        }
        let mut t = vec![ROLE_TAG.to_string()];
        t.extend(self.games.iter().map(|g| g.tag()));
        t
    }
}

fn host_from_config(cfg: &crate::Config) -> Option<String> {
    public_host(cfg.gameserver.public_host.as_deref(), cfg.gateway.announce.as_deref(), std::env::var("C0MPUTE_P2P_EXTERNAL").ok().as_deref())
}

pub fn probe(cfg: &crate::Config) -> Readiness {
    let docker = docker_sync_ok(&["info", "--format", "{{.ServerVersion}}"]);
    let games = if docker {
        cfg.gameserver
            .games
            .iter()
            .copied()
            .filter(|g| g.supports_arch(std::env::consts::ARCH) && docker_sync_ok(&["image", "inspect", g.image()]))
            .collect()
    } else {
        Vec::new()
    };
    Readiness { docker, host: host_from_config(cfg), games }
}

static READY: OnceLock<Mutex<Option<(Instant, Readiness)>>> = OnceLock::new();

pub fn readiness_cached(cfg: &crate::Config) -> Readiness {
    let cell = READY.get_or_init(|| Mutex::new(None));
    if let Ok(g) = cell.lock()
        && let Some((at, r)) = g.as_ref()
        && at.elapsed() < Duration::from_secs(60)
    {
        return r.clone();
    }
    let r = probe(cfg);
    if let Ok(mut g) = cell.lock() {
        *g = Some((Instant::now(), r.clone()));
    }
    r
}

/// Capability tags for the advertise loop: empty unless the node can host.
pub fn role_tags(cfg: &crate::Config) -> Vec<String> {
    readiness_cached(cfg).tags()
}

/// Pull the pinned images this node may serve (in the background, at boot).
pub async fn ensure_images(cfg: &crate::config::GameserverConfig) {
    for g in &cfg.games {
        if !g.supports_arch(std::env::consts::ARCH) {
            continue;
        }
        if docker(&["image", "inspect", g.image()], Duration::from_secs(30)).await.is_ok() {
            continue;
        }
        info!(game = g.name(), image = g.image(), "gameserver: pulling image");
        if let Err(e) = docker(&["pull", g.image()], Duration::from_secs(1800)).await {
            warn!(game = g.name(), err = %format!("{e:#}"), "gameserver: image pull failed");
        }
    }
}

// ── worker state ────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Backup {
    /// Blake3 hex of the world manifest.
    pub manifest: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ServerState {
    pub id: String,
    pub game: Game,
    pub container: String,
    pub volume: String,
    pub ram_mb: u64,
    pub host_port: u16,
    pub address: String,
    pub started_at: u64,
    pub expires_at: u64,
    pub stop_hash: String,
    pub buyer: String,
    #[serde(default)]
    pub stopped_at: Option<u64>,
    #[serde(default)]
    pub backup: Option<Backup>,
    #[serde(default)]
    pub backup_error: Option<String>,
}

impl ServerState {
    pub fn running(&self) -> bool {
        self.stopped_at.is_none()
    }
    /// Host ports this server holds.
    pub fn ports(&self) -> std::ops::Range<u16> {
        self.host_port..self.host_port + self.game.ports().len() as u16
    }
}

/// Running servers past their expiry.
pub fn due_for_stop(servers: &HashMap<String, ServerState>, now: u64) -> Vec<String> {
    let mut ids: Vec<String> = servers.values().filter(|s| s.running() && now >= s.expires_at).map(|s| s.id.clone()).collect();
    ids.sort();
    ids
}

/// Can this node take `spec` next to what it already runs?
pub fn has_capacity(cfg: &crate::config::GameserverConfig, total_ram_mb: u64, servers: &HashMap<String, ServerState>, spec: &StartSpec) -> std::result::Result<(), String> {
    if !cfg.games.contains(&spec.game) {
        return Err(format!("{} is not offered here", spec.game.name()));
    }
    if spec.hours > cfg.max_hours.min(MAX_HOURS) {
        return Err(format!("this node rents for at most {} hours", cfg.max_hours));
    }
    let running: Vec<&ServerState> = servers.values().filter(|s| s.running()).collect();
    if running.len() >= cfg.max_servers as usize {
        return Err("all server slots are taken".into());
    }
    let budget_mb = match cfg.max_total_ram_gb {
        0 => total_ram_mb / 2,
        gb => u64::from(gb) * 1024,
    };
    let used: u64 = running.iter().map(|s| s.ram_mb).sum();
    if used + u64::from(spec.ram_gb) * 1024 > budget_mb {
        return Err("not enough reservable RAM".into());
    }
    Ok(())
}

/// First host port in `lo..=hi` that starts a free block of `need` ports.
pub fn allocate_ports(lo: u16, hi: u16, need: usize, used: &HashSet<u16>, bindable: impl Fn(u16) -> bool) -> Option<u16> {
    let need = need.max(1) as u32;
    let mut p = u32::from(lo);
    while p + need - 1 <= u32::from(hi) {
        if (p..p + need).all(|q| !used.contains(&(q as u16)) && bindable(q as u16)) {
            return Some(p as u16);
        }
        p += 1;
    }
    None
}

fn port_bindable(p: u16) -> bool {
    std::net::TcpListener::bind(("0.0.0.0", p)).is_ok() && std::net::UdpSocket::bind(("0.0.0.0", p)).is_ok()
}

// ── world backups ───────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WorldManifest {
    pub v: u32,
    pub kind: String,
    pub game: Game,
    /// A tar of this directory (entries start with its name, e.g. `data/`).
    pub data_dir: String,
    pub bytes: u64,
    pub sha256: String,
    /// Blake3 hex of each chunk, in order.
    pub chunks: Vec<String>,
}

impl WorldManifest {
    pub fn new(game: Game) -> Self {
        Self { v: 1, kind: MANIFEST_KIND.into(), game, data_dir: game.data_dir().into(), bytes: 0, sha256: String::new(), chunks: Vec::new() }
    }
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }
    pub fn from_bytes(b: &[u8]) -> Result<Self> {
        let m: Self = serde_json::from_slice(b).context("world manifest is not JSON")?;
        m.check()?;
        Ok(m)
    }
    pub fn check(&self) -> Result<()> {
        if self.v != 1 || self.kind != MANIFEST_KIND {
            bail!("not a c0mpute game world manifest");
        }
        if self.data_dir != self.game.data_dir() {
            bail!("manifest data_dir does not match {}", self.game.name());
        }
        if self.bytes > MAX_WORLD_BYTES {
            bail!("world is over {} GiB", MAX_WORLD_BYTES >> 30);
        }
        let expected = self.bytes.div_ceil(CHUNK_BYTES as u64) as usize;
        if self.chunks.len() != expected || !self.chunks.iter().all(|c| is_hex(c, 64)) || !valid_sha256_ref(&self.sha256) {
            bail!("world manifest chunk list is inconsistent");
        }
        Ok(())
    }
}

/// Read `r` to the end into 1 MiB chunks in the store; returns the manifest
/// (not yet stored). Chunks are announced so any peer can fetch them.
pub async fn chunk_into_store<R: tokio::io::AsyncRead + Unpin>(store: &ChunkStore, net: Option<&Libp2pNetwork>, game: Game, mut r: R) -> Result<WorldManifest> {
    let mut m = WorldManifest::new(game);
    let mut sha = Sha256::new();
    let mut buf = vec![0u8; CHUNK_BYTES];
    loop {
        let mut filled = 0;
        while filled < CHUNK_BYTES {
            let n = r.read(&mut buf[filled..]).await?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        if filled == 0 {
            break;
        }
        m.bytes += filled as u64;
        if m.bytes > MAX_WORLD_BYTES {
            bail!("world is over {} GiB", MAX_WORLD_BYTES >> 30);
        }
        sha.update(&buf[..filled]);
        let h = store.put(&buf[..filled]).await?;
        if let Some(net) = net {
            let _ = net.announce(&h).await;
        }
        m.chunks.push(h.to_hex());
        if filled < CHUNK_BYTES {
            break;
        }
    }
    m.sha256 = format!("sha256:{}", hex::encode(sha.finalize()));
    Ok(m)
}

/// Store and announce a manifest; returns its hash.
pub async fn put_manifest(store: &ChunkStore, net: &Libp2pNetwork, m: &WorldManifest) -> Result<String> {
    let h = store.put(&m.to_bytes()).await?;
    net.announce(&h).await?;
    Ok(h.to_hex())
}

async fn fetch_bytes(net: &Libp2pNetwork, store: &ChunkStore, hash: &str, provider: &str, max: usize) -> Result<Vec<u8>> {
    let h = Hash::from_hex(hash).context("bad hash")?;
    if store.has(&h).await
        && let Ok(b) = store.get(&h).await
        && Hash::of(&b) == h
    {
        return Ok(b);
    }
    crate::gateway_util::fetch_verified(net, hash, provider, max, Duration::from_secs(60)).await.map_err(|e| anyhow::anyhow!("fetch {hash} from {provider}: {e:?}"))
}

/// Fetch a world (manifest, then each chunk) from `provider` (or any peer
/// that announced it), verifying every chunk and the whole tar's sha256,
/// and write the tar to `out` as it arrives.
pub async fn fetch_world<W: tokio::io::AsyncWrite + Unpin>(net: &Libp2pNetwork, store: &ChunkStore, manifest_hash: &str, provider: &str, mut out: W) -> Result<WorldManifest> {
    let mb = fetch_bytes(net, store, manifest_hash, provider, MAX_MANIFEST_BYTES).await.context("world manifest")?;
    let m = WorldManifest::from_bytes(&mb)?;
    let mut sha = Sha256::new();
    for c in &m.chunks {
        let b = fetch_bytes(net, store, c, provider, CHUNK_BYTES).await?;
        sha.update(&b);
        out.write_all(&b).await?;
    }
    out.shutdown().await?;
    if format!("sha256:{}", hex::encode(sha.finalize())) != m.sha256 {
        bail!("world tar does not match the manifest's sha256");
    }
    Ok(m)
}

// ── worker ──────────────────────────────────────────────────────────────

pub struct Worker {
    net: Arc<Libp2pNetwork>,
    store: ChunkStore,
    cfg: crate::config::GameserverConfig,
    host: String,
    state_path: PathBuf,
    servers: Mutex<HashMap<String, ServerState>>,
    /// Servers a start, stop or reap is working on right now.
    busy: Mutex<HashSet<String>>,
}

impl Worker {
    pub fn new(net: Arc<Libp2pNetwork>, store: ChunkStore, cfg: crate::config::GameserverConfig, host: String, state_dir: &Path) -> Arc<Self> {
        let state_path = state_dir.join("servers.json");
        let servers: HashMap<String, ServerState> = std::fs::read(&state_path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Arc::new(Self { net, store, cfg, host, state_path, servers: Mutex::new(servers), busy: Mutex::new(HashSet::new()) })
    }

    fn snapshot(&self) -> HashMap<String, ServerState> {
        self.servers.lock().map(|g| g.clone()).unwrap_or_default()
    }

    fn save(&self) {
        let snap = self.snapshot();
        if let Some(dir) = self.state_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = self.state_path.with_extension("tmp");
        if std::fs::write(&tmp, serde_json::to_vec_pretty(&snap).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(&tmp, &self.state_path);
        }
    }

    fn claim(&self, id: &str) -> bool {
        self.busy.lock().map(|mut b| b.insert(id.to_string())).unwrap_or(false)
    }
    fn release(&self, id: &str) {
        if let Ok(mut b) = self.busy.lock() {
            b.remove(id);
        }
    }

    pub fn accepts_start(&self, offer: &JobOffer) -> bool {
        let Some(spec) = offer.spec_inline.as_ref().and_then(|v| serde_json::from_value::<StartSpec>(v.clone()).ok()) else {
            return false;
        };
        if spec.validate().is_err() {
            return false;
        }
        let floor = self.cfg.min_usd_per_gb_hour * f64::from(spec.ram_gb) * f64::from(spec.hours);
        if offer.max_price_usd < floor {
            return false;
        }
        let servers = self.snapshot();
        if has_capacity(&self.cfg, crate::text::local_ram_mb(), &servers, &spec).is_err() {
            return false;
        }
        let used: HashSet<u16> = servers.values().filter(|s| s.running()).flat_map(|s| s.ports()).collect();
        allocate_ports(self.cfg.port_min, self.cfg.port_max, spec.game.ports().len(), &used, port_bindable).is_some()
    }

    pub fn accepts_stop(&self, offer: &JobOffer) -> bool {
        let Some(spec) = offer.spec_inline.as_ref().and_then(|v| serde_json::from_value::<StopSpec>(v.clone()).ok()) else {
            return false;
        };
        spec.validate().is_ok() && self.snapshot().get(&spec.id).is_some_and(|s| s.stop_hash == sha256_ref(spec.token.as_bytes()))
    }

    async fn start(&self, job: &AcceptedJob) -> Result<Value> {
        let spec: StartSpec = serde_json::from_value(job.accept.spec_inline.clone().context("accept lacks spec_inline")?)?;
        spec.validate().map_err(|e| anyhow::anyhow!(e))?;
        let id = server_id(&job.accept.job_id);
        // Reserve RAM and ports under the lock, so two starts cannot both take the last slot.
        let (state, plan) = {
            let mut servers = self.servers.lock().map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            if servers.contains_key(&id) {
                bail!("server {id} already exists");
            }
            has_capacity(&self.cfg, crate::text::local_ram_mb(), &servers, &spec).map_err(|e| anyhow::anyhow!(e))?;
            let used: HashSet<u16> = servers.values().filter(|s| s.running()).flat_map(|s| s.ports()).collect();
            let port = allocate_ports(self.cfg.port_min, self.cfg.port_max, spec.game.ports().len(), &used, port_bindable).context("no free port in the range")?;
            let started = now_secs();
            let expires = expires_at(started, spec.hours);
            let plan = ContainerPlan::new(&id, &spec, port, expires);
            let state = ServerState {
                id: id.clone(),
                game: spec.game,
                container: plan.name.clone(),
                volume: plan.volume.clone(),
                ram_mb: plan.memory_mb,
                host_port: port,
                address: host_port_str(&self.host, port),
                started_at: started,
                expires_at: expires,
                stop_hash: spec.stop_hash.clone(),
                buyer: job.accept.buyer_peer_id.clone(),
                stopped_at: None,
                backup: None,
                backup_error: None,
            };
            servers.insert(id.clone(), state.clone());
            (state, plan)
        };
        self.claim(&id);
        let res = self.create_and_start(&plan, &spec, &job.accept.buyer_peer_id).await;
        self.release(&id);
        if let Err(e) = res {
            let _ = docker(&["rm", "-f", &plan.name], Duration::from_secs(60)).await;
            let _ = docker(&["volume", "rm", &plan.volume], Duration::from_secs(60)).await;
            if let Ok(mut s) = self.servers.lock() {
                s.remove(&id);
            }
            return Err(e);
        }
        self.save();
        info!(%id, game = spec.game.name(), address = %state.address, expires_at = state.expires_at, "gameserver: started");
        Ok(json!({
            "v": 1, "ok": true, "id": id, "game": spec.game.name(), "address": state.address,
            "ports": spec.game.ports().iter().enumerate().map(|(i, (p, proto))| json!({"host": plan.host_port + i as u16, "container": p, "proto": proto.as_str()})).collect::<Vec<_>>(),
            "expires_at": state.expires_at, "image": spec.game.image(), "restored": spec.world,
        }))
    }

    async fn create_and_start(&self, plan: &ContainerPlan, spec: &StartSpec, buyer: &str) -> Result<()> {
        let argv = plan.create_argv();
        let args: Vec<&str> = argv.iter().map(String::as_str).collect();
        docker(&args, Duration::from_secs(1800)).await?;
        if let Some(world) = &spec.world {
            // `docker cp - <c>:/` extracts the tar (entries `data/...`) into the volume.
            let mut child = tokio::process::Command::new("docker")
                .args(["cp", "-", &format!("{}:/", plan.name)])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .context("spawn docker cp")?;
            let stdin = child.stdin.take().context("docker cp stdin")?;
            let m = fetch_world(&self.net, &self.store, world, buyer, stdin).await.context("restore world")?;
            if m.game != spec.game {
                bail!("world is a {} world, not {}", m.game.name(), spec.game.name());
            }
            let out = child.wait_with_output().await?;
            if !out.status.success() {
                bail!("docker cp into {}: {}", plan.name, String::from_utf8_lossy(&out.stderr).trim());
            }
        }
        docker(&["start", &plan.name], Duration::from_secs(120)).await?;
        Ok(())
    }

    /// Stop a server, back its world up into the store, remove the container
    /// and volume. A failed backup leaves the volume in place.
    async fn stop_and_backup(&self, id: &str) -> Result<Option<Backup>> {
        let Some(s) = self.snapshot().get(id).cloned() else { bail!("no server {id}") };
        if !s.running() {
            return Ok(s.backup);
        }
        if !self.claim(id) {
            bail!("server {id} is busy");
        }
        let res = async {
            let _ = docker(&["stop", "-t", "60", &s.container], Duration::from_secs(120)).await;
            let backup = self.backup(&s).await;
            if backup.is_ok() {
                let _ = docker(&["rm", "-f", &s.container], Duration::from_secs(60)).await;
                let _ = docker(&["volume", "rm", &s.volume], Duration::from_secs(60)).await;
            }
            backup
        }
        .await;
        self.release(id);
        if let Ok(mut servers) = self.servers.lock()
            && let Some(e) = servers.get_mut(id)
        {
            e.stopped_at = Some(now_secs());
            match &res {
                Ok(b) => e.backup = Some(b.clone()),
                Err(err) => e.backup_error = Some(format!("{err:#}")),
            }
        }
        self.save();
        res.map(Some)
    }

    async fn backup(&self, s: &ServerState) -> Result<Backup> {
        let mut child = tokio::process::Command::new("docker")
            .args(["cp", &format!("{}:{}", s.container, s.game.data_dir()), "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("spawn docker cp")?;
        let stdout = child.stdout.take().context("docker cp stdout")?;
        let m = chunk_into_store(&self.store, Some(&self.net), s.game, stdout).await?;
        let out = child.wait_with_output().await?;
        if !out.status.success() {
            bail!("docker cp out of {}: {}", s.container, String::from_utf8_lossy(&out.stderr).trim());
        }
        let manifest = put_manifest(&self.store, &self.net, &m).await?;
        info!(id = %s.id, %manifest, bytes = m.bytes, "gameserver: world backed up");
        Ok(Backup { manifest, bytes: m.bytes, sha256: m.sha256 })
    }

    async fn stop(&self, job: &AcceptedJob) -> Result<Value> {
        let spec: StopSpec = serde_json::from_value(job.accept.spec_inline.clone().context("accept lacks spec_inline")?)?;
        spec.validate().map_err(|e| anyhow::anyhow!(e))?;
        let s = self.snapshot().get(&spec.id).cloned().context("unknown server")?;
        if s.stop_hash != sha256_ref(spec.token.as_bytes()) {
            bail!("stop token does not match");
        }
        let backup = self.stop_and_backup(&spec.id).await?;
        Ok(json!({"v": 1, "ok": true, "id": spec.id, "game": s.game.name(), "backup": backup}))
    }

    /// Stop servers whose reservation ended; forget old stopped ones.
    pub async fn reap_once(&self) {
        let now = now_secs();
        for id in due_for_stop(&self.snapshot(), now) {
            info!(%id, "gameserver: reservation ended, stopping");
            if let Err(e) = self.stop_and_backup(&id).await {
                warn!(%id, err = %format!("{e:#}"), "gameserver: stop at expiry failed");
            }
        }
        let mut changed = false;
        if let Ok(mut s) = self.servers.lock() {
            let before = s.len();
            s.retain(|_, v| v.stopped_at.is_none_or(|t| now.saturating_sub(t) < KEEP_STOPPED_SECS));
            changed = s.len() != before;
        }
        if changed {
            self.save();
        }
    }

    async fn publish_result(&self, v: &Value) -> Result<String> {
        let h = self.store.put(&serde_json::to_vec(v)?).await?;
        self.net.announce(&h).await?;
        Ok(h.to_hex())
    }
}

fn spawn_runner(worker: Arc<Worker>, slots: Arc<Semaphore>, stop: bool) -> mpsc::Sender<AcceptedJob> {
    let (tx, mut rx) = mpsc::channel::<AcceptedJob>(8);
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let Ok(slot) = slots.clone().acquire_owned().await else { return };
            let w = worker.clone();
            tokio::spawn(async move {
                let _slot = slot;
                let res = if stop { w.stop(&job).await } else { w.start(&job).await };
                let res = match res {
                    Ok(v) => w.publish_result(&v).await,
                    Err(e) => Err(e),
                };
                match res {
                    Ok(hash) => publish_receipt(&w.net, &job.accept, Some(hash), JobStatus::Completed).await,
                    Err(e) => {
                        warn!(job_id = %job.accept.job_id, stop, err = %format!("{e:#}"), "gameserver: job failed");
                        publish_receipt(&w.net, &job.accept, None, JobStatus::Failed).await;
                    }
                }
            });
        }
    });
    tx
}

/// Supervisor entry: pull images, then subscribe to start and stop offers and
/// run the expiry reaper. Returns without subscribing when the node cannot host.
pub async fn run_worker(net: Arc<Libp2pNetwork>, store: ChunkStore, config: crate::Config, state_dir: PathBuf) {
    if !docker_sync_ok(&["info", "--format", "{{.ServerVersion}}"]) {
        info!("gameserver: docker is not usable here; not serving game servers");
        return;
    }
    let Some(host) = host_from_config(&config) else {
        info!("gameserver: no public address (set [gameserver] public_host, gateway.announce or C0MPUTE_P2P_EXTERNAL); not serving game servers");
        return;
    };
    let worker = Worker::new(net.clone(), store, config.gameserver.clone(), host, &state_dir);
    // Enforce expiries even before the images finish pulling.
    let reaper = worker.clone();
    tokio::spawn(async move {
        loop {
            reaper.reap_once().await;
            tokio::time::sleep(REAP_EVERY).await;
        }
    });
    ensure_images(&config.gameserver).await;
    let ready = probe(&config);
    if !ready.ready() {
        info!(?ready, "gameserver: no game image available; not serving game servers");
        return;
    }
    if let Ok(mut g) = READY.get_or_init(|| Mutex::new(None)).lock() {
        *g = Some((Instant::now(), ready.clone()));
    }
    let tags = crate::capabilities::tags_from_config(&config);
    let slots = Arc::new(Semaphore::new(JOB_SLOTS));
    let load = WorkerLoad::new(slots.clone(), JOB_SLOTS);
    let w = worker.clone();
    let start_filter: crate::dispatch::OfferFilter = Arc::new(move |o| w.accepts_start(o));
    let w = worker.clone();
    let stop_filter: crate::dispatch::OfferFilter = Arc::new(move |o| w.accepts_stop(o));
    info!(games = ?ready.games, host = ?ready.host, "gameserver: serving");
    crate::dispatch::run_worker_subscriber_with_load(
        net.clone(),
        WORKLOAD_START.into(),
        tags.clone(),
        Some(spawn_runner(worker.clone(), slots.clone(), false)),
        Some(start_filter),
        Some(config.pricing.clone()),
        Some(load.clone()),
    );
    // Stopping is part of the rent: no price floor.
    crate::dispatch::run_worker_subscriber_with_load(net, WORKLOAD_STOP.into(), tags, Some(spawn_runner(worker, slots, true)), Some(stop_filter), None, Some(load));
}

// ── buyer ───────────────────────────────────────────────────────────────

/// What the buyer keeps about a server it rented (`gameserver ls`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Rental {
    pub id: String,
    pub game: Game,
    pub address: String,
    pub worker: String,
    pub expires_at: u64,
    pub started_at: u64,
    /// Proves ownership to the worker on stop. Never leaves this machine until then.
    pub token: String,
    pub job_id: String,
    pub price_usd: f64,
    #[serde(default)]
    pub stopped_at: Option<u64>,
    #[serde(default)]
    pub backup: Option<String>,
}

pub fn buyer_dir() -> PathBuf {
    crate::config::data_dir().unwrap_or_else(|| PathBuf::from("./c0mpute-data")).join("gameserver")
}

pub fn load_rentals(dir: &Path) -> Vec<Rental> {
    std::fs::read(dir.join("rentals.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save_rentals(dir: &Path, rentals: &[Rental]) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let p = dir.join("rentals.json");
    std::fs::write(&p, serde_json::to_vec_pretty(rentals)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub struct StartRequest {
    pub game: Game,
    pub hours: u32,
    pub ram_gb: u32,
    pub cpus: f64,
    pub accept_eula: bool,
    /// A manifest hash from an earlier stop, or a `.tar` laid out like a backup.
    pub world: Option<String>,
    pub password: Option<String>,
    pub server_name: Option<String>,
    pub max_price_usd: f64,
    pub workers: Vec<String>,
    pub timeout: Duration,
}

async fn auction(net: Arc<Libp2pNetwork>, workload: &str, spec: Value, caps: Vec<String>, workers: Vec<String>, max_price: f64, timeout: Duration) -> Result<(crate::buyer::AuctionOutcome, Value)> {
    let a = crate::JobAuction::new(workload, spec)
        .with_required_capabilities(caps)
        .with_allowed_bidders(workers)
        .with_max_price_usd(max_price)
        .with_bid_window(Duration::from_secs(4))
        .with_deadline(timeout)
        .with_receipt_timeout(timeout);
    let o = crate::run_auction(net.clone(), a).await?;
    if o.receipt.status != JobStatus::Completed {
        bail!("worker {} reported {:?}", o.accepted_bid.bidder_peer_id, o.receipt.status);
    }
    let hash = o.receipt.output_hash.clone().context("worker returned no result hash")?;
    let bytes = crate::gateway_util::fetch_result(&net, &hash, &o.accepted_bid.bidder_peer_id, 64 * 1024, Duration::from_secs(30)).await.map_err(|e| anyhow::anyhow!("fetch result: {e:?}"))?;
    let v: Value = serde_json::from_slice(&bytes)?;
    if v["ok"] != json!(true) {
        bail!("worker result: {v}");
    }
    Ok((o, v))
}

/// Put a world into the local store for the worker to fetch; returns the manifest hash.
async fn publish_world(store: &ChunkStore, net: &Libp2pNetwork, dir: &Path, game: Game, world: &str) -> Result<String> {
    let (tar, saved_manifest) = if is_hex(world, 64) {
        (dir.join("worlds").join(format!("{world}.tar")), Some(dir.join("worlds").join(format!("{world}.json"))))
    } else {
        (PathBuf::from(world), None)
    };
    let f = tokio::fs::File::open(&tar).await.with_context(|| format!("open world {}", tar.display()))?;
    let m = chunk_into_store(store, Some(net), game, f).await?;
    if let Some(p) = saved_manifest {
        let want = WorldManifest::from_bytes(&std::fs::read(&p).with_context(|| format!("read {}", p.display()))?)?;
        if want != m {
            bail!("{} does not match its manifest", tar.display());
        }
        if want.game != game {
            bail!("that world is a {} world", want.game.name());
        }
    }
    put_manifest(store, net, &m).await
}

/// Rent a server: auction it, wait for the address, remember the rental.
pub async fn start(req: StartRequest) -> Result<Rental> {
    let token = {
        let b: [u8; 32] = rand::random();
        hex::encode(b)
    };
    let sup = crate::Supervisor::boot_buyer(crate::Config::default()).await?;
    let dir = buyer_dir();
    let world = match &req.world {
        Some(w) => Some(publish_world(&sup.store, &sup.libp2p, &dir, req.game, w).await?),
        None => None,
    };
    let spec = StartSpec {
        v: 1,
        game: req.game,
        hours: req.hours,
        ram_gb: req.ram_gb,
        cpus: req.cpus,
        accept_eula: req.accept_eula,
        stop_hash: sha256_ref(token.as_bytes()),
        world,
        password: req.password.clone(),
        server_name: req.server_name.clone(),
    };
    spec.validate().map_err(|e| anyhow::anyhow!(e))?;
    tokio::time::sleep(Duration::from_secs(5)).await;
    let (o, v) = auction(sup.libp2p.clone(), WORKLOAD_START, serde_json::to_value(&spec)?, vec![ROLE_TAG.into(), req.game.tag()], req.workers.clone(), req.max_price_usd, req.timeout).await?;
    let rental = Rental {
        id: v["id"].as_str().context("result has no id")?.to_string(),
        game: req.game,
        address: v["address"].as_str().context("result has no address")?.to_string(),
        worker: o.accepted_bid.bidder_peer_id.clone(),
        expires_at: v["expires_at"].as_u64().context("result has no expires_at")?,
        started_at: now_secs(),
        token,
        job_id: o.job_id.clone(),
        price_usd: o.accepted_bid.price_usd,
        stopped_at: None,
        backup: None,
    };
    let mut all = load_rentals(&dir);
    all.push(rental.clone());
    save_rentals(&dir, &all)?;
    Ok(rental)
}

/// Stop a rental early (or collect the backup of one that expired) and
/// download its world to `<buyer_dir>/worlds/<hash>.tar`.
pub async fn stop(id: &str, timeout: Duration) -> Result<Rental> {
    let dir = buyer_dir();
    let mut all = load_rentals(&dir);
    let idx = all.iter().position(|r| r.id == id).with_context(|| format!("no rental {id} on this machine (see `c0mpute gameserver ls`)"))?;
    let r = all[idx].clone();
    let sup = crate::Supervisor::boot_buyer(crate::Config::default()).await?;
    tokio::time::sleep(Duration::from_secs(5)).await;
    let spec = StopSpec { v: 1, id: r.id.clone(), token: r.token.clone() };
    let (_, v) = auction(sup.libp2p.clone(), WORKLOAD_STOP, serde_json::to_value(&spec)?, vec![ROLE_TAG.into()], vec![r.worker.clone()], 0.0, timeout).await?;
    let backup: Option<Backup> = serde_json::from_value(v["backup"].clone()).ok().flatten();
    let mut updated = r.clone();
    updated.stopped_at = Some(now_secs());
    if let Some(b) = &backup {
        let worlds = dir.join("worlds");
        tokio::fs::create_dir_all(&worlds).await?;
        let tmp = worlds.join(format!("{}.partial", b.manifest));
        let f = tokio::fs::File::create(&tmp).await?;
        let m = fetch_world(&sup.libp2p, &sup.store, &b.manifest, &r.worker, f).await.context("download world backup")?;
        tokio::fs::rename(&tmp, worlds.join(format!("{}.tar", b.manifest))).await?;
        tokio::fs::write(worlds.join(format!("{}.json", b.manifest)), m.to_bytes()).await?;
        updated.backup = Some(b.manifest.clone());
    }
    all[idx] = updated.clone();
    save_rentals(&dir, &all)?;
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(game: Game) -> StartSpec {
        StartSpec {
            v: 1,
            game,
            hours: 4,
            ram_gb: 4,
            cpus: 2.0,
            accept_eula: true,
            stop_hash: sha256_ref(b"t"),
            world: None,
            password: (game == Game::Valheim).then(|| "hunter22".into()),
            server_name: None,
        }
    }

    #[test]
    fn spec_validation() {
        for g in Game::ALL {
            assert!(spec(g).validate().is_ok(), "{g:?}");
        }
        let bad = |f: &dyn Fn(&mut StartSpec)| {
            let mut s = spec(Game::Minecraft);
            f(&mut s);
            s.validate().is_err()
        };
        assert!(bad(&|s| s.accept_eula = false), "minecraft needs the EULA");
        assert!(bad(&|s| s.hours = 0));
        assert!(bad(&|s| s.hours = MAX_HOURS + 1));
        assert!(bad(&|s| s.ram_gb = 1));
        assert!(bad(&|s| s.ram_gb = MAX_RAM_GB + 1));
        assert!(bad(&|s| s.cpus = 0.1));
        assert!(bad(&|s| s.cpus = f64::INFINITY));
        assert!(bad(&|s| s.cpus = 64.0));
        assert!(bad(&|s| s.stop_hash = "sha256:XYZ".into()));
        assert!(bad(&|s| s.world = Some("../etc".into())));
        assert!(bad(&|s| s.password = Some("hunter22".into())), "only valheim takes a password");
        assert!(bad(&|s| s.server_name = Some("$(rm -rf /)".into())));
        assert!(bad(&|s| s.v = 2));
        // Factorio does not need the EULA flag; Valheim needs a password.
        let mut f = spec(Game::Factorio);
        f.accept_eula = false;
        assert!(f.validate().is_ok());
        let mut v = spec(Game::Valheim);
        v.password = Some("abc".into());
        assert!(v.validate().is_err());
        v.ram_gb = 2;
        v.password = Some("hunter22".into());
        assert!(v.validate().is_err(), "valheim needs 4 GB");
        // Unknown games never parse.
        let mut raw = serde_json::to_value(spec(Game::Minecraft)).unwrap();
        raw["game"] = json!("doom");
        assert!(serde_json::from_value::<StartSpec>(raw).is_err());
        assert_eq!(Game::parse(" Minecraft"), Some(Game::Minecraft));
        assert_eq!(Game::parse("terraria"), None);
    }

    #[test]
    fn stop_spec_validation() {
        let ok = StopSpec { v: 1, id: server_id("job"), token: "a".repeat(64) };
        assert!(ok.validate().is_ok());
        assert!(valid_server_id(&ok.id));
        assert!(StopSpec { id: "gs-../../x".into(), ..ok.clone() }.validate().is_err());
        assert!(StopSpec { token: "short".into(), ..ok.clone() }.validate().is_err());
    }

    #[test]
    fn docker_argv_has_limits_ports_volume_and_no_shell() {
        let s = spec(Game::Minecraft);
        let plan = ContainerPlan::new("gs-0123456789abcdef", &s, 27500, 1_000);
        let a = plan.create_argv();
        let pair = |k: &str, v: &str| a.windows(2).any(|w| w[0] == k && w[1] == v);
        assert_eq!(a[0], "create");
        assert!(pair("--name", "c0mpute-gs-0123456789abcdef"));
        assert!(pair("--memory", "4096m"));
        assert!(pair("--memory-swap", "4096m"));
        assert!(pair("--cpus", "2"));
        assert!(pair("--pids-limit", "1024"));
        assert!(pair("--restart", "no"));
        assert!(pair("--security-opt", "no-new-privileges"));
        assert!(pair("-p", "27500:25565/tcp"));
        assert!(pair("-v", "c0mpute-gs-0123456789abcdef:/data"));
        assert!(pair("-e", "EULA=TRUE"));
        assert!(pair("-e", "MEMORY=3328M"));
        assert!(pair("--label", "c0mpute.gameserver.expires=1000"));
        assert_eq!(a.last().unwrap(), Game::Minecraft.image());
        assert!(!a.iter().any(|x| x == "sh" || x == "-c" || x == "--privileged" || x.starts_with("--network=host")));
        // Valheim maps two consecutive UDP ports and passes its password as one argument.
        let v = ContainerPlan::new("gs-0123456789abcdef", &spec(Game::Valheim), 27510, 1).create_argv();
        assert!(v.windows(2).any(|w| w[0] == "-p" && w[1] == "27510:2456/udp"));
        assert!(v.windows(2).any(|w| w[0] == "-p" && w[1] == "27511:2457/udp"));
        assert!(v.windows(2).any(|w| w[0] == "-e" && w[1] == "SERVER_PASS=hunter22"));
        assert!(v.windows(2).any(|w| w[0] == "-v" && w[1] == "c0mpute-gs-0123456789abcdef:/config"));
        // Images are pinned by digest.
        for g in Game::ALL {
            assert!(g.image().contains("@sha256:"), "{g:?}");
        }
    }

    fn state(id: &str, ram_mb: u64, port: u16, expires: u64, stopped: Option<u64>) -> ServerState {
        ServerState {
            id: id.into(),
            game: Game::Minecraft,
            container: format!("c0mpute-{id}"),
            volume: format!("c0mpute-{id}"),
            ram_mb,
            host_port: port,
            address: format!("1.2.3.4:{port}"),
            started_at: 0,
            expires_at: expires,
            stop_hash: sha256_ref(b"t"),
            buyer: "b".into(),
            stopped_at: stopped,
            backup: None,
            backup_error: None,
        }
    }

    #[test]
    fn expiry() {
        assert_eq!(expires_at(100, 4), 100 + 4 * 3600);
        let mut m = HashMap::new();
        m.insert("a".into(), state("a", 1024, 1, 1_000, None));
        m.insert("b".into(), state("b", 1024, 2, 5_000, None));
        m.insert("c".into(), state("c", 1024, 3, 10, Some(20)));
        assert!(due_for_stop(&m, 999).is_empty());
        assert_eq!(due_for_stop(&m, 1_000), vec!["a".to_string()]);
        assert_eq!(due_for_stop(&m, 9_999), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn capacity_and_ports() {
        let cfg = crate::config::GameserverConfig { max_servers: 2, max_total_ram_gb: 8, max_hours: 24, ..Default::default() };
        let mut m = HashMap::new();
        assert!(has_capacity(&cfg, 0, &m, &spec(Game::Minecraft)).is_ok());
        let mut long = spec(Game::Minecraft);
        long.hours = 48;
        assert!(has_capacity(&cfg, 0, &m, &long).is_err());
        m.insert("a".into(), state("a", 6 * 1024, 27500, 9, None));
        assert!(has_capacity(&cfg, 0, &m, &spec(Game::Minecraft)).unwrap_err().contains("RAM"));
        m.insert("a".into(), state("a", 2 * 1024, 27500, 9, None));
        m.insert("b".into(), state("b", 1024, 27501, 9, None));
        assert!(has_capacity(&cfg, 0, &m, &spec(Game::Minecraft)).unwrap_err().contains("slots"));
        m.get_mut("b").unwrap().stopped_at = Some(1);
        assert!(has_capacity(&cfg, 0, &m, &spec(Game::Minecraft)).is_ok(), "stopped servers free their slot");
        // Default budget is half the box.
        let half = crate::config::GameserverConfig { max_total_ram_gb: 0, ..cfg.clone() };
        assert!(has_capacity(&half, 6 * 1024, &HashMap::new(), &spec(Game::Minecraft)).is_err());
        assert!(has_capacity(&half, 16 * 1024, &HashMap::new(), &spec(Game::Minecraft)).is_ok());
        let used: HashSet<u16> = [27500, 27502].into_iter().collect();
        assert_eq!(allocate_ports(27500, 27510, 1, &used, |_| true), Some(27501));
        assert_eq!(allocate_ports(27500, 27510, 2, &used, |_| true), Some(27503));
        assert_eq!(allocate_ports(27500, 27503, 2, &used, |p| p != 27503), None);
        assert_eq!(allocate_ports(65535, 65535, 2, &HashSet::new(), |_| true), None);
    }

    #[test]
    fn public_address_sources() {
        assert_eq!(public_host(Some("play.example.com"), None, None).as_deref(), Some("play.example.com"));
        assert_eq!(public_host(Some("192.168.1.5"), Some("https://gw.example.net"), None).as_deref(), Some("gw.example.net"));
        assert_eq!(public_host(None, Some("http://127.0.0.1:7777"), Some("/ip4/8.8.8.8/tcp/4001")).as_deref(), Some("8.8.8.8"));
        assert_eq!(public_host(None, None, Some("/ip4/10.0.0.2/tcp/1,/dns4/node.example.org/tcp/4001")).as_deref(), Some("node.example.org"));
        assert_eq!(public_host(Some("localhost"), Some("http://box.local"), Some("/ip4/127.0.0.1/tcp/1")), None);
        assert_eq!(public_host(None, None, None), None);
        assert_eq!(host_port_str("2001:db8::1", 5), "[2001:db8::1]:5");
    }

    #[test]
    fn manifest_checks() {
        let mut m = WorldManifest::new(Game::Factorio);
        m.bytes = CHUNK_BYTES as u64 + 1;
        m.sha256 = sha256_ref(b"x");
        m.chunks = vec!["a".repeat(64), "b".repeat(64)];
        assert!(WorldManifest::from_bytes(&m.to_bytes()).is_ok());
        let mut short = m.clone();
        short.chunks.pop();
        assert!(short.check().is_err());
        let mut wrong_dir = m.clone();
        wrong_dir.data_dir = "/".into();
        assert!(wrong_dir.check().is_err());
        let mut huge = m.clone();
        huge.bytes = MAX_WORLD_BYTES + 1;
        assert!(huge.check().is_err());
    }

    #[tokio::test]
    async fn chunking_round_trips_through_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChunkStore::open(dir.path()).await.unwrap();
        let data: Vec<u8> = (0..(CHUNK_BYTES * 2 + 17)).map(|i| (i % 251) as u8).collect();
        let m = chunk_into_store(&store, None, Game::Minecraft, &data[..]).await.unwrap();
        m.check().unwrap();
        assert_eq!(m.chunks.len(), 3);
        assert_eq!(m.bytes, data.len() as u64);
        assert_eq!(m.sha256, sha256_ref(&data));
        let mut back = Vec::new();
        for c in &m.chunks {
            back.extend(store.get(&Hash::from_hex(c).unwrap()).await.unwrap());
        }
        assert_eq!(back, data);
        let empty = chunk_into_store(&store, None, Game::Minecraft, &b""[..]).await.unwrap();
        assert!(empty.chunks.is_empty() && empty.check().is_ok());
    }
}
