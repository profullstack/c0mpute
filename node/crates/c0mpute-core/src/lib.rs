//! Core node coordination.
//!
//! Loads `~/.config/c0mpute/config.toml`, decides which roles to run based on
//! flags + config + detected hardware, and supervises the long-lived tasks
//! for each role.

pub mod capabilities;
pub mod config;
pub mod dispatch;
pub mod supervisor;

pub use capabilities::{Registry, advertise_loop, tags_from_config};
pub use dispatch::{run_worker_subscriber, workload_types_from_roles};
pub use config::Config;
pub use supervisor::Supervisor;

use anyhow::Result;
use tracing::info;

/// Convenience: install a default `tracing-subscriber` for the binary.
pub fn init_tracing() -> Result<()> {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,c0mpute=debug"));
    fmt().with_env_filter(filter).try_init().ok();
    info!("tracing initialised");
    Ok(())
}
