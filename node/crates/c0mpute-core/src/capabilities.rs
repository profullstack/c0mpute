//! Capability advertisement + peer registry.
//!
//! Two halves:
//!
//!   1. **`advertise_loop`** — spawned per-worker. Builds a CapabilityAd
//!      from local config + hardware probe and publishes it to the
//!      `c0mpute/cap/v1` gossipsub topic at startup and every `interval`
//!      thereafter. Sets a TTL implicitly via `published_at_ms`;
//!      receivers ignore ads older than `MAX_AD_AGE`.
//!
//!   2. **`Registry`** — keeps a HashMap<PeerId, CapabilityAd> populated
//!      from inbound `c0mpute/cap/v1` messages. Exposes `find_with_tag`
//!      so the rest of the daemon can do "find peers with capability X"
//!      lookups for job dispatch / shard placement.
//!
//! Real-world capability tags use colon-separated hierarchy:
//!
//!   c0mpute:role:storage
//!   c0mpute:role:transcode
//!   c0mpute:role:gateway
//!   c0mpute:transcode:h264:nvenc
//!   c0mpute:gpu:nvidia
//!   c0mpute:storage:hot
//!
//! The `tags_from_config` helper builds an opinionated default set from
//! a `Config`. Workers can override via `[worker.capabilities]` in their
//! config file (not yet exposed; lands when we wire that knob).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use c0mpute_net::topics::{CAPABILITY_TOPIC, CapabilityAd};
use c0mpute_net::{GossipMessage, Libp2pNetwork, PeerId};
use c0mpute_proto::Role;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::Config;

/// Reject capability ads older than this. Loose enough that a node
/// publishing every 60s with some clock skew still works; tight enough
/// that stale peer state expires within a few minutes.
pub const MAX_AD_AGE: Duration = Duration::from_secs(5 * 60);

/// Default re-advertise interval. Each worker republishes its
/// CapabilityAd this often so receivers' TTLs don't expire.
pub const DEFAULT_ADVERTISE_INTERVAL: Duration = Duration::from_secs(60);

/// Build a default capability-tag set from config: one `c0mpute:role:*` per
/// configured role, plus exactly one hardware tag so schedulers can match
/// GPU-only work (transcode/inference) to GPU boxes.
pub fn tags_from_config(config: &Config) -> Vec<String> {
    let mut tags = Vec::new();
    for role in &config.roles {
        let role_tag = match role {
            Role::Storage => "c0mpute:role:storage",
            Role::Transcode => "c0mpute:role:transcode",
            Role::Gateway => "c0mpute:role:gateway",
            Role::Verifier => "c0mpute:role:verifier",
        };
        tags.push(role_tag.to_string());
    }
    tags.push(detect_hardware_tag());
    tags
}

/// Detect the host's compute hardware and return the matching capability tag.
/// Exactly one is returned; a box with no usable accelerator advertises
/// `c0mpute:cpu`. Runs once at startup (the tag set is built then passed to
/// the advertise loop), so a one-off `nvidia-smi`/`rocm-smi` probe is cheap.
pub fn detect_hardware_tag() -> String {
    // Apple Silicon — Metal GPU on macOS aarch64.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        return "c0mpute:gpu:apple".to_string();
    }
    if nvidia_present() {
        return "c0mpute:gpu:nvidia".to_string();
    }
    if amd_present() {
        return "c0mpute:gpu:amd".to_string();
    }
    "c0mpute:cpu".to_string()
}

/// NVIDIA GPU present — a device node or a working `nvidia-smi -L`.
fn nvidia_present() -> bool {
    if std::path::Path::new("/dev/nvidia0").exists() {
        return true;
    }
    std::process::Command::new("nvidia-smi")
        .arg("-L")
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false)
}

/// AMD ROCm GPU present — the KFD device node or a working `rocm-smi`.
fn amd_present() -> bool {
    if std::path::Path::new("/dev/kfd").exists() {
        return true;
    }
    std::process::Command::new("rocm-smi")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Build the per-host hardware blob attached to capability ads. Today
/// it's a coarse summary; future: probe ffmpeg encoders, GPU vendor,
/// free disk, free VRAM, etc.
pub fn hardware_blob(config: &Config) -> serde_json::Value {
    serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "free_disk_root": config.storage.root.display().to_string(),
        // Real hardware fields land in a follow-up — sysinfo + ffmpeg
        // -encoders + nvidia-smi-style probes.
    })
}

