//! Self-upgrade: poll the release manifest, verify signatures, swap binary.
//!
//! Every update is checked twice before the swap: sha256 (corruption) and a
//! minisign signature against the embedded production key [`RELEASE_PUBKEY`]
//! (authenticity). A missing or invalid signature aborts the update.

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
    pub os: String,   // "linux" | "darwin" | "windows"
    pub arch: String, // "x86_64" | "aarch64"
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

/// Whether this process is allowed to update itself. Operators can disable
/// background and opportunistic updates with `C0MPUTE_NO_AUTO_UPDATE=1`.
pub fn auto_update_enabled(configured: bool) -> bool {
    configured
        && !std::env::var("C0MPUTE_NO_AUTO_UPDATE")
            .map(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false)
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
    // Save the executable path before replacing its on-disk inode. After the
    // swap, launch the new binary's full benchmark under this node's identity
    // so its refreshed report is what the worker advertises on restart.
    let new_exe = std::env::current_exe().ok();
    perform_upgrade(&manifest).await?;
    if let Some(exe) = new_exe {
        if let Err(error) = run_post_update_benchmark(exe).await {
            warn!(err = %error, "post-update benchmark failed; continuing with the updated binary");
        }
    } else {
        warn!("could not locate updated executable to run the post-update benchmark");
    }
    Ok(UpgradeOutcome::Upgraded {
        from: current_version.to_string(),
        to: manifest.version,
    })
}

