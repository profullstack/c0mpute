//! `c0mpute storage` — the storage sub-feature of the umbrella CLI.
//!
//! Declared by `plugins/storage/module.toml` (`cli = "c0mpute storage"`) and
//! specified in `docs/prds/009-mount-cli.md`. This is the CIP-002 slice of
//! that surface: objects, shards and the local node's storage service.
//!
//! Objects are private by default: `put` encrypts on this machine before
//! anything leaves it and publishes only ciphertext (DIP-0012 `private`
//! tier, see `private_storage.rs`). `--public` stores plaintext, as before.
//!
//! Provider commands arrive with their CIPs; see [`unimplemented_note`].
//! They are deliberately absent rather than stubbed, so `--help` never
//! advertises something that does not work.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use c0mpute_core::config;
use c0mpute_gateway::auth::AllowAll;
use c0mpute_gateway::storage_api::{self, Limits, StorageApiState};
use c0mpute_placement::{DistributedStorage, HttpTransport, PlacementPolicy};
use c0mpute_proto::Hash;
use c0mpute_store::{ChunkStore, Storage, Tier};
use clap::Subcommand;
use tokio::sync::RwLock;

use crate::peers;
use crate::private_storage;
use std::time::Duration;

#[derive(Subcommand, Debug)]
pub enum StorageCmd {
    /// Store a file. Private by default: encrypted on this machine, then
    /// published to the network as ciphertext (three self-healing copies).
    /// Prints a `c0p1:` reference that only you can open.
    ///
    /// --public stores plaintext instead (content-addressed, readable by
    /// anyone with the hash): placed across storage peers when configured
    /// (`c0mpute storage peer add`), otherwise on this node alone.
    Put {
        file: PathBuf,
        /// Store plaintext: anyone with the hash can read it. Only for data
        /// that is meant to be public.
        #[arg(long)]
        public: bool,
        /// Also let these storage identities (`c0ps1...`) open it. Repeatable.
        #[arg(long = "share-with")]
        share_with: Vec<String>,
        /// First use only: where to write your encrypted key backup.
        #[arg(long)]
        backup_to: Option<PathBuf>,
        /// Gateway used to read objects back.
        #[arg(long, default_value = c0mpute_core::sealed_store::DEFAULT_GATEWAY)]
        gateway: String,
        /// Give up waiting for network copies after this many seconds.
        #[arg(long, default_value_t = 600)]
        wait_secs: u64,
        /// Redundancy tier: hot, standard (default) or critical.
        #[arg(long, default_value = "standard")]
        tier: String,
        /// Keep it on this node: a private object's ciphertext, or with
        /// --public every shard, even when peers are configured.
        #[arg(long)]
        local: bool,
        /// Place without requiring failure-domain diversity.
        ///
        /// CIP-001's durability figures assume shard hosts fail
        /// independently. Fourteen shards behind one ISP are one host wearing
        /// fourteen hats, and nothing downstream can tell the difference.
        /// Testnets only.
        #[arg(long)]
        insecure_ignore_diversity: bool,
    },
    /// Storage peers this node knows about.
    Peer {
        #[command(subcommand)]
        cmd: PeerCmd,
    },
    /// Fetch an object: `c0p1:...` (private, decrypted here), a private
    /// link (`...#k=...`), or `blake3:...` (on this node or its peers).
    Get {
        #[arg(value_name = "REF")]
        hash: String,
        /// Write to this path (a directory for a private site) instead of stdout.
        #[arg(short = 'o', long)]
        out: Option<PathBuf>,
        /// Read only this byte range, e.g. `1000-2047` (blake3 objects).
        #[arg(long)]
        range: Option<String>,
        /// Write the ciphertext exactly as operators hold it; do not decrypt.
        #[arg(long)]
        raw: bool,
        #[arg(long, default_value = c0mpute_core::sealed_store::DEFAULT_GATEWAY)]
        gateway: String,
    },
    /// Let more storage identities open a private object (you must own it).
    Share {
        #[arg(value_name = "REF")]
        reference: String,
        /// Their storage identity (`c0ps1...`, from `c0mpute storage key show`). Repeatable.
        #[arg(long = "with", required = true)]
        with: Vec<String>,
        #[arg(long, default_value = c0mpute_core::sealed_store::DEFAULT_GATEWAY)]
        gateway: String,
        #[arg(long, default_value_t = 180)]
        wait_secs: u64,
    },
    /// Stop identities reading a private object: re-encrypts it under a new
    /// key, which also kills every link given out so far.
    Revoke {
        #[arg(value_name = "REF")]
        reference: String,
        /// Storage identities to remove (`c0ps1...`). Omit to just rotate the key.
        #[arg(long = "from")]
        from: Vec<String>,
        #[arg(long, default_value = c0mpute_core::sealed_store::DEFAULT_GATEWAY)]
        gateway: String,
        #[arg(long, default_value_t = 600)]
        wait_secs: u64,
    },
    /// Print a link that opens a private object for anyone who has it (in a
    /// browser, or with `storage get`). The key rides in the # fragment.
    Link {
        #[arg(value_name = "REF")]
        reference: String,
        #[arg(long, default_value = c0mpute_core::sealed_store::DEFAULT_GATEWAY)]
        gateway: String,
    },
    /// Your c0mpute identity: create it, show your storage identity, back it up.
    Key {
        #[command(subcommand)]
        cmd: crate::private_storage::KeyCmd,
    },
    /// Mount an encrypted volume: files written here are stored as private
    /// objects (Linux, FUSE).
    #[cfg(all(target_os = "linux", feature = "fuse"))]
    Mount {
        dir: PathBuf,
        /// Volume name (each name is a separate volume).
        #[arg(long, default_value = "default")]
        volume: String,
        #[arg(long, default_value = c0mpute_core::sealed_store::DEFAULT_GATEWAY)]
        gateway: String,
    },
    /// List objects held on this node.
    Ls {
        /// Print one hash per line with no header.
        #[arg(long)]
        quiet: bool,
        /// List your private objects (from this machine's encrypted index).
        #[arg(long)]
        private: bool,
    },
    /// Show an object's block and shard layout.
    Info { hash: String },
    /// Re-read an object and verify every block against its hash.
    Verify { hash: String },
    /// Rebuild shards lost to departed peers (CIP-005).
    Repair {
        /// Repair one object. Omit to sweep every object on this node.
        hash: Option<String>,
        /// Report what would be repaired without changing anything.
        #[arg(long)]
        dry_run: bool,
        /// Treat an unreachable peer as gone immediately, skipping the grace
        /// window. The window exists so a rebooting node is not repaired away;
        /// override it only when you know a peer is really gone.
        #[arg(long)]
        now: bool,
    },
    /// Delete an object: `blake3:...` from this node, or `c0p1:...` from
    /// the network (its holders drop the ciphertext).
    Rm {
        #[arg(value_name = "REF")]
        hash: String,
        #[arg(long)]
        yes: bool,
        #[arg(long, default_value = c0mpute_core::sealed_store::DEFAULT_GATEWAY)]
        gateway: String,
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

#[derive(Subcommand, Debug)]
pub enum PeerCmd {
    /// Register a storage peer.
    Add {
        peer_id: String,
        /// Base URL of the peer's storage API, e.g. http://10.0.0.2:7780
        endpoint: String,
        /// Autonomous system number. Without it the failure domain falls back
        /// to the peer's IP prefix, or is unknown for a DNS name — and
        /// unknown-domain peers are excluded from placement.
        #[arg(long)]
        asn: Option<u32>,
        #[arg(long)]
        region: Option<String>,
    },
    /// List known peers and the failure domains they span.
    Ls,
    /// Forget a peer.
    Rm { peer_id: String },
    /// Check which peers are reachable right now.
    Ping,
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
        StorageCmd::Put {
            file,
            public,
            share_with,
            backup_to,
            gateway,
            wait_secs,
            tier,
            local,
            insecure_ignore_diversity,
        } => {
            if public {
                if !share_with.is_empty() {
                    bail!("--share-with is for private objects; a --public object is readable by anyone");
                }
                put(config_path, file, &tier, local, insecure_ignore_diversity, None).await
            } else if local {
                put(config_path, file, &tier, true, insecure_ignore_diversity, Some(backup_to)).await
            } else {
                private_storage::put(&file, &share_with, &gateway, Duration::from_secs(wait_secs), backup_to.as_deref()).await
            }
        }
        StorageCmd::Peer { cmd } => peer(config_path, cmd).await,
        StorageCmd::Get { hash, out, range, raw, gateway } => {
            if is_private_ref(&hash) {
                if range.is_some() {
                    bail!("--range reads plaintext objects; private objects are fetched whole and decrypted");
                }
                private_storage::get(&hash, out.as_deref(), raw, &gateway).await
            } else {
                get(config_path, &hash, out, range, raw).await
            }
        }
        StorageCmd::Share { reference, with, gateway, wait_secs } => {
            private_storage::share(&reference, &with, &gateway, Duration::from_secs(wait_secs)).await
        }
        StorageCmd::Revoke { reference, from, gateway, wait_secs } => {
            private_storage::revoke(&reference, &from, &gateway, Duration::from_secs(wait_secs)).await
        }
        StorageCmd::Link { reference, gateway } => private_storage::link(&reference, &gateway).await,
        StorageCmd::Key { cmd } => private_storage::key(cmd),
        #[cfg(all(target_os = "linux", feature = "fuse"))]
        StorageCmd::Mount { dir, volume, gateway } => crate::mount::run(&dir, &volume, &gateway).await,
        StorageCmd::Ls { quiet, private } => {
            if private {
                private_storage::ls()
            } else {
                ls(config_path, quiet).await
            }
        }
        StorageCmd::Info { hash } => info(config_path, &hash).await,
        StorageCmd::Verify { hash } => verify(config_path, &hash).await,
        StorageCmd::Repair { hash, dry_run, now } => {
            repair(config_path, hash.as_deref(), dry_run, now).await
        }
        StorageCmd::Rm { hash, yes, gateway } => {
            if is_private_ref(&hash) {
                private_storage::rm(&hash, yes, &gateway).await
            } else {
                rm(config_path, &hash, yes).await
            }
        }
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

/// Open a distributed view of this node's storage, if peers are configured.
async fn open_distributed(
    config_path: &std::path::Path,
    ignore_diversity: bool,
) -> Result<Option<(DistributedStorage, usize, usize)>> {
    let root = storage_root(config_path)?;
    let catalog = peers::load(&root)?;
    if catalog.is_empty() {
        return Ok(None);
    }
    let peer_count = catalog.len();
    let domains = catalog.domain_count();

    let mut config = c0mpute_placement::DistributedConfig::default();
    if ignore_diversity {
        config.policy = Some(PlacementPolicy {
            max_per_domain: usize::MAX,
            allow_unknown_domain: true,
            ..PlacementPolicy::for_parity(4)
        });
    }

    let storage = DistributedStorage::new(
        open(config_path).await?,
        Arc::new(HttpTransport::default()),
        Arc::new(RwLock::new(catalog)),
    )
    .with_config(config);
    Ok(Some((storage, peer_count, domains)))
}

/// `c0p1:...`, a private link, or a bare 20-hex site id (a blake3 hash is
/// 64 hex, so the two never collide).
fn is_private_ref(raw: &str) -> bool {
    let raw = raw.trim();
    raw.starts_with(c0mpute_core::sealed_store::REF_PREFIX)
        || raw.contains("/p/")
        || (raw.len() == 20 && raw.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Store a file in this node's own engine. `seal` is `Some(backup_to)` for a
/// private `--local` object: the file is encrypted first and only the
/// ciphertext is stored.
async fn put(
    config_path: &std::path::Path,
    file: PathBuf,
    tier: &str,
    force_local: bool,
    ignore_diversity: bool,
    seal: Option<Option<PathBuf>>,
) -> Result<()> {
    let tier: Tier = tier.parse()?;
    let mut bytes = tokio::fs::read(&file)
        .await
        .with_context(|| format!("read {}", file.display()))?;
    if let Some(backup_to) = &seal {
        bytes = private_storage::seal_local(&bytes, &file, backup_to.as_deref())?;
    }
    let len = bytes.len() as u64;

    if ignore_diversity {
        eprintln!(
            "warning: --insecure-ignore-diversity — shards may all land in one\n\
             failure domain, which makes the durability claim meaningless."
        );
    }

    let distributed = if force_local {
        None
    } else {
        open_distributed(config_path, ignore_diversity).await?
    };

    let manifest = match &distributed {
        Some((storage, peer_count, domains)) => {
            eprintln!("  placing across {peer_count} peer(s) in {domains} failure domain(s)");
            storage.put(&bytes, tier).await?
        }
        None => open(config_path).await?.put_tiered(&bytes, tier).await?,
    };

    println!("blake3:{}", manifest.object_hash.to_hex());
    if seal.is_some() {
        eprintln!("  private: this node holds only ciphertext; `storage get` decrypts it with your identity");
    }
    eprintln!(
        "  {} in {} block(s), {} shards, tier {} ({:.1}x expansion, {} raw)",
        human(len),
        manifest.blocks.len(),
        manifest.shard_count(),
        tier,
        tier.expansion(),
        human((len as f64 * tier.expansion()).ceil() as u64),
    );
    if distributed.is_none() {
        eprintln!(
            "  single-node: every shard is on this disk, so the erasure coding is\n  \
             overhead without durability. Add peers with `c0mpute storage peer add`."
        );
    }
    Ok(())
}

async fn peer(config_path: &std::path::Path, cmd: PeerCmd) -> Result<()> {
    let root = storage_root(config_path)?;
    let mut catalog = peers::load(&root)?;

    match cmd {
        PeerCmd::Add {
            peer_id,
            endpoint,
            asn,
            region,
        } => {
            let info = peers::build(peer_id.clone(), endpoint.clone(), asn, region);
            let domain = info.domain();
            catalog.upsert(info);
            peers::save(&root, &catalog)?;
            println!("added {peer_id} at {endpoint}");
            println!("  failure domain: {domain:?}");
            if matches!(domain, c0mpute_placement::FailureDomain::Unknown) {
                eprintln!(
                    "  warning: unknown failure domain — this peer will be skipped by\n  \
                     placement. Pass --asn, or use an endpoint with a literal IP."
                );
            }
            report_capacity(&catalog);
        }
        PeerCmd::Ls => {
            if catalog.is_empty() {
                println!("no storage peers configured");
                return Ok(());
            }
            println!(
                "{:<20} {:<32} {:<24} {}",
                "PEER", "ENDPOINT", "DOMAIN", "FREE"
            );
            for p in catalog.peers() {
                println!(
                    "{:<20} {:<32} {:<24} {}",
                    p.peer_id,
                    p.endpoint,
                    format!("{:?}", p.domain()),
                    if p.free_bytes == u64::MAX {
                        "unknown".to_string()
                    } else {
                        human(p.free_bytes)
                    }
                );
            }
            report_capacity(&catalog);
        }
        PeerCmd::Rm { peer_id } => {
            catalog.remove(&peer_id);
            peers::save(&root, &catalog)?;
            println!("removed {peer_id}");
            report_capacity(&catalog);
        }
        PeerCmd::Ping => {
            if catalog.is_empty() {
                println!("no storage peers configured");
                return Ok(());
            }
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()?;
            for p in catalog.peers() {
                let url = format!("{}/storage/v1/status", p.endpoint.trim_end_matches('/'));
                let started = std::time::Instant::now();
                match client.get(&url).send().await {
                    Ok(r) if r.status().is_success() => {
                        println!(
                            "{:<20} up    {:>5} ms",
                            p.peer_id,
                            started.elapsed().as_millis()
                        )
                    }
                    Ok(r) => println!("{:<20} HTTP {}", p.peer_id, r.status()),
                    Err(e) => println!("{:<20} down  {e}", p.peer_id),
                }
            }
        }
    }
    Ok(())
}

/// Say plainly whether this network can place at each tier.
///
/// The peer count is the number people look at; the domain count is the one
/// that decides whether a placement is durable, so print both together.
fn report_capacity(catalog: &c0mpute_placement::PeerCatalog) {
    let domains = catalog.domain_count();
    println!(
        "\n{} peer(s) across {domains} failure domain(s)",
        catalog.len()
    );
    for tier in [Tier::Hot, Tier::Standard, Tier::Critical] {
        let policy = PlacementPolicy::for_parity(tier.parity());
        let needed = policy.domains_required(tier.n());
        let ok = catalog.len() >= tier.n() && domains >= needed;
        println!(
            "  {:<9} {} — needs {} peers across {needed} domains",
            tier.to_string(),
            if ok { "ready" } else { "NOT ready" },
            tier.n()
        );
    }
}

async fn get(
    config_path: &std::path::Path,
    hash: &str,
    out: Option<PathBuf>,
    range: Option<String>,
    raw: bool,
) -> Result<()> {
    let hash = parse_hash(hash)?;
    let storage = open(config_path).await?;
    if !storage.has(&hash).await {
        bail!("no object {hash} on this node");
    }

    let ranged = range.is_some();
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
            // Range reads are served locally. An object placed across the
            // network has no local shards, so this only works for a
            // single-node object today; distributed range reads land with the
            // filesystem layer (CIP-007), which is what needs them.
            storage.get_range(&hash, start, end - start + 1).await?
        }
        None => {
            // Whether the shards are local or on peers is a property of the
            // object, not of the command. Prefer the distributed path when
            // peers are configured; it falls back to local shards per block,
            // so a single-node object still reads.
            match open_distributed(config_path, false).await? {
                Some((distributed, _, _)) => distributed.get(&hash).await?,
                None => storage.get(&hash).await?,
            }
        }
    };

    // A private object stored with `put --local` decrypts here, on this
    // machine, unless the raw ciphertext was asked for.
    let bytes = if !raw && !ranged && private_storage::is_sealed(&bytes) {
        private_storage::open_local(&bytes)?.to_vec()
    } else {
        bytes
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

    // Where the shards live decides how to check them: probe the peers when
    // the object was placed across the network, the local disk otherwise.
    match open_distributed(config_path, false).await? {
        Some((distributed, _, _)) => {
            let health = distributed.health(&m).await?;
            let placed: Vec<&str> = m.blocks[0]
                .shards
                .iter()
                .filter_map(|s| s.host_hint.as_deref())
                .collect();
            if !placed.is_empty() {
                println!("hosts     {}", placed.join(", "));
            }
            for h in &health {
                println!(
                    "block {:<3} {} of {} shards present — {:?}{}",
                    h.index,
                    h.healthy,
                    h.total,
                    h.state,
                    if h.missing.is_empty() {
                        String::new()
                    } else {
                        format!(" (missing shards {:?})", h.missing)
                    }
                );
            }
            if health.iter().any(|h| h.state.needs_repair()) {
                println!("          repair is CIP-005; not implemented yet");
            }
        }
        None => {
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
                println!(
                    "          repair is CIP-005; on a single node there is nowhere to repair from"
                );
            }
        }
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
    // Anonymous writes on loopback are exactly what a DNS-rebinding page
    // would abuse: answer only local Host names there.
    let local = listener.local_addr().unwrap_or(addr);
    axum::serve(listener, c0mpute_gateway::host_guard::guard_loopback(app, local)).await?;
    Ok(())
}

/// Which CIP delivers each command that is not here yet. Referenced from the
/// long help so the gap is discoverable without reading `docs/prds/`.
pub fn unimplemented_note() -> &'static str {
    "Private by default: `put` encrypts on this machine and publishes only\n\
     ciphertext; `--public` stores plaintext. See docs/private-storage.md.\n\
     \nNot yet available:\n  \
     volume create|list|destroy   CIP-004\n  \
     provide | earnings | retire  CIP-006\n  \
     recover                      CIP-005\n\
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
        for cip in ["CIP-004", "CIP-005", "CIP-006"] {
            assert!(note.contains(cip), "note should mention {cip}");
        }
    }
}

/// Rebuild shards lost to departed peers (CIP-005).
///
/// Explicit rather than elected: an operator running this has asked for the
/// work directly, and is usually not one of the shard holders, so they could
/// never win the rendezvous election that coordinates the background daemon.
async fn repair(
    config_path: &std::path::Path,
    hash: Option<&str>,
    dry_run: bool,
    condemn_now: bool,
) -> Result<()> {
    let root = storage_root(config_path)?;
    let catalog = peers::load(&root)?;
    if catalog.is_empty() {
        bail!(
            "no storage peers configured — there is nowhere to repair from or to.\n\
             Add peers with `c0mpute storage peer add`."
        );
    }
    let storage = open(config_path).await?;
    let local_id = format!("local:{}", root.display());
    let repairer = c0mpute_placement::Repairer::new(
        Arc::new(HttpTransport::default()),
        Arc::new(RwLock::new(catalog)),
        local_id,
    )
    .manual();

    let objects = match hash {
        Some(h) => vec![parse_hash(h)?],
        None => storage.list().await?,
    };
    if objects.is_empty() {
        println!("no objects on this node");
        return Ok(());
    }

    let mut total_repaired = 0usize;
    let mut total_shards = 0usize;
    let mut total_lost = 0usize;

    for object in objects {
        let mut manifest = match storage.read_manifest(&object).await {
            Ok(m) => m,
            Err(e) => {
                eprintln!("blake3:{}: unreadable manifest: {e:#}", object.to_hex());
                continue;
            }
        };

        if dry_run {
            let plans = repairer.scan(&manifest, condemn_now).await?;
            for plan in plans.iter().filter(|p| p.state.needs_repair()) {
                println!(
                    "blake3:{} block {} — {:?}, {} shard(s) missing {:?}",
                    object.to_hex(),
                    plan.block,
                    plan.state,
                    plan.missing.len(),
                    plan.missing
                );
                total_repaired += 1;
            }
            continue;
        }

        let report = repairer
            .repair_object(&mut manifest, 0, condemn_now)
            .await?;
        if report.blocks_repaired > 0 {
            // The manifest now points at the new shard homes, so it has to be
            // written back or the next read still looks for the dead peers.
            storage.write_manifest(&manifest).await?;
            println!(
                "blake3:{} — repaired {} block(s), {} shard(s) regenerated",
                object.to_hex(),
                report.blocks_repaired,
                report.shards_regenerated
            );
        }
        for att in &report.attestations {
            println!(
                "  block {} shards {:?} → {}",
                att.block,
                att.shards_regenerated,
                att.destinations.join(", ")
            );
        }
        if report.blocks_lost > 0 {
            eprintln!(
                "blake3:{} — {} block(s) LOST: fewer than k shards remain, repair cannot help",
                object.to_hex(),
                report.blocks_lost
            );
        }
        for f in &report.failures {
            eprintln!("blake3:{}: {f}", object.to_hex());
        }
        total_repaired += report.blocks_repaired;
        total_shards += report.shards_regenerated;
        total_lost += report.blocks_lost;
    }

    if dry_run {
        println!("\n{total_repaired} block(s) would be repaired (dry run)");
        if total_repaired > 0 {
            println!("re-run without --dry-run to rebuild them");
        }
    } else {
        println!("\n{total_repaired} block(s) repaired, {total_shards} shard(s) regenerated");
        if total_lost > 0 {
            println!("{total_lost} block(s) unrecoverable");
        }
    }
    if !condemn_now && total_repaired == 0 {
        println!(
            "note: peers unreachable for less than the grace window are left alone,\n\
             so a node that is merely rebooting is not repaired away. Use --now to override."
        );
    }
    Ok(())
}
