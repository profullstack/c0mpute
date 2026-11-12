//! Game servers end to end on loopback libp2p, with a fake container runtime.
//!
//! The workers, buyer, auction, sealed handoff, encrypted backups, status
//! gossip, health probes and failover are the shipping code. Only
//! [`gameserver::Runtime`] is a double: a "volume" is a byte blob, and a
//! started Minecraft "container" is a TCP listener on the allocated port
//! that answers the real Minecraft status ping, so the buyer's probe has
//! something true to test.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use c0mpute_core::gameserver::{self, ContainerPlan, Game, Runtime, ServerState, StartRequest, WatchEnd, WatchOptions};
use c0mpute_core::pricing::PricingConfig;
use c0mpute_net::topics::job_topic;
use c0mpute_net::{ChunkSource, Libp2pNetwork, NetworkConfig};
use c0mpute_proto::Hash;
use c0mpute_store::ChunkStore;
use libp2p::multiaddr::Protocol;
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;

struct StoreSource(ChunkStore);

#[async_trait::async_trait]
impl ChunkSource for StoreSource {
    async fn read_chunk(&self, hash: &Hash) -> anyhow::Result<Vec<u8>> {
        self.0.get(hash).await
    }
}

/// A world: 2.5 MiB of incompressible bytes, so it spans three chunks.
fn world(seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    let mut v = Vec::with_capacity(2_621_440);
    while v.len() < 2_621_440 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.extend_from_slice(&x.to_le_bytes());
    }
    v
}

#[derive(Default)]
struct Fake {
    seed: Vec<u8>,
    plans: Mutex<Vec<ContainerPlan>>,
    vols: Mutex<HashMap<String, Vec<u8>>>,
    live: Mutex<HashMap<String, JoinHandle<()>>>,
    labelled: Mutex<Vec<(String, u64)>>,
    killed: Mutex<Vec<String>>,
    restored: Mutex<Vec<String>>,
}

impl Fake {
    fn new(seed: Vec<u8>) -> Arc<Self> {
        Arc::new(Self { seed, ..Default::default() })
    }
    fn plan(&self, name: &str) -> Option<ContainerPlan> {
        self.plans.lock().unwrap().iter().rev().find(|p| p.name == name).cloned()
    }
    /// The server process dies (host trouble, OOM, the worker went away).
    fn crash(&self, name: &str) {
        if let Some(h) = self.live.lock().unwrap().remove(name) {
            h.abort();
        }
    }
}

#[async_trait::async_trait]
impl Runtime for Fake {
    async fn create(&self, plan: &ContainerPlan) -> anyhow::Result<()> {
        self.plans.lock().unwrap().push(plan.clone());
        self.vols.lock().unwrap().entry(plan.volume.clone()).or_insert_with(|| self.seed.clone());
        let exp = plan.labels.iter().find(|(k, _)| k == "c0mpute.gameserver.expires").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
        self.labelled.lock().unwrap().push((plan.name.clone(), exp));
        Ok(())
    }
    async fn restore(&self, plan: &ContainerPlan, tar: &Path) -> anyhow::Result<()> {
        self.vols.lock().unwrap().insert(plan.volume.clone(), std::fs::read(tar)?);
        self.restored.lock().unwrap().push(plan.volume.clone());
        Ok(())
    }
    async fn start(&self, name: &str) -> anyhow::Result<()> {
        let plan = self.plan(name).ok_or_else(|| anyhow::anyhow!("no container {name}"))?;
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", plan.host_port)).await?;
        let h = tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 512];
                    let _ = s.read(&mut buf).await;
                    let _ = s.write_all(&gameserver::minecraft_status_response(r#"{"version":{"name":"fake 1.21"},"players":{"online":1,"max":8}}"#)).await;
                });
            }
        });
        self.live.lock().unwrap().insert(name.to_string(), h);
        Ok(())
    }
    async fn running(&self, name: &str) -> bool {
        self.live.lock().unwrap().get(name).is_some_and(|h| !h.is_finished())
    }
    async fn flush(&self, _name: &str, _game: Game) {}
    async fn stop(&self, name: &str) -> anyhow::Result<()> {
        self.crash(name);
        Ok(())
    }
    async fn tar_world(&self, volume: &str, _game: Game, out: &Path) -> anyhow::Result<()> {
        let v = self.vols.lock().unwrap().get(volume).cloned().ok_or_else(|| anyhow::anyhow!("no volume {volume}"))?;
        std::fs::write(out, v)?;
        Ok(())
    }
    async fn remove(&self, name: &str, volume: &str) {
        self.crash(name);
        self.vols.lock().unwrap().remove(volume);
        self.labelled.lock().unwrap().retain(|(n, _)| n != name);
    }
    async fn labelled(&self) -> anyhow::Result<Vec<(String, u64)>> {
        Ok(self.labelled.lock().unwrap().clone())
    }
    async fn kill(&self, name: &str) {
        self.crash(name);
        self.killed.lock().unwrap().push(name.to_string());
        self.labelled.lock().unwrap().retain(|(n, _)| n != name);
    }
}

