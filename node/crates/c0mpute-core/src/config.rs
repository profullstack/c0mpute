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
    /// CPU speech (TTS) provisioning. Every node serves text-to-speech by
    /// default; this is where an operator turns that off.
    #[serde(default)]
    pub speech: SpeechConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api: ApiConfig::default(),
            storage: StorageConfig::default(),
            gateway: GatewayConfig::default(),
            roles: vec![Role::Storage, Role::Gateway, Role::Verifier, Role::Tts],
            update_channel: default_update_channel(),
            update_auto: default_auto_update(),
            update_interval_secs: default_update_interval_secs(),
            update_feed_url: None,
            pricing: Default::default(),
            speech: SpeechConfig::default(),
        }
    }
}

/// CPU text-to-speech provisioning.
///
/// Kokoro is small enough (an int8 ONNX export of hexgrad/Kokoro-82M, about
/// 100 MB of model plus voices) that every node can serve speech, so
/// `auto_install` is on by default and the daemon provisions it in the
/// background the first time a worker starts. Operators opt out with
/// `c0mpute tts auto off`, `C0MPUTE_TTS_AUTO=0`, or this section.
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
            root: data_dir().unwrap_or_else(|| PathBuf::from("./c0mpute-data")),
            cap_bytes: None,
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

impl Config {
    pub fn load_or_default(path: &Path) -> Result<Self> {
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
