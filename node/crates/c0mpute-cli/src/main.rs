//! `c0mpute` — the c0mpute.com umbrella CLI.
//!
//! Top-level surface:
//!
//!   c0mpute doctor
//!   c0mpute worker register|start|stop|status
//!   c0mpute job submit|status|logs|cancel
//!   c0mpute modules list|install|enable|disable
//!   c0mpute version
//!
//! Plugin (module) subcommands. Each one delegates / dispatches into the
//! relevant module — built-in modules run in-process, peer-CLI modules
//! shell out to their binary on PATH (per DIP-0006):
//!
//!   c0mpute transcode <sub>     # in-process FFmpeg workload
//!   c0mpute coinpay   <args…>   # delegates to `coinpay`
//!   c0mpute infernet  <args…>   # delegates to `infernet`
//!
//! The plugin form mirrors the URL namespace: c0mpute.com/transcode,
//! c0mpute.com/coinpay, c0mpute.com/infernet.

use std::path::PathBuf;
use std::process::Command;

use anyhow::Result;
use clap::{Parser, Subcommand};
use c0mpute_core::{
    Config, JobAuction, Supervisor, TranscodeJobInline, config, init_tracing, run_auction,
    run_register,
};
use c0mpute_proto::Role;
use c0mpute_secure_chat as chat;

#[derive(Parser, Debug)]
#[command(
    name = "c0mpute",
    version,
    about = "c0mpute.com — decentralized compute network",
    long_about = "c0mpute.com CLI. Submit jobs, run a worker, manage modules.\n\nBuilt-in plugins:\n  transcode  (FFmpeg, in-process)\n  coinpay    (DID + payments, peer CLI)\n  infernet   (AI inference, peer CLI)\n\n  c0mpute coinpay reputation did claim\n  c0mpute transcode submit input.mov --preset hls\n  c0mpute infernet run prompts.jsonl --model qwen"
)]
struct Cli {
    /// Override the config file location.
    #[arg(long, env = "C0MPUTE_CONFIG", global = true)]
    config: Option<PathBuf>,

    /// Print c0mpute + every installed plugin/peer-CLI version, then exit.
    #[arg(short = 'v', long = "versions", global = true)]
    versions: bool,

    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run full-stack diagnostic checks.
    Doctor,
    /// Worker lifecycle.
    Worker {
        #[command(subcommand)]
        cmd: WorkerCmd,
    },
    /// Job lifecycle.
    Job {
        #[command(subcommand)]
        cmd: JobCmd,
    },
    /// Plugin management (list / install / enable / disable / uninstall).
    #[command(alias = "plugins")]
    Plugin {
        #[command(subcommand)]
        cmd: PluginCmd,
    },

    /// Transcode plugin (built-in FFmpeg workload).
    Transcode {
        #[command(subcommand)]
        cmd: TranscodeCmd,
    },
    /// Coinpay plugin — delegates to the `coinpay` peer CLI.
    Coinpay {
        /// Arguments forwarded to `coinpay`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Infernet plugin — delegates to the `infernet` peer CLI.
    Infernet {
        /// Arguments forwarded to `infernet`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Sign in to every c0mpute network (coinpay + infernet), one at a time.
    Login,

    /// Secure chat — E2E encrypted p2p messaging (DIP-0018).
    Chat {
        #[command(subcommand)]
        cmd: ChatCmd,
    },

    /// Launch the interactive TUI (worker / job / module dashboard).
    ///
    /// Subprocess-launches `c0mpute-tui` (a react-blessed terminal UI built
    /// on Bun). See apps/tui in the repo. Long-term we move to Perry once
    /// their CLI surface ships.
    Tui {
        /// Arguments forwarded to the TUI binary.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Check for and install a newer c0mpute release.
    #[command(alias = "upgrade")]
    Update {
        /// Only check; don't apply the upgrade even if available.
        #[arg(long)]
        check: bool,
        /// Override the release-feed URL (defaults to the c0mpute.com one).
        #[arg(long)]
        feed: Option<String>,
    },

    /// Uninstall the c0mpute binary (and optionally peer binaries).
    #[command(alias = "remove")]
    Uninstall {
        /// Also remove `coinpay` and `infernet` from `~/.c0mpute/bin`.
        #[arg(long)]
        all: bool,
        /// Also remove the c0mpute config dir (`~/.config/c0mpute`).
        #[arg(long)]
        purge: bool,
        /// Skip the y/N confirmation.
        #[arg(long)]
        yes: bool,
    },

    /// Run the read-only status aggregator (DIP-0014).
    ///
    /// Boots an observer libp2p node that crawls the Kad-DHT and listens on
    /// the public gossipsub topics, then serves aggregate network health as
    /// JSON over HTTP. No roles are advertised — it never poses as a worker,
    /// and no private data is ever exposed. This is the service behind
    /// c0mpute.com/status; anyone can run their own and get the same numbers.
    #[command(name = "status-aggregator", alias = "aggregator")]
    StatusAggregator {
        /// Address to serve the status JSON on (`GET /`, plus `/healthz`).
        #[arg(long, env = "C0MPUTE_STATUS_BIND", default_value = "0.0.0.0:8080")]
        bind: std::net::SocketAddr,
    },

    /// Print the c0mpute binary version.
    Version,
}

#[derive(Subcommand, Debug)]
enum WorkerCmd {
    /// Register this machine as a worker (requires a CoinPay worker DID).
    Register,
    /// Start the worker daemon and accept jobs.
    Start {
        #[arg(long, value_delimiter = ',')]
        roles: Option<Vec<String>>,
        #[arg(long)]
        storage: Option<String>,
        #[arg(long)]
        gpu: bool,
        /// Start the worker as a background daemon, then stay attached and
        /// stream its log. Press Ctrl-D or Ctrl-C to detach — the worker keeps
        /// running (use `worker stop` to stop it). Attaches to an already
        /// running worker if there is one.
        #[arg(short = 'a', long, conflicts_with = "daemon")]
        attach: bool,
        /// Detach and run in the background as a daemon (writes a PID file and
        /// redirects output to a log). Use `worker stop` / `worker status`.
        #[arg(short = 'd', long)]
        daemon: bool,
    },
    /// Stop a running worker.
    Stop,
    /// Show worker status.
    Status,
}

#[derive(Subcommand, Debug)]
enum JobCmd {
    /// Submit a job manifest JSON.
    Submit { manifest: PathBuf },
    /// Show status for a job ID.
    Status { id: String },
    /// Tail logs for a job ID.
    Logs {
        id: String,
        #[arg(long)]
        follow: bool,
    },
    /// Cancel a queued/running job.
    Cancel { id: String },
}

#[derive(Subcommand, Debug)]
enum PluginCmd {
    /// List installed plugins.
    List,
    /// Install a plugin by id (from the c0mpute marketplace) or by URL.
    ///
    /// Examples:
    ///   c0mpute plugin install transcode
    ///   c0mpute plugin install https://example.com/my-plugin/install.sh
    Install { target: String },
    /// Enable a previously disabled plugin.
    Enable { id: String },
    /// Disable a plugin without uninstalling.
    Disable { id: String },
    /// Uninstall a plugin (alias: `delete`, `remove`).
    #[command(alias = "delete", alias = "remove")]
    Uninstall { id: String },
}

#[derive(Subcommand, Debug)]
enum TranscodeCmd {
    /// Submit an FFmpeg transcode job.
    Submit {
        input: PathBuf,
        #[arg(long, default_value = "video-1080p")]
        preset: String,
        #[arg(long)]
        max_price: Option<f64>,
    },
    /// Manage transcode presets.
    Preset {
        #[command(subcommand)]
        cmd: PresetCmd,
    },
    /// Run local diagnostics for the transcode module.
    Doctor,
}

#[derive(Subcommand, Debug)]
enum PresetCmd {
    /// List available presets.
    List,
}

#[derive(Subcommand, Debug)]
enum ChatCmd {
    /// Generate a new keypair and write the encrypted keyfile.
    Keygen,
    /// Key management subcommands.
    Key {
        #[command(subcommand)]
        cmd: KeyCmd,
    },
    /// Restore a keypair from a backup JSON file.
    Restore {
        #[arg(long = "from-backup")]
        from_backup: PathBuf,
    },
    /// Send an encrypted DM to a recipient DID. [v0.2: transport not yet live]
    Send {
        to: String,
        message: String,
        /// Hide sender identity from relay nodes.
        #[arg(long)]
        sealed: bool,
    },
    /// Fetch queued messages from the relay. [v0.2: relay not yet live]
    Pull,
    /// Resolve a DID to its chat public key. [v0.2: DHT not yet live]
    Lookup { did: String },
    /// List saved contacts.
    Contacts,
    /// Block a DID (messages dropped locally).
    Block { did: String },
}

#[derive(Subcommand, Debug)]
enum KeyCmd {
    /// Print the public key, fingerprint, and DID.
    Show,
    /// Re-export the encrypted backup JSON (pipe to a file for safe storage).
    Export,
    /// Generate a new keypair and re-announce (old key still valid until revoked).
    Rotate,
}

/// Opportunistic self-update run at the start of any command. Throttled to once
/// per 5 minutes (a marker under the data dir); when a newer release exists it
/// downloads + verifies + swaps the binary and re-executes the same command on
/// it. This keeps `c0mpute` current even when no worker is running — the
/// worker's poll loop only updates while the worker is up. Opt out with
/// `C0MPUTE_NO_AUTO_UPDATE=1`.
#[cfg(unix)]
fn maybe_self_update(cli: &Cli) {
    // `update` handles this itself; don't double up. Opt-out for CI/scripts.
    if matches!(cli.command, Some(Cmd::Update { .. }))
        || std::env::var_os("C0MPUTE_NO_AUTO_UPDATE").is_some()
    {
        return;
    }
    let marker = config::data_dir().map(|d| d.join(".update-check"));
    if let Some(m) = &marker {
        if let Ok(Ok(elapsed)) = std::fs::metadata(m)
            .and_then(|md| md.modified())
            .map(|t| t.elapsed())
        {
            if elapsed < std::time::Duration::from_secs(300) {
                return;
            }
        }
        if let Some(parent) = m.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(m, b"");
    }

    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    let current = env!("CARGO_PKG_VERSION");
    if let Ok(c0mpute_update::UpgradeOutcome::Upgraded { .. }) =
        rt.block_on(c0mpute_update::upgrade_now(current, c0mpute_update::DEFAULT_RELEASE_FEED))
    {
        use std::os::unix::process::CommandExt;
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("c0mpute"));
        let args: Vec<String> = std::env::args().skip(1).collect();
        // exec only returns on failure; if it fails, fall through and run the
        // requested command on the current (pre-swap) image.
        let _ = std::process::Command::new(exe).args(args).exec();
    }
}

#[cfg(not(unix))]
fn maybe_self_update(_cli: &Cli) {}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Opportunistic self-update on any command (throttled), so c0mpute stays
    // current even when no worker is running. May re-exec into the new binary.
    maybe_self_update(&cli);

    // `-v` / `--versions`: full version report, no async runtime needed.
    if cli.versions {
        print_all_versions();
        return Ok(());
    }

    // Attach mode: launch the worker as a detached daemon (or find the one
    // already running) and follow its log until Ctrl-D / Ctrl-C. This is a
    // plain supervisor + file tail — it never builds the async runtime, so it
    // can hand off to the daemon and return your shell without killing it.
    if let Some(Cmd::Worker {
        cmd:
            WorkerCmd::Start {
                attach: true,
                roles,
                storage,
                gpu,
                daemon: _,
            },
    }) = &cli.command
    {
        let mut passthrough = Vec::new();
        if let Some(rs) = roles {
            passthrough.push("--roles".to_string());
            passthrough.push(rs.join(","));
        }
        if let Some(s) = storage {
            passthrough.push("--storage".to_string());
            passthrough.push(s.clone());
        }
        if *gpu {
            passthrough.push("--gpu".to_string());
        }
        if let Some(cfg) = &cli.config {
            passthrough.push("--config".to_string());
            passthrough.push(cfg.display().to_string());
        }
        return run_attached_worker(passthrough);
    }

    // Daemonize BEFORE the async runtime starts. Forking is unsafe once the
    // multi-threaded Tokio runtime has spawned worker threads, so this must
    // happen while the process is still single-threaded. Only `worker start
    // --daemon` detaches; every other command runs in the foreground.
    if let Some(Cmd::Worker {
        cmd: WorkerCmd::Start { daemon: true, .. },
    }) = &cli.command
    {
        daemonize_worker()?;
    }

    init_tracing()?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run_app(cli))
}

