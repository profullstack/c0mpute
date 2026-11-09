//! Core node coordination.
//!
//! Loads `~/.config/c0mpute/config.toml`, decides which roles to run based on
//! flags + config + detected hardware, and supervises the long-lived tasks
//! for each role.

pub mod activity;
pub mod apps;
pub mod ads;
pub mod auction_stats;
pub mod buyer;
pub mod canary;
pub mod capabilities;
pub mod config;
pub mod container;
pub mod crawl;
pub mod crowdgpt;
pub mod diffusion;
pub mod dm;
pub mod music;
pub mod job_progress;
pub mod job_process;
pub mod hosting;
pub mod live;
pub mod dispatch;
pub mod embed;
pub mod gameserver;
pub mod gateway_util;
pub mod gitremote;
pub mod handoff;
pub mod jobs_gateway;
pub mod net_guard;
pub mod ocr;
pub mod opendisk;
pub mod payments;
pub mod penalty_box;
pub mod pieces;
pub mod podcast;
pub mod pricing;
pub mod reachability;
pub mod render;
pub mod inference;
pub mod private;
pub mod register;
pub mod resources;
pub mod routing;
pub mod runner;
pub mod speech;
pub mod speech_gateway;
pub mod speech_profiles;
pub mod speech_session;
pub mod status_aggregator;
pub mod supervisor;
pub mod temporal;
pub mod text;
pub mod train;
pub mod train_verify;
pub mod video;
pub mod video_cpu;
pub mod webhook;
pub mod workflow;

pub use buyer::{AuctionOutcome, JobAuction, run_auction};
pub use capabilities::{Registry, advertise_loop, tags_from_config};
pub use config::Config;
pub use dispatch::{
    OfferFilter, run_worker_subscriber, run_worker_subscriber_filtered,
    run_worker_subscriber_priced, run_worker_subscriber_with_load, workload_types_from_roles,
};
pub use register::{Registration, run_register};
pub use runner::{SttJobInline, TranscodeJobInline, TtsJobInline, WorkerLoad};
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

/// The path to run for a peer binary that was resolved once at startup.
///
/// Long-running workers (`infernet text serve`, temporal serve) resolve
/// `infernet` when they start, but Infernet's installer and self-update can
/// move or prune the copy they found (0.1.87 pruned stray mise shims), and
/// every later job then failed with "No such file or directory". If the cached
/// path is gone, look the same file name up on PATH again.
pub fn live_bin(cached: &std::path::Path) -> std::path::PathBuf {
    if cached.is_file() {
        return cached.to_path_buf();
    }
    let found = cached.file_name().and_then(|name| {
        std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join(name))
                .find(|p| p.is_file())
        })
    });
    match found {
        Some(p) => {
            tracing::warn!(old = %cached.display(), new = %p.display(), "peer binary moved; using the copy now on PATH");
            p
        }
        None => cached.to_path_buf(),
    }
}

#[cfg(test)]
mod live_bin_tests {
    use super::live_bin;

    #[test]
    fn keeps_an_existing_path() {
        let exe = std::env::current_exe().unwrap();
        assert_eq!(live_bin(&exe), exe);
    }

    #[test]
    fn re_resolves_a_vanished_path_by_name_on_path() {
        // `sh` is on PATH everywhere the tests run; a cached path under a
        // directory that no longer exists must resolve to the PATH copy.
        let got = live_bin(std::path::Path::new("/nonexistent-c0mpute-dir/sh"));
        assert!(got.is_file(), "{got:?}");
        assert_ne!(got, std::path::Path::new("/nonexistent-c0mpute-dir/sh"));
    }

    #[test]
    fn leaves_an_unknown_binary_alone() {
        let p = std::path::Path::new("/nonexistent-c0mpute-dir/no-such-bin-c0mpute-test");
        assert_eq!(live_bin(p), p);
    }
}
