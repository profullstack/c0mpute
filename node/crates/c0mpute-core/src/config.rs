//! On-disk node configuration.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use c0mpute_proto::Role;
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    // Each section defaults, so a partial config.toml that sets only what the
    // operator cares about (e.g. just `[storage] root`) loads cleanly instead
    // of erroring on the sections they left out.
    #[serde(default)]
    pub api: ApiConfig,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub gateway: GatewayConfig,
    #[serde(default)]
    pub roles: Vec<Role>,
    #[serde(default = "default_update_channel")]
    pub update_channel: String,
    #[serde(default = "default_auto_update")]
    pub update_auto: bool,
    /// Retained for compatibility with existing config files. Update checks
    /// use a fixed 60-second cadence.
    #[serde(default = "default_update_interval_secs")]
    pub update_interval_secs: u64,
    /// Override the release-feed URL.
    #[serde(default)]
    pub update_feed_url: Option<String>,
    /// The operator's listing: dynamic or fixed price per job, with a floor.
    #[serde(default)]
    pub pricing: crate::pricing::PricingConfig,
    /// Private mode: serve only your own buyers, stay off the public list.
    #[serde(default)]
    pub private: crate::private::PrivateConfig,
    /// CPU speech (TTS) provisioning. Every node serves text-to-speech by
    /// default; this is where an operator turns that off.
    #[serde(default)]
    pub speech: SpeechConfig,
    /// CPU image generation (`diffusion.txt2img`) provisioning and gates.
    #[serde(default)]
    pub diffusion: DiffusionConfig,
    /// Music generation (`music.txt2music`, ACE-Step 1.5) provisioning and gates.
    #[serde(default)]
    pub music: MusicConfig,
    /// Video generation. The GPU path needs nothing here; this is the CPU
    /// path (Wan 2.1 1.3B through stable-diffusion.cpp, opt-in).
    #[serde(default)]
    pub video: VideoConfig,
    /// Static-site hosting: how much this node holds for the network.
    #[serde(default)]
    pub hosting: HostingConfig,
    /// CPU text embeddings (`embed.text`) provisioning.
    #[serde(default)]
    pub embed: EmbedConfig,
    /// Captcha solving (`captcha.solve`, ddddocr) provisioning.
    #[serde(default)]
    pub captcha: CaptchaConfig,
    /// Ads: gateways serve VAST/VMAP and podcast ad insertion from the
    /// gossiped campaign index.
    #[serde(default)]
    pub ads: AdsConfig,
    /// Game server rentals (`gameserver` role, opt-in).
    #[serde(default)]
    pub gameserver: GameserverConfig,
    /// Apps (`apps` role, opt-in): how many backends this node runs for the
    /// network and how much memory they may take together.
    #[serde(default)]
    pub apps: AppsConfig,
    /// Money movement (hosting settlement; ads billing later). Off unless
    /// `enabled = true` and the rail's key is in the environment.
    #[serde(default)]
    pub payments: crate::payments::PaymentsConfig,
    /// Donate idle compute to CrowdGPT (`crowdgpt` plugin). Off by default;
    /// earns nothing; stops the moment paid work arrives.
    #[serde(default)]
    pub crowdgpt: crate::crowdgpt::CrowdgptConfig,
}

/// Ads on a gateway (`ads-manager`, `ctv-ads`, `podcast-ads`). Every gateway
/// keeps the campaign index; `serve = false` stops it answering ad requests
/// (VAST stays empty, podcast episodes are always served clean).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AdsConfig {
    #[serde(default = "default_true")]
    pub serve: bool,
    /// Splice ads into opted-in podcast episodes (needs ffmpeg + ffprobe).
    #[serde(default = "default_true")]
    pub podcast_insertion: bool,
    /// Gateway: where this gateway is paid for ads it serves on billed
    /// campaigns, `<COIN>:<address>` (e.g. "SOL:9xQe..."). Without it the
    /// gateway serves and signs receipts but never claims payment.
    #[serde(default)]
    pub payout: Option<String>,
    /// Gateway: smallest claim worth making, USD.
    #[serde(default = "default_min_claim_usd")]
    pub min_claim_usd: String,
    /// Advertiser: let `c0mpute ads settle --pay` send real money through
    /// the node's payment rail (also needs `[payments] enabled = true`). Off
    /// by default: settle only reports what it would pay.
    #[serde(default)]
    pub payouts: bool,
    /// Advertiser: where the 5% network fee is sent, `<COIN>:<address>`.
    /// Without it the fee is recorded as owed and not sent.
    #[serde(default)]
    pub fee_payout: Option<String>,
}