async fn run_app(cli: Cli) -> Result<()> {
    let config_path = cli.config.unwrap_or_else(config::default_config_path);

    // No subcommand: open the interactive menu in a terminal, else print help
    // (keeps piped/non-TTY and CI usage unchanged).
    let Some(command) = cli.command else {
        use std::io::IsTerminal;
        if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            return run_menu();
        }
        use clap::CommandFactory;
        Cli::command().print_help()?;
        println!();
        return Ok(());
    };

    match command {
        Cmd::Version => {
            print_all_versions();
            Ok(())
        }
        Cmd::Doctor => run_doctor().await,
        Cmd::StatusAggregator { bind } => c0mpute_core::status_aggregator::run(bind).await,
        Cmd::Worker { cmd } => run_worker(cmd, &config_path).await,
        Cmd::Job { cmd } => run_job(cmd).await,
        Cmd::Plugin { cmd } => run_plugin(cmd),

        Cmd::Transcode { cmd } => run_transcode(cmd).await,
        Cmd::Chat { cmd } => run_chat(cmd),
        Cmd::Tui { args } => run_tui(&args),
        Cmd::Update { check, feed } => run_update(check, feed).await,
        Cmd::Uninstall { all, purge, yes } => run_uninstall(all, purge, yes),
        Cmd::Coinpay { args } => delegate("coinpay", &args),
        Cmd::Login => run_login(),
        Cmd::Infernet { mut args } => {
            // Default the network to c0mpute when caller didn't specify.
            if matches!(args.first().map(String::as_str), Some("run"))
                && !args.iter().any(|a| a == "--network")
            {
                args.push("--network".into());
                args.push("c0mpute".into());
            }
            delegate("infernet", &args)
        }
    }
}

// ────────────────────────────────────────────────────────────────────────
// doctor
// ────────────────────────────────────────────────────────────────────────

async fn run_doctor() -> Result<()> {
    let local = c0mpute_doctor::run().await;
    for r in &local {
        println!("{:5} {} — {:?}", status_label(&r.status), r.name, r.status);
    }

    println!("{:5} c0mpute — Ok (this binary)", "OK");
    println!("{:5} coinpay — {}", peer_label("coinpay"), peer_status_text("coinpay"));
    println!("{:5} infernet — {}", peer_label("infernet"), peer_status_text("infernet"));

    Ok(())
}

fn status_label(s: &c0mpute_doctor::Status) -> &'static str {
    match s {
        c0mpute_doctor::Status::Ok => "OK",
        c0mpute_doctor::Status::Warn(_) => "WARN",
        c0mpute_doctor::Status::Fail(_) => "FAIL",
    }
}

fn peer_label(bin: &str) -> &'static str {
    if which_on_path(bin).is_some() { "OK" } else { "WARN" }
}

fn peer_status_text(bin: &str) -> String {
    match which_on_path(bin) {
        Some(p) => format!("Ok ({})", p.display()),
        None => format!(
            "not on PATH — install via `curl -fsSL https://c0mpute.com/install.sh | sh`"
        ),
    }
}

// ────────────────────────────────────────────────────────────────────────
// worker
// ────────────────────────────────────────────────────────────────────────

