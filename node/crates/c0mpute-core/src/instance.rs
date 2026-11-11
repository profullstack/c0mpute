//! One worker per identity per machine. The newest `worker start` wins.
//!
//! Two processes with one identity bid under one peer id with different
//! per-job keys, and the one the buyer did not pick fails the job (bonita,
//! 2026-11: a root-owned system unit with `User=ettinger` beside the per-user
//! unit). 0.3.9 made the second process refuse to start, and systemd
//! restarted it forever. Now:
//!
//! * The identity lock (`worker.lock`, an `flock`) records who holds it: pid,
//!   process start time, binary, systemd unit, user, instance id.
//! * A new `worker start` that finds the lock held asks the holder to hand
//!   over (`worker.takeover`). The holder stops bidding, lets running jobs
//!   finish (up to [`TAKEOVER_DRAIN`]) and exits with [`EXIT_REPLACED`]. A
//!   holder that does not (an old binary, or wedged) gets SIGTERM, then
//!   SIGKILL, but only after its pid, start time and binary are re-checked,
//!   so a reused pid is never signalled.
//! * Every unit c0mpute installs carries `RestartPreventExitStatus=90 91`, so
//!   systemd does not restart the replaced process; a running worker adds a
//!   drop-in to its own unit when an older install lacks it. The new process
//!   disables the evicted unit when it may (its own user's unit; a system unit
//!   only as root) and otherwise logs the exact `sudo` command.
//! * An evicted unit that restarts anyway (no drop-in, no root) finds itself
//!   named in `worker.evicted` and stands down instead of evicting the new
//!   process back: no ping-pong.
//!
//! `c0mpute doctor` reads the same files ([`scan`]).

use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// Exit code of a worker replaced by a newer `worker start` of the same
/// identity. systemd must not restart it.
pub const EXIT_REPLACED: i32 = 90;
/// Exit code of a worker that could not take over (the running one belongs
/// to another user, or its unit was just evicted). systemd must not restart
/// it either.
pub const EXIT_DUPLICATE: i32 = 91;
/// The `RestartPreventExitStatus=` value for every c0mpute worker unit.
pub const RESTART_PREVENT_EXIT_STATUS: &str = "90 91";
/// The drop-in a running worker writes into its own unit when an older
/// install left `RestartPreventExitStatus` out.
pub const DROPIN_FILE: &str = "10-c0mpute-no-restart-loop.conf";

/// How long a replaced worker may take to finish its running jobs.
pub const TAKEOVER_DRAIN: Duration = Duration::from_secs(10 * 60);
/// SIGTERM to SIGKILL.
pub const TERM_GRACE: Duration = Duration::from_secs(30);
/// A holder whose heartbeat is older than this is wedged: it is not waited on.
pub const WEDGED_AFTER: Duration = Duration::from_secs(5 * 60);
const HEARTBEAT_EVERY: Duration = Duration::from_secs(30);

const LOCK_FILE: &str = "worker.lock";
const HEARTBEAT_FILE: &str = "worker.heartbeat";
const TAKEOVER_FILE: &str = "worker.takeover";
const EVICTED_FILE: &str = "worker.evicted";

fn drain_limit() -> Duration {
    std::env::var("C0MPUTE_TAKEOVER_DRAIN_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(TAKEOVER_DRAIN)
}

fn term_grace() -> Duration {
    std::env::var("C0MPUTE_TAKEOVER_TERM_GRACE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(TERM_GRACE)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ─── systemd units ─────────────────────────────────────────────────────────

/// A systemd service unit a process runs in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unit {
    /// `c0mpute-worker.service`.
    pub name: String,
    /// A `systemctl --user` unit (of user `uid`) rather than a system unit.
    pub user: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<u32>,
}

impl Unit {
    pub fn describe(&self) -> String {
        if self.user {
            format!(
                "user unit {} (uid {})",
                self.name,
                self.uid.map(|u| u.to_string()).unwrap_or("?".into())
            )
        } else {
            format!("system unit {}", self.name)
        }
    }

    /// The command that stops and disables this unit, for the operator.
    pub fn disable_command(&self) -> String {
        if self.user {
            format!("systemctl --user disable --now {}", self.name)
        } else {
            format!("sudo systemctl disable --now {}", self.name)
        }
    }

    /// The drop-in directory for this unit (user units of the current user).
    fn dropin_dir(&self) -> Option<PathBuf> {
        if self.user {
            Some(user_unit_dir()?.join(format!("{}.d", self.name)))
        } else {
            Some(PathBuf::from("/etc/systemd/system").join(format!("{}.d", self.name)))
        }
    }

    /// The commands that add the drop-in by hand.
    pub fn dropin_command(&self) -> String {
        let dir = self
            .dropin_dir()
            .map(|d| d.display().to_string())
            .unwrap_or_else(|| format!("~/.config/systemd/user/{}.d", self.name));
        let sudo = if self.user { "" } else { "sudo " };
        let ctl = if self.user {
            "systemctl --user"
        } else {
            "sudo systemctl"
        };
        format!(
            "{sudo}mkdir -p {dir} && printf '[Service]\\nRestartPreventExitStatus={RESTART_PREVENT_EXIT_STATUS}\\n' | {sudo}tee {dir}/{DROPIN_FILE} >/dev/null && {ctl} daemon-reload"
        )
    }

    /// Whether this process may manage the unit itself.
    fn manageable(&self) -> bool {
        if self.user {
            self.uid == Some(current_uid())
        } else {
            is_root()
        }
    }

    fn systemctl(&self) -> std::process::Command {
        let mut c = std::process::Command::new(systemctl_program());
        if self.user {
            c.arg("--user");
            // A system unit running as this user has no user-bus address.
            if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
                let run = format!("/run/user/{}", current_uid());
                c.env("DBUS_SESSION_BUS_ADDRESS", format!("unix:path={run}/bus"));
                c.env("XDG_RUNTIME_DIR", run);
            }
        }
        c.stdin(std::process::Stdio::null());
        c
    }

    /// The live `RestartPreventExitStatus` systemd reports for the unit.
    fn restart_prevent(&self) -> Option<String> {
        let out = self
            .systemctl()
            .args([
                "show",
                "-p",
                "RestartPreventExitStatus",
                "--value",
                &self.name,
            ])
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}

/// Whether a `RestartPreventExitStatus` value covers both our exit codes.
pub fn prevents_our_restarts(value: &str) -> bool {
    let codes: Vec<&str> = value
        .split(|c: char| c.is_whitespace() || c == ',')
        .collect();
    codes.contains(&"90") && codes.contains(&"91")
}

/// `systemctl`, or a stand-in for tests (`C0MPUTE_SYSTEMCTL`, or
/// [`set_systemctl_for_tests`]).
fn systemctl_program() -> String {
    if let Some(p) = SYSTEMCTL_OVERRIDE.lock().ok().and_then(|g| g.clone()) {
        return p;
    }
    std::env::var("C0MPUTE_SYSTEMCTL")
        .ok()
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "systemctl".into())
}

static SYSTEMCTL_OVERRIDE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

#[doc(hidden)]
pub fn set_systemctl_for_tests(program: Option<String>) {
    *SYSTEMCTL_OVERRIDE.lock().unwrap_or_else(|e| e.into_inner()) = program;
}

/// The unit this process is the worker of: its cgroup's service, when that
/// unit is a c0mpute one (by name) or systemd names us its main process. A
/// worker started by hand inside some other service (a CI runner, tmux
/// under a user manager) never touches that service.
fn own_unit(pid: u32) -> Option<Unit> {
    if let Ok(v) = std::env::var("C0MPUTE_INSTANCE_UNIT")
        && !v.is_empty()
    {
        // Testing hook: pretend to run in this user unit.
        return Some(Unit {
            name: v,
            user: true,
            uid: Some(current_uid()),
        });
    }
    let unit = unit_of(pid)?;
    if unit.name.contains("c0mpute") {
        return Some(unit);
    }
    let main = unit
        .systemctl()
        .args(["show", "-p", "MainPID", "--value", &unit.name])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<u32>()
                .ok()
        });
    (main == Some(pid)).then_some(unit)
}