fn default_min_claim_usd() -> String {
    "1.00".into()
}

impl Default for AdsConfig {
    fn default() -> Self {
        Self {
            serve: true,
            podcast_insertion: true,
            payout: None,
            min_claim_usd: default_min_claim_usd(),
            payouts: false,
            fee_payout: None,
        }
    }
}

/// Game server rentals (`gameserver` role). Opt-in: a rented server holds a
/// port and RAM for hours. Players connect to `public_host:<port>`, so the
/// operator must open `port_min..=port_max` (TCP and UDP) in the firewall.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct GameserverConfig {
    /// Address players use (a public IP or DNS name). Default: the gateway's
    /// announce host, else `C0MPUTE_P2P_EXTERNAL`. No public address, no role.
    pub public_host: Option<String>,
    pub port_min: u16,
    pub port_max: u16,
    /// Servers running at once.
    pub max_servers: u32,
    /// RAM all rented servers may reserve together; 0 = half the box.
    pub max_total_ram_gb: u32,
    /// Longest reservation this node takes (capped at 72).
    pub max_hours: u32,
    /// Lowest price per GB of RAM per hour; offers capped below it are skipped.
    pub min_usd_per_gb_hour: f64,
    pub games: Vec<crate::gameserver::Game>,
}

/// `[apps]`: limits for the apps this node runs (`apps` role).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AppsConfig {
    /// Apps held at once (primary or standby).
    pub max_apps: usize,
    /// Memory all held apps may reserve together, MiB.
    pub max_memory_mb: u32,
    /// Run only apps signed by these owner keys (hex ed25519, as `c0mpute
    /// apps status` shows them). Empty = anyone's apps.
    pub owners: Vec<String>,
}

impl Default for AppsConfig {
    fn default() -> Self {
        Self {
            max_apps: 4,
            max_memory_mb: 2048,
            owners: Vec::new(),
        }
    }
}

impl Default for GameserverConfig {
    fn default() -> Self {
        Self {
            public_host: None,
            port_min: 27500,
            port_max: 27599,
            max_servers: 2,
            max_total_ram_gb: 0,
            max_hours: 24,
            min_usd_per_gb_hour: 0.0,
            games: crate::gameserver::Game::ALL.to_vec(),
        }
    }
}

/// Static-site hosting (`hosting` role). A hosting node holds copies of
/// sites so the network keeps three of each; every gateway serves them.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostingConfig {
    /// Hold sites for the network. On by default on every node, whatever
    /// `roles` lists; `false` (or `C0MPUTE_HOSTING=0`, or `worker start
    /// --no-hosting`) is the opt-out.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Most bytes of other people's sites this node will hold. `0` (the
    /// default) follows the disk: what it holds plus a quarter of the free
    /// space above the free-disk floor (see `hosting::auto_budget`).
    #[serde(default = "default_hosting_max_bytes")]
    pub max_bytes: u64,
    /// What this node asks to be a preferred (paid) host, USD per GiB-month.
    /// Published in the OpenDisk descriptor and in hosting gossip; a site
    /// offering less does not count this node as preferred.
    #[serde(default = "default_hosting_price")]
    pub price_usd_per_gib_month: f64,
    /// Where marketplace payments for holding sites go (a wallet address on
    /// `payout_coin`'s chain). Without it this node can still be preferred,
    /// but nobody can pay it.
    #[serde(default)]
    pub payout_wallet: Option<String>,
    #[serde(default = "crate::payments::default_coin")]
    pub payout_coin: String,
    /// Which sites this node challenges for proof of storage: `own` (sites
    /// its publisher key owns or pays for), `all` (also a sample of everyone
    /// else's, as a gateway might), or `off`.
    #[serde(default = "default_challenge")]
    pub challenge: String,
    /// Seconds between challenge rounds.
    #[serde(default = "default_challenge_every")]
    pub challenge_every_secs: u64,
    /// Seconds between settlement rounds (paying preferred holders of the
    /// sites this node pays for). Payments only move when `[payments]` is on.
    #[serde(default = "default_settle_every")]
    pub settle_every_secs: u64,
    /// Accept `git push` over HTTP on this gateway (`POST /git/v1/repos/...`,
    /// owner-signed, within `max_bytes`). Reads and clones are always served.
    #[serde(default = "default_git_pushes")]
    pub git_pushes: bool,
}

