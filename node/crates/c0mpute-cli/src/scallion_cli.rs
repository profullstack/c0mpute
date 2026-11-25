//! `c0mpute scallion`: client-mode diagnostics and relay participation.
//!
//! No proxy is started here and no traffic is carried. Users can choose
//! native (direct c0mpute app) or OpenVPN Connect compatibility, but
//! *neither* client mode becomes usable until a security-reviewed encrypted
//! data plane and adapter is actually built.
//!
//! What a node can do today is ADVERTISE relay roles: `relay on|off` and
//! `exit on|off` set what a worker says it would do (guard and middle by
//! default, exit only when the operator opts in), and `status` / `relays`
//! show that and the verified descriptors a running worker has heard.
//! Nothing is relayed yet.

use std::path::Path;

use anyhow::{Context, Result, ensure};
use c0mpute_core::Config;
use c0mpute_core::scallion_node::{self as node, NOT_CARRYING, RELAYS_PATH, ScallionEnv};
use c0mpute_net::scallion_clients::{ClientMode, current_readiness};
use c0mpute_net::scallion_participation::Participation;
use clap::{Subcommand, ValueEnum};

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum ModeArg {
    Native,
    Openvpn,
}

impl From<ModeArg> for ClientMode {
    fn from(value: ModeArg) -> Self {
        match value {
            ModeArg::Native => ClientMode::Native,
            ModeArg::Openvpn => ClientMode::OpenVpn,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum OnOff {
    On,
    Off,
}

#[derive(Subcommand, Debug)]
pub enum ExitCmd {
    /// Opt in: advertise this node as an exit to the listed ports as well as
    /// guard and middle. Advertising only; no traffic leaves this node.
    On {
        /// TCP ports to advertise, comma-separated. Default: the ports
        /// already in the config (80,443 unless changed).
        #[arg(long, value_delimiter = ',')]
        ports: Option<Vec<u16>>,
    },
    /// Stop advertising the exit role (the default).
    Off,
}

#[derive(Subcommand, Debug)]
pub enum ScallionCmd {
    /// Report both supported client modes and their actual readiness.
    Modes {
        /// Print machine-readable status.
        #[arg(long)]
        json: bool,
    },
    /// Diagnose a single client mode without starting a tunnel.
    Doctor {
        /// Select native Scallion or OpenVPN compatibility.
        #[arg(long, value_enum, default_value = "native")]
        mode: ModeArg,
        /// Print machine-readable status.
        #[arg(long)]
        json: bool,
    },
    /// Which relay roles this node advertises (guard, middle, exit) and why,
    /// plus what a running worker reports. Nothing is relayed yet.
    Status {
        /// Print machine-readable status.
        #[arg(long)]
        json: bool,
        /// The local worker's gateway, asked what it advertises and knows.
        #[arg(long, default_value = "http://127.0.0.1:7777")]
        gateway: String,
    },
    /// Turn relay participation on (the default) or off. On: the worker
    /// advertises itself as a guard and middle relay. Off: it advertises no
    /// Scallion role at all, exit included. Advertising only.
    Relay {
        #[arg(value_enum)]
        state: OnOff,
    },
    /// Opt in or out of advertising this node as an exit. Off by default
    /// and never turned on by anything but the operator.
    Exit {
        #[command(subcommand)]
        cmd: ExitCmd,
    },
    /// List the relays a running worker has heard advertise, with the roles
    /// in their verified descriptors. A list of advertisements: no circuit
    /// is built through them.
    Relays {
        /// Print machine-readable output.
        #[arg(long)]
        json: bool,
        /// The local worker's gateway.
        #[arg(long, default_value = "http://127.0.0.1:7777")]
        gateway: String,
    },
}

const RESTART_HINT: &str = "restart the worker to apply it (a running worker keeps advertising what it started with).";

async fn fetch_relays(gateway: &str) -> Result<serde_json::Value> {
    let url = format!("{}{}", gateway.trim_end_matches('/'), RELAYS_PATH);
    let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(5)).build()?;
    let res = http.get(&url).send().await.with_context(|| format!("reach the local worker at {url}"))?;
    ensure!(res.status().is_success(), "{url} answered {}", res.status());
    res.json().await.with_context(|| format!("read {url}"))
}

fn list_of(value: &serde_json::Value) -> String {
    let items: Vec<String> = value
        .as_array()
        .map(|a| a.iter().map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())).collect())
        .unwrap_or_default();
    if items.is_empty() { "none".to_string() } else { items.join(",") }
}

/// The exit opt-in on a config: `ports` (or the ones already configured) go
/// through the same checks the worker applies, so a bad list is refused
/// here instead of silently leaving exit off. Nothing changes on an error.
fn exit_on(cfg: &mut Config, ports: Option<Vec<u16>>) -> Result<Participation> {
    let ports = ports.unwrap_or_else(|| cfg.scallion.exit_ports.clone());
    let checked = Participation::with_exit(ports).context("exit ports")?;
    cfg.scallion.exit = true;
    cfg.scallion.exit_ports = checked.exit_ports().to_vec();
    Ok(checked)
}

