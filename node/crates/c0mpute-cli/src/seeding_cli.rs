//! `c0mpute seeding`: seed the torrents c0mpute publishes.
//!
//! Off by default. `on` / `off` write `[seeding]` in the config; `status`
//! shows what this node would do and why; `list` shows what a running worker
//! seeds and how much disk it uses. For whoever publishes the list, `entry`
//! prints a manifest entry read from a real `.torrent` file and `verify`
//! checks a signed manifest against the pinned release key before it goes up.
//!
//! There is deliberately no `add`: a node seeds the signed list and the
//! `.torrent` files its operator lists in `[seeding] local_torrents`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use c0mpute_core::Config;
use c0mpute_core::seeding_node::{
    self as node, STATUS_PATH, SeedingEnv, WHAT_IT_SEEDS, human_bytes,
};
use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum SeedingCmd {
    /// Whether this node seeds, within which limits and why, plus what a
    /// running worker reports.
    Status {
        /// Print machine-readable status.
        #[arg(long)]
        json: bool,
        /// The local worker's gateway, asked what it seeds.
        #[arg(long, default_value = "http://127.0.0.1:7777")]
        gateway: String,
    },
    /// Opt in: seed the torrents c0mpute publishes (a signed list this node
    /// verifies), within the disk cap. Costs disk and upstream bandwidth.
    On {
        /// Most disk seeding may use: bytes, or a size like 20GiB or 500MiB.
        #[arg(long)]
        max_bytes: Option<String>,
        /// Upload cap in KiB per second (0: no cap).
        #[arg(long)]
        max_upload_kib: Option<u32>,
        /// TCP port peers connect to (0: any free port).
        #[arg(long)]
        port: Option<u16>,
    },
    /// Stop seeding (the default). Data already downloaded stays on disk
    /// until you delete the seeding directory.
    Off,
    /// What a running worker seeds, and how much disk it uses.
    #[command(alias = "ls")]
    List {
        /// Print machine-readable output.
        #[arg(long)]
        json: bool,
        /// The local worker's gateway.
        #[arg(long, default_value = "http://127.0.0.1:7777")]
        gateway: String,
    },
    /// Print the manifest entry for a .torrent file (info hash, name and
    /// size read from the file), for whoever publishes the list. Paste the
    /// output into the manifest's `torrents`, then sign it with
    /// scripts/sign-seeding-manifest.sh. See docs/seeding.md.
    Entry {
        /// The .torrent file of something we publish.
        torrent: PathBuf,
        /// The license that lets nodes redistribute it (an SPDX id such as
        /// GPL-2.0-only or CC-BY-4.0, or a short statement).
        #[arg(long)]
        license: String,
        /// The https page where we publish it.
        #[arg(long)]
        source: String,
        /// A tracker URL (udp, http or https); repeat for more. There is no
        /// DHT, so list at least one.
        #[arg(long = "tracker")]
        trackers: Vec<String>,
    },
    /// Check a manifest file and its minisign signature the way a node
    /// does, against the release key pinned in this binary. For whoever
    /// publishes the list; see docs/seeding.md.
    Verify {
        /// The manifest JSON file.
        manifest: PathBuf,
        /// Its signature. Default: `<manifest>.minisig`.
        #[arg(long)]
        sig: Option<PathBuf>,
    },
}

const RESTART_HINT: &str = "restart the worker to apply it (a running worker keeps what it started with).";

/// A size: plain bytes, or a number with KiB/MiB/GiB/TiB (KB/MB/GB/TB and
/// K/M/G/T mean the same binary units).
pub fn parse_size(raw: &str) -> Result<u64> {
    let s = raw.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (number, unit) = s.split_at(split);
    let shift = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 0,
        "k" | "kb" | "kib" => 10,
        "m" | "mb" | "mib" => 20,
        "g" | "gb" | "gib" => 30,
        "t" | "tb" | "tib" => 40,
        other => anyhow::bail!("'{other}' is not a size unit (use KiB, MiB, GiB or TiB)"),
    };
    if shift == 0 {
        return number
            .parse::<u64>()
            .with_context(|| format!("'{raw}' is not a size"));
    }
    let value: f64 = number
        .parse()
        .with_context(|| format!("'{raw}' is not a size"))?;
    let bytes = value * (1u64 << shift) as f64;
    ensure!(
        value.is_finite() && value >= 0.0 && bytes < u64::MAX as f64,
        "'{raw}' is not a size"
    );
    Ok(bytes as u64)
}

