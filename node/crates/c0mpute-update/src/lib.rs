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
        && !running_dev_build()
}

/// True when `exe` is a cargo build output (some directory on its path is
/// named `target`). Such a binary must never replace itself with the published
/// release: a freshly built branch reports an older version than the feed and
/// would silently become the release mid-test. `c0mpute update` still works.
pub fn is_dev_build_path(exe: &std::path::Path) -> bool {
    exe.parent()
        .is_some_and(|dir| dir.components().any(|c| c.as_os_str() == "target"))
}

fn running_dev_build() -> bool {
    current_exe_path().is_some_and(|exe| is_dev_build_path(&exe))
}

/// Linux reports a replaced (unlinked) executable as `<path> (deleted)`. That
/// name neither execs nor is where the next binary belongs, so strip it.
pub fn strip_deleted_suffix(exe: std::path::PathBuf) -> std::path::PathBuf {
    match exe.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
        Some(clean) => std::path::PathBuf::from(clean),
        None => exe,
    }
}

/// The path this binary was installed at, usable after a swap: resolve it
/// before replacing the file and keep the value, or call this at any time.
pub fn current_exe_path() -> Option<std::path::PathBuf> {
    std::env::current_exe().ok().map(strip_deleted_suffix)
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

/// What to do with the new binary's benchmark once an update is installed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostBench {
    /// Run it and wait (bounded by [`POST_BENCH_TIMEOUT`]), output shown: the
    /// worker's poller and `c0mpute update`.
    Wait,
    /// Start it and return at once with its output discarded: a CLI command
    /// that updated in passing must not wait on it or print its table into
    /// that command's stdout (`c0mpute status --json | jq`).
    Detached,
}

/// Longest an update waits for the new binary's benchmark.
pub const POST_BENCH_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Check, then actually apply the upgrade: download the platform artifact,
/// verify its sha256, and atomically swap the running binary in place. Returns
/// `AlreadyLatest` when nothing to do, `Upgraded` on success; errors bubble up
/// so the caller can fall back to a manual reinstall.
pub async fn upgrade_now(current_version: &str, release_feed_url: &str) -> Result<UpgradeOutcome> {
    upgrade_with(current_version, release_feed_url, PostBench::Wait).await
}

/// [`upgrade_now`] with a choice of how the post-update benchmark runs.
pub async fn upgrade_with(current_version: &str, release_feed_url: &str, bench: PostBench) -> Result<UpgradeOutcome> {
    let manifest = fetch_latest(release_feed_url).await?;
    if !should_upgrade(current_version, &manifest)? {
        return Ok(UpgradeOutcome::AlreadyLatest {
            current: current_version.to_string(),
        });
    }
    install(current_version, &manifest, bench).await
}

/// Install `manifest`'s release over the running binary (the caller checked it
/// is newer).
async fn install(current_version: &str, manifest: &ReleaseManifest, bench: PostBench) -> Result<UpgradeOutcome> {
    // Resolve the executable path before replacing its on-disk inode: after
    // the swap `current_exe()` names the unlinked file ("... (deleted)").
    // The same path is where the new binary goes and what the benchmark runs.
    let exe = current_exe_path().context("locate current executable")?;
    perform_upgrade(manifest, &exe).await?;
    // Launch the new binary's full benchmark under this node's identity so
    // its refreshed report is what the worker advertises on restart.
    if let Err(error) = run_post_update_benchmark(exe, bench).await {
        warn!(err = %error, "post-update benchmark failed; continuing with the updated binary");
    }
    Ok(UpgradeOutcome::Upgraded {
        from: current_version.to_string(),
        to: manifest.version.clone(),
    })
}

async fn run_post_update_benchmark(exe: std::path::PathBuf, bench: PostBench) -> Result<()> {
    let mut command = std::process::Command::new(exe);
    command
        .arg("benchmark")
        // The child already is the newly installed version; avoid another
        // release-feed request before it starts measuring this machine.
        .env("C0MPUTE_NO_AUTO_UPDATE", "1")
        .stdin(std::process::Stdio::null());
    if bench == PostBench::Detached {
        command.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        let mut child = command.spawn().context("start post-update benchmark")?;
        // Reap it whenever it ends; nobody waits on the result.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        return Ok(());
    }
    let status = tokio::task::spawn_blocking(move || -> Result<std::process::ExitStatus> {
        let mut child = command.spawn().context("start post-update benchmark")?;
        let started = std::time::Instant::now();
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(status);
            }
            if started.elapsed() >= POST_BENCH_TIMEOUT {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("`c0mpute benchmark` ran over {}s; stopped", POST_BENCH_TIMEOUT.as_secs());
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    })
    .await
    .context("join post-update benchmark")??;
    if !status.success() {
        anyhow::bail!("`c0mpute benchmark` exited with {status}");
    }
    Ok(())
}

