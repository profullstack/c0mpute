//! Run one heavy job child (sd-cli, the ACE-Step worker) with a hard
//! deadline, and never leave any of its processes behind.
//!
//! `tokio::time::timeout(child.wait_with_output())` with `kill_on_drop`
//! looked safe but was not: on a timeout tokio sends SIGKILL to the direct
//! child only and does not wait for it, so the render stayed `<defunct>`
//! until some later SIGCHLD, and anything the child had started itself (a
//! Python worker's helpers, a shell wrapper's sd-cli) kept running with every
//! core. vienna collected stale sd-cli processes that way and got benched for
//! the timeouts they caused.
//!
//! Here the child leads its own process group (unix), and the whole group is
//! killed on a timeout and whenever the future is dropped (a cancelled job,
//! a runner shutting down). On a timeout the leader is also reaped before
//! returning. On Windows `taskkill /T` takes the tree.

use std::process::Output;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

/// Spawn `command`, collect its piped stdout/stderr, and wait at most
/// `timeout`. `what` names the program in errors ("sd-cli").
pub async fn run_with_timeout(command: &mut Command, timeout: Duration, what: &str) -> Result<Output> {
    run_watched(command, timeout, what, LIVENESS_POLL, LIVENESS_GRACE).await
}

/// How often a waiting job checks that its child still exists.
const LIVENESS_POLL: Duration = Duration::from_secs(5);
/// How long the child may be gone while the wait has not returned.
///
/// dev2 2026-11-11: a diffusion worker logged "applying per-job compute
/// limits" (sd-cli about to start), then nothing for 12+ minutes, with no
/// sd-cli process left in the container. The render had ended, but the wait
/// never did (a pipe still held open, or an exit status nobody delivered),
/// so the node sat on a won frame until its 15-minute deadline and a promo
/// stalled at 2/7 frames. Once the leader has been gone this long, the job
/// fails now and its buyer can re-award it.
const LIVENESS_GRACE: Duration = Duration::from_secs(30);

async fn run_watched(
    command: &mut Command,
    timeout: Duration,
    what: &str,
    poll: Duration,
    grace: Duration,
) -> Result<Output> {
    command.kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().with_context(|| format!("start {what}"))?;
    let pid = child.id();
    tracing::debug!(pid = ?pid, "{what} started");
    // Declared after `child`, so on a drop it runs first, while the leader's
    // pid (and with it the group id) is still held by its zombie at worst.
    let mut guard = TreeGuard { pid };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let outcome = {
        let work = async { tokio::try_join!(child.wait(), drain(stdout), drain(stderr)) };
        tokio::pin!(work);
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        let mut ticker = tokio::time::interval(poll);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut gone_since: Option<std::time::Instant> = None;
        loop {
            tokio::select! {
                done = &mut work => break Waited::Done(done),
                _ = &mut deadline => break Waited::TimedOut,
                _ = ticker.tick() => {
                    if pid.is_some_and(leader_gone) {
                        let since = *gone_since.get_or_insert_with(std::time::Instant::now);
                        if since.elapsed() >= grace {
                            break Waited::Lost;
                        }
                    } else {
                        gone_since = None;
                    }
                }
            }
        }
    };
    match outcome {
        Waited::Done(Ok((status, stdout, stderr))) => {
            // The leader is reaped: its pid may be reused, so never signal it.
            guard.pid = None;
            Ok(Output { status, stdout, stderr })
        }
        Waited::Done(Err(err)) => {
            guard.kill();
            let _ = child.kill().await;
            Err(err).with_context(|| format!("wait for {what}"))
        }
        Waited::TimedOut => {
            guard.kill();
            // SIGKILL the leader too (already dead from the group kill on
            // unix) and wait for it, so no zombie is left.
            let _ = child.kill().await;
            bail!("{what} timed out after {} s", timeout.as_secs())
        }
        Waited::Lost => {
            // Whatever still holds its pipes dies with the group.
            guard.kill();
            let _ = child.kill().await;
            tracing::warn!(pid = ?pid, "{what} is gone but its wait never finished; failing the job");
            bail!("{what} exited but its wait never finished (gone for {} s)", grace.as_secs())
        }
    }
}