/// The manifest entry for a `.torrent` file, checked the way a node checks it.
pub fn manifest_entry(
    torrent: &Path,
    license: String,
    source: String,
    trackers: Vec<String>,
) -> Result<c0mpute_seeding::Entry> {
    let bytes = std::fs::read(torrent).with_context(|| format!("read {}", torrent.display()))?;
    let info = c0mpute_seeding::inspect_torrent(&bytes)?;
    let mut entry = c0mpute_seeding::Entry {
        info_hash: info.info_hash,
        name: info.name,
        bytes: info.bytes,
        license,
        source,
        trackers,
    };
    c0mpute_seeding::manifest::check_entry(&mut entry).context("a node would refuse this entry")?;
    Ok(entry)
}

async fn fetch_status(gateway: &str) -> Result<serde_json::Value> {
    let url = format!("{}{}", gateway.trim_end_matches('/'), STATUS_PATH);
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let res = http
        .get(&url)
        .send()
        .await
        .with_context(|| format!("reach the local worker at {url}"))?;
    ensure!(res.status().is_success(), "{url} answered {}", res.status());
    res.json().await.with_context(|| format!("read {url}"))
}

fn resolved_for(cfg: &Config) -> node::Resolved {
    let private = cfg.private.enabled || c0mpute_core::private::env_forces_private();
    cfg.seeding
        .resolve(&SeedingEnv::from_process(), private, &cfg.storage.root)
}

fn print_torrents(status: &serde_json::Value) {
    let torrents = status["torrents"].as_array().cloned().unwrap_or_default();
    for t in &torrents {
        println!(
            "{}  {:<11} {:>10} of {:<10} up {:<10} {}  [{}]",
            t["info_hash"].as_str().unwrap_or("?"),
            t["state"].as_str().unwrap_or("?"),
            human_bytes(t["held_bytes"].as_u64().unwrap_or(0)),
            human_bytes(t["bytes"].as_u64().unwrap_or(0)),
            human_bytes(t["uploaded_bytes"].as_u64().unwrap_or(0)),
            t["name"].as_str().unwrap_or(""),
            t["origin"].as_str().unwrap_or("?"),
        );
        if let Some(e) = t["error"].as_str() {
            println!("    {e}");
        }
    }
    let skipped = status["skipped"].as_array().cloned().unwrap_or_default();
    for s in &skipped {
        println!(
            "skipped  {}  {}  {}: {}",
            s["info_hash"].as_str().filter(|h| !h.is_empty()).unwrap_or("-"),
            human_bytes(s["bytes"].as_u64().unwrap_or(0)),
            s["name"].as_str().unwrap_or(""),
            s["reason"].as_str().unwrap_or(""),
        );
    }
    println!(
        "{} torrent(s), {} on disk of a {} cap ({} usable now); {} skipped.",
        torrents.len(),
        human_bytes(status["held_bytes"].as_u64().unwrap_or(0)),
        human_bytes(status["max_bytes"].as_u64().unwrap_or(0)),
        human_bytes(status["cap_bytes_now"].as_u64().unwrap_or(0)),
        skipped.len(),
    );
}

fn print_list_source(status: &serde_json::Value) {
    let list = &status["list"];
    match list["from"].as_str().unwrap_or("none") {
        "network" => println!(
            "  published list: #{} with {} torrent(s), verified (fetched from {})",
            list["seq"],
            list["torrents"],
            list["url"].as_str().unwrap_or("?")
        ),
        "cache" => println!(
            "  published list: #{} with {} torrent(s), the last verified copy ({})",
            list["seq"],
            list["torrents"],
            list["error"].as_str().unwrap_or("the URL is unreachable")
        ),
        _ => println!(
            "  published list: none verified yet ({}); only your local torrents are seeded",
            list["error"].as_str().unwrap_or("not fetched yet")
        ),
    }
    if let Some(e) = status["engine_error"].as_str() {
        println!("  engine: not running ({e})");
    }
}