fn default_git_pushes() -> bool {
    true
}

impl Default for HostingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_bytes: default_hosting_max_bytes(),
            price_usd_per_gib_month: default_hosting_price(),
            payout_wallet: None,
            payout_coin: crate::payments::default_coin(),
            challenge: default_challenge(),
            challenge_every_secs: default_challenge_every(),
            settle_every_secs: default_settle_every(),
            git_pushes: default_git_pushes(),
        }
    }
}

impl HostingConfig {
    /// The hosting price in micro-USD per GiB-month (the default when unusable).
    pub fn price_micros(&self) -> crate::payments::Micros {
        let p = if self.price_usd_per_gib_month.is_finite() && self.price_usd_per_gib_month >= 0.0 {
            self.price_usd_per_gib_month
        } else {
            default_hosting_price()
        };
        (p * crate::payments::MICROS_PER_USD as f64).round() as crate::payments::Micros
    }

    /// The payout address, when set and well formed.
    pub fn payee(&self) -> Option<crate::payments::Payee> {
        let p = crate::payments::Payee {
            wallet: self.payout_wallet.clone()?.trim().to_string(),
            coin: self.payout_coin.trim().to_string(),
        };
        p.valid().then_some(p)
    }
}

fn default_hosting_price() -> f64 {
    crate::opendisk::DEFAULT_PRICE_USD_PER_GIB_MONTH
}

fn default_challenge() -> String {
    "own".into()
}

fn default_challenge_every() -> u64 {
    600
}

fn default_settle_every() -> u64 {
    24 * 60 * 60
}

fn default_hosting_max_bytes() -> u64 {
    crate::hosting::DEFAULT_MAX_BYTES
}

/// CPU text embeddings: fastembed (ONNX Runtime) in a per-user virtualenv with
/// BAAI/bge-small-en-v1.5, about 130 MB of model. Small enough that every node
/// provisions it in the background by default. Opt out with
/// `C0MPUTE_EMBED_AUTO=0` or `auto_install = false`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EmbedConfig {
    #[serde(default = "default_true")]
    pub auto_install: bool,
    /// Skip provisioning when the disk has less than this much free.
    #[serde(default = "default_embed_min_free_disk_gb")]
    pub min_free_disk_gb: u64,
}

impl Default for EmbedConfig {
    fn default() -> Self {
        Self {
            auto_install: true,
            min_free_disk_gb: default_embed_min_free_disk_gb(),
        }
    }
}

fn default_embed_min_free_disk_gb() -> u64 {
    3
}

impl EmbedConfig {
    pub fn auto_install_enabled(&self) -> bool {
        auto_install_enabled_with(self.auto_install, std::env::var("C0MPUTE_EMBED_AUTO").ok())
    }
}

/// Captcha solving (`captcha.solve`): ddddocr (MIT, ONNX, CPU, ~150 MB of
/// wheels, models included) in a per-user virtualenv. On by default; opt out
/// with `C0MPUTE_CAPTCHA_AUTO=0` or `auto_install = false`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CaptchaConfig {
    #[serde(default = "default_true")]
    pub auto_install: bool,
    /// Skip provisioning when the disk has less than this much free.
    #[serde(default = "default_captcha_min_free_disk_gb")]
    pub min_free_disk_gb: u64,
}

impl Default for CaptchaConfig {
    fn default() -> Self {
        Self {
            auto_install: true,
            min_free_disk_gb: default_captcha_min_free_disk_gb(),
        }
    }
}

fn default_captcha_min_free_disk_gb() -> u64 {
    1
}

impl CaptchaConfig {
    pub fn auto_install_enabled(&self) -> bool {
        auto_install_enabled_with(self.auto_install, std::env::var("C0MPUTE_CAPTCHA_AUTO").ok())
    }
}

