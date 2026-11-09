//! `gameserver.start` / `gameserver.stop`: rent a Minecraft, Factorio or
//! Valheim server on a worker for a fixed number of hours.
//!
//! A start job is an ordinary auction whose job is short: the winner creates
//! a container from a pinned, well-known image (`docker create`, argv only,
//! never a shell) with memory, CPU and pid limits, a named volume for the
//! world and a published port, optionally restores a world the buyer serves
//! by hash, starts it, and returns `address:port` and the expiry.
//!
//! Secrets never ride in the offer. The offer carries the buyer's fresh
//! X25519 key; after the accept the winner announces its own and the buyer
//! seals the Valheim password and the restore key to it ([`crate::handoff`]).
//!
//! Worlds are encrypted. A world the buyer restores is published as AES-GCM
//! chunks under a fresh key that only the winning worker receives. Every
//! backup a worker takes is sealed to the buyer's long-term backup key
//! (X25519 → HKDF → AES-256-GCM per 1 MiB chunk): the manifest hash in a
//! receipt or a status record is useless to anyone else.
//!
//! While a server runs the worker backs the world up every `backup_mins`
//! (default 30) and gossips a status record (container running, latest
//! backup hash) on [`STATUS_TOPIC`], signed by its peer key. The buyer's
//! `gameserver watch` probes the game port (Minecraft status ping, Valheim
//! Steam A2S query, Factorio UDP reachability), logs every probe, mirrors
//! each new backup to its own machine, and when the server fails its checks
//! for `fail_after` it starts the same world on another worker from the
//! latest backup. No coordinator anywhere.
//!
//! Expiry does not depend on the worker being online: the container runs
//! under `timeout` (with `--init`) so it exits by itself when the
//! reservation ends, and `--rm` removes it; the named volume keeps the world
//! until the worker, when it next runs, backs it up and removes it. At boot
//! and on every sweep the worker also kills any labelled container whose
//! reservation has passed.
//!
//! Only opt-in nodes serve it (`--roles ...,gameserver`): it holds a port and
//! RAM for hours. A node advertises it only when `docker info` works and it
//! knows a public address (`[gameserver] public_host`, the gateway's announce
//! URL, or `C0MPUTE_P2P_EXTERNAL`); games appear once their image is pulled.
//! The operator must open the port range in the firewall.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, Result, anyhow, bail};
use c0mpute_net::topics::{JobOffer, JobStatus};
use c0mpute_net::{Libp2pNetwork, Network};
use c0mpute_proto::Hash;
use c0mpute_store::ChunkStore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Semaphore, broadcast, mpsc};
use tracing::{info, warn};
use x25519_dalek::StaticSecret;

use crate::runner::{AcceptedJob, WorkerLoad, publish_receipt};
use crate::temporal::crypto;

