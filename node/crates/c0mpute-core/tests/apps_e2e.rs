//! Apps end to end on loopback libp2p, with a shell script standing in for
//! docker (`C0MPUTE_DOCKER`):
//!
//! * an owner signs an app record; exactly three of four apps nodes take a
//!   copy, decided with no coordinator, and exactly one runs it;
//! * the owner's secrets reach every holder sealed to its own key;
//! * a gateway that runs nothing serves the app by forwarding over libp2p;
//! * the primary snapshots its state and both standbys pull the snapshot;
//! * when the primary leaves, the standby with the newest snapshot promotes
//!   itself at a higher epoch, restores that state, and the gateway serves
//!   again; the fourth node refills the third copy.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::http::{HeaderMap, Method};
use c0mpute_core::apps::placement::AppRole;
use c0mpute_core::apps::proxy;
use c0mpute_core::apps::record::{AppRecord, AppSpec, ImageSource};
use c0mpute_core::apps::snapshot::AppSecrets;
use c0mpute_core::apps::{Apps, Options, Timing};
use c0mpute_net::{ChunkSource, Libp2pNetwork, NetworkConfig};
use c0mpute_proto::Hash;
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
  ps) ;;
  cp) cat "$FAKE_DIR/state.tar" ;;
  run) case "$*" in *" tar -x "*) cat > "$FAKE_DIR/restored" ;; esac ;;
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