/// Periodically publish this worker's capability ad.
pub async fn advertise_loop(
    net: Arc<Libp2pNetwork>,
    tags: Vec<String>,
    hardware: serde_json::Value,
    interval: Duration,
) {
    // Always subscribe before we publish — gossipsub requires the
    // publisher to be in the topic's mesh to actually fan out.
    if let Err(e) = net.subscribe(CAPABILITY_TOPIC).await {
        warn!(err = %e, "failed to subscribe to capability topic");
        return;
    }

    let peer_id = net.peer_id().to_base58();
    info!(
        topic = CAPABILITY_TOPIC,
        interval_secs = interval.as_secs(),
        "capability advertisement loop starting"
    );

    let mut ticker = tokio::time::interval(interval);
    loop {
        ticker.tick().await;
        let ad = CapabilityAd::now(peer_id.clone(), tags.clone(), hardware.clone());
        let payload = match serde_json::to_vec(&ad) {
            Ok(b) => b,
            Err(e) => {
                warn!(err = %e, "serialize capability ad");
                continue;
            }
        };
        match net.publish(CAPABILITY_TOPIC, payload).await {
            Ok(()) => debug!("published capability ad"),
            Err(e) => {
                // The first publish often fails because gossipsub mesh
                // hasn't been built yet. Log at debug, keep trying.
                debug!(err = %e, "capability publish failed (retrying next tick)");
            }
        }
    }
}

/// In-memory registry of recently-seen peer capabilities.
#[derive(Clone, Default)]
pub struct Registry {
    inner: Arc<RwLock<HashMap<PeerId, CapabilityAd>>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take the gossipsub broadcast stream and feed messages into the
    /// registry. Spawns a task that runs until the network is dropped.
    pub fn run(&self, net: Arc<Libp2pNetwork>) {
        let inner = self.inner.clone();
        tokio::spawn(async move {
            // Subscribe so we receive the topic.
            if let Err(e) = net.subscribe(CAPABILITY_TOPIC).await {
                warn!(err = %e, "registry: subscribe failed");
                return;
            }
            let mut rx = net.messages();
            loop {
                let msg: GossipMessage = match rx.recv().await {
                    Ok(m) => m,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(skipped = n, "registry: broadcast channel lagged");
                        continue;
                    }
                    Err(_) => {
                        info!("registry: gossip channel closed");
                        return;
                    }
                };
                if msg.topic != CAPABILITY_TOPIC {
                    continue;
                }
                let ad: CapabilityAd = match serde_json::from_slice(&msg.data) {
                    Ok(a) => a,
                    Err(e) => {
                        debug!(err = %e, "registry: discard malformed ad");
                        continue;
                    }
                };
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                if now_ms.saturating_sub(ad.published_at_ms) > MAX_AD_AGE.as_millis() as u64 {
                    debug!(peer = %ad.peer_id, "registry: discard stale ad");
                    continue;
                }
                let Some(source) = msg.source else {
                    debug!("registry: discard ad with unknown source peer");
                    continue;
                };
                let mut guard = inner.write().await;
                guard.insert(source, ad);
            }
        });