/// CPU image generation: stable-diffusion.cpp + Z-Image-Turbo, ~6.7 GB of
/// weights and ~9 GB of RAM per job. On by default, but a node installs it only
/// with `min_ram_gb` total RAM and `min_free_disk_gb` free disk, and bids only
/// while `min_free_ram_gb` is available. Opt out with `C0MPUTE_DIFFUSION_AUTO=0`
/// or `auto_install = false`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiffusionConfig {
    #[serde(default = "default_true")]
    pub auto_install: bool,
    #[serde(default = "default_diffusion_min_ram_gb")]
    pub min_ram_gb: u64,
    #[serde(default = "default_diffusion_min_free_ram_gb")]
    pub min_free_ram_gb: u64,
    #[serde(default = "default_diffusion_min_free_disk_gb")]
    pub min_free_disk_gb: u64,
    /// Smallest per-job CPU thread budget (`resources::cpu_threads_per_job`,
    /// half the box by default) worth serving images with. At 8 threads a
    /// 768x512 image takes ~2-4 min; at 3 it passes the 15-minute job limit.
    #[serde(default = "default_diffusion_min_cpu_threads")]
    pub min_cpu_threads: usize,
    /// Busiest the box may be (1-minute load average per logical CPU) and
    /// still take a render. A low-priority container on a busy host can set
    /// this high: its renders only use idle CPU.
    #[serde(default = "default_diffusion_max_load_per_cpu")]
    pub max_load_per_cpu: f64,
}

impl Default for DiffusionConfig {
    fn default() -> Self {
        Self {
            auto_install: true,
            min_ram_gb: default_diffusion_min_ram_gb(),
            min_free_ram_gb: default_diffusion_min_free_ram_gb(),
            min_free_disk_gb: default_diffusion_min_free_disk_gb(),
            min_cpu_threads: default_diffusion_min_cpu_threads(),
            max_load_per_cpu: default_diffusion_max_load_per_cpu(),
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_diffusion_min_ram_gb() -> u64 {
    16
}
fn default_diffusion_min_free_ram_gb() -> u64 {
    // A job peaks at ~4 GB with --params-backend disk; keep ~4 GB spare.
    8
}
fn default_diffusion_min_free_disk_gb() -> u64 {
    25
}
fn default_diffusion_min_cpu_threads() -> usize {
    8
}
fn default_diffusion_max_load_per_cpu() -> f64 {
    crate::diffusion::MAX_LOAD_PER_CPU
}

impl DiffusionConfig {
    pub fn auto_install_enabled(&self) -> bool {
        auto_install_enabled_with(
            self.auto_install,
            std::env::var("C0MPUTE_DIFFUSION_AUTO").ok(),
        )
    }
}

/// Music generation: ACE-Step 1.5 (CPU, or CUDA on an NVIDIA GPU), ~8 GB on disk and ~7-9 GB of RAM per
/// song (bf16 on CPU). On by default, installed only with `min_ram_gb` total
/// RAM and `min_free_disk_gb` free disk; bids only while `min_free_ram_gb` is
/// available. Opt out with `C0MPUTE_MUSIC_AUTO=0` or `auto_install = false`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MusicConfig {
    #[serde(default = "default_true")]
    pub auto_install: bool,
    #[serde(default = "default_music_min_ram_gb")]
    pub min_ram_gb: u64,
    #[serde(default = "default_music_min_free_ram_gb")]
    pub min_free_ram_gb: u64,
    #[serde(default = "default_music_min_free_disk_gb")]
    pub min_free_disk_gb: u64,
    /// Smallest per-job CPU thread budget worth serving songs with.
    #[serde(default = "default_diffusion_min_cpu_threads")]
    pub min_cpu_threads: usize,
    /// Busiest the box may be (1-minute load per logical CPU) and still take a song.
    #[serde(default = "default_diffusion_max_load_per_cpu")]
    pub max_load_per_cpu: f64,
}

impl Default for MusicConfig {
    fn default() -> Self {
        Self {
            auto_install: true,
            min_ram_gb: default_music_min_ram_gb(),
            min_free_ram_gb: default_music_min_free_ram_gb(),
            min_free_disk_gb: default_music_min_free_disk_gb(),
            min_cpu_threads: default_diffusion_min_cpu_threads(),
            max_load_per_cpu: default_diffusion_max_load_per_cpu(),
        }
    }
}

fn default_music_min_ram_gb() -> u64 {
    // A CPU song peaks at 8.5 GB RSS; 16 GB boxes qualify (most gaming PCs).
    16
}
fn default_music_min_free_ram_gb() -> u64 {
    // A bf16 DiT-only song peaked at 8.5 GB RSS (measured 2026-11-09).
    10
}
fn default_music_min_free_disk_gb() -> u64 {
    30
}

impl MusicConfig {
    pub fn auto_install_enabled(&self) -> bool {
        auto_install_enabled_with(self.auto_install, std::env::var("C0MPUTE_MUSIC_AUTO").ok())
    }
}

/// CPU video (`video_cpu`): Wan 2.1 T2V 1.3B on stable-diffusion.cpp.
///
/// Opt-in, because a 33-frame clip holds 16 threads for many minutes:
/// - `cpu` unset (the default): serve only if an operator installed it with
///   `c0mpute video setup --cpu`; never download anything on its own.
/// - `cpu = true` (or `C0MPUTE_VIDEO_CPU=1`): install in the background on a
///   box that passes the gates, then serve.
/// - `cpu = false` (or `C0MPUTE_VIDEO_CPU=0`): never serve, even if installed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VideoConfig {
    #[serde(default)]
    pub cpu: Option<bool>,
    #[serde(default = "default_video_min_ram_gb")]
    pub min_ram_gb: u64,
    #[serde(default = "default_video_min_free_ram_gb")]
    pub min_free_ram_gb: u64,
    #[serde(default = "default_video_min_free_disk_gb")]
    pub min_free_disk_gb: u64,
    /// Smallest per-job CPU thread budget worth serving clips with.
    #[serde(default = "default_video_min_cpu_threads")]
    pub min_cpu_threads: usize,
    /// Busiest the box may be (1-minute load per logical CPU) and still take a clip.
    #[serde(default = "default_diffusion_max_load_per_cpu")]
    pub max_load_per_cpu: f64,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            cpu: None,
            min_ram_gb: default_video_min_ram_gb(),
            min_free_ram_gb: default_video_min_free_ram_gb(),
            min_free_disk_gb: default_video_min_free_disk_gb(),
            min_cpu_threads: default_video_min_cpu_threads(),
            max_load_per_cpu: default_diffusion_max_load_per_cpu(),
        }
    }
}

