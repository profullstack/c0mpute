//! The in-process BitTorrent engine (librqbit) behind a small surface.
//!
//! The engine seeds exactly the plan it is handed ([`Engine::apply`]) and
//! nothing else. It has no add API of its own, keeps no session state across
//! restarts (every start re-adds from a freshly verified plan), and runs with
//! the peer-discovery side doors closed:
//!
//! - no DHT (peers come from the trackers in the signed manifest or in the
//!   operator's own `.torrent` file),
//! - no local service discovery, no UPnP,
//! - TCP only, no uTP.
//!
//! Each torrent lives in its own directory, `<data_dir>/<info_hash>/`. A
//! torrent that leaves the plan is deleted with its files, and any info-hash
//! directory the plan does not name is swept, so what is on disk is what the
//! plan allows.

use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use librqbit::api::TorrentIdOrHash;
use librqbit::dht::Id20;
use librqbit::limits::LimitsConfig;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, ListenerMode, ListenerOptions,
    ManagedTorrent, Session, SessionOptions, TorrentStatsState,
};
use serde::Serialize;
use tracing::{debug, info, warn};

use crate::manifest::normalize_info_hash;
use crate::plan::{Candidate, Origin};

/// How long a published torrent may take to hand over its metadata before
/// the node gives up until the next round.
pub const DEFAULT_METADATA_TIMEOUT: Duration = Duration::from_secs(60);

/// How the engine runs.
#[derive(Clone, Debug)]
pub struct EngineOptions {
    /// Where torrents live, one directory per info hash.
    pub data_dir: PathBuf,
    /// TCP port peers connect to (`0`: any free port).
    pub port: u16,
    /// Upload cap for the whole engine, bytes per second (`None`: no cap).
    pub upload_bytes_per_sec: Option<NonZeroU32>,
    /// Peers tried for every torrent besides the ones trackers return. Not
    /// reachable from the node config; the loopback test uses it.
    pub bootstrap_peers: Vec<SocketAddr>,
    pub metadata_timeout: Duration,
}

impl EngineOptions {
    pub fn new(data_dir: PathBuf, port: u16, upload_bytes_per_sec: Option<NonZeroU32>) -> Self {
        Self {
            data_dir,
            port,
            upload_bytes_per_sec,
            bootstrap_peers: Vec::new(),
            metadata_timeout: DEFAULT_METADATA_TIMEOUT,
        }
    }
}

/// One planned torrent as the engine sees it now.
#[derive(Clone, Debug, Serialize)]
pub struct TorrentStatus {
    pub info_hash: String,
    pub name: String,
    /// Size when complete, as planned.
    pub bytes: u64,
    /// `published` (our signed list) or `local` (the operator's config).
    pub origin: &'static str,
    pub license: String,
    pub source: String,
    /// `seeding`, `downloading`, `checking`, `paused`, `waiting` (not added
    /// yet, see `error`) or `error`.
    pub state: String,
    /// Verified bytes of it on disk.
    pub held_bytes: u64,
    pub uploaded_bytes: u64,
    pub error: Option<String>,
}

/// What a `.torrent` file describes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TorrentFileInfo {
    pub info_hash: String,
    pub name: String,
    pub bytes: u64,
}

