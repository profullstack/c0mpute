//! An app owned by the account key, controlled from a machine that never
//! deployed it. Loopback libp2p, a shell script standing in for docker
//! (`C0MPUTE_DOCKER`), as in apps_e2e.rs.
//!
//! * "machine A" signs the app record with the account key derived from a
//!   c0mpute identity; any node may carry a signed record, so here a
//!   gateway with its own machine key publishes it;
//! * "machine B" holds the same identity and nothing else. It rebuilds the
//!   app key from the identity, joins the network *as the account key*, and
//!   the holders accept its sealed secrets and answer its log request: the
//!   owner-peer check they already make, with no change on their side;
//! * a node joined with a machine key is refused the logs.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use c0mpute_core::apps::owner::AppOwner;
use c0mpute_core::apps::placement::AppRole;
use c0mpute_core::apps::record::{AppRecord, AppSpec, ImageSource};
use c0mpute_core::apps::snapshot::AppSecrets;
use c0mpute_core::apps::{Apps, Options, Timing};
use c0mpute_net::{ChunkSource, Libp2pNetwork, NetworkConfig};
use c0mpute_proto::Hash;
use c0mpute_secure_chat::ChatKey;
use c0mpute_store::ChunkStore;
use libp2p::multiaddr::Protocol;
use tempfile::tempdir;

const FAKE_DOCKER: &str = r#"#!/bin/sh
# Stand-in for the docker CLI (see apps_e2e.rs).
echo "$*" >> "$FAKE_DIR/log"
case "$1" in
  info) echo 27.0 ;;
  image) echo sha256:feed ;;
  port) echo "127.0.0.1:$(cat "$FAKE_DIR/port")" ;;
  inspect) echo true ;;
  logs) echo "app log line" ;;
  *) ;;
esac
exit 0
"#;

struct StoreSource(ChunkStore);

#[async_trait::async_trait]
impl ChunkSource for StoreSource {
    async fn read_chunk(&self, hash: &Hash) -> anyhow::Result<Vec<u8>> {
        self.0.get(hash).await
    }
}

/// A node; with `keypair` it joins as that key instead of one of its own.
async fn node(
    dir: &Path,
    bootstrap: Vec<libp2p::Multiaddr>,
    keypair: Option<libp2p::identity::Keypair>,
) -> (Arc<Libp2pNetwork>, ChunkStore) {
    let store = ChunkStore::open(dir.join("store")).await.unwrap();
    let mut cfg = NetworkConfig::for_dir(dir.join("id"))
        .with_listen(vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()])
        .with_local_source(Arc::new(StoreSource(store.clone())))
        .with_bootstrap(bootstrap);
    if let Some(k) = keypair {
        cfg = cfg.with_keypair(k);
    }
    (Arc::new(Libp2pNetwork::spawn(cfg).await.unwrap()), store)
}