fn default_video_min_ram_gb() -> u64 {
    16
}
fn default_video_min_free_ram_gb() -> u64 {
    // sd-cli holds ~1.8 GB RSS with the encoder and VAE streamed from disk
    // (cgroup peak 4.8 GB with page cache; dev2, 2026-11-09). The headroom
    // is for the host: a clip runs for ~25 minutes, and on a box that dips
    // low in that window an OOM guard kills it.
    8
}
fn default_video_min_free_disk_gb() -> u64 {
    // 5.4 GB of weights on top of the sd.cpp runtime, plus frames.
    12
}
fn default_video_min_cpu_threads() -> usize {
    // The per-job budget is half the box (`resources::cpu_threads_per_job`),
    // so 16 here demanded a 32-core machine and a 16-core box that runs the
    // CPU clip fine (bonita, 2026-11-09) never bid. Same floor as images.
    8
}

impl VideoConfig {
    /// `Some(true)`: install and serve; `Some(false)`: never; `None`: serve
    /// only what an operator installed by hand. The environment wins.
    pub fn cpu_setting(&self) -> Option<bool> {
        match std::env::var("C0MPUTE_VIDEO_CPU") {
            Ok(value) if !value.trim().is_empty() => {
                Some(auto_install_enabled_with(true, Some(value)))
            }
            _ => self.cpu,
        }
    }
}

/// CPU text-to-speech provisioning.
///
/// Kokoro is small enough — an int8 ONNX export of hexgrad/Kokoro-82M, about
/// 115 MB of model plus voices, CPU-only — that every node can serve speech,
/// so `auto_install` is on by default and a worker provisions it in the
/// background on first start. Operators opt out with `c0mpute tts auto off`,
/// `C0MPUTE_TTS_AUTO=0`, or this section.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SpeechConfig {
    #[serde(default = "default_speech_auto_install")]
    pub auto_install: bool,
    /// Skip provisioning when the disk has less than this much free.
    #[serde(default = "default_speech_min_free_disk_gb")]
    pub min_free_disk_gb: u64,
}

impl Default for SpeechConfig {
    fn default() -> Self {
        Self {
            auto_install: default_speech_auto_install(),
            min_free_disk_gb: default_speech_min_free_disk_gb(),
        }
    }
}

fn default_speech_auto_install() -> bool {
    true
}

fn default_speech_min_free_disk_gb() -> u64 {
    5
}

impl SpeechConfig {
    /// The config value, with `C0MPUTE_TTS_AUTO=0` (or `false`/`no`/`off`)
    /// winning over it so an operator can turn provisioning off without
    /// editing TOML.
    pub fn auto_install_enabled(&self) -> bool {
        auto_install_enabled_with(self.auto_install, std::env::var("C0MPUTE_TTS_AUTO").ok())
    }
}

