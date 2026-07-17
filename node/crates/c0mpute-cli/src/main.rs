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

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing()?;
    let cli = Cli::parse();
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
            println!("c0mpute {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Cmd::Doctor => run_doctor().await,
        Cmd::StatusAggregator { bind } => c0mpute_core::status_aggregator::run(bind).await,
        Cmd::Worker { cmd } => run_worker(cmd, &config_path).await,
        Cmd::Job { cmd } => run_job(cmd).await,
        Cmd::Plugin { cmd } => run_plugin(cmd),

        Cmd::Transcode { cmd } => run_transcode(cmd).await,
        Cmd::Chat { cmd } => run_chat(cmd),
        Cmd::Tui { args } => delegate("c0mpute-tui", &args),
        Cmd::Update { check, feed } => run_update(check, feed).await,
        Cmd::Uninstall { all, purge, yes } => run_uninstall(all, purge, yes),
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
            println!("  c0mpute worker start   # join the swarm");
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
    let feed = feed.unwrap_or_else(|| c0mpute_update::DEFAULT_RELEASE_FEED.to_string());
    let current = env!("CARGO_PKG_VERSION");
    let outcome = c0mpute_update::try_upgrade(current, &feed).await?;
    match outcome {
        c0mpute_update::UpgradeOutcome::AlreadyLatest { current } => {
            println!("c0mpute {current} — already latest");
        }
        c0mpute_update::UpgradeOutcome::Available { current, latest } => {
            if check_only {
                println!("update available: {current} → {latest}");
            } else {
                println!("update available: {current} → {latest}");
                println!(
                    "(downloading + signature-verified swap is stubbed; \
                     reinstall via: curl -fsSL https://c0mpute.com/install.sh | sh -s -- --force)"
                );
            }
        }
        c0mpute_update::UpgradeOutcome::Upgraded { from, to } => {
            println!("upgraded {from} → {to}; restart to apply");
        }
    }
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