/// Read a `.torrent` file's info hash, name and total size.
pub fn inspect_torrent(bytes: &[u8]) -> Result<TorrentFileInfo> {
    let t = librqbit::torrent_from_bytes(bytes)
        .map_err(|e| anyhow!("not a .torrent file: {e}"))?;
    let info_hash = t.info_hash.as_string();
    let info = t
        .info
        .data
        .validate()
        .map_err(|e| anyhow!("invalid torrent: {e}"))?;
    let name = info
        .name()
        .map(|n| n.into_owned())
        .unwrap_or_else(|| info_hash.clone());
    Ok(TorrentFileInfo {
        info_hash,
        name,
        bytes: info.iter_file_lengths().sum(),
    })
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The magnet link for a published torrent: its info hash and trackers.
pub fn magnet_uri(info_hash: &str, trackers: &[String]) -> String {
    let mut m = format!("magnet:?xt=urn:btih:{info_hash}");
    for t in trackers {
        m.push_str("&tr=");
        m.push_str(&percent_encode(t));
    }
    m
}

struct Held {
    candidate: Candidate,
    handle: Arc<ManagedTorrent>,
}

/// A running engine.
pub struct Engine {
    session: Arc<Session>,
    opts: EngineOptions,
    held: Mutex<HashMap<String, Held>>,
    /// Planned torrents that are not added (yet), and why.
    pending: Mutex<Vec<(Candidate, String)>>,
    /// One `apply` at a time.
    applying: tokio::sync::Mutex<()>,
}

fn id20(info_hash: &str) -> Result<Id20> {
    let raw = hex::decode(info_hash).context("info hash is not hex")?;
    Id20::from_bytes(&raw).map_err(|e| anyhow!("info hash: {e}"))
}

impl Engine {
    /// Start the engine: bind the port and create the data directory.
    pub async fn start(opts: EngineOptions) -> Result<Self> {
        tokio::fs::create_dir_all(&opts.data_dir)
            .await
            .with_context(|| format!("create {}", opts.data_dir.display()))?;
        let session_opts = |addr: SocketAddr, ipv4_only: bool| SessionOptions {
            dht: None,
            disable_local_service_discovery: true,
            persistence: None,
            fastresume: false,
            ipv4_only,
            listen: Some(ListenerOptions {
                mode: ListenerMode::TcpOnly,
                listen_addr: addr,
                enable_upnp_port_forwarding: false,
                ipv4_only,
                ..Default::default()
            }),
            ratelimits: LimitsConfig {
                upload_bps: opts.upload_bytes_per_sec,
                download_bps: None,
            },
            client_name_and_version: Some(format!("c0mpute {}", env!("CARGO_PKG_VERSION"))),
            ..Default::default()
        };
        let dual = (Ipv6Addr::UNSPECIFIED, opts.port).into();
        let session =
            match Session::new_with_opts(opts.data_dir.clone(), session_opts(dual, false)).await {
                Ok(s) => s,
                Err(e) => {
                    // A box with IPv6 turned off cannot bind [::].
                    debug!(err = %format!("{e:#}"), "seeding: dual-stack listen failed, trying IPv4 only");
                    let v4 = (Ipv4Addr::UNSPECIFIED, opts.port).into();
                    Session::new_with_opts(opts.data_dir.clone(), session_opts(v4, true))
                        .await
                        .with_context(|| format!("listen on TCP port {}", opts.port))?
                }
            };
        info!(
            port = session.listen_addr().map(|a| a.port()),
            dir = %opts.data_dir.display(),
            upload_bytes_per_sec = opts.upload_bytes_per_sec.map(NonZeroU32::get),
            "seeding: engine up (TCP only, no DHT)"
        );
        Ok(Self {
            session,
            opts,
            held: Mutex::new(HashMap::new()),
            pending: Mutex::new(Vec::new()),
            applying: tokio::sync::Mutex::new(()),
        })
    }

    /// The TCP port peers connect to.
    pub fn listen_port(&self) -> Option<u16> {
        self.session.listen_addr().map(|a| a.port())
    }

    fn dir_of(&self, info_hash: &str) -> PathBuf {
        self.opts.data_dir.join(info_hash)
    }

    /// Make the engine seed exactly `plan`: torrents not in it are removed
    /// with their files, missing ones are added. A torrent that cannot be
    /// added now (no peer has its metadata yet, or it is refused) is reported
    /// in the snapshot and tried again on the next call.
    pub async fn apply(&self, plan: &[Candidate]) {
        let _one = self.applying.lock().await;
        let wanted: HashSet<&str> = plan.iter().map(|c| c.info_hash.as_str()).collect();

        let gone: Vec<String> = {
            let held = self.held.lock().unwrap_or_else(|e| e.into_inner());
            held.keys()
                .filter(|h| !wanted.contains(h.as_str()))
                .cloned()
                .collect()
        };
        for hash in gone {
            match id20(&hash) {
                Ok(id) => {
                    if let Err(e) = self.session.delete(TorrentIdOrHash::Hash(id), true).await {
                        warn!(%hash, err = %format!("{e:#}"), "seeding: could not remove a withdrawn torrent");
                    }
                }
                Err(e) => warn!(%hash, err = %e, "seeding: bad held hash"),
            }
            self.held
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&hash);
            info!(%hash, "seeding: stopped seeding (no longer in the plan)");
        }
        self.sweep(&wanted).await;

        let mut pending = Vec::new();
        for c in plan {
            if self
                .held
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(&c.info_hash)
            {
                continue;
            }
            match self.add(c).await {
                Ok(handle) => {
                    info!(hash = %c.info_hash, name = %c.name, bytes = c.bytes, origin = c.origin.label(), "seeding: added");
                    self.held
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(
                            c.info_hash.clone(),
                            Held {
                                candidate: c.clone(),
                                handle,
                            },
                        );
                }
                Err(e) => {
                    let why = format!("{e:#}");
                    debug!(hash = %c.info_hash, err = %why, "seeding: not added this round");
                    pending.push((c.clone(), why));
                }
            }
        }
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = pending;
    }

    /// Remove every info-hash directory the plan does not name. Only
    /// directories named like an info hash are touched.
    async fn sweep(&self, wanted: &HashSet<&str>) {
        let Ok(mut entries) = tokio::fs::read_dir(&self.opts.data_dir).await else {
            return;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            if normalize_info_hash(&name).as_deref() != Some(name.as_str())
                || wanted.contains(name.as_str())
            {
                continue;
            }
            if entry.file_type().await.is_ok_and(|t| t.is_dir()) {
                match tokio::fs::remove_dir_all(entry.path()).await {
                    Ok(()) => info!(hash = %name, "seeding: removed data of a torrent that is not in the plan"),
                    Err(e) => warn!(hash = %name, err = %e, "seeding: could not remove stale data"),
                }
            }
        }
    }

    fn add_opts(&self, c: &Candidate) -> AddTorrentOptions {
        AddTorrentOptions {
            overwrite: true,
            output_folder: Some(self.dir_of(&c.info_hash).to_string_lossy().into_owned()),
            initial_peers: (!self.opts.bootstrap_peers.is_empty())
                .then(|| self.opts.bootstrap_peers.clone()),
            ..Default::default()
        }
    }

    async fn add(&self, c: &Candidate) -> Result<Arc<ManagedTorrent>> {
        let (torrent_bytes, trackers) = match &c.origin {
            Origin::Local { torrent } => {
                let bytes = tokio::fs::read(torrent)
                    .await
                    .with_context(|| format!("read {}", torrent.display()))?;
                // The file may have changed since it was planned.
                let info = inspect_torrent(&bytes)?;
                ensure!(
                    info.info_hash == c.info_hash && info.bytes <= c.bytes,
                    "{} changed since it was planned",
                    torrent.display()
                );
                (bytes::Bytes::from(bytes), None)
            }
            Origin::Published { trackers } => {
                let listed = tokio::time::timeout(
                    self.opts.metadata_timeout,
                    self.session.add_torrent(
                        AddTorrent::from_url(magnet_uri(&c.info_hash, trackers)),
                        Some(AddTorrentOptions {
                            list_only: true,
                            ..self.add_opts(c)
                        }),
                    ),
                )
                .await
                .map_err(|_| anyhow!("no peer has sent this torrent's metadata yet"))?
                .context("fetch the torrent's metadata")?;
                let AddTorrentResponse::ListOnly(listed) = listed else {
                    bail!("the engine added a torrent it was only asked to describe");
                };
                ensure!(
                    listed.info_hash.as_string() == c.info_hash,
                    "metadata is for another torrent"
                );
                // The plan (and so the disk cap) counted the size the signed
                // manifest declares. A torrent that is larger is not seeded.
                let actual: u64 = listed.info.iter_file_lengths().sum();
                ensure!(
                    actual <= c.bytes,
                    "refused: the torrent is {actual} bytes, more than the {} bytes the signed manifest declares",
                    c.bytes
                );
                (listed.torrent_bytes, Some(trackers.clone()))
            }
        };
        let added = self
            .session
            .add_torrent(
                AddTorrent::from_bytes(torrent_bytes),
                Some(AddTorrentOptions {
                    trackers,
                    ..self.add_opts(c)
                }),
            )
            .await?;
        added
            .into_handle()
            .context("the engine did not return a torrent")
    }

    /// Every planned torrent, held or not.
    pub fn snapshot(&self) -> Vec<TorrentStatus> {
        let row = |c: &Candidate| TorrentStatus {
            info_hash: c.info_hash.clone(),
            name: c.name.clone(),
            bytes: c.bytes,
            origin: c.origin.label(),
            license: c.license.clone(),
            source: c.source.clone(),
            state: String::new(),
            held_bytes: 0,
            uploaded_bytes: 0,
            error: None,
        };
        let mut out: Vec<TorrentStatus> = self
            .held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|h| {
                let s = h.handle.stats();
                let state = if s.error.is_some() {
                    "error"
                } else if s.finished {
                    "seeding"
                } else {
                    match s.state {
                        TorrentStatsState::Initializing { .. } => "checking",
                        TorrentStatsState::Live => "downloading",
                        TorrentStatsState::Paused => "paused",
                        TorrentStatsState::Error => "error",
                    }
                };
                TorrentStatus {
                    state: state.to_string(),
                    held_bytes: s.progress_bytes,
                    uploaded_bytes: s.uploaded_bytes,
                    error: s.error,
                    ..row(&h.candidate)
                }
            })
            .collect();
        out.extend(
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .map(|(c, why)| TorrentStatus {
                    state: "waiting".to_string(),
                    error: Some(why.clone()),
                    ..row(c)
                }),
        );
        out.sort_by(|a, b| a.info_hash.cmp(&b.info_hash));
        out
    }

    /// Stop the engine. Files stay on disk.
    pub async fn stop(&self) {
        self.session.stop().await;
    }
}