/// Resolve the auto-install decision from the config value and the raw
/// environment override. Split out so it is testable without touching the
/// process environment.
pub(crate) fn auto_install_enabled_with(configured: bool, env_override: Option<String>) -> bool {
    match env_override {
        Some(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        None => configured,
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api: ApiConfig::default(),
            storage: StorageConfig::default(),
            gateway: GatewayConfig::default(),
            roles: vec![
                Role::Storage,
                Role::Gateway,
                Role::Verifier,
                Role::Tts,
                Role::Stt,
                Role::WorkflowStep,
                Role::Diffusion,
                Role::Music,
                // Subscribes only where it can serve: a ready GPU runner, or
                // CPU video the operator opted into.
                Role::Video,
                Role::Crawl,
                Role::Hosting,
                Role::Ocr,
                Role::Embed,
                Role::Captcha,
                Role::LiveStream,
                Role::Render,
                Role::Inference,
                Role::Mailbox,
            ],
            update_channel: default_update_channel(),
            update_auto: default_auto_update(),
            update_interval_secs: default_update_interval_secs(),
            update_feed_url: None,
            pricing: Default::default(),
            private: Default::default(),
            speech: SpeechConfig::default(),
            diffusion: DiffusionConfig::default(),
            music: MusicConfig::default(),
            video: VideoConfig::default(),
            hosting: HostingConfig::default(),
            ads: AdsConfig::default(),
            embed: EmbedConfig::default(),
            captcha: CaptchaConfig::default(),
            gameserver: GameserverConfig::default(),
            apps: AppsConfig::default(),
            payments: Default::default(),
            crowdgpt: Default::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApiConfig {
    /// Coordinator base URL — defaults to the production deployment.
    pub base_url: String,
    pub token: Option<String>,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            base_url: "https://c0mpute.com/".into(),
            token: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StorageConfig {
    /// Be a disk provider: store shards for the network and offer this disk
    /// on d1sks. On by default on every node, whatever `roles` lists; `false`
    /// (or `C0MPUTE_STORAGE=0`, or `worker start --no-storage`) is the opt-out.
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_storage_root")]
    pub root: PathBuf,
    pub cap_bytes: Option<u64>,
    /// The price this node asks to hold data, per GiB-month in USD. Published
    /// in its OpenDisk descriptor (`/.well-known/opendisk.json`).
    #[serde(default = "default_storage_price")]
    pub price_usd_per_gib_month: f64,
    /// Hex ed25519 public keys allowed to write to this node's storage API,
    /// besides the node's own identity. Reads are open (a blake3 hash is the
    /// capability); writes are signed so a stranger cannot fill the disk.
    #[serde(default)]
    pub trusted_writers: Vec<String>,
    /// Serve `/.well-known/opendisk.json` and list this disk on d1sks when
    /// the gateway announces a public https origin. On by default.
    #[serde(default = "default_true")]
    pub opendisk: bool,
    /// The OpenDisk marketplace this node registers with.
    #[serde(default = "default_d1sks_url")]
    pub d1sks_url: String,
    /// List this disk on d1sks with a signed heartbeat (every few minutes,
    /// expiring when the node goes quiet), NAT or not. On by default;
    /// `false` or `C0MPUTE_D1SKS=0` keeps the disk off the marketplace while
    /// it still stores for the network. Private mode never lists.
    #[serde(default = "default_true")]
    pub list_on_d1sks: bool,
    /// ISO 3166-1 alpha-2 country this disk is in, published in the listing
    /// so publishers can pick hosts by country (`C0MPUTE_COUNTRY` wins).
    #[serde(default)]
    pub country: Option<String>,
}

fn default_storage_root() -> PathBuf {
    data_dir().unwrap_or_else(|| PathBuf::from("./c0mpute-data"))
}

fn default_storage_price() -> f64 {
    crate::opendisk::DEFAULT_PRICE_USD_PER_GIB_MONTH
}

fn default_d1sks_url() -> String {
    crate::opendisk::DEFAULT_D1SKS_URL.to_string()
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            root: default_storage_root(),
            cap_bytes: None,
            price_usd_per_gib_month: default_storage_price(),
            trusted_writers: Vec::new(),
            opendisk: true,
            d1sks_url: default_d1sks_url(),
            list_on_d1sks: true,
            country: None,
        }
    }
}

// Storage root is XDG-standard `~/.local/share/c0mpute`. install.sh may
// symlink this to a larger volume (e.g. `/mnt/data/c0mpute`) at install
// time when one is detected — see `detect_storage_volume` in install.sh,
// modelled on infernet's relocate flow. The Rust side stays unaware of
// the symlink: same path, bigger disk underneath.

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GatewayConfig {
    pub bind: String,
    pub announce: Option<String>,
    /// With no `announce`, find this node's public IPv4 and announce
    /// `http://<ip>:<port>` once the gateway answers there
    /// (`gateway_announce`). Opt out: false, or `C0MPUTE_GATEWAY_AUTO=0`.
    #[serde(default = "default_true")]
    pub auto_announce: bool,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:7777".into(),
            announce: None,
            auto_announce: true,
        }
    }
}