async fn node(dir: &Path, bootstrap: Vec<libp2p::Multiaddr>) -> (Arc<Libp2pNetwork>, ChunkStore) {
    let store = ChunkStore::open(dir.join("store")).await.unwrap();
    let net = Libp2pNetwork::spawn(
        NetworkConfig::for_dir(dir.join("id"))
            .with_listen(vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()])
            .with_local_source(Arc::new(StoreSource(store.clone())))
            .with_bootstrap(bootstrap),
    )
    .await
    .unwrap();
    (Arc::new(net), store)
}

async fn wait_for_listen(net: &Libp2pNetwork) -> libp2p::Multiaddr {
    for _ in 0..50 {
        if let Ok(addrs) = net.listen_addrs().await
            && let Some(a) = addrs.into_iter().next()
        {
            return a;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no listen addr after 2.5s");
}

fn cfg(port_min: u16) -> c0mpute_core::config::GameserverConfig {
    c0mpute_core::config::GameserverConfig { port_min, port_max: port_min + 9, max_servers: 2, max_total_ram_gb: 16, max_hours: 24, ..Default::default() }
}

/// A gameserver worker over `rt`, serving on loopback.
async fn worker(dir: &Path, bootstrap: Vec<libp2p::Multiaddr>, port_min: u16, rt: Arc<Fake>) -> Arc<gameserver::Worker> {
    let (net, store) = node(dir, bootstrap).await;
    let w = gameserver::Worker::with_runtime(net, store, cfg(port_min), "127.0.0.1".into(), &dir.join("state"), rt);
    let tags = vec![gameserver::ROLE_TAG.to_string(), Game::Minecraft.tag(), Game::Valheim.tag()];
    gameserver::subscribe(w.clone(), tags, Some(PricingConfig { min_usd: 0.0, ..PricingConfig::default() }));
    w
}

/// Everything a bystander node sees on the job and status topics.
async fn bystander(dir: &Path, bootstrap: Vec<libp2p::Multiaddr>) -> (Arc<Libp2pNetwork>, Arc<Mutex<Vec<u8>>>) {
    let (net, _) = node(dir, bootstrap).await;
    net.subscribe(&job_topic(gameserver::WORKLOAD_START)).await.unwrap();
    net.subscribe(gameserver::STATUS_TOPIC).await.unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut rx = net.messages();
    let s = seen.clone();
    tokio::spawn(async move {
        while let Ok(m) = rx.recv().await {
            let mut g = s.lock().unwrap();
            g.extend_from_slice(&m.data);
            g.push(b'\n');
        }
    });
    (net, seen)
}

fn req(game: Game, password: Option<&str>, workers: Vec<String>) -> StartRequest {
    StartRequest {
        game,
        hours: 2,
        ram_gb: game.min_ram_gb(),
        cpus: 1.0,
        accept_eula: true,
        world: None,
        password: password.map(str::to_string),
        server_name: None,
        backup_mins: 30,
        max_price_usd: 100.0,
        workers,
        exclude: Vec::new(),
        timeout: Duration::from_secs(120),
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// Valheim: the password reaches only the winning worker (sealed), the
/// world comes back sealed to the buyer's key, and the stop downloads and
/// decrypts it. A bystander sees neither.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn secrets_are_sealed_and_backups_readable_only_by_the_buyer() {
    let _ = tracing_subscriber::fmt().with_env_filter("c0mpute_core=info,c0mpute_net=warn").with_test_writer().try_init();
    let tmp = tempdir().unwrap();
    let rt = Fake::new(world(7));
    let a = worker(&tmp.path().join("a"), vec![], 41100, rt.clone()).await;
    let a_addr = wait_for_listen(worker_net(&a)).await.with(Protocol::P2p(worker_net(&a).peer_id()));
    let a_id = worker_net(&a).peer_id().to_base58();
    let (eve, seen) = bystander(&tmp.path().join("eve"), vec![a_addr.clone()]).await;
    let (buyer, buyer_store) = node(&tmp.path().join("buyer"), vec![a_addr.clone()]).await;
    let dir = tmp.path().join("buyer-data");
    tokio::time::sleep(Duration::from_secs(2)).await;

    let rental = gameserver::start_with(buyer.clone(), &buyer_store, &dir, req(Game::Valheim, Some("hunter22pw"), vec![a_id.clone()])).await.expect("valheim start");
    assert_eq!(rental.worker, a_id);
    assert!(rental.address.starts_with("127.0.0.1:"));
    let plan = rt.plan(&format!("c0mpute-{}", rental.id)).unwrap();
    assert!(plan.env.iter().any(|(k, v)| k == "SERVER_PASS" && v == "hunter22pw"), "the worker got the password through the handoff");
    assert!(plan.run_secs > 7000 && plan.run_secs <= 7200, "the container stops itself at expiry: {}", plan.run_secs);

    // A live backup, announced in a status record the bystander can see.
    let live = a.backup_now(&rental.id).await.unwrap();
    a.publish_status().await;

    let stopped = gameserver::stop_with(buyer.clone(), &buyer_store, &dir, &rental.id, Duration::from_secs(60)).await.expect("stop");
    let manifest = stopped.backup.clone().expect("stop returns a backup");
    assert_ne!(manifest, live.manifest, "the final backup is a new one");
    let tar = std::fs::read(dir.join("worlds").join(format!("{manifest}.tar"))).unwrap();
    assert!(tar == world(7), "the buyer decrypted exactly the world");
    assert!(rt.vols.lock().unwrap().is_empty(), "the volume is removed after a good backup");

    // Eve has every gossip message and can fetch every chunk, and learns nothing.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let wire = seen.lock().unwrap().clone();
    assert!(contains(&wire, b"handoff_ready") && contains(&wire, b"handoff_secret"), "eve saw the sealed exchange");
    assert!(contains(&wire, live.manifest.as_bytes()), "eve saw the backup hash in a status record");
    assert!(!contains(&wire, b"hunter22pw"), "the password leaked onto gossip");
    let mb = eve.fetch_from_peer(worker_net(&a).peer_id(), &c0mpute_proto::ChunkRequest { chunk_hash: Hash::from_hex(&manifest).unwrap(), shard_index: None }).await.unwrap();
    let m = gameserver::WorldManifest::from_bytes(&mb).unwrap();
    assert!(m.enc.epk.is_some());
    let first = eve.fetch_from_peer(worker_net(&a).peer_id(), &c0mpute_proto::ChunkRequest { chunk_hash: Hash::from_hex(&m.chunks[0]).unwrap(), shard_index: None }).await.unwrap();
    assert!(!contains(&first, &world(7)[..64]), "chunks are ciphertext");
    assert!(gameserver::open_chunk(&gameserver::random_key(), 0, &first).is_err());

    // The rentals file (token, password) is private.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(dir.join("rentals.json")).unwrap().permissions().mode() & 0o777, 0o600);
    }
}

/// Minecraft on worker A: the buyer's watch probes it, mirrors A's backup,
/// and when the server stops answering it starts the same world on worker B
/// from that backup (handed to B under a fresh key).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watch_fails_over_to_another_worker_from_the_latest_backup() {
    let _ = tracing_subscriber::fmt().with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn,c0mpute_core::buyer=info,c0mpute_core::dispatch=info,c0mpute_core::gameserver=info,c0mpute_core::handoff=debug,c0mpute_net=debug".into())).with_test_writer().try_init();
    let tmp = tempdir().unwrap();
    let rt_a = Fake::new(world(11));
    let rt_b = Fake::new(world(99));
    let a = worker(&tmp.path().join("a"), vec![], 41200, rt_a.clone()).await;
    let a_addr = wait_for_listen(worker_net(&a)).await.with(Protocol::P2p(worker_net(&a).peer_id()));
    let a_id = worker_net(&a).peer_id().to_base58();
    let b = worker(&tmp.path().join("b"), vec![a_addr.clone()], 41210, rt_b.clone()).await;
    let b_id = worker_net(&b).peer_id().to_base58();
    let (buyer, buyer_store) = node(&tmp.path().join("buyer"), vec![a_addr.clone()]).await;
    let dir = tmp.path().join("buyer-data");
    tokio::time::sleep(Duration::from_secs(3)).await;

    let rental = gameserver::start_with(buyer.clone(), &buyer_store, &dir, req(Game::Minecraft, None, vec![a_id.clone()])).await.expect("minecraft start");
    assert_eq!(rental.worker, a_id);
    let p = gameserver::probe_server(Game::Minecraft, &rental.address, Duration::from_secs(3)).await;
    assert!(p.ok && p.detail.contains("fake 1.21"), "{p:?}");

    let opts = WatchOptions {
        probe_every: Duration::from_secs(1),
        probe_timeout: Duration::from_secs(1),
        fail_after: Duration::from_secs(4),
        max_price_usd: 100.0,
        start_timeout: Duration::from_secs(120),
        stop_after_failover: true,
        ..Default::default()
    };
    let watch = {
        let (buyer, store, dir, id) = (buyer.clone(), buyer_store.clone(), dir.clone(), rental.id.clone());
        tokio::spawn(async move { gameserver::watch_with(buyer, &store, &dir, &id, opts).await })
    };

    // A's 30-minute backup, then its status records until the watch mirrors it.
    let backup = a.backup_now(&rental.id).await.unwrap();
    let mut mirrored = false;
    for _ in 0..40 {
        a.publish_status().await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        if gameserver::load_rentals(&dir).iter().any(|r| r.id == rental.id && r.latest_backup.as_deref() == Some(backup.manifest.as_str())) {
            mirrored = true;
            break;
        }
    }
    assert!(mirrored, "the watch mirrored A's backup");
    assert!(std::fs::read(dir.join("worlds").join(format!("{}.tar", backup.manifest))).unwrap() == world(11));
    assert!(gameserver::health_log(&dir, &rental.id).iter().any(|p| p.ok), "healthy probes were logged");

    // The server stops answering (and A says nothing more).
    rt_a.crash(&format!("c0mpute-{}", rental.id));
    let end = tokio::time::timeout(Duration::from_secs(150), watch).await.expect("watch returns").unwrap().expect("watch ok");
    let WatchEnd::FailedOver { from, to } = end else { panic!("expected a failover, got {end:?}") };
    assert_eq!(from.id, rental.id);
    assert_eq!(to.worker, b_id, "the replacement runs on the other worker");
    assert_eq!(to.replaces.as_deref(), Some(rental.id.as_str()));
    let log = gameserver::health_log(&dir, &rental.id);
    assert!(log.iter().any(|p| !p.ok), "failed probes were logged");

    // B restored A's world (not its own seed) and is serving it.
    let vol = format!("c0mpute-{}", to.id);
    assert_eq!(rt_b.restored.lock().unwrap().as_slice(), &[vol.clone()]);
    assert!(rt_b.vols.lock().unwrap().get(&vol).unwrap() == &world(11));
    assert!(gameserver::probe_server(Game::Minecraft, &to.address, Duration::from_secs(3)).await.ok);
    let all = gameserver::load_rentals(&dir);
    assert_eq!(all.iter().find(|r| r.id == rental.id).unwrap().superseded_by.as_deref(), Some(to.id.as_str()));
    drop(b);
}

/// The worker was offline when a reservation ended: at its next sweep it
/// kills the expired container, backs the world up from the volume (sealed
/// to the buyer) and removes the volume.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expiry_missed_while_offline_is_enforced_at_the_next_sweep() {
    let tmp = tempdir().unwrap();
    let state_dir = tmp.path().join("w").join("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    let now = gameserver::now_secs();
    let id = gameserver::server_id("job-expired");
    let buyer_key = c0mpute_core::temporal::crypto::keypair();
    let st = ServerState {
        id: id.clone(),
        game: Game::Factorio,
        container: format!("c0mpute-{id}"),
        volume: format!("c0mpute-{id}"),
        ram_mb: 2048,
        host_port: 41300,
        address: "127.0.0.1:41300".into(),
        started_at: now - 7200,
        expires_at: now - 60,
        stop_hash: gameserver::sha256_ref(b"t"),
        buyer: "buyer".into(),
        backup_pubkey: buyer_key.public_hex.clone(),
        backup_every_secs: 1800,
        stopped_at: None,
        backup: None,
        backup_error: None,
    };
    let servers: HashMap<String, ServerState> = [(id.clone(), st.clone())].into_iter().collect();
    std::fs::write(state_dir.join("servers.json"), serde_json::to_vec(&servers).unwrap()).unwrap();

    let rt = Fake::new(world(5));
    rt.vols.lock().unwrap().insert(st.volume.clone(), world(5));
    rt.labelled.lock().unwrap().push((st.container.clone(), now - 60));
    rt.labelled.lock().unwrap().push(("c0mpute-gs-ffffffffffffffff".into(), now + 3600));
    let (net, store) = node(&tmp.path().join("w"), vec![]).await;
    let w = gameserver::Worker::with_runtime(net, store.clone(), cfg(41300), "127.0.0.1".into(), &state_dir, rt.clone());

    assert_eq!(w.sweep_expired().await, vec![st.container.clone()], "only the expired container dies");
    w.reap_once().await;
    let after = w.snapshot().get(&id).cloned().unwrap();
    assert!(after.stopped_at.is_some());
    let b = after.backup.expect("backed up from the volume");
    assert!(rt.vols.lock().unwrap().get(&st.volume).is_none(), "volume removed");
    // The backup opens with the buyer's key and nobody else's.
    let m = gameserver::WorldManifest::from_bytes(&store.get(&Hash::from_hex(&b.manifest).unwrap()).await.unwrap()).unwrap();
    let epk = m.enc.epk.clone().unwrap();
    let k = gameserver::backup_key(&buyer_key.secret, &epk, &epk, &buyer_key.public_hex).unwrap();
    let mut plain = Vec::new();
    for (i, c) in m.chunks.iter().enumerate() {
        plain.extend(gameserver::open_chunk(&k, i as u64, &store.get(&Hash::from_hex(c).unwrap()).await.unwrap()).unwrap());
    }
    assert!(plain == world(5));
    // Persisted: a restart does not stop it twice.
    let saved: HashMap<String, ServerState> = serde_json::from_slice(&std::fs::read(state_dir.join("servers.json")).unwrap()).unwrap();
    assert!(saved[&id].stopped_at.is_some());
}

fn worker_net(w: &gameserver::Worker) -> &Libp2pNetwork {
    w.net()
}