fn resolved_for(cfg: &Config) -> node::Resolved {
    let private = cfg.private.enabled || c0mpute_core::private::env_forces_private();
    cfg.scallion.resolve(&ScallionEnv::from_process(), private)
}

pub async fn run(cmd: ScallionCmd, config_path: &Path) -> Result<()> {
    match cmd {
        ScallionCmd::Modes { json } => {
            let modes = [
                current_readiness(ClientMode::Native),
                current_readiness(ClientMode::OpenVpn),
            ];
            if json {
                println!("{}", serde_json::to_string_pretty(&modes)?);
            } else {
                for status in modes {
                    println!(
                        "{}: {}",
                        status.mode,
                        if status.ready { "ready" } else { "not ready" }
                    );
                    for blocker in status.blockers {
                        println!("  - {blocker}");
                    }
                }
            }
        }
        ScallionCmd::Doctor { mode, json } => {
            let status = current_readiness(mode.into());
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                println!(
                    "Scallion {}: {}",
                    status.mode,
                    if status.ready { "ready" } else { "not ready" }
                );
                for blocker in status.blockers {
                    println!("  - {blocker}");
                }
            }
        }
        ScallionCmd::Status { json, gateway } => {
            let cfg = Config::load_or_default(config_path)?;
            let resolved = resolved_for(&cfg);
            let worker = fetch_relays(&gateway).await;
            if json {
                let mut out = node::status_json(&resolved);
                out["config_path"] = serde_json::json!(config_path);
                out["config"] = serde_json::to_value(&cfg.scallion)?;
                out["worker"] = match &worker {
                    Ok(w) => serde_json::json!({
                        "reachable": true,
                        "self": w.get("self"),
                        "known_relays": w.get("relays").and_then(|r| r.as_array()).map(|a| a.len()),
                    }),
                    Err(e) => serde_json::json!({ "reachable": false, "error": format!("{e:#}") }),
                };
                println!("{}", serde_json::to_string_pretty(&out)?);
                return Ok(());
            }
            let p = &resolved.participation;
            println!("Scallion relay participation (advertise only)");
            println!(
                "  relay: {}   (from {})",
                if p.relays() { "on, advertises guard + middle" } else { "off, advertises no relay role" },
                resolved.relay_source
            );
            if p.exits() {
                println!(
                    "  exit:  on, advertises exit to ports {}   (from {})",
                    p.exit_ports().iter().map(u16::to_string).collect::<Vec<_>>().join(","),
                    resolved.exit_source
                );
            } else {
                println!("  exit:  off   (from {}; opt in with `c0mpute scallion exit on`)", resolved.exit_source);
            }
            if !resolved.operator.is_empty() {
                println!("  operator label: {}", resolved.operator);
            }
            for w in &resolved.warnings {
                println!("  warning: {w}");
            }
            println!("  config: {}", config_path.display());
            match &worker {
                Ok(w) => {
                    match w.get("self").filter(|s| !s.is_null()) {
                        Some(me) => println!(
                            "  running worker: advertises {} (exit ports: {})",
                            list_of(&me["roles"]),
                            list_of(&me["exit_ports"])
                        ),
                        None => println!("  running worker: advertises no relay role"),
                    }
                    let known = w.get("relays").and_then(|r| r.as_array()).map(|a| a.len()).unwrap_or(0);
                    println!("  relays heard from peers: {known} (`c0mpute scallion relays`)");
                }
                Err(_) => println!(
                    "  running worker: not reachable at {gateway} (not running, no gateway role, or an older version); the lines above are what it would advertise from this config and environment"
                ),
            }
            println!("{NOT_CARRYING}");
        }
        ScallionCmd::Relay { state } => {
            let mut cfg = Config::load_or_default(config_path)?;
            cfg.scallion.relay = state == OnOff::On;
            cfg.save(config_path)?;
            match state {
                OnOff::On => println!("relay on: this node will advertise itself as a guard and middle relay. Exit stays as configured ({}).", if cfg.scallion.exit { "on" } else { "off" }),
                OnOff::Off => println!("relay off: this node will advertise no Scallion role, exit included."),
            }
            for w in resolved_for(&cfg).warnings {
                println!("warning: {w}");
            }
            println!("{RESTART_HINT}");
            println!("{NOT_CARRYING}");
        }
        ScallionCmd::Exit { cmd } => {
            let mut cfg = Config::load_or_default(config_path)?;
            match cmd {
                ExitCmd::On { ports } => {
                    let checked = exit_on(&mut cfg, ports)?;
                    cfg.save(config_path)?;
                    println!(
                        "exit on: this node will advertise itself as an exit to ports {}, as well as guard and middle.",
                        checked.exit_ports().iter().map(u16::to_string).collect::<Vec<_>>().join(",")
                    );
                    if !cfg.scallion.relay {
                        println!("note: relay participation is off, so nothing is advertised until `c0mpute scallion relay on`.");
                    }
                }
                ExitCmd::Off => {
                    cfg.scallion.exit = false;
                    cfg.save(config_path)?;
                    println!("exit off: this node will not advertise the exit role.");
                }
            }
            let resolved = resolved_for(&cfg);
            for w in &resolved.warnings {
                println!("warning: {w}");
            }
            if resolved.participation.exits() != cfg.scallion.exit {
                println!(
                    "note: the environment or private mode overrides the config here ({}); effective exit is {}.",
                    resolved.exit_source,
                    if resolved.participation.exits() { "on" } else { "off" }
                );
            }
            println!("{RESTART_HINT}");
            println!("{NOT_CARRYING}");
        }
        ScallionCmd::Relays { json, gateway } => {
            let listing = fetch_relays(&gateway).await.context(
                "the relay list lives in a running worker with the gateway role (`c0mpute worker start`)",
            )?;
            if json {
                println!("{}", serde_json::to_string_pretty(&listing)?);
                return Ok(());
            }
            match listing.get("self").filter(|s| !s.is_null()) {
                Some(me) => println!(
                    "this node  {}  advertises {} (exit ports: {})",
                    me["peer_id"].as_str().unwrap_or("?"),
                    list_of(&me["roles"]),
                    list_of(&me["exit_ports"])
                ),
                None => println!("this node advertises no relay role"),
            }
            let relays = listing.get("relays").and_then(|r| r.as_array()).cloned().unwrap_or_default();
            if relays.is_empty() {
                println!("no relay descriptors heard from peers yet (ads arrive about once a minute)");
            }
            for r in &relays {
                let operator = r["operator"].as_str().filter(|o| !o.is_empty());
                println!(
                    "{}  {}  exit ports: {}{}",
                    r["peer_id"].as_str().unwrap_or("?"),
                    list_of(&r["roles"]),
                    list_of(&r["exit_ports"]),
                    operator.map(|o| format!("  operator: {o}")).unwrap_or_default()
                );
            }
            println!("{} relay(s) advertised by peers, descriptors verified.", relays.len());
            println!("{NOT_CARRYING}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{Parser, Subcommand};

    #[derive(Parser, Debug)]
    struct TestRoot {
        #[command(subcommand)]
        command: ScallionCmd,
    }

    #[test]
    fn both_modes_have_distinct_cli_commands() {
        assert!(TestRoot::try_parse_from(["scallion", "modes"]).is_ok());
        assert!(TestRoot::try_parse_from(["scallion", "doctor", "--mode", "native"]).is_ok());
        assert!(TestRoot::try_parse_from(["scallion", "doctor", "--mode", "openvpn"]).is_ok());
        assert!(TestRoot::try_parse_from(["scallion", "doctor", "--mode", "other"]).is_err());
    }

    #[test]
    fn diagnostics_are_false_until_real_transports_exist() {
        assert!(!current_readiness(ClientMode::Native).ready);
        assert!(!current_readiness(ClientMode::OpenVpn).ready);
    }

    #[test]
    fn participation_commands_parse() {
        for args in [
            vec!["scallion", "status"],
            vec!["scallion", "status", "--json"],
            vec!["scallion", "relay", "on"],
            vec!["scallion", "relay", "off"],
            vec!["scallion", "exit", "on"],
            vec!["scallion", "exit", "on", "--ports", "80,443"],
            vec!["scallion", "exit", "off"],
            vec!["scallion", "relays", "--json"],
        ] {
            assert!(TestRoot::try_parse_from(args.clone()).is_ok(), "{args:?}");
        }
        assert!(TestRoot::try_parse_from(["scallion", "relay", "maybe"]).is_err());
        assert!(TestRoot::try_parse_from(["scallion", "exit"]).is_err());
        assert!(TestRoot::try_parse_from(["scallion", "exit", "on", "--ports", "http"]).is_err());
        match TestRoot::try_parse_from(["scallion", "exit", "on", "--ports", "443,8443"]).unwrap().command {
            ScallionCmd::Exit { cmd: ExitCmd::On { ports } } => assert_eq!(ports, Some(vec![443, 8443])),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_exit_switch_is_explicit_and_checks_its_ports() {
        let mut cfg = Config::default();
        // Untouched config: relay on, exit off.
        assert!(cfg.scallion.relay && !cfg.scallion.exit);

        exit_on(&mut cfg, None).unwrap();
        assert!(cfg.scallion.exit);
        assert_eq!(cfg.scallion.exit_ports, [80, 443]);

        exit_on(&mut cfg, Some(vec![8443, 443, 443])).unwrap();
        assert_eq!(cfg.scallion.exit_ports, [443, 8443]);

        // A bad port list is refused and changes nothing.
        assert!(exit_on(&mut cfg, Some(vec![0])).is_err());
        assert!(exit_on(&mut cfg, Some(vec![])).is_err());
        assert_eq!(cfg.scallion.exit_ports, [443, 8443]);

        cfg.scallion.exit = false;
        let mut fresh = Config::default();
        fresh.scallion.exit_ports = vec![];
        assert!(exit_on(&mut fresh, None).is_err());
        assert!(!fresh.scallion.exit, "a refused opt-in leaves exit off");
    }
}