/// The release artifact for a platform, exactly as
/// `.github/workflows/release.yml` names it: a zip on Windows, a tarball
/// everywhere else.
pub fn artifact_name(os: &str, arch: &str) -> String {
    let ext = if os == "windows" { "zip" } else { "tar.gz" };
    format!("c0mpute-{os}-{arch}.{ext}")
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
    let artifact = artifact_name(os, arch);

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
async fn perform_upgrade(manifest: &ReleaseManifest, exe: &std::path::Path) -> Result<()> {
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
    let exe = exe.to_path_buf();
    tokio::task::spawn_blocking(move || extract_and_swap(&bytes, &artifact, &exe))
        .await
        .context("join extract/swap task")??;
    Ok(())
}

/// Name stem of every scratch file an update leaves next to the binary.
const SCRATCH_PREFIX: &str = ".c0mpute-update-";

/// A scratch name no other process shares. The worker's poller and CLI
/// commands all update within the same minute of a release; with fixed names
/// each run deleted the other's half-written files.
fn scratch_tag() -> String {
    format!("{}-{:08x}", std::process::id(), stagger_secs(u32::MAX as u64))
}

/// Remove scratch left by an update that died mid-way (a crash, a full disk).
/// Only entries older than an hour, so a concurrent update keeps its files.
fn sweep_stale_scratch(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with(SCRATCH_PREFIX) {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > Duration::from_secs(60 * 60));
        if old {
            let path = entry.path();
            if std::fs::remove_dir_all(&path).is_err() {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

/// Removes an update's scratch directory on every exit path.
struct Scratch(std::path::PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The binary's file name inside a release archive.
fn archived_binary(artifact: &str) -> &'static str {
    if artifact.ends_with(".zip") {
        "c0mpute.exe"
    } else {
        "c0mpute"
    }
}

/// Extract the binary from the archive and atomically rename it over
/// `current`, the path resolved before anything was replaced. Renaming over a
/// running executable is safe on Unix: the live process keeps the old inode;
/// the next launch gets the new one.
fn extract_and_swap(archive: &[u8], artifact: &str, current: &std::path::Path) -> Result<()> {
    let dir = current
        .parent()
        .context("current executable has no parent directory")?;
    sweep_stale_scratch(dir);

    // Everything lives in one per-process directory beside the binary (same
    // filesystem, so the final rename is atomic).
    let scratch = Scratch(dir.join(format!("{SCRATCH_PREFIX}{}", scratch_tag())));
    std::fs::create_dir_all(&scratch.0).context("create update scratch dir")?;
    let tmp_archive = scratch.0.join(artifact);
    let extract_dir = scratch.0.join("extract");
    let staged = scratch.0.join("c0mpute.new");

    std::fs::write(&tmp_archive, archive).context("write downloaded archive")?;
    std::fs::create_dir_all(&extract_dir).context("create extract dir")?;

    // bsdtar (what Windows 10+ ships as tar.exe) reads zip archives too.
    let flags = if artifact.ends_with(".zip") { "-xf" } else { "-xzf" };
    let status = std::process::Command::new("tar")
        .arg(flags)
        .arg(&tmp_archive)
        .arg("-C")
        .arg(&extract_dir)
        .status()
        .context("run tar (is it installed?)")?;
    if !status.success() {
        anyhow::bail!("tar extraction failed for {artifact}");
    }

    let new_bin = extract_dir.join(archived_binary(artifact));
    if !new_bin.exists() {
        anyhow::bail!("archive did not contain a c0mpute binary");
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&new_bin, std::fs::Permissions::from_mode(0o755))
            .context("chmod new binary")?;
    }

    std::fs::rename(&new_bin, &staged).context("stage new binary")?;
    swap_into_place(&staged, current).with_context(|| format!("swap new binary into {}", current.display()))
}

#[cfg(not(windows))]
fn swap_into_place(staged: &std::path::Path, current: &std::path::Path) -> std::io::Result<()> {
    std::fs::rename(staged, current)
}

/// Windows refuses to overwrite a running executable but lets it be renamed:
/// move it aside, put the new one in its place, and put the old one back if
/// that fails. The moved-aside file is removed by a later update's sweep.
#[cfg(windows)]
fn swap_into_place(staged: &std::path::Path, current: &std::path::Path) -> std::io::Result<()> {
    let dir = current.parent().unwrap_or(std::path::Path::new("."));
    let aside = dir.join(format!("{SCRATCH_PREFIX}old-{}.exe", scratch_tag()));
    std::fs::rename(current, &aside)?;
    if let Err(e) = std::fs::rename(staged, current) {
        let _ = std::fs::rename(&aside, current);
        return Err(e);
    }
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

/// How many long jobs this process is running now: [`LongJob`] guards plus
/// whatever the registered [`set_busy_probe`] reports (a worker's won jobs
/// whose receipt has not gone out yet).
pub fn long_jobs_running() -> usize {
    let probe = BUSY_PROBE.get().map(|f| f()).unwrap_or(0);
    LONG_JOBS.load(std::sync::atomic::Ordering::SeqCst) + probe
}

static BUSY_PROBE: std::sync::OnceLock<fn() -> usize> = std::sync::OnceLock::new();

/// Tell the poller how to count this process's running jobs, so it never
/// re-executes under one. First registration wins.
pub fn set_busy_probe(probe: fn() -> usize) {
    let _ = BUSY_PROBE.set(probe);
}

static DRAINING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// True while an installed update waits for running jobs to finish. Workers
/// stop bidding then, so the drain ends and no won job dies in the re-exec.
pub fn draining() -> bool {
    DRAINING.load(std::sync::atomic::Ordering::SeqCst)
}

/// Stop taking new work for good: this process is about to exit (a newer
/// worker with the same identity is taking over). Running jobs finish.
pub fn begin_drain() {
    set_draining(true);
}

fn set_draining(on: bool) {
    DRAINING.store(on, std::sync::atomic::Ordering::SeqCst);
}

/// Longest an installed update waits, at random, before draining. Releases
/// land every few minutes on busy days and every node polls the same feed:
/// without a spread the whole network restarts within one poll interval and
/// every gossip mesh re-forms at once.
pub const MAX_UPGRADE_STAGGER: Duration = Duration::from_secs(120);

/// Longest the poller waits for running jobs before re-executing anyway.
/// Covers the longest job a worker takes (a video clip may render for 2 h,
/// `c0mpute_core::video::JOB_TIMEOUT`), so an update never kills one that
/// was awarded just before it landed.
pub const MAX_DRAIN: Duration = Duration::from_secs(2 * 60 * 60 + 5 * 60);

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

/// Longest wait between polls after repeated failures.
pub const MAX_POLL_BACKOFF: Duration = Duration::from_secs(6 * 60 * 60);

/// How long to wait after failed polls. Doubles per consecutive failure of the
/// same thing (a release version that will not install, or the feed itself),
/// from the normal interval up to [`MAX_POLL_BACKOFF`]. A release that cannot
/// be installed here (full disk, read-only binary directory, missing artifact)
/// used to be downloaded in full again every minute, forever.
#[derive(Debug, Default)]
pub struct PollBackoff {
    key: Option<String>,
    failures: u32,
}

impl PollBackoff {
    /// Record a failure of `key`; returns the wait before the next poll.
    pub fn failed(&mut self, key: &str, interval: Duration) -> Duration {
        if self.key.as_deref() != Some(key) {
            self.key = Some(key.to_string());
            self.failures = 0;
        }
        self.failures = self.failures.saturating_add(1);
        let factor = 1u32.checked_shl(self.failures.min(31)).unwrap_or(u32::MAX);
        interval.saturating_mul(factor).min(MAX_POLL_BACKOFF).max(interval)
    }

    /// A poll went through: the next failure starts from the interval again.
    pub fn succeeded(&mut self) {
        self.key = None;
        self.failures = 0;
    }
}

/// Long-running background poller. Every `interval` it checks the feed and, if
/// a newer release exists, downloads + verifies + swaps the binary, then
/// re-executes into it so the update applies without an operator restart.
/// Failures are logged, never propagated: a flaky feed must not take down the
/// worker. Repeated failures back off (see [`PollBackoff`]).
pub async fn poll_loop(current_version: String, release_feed_url: String, interval: Duration) {
    // Path to re-exec captured up front: after a swap, current_exe() reports
    // the now-unlinked inode ("... (deleted)"), which won't exec.
    let exe = current_exe_path();

    // Small initial jitter (0–60s) so 1000 nodes don't all hit at once.
    let jitter = fastish_jitter_secs();
    info!(
        secs = jitter,
        "auto-upgrade poll loop starting after jitter"
    );
    tokio::time::sleep(Duration::from_secs(jitter)).await;

    let mut backoff = PollBackoff::default();
    let mut wait = Duration::ZERO;
    loop {
        tokio::time::sleep(wait).await;
        wait = interval;
        let manifest = match fetch_latest(&release_feed_url).await {
            Ok(m) => m,
            Err(e) => {
                wait = backoff.failed("feed", interval);
                warn!(err = %e, retry_secs = wait.as_secs(), "auto-upgrade: poll failed");
                continue;
            }
        };
        match should_upgrade(&current_version, &manifest) {
            Ok(true) => {}
            Ok(false) => {
                backoff.succeeded();
                tracing::debug!(version = %current_version, "auto-upgrade: already latest");
                continue;
            }
            Err(e) => {
                wait = backoff.failed("feed", interval);
                warn!(err = %e, retry_secs = wait.as_secs(), "auto-upgrade: poll failed");
                continue;
            }
        }
        match install(&current_version, &manifest, PostBench::Wait).await {
            Ok(UpgradeOutcome::Upgraded { from, to }) => {
                info!(%from, %to, "auto-upgrade: installed new binary");
                #[cfg(unix)]
                if let Some(exe) = &exe {
                    // Spread restarts across the network, keep working
                    // meanwhile, then stop taking work and let it finish.
                    let stagger = stagger_secs(MAX_UPGRADE_STAGGER.as_secs());
                    info!(secs = stagger, "auto-upgrade: restarting after a random stagger");
                    tokio::time::sleep(Duration::from_secs(stagger)).await;
                    set_draining(true);
                    let still = drain(MAX_DRAIN, Duration::from_secs(5)).await;
                    if still > 0 {
                        warn!(still, "auto-upgrade: jobs still running after the drain limit; re-executing anyway");
                    }
                    info!("auto-upgrade: re-executing into the new binary");
                    // reexec only returns on failure (success replaces us).
                    let e = reexec(exe);
                    set_draining(false);
                    warn!(err = %e, "auto-upgrade: re-exec failed; staged update applies on next restart");
                }
                // Update installed (and, on Unix, re-exec was attempted). Stop
                // polling so we don't re-download the same version while a
                // failed re-exec leaves us on the old process.
                return;
            }
            Ok(_) => unreachable!("install installs or errors"),
            Err(e) => {
                wait = backoff.failed(&manifest.version, interval);
                warn!(err = %format!("{e:#}"), version = %manifest.version, retry_secs = wait.as_secs(), "auto-upgrade: install failed");
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

/// A pseudo-random 0..max seconds, different per process and per call.
fn stagger_secs(max: u64) -> u64 {
    if max == 0 {
        return 0;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    (nanos ^ (std::process::id() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)) % max
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
    fn stagger_in_range() {
        for _ in 0..100 {
            assert!(stagger_secs(120) < 120);
        }
        assert_eq!(stagger_secs(0), 0);
    }

    #[test]
    fn jitter_in_range() {
        for _ in 0..50 {
            assert!(fastish_jitter_secs() < 60);
        }
    }

    #[test]
    fn backoff_doubles_per_failure_caps_and_resets() {
        let minute = Duration::from_secs(60);
        let mut b = PollBackoff::default();
        assert_eq!(b.failed("0.4.0", minute), Duration::from_secs(120));
        assert_eq!(b.failed("0.4.0", minute), Duration::from_secs(240));
        assert_eq!(b.failed("0.4.0", minute), Duration::from_secs(480));
        for _ in 0..40 {
            assert!(b.failed("0.4.0", minute) <= MAX_POLL_BACKOFF);
        }
        assert_eq!(b.failed("0.4.0", minute), MAX_POLL_BACKOFF, "capped");
        // A different release starts over: it may install where the last did not.
        assert_eq!(b.failed("0.4.1", minute), Duration::from_secs(120));
        b.succeeded();
        assert_eq!(b.failed("0.4.1", minute), Duration::from_secs(120));
        // Never shorter than the poll interval, even past the cap.
        let day = Duration::from_secs(24 * 60 * 60);
        assert_eq!(PollBackoff::default().failed("feed", day), day);
    }

    #[test]
    fn deleted_suffix_is_stripped_from_the_exe_path() {
        use std::path::PathBuf;
        assert_eq!(strip_deleted_suffix(PathBuf::from("/usr/local/bin/c0mpute (deleted)")), PathBuf::from("/usr/local/bin/c0mpute"));
        assert_eq!(strip_deleted_suffix(PathBuf::from("/usr/local/bin/c0mpute")), PathBuf::from("/usr/local/bin/c0mpute"));
        assert_eq!(strip_deleted_suffix(PathBuf::from("/opt/x (deleted)/c0mpute")), PathBuf::from("/opt/x (deleted)/c0mpute"));
    }

    #[test]
    fn cargo_build_output_is_a_dev_build() {
        use std::path::Path;
        assert!(is_dev_build_path(Path::new("/home/me/src/c0mpute/target/release/c0mpute")));
        assert!(is_dev_build_path(Path::new("/home/me/src/c0mpute/target/debug/deps/c0mpute-1a2b")));
        assert!(!is_dev_build_path(Path::new("/home/me/.c0mpute/bin/c0mpute")));
        assert!(!is_dev_build_path(Path::new("/usr/local/bin/c0mpute")));
        // Only a directory named exactly `target`, and not the binary's own name.
        assert!(!is_dev_build_path(Path::new("/opt/targets/c0mpute")));
        assert!(!is_dev_build_path(Path::new("/usr/local/bin/target")));
    }

    #[test]
    fn artifact_names_match_the_release_workflow() {
        assert_eq!(artifact_name("linux", "x86_64"), "c0mpute-linux-x86_64.tar.gz");
        assert_eq!(artifact_name("darwin", "aarch64"), "c0mpute-darwin-aarch64.tar.gz");
        assert_eq!(artifact_name("windows", "x86_64"), "c0mpute-windows-x86_64.zip");
        assert_eq!(archived_binary("c0mpute-windows-x86_64.zip"), "c0mpute.exe");
        assert_eq!(archived_binary("c0mpute-linux-x86_64.tar.gz"), "c0mpute");
        // Every name the release workflow publishes is one this function makes.
        let workflow = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../.github/workflows/release.yml")).expect("read release.yml");
        for (os, arch) in [("linux", "x86_64"), ("linux", "aarch64"), ("darwin", "x86_64"), ("darwin", "aarch64"), ("windows", "x86_64")] {
            assert!(workflow.contains(&artifact_name(os, arch)), "{os}-{arch}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn swap_replaces_the_binary_at_the_given_path_and_leaves_no_scratch() {
        let dir = std::env::temp_dir().join(format!("c0-update-swap-{}", scratch_tag()));
        std::fs::create_dir_all(dir.join("pack")).unwrap();
        std::fs::write(dir.join("pack/c0mpute"), b"new binary").unwrap();
        let tar = dir.join("a.tar.gz");
        assert!(std::process::Command::new("tar").arg("-czf").arg(&tar).arg("-C").arg(dir.join("pack")).arg("c0mpute").status().unwrap().success());
        let bin_dir = dir.join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let exe = bin_dir.join("c0mpute");
        std::fs::write(&exe, b"old binary").unwrap();
        // The path is taken as given: never re-derived from current_exe().
        extract_and_swap(&std::fs::read(&tar).unwrap(), "c0mpute-linux-x86_64.tar.gz", &exe).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new binary");
        let left: Vec<_> = std::fs::read_dir(&bin_dir).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left, vec![std::ffi::OsString::from("c0mpute")], "scratch removed");
        // A broken archive fails and still cleans up, leaving the binary alone.
        assert!(extract_and_swap(b"not an archive", "c0mpute-linux-x86_64.tar.gz", &exe).is_err());
        assert_eq!(std::fs::read(&exe).unwrap(), b"new binary");
        assert_eq!(std::fs::read_dir(&bin_dir).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
