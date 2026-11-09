//! `c0mpute gameserver start|ls|stop`: rent a Minecraft, Factorio or Valheim
//! server on a c0mpute worker for a few hours, and keep the world.

use std::time::Duration;

use anyhow::{Context, Result};
use c0mpute_core::gameserver::{self, Game, StartRequest};
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "c0mpute gameserver", about = "Rent a game server (Minecraft, Factorio, Valheim) on a c0mpute worker")]
struct GameserverCli {
    #[command(subcommand)]
    cmd: Sub,
}

#[derive(Subcommand, Debug)]
enum Sub {
    /// Start a server; prints the address players connect to.
    Start {
        /// minecraft, factorio or valheim.
        game: String,
        /// Reservation length; the worker stops the server (and backs the world up) when it ends.
        #[arg(long, default_value_t = 2)]
        hours: u32,
        /// RAM limit, e.g. 4g (whole GB).
        #[arg(long, default_value = "4g")]
        ram: String,
        #[arg(long, default_value_t = 2.0)]
        cpus: f64,
        /// Minecraft: you accept the Minecraft EULA (https://aka.ms/MinecraftEULA).
        #[arg(long)]
        accept_eula: bool,
        /// Restore a world: the hash `gameserver stop` printed, or a .tar of the game's data dir.
        #[arg(long)]
        world: Option<String>,
        /// Valheim server password (5-32 letters/digits; peers on the job topic can see it).
        #[arg(long)]
        password: Option<String>,
        /// Server name / MOTD.
        #[arg(long)]
        name: Option<String>,
        /// Most you pay for the whole reservation, USD.
        #[arg(long, default_value_t = 1.0)]
        max_price: f64,
        /// Only accept bids from these worker peer ids. Repeatable.
        #[arg(long)]
        worker: Vec<String>,
        #[arg(long, default_value_t = 900)]
        timeout_secs: u64,
        #[arg(long)]
        json: bool,
    },
    /// List the servers rented from this machine.
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// Stop a server early (or collect an expired one's backup) and download its world.
    Stop {
        id: String,
        #[arg(long, default_value_t = 600)]
        timeout_secs: u64,
        #[arg(long)]
        json: bool,
    },
}

/// "4g", "4G", "4gb" or "4" -> 4.
fn parse_ram_gb(s: &str) -> Result<u32> {
    let t = s.trim().to_ascii_lowercase();
    let n = t.trim_end_matches("gb").trim_end_matches('g');
    n.parse::<u32>().with_context(|| format!("--ram {s:?}: use whole gigabytes, e.g. 4g"))
}

fn left(expires_at: u64) -> String {
    let now = gameserver::now_secs();
    if now >= expires_at {
        return "expired".into();
    }
    let s = expires_at - now;
    format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
}

pub async fn run(args: &[String]) -> Result<()> {
    let cli = GameserverCli::try_parse_from(std::iter::once("c0mpute gameserver".to_string()).chain(args.iter().cloned())).unwrap_or_else(|e| e.exit());
    match cli.cmd {
        Sub::Start { game, hours, ram, cpus, accept_eula, world, password, name, max_price, worker, timeout_secs, json } => {
            let game = Game::parse(&game).with_context(|| format!("unknown game {game:?}: minecraft, factorio or valheim"))?;
            let req = StartRequest {
                game,
                hours,
                ram_gb: parse_ram_gb(&ram)?,
                cpus,
                accept_eula,
                world,
                password,
                server_name: name,
                max_price_usd: max_price,
                workers: worker,
                timeout: Duration::from_secs(timeout_secs.clamp(60, 3600)),
            };
            eprintln!("looking for a worker that hosts {} ...", game.name());
            let r = gameserver::start(req).await?;
            if json {
                println!("{}", serde_json::to_string(&r)?);
            } else {
                println!("{} {} is up at {}", r.game.name(), r.id, r.address);
                println!("  worker {}  ${}  stops in {}", r.worker, r.price_usd, left(r.expires_at));
                println!("  stop early (and keep the world): c0mpute gameserver stop {}", r.id);
            }
            Ok(())
        }
        Sub::Ls { json } => {
            let all = gameserver::load_rentals(&gameserver::buyer_dir());
            if json {
                println!("{}", serde_json::to_string(&all)?);
                return Ok(());
            }
            if all.is_empty() {
                println!("no servers rented from this machine");
            }
            for r in all {
                let status = match (r.stopped_at, &r.backup) {
                    (Some(_), Some(b)) => format!("stopped  world {b}"),
                    (Some(_), None) => "stopped".into(),
                    (None, _) => left(r.expires_at),
                };
                println!("{:20} {:10} {:28} {}", r.id, r.game.name(), r.address, status);
            }
            Ok(())
        }
        Sub::Stop { id, timeout_secs, json } => {
            let r = gameserver::stop(&id, Duration::from_secs(timeout_secs.clamp(30, 3600))).await?;
            if json {
                println!("{}", serde_json::to_string(&r)?);
            } else if let Some(b) = &r.backup {
                println!("{} stopped; world saved to {}", r.id, gameserver::buyer_dir().join("worlds").join(format!("{b}.tar")).display());
                println!("  restore it anywhere: c0mpute gameserver start {} --world {b}", r.game.name());
            } else {
                println!("{} stopped (no world backup)", r.id);
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ram_flag() {
        assert_eq!(parse_ram_gb("4g").unwrap(), 4);
        assert_eq!(parse_ram_gb("8GB").unwrap(), 8);
        assert_eq!(parse_ram_gb("2").unwrap(), 2);
        assert!(parse_ram_gb("1.5g").is_err());
        assert!(parse_ram_gb("4m").is_err());
    }
}