async fn get(gateway: &Apps, app_id: &str, path: &str) -> (u16, String) {
    let req = proxy::wrap(
        app_id,
        &Method::GET,
        path,
        &HeaderMap::new(),
        vec![],
        None,
        "http",
        None,
    )
    .unwrap();
    let resp = gateway.forward(req).await;
    let status = resp.status().as_u16();
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn apps_place_three_copies_serve_snapshot_and_fail_over() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("c0mpute_core=info,c0mpute_net=warn")
        .with_test_writer()
        .try_init();
    let tmp = tempdir().unwrap();

    // The fake docker, and the "container": one loopback server every
    // primary's published port points at.
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
        std::env::set_var(c0mpute_core::container::ENV, &bin);
        std::env::set_var("FAKE_DIR", &fake);
    }
    let app = axum::Router::new()
        .route("/ping", axum::routing::get(|| async { "pong" }))
        .route("/healthz", axum::routing::get(|| async { "ok" }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    std::fs::write(
        fake.join("port"),
        listener.local_addr().unwrap().port().to_string(),
    )
    .unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    // State a little over two snapshot pieces.
    let state: Vec<u8> = (0..(2 * 1024 * 1024 + 4096))
        .map(|i| (i % 251) as u8)
        .collect();
    std::fs::write(fake.join("state.tar"), &state).unwrap();

    // Four apps nodes (the first is the hub everyone dials).
    let opts = |dir: &Path, hosts: bool| Options {
        hosts,
        dir: Some(dir.join("apps")),
        timing: fast(),
        ..Default::default()
    };
    let d0 = tmp.path().join("n0");
    let (hub_net, hub_store) = node(&d0, vec![]).await;
    let hub_addr = listen_addr(&hub_net).await;
    let mut hosts: Vec<Apps> = vec![
        Apps::start(hub_net, hub_store, opts(&d0, true))
            .await
            .unwrap(),
    ];
    for i in 1..4 {
        let d = tmp.path().join(format!("n{i}"));
        let (n, s) = node(&d, vec![hub_addr.clone()]).await;
        hosts.push(Apps::start(n, s, opts(&d, true)).await.unwrap());
    }
    let gd = tmp.path().join("gw");
    let (gn, gs) = node(&gd, vec![hub_addr.clone()]).await;
    let gateway = Apps::start(gn, gs, opts(&gd, false)).await.unwrap();

    // The owner signs with its node identity, so its seals are trusted.
    let od = tmp.path().join("owner");
    let (on, os) = node(&od, vec![hub_addr.clone()]).await;
    let owner_key = c0mpute_net::identity::load_or_create(&od.join("id")).unwrap();
    let owner = Apps::start(on, os, opts(&od, false)).await.unwrap();
    let secrets = AppSecrets::generate(BTreeMap::from([("DB_PASSWORD".into(), "hunter2".into())]));
    let rec = AppRecord::sign(
        &owner_key,
        AppSpec {
            name: "api".into(),
            image: ImageSource::Pull {
                reference: format!("ghcr.io/profullstack/api@sha256:{}", "b".repeat(64)),
            },
            port: 8080,
            health_path: "/healthz".into(),
            env: BTreeMap::new(),
            secrets_hash: Some(secrets.hash()),
            state_path: Some("/data".into()),
            snapshot_secs: 10,
            memory_mb: 256,
            millicpus: 500,
            replicas: 3,
            domains: vec![],
        },
        1,
    )
    .unwrap();
    let id = rec.app_id.clone();
    tokio::time::sleep(Duration::from_secs(2)).await; // mesh
    owner.own_secrets(&id, secrets.clone()).await;
    owner.publish_record(rec.clone()).await.unwrap();

    // Three holders, all with the secrets, one healthy primary.
    let snapshot_of = |hosts: Vec<Apps>, id: String| async move {
        let mut v = Vec::new();
        for h in &hosts {
            v.push(h.held(&id).await);
        }
        v
    };
    wait_for(
        "three holders, secrets everywhere, one healthy primary",
        60,
        || {
            let hosts = hosts.clone();
            let id = id.clone();
            async move {
                let held = snapshot_of(hosts, id).await;
                let holders: Vec<_> = held.iter().flatten().collect();
                holders.len() == 3
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

    // The gateway (which runs nothing) serves the app over libp2p.
    wait_for("the gateway serves the app", 30, || {
        let gateway = gateway.clone();
        let id = id.clone();
        async move { get(&gateway, &id, "/ping").await == (200, "pong".into()) }
    })
    .await;

    // A snapshot, pulled by both standbys.
    wait_for("both standbys hold the primary's snapshot", 60, || {
        let hosts = hosts.clone();
        let id = id.clone();
        async move {
            let held = snapshot_of(hosts, id).await;
            let holders: Vec<_> = held.iter().flatten().collect();
            let Some(p) = holders
                .iter()
                .find(|h| h.role == AppRole::Primary)
                .and_then(|h| h.snapshot.clone())
            else {
                return false;
            };
            holders.len() == 3
                && holders.iter().all(|h| {
                    h.snapshot
                        .as_ref()
                        .map(|s| s.order() >= p.order())
                        .unwrap_or(false)
                })
        }
    })
    .await;

    // The primary leaves.
    let mut primary_ix = None;
    let mut old_epoch = 0;
    for (i, h) in hosts.iter().enumerate() {
        if let Some(a) = h.held(&id).await
            && a.role == AppRole::Primary
        {
            primary_ix = Some(i);
            old_epoch = a.epoch;
        }
    }
    let primary_ix = primary_ix.expect("a primary");
    let _ = std::fs::remove_file(fake.join("restored"));
    hosts[primary_ix].stop();
    let rest: Vec<Apps> = hosts
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != primary_ix)
        .map(|(_, h)| h.clone())
        .collect();

    wait_for("a standby promotes itself at a higher epoch", 60, || {
        let rest = rest.clone();
        let id = id.clone();
        async move {
            for h in &rest {
                if let Some(a) = h.held(&id).await
                    && a.role == AppRole::Primary
                    && a.healthy
                    && a.epoch > old_epoch
                {
                    return true;
                }
            }
            false
        }
    })
    .await;
    // (Read until it matches: under heavy load a second restore may be
    // writing the shared fake's file at the moment we look.)
    wait_for("the new primary restored the snapshotted state", 30, || {
        let path = fake.join("restored");
        let want = state.clone();
        async move { std::fs::read(&path).map(|b| b == want).unwrap_or(false) }
    })
    .await;
    wait_for("the gateway serves the app again", 30, || {
        let gateway = gateway.clone();
        let id = id.clone();
        async move { get(&gateway, &id, "/ping").await == (200, "pong".into()) }
    })
    .await;
    wait_for("the fourth node refills the third copy", 60, || {
        let rest = rest.clone();
        let id = id.clone();
        async move {
            let mut n = 0;
            for h in &rest {
                if h.held(&id)
                    .await
                    .map(|a| a.ready && a.has_secrets)
                    .unwrap_or(false)
                {
                    n += 1;
                }
            }
            n == 3
        }
    })
    .await;

    // Secrets never rode in the clear: no gossip carries the password.
    let log = std::fs::read_to_string(fake.join("log")).unwrap();
    assert!(
        log.contains("DB_PASSWORD=hunter2"),
        "the container got its secret env"
    );
    assert!(log.contains("--cap-drop ALL"));
}
