//! `c0mpute crowdgpt on|off|status|setup|register`: donate idle compute to
//! CrowdGPT. See c0mpute_core::crowdgpt for how it runs.

use std::path::Path;

use anyhow::{Context, Result};
use c0mpute_core::Config;
use c0mpute_core::crowdgpt as cg;
use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum CrowdgptCmd {
    /// Start donating: the CrowdGPT client runs in a container whenever this
    /// node has no paid work, and is stopped the moment a bid or job arrives.
    /// Earns nothing. Pulls (or builds) the runner image if it is missing.
    On {
        /// A CrowdGPT account token to train under (stored 0600 in the
        /// plugin's cache dir, never on a command line). Without one, and
        /// without `c0mpute crowdgpt register`, the client trains anonymously.
        #[arg(long)]
        token: Option<String>,
        /// Also donate while private mode is on.
        #[arg(long)]
        allow_private: bool,
    },
    /// Stop donating (and stop a running client now).
    Off,
    /// What the plugin is doing: state, rounds submitted, tokens trained.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Put the runner image on this node: pull the pinned build, or build it
    /// from CrowdGPT's repository at the pinned commit.
    Setup {
        /// Build from source without trying the registry.
        #[arg(long)]
        build: bool,
        /// cpu (default) or cuda (builds on the PyTorch CUDA image).
        #[arg(long, default_value = "cpu")]
        backend: String,
    },
    /// Create a CrowdGPT account for this node (`c0mpute-<nickname>` by
    /// default) so its contributions are credited by name. Prompts for a
    /// password; the session is saved in the plugin's cache dir.
    Register {
        #[arg(long)]
        email: String,
        /// Account name (default `c0mpute-<node nickname>`).
        #[arg(long)]
        user: Option<String>,
    },
}

const EARNS_NOTHING: &str = "CrowdGPT is a donation: it earns nothing and only uses this node while it has no paid work.";

