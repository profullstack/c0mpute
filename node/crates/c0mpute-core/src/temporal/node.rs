//! Process-level wiring for `c0mpute infernet temporal serve` and the buyer
//! commands: a libp2p node, capability ads and the two workload subscribers.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use c0mpute_net::{Libp2pNetwork, Multiaddr, NetworkConfig, bootstrap};
use tracing::info;

use super::caps::{self, RuntimeCaps};
use super::worker::{self, InferServer, ModelStore};
use super::{INFER_WORKLOAD, TRAIN_WORKLOAD};
use crate::capabilities;
use crate::dispatch;

#[derive(Clone, Debug)]
pub struct NetOptions {
    pub identity_dir: PathBuf,
    pub listen: Vec<String>,
    pub bootstrap: Vec<String>,
    /// Also dial c0mpute.com's seeds (off for isolated/local runs).
    pub default_bootstrap: bool,
}

pub async fn spawn_node(o: &NetOptions) -> Result<Arc<Libp2pNetwork>> {
    std::fs::create_dir_all(&o.identity_dir)?;
    let parse = |v: &[String]| -> Result<Vec<Multiaddr>> { v.iter().map(|s| s.parse::<Multiaddr>().with_context(|| format!("bad multiaddr {s}"))).collect() };
    let mut boot = parse(&o.bootstrap)?;
    if o.default_bootstrap {
        boot.extend(bootstrap::fetch_or_fallback(bootstrap::DEFAULT_BOOTSTRAP_URL, bootstrap::default_seeds()).await);
    }
    let listen = if o.listen.is_empty() { vec!["/ip4/0.0.0.0/tcp/0".to_string()] } else { o.listen.clone() };
    let cfg = NetworkConfig::for_dir(o.identity_dir.clone()).with_listen(parse(&listen)?).with_bootstrap(boot);
    Ok(Arc::new(Libp2pNetwork::spawn(cfg).await?))
}

#[derive(Clone, Debug)]
pub struct ServeOptions {
    pub net: NetOptions,
    pub infernet: PathBuf,
    /// Model dirs or `sha256:` hashes from the store to serve for inference.
    pub models: Vec<String>,
    /// Accept `infernet.temporal.train` jobs too.
    pub train: bool,
    pub max_sessions: u32,
    pub store: ModelStore,
    pub cache_root: PathBuf,
}

pub struct Serving {
    pub net: Arc<Libp2pNetwork>,
    pub caps: RuntimeCaps,
    pub tags: Vec<String>,
    pub infer: Option<Arc<InferServer>>,
}

/// Start serving. Advertises only what the local Infernet reports it can run.
pub async fn serve(o: &ServeOptions) -> Result<Serving> {
    let caps = caps::probe(&o.infernet).await?;
    if !o.train && o.models.is_empty() {
        bail!("nothing to serve: pass --model <dir|sha256:...> and/or --train");
    }
    let net = spawn_node(&o.net).await?;
    let me = net.peer_id().to_base58();
    let mut resolved = Vec::new();
    for m in &o.models {
        resolved.push(o.store.resolve(m)?);
    }
    let infer = if resolved.is_empty() { None } else { Some(InferServer::start(&o.infernet, &resolved, &me, o.max_sessions).await?) };
    let models: Vec<String> = infer.as_ref().map(|s| s.models.clone()).unwrap_or_default();
    let tags = caps::tags(&caps, &models, o.train, infer.is_some());
    let mut hardware = serde_json::json!({"version": env!("CARGO_PKG_VERSION"), "temporal_runtime": caps.runtime.version, "ram_mb": caps.ram_mb, "cuda": caps.devices.cuda});
    hardware["models"] = serde_json::json!(models);
    tokio::spawn(capabilities::advertise_loop(net.clone(), tags.clone(), hardware, capabilities::local_bench_score(), capabilities::DEFAULT_ADVERTISE_INTERVAL));
    if o.train {
        let busy = Arc::new(AtomicBool::new(false));
        let tx = worker::spawn_train_runner(net.clone(), o.cache_root.clone(), o.infernet.clone(), o.store.clone(), busy.clone());
        dispatch::run_worker_subscriber_filtered(net.clone(), TRAIN_WORKLOAD.into(), tags.clone(), Some(tx), Some(worker::train_offer_filter(busy)));
    }
    if let Some(server) = &infer {
        let tx = worker::spawn_infer_runner(net.clone(), server.clone());
        dispatch::run_worker_subscriber_filtered(net.clone(), INFER_WORKLOAD.into(), tags.clone(), Some(tx), Some(worker::infer_offer_filter(models.clone())));
        worker::spawn_call_listener(net.clone(), server.clone());
    }
    info!(peer = %me, ?tags, "temporal: serving");
    // Let the subscriptions land before callers start publishing.
    tokio::time::sleep(Duration::from_millis(300)).await;
    Ok(Serving { net, caps, tags, infer })
}