fn default_update_channel() -> String {
    "stable".into()
}
fn default_auto_update() -> bool {
    true
}
fn default_update_interval_secs() -> u64 {
    UPDATE_CHECK_INTERVAL_SECS
}

pub const UPDATE_CHECK_INTERVAL_SECS: u64 = 60;

pub fn config_dir() -> Option<PathBuf> {
    // ~/.config/c0mpute on Linux, equivalent on macOS. Mirrors the install
    // layout (~/.c0mpute/bin per DIP-0005).
    ProjectDirs::from("com", "c0mpute", "c0mpute").map(|d| d.config_dir().to_path_buf())
}

pub fn data_dir() -> Option<PathBuf> {
    ProjectDirs::from("com", "c0mpute", "c0mpute").map(|d| d.data_dir().to_path_buf())
}

pub fn default_config_path() -> PathBuf {
    config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("config.toml")
}

/// An on/off switch from the environment: `0`/`false`/`no`/`off` is off,
/// anything else set is on, unset keeps `configured`.
fn env_switch(name: &str, configured: bool) -> bool {
    auto_install_enabled_with(configured, std::env::var(name).ok().filter(|v| !v.trim().is_empty()))
}

impl Config {
    /// Storage (disk provider) is on: the config and `C0MPUTE_STORAGE` agree.
    pub fn storage_enabled(&self) -> bool {
        env_switch("C0MPUTE_STORAGE", self.storage.enabled)
    }

    /// Hosting (holding sites) is on: the config and `C0MPUTE_HOSTING` agree.
    pub fn hosting_enabled(&self) -> bool {
        env_switch("C0MPUTE_HOSTING", self.hosting.enabled)
    }

    /// This disk goes on d1sks: storage on, OpenDisk on, listing on,
    /// `C0MPUTE_D1SKS` not off, and not private mode.
    pub fn lists_on_d1sks(&self) -> bool {
        self.roles.contains(&Role::Storage)
            && self.storage.opendisk
            && env_switch("C0MPUTE_D1SKS", self.storage.list_on_d1sks)
            && !self.private.enabled
    }

    /// The country to publish: `C0MPUTE_COUNTRY`, else `[storage] country`,
    /// when it is a two-letter code.
    pub fn country(&self) -> Option<String> {
        let raw = std::env::var("C0MPUTE_COUNTRY").ok().or_else(|| self.storage.country.clone())?;
        let c = raw.trim().to_ascii_uppercase();
        (c.len() == 2 && c.chars().all(|ch| ch.is_ascii_uppercase())).then_some(c)
    }

    /// Every node is a disk provider: `storage` and `hosting` join the roles
    /// even when `roles` (or `--roles`) lists others, unless the operator
    /// turned them off (`enabled = false` or the env switch), which also
    /// takes them out of an explicit list. Private mode adds nothing: a
    /// private node keeps exactly the roles it asked for.
    pub fn apply_disk_provider_defaults(&mut self) {
        let wanted = [
            (Role::Storage, self.storage_enabled()),
            (Role::Hosting, self.hosting_enabled()),
        ];
        for (role, on) in wanted {
            if !on {
                self.roles.retain(|r| *r != role);
            } else if !self.private.enabled && !self.roles.contains(&role) {
                self.roles.push(role);
            }
        }
    }

    pub fn load_or_default(path: &Path) -> Result<Self> {
        let mut cfg = Self::load_raw(path)?;
        cfg.apply_disk_provider_defaults();
        Ok(cfg)
    }

    /// The file as written, without the disk-provider defaults applied.
    pub fn load_raw(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }

        // Canonicalize path to resolve symlinks and ".." components, preventing
        // path traversal attacks.
        let canonical = path
            .canonicalize()
            .with_context(|| format!("canonicalize config path {}", path.display()))?;

        // Validate the canonical path is within the expected config directory.
        if let Some(cfg_dir) = config_dir() {
            if let Ok(canonical_cfg_dir) = cfg_dir.canonicalize() {
                if !canonical.starts_with(&canonical_cfg_dir) {
                    anyhow::bail!(
                        "config path {} is outside allowed directory {}",
                        canonical.display(),
                        canonical_cfg_dir.display()
                    );
                }
            }
        }

