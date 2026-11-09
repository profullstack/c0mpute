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
    /// Boot as this machine's worker: identity at `<config_dir>/identity.key`.
    pub async fn boot(config: Config) -> Result<Self> {
        let identity_dir = config::config_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
        Self::boot_with_identity(config, identity_dir).await
    }

    /// Boot as a buyer (the speech and jobs bridges, one-shot submits) with
    /// its own identity at `<data_dir>/buyer/identity.key` (where private mode
    /// already looks for this machine's buyer identities, so a private node
    /// still trusts its own bridges).
    ///
    /// A buyer must not reuse the worker's key: two processes with one peer
    /// id on the same box make remote peers keep only one connection, and
    /// the buyer's auctions fail with `NoPeersSubscribedToTopic` while the
    /// network has plenty of workers on the topic.
    pub async fn boot_buyer(config: Config) -> Result<Self> {
        let identity_dir = config::data_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("buyer");
        std::fs::create_dir_all(&identity_dir)?;
        Self::boot_with_identity(config, identity_dir).await
    }

    async fn boot_with_identity(config: Config, identity_dir: std::path::PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&config.storage.root)?;
        let store = ChunkStore::open(&config.storage.root).await?;

        // Real libp2p network. The identity persists in `identity_dir`.

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

        let mut net_cfg = NetworkConfig::for_dir(identity_dir)
            .with_local_source(local_source)
            .with_bootstrap(bootstrap_addrs);
        // A fixed, firewall-opened port for nodes that must be dialable
        // (buyer gateways): C0MPUTE_P2P_LISTEN=/ip4/0.0.0.0/tcp/46341
        let listen =
            crate::reachability::listen_addrs(std::env::var("C0MPUTE_P2P_LISTEN").ok().as_deref());
        if !listen.is_empty() {
            info!(?listen, "libp2p listening on configured addresses");
            net_cfg = net_cfg.with_listen(listen);
        }

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
            } else if workload_type == crate::crawl::WORKLOAD {
                crate::crawl::worker_load()
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
            // Same shape for CPU diffusion: install in the background (6.7 GB,
            // only on boxes with the RAM and disk), subscribe once it works.
            // The role tag itself is gated per tick on free memory.
            if workload_type == crate::diffusion::WORKLOAD_TXT2IMG {
                let net = net.clone();
                let store = self.store.clone();
                let cache_root = cache_root.clone();
                let config = self.config.clone();
                tokio::spawn(async move {
                    if !crate::diffusion::ensure(&config.diffusion).await {
                        info!(
                            "diffusion not available on this node; not subscribing to diffusion.txt2img"
                        );
                        return;
                    }
                    let runner_tx = crate::diffusion::spawn_diffusion_runner(
                        net.clone(),
                        store,
                        cache_root,
                        load.clone(),
                    );
                    info!(
                        workload_type = crate::diffusion::WORKLOAD_TXT2IMG,
                        "subscribing to job topic"
                    );
                    // Re-check free memory on every offer: the tag set is
                    // fixed at subscribe time, the box's load is not.
                    let gate = config.diffusion.clone();
                    let filter: dispatch::OfferFilter =
                        std::sync::Arc::new(move |_offer| crate::diffusion::can_serve(&gate));
                    dispatch::run_worker_subscriber_with_load(
                        net,
                        crate::diffusion::WORKLOAD_TXT2IMG.to_string(),
                        capabilities::tags_from_config(&config),
                        Some(runner_tx),
                        Some(filter),
                        Some(config.pricing.clone()),
                        Some(load),
                    );
                });
                continue;
            }
            // Music: the same shape (ACE-Step 1.5, ~8 GB, only on boxes with
            // the RAM and disk), subscribe once it works.
            if workload_type == crate::music::WORKLOAD_TXT2MUSIC {
                let net = net.clone();
                let store = self.store.clone();
                let cache_root = cache_root.clone();
                let config = self.config.clone();
                tokio::spawn(async move {
                    if !crate::music::ensure(&config.music).await {
                        info!(
                            "music not available on this node; not subscribing to music.txt2music"
                        );
                        return;
                    }
                    let runner_tx = crate::music::spawn_music_runner(
                        net.clone(),
                        store,
                        cache_root,
                        load.clone(),
                    );
                    info!(
                        workload_type = crate::music::WORKLOAD_TXT2MUSIC,
                        "subscribing to job topic"
                    );
                    // Re-check free memory on every offer: the tag set is
                    // fixed at subscribe time, the box's load is not.
                    let gate = config.music.clone();
                    let filter: dispatch::OfferFilter =
                        std::sync::Arc::new(move |_offer| crate::music::can_serve(&gate));
                    dispatch::run_worker_subscriber_with_load(
                        net,
                        crate::music::WORKLOAD_TXT2MUSIC.to_string(),
                        capabilities::tags_from_config(&config),
                        Some(runner_tx),
                        Some(filter),
                        Some(config.pricing.clone()),
                        Some(load),
                    );
                });
                continue;
            }
            // OCR runs the operator's own Tesseract: subscribe only where it
            // exists, and bid only on languages/formats this box has.
            if workload_type == crate::ocr::WORKLOAD {
                if !crate::ocr::runtime().ready() {
                    info!(
                        %workload_type,
                        "not subscribing: tesseract is not installed (apt install tesseract-ocr poppler-utils)"
                    );
                    continue;
                }
                let runner_tx = crate::ocr::spawn_ocr_runner(
                    net.clone(),
                    self.store.clone(),
                    cache_root.clone(),
                    load.clone(),
                );
                info!(%workload_type, "subscribing to job topic");
                dispatch::run_worker_subscriber_with_load(
                    net.clone(),
                    workload_type,
                    tags.clone(),
                    Some(runner_tx),
                    Some(crate::ocr::offer_filter()),
                    Some(self.config.pricing.clone()),
                    Some(load),
                );
                continue;
            }
            // Game servers: pull the pinned images, then subscribe to both
            // start and stop offers and run the expiry reaper.
            if workload_type == crate::gameserver::WORKLOAD_START {
                tokio::spawn(crate::gameserver::run_worker(
                    net.clone(),
                    self.store.clone(),
                    self.config.clone(),
                    cache_root.join("gameserver"),
                ));
                continue;
            }
            if workload_type == crate::gameserver::WORKLOAD_STOP {
                continue;
            }
            // Rendering runs the operator's own Blender. Probe it (a few
            // seconds: version, then which Cycles GPU backends it sees) off the
            // async threads, then subscribe only if it works.
            if workload_type == crate::render::WORKLOAD {
                let net = net.clone();
                let store = self.store.clone();
                let cache_root = cache_root.clone();
                let config = self.config.clone();
                tokio::spawn(async move {
                    let ready = tokio::task::spawn_blocking(|| crate::render::runtime().ready())
                        .await
                        .unwrap_or(false);
                    if !ready {
                        info!(
                            workload_type = crate::render::WORKLOAD,
                            "not subscribing: blender is not installed (apt install blender, or blender.org)"
                        );
                        return;
                    }
                    let runner_tx = crate::render::spawn_render_runner(
                        net.clone(),
                        store,
                        cache_root,
                        load.clone(),
                    );
                    info!(workload_type = crate::render::WORKLOAD, "subscribing to job topic");
                    dispatch::run_worker_subscriber_with_load(
                        net,
                        crate::render::WORKLOAD.to_string(),
                        capabilities::tags_from_config(&config),
                        Some(runner_tx),
                        Some(crate::render::offer_filter()),
                        Some(config.pricing.clone()),
                        Some(load),
                    );
                });
                continue;
            }
            // Embeddings: provision fastembed in the background (never blocks
            // the worker), subscribe once a model answers. Same shape as TTS.
            if workload_type == crate::embed::WORKLOAD {
                let net = net.clone();
                let store = self.store.clone();
                let config = self.config.clone();
                tokio::spawn(async move {
                    if !crate::embed::ensure(&config.embed).await {
                        info!("no embedding model installed; not subscribing to embed.text");
                        return;
                    }
                    let runner_tx = crate::embed::spawn_embed_runner(net.clone(), store, load.clone());
                    info!(workload_type = crate::embed::WORKLOAD, "subscribing to job topic");
                    dispatch::run_worker_subscriber_with_load(
                        net,
                        crate::embed::WORKLOAD.to_string(),
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
                crate::crawl::WORKLOAD => Some(crate::crawl::spawn_crawl_runner(
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

        // Hosting: `hosting` nodes hold copies; every gateway serves sites,
        // proxying from holders. Either role starts the service.
        let hosting = if self.config.roles.contains(&c0mpute_proto::Role::Hosting)
            || self.config.roles.contains(&c0mpute_proto::Role::Gateway)
        {
            match self.start_hosting().await {
                Ok(h) => Some(h),
                Err(e) => {
                    tracing::warn!(err = %e, "hosting: not started");
                    None
                }
            }
        } else {
            None
        };

        // Ads: every node that runs hosting keeps the campaign index and
        // relays c0mpute/ads/v1 (gossipsub forwards only through subscribed
        // peers); only gateways answer ad requests.
        let gateway = self.config.roles.contains(&c0mpute_proto::Role::Gateway);
        let ads = match &hosting {
            Some(h) => match self.start_ads(h.clone(), gateway).await {
                Ok(a) => Some(a),
                Err(e) => {
                    tracing::warn!(err = %e, "ads: not started");
                    None
                }
            },
            None => None,
        };

        // Live streams: every `live-stream` node relays (verifies records,
        // holds and re-serves the segments it fetched); a gateway also serves
        // the playlists and segments over HTTP.
        let live = if self.config.roles.contains(&c0mpute_proto::Role::LiveStream) {
            match crate::live::LiveStream::start(
                self.libp2p.clone(),
                self.store.clone(),
                crate::live::Timing::default(),
            )
            .await
            {
                Ok(l) => Some(l),
                Err(e) => {
                    tracing::warn!(err = %e, "live-stream: not started");
                    None
                }
            }
        } else {
            None
        };

        if self.config.roles.contains(&c0mpute_proto::Role::Gateway) {
            let bind: std::net::SocketAddr = self.config.gateway.bind.parse()?;
            let state = c0mpute_gateway::GatewayState {
                store: self.store.clone(),
                net: self.net.clone(),
            };
            let mut app = c0mpute_gateway::router(state);
            if self.config.roles.contains(&c0mpute_proto::Role::Storage) {
                app = app.merge(self.storage_router());
            }
            if let Some(l) = &live {
                app = app.merge(l.router());
            }
            if let Some(a) = &ads {
                app = app.merge(a.router());
            }
            if let Some(h) = &hosting {
                app = app.merge(h.router());
            }
            tokio::spawn(async move {
                if let Err(e) = c0mpute_gateway::serve_router(app, bind).await {
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

    /// Start the hosting service: gossip, replica map, self-healing copies.
    pub async fn start_hosting(&self) -> Result<crate::hosting::Hosting> {
        let hosts = self.config.roles.contains(&c0mpute_proto::Role::Hosting);
        let dir = self.config.storage.root.join("hosting");
        let catalog_path = Some(dir.join("catalog.json"));
        let cfg = &self.config.hosting;
        // Receipts are signed with this machine's publisher key: the key that
        // owns (and pays for) the sites published from here.
        let challenge = crate::hosting::ChallengeScope::parse(&cfg.challenge);
        let challenger = if challenge == crate::hosting::ChallengeScope::Off || self.config.private.enabled {
            None
        } else {
            crate::hosting::publisher_keypair().ok()
        };
        let hosting = crate::hosting::Hosting::start(
            self.libp2p.clone(),
            self.store.clone(),
            crate::hosting::Options {
                hosts,
                max_bytes: cfg.max_bytes,
                catalog_path,
                challenger,
                challenge,
                challenge_every: std::time::Duration::from_secs(cfg.challenge_every_secs.max(30)),
                price: Some(cfg.price_micros()),
                payout: cfg.payee(),
                receipts_path: Some(dir.join("receipts.jsonl")),
                ..Default::default()
            },
        )
        .await?;
        self.spawn_settlement(hosting.clone(), dir);
        Ok(hosting)
    }

    /// Settle with the preferred holders of the sites this machine pays for,
    /// once per `hosting.settle_every_secs`. With `[payments]` off (the
    /// default) it only logs what it would pay; nothing moves.
    fn spawn_settlement(&self, hosting: crate::hosting::Hosting, dir: std::path::PathBuf) {
        if hosting.challenger_key().is_none() {
            return;
        }
        let every = self.config.hosting.settle_every_secs.max(60);
        let payments = self.config.payments.clone();
        let rail = match crate::payments::rail_from_config(&payments) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(err = %e, "payments: misconfigured, hosting settlement stays dry-run");
                None
            }
        };
        let ledger = std::sync::Arc::new(crate::payments::Ledger::open(dir.join("ledger.json")));
        let cursor_path = dir.join("settled-until");
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(every)).await;
                let now = crate::payments::now_secs();
                let from = std::fs::read_to_string(&cursor_path)
                    .ok()
                    .and_then(|s| s.trim().parse::<u64>().ok())
                    .filter(|f| *f < now)
                    .unwrap_or(now.saturating_sub(every));
                match &rail {
                    Some(rail) => {
                        let (dues, outcomes) = hosting
                            .settle_round(rail.as_ref(), &ledger, from, now, payments.round_cap())
                            .await;
                        for (t, o) in &outcomes {
                            tracing::info!(key = %t.key, amount = %crate::payments::format_usd(t.amount), wallet = %t.payee.wallet, outcome = ?o, "hosting settlement");
                        }
                        tracing::debug!(dues = dues.len(), "hosting settlement round");
                        // Always move on: windows never overlap, so a key is
                        // never paid twice. A failed transfer is logged above
                        // and not retried (an underpayment, never a double one).
                        let _ = std::fs::write(&cursor_path, now.to_string());
                    }
                    None => {
                        for d in hosting.dues(from, now).await.into_iter().filter(|d| d.amount > 0) {
                            tracing::info!(
                                site = %d.site_id,
                                holder = %d.holder,
                                amount = %crate::payments::format_usd(d.amount),
                                passed = d.passed,
                                challenged = d.challenged,
                                "hosting settlement (dry run: [payments] is off, nothing paid)"
                            );
                        }
                        let _ = std::fs::write(&cursor_path, now.to_string());
                    }
                }
            }
        });
    }

    /// Start the ads service on a gateway: campaign index, VAST/VMAP, and
    /// podcast ad insertion when ffmpeg is here.
    pub async fn start_ads(&self, hosting: crate::hosting::Hosting, gateway: bool) -> Result<crate::ads::Ads> {
        let identity_dir = config::config_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
        let signer = c0mpute_net::identity::load_or_create(&identity_dir).ok();
        let cfg = &self.config.ads;
        let serve = gateway && cfg.serve;
        let (ffmpeg, ffprobe) = if serve && cfg.podcast_insertion {
            (crate::ads::stitch::find_tool("ffmpeg"), crate::ads::stitch::find_tool("ffprobe"))
        } else {
            (None, None)
        };
        let ads = crate::ads::Ads::start(
            self.libp2p.clone(),
            hosting,
            crate::ads::Options {
                signer,
                data_dir: Some(self.config.storage.root.join("ads")),
                serve,
                ffmpeg,
                ffprobe,
                public_origin: crate::opendisk::public_origin(&self.config),
                payout: cfg.payout.clone(),
                min_claim_micros: crate::ads::campaign::parse_usd_micros(&cfg.min_claim_usd)
                    .unwrap_or(crate::ads::billing::DEFAULT_MIN_CLAIM_MICROS),
                ..Default::default()
            },
        )
        .await?;
        if serve {
            // Without ffmpeg the hook still answers ?ads=1, with the clean
            // episode and an x-c0mpute-ads header saying why.
            ads.install_podcast_hook();
        }
        info!(
            serve,
            podcast_insertion = ads.can_stitch(),
            "ads: campaign index (gateways serve /ads/v1)"
        );
        Ok(ads)
    }

    /// The storage API (CIP-002) and this node's OpenDisk descriptor, served
    /// by the gateway on every node with the `storage` role.
    ///
    /// Reads are open: a blake3 hash is the capability, and private-tier
    /// objects are ciphertext. Writes must be signed by this node's own
    /// identity or a key the operator listed in `storage.trusted_writers`,
    /// so a stranger cannot fill the disk.
    fn storage_router(&self) -> axum::Router {
        use axum::routing::get;
        use c0mpute_gateway::auth::SignedEnvelope;
        use c0mpute_gateway::storage_api::{self, Limits, StorageApiState};

        let peer = self.libp2p.peer_id();
        let key_hex = c0mpute_net::identity::ed25519_public_hex(&peer);
        let mut writers: Vec<&str> = self
            .config
            .storage
            .trusted_writers
            .iter()
            .map(String::as_str)
            .collect();
        if let Some(own) = key_hex.as_deref() {
            writers.push(own);
        }
        let (auth, rejected) = SignedEnvelope::from_hex_keys(writers);
        for bad in rejected {
            tracing::warn!(key = %bad, "storage: ignoring trusted_writers entry that is not an ed25519 key");
        }
        let limits = Limits {
            disk_budget_bytes: self.config.storage.cap_bytes,
            ..Limits::default()
        };
        let state = StorageApiState::new(
            c0mpute_store::Storage::new(self.store.clone()),
            Arc::new(auth),
            limits,
        );
        let budget = state.budget.clone();
        let mut app = storage_api::router(state);
        info!("storage API on the gateway: /storage/v1 (signed writes)");

        if self.config.storage.opendisk {
            let config = self.config.clone();
            let peer_id = peer.to_string();
            let origin = crate::opendisk::public_origin(&config);
            app = app.route(
                crate::opendisk::WELL_KNOWN_PATH,
                get(move || {
                    let config = config.clone();
                    let peer_id = peer_id.clone();
                    let key_hex = key_hex.clone();
                    let origin = origin.clone();
                    let budget = budget.clone();
                    async move {
                        let cap = crate::opendisk::capacity(&config.storage, budget.used());
                        let doc = crate::opendisk::descriptor(
                            &config,
                            &peer_id,
                            key_hex.as_deref(),
                            origin.as_deref(),
                            cap,
                        );
                        (
                            [(axum::http::header::CACHE_CONTROL, "public, max-age=300")],
                            axum::Json(doc),
                        )
                    }
                }),
            );
            crate::opendisk::spawn_self_listing(&self.config);
        }
        app
    }
}

/// The ffmpeg on PATH, if any. A node with one serves transcodes by default.
pub fn which_ffmpeg() -> Option<std::path::PathBuf> {
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
