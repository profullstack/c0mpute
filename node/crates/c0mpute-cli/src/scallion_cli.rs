//! Scallion client capability diagnostics. No proxy is started here.
//!
//! Users can choose native (direct c0mpute app) or OpenVPN Connect
//! compatibility, but *neither* client mode becomes usable until a
//! security-reviewed encrypted data plane and adapter is actually built.

use anyhow::Result;
use clap::{Subcommand, ValueEnum};
use c0mpute_net::scallion_clients::{ClientMode, current_readiness};

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
        #[arg(long, value_enum, default_value_t = ModeArg::Native)]
        mode: ModeArg,
        /// Print machine-readable status.
        #[arg(long)]
        json: bool,
    },
}

pub fn run(cmd: ScallionCmd) -> Result<()> {
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
}
