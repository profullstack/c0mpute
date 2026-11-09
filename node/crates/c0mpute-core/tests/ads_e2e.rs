//! End-to-end ads on loopback libp2p, no ad server anywhere:
//!
//! 1. An advertiser signs a campaign; its creatives go to a hosting site
//!    (three copies) and the record is gossiped to a gateway.
//! 2. Two podcasts are published: one opted in to ads (signed
//!    `c0mpute-ads.json` in its site), one not.
//! 3. The gateway serves `?ads=1` episodes: the opted-in show with the audio
//!    creative spliced in front (when ffmpeg is installed; otherwise clean,
//!    with a header saying why), the other show always clean. It answers a
//!    VAST request with the video creative, served from the hosting swarm,
//!    and its tracking beacons become signed receipts.
//! 4. The advertiser's own node asks for receipts over gossip and counts.

use std::sync::Arc;
use std::time::Duration;

use c0mpute_core::ads::campaign::{MICROS, Placement};
use c0mpute_core::ads::manage::{self, CampaignSpec};
use c0mpute_core::ads::stitch::{self, find_tool};
use c0mpute_core::ads::{self, Ads};
use c0mpute_core::hosting::site::{SiteRecord, build_from_dir, owner_hex, site_id_for};
use c0mpute_core::hosting::{Hosting, Options, Timing};
use c0mpute_core::podcast::show::{build_show, load_input};
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