enum Waited<T> {
    Done(std::io::Result<T>),
    TimedOut,
    Lost,
}

/// The leader no longer runs: no such process, or a zombie nobody reaped.
/// A reused pid reads as alive, which only means the deadline decides.
#[cfg(target_os = "linux")]
fn leader_gone(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => crate::diffusion::parse_stat(&stat).is_none_or(|(state, _)| state == 'Z'),
        Err(_) => true,
    }
}

#[cfg(not(target_os = "linux"))]
fn leader_gone(_pid: u32) -> bool {
    false
}

/// How a job child ended, for error messages: "exit 1", or on unix the
/// signal that killed it. `ExitStatus::code()` alone prints "None" for a
/// signal, which hid that dev2's earlyoom was SIGTERMing every sd-cli render
/// under host memory pressure (2026-11-10).
pub fn describe_exit(status: &std::process::ExitStatus) -> String {
    if let Some(code) = status.code() {
        return format!("exit {code}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            let name = match sig {
                libc::SIGKILL => "SIGKILL",
                libc::SIGTERM => "SIGTERM",
                libc::SIGINT => "SIGINT",
                libc::SIGSEGV => "SIGSEGV",
                libc::SIGABRT => "SIGABRT",
                libc::SIGBUS => "SIGBUS",
                libc::SIGILL => "SIGILL",
                _ => "signal",
            };
            let hint = if sig == libc::SIGKILL || sig == libc::SIGTERM {
                "; likely the host's OOM guard (earlyoom, systemd-oomd, kernel) or a shutdown, see `journalctl -u earlyoom`"
            } else {
                ""
            };
            return format!("killed by {name} ({sig}){hint}");
        }
    }
    "no exit status".to_string()
}

async fn drain<R: AsyncRead + Unpin>(pipe: Option<R>) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    if let Some(mut pipe) = pipe {
        pipe.read_to_end(&mut buf).await?;
    }
    Ok(buf)
}

/// Kills the child's whole process tree when dropped, unless disarmed.
struct TreeGuard {
    pid: Option<u32>,
}

impl TreeGuard {
    fn kill(&mut self) {
        if let Some(pid) = self.pid.take() {
            kill_tree(pid);
        }
    }
}

impl Drop for TreeGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(unix)]
fn kill_tree(pid: u32) {
    let Ok(pgid) = libc::pid_t::try_from(pid) else {
        return;
    };
    // pgid 0 or 1 would mean our own group or init's: never.
    if pgid <= 1 {
        return;
    }
    // SAFETY: killpg only sends a signal; the group was created for this child.
    unsafe {
        libc::killpg(pgid, libc::SIGKILL);
    }
}