pub async fn run(cmd: CrowdgptCmd, config_path: &Path) -> Result<()> {
    match cmd {
        CrowdgptCmd::On { token, allow_private } => {
            let mut cfg = Config::load_or_default(config_path)?;
            cfg.crowdgpt.enabled = true;
            if allow_private {
                cfg.crowdgpt.allow_in_private = true;
            }
            cfg.save(config_path)?;
            if let Some(t) = token {
                write_token(t.trim())?;
                println!("token saved to {}", cg::token_path().display());
            }
            println!("{EARNS_NOTHING}");
            if cg::image_commit(cg::RUNNER_IMAGE).as_deref() != Some(cg::CROWDGPT_COMMIT) {
                let how = cg::setup(false, &cfg.crowdgpt.backend, &|l: &str| println!("{l}")).await?;
                println!("runner image ready ({how:?})");
            }
            let ram = c0mpute_core::text::local_ram_mb() / 1024;
            println!(
                "on: up to {:.1} CPUs and {} GB RAM (engram {}), after {}s idle; stopped within {}s of paid work.",
                cfg.crowdgpt.effective_cpus(),
                cfg.crowdgpt.effective_memory_gb(ram),
                if cfg.crowdgpt.engram_on(ram) { "on" } else { "off" },
                cfg.crowdgpt.idle_secs,
                cfg.crowdgpt.stop_grace_secs
            );
            match cg::blocked_reason(&cfg.crowdgpt) {
                Some(why) => println!("not running yet: {why}"),
                None => println!("a running worker picks this up within 10 s (`c0mpute crowdgpt status`)."),
            }
            Ok(())
        }
        CrowdgptCmd::Off => {
            let mut cfg = Config::load_or_default(config_path)?;
            cfg.crowdgpt.enabled = false;
            cfg.save(config_path)?;
            if c0mpute_core::container::which().is_some() {
                cg::stop_all(std::time::Duration::from_secs(cfg.crowdgpt.stop_grace_secs)).await;
            }
            println!("off: no longer donating to CrowdGPT.");
            Ok(())
        }
        CrowdgptCmd::Status { json } => {
            let cfg = Config::load_or_default(config_path)?;
            let st = cg::read_status();
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "enabled": cfg.crowdgpt.enabled,
                        "blocked": cg::blocked_reason(&cfg.crowdgpt),
                        "config": cfg.crowdgpt,
                        "status": st,
                        "crowdgpt_commit": cg::CROWDGPT_COMMIT,
                        "worker_name": cg::worker_name(),
                    }))?
                );
                return Ok(());
            }
            println!("{EARNS_NOTHING}");
            println!("enabled: {}", cfg.crowdgpt.enabled);
            if let Some(why) = cg::blocked_reason(&cfg.crowdgpt) {
                println!("blocked: {why}");
            }
            println!("client: CrowdGPT @ {} ({})", &cg::CROWDGPT_COMMIT[..12], cg::CROWDGPT_REPO);
            println!("credited as: {}", credited_as());
            match st {
                Some(s) => {
                    println!("state: {} ({})", s.state, s.detail);
                    println!(
                        "rounds: {} submitted, {} started, {} lost; {} preempted by paid work",
                        s.rounds_submitted, s.rounds_started, s.rounds_lost, s.preemptions
                    );
                    println!("tokens trained (estimate): {}", s.tokens_trained_estimate);
                    if let (Some(l), Some(t)) = (s.last_loss, s.last_tokens_per_sec) {
                        println!("last step: loss {l:.4}, {t:.0} tokens/s");
                    }
                    if let Some(e) = &s.last_exit {
                        println!("last exit: {e}");
                    }
                    if s.client_outdated {
                        println!("CrowdGPT refused this pinned client: update c0mpute (`c0mpute update`) for a newer pin.");
                    }
                }
                None => println!("state: no worker has run the governor yet (`c0mpute worker start`)"),
            }
            Ok(())
        }
        CrowdgptCmd::Setup { build, backend } => {
            let how = cg::setup(build, &backend, &|l: &str| println!("{l}")).await?;
            println!("runner image ready as {} ({how:?}), CrowdGPT @ {}", cg::RUNNER_IMAGE, &cg::CROWDGPT_COMMIT[..12]);
            if backend == "cuda" {
                println!("set `[crowdgpt] backend = \"cuda\"` in config.toml to run it on the GPU.");
            }
            Ok(())
        }
        CrowdgptCmd::Register { email, user } => {
            let user = user.unwrap_or_else(cg::worker_name);
            let cache = cg::cache_dir();
            std::fs::create_dir_all(cache.join("home"))?;
            println!("registering CrowdGPT account {user} ({email}); the client asks for a password.");
            let status = c0mpute_core::container::command()
                .args(["run", "--rm", "-it", "--read-only", "--tmpfs", "/tmp", "--cap-drop", "ALL", "--security-opt", "no-new-privileges"])
                .args(owner_args(&cache))
                .args(["-v", &format!("{}:/data", cache.display()), cg::RUNNER_IMAGE, "--register", "--user", &user, "--email", &email])
                .status()
                .await
                .context("docker run")?;
            anyhow::ensure!(status.success(), "registration failed ({status})");
            println!("saved; the next round trains as {user}.");
            Ok(())
        }
    }
}

fn owner_args(dir: &Path) -> Vec<String> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(dir).map(|m| vec!["--user".to_string(), format!("{}:{}", m.uid(), m.gid())]).unwrap_or_default()
}

fn credited_as() -> String {
    if cg::token_path().exists() {
        "the CrowdGPT account of the saved token".into()
    } else if cg::cache_dir().join("checkpoints/auth_token.json").exists() {
        "the registered CrowdGPT account".into()
    } else {
        format!(
            "anonymous (CrowdGPT has no worker-name flag; `c0mpute crowdgpt register --email <you>` creates {} so rounds are credited by name)",
            cg::worker_name()
        )
    }
}

fn write_token(token: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    anyhow::ensure!(!token.is_empty() && token.len() < 4096 && !token.chars().any(char::is_whitespace), "that does not look like a token");
    let p = cg::token_path();
    std::fs::create_dir_all(p.parent().unwrap())?;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&p)?;
    f.write_all(token.as_bytes())?;
    Ok(())
}

/// One line for `c0mpute status` / `c0mpute worker status`.
pub fn status_line(config_path: &Path) -> String {
    let enabled = Config::load_or_default(config_path).map(|c| c.crowdgpt.enabled).unwrap_or(false);
    if !enabled {
        return "crowdgpt: off (donate idle compute: `c0mpute crowdgpt on`)".into();
    }
    match cg::read_status() {
        Some(s) => format!("crowdgpt: {}", cg::summary(&s)),
        None => "crowdgpt: on, waiting for the worker".into(),
    }
}
