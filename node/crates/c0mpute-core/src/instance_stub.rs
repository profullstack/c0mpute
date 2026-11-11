//! Non-Unix stand-in for the worker identity lock (see `instance.rs`): no
//! flock, /proc or systemd there, so a second worker is not detected locally.
//! Twins are still handled on the network (`twin`, buyer re-award).

use std::path::{Path, PathBuf};

use anyhow::Result;

pub const EXIT_REPLACED: i32 = 90;
pub const EXIT_DUPLICATE: i32 = 91;
pub const RESTART_PREVENT_EXIT_STATUS: &str = "90 91";

/// Why a worker did not start: the CLI exits with [`EXIT_DUPLICATE`].
#[derive(Debug)]
pub struct DuplicateWorker(pub String);

impl std::fmt::Display for DuplicateWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DuplicateWorker {}

pub fn claim(_identity_dir: &Path) -> Result<()> {
    Ok(())
}

pub fn spawn_runtime_heartbeat(_identity_dir: PathBuf) {}

/// No per-process buyer identities off unix: the shared one, as before.
pub fn buyer_identity_dir(base: &Path) -> Result<std::path::PathBuf> {
    std::fs::create_dir_all(base)?;
    Ok(base.to_path_buf())
}
