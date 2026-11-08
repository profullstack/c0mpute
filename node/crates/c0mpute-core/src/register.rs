//! Local-side worker registration.
//!
//! `c0mpute worker register` ensures the box has the local state needed
//! for `c0mpute worker start` to succeed:
//!
//!   1. The libp2p ed25519 identity exists at `<config_dir>/identity.key`
//!      (generated if missing). The peer-id derived from it is the node's
//!      stable network identity.
//!   2. The config file exists at `<config_dir>/config.toml` with the
//!      defaults applied (creates a fresh one if missing).
//!   3. The storage root (`~/data/c0mpute` by default) exists on disk.
//!
//! This is purely local. The on-network registration (announcing the
//! peer-id + capabilities + DID to the swarm) happens when the supervisor
//! boots, and the CoinPay-side DID/registration is a separate flow
//! (`c0mpute coinpay reputation did claim`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use c0mpute_net::PeerId;

use crate::config::{Config, config_dir, default_config_path};

pub struct Registration {
    pub peer_id: PeerId,
    pub config_path: PathBuf,
    pub identity_path: PathBuf,
    pub storage_root: PathBuf,
    pub created_config: bool,
    pub created_identity: bool,
}

/// Idempotent: safe to call repeatedly. Reports what (if anything) was
/// freshly created so the CLI can show a tidy summary.
pub fn run_register(config_path_override: Option<&Path>) -> Result<Registration> {
    let config_path = config_path_override
        .map(Path::to_path_buf)
        .unwrap_or_else(default_config_path);

    let cfg_dir = config_dir().context("resolve XDG config dir")?;
    std::fs::create_dir_all(&cfg_dir)
        .with_context(|| format!("create_dir_all {}", cfg_dir.display()))?;

    let identity_path = cfg_dir.join("identity.key");
    let created_identity = !identity_path.exists();
    let keypair = c0mpute_net::identity::load_or_create(&cfg_dir)?;
    let peer_id = keypair.public().to_peer_id();

    let created_config = !config_path.exists();
    let cfg = Config::load_or_default(&config_path)?;
    if created_config {
        cfg.save(&config_path)
            .with_context(|| format!("write {}", config_path.display()))?;
    }

    std::fs::create_dir_all(&cfg.storage.root).with_context(|| {
        format!("create storage root {}", cfg.storage.root.display())
    })?;

    Ok(Registration {
        peer_id,
        config_path,
        identity_path,
        storage_root: cfg.storage.root,
        created_config,
        created_identity,
    })
}