/// Bytes under `dir` (regular files, not following symlinks).
pub fn dir_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_bytes(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_magnet_carries_the_hash_and_the_trackers() {
        let hash = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        assert_eq!(magnet_uri(hash, &[]), format!("magnet:?xt=urn:btih:{hash}"));
        let m = magnet_uri(hash, &["udp://t.example.org:1337/announce?a=b&c".to_string()]);
        assert_eq!(
            m,
            format!("magnet:?xt=urn:btih:{hash}&tr=udp%3A%2F%2Ft.example.org%3A1337%2Fannounce%3Fa%3Db%26c")
        );
    }

    #[test]
    fn dir_bytes_adds_up_files() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(dir_bytes(dir.path()), 0);
        assert_eq!(dir_bytes(&dir.path().join("missing")), 0);
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        std::fs::write(dir.path().join("a/one"), [0u8; 10]).unwrap();
        std::fs::write(dir.path().join("a/b/two"), [0u8; 32]).unwrap();
        assert_eq!(dir_bytes(dir.path()), 42);
    }

    fn published(info_hash: &str, bytes: u64) -> Candidate {
        Candidate {
            info_hash: info_hash.to_string(),
            name: "release".into(),
            bytes,
            license: "CC0-1.0".into(),
            source: "https://example.org/".into(),
            origin: Origin::Published { trackers: vec![] },
        }
    }

    async fn wait_for(engine: &Engine, state: &str) -> TorrentStatus {
        for _ in 0..600 {
            if let Some(t) = engine.snapshot().into_iter().find(|t| t.state == state) {
                return t;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("never reached '{state}': {:?}", engine.snapshot());
    }

    /// Two engines on loopback: a publisher seeding a file, and a node that
    /// is handed a plan naming it. No tracker, no DHT: the node is told the
    /// publisher's address directly.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_node_seeds_its_plan_within_the_declared_size_and_drops_what_is_withdrawn() {
        const SIZE: usize = 300 * 1024 + 17;
        let src = tempfile::tempdir().unwrap();
        let file = src.path().join("release.bin");
        let content: Vec<u8> = (0..SIZE).map(|i| (i * 31 % 251) as u8).collect();
        std::fs::write(&file, &content).unwrap();

        let publisher = Session::new_with_opts(
            src.path().to_path_buf(),
            SessionOptions {
                dht: None,
                disable_local_service_discovery: true,
                persistence: None,
                listen: Some(ListenerOptions {
                    mode: ListenerMode::TcpOnly,
                    listen_addr: (Ipv4Addr::LOCALHOST, 0).into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let (created, _serving) = publisher
            .create_and_serve_torrent(&file, Default::default())
            .await
            .map_err(|e| anyhow!("{e:?}"))
            .unwrap();
        let hash = created.info_hash().as_string();
        let publisher_addr: SocketAddr = (
            Ipv4Addr::LOCALHOST,
            publisher.listen_addr().unwrap().port(),
        )
            .into();

        // A .torrent file describes the same torrent.
        let described = inspect_torrent(&created.as_bytes().unwrap()).unwrap();
        assert_eq!(described.info_hash, hash);
        assert_eq!(described.bytes, SIZE as u64);
        assert!(inspect_torrent(b"not a torrent").is_err());

        let data = tempfile::tempdir().unwrap();
        let mut opts = EngineOptions::new(data.path().join("data"), 0, NonZeroU32::new(8 << 20));
        opts.bootstrap_peers = vec![publisher_addr];
        opts.metadata_timeout = Duration::from_secs(30);
        let engine = Engine::start(opts).await.unwrap();
        assert!(engine.listen_port().is_some_and(|p| p != 0));
        let dir = data.path().join("data").join(&hash);

        // The manifest declared one byte less than the torrent really is:
        // refused, nothing downloaded.
        engine.apply(&[published(&hash, SIZE as u64 - 1)]).await;
        let refused = engine.snapshot();
        assert_eq!(refused.len(), 1);
        assert_eq!(refused[0].state, "waiting");
        assert!(
            refused[0].error.as_deref().unwrap_or("").contains("more than"),
            "{refused:?}"
        );
        assert_eq!(dir_bytes(&dir), 0);

        // Declared honestly: downloaded, verified, then seeded.
        engine.apply(&[published(&hash, SIZE as u64)]).await;
        let seeding = wait_for(&engine, "seeding").await;
        assert_eq!(seeding.info_hash, hash);
        assert_eq!(seeding.held_bytes, SIZE as u64);
        assert_eq!(seeding.origin, "published");
        assert_eq!(std::fs::read(dir.join("release.bin")).unwrap(), content);
        assert_eq!(dir_bytes(&dir), SIZE as u64);

        // A stray info-hash directory that no plan names is swept; other
        // directories are left alone.
        let stray = data.path().join("data").join("b".repeat(40));
        let other = data.path().join("data").join("notes");
        std::fs::create_dir_all(&stray).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        engine.apply(&[published(&hash, SIZE as u64)]).await;
        assert!(!stray.exists() && other.exists() && dir.exists());

        // Withdrawn from the plan: gone, with its files.
        engine.apply(&[]).await;
        assert!(engine.snapshot().is_empty());
        assert!(!dir.exists());

        engine.stop().await;
        publisher.stop().await;
    }
}
