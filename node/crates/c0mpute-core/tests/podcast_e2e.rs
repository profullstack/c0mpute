//! End-to-end podcasts on loopback libp2p: a publisher builds a show
//! (`podcasting`) and hands it to the network as a hosting site, hosting
//! nodes take copies, the publisher leaves, and a listener (`podcasts`)
//! subscribes through a gateway that holds nothing, reads the feed and
//! downloads an episode byte for byte.

use std::sync::Arc;
use std::time::Duration;

use c0mpute_core::hosting::site::{SiteRecord, build_from_dir, owner_hex, site_id_for};
use c0mpute_core::hosting::{Hosting, Options, Timing};
use c0mpute_core::podcast::listen::{Client, Source, Store, find_episode};
use c0mpute_core::podcast::show::{build_show, load_input, podcast_guid_for};
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

fn allow_all(_: std::net::IpAddr) -> bool {
    true
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_published_show_is_read_and_downloaded_through_any_gateway() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("c0mpute_core=info,c0mpute_net=warn")
        .with_test_writer()
        .try_init();
    let tmp = tempdir().unwrap();

    // Three hosting nodes (a hub plus two), a gateway that holds nothing, a publisher.
    let (hub_net, hub_store) = node(&tmp.path().join("h0"), vec![]).await;
    let hub_addr = listen_addr(&hub_net).await;
    let opts = |dir: &std::path::Path| Options {
        hosts: true,
        catalog_path: Some(dir.join("catalog.json")),
        timing: fast(),
        ..Default::default()
    };
    let mut hosts = vec![Hosting::start(hub_net, hub_store, opts(&tmp.path().join("h0"))).await.unwrap()];
    for i in 1..3 {
        let d = tmp.path().join(format!("h{i}"));
        let (n, s) = node(&d, vec![hub_addr.clone()]).await;
        hosts.push(Hosting::start(n, s, opts(&d)).await.unwrap());
    }
    let client_opts = || Options {
        hosts: false,
        timing: fast(),
        ..Default::default()
    };
    let (gn, gs) = node(&tmp.path().join("gw"), vec![hub_addr.clone()]).await;
    let gateway = Hosting::start(gn, gs, client_opts()).await.unwrap();
    let (pn, ps) = node(&tmp.path().join("pub"), vec![hub_addr.clone()]).await;
    let publisher = Hosting::start(pn, ps, client_opts()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;

    // The show: two episodes (one spans two 1 MiB chunks), cover art, metadata.
    let show_dir = tmp.path().join("show");
    std::fs::create_dir_all(&show_dir).unwrap();
    let ep1: Vec<u8> = (0..(1024 * 1024 + 4321)).map(|i| (i % 253) as u8).collect();
    let ep2: Vec<u8> = (0..20_000).map(|i| (i % 7) as u8).collect();
    std::fs::write(show_dir.join("pilot.mp3"), &ep1).unwrap();
    std::fs::write(show_dir.join("second.mp3"), &ep2).unwrap();
    std::fs::write(show_dir.join("cover.jpg"), b"\xff\xd8\xff fake jpeg").unwrap();
    std::fs::write(
        show_dir.join("show.toml"),
        "title = \"Loopback Radio\"\nauthor = \"The Test Suite\"\ndescription = \"Two episodes over libp2p.\"\n",
    )
    .unwrap();
    std::fs::write(
        show_dir.join("episodes.toml"),
        "[[episode]]\nfile = \"pilot.mp3\"\ntitle = \"Pilot\"\npub_date = \"2026-06-01\"\nduration = 65\n\n\
         [[episode]]\nfile = \"second.mp3\"\ntitle = \"Second\"\npub_date = \"2026-06-08\"\nduration = 1\n",
    )
    .unwrap();

    let owner = libp2p::identity::Keypair::generate_ed25519();
    let site_id = site_id_for(&owner_hex(&owner).unwrap(), "loopback-radio");
    let input = load_input(&[show_dir]).unwrap();
    let out = tmp.path().join("built");
    let built_show = build_show(&input, "loopback-radio", &site_id, None, &out).unwrap();
    let built = build_from_dir(&out, "loopback-radio").unwrap();
    let rec = SiteRecord::sign(
        &owner,
        "loopback-radio",
        &Hash::of(&built.manifest.to_bytes()),
        1,
        built.manifest.total_bytes(),
        vec![],
    )
    .unwrap();
    assert_eq!(rec.site_id, site_id, "the show's guid seed is the site it is published as");
    publisher.publish_site(&built, rec.clone()).await.unwrap();

    let p = publisher.clone();
    let s = site_id.clone();
    let r = rec.clone();
    wait_for("three copies", 60, || {
        let (p, s, r) = (p.clone(), s.clone(), r.clone());
        async move {
            p.regossip(&r).await;
            p.holders(&s).await.len() >= 3
        }
    })
    .await;
    publisher.stop();

    let app = gateway.router();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    let g = gateway.clone();
    let s = site_id.clone();
    wait_for("gateway learns the show", 30, || {
        let (g, s) = (g.clone(), s.clone());
        async move { g.holders(&s).await.len() >= 3 }
    })
    .await;

    // The feed is served as RSS.
    let raw = reqwest::get(format!("http://{addr}/s/{site_id}/feed.xml")).await.unwrap();
    assert_eq!(raw.status(), 200);
    assert_eq!(
        raw.headers()["content-type"].to_str().unwrap(),
        "application/rss+xml"
    );
    assert_eq!(raw.text().await.unwrap(), built_show.feed_xml);

    // Listener: subscribe by site id through the gateway.
    let gw = format!("http://{addr}");
    let client = Client::new(&gw).unwrap();
    let source = Source::parse(&site_id).unwrap();
    let (feed_url, feed) = client.fetch_feed(&source).await.unwrap();
    assert_eq!(feed_url.as_str(), format!("{gw}/s/{site_id}/feed.xml"));
    assert_eq!(feed.title, "Loopback Radio");
    assert_eq!(feed.podcast_guid.as_deref(), Some(podcast_guid_for(&site_id).as_str()));
    assert_eq!(feed.episodes.len(), 2);

    let mut store = Store::open(tmp.path().join("listener/subscriptions.json")).unwrap();
    let id = store.upsert(source.clone(), feed_url.to_string(), feed.clone());
    store.save().unwrap();
    let store = Store::open(tmp.path().join("listener/subscriptions.json")).unwrap();
    assert_eq!(store.subs[store.find(&id).unwrap()].title, "Loopback Radio");

    // Download: newest is "Second"; "Pilot" spans two chunks.
    let dl = tmp.path().join("downloads");
    let newest = find_episode(&feed, None).unwrap();
    assert_eq!(newest.title, "Second");
    let p2 = client.download(&feed_url, newest, &dl).await.unwrap();
    assert_eq!(std::fs::read(&p2).unwrap(), ep2);
    let pilot = find_episode(&feed, Some("pilot")).unwrap();
    assert_eq!(pilot.enclosure.as_ref().unwrap().length, Some(ep1.len() as u64));
    let p1 = client.download(&feed_url, pilot, &dl).await.unwrap();
    assert_eq!(std::fs::read(&p1).unwrap(), ep1, "multi-chunk episode reassembles exactly");
    assert!(p1.file_name().unwrap().to_str().unwrap().starts_with("2026-06-01-pilot"));
    // A second download of the same file is a no-op that returns the same path.
    assert_eq!(client.download(&feed_url, pilot, &dl).await.unwrap(), p1);

    // The same feed as a plain URL subscription: the outbound guard refuses
    // loopback for anything that is not the configured gateway...
    let plain = Source::Url(format!("{gw}/s/{site_id}/feed.xml"));
    let other = Client::new("http://127.0.0.1:9").unwrap();
    let err = other.fetch_feed(&plain).await.unwrap_err();
    assert!(format!("{err:#}").contains("non-public"), "{err:#}");
    // ...and the same path works when the guard allows it.
    let (_, again) = other.with_allow(allow_all).fetch_feed(&plain).await.unwrap();
    assert_eq!(again, feed);

    for h in &hosts {
        h.stop();
    }
}