pub const WORKLOAD_START: &str = "gameserver.start";
pub const WORKLOAD_STOP: &str = "gameserver.stop";
pub const ROLE_TAG: &str = "c0mpute:role:gameserver";
/// Where workers gossip [`StatusRecord`]s.
pub const STATUS_TOPIC: &str = "c0mpute/gameserver/v1";
pub const SPEC_VERSION: u32 = 2;
pub const MAX_HOURS: u32 = 72;
pub const MAX_RAM_GB: u32 = 32;
pub const MAX_CPUS: f64 = 8.0;
/// World backups are stored and moved as chunks of this size (plaintext).
pub const CHUNK_BYTES: usize = 1024 * 1024;
/// AES-GCM tag on every chunk.
const TAG_BYTES: usize = 16;
pub const MAX_WORLD_BYTES: u64 = 4 * 1024 * 1024 * 1024;
pub const DEFAULT_BACKUP_MINS: u32 = 30;
pub const MIN_BACKUP_MINS: u32 = 5;
pub const MAX_BACKUP_MINS: u32 = 720;
pub const ENC_SCHEME: &str = "x25519-hkdf-sha256-aes256gcm-chunked/v1";
const MANIFEST_KIND: &str = "c0mpute-gameserver-world";
const MANIFEST_VERSION: u32 = 2;
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
/// Start/stop jobs handled at once (the servers themselves are not jobs).
const JOB_SLOTS: usize = 2;
/// Stopped-server records (and their backup hash) are kept this long.
const KEEP_STOPPED_SECS: u64 = 7 * 86_400;
pub const REAP_EVERY: Duration = Duration::from_secs(30);
/// How long a worker waits for the buyer's sealed secrets after the accept.
const HANDOFF_WAIT: Duration = Duration::from_secs(120);
/// `timeout -k`: after the TERM at expiry, KILL this many seconds later.
const EXPIRY_KILL_AFTER_SECS: u64 = 90;

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
    /// The pinned image's own ENTRYPOINT + CMD (read from each image config
    /// at the digest above). The container runs it under `timeout`, so the
    /// server exits at expiry whether or not the worker is online.
    pub fn command(self) -> &'static [&'static str] {
        match self {
            Game::Minecraft => &["/image/scripts/start"],
            Game::Factorio => &["/docker-entrypoint.sh"],
            Game::Valheim => &["/usr/local/sbin/bootstrap"],
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

/// `spec_inline` of a `gameserver.start` offer. Everything in it is public:
/// every subscriber of the job topic can read it.
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
    /// The buyer's fresh X25519 key for this job's [`crate::handoff`].
    pub client_pubkey: String,
    /// The buyer's long-term X25519 key every backup is sealed to.
    pub backup_pubkey: String,
    /// Minutes between live backups; 0 = only at stop or expiry.
    #[serde(default = "default_backup_mins")]
    pub backup_mins: u32,
    /// Manifest hash (blake3 hex) of an encrypted world to restore; its key
    /// arrives in the sealed [`StartSecrets`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub world: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,
}

fn default_backup_mins() -> u32 {
    DEFAULT_BACKUP_MINS
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn valid_sha256_ref(s: &str) -> bool {
    s.strip_prefix("sha256:").is_some_and(|h| is_hex(h, 64))
}

impl StartSpec {
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.v != SPEC_VERSION {
            return Err(format!("unsupported gameserver spec version {} (this node speaks {SPEC_VERSION})", self.v));
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
        if !is_hex(&self.client_pubkey, 64) || !is_hex(&self.backup_pubkey, 64) {
            return Err("client_pubkey and backup_pubkey must be X25519 keys (64 hex)".into());
        }
        if self.backup_mins != 0 && !(MIN_BACKUP_MINS..=MAX_BACKUP_MINS).contains(&self.backup_mins) {
            return Err(format!("backup_mins must be 0 or {MIN_BACKUP_MINS}..={MAX_BACKUP_MINS}"));
        }
        if self.world.as_deref().is_some_and(|w| !is_hex(w, 64)) {
            return Err("world must be a 64-hex manifest hash".into());
        }
        if let Some(n) = &self.server_name
            && (n.is_empty() || n.len() > 32 || !n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.')))
        {
            return Err("server_name must be 1-32 letters, digits, spaces, '-', '_' or '.'".into());
        }
        Ok(())
    }
}

/// What the buyer seals to the winning worker after the accept.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct StartSecrets {
    pub v: u32,
    /// Valheim only (it requires one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Hex AES-256 key of the world named in the spec.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub world_key: Option<String>,
}

impl StartSecrets {
    pub fn validate(&self, spec: &StartSpec) -> std::result::Result<(), String> {
        if self.v != 1 {
            return Err("unsupported secrets version".into());
        }
        match (spec.game, &self.password) {
            (Game::Valheim, Some(p)) if valid_password(p) => {}
            (Game::Valheim, _) => return Err("valheim needs a password of 5-32 letters or digits".into()),
            (_, Some(_)) => return Err("only valheim takes a password".into()),
            _ => {}
        }
        match (&spec.world, &self.world_key) {
            (Some(_), Some(k)) if is_hex(k, 64) => {}
            (Some(_), _) => return Err("a world restore needs its 32-byte key".into()),
            (None, Some(_)) => return Err("a world key without a world".into()),
            (None, None) => {}
        }
        Ok(())
    }

    pub fn world_key(&self) -> Result<Option<[u8; 32]>> {
        self.world_key.as_deref().map(key_from_hex).transpose()
    }
}

pub fn valid_password(p: &str) -> bool {
    (5..=32).contains(&p.len()) && p.chars().all(|c| c.is_ascii_alphanumeric())
}

fn key_from_hex(h: &str) -> Result<[u8; 32]> {
    hex::decode(h)?.try_into().map_err(|_| anyhow!("key must be 32 bytes"))
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
    /// Seconds the server may run before it stops itself (the reservation).
    pub run_secs: u64,
    pub env: Vec<(String, String)>,
    pub labels: Vec<(String, String)>,
}

impl ContainerPlan {
    pub fn new(id: &str, spec: &StartSpec, secrets: &StartSecrets, host_port: u16, now: u64, expires: u64) -> Self {
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
                env.push(("SERVER_PASS".into(), secrets.password.clone().unwrap_or_default()));
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
        Self {
            volume: name.clone(),
            name,
            game: spec.game,
            memory_mb,
            cpus: spec.cpus,
            host_port,
            run_secs: expires.saturating_sub(now).max(1),
            env,
            labels,
        }
    }

    /// `docker create` argv (without the leading `docker`). Every value is its
    /// own argument: nothing passes through a shell.
    ///
    /// The image's own command runs under `timeout <run_secs>` behind
    /// `--init`, and `--rm` removes the container when it exits: the server
    /// ends at expiry even if this worker is offline then. The world lives
    /// in the named volume, which `--rm` keeps.
    pub fn create_argv(&self) -> Vec<String> {
        let mut a: Vec<String> = vec!["create".into(), "--name".into(), self.name.clone(), "--rm".into(), "--init".into()];
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
            ("--entrypoint", "timeout".into()),
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
        // Arguments to `timeout`: TERM at expiry, KILL if still alive after.
        for x in ["-s".to_string(), "TERM".into(), "-k".into(), format!("{EXPIRY_KILL_AFTER_SECS}s"), format!("{}s", self.run_secs)] {
            a.push(x);
        }
        a.extend(self.game.command().iter().map(|s| s.to_string()));
        a
    }
}

/// What a worker needs from a container runtime. [`Docker`] in production;
/// the loopback end-to-end tests supply a double.
#[async_trait::async_trait]
pub trait Runtime: Send + Sync + 'static {
    /// Create (not start) the container and its volume.
    async fn create(&self, plan: &ContainerPlan) -> Result<()>;
    /// Extract a world tar (entries under the data dir's name) into the
    /// created container's volume.
    async fn restore(&self, plan: &ContainerPlan, tar: &Path) -> Result<()>;
    async fn start(&self, name: &str) -> Result<()>;
    async fn running(&self, name: &str) -> bool;
    /// Ask the server to write its world to disk (best effort).
    async fn flush(&self, name: &str, game: Game);
    async fn stop(&self, name: &str) -> Result<()>;
    /// Write a tar of the volume's world to `out`. Works whether or not the
    /// server container still exists.
    async fn tar_world(&self, volume: &str, game: Game, out: &Path) -> Result<()>;
    /// Remove the container (if it is still there) and its volume.
    async fn remove(&self, name: &str, volume: &str);
    /// Containers this runtime labelled as game servers, with their expiry.
    async fn labelled(&self) -> Result<Vec<(String, u64)>>;
    async fn kill(&self, name: &str);
}

async fn docker(args: &[&str], timeout: Duration) -> Result<String> {
    let out = tokio::time::timeout(timeout, crate::container::command().args(args).stdin(Stdio::null()).kill_on_drop(true).output())
        .await
        .with_context(|| format!("docker {} timed out", args.first().unwrap_or(&"")))?
        .context("run docker")?;
    if !out.status.success() {
        bail!("docker {}: {}", args.first().unwrap_or(&""), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn docker_sync_ok(args: &[&str]) -> bool {
    crate::container::command_sync().args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

/// `docker run` argv that tars a world volume to stdout with the game's own
/// (already pulled) image: no network, the volume read-only.
pub fn tar_argv(volume: &str, game: Game) -> Vec<String> {
    let dir = game.data_dir();
    vec![
        "run".into(),
        "--rm".into(),
        "--network".into(),
        "none".into(),
        "--entrypoint".into(),
        "tar".into(),
        "-v".into(),
        format!("{volume}:{dir}:ro"),
        game.image().into(),
        "-C".into(),
        "/".into(),
        "-cf".into(),
        "-".into(),
        dir.trim_start_matches('/').into(),
    ]
}

/// The real container runtime: the `docker` CLI (or `$C0MPUTE_DOCKER`).
pub struct Docker;

#[async_trait::async_trait]
impl Runtime for Docker {
    async fn create(&self, plan: &ContainerPlan) -> Result<()> {
        let argv = plan.create_argv();
        let args: Vec<&str> = argv.iter().map(String::as_str).collect();
        docker(&args, Duration::from_secs(1800)).await.map(drop)
    }

    async fn restore(&self, plan: &ContainerPlan, tar: &Path) -> Result<()> {
        // `docker cp - <c>:/` extracts the tar (entries `data/...`) into the volume.
        let f = std::fs::File::open(tar).with_context(|| format!("open {}", tar.display()))?;
        let out = crate::container::command()
            .args(["cp", "-", &format!("{}:/", plan.name)])
            .stdin(Stdio::from(f))
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output()
            .await
            .context("run docker cp")?;
        if !out.status.success() {
            bail!("docker cp into {}: {}", plan.name, String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    }

    async fn start(&self, name: &str) -> Result<()> {
        docker(&["start", name], Duration::from_secs(120)).await.map(drop)
    }

    async fn running(&self, name: &str) -> bool {
        docker(&["inspect", "-f", "{{.State.Running}}", name], Duration::from_secs(30)).await.is_ok_and(|s| s == "true")
    }

    async fn flush(&self, name: &str, game: Game) {
        // itzg's image ships rcon-cli with RCON on; the others autosave.
        if game == Game::Minecraft {
            let _ = docker(&["exec", name, "rcon-cli", "save-all", "flush"], Duration::from_secs(60)).await;
        }
    }

    async fn stop(&self, name: &str) -> Result<()> {
        docker(&["stop", "-t", "60", name], Duration::from_secs(120)).await.map(drop)
    }

    async fn tar_world(&self, volume: &str, game: Game, out: &Path) -> Result<()> {
        let f = std::fs::File::create(out).with_context(|| format!("create {}", out.display()))?;
        let argv = tar_argv(volume, game);
        let res = tokio::time::timeout(
            Duration::from_secs(1800),
            crate::container::command().args(&argv).stdin(Stdio::null()).stdout(Stdio::from(f)).stderr(Stdio::piped()).kill_on_drop(true).output(),
        )
        .await
        .context("tar of the world timed out")?
        .context("run docker")?;
        if !res.status.success() {
            bail!("tar of volume {volume}: {}", String::from_utf8_lossy(&res.stderr).trim());
        }
        Ok(())
    }

    async fn remove(&self, name: &str, volume: &str) {
        let _ = docker(&["rm", "-f", name], Duration::from_secs(60)).await;
        let _ = docker(&["volume", "rm", volume], Duration::from_secs(60)).await;
    }

    async fn labelled(&self) -> Result<Vec<(String, u64)>> {
        let out = docker(&["ps", "-a", "--filter", "label=c0mpute.gameserver=1", "--format", "{{.Names}}\t{{.Label \"c0mpute.gameserver.expires\"}}"], Duration::from_secs(30)).await?;
        Ok(parse_labelled(&out))
    }

    async fn kill(&self, name: &str) {
        let _ = docker(&["kill", name], Duration::from_secs(30)).await;
        let _ = docker(&["rm", "-f", name], Duration::from_secs(30)).await;
    }
}

/// `docker ps --format '{{.Names}}\t{{.Label ...expires}}'` → (name, expiry).
/// A container without a readable expiry counts as already expired.
pub fn parse_labelled(out: &str) -> Vec<(String, u64)> {
    out.lines()
        .filter_map(|l| {
            let mut it = l.split('\t');
            let name = it.next()?.trim();
            if name.is_empty() {
                return None;
            }
            Some((name.to_string(), it.next().and_then(|e| e.trim().parse().ok()).unwrap_or(0)))
        })
        .collect()
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
    /// Blake3 hex of the (encrypted) world manifest.
    pub manifest: String,
    /// Plaintext tar size.
    pub bytes: u64,
    /// sha256 of the plaintext tar.
    pub sha256: String,
    /// When it was taken (unix seconds).
    #[serde(default)]
    pub at: u64,
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
    /// Every backup is sealed to this key.
    #[serde(default)]
    pub backup_pubkey: String,
    /// Seconds between live backups (0 = none).
    #[serde(default)]
    pub backup_every_secs: u64,
    #[serde(default)]
    pub stopped_at: Option<u64>,
    /// The latest backup (live, or the final one at stop/expiry).
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

/// Running servers whose next live backup is due.
pub fn due_for_backup(servers: &HashMap<String, ServerState>, now: u64) -> Vec<String> {
    let mut ids: Vec<String> = servers
        .values()
        .filter(|s| {
            let last = s.backup.as_ref().map(|b| b.at).unwrap_or(s.started_at).max(s.started_at);
            s.running() && s.backup_every_secs > 0 && now < s.expires_at && now >= last + s.backup_every_secs
        })
        .map(|s| s.id.clone())
        .collect();
    ids.sort();
    ids
}

/// Labelled containers whose reservation has passed: killed on sight, at
/// boot and on every sweep, whatever the state file says.
pub fn expired_containers(labelled: &[(String, u64)], now: u64) -> Vec<String> {
    labelled.iter().filter(|(_, exp)| now >= *exp).map(|(n, _)| n.clone()).collect()
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

// ── world encryption ────────────────────────────────────────────────────

/// How a world's chunks are encrypted.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WorldEnc {
    pub scheme: String,
    /// Present on backups: the worker's one-time X25519 key; the buyer's
    /// backup key and this give the chunk key. Absent on a world the buyer
    /// publishes for a restore: its key travels in the sealed secrets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epk: Option<String>,
}

impl WorldEnc {
    pub fn handed_off() -> Self {
        Self { scheme: ENC_SCHEME.into(), epk: None }
    }
}

/// A fresh random world key.
pub fn random_key() -> [u8; 32] {
    rand::random()
}

/// The chunk key of a backup sealed to `recipient_pub`: derived from the
/// worker's one-time secret and the recipient's key (worker side), or the
/// recipient's secret and the one-time `epk` (buyer side).
pub fn backup_key(secret: &StaticSecret, peer_pub_hex: &str, epk_hex: &str, recipient_hex: &str) -> Result<[u8; 32]> {
    crypto::session_key(secret, peer_pub_hex, &format!("gameserver-backup:{epk_hex}:{recipient_hex}"))
}

fn chunk_nonce(i: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&i.to_be_bytes());
    n
}

/// Chunk `i` under `key`. The key is single-use (one per world), so the
/// index is a safe nonce, and it pins every chunk to its position.
pub fn seal_chunk(key: &[u8; 32], i: u64, plain: &[u8]) -> Result<Vec<u8>> {
    let c = Aes256Gcm::new_from_slice(key).map_err(|_| anyhow!("key"))?;
    c.encrypt(Nonce::from_slice(&chunk_nonce(i)), plain).map_err(|_| anyhow!("encrypt chunk"))
}

pub fn open_chunk(key: &[u8; 32], i: u64, sealed: &[u8]) -> Result<Vec<u8>> {
    let c = Aes256Gcm::new_from_slice(key).map_err(|_| anyhow!("key"))?;
    c.decrypt(Nonce::from_slice(&chunk_nonce(i)), sealed).map_err(|_| anyhow!("world chunk {i} does not decrypt (wrong key or tampered)"))
}

// ── world backups ───────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WorldManifest {
    pub v: u32,
    pub kind: String,
    pub game: Game,
    /// A tar of this directory (entries start with its name, e.g. `data/`).
    pub data_dir: String,
    /// Plaintext tar size.
    pub bytes: u64,
    /// sha256 of the plaintext tar, checked after decryption.
    pub sha256: String,
    pub enc: WorldEnc,
    /// Blake3 hex of each encrypted chunk, in order.
    pub chunks: Vec<String>,
}

impl WorldManifest {
    pub fn new(game: Game, enc: WorldEnc) -> Self {
        Self { v: MANIFEST_VERSION, kind: MANIFEST_KIND.into(), game, data_dir: game.data_dir().into(), bytes: 0, sha256: String::new(), enc, chunks: Vec::new() }
    }
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }
    pub fn from_bytes(b: &[u8]) -> Result<Self> {
        let m: Self = serde_json::from_slice(b).context("world manifest is not a v2 JSON manifest")?;
        m.check()?;
        Ok(m)
    }
    pub fn check(&self) -> Result<()> {
        if self.v != MANIFEST_VERSION || self.kind != MANIFEST_KIND {
            bail!("not a c0mpute game world manifest");
        }
        if self.enc.scheme != ENC_SCHEME || self.enc.epk.as_deref().is_some_and(|k| !is_hex(k, 64)) {
            bail!("unknown world encryption");
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

/// Read `r` to the end, encrypt it under `key` in 1 MiB chunks, and put the
/// chunks in the store; returns the manifest (not yet stored). Chunks are
/// announced so any peer can fetch them; only the key holder can read them.
pub async fn chunk_into_store<R: tokio::io::AsyncRead + Unpin>(store: &ChunkStore, net: Option<&Libp2pNetwork>, game: Game, mut r: R, key: &[u8; 32], enc: WorldEnc) -> Result<WorldManifest> {
    let mut m = WorldManifest::new(game, enc);
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
        let sealed = seal_chunk(key, m.chunks.len() as u64, &buf[..filled])?;
        let h = store.put(&sealed).await?;
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
    let b = crate::gateway_util::fetch_verified(net, hash, provider, max, Duration::from_secs(60)).await.map_err(|e| anyhow!("fetch {hash} from {provider}: {e:?}"))?;
    // Keep a copy: a buyer mirroring backups must be able to restore after
    // the worker that took them is gone.
    let _ = store.put(&b).await;
    Ok(b)
}

/// Fetch an encrypted world (manifest, then each chunk) from `provider` (or
/// a local copy), verify every chunk's hash, decrypt it with the key
/// `key_of` gives for the manifest, check the plaintext sha256, and write the
/// tar to `out` as it arrives.
pub async fn fetch_world<W: tokio::io::AsyncWrite + Unpin>(
    net: &Libp2pNetwork,
    store: &ChunkStore,
    manifest_hash: &str,
    provider: &str,
    key_of: impl FnOnce(&WorldManifest) -> Result<[u8; 32]>,
    mut out: W,
) -> Result<WorldManifest> {
    let mb = fetch_bytes(net, store, manifest_hash, provider, MAX_MANIFEST_BYTES).await.context("world manifest")?;
    let m = WorldManifest::from_bytes(&mb)?;
    let key = key_of(&m)?;
    let mut sha = Sha256::new();
    for (i, c) in m.chunks.iter().enumerate() {
        let sealed = fetch_bytes(net, store, c, provider, CHUNK_BYTES + TAG_BYTES).await?;
        let b = open_chunk(&key, i as u64, &sealed)?;
        sha.update(&b);
        out.write_all(&b).await?;
    }
    out.shutdown().await?;
    if format!("sha256:{}", hex::encode(sha.finalize())) != m.sha256 {
        bail!("world tar does not match the manifest's sha256");
    }
    Ok(m)
}

// ── status records ──────────────────────────────────────────────────────

/// What a worker gossips about each server it holds, every sweep. The
/// gossipsub message is signed by the worker's peer key (strict validation),
/// so a watcher trusts a record only when its source is the rental's worker.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StatusRecord {
    pub t: String,
    pub v: u32,
    pub id: String,
    pub worker: String,
    pub game: Game,
    pub address: String,
    /// The container is up, per the worker's runtime.
    pub running: bool,
    pub expires_at: u64,
    #[serde(default)]
    pub stopped_at: Option<u64>,
    /// Latest backup: sealed to the buyer, so the hash is safe to gossip.
    #[serde(default)]
    pub backup: Option<Backup>,
    pub ts: u64,
}

pub const STATUS_KIND: &str = "gs_status";

// ── worker ──────────────────────────────────────────────────────────────

pub struct Worker {
    net: Arc<Libp2pNetwork>,
    store: ChunkStore,
    cfg: crate::config::GameserverConfig,
    host: String,
    state_dir: PathBuf,
    state_path: PathBuf,
    rt: Arc<dyn Runtime>,
    servers: Mutex<HashMap<String, ServerState>>,
    /// Servers a start, stop, backup or reap is working on right now.
    busy: Mutex<HashSet<String>>,
}

impl Worker {
    pub fn new(net: Arc<Libp2pNetwork>, store: ChunkStore, cfg: crate::config::GameserverConfig, host: String, state_dir: &Path) -> Arc<Self> {
        Self::with_runtime(net, store, cfg, host, state_dir, Arc::new(Docker))
    }

    pub fn with_runtime(net: Arc<Libp2pNetwork>, store: ChunkStore, cfg: crate::config::GameserverConfig, host: String, state_dir: &Path, rt: Arc<dyn Runtime>) -> Arc<Self> {
        let state_path = state_dir.join("servers.json");
        let servers: HashMap<String, ServerState> = std::fs::read(&state_path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Arc::new(Self { net, store, cfg, host, state_dir: state_dir.to_path_buf(), state_path, rt, servers: Mutex::new(servers), busy: Mutex::new(HashSet::new()) })
    }

    pub fn net(&self) -> &Libp2pNetwork {
        &self.net
    }

    pub fn snapshot(&self) -> HashMap<String, ServerState> {
        self.servers.lock().map(|g| g.clone()).unwrap_or_default()
    }

    fn save(&self) {
        let snap = self.snapshot();
        let _ = std::fs::create_dir_all(&self.state_dir);
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

    fn scratch(&self, what: &str, id: &str) -> PathBuf {
        self.state_dir.join(format!("{what}-{id}.tar"))
    }

    pub fn accepts_start(&self, offer: &JobOffer) -> bool {
        // An e2e offer keeps its keys, stop hash and world sealed: stand-ins
        // let the size and price checks run; the opened spec is checked again.
        let spec = match crate::job_e2e::bid_spec(offer) {
            Some(crate::job_e2e::BidSpec::Public(v)) => Some(crate::job_e2e::with_placeholders(
                v,
                &[
                    ("stop_hash", serde_json::json!(format!("sha256:{}", "0".repeat(64)))),
                    ("client_pubkey", serde_json::json!("1".repeat(64))),
                    ("backup_pubkey", serde_json::json!("1".repeat(64))),
                ],
            )),
            Some(crate::job_e2e::BidSpec::Full(v)) => Some(v.clone()),
            None => None,
        };
        let Some(spec) = spec.and_then(|v| serde_json::from_value::<StartSpec>(v).ok()) else {
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
        // An e2e offer shows the token's hash, never the token.
        if let Some(crate::job_e2e::BidSpec::Public(v)) = crate::job_e2e::bid_spec(offer) {
            let (Some(id), Some(h)) = (v["id"].as_str(), v["token_hash"].as_str()) else {
                return false;
            };
            return self.snapshot().get(id).is_some_and(|s| s.stop_hash == h);
        }
        let Some(spec) = offer.spec_inline.as_ref().and_then(|v| serde_json::from_value::<StopSpec>(v.clone()).ok()) else {
            return false;
        };
        spec.validate().is_ok() && self.snapshot().get(&spec.id).is_some_and(|s| s.stop_hash == sha256_ref(spec.token.as_bytes()))
    }

    async fn start(&self, job: &AcceptedJob) -> Result<Value> {
        let spec: StartSpec = serde_json::from_value(job.accept.spec_inline.clone().context("accept lacks spec_inline")?)?;
        spec.validate().map_err(|e| anyhow!(e))?;
        // The password and the restore key come sealed, after the accept.
        let raw = crate::handoff::receive(&self.net, WORKLOAD_START, &job.accept, &spec.client_pubkey, HANDOFF_WAIT).await.context("receive the sealed secrets")?;
        let secrets: StartSecrets = serde_json::from_slice(&raw).context("sealed secrets are not JSON")?;
        drop(raw);
        secrets.validate(&spec).map_err(|e| anyhow!(e))?;
        let id = server_id(&job.accept.job_id);
        // Reserve RAM and ports under the lock, so two starts cannot both take the last slot.
        let (state, plan) = {
            let mut servers = self.servers.lock().map_err(|_| anyhow!("state lock poisoned"))?;
            if servers.contains_key(&id) {
                bail!("server {id} already exists");
            }
            has_capacity(&self.cfg, crate::text::local_ram_mb(), &servers, &spec).map_err(|e| anyhow!(e))?;
            let used: HashSet<u16> = servers.values().filter(|s| s.running()).flat_map(|s| s.ports()).collect();
            let port = allocate_ports(self.cfg.port_min, self.cfg.port_max, spec.game.ports().len(), &used, port_bindable).context("no free port in the range")?;
            let started = now_secs();
            let expires = expires_at(started, spec.hours);
            let plan = ContainerPlan::new(&id, &spec, &secrets, port, started, expires);
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
                backup_pubkey: spec.backup_pubkey.clone(),
                backup_every_secs: u64::from(spec.backup_mins) * 60,
                stopped_at: None,
                backup: None,
                backup_error: None,
            };
            servers.insert(id.clone(), state.clone());
            (state, plan)
        };
        self.claim(&id);
        let res = self.create_and_start(&id, &plan, &spec, &secrets, &job.accept.buyer_peer_id).await;
        self.release(&id);
        if let Err(e) = res {
            self.rt.remove(&plan.name, &plan.volume).await;
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
            "backup_mins": spec.backup_mins,
        }))
    }

    async fn create_and_start(&self, id: &str, plan: &ContainerPlan, spec: &StartSpec, secrets: &StartSecrets, buyer: &str) -> Result<()> {
        self.rt.create(plan).await?;
        if let Some(world) = &spec.world {
            let key = secrets.world_key()?.context("world key missing")?;
            let tmp = self.scratch("restore", id);
            let _ = tokio::fs::create_dir_all(&self.state_dir).await;
            let res = async {
                let f = tokio::fs::File::create(&tmp).await?;
                let m = fetch_world(&self.net, &self.store, world, buyer, |m| {
                    if m.enc.epk.is_some() {
                        bail!("a restore world must be encrypted under a handed-off key");
                    }
                    Ok(key)
                }, f)
                .await
                .context("restore world")?;
                if m.game != spec.game {
                    bail!("world is a {} world, not {}", m.game.name(), spec.game.name());
                }
                self.rt.restore(plan, &tmp).await
            }
            .await;
            let _ = tokio::fs::remove_file(&tmp).await;
            res?;
        }
        self.rt.start(&plan.name).await
    }

    /// Back the world up now (live if the server runs), seal it to the
    /// buyer, and record it as the latest backup.
    pub async fn backup_now(&self, id: &str) -> Result<Backup> {
        let s = self.snapshot().get(id).cloned().with_context(|| format!("no server {id}"))?;
        if !self.claim(id) {
            bail!("server {id} is busy");
        }
        let res = self.backup(&s).await;
        self.release(id);
        if let Ok(mut servers) = self.servers.lock()
            && let Some(e) = servers.get_mut(id)
        {
            match &res {
                Ok(b) => {
                    e.backup = Some(b.clone());
                    e.backup_error = None;
                }
                Err(err) => e.backup_error = Some(format!("{err:#}")),
            }
        }
        self.save();
        res
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
            // Already gone when it stopped itself at expiry (`--rm`).
            let _ = self.rt.stop(&s.container).await;
            let backup = self.backup(&s).await;
            if backup.is_ok() {
                self.rt.remove(&s.container, &s.volume).await;
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
        if !is_hex(&s.backup_pubkey, 64) {
            bail!("server {} has no backup key", s.id);
        }
        if self.rt.running(&s.container).await {
            self.rt.flush(&s.container, s.game).await;
        }
        let _ = tokio::fs::create_dir_all(&self.state_dir).await;
        let tmp = self.scratch("backup", &s.id);
        let res = async {
            self.rt.tar_world(&s.volume, s.game, &tmp).await?;
            let e = crypto::keypair();
            let key = backup_key(&e.secret, &s.backup_pubkey, &e.public_hex, &s.backup_pubkey)?;
            let f = tokio::fs::File::open(&tmp).await?;
            let m = chunk_into_store(&self.store, Some(&self.net), s.game, f, &key, WorldEnc { scheme: ENC_SCHEME.into(), epk: Some(e.public_hex.clone()) }).await?;
            let manifest = put_manifest(&self.store, &self.net, &m).await?;
            Ok::<_, anyhow::Error>(Backup { manifest, bytes: m.bytes, sha256: m.sha256, at: now_secs() })
        }
        .await;
        let _ = tokio::fs::remove_file(&tmp).await;
        let b = res?;
        info!(id = %s.id, manifest = %b.manifest, bytes = b.bytes, "gameserver: world backed up (sealed to the buyer)");
        Ok(b)
    }

    async fn stop(&self, job: &AcceptedJob) -> Result<Value> {
        let spec: StopSpec = serde_json::from_value(job.accept.spec_inline.clone().context("accept lacks spec_inline")?)?;
        spec.validate().map_err(|e| anyhow!(e))?;
        let s = self.snapshot().get(&spec.id).cloned().context("unknown server")?;
        if s.stop_hash != sha256_ref(spec.token.as_bytes()) {
            bail!("stop token does not match");
        }
        let backup = self.stop_and_backup(&spec.id).await?;
        Ok(json!({"v": 1, "ok": true, "id": spec.id, "game": s.game.name(), "backup": backup}))
    }

    /// Kill labelled containers whose reservation passed. Runs at boot (the
    /// worker may have been offline at expiry) and on every sweep.
    pub async fn sweep_expired(&self) -> Vec<String> {
        let Ok(labelled) = self.rt.labelled().await else { return Vec::new() };
        let dead = expired_containers(&labelled, now_secs());
        for name in &dead {
            warn!(container = %name, "gameserver: reservation passed; killing");
            self.rt.kill(name).await;
        }
        dead
    }

    /// Kill expired containers, stop + back up servers whose reservation
    /// ended, take due live backups, forget old stopped ones.
    pub async fn reap_once(&self) {
        let now = now_secs();
        self.sweep_expired().await;
        for id in due_for_stop(&self.snapshot(), now) {
            info!(%id, "gameserver: reservation ended, stopping");
            if let Err(e) = self.stop_and_backup(&id).await {
                warn!(%id, err = %format!("{e:#}"), "gameserver: stop at expiry failed");
            }
        }
        for id in due_for_backup(&self.snapshot(), now) {
            if let Err(e) = self.backup_now(&id).await {
                warn!(%id, err = %format!("{e:#}"), "gameserver: live backup failed");
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

    /// The status record for one server.
    pub async fn status(&self, s: &ServerState) -> StatusRecord {
        StatusRecord {
            t: STATUS_KIND.into(),
            v: 1,
            id: s.id.clone(),
            worker: self.net.peer_id().to_base58(),
            game: s.game,
            address: s.address.clone(),
            running: s.running() && self.rt.running(&s.container).await,
            expires_at: s.expires_at,
            stopped_at: s.stopped_at,
            backup: s.backup.clone(),
            ts: now_secs(),
        }
    }

    /// Gossip a status record for every server held (stopped ones for a day,
    /// so a watcher learns the final backup).
    pub async fn publish_status(&self) {
        let now = now_secs();
        for s in self.snapshot().values() {
            if s.stopped_at.is_some_and(|t| now.saturating_sub(t) > 86_400) {
                continue;
            }
            let r = self.status(s).await;
            if let Ok(b) = serde_json::to_vec(&r) {
                let _ = self.net.publish(STATUS_TOPIC, b).await;
            }
        }
    }

    /// Store the job's result, sealed to the buyer for e2e jobs (DIP-0032).
    async fn publish_result(&self, accept: &c0mpute_net::topics::JobAccept, v: &Value) -> Result<String> {
        crate::pieces::publish_result_bytes(&self.net, &self.store, accept, &serde_json::to_vec(v)?).await
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
                    Ok(v) => w.publish_result(&job.accept, &v).await,
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

/// Run the sweep (expiry, live backups) and gossip status every `every`.
pub fn spawn_reaper(worker: Arc<Worker>, every: Duration) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let _ = worker.net.subscribe(STATUS_TOPIC).await;
        loop {
            worker.reap_once().await;
            worker.publish_status().await;
            tokio::time::sleep(every).await;
        }
    })
}

/// Subscribe a worker to start and stop offers.
pub fn subscribe(worker: Arc<Worker>, tags: Vec<String>, pricing: Option<crate::pricing::PricingConfig>) {
    let slots = Arc::new(Semaphore::new(JOB_SLOTS));
    let load = WorkerLoad::new(slots.clone(), JOB_SLOTS);
    let w = worker.clone();
    let start_filter: crate::dispatch::OfferFilter = Arc::new(move |o| w.accepts_start(o));
    let w = worker.clone();
    let stop_filter: crate::dispatch::OfferFilter = Arc::new(move |o| w.accepts_stop(o));
    crate::dispatch::run_worker_subscriber_with_load(
        worker.net.clone(),
        WORKLOAD_START.into(),
        tags.clone(),
        Some(spawn_runner(worker.clone(), slots.clone(), false)),
        Some(start_filter),
        pricing,
        Some(load.clone()),
    );
    // Stopping is part of the rent: no price floor.
    crate::dispatch::run_worker_subscriber_with_load(worker.net.clone(), WORKLOAD_STOP.into(), tags, Some(spawn_runner(worker.clone(), slots, true)), Some(stop_filter), None, Some(load));
}

/// Supervisor entry: kill expired leftovers, pull images, then subscribe to
/// start and stop offers and run the sweep. Returns without subscribing when
/// the node cannot host.
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
    // Boot watchdog: anything whose reservation passed while we were down dies now.
    worker.sweep_expired().await;
    // Enforce expiries even before the images finish pulling.
    spawn_reaper(worker.clone(), REAP_EVERY);
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
    info!(games = ?ready.games, host = ?ready.host, "gameserver: serving");
    subscribe(worker, tags, Some(config.pricing.clone()));
}

// ── health probes ───────────────────────────────────────────────────────

/// One probe of a server's game port.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ProbeResult {
    pub ts: u64,
    pub ok: bool,
    /// `minecraft-status`, `a2s-info` or `udp-reachable`.
    pub method: String,
    pub rtt_ms: u64,
    pub detail: String,
}

fn put_varint(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let mut b = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        out.push(b);
        if v == 0 {
            break;
        }
    }
}

async fn read_varint<R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> Result<u32> {
    let mut v = 0u32;
    for i in 0..5 {
        let b = r.read_u8().await?;
        v |= u32::from(b & 0x7f) << (7 * i);
        if b & 0x80 == 0 {
            return Ok(v);
        }
    }
    bail!("varint too long")
}

fn read_varint_slice(b: &[u8], at: &mut usize) -> Result<u32> {
    let mut v = 0u32;
    for i in 0..5 {
        let x = *b.get(*at).context("truncated varint")?;
        *at += 1;
        v |= u32::from(x & 0x7f) << (7 * i);
        if x & 0x80 == 0 {
            return Ok(v);
        }
    }
    bail!("varint too long")
}

/// Minecraft Java "server list ping": handshake (next state 1) + status request.
pub fn minecraft_status_request(host: &str, port: u16) -> Vec<u8> {
    let mut body = Vec::new();
    put_varint(&mut body, 0x00);
    put_varint(&mut body, 767);
    put_varint(&mut body, host.len() as u32);
    body.extend_from_slice(host.as_bytes());
    body.extend_from_slice(&port.to_be_bytes());
    put_varint(&mut body, 1);
    let mut pkt = Vec::new();
    put_varint(&mut pkt, body.len() as u32);
    pkt.extend(body);
    pkt.extend([0x01, 0x00]);
    pkt
}

/// A status response packet carrying `json` (what a server answers).
pub fn minecraft_status_response(json: &str) -> Vec<u8> {
    let mut body = Vec::new();
    put_varint(&mut body, 0x00);
    put_varint(&mut body, json.len() as u32);
    body.extend_from_slice(json.as_bytes());
    let mut pkt = Vec::new();
    put_varint(&mut pkt, body.len() as u32);
    pkt.extend(body);
    pkt
}

/// Parse a status response body (after the length prefix) into its JSON.
pub fn parse_minecraft_status(body: &[u8]) -> Result<Value> {
    let mut at = 0;
    if read_varint_slice(body, &mut at)? != 0 {
        bail!("not a status response");
    }
    let n = read_varint_slice(body, &mut at)? as usize;
    let s = body.get(at..at + n).context("truncated status")?;
    let v: Value = serde_json::from_slice(s).context("status is not JSON")?;
    if !v.is_object() {
        bail!("status is not an object");
    }
    Ok(v)
}

async fn minecraft_status(addr: SocketAddr, host: &str, timeout: Duration) -> Result<String> {
    tokio::time::timeout(timeout, async {
        let mut s = tokio::net::TcpStream::connect(addr).await?;
        s.write_all(&minecraft_status_request(host, addr.port())).await?;
        let len = read_varint(&mut s).await? as usize;
        if len == 0 || len > 256 * 1024 {
            bail!("status packet length {len}");
        }
        let mut body = vec![0u8; len];
        s.read_exact(&mut body).await?;
        let v = parse_minecraft_status(&body)?;
        let online = v["players"]["online"].as_u64().unwrap_or(0);
        let version = v["version"]["name"].as_str().unwrap_or("?").to_string();
        Ok(format!("{version}, {online} online"))
    })
    .await
    .context("timed out")?
}

/// Steam A2S_INFO request (Valheim answers on its query port, game port + 1).
pub const A2S_INFO: &[u8] = b"\xFF\xFF\xFF\xFFTSource Engine Query\0";

async fn a2s_info(addr: SocketAddr, timeout: Duration) -> Result<String> {
    tokio::time::timeout(timeout, async {
        let bind: SocketAddr = if addr.is_ipv4() { "0.0.0.0:0".parse()? } else { "[::]:0".parse()? };
        let sock = tokio::net::UdpSocket::bind(bind).await?;
        sock.connect(addr).await?;
        let mut q = A2S_INFO.to_vec();
        let mut buf = vec![0u8; 1400];
        for _ in 0..2 {
            sock.send(&q).await?;
            let n = sock.recv(&mut buf).await?;
            let r = &buf[..n];
            if n < 5 || r[..4] != [0xFF; 4] {
                bail!("not an A2S answer");
            }
            match r[4] {
                // Challenge: repeat the query with it appended.
                0x41 if n >= 9 => {
                    q = A2S_INFO.to_vec();
                    q.extend_from_slice(&r[5..9]);
                }
                0x49 => return Ok(format!("a2s info ({n} bytes)")),
                other => bail!("unexpected A2S answer 0x{other:02x}"),
            }
        }
        bail!("no A2S info after the challenge")
    })
    .await
    .context("timed out")?
}

/// Factorio has no cheap unauthenticated query. A connected UDP socket learns
/// of a closed port from the ICMP "port unreachable" (ECONNREFUSED on recv);
/// silence means "not refused". Weak on its own, so a watcher also requires
/// a fresh worker status record saying the container runs.
async fn udp_reachable(addr: SocketAddr, timeout: Duration) -> Result<String> {
    let bind: SocketAddr = if addr.is_ipv4() { "0.0.0.0:0".parse()? } else { "[::]:0".parse()? };
    let sock = tokio::net::UdpSocket::bind(bind).await?;
    sock.connect(addr).await?;
    sock.send(&[0u8]).await?;
    let mut buf = [0u8; 64];
    match tokio::time::timeout(timeout, sock.recv(&mut buf)).await {
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => bail!("port unreachable"),
        Ok(Err(e)) => bail!("{e}"),
        Ok(Ok(_)) => Ok("answered".into()),
        Err(_) => Ok("not refused".into()),
    }
}

fn split_address(address: &str) -> Option<(String, u16)> {
    let (h, p) = address.rsplit_once(':')?;
    Some((h.trim_start_matches('[').trim_end_matches(']').to_string(), p.parse().ok()?))
}

/// Probe a server's game port the way its players' clients would.
pub async fn probe_server(game: Game, address: &str, timeout: Duration) -> ProbeResult {
    let started = Instant::now();
    let method = match game {
        Game::Minecraft => "minecraft-status",
        Game::Valheim => "a2s-info",
        Game::Factorio => "udp-reachable",
    };
    let res: Result<String> = async {
        let (host, port) = split_address(address).context("address is not host:port")?;
        let mut addrs = tokio::net::lookup_host((host.as_str(), port)).await?;
        let addr = addrs.next().context("address does not resolve")?;
        match game {
            Game::Minecraft => minecraft_status(addr, &host, timeout).await,
            Game::Valheim => a2s_info(SocketAddr::new(addr.ip(), port.checked_add(1).context("port")?), timeout).await,
            Game::Factorio => udp_reachable(addr, timeout).await,
        }
    }
    .await;
    let (ok, detail) = match res {
        Ok(d) => (true, d),
        Err(e) => (false, format!("{e:#}")),
    };
    ProbeResult { ts: now_secs(), ok, method: method.into(), rtt_ms: started.elapsed().as_millis() as u64, detail }
}

/// Decides when a server has been down long enough to fail over.
#[derive(Clone, Debug)]
pub struct HealthTracker {
    fail_after_secs: u64,
    first_fail: Option<u64>,
    pub last_ok: Option<u64>,
}

impl HealthTracker {
    pub fn new(fail_after: Duration) -> Self {
        Self { fail_after_secs: fail_after.as_secs().max(1), first_fail: None, last_ok: None }
    }
    /// Record one check; `true` once checks have failed without a pass for
    /// `fail_after`.
    pub fn observe(&mut self, now: u64, ok: bool) -> bool {
        if ok {
            self.first_fail = None;
            self.last_ok = Some(now);
            return false;
        }
        let since = *self.first_fail.get_or_insert(now);
        now.saturating_sub(since) >= self.fail_after_secs
    }
    pub fn reset(&mut self) {
        self.first_fail = None;
    }
}

/// Is the server healthy, given a probe and the worker's latest status
/// record (and when it arrived)? A record that says the container is down
/// always fails it; for Factorio, whose probe is weak, a missing or stale
/// record fails it too.
pub fn judge(game: Game, probe: &ProbeResult, status: Option<(&StatusRecord, u64)>, now: u64) -> bool {
    if let Some((s, _)) = status
        && (!s.running || s.stopped_at.is_some())
    {
        return false;
    }
    if game == Game::Factorio {
        let fresh = status.is_some_and(|(s, seen)| s.running && now.saturating_sub(seen) <= 180);
        return probe.ok && fresh;
    }
    probe.ok
}

// ── buyer ───────────────────────────────────────────────────────────────

/// What the buyer keeps about a server it rented (`gameserver ls`). The file
/// is mode 0600 and never leaves this machine.
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
    // What a failover needs to start the same server elsewhere.
    #[serde(default)]
    pub ram_gb: u32,
    #[serde(default)]
    pub cpus: f64,
    #[serde(default)]
    pub accept_eula: bool,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub server_name: Option<String>,
    #[serde(default)]
    pub backup_mins: u32,
    /// The newest backup mirrored to `worlds/<hash>.tar` on this machine.
    #[serde(default)]
    pub latest_backup: Option<String>,
    /// The rental this one replaced after a failover.
    #[serde(default)]
    pub replaces: Option<String>,
    /// The rental that replaced this one after a failover.
    #[serde(default)]
    pub superseded_by: Option<String>,
}

pub fn buyer_dir() -> PathBuf {
    crate::config::data_dir().unwrap_or_else(|| PathBuf::from("./c0mpute-data")).join("gameserver")
}

pub fn load_rentals(dir: &Path) -> Vec<Rental> {
    std::fs::read(dir.join("rentals.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn write_private(p: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = p.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, p)?;
    Ok(())
}

pub fn save_rentals(dir: &Path, rentals: &[Rental]) -> Result<()> {
    write_private(&dir.join("rentals.json"), &serde_json::to_vec_pretty(rentals)?)
}

fn upsert_rental(dir: &Path, r: &Rental) -> Result<()> {
    let mut all = load_rentals(dir);
    match all.iter_mut().find(|x| x.id == r.id) {
        Some(x) => *x = r.clone(),
        None => all.push(r.clone()),
    }
    save_rentals(dir, &all)
}

/// The buyer's long-term backup key (`<dir>/backup.key`, 0600), created on
/// first use. Every worker seals its backups to the public half.
pub fn backup_keypair(dir: &Path) -> Result<crypto::KeyPair> {
    let p = dir.join("backup.key");
    if let Ok(h) = std::fs::read_to_string(&p) {
        let secret = crypto::secret_from_hex(h.trim()).context("backup.key is not a 32-byte hex key")?;
        let public_hex = hex::encode(x25519_dalek::PublicKey::from(&secret).as_bytes());
        return Ok(crypto::KeyPair { secret, public_hex });
    }
    let kp = crypto::keypair();
    write_private(&p, hex::encode(kp.secret.to_bytes()).as_bytes())?;
    Ok(kp)
}

pub struct StartRequest {
    pub game: Game,
    pub hours: u32,
    pub ram_gb: u32,
    pub cpus: f64,
    pub accept_eula: bool,
    /// A manifest hash from an earlier backup, or a `.tar` laid out like a backup.
    pub world: Option<String>,
    pub password: Option<String>,
    pub server_name: Option<String>,
    /// Minutes between live backups (0 = only at stop/expiry).
    pub backup_mins: u32,
    pub max_price_usd: f64,
    pub workers: Vec<String>,
    /// Never use these workers (a failover excludes the one that failed).
    pub exclude: Vec<String>,
    pub timeout: Duration,
}

async fn auction(
    net: Arc<Libp2pNetwork>,
    workload: &str,
    spec: Value,
    caps: Vec<String>,
    workers: Vec<String>,
    exclude: Vec<String>,
    max_price: f64,
    timeout: Duration,
    notify: Option<mpsc::UnboundedSender<(String, c0mpute_net::topics::JobBid)>>,
) -> Result<(crate::buyer::AuctionOutcome, Value)> {
    let mut a = crate::JobAuction::new(workload, spec)
        .with_required_capabilities(caps)
        .with_allowed_bidders(workers)
        .with_excluded_bidders(exclude)
        .with_max_price_usd(max_price)
        .with_bid_window(Duration::from_secs(4))
        .with_deadline(timeout)
        .with_receipt_timeout(timeout);
    if let Some(tx) = notify {
        a = a.with_accept_notify(tx);
    }
    let o = crate::run_auction(net.clone(), a).await?;
    if o.receipt.status != JobStatus::Completed {
        bail!("worker {} reported {:?}", o.accepted_bid.bidder_peer_id, o.receipt.status);
    }
    o.receipt.output_hash.as_ref().context("worker returned no result hash")?;
    let bytes = crate::gateway_util::fetch_result(&net, &o, 64 * 1024, Duration::from_secs(30)).await.map_err(|e| anyhow::anyhow!("fetch result: {e:?}"))?;
    let v: Value = serde_json::from_slice(&bytes)?;
    if v["ok"] != json!(true) {
        bail!("worker result: {v}");
    }
    Ok((o, v))
}

/// Encrypt a world under a fresh key and put it in the local store for the
/// worker to fetch; returns the manifest hash and the key.
async fn publish_world(store: &ChunkStore, net: &Libp2pNetwork, dir: &Path, game: Game, world: &str) -> Result<(String, [u8; 32])> {
    let (tar, saved_manifest) = if is_hex(world, 64) {
        (dir.join("worlds").join(format!("{world}.tar")), Some(dir.join("worlds").join(format!("{world}.json"))))
    } else {
        (PathBuf::from(world), None)
    };
    let f = tokio::fs::File::open(&tar).await.with_context(|| format!("open world {}", tar.display()))?;
    let key = random_key();
    let m = chunk_into_store(store, Some(net), game, f, &key, WorldEnc::handed_off()).await?;
    if let Some(p) = saved_manifest
        && let Ok(b) = std::fs::read(&p)
    {
        // The manifest saved beside the tar names its game and plaintext sha256.
        let saved: Value = serde_json::from_slice(&b).with_context(|| format!("read {}", p.display()))?;
        if saved["sha256"].as_str() != Some(m.sha256.as_str()) {
            bail!("{} does not match its manifest", tar.display());
        }
        if saved["game"].as_str().is_some_and(|g| g != game.name()) {
            bail!("that world is a {} world", saved["game"].as_str().unwrap_or("?"));
        }
    }
    Ok((put_manifest(store, net, &m).await?, key))
}

/// Rent a server on `net` (a booted buyer node), remembering it under `dir`.
pub async fn start_with(net: Arc<Libp2pNetwork>, store: &ChunkStore, dir: &Path, req: StartRequest) -> Result<Rental> {
    let token = hex::encode(rand::random::<[u8; 32]>());
    let backup = backup_keypair(dir)?;
    let (world, world_key) = match &req.world {
        Some(w) => {
            let (h, k) = publish_world(store, &net, dir, req.game, w).await?;
            (Some(h), Some(hex::encode(k)))
        }
        None => (None, None),
    };
    let kp = crate::handoff::keypair();
    let spec = StartSpec {
        v: SPEC_VERSION,
        game: req.game,
        hours: req.hours,
        ram_gb: req.ram_gb,
        cpus: req.cpus,
        accept_eula: req.accept_eula,
        stop_hash: sha256_ref(token.as_bytes()),
        client_pubkey: kp.public_hex.clone(),
        backup_pubkey: backup.public_hex.clone(),
        backup_mins: req.backup_mins,
        world,
        server_name: req.server_name.clone(),
    };
    spec.validate().map_err(|e| anyhow!(e))?;
    let secrets = StartSecrets { v: 1, password: req.password.clone(), world_key };
    secrets.validate(&spec).map_err(|e| anyhow!(e))?;
    let mut handoff = crate::handoff::BuyerHandoff::spawn(net.clone(), WORKLOAD_START, kp, serde_json::to_vec(&secrets)?)?;
    let res = auction(
        net.clone(),
        WORKLOAD_START,
        serde_json::to_value(&spec)?,
        vec![ROLE_TAG.into(), req.game.tag()],
        req.workers.clone(),
        req.exclude.clone(),
        req.max_price_usd,
        req.timeout,
        Some(handoff.notify()?),
    )
    .await;
    handoff.finish();
    let (o, v) = res?;
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
        ram_gb: req.ram_gb,
        cpus: req.cpus,
        accept_eula: req.accept_eula,
        password: req.password,
        server_name: req.server_name,
        backup_mins: req.backup_mins,
        latest_backup: None,
        replaces: None,
        superseded_by: None,
    };
    upsert_rental(dir, &rental)?;
    Ok(rental)
}

/// Rent a server: boot a buyer node, auction it, wait for the address,
/// remember the rental.
pub async fn start(req: StartRequest) -> Result<Rental> {
    let sup = crate::Supervisor::boot_buyer(crate::Config::default()).await?;
    tokio::time::sleep(Duration::from_secs(5)).await;
    start_with(sup.libp2p.clone(), &sup.store, &buyer_dir(), req).await
}

/// Download, decrypt and keep a backup as `<dir>/worlds/<hash>.tar` (+ its
/// manifest as `.json`).
pub async fn download_backup(net: &Libp2pNetwork, store: &ChunkStore, dir: &Path, manifest: &str, provider: &str) -> Result<WorldManifest> {
    let key = backup_keypair(dir)?;
    let worlds = dir.join("worlds");
    tokio::fs::create_dir_all(&worlds).await?;
    let tmp = worlds.join(format!("{manifest}.partial"));
    let f = tokio::fs::File::create(&tmp).await?;
    let res = fetch_world(net, store, manifest, provider, |m| {
        let epk = m.enc.epk.as_deref().context("backup has no one-time key; it was not sealed to us")?;
        backup_key(&key.secret, epk, epk, &key.public_hex)
    }, f)
    .await;
    let m = match res {
        Ok(m) => m,
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e.context("download world backup"));
        }
    };
    tokio::fs::rename(&tmp, worlds.join(format!("{manifest}.tar"))).await?;
    tokio::fs::write(worlds.join(format!("{manifest}.json")), m.to_bytes()).await?;
    Ok(m)
}

/// Stop a rental early (or collect the backup of one that expired) and
/// download its world to `<dir>/worlds/<hash>.tar`.
pub async fn stop_with(net: Arc<Libp2pNetwork>, store: &ChunkStore, dir: &Path, id: &str, timeout: Duration) -> Result<Rental> {
    let all = load_rentals(dir);
    let r = all.iter().find(|r| r.id == id).cloned().with_context(|| format!("no rental {id} on this machine (see `c0mpute gameserver ls`)"))?;
    let spec = StopSpec { v: 1, id: r.id.clone(), token: r.token.clone() };
    let (_, v) = auction(net.clone(), WORKLOAD_STOP, serde_json::to_value(&spec)?, vec![ROLE_TAG.into()], vec![r.worker.clone()], vec![], 0.0, timeout, None).await?;
    let backup: Option<Backup> = serde_json::from_value(v["backup"].clone()).ok().flatten();
    let mut updated = r.clone();
    updated.stopped_at = Some(now_secs());
    if let Some(b) = &backup {
        download_backup(&net, store, dir, &b.manifest, &r.worker).await?;
        updated.backup = Some(b.manifest.clone());
        updated.latest_backup = Some(b.manifest.clone());
    }
    upsert_rental(dir, &updated)?;
    Ok(updated)
}

pub async fn stop(id: &str, timeout: Duration) -> Result<Rental> {
    let sup = crate::Supervisor::boot_buyer(crate::Config::default()).await?;
    tokio::time::sleep(Duration::from_secs(5)).await;
    stop_with(sup.libp2p.clone(), &sup.store, &buyer_dir(), id, timeout).await
}

/// `gameserver watch` knobs.
#[derive(Clone, Debug)]
pub struct WatchOptions {
    pub probe_every: Duration,
    pub probe_timeout: Duration,
    /// Fail over after the server has failed every check for this long.
    pub fail_after: Duration,
    pub failover: bool,
    pub max_price_usd: f64,
    pub workers: Vec<String>,
    pub start_timeout: Duration,
    /// Return after the first failover instead of watching the replacement.
    pub stop_after_failover: bool,
}

impl Default for WatchOptions {
    fn default() -> Self {
        Self {
            probe_every: Duration::from_secs(30),
            probe_timeout: Duration::from_secs(5),
            fail_after: Duration::from_secs(5 * 60),
            failover: true,
            max_price_usd: 1.0,
            workers: Vec::new(),
            start_timeout: Duration::from_secs(900),
            stop_after_failover: false,
        }
    }
}

/// How a watch ended.
#[derive(Clone, Debug, PartialEq)]
pub enum WatchEnd {
    /// The reservation ended or the server was stopped.
    Ended(Rental),
    /// It failed over and `stop_after_failover` was set.
    FailedOver { from: Rental, to: Rental },
}

fn log_probe(dir: &Path, id: &str, p: &ProbeResult) {
    let d = dir.join("health");
    let _ = std::fs::create_dir_all(&d);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(d.join(format!("{id}.jsonl"))) {
        use std::io::Write;
        let _ = writeln!(f, "{}", serde_json::to_string(p).unwrap_or_default());
    }
}

/// Read a server's probe log.
pub fn health_log(dir: &Path, id: &str) -> Vec<ProbeResult> {
    std::fs::read_to_string(dir.join("health").join(format!("{id}.jsonl"))).unwrap_or_default().lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

/// Watch a rental: probe its port every `probe_every` (logged to
/// `<dir>/health/<id>.jsonl`), mirror each backup its worker announces, and
/// when it has failed every check for `fail_after`, start the same game on
/// another worker from the newest mirrored backup.
pub async fn watch_with(net: Arc<Libp2pNetwork>, store: &ChunkStore, dir: &Path, id: &str, opts: WatchOptions) -> Result<WatchEnd> {
    net.subscribe(STATUS_TOPIC).await?;
    let mut rx = net.messages();
    let mut rental = load_rentals(dir).into_iter().find(|r| r.id == id).with_context(|| format!("no rental {id} on this machine"))?;
    let mut health = HealthTracker::new(opts.fail_after);
    let mut status: Option<(StatusRecord, u64)> = None;
    let mut tick = tokio::time::interval(opts.probe_every);
    loop {
        if now_secs() >= rental.expires_at || rental.stopped_at.is_some() {
            return Ok(WatchEnd::Ended(rental));
        }
        tokio::select! {
            m = rx.recv() => {
                let msg = match m {
                    Ok(m) => m,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => bail!("network closed"),
                };
                if msg.topic != STATUS_TOPIC || msg.source.map(|p| p.to_base58()).as_deref() != Some(rental.worker.as_str()) {
                    continue;
                }
                let Ok(rec) = serde_json::from_slice::<StatusRecord>(&msg.data) else { continue };
                if rec.t != STATUS_KIND || rec.id != rental.id || rec.worker != rental.worker {
                    continue;
                }
                if let Some(b) = &rec.backup
                    && rental.latest_backup.as_deref() != Some(b.manifest.as_str())
                {
                    match download_backup(&net, store, dir, &b.manifest, &rental.worker).await {
                        Ok(_) => {
                            info!(id = %rental.id, manifest = %b.manifest, "gameserver watch: backup mirrored");
                            rental.latest_backup = Some(b.manifest.clone());
                            upsert_rental(dir, &rental)?;
                        }
                        Err(e) => warn!(id = %rental.id, err = %format!("{e:#}"), "gameserver watch: could not mirror backup"),
                    }
                }
                status = Some((rec, now_secs()));
            }
            _ = tick.tick() => {
                let p = probe_server(rental.game, &rental.address, opts.probe_timeout).await;
                let now = now_secs();
                let ok = judge(rental.game, &p, status.as_ref().map(|(s, at)| (s, *at)), now);
                log_probe(dir, &rental.id, &ProbeResult { ok, ..p.clone() });
                if !health.observe(now, ok) || !opts.failover {
                    continue;
                }
                warn!(id = %rental.id, worker = %rental.worker, "gameserver watch: failing checks; failing over");
                match failover(net.clone(), store, dir, &rental, &opts).await {
                    Ok(next) => {
                        let from = rental.clone();
                        if opts.stop_after_failover {
                            return Ok(WatchEnd::FailedOver { from, to: next });
                        }
                        info!(from = %from.id, to = %next.id, address = %next.address, "gameserver watch: failed over");
                        rental = next;
                        status = None;
                        health = HealthTracker::new(opts.fail_after);
                    }
                    Err(e) => {
                        warn!(id = %rental.id, err = %format!("{e:#}"), "gameserver watch: failover failed; retrying next check");
                        health.reset();
                    }
                }
            }
        }
    }
}

pub async fn watch(id: &str, opts: WatchOptions) -> Result<WatchEnd> {
    let sup = crate::Supervisor::boot_buyer(crate::Config::default()).await?;
    tokio::time::sleep(Duration::from_secs(5)).await;
    watch_with(sup.libp2p.clone(), &sup.store, &buyer_dir(), id, opts).await
}

/// Start `old`'s game on a different worker from its newest mirrored backup
/// for the rest of its reservation, and link the two rentals.
pub async fn failover(net: Arc<Libp2pNetwork>, store: &ChunkStore, dir: &Path, old: &Rental, opts: &WatchOptions) -> Result<Rental> {
    let left = old.expires_at.saturating_sub(now_secs());
    if left == 0 {
        bail!("the reservation already ended");
    }
    let req = StartRequest {
        game: old.game,
        hours: left.div_ceil(3600).clamp(1, u64::from(MAX_HOURS)) as u32,
        ram_gb: old.ram_gb.max(old.game.min_ram_gb()),
        cpus: if old.cpus > 0.0 { old.cpus } else { 2.0 },
        accept_eula: old.accept_eula,
        world: old.latest_backup.clone(),
        password: old.password.clone(),
        server_name: old.server_name.clone(),
        backup_mins: if old.backup_mins == 0 { 0 } else { old.backup_mins.clamp(MIN_BACKUP_MINS, MAX_BACKUP_MINS) },
        max_price_usd: opts.max_price_usd,
        workers: opts.workers.clone(),
        exclude: vec![old.worker.clone()],
        timeout: opts.start_timeout,
    };
    if req.world.is_none() {
        warn!(id = %old.id, "gameserver failover: no backup mirrored yet; the replacement starts a fresh world");
    }
    let mut next = start_with(net, store, dir, req).await?;
    next.replaces = Some(old.id.clone());
    upsert_rental(dir, &next)?;
    let mut prev = old.clone();
    prev.superseded_by = Some(next.id.clone());
    upsert_rental(dir, &prev)?;
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> String {
        crypto::keypair().public_hex
    }

    fn spec(game: Game) -> StartSpec {
        StartSpec {
            v: SPEC_VERSION,
            game,
            hours: 4,
            ram_gb: 4,
            cpus: 2.0,
            accept_eula: true,
            stop_hash: sha256_ref(b"t"),
            client_pubkey: key(),
            backup_pubkey: key(),
            backup_mins: 30,
            world: None,
            server_name: None,
        }
    }

    fn secrets(game: Game) -> StartSecrets {
        StartSecrets { v: 1, password: (game == Game::Valheim).then(|| "hunter22".into()), world_key: None }
    }

    #[test]
    fn spec_validation() {
        for g in Game::ALL {
            assert!(spec(g).validate().is_ok(), "{g:?}");
            assert!(secrets(g).validate(&spec(g)).is_ok(), "{g:?}");
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
        assert!(bad(&|s| s.server_name = Some("$(rm -rf /)".into())));
        assert!(bad(&|s| s.v = 1), "v1 offers put the password in public");
        assert!(bad(&|s| s.client_pubkey = "nope".into()));
        assert!(bad(&|s| s.backup_pubkey = String::new()));
        assert!(bad(&|s| s.backup_mins = 1));
        assert!(bad(&|s| s.backup_mins = MAX_BACKUP_MINS + 1));
        assert!(!bad(&|s| s.backup_mins = 0), "0 turns live backups off");
        // Factorio does not need the EULA flag.
        let mut f = spec(Game::Factorio);
        f.accept_eula = false;
        assert!(f.validate().is_ok());
        let mut v = spec(Game::Valheim);
        v.ram_gb = 2;
        assert!(v.validate().is_err(), "valheim needs 4 GB");
        // Unknown games never parse.
        let mut raw = serde_json::to_value(spec(Game::Minecraft)).unwrap();
        raw["game"] = json!("doom");
        assert!(serde_json::from_value::<StartSpec>(raw).is_err());
        assert_eq!(Game::parse(" Minecraft"), Some(Game::Minecraft));
        assert_eq!(Game::parse("terraria"), None);
    }

    #[test]
    fn secrets_never_ride_in_the_offer() {
        // There is no password field in a start spec at all.
        let raw = serde_json::to_string(&spec(Game::Valheim)).unwrap();
        assert!(!raw.contains("password") && !raw.contains("hunter22"), "{raw}");
        // Secrets are checked against the spec.
        let v = spec(Game::Valheim);
        assert!(StartSecrets { v: 1, password: None, world_key: None }.validate(&v).is_err(), "valheim needs one");
        assert!(StartSecrets { v: 1, password: Some("abc".into()), world_key: None }.validate(&v).is_err());
        assert!(StartSecrets { v: 1, password: Some("hunter22".into()), world_key: None }.validate(&spec(Game::Factorio)).is_err());
        let mut w = spec(Game::Minecraft);
        w.world = Some("a".repeat(64));
        assert!(StartSecrets { v: 1, password: None, world_key: None }.validate(&w).is_err(), "a restore needs its key");
        assert!(StartSecrets { v: 1, password: None, world_key: Some("b".repeat(64)) }.validate(&w).is_ok());
        assert!(StartSecrets { v: 1, password: None, world_key: Some("b".repeat(64)) }.validate(&spec(Game::Minecraft)).is_err());
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
    fn docker_argv_has_limits_ports_volume_expiry_and_no_shell() {
        let s = spec(Game::Minecraft);
        let plan = ContainerPlan::new("gs-0123456789abcdef", &s, &secrets(Game::Minecraft), 27500, 1_000, 1_000 + 4 * 3600);
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
        assert!(pair("--stop-timeout", "60"));
        assert!(pair("-p", "27500:25565/tcp"));
        assert!(pair("-v", "c0mpute-gs-0123456789abcdef:/data"));
        assert!(pair("-e", "EULA=TRUE"));
        assert!(pair("-e", "MEMORY=3328M"));
        assert!(pair("--label", &format!("c0mpute.gameserver.expires={}", 1_000 + 4 * 3600)));
        // Hard expiry without the worker: --rm + --init + timeout around the image's own command.
        assert!(a.contains(&"--rm".to_string()) && a.contains(&"--init".to_string()));
        assert!(pair("--entrypoint", "timeout"));
        let img = a.iter().position(|x| x == Game::Minecraft.image()).unwrap();
        assert_eq!(&a[img + 1..], &["-s", "TERM", "-k", "90s", "14400s", "/image/scripts/start"]);
        assert!(!a.iter().any(|x| x == "sh" || x == "-c" || x == "--privileged" || x.starts_with("--network=host")));
        // Valheim maps two consecutive UDP ports; its password comes from the sealed secrets.
        let v = ContainerPlan::new("gs-0123456789abcdef", &spec(Game::Valheim), &secrets(Game::Valheim), 27510, 1, 2).create_argv();
        assert!(v.windows(2).any(|w| w[0] == "-p" && w[1] == "27510:2456/udp"));
        assert!(v.windows(2).any(|w| w[0] == "-p" && w[1] == "27511:2457/udp"));
        assert!(v.windows(2).any(|w| w[0] == "-e" && w[1] == "SERVER_PASS=hunter22"));
        assert!(v.windows(2).any(|w| w[0] == "-v" && w[1] == "c0mpute-gs-0123456789abcdef:/config"));
        assert_eq!(v.last().unwrap(), "/usr/local/sbin/bootstrap");
        // Images are pinned by digest.
        for g in Game::ALL {
            assert!(g.image().contains("@sha256:"), "{g:?}");
            assert!(!g.command().is_empty());
        }
    }

    #[test]
    fn tar_argv_reads_the_volume_read_only_with_no_network() {
        let a = tar_argv("c0mpute-gs-x", Game::Factorio);
        assert!(a.windows(2).any(|w| w[0] == "--network" && w[1] == "none"));
        assert!(a.windows(2).any(|w| w[0] == "-v" && w[1] == "c0mpute-gs-x:/factorio:ro"));
        assert!(a.windows(2).any(|w| w[0] == "--entrypoint" && w[1] == "tar"));
        assert_eq!(a.last().unwrap(), "factorio");
    }

    #[test]
    fn labelled_listing_and_expired_containers() {
        let l = parse_labelled("c0mpute-gs-a\t100\nc0mpute-gs-b\t9999999999\nc0mpute-gs-c\t\n\n");
        assert_eq!(l, vec![("c0mpute-gs-a".into(), 100), ("c0mpute-gs-b".into(), 9_999_999_999), ("c0mpute-gs-c".into(), 0)]);
        assert_eq!(expired_containers(&l, 1_000), vec!["c0mpute-gs-a".to_string(), "c0mpute-gs-c".to_string()]);
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
            backup_pubkey: key(),
            backup_every_secs: 1800,
            stopped_at: stopped,
            backup: None,
            backup_error: None,
        }
    }

    #[test]
    fn expiry_and_backup_schedule() {
        assert_eq!(expires_at(100, 4), 100 + 4 * 3600);
        let mut m = HashMap::new();
        m.insert("a".into(), state("a", 1024, 1, 1_000, None));
        m.insert("b".into(), state("b", 1024, 2, 5_000, None));
        m.insert("c".into(), state("c", 1024, 3, 10, Some(20)));
        assert!(due_for_stop(&m, 999).is_empty());
        assert_eq!(due_for_stop(&m, 1_000), vec!["a".to_string()]);
        assert_eq!(due_for_stop(&m, 9_999), vec!["a".to_string(), "b".to_string()]);
        // Live backups: every 30 min from the start, then from the last backup.
        assert!(due_for_backup(&m, 1_799).is_empty());
        assert_eq!(due_for_backup(&m, 1_800), vec!["b".to_string()], "a is past expiry, c stopped");
        m.get_mut("b").unwrap().backup = Some(Backup { manifest: "x".into(), bytes: 1, sha256: "s".into(), at: 1_800 });
        assert!(due_for_backup(&m, 3_599).is_empty());
        assert_eq!(due_for_backup(&m, 3_600), vec!["b".to_string()]);
        m.get_mut("b").unwrap().backup_every_secs = 0;
        assert!(due_for_backup(&m, 4_000).is_empty(), "0 = no live backups");
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
        assert_eq!(split_address("[2001:db8::1]:5"), Some(("2001:db8::1".into(), 5)));
        assert_eq!(split_address("1.2.3.4:27500"), Some(("1.2.3.4".into(), 27500)));
        assert_eq!(split_address("nope"), None);
    }

    #[test]
    fn manifest_checks() {
        let mut m = WorldManifest::new(Game::Factorio, WorldEnc::handed_off());
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
        let mut plain = m.clone();
        plain.enc.scheme = "none".into();
        assert!(plain.check().is_err(), "unencrypted worlds are refused");
        let mut v1 = serde_json::to_value(&m).unwrap();
        v1["v"] = json!(1);
        v1.as_object_mut().unwrap().remove("enc");
        assert!(WorldManifest::from_bytes(&serde_json::to_vec(&v1).unwrap()).is_err());
    }

    #[tokio::test]
    async fn encrypted_chunking_round_trips_and_only_the_key_reads_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChunkStore::open(dir.path()).await.unwrap();
        let data: Vec<u8> = (0..(CHUNK_BYTES * 2 + 17)).map(|i| (i % 251) as u8).collect();
        let k = random_key();
        let m = chunk_into_store(&store, None, Game::Minecraft, &data[..], &k, WorldEnc::handed_off()).await.unwrap();
        m.check().unwrap();
        assert_eq!(m.chunks.len(), 3);
        assert_eq!(m.bytes, data.len() as u64);
        assert_eq!(m.sha256, sha256_ref(&data));
        let mut back = Vec::new();
        for (i, c) in m.chunks.iter().enumerate() {
            let sealed = store.get(&Hash::from_hex(c).unwrap()).await.unwrap();
            let n = (sealed.len() - TAG_BYTES).min(64);
            assert_ne!(&sealed[..n], &data[i * CHUNK_BYTES..i * CHUNK_BYTES + n], "chunks are stored encrypted");
            assert!(open_chunk(&random_key(), i as u64, &sealed).is_err(), "another key cannot read it");
            assert!(open_chunk(&k, i as u64 + 1, &sealed).is_err(), "a chunk cannot move position");
            back.extend(open_chunk(&k, i as u64, &sealed).unwrap());
        }
        assert_eq!(back, data);
        let empty = chunk_into_store(&store, None, Game::Minecraft, &b""[..], &k, WorldEnc::handed_off()).await.unwrap();
        assert!(empty.chunks.is_empty() && empty.check().is_ok());
    }

    #[test]
    fn backups_are_sealed_to_the_buyer_key() {
        let buyer = crypto::keypair();
        let e = crypto::keypair();
        let worker_side = backup_key(&e.secret, &buyer.public_hex, &e.public_hex, &buyer.public_hex).unwrap();
        let buyer_side = backup_key(&buyer.secret, &e.public_hex, &e.public_hex, &buyer.public_hex).unwrap();
        assert_eq!(worker_side, buyer_side);
        let eve = crypto::keypair();
        assert_ne!(worker_side, backup_key(&eve.secret, &e.public_hex, &e.public_hex, &buyer.public_hex).unwrap());
    }

    #[test]
    fn backup_key_file_is_created_once_and_private() {
        let dir = tempfile::tempdir().unwrap();
        let a = backup_keypair(dir.path()).unwrap();
        let b = backup_keypair(dir.path()).unwrap();
        assert_eq!(a.public_hex, b.public_hex);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("backup.key")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn minecraft_status_codec() {
        let req = minecraft_status_request("play.example.com", 25565);
        // length-prefixed handshake, then the 1-byte status request.
        assert_eq!(&req[req.len() - 2..], &[0x01, 0x00]);
        assert_eq!(req[0] as usize, req.len() - 3);
        let resp = minecraft_status_response(r#"{"version":{"name":"1.21"},"players":{"online":3,"max":10}}"#);
        let mut at = 0;
        let len = read_varint_slice(&resp, &mut at).unwrap() as usize;
        let v = parse_minecraft_status(&resp[at..at + len]).unwrap();
        assert_eq!(v["players"]["online"], 3);
        assert!(parse_minecraft_status(&[0x01, 0x00]).is_err());
        assert!(parse_minecraft_status(&[0x00, 0x05, b'{']).is_err());
        let mut big = Vec::new();
        put_varint(&mut big, 300);
        assert_eq!(big, vec![0xAC, 0x02]);
    }

    #[test]
    fn health_tracker_fails_over_only_after_a_sustained_outage() {
        let mut h = HealthTracker::new(Duration::from_secs(300));
        assert!(!h.observe(0, true));
        assert!(!h.observe(30, false));
        assert!(!h.observe(200, false));
        assert!(!h.observe(250, true), "one pass resets the outage");
        assert!(!h.observe(300, false));
        assert!(!h.observe(599, false));
        assert!(h.observe(600, false));
        assert_eq!(h.last_ok, Some(250));
    }

    #[test]
    fn judging_uses_the_signed_status_record() {
        let up = ProbeResult { ts: 0, ok: true, method: "x".into(), rtt_ms: 1, detail: String::new() };
        let down = ProbeResult { ok: false, ..up.clone() };
        let rec = StatusRecord { t: STATUS_KIND.into(), v: 1, id: "gs-x".into(), worker: "w".into(), game: Game::Minecraft, address: "a:1".into(), running: true, expires_at: 9, stopped_at: None, backup: None, ts: 0 };
        assert!(judge(Game::Minecraft, &up, None, 0));
        assert!(!judge(Game::Minecraft, &down, Some((&rec, 0)), 0));
        let stopped = StatusRecord { running: false, ..rec.clone() };
        assert!(!judge(Game::Minecraft, &up, Some((&stopped, 0)), 0), "the worker says it is down");
        // Factorio's probe is weak: it also needs a fresh "running" record.
        assert!(!judge(Game::Factorio, &up, None, 0));
        assert!(judge(Game::Factorio, &up, Some((&rec, 100)), 200));
        assert!(!judge(Game::Factorio, &up, Some((&rec, 0)), 500), "stale record");
    }

    #[tokio::test]
    async fn probes_answer_for_live_servers_and_fail_for_dead_ports() {
        // A Minecraft-status responder on loopback.
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 512];
                    let _ = s.read(&mut buf).await;
                    let _ = s.write_all(&minecraft_status_response(r#"{"version":{"name":"fake"},"players":{"online":0}}"#)).await;
                });
            }
        });
        let p = probe_server(Game::Minecraft, &format!("127.0.0.1:{port}"), Duration::from_secs(3)).await;
        assert!(p.ok, "{p:?}");
        assert_eq!(p.method, "minecraft-status");
        // An A2S responder (with a challenge round) on game port + 1.
        let game = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let gport = game.local_addr().unwrap().port();
        let q = tokio::net::UdpSocket::bind(("127.0.0.1", gport + 1)).await;
        if let Ok(q) = q {
            tokio::spawn(async move {
                let mut buf = [0u8; 256];
                while let Ok((n, from)) = q.recv_from(&mut buf).await {
                    let reply: Vec<u8> = if n == A2S_INFO.len() { b"\xFF\xFF\xFF\xFFA\x01\x02\x03\x04".to_vec() } else { b"\xFF\xFF\xFF\xFFI\x11fake".to_vec() };
                    let _ = q.send_to(&reply, from).await;
                }
            });
            let p = probe_server(Game::Valheim, &format!("127.0.0.1:{gport}"), Duration::from_secs(3)).await;
            assert!(p.ok, "{p:?}");
        }
        drop(game);
        // A closed port: refused (TCP) / unreachable (UDP).
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        assert!(!probe_server(Game::Minecraft, &format!("127.0.0.1:{closed}"), Duration::from_secs(2)).await.ok);
        let uclosed = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        assert!(!probe_server(Game::Factorio, &format!("127.0.0.1:{uclosed}"), Duration::from_secs(2)).await.ok);
    }
}
