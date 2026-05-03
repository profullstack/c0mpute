//! On-disk node configuration.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::{BaseDirs, ProjectDirs};
use c0mpute_proto::Role;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub api: ApiConfig,
    pub storage: StorageConfig,
    pub gateway: GatewayConfig,
    #[serde(default)]
    pub roles: Vec<Role>,
    #[serde(default = "default_update_channel")]
    pub update_channel: String,
    #[serde(default = "default_auto_update")]
    pub update_auto: bool,
    /// How often the worker polls the release feed for new versions. Defaults
    /// to 5 minutes per user direction; override via config or env.
    #[serde(default = "default_update_interval_secs")]
    pub update_interval_secs: u64,
    /// Override the release-feed URL.
    #[serde(default)]
    pub update_feed_url: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api: ApiConfig::default(),
            storage: StorageConfig::default(),
            gateway: GatewayConfig::default(),
            roles: vec![Role::Storage, Role::Gateway, Role::Verifier],
            update_channel: default_update_channel(),
            update_auto: default_auto_update(),
            update_interval_secs: default_update_interval_secs(),
            update_feed_url: None,
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
    pub root: PathBuf,
    pub cap_bytes: Option<u64>,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            root: default_storage_root(),
            cap_bytes: None,
        }
    }
}

/// Default location for encrypted shards. We deliberately put this at
/// `~/data/c0mpute` rather than the XDG data dir (`~/.local/share/c0mpute`)
/// so operators can find, monitor, and migrate the disk that holds bulk
/// shard data without spelunking dotfiles.
pub fn default_storage_root() -> PathBuf {
    if let Some(base) = BaseDirs::new() {
        base.home_dir().join("data").join("c0mpute")
    } else {
        PathBuf::from("./c0mpute-data")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GatewayConfig {
    pub bind: String,
    pub announce: Option<String>,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:7777".into(),
            announce: None,
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
    300 // 5 minutes
}

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

impl Config {
    pub fn load_or_default(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read config {}", path.display()))?;
        let cfg: Config = toml::from_str(&text)
            .with_context(|| format!("parse config {}", path.display()))?;
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