fn ads_fast() -> ads::Timing {
    ads::Timing {
        regossip_every: Duration::from_secs(2),
        tick: Duration::from_millis(300),
        flush_every: Duration::from_millis(500),
        stitch_ttl: Duration::from_secs(60),
        stitch_timeout: Duration::from_secs(60),
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

/// A real media file from ffmpeg's test sources, or `None` without ffmpeg.
fn media(ffmpeg: Option<&std::path::Path>, out: &std::path::Path, args: &[&str]) -> bool {
    let Some(ff) = ffmpeg else { return false };
    std::process::Command::new(ff)
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .arg(out)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Text of the first `<tag>` (CDATA or text) in an XML document.
fn xml_text(xml: &str, tag: &str) -> Option<String> {
    let mut r = quick_xml::Reader::from_str(xml);
    let mut inside = false;
    let mut out = String::new();
    loop {
        match r.read_event().expect("well-formed XML") {
            quick_xml::events::Event::Start(e) if e.name().as_ref() == tag.as_bytes() => inside = true,
            quick_xml::events::Event::End(e) if inside && e.name().as_ref() == tag.as_bytes() => return Some(out.trim().to_string()),
            quick_xml::events::Event::Text(t) if inside => out.push_str(&t.unescape().unwrap()),
            quick_xml::events::Event::CData(t) if inside => out.push_str(std::str::from_utf8(&t.into_inner()).unwrap()),
            quick_xml::events::Event::Eof => return None,
            _ => {}
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn campaign_to_stitched_episode_vast_and_report() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("c0mpute_core=info,c0mpute_net=warn")
        .with_test_writer()
        .try_init();
    let tmp = tempdir().unwrap();
    let ffmpeg = find_tool("ffmpeg");
    let ffprobe = find_tool("ffprobe");
    let real_media = ffmpeg.is_some() && ffprobe.is_some();
    if !real_media {
        eprintln!("ffmpeg/ffprobe not installed: checking the clean-episode fallback instead of a real splice");
    }

    // Network: three hosting nodes, a gateway, an advertiser, a podcaster.
    let (hub_net, hub_store) = node(&tmp.path().join("h0"), vec![]).await;
    let hub_addr = listen_addr(&hub_net).await;
    let host_opts = |dir: &std::path::Path| Options {
        hosts: true,
        catalog_path: Some(dir.join("catalog.json")),
        timing: fast(),
        ..Default::default()
    };
    let mut hosts = vec![Hosting::start(hub_net, hub_store, host_opts(&tmp.path().join("h0"))).await.unwrap()];
    for i in 1..3 {
        let d = tmp.path().join(format!("h{i}"));
        let (n, s) = node(&d, vec![hub_addr.clone()]).await;
        hosts.push(Hosting::start(n, s, host_opts(&d)).await.unwrap());
    }
    let client = || Options {
        hosts: false,
        timing: fast(),
        ..Default::default()
    };
    let (gn, gs) = node(&tmp.path().join("gw"), vec![hub_addr.clone()]).await;
    let gw_hosting = Hosting::start(gn.clone(), gs, client()).await.unwrap();
    let gw_key = libp2p::identity::Keypair::generate_ed25519();
    let gw_ads = Ads::start(
        gn,
        gw_hosting.clone(),
        ads::Options {
            signer: Some(gw_key.clone()),
            data_dir: Some(tmp.path().join("gw/ads")),
            serve: true,
            ffmpeg: ffmpeg.clone(),
            ffprobe: ffprobe.clone(),
            public_origin: None,
            timing: ads_fast(),
        },
    )
    .await
    .unwrap();
    assert!(gw_ads.install_podcast_hook());
    let (an, ast) = node(&tmp.path().join("adv"), vec![hub_addr.clone()]).await;
    let adv_hosting = Hosting::start(an.clone(), ast, client()).await.unwrap();
    let adv_ads = Ads::start(
        an,
        adv_hosting.clone(),
        ads::Options {
            timing: ads_fast(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let (pn, ps) = node(&tmp.path().join("pub"), vec![hub_addr.clone()]).await;
    let publisher = Hosting::start(pn, ps, client()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;

    // ── 1. the advertiser's campaign ─────────────────────────────────────
    let media_dir = tmp.path().join("adset");
    std::fs::create_dir_all(&media_dir).unwrap();
    let spot = media_dir.join("spot.mp3");
    let preroll = media_dir.join("preroll.mp4");
    if real_media {
        assert!(media(
            ffmpeg.as_deref(),
            &spot,
            &["-f", "lavfi", "-i", "sine=frequency=880:sample_rate=44100:duration=3", "-ac", "2"]
        ));
        assert!(media(
            ffmpeg.as_deref(),
            &preroll,
            &["-f", "lavfi", "-i", "testsrc=size=320x180:rate=10:duration=2", "-c:v", "mpeg4", "-pix_fmt", "yuv420p"]
        ));
    } else {
        std::fs::write(&spot, (0..30_000).map(|i| (i % 211) as u8).collect::<Vec<u8>>()).unwrap();
        std::fs::write(&preroll, (0..50_000).map(|i| (i % 199) as u8).collect::<Vec<u8>>()).unwrap();
    }
    let adv_key = libp2p::identity::Keypair::generate_ed25519();
    let adv_owner = owner_hex(&adv_key).unwrap();
    let input = manage::collect_creatives(std::slice::from_ref(&media_dir)).unwrap();
    let creative_site = tmp.path().join("adv/creatives");
    let creatives = manage::build_creative_dir(&input.files, &creative_site, Some(3000)).unwrap();
    let spec = CampaignSpec {
        name: "loopback-launch".into(),
        placements: vec![Placement::PodcastPre, Placement::CtvPreroll],
        categories: vec!["technology".into()],
        max_cpm_micros: 2 * MICROS,
        landing_url: "https://example.org/launch?src=c0mpute".into(),
        ..Default::default()
    };
    let record = manage::record_from_spec(&spec, &adv_owner, creatives.clone(), 1)
        .sign(&adv_key)
        .unwrap();
    let campaign_id = record.campaign_id.clone();
    let site_rec = manage::publish_with(&adv_hosting, &adv_ads, &adv_key, Some(&creative_site), &record)
        .await
        .unwrap()
        .unwrap();
    {
        let (h, a, s, r) = (adv_hosting.clone(), adv_ads.clone(), site_rec.clone(), record.clone());
        wait_for("creative site copies + campaign at the gateway", 60, || {
            let (h, a, s, r, g) = (h.clone(), a.clone(), s.clone(), r.clone(), gw_ads.clone());
            async move {
                h.regossip(&s).await;
                a.regossip(&r).await;
                h.holders(&s.site_id).await.len() >= 3 && g.campaign(&r.campaign_id).await.is_some()
            }
        })
        .await;
    }
    // The advertiser can go offline: creatives live on hosting nodes.
    adv_hosting.stop();

    // ── 2. two shows, one opted in ───────────────────────────────────────
    let episode = |dir: &std::path::Path| -> Vec<u8> {
        let p = dir.join("ep.mp3");
        if real_media {
            assert!(media(
                ffmpeg.as_deref(),
                &p,
                &["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=44100:duration=10", "-ac", "2", "-b:a", "96k"]
            ));
        } else {
            std::fs::write(&p, (0..40_000).map(|i| (i % 7) as u8).collect::<Vec<u8>>()).unwrap();
        }
        std::fs::read(p).unwrap()
    };
    let pod_key = libp2p::identity::Keypair::generate_ed25519();
    let pod_owner = owner_hex(&pod_key).unwrap();
    let mut shows = Vec::new();
    for (name, toml) in [
        ("ads-radio", "title = \"Ads Radio\"\ncategory = \"Technology\"\n[ads]\nplacements = [\"pre\"]\n"),
        ("clean-radio", "title = \"Clean Radio\"\ncategory = \"Technology\"\n"),
    ] {
        let src = tmp.path().join(format!("show-{name}"));
        std::fs::create_dir_all(&src).unwrap();
        let bytes = episode(&src);
        std::fs::write(src.join("show.toml"), toml).unwrap();
        std::fs::write(src.join("episodes.toml"), "[[episode]]\nfile = \"ep.mp3\"\ntitle = \"One\"\npub_date = \"2026-06-01\"\nduration = 10\n").unwrap();
        let site_id = site_id_for(&pod_owner, name);
        let out = tmp.path().join(format!("built-{name}"));
        let built = build_show(&load_input(&[src]).unwrap(), name, &site_id, None, &out).unwrap();
        assert_eq!(built.ads, name == "ads-radio");
        let site = build_from_dir(&out, name).unwrap();
        let rec = SiteRecord::sign(&pod_key, name, &Hash::of(&site.manifest.to_bytes()), 1, site.manifest.total_bytes(), vec![]).unwrap();
        publisher.publish_site(&site, rec.clone()).await.unwrap();
        shows.push((rec, bytes, built));
    }
    {
        let recs: Vec<SiteRecord> = shows.iter().map(|s| s.0.clone()).collect();
        let p = publisher.clone();
        wait_for("show copies at the gateway", 60, || {
            let (p, g, recs) = (p.clone(), gw_hosting.clone(), recs.clone());
            async move {
                let mut ok = true;
                for r in &recs {
                    p.regossip(r).await;
                    ok &= p.holders(&r.site_id).await.len() >= 3 && g.holders(&r.site_id).await.len() >= 3;
                }
                ok
            }
        })
        .await;
    }
    publisher.stop();

    // ── 3. the gateway ───────────────────────────────────────────────────
    let app = gw_ads.router().merge(gw_hosting.router());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    let base = format!("http://{addr}");
    let http = reqwest::Client::builder().user_agent("LoopbackPodcastApp/1.0").build().unwrap();

    // The opted-in show's feed points players at ?ads=1.
    let (ads_show, ads_episode, ads_built) = &shows[0];
    assert!(ads_built.feed_xml.contains("episodes/ep.mp3?ads=1"));
    let feed = http.get(format!("{base}/s/{}/feed.xml", ads_show.site_id)).send().await.unwrap().text().await.unwrap();
    assert!(feed.contains("episodes/ep.mp3?ads=1"));

    let ep_url = format!("{base}/s/{}/episodes/ep.mp3?ads=1", ads_show.site_id);
    let resp = http.get(&ep_url).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let marker = resp.headers().get("x-c0mpute-ads").map(|v| v.to_str().unwrap().to_string()).unwrap_or_default();
    let declared_len: u64 = resp.headers()[reqwest::header::CONTENT_LENGTH].to_str().unwrap().parse().unwrap();
    let body = resp.bytes().await.unwrap().to_vec();
    assert_eq!(declared_len, body.len() as u64, "Content-Length matches the body");
    let mut podcast_impressions = 0;
    if real_media {
        assert!(marker.starts_with("inserted;"), "{marker}");
        assert!(marker.contains(&format!("podcast_pre={campaign_id}")), "{marker}");
        assert_ne!(&body, ads_episode, "ad was spliced in");
        let out = tmp.path().join("stitched.mp3");
        std::fs::write(&out, &body).unwrap();
        let dur = stitch::probe(ffprobe.as_deref().unwrap(), &out).await.unwrap().duration_secs.unwrap();
        assert!((dur - 13.0).abs() < 0.6, "10 s episode + 3 s spot = {dur}");
        podcast_impressions = 1;
        // The same listener re-requesting (podcast apps do, with ranges)
        // gets the same bytes and is not counted again.
        let again = http.get(&ep_url).send().await.unwrap().bytes().await.unwrap().to_vec();
        assert_eq!(again, body);
        let part = http.get(&ep_url).header("range", "bytes=0-99").send().await.unwrap();
        assert_eq!(part.status(), 206);
        assert_eq!(
            part.headers()["content-range"].to_str().unwrap(),
            format!("bytes 0-99/{}", body.len())
        );
        assert_eq!(part.bytes().await.unwrap().to_vec(), body[..100].to_vec());
    } else {
        assert!(marker.contains("no ffmpeg"), "{marker}");
        assert_eq!(&body, ads_episode, "clean episode when this gateway cannot splice");
    }
    // Without ?ads=1 the episode is always the clean file.
    let plain = http
        .get(format!("{base}/s/{}/episodes/ep.mp3", ads_show.site_id))
        .send()
        .await
        .unwrap();
    assert!(plain.headers().get("x-c0mpute-ads").is_none());
    assert_eq!(&plain.bytes().await.unwrap().to_vec(), ads_episode);

    // A show that did not opt in never gets ads, even when asked.
    let (clean_show, clean_episode, clean_built) = &shows[1];
    assert!(!clean_built.feed_xml.contains("ads=1"));
    let r = http
        .get(format!("{base}/s/{}/episodes/ep.mp3?ads=1", clean_show.site_id))
        .send()
        .await
        .unwrap();
    assert!(r.headers()["x-c0mpute-ads"].to_str().unwrap().contains("not opted in"));
    assert_eq!(&r.bytes().await.unwrap().to_vec(), clean_episode);

    // VAST for a CTV pre-roll: the video creative, served from the swarm.
    let vast = http
        .get(format!("{base}/ads/v1/vast?placement=ctv_preroll&uid=tv-1&categories=technology"))
        .send()
        .await
        .unwrap();
    assert_eq!(vast.headers()["access-control-allow-origin"], "*");
    let vast = vast.text().await.unwrap();
    assert!(vast.contains("<VAST version=\"4.2\""), "{vast}");
    assert_eq!(xml_text(&vast, "AdTitle").as_deref(), Some("loopback-launch"));
    assert_eq!(xml_text(&vast, "ClickThrough").as_deref(), Some("https://example.org/launch?src=c0mpute"));
    let media_url = xml_text(&vast, "MediaFile").unwrap();
    assert!(media_url.starts_with(&base), "{media_url}");
    let video = http.get(&media_url).send().await.unwrap();
    assert_eq!(video.headers()["content-type"], "video/mp4");
    assert_eq!(video.bytes().await.unwrap().to_vec(), std::fs::read(&preroll).unwrap());
    let impression = xml_text(&vast, "Impression").unwrap();
    for _ in 0..2 {
        // A replayed beacon counts once.
        assert_eq!(http.get(&impression).send().await.unwrap().status(), 200);
    }
    let complete = impression.replace("ev=impression", "ev=complete");
    assert_eq!(http.get(&complete).send().await.unwrap().status(), 200);
    let forged = impression.replace("tok=", "tok=x");
    assert_eq!(http.get(&forged).send().await.unwrap().status(), 403);

    // A category the campaign does not target: no fill.
    let none = http
        .get(format!("{base}/ads/v1/vast?placement=ctv_preroll&categories=gardening"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!none.contains("<Ad "), "{none}");
    // The explanation endpoint says why.
    let ex: serde_json::Value = http
        .get(format!("{base}/ads/v1/select?placement=display"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(ex["winner"].is_null());
    assert_eq!(ex["rejected"][0]["reason"], "placement");

    // VMAP: a pre-roll and a mid-roll, each a VAST tag on this gateway.
    let vmap = http
        .get(format!("{base}/ads/v1/vmap?breaks=start,300&uid=tv-1"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(vmap.matches("<vmap:AdBreak ").count(), 2, "{vmap}");
    assert!(vmap.contains("timeOffset=\"00:05:00.000\""));
    assert!(vmap.contains("placement=ctv_midroll"));

    // ── 4. the advertiser counts ─────────────────────────────────────────
    let report = manage::collect_report(&adv_ads, &campaign_id, &[], Duration::from_secs(6)).await;
    assert_eq!(report.impressions(), 1 + podcast_impressions, "{report:?}");
    assert_eq!(report.events.get("complete").copied(), Some(1));
    assert_eq!(report.by_gateway.len(), 1);
    assert_eq!(report.by_gateway.keys().next().unwrap(), gw_ads.gateway_key());
    assert_eq!(report.rejected_batches, 0);
    assert!(report.gaps.is_empty());
    if real_media {
        assert_eq!(report.by_site.get(&ads_show.site_id).copied(), Some(1));
        assert_eq!(report.by_placement.get("podcast_pre").copied(), Some(1));
    }
    assert_eq!(report.by_placement.get("ctv_preroll").copied(), Some(1));
    // The same numbers straight from the gateway's HTTP log.
    let direct: serde_json::Value = http
        .get(format!("{base}/ads/v1/receipts?campaign={campaign_id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let batches: Vec<c0mpute_core::ads::receipt::ReceiptBatch> = serde_json::from_value(direct["batches"].clone()).unwrap();
    assert_eq!(
        c0mpute_core::ads::receipt::Report::build(&campaign_id, &batches).impressions(),
        report.impressions()
    );

    // Pausing is a signed new version; the gateway stops filling.
    let mut paused = record.clone();
    paused.paused = true;
    paused.seq = 2;
    let paused = paused.sign(&adv_key).unwrap();
    adv_ads.publish_campaign(paused.clone()).await.unwrap();
    {
        let (a, p) = (adv_ads.clone(), paused.clone());
        wait_for("pause reaches the gateway", 30, || {
            let (a, p, g) = (a.clone(), p.clone(), gw_ads.clone());
            async move {
                a.regossip(&p).await;
                g.campaign(&p.campaign_id).await.is_some_and(|c| c.paused)
            }
        })
        .await;
    }
    let after = http
        .get(format!("{base}/ads/v1/vast?placement=ctv_preroll"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!after.contains("<Ad "), "paused campaign still served: {after}");
    // An older version replayed later does not un-pause it.
    adv_ads.regossip(&record).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(gw_ads.campaign(&campaign_id).await.unwrap().paused);

    for h in &hosts {
        h.stop();
    }
}