async fn run_post_update_benchmark(exe: std::path::PathBuf) -> Result<()> {
    let status = tokio::task::spawn_blocking(move || {
        std::process::Command::new(exe)
            .arg("benchmark")
            // The child already is the newly installed version; avoid another
            // release-feed request before it starts measuring this machine.
            .env("C0MPUTE_NO_AUTO_UPDATE", "1")
            .status()
    })
    .await
    .context("join post-update benchmark")??;
    if !status.success() {
        anyhow::bail!("`c0mpute benchmark` exited with {status}");
    }
    Ok(())
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
async fn resolve_artifact(manifest: &ReleaseManifest) -> Result<(String, String, String, String)> {
    let (os, arch) = target_os_arch()?;
    let artifact = format!("c0mpute-{os}-{arch}.tar.gz");

    if let Some(a) = manifest
        .artifacts
        .iter()
        .find(|a| a.os == os && a.arch == arch)
    {
        if !a.url.is_empty() && !a.sha256_hex.is_empty() {
            let sig_url = if a.minisig_url.is_empty() {
                format!("{}.minisig", a.url)
            } else {
                a.minisig_url.clone()
            };
            return Ok((
                a.url.clone(),
                a.sha256_hex.to_lowercase(),
                sig_url,
                artifact,
            ));
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
    let sig_url = format!("{url}.minisig");
    Ok((url, expected, sig_url, artifact))
}

/// Production minisign public key for c0mpute release artifacts. Must match
/// `minisign.pub` at the repo root (a unit test enforces it) and the key in
/// scripts/install.sh. Kept as a const rather than `include_str!` because the
/// Docker builds copy only `node/` into the Rust build stage.
pub const RELEASE_PUBKEY: &str = "RWQqOew3QbWhAWH36TA5roreh18RgRqgozSwhE3yZtdza7eeLKmbDbJM";

/// Verify a minisign signature (`sig_text`, the `.minisig` file contents) over
/// `bytes` with `pubkey_b64`. The signed trusted comment must name `artifact`
/// (`file:<artifact>`) and, when given, the release tag (`version:<tag>`), so a
/// validly signed file for another platform or release can't be swapped in.
pub fn verify_release_signature(
    pubkey_b64: &str,
    bytes: &[u8],
    sig_text: &str,
    artifact: &str,
    tag: Option<&str>,
) -> Result<()> {
    let pk = minisign_verify::PublicKey::from_base64(pubkey_b64)
        .map_err(|e| anyhow::anyhow!("bad release public key: {e}"))?;
    let sig = minisign_verify::Signature::decode(sig_text)
        .map_err(|e| anyhow::anyhow!("malformed signature for {artifact}: {e}"))?;
    pk.verify(bytes, &sig, false)
        .map_err(|e| anyhow::anyhow!("signature verification FAILED for {artifact}: {e}"))?;
    let comment = sig.trusted_comment();
    let names_file = comment
        .split_whitespace()
        .any(|w| w == format!("file:{artifact}"));
    if !names_file {
        anyhow::bail!("signature for {artifact} signs a different file ({comment})");
    }
    if let Some(tag) = tag {
        if !comment
            .split_whitespace()
            .any(|w| w == format!("version:{tag}"))
        {
            anyhow::bail!("signature for {artifact} is not for release {tag} ({comment})");
        }
    }
    Ok(())
}

fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?)
}

/// Download the artifact, verify its checksum, and swap it into place.
async fn perform_upgrade(manifest: &ReleaseManifest) -> Result<()> {
    let (url, expected_sha, sig_url, artifact) = resolve_artifact(manifest).await?;

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
        anyhow::bail!(
            "checksum mismatch for {artifact}: expected {expected_sha}, got {actual_sha}"
        );
    }

    // Fail closed: the sha256 above comes from the same origin as the binary,
    // so it only catches corruption. The minisign signature is what proves the
    // release came from us. No signature or a bad one means no swap.
    let sig_text = http_client()?
        .get(&sig_url)
        .send()
        .await
        .with_context(|| format!("GET {sig_url}"))?
        .error_for_status()
        .with_context(|| format!("no signature for {artifact} at {sig_url}; refusing to update"))?
        .text()
        .await?;
    let tag = format!("v{}", manifest.version.trim_start_matches('v'));
    verify_release_signature(RELEASE_PUBKEY, &bytes, &sig_text, &artifact, Some(&tag))?;
    info!("checksum + signature verified; swapping binary");

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

// ---- drain before re-exec ------------------------------------------------
//
// A re-exec replaces the process, so whatever it was doing dies with it: a
// song or image half rendered on a worker, a song a gateway was waiting on.
// Long jobs hold a `LongJob` guard; the poller re-execs only when none is
// running (or after MAX_DRAIN, so a stuck job cannot pin an old binary).

static LONG_JOBS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Held for the life of a job a restart would kill (see `poll_loop`).
#[must_use = "the job only counts while the guard is alive"]
pub struct LongJob(());

impl LongJob {
    pub fn start() -> Self {
        LONG_JOBS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        LongJob(())
    }
}

impl Drop for LongJob {
    fn drop(&mut self) {
        LONG_JOBS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// How many long jobs this process is running now.
pub fn long_jobs_running() -> usize {
    LONG_JOBS.load(std::sync::atomic::Ordering::SeqCst)
}

/// Longest the poller waits for running jobs before re-executing anyway.
pub const MAX_DRAIN: Duration = Duration::from_secs(30 * 60);

/// Wait (up to `max`) until no long job runs; returns how many still run.
pub async fn drain(max: Duration, every: Duration) -> usize {
    let started = std::time::Instant::now();
    let mut logged = false;
    loop {
        let running = long_jobs_running();
        if running == 0 || started.elapsed() >= max {
            return running;
        }
        if !logged {
            info!(running, "auto-upgrade: waiting for running jobs to finish before re-executing");
            logged = true;
        }
        tokio::time::sleep(every).await;
    }
}

/// Long-running background poller. Every `interval` it checks the feed and, if
/// a newer release exists, downloads + verifies + swaps the binary, then
/// re-executes into it so the update applies without an operator restart.
/// Failures are logged, never propagated — a flaky feed must not take down the
/// worker.
pub async fn poll_loop(current_version: String, release_feed_url: String, interval: Duration) {
    // Path to re-exec captured up front — after a swap, current_exe() reports
    // the now-unlinked inode ("… (deleted)"), which won't exec.
    let exe = std::env::current_exe().ok();

    // Small initial jitter (0–60s) so 1000 nodes don't all hit at once.
    let jitter = fastish_jitter_secs();
    info!(
        secs = jitter,
        "auto-upgrade poll loop starting after jitter"
    );
    tokio::time::sleep(Duration::from_secs(jitter)).await;

    let mut ticker = tokio::time::interval(interval);
    loop {
        ticker.tick().await;
        match upgrade_now(&current_version, &release_feed_url).await {
            Ok(UpgradeOutcome::AlreadyLatest { current }) => {
                tracing::debug!(version = %current, "auto-upgrade: already latest");
            }
            Ok(UpgradeOutcome::Upgraded { from, to }) => {
                info!(%from, %to, "auto-upgrade: installed new binary");
                #[cfg(unix)]
                if let Some(exe) = &exe {
                    let still = drain(MAX_DRAIN, Duration::from_secs(10)).await;
                    if still > 0 {
                        warn!(still, "auto-upgrade: jobs still running after the drain limit; re-executing anyway");
                    }
                    info!("auto-upgrade: re-executing into the new binary");
                    // reexec only returns on failure (success replaces us).
                    let e = reexec(exe);
                    warn!(err = %e, "auto-upgrade: re-exec failed; staged update applies on next restart");
                }
                // Update installed (and, on Unix, re-exec was attempted). Stop
                // polling so we don't re-download the same version while a
                // failed re-exec leaves us on the old process.
                return;
            }
            Ok(UpgradeOutcome::Available { .. }) => {
                unreachable!("upgrade_now installs or errors")
            }
            Err(e) => {
                warn!(err = %e, "auto-upgrade: poll failed");
            }
        }
    }
}

#[cfg(test)]
mod drain_tests {
    use super::*;

    #[tokio::test]
    async fn drain_waits_for_long_jobs_and_gives_up_at_the_limit() {
        assert_eq!(drain(Duration::from_millis(50), Duration::from_millis(5)).await, 0);
        let job = LongJob::start();
        assert_eq!(long_jobs_running(), 1);
        let t = std::time::Instant::now();
        assert_eq!(drain(Duration::from_millis(60), Duration::from_millis(5)).await, 1, "gives up at the limit");
        assert!(t.elapsed() >= Duration::from_millis(60));
        let waiter = tokio::spawn(drain(Duration::from_secs(5), Duration::from_millis(5)));
        tokio::time::sleep(Duration::from_millis(30)).await;
        drop(job);
        assert_eq!(waiter.await.unwrap(), 0, "returns once the job ends");
    }
}

/// Replace the current process with the (freshly swapped) binary at `exe`,
/// preserving PID, session, and the daemon's redirected stdio. Detach flags are
/// dropped — we are already the detached worker, so re-running `-d`/`-a` would
/// re-daemonize and orphan the PID file. Only returns (an error) on failure.
#[cfg(unix)]
fn reexec(exe: &std::path::Path) -> anyhow::Error {
    use std::os::unix::process::CommandExt;
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| !matches!(a.as_str(), "-d" | "--daemon" | "-a" | "--attach"))
        .collect();
    let err = std::process::Command::new(exe).args(args).exec();
    anyhow::anyhow!("exec {}: {err}", exe.display())
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

    // Test keypair (not the production key), generated with minisign 0.12.
    const TEST_PUBKEY: &str = "RWSgKx30JhkH5l0N/vDWkkaQiHCDHjU9rB8KsX8Wa5/b6CP9AcF3mXX1";
    const OTHER_PUBKEY: &str = "RWTTP1/dlNYVmhsaFYelmTJhVmvUctWmBqnIOBYeewyBgJf/KFdIzQYE";
    const TEST_PAYLOAD: &[u8] = b"c0mpute test artifact\n";
    const TEST_SIG: &str = "untrusted comment: signature from minisign secret key
RUSgKx30JhkH5uRp7VRolVR4bM+n44NjjP5/duvM3raMiSDyRlUhTyEYAKVmuWzEBxhLEQE8VKRybfYLgQ+53uUy38iJOMrkrwg=
trusted comment: timestamp:1 version:v9.9.9 file:c0mpute-linux-x86_64.tar.gz
Blqf7m00l6vWH+cZyF7MfEWS3yzPB72bD9SD4FKBHNxraBa9P0juzgFm7uK6VFEpq3Fn0wr21tqB4o9lMbP2DA==
";
    const ART: &str = "c0mpute-linux-x86_64.tar.gz";

    #[test]
    fn good_signature_passes() {
        verify_release_signature(TEST_PUBKEY, TEST_PAYLOAD, TEST_SIG, ART, Some("v9.9.9")).unwrap();
        verify_release_signature(TEST_PUBKEY, TEST_PAYLOAD, TEST_SIG, ART, None).unwrap();
    }

    #[test]
    fn tampered_bytes_fail() {
        let err =
            verify_release_signature(TEST_PUBKEY, b"c0mpute test artifacT\n", TEST_SIG, ART, None)
                .unwrap_err();
        assert!(err.to_string().contains("FAILED"), "{err}");
    }

    #[test]
    fn wrong_key_fails() {
        assert!(verify_release_signature(OTHER_PUBKEY, TEST_PAYLOAD, TEST_SIG, ART, None).is_err());
        assert!(
            verify_release_signature(RELEASE_PUBKEY, TEST_PAYLOAD, TEST_SIG, ART, None).is_err()
        );
    }

    #[test]
    fn wrong_file_or_release_fails() {
        assert!(verify_release_signature(
            TEST_PUBKEY,
            TEST_PAYLOAD,
            TEST_SIG,
            "c0mpute-darwin-aarch64.tar.gz",
            None
        )
        .is_err());
        assert!(
            verify_release_signature(TEST_PUBKEY, TEST_PAYLOAD, TEST_SIG, ART, Some("v9.9.8"))
                .is_err()
        );
    }

    #[test]
    fn missing_or_garbage_signature_fails() {
        assert!(verify_release_signature(TEST_PUBKEY, TEST_PAYLOAD, "", ART, None).is_err());
        assert!(
            verify_release_signature(TEST_PUBKEY, TEST_PAYLOAD, "<html>404</html>", ART, None)
                .is_err()
        );
    }

    #[test]
    fn release_pubkey_matches_repo_minisign_pub() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../minisign.pub");
        let file = std::fs::read_to_string(path).expect("read minisign.pub at repo root");
        assert_eq!(file.lines().nth(1).map(str::trim), Some(RELEASE_PUBKEY));
    }

    #[test]
    fn jitter_in_range() {
        for _ in 0..50 {
            assert!(fastish_jitter_secs() < 60);
        }
    }
}
