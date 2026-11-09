//! End-to-end live streaming on loopback libp2p: a broadcaster pushes
//! segments into a signed live window, two relays serve the playlist and
//! segments over HTTP, and the second relay gets its segments from the first
//! (the swarm) rather than only from the broadcaster. Then, if ffmpeg is
//! installed, a real `ffmpeg -f lavfi testsrc` broadcast through the same path.

use std::sync::Arc;
use std::time::Duration;

use c0mpute_core::live::broadcast::{Broadcaster, Codec, GoOptions, ffmpeg_args};
use c0mpute_core::live::playlist::parse_media_playlist;
use c0mpute_core::live::{LiveStream, Timing};
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
        have_every: Duration::from_millis(700),
        holder_ttl: Duration::from_secs(10),
        fetch_timeout: Duration::from_secs(10),
        evict_every: Duration::from_millis(300),
        grace: Duration::from_secs(3),
        stream_ttl: Duration::from_secs(60),
        viewer_ttl: Duration::from_secs(30),
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
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn serve(ls: &LiveStream) -> std::net::SocketAddr {
    let app = ls.router();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    addr
}

/// Synthetic "segment" bytes: distinct per index, some over one chunk.
fn synthetic(i: usize) -> Vec<u8> {
    let len = if i % 3 == 2 { 1024 * 1024 + 4321 } else { 40_000 + i * 997 };
    (0..len).map(|j| ((j * 31 + i * 7) % 251) as u8).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn broadcaster_and_two_relays_swarm_segments() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("c0mpute_core=info,c0mpute_net=warn")
        .with_test_writer()
        .try_init();
    let tmp = tempdir().unwrap();

    // Broadcaster (also the hub everyone dials), then two relays.
    let (bn, bs) = node(&tmp.path().join("b"), vec![]).await;
    let hub = listen_addr(&bn).await;
    let b_live = LiveStream::start(bn, bs, fast()).await.unwrap();
    let (n1, s1) = node(&tmp.path().join("r1"), vec![hub.clone()]).await;
    let relay1 = LiveStream::start(n1, s1.clone(), fast()).await.unwrap();
    let (n2, s2) = node(&tmp.path().join("r2"), vec![hub.clone()]).await;
    let relay2 = LiveStream::start(n2, s2.clone(), fast()).await.unwrap();
    let a1 = serve(&relay1).await;
    let a2 = serve(&relay2).await;

    tokio::time::sleep(Duration::from_secs(3)).await; // gossipsub mesh warm-up

    let owner = libp2p::identity::Keypair::generate_ed25519();
    let mut b = Broadcaster::new(b_live.clone(), owner, "e2e-show", &[("src", None)], 4).unwrap();
    let id = b.stream_id();
    let mut sent: Vec<Vec<u8>> = Vec::new();
    for i in 0..3 {
        let bytes = synthetic(i);
        b.push(0, &bytes, 2000 + i as u32 * 10, false).await.unwrap();
        sent.push(bytes);
    }

    // Both relays learn the stream from gossip.
    for r in [&relay1, &relay2] {
        let r = r.clone();
        let id = id.clone();
        wait_for("relay learns the stream", 30, || {
            let r = r.clone();
            let id = id.clone();
            async move { r.record(&id).await.is_some_and(|rec| rec.body.renditions[0].segments.len() == 3) }
        })
        .await;
    }

    let client = reqwest::Client::new();
    let get = |url: String| {
        let c = client.clone();
        async move {
            let r = c.get(&url).send().await.unwrap();
            (r.status(), r.headers().clone(), r.bytes().await.unwrap())
        }
    };

    // Relay 1: playlist, then every segment (fetched from the broadcaster).
    let (status, headers, body) = get(format!("http://{a1}/live/{id}/index.m3u8")).await;
    assert_eq!(status, 200);
    assert_eq!(headers["content-type"], "application/vnd.apple.mpegurl");
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.starts_with("#EXTM3U\n"), "{text}");
    assert!(text.contains("#EXT-X-TARGETDURATION:3\n"), "{text}");
    assert!(text.contains("#EXT-X-MEDIA-SEQUENCE:0\n"), "{text}");
    let pl = parse_media_playlist(&text);
    assert_eq!(pl.segments.len(), 3);
    for (seg, want) in pl.segments.iter().zip(&sent) {
        let (status, headers, got) = get(format!("http://{a1}/live/{id}/{}", seg.uri)).await;
        assert_eq!(status, 200, "{}", seg.uri);
        assert_eq!(headers["content-type"], "video/mp2t");
        assert!(got[..] == want[..], "relay 1 serves the exact bytes");
    }
    let info1 = relay1.info(&id).await.unwrap();
    assert_eq!(info1.fetched_from.keys().collect::<Vec<_>>(), vec![b_live.peer_id()], "{info1:?}");

    // Relay 2 learns that relay 1 holds the segments (Have gossip) ...
    let first = pl.segments.iter().map(|s| s.uri.trim_end_matches(".ts").to_string()).collect::<Vec<_>>();
    {
        let r2 = relay2.clone();
        let id = id.clone();
        let want = relay1.peer_id().to_string();
        let segs = first.clone();
        wait_for("relay 2 sees relay 1 as a holder", 30, || {
            let r2 = r2.clone();
            let id = id.clone();
            let want = want.clone();
            let segs = segs.clone();
            async move {
                for s in &segs {
                    if !r2.holders_of(&id, s).await.contains(&want) {
                        return false;
                    }
                }
                true
            }
        })
        .await;
    }
    // ... and fetches from relay 1, not only the broadcaster.
    for (seg, want) in pl.segments.iter().zip(&sent) {
        let (status, _, got) = get(format!("http://{a2}/live/{id}/{}", seg.uri)).await;
        assert_eq!(status, 200);
        assert!(got[..] == want[..], "relay 2 serves the exact bytes");
    }
    let info2 = relay2.info(&id).await.unwrap();
    let from_relay1 = info2.fetched_from.get(relay1.peer_id()).copied().unwrap_or(0);
    assert!(from_relay1 > 0, "relay 2 fetched from relay 1: {:?}", info2.fetched_from);
    let from_origin = info2.fetched_from.get(b_live.peer_id()).copied().unwrap_or(0);
    assert!(from_relay1 > from_origin, "the swarm, not the origin, served relay 2: {:?}", info2.fetched_from);

    // A playlist request marks relay 2 as watched, so new segments are
    // prefetched as their record arrives.
    let _ = get(format!("http://{a2}/live/{id}/index.m3u8")).await;
    let bytes = synthetic(3);
    let rec = b.push(0, &bytes, 2000, true).await.unwrap(); // after an encoder restart
    sent.push(bytes);
    let newest = rec.body.renditions[0].segments.last().unwrap().hash.clone();
    {
        let r2 = relay2.clone();
        let id = id.clone();
        let h = newest.clone();
        wait_for("relay 2 prefetches the newest segment", 30, || {
            let r2 = r2.clone();
            let id = id.clone();
            let h = h.clone();
            async move { r2.holds(&id, &h).await }
        })
        .await;
    }
    let (_, _, body) = get(format!("http://{a2}/live/{id}/index.m3u8")).await;
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert_eq!(text.matches("#EXT-X-DISCONTINUITY\n").count(), 1, "{text}");

    // Slide the window: old segments leave the playlist and, after the grace
    // period, the relays' stores.
    for i in 4..9 {
        let bytes = synthetic(i);
        b.push(0, &bytes, 2000, false).await.unwrap();
        sent.push(bytes);
    }
    {
        let r1 = relay1.clone();
        let id = id.clone();
        wait_for("relay 1 sees the newest window", 30, || {
            let r1 = r1.clone();
            let id = id.clone();
            async move { r1.record(&id).await.is_some_and(|r| r.body.renditions[0].media_seq == 5) }
        })
        .await;
    }
    let (_, _, body) = get(format!("http://{a1}/live/{id}/index.m3u8")).await;
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("#EXT-X-MEDIA-SEQUENCE:5\n"), "{text}");
    assert!(text.contains("#EXT-X-DISCONTINUITY-SEQUENCE:1\n"), "the restarted segment left: {text}");
    assert!(!text.contains(&first[0]), "segment 0 left the playlist");
    // Grace is 3 s in this test; keep the record fresh while we wait.
    let old_chunk = Hash::from_hex(&first[0]).unwrap();
    {
        let s1 = s1.clone();
        let s2 = s2.clone();
        wait_for("old segments evicted from both relays", 30, || {
            let s1 = s1.clone();
            let s2 = s2.clone();
            async move { !s1.has(&old_chunk).await && !s2.has(&old_chunk).await }
        })
        .await;
    }
    let (status, _, _) = get(format!("http://{a1}/live/{id}/{}.ts", first[0])).await;
    assert_eq!(status, 404, "segments outside the window are not served");
    // The current window still is.
    let cur = relay1.record(&id).await.unwrap();
    let last = cur.body.renditions[0].segments.last().unwrap();
    let (status, _, got) = get(format!("http://{a1}/live/{id}/{}.ts", last.hash)).await;
    assert_eq!(status, 200);
    assert!(got[..] == sent.last().unwrap()[..]);

    // End of broadcast: ENDLIST, and the listing knows the stream.
    b.end().await.unwrap();
    {
        let r1 = relay1.clone();
        let id = id.clone();
        wait_for("relay 1 sees the end", 30, || {
            let r1 = r1.clone();
            let id = id.clone();
            async move { r1.record(&id).await.is_some_and(|r| r.body.ended) }
        })
        .await;
    }
    let (_, _, body) = get(format!("http://{a1}/live/{id}/index.m3u8")).await;
    assert!(String::from_utf8(body.to_vec()).unwrap().ends_with("#EXT-X-ENDLIST\n"));
    let (status, _, body) = get(format!("http://{a1}/live/v1/streams")).await;
    assert_eq!(status, 200);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["streams"][0]["stream_id"], id);
    assert_eq!(v["streams"][0]["name"], "e2e-show");
    assert_eq!(v["streams"][0]["path"], format!("/live/{id}/index.m3u8"));
    let (status, _, _) = get(format!("http://{a1}/live/{}/index.m3u8", "0".repeat(20))).await;
    assert_eq!(status, 404);
}