pub async fn run(cmd: SeedingCmd, config_path: &Path) -> Result<()> {
    match cmd {
        SeedingCmd::Status { json, gateway } => {
            let cfg = Config::load_or_default(config_path)?;
            let resolved = resolved_for(&cfg);
            let worker = fetch_status(&gateway).await;
            if json {
                let mut out = node::status_json(&resolved);
                out["config_path"] = serde_json::json!(config_path);
                out["config"] = serde_json::to_value(&cfg.seeding)?;
                out["worker"] = match &worker {
                    Ok(w) => serde_json::json!({ "reachable": true, "seeding": w }),
                    Err(e) => serde_json::json!({ "reachable": false, "error": format!("{e:#}") }),
                };
                println!("{}", serde_json::to_string_pretty(&out)?);
                return Ok(());
            }
            println!("Seeding");
            if resolved.enabled {
                println!("  on   (from {})", resolved.source);
                println!("  disk cap: {}", human_bytes(resolved.max_bytes));
                match resolved.max_upload_kib_per_sec {
                    0 => println!("  upload cap: none"),
                    n => println!("  upload cap: {n} KiB/s"),
                }
                println!("  TCP port: {}", resolved.port);
                println!("  data: {}", resolved.dir.display());
                println!("  list: {} (signed, checked against the pinned release key)", resolved.manifest_url);
                if !cfg.seeding.only.is_empty() {
                    println!("  only: {}", cfg.seeding.only.join(", "));
                }
                if !cfg.seeding.skip.is_empty() {
                    println!("  skip: {}", cfg.seeding.skip.join(", "));
                }
                for p in &cfg.seeding.local_torrents {
                    println!("  local torrent: {}", p.display());
                }
            } else {
                println!("  off   (from {}; opt in with `c0mpute seeding on`)", resolved.source);
            }
            for w in &resolved.warnings {
                println!("  warning: {w}");
            }
            println!("  config: {}", config_path.display());
            match &worker {
                Ok(w) if w["enabled"] == true => {
                    println!(
                        "  running worker: seeding, {} on disk (`c0mpute seeding list`)",
                        human_bytes(w["held_bytes"].as_u64().unwrap_or(0))
                    );
                    print_list_source(w);
                }
                Ok(_) => println!("  running worker: not seeding"),
                Err(_) => println!(
                    "  running worker: not reachable at {gateway} (not running, no gateway role, or an older version); the lines above are what it would do from this config and environment"
                ),
            }
            println!("{WHAT_IT_SEEDS}");
        }
        SeedingCmd::On { max_bytes, max_upload_kib, port } => {
            let mut cfg = Config::load_or_default(config_path)?;
            if let Some(raw) = max_bytes {
                cfg.seeding.max_bytes = parse_size(&raw).context("--max-bytes")?;
            }
            if let Some(n) = max_upload_kib {
                cfg.seeding.max_upload_kib_per_sec = n;
            }
            if let Some(p) = port {
                cfg.seeding.port = p;
            }
            cfg.seeding.enabled = true;
            cfg.save(config_path)?;
            let resolved = resolved_for(&cfg);
            println!(
                "seeding on: this node will seed the torrents c0mpute publishes, using up to {} of disk.",
                human_bytes(cfg.seeding.max_bytes)
            );
            for w in &resolved.warnings {
                println!("warning: {w}");
            }
            if !resolved.enabled {
                println!(
                    "note: the environment or private mode overrides the config here ({}); seeding is effectively off.",
                    resolved.source
                );
            }
            println!("{RESTART_HINT}");
            println!("{WHAT_IT_SEEDS}");
        }
        SeedingCmd::Off => {
            let mut cfg = Config::load_or_default(config_path)?;
            cfg.seeding.enabled = false;
            cfg.save(config_path)?;
            let resolved = resolved_for(&cfg);
            println!("seeding off: this node will not seed.");
            if resolved.enabled {
                println!(
                    "note: {} is set in the environment and turns seeding on regardless.",
                    resolved.source
                );
            }
            println!("data already downloaded stays in {} until you delete it.", resolved.dir.display());
            println!("{RESTART_HINT}");
        }
        SeedingCmd::List { json, gateway } => {
            let status = fetch_status(&gateway).await.context(
                "the list of seeded torrents lives in a running worker with the gateway role (`c0mpute worker start`)",
            )?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
                return Ok(());
            }
            if status["enabled"] != true {
                println!("this worker is not seeding (opt in with `c0mpute seeding on`, then restart it).");
                return Ok(());
            }
            print_list_source(&status);
            print_torrents(&status);
        }
        SeedingCmd::Entry {
            torrent,
            license,
            source,
            trackers,
        } => {
            let entry = manifest_entry(&torrent, license, source, trackers)?;
            if entry.trackers.is_empty() {
                eprintln!(
                    "warning: no --tracker given; with no DHT, only peers that already know a seeding node will find it"
                );
            }
            println!("{}", serde_json::to_string_pretty(&entry)?);
        }
        SeedingCmd::Verify { manifest, sig } => {
            let sig = sig.unwrap_or_else(|| {
                let mut p = manifest.clone().into_os_string();
                p.push(".minisig");
                PathBuf::from(p)
            });
            let bytes = std::fs::read(&manifest).with_context(|| format!("read {}", manifest.display()))?;
            let sig_text = std::fs::read_to_string(&sig).with_context(|| format!("read {}", sig.display()))?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let m = c0mpute_seeding::manifest::verify(c0mpute_seeding::PINNED_PUBKEY, &bytes, &sig_text, now)
                .context("a node would refuse this manifest")?;
            println!(
                "ok: manifest #{} verifies against the pinned release key; {} torrent(s), {} in all.",
                m.seq,
                m.torrents.len(),
                human_bytes(m.torrents.iter().map(|t| t.bytes).sum())
            );
            for t in &m.torrents {
                println!("{}  {:>10}  {}  ({})", t.info_hash, human_bytes(t.bytes), t.name, t.license);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser, Debug)]
    struct TestRoot {
        #[command(subcommand)]
        command: SeedingCmd,
    }

    #[test]
    fn the_commands_parse() {
        let parse = |args: &[&str]| {
            TestRoot::try_parse_from(std::iter::once("c0mpute").chain(args.iter().copied()))
        };
        assert!(matches!(parse(&["status"]).unwrap().command, SeedingCmd::Status { json: false, .. }));
        assert!(matches!(parse(&["status", "--json"]).unwrap().command, SeedingCmd::Status { json: true, .. }));
        assert!(matches!(parse(&["off"]).unwrap().command, SeedingCmd::Off));
        assert!(matches!(parse(&["list"]).unwrap().command, SeedingCmd::List { .. }));
        assert!(matches!(parse(&["ls", "--json"]).unwrap().command, SeedingCmd::List { json: true, .. }));
        match parse(&["on", "--max-bytes", "5GiB", "--max-upload-kib", "512", "--port", "0"]).unwrap().command {
            SeedingCmd::On { max_bytes, max_upload_kib, port } => {
                assert_eq!(max_bytes.as_deref(), Some("5GiB"));
                assert_eq!(max_upload_kib, Some(512));
                assert_eq!(port, Some(0));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(parse(&["verify", "m.json"]).unwrap().command, SeedingCmd::Verify { sig: None, .. }));
        match parse(&["entry", "x.torrent", "--license", "GPL-2.0-only", "--source", "https://example.org/", "--tracker", "udp://t.example.org:1337/announce", "--tracker", "https://t.example.org/announce"]).unwrap().command {
            SeedingCmd::Entry { trackers, license, .. } => {
                assert_eq!(trackers.len(), 2);
                assert_eq!(license, "GPL-2.0-only");
            }
            other => panic!("{other:?}"),
        }
        // An entry needs a license and a source page.
        assert!(parse(&["entry", "x.torrent", "--source", "https://example.org/"]).is_err());
        assert!(parse(&["entry", "x.torrent", "--license", "CC0-1.0"]).is_err());
        // There is no way to hand the node a torrent from the command line.
        for args in [&["add", "magnet:?xt=urn:btih:aaaa"][..], &["add"], &["magnet:?xt=urn:btih:aaaa"]] {
            assert!(parse(args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn sizes_parse() {
        assert_eq!(parse_size("0").unwrap(), 0);
        assert_eq!(parse_size("1048576").unwrap(), 1 << 20);
        assert_eq!(parse_size("20GiB").unwrap(), 20 << 30);
        assert_eq!(parse_size("20 gb").unwrap(), 20 << 30);
        assert_eq!(parse_size("500M").unwrap(), 500 << 20);
        assert_eq!(parse_size("1.5TiB").unwrap(), 3 << 39);
        for bad in ["", "GiB", "ten", "5 parsecs", "-1", "1.5"] {
            assert!(parse_size(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn an_entry_is_read_from_a_real_torrent_file_only() {
        let dir = tempfile::tempdir().unwrap();
        let junk = dir.path().join("junk.torrent");
        std::fs::write(&junk, "magnet:?xt=urn:btih:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        let err = manifest_entry(&junk, "CC0-1.0".into(), "https://example.org/".into(), vec![]).unwrap_err();
        assert!(format!("{err:#}").contains("torrent"), "{err:#}");
        assert!(manifest_entry(&dir.path().join("missing.torrent"), "CC0-1.0".into(), "https://example.org/".into(), vec![]).is_err());
    }

    #[test]
    fn a_manifest_not_signed_by_the_release_key_fails_verify() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("seeding-manifest.json");
        std::fs::write(
            &manifest,
            r#"{"kind":"c0mpute.seeding.manifest","v":1,"seq":1,"issued_at":1,"torrents":[]}"#,
        )
        .unwrap();
        // The c0mpute-update test signature: valid minisign, wrong key and file.
        std::fs::write(
            dir.path().join("seeding-manifest.json.minisig"),
            "untrusted comment: signature from minisign secret key\nRUSgKx30JhkH5uRp7VRolVR4bM+n44NjjP5/duvM3raMiSDyRlUhTyEYAKVmuWzEBxhLEQE8VKRybfYLgQ+53uUy38iJOMrkrwg=\ntrusted comment: timestamp:1 version:v9.9.9 file:c0mpute-linux-x86_64.tar.gz\nBlqf7m00l6vWH+cZyF7MfEWS3yzPB72bD9SD4FKBHNxraBa9P0juzgFm7uK6VFEpq3Fn0wr21tqB4o9lMbP2DA==\n",
        )
        .unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let err = rt
            .block_on(run(SeedingCmd::Verify { manifest, sig: None }, &dir.path().join("config.toml")))
            .unwrap_err();
        assert!(format!("{err:#}").contains("refuse"), "{err:#}");
    }
}
