//! `c0mpute storage volume` — durable metadata (CIP-004).
//!
//! A volume gives a customer one stable name for a mutable dataset, and gives
//! the network somewhere durable to keep manifests. Without it, losing the
//! node that ran a write orphans every shard it described.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use c0mpute_proto::Hash;
use c0mpute_store::ChunkStore;
use c0mpute_volume::{FileAnchor, LocalSink, RootAnchor, Volume, gc};
use clap::Subcommand;
use ed25519_dalek::SigningKey;

#[derive(Subcommand, Debug)]
pub enum VolumeCmd {
    /// Create a volume.
    Create { name: String },
    /// List volumes on this node.
    Ls,
    /// Show a volume's sequence, size and history.
    Info { name: String },
    /// Bind a name to an object hash already in the store.
    Bind {
        volume: String,
        name: String,
        hash: String,
    },
    /// Unbind a name.
    Unbind { volume: String, name: String },
    /// List the names in a volume.
    Cat { volume: String },
    /// Restore a volume to an earlier sequence.
    ///
    /// Recorded as a new root naming the old snapshot, so history stays
    /// append-only and the rollback is itself undoable.
    Rollback {
        volume: String,
        #[arg(long)]
        to: u64,
    },
    /// Reclaim objects no retained root references.
    Gc {
        volume: String,
        /// Report what would be freed without deleting anything.
        #[arg(long)]
        dry_run: bool,
        /// Days an object must be unreferenced before collection.
        #[arg(long, default_value = "14")]
        grace_days: u64,
    },
    /// Rebuild a volume's view from its anchored root alone.
    Recover { name: String },
}

fn anchor_dir(root: &Path) -> PathBuf {
    root.join("volumes")
}

/// The volume signing key.
///
/// Generated on first use and kept beside the store. DIP-0007 makes the
/// CoinPay DID key canonical; this stands in until CIP-006 lands the DID
/// plumbing, and the file it writes is the thing that would be replaced.
fn signing_key(root: &Path) -> Result<(SigningKey, String)> {
    let path = root.join("volume.key");
    if let Ok(bytes) = std::fs::read(&path)
        && bytes.len() == 32
    {
        let arr: [u8; 32] = bytes.as_slice().try_into().unwrap();
        let key = SigningKey::from_bytes(&arr);
        let did = did_for(&key);
        return Ok((key, did));
    }
    // `getrandom` via ed25519-dalek's OsRng would add a dependency for one
    // call; blake3 over OS entropy sources is sufficient for a stand-in key
    // that CIP-006 replaces.
    let mut seed = blake3::Hasher::new();
    seed.update(&std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos().to_be_bytes())
        .unwrap_or([0; 16]));
    seed.update(&std::process::id().to_be_bytes());
    if let Ok(extra) = std::fs::read("/dev/urandom") {
        seed.update(&extra[..extra.len().min(64)]);
    }
    let key = SigningKey::from_bytes(seed.finalize().as_bytes());

    std::fs::create_dir_all(root)?;
    std::fs::write(&path, key.to_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    let did = did_for(&key);
    Ok((key, did))
}

fn did_for(key: &SigningKey) -> String {
    format!(
        "did:c0mpute:{}",
        hex::encode(&key.verifying_key().to_bytes()[..8])
    )
}

async fn open_volume(root: &Path, name: &str) -> Result<Volume<LocalSink, FileAnchor>> {
    let (key, did) = signing_key(root)?;
    let sink = LocalSink::new(ChunkStore::open(root).await?);
    Volume::open(name, sink, FileAnchor::new(anchor_dir(root)), key, did)
        .await
        .with_context(|| format!("open volume {name}"))
}

fn parse_hash(raw: &str) -> Result<Hash> {
    let hex = raw.strip_prefix("blake3:").unwrap_or(raw);
    Hash::from_hex(hex).map_err(|_| anyhow::anyhow!("`{raw}` is not a blake3 hash"))
}

