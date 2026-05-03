//! Core node coordination.
//!
//! Loads `~/.quest/config.toml`, decides which roles to run based on flags +
//! config + detected hardware, and supervises the long-lived tasks for each
//! role. Today only the `Config` struct and a rudimentary `Supervisor` shell
//! are implemented; M0 fills in the role bodies.

pub mod config;
pub mod supervisor;

pub use config::Config;
pub use supervisor::Supervisor;

use anyhow::Result;
use tracing::info;

/// Convenience: install a default `tracing-subscriber` for the binary.
pub fn init_tracing() -> Result<()> {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,quest=debug"));
    fmt().with_env_filter(filter).try_init().ok();
    info!("tracing initialised");
    Ok(())
}