async fn run_worker(cmd: WorkerCmd, config_path: &std::path::Path) -> Result<()> {
    match cmd {
        WorkerCmd::Register => {
            let r = run_register(Some(config_path))?;
            println!("registered as worker:");
            println!("  peer-id   : {}", r.peer_id);
            println!(
                "  identity  : {}{}",
                r.identity_path.display(),
                if r.created_identity { "  (new)" } else { "" }
            );
            println!(
                "  config    : {}{}",
                r.config_path.display(),
                if r.created_config { "  (new)" } else { "" }
            );
            println!("  storage   : {}", r.storage_root.display());
            println!();
            // Auto-mint the payable DID so the operator never has to run a
            // separate coinpay command. Idempotent (coinpay returns the
            // existing DID) and best-effort — a worker runs fine without it,
            // so a missing/logged-out coinpay must not fail registration.
            ensure_coinpay_did();
            println!();
            println!("next:");
            println!("  c0mpute login          # sign in to coinpay + infernet (ties this node to your accounts)");
            println!("  c0mpute worker start   # join the swarm");
            Ok(())
        }
        WorkerCmd::Status => {
            println!("version: c0mpute {}", env!("CARGO_PKG_VERSION"));
            match read_worker_pid() {
                Some(pid) if pid_alive(pid) => println!("worker: running (pid {pid})"),
                Some(pid) => println!("worker: not running (stale pid {pid})"),
                None => println!("worker: not running"),
            }
            let cfg = Config::load_or_default(config_path)?;
            if cfg.update_auto {
                let feed = cfg
                    .update_feed_url
                    .clone()
                    .unwrap_or_else(|| c0mpute_update::DEFAULT_RELEASE_FEED.to_string());
                println!(
                    "auto-update: on — checks {} every {}s, applies in place",
                    feed, cfg.update_interval_secs
                );
            } else {
                println!("auto-update: off");
            }
            println!("{}", serde_json::to_string_pretty(&cfg)?);
            Ok(())
        }
        WorkerCmd::Stop => stop_worker(),
        WorkerCmd::Start {
            roles,
            storage: _,
            gpu,
            // Both handled in `main` before the runtime started; at this point
            // we are the (possibly detached) worker process itself.
            attach: _,
            daemon: _,
        } => {
            let mut cfg = Config::load_or_default(config_path)?;
            if let Some(rs) = roles {
                cfg.roles = rs.iter().filter_map(|s| parse_role(s)).collect();
            }
            if gpu && !cfg.roles.contains(&Role::Transcode) {
                cfg.roles.push(Role::Transcode);
            }
            // First run: auto-configure the infernet peer (init + register, plus
            // token login if INFERNET_TOKEN is set) so the node shows up on the
            // infernet control plane. Then make sure its node daemon is up so it
            // picks up jobs (model pulls, inference). Both best-effort; c0mpute
            // drives the plugins so the operator doesn't run them by hand.
            let auto = cfg.update_auto;
            let interval_secs = cfg.update_interval_secs.max(60);
            bootstrap_infernet_first_run();
            ensure_infernet_daemon();
            // Opt-in: serve configured models over infernet RPC (builds llama.cpp
            // in the background on first run, then serves once ready).
            let _ = tokio::task::spawn_blocking(bootstrap_infernet_rpc);
            // Check every surface for upgrades on the poll cadence (default 5m):
            // c0mpute self-updates in the supervisor's poll loop; here we refresh
            // the plugins and (re)attempt RPC serving on the same interval. First
            // tick fires immediately.
            if auto {
                let interval = std::time::Duration::from_secs(interval_secs);
                tokio::spawn(async move {
                    let mut ticker = tokio::time::interval(interval);
                    loop {
                        ticker.tick().await;
                        let _ = tokio::task::spawn_blocking(refresh_plugins).await;
                        let _ = tokio::task::spawn_blocking(bootstrap_infernet_rpc).await;
                    }
                });
            }
            let sup = Supervisor::boot(cfg).await?;
            sup.run().await
        }
    }
}

// ────────────────────────────────────────────────────────────────────────
// worker daemon: detach, PID file, stop/status
// ────────────────────────────────────────────────────────────────────────

/// `(pid_file, log_file)` for the background worker, under the data dir
/// (`~/.local/share/c0mpute` on Linux), falling back to the cwd.
fn worker_runtime_paths() -> (PathBuf, PathBuf) {
    let dir = config::data_dir().unwrap_or_else(|| PathBuf::from("."));
    (dir.join("worker.pid"), dir.join("worker.log"))
}

fn read_worker_pid() -> Option<i32> {
    let (pid_file, _) = worker_runtime_paths();
    std::fs::read_to_string(&pid_file)
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
}

/// Fork into the background, redirect stdout/stderr to the worker log, and
/// write a locked PID file. Runs before the Tokio runtime starts, so the
/// process is still single-threaded when it forks.
#[cfg(unix)]
fn daemonize_worker() -> Result<()> {
    use std::fs::OpenOptions;

    let (pid_file, log_file) = worker_runtime_paths();
    if let Some(parent) = pid_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let stdout = OpenOptions::new().create(true).append(true).open(&log_file)?;
    let stderr = stdout.try_clone()?;

    // Print to the launching terminal *before* forking — once detached,
    // stdout points at the log file.
    println!("c0mpute worker starting in background");
    println!("  pid file: {}", pid_file.display());
    println!("  logs:     {}", log_file.display());
    println!("  stop:     c0mpute worker stop");

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    daemonize::Daemonize::new()
        .pid_file(&pid_file)
        .working_directory(cwd)
        .stdout(stdout)
        .stderr(stderr)
        .start()
        .map_err(|e| {
            anyhow::anyhow!(
                "failed to start worker daemon: {e} \
                 (already running? check `c0mpute worker status`)"
            )
        })?;
    Ok(())
}

#[cfg(not(unix))]
fn daemonize_worker() -> Result<()> {
    anyhow::bail!("`worker start --daemon` is only supported on Unix platforms")
}

/// Attach mode (`worker start -a`): make sure a background worker is running
/// (launch one via `worker start -d` if not), then stream its log to the
/// terminal until the user detaches with Ctrl-D (stdin EOF) or Ctrl-C. The
/// worker is a standalone daemon the whole time, so detaching just stops the
/// viewer — it never touches the worker.
#[cfg(unix)]
fn run_attached_worker(passthrough: Vec<String>) -> Result<()> {
    use std::io::{IsTerminal, Read, Seek, SeekFrom, Write};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    static DETACH: AtomicBool = AtomicBool::new(false);
    extern "C" fn on_signal(_sig: libc::c_int) {
        DETACH.store(true, Ordering::SeqCst);
    }

    let (_pid_file, log_file) = worker_runtime_paths();

    // Attach to an already-running worker, or spawn a detached one. The spawned
    // child does the fork-before-runtime itself, so nothing unsafe happens here.
    let pid = if let Some(p) = read_worker_pid().filter(|p| pid_alive(*p)) {
        println!("worker already running (pid {p}); attaching");
        Some(p)
    } else {
        let exe = std::env::current_exe()?;
        let mut args = vec!["worker".to_string(), "start".to_string(), "-d".to_string()];
        args.extend(passthrough);
        let status = Command::new(&exe)
            .args(&args)
            .stdout(std::process::Stdio::null()) // we print our own banner
            .status()?;
        if !status.success() {
            anyhow::bail!("worker failed to start");
        }
        // The daemon writes its PID file just after forking — poll briefly.
        let mut pid = None;
        for _ in 0..50 {
            pid = read_worker_pid().filter(|p| pid_alive(*p));
            if pid.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        pid
    };

    match pid {
        Some(p) => println!(
            "attached to worker (pid {p}) — Ctrl-D or Ctrl-C to detach; the worker keeps running"
        ),
        None => {
            println!("attached to worker — Ctrl-D or Ctrl-C to detach; the worker keeps running")
        }
    }

    // Replay recent history so attaching isn't a blank screen — a settled
    // worker is nearly silent, so tailing only *new* output shows nothing.
    const HISTORY_BYTES: u64 = 16 * 1024;
    let end = std::fs::metadata(&log_file).map(|m| m.len()).unwrap_or(0);
    if end == 0 {
        println!(
            "(no output at {} yet — if the worker runs under systemd, follow it with: \
             journalctl --user -u c0mpute-worker -f)",
            log_file.display()
        );
    } else if let Ok(mut f) = std::fs::File::open(&log_file) {
        let from = end.saturating_sub(HISTORY_BYTES);
        if f.seek(SeekFrom::Start(from)).is_ok() {
            let mut buf = Vec::new();
            if f.read_to_end(&mut buf).is_ok() {
                // Drop a partial first line when we started mid-file.
                let slice: &[u8] = match (from > 0, buf.iter().position(|&b| b == b'\n')) {
                    (true, Some(i)) => &buf[i + 1..],
                    _ => &buf,
                };
                let out = std::io::stdout();
                let _ = out.lock().write_all(slice);
            }
        }
    }

    // Detach on Ctrl-D (0x04) or Ctrl-C (0x03). Put the terminal in raw mode so
    // we catch the keystroke itself rather than relying on canonical-mode EOF
    // (which only fires on an empty line and proved unreliable across terminals).
    // Only c_lflag is touched, so OPOST newline translation still renders the
    // streamed log correctly.
    struct RawGuard {
        fd: libc::c_int,
        orig: libc::termios,
    }
    impl Drop for RawGuard {
        fn drop(&mut self) {
            unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.orig) };
        }
    }
    let mut raw_guard: Option<RawGuard> = None;
    if std::io::stdin().is_terminal() {
        unsafe {
            let fd = libc::STDIN_FILENO;
            let mut orig: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut orig) == 0 {
                let mut raw = orig;
                raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
                raw.c_cc[libc::VMIN] = 1;
                raw.c_cc[libc::VTIME] = 0;
                if libc::tcsetattr(fd, libc::TCSANOW, &raw) == 0 {
                    raw_guard = Some(RawGuard { fd, orig });
                }
            }
        }
        std::thread::spawn(|| {
            let mut b = [0u8; 1];
            loop {
                let n =
                    unsafe { libc::read(libc::STDIN_FILENO, b.as_mut_ptr() as *mut libc::c_void, 1) };
                // EOF/error, Ctrl-D, or Ctrl-C → detach.
                if n <= 0 || b[0] == 0x04 || b[0] == 0x03 {
                    DETACH.store(true, Ordering::SeqCst);
                    break;
                }
            }
        });
    }
    // Safety net for `kill -INT/-TERM` (and Ctrl-C when raw mode was unavailable).
    let handler = on_signal as extern "C" fn(libc::c_int);
    unsafe {
        libc::signal(libc::SIGINT, handler as usize as libc::sighandler_t);
        libc::signal(libc::SIGTERM, handler as usize as libc::sighandler_t);
    }

    // Follow the log until detached.
    let drain = |from: u64| -> u64 {
        let mut at = from;
        if let Ok(mut f) = std::fs::File::open(&log_file) {
            let len = f.metadata().map(|m| m.len()).unwrap_or(at);
            if len > at && f.seek(SeekFrom::Start(at)).is_ok() {
                let mut chunk = Vec::new();
                if f.take(len - at).read_to_end(&mut chunk).is_ok() {
                    let out = std::io::stdout();
                    let mut h = out.lock();
                    let _ = h.write_all(&chunk);
                    let _ = h.flush();
                    at = len;
                }
            }
        }
        at
    };

    let mut pos = end;
    while !DETACH.load(Ordering::SeqCst) {
        pos = drain(pos);
        std::thread::sleep(Duration::from_millis(150));
    }
    drain(pos); // final flush
    drop(raw_guard); // restore cooked terminal before the final line + shell prompt

    match read_worker_pid().filter(|p| pid_alive(*p)) {
        Some(p) => {
            println!("\ndetached — worker still running (pid {p}); stop with: c0mpute worker stop")
        }
        None => println!("\ndetached"),
    }
    Ok(())
}

