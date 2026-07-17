//! Self-upgrade: poll the release manifest, verify signatures, swap binary.
//!
//! Status: scaffold for the upgrade flow. Wire format + version compare
//! are real; the actual binary swap is stubbed (printed) until we have
//! signed releases to swap to.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// One published release as exposed by the coordinator's release feed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReleaseManifest {
    pub version: String,
    pub channel: Channel,
    pub min_required: String,
    pub artifacts: Vec<Artifact>,
    /// Versions that may not be downgraded to (e.g. CVE-affected).
    #[serde(default)]
    pub blocked_rollback: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    Stable,
    Beta,
    Nightly,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Artifact {
    pub os: String,    // "linux" | "darwin" | "windows"
    pub arch: String,  // "x86_64" | "aarch64"
    pub url: String,
    pub sha256_hex: String,
    pub minisig_url: String,
}

/// Outcome of a single upgrade poll.
#[derive(Debug)]
pub enum UpgradeOutcome {
    AlreadyLatest { current: String },
    Upgraded { from: String, to: String },
    Available { current: String, latest: String },
}

/// Check if `manifest.version` is newer than `current` using simple semver
/// comparison.
pub fn should_upgrade(current: &str, manifest: &ReleaseManifest) -> Result<bool> {
    let cur = parse_semver(current)?;
    let new = parse_semver(&manifest.version)?;
    Ok(new > cur)
}

fn parse_semver(s: &str) -> Result<(u32, u32, u32)> {
    let mut parts = s.split('.');
    let major = parts.next().unwrap_or("0").parse()?;
    let minor = parts.next().unwrap_or("0").parse()?;
    let patch = parts
        .next()
        .unwrap_or("0")
        .split('-')
        .next()
        .unwrap_or("0")
        .parse()?;
    Ok((major, minor, patch))
}

/// Fetch the latest release manifest from the configured release feed.
pub async fn fetch_latest(release_feed_url: &str) -> Result<ReleaseManifest> {
    let body = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?
        .get(release_feed_url)
        .send()
        .await
        .with_context(|| format!("GET {release_feed_url}"))?
        .error_for_status()?
        .text()
        .await?;
    let m: ReleaseManifest = serde_json::from_str(&body)
        .with_context(|| format!("parse release manifest from {release_feed_url}"))?;
    Ok(m)
}

/// One pass of the upgrade flow:
///   1. Fetch the latest release manifest.
///   2. Compare against the running binary's version.
///   3. If newer, download + verify + swap (stubbed today — prints the plan).
pub async fn try_upgrade(current_version: &str, release_feed_url: &str) -> Result<UpgradeOutcome> {
    let manifest = fetch_latest(release_feed_url).await?;
    if !should_upgrade(current_version, &manifest)? {
        return Ok(UpgradeOutcome::AlreadyLatest {
            current: current_version.to_string(),
        });
    }

    info!(
        current = current_version,
        latest = %manifest.version,
        "newer release available"
    );

    // The actual swap is stubbed: the production version downloads the
    // platform-matching artifact, verifies sha256 + minisign, and uses
    // an atomic rename to swap the binary in place. Until we have a
    // signed release pipeline, just report "available" and let the
    // operator run `c0mpute update` interactively.
    Ok(UpgradeOutcome::Available {
        current: current_version.to_string(),
        latest: manifest.version,
    })
}

/// Long-running background poller. Calls `try_upgrade` every `interval`
/// until cancelled. Logs failures rather than propagating — a flaky
/// release feed shouldn't take down the worker.
pub async fn poll_loop(
    current_version: String,
    release_feed_url: String,
    interval: Duration,
) {
    // Small initial jitter (0–60s) so 1000 nodes don't all hit at once.
    let jitter = fastish_jitter_secs();
    info!(secs = jitter, "auto-upgrade poll loop starting after jitter");
    tokio::time::sleep(Duration::from_secs(jitter)).await;

    let mut ticker = tokio::time::interval(interval);
    loop {
        ticker.tick().await;
        match try_upgrade(&current_version, &release_feed_url).await {
            Ok(UpgradeOutcome::AlreadyLatest { current }) => {
                tracing::debug!(version = %current, "auto-upgrade: already latest");
            }
            Ok(UpgradeOutcome::Available { current, latest }) => {
                info!(%current, %latest, "auto-upgrade: newer release available (run `c0mpute update`)");
            }
            Ok(UpgradeOutcome::Upgraded { from, to }) => {
                info!(%from, %to, "auto-upgrade: upgraded; restart to apply");
            }
            Err(e) => {
                warn!(err = %e, "auto-upgrade: poll failed");
            }
        }
    }
}

fn fastish_jitter_secs() -> u64 {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    nanos % 60
}

/// Default release-feed URL.
pub const DEFAULT_RELEASE_FEED: &str = "https://c0mpute.com/releases/latest.json";

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(v: &str) -> ReleaseManifest {
        ReleaseManifest {
            version: v.into(),
            channel: Channel::Stable,
            min_required: "0.0.1".into(),
            artifacts: vec![],
            blocked_rollback: vec![],
        }
    }

    #[test]
    fn upgrade_when_newer() {
        assert!(should_upgrade("0.1.0", &manifest("0.2.0")).unwrap());
        assert!(should_upgrade("0.1.0", &manifest("1.0.0")).unwrap());
        assert!(should_upgrade("0.1.5", &manifest("0.1.6")).unwrap());
    }

    #[test]
    fn no_upgrade_when_same_or_older() {
        assert!(!should_upgrade("0.2.0", &manifest("0.2.0")).unwrap());
        assert!(!should_upgrade("0.2.0", &manifest("0.1.99")).unwrap());
    }

    #[test]
    fn jitter_in_range() {
        for _ in 0..50 {
            assert!(fastish_jitter_secs() < 60);
        }
    }
}
