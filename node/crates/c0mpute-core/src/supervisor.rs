//! Long-running supervisor that owns the per-role tasks.

use std::sync::Arc;

use anyhow::Result;
use c0mpute_net::{ChunkSource, Loopback, Network};
use c0mpute_proto::{ChunkRequest, Hash};
use c0mpute_store::ChunkStore;
use tracing::info;

use crate::Config;

pub struct Supervisor {
    pub config: Config,
    pub store: ChunkStore,
    pub net: Arc<dyn Network>,
}

impl Supervisor {
    pub async fn boot(config: Config) -> Result<Self> {
        std::fs::create_dir_all(&config.storage.root)?;
        let store = ChunkStore::open(&config.storage.root).await?;
        // Until libp2p is wired up, we use the Loopback "network" backed by
        // the local store so end-to-end testing of the gateway works.
        let net: Arc<dyn Network> = Arc::new(Loopback::new(Arc::new(StoreSource(store.clone()))));
        Ok(Self {
            config,
            store,
            net,
        })
    }

    pub async fn run(self) -> Result<()> {
        info!(
            roles = ?self.config.roles,
            store = %self.config.storage.root.display(),
            "supervisor up"
        );

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

#[allow(dead_code)]
fn _silence_unused(_: ChunkRequest) {}