#[cfg(not(unix))]
fn run_attached_worker(_passthrough: Vec<String>) -> Result<()> {
    anyhow::bail!("`worker start --attach` is only supported on Unix platforms")
}

/// True if a process with this PID exists (signal 0 probes without delivering).
#[cfg(unix)]
fn pid_alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(not(unix))]
fn pid_alive(_pid: i32) -> bool {
    false
}

#[cfg(unix)]
fn stop_worker() -> Result<()> {
    match read_worker_pid() {
        Some(pid) if pid_alive(pid) => {
            if unsafe { libc::kill(pid, libc::SIGTERM) } == 0 {
                println!("sent SIGTERM to worker (pid {pid})");
                Ok(())
            } else {
                Err(anyhow::anyhow!(
                    "failed to signal worker pid {pid}: {}",
                    std::io::Error::last_os_error()
                ))
            }
        }
        Some(pid) => {
            // Stale PID file — the flock is released, so the next `start`
            // will reclaim it. Clean it up so `status` reads true.
            let (pid_file, _) = worker_runtime_paths();
            let _ = std::fs::remove_file(pid_file);
            println!("no running worker (cleared stale pid {pid})");
            Ok(())
        }
        None => {
            println!("no running worker");
            Ok(())
        }
    }
}

#[cfg(not(unix))]
fn stop_worker() -> Result<()> {
    anyhow::bail!("`worker stop` is only supported on Unix platforms")
}

// ────────────────────────────────────────────────────────────────────────
// job
// ────────────────────────────────────────────────────────────────────────

async fn run_job(cmd: JobCmd) -> Result<()> {
    match cmd {
        JobCmd::Submit { manifest } => {
            println!("[stub] would POST {} to coordinator", manifest.display());
            Ok(())
        }
        JobCmd::Status { id } => {
            println!("[stub] status for {id}");
            Ok(())
        }
        JobCmd::Logs { id, follow } => {
            println!("[stub] logs for {id} (follow={follow})");
            Ok(())
        }
        JobCmd::Cancel { id } => {
            println!("[stub] cancel {id}");
            Ok(())
        }
    }
}

// ────────────────────────────────────────────────────────────────────────
// transcode plugin (in-process)
// ────────────────────────────────────────────────────────────────────────

async fn run_transcode(cmd: TranscodeCmd) -> Result<()> {
    match cmd {
        TranscodeCmd::Submit {
            input,
            preset,
            max_price,
        } => {
            // Phase 1: only HTTP(S) URLs are supported as inputs (the
            // worker fetches via reqwest). Local-file submission needs
            // chunk-store + libp2p request-response, which is Phase 2.
            let input_url = input.to_string_lossy().to_string();
            if !(input_url.starts_with("http://") || input_url.starts_with("https://")) {
                anyhow::bail!(
                    "Phase 1 requires an http(s) URL for --input (local file submission \
                     uses the chunk store and lands in Phase 2). Got: {input_url}"
                );
            }
            let spec = preset_to_spec(&preset)?;
            let inline = TranscodeJobInline {
                input_url: input_url.clone(),
                preset: preset.clone(),
                spec,
            };
            let spec_inline = serde_json::to_value(&inline)?;

            // Boot a buyer-mode libp2p node — this CLI invocation is
            // ephemeral, no roles, no advertise.
            let cfg = Config::default();
            let sup = Supervisor::boot(cfg).await?;
            let net = sup.libp2p.clone();
            // Tiny grace period so peer discovery (mDNS) sees us.
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;

            let auction = JobAuction::new("ffmpeg.transcode", spec_inline)
                .with_required_capabilities(vec!["c0mpute:role:transcode".into()])
                .with_max_price_usd(max_price.unwrap_or(1.0));
            let outcome = run_auction(net, auction).await?;
            println!("job_id        : {}", outcome.job_id);
            println!("winner        : {}", outcome.accepted_bid.bidder_peer_id);
            println!("price_usd     : {}", outcome.accepted_bid.price_usd);
            println!("status        : {:?}", outcome.receipt.status);
            if let Some(h) = &outcome.receipt.output_hash {
                println!("output_hash   : {h}");
            }
            Ok(())
        }
        TranscodeCmd::Preset {
            cmd: PresetCmd::List,
        } => {
            for p in [
                "audio-mp3",
                "audio-aac",
                "audio-opus",
                "video-720p",
                "video-1080p",
                "video-4k",
                "hls",
                "dash",
                "thumbnail",
                "gif",
                "extract-audio",
                "normalize-audio",
            ] {
                println!("{p}");
            }
            Ok(())
        }
        TranscodeCmd::Doctor => {
            println!("OK   ffmpeg presence (delegated to top-level doctor)");
            Ok(())
        }
    }
}

// ────────────────────────────────────────────────────────────────────────
// secure-chat plugin (in-process)
// ────────────────────────────────────────────────────────────────────────