#[cfg(windows)]
fn kill_tree(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(not(any(unix, windows)))]
fn kill_tree(_pid: u32) {}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::Instant;

    /// A stand-in sd-cli: records its own pid and a grandchild's, then hangs.
    fn fake_job(dir: &Path) -> Command {
        let script = dir.join("fake-sd-cli.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\nsleep 300 &\necho \"$$ $!\" > \"$1/pids\"\necho started\nsleep 300\n",
        )
        .unwrap();
        let mut command = Command::new("sh");
        command
            .arg(&script)
            .arg(dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        command
    }

    fn pids(dir: &Path) -> (u32, u32) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(text) = std::fs::read_to_string(dir.join("pids")) {
                let mut it = text.split_whitespace().map(|p| p.parse::<u32>().unwrap());
                if let (Some(a), Some(b)) = (it.next(), it.next()) {
                    return (a, b);
                }
            }
            assert!(Instant::now() < deadline, "fake job never started");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Live means present and not a zombie.
    fn alive(pid: u32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => crate::diffusion::parse_stat(&stat).is_some_and(|(state, _)| state != 'Z'),
            Err(_) => false,
        }
    }

    fn gone(pid: u32) -> bool {
        std::fs::metadata(format!("/proc/{pid}")).is_err()
    }

    async fn wait_until(mut done: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if done() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        done()
    }

    #[test]
    fn describe_exit_names_the_signal() {
        use std::os::unix::process::ExitStatusExt;
        let term = std::process::ExitStatus::from_raw(libc::SIGTERM);
        let text = describe_exit(&term);
        assert!(text.starts_with("killed by SIGTERM (15)"), "{text}");
        assert!(text.contains("earlyoom"), "{text}");
        let code = std::process::ExitStatus::from_raw(3 << 8);
        assert_eq!(describe_exit(&code), "exit 3");
    }

    #[tokio::test]
    async fn a_timed_out_job_leaves_no_process_behind() {
        let dir = tempfile::tempdir().unwrap();
        let mut command = fake_job(dir.path());
        let err = run_with_timeout(&mut command, Duration::from_secs(1), "sd-cli")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err:#}");
        let (leader, grandchild) = pids(dir.path());
        // The leader was reaped before returning: not even a zombie.
        assert!(gone(leader), "sd-cli leader {leader} still in the process table");
        // The grandchild was killed with the group (init reaps it).
        assert!(
            wait_until(|| !alive(grandchild)).await,
            "grandchild {grandchild} survived the timeout"
        );
    }

    #[tokio::test]
    async fn a_cancelled_job_kills_its_whole_tree() {
        let dir = tempfile::tempdir().unwrap();
        let mut command = fake_job(dir.path());
        // The outer timeout drops the job future, as a cancelled job does.
        let cancelled = tokio::time::timeout(
            Duration::from_millis(500),
            run_with_timeout(&mut command, Duration::from_secs(600), "sd-cli"),
        )
        .await;
        assert!(cancelled.is_err());
        let (leader, grandchild) = pids(dir.path());
        assert!(wait_until(|| !alive(leader)).await, "leader {leader} survived the cancel");
        assert!(
            wait_until(|| !alive(grandchild)).await,
            "grandchild {grandchild} survived the cancel"
        );
    }

    #[tokio::test]
    async fn a_finished_job_returns_its_output() {
        let mut command = Command::new("sh");
        command
            .args(["-c", "echo out; echo err >&2; exit 3"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let output = run_with_timeout(&mut command, Duration::from_secs(10), "sh").await.unwrap();
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(output.stdout, b"out\n");
        assert_eq!(output.stderr, b"err\n");
    }

    /// dev2 2026-11-11: the render was gone, the wait was not. A leader that
    /// exits while something it started still holds its stdout used to sit
    /// out the whole deadline; now it fails once the grace has passed, and
    /// the holder dies with the group.
    #[tokio::test]
    async fn a_gone_leader_fails_the_job_after_the_grace_not_the_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(format!("sleep 300 & echo $! > {}/holder; exit 0", dir.path().display()))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let started = Instant::now();
        let err = run_watched(&mut command, Duration::from_secs(600), "sd-cli", Duration::from_millis(50), Duration::from_millis(300))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("wait never finished"), "{err:#}");
        assert!(started.elapsed() < Duration::from_secs(10), "took {:?}", started.elapsed());
        let holder: u32 = std::fs::read_to_string(dir.path().join("holder")).unwrap().trim().parse().unwrap();
        assert!(wait_until(|| !alive(holder)).await, "the pipe holder survived");
    }

    #[tokio::test]
    async fn a_running_leader_is_not_mistaken_for_a_gone_one() {
        let mut command = Command::new("sh");
        command
            .args(["-c", "sleep 1; echo done"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let output = run_watched(&mut command, Duration::from_secs(10), "sh", Duration::from_millis(50), Duration::from_millis(300))
            .await
            .unwrap();
        assert_eq!(output.stdout, b"done\n");
    }
}
