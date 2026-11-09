//! A large (striped) site end to end on loopback libp2p: a publisher seeds a
//! 260 MiB folder (the striping threshold is lowered to 256 MiB for the test) straight from its files, five hosting nodes each take only
//! some 64 MiB groups until every group has three copies, a gateway that holds
//! nothing serves byte ranges (start, middle, end, suffix, open-ended,
//! multi-chunk spans) from the holders, and when a holder leaves its groups
//! are refilled elsewhere.
//!
//! Writes about 1 GiB under TMPDIR (the file plus three copies).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use c0mpute_core::hosting::site::{ScanCache, SiteRecord, scan_dir};
use c0mpute_core::hosting::{self, Hosting, Options, Timing, stripe};
use c0mpute_net::{ChunkSource, Libp2pNetwork, NetworkConfig};
use c0mpute_proto::Hash;
use c0mpute_store::ChunkStore;
use libp2p::multiaddr::Protocol;
use tempfile::tempdir;

/// The node's chunk server, with the publisher's seed fallback (as the
/// supervisor's is).
struct StoreSource(ChunkStore);

#[async_trait::async_trait]
impl ChunkSource for StoreSource {
    async fn read_chunk(&self, hash: &Hash) -> anyhow::Result<Vec<u8>> {
        match self.0.get(hash).await {
            Ok(b) => Ok(b),
            Err(e) => hosting::seed::read_async(hash).await.ok_or(e),
        }
    }
}

