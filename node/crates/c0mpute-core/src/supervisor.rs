//! Long-running supervisor that owns the per-role tasks.

use std::sync::Arc;

use anyhow::Result;
use c0mpute_net::{ChunkSource, Libp2pNetwork, Network, NetworkConfig, bootstrap};

use crate::capabilities::{self, Registry};
use crate::dispatch;
use crate::runner;
use crate::speech;
use c0mpute_proto::Hash;
use c0mpute_store::ChunkStore;
use tokio::sync::Semaphore;
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
        let identity_dir = config::config_dir().unwrap_or_else(|| std::path::PathBuf::from("."));

        let local_source: Arc<dyn ChunkSource> = Arc::new(StoreSource(store.clone()));

        // Fetch bootstrap list from c0mpute.com (best-effort) and merge
        // with the hardcoded fallback list. mDNS handles LAN discovery
        // independently. Empty bootstrap is fine for a standalone /
        // first-on-network node.
        let bootstrap_addrs = bootstrap::fetch_or_fallback(
            bootstrap::DEFAULT_BOOTSTRAP_URL,
            bootstrap::default_seeds(),
        )
        .await;
        info!(count = bootstrap_addrs.len(), "bootstrap peers loaded");

        let net_cfg = NetworkConfig::for_dir(identity_dir)
            .with_local_source(local_source)
            .with_bootstrap(bootstrap_addrs);

        let libp2p_net: Arc<Libp2pNetwork> = Arc::new(Libp2pNetwork::spawn(net_cfg).await?);
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
        let bench_score = capabilities::local_bench_score();
        let net = self.libp2p.clone();
        match bench_score {
            Some(score) => info!(?tags, score, "advertising capabilities"),
            None => info!(
                ?tags,
                "advertising capabilities (no bench score — run `c0mpute bench` to publish one)"
            ),
        }
        // Private mode keeps the node off the public listing: no ad.
        let policy = crate::private::current();
        if policy.advertises() {
            // Live tags: TTS can appear a minute or two into the run, once
            // Kokoro finishes provisioning, and should be advertised then
            // rather than at the next restart.
            tokio::spawn(capabilities::advertise_loop_live(
                net.clone(),
                self.config.clone(),
                capabilities::detect_hardware_tag(),
                hardware,
                bench_score,
                capabilities::DEFAULT_ADVERTISE_INTERVAL,
            ));
        } else {
            info!(
                trusted_buyers = policy.trusted.len(),
                "private mode: serving trusted buyers only, not advertising"
            );
        }

        // Job dispatch: subscribe to c0mpute/jobs/<workload> for each
        // workload our roles imply. Today: only ffmpeg.transcode if
        // the Transcode role is enabled.
        let cache_root = config::data_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("./c0mpute-data"))
            .join("cache");
        let total_slots = crate::resources::max_concurrent_cpu_jobs();
        let compute_slots = Arc::new(Semaphore::new(total_slots));
        info!(
            cpu_threads_per_job = crate::resources::cpu_threads_per_job(),
            max_concurrent_jobs = crate::resources::max_concurrent_cpu_jobs(),
            cuda_devices_per_job = ?crate::resources::cuda_devices_per_job(),
            "node compute budget configured"
        );
        for workload_type in dispatch::workload_types_from_roles(&self.config.roles) {
            // Node-wide slots, per-workload job timing: the dispatch loop
            // reads this to stop bidding when full and to quote an ETA.
            // workflow.step draws on its own light-work slots instead.
            let load = if workload_type == crate::workflow::WORKLOAD {
                crate::workflow::worker_load()
            } else {
                runner::WorkerLoad::new(compute_slots.clone(), total_slots)
            };
            // Speech is the one workload whose runtime we install ourselves.
            // Provision it first, in the background, then subscribe with the
            // tag set that install produced — a node advertising TTS it cannot
            // perform would win auctions only to fail them.
            if workload_type == "tts.synthesize" {
                let net = net.clone();
                let store = self.store.clone();
                let cache_root = cache_root.clone();
                let config = self.config.clone();
                tokio::spawn(async move {
                    if !speech::ensure_kokoro(&config.speech).await
                        && speech::available_tts_engines().is_empty()
                    {
                        info!("no TTS engine installed; not subscribing to tts.synthesize");
                        return;
                    }
                    let runner_tx =
                        runner::spawn_tts_runner(net.clone(), store, cache_root, load.clone());
                    info!(workload_type = "tts.synthesize", "subscribing to job topic");
                    dispatch::run_worker_subscriber_with_load(
                        net,
                        "tts.synthesize".to_string(),
                        capabilities::tags_from_config(&config),
                        Some(runner_tx),
                        None,
                        Some(config.pricing.clone()),
                        Some(load),
                    );
                });
                continue;
            }
            info!(%workload_type, "subscribing to job topic");
            let runner_tx = match workload_type.as_str() {
                "ffmpeg.transcode" => {
                    let ffmpeg_bin = which_ffmpeg().unwrap_or_else(|| "ffmpeg".into());
                    Some(runner::spawn_transcode_runner(
                        net.clone(),
                        self.store.clone(),
                        cache_root.clone(),
                        ffmpeg_bin,
                        load.clone(),
                    ))
                }
                // tts.synthesize is handled above, after provisioning.
                "stt.transcribe" => Some(runner::spawn_stt_runner(
                    net.clone(),
                    self.store.clone(),
                    cache_root.clone(),
                    load.clone(),
                )),
                crate::workflow::WORKLOAD => Some(crate::workflow::spawn_workflow_runner(
                    net.clone(),
                    self.store.clone(),
                    load.clone(),
                )),
                crate::video::WORKLOAD_TXT2VID | crate::video::WORKLOAD_IMG2VID => {
                    // The only container workload, so unlike the in-process
                    // runners it can be unservable on a box that configured
                    // the role: no GPU, no docker, or the image never pulled.
                    // Skip the subscription entirely in that case rather than
                    // bidding and failing.
                    if let Some(not_ready) = crate::video::readiness(crate::video::RUNNER_IMAGE) {
                        info!(
                            %workload_type,
                            reason = not_ready.reason(),
                            "not subscribing: this node cannot serve video"
                        );
                        continue;
                    }
                    Some(crate::video::spawn_video_runner(
                        net.clone(),
                        self.store.clone(),
                        cache_root.clone(),
                        workload_type.clone(),
                        crate::video::RUNNER_IMAGE.to_string(),
                        load.clone(),
                    ))
                }
                _ => None,
            };
            dispatch::run_worker_subscriber_with_load(
                net.clone(),
                workload_type,
                tags.clone(),
                runner_tx,
                None,
                Some(self.config.pricing.clone()),
                Some(load),
            );
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

        if c0mpute_update::auto_update_enabled(self.config.update_auto) {
            let feed = self
                .config
                .update_feed_url
                .clone()
                .unwrap_or_else(|| c0mpute_update::DEFAULT_RELEASE_FEED.to_string());
            let interval = std::time::Duration::from_secs(config::UPDATE_CHECK_INTERVAL_SECS);
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

fn which_ffmpeg() -> Option<std::path::PathBuf> {
    use std::env;
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        let p = dir.join("ffmpeg");
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

struct StoreSource(ChunkStore);

#[async_trait::async_trait]
impl ChunkSource for StoreSource {
    async fn read_chunk(&self, hash: &Hash) -> Result<Vec<u8>> {
        self.0.get(hash).await
    }
}