async fn listen_addr(net: &Libp2pNetwork) -> libp2p::Multiaddr {
    for _ in 0..50 {
        if let Ok(addrs) = net.listen_addrs().await
            && let Some(a) = addrs.into_iter().next()
        {
            return a.with(Protocol::P2p(net.peer_id()));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no listen addr");
}

fn fast() -> Timing {
    Timing {
        announce_every: Duration::from_millis(400),
        records_every: Duration::from_millis(1000),
        tick: Duration::from_millis(400),
        node_ttl: Duration::from_secs(6),
        warmup: Duration::from_secs(3),
        fetch_timeout: Duration::from_secs(10),
    }
}

async fn wait_for<F, Fut>(what: &str, secs: u64, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while tokio::time::Instant::now() < deadline {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_account_owned_app_takes_secrets_and_logs_from_any_machine_with_the_identity() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("c0mpute_core=info,c0mpute_net=warn")
        .with_test_writer()
        .try_init();
    let tmp = tempdir().unwrap();

    let fake = tmp.path().join("fake");
    std::fs::create_dir_all(&fake).unwrap();
    let bin = fake.join("docker");
    std::fs::write(&bin, FAKE_DOCKER).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    unsafe {
        // Loopback peers only: never join a live network on this machine.
        std::env::set_var("C0MPUTE_DISABLE_MDNS", "1");
        std::env::set_var(c0mpute_core::container::ENV, &bin);
        std::env::set_var("FAKE_DIR", &fake);
    }
    let app = axum::Router::new().route("/healthz", axum::routing::get(|| async { "ok" }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    std::fs::write(
        fake.join("port"),
        listener.local_addr().unwrap().port().to_string(),
    )
    .unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let opts = |dir: &Path, hosts: bool| Options {
        hosts,
        dir: Some(dir.join("apps")),
        timing: fast(),
        egress_guard: Some(false),
        require_egress_guard: Some(false),
        max_disk_mb: Some(c0mpute_core::apps::DEFAULT_MAX_DISK_MB),
        ..Default::default()
    };
    // Three apps nodes (the first is the hub everyone dials).
    let d0 = tmp.path().join("n0");
    let (hub_net, hub_store) = node(&d0, vec![], None).await;
    let hub_addr = listen_addr(&hub_net).await;
    let mut hosts: Vec<Apps> = vec![
        Apps::start(hub_net, hub_store, opts(&d0, true))
            .await
            .unwrap(),
    ];
    for i in 1..3 {
        let d = tmp.path().join(format!("n{i}"));
        let (n, s) = node(&d, vec![hub_addr.clone()], None).await;
        hosts.push(Apps::start(n, s, opts(&d, true)).await.unwrap());
    }
    // A node with a machine key of its own: carries the record, owns nothing.
    let gd = tmp.path().join("gw");
    let (gn, gs) = node(&gd, vec![hub_addr.clone()], None).await;
    let stranger = Apps::start(gn, gs, opts(&gd, false)).await.unwrap();

    // Machine A: the identity signs the record. It never joins the network.
    let identity = [7u8; 32];
    let secret_env = BTreeMap::from([("DB_PASSWORD".to_string(), "hunter2".to_string())]);
    let rec = {
        let a = AppOwner::account(ChatKey::from_seed(identity)).unwrap();
        let secrets = AppSecrets {
            app_key: a.app_key("api").unwrap(),
            env: secret_env.clone(),
        };
        AppRecord::sign(
            &a.keypair,
            AppSpec {
                name: "api".into(),
                image: ImageSource::Pull {
                    reference: format!("ghcr.io/profullstack/api@sha256:{}", "b".repeat(64)),
                },
                port: 8080,
                health_path: "/healthz".into(),
                env: BTreeMap::new(),
                secrets_hash: Some(secrets.hash()),
                state_path: None,
                snapshot_secs: 30,
                memory_mb: 256,
                millicpus: 500,
                replicas: 2,
                domains: vec![],
                hosts: vec![],
            },
            1,
        )
        .unwrap()
    };
    let id = rec.app_id.clone();

    // Machine B: the same identity on another machine, with no app state.
    // The app id and the app key come from the identity and the name alone.
    let b = AppOwner::account(ChatKey::from_seed(identity)).unwrap();
    assert_eq!(b.app_id("api"), id);
    assert_eq!(rec.owner_peer(), Some(b.peer_id()));
    let od = tmp.path().join("machine-b");
    let (on, os) = node(&od, vec![hub_addr.clone()], Some(b.keypair.clone())).await;
    assert_eq!(
        on.peer_id().to_base58(),
        b.peer_id(),
        "it joins as the account key"
    );
    let owner = Apps::start(on, os, opts(&od, false)).await.unwrap();
    assert_ne!(stranger.peer_id(), b.peer_id());

    tokio::time::sleep(Duration::from_secs(2)).await; // mesh
    stranger.publish_record(rec.clone()).await.unwrap();
    // The secret env is set again on machine B; the key is rebuilt.
    let secrets = AppSecrets {
        app_key: b.app_key("api").unwrap(),
        env: secret_env,
    };
    assert_eq!(Some(secrets.hash()), rec.secrets_hash);
    owner.own_secrets(&id, secrets).await;

    // The holders take the secrets sealed by the account peer.
    wait_for(
        "two holders with the secrets and one healthy primary",
        60,
        || {
            let hosts = hosts.clone();
            let id = id.clone();
            async move {
                let mut held = Vec::new();
                for h in &hosts {
                    held.push(h.held(&id).await);
                }
                let holders: Vec<_> = held.iter().flatten().collect();
                holders.len() == 2
                    && holders.iter().all(|h| h.has_secrets && h.ready)
                    && holders
                        .iter()
                        .filter(|h| h.role == AppRole::Primary && h.healthy)
                        .count()
                        == 1
            }
        },
    )
    .await;
    let log = std::fs::read_to_string(fake.join("log")).unwrap();
    assert!(
        log.contains("DB_PASSWORD=hunter2"),
        "the container got its secret env"
    );

    // The primary answers the account peer's log request ...
    let mut logs = None;
    for _ in 0..40 {
        match owner.logs(&id, 10).await {
            Ok(t) => {
                logs = Some(t);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
    assert!(
        logs.expect("the owner reads the logs")
            .contains("app log line")
    );

    // ... and refuses a peer that is not the owner.
    wait_for("the stranger sees the primary", 30, || {
        let stranger = stranger.clone();
        let id = id.clone();
        async move {
            match stranger.logs(&id, 10).await {
                Ok(_) => panic!("a peer that is not the owner read the logs"),
                Err(e) => format!("{e:#}").contains("only the app's owner"),
            }
        }
    })
    .await;
}