async fn node(
    dir: &std::path::Path,
    bootstrap: Vec<libp2p::Multiaddr>,
) -> (Arc<Libp2pNetwork>, ChunkStore) {
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
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Deterministic bytes that differ in every chunk.
fn byte_at(i: u64) -> u8 {
    let x = i.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (i >> 7);
    (x >> 24) as u8
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_large_site_stripes_serves_ranges_and_heals() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("c0mpute_core=info,c0mpute_net=warn")
        .with_test_writer()
        .try_init();
    // Stripe above 256 MiB here (1 GiB in production) so the test writes
    // about 1 GiB rather than 4.
    stripe::set_stripe_above_for_tests(256 * 1024 * 1024);
    let tmp = tempdir().unwrap();
    // Plenty of (pretend) disk: the test box may be below the real floor.
    let disk = Some((1u64 << 40, 2u64 << 40));

    let (hub_net, hub_store) = node(&tmp.path().join("h0"), vec![]).await;
    let hub_addr = listen_addr(&hub_net).await;
    let opts = |dir: &std::path::Path| Options {
        hosts: true,
        catalog_path: Some(dir.join("catalog.json")),
        timing: fast(),
        disk_override: disk,
        max_bytes: 4 << 30,
        ..Default::default()
    };
    let mut hosts: Vec<Hosting> = vec![
        Hosting::start(hub_net.clone(), hub_store, opts(&tmp.path().join("h0")))
            .await
            .unwrap(),
    ];
    for i in 1..5 {
        let d = tmp.path().join(format!("h{i}"));
        let (n, s) = node(&d, vec![hub_addr.clone()]).await;
        hosts.push(Hosting::start(n, s, opts(&d)).await.unwrap());
    }
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
    let pd = tmp.path().join("pub");
    let (pn, ps) = node(&pd, vec![hub_addr.clone()]).await;
    let publisher = Hosting::start(
        pn.clone(),
        ps,
        Options {
            hosts: false,
            timing: fast(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;

    // 260 MiB + a bit: over the stripe threshold, five groups.
    let site_dir = tmp.path().join("movies");
    std::fs::create_dir_all(&site_dir).unwrap();
    let size: u64 = 260 * 1024 * 1024 + 12_345;
    {
        use std::io::Write;
        let mut f =
            std::io::BufWriter::new(std::fs::File::create(site_dir.join("movie.mp4")).unwrap());
        let mut buf = Vec::with_capacity(1 << 20);
        let mut i = 0u64;
        while i < size {
            buf.clear();
            let end = (i + (1 << 20)).min(size);
            buf.extend((i..end).map(byte_at));
            f.write_all(&buf).unwrap();
            i = end;
        }
    }
    std::fs::write(
        site_dir.join("index.html"),
        b"<video src=movie.mp4 controls></video>",
    )
    .unwrap();
    let scanned = scan_dir(
        &site_dir,
        "movies",
        &mut ScanCache::default(),
        &mut |_, _| {},
    )
    .unwrap();
    assert!(stripe::is_striped(scanned.manifest.total_bytes()));
    let stored = scanned.manifest.stored();
    assert!(
        !stored.parts.is_empty(),
        "a striped site's manifest is parted"
    );
    let kp = libp2p::identity::Keypair::generate_ed25519();
    let rec = SiteRecord::sign(
        &kp,
        "movies",
        &stored.hash(),
        1,
        scanned.manifest.total_bytes(),
        vec![],
    )
    .unwrap();
    publisher
        .publish_scanned(&scanned, rec.clone())
        .await
        .unwrap();

    // Every group reaches three copies, and nobody holds the whole site.
    let started = std::time::Instant::now();
    wait_for("three copies of every group", 300, || {
        let h = hosts[0].clone();
        let rec = rec.clone();
        async move {
            h.regossip(&rec).await;
            h.health(&rec)
                .await
                .is_some_and(|x| x.groups == 5 && x.short == 0)
        }
    })
    .await;
    let secs = started.elapsed().as_secs_f64();
    eprintln!(
        "striped: 3 copies of {:.0} MiB placed in {secs:.1} s ({:.0} MB/s of copies over loopback)",
        size as f64 / 1048576.0,
        3.0 * size as f64 / secs / 1e6
    );
    let (layout, holders) = hosts[0].group_holders(&rec).await.unwrap();
    assert_eq!(layout.len(), 5);
    let mut per_host: HashMap<String, usize> = HashMap::new();
    for hs in &holders {
        assert_eq!(hs.len(), 3, "{holders:?}");
        for p in hs {
            *per_host.entry(p.clone()).or_default() += 1;
        }
    }
    // Spread over the network (with only 5 groups one host may still rank
    // top-3 for all of them; the unit test checks that over 40 groups).
    assert!(per_host.len() >= 4, "groups spread over the hosts: {per_host:?}");
    assert_eq!(per_host.values().sum::<usize>(), 15);

    // The gateway serves ranges from the holders.
    let gw_router = gateway.router();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, gw_router).await.unwrap() });
    wait_for("the gateway to know the site", 30, || {
        let g = gateway.clone();
        let id = rec.site_id.clone();
        async move { g.record_for(&id).await.is_some() }
    })
    .await;
    let url = format!("http://{gw_addr}/s/{}/movie.mp4", rec.site_id);
    let client = reqwest::Client::new();
    let mb = 1024 * 1024u64;
    for (range, from, to) in [
        ("bytes=0-99", 0, 99),
        ("bytes=1048000-3146000", 1_048_000, 3_146_000),
        (
            &*format!("bytes={}-{}", 130 * mb - 1, 131 * mb),
            130 * mb - 1,
            131 * mb,
        ),
        ("bytes=-500", size - 500, size - 1),
        (
            &*format!("bytes={}-", size - 3 * mb - 7),
            size - 3 * mb - 7,
            size - 1,
        ),
        (
            &*format!("bytes={}-{}", size - 1, size + 100),
            size - 1,
            size - 1,
        ),
    ] {
        let r = client
            .get(&url)
            .header("range", range)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 206, "{range}");
        assert_eq!(r.headers()["content-type"], "video/mp4");
        assert_eq!(r.headers()["accept-ranges"], "bytes");
        assert_eq!(
            r.headers()["content-range"].to_str().unwrap(),
            format!("bytes {from}-{to}/{size}"),
            "{range}"
        );
        let body = r.bytes().await.unwrap();
        assert_eq!(body.len() as u64, to - from + 1, "{range}");
        assert!(
            body.iter()
                .enumerate()
                .all(|(i, b)| *b == byte_at(from + i as u64)),
            "{range}: wrong bytes"
        );
    }
    let r = client
        .get(&url)
        .header("range", format!("bytes={size}-"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 416);
    assert_eq!(
        r.headers()["content-range"].to_str().unwrap(),
        format!("bytes */{size}")
    );
    // The whole file, streamed with read-ahead.
    let t = std::time::Instant::now();
    let r = client.get(&url).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.headers()["content-length"].to_str().unwrap(),
        size.to_string()
    );
    let etag = r.headers()["etag"].to_str().unwrap().to_string();
    let body = r.bytes().await.unwrap();
    eprintln!(
        "gateway: full {:.0} MiB in {:.1} s",
        size as f64 / 1048576.0,
        t.elapsed().as_secs_f64()
    );
    assert_eq!(body.len() as u64, size);
    assert!(
        body.iter()
            .enumerate()
            .step_by(4093)
            .all(|(i, b)| *b == byte_at(i as u64))
    );
    let r = client
        .get(&url)
        .header("if-none-match", &etag)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 304);
    // If-Range with an old ETag: the whole file, not a range.
    let r = client
        .get(&url)
        .header("range", "bytes=0-9")
        .header("if-range", "\"old\"")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    drop(r);

    // A holder leaves: its groups are refilled to three copies elsewhere.
    let gone = holders[0][0].clone();
    let idx = hosts.iter().position(|h| h.peer_id() == gone).unwrap();
    let leaving = hosts.remove(idx);
    leaving.stop();
    let watcher = hosts[0].clone();
    wait_for("groups refilled after a holder left", 300, || {
        let h = watcher.clone();
        let rec = rec.clone();
        let gone = gone.clone();
        async move {
            h.regossip(&rec).await;
            let Ok((_, holders)) = h.group_holders(&rec).await else {
                return false;
            };
            holders
                .iter()
                .all(|hs| hs.len() >= 3 && !hs.contains(&gone))
        }
    })
    .await;
    let _ = pn;
}
