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
        // Plenty of (pretend) disk: the test box may be below the free-disk floor.
        disk_override: Some((1 << 40, 2 << 40)),
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
    std::fs::write(site_dir.join("index.html"), b"<meta property=\"og:image\" content=\"img/hero.png\"><h1>hello from the network</h1>").unwrap();
    std::fs::write(site_dir.join("docs/index.html"), b"docs page").unwrap();
    // A folder of tracks with no index.html (an album upload).
    std::fs::create_dir_all(site_dir.join("My Album")).unwrap();
    std::fs::write(site_dir.join("My Album/01 Intro.mp3"), b"ID3 fake mp3").unwrap();
    std::fs::write(site_dir.join("My Album/02 <Outro>.mp3"), b"ID3 fake mp3 2").unwrap();
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
    assert!(body.ends_with("<h1>hello from the network</h1>"), "{body}");
    // Relative og:image comes back absolute against the URL it was served at.
    assert!(
        body.contains(&format!(r#"content="http://{addr}/s/{site}/img/hero.png""#)),
        "{body}"
    );
    // Behind a TLS proxy the forwarded origin wins.
    let proxied = client
        .get(format!("http://{addr}/s/{site}/"))
        .header("x-forwarded-proto", "https")
        .header("x-forwarded-host", "gateway.example")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        proxied.contains(&format!(r#"content="https://gateway.example/s/{site}/img/hero.png""#)),
        "{proxied}"
    );
    // `/s/<id>` (no slash) redirects so relative URLs resolve under the site.
    let no_slash = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .get(format!("http://{addr}/s/{site}?a=1"))
        .send()
        .await
        .unwrap();
    assert_eq!(no_slash.status(), 308);
    assert_eq!(no_slash.headers()["location"], format!("/s/{site}/?a=1").as_str());
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
    // A folder with no index.html gets a generated page with players; without
    // its slash it redirects so relative links resolve.
    let album = client.get(format!("http://{addr}/s/{site}/My%20Album/")).send().await.unwrap();
    assert_eq!(album.status(), 200);
    assert_eq!(album.headers()["x-c0mpute-listing"], "1");
    let page = album.text().await.unwrap();
    assert!(page.contains("<audio controls preload=\"none\" src=\"01%20Intro.mp3\">"), "{page}");
    assert!(page.contains("02 &lt;Outro&gt;.mp3") && page.contains("Play all"), "{page}");
    let track = client.get(format!("http://{addr}/s/{site}/My%20Album/01%20Intro.mp3")).send().await.unwrap();
    assert_eq!(track.status(), 200);
    assert_eq!(track.headers()["content-type"], "audio/mpeg");
    let bare = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .get(format!("http://{addr}/s/{site}/My%20Album"))
        .send()
        .await
        .unwrap();
    assert_eq!(bare.status(), 308);
    assert_eq!(bare.headers()["location"], "My%20Album/");
    // The site's own index.html still wins at the root.
    let root_page = client.get(format!("http://{addr}/s/{site}/")).send().await.unwrap();
    assert!(root_page.headers().get("x-c0mpute-listing").is_none());
    let missing = client.get(format!("http://{addr}/s/{site}/nope")).send().await.unwrap();
    assert_eq!(missing.status(), 404);

    // A client that does not trust this gateway can check every byte: the
    // resolve proof carries the owner-signed record and the manifest bytes,
    // and each chunk comes back by hash.
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD;
    let proof: serde_json::Value = client
        .get(format!("http://{addr}/hosting/v1/sites/{site}/resolve?path=/big.bin"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rec: SiteRecord = serde_json::from_value(proof["record"].clone()).unwrap();
    assert!(rec.verify(), "owner signature checks out");
    assert_eq!(rec.site_id, site);
    let manifest = b64.decode(proof["manifest"].as_str().unwrap()).unwrap();
    assert_eq!(Hash::of(&manifest).to_hex(), rec.manifest, "manifest pinned by the record");
    assert_eq!(proof["path"], "big.bin");
    assert_eq!(proof["status"], 200);
    let mut rebuilt = Vec::new();
    for h in proof["entry"]["chunks"].as_array().unwrap() {
        let h = h.as_str().unwrap();
        let bytes = client
            .get(format!("http://{addr}/hosting/v1/sites/{site}/chunks/{h}"))
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(Hash::of(&bytes).to_hex(), h, "chunk matches its hash");
        rebuilt.extend_from_slice(&bytes);
    }
    assert!(rebuilt[..] == big[..], "verified chunks rebuild the file");
    let root: serde_json::Value = client
        .get(format!("http://{addr}/hosting/v1/sites/{site}/resolve?path=/"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(root["path"], "index.html");
    let none: serde_json::Value = client
        .get(format!("http://{addr}/hosting/v1/sites/{site}/resolve?path=/nope"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(none["path"], serde_json::Value::Null);
    assert_eq!(none["status"], 404);
    // Not an open relay: a hash that is not this site's is refused.
    let stranger = Hash::of(b"not part of the site").to_hex();
    let refused = client
        .get(format!("http://{addr}/hosting/v1/sites/{site}/chunks/{stranger}"))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 404);

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
    assert!(body.ends_with("<h1>hello from the network</h1>"), "{body}");

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
