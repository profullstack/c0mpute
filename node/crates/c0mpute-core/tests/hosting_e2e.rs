//! End-to-end hosting on loopback libp2p: a publisher hands a site to the
//! network, exactly three of five hosting nodes take copies (decided with no
//! coordinator), a gateway that holds nothing serves the site by proxying
//! from holders, and when a holder leaves another node takes its place.

use std::sync::Arc;
use std::time::Duration;

use c0mpute_core::hosting::site::{SiteRecord, build_from_dir};
use c0mpute_core::hosting::{self, Hosting, Options, Timing};
use c0mpute_net::{ChunkSource, Libp2pNetwork, NetworkConfig};
use c0mpute_proto::Hash;
use c0mpute_store::ChunkStore;
use libp2p::multiaddr::Protocol;
use tempfile::tempdir;

struct StoreSource(ChunkStore);

#[async_trait::async_trait]
impl ChunkSource for StoreSource {
    async fn read_chunk(&self, hash: &Hash) -> anyhow::Result<Vec<u8>> {
        self.0.get(hash).await
    }
}

async fn node(dir: &std::path::Path, bootstrap: Vec<libp2p::Multiaddr>) -> (Arc<Libp2pNetwork>, ChunkStore) {
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
        announce_every: Duration::from_millis(700),
        records_every: Duration::from_millis(1500),
        repair_every: Duration::from_millis(500),
        holder_ttl: Duration::from_secs(4),
        fetch_timeout: Duration::from_secs(20),
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
async fn sites_replicate_to_three_heal_and_serve_from_any_gateway() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("c0mpute_core=info,c0mpute_net=warn")
        .with_test_writer()
        .try_init();
    let tmp = tempdir().unwrap();

    // A hub everyone dials (it also hosts), then four more hosting nodes.
    let (hub_net, hub_store) = node(&tmp.path().join("h0"), vec![]).await;
    let hub_addr = listen_addr(&hub_net).await;
    let mut hosts: Vec<Hosting> = Vec::new();
    let opts = |dir: &std::path::Path| Options {
        hosts: true,
        catalog_path: Some(dir.join("catalog.json")),
        timing: fast(),
        ..Default::default()
    };
    hosts.push(Hosting::start(hub_net.clone(), hub_store, opts(&tmp.path().join("h0"))).await.unwrap());
    for i in 1..5 {
        let d = tmp.path().join(format!("h{i}"));
        let (n, s) = node(&d, vec![hub_addr.clone()]).await;
        hosts.push(Hosting::start(n, s, opts(&d)).await.unwrap());
    }

    // A gateway that hosts nothing.
    let gd = tmp.path().join("gw");
    let (gn, gs) = node(&gd, vec![hub_addr.clone()]).await;
    let gateway = Hosting::start(
        gn,
        gs,
        Options {
            hosts: false,
            timing: fast(),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // The publisher: seeds its own site, does not count as a copy.
    let pd = tmp.path().join("pub");
    let (pn, ps) = node(&pd, vec![hub_addr.clone()]).await;
    let publisher = Hosting::start(
        pn,
        ps,
        Options {
            hosts: false,
            timing: fast(),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // gossipsub mesh warm-up
    tokio::time::sleep(Duration::from_secs(3)).await;

    let site_dir = tmp.path().join("site");
    std::fs::create_dir_all(site_dir.join("docs")).unwrap();
    std::fs::write(site_dir.join("index.html"), b"<h1>hello from the network</h1>").unwrap();
    std::fs::write(site_dir.join("docs/index.html"), b"docs page").unwrap();
    let big: Vec<u8> = (0..(5 * 1024 * 1024)).map(|i| (i % 251) as u8).collect();
    std::fs::write(site_dir.join("big.bin"), &big).unwrap();
    let built = build_from_dir(&site_dir, "demo").unwrap();
    let owner = libp2p::identity::Keypair::generate_ed25519();
    let rec = SiteRecord::sign(
        &owner,
        "demo",
        &Hash::of(&built.manifest.to_bytes()),
        1,
        built.manifest.total_bytes(),
        vec![],
    )
    .unwrap();
    let site = rec.site_id.clone();
    publisher.publish_site(&built, rec.clone()).await.unwrap();

    // Exactly three hosting copies, with no coordinator.
    let p = publisher.clone();
    let s = site.clone();
    wait_for("three copies", 60, || {
        let p = p.clone();
        let s = s.clone();
        async move {
            p.regossip(&rec_clone_hack(&p, &s).await).await;
            p.holders(&s).await.len() >= 3
        }
    })
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let holders = hosts[0].holders(&site).await;
    assert_eq!(holders.len(), 3, "settles at exactly the target: {holders:?}");

    // The publisher leaves; the site stays up.
    publisher.stop();

    // Serve through the non-holding gateway (proxied over libp2p).
    let app = gateway.router();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    let g = gateway.clone();
    let s = site.clone();
    wait_for("gateway learns the site", 30, || {
        let g = g.clone();
        let s = s.clone();
        async move { g.holders(&s).await.len() >= 3 }
    })
    .await;
    let client = reqwest::Client::new();
    let body = client
        .get(format!("http://{addr}/s/{site}/"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body, "<h1>hello from the network</h1>");
    let docs = client.get(format!("http://{addr}/s/{site}/docs")).send().await.unwrap();
    assert_eq!(docs.status(), 200);
    assert_eq!(docs.text().await.unwrap(), "docs page");
    let got = client
        .get(format!("http://{addr}/s/{site}/big.bin"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(got.len(), big.len());
    assert!(got[..] == big[..], "multi-chunk file reassembles exactly");
    let missing = client.get(format!("http://{addr}/s/{site}/nope")).send().await.unwrap();
    assert_eq!(missing.status(), 404);

    // A holder leaves: another hosting node takes its place.
    let gone = holders[0].clone();
    let leaver = hosts.iter().find(|h| h.peer_id() == gone).unwrap().clone();
    leaver.stop();
    let watcher = hosts.iter().find(|h| h.peer_id() != gone).unwrap().clone();
    let s = site.clone();
    let gone2 = gone.clone();
    wait_for("a replacement copy", 60, || {
        let w = watcher.clone();
        let s = s.clone();
        let gone = gone2.clone();
        async move {
            let hs = w.holders(&s).await;
            hs.len() >= 3 && !hs.contains(&gone)
        }
    })
    .await;

    // Still served.
    let body = client
        .get(format!("http://{addr}/s/{site}/"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body, "<h1>hello from the network</h1>");

    let listing: serde_json::Value = client
        .get(format!("http://{addr}/hosting/v1/sites"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listing["sites"][0]["site_id"], site);
    assert_eq!(listing["target_copies"], hosting::TARGET_REPLICAS);
}

/// The publisher's current record for `site` (it is the newest it knows).
async fn rec_clone_hack(p: &Hosting, site: &str) -> SiteRecord {
    p.sites()
        .await
        .into_iter()
        .find(|(r, _)| r.site_id == site)
        .map(|(r, _)| r)
        .expect("publisher knows its own record")
}
