//! Self-upgrade: poll the release manifest, verify signatures, swap binary.
//!
//! Status: scaffold. The real implementation pulls from
//! `https://depin.quest/video/api/v1/releases/latest`, verifies a minisign
//! signature, and uses an atomic rename to swap the binary in place. For now
//! we only model the manifest types and the version-comparison logic.

use anyhow::Result;
use serde::{Deserialize, Serialize};

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

/// Check if `manifest.version` is newer than `current` using simple semver
/// comparison. Returns `Ok(true)` if we should upgrade.
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
}