fn run_chat(cmd: ChatCmd) -> Result<()> {
    match cmd {
        ChatCmd::Keygen => {
            let path = chat::key_file_path()?;
            if path.exists() {
                eprintln!(
                    "A keyfile already exists at {}.\n\
                     Use `c0mpute chat key rotate` to generate a new one, or\n\
                     delete the file manually to start fresh.",
                    path.display()
                );
                anyhow::bail!("keyfile already exists");
            }
            let key = chat::ChatKey::generate();
            let password = prompt_new_password()?;
            let kf = key.encrypt_to_keyfile(&password, None)?;
            chat::save_keyfile(&path, &kf)?;

            println!("keypair generated");
            println!("  enc pubkey  : {}", kf.pubkey_enc);
            println!("  sig pubkey  : {}", kf.pubkey_sig);
            println!("  fingerprint : {}", key.fingerprint());
            println!("  keyfile     : {}", path.display());
            println!();
            println!("── encrypted backup (save this somewhere safe) ──");
            println!("{}", serde_json::to_string_pretty(&kf)?);
            println!("─────────────────────────────────────────────────");
            println!();
            println!("restore with: c0mpute chat restore --from-backup <file>");
            Ok(())
        }

        ChatCmd::Key { cmd } => match cmd {
            KeyCmd::Show => {
                let path = chat::key_file_path()?;
                let kf = chat::load_keyfile(&path)?;
                println!("enc pubkey  : {}", kf.pubkey_enc);
                println!("sig pubkey  : {}", kf.pubkey_sig);
                if let Some(did) = &kf.did {
                    println!("did         : {did}");
                }
                // Fingerprint needs decryption — show pubkey hash instead
                use base64::prelude::*;
                let enc_bytes = BASE64_URL_SAFE_NO_PAD.decode(&kf.pubkey_enc)?;
                let fingerprint: String = enc_bytes[..8]
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<Vec<_>>()
                    .join(":");
                println!("fingerprint : {fingerprint}");
                Ok(())
            }
            KeyCmd::Export => {
                let path = chat::key_file_path()?;
                let kf = chat::load_keyfile(&path)?;
                println!("{}", serde_json::to_string_pretty(&kf)?);
                Ok(())
            }
            KeyCmd::Rotate => {
                println!("[v0.2] key rotation requires DHT re-announcement (not yet live)");
                println!("       Generate a new keypair manually:");
                println!("         1. Delete ~/.config/c0mpute/chat.key");
                println!("         2. Run: c0mpute chat keygen");
                Ok(())
            }
        },

        ChatCmd::Restore { from_backup } => {
            let dest = chat::key_file_path()?;
            if dest.exists() {
                eprintln!(
                    "A keyfile already exists at {}. Remove it first.",
                    dest.display()
                );
                anyhow::bail!("keyfile already exists");
            }
            let raw = std::fs::read_to_string(&from_backup)?;
            let kf: chat::KeyFile = serde_json::from_str(&raw)?;
            let password = rpassword::prompt_password("Password: ")?;
            // Verify the password decrypts correctly before saving.
            chat::decrypt_keyfile(&kf, &password)?;
            chat::save_keyfile(&dest, &kf)?;
            println!("keyfile restored to {}", dest.display());
            Ok(())
        }

        ChatCmd::Send { to, message: _, sealed: _ } => {
            println!("[v0.2] direct send to {to} requires p2p transport (not yet live)");
            println!("       DHT key lookup and gossip relay land in v0.2.");
            Ok(())
        }

        ChatCmd::Pull => {
            println!("[v0.2] pull requires relay node support (not yet live)");
            Ok(())
        }

        ChatCmd::Lookup { did } => {
            println!("[v0.2] DHT lookup for {did} not yet live");
            Ok(())
        }

        ChatCmd::Contacts => {
            println!("[v0.2] contact list not yet implemented");
            Ok(())
        }

        ChatCmd::Block { did } => {
            println!("[v0.2] block list for {did} not yet implemented");
            Ok(())
        }
    }
}

fn prompt_new_password() -> Result<String> {
    loop {
        let pw = rpassword::prompt_password("New password (min 10 chars): ")?;
        if pw.len() < 10 {
            eprintln!("password too short — must be at least 10 characters");
            continue;
        }
        let confirm = rpassword::prompt_password("Confirm password: ")?;
        if pw != confirm {
            eprintln!("passwords do not match, try again");
            continue;
        }
        return Ok(pw);
    }
}

// ────────────────────────────────────────────────────────────────────────
// plugin registry stub
// ────────────────────────────────────────────────────────────────────────

fn run_plugin(cmd: PluginCmd) -> Result<()> {
    match cmd {
        PluginCmd::List => {
            println!("transcode  v0.1.0  in-process  built-in");
            println!("coinpay    v0.1.0  subprocess  {}", peer_status_text("coinpay"));
            println!("infernet   v0.1.0  subprocess  {}", peer_status_text("infernet"));
            Ok(())
        }
        PluginCmd::Install { target } => install_plugin(&target),
        PluginCmd::Enable { id } => {
            println!("[stub] enable {id}");
            Ok(())
        }
        PluginCmd::Disable { id } => {
            println!("[stub] disable {id}");
            Ok(())
        }
        PluginCmd::Uninstall { id } => {
            println!("[stub] uninstall {id}");
            Ok(())
        }
    }
}

/// Phase-1 preset → TranscodeSpec. Three are wired so the round-trip
/// demo works — the full preset library lands when the marketplace
/// flow is mature enough to be worth fleshing out.
fn preset_to_spec(preset: &str) -> Result<c0mpute_proto::TranscodeSpec> {
    use c0mpute_proto::{Codec, TranscodeSpec};
    let s = match preset {
        "video-720p" => TranscodeSpec {
            codec: Codec::H264,
            bitrate_bps: 2_500_000,
            width: 1280,
            height: 720,
            keyframe_interval: 60,
            hardware_pref: None,
            extra_ffmpeg_args: vec![],
        },
        "video-1080p" => TranscodeSpec {
            codec: Codec::H264,
            bitrate_bps: 5_000_000,
            width: 1920,
            height: 1080,
            keyframe_interval: 60,
            hardware_pref: None,
            extra_ffmpeg_args: vec![],
        },
        "video-4k" => TranscodeSpec {
            codec: Codec::Hevc,
            bitrate_bps: 15_000_000,
            width: 3840,
            height: 2160,
            keyframe_interval: 60,
            hardware_pref: None,
            extra_ffmpeg_args: vec![],
        },
        other => anyhow::bail!(
            "preset '{other}' isn't wired in Phase 1 (try video-720p, video-1080p, video-4k)"
        ),
    };
    Ok(s)
}

/// Resolve a `c0mpute plugin install <target>` argument and dispatch.
///
/// Accepts either:
///   - a plugin **id** registered in the c0mpute marketplace, in which
///     case we chain to `https://c0mpute.com/plugins/<id>/install.sh`
///   - or any **install.sh URL** (third-party plugins published at
///     their own URL — e.g. a GitHub raw URL)
///
/// Both flows pipe `curl -fsSL <url>` into `sh`. Future: minisign
/// signature verification before execution (per DIP-0006).
fn install_plugin(target: &str) -> Result<()> {
    let url = resolve_plugin_target(target);
    println!("→ installing plugin via {url}");
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("curl -fsSL {url} | sh"))
        .status()?;
    if !status.success() {
        anyhow::bail!("plugin install failed (exit {status})");
    }
    Ok(())
}

fn resolve_plugin_target(target: &str) -> String {
    let t = target.trim();
    if t.starts_with("http://") || t.starts_with("https://") {
        return t.to_string();
    }
    // Treat anything else as a plugin id and resolve through the
    // c0mpute marketplace. The route at c0mpute.com/plugins/<id>/install.sh
    // serves the manifest-checked-in install script for that plugin.
    format!("https://c0mpute.com/plugins/{t}/install.sh")
}

// ────────────────────────────────────────────────────────────────────────
// update / uninstall (alias: upgrade / remove)
// ────────────────────────────────────────────────────────────────────────

async fn run_update(check_only: bool, feed: Option<String>) -> Result<()> {
    use c0mpute_update::UpgradeOutcome;
    let feed = feed.unwrap_or_else(|| c0mpute_update::DEFAULT_RELEASE_FEED.to_string());
    let current = env!("CARGO_PKG_VERSION");

    if check_only {
        match c0mpute_update::try_upgrade(current, &feed).await? {
            UpgradeOutcome::AlreadyLatest { current } => {
                println!("c0mpute {current} — already latest")
            }
            UpgradeOutcome::Available { current, latest } => {
                println!("update available: {current} → {latest} (run `c0mpute update` to install)")
            }
            UpgradeOutcome::Upgraded { .. } => unreachable!("try_upgrade never swaps"),
        }
        return Ok(());
    }

    match c0mpute_update::upgrade_now(current, &feed).await {
        Ok(UpgradeOutcome::AlreadyLatest { current }) => {
            println!("c0mpute {current} — already latest");
        }
        Ok(UpgradeOutcome::Upgraded { from, to }) => {
            println!("upgraded {from} → {to}");
            println!("restart any running worker to apply: c0mpute worker stop && c0mpute worker start -d");
        }
        Ok(UpgradeOutcome::Available { .. }) => unreachable!("upgrade_now applies or errors"),
        Err(e) => {
            println!("update failed: {e:#}");
            println!(
                "reinstall manually: curl -fsSL https://c0mpute.com/install.sh | sh -s -- --force"
            );
        }
    }
    // `c0mpute update` always brings the plugins along too.
    upgrade_plugins_foreground();
    Ok(())
}

