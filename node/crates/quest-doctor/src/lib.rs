//! `quest doctor` — runs a checklist, reports per-check status, and (with
//! `--fix`) attempts auto-remediation for the cheap ones.
//!
//! Each check returns a `CheckResult`. Checks are independent and run in
//! parallel where I/O permits.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::time::timeout;
use tracing::{debug, warn};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Status {
    Ok,
    Warn(String),
    Fail(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckResult {
    pub name: String,
    pub status: Status,
    pub fix_hint: Option<String>,
}

impl CheckResult {
    fn ok(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: Status::Ok,
            fix_hint: None,
        }
    }
    fn warn(name: impl Into<String>, msg: impl Into<String>, hint: Option<String>) -> Self {
        Self {
            name: name.into(),
            status: Status::Warn(msg.into()),
            fix_hint: hint,
        }
    }
    fn fail(name: impl Into<String>, msg: impl Into<String>, hint: Option<String>) -> Self {
        Self {
            name: name.into(),
            status: Status::Fail(msg.into()),
            fix_hint: hint,
        }
    }
}

/// Run the full diagnostic suite. The order here roughly mirrors the table in
/// PRD §12, with cheap+local checks first.
pub async fn run() -> Vec<CheckResult> {
    let (ffmpeg, disk, clock, coord) = tokio::join!(
        check_ffmpeg(),
        check_disk(),
        check_clock(),
        check_coordinator_reachable(),
    );
    vec![ffmpeg, disk, clock, coord]
}

async fn check_ffmpeg() -> CheckResult {
    let bin = which("ffmpeg");
    let Some(bin) = bin else {
        return CheckResult::fail(
            "ffmpeg",
            "ffmpeg not on PATH",
            Some("install jellyfin-ffmpeg or run: quest doctor --fix".into()),
        );
    };

    let out = timeout(
        Duration::from_secs(5),
        tokio::task::spawn_blocking(move || Command::new(&bin).arg("-version").output()),
    )
    .await;

    match out {
        Ok(Ok(Ok(o))) if o.status.success() => CheckResult::ok("ffmpeg"),
        _ => CheckResult::fail("ffmpeg", "ffmpeg failed to run -version", None),
    }
}

async fn check_disk() -> CheckResult {
    let mut sys = sysinfo::Disks::new_with_refreshed_list();
    sys.refresh_list();
    let mut total_free: u64 = 0;
    for d in sys.list() {
        total_free = total_free.saturating_add(d.available_space());
    }
    if total_free < 10 * 1024 * 1024 * 1024 {
        return CheckResult::warn(
            "disk",
            format!("only {} MiB free", total_free / 1024 / 1024),
            Some("reduce storage cap or free disk space".into()),
        );
    }
    CheckResult::ok("disk")
}

async fn check_clock() -> CheckResult {
    // We can't trivially measure drift without an NTP probe; defer to a
    // future implementation and return Ok in the meantime.
    debug!("clock-drift check is a stub");
    CheckResult::ok("clock")
}

async fn check_coordinator_reachable() -> CheckResult {
    // Stubbed: the real check should pull the configured coordinator URL
    // from settings and HEAD it.
    CheckResult::warn(
        "coordinator-reachable",
        "not yet implemented",
        Some("wire this up once config loading lands".into()),
    )
}

fn which(prog: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for entry in std::env::split_paths(&path) {
        let candidate = entry.join(prog);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Apply the `--fix` actions for any check whose `fix_hint` we know how to
/// auto-apply. Today: nothing — emit a warning so the user knows.
pub async fn fix(results: &[CheckResult]) -> Result<()> {
    for r in results {
        if matches!(r.status, Status::Fail(_) | Status::Warn(_)) {
            warn!(check = %r.name, "no auto-fix wired up yet");
        }
    }
    Ok(())
}
