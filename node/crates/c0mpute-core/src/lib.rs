//! Core node coordination.
//!
//! Loads `~/.config/c0mpute/config.toml`, decides which roles to run based on
//! flags + config + detected hardware, and supervises the long-lived tasks
//! for each role.

pub mod buyer;
pub mod capabilities;
pub mod config;
pub mod dispatch;
pub mod register;
pub mod runner;
pub mod status_aggregator;
pub mod supervisor;

pub use buyer::{AuctionOutcome, JobAuction, run_auction};
pub use capabilities::{Registry, advertise_loop, tags_from_config};
pub use config::Config;
pub use dispatch::{run_worker_subscriber, workload_types_from_roles};
pub use register::{Registration, run_register};
pub use runner::TranscodeJobInline;
pub use supervisor::Supervisor;

use anyhow::Result;
use tracing::info;

/// Convenience: install a default `tracing-subscriber` for the binary.
pub fn init_tracing() -> Result<()> {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,c0mpute=debug"));
    // Diagnostics go to stderr so stdout stays a clean data channel: commands
    // like `c0mpute storage put` print only an object hash, which callers
    // capture with `$(...)`. With the default stdout writer the logs land in
    // that capture and every scripted use breaks.
    //
    // Daemon mode is unaffected: daemonize_worker points stdout and stderr at
    // the same log file.
    fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init()
        .ok();
    info!("tracing initialised");
    Ok(())
}