fn run_uninstall(all: bool, purge: bool, yes: bool) -> Result<()> {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let bin_dir = std::path::PathBuf::from(&home).join(".c0mpute/bin");

    let mut targets: Vec<std::path::PathBuf> = vec![bin_dir.join("c0mpute")];
    if all {
        for peer in ["coinpay", "infernet", "c0mpute-tui"] {
            targets.push(bin_dir.join(peer));
        }
    }
    if purge {
        targets.push(std::path::PathBuf::from(&home).join(".config/c0mpute"));
    }

    println!("Will remove:");
    for t in &targets {
        println!("  {}", t.display());
    }
    if !yes {
        print!("Proceed? [y/N] ");
        use std::io::Write;
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        let a = answer.trim();
        if a != "y" && a != "Y" {
            println!("aborted");
            return Ok(());
        }
    }

    for t in &targets {
        if t.is_file() || t.is_symlink() {
            std::fs::remove_file(t).ok();
        } else if t.is_dir() {
            std::fs::remove_dir_all(t).ok();
        }
    }
    println!("uninstalled. (PATH entries in shell rc files left in place)");
    Ok(())
}

// ────────────────────────────────────────────────────────────────────────
// helpers
// ────────────────────────────────────────────────────────────────────────

fn parse_role(s: &str) -> Option<Role> {
    match s.trim().to_ascii_lowercase().as_str() {
        "storage" => Some(Role::Storage),
        "transcode" => Some(Role::Transcode),
        "gateway" => Some(Role::Gateway),
        "verifier" => Some(Role::Verifier),
        other => {
            eprintln!("warning: unknown role '{other}' ignored");
            None
        }
    }
}

/// Interactive command menu shown when `c0mpute` is run with no subcommand in a
/// TTY: browse the command tree, drill into subcommands, and run — spawning a
/// child `c0mpute` so the real handlers run unchanged.
fn run_menu() -> Result<()> {
    use clap::CommandFactory;
    use dialoguer::{theme::ColorfulTheme, Select};

    let root = Cli::command();
    let mut path: Vec<String> = Vec::new();

    loop {
        // Resolve the current node by descending `path` from the root each pass
        // (avoids holding a borrow of `root` across mutations of `path`).
        let mut node = &root;
        for p in &path {
            match node.find_subcommand(p) {
                Some(sc) => node = sc,
                None => break,
            }
        }

        let subs: Vec<(String, String)> = node
            .get_subcommands()
            .filter(|c| c.get_name() != "help" && !c.is_hide_set())
            .map(|c| {
                (
                    c.get_name().to_string(),
                    c.get_about().map(|s| s.to_string()).unwrap_or_default(),
                )
            })
            .collect();

        if subs.is_empty() {
            break; // leaf — run the accumulated path
        }

        let mut labels: Vec<String> = subs
            .iter()
            .map(|(n, a)| if a.is_empty() { n.clone() } else { format!("{n:<12} {a}") })
            .collect();
        let has_extra = !path.is_empty();
        if has_extra {
            labels.push(format!("▶ run: c0mpute {}", path.join(" ")));
            labels.push("↩ back".to_string());
        }

        let prompt = if path.is_empty() {
            "c0mpute — pick a command".to_string()
        } else {
            format!("c0mpute {}", path.join(" "))
        };

        let Some(idx) = Select::with_theme(&ColorfulTheme::default())
            .with_prompt(prompt)
            .items(&labels)
            .default(0)
            .interact_opt()?
        else {
            return Ok(()); // Esc / Ctrl-C
        };

        if has_extra && idx == subs.len() {
            break; // "▶ run" the current path
        }
        if has_extra && idx == subs.len() + 1 {
            path.pop(); // "↩ back"
            continue;
        }
        path.push(subs[idx].0.clone());
    }

    if path.is_empty() {
        return Ok(());
    }

    // Re-exec `c0mpute <path…>` so the real command handlers run.
    let exe = std::env::current_exe()?;
    let status = std::process::Command::new(exe).args(&path).status()?;
    std::process::exit(status.code().unwrap_or(0));
}

/// `c0mpute tui` — launch the terminal UI, installing it on demand the first
/// time. The TUI is a Bun app (react-blessed), not part of the Rust binary, so
/// `c0mpute tui` fetches the source, runs `bun install`, and writes a launcher —
/// the user never has to know about the underlying `c0mpute-tui` binary.
fn run_tui(args: &[String]) -> Result<()> {
    if which_on_path("c0mpute-tui").is_none() {
        install_tui()?;
    }
    delegate("c0mpute-tui", args)
}

/// Locate a usable `bun`, installing it via mise if needed. bun may be on PATH,
/// under ~/.bun, or managed by mise (which the c0mpute installer uses, sometimes
/// with a relocated data dir) — so a plain `which bun` isn't enough.
fn ensure_bun() -> Option<PathBuf> {
    if let Some(p) = which_on_path("bun") {
        return Some(p);
    }
    if let Ok(home) = std::env::var("HOME") {
        let p = PathBuf::from(home).join(".bun/bin/bun");
        if p.exists() {
            return Some(p);
        }
    }
    let mise = which_on_path("mise")?;
    let mise_which = |m: &std::path::Path| -> Option<PathBuf> {
        let out = Command::new(m).args(["which", "bun"]).output().ok()?;
        if !out.status.success() {
            return None;
        }
        let p = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        p.exists().then_some(p)
    };
    if let Some(p) = mise_which(&mise) {
        return Some(p);
    }
    // Not installed yet — install it via mise.
    println!("installing bun via mise…");
    let _ = Command::new(&mise)
        .args(["use", "--global", "bun@latest"])
        .status();
    mise_which(&mise).or_else(|| which_on_path("bun"))
}

fn install_tui() -> Result<()> {
    println!("c0mpute tui: installing the terminal UI (one-time)…");
    let home = std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| anyhow::anyhow!("HOME not set"))?;
    let bun = ensure_bun().ok_or_else(|| {
        anyhow::anyhow!(
            "bun is required for the TUI and couldn't be installed via mise. \
             Install it, then rerun `c0mpute tui`:\n  curl -fsSL https://bun.sh/install | bash"
        )
    })?;

    let tui_dir = home.join(".c0mpute/tui");
    let wrapper = home.join(".c0mpute/bin/c0mpute-tui");
    let tmp = std::env::temp_dir().join("c0mpute-tui-install");
    let git_ref = std::env::var("C0MPUTE_TUI_REF").unwrap_or_else(|_| "master".into());

    // Fetch the repo, copy apps/tui into ~/.c0mpute/tui, and materialise deps.
    let script = format!(
        "set -e\n\
         rm -rf '{tmp}' && mkdir -p '{tmp}'\n\
         curl -fsSL 'https://github.com/profullstack/c0mpute/archive/refs/heads/{git_ref}.tar.gz' | tar -xz -C '{tmp}'\n\
         src=$(find '{tmp}' -type d -path '*/apps/tui' | head -1)\n\
         [ -d \"$src/src\" ] || {{ echo 'TUI source not found' >&2; exit 1; }}\n\
         rm -rf '{tui}' && mkdir -p '{tui}'\n\
         cp -R \"$src/src\" \"$src/package.json\" \"$src/tsconfig.json\" '{tui}/'\n\
         cd '{tui}' && '{bun}' install --no-save >/dev/null 2>&1\n\
         rm -rf '{tmp}'\n",
        tmp = tmp.display(),
        git_ref = git_ref,
        tui = tui_dir.display(),
        bun = bun.display(),
    );
    let ok = Command::new("sh")
        .arg("-c")
        .arg(&script)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        anyhow::bail!("failed to fetch/build the TUI (need network + bun)");
    }

    // Launcher — runs the Bun source (bun --compile can't bundle blessed's
    // dynamic widget requires).
    let body = format!(
        "#!/usr/bin/env sh\n\
         # c0mpute-tui launcher (installed on demand by `c0mpute tui`).\n\
         BUN=\"$(command -v bun 2>/dev/null || true)\"\n\
         [ -z \"$BUN\" ] && [ -x \"$HOME/.bun/bin/bun\" ] && BUN=\"$HOME/.bun/bin/bun\"\n\
         [ -z \"$BUN\" ] && command -v mise >/dev/null 2>&1 && BUN=\"$(mise which bun 2>/dev/null || true)\"\n\
         [ -n \"$BUN\" ] || {{ echo 'bun not found; run: mise use --global bun@latest' >&2; exit 1; }}\n\
         exec \"$BUN\" run \"{}/src/index.tsx\" \"$@\"\n",
        tui_dir.display()
    );
    std::fs::write(&wrapper, body).map_err(|e| anyhow::anyhow!("write launcher: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755));
    }
    Ok(())
}

