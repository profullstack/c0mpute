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
use c0mpute_core::{Config, Supervisor, config, init_tracing};
use c0mpute_proto::Role;

#[derive(Parser, Debug)]
#[command(
    name = "c0mpute",
    version,
    about = "c0mpute.com — decentralized compute network",
    long_about = "c0mpute.com CLI. Submit jobs, run a worker, manage modules.\n\nBuilt-in plugins:\n  transcode  (FFmpeg, in-process)\n  coinpay    (DID + payments, peer CLI)\n  infernet   (AI inference, peer CLI)\n\n  c0mpute coinpay did create\n  c0mpute transcode submit input.mov --preset hls\n  c0mpute infernet run prompts.jsonl --model qwen"
)]
struct Cli {
    /// Override the config file location.
    #[arg(long, env = "C0MPUTE_CONFIG", global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Cmd,
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
    /// Uninstall a plugin.
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

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing()?;
    let cli = Cli::parse();
    let config_path = cli.config.unwrap_or_else(config::default_config_path);

    match cli.command {
        Cmd::Version => {
            println!("c0mpute {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Cmd::Doctor => run_doctor().await,
        Cmd::Worker { cmd } => run_worker(cmd, &config_path).await,
        Cmd::Job { cmd } => run_job(cmd).await,
        Cmd::Plugin { cmd } => run_plugin(cmd),

        Cmd::Transcode { cmd } => run_transcode(cmd).await,
        Cmd::Tui { args } => delegate("c0mpute-tui", &args),
        Cmd::Coinpay { args } => delegate("coinpay", &args),
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
            println!("[stub] worker registration — pending CoinPay DID + coordinator wiring");
            println!("       run: c0mpute coinpay did create --role worker");
            Ok(())
        }
        WorkerCmd::Status => {
            let cfg = Config::load_or_default(config_path)?;
            println!("{}", serde_json::to_string_pretty(&cfg)?);
            Ok(())
        }
        WorkerCmd::Stop => {
            println!("[stub] no running worker to stop");
            Ok(())
        }
        WorkerCmd::Start {
            roles,
            storage: _,
            gpu,
        } => {
            let mut cfg = Config::load_or_default(config_path)?;
            if let Some(rs) = roles {
                cfg.roles = rs.iter().filter_map(|s| parse_role(s)).collect();
            }
            if gpu && !cfg.roles.contains(&Role::Transcode) {
                cfg.roles.push(Role::Transcode);
            }
            let sup = Supervisor::boot(cfg).await?;
            sup.run().await
        }
    }
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
            println!(
                "[stub] would build ffmpeg.transcode job manifest for {} (preset={}, max_price={:?})",
                input.display(),
                preset,
                max_price
            );
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

/// Resolve a `c0mpute plugin install <target>` argument and dispatch.
///
/// Resolution rules (per DIP-0006):
///   1. If target looks like a URL ending in `install.sh`, chain-call it.
///   2. If target is an http(s) URL ending in `.tar.gz`, download +
///      verify the signed tarball.
///   3. Otherwise treat target as a marketplace plugin id and resolve via
///      `c0mpute.com/api/v1/plugins/<id>`.
///
/// Today only #1 is wired (chains to a third-party install.sh). #2 and #3
/// are stubs.
fn install_plugin(target: &str) -> Result<()> {
    if target.starts_with("http://") || target.starts_with("https://") {
        if target.ends_with("install.sh") || target.ends_with("install") {
            println!(
                "[chain] curl -fsSL {target} | sh   # would run the upstream installer"
            );
            // Real implementation: spawn `curl -fsSL <target> | sh` after
            // confirming integrity (signed checksum). Stubbed for safety
            // until the marketplace ships a signing scheme.
            return Ok(());
        }
        if target.ends_with(".tar.gz") {
            println!("[stub] would download + verify minisign signature for {target}");
            return Ok(());
        }
        anyhow::bail!(
            "unsupported plugin URL (expected install.sh or .tar.gz): {target}"
        );
    }
    println!("[stub] would resolve marketplace plugin id `{target}` via c0mpute.com");
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
