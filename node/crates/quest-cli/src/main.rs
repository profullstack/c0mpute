//! `depin` — the depin.quest CLI.
//!
//! Commands are nested under product-line namespaces that mirror the URL
//! structure on `depin.quest/<line>`. Today the only line is `video` (Quest);
//! `depin storage`, `depin compute`, etc. plug in alongside as we add them.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use quest_core::{Config, Supervisor, config, init_tracing};
use quest_proto::Role;

#[derive(Parser, Debug)]
#[command(
    name = "depin",
    version,
    about = "depin.quest — decentralized infrastructure CLI",
    long_about = "depin.quest CLI. Commands are namespaced by product line, e.g. `depin video start`."
)]
struct Cli {
    /// Override the config file location.
    #[arg(long, env = "DEPIN_CONFIG", global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: TopCmd,
}

#[derive(Subcommand, Debug)]
enum TopCmd {
    /// Quest — decentralized video transcoding & hosting.
    Video {
        #[command(subcommand)]
        cmd: VideoCmd,
    },
    /// Print the version and exit.
    Version,
}

#[derive(Subcommand, Debug)]
enum VideoCmd {
    /// Start the node and run until ctrl-c.
    Start {
        /// Comma-separated roles, e.g. `storage,transcode,gateway,verifier`.
        #[arg(long, value_delimiter = ',')]
        roles: Option<Vec<String>>,
        /// Storage cap (e.g. `500GB`). Stored in config; not parsed here.
        #[arg(long)]
        storage: Option<String>,
        /// Force-enable transcode role even without GPU.
        #[arg(long)]
        gpu: bool,
    },
    /// Print the resolved config and exit.
    Status,
    /// Run diagnostic checks and (optionally) auto-fix.
    Doctor {
        #[arg(long)]
        fix: bool,
        #[arg(long)]
        report: bool,
    },
    /// Read/write a config key.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand, Debug)]
enum ConfigAction {
    Get { key: String },
    Set { key: String, value: String },
    List,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing()?;
    let cli = Cli::parse();
    let config_path = cli.config.unwrap_or_else(config::default_config_path);

    match cli.command {
        TopCmd::Version => {
            println!("depin {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        TopCmd::Video { cmd } => run_video(cmd, &config_path).await,
    }
}

async fn run_video(cmd: VideoCmd, config_path: &std::path::Path) -> Result<()> {
    match cmd {
        VideoCmd::Status => {
            let cfg = Config::load_or_default(config_path)?;
            println!("{}", serde_json::to_string_pretty(&cfg)?);
            Ok(())
        }
        VideoCmd::Doctor { fix, report: _ } => {
            let results = quest_doctor::run().await;
            for r in &results {
                let label = match &r.status {
                    quest_doctor::Status::Ok => "OK   ",
                    quest_doctor::Status::Warn(_) => "WARN ",
                    quest_doctor::Status::Fail(_) => "FAIL ",
                };
                println!("{label} {} — {:?}", r.name, r.status);
            }
            if fix {
                quest_doctor::fix(&results).await?;
            }
            Ok(())
        }
        VideoCmd::Config { action } => handle_config(config_path, action),
        VideoCmd::Start {
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

fn handle_config(path: &std::path::Path, action: ConfigAction) -> Result<()> {
    let mut cfg = Config::load_or_default(path)?;
    match action {
        ConfigAction::List => {
            println!("{}", toml::to_string_pretty(&cfg)?);
        }
        ConfigAction::Get { key } => {
            let value = lookup_key(&cfg, &key);
            match value {
                Some(v) => println!("{v}"),
                None => {
                    eprintln!("unknown key: {key}");
                    std::process::exit(2);
                }
            }
        }
        ConfigAction::Set { key, value } => {
            set_key(&mut cfg, &key, &value)?;
            cfg.save(path)?;
            println!("saved {} -> {}", key, path.display());
        }
    }
    Ok(())
}

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

fn lookup_key(cfg: &Config, key: &str) -> Option<String> {
    Some(match key {
        "api.base_url" => cfg.api.base_url.clone(),
        "api.token" => cfg.api.token.clone().unwrap_or_default(),
        "storage.root" => cfg.storage.root.display().to_string(),
        "gateway.bind" => cfg.gateway.bind.clone(),
        "update.channel" => cfg.update_channel.clone(),
        "update.auto" => cfg.update_auto.to_string(),
        _ => return None,
    })
}

fn set_key(cfg: &mut Config, key: &str, value: &str) -> Result<()> {
    match key {
        "api.base_url" => cfg.api.base_url = value.into(),
        "api.token" => cfg.api.token = Some(value.into()),
        "storage.root" => cfg.storage.root = value.into(),
        "gateway.bind" => cfg.gateway.bind = value.into(),
        "update.channel" => cfg.update_channel = value.into(),
        "update.auto" => cfg.update_auto = value.parse()?,
        other => anyhow::bail!("unknown config key: {other}"),
    }
    Ok(())
}