fn delegate(bin: &str, args: &[String]) -> Result<()> {
    let path = which_on_path(bin).ok_or_else(|| {
        anyhow::anyhow!(
            "{bin} not found on PATH. Install with:\n  curl -fsSL https://c0mpute.com/install.sh | sh"
        )
    })?;
    let status = Command::new(path).args(args).status()?;
    if !status.success() {
        anyhow::bail!("{bin} exited {status}");
    }
    Ok(())
}

/// Ensure the operator has a payable CoinPay DID, as part of `worker register`.
///
/// Runs `coinpay reputation did setup`, which discovers an existing DID and
/// confirms its use, or offers to create one — interactively when a terminal
/// is attached, and non-interactively (sensible default) under `curl | sh`.
/// Best-effort: a worker runs without a DID, so a missing or logged-out
/// coinpay must never fail registration — we just point the operator at it.
fn ensure_coinpay_did() {
    let Some(coinpay) = which_on_path("coinpay") else {
        println!("payable DID: `coinpay` not installed — run `c0mpute coinpay reputation did setup` later to set one up.");
        return;
    };
    println!("payable DID:");
    match Command::new(coinpay)
        .args(["reputation", "did", "setup"])
        .status()
    {
        Ok(s) if s.success() => {}
        Ok(_) => println!(
            "  (not set up — are you logged in? run `c0mpute coinpay login`, then `c0mpute coinpay reputation did setup`)"
        ),
        Err(e) => println!("  (couldn't run coinpay: {e})"),
    }
}

/// `c0mpute login` — sign in to every network this node participates in, one
/// at a time: coinpay (payments / payable DID) then infernet (ties the node to
/// your infernetprotocol.com account). Each is a browser device-code flow.
/// Best-effort per service — one failing/declined login doesn't abort the rest.
fn run_login() -> Result<()> {
    println!("Signing in to your c0mpute accounts (coinpay + infernet)...\n");
    login_one("coinpay", "payments + payable DID");
    // infernet login needs a configured control plane — init + register first,
    // otherwise `infernet login` errors with "no control plane configured".
    if which_on_path("infernet").is_some() && !infernet_initialized() {
        println!("── infernet ── initializing node (init + register)…");
        ensure_infernet_initialized();
    }
    login_one("infernet", "ties this node to your infernetprotocol.com account");
    // Bind this node to the account so it shows on /dashboard and can be
    // assigned models/jobs (best-effort; no-op if not installed or not authed).
    if which_on_path("infernet").is_some() {
        println!("── infernet ── linking node to your account…");
        let _ = infernet_cmd(&["pubkey", "link"]);
    }
    login_hf();
    println!("Done. Next: c0mpute worker register  →  c0mpute worker start");
    Ok(())
}

/// HuggingFace sign-in — for model downloads (only strictly required for gated
/// models; public ones need nothing). Runs like the other providers: prefer the
/// new `hf auth login`, else legacy `huggingface-cli login`. Best-effort — a
/// skip/Ctrl-C just moves on, and HF_TOKEN in the env counts as signed in.
fn login_hf() {
    println!("── huggingface ── (model downloads; token-based — no OAuth device flow)");
    if std::env::var_os("HF_TOKEN").is_some() {
        println!("  ✓ HF_TOKEN already set in the environment\n");
        return;
    }
    let (bin, args): (&str, &[&str]) = if which_on_path("hf").is_some() {
        ("hf", &["auth", "login"])
    } else if which_on_path("huggingface-cli").is_some() {
        ("huggingface-cli", &["login"])
    } else {
        println!("  ! huggingface CLI not installed — skip (installed with vLLM on NVIDIA nodes)\n");
        return;
    };
    let path = which_on_path(bin).unwrap();
    match Command::new(path).args(args).status() {
        Ok(s) if s.success() => println!("  ✓ huggingface signed in\n"),
        Ok(_) => println!("  ! huggingface login skipped/failed (fine unless you use gated models)\n"),
        Err(e) => println!("  ! couldn't run {bin}: {e}\n"),
    }
}

/// Run `<bin> login`, inheriting stdio so its browser device-code flow works.
fn login_one(bin: &str, why: &str) {
    println!("── {bin} ── ({why})");
    match which_on_path(bin) {
        Some(path) => match Command::new(path).arg("login").status() {
            Ok(s) if s.success() => println!("  ✓ {bin} signed in\n"),
            Ok(_) => println!("  ! {bin} login did not complete — re-run `c0mpute login`\n"),
            Err(e) => println!("  ! couldn't run {bin}: {e}\n"),
        },
        None => println!("  ! {bin} not installed — skipping\n"),
    }
}

/// Path to infernet's config (`$XDG_CONFIG_HOME`/`~/.config` + infernet/config.json).
fn infernet_config_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|base| base.join("infernet").join("config.json"))
}

/// True once infernet has been initialized on this machine.
fn infernet_initialized() -> bool {
    infernet_config_path().map(|p| p.exists()).unwrap_or(false)
}

/// Run `infernet <args…>` inheriting stdio; returns whether it succeeded.
fn infernet_cmd(args: &[&str]) -> bool {
    match which_on_path("infernet") {
        Some(bin) => Command::new(bin)
            .args(args)
            .status()
            .map(|s| s.success())
            .unwrap_or(false),
        None => false,
    }
}

/// A human-readable node name for infernet — the machine hostname, falling
/// back to a constant.
fn infernet_node_name() -> String {
    Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "c0mpute-node".to_string())
}

/// Idempotently configure infernet as a provider node: `init` (generates a
/// Nostr identity) + `register` (announces to the control plane). No-op if
/// infernet isn't installed or is already initialized. Login is intentionally
/// NOT run here — it's a device-code flow.
///
/// `init` prompts for URL / name / firewall unless every flag is supplied, so
/// we pass them all and close stdin — a stray prompt must never hang a headless
/// worker. Control-plane URL is overridable via `INFERNET_URL`.
fn ensure_infernet_initialized() -> bool {
    let Some(bin) = which_on_path("infernet") else {
        return false;
    };
    if infernet_initialized() {
        return false;
    }
    let url =
        std::env::var("INFERNET_URL").unwrap_or_else(|_| "https://infernetprotocol.com".to_string());
    let name = infernet_node_name();
    // "Always accept": every prompt is supplied as a flag, and we pipe a stream
    // of "y" so any remaining prompt (e.g. the firewall rule, which opens the
    // P2P port) is auto-accepted — while never blocking a headless worker.
    if let Ok(mut child) = Command::new(&bin)
        .args(["init", "--url", &url, "--role", "provider", "--name", &name])
        .stdin(std::process::Stdio::piped())
        .spawn()
    {
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            let _ = stdin.write_all("y\n".repeat(20).as_bytes());
        }
        let _ = child.wait();
    }
    let _ = infernet_cmd(&["register"]);
    true
}

/// Best-effort version string for a peer CLI on PATH. Runs `<bin> --version`
/// with stdin closed (so tools that otherwise read a prompt can't hang) and
/// reads the first stdout line. Returns None if the binary isn't installed.
fn tool_version(bin: &str) -> Option<String> {
    let path = which_on_path(bin)?;
    let out = Command::new(&path)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    let raw = String::from_utf8_lossy(&out.stdout);
    let mut line = raw.lines().next().unwrap_or("").trim();
    // Normalize e.g. "infernet v0.1.45" → "0.1.45": drop a redundant leading
    // binary name and a leading "v" before the number.
    if let Some(rest) = line.strip_prefix(bin) {
        line = rest.trim();
    }
    if let Some(rest) = line.strip_prefix('v') {
        if rest.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            line = rest;
        }
    }
    Some(if line.is_empty() {
        "installed".to_string()
    } else {
        line.to_string()
    })
}

/// Print c0mpute's version plus every installed plugin (transcode built-in;
/// coinpay + infernet peer CLIs). `c0mpute tui` is a subcommand, not a plugin,
/// so it isn't listed here.
fn print_all_versions() {
    println!("{:<10} {}", "c0mpute", env!("CARGO_PKG_VERSION"));
    println!("{:<10} built-in", "transcode");
    for bin in ["coinpay", "infernet"] {
        match tool_version(bin) {
            Some(v) => println!("{bin:<10} {v}"),
            None => println!("{bin:<10} not installed"),
        }
    }
}