pub async fn run(cmd: VolumeCmd, root: &Path) -> Result<()> {
    match cmd {
        VolumeCmd::Create { name } => {
            let (key, did) = signing_key(root)?;
            let sink = LocalSink::new(ChunkStore::open(root).await?);
            let v = Volume::create(&name, sink, FileAnchor::new(anchor_dir(root)), key, &did)
                .await?;
            println!("created volume {}", v.id());
            println!("  writer   {did}");
            println!("  sequence {}", v.sequence());
            println!("\nEverything below the root pointer is immutable and content-addressed;");
            println!("only the root moves, and it is signed. Keep {}/volume.key safe —", root.display());
            println!("without it the volume cannot be opened or recovered.");
        }
        VolumeCmd::Ls => {
            let names = FileAnchor::new(anchor_dir(root)).list().await?;
            if names.is_empty() {
                println!("no volumes on this node");
                return Ok(());
            }
            println!("{:<24} {:>10} {:>10}", "VOLUME", "SEQUENCE", "ENTRIES");
            for name in names {
                match open_volume(root, &name).await {
                    Ok(v) => println!("{:<24} {:>10} {:>10}", v.id(), v.sequence(), v.len()),
                    Err(e) => println!("{name:<24} <unreadable: {e}>"),
                }
            }
        }
        VolumeCmd::Info { name } => {
            let v = open_volume(root, &name).await?;
            println!("volume    {}", v.id());
            println!("sequence  {}", v.sequence());
            println!("entries   {}", v.len());
            println!(
                "snapshot  {}",
                v.root()
                    .snapshot
                    .map(|h| format!("blake3:{}", h.to_hex()))
                    .unwrap_or_else(|| "<empty>".into())
            );
            println!("writer    {}", v.root().writer_did);
            let hist = v.history().await?;
            println!("history   {} retained root(s)", hist.len());
            for r in hist.iter().take(5) {
                println!(
                    "  seq {:<6} {} entries",
                    r.sequence,
                    r.snapshot.map(|_| "…").unwrap_or("0")
                );
            }
        }
        VolumeCmd::Bind {
            volume,
            name,
            hash,
        } => {
            let object = parse_hash(&hash)?;
            let mut v = open_volume(root, &volume).await?;
            v.put(&name, object).await?;
            println!("{volume}/{name} → blake3:{}", object.to_hex());
            println!("sequence now {}", v.sequence());
        }
        VolumeCmd::Unbind { volume, name } => {
            let mut v = open_volume(root, &volume).await?;
            if v.get(&name).await?.is_none() {
                bail!("{volume} has no entry {name}");
            }
            v.remove(&name).await?;
            println!("removed {volume}/{name}; sequence now {}", v.sequence());
        }
        VolumeCmd::Cat { volume } => {
            let v = open_volume(root, &volume).await?;
            let entries = v.list().await?;
            if entries.is_empty() {
                println!("volume {volume} is empty");
                return Ok(());
            }
            for (name, hash) in entries {
                println!("blake3:{}  {}", hash.to_hex(), name);
            }
        }
        VolumeCmd::Rollback { volume, to } => {
            let mut v = open_volume(root, &volume).await?;
            let from = v.sequence();
            v.rollback(to).await?;
            println!(
                "rolled {volume} back to the state at sequence {to} (was {from}, now {})",
                v.sequence()
            );
            println!("history is append-only, so this rollback is itself undoable");
        }
        VolumeCmd::Gc {
            volume,
            dry_run,
            grace_days,
        } => {
            let v = open_volume(root, &volume).await?;
            let keep = v.keep_set().await?;
            let grace_ms = grace_days * 24 * 3_600_000;

            // Everything this node holds, against everything the volume needs.
            let all = all_chunk_hashes(root);
            let tracker_path = root.join("volumes").join(format!("{volume}.gc.json"));
            let mut tracker: gc::UnreferencedSince = std::fs::read(&tracker_path)
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or_default();

            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let plan = gc::plan(&all, &keep, &mut tracker, now, grace_ms);

            println!("volume     {volume}");
            println!("reachable  {} object(s)", plan.keep.len());
            println!("collect    {} object(s)", plan.collect.len());
            println!(
                "deferred   {} object(s) still inside the {grace_days}-day grace period",
                plan.deferred.len()
            );

            if dry_run {
                println!("\ndry run — nothing deleted");
                return Ok(());
            }

            let store = ChunkStore::open(root).await?;
            let stats = gc::sweep(&plan, |hash| {
                let store = store.clone();
                async move {
                    let size = store.get(&hash).await.map(|b| b.len() as u64).unwrap_or(0);
                    store.delete(&hash).await?;
                    Ok(size)
                }
            })
            .await?;

            std::fs::create_dir_all(root.join("volumes"))?;
            std::fs::write(&tracker_path, serde_json::to_vec(&tracker)?)?;
            println!(
                "\ncollected {} object(s), {} bytes freed",
                stats.collected, stats.bytes_freed
            );
        }
        VolumeCmd::Recover { name } => {
            // The recovery story: with the key and the volume id, everything
            // else is reachable from the anchored root.
            let v = open_volume(root, &name).await?;
            let entries = v.list().await?;
            println!("recovered volume {} at sequence {}", v.id(), v.sequence());
            println!("{} entries:", entries.len());

            let store = ChunkStore::open(root).await?;
            let mut intact = 0usize;
            let mut missing = Vec::new();
            for (entry, hash) in &entries {
                if store.has(hash).await {
                    intact += 1;
                } else {
                    missing.push(entry.clone());
                }
            }
            println!("  {intact} object(s) present on this node");
            if !missing.is_empty() {
                println!("  {} not held here: {:?}", missing.len(), &missing[..missing.len().min(5)]);
                println!("  (placed objects live on peers — this only checks the local store)");
            }
        }
    }
    Ok(())
}

/// Every chunk this node holds. GC compares this against the keep set.
fn all_chunk_hashes(root: &Path) -> Vec<Hash> {
    fn walk(dir: &Path, out: &mut Vec<Hash>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if let Some(stem) = p.file_name().and_then(|s| s.to_str())
                && let Ok(h) = Hash::from_hex(stem)
            {
                out.push(h);
            }
        }
    }
    let mut out = Vec::new();
    walk(&root.join("shards"), &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "c0mpute-volcli-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_key_is_generated_once_and_then_reused() {
        let dir = tmpdir();
        let (k1, did1) = signing_key(&dir).unwrap();
        let (k2, did2) = signing_key(&dir).unwrap();
        assert_eq!(k1.to_bytes(), k2.to_bytes(), "key must be stable");
        assert_eq!(did1, did2);
        assert!(dir.join("volume.key").exists());
    }

    #[test]
    fn different_nodes_get_different_keys() {
        let (a, _) = signing_key(&tmpdir()).unwrap();
        let (b, _) = signing_key(&tmpdir()).unwrap();
        assert_ne!(a.to_bytes(), b.to_bytes());
    }

    #[cfg(unix)]
    #[test]
    fn the_key_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmpdir();
        signing_key(&dir).unwrap();
        let mode = std::fs::metadata(dir.join("volume.key"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "key file is readable by others");
    }

    #[test]
    fn hashes_parse_with_or_without_the_scheme() {
        let h = Hash::of(b"x");
        assert_eq!(parse_hash(&h.to_hex()).unwrap(), h);
        assert_eq!(parse_hash(&format!("blake3:{}", h.to_hex())).unwrap(), h);
        assert!(parse_hash("nope").is_err());
    }
}