/// Real ffmpeg: `testsrc` through the HLS muxer, into the network, out of a
/// relay. Skips (passes) when ffmpeg is not installed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_ffmpeg_testsrc_through_a_relay() {
    let Some(ffmpeg) = c0mpute_core::supervisor::which_ffmpeg() else {
        eprintln!("SKIP: ffmpeg not on PATH");
        return;
    };
    let tmp = tempdir().unwrap();
    let (bn, bs) = node(&tmp.path().join("b"), vec![]).await;
    let hub = listen_addr(&bn).await;
    let b_live = LiveStream::start(bn, bs, fast()).await.unwrap();
    let (n1, s1) = node(&tmp.path().join("r1"), vec![hub]).await;
    let relay = LiveStream::start(n1, s1, fast()).await.unwrap();
    let addr = serve(&relay).await;
    tokio::time::sleep(Duration::from_secs(3)).await;

    let work = tmp.path().join("work");
    let opts = GoOptions {
        input: "testsrc=size=320x240:rate=25".into(),
        name: "ffmpeg-test".into(),
        format: Some("lavfi".into()),
        segment_secs: 2,
        window: 4,
        low: true,
        codec: Codec::Auto,
        listen: false,
        bind: None,
        linger: Duration::ZERO,
        reconnect: false,
    };
    std::fs::create_dir_all(work.join("r0")).unwrap();
    std::fs::create_dir_all(work.join("r1")).unwrap();
    let mut args = ffmpeg_args(&opts, &work);
    // stop after 7 s of video (an input option, so it ends every output)
    let i = args.iter().position(|a| a == "-i").unwrap();
    args.insert(i, "-t".into());
    args.insert(i + 1, "7".into());
    let status = tokio::process::Command::new(&ffmpeg).args(&args).status().await.unwrap();
    assert!(status.success(), "ffmpeg {args:?}");

    let owner = libp2p::identity::Keypair::generate_ed25519();
    let mut b = Broadcaster::new(b_live, owner, "ffmpeg-test", &[("src", None), ("480p", None)], 4).unwrap();
    let id = b.stream_id();
    let mut bytes_by_hash = std::collections::HashMap::new();
    for (r, dir) in ["r0", "r1"].iter().enumerate() {
        let pl = parse_media_playlist(&std::fs::read_to_string(work.join(dir).join("index.m3u8")).unwrap());
        assert!(pl.ended && pl.segments.len() >= 3, "{pl:?}");
        for s in pl.segments {
            let bytes = std::fs::read(work.join(dir).join(&s.uri)).unwrap();
            assert_eq!(bytes[0], 0x47, "MPEG-TS sync byte");
            bytes_by_hash.insert(Hash::of(&bytes).to_hex(), bytes.clone());
            b.push(r, &bytes, s.dur_ms, s.disc).await.unwrap();
        }
    }
    let r = relay.clone();
    let id2 = id.clone();
    wait_for("relay learns the ffmpeg stream", 30, || {
        let r = r.clone();
        let id = id2.clone();
        async move { r.record(&id).await.is_some_and(|rec| rec.body.renditions.iter().all(|x| x.segments.len() >= 3)) }
    })
    .await;
    let client = reqwest::Client::new();
    let master = client.get(format!("http://{addr}/live/{id}/index.m3u8")).send().await.unwrap().text().await.unwrap();
    assert!(master.contains("#EXT-X-STREAM-INF:BANDWIDTH="), "{master}");
    assert!(master.contains("src/index.m3u8") && master.contains("480p/index.m3u8"), "{master}");
    for rend in ["src", "480p"] {
        let media = client
            .get(format!("http://{addr}/live/{id}/{rend}/index.m3u8"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let pl = parse_media_playlist(&media);
        assert!(!pl.segments.is_empty(), "{media}");
        for s in pl.segments {
            let got = client
                .get(format!("http://{addr}/live/{id}/{rend}/{}", s.uri))
                .send()
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap();
            let want = &bytes_by_hash[s.uri.trim_end_matches(".ts")];
            assert!(got[..] == want[..], "{rend} {} matches", s.uri);
        }
    }
    eprintln!("ffmpeg testsrc: master + 2 renditions served through the relay");
}
