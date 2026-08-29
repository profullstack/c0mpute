//! `c0mpute storage` — the storage sub-feature of the umbrella CLI.
//!
//! Declared by `plugins/storage/module.toml` (`cli = "c0mpute storage"`) and
//! specified in `docs/prds/009-mount-cli.md`. This is the CIP-002 slice of
//! that surface: objects, shards and the local node's storage service.
//!
//! Volume, mount and provider commands arrive with their CIPs — see
//! [`unimplemented_note`] for what maps to what. They are deliberately absent
//! rather than stubbed, so `--help` never advertises something that does not
//! work.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use c0mpute_core::config;
use c0mpute_gateway::auth::AllowAll;
use c0mpute_gateway::storage_api::{self, Limits, StorageApiState};
use c0mpute_proto::Hash;
use c0mpute_store::{ChunkStore, Storage, Tier};
use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum StorageCmd {
    /// Store a file and print its object hash.
    Put {
        file: PathBuf,
        /// Redundancy tier: hot, standard (default) or critical.
        #[arg(long, default_value = "standard")]
        tier: String,
    },
    /// Fetch an object by hash.
    Get {
        hash: String,
        /// Write to this path instead of stdout.
        #[arg(short = 'o', long)]
        out: Option<PathBuf>,
        /// Read only this byte range, e.g. `1000-2047`.
        #[arg(long)]
        range: Option<String>,
    },
    /// List objects held on this node.
    Ls {
        /// Print one hash per line with no header.
        #[arg(long)]
        quiet: bool,
    },
    /// Show an object's block and shard layout.
    Info { hash: String },
    /// Re-read an object and verify every block against its hash.
    Verify { hash: String },
    /// Delete an object and its shards.
    Rm {
        hash: String,
        #[arg(long)]
        yes: bool,
    },
    /// Disk usage, budget, and per-tier redundancy.
    Status,
    /// Show the tier table: redundancy, durability and price.
    Tiers,
    /// Run the storage HTTP API on this node.
    Serve {
        #[arg(long, default_value = "127.0.0.1:7780")]
        bind: String,
        /// Accept unauthenticated writes. Only for a node that is not
        /// reachable from the network.
        #[arg(long)]
        insecure_allow_anonymous_writes: bool,
    },
}

/// Where the local shard store lives.
fn storage_root(config_path: &std::path::Path) -> Result<PathBuf> {
    let cfg = config::Config::load_or_default(config_path)?;
    Ok(cfg.storage.root)
}

async fn open(config_path: &std::path::Path) -> Result<Storage> {
    let root = storage_root(config_path)?;
    let store = ChunkStore::open(&root)
        .await
        .with_context(|| format!("open chunk store at {}", root.display()))?;
    Ok(Storage::new(store))
}