fn user_unit_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("systemd/user"))
}

/// The service unit a `/proc/<pid>/cgroup` text places the process in, if
/// its innermost cgroup is one (a shell, tmux or session scope is not).
pub fn parse_cgroup(text: &str) -> Option<Unit> {
    // cgroup v2: "0::/path"; v1: pick the systemd hierarchy.
    let path = text
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .or_else(|| {
            text.lines()
                .find_map(|l| l.split_once(":name=systemd:").map(|(_, p)| p))
        })?;
    let last = path.trim_end_matches('/').rsplit('/').next()?;
    if !last.ends_with(".service") || last.starts_with("user@") {
        return None;
    }
    let user_uid = path.split('/').find_map(|seg| {
        seg.strip_prefix("user@")
            .and_then(|r| r.strip_suffix(".service"))
            .and_then(|u| u.parse::<u32>().ok())
    });
    Some(Unit {
        name: last.to_string(),
        user: user_uid.is_some(),
        uid: user_uid,
    })
}

/// The service unit `pid` runs in.
pub fn unit_of(pid: u32) -> Option<Unit> {
    parse_cgroup(&std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?)
}

// ─── processes ─────────────────────────────────────────────────────────────

fn current_uid() -> u32 {
    // SAFETY: getuid has no preconditions.
    unsafe { libc::getuid() }
}

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// Start time of `pid` in clock ticks since boot (`/proc/<pid>/stat` field 22):
/// with the pid, it names one process for good.
pub fn proc_start(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name may hold spaces and parens: fields resume after the last ')'.
    let rest = &stat[stat.rfind(')')? + 2..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

fn exe_of(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .map(|p| p.display().to_string())
}

fn cmdline_of(pid: u32) -> Vec<String> {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| {
            b.split(|c| *c == 0)
                .filter(|a| !a.is_empty())
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn uid_of(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// The login name of `uid`, from /etc/passwd.
pub fn user_name(uid: u32) -> String {
    std::fs::read_to_string("/etc/passwd")
        .ok()
        .and_then(|p| {
            p.lines().find_map(|l| {
                let f: Vec<&str> = l.split(':').collect();
                (f.len() > 2 && f[2].parse() == Ok(uid)).then(|| f[0].to_string())
            })
        })
        .unwrap_or_else(|| uid.to_string())
}

fn pid_alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    let r = unsafe { libc::kill(pid as i32, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Whether `args` (a cmdline) is a foreground or daemon `c0mpute worker
/// start` (not `-a`, which only tails a daemon's log).
pub fn is_worker_cmdline(args: &[String]) -> bool {
    let Some(bin) = args.first() else {
        return false;
    };
    let base = Path::new(bin)
        .file_name()
        .map(|b| b.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !base.starts_with("c0mpute") {
        return false;
    }
    let rest: Vec<&str> = args[1..]
        .iter()
        .map(String::as_str)
        .filter(|a| !a.starts_with('-'))
        .collect();
    let starts = rest.windows(2).any(|w| w == ["worker", "start"]);
    starts && !args.iter().any(|a| a == "-a" || a == "--attach")
}

// ─── lock file ─────────────────────────────────────────────────────────────

/// Who holds an identity's worker lock, as written into `worker.lock`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Holder {
    pub pid: u32,
    /// [`proc_start`] of `pid`, so a reused pid is told apart.
    #[serde(default)]
    pub proc_start: Option<u64>,
    pub started_ms: u64,
    pub exe: String,
    #[serde(default)]
    pub unit: Option<Unit>,
    pub uid: u32,
    pub user: String,
    /// This process's own id (not the swarm's); orders twins by start time.
    pub instance: String,
    pub version: String,
}

impl Holder {
    fn me() -> Self {
        let pid = std::process::id();
        let uid = current_uid();
        Self {
            pid,
            proc_start: proc_start(pid),
            started_ms: now_ms(),
            exe: exe_of(pid).unwrap_or_default(),
            unit: own_unit(pid),
            uid,
            user: user_name(uid),
            instance: process_instance().to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    pub fn describe(&self) -> String {
        format!(
            "pid {} (user {}, {}, c0mpute {}, {})",
            self.pid,
            self.user,
            self.unit
                .as_ref()
                .map(Unit::describe)
                .unwrap_or_else(|| "no systemd unit".into()),
            self.version,
            self.exe
        )
    }

    /// The recorded process is still the one running under that pid.
    pub fn is_running(&self) -> bool {
        if !pid_alive(self.pid) {
            return false;
        }
        match (self.proc_start, proc_start(self.pid)) {
            (Some(a), Some(b)) => a == b,
            // Unreadable (another user's process, not as root): trust the
            // pid; nothing is signalled across users anyway.
            _ => true,
        }
    }

    /// Re-checked right before a signal: same pid, same start time, a
    /// c0mpute binary. Never signals a process that only reused the pid.
    fn safe_to_signal(&self) -> bool {
        let Some(now) = proc_start(self.pid) else {
            return false;
        };
        if self.proc_start.is_some_and(|s| s != now) {
            return false;
        }
        let exe = exe_of(self.pid).unwrap_or_default();
        Path::new(exe.trim_end_matches(" (deleted)"))
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("c0mpute"))
            || is_worker_cmdline(&cmdline_of(self.pid))
    }
}

/// What a lock file says about its holder: the record, or (pre-0.3.20
/// workers wrote only a pid) a minimal one.
pub fn read_holder(text: &str) -> Option<Holder> {
    if let Ok(h) = serde_json::from_str::<Holder>(text) {
        return Some(h);
    }
    let pid: u32 = text.trim().parse().ok()?;
    let uid = uid_of(pid).unwrap_or(u32::MAX);
    Some(Holder {
        pid,
        proc_start: None,
        started_ms: 0,
        exe: exe_of(pid).unwrap_or_default(),
        unit: unit_of(pid),
        uid,
        user: user_name(uid),
        instance: String::new(),
        version: "older than 0.3.20".into(),
    })
}

/// A random id for this process, fixed for its life (the lock record's
/// `instance`; the swarm has its own, see `Libp2pNetwork::instance_id`).
pub fn process_instance() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(c0mpute_net::new_instance_id)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Takeover {
    pid: u32,
    proc_start: Option<u64>,
    instance: String,
    unit: Option<Unit>,
    at_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Evicted {
    unit: Unit,
    /// The instance that evicted it (the holder now).
    by: String,
    at_ms: u64,
}

/// Why a worker did not start: the CLI exits with [`EXIT_DUPLICATE`].
#[derive(Debug)]
pub struct DuplicateWorker(pub String);

impl std::fmt::Display for DuplicateWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DuplicateWorker {}

static HELD: std::sync::Mutex<Option<(PathBuf, std::fs::File)>> = std::sync::Mutex::new(None);

fn try_flock(file: &std::fs::File) -> bool {
    use std::os::fd::AsRawFd;
    // SAFETY: flock on a descriptor we own; no memory is shared.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

fn open_lock(path: &Path) -> Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("open {}", path.display()))
}

fn say(msg: &str) {
    // Claims run before tracing is set up (and before a daemon forks): the
    // journal or terminal still gets those on stderr.
    if tracing::dispatcher::has_been_set() {
        warn!("{msg}");
    } else {
        eprintln!("c0mpute worker: {msg}");
    }
}

/// Whether this process already holds `identity_dir`'s lock.
pub fn held(identity_dir: &Path) -> bool {
    HELD.lock()
        .ok()
        .and_then(|h| h.as_ref().map(|(p, _)| p == identity_dir))
        .unwrap_or(false)
}

/// Become `identity_dir`'s one worker: take the lock, replacing a running
/// worker if there is one (see the module docs). Returns once held. Errors
/// with [`DuplicateWorker`] when the running worker cannot be replaced.
#[cfg(unix)]
pub fn claim(identity_dir: &Path) -> Result<()> {
    if held(identity_dir) {
        // A daemon forked after claiming: new pid, and threads do not
        // survive a fork.
        record_self(identity_dir)?;
        if WATCH_PID.load(std::sync::atomic::Ordering::SeqCst) != std::process::id() {
            spawn_watch(identity_dir.to_path_buf());
        }
        return Ok(());
    }
    std::fs::create_dir_all(identity_dir)?;
    let path = identity_dir.join(LOCK_FILE);
    let mut file = open_lock(&path)?;
    let mut replaced: Option<Holder> = None;
    if !try_flock(&file) {
        let mut text = String::new();
        let _ = file.read_to_string(&mut text);
        let holder = read_holder(&text);
        take_over(identity_dir, &file, holder.clone())?;
        replaced = holder;
    }
    *HELD.lock().unwrap_or_else(|e| e.into_inner()) = Some((identity_dir.to_path_buf(), file));
    record_self(identity_dir)?;
    let me = Holder::me();
    let _ = std::fs::remove_file(identity_dir.join(TAKEOVER_FILE));
    // The worker this one replaced must not come back from its unit (a
    // restart of our own unit is just a restart).
    if let Some(unit) = replaced
        .and_then(|p| p.unit)
        .filter(|u| Some(u) != me.unit.as_ref())
    {
        evict_unit(identity_dir, &unit, &me);
    }
    let _ = std::fs::write(identity_dir.join(HEARTBEAT_FILE), now_ms().to_string());
    spawn_watch(identity_dir.to_path_buf());
    if let Some(unit) = &me.unit {
        let unit = unit.clone();
        std::thread::spawn(move || ensure_restart_prevention(&unit));
    }
    // A second unit for this identity would start a second worker at every
    // boot: say so now (it is replaced each time, but should be removed).
    let report = scan();
    for f in report
        .findings
        .iter()
        .filter(|f| f.kind == FindingKind::DuplicateUnits)
    {
        say(&format!(
            "{} Fix: {}",
            f.message,
            f.fix
                .clone()
                .unwrap_or_else(|| "c0mpute doctor --fix".into())
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn claim(_identity_dir: &Path) -> Result<()> {
    Ok(())
}

/// Write this process's record into the held lock file.
fn record_self(identity_dir: &Path) -> Result<()> {
    let mut guard = HELD.lock().unwrap_or_else(|e| e.into_inner());
    let Some((dir, file)) = guard.as_mut() else {
        return Ok(());
    };
    if dir != identity_dir {
        return Ok(());
    }
    let json = serde_json::to_string(&Holder::me())?;
    file.set_len(0)?;
    file.rewind()?;
    file.write_all(json.as_bytes())?;
    file.flush()?;
    Ok(())
}

/// Ask `holder` to hand over and wait until the lock is ours.
fn take_over(identity_dir: &Path, file: &std::fs::File, holder: Option<Holder>) -> Result<()> {
    let me = Holder::me();
    let who = holder
        .as_ref()
        .map(Holder::describe)
        .unwrap_or_else(|| "an unknown process".into());

    // This unit was evicted by the worker that holds the lock now and was
    // restarted anyway (no RestartPreventExitStatus): stand down instead of
    // evicting it back.
    if let (Some(unit), Some(h)) = (&me.unit, &holder)
        && let Some(ev) = read_evicted(identity_dir)
        && &ev.unit == unit
        && ev.by == h.instance
        && h.is_running()
    {
        let msg = format!(
            "this {} was replaced by a newer worker with the same identity ({who}); not starting a second one. \
             Remove this unit: {}",
            unit.describe(),
            unit.disable_command()
        );
        say(&msg);
        let prevented = ensure_restart_prevention(unit);
        if !prevented {
            // systemd would restart us within seconds, forever: wait instead.
            park(&msg);
        }
        return Err(DuplicateWorker(msg).into());
    }

    let Some(h) = holder else {
        return wait_for_lock(file, Duration::from_secs(30)).map_err(|_| {
            DuplicateWorker(format!(
                "another process holds {} and records no owner; stop the other c0mpute worker first (`c0mpute doctor`)",
                identity_dir.join(LOCK_FILE).display()
            ))
            .into()
        });
    };

    // The record names a process that is gone: the lock frees in a moment
    // (it is exiting), or someone unrecorded holds it.
    if !h.is_running() {
        return wait_for_lock(file, Duration::from_secs(30)).map_err(|_| {
            DuplicateWorker(format!(
                "{} is held, but not by the worker it names ({who}); `c0mpute doctor` lists every worker",
                identity_dir.join(LOCK_FILE).display()
            ))
            .into()
        });
    }

    say(&format!(
        "another worker runs this identity: {who}. Replacing it (newest start wins): it stops bidding, finishes \
         running jobs (up to {} min) and exits.",
        drain_limit().as_secs() / 60
    ));
    let request = Takeover {
        pid: me.pid,
        proc_start: me.proc_start,
        instance: me.instance.clone(),
        unit: me.unit.clone(),
        at_ms: now_ms(),
    };
    std::fs::write(
        identity_dir.join(TAKEOVER_FILE),
        serde_json::to_vec(&request)?,
    )
    .context("write the takeover request")?;

    let legacy = h.instance.is_empty();
    let wedged = heartbeat_age(identity_dir).is_some_and(|a| a > WEDGED_AFTER);
    let started = std::time::Instant::now();
    // Old binaries never read the request and wedged ones cannot: no wait.
    let term_at = if legacy || wedged {
        if wedged {
            say(&format!(
                "the running worker (pid {}) has not written a heartbeat for over {} min: treating it as wedged",
                h.pid,
                WEDGED_AFTER.as_secs() / 60
            ));
        }
        Duration::ZERO
    } else {
        drain_limit() + Duration::from_secs(15)
    };
    let mut termed: Option<std::time::Instant> = None;
    let mut killed: Option<std::time::Instant> = None;
    loop {
        if try_flock(file) {
            info!(previous = %who, "took over the identity lock");
            return Ok(());
        }
        let elapsed = started.elapsed();
        if termed.is_none() && elapsed >= term_at {
            signal_holder(&h, libc::SIGTERM)?;
            termed = Some(std::time::Instant::now());
        } else if let Some(t) = termed
            && killed.is_none()
            && t.elapsed() >= term_grace()
        {
            signal_holder(&h, libc::SIGKILL)?;
            killed = Some(std::time::Instant::now());
        } else if let Some(k) = killed
            && k.elapsed() >= Duration::from_secs(30)
        {
            let msg = format!(
                "the identity lock is still held after SIGKILL of pid {}; `c0mpute doctor` lists every worker",
                h.pid
            );
            say(&msg);
            return Err(DuplicateWorker(msg).into());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Signal the recorded holder, after checking it is still that process.
fn signal_holder(h: &Holder, sig: i32) -> Result<()> {
    if !h.is_running() {
        return Ok(()); // gone; the lock frees with it
    }
    if !h.safe_to_signal() {
        let msg = format!(
            "the running worker ({}) does not hand over and cannot be checked or signalled from this user. Stop it: {}",
            h.describe(),
            h.unit
                .as_ref()
                .map(Unit::disable_command)
                .unwrap_or_else(|| format!("sudo kill {}", h.pid))
        );
        say(&msg);
        return Err(DuplicateWorker(msg).into());
    }
    let name = if sig == libc::SIGKILL {
        "SIGKILL"
    } else {
        "SIGTERM"
    };
    say(&format!(
        "sending {name} to the worker it replaces ({})",
        h.describe()
    ));
    // SAFETY: plain kill(2) on a pid just verified to be the recorded worker.
    if unsafe { libc::kill(h.pid as i32, sig) } != 0 {
        let msg = format!(
            "cannot signal pid {} ({}). Stop it: {}",
            h.pid,
            std::io::Error::last_os_error(),
            h.unit
                .as_ref()
                .map(Unit::disable_command)
                .unwrap_or_else(|| format!("sudo kill {}", h.pid))
        );
        say(&msg);
        return Err(DuplicateWorker(msg).into());
    }
    Ok(())
}

fn wait_for_lock(file: &std::fs::File, max: Duration) -> Result<()> {
    let started = std::time::Instant::now();
    while started.elapsed() < max {
        if try_flock(file) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    anyhow::bail!("lock still held")
}

/// Sleep forever, saying why every hour: systemd sees a running unit and
/// does not restart it every few seconds.
fn park(msg: &str) -> ! {
    loop {
        say(&format!("standing by, not working: {msg}"));
        std::thread::sleep(Duration::from_secs(3600));
    }
}

fn read_evicted(identity_dir: &Path) -> Option<Evicted> {
    serde_json::from_slice(&std::fs::read(identity_dir.join(EVICTED_FILE)).ok()?).ok()
}

/// The previous holder's unit must not start it again: record it, and
/// disable it when this process may.
fn evict_unit(identity_dir: &Path, unit: &Unit, me: &Holder) {
    let rec = Evicted {
        unit: unit.clone(),
        by: me.instance.clone(),
        at_ms: now_ms(),
    };
    if let Ok(json) = serde_json::to_vec(&rec) {
        let _ = std::fs::write(identity_dir.join(EVICTED_FILE), json);
    }
    // A run by hand (no unit) replaces the service for now but does not
    // remove it; only a service replacing a service does.
    if me.unit.is_none() {
        say(&format!(
            "replaced the worker of {}; it stays installed and starts again at boot. To keep only one: {}",
            unit.describe(),
            unit.disable_command()
        ));
        return;
    }
    if unit.manageable() {
        let ok = unit
            .systemctl()
            .args(["disable", &unit.name])
            .status()
            .is_ok_and(|s| s.success());
        if ok {
            say(&format!(
                "disabled the replaced {} so only one worker runs this identity",
                unit.describe()
            ));
            return;
        }
    }
    say(&format!(
        "the replaced {} is still enabled and will start a second worker at boot (it then stands down). Remove it: {}",
        unit.describe(),
        unit.disable_command()
    ));
}

/// Make sure systemd will not restart `unit` after [`EXIT_REPLACED`] or
/// [`EXIT_DUPLICATE`]: add the drop-in when the unit lacks it and this
/// process may write it, else log the command. Returns whether it is set.
pub fn ensure_restart_prevention(unit: &Unit) -> bool {
    ensure_restart_prevention_in(unit, unit.dropin_dir())
}

/// [`ensure_restart_prevention`] with the drop-in directory given (doctor
/// knows where each unit file lives).
pub fn ensure_restart_prevention_in(unit: &Unit, dropin_dir: Option<PathBuf>) -> bool {
    if unit
        .restart_prevent()
        .is_some_and(|v| prevents_our_restarts(&v))
    {
        return true;
    }
    if unit.manageable()
        && let Some(dir) = dropin_dir
        && std::fs::create_dir_all(&dir).is_ok()
        && std::fs::write(
            dir.join(DROPIN_FILE),
            format!("# Written by c0mpute: a replaced or duplicate worker must not restart-loop.\n[Service]\nRestartPreventExitStatus={RESTART_PREVENT_EXIT_STATUS}\n"),
        )
        .is_ok()
    {
        let _ = unit.systemctl().arg("daemon-reload").status();
        info!(unit = %unit.name, "added RestartPreventExitStatus={RESTART_PREVENT_EXIT_STATUS} to the worker's unit");
        return true;
    }
    say(&format!(
        "{} lacks RestartPreventExitStatus={RESTART_PREVENT_EXIT_STATUS}, so a replaced worker would restart in a loop. Add it: {}",
        unit.describe(),
        unit.dropin_command()
    ));
    false
}

fn heartbeat_age(identity_dir: &Path) -> Option<Duration> {
    let ms: u64 = std::fs::read_to_string(identity_dir.join(HEARTBEAT_FILE))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(Duration::from_millis(now_ms().saturating_sub(ms)))
}

static RUNTIME_HEARTBEAT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Keep the heartbeat from the async runtime once it runs, so a wedged
/// runtime shows as a stale heartbeat (`c0mpute doctor`, and a newer start
/// does not wait on it).
pub fn spawn_runtime_heartbeat(identity_dir: PathBuf) {
    RUNTIME_HEARTBEAT.store(true, std::sync::atomic::Ordering::SeqCst);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(HEARTBEAT_EVERY);
        loop {
            tick.tick().await;
            let _ = tokio::fs::write(identity_dir.join(HEARTBEAT_FILE), now_ms().to_string()).await;
        }
    });
}

/// Watch for a takeover request (and keep the heartbeat until the runtime
/// does). Runs on its own thread so it works even when the runtime is stuck.
static WATCH_PID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn spawn_watch(identity_dir: PathBuf) {
    WATCH_PID.store(std::process::id(), std::sync::atomic::Ordering::SeqCst);
    let _ = std::thread::Builder::new()
        .name("c0mpute-instance".into())
        .spawn(move || {
            let mut last_beat = std::time::Instant::now();
            loop {
                std::thread::sleep(Duration::from_secs(1));
                if !RUNTIME_HEARTBEAT.load(std::sync::atomic::Ordering::SeqCst)
                    && last_beat.elapsed() >= HEARTBEAT_EVERY
                {
                    let _ = std::fs::write(identity_dir.join(HEARTBEAT_FILE), now_ms().to_string());
                    last_beat = std::time::Instant::now();
                }
                let Some(req) = std::fs::read(identity_dir.join(TAKEOVER_FILE))
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Takeover>(&b).ok())
                else {
                    continue;
                };
                if req.pid == std::process::id() || req.instance == process_instance() {
                    continue;
                }
                let requester_live = pid_alive(req.pid)
                    && match (req.proc_start, proc_start(req.pid)) {
                        (Some(a), Some(b)) => a == b,
                        _ => true,
                    };
                if !requester_live {
                    let _ = std::fs::remove_file(identity_dir.join(TAKEOVER_FILE));
                    continue;
                }
                hand_over(&req);
            }
        });
}

/// Stop bidding, let running jobs finish (capped), exit [`EXIT_REPLACED`].
fn hand_over(req: &Takeover) -> ! {
    let by = format!(
        "pid {}{}",
        req.pid,
        req.unit
            .as_ref()
            .map(|u| format!(", {}", u.describe()))
            .unwrap_or_default()
    );
    say(&format!(
        "replaced by a newer `c0mpute worker start` with this identity ({by}): no new work; finishing running jobs \
         (up to {} min), then exiting with code {EXIT_REPLACED}",
        drain_limit().as_secs() / 60
    ));
    c0mpute_update::begin_drain();
    let started = std::time::Instant::now();
    while c0mpute_update::long_jobs_running() + crate::activity::running_jobs() > 0
        && started.elapsed() < drain_limit()
    {
        std::thread::sleep(Duration::from_secs(2));
    }
    say(&format!("handing over to {by}; exiting"));
    std::process::exit(EXIT_REPLACED);
}

// ─── doctor ────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize)]
pub struct WorkerProcess {
    pub pid: u32,
    pub uid: u32,
    pub user: String,
    pub unit: Option<Unit>,
    pub exe: Option<String>,
    /// The binary was replaced on disk since the process started.
    pub exe_deleted: bool,
    pub args: Vec<String>,
    /// The container it runs in (short id), if any: each container has its
    /// own filesystem and so its own identity.
    pub container: Option<String>,
}

/// The container a `/proc/<pid>/cgroup` text places a process in (docker,
/// podman, containerd), as a 12-character id.
pub fn container_of(cgroup: &str) -> Option<String> {
    for seg in cgroup.lines().flat_map(|l| l.rsplit(':').next().unwrap_or("").split('/')) {
        let id = seg
            .strip_suffix(".scope")
            .and_then(|s| s.rsplit_once('-').map(|(_, id)| id))
            .filter(|_| seg.starts_with("docker-") || seg.starts_with("libpod-") || seg.starts_with("cri-containerd-"))
            .or_else(|| (seg.len() == 64 && seg.bytes().all(|b| b.is_ascii_hexdigit())).then_some(seg));
        if let Some(id) = id {
            return Some(id.chars().take(12).collect());
        }
    }
    None
}

#[derive(Clone, Debug, Serialize)]
pub struct UnitFile {
    pub unit: Unit,
    pub path: PathBuf,
    /// Who the worker runs as (`User=`, the owner for user units).
    pub runs_as: String,
    pub exec: Option<String>,
    pub enabled: bool,
    pub restart_prevented: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct IdentityLock {
    pub dir: PathBuf,
    pub owner: String,
    /// Some process holds the flock right now.
    pub held: bool,
    pub holder: Option<Holder>,
    pub heartbeat_age_s: Option<u64>,
    pub takeover_pending: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum FindingKind {
    DuplicateProcesses,
    DuplicateUnits,
    StaleLock,
    Wedged,
    OldBinary,
    NoRestartPrevention,
}

#[derive(Clone, Debug, Serialize)]
pub struct Finding {
    pub kind: FindingKind,
    pub message: String,
    /// What `--fix` runs or what to run by hand.
    pub fix: Option<String>,
    /// A unit `--fix` disables, when it may.
    #[serde(skip)]
    pub disable: Option<Unit>,
    /// A unit `--fix` adds the restart-loop drop-in to, and where.
    #[serde(skip)]
    pub add_dropin: Option<(Unit, PathBuf)>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Report {
    pub processes: Vec<WorkerProcess>,
    pub units: Vec<UnitFile>,
    pub locks: Vec<IdentityLock>,
    pub findings: Vec<Finding>,
}

/// Every c0mpute worker process, unit and identity lock on this machine,
/// and what is wrong with them.
pub fn scan() -> Report {
    scan_with(Path::new("/"), &homes(Path::new("/")))
}

fn homes(root: &Path) -> Vec<(PathBuf, u32)> {
    let mut out = Vec::new();
    let me = current_uid();
    if let Some(h) = std::env::var_os("HOME") {
        out.push((PathBuf::from(h), me));
    }
    if is_root() {
        for base in [root.join("home"), root.join("root")] {
            let dirs: Vec<PathBuf> = if base.ends_with("root") {
                vec![base.clone()]
            } else {
                std::fs::read_dir(&base)
                    .map(|r| r.flatten().map(|e| e.path()).collect())
                    .unwrap_or_default()
            };
            for d in dirs {
                use std::os::unix::fs::MetadataExt;
                if let Ok(m) = std::fs::metadata(&d)
                    && m.is_dir()
                    && !out.iter().any(|(p, _)| p == &d)
                {
                    out.push((d, m.uid()));
                }
            }
        }
    }
    out
}

/// [`scan`] with system unit files under `root` and these `(home, uid)`
/// pairs (tests use temp dirs; processes always come from /proc).
pub fn scan_with(root: &Path, homes: &[(PathBuf, u32)]) -> Report {
    let mut r = Report::default();
    // Processes.
    if let Ok(rd) = std::fs::read_dir("/proc") {
        for e in rd.flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            let args = cmdline_of(pid);
            if !is_worker_cmdline(&args) {
                continue;
            }
            let uid = uid_of(pid).unwrap_or(u32::MAX);
            let exe = exe_of(pid);
            r.processes.push(WorkerProcess {
                pid,
                uid,
                user: user_name(uid),
                unit: unit_of(pid),
                exe_deleted: exe.as_deref().is_some_and(|e| e.ends_with(" (deleted)")),
                exe,
                args,
                container: std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
                    .ok()
                    .and_then(|c| container_of(&c)),
            });
        }
    }
    r.processes.sort_by_key(|p| p.pid);

    // Units.
    for dir in [
        "etc/systemd/system",
        "lib/systemd/system",
        "usr/lib/systemd/system",
    ] {
        let dir = root.join(dir);
        for (path, text) in unit_files(&dir) {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let runs_as = unit_value(&text, "User").unwrap_or_else(|| "root".into());
            let enabled = wants_link_exists(&dir, &name);
            r.units.push(UnitFile {
                restart_prevented: restart_prevented_in(&path, &text, &dir),
                unit: Unit {
                    name,
                    user: false,
                    uid: None,
                },
                exec: exec_binary(&text),
                path,
                runs_as,
                enabled,
            });
        }
    }
    for (home, uid) in homes.iter().cloned() {
        let dir = home.join(".config/systemd/user");
        for (path, text) in unit_files(&dir) {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            r.units.push(UnitFile {
                restart_prevented: restart_prevented_in(&path, &text, &dir),
                enabled: wants_link_exists(&dir, &name),
                unit: Unit {
                    name,
                    user: true,
                    uid: Some(uid),
                },
                exec: exec_binary(&text),
                path,
                runs_as: user_name(uid),
            });
        }
    }

    // Identity locks.
    for (home, uid) in homes.iter().cloned() {
        let dir = if uid == current_uid()
            && root == Path::new("/")
            && std::env::var_os("HOME").is_some_and(|h| PathBuf::from(h) == home)
        {
            crate::config::config_dir().unwrap_or_else(|| home.join(".config/c0mpute"))
        } else {
            home.join(".config/c0mpute")
        };
        let path = dir.join(LOCK_FILE);
        if !path.exists() || r.locks.iter().any(|l| l.dir == dir) {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let held_here = held(&dir);
        let held = held_here
            || std::fs::OpenOptions::new()
                .read(true)
                .open(&path)
                .map(|f| {
                    let free = try_flock(&f);
                    // Dropping `f` releases the probe lock.
                    !free
                })
                .unwrap_or(false);
        r.locks.push(IdentityLock {
            owner: user_name(uid),
            held,
            holder: read_holder(&text),
            heartbeat_age_s: heartbeat_age(&dir).map(|d| d.as_secs()),
            takeover_pending: dir.join(TAKEOVER_FILE).exists(),
            dir,
        });
    }

    r.findings = findings(&r);
    r
}

fn unit_files(dir: &Path) -> Vec<(PathBuf, String)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut v: Vec<(PathBuf, String)> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "service") && p.is_file())
        .filter_map(|p| {
            let text = std::fs::read_to_string(&p).ok()?;
            let exec = text
                .lines()
                .find(|l| l.trim_start().starts_with("ExecStart="))?;
            (exec.contains("c0mpute") && exec.contains("worker") && exec.contains("start"))
                .then_some((p, text))
        })
        .collect();
    v.sort();
    v
}

fn unit_value(text: &str, key: &str) -> Option<String> {
    text.lines()
        .filter_map(|l| l.trim().strip_prefix(&format!("{key}=")))
        .map(|v| v.trim().to_string())
        .rfind(|v| !v.is_empty())
}

fn exec_binary(text: &str) -> Option<String> {
    let v = unit_value(text, "ExecStart")?;
    Some(
        v.trim_start_matches(['-', '@', '+', '!'])
            .split_whitespace()
            .next()?
            .to_string(),
    )
}

fn wants_link_exists(dir: &Path, name: &str) -> bool {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten().any(|e| {
                e.file_name().to_string_lossy().ends_with(".wants") && e.path().join(name).exists()
            })
        })
        .unwrap_or(false)
}

fn restart_prevented_in(path: &Path, text: &str, dir: &Path) -> bool {
    let mut value = unit_value(text, "RestartPreventExitStatus");
    let dropins = dir.join(format!("{}.d", path.file_name().unwrap().to_string_lossy()));
    if let Ok(rd) = std::fs::read_dir(&dropins) {
        let mut files: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        files.sort();
        for f in files {
            if let Ok(t) = std::fs::read_to_string(&f)
                && let Some(v) = unit_value(&t, "RestartPreventExitStatus")
            {
                value = Some(v);
            }
        }
    }
    value.is_some_and(|v| prevents_our_restarts(&v))
}

fn findings(r: &Report) -> Vec<Finding> {
    let mut out = Vec::new();
    // Two worker processes for one user (in one container, or both on the
    // host): one identity running twice.
    let mut groups: Vec<(u32, Option<String>)> =
        r.processes.iter().map(|p| (p.uid, p.container.clone())).collect();
    groups.sort();
    groups.dedup();
    for (uid, container) in groups {
        let ps: Vec<&WorkerProcess> = r
            .processes
            .iter()
            .filter(|p| p.uid == uid && p.container == container)
            .collect();
        if ps.len() > 1 {
            let list: Vec<String> = ps
                .iter()
                .map(|p| {
                    format!(
                        "pid {} ({})",
                        p.pid,
                        p.unit
                            .as_ref()
                            .map(Unit::describe)
                            .unwrap_or_else(|| "no unit".into())
                    )
                })
                .collect();
            out.push(Finding {
                kind: FindingKind::DuplicateProcesses,
                message: format!(
                    "{} c0mpute workers run as {}: {}",
                    ps.len(),
                    ps[0].user,
                    list.join(", ")
                ),
                fix: Some(
                    "the newest replaces the others within minutes; remove the extra unit below"
                        .into(),
                ),
                disable: None,
                add_dropin: None,
            });
        }
    }
    // Two enabled units for one user.
    let mut users: Vec<String> = r
        .units
        .iter()
        .filter(|u| u.enabled)
        .map(|u| u.runs_as.clone())
        .collect();
    users.sort();
    users.dedup();
    for user in users {
        let units: Vec<&UnitFile> = r
            .units
            .iter()
            .filter(|u| u.enabled && u.runs_as == user)
            .collect();
        if units.len() < 2 {
            continue;
        }
        // Keep the unit whose worker holds the lock; else the user unit
        // (what the installer makes by default); else the first.
        let holder_unit = r
            .locks
            .iter()
            .filter(|l| l.owner == user && l.held)
            .find_map(|l| l.holder.as_ref()?.unit.clone());
        let keep = units
            .iter()
            .position(|u| {
                holder_unit
                    .as_ref()
                    .is_some_and(|h| h.name == u.unit.name && h.user == u.unit.user)
            })
            .or_else(|| units.iter().position(|u| u.unit.user))
            .unwrap_or(0);
        for (i, u) in units.iter().enumerate() {
            if i == keep {
                continue;
            }
            out.push(Finding {
                kind: FindingKind::DuplicateUnits,
                message: format!(
                    "{} and {} both start a c0mpute worker as {user} (one identity, two workers)",
                    units[keep].unit.describe(),
                    u.unit.describe()
                ),
                fix: Some(u.unit.disable_command()),
                disable: Some(u.unit.clone()),
                add_dropin: None,
            });
        }
    }
    for u in &r.units {
        if let Some(exec) = &u.exec
            && !Path::new(exec).exists()
            && !exec.contains('%')
            && !exec.contains('$')
        {
            out.push(Finding {
                kind: FindingKind::OldBinary,
                message: format!("{} starts {exec}, which does not exist", u.unit.describe()),
                fix: Some(format!(
                    "point ExecStart at the current binary, or remove the unit: {}",
                    u.unit.disable_command()
                )),
                disable: None,
                add_dropin: None,
            });
        }
        if !u.restart_prevented {
            out.push(Finding {
                kind: FindingKind::NoRestartPrevention,
                message: format!(
                    "{} lacks RestartPreventExitStatus={RESTART_PREVENT_EXIT_STATUS}: a replaced worker would restart in a loop",
                    u.unit.describe()
                ),
                fix: Some(u.unit.dropin_command()),
                disable: None,
                add_dropin: u
                    .path
                    .parent()
                    .map(|d| (u.unit.clone(), d.join(format!("{}.d", u.unit.name)))),
            });
        }
    }
    for p in &r.processes {
        if p.exe_deleted {
            out.push(Finding {
                kind: FindingKind::OldBinary,
                message: format!("worker pid {} runs a binary that has since been replaced (it re-executes on its next update check)", p.pid),
                fix: p.unit.as_ref().map(|u| {
                    if u.user {
                        format!("systemctl --user restart {}", u.name)
                    } else {
                        format!("sudo systemctl restart {}", u.name)
                    }
                }),
                disable: None,
                add_dropin: None,
            });
        }
    }
    for l in &r.locks {
        match (&l.holder, l.held) {
            (Some(h), false) => out.push(Finding {
                kind: FindingKind::StaleLock,
                message: format!(
                    "{} names pid {} but nothing holds it (that worker exited; harmless, the next start takes it)",
                    l.dir.join(LOCK_FILE).display(),
                    h.pid
                ),
                fix: None,
                disable: None,
                add_dropin: None,
            }),
            (Some(h), true) if l.heartbeat_age_s.is_some_and(|a| a > WEDGED_AFTER.as_secs()) && !h.instance.is_empty() => {
                out.push(Finding {
                    kind: FindingKind::Wedged,
                    message: format!(
                        "worker {} holds the identity but its heartbeat is {} min old (wedged?)",
                        h.describe(),
                        l.heartbeat_age_s.unwrap_or(0) / 60
                    ),
                    fix: Some(
                        h.unit
                            .as_ref()
                            .map(|u| if u.user { format!("systemctl --user restart {}", u.name) } else { format!("sudo systemctl restart {}", u.name) })
                            .unwrap_or_else(|| "c0mpute worker start (replaces it without waiting)".into()),
                    ),
                    disable: None,
                    add_dropin: None,
                })
            }
            _ => {}
        }
    }
    out
}

/// What `doctor --fix` did, line by line.
pub fn fix(report: &Report) -> Vec<String> {
    let mut done = Vec::new();
    for f in &report.findings {
        if let Some(u) = &f.disable {
            if u.manageable() {
                let ok = u
                    .systemctl()
                    .args(["disable", "--now", &u.name])
                    .status()
                    .is_ok_and(|s| s.success());
                done.push(if ok {
                    format!("disabled {}", u.describe())
                } else {
                    format!(
                        "could not disable {}; run: {}",
                        u.describe(),
                        u.disable_command()
                    )
                });
            } else {
                done.push(format!(
                    "not allowed to disable {} from this user; run: {}",
                    u.describe(),
                    u.disable_command()
                ));
            }
        }
        if let Some((u, dir)) = &f.add_dropin {
            // Disabled duplicates need no drop-in.
            if report
                .findings
                .iter()
                .any(|g| g.disable.as_ref() == Some(u))
            {
                continue;
            }
            if u.manageable() {
                if ensure_restart_prevention_in(u, Some(dir.clone())) {
                    done.push(format!(
                        "added RestartPreventExitStatus={RESTART_PREVENT_EXIT_STATUS} to {}",
                        u.describe()
                    ));
                }
            } else {
                done.push(format!(
                    "not allowed to change {} from this user; run: {}",
                    u.describe(),
                    u.dropin_command()
                ));
            }
        }
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgroup_paths_name_the_unit() {
        let sys = "0::/system.slice/c0mpute-worker.service\n";
        assert_eq!(
            parse_cgroup(sys),
            Some(Unit {
                name: "c0mpute-worker.service".into(),
                user: false,
                uid: None
            })
        );
        let user =
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/c0mpute-worker.service\n";
        assert_eq!(
            parse_cgroup(user),
            Some(Unit {
                name: "c0mpute-worker.service".into(),
                user: true,
                uid: Some(1000)
            })
        );
        // A shell or tmux scope is not a unit we manage.
        assert_eq!(
            parse_cgroup("0::/user.slice/user-1000.slice/session-4.scope\n"),
            None
        );
        assert_eq!(
            parse_cgroup(
                "0::/user.slice/user-1000.slice/user@1000.service/app.slice/tmux-spawn-1.scope\n"
            ),
            None
        );
        assert_eq!(
            parse_cgroup("0::/user.slice/user-1000.slice/user@1000.service\n"),
            None
        );
        // cgroup v1.
        let v1 = "12:pids:/x\n1:name=systemd:/system.slice/c0mpute-worker.service\n";
        assert_eq!(parse_cgroup(v1).map(|u| u.user), Some(false));
    }

    #[test]
    fn container_cgroups() {
        let id = "3cafaca0397090a4cead5d0b2572298fffc577a9bd5c906192ffaa920b1ca21c";
        assert_eq!(
            container_of(&format!("0::/system.slice/docker-{id}.scope\n")).as_deref(),
            Some("3cafaca03970")
        );
        assert_eq!(container_of(&format!("12:pids:/docker/{id}\n")).as_deref(), Some("3cafaca03970"));
        assert_eq!(container_of("0::/system.slice/c0mpute-worker.service\n"), None);
        assert_eq!(container_of("0::/user.slice/user-1000.slice/session-4.scope\n"), None);
    }

    #[test]
    fn worker_cmdlines() {
        let v = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        assert!(is_worker_cmdline(&v(
            "/home/a/.c0mpute/bin/c0mpute worker start"
        )));
        assert!(is_worker_cmdline(&v("c0mpute worker start -d")));
        assert!(is_worker_cmdline(&v(
            "c0mpute --config /x worker start --gpu"
        )));
        assert!(!is_worker_cmdline(&v("c0mpute worker start -a")));
        assert!(!is_worker_cmdline(&v("c0mpute worker status")));
        assert!(!is_worker_cmdline(&v("/usr/bin/vim worker start")));
    }

    #[test]
    fn restart_prevention_values() {
        assert!(prevents_our_restarts("90 91"));
        assert!(prevents_our_restarts("1 90 91 SIGTERM"));
        assert!(!prevents_our_restarts("90"));
        assert!(!prevents_our_restarts(""));
    }

    #[test]
    fn legacy_lock_files_hold_only_a_pid() {
        let h = read_holder(&std::process::id().to_string()).unwrap();
        assert_eq!(h.pid, std::process::id());
        assert!(h.instance.is_empty(), "legacy holders have no instance");
        let me = Holder::me();
        let back = read_holder(&serde_json::to_string(&me).unwrap()).unwrap();
        assert_eq!(back, me);
        assert!(me.is_running());
        assert!(me.proc_start.is_some());
    }

    #[test]
    fn a_reused_pid_is_not_the_recorded_worker() {
        let mut me = Holder::me();
        me.proc_start = me.proc_start.map(|s| s + 1);
        assert!(!me.is_running());
        assert!(!me.safe_to_signal());
    }
    // ── processes: the test binary re-runs itself as a "worker" ──────────

    /// Not a test on its own: with `C0MPUTE_TEST_CHILD_DIR` set (by the tests
    /// below, in a child process) it claims that identity dir and idles like
    /// a worker until replaced.
    #[test]
    fn child_worker() {
        let Ok(dir) = std::env::var("C0MPUTE_TEST_CHILD_DIR") else {
            return;
        };
        match claim(Path::new(&dir)) {
            Ok(()) => loop {
                std::thread::sleep(Duration::from_secs(1));
            },
            Err(e) if e.downcast_ref::<DuplicateWorker>().is_some() => {
                std::process::exit(EXIT_DUPLICATE)
            }
            Err(e) => panic!("{e:#}"),
        }
    }

    fn spawn_child(dir: &Path, unit: &str, xdg: &Path) -> std::process::Child {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "instance::tests::child_worker",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("C0MPUTE_TEST_CHILD_DIR", dir)
            .env("C0MPUTE_INSTANCE_UNIT", unit)
            .env("C0MPUTE_SYSTEMCTL", "true")
            .env("XDG_CONFIG_HOME", xdg)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap()
    }

    fn holder_pid(dir: &Path) -> Option<u32> {
        let text = std::fs::read_to_string(dir.join(LOCK_FILE)).ok()?;
        serde_json::from_str::<Holder>(&text).ok().map(|h| h.pid)
    }

    fn wait_holder(dir: &Path, pid: u32) {
        for _ in 0..200 {
            if holder_pid(dir) == Some(pid) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!(
            "pid {pid} never took the lock (holder: {:?})",
            holder_pid(dir)
        );
    }

    fn wait_exit(child: &mut std::process::Child, max: Duration) -> Option<i32> {
        let start = std::time::Instant::now();
        while start.elapsed() < max {
            if let Some(st) = child.try_wait().unwrap() {
                return st.code();
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = child.kill();
        panic!("pid {} did not exit within {max:?}", child.id());
    }

    #[test]
    fn a_second_start_replaces_the_running_worker_gracefully() {
        let tmp = tempfile::tempdir().unwrap();
        let id = tmp.path().join("id");
        let mut a = spawn_child(&id, "", tmp.path());
        wait_holder(&id, a.id());
        let mut b = spawn_child(&id, "", tmp.path());
        // The first one drains (nothing running) and exits "replaced", a code
        // every unit lists in RestartPreventExitStatus: no restart loop.
        assert_eq!(
            wait_exit(&mut a, Duration::from_secs(30)),
            Some(EXIT_REPLACED)
        );
        wait_holder(&id, b.id());
        assert!(
            !id.join(TAKEOVER_FILE).exists(),
            "the request is cleared once served"
        );
        let _ = b.kill();
        let _ = b.wait();
    }

    #[test]
    fn an_evicted_unit_that_restarts_stands_down_instead_of_ping_ponging() {
        let tmp = tempfile::tempdir().unwrap();
        let id = tmp.path().join("id");
        let mut a = spawn_child(&id, "c0mpute-a.service", tmp.path());
        wait_holder(&id, a.id());
        let mut b = spawn_child(&id, "c0mpute-b.service", tmp.path());
        assert_eq!(
            wait_exit(&mut a, Duration::from_secs(30)),
            Some(EXIT_REPLACED)
        );
        wait_holder(&id, b.id());
        let ev = read_evicted(&id).expect("the replaced unit is recorded");
        assert_eq!(ev.unit.name, "c0mpute-a.service");

        // systemd restarts unit a anyway (an old unit without the guard): it
        // must not evict b back.
        let mut a2 = spawn_child(&id, "c0mpute-a.service", tmp.path());
        assert_eq!(
            wait_exit(&mut a2, Duration::from_secs(20)),
            Some(EXIT_DUPLICATE)
        );
        assert_eq!(holder_pid(&id), Some(b.id()), "b keeps working");
        // ...and it added the guard to its own unit on the way out, so this
        // was the last restart.
        let dropin = tmp
            .path()
            .join("systemd/user/c0mpute-a.service.d")
            .join(DROPIN_FILE);
        let text = std::fs::read_to_string(&dropin).expect("drop-in written");
        assert!(text.contains("RestartPreventExitStatus=90 91"), "{text}");

        // A restart of b's own unit is a plain restart, not an eviction.
        let _ = b.kill();
        let _ = b.wait();
        let mut b2 = spawn_child(&id, "c0mpute-b.service", tmp.path());
        wait_holder(&id, b2.id());
        let _ = b2.kill();
        let _ = b2.wait();
    }

    // ── doctor ──────────────────────────────────────────────────────────

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn enable(dir: &Path, target: &str, name: &str) {
        let wants = dir.join(format!("{target}.wants"));
        std::fs::create_dir_all(&wants).unwrap();
        std::os::unix::fs::symlink(dir.join(name), wants.join(name)).unwrap();
    }

    #[test]
    fn doctor_finds_a_system_and_a_user_unit_for_one_user_and_fixes_the_user_one() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home/ettinger");
        let uid = current_uid();
        let me = user_name(uid);
        let bin = root.path().join("c0mpute");
        write(&bin, "");
        // The bonita layout: a system unit running as the user, and the
        // user's own unit, both enabled.
        let sys_dir = root.path().join("etc/systemd/system");
        write(
            &sys_dir.join("c0mpute-worker.service"),
            &format!(
                "[Service]\nUser={me}\nExecStart={} worker start\nRestart=always\n",
                bin.display()
            ),
        );
        enable(&sys_dir, "multi-user.target", "c0mpute-worker.service");
        let user_dir = home.join(".config/systemd/user");
        write(
            &user_dir.join("c0mpute-worker.service"),
            "[Service]\nExecStart=/gone/.c0mpute/bin/c0mpute worker start\nRestart=on-failure\n",
        );
        enable(&user_dir, "default.target", "c0mpute-worker.service");
        // The system unit's worker holds the identity: that one is kept.
        let id = home.join(".config/c0mpute");
        let mut holder = Holder::me();
        holder.unit = Some(Unit {
            name: "c0mpute-worker.service".into(),
            user: false,
            uid: None,
        });
        write(
            &id.join(LOCK_FILE),
            &serde_json::to_string(&holder).unwrap(),
        );
        let lock = std::fs::File::open(id.join(LOCK_FILE)).unwrap();
        assert!(
            try_flock(&lock),
            "the test holds the lock like a live worker"
        );

        let r = scan_with(root.path(), &[(home.clone(), uid)]);
        let kinds: Vec<FindingKind> = r.findings.iter().map(|f| f.kind).collect();
        let dup = r
            .findings
            .iter()
            .find(|f| f.kind == FindingKind::DuplicateUnits)
            .expect("duplicate units");
        let disable = dup.disable.as_ref().unwrap();
        assert!(
            disable.user,
            "keeps the unit whose worker holds the lock: {}",
            dup.message
        );
        assert!(
            kinds.contains(&FindingKind::OldBinary),
            "user unit starts a missing binary: {kinds:?}"
        );
        assert_eq!(
            kinds
                .iter()
                .filter(|k| **k == FindingKind::NoRestartPrevention)
                .count(),
            2,
            "neither unit has the guard: {kinds:?}"
        );
        assert!(
            r.locks
                .iter()
                .any(|l| l.held && l.holder.as_ref().is_some_and(|h| h.pid == holder.pid))
        );

        // --fix: the user unit is ours to disable; the system unit is not
        // (unless root), so the doctor prints the sudo command for it.
        let log = root.path().join("systemctl.log");
        let fake = root.path().join("systemctl");
        write(
            &fake,
            &format!("#!/bin/sh\necho \"$@\" >> {}\n", log.display()),
        );
        std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        set_systemctl_for_tests(Some(fake.display().to_string()));
        let done = fix(&r);
        set_systemctl_for_tests(None);
        let calls = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            calls.contains("--user disable --now c0mpute-worker.service"),
            "{calls}\n{done:?}"
        );
        if !is_root() {
            assert!(
                done.iter()
                    .any(|l| l.contains("sudo") && l.contains("RestartPreventExitStatus")),
                "system unit guard is printed as a sudo command: {done:?}"
            );
        }
        drop(lock);

        // Once the lock is released the record is only a stale note.
        let r = scan_with(root.path(), &[(home, uid)]);
        assert!(r.findings.iter().any(|f| f.kind == FindingKind::StaleLock));
    }

    #[test]
    fn doctor_flags_a_live_holder_with_an_old_heartbeat_as_wedged() {
        let h = Holder::me();
        let r = Report {
            locks: vec![IdentityLock {
                dir: PathBuf::from("/x"),
                owner: "u".into(),
                held: true,
                holder: Some(h),
                heartbeat_age_s: Some(WEDGED_AFTER.as_secs() + 60),
                takeover_pending: false,
            }],
            ..Default::default()
        };
        let f = findings(&r);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].kind, FindingKind::Wedged);
        // A fresh heartbeat is healthy, and a healthy worker is never signalled
        // by doctor (it only reports).
        let mut r = r;
        r.locks[0].heartbeat_age_s = Some(5);
        assert!(findings(&r).is_empty());
    }
}

/// Lock files of buyer identities this process holds (kept open for life:
/// closing one releases its lock).
#[cfg(unix)]
static BUYER_LOCKS: std::sync::Mutex<Vec<std::fs::File>> = std::sync::Mutex::new(Vec::new());

const BUYER_LOCK_FILE: &str = "in-use.lock";

/// The libp2p identity directory for a buyer process (`hosting publish`,
/// `chat`, `disks --live`, ...): `base` when no other live process is using
/// it, else a fresh per-process directory beside it.
///
/// Two processes on one peer id break each other: remote peers keep one
/// connection per peer id, so the second process's gossip fails with
/// `NoPeersSubscribedToTopic` and its transfers stall (a terminal `hosting
/// publish` beside the systemd publisher did exactly that). A site is
/// signed with the buyer key loaded on its own ([`crate::hosting::publisher_keypair`]),
/// so a separate network identity changes nothing about which site it is.
#[cfg(unix)]
pub fn buyer_identity_dir(base: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(base)?;
    let file = open_lock(&base.join(BUYER_LOCK_FILE))?;
    if try_flock(&file) {
        BUYER_LOCKS.lock().unwrap_or_else(|e| e.into_inner()).push(file);
        return Ok(base.to_path_buf());
    }
    let sessions = base.with_file_name(format!(
        "{}-sessions",
        base.file_name().and_then(|n| n.to_str()).unwrap_or("buyer")
    ));
    prune_dead_sessions(&sessions);
    let dir = sessions.join(std::process::id().to_string());
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    say(&format!(
        "another c0mpute process is using the identity in {}; this one joins the network as its own peer ({}) so neither stalls",
        base.display(),
        dir.display()
    ));
    Ok(dir)
}

/// Remove per-process buyer directories whose process is gone.
#[cfg(unix)]
fn prune_dead_sessions(sessions: &Path) {
    let Ok(rd) = std::fs::read_dir(sessions) else {
        return;
    };
    for e in rd.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        // SAFETY: kill with signal 0 only checks that the pid exists.
        let alive = unsafe { libc::kill(pid, 0) == 0 }
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        if !alive {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

#[cfg(all(test, unix))]
mod buyer_lock_tests {
    use super::*;

    #[test]
    fn a_second_buyer_gets_its_own_identity_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("buyer");
        // Another process holds the lock (flock is per open file description,
        // so a second open in this process stands in for it).
        std::fs::create_dir_all(&base).unwrap();
        let other = open_lock(&base.join(BUYER_LOCK_FILE)).unwrap();
        assert!(try_flock(&other));
        let dir = buyer_identity_dir(&base).unwrap();
        assert_ne!(dir, base);
        assert!(dir.starts_with(tmp.path().join("buyer-sessions")));
        drop(other);
        // Freed again: the next buyer gets the shared identity. (A process
        // another test forks inherits our descriptors until it execs, so the
        // lock can stay held for a moment after the drop.)
        let mut got = PathBuf::new();
        for _ in 0..50 {
            got = buyer_identity_dir(&base).unwrap();
            if got == base {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(got, base);
    }
}