        let text = std::fs::read_to_string(&canonical)
            .with_context(|| format!("read config {}", canonical.display()))?;
        let mut cfg: Config = toml::from_str(&text)
            .with_context(|| format!("parse config {}", canonical.display()))?;
        // Older configs contain a 300-second cadence. Keep the serialized
        // field for backwards compatibility, but normalize it to the fixed
        // 60-second updater interval when loading them.
        cfg.update_interval_secs = UPDATE_CHECK_INTERVAL_SECS;
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)?;
        std::fs::write(path, text)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_role_list_still_provides_disk() {
        let mut cfg: Config = toml::from_str("roles = [\"tts\", \"stt\"]\n").unwrap();
        cfg.apply_disk_provider_defaults();
        assert!(cfg.roles.contains(&Role::Storage));
        assert!(cfg.roles.contains(&Role::Hosting));
        assert!(cfg.roles.contains(&Role::Tts));
        assert!(cfg.storage.list_on_d1sks);
    }

    #[test]
    fn enabled_false_is_the_opt_out_even_from_an_explicit_list() {
        let mut cfg: Config = toml::from_str(
            "roles = [\"storage\", \"hosting\", \"tts\"]\n[storage]\nenabled = false\n[hosting]\nenabled = false\n",
        )
        .unwrap();
        cfg.apply_disk_provider_defaults();
        assert!(!cfg.roles.contains(&Role::Storage));
        assert!(!cfg.roles.contains(&Role::Hosting));
        assert!(!cfg.lists_on_d1sks());
    }

    #[test]
    fn private_mode_adds_nothing_and_never_lists() {
        let mut cfg: Config = toml::from_str("roles = [\"tts\"]\n[private]\nenabled = true\n").unwrap();
        cfg.apply_disk_provider_defaults();
        assert_eq!(cfg.roles, vec![Role::Tts]);
        let mut cfg = Config::default();
        cfg.private.enabled = true;
        assert!(!cfg.lists_on_d1sks());
    }

    #[test]
    fn listing_can_be_turned_off_alone() {
        let mut cfg: Config = toml::from_str("[storage]\nlist_on_d1sks = false\n").unwrap();
        cfg.apply_disk_provider_defaults();
        assert!(cfg.roles.contains(&Role::Storage), "still stores for the network");
        assert!(!cfg.lists_on_d1sks());
        assert!(Config::default().lists_on_d1sks());
    }

    #[test]
    fn game_servers_are_opt_in() {
        let config = Config::default();
        assert!(!config.roles.contains(&Role::Gameserver), "a rented server holds a port and RAM for hours");
        let parsed: Config = toml::from_str("[gameserver]\npublic_host = \"play.example.com\"\ngames = [\"minecraft\"]\n").unwrap();
        assert_eq!(parsed.gameserver.public_host.as_deref(), Some("play.example.com"));
        assert_eq!(parsed.gameserver.games, vec![crate::gameserver::Game::Minecraft]);
        assert_eq!(parsed.gameserver.port_min, 27500);
    }

    #[test]
    fn every_node_serves_tts_by_default() {
        let config = Config::default();
        assert!(
            config.roles.contains(&Role::Tts),
            "speech is a house capability: a node with no config should still offer it"
        );
        assert!(config.speech.auto_install);
    }

    #[test]
    fn a_config_without_a_speech_section_still_auto_installs() {
        // Config files written before the section existed must keep working.
        let config: Config = toml::from_str("roles = [\"storage\"]\n").unwrap();
        assert!(config.speech.auto_install);
        assert_eq!(config.speech.min_free_disk_gb, 5);
    }

    #[test]
    fn an_operator_can_turn_provisioning_off_in_toml() {
        let config: Config = toml::from_str("[speech]\nauto_install = false\n").unwrap();
        assert!(!config.speech.auto_install);
    }

    #[test]
    fn the_env_override_wins_over_the_config_value_both_ways() {
        for off in ["0", "false", "no", "off", " OFF "] {
            assert!(
                !auto_install_enabled_with(true, Some(off.to_string())),
                "{off:?} should turn provisioning off"
            );
        }
        assert!(auto_install_enabled_with(false, Some("1".to_string())));
        assert!(auto_install_enabled_with(true, None));
        assert!(!auto_install_enabled_with(false, None));
    }
}