fn parse_hash(raw: &str) -> Result<Hash> {
    let hex = raw.strip_prefix("blake3:").unwrap_or(raw);
    Hash::from_hex(hex).map_err(|_| anyhow::anyhow!("`{raw}` is not a blake3 hash"))
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

pub async fn run(cmd: StorageCmd, config_path: &std::path::Path) -> Result<()> {
    match cmd {
        StorageCmd::Put { file, tier } => put(config_path, file, &tier).await,
        StorageCmd::Get { hash, out, range } => get(config_path, &hash, out, range).await,
        StorageCmd::Ls { quiet } => ls(config_path, quiet).await,
        StorageCmd::Info { hash } => info(config_path, &hash).await,
        StorageCmd::Verify { hash } => verify(config_path, &hash).await,
        StorageCmd::Rm { hash, yes } => rm(config_path, &hash, yes).await,
        StorageCmd::Status => status(config_path).await,
        StorageCmd::Tiers => {
            tiers();
            Ok(())
        }
        StorageCmd::Serve {
            bind,
            insecure_allow_anonymous_writes,
        } => serve(config_path, &bind, insecure_allow_anonymous_writes).await,
    }
}

async fn put(config_path: &std::path::Path, file: PathBuf, tier: &str) -> Result<()> {
    let tier: Tier = tier.parse()?;
    let storage = open(config_path).await?;

    let bytes = tokio::fs::read(&file)
        .await
        .with_context(|| format!("read {}", file.display()))?;
    let len = bytes.len() as u64;

    let manifest = storage.put_tiered(&bytes, tier).await?;
    println!("blake3:{}", manifest.object_hash.to_hex());
    eprintln!(
        "  {} in {} block(s), {} shards, tier {} ({:.1}x expansion, {} raw)",
        human(len),
        manifest.blocks.len(),
        manifest.shard_count(),
        tier,
        tier.expansion(),
        human((len as f64 * tier.expansion()).ceil() as u64),
    );
    Ok(())
}

async fn get(
    config_path: &std::path::Path,
    hash: &str,
    out: Option<PathBuf>,
    range: Option<String>,
) -> Result<()> {
    let hash = parse_hash(hash)?;
    let storage = open(config_path).await?;
    if !storage.has(&hash).await {
        bail!("no object {hash} on this node");
    }

    let bytes = match range {
        Some(spec) => {
            let (start, end) = spec
                .split_once('-')
                .context("range must look like `START-END`")?;
            let start: u64 = start.parse().context("range start")?;
            let end: u64 = end.parse().context("range end")?;
            if end < start {
                bail!("range end {end} is before start {start}");
            }
            storage.get_range(&hash, start, end - start + 1).await?
        }
        None => storage.get(&hash).await?,
    };

    match out {
        Some(path) => {
            tokio::fs::write(&path, &bytes).await?;
            eprintln!("wrote {} to {}", human(bytes.len() as u64), path.display());
        }
        None => {
            use std::io::Write;
            std::io::stdout().write_all(&bytes)?;
        }
    }
    Ok(())
}

async fn ls(config_path: &std::path::Path, quiet: bool) -> Result<()> {
    let storage = open(config_path).await?;
    let objects = storage.list().await?;
    if objects.is_empty() && !quiet {
        println!("no objects on this node");
        return Ok(());
    }
    if !quiet {
        println!(
            "{:<66}  {:>10}  {:>8}  {}",
            "OBJECT", "SIZE", "TIER", "BLOCKS"
        );
    }
    for hash in objects {
        if quiet {
            println!("blake3:{}", hash.to_hex());
            continue;
        }
        match storage.read_manifest(&hash).await {
            Ok(m) => println!(
                "blake3:{}  {:>10}  {:>8}  {}",
                hash.to_hex(),
                human(m.original_len),
                m.tier.to_string(),
                m.blocks.len()
            ),
            Err(e) => println!("blake3:{}  <unreadable manifest: {e}>", hash.to_hex()),
        }
    }
    Ok(())
}

async fn info(config_path: &std::path::Path, hash: &str) -> Result<()> {
    let hash = parse_hash(hash)?;
    let storage = open(config_path).await?;
    let m = storage.read_manifest(&hash).await?;

    println!("object    blake3:{}", m.object_hash.to_hex());
    println!(
        "size      {} ({} bytes)",
        human(m.original_len),
        m.original_len
    );
    println!(
        "tier      {} — RS {}/{}, {:.1}x expansion, tolerates {} shard losses",
        m.tier,
        m.k,
        m.n(),
        m.tier.expansion(),
        m.parity
    );
    println!(
        "blocks    {} of {}",
        m.blocks.len(),
        human(m.block_size as u64)
    );
    println!("shards    {}", m.shard_count());
    println!("manifest  v{}", m.version);

    let mut healthy = 0usize;
    let mut missing = 0usize;
    for block in &m.blocks {
        for shard in &block.shards {
            if storage.chunk_store().has(&shard.hash).await {
                healthy += 1;
            } else {
                missing += 1;
            }
        }
    }
    let state = if missing == 0 {
        "healthy"
    } else if missing <= m.parity as usize {
        "degraded (readable)"
    } else {
        "LOST"
    };
    println!("health    {healthy} present, {missing} missing — {state}");
    if missing > 0 {
        println!("          repair is CIP-005; on a single node there is nowhere to repair from");
    }
    Ok(())
}

async fn verify(config_path: &std::path::Path, hash: &str) -> Result<()> {
    let hash = parse_hash(hash)?;
    let storage = open(config_path).await?;
    let m = storage.read_manifest(&hash).await?;

    let mut bad = Vec::new();
    for i in 0..m.blocks.len() {
        if let Err(e) = storage.read_block(&m, i).await {
            bad.push(format!("block {i}: {e}"));
        }
    }
    if bad.is_empty() {
        println!(
            "ok — {} block(s) of blake3:{} verified against their hashes",
            m.blocks.len(),
            m.object_hash.to_hex()
        );
        Ok(())
    } else {
        for b in &bad {
            eprintln!("FAIL {b}");
        }
        bail!(
            "{} of {} blocks failed verification",
            bad.len(),
            m.blocks.len()
        );
    }
}

async fn rm(config_path: &std::path::Path, hash: &str, yes: bool) -> Result<()> {
    let hash = parse_hash(hash)?;
    let storage = open(config_path).await?;
    if !storage.has(&hash).await {
        bail!("no object {hash} on this node");
    }
    if !yes {
        let m = storage.read_manifest(&hash).await?;
        eprintln!(
            "about to delete blake3:{} ({}, {} shards)",
            hash.to_hex(),
            human(m.original_len),
            m.shard_count()
        );
        eprintln!("re-run with --yes to confirm");
        return Ok(());
    }
    storage.delete(&hash).await?;
    println!("deleted blake3:{}", hash.to_hex());
    Ok(())
}

async fn status(config_path: &std::path::Path) -> Result<()> {
    let root = storage_root(config_path)?;
    let storage = open(config_path).await?;
    let cfg = config::Config::load_or_default(config_path)?;
    let used = storage_api::DiskBudget::scan(&root);
    let objects = storage.list().await?;

    println!("root      {}", root.display());
    println!("objects   {}", objects.len());
    println!(
        "used      {} of {}",
        human(used),
        cfg.storage
            .cap_bytes
            .map(human)
            .unwrap_or_else(|| "uncapped".into())
    );

    let mut logical = 0u64;
    let mut degraded = 0usize;
    for h in &objects {
        if let Ok(m) = storage.read_manifest(h).await {
            logical += m.original_len;
            for block in &m.blocks {
                let mut present = 0;
                for s in &block.shards {
                    if storage.chunk_store().has(&s.hash).await {
                        present += 1;
                    }
                }
                if present < block.shards.len() {
                    degraded += 1;
                }
            }
        }
    }
    println!(
        "logical   {} across {} objects",
        human(logical),
        objects.len()
    );
    if degraded > 0 {
        println!("degraded  {degraded} block(s) missing at least one shard");
    }
    println!();
    println!("single-node: every shard is on this disk, so the erasure coding is");
    println!("overhead without durability until cross-node placement (CIP-003).");
    Ok(())
}

fn tiers() {
    println!(
        "{:<10} {:<10} {:>9} {:>10} {:>9}  {}",
        "TIER", "SCHEME", "EXPANSION", "TOLERATES", "$/GB-mo", "BEST FOR"
    );
    for (tier, scheme, best) in [
        (Tier::Hot, "3-copy", "small hot files, metadata"),
        (Tier::Standard, "RS 10/14", "default; bulk data, media"),
        (Tier::Critical, "RS 20/32", "irreplaceable, long retention"),
    ] {
        println!(
            "{:<10} {:<10} {:>8.1}x {:>10} {:>9}  {}",
            tier.to_string(),
            scheme,
            tier.expansion(),
            format!("{} lost", tier.parity()),
            format!("${}", tier.price_usd_per_gb_month()),
            best
        );
    }
    println!();
    println!("Expansion is the cost of goods: raw GB paid for per usable GB sold.");
    println!("See docs/prds/001-storage-program.md for the durability arithmetic.");
}

async fn serve(config_path: &std::path::Path, bind: &str, allow_anon: bool) -> Result<()> {
    let cfg = config::Config::load_or_default(config_path)?;
    let storage = open(config_path).await?;
    let addr: std::net::SocketAddr = bind.parse().context("--bind must be HOST:PORT")?;

    if !allow_anon {
        bail!(
            "refusing to serve without an auth keyring.\n\
             Signed-request verification needs CoinPay DID resolution, which lands\n\
             with CIP-006. For a node that is not reachable from the network, pass\n\
             --insecure-allow-anonymous-writes."
        );
    }

    let limits = Limits {
        disk_budget_bytes: cfg.storage.cap_bytes,
        ..Limits::default()
    };
    let state = StorageApiState::new(storage, Arc::new(AllowAll), limits);
    let app = storage_api::router(state);

    eprintln!("storage API on http://{addr}  (anonymous writes ALLOWED)");
    eprintln!("  PUT  /storage/v1/objects/{{hash}}");
    eprintln!("  GET  /storage/v1/objects/{{hash}}   (supports Range)");
    eprintln!("  GET  /storage/v1/status");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// Which CIP delivers each command that is not here yet. Referenced from the
/// long help so the gap is discoverable without reading `docs/prds/`.
pub fn unimplemented_note() -> &'static str {
    "Not yet available:\n  \
     volume create|list|destroy   CIP-004\n  \
     mount | umount               CIP-009 (needs CIP-007 and CIP-008)\n  \
     provide | earnings | retire  CIP-006\n  \
     repair | recover             CIP-005\n\
     \nSee docs/prds/README.md for the delivery order."
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_sizes_read_sensibly() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(512), "512 B");
        assert_eq!(human(1024), "1.0 KiB");
        assert_eq!(human(1024 * 1024 * 3 / 2), "1.5 MiB");
        assert_eq!(human(1u64 << 40), "1.0 TiB");
    }

    #[test]
    fn hashes_parse_with_or_without_the_scheme() {
        let h = Hash::of(b"x");
        assert_eq!(parse_hash(&h.to_hex()).unwrap(), h);
        assert_eq!(parse_hash(&format!("blake3:{}", h.to_hex())).unwrap(), h);
        assert!(parse_hash("nope").is_err());
    }

    #[test]
    fn unimplemented_note_points_at_real_cips() {
        let note = unimplemented_note();
        for cip in ["CIP-004", "CIP-005", "CIP-006", "CIP-007", "CIP-009"] {
            assert!(note.contains(cip), "note should mention {cip}");
        }
    }
}