        // Spawn a separate eviction task that drops entries older than
        // MAX_AD_AGE every 30s.
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(30));
            loop {
                ticker.tick().await;
                let cutoff = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0)
                    .saturating_sub(MAX_AD_AGE.as_millis() as u64);
                let mut guard = inner.write().await;
                guard.retain(|_, ad| ad.published_at_ms >= cutoff);
            }
        });
    }

    pub async fn list(&self) -> Vec<(PeerId, CapabilityAd)> {
        self.inner
            .read()
            .await
            .iter()
            .map(|(p, a)| (*p, a.clone()))
            .collect()
    }

    pub async fn find_with_tag(&self, tag: &str) -> Vec<(PeerId, CapabilityAd)> {
        self.inner
            .read()
            .await
            .iter()
            .filter(|(_, a)| a.tags.iter().any(|t| t == tag))
            .map(|(p, a)| (*p, a.clone()))
            .collect()
    }

    pub async fn count(&self) -> usize {
        self.inner.read().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use c0mpute_net::{Libp2pNetwork, Multiaddr, NetworkConfig, Protocol};

    fn tempdir(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "c0mpute-cap-test-{}-{}",
            name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    async fn wait_for_listen(net: &Libp2pNetwork) -> Multiaddr {
        for _ in 0..50 {
            let addrs = net.listen_addrs().await.unwrap_or_default();
            if let Some(a) = addrs.into_iter().next() {
                return a;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("node never bound a listen addr");
    }

    /// Two nodes: A advertises with capability tags, B's registry should
    /// observe A's ad with the same tags within a few seconds.
    #[tokio::test]
    async fn registry_observes_advertised_capabilities() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("c0mpute_core=info,c0mpute_net=info,libp2p_gossipsub=warn")
            .with_test_writer()
            .try_init();

        let net_a = Arc::new(
            Libp2pNetwork::spawn(
                NetworkConfig::for_dir(tempdir("cap-a"))
                    .with_listen(vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()]),
            )
            .await
            .unwrap(),
        );
        let a_addr = wait_for_listen(&net_a).await;
        let a_full = a_addr.with(Protocol::P2p(net_a.peer_id()));

        let net_b = Arc::new(
            Libp2pNetwork::spawn(
                NetworkConfig::for_dir(tempdir("cap-b"))
                    .with_listen(vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()])
                    .with_bootstrap(vec![a_full]),
            )
            .await
            .unwrap(),
        );

        // B's registry watches inbound capability ads.
        let registry = Registry::new();
        registry.run(net_b.clone());

        // A advertises every 200ms (faster than prod's 60s) so the
        // test doesn't have to wait long.
        let tags = vec![
            "c0mpute:role:storage".to_string(),
            "c0mpute:role:transcode".to_string(),
        ];
        let hardware = serde_json::json!({"test": true});
        tokio::spawn(advertise_loop(
            net_a.clone(),
            tags.clone(),
            hardware,
            Duration::from_millis(200),
        ));

        // Poll the registry until A's ad shows up (up to 5s).
        let mut found: Option<CapabilityAd> = None;
        for _ in 0..50 {
            let entries = registry.list().await;
            if let Some((_, ad)) =
                entries.iter().find(|(p, _)| *p == net_a.peer_id())
            {
                found = Some(ad.clone());
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let ad = found.expect("registry never saw A's advertisement");
        assert_eq!(ad.peer_id, net_a.peer_id().to_base58());
        assert_eq!(ad.tags, tags);

        // find_with_tag should return A.
        let storage_workers =
            registry.find_with_tag("c0mpute:role:storage").await;
        assert_eq!(storage_workers.len(), 1);
        assert_eq!(storage_workers[0].0, net_a.peer_id());
    }

    #[test]
    fn detect_hardware_tag_returns_one_known_tag() {
        let tag = super::detect_hardware_tag();
        assert!(
            [
                "c0mpute:gpu:nvidia",
                "c0mpute:gpu:amd",
                "c0mpute:gpu:apple",
                "c0mpute:cpu",
            ]
            .contains(&tag.as_str()),
            "unexpected hardware tag: {tag}"
        );
    }

    #[test]
    fn tags_from_config_includes_roles_and_exactly_one_hardware_tag() {
        let cfg = Config::default(); // roles: storage, gateway, verifier
        let tags = super::tags_from_config(&cfg);
        assert!(tags.contains(&"c0mpute:role:storage".to_string()));
        assert!(tags.contains(&"c0mpute:role:gateway".to_string()));
        let hw: Vec<_> = tags
            .iter()
            .filter(|t| t.starts_with("c0mpute:gpu:") || t.as_str() == "c0mpute:cpu")
            .collect();
        assert_eq!(hw.len(), 1, "expected exactly one hardware tag, got {hw:?}");
    }
}
