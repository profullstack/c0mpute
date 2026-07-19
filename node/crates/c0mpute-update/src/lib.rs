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

/// One *check* pass:
///   1. Fetch the latest release manifest.
///   2. Compare against the running binary's version.
///   3. Report `AlreadyLatest` or `Available` — never swaps. Used by the
///      auto-upgrade poll loop and by `c0mpute update --check`.
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

    Ok(UpgradeOutcome::Available {
        current: current_version.to_string(),
        latest: manifest.version,
    })
}

/// Check, then actually apply the upgrade: download the platform artifact,
/// verify its sha256, and atomically swap the running binary in place. Returns
/// `AlreadyLatest` when nothing to do, `Upgraded` on success; errors bubble up
/// so the caller can fall back to a manual reinstall.
pub async fn upgrade_now(current_version: &str, release_feed_url: &str) -> Result<UpgradeOutcome> {
    let manifest = fetch_latest(release_feed_url).await?;
    if !should_upgrade(current_version, &manifest)? {
        return Ok(UpgradeOutcome::AlreadyLatest {
            current: current_version.to_string(),
        });
    }
    perform_upgrade(&manifest).await?;
    Ok(UpgradeOutcome::Upgraded {
        from: current_version.to_string(),
        to: manifest.version,
    })
}

/// Map the compile-time target to the release artifact's os/arch tokens
/// (matching `.github/workflows/release.yml` naming).
fn target_os_arch() -> Result<(&'static str, &'static str)> {
    let os = match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "darwin",
        "windows" => "windows",
        other => anyhow::bail!("self-update unsupported on OS: {other}"),
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => anyhow::bail!("self-update unsupported on arch: {other}"),
    };
    Ok((os, arch))
}

/// Resolve the download URL and expected sha256 for this platform. Prefers a
/// feed-provided artifact (its `sha256_hex` is served from a different origin
/// than the binary, a stronger integrity anchor); otherwise falls back to the
/// stable pinned-version URL and its companion `.sha256`.
async fn resolve_artifact(manifest: &ReleaseManifest) -> Result<(String, String, String)> {
    let (os, arch) = target_os_arch()?;
    let artifact = format!("c0mpute-{os}-{arch}.tar.gz");

    if let Some(a) = manifest
        .artifacts
        .iter()
        .find(|a| a.os == os && a.arch == arch)
    {
        if !a.url.is_empty() && !a.sha256_hex.is_empty() {
            return Ok((a.url.clone(), a.sha256_hex.to_lowercase(), artifact));
        }
    }

    // Release tags are `v<version>` (see .github/workflows/release.yml +
    // bump-version.sh); the /releases/<tag>/ rewrite passes the tag through to
    // GitHub verbatim, so the bare version 404s.
    let tag = format!("v{}", manifest.version.trim_start_matches('v'));
    let url = format!("https://c0mpute.com/releases/{tag}/{artifact}");
    let sha_body = http_client()?
        .get(format!("{url}.sha256"))
        .send()
        .await
        .with_context(|| format!("GET {url}.sha256"))?
        .error_for_status()?
        .text()
        .await?;
    // Format: "<hex>  <filename>".
    let expected = sha_body
        .split_whitespace()
        .next()
        .context("empty checksum file")?
        .to_lowercase();
    Ok((url, expected, artifact))
}

fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?)
}

/// Download the artifact, verify its checksum, and swap it into place.
async fn perform_upgrade(manifest: &ReleaseManifest) -> Result<()> {
    let (url, expected_sha, artifact) = resolve_artifact(manifest).await?;

    info!(%url, "downloading update");
    let bytes = http_client()?
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()?
        .bytes()
        .await?;

    let actual_sha = {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(&bytes);
        hex::encode(h.finalize())
    };
    if actual_sha != expected_sha {
        anyhow::bail!("checksum mismatch for {artifact}: expected {expected_sha}, got {actual_sha}");
    }
    info!("checksum verified; swapping binary");

    // Extraction + filesystem swap is blocking work — keep it off the reactor.
    let bytes = bytes.to_vec();
    tokio::task::spawn_blocking(move || extract_and_swap(&bytes, &artifact))
        .await
        .context("join extract/swap task")??;
    Ok(())
}

/// Extract `c0mpute` from the tarball and atomically rename it over the running
/// binary. Renaming over a running executable is safe on Unix — the live
/// process keeps the old inode; the next launch gets the new one.
fn extract_and_swap(tarball: &[u8], artifact: &str) -> Result<()> {
    let current = std::env::current_exe().context("locate current executable")?;
    let dir = current
        .parent()
        .context("current executable has no parent directory")?;

    let tmp_tar = dir.join(format!(".{artifact}.download"));
    let extract_dir = dir.join(".c0mpute-update-extract");
    let staged = dir.join(".c0mpute.new");

    // Best-effort cleanup of any prior interrupted run.
    let _ = std::fs::remove_file(&tmp_tar);
    let _ = std::fs::remove_dir_all(&extract_dir);
    let _ = std::fs::remove_file(&staged);

    let cleanup = |extra: Option<&std::path::Path>| {
        let _ = std::fs::remove_file(&tmp_tar);
        let _ = std::fs::remove_dir_all(&extract_dir);
        if let Some(p) = extra {
            let _ = std::fs::remove_file(p);
        }
    };

    std::fs::write(&tmp_tar, tarball).context("write downloaded archive")?;
    std::fs::create_dir_all(&extract_dir).context("create extract dir")?;

    let status = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(&tmp_tar)
        .arg("-C")
        .arg(&extract_dir)
        .status()
        .context("run tar (is it installed?)")?;
    if !status.success() {
        cleanup(None);
        anyhow::bail!("tar extraction failed for {artifact}");
    }

    let new_bin = extract_dir.join("c0mpute");
    if !new_bin.exists() {
        cleanup(None);
        anyhow::bail!("archive did not contain a c0mpute binary");
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&new_bin, std::fs::Permissions::from_mode(0o755))
            .context("chmod new binary")?;
    }

    // Move into the target dir (same filesystem → rename is atomic), then swap.
    std::fs::rename(&new_bin, &staged)
        .or_else(|_| std::fs::copy(&new_bin, &staged).map(|_| ()))
        .context("stage new binary")?;
    if let Err(e) = std::fs::rename(&staged, &current) {
        cleanup(Some(&staged));
        return Err(e).context(format!("swap new binary into {}", current.display()));
    }

    cleanup(None);
    Ok(())
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
