//! The container CLI the container workloads (`video`, `gameserver`) drive.
//!
//! `docker` by default. `C0MPUTE_DOCKER` names another binary with the same
//! command line: `podman`, a wrapper that adds `--context`, or a test double
//! that stands in for the daemon in the loopback end-to-end tests.

use std::ffi::OsString;
use std::path::PathBuf;

/// Environment variable that overrides the container CLI.
pub const ENV: &str = "C0MPUTE_DOCKER";

/// The container CLI to run: `$C0MPUTE_DOCKER`, else `docker`.
pub fn cli() -> OsString {
    match std::env::var_os(ENV) {
        Some(v) if !v.is_empty() => v,
        _ => OsString::from("docker"),
    }
}

/// Where [`cli`] resolves: the path itself when it names a file, else the
/// first match on `PATH`. `None` means there is no container runtime.
pub fn which() -> Option<PathBuf> {
    let bin = cli();
    let as_path = PathBuf::from(&bin);
    if as_path.components().count() > 1 {
        return as_path.is_file().then_some(as_path);
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(&bin))
            .find(|candidate| candidate.is_file())
    })
}

/// A `tokio` command for the container CLI.
pub fn command() -> tokio::process::Command {
    tokio::process::Command::new(cli())
}

/// A blocking command for the container CLI (readiness probes only).
pub fn command_sync() -> std::process::Command {
    std::process::Command::new(cli())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_docker() {
        // The variable is not set in the unit-test process.
        if std::env::var_os(ENV).is_none() {
            assert_eq!(cli(), OsString::from("docker"));
        }
    }
}
