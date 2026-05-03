//! Long-running supervisor that owns the per-role tasks.

use std::sync::Arc;

use anyhow::Result;
use c0mpute_net::{ChunkSource, Libp2pNetwork, Network, NetworkConfig, bootstrap};

use crate::capabilities::{self, Registry};
use crate::dispatch;
use c0mpute_proto::Hash;
use c0mpute_store::ChunkStore;
use tracing::info;

use crate::{Config, config};

pub struct Supervisor {
    pub config: Config,
    pub store: ChunkStore,
    pub net: Arc<dyn Network>,
    pub libp2p: Arc<Libp2pNetwork>,
    pub registry: Registry,
}

impl Supervisor {
    pub async fn boot(config: Config) -> Result<Self> {
        std::fs::create_dir_all(&config.storage.root)?;
        let store = ChunkStore::open(&config.storage.root).await?;

        // Real libp2p network. Identity persists at
        // <config_dir>/identity.key. Bootstrap list is empty for now —
        // DIP-0010 wires up c0mpute.com/bootstrap.json once we run
        // public seed nodes.
        let identity_dir = config::config_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."));

        let local_source: Arc<dyn ChunkSource> =
            Arc::new(StoreSource(store.clone()));

        // Fetch bootstrap list from c0mpute.com (best-effort) and merge
        // with the hardcoded fallback list. mDNS handles LAN discovery
        // independently. Empty bootstrap is fine for a standalone /
        // first-on-network node.
        let bootstrap_addrs = bootstrap::fetch_or_fallback(
            bootstrap::DEFAULT_BOOTSTRAP_URL,
            vec![],
        )
        .await;
        info!(
            count = bootstrap_addrs.len(),
            "bootstrap peers loaded"
        );

        let net_cfg = NetworkConfig::for_dir(identity_dir)
            .with_local_source(local_source)
            .with_bootstrap(bootstrap_addrs);

        let libp2p_net: Arc<Libp2pNetwork> =
            Arc::new(Libp2pNetwork::spawn(net_cfg).await?);
        info!(
            peer_id = %libp2p_net.peer_id(),
            "libp2p network up"
        );
        // Same Arc, two views: the dyn-Network trait object for the
        // gateway / chunk-fetch path, and the concrete Libp2pNetwork
        // for the pub/sub APIs that aren't on the trait.
        let net: Arc<dyn Network> = libp2p_net.clone() as Arc<dyn Network>;
        let registry = Registry::new();

        Ok(Self {
            config,
            store,
            net,
            libp2p: libp2p_net,
            registry,
        })
    }

    pub async fn run(self) -> Result<()> {
        info!(
            roles = ?self.config.roles,
            store = %self.config.storage.root.display(),
            "supervisor up"
        );

        // Capability registry: subscribe to c0mpute/cap/v1 and keep a
        // map of seen peers + their advertised capabilities.
        self.registry.run(self.libp2p.clone());

        // Capability advertise loop: periodically publish our own ad.
        let tags = capabilities::tags_from_config(&self.config);
        let hardware = capabilities::hardware_blob(&self.config);
        let net = self.libp2p.clone();
        info!(?tags, "advertising capabilities");
        tokio::spawn(capabilities::advertise_loop(
            net.clone(),
            tags.clone(),
            hardware,
            capabilities::DEFAULT_ADVERTISE_INTERVAL,
        ));

        // Job dispatch: subscribe to c0mpute/jobs/<workload> for each
        // workload our roles imply. Today: only ffmpeg.transcode if
        // the Transcode role is enabled.
        for workload_type in dispatch::workload_types_from_roles(&self.config.roles) {
            info!(%workload_type, "subscribing to job topic");
            dispatch::run_worker_subscriber(net.clone(), workload_type, tags.clone());
        }

        if self.config.roles.contains(&c0mpute_proto::Role::Gateway) {
            let bind: std::net::SocketAddr = self.config.gateway.bind.parse()?;
            let state = c0mpute_gateway::GatewayState {
                store: self.store.clone(),
                net: self.net.clone(),
            };
            tokio::spawn(async move {
                if let Err(e) = c0mpute_gateway::serve(state, bind).await {
                    tracing::error!(err = %e, "gateway server exited");
                }
            });
        }

        if self.config.update_auto {
            let feed = self
                .config
                .update_feed_url
                .clone()
                .unwrap_or_else(|| c0mpute_update::DEFAULT_RELEASE_FEED.to_string());
            let interval =
                std::time::Duration::from_secs(self.config.update_interval_secs.max(60));
            let current = env!("CARGO_PKG_VERSION").to_string();
            info!(
                interval_secs = interval.as_secs(),
                feed = %feed,
                "auto-upgrade poller starting"
            );
            tokio::spawn(c0mpute_update::poll_loop(current, feed, interval));
        }

        // Hold open until ctrl-c.
        tokio::signal::ctrl_c().await?;
        info!("ctrl-c received; shutting down");
        Ok(())
    }
}

struct StoreSource(ChunkStore);

#[async_trait::async_trait]
impl ChunkSource for StoreSource {
    async fn read_chunk(&self, hash: &Hash) -> Result<Vec<u8>> {
        self.0.get(hash).await
    }
}