/// First-run infernet bootstrap for `worker start`: init + register, then a
/// non-interactive token login when `INFERNET_TOKEN` is set (device-code login
/// can't run unattended, so it's skipped otherwise). Best-effort and idempotent
/// — a worker runs fine without infernet.
fn bootstrap_infernet_first_run() {
    if which_on_path("infernet").is_none() || infernet_initialized() {
        return;
    }
    tracing::info!("first run: bootstrapping infernet (init + register)");
    ensure_infernet_initialized();
    match std::env::var("INFERNET_TOKEN") {
        Ok(token) if !token.is_empty() => {
            tracing::info!("infernet: logging in with INFERNET_TOKEN + linking node to account");
            let _ = infernet_cmd(&["login", "--token", &token]);
            // Bind the node's pubkey to the account so /dashboard lists it and
            // can route models/jobs to it (registration alone only makes it
            // "available", not owned by the account).
            let _ = infernet_cmd(&["pubkey", "link"]);
        }
        _ => tracing::info!(
            "infernet: login + account-link skipped (set INFERNET_TOKEN, or run `c0mpute login`)"
        ),
    }
}

/// Best-effort: make sure the infernet node daemon is running so the node picks
/// up pending jobs (model pulls, inference). Runs on every `worker start` once
/// infernet is initialized; `infernet start` detaches and is a no-op if the
/// daemon is already up. A worker runs fine if this fails — the infernet daemon
/// is infernet's own process, with its own lifecycle and health.
fn ensure_infernet_daemon() {
    if which_on_path("infernet").is_none() || !infernet_initialized() {
        return;
    }
    tracing::info!("ensuring infernet node daemon is running");
    let _ = infernet_cmd(&["start"]);
}

// ── infernet federated RPC serving (IPIP-0033) — opt-in ─────────────────────
//
// A node only counts toward "Distribute across all nodes" for a model by
// SERVING it over llama.cpp RPC: ≥2 slices (`infernet inference serve --backend
// rpc`) + a primary holding the GGUF (`infernet inference primary`). infernet
// provisions neither the llama.cpp binaries nor the GGUF, so c0mpute builds the
// binaries and drives the serve — config-driven, since the model + GGUF are
// operator choices:
//
//   C0MPUTE_RPC_MODELS="qwen2.5:72b,llama3:70b"          # serve as RPC slice
//   C0MPUTE_RPC_PRIMARY="qwen2.5:72b=/abs/model.gguf"    # host as primary
//
// The infernet daemon heartbeat advertises specs.rpc / specs.rpc_primary once
// the serve processes are up.

/// Built llama.cpp binary dir under ~/.c0mpute.
fn llama_build_bin_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".c0mpute/llama.cpp/build/bin"))
}

/// Kick off a background llama.cpp (RPC-enabled) build; single-flight via a lock
/// marker. Symlinks rpc-server + llama-server into ~/.c0mpute/bin when done.
fn build_llama_rpc_background() {
    let Ok(home) = std::env::var("HOME") else {
        return;
    };
    let lock = PathBuf::from(&home).join(".c0mpute/.llama-build.lock");
    if let Ok(Ok(e)) = std::fs::metadata(&lock)
        .and_then(|m| m.modified())
        .map(|t| t.elapsed())
    {
        if e < std::time::Duration::from_secs(3600) {
            return; // a build was started recently
        }
    }
    if let Some(p) = lock.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let _ = std::fs::write(&lock, b"");
    tracing::info!("building llama.cpp (rpc-server/llama-server) in background for infernet RPC serving");
    let script = "set -e\n\
         D=\"$HOME/.c0mpute/llama.cpp\"\n\
         [ -d \"$D/.git\" ] || git clone --depth 1 https://github.com/ggml-org/llama.cpp \"$D\"\n\
         cd \"$D\" && (git pull --ff-only 2>/dev/null || true)\n\
         cmake -B build -DGGML_RPC=ON -DCMAKE_BUILD_TYPE=Release >/dev/null\n\
         cmake --build build -j --target rpc-server llama-server\n\
         mkdir -p \"$HOME/.c0mpute/bin\"\n\
         ln -sf \"$D/build/bin/rpc-server\" \"$HOME/.c0mpute/bin/rpc-server\"\n\
         ln -sf \"$D/build/bin/llama-server\" \"$HOME/.c0mpute/bin/llama-server\"\n";
    let logf = std::fs::File::create(PathBuf::from(&home).join(".c0mpute/llama-build.log")).ok();
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(script).stdin(std::process::Stdio::null());
    if let Some(f) = logf {
        if let Ok(err) = f.try_clone() {
            cmd.stdout(f).stderr(err);
        }
    }
    let _ = cmd.spawn();
}

/// True if llama.cpp RPC binaries are ready (symlinking a prior build onto PATH
/// if needed); otherwise starts a background build and returns false.
fn ensure_llama_rpc(want_primary: bool) -> bool {
    let ready = || {
        which_on_path("rpc-server").is_some()
            && (!want_primary || which_on_path("llama-server").is_some())
    };
    if ready() {
        return true;
    }
    // A previous build may be present but not symlinked onto PATH yet.
    if let (Some(build), Ok(home)) = (llama_build_bin_dir(), std::env::var("HOME")) {
        if build.join("rpc-server").exists() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::symlink;
                let dst = PathBuf::from(&home).join(".c0mpute/bin");
                let _ = std::fs::create_dir_all(&dst);
                for b in ["rpc-server", "llama-server"] {
                    if build.join(b).exists() {
                        let _ = std::fs::remove_file(dst.join(b));
                        let _ = symlink(build.join(b), dst.join(b));
                    }
                }
            }
            if ready() {
                return true;
            }
        }
    }
    build_llama_rpc_background();
    false
}

/// Opt-in: serve configured models over infernet RPC so the node counts toward
/// "Distribute across all nodes". Idempotent-ish — `infernet inference
/// serve/primary` records state and the daemon heartbeat advertises it.
fn bootstrap_infernet_rpc() {
    let slices = std::env::var("C0MPUTE_RPC_MODELS").unwrap_or_default();
    let primaries = std::env::var("C0MPUTE_RPC_PRIMARY").unwrap_or_default();
    let slice_models: Vec<&str> = slices.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    let primary_entries: Vec<&str> = primaries.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    if slice_models.is_empty() && primary_entries.is_empty() {
        return;
    }
    if which_on_path("infernet").is_none() || !infernet_initialized() {
        return;
    }
    if !ensure_llama_rpc(!primary_entries.is_empty()) {
        tracing::info!(
            "infernet RPC serving deferred until llama.cpp finishes building (~/.c0mpute/llama-build.log)"
        );
        return;
    }
    for entry in primary_entries {
        match entry.split_once('=') {
            Some((model, gguf)) if std::path::Path::new(gguf.trim()).exists() => {
                tracing::info!(model = model.trim(), "infernet: hosting RPC primary");
                let _ = infernet_cmd(&[
                    "inference", "primary", "--model", model.trim(), "--gguf", gguf.trim(),
                ]);
            }
            _ => tracing::warn!(
                entry,
                "infernet RPC primary needs `model=/abs/model.gguf` with an existing GGUF; skipping"
            ),
        }
    }
    for model in slice_models {
        tracing::info!(model, "infernet: serving RPC slice");
        let _ = infernet_cmd(&["inference", "serve", "--backend", "rpc", "--model", model]);
    }
}

/// (binary, self-update args) for the peer-CLI plugins c0mpute manages.
const PLUGIN_UPDATERS: &[(&str, &[&str])] = &[
    ("coinpay", &["self", "update"]),
    ("infernet", &["update"]),
];

/// Upgrade every installed plugin in the foreground, streaming their output.
/// Used by `c0mpute update` so a manual update always brings plugins along.
fn upgrade_plugins_foreground() {
    for (bin, args) in PLUGIN_UPDATERS {
        if let Some(path) = which_on_path(bin) {
            println!("→ upgrading {bin}…");
            match Command::new(path).args(*args).status() {
                Ok(s) if s.success() => println!("  ✓ {bin} up to date"),
                Ok(_) => println!("  ! {bin} update reported a problem"),
                Err(e) => println!("  ! couldn't run {bin}: {e}"),
            }
        }
    }
}

/// Run each managed plugin's self-update once, waiting for each to finish so the
/// periodic loop never starts two overlapping (potentially heavy) updates.
/// Output is discarded; c0mpute drives the plugins so operators never update
/// nodes by hand. Also self-heals a stuck infernet whose old daemon can't
/// update itself. Runs on the background poll cadence.
fn refresh_plugins() {
    for (bin, args) in PLUGIN_UPDATERS {
        if let Some(path) = which_on_path(bin) {
            tracing::debug!(plugin = bin, "checking plugin for upgrades");
            let _ = Command::new(path)
                .args(*args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
}

fn which_on_path(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for entry in std::env::split_paths(&path) {
        let candidate = entry.join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}
