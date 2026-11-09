//! Ads billing end to end on loopback libp2p, on a fake payment rail (no
//! real money, no keys, no CoinPay endpoint):
//!
//! 1. An advertiser funds a billed campaign ($0.016 budget, $2 max CPM) and
//!    gossips it to two gateways. A show opts in to ads with a payout
//!    address in its signed `c0mpute-ads.json`.
//! 2. Gateway A serves three VAST impressions; gateway B two VAST
//!    impressions and two impressions on the show (recorded the way podcast
//!    insertion records them; splicing needs ffmpeg and is covered in
//!    `ads_e2e`). Each receipt carries the cleared (second) price.
//! 3. Both gateways sign claims over their receipt chains; the advertiser's
//!    node hears them over gossip, verifies them against the receipts,
//!    settles and pays through the fake rail: payouts add up to the gross,
//!    split 75/20/5 (show/gateway/network) or 95/5 without a show, and the
//!    budget goes down by exactly that.
//! 4. The settlements are gossiped back; each gateway learns the budget
//!    left. Gateway A stops filling once its own unsettled spend reaches the
//!    budget; gateway B, which does not know about A's, serves one more.
//!    Both claim; the second claim would overspend and is refused.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use c0mpute_core::ads::billing::{Budget, ClaimError, Ledger, Role};
use c0mpute_core::payments::{FakeRail, Ledger as PayLedger, PaymentsConfig, Rail};
use c0mpute_core::ads::campaign::{MICROS, Placement};
use c0mpute_core::ads::manage::{self, CampaignSpec};
use c0mpute_core::ads::receipt::{Event, Receipt};
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
        // Claims are made explicitly below.
        claim_every: Duration::from_secs(3600),
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

/// Serve a gateway's ads routes on a loopback port.
async fn serve(gw: &Ads) -> String {
    let app = gw.router();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    format!("http://{addr}")
}

/// One VAST pre-roll for a device; fires its impression beacon (twice: the
/// replay must not count). True when the gateway filled the slot.
async fn watch(http: &reqwest::Client, base: &str, uid: &str) -> bool {
    let vast = http
        .get(format!("{base}/ads/v1/vast?placement=ctv_preroll&uid={uid}"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let Some(imp) = xml_text(&vast, "Impression") else {
        return false;
    };
    for _ in 0..2 {
        assert_eq!(http.get(&imp).send().await.unwrap().status(), 200);
    }
    true
}

const PAYER: &str = "SOL:advertiserwallet1";
const FEE: &str = "SOL:networkfee0001";
const GW_A: &str = "SOL:gatewaywalletA1";
const GW_B: &str = "SOL:gatewaywalletB1";
const SHOW: &str = "SOL:showwallet0001";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn funded_campaign_two_gateways_claims_payouts_and_overspend() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("c0mpute_core=info,c0mpute_net=warn")
        .with_test_writer()
        .try_init();
    let tmp = tempdir().unwrap();

    // Network: three hosting nodes (also ads relays), two gateways, an
    // advertiser, a podcaster.
    let (hub_net, hub_store) = node(&tmp.path().join("h0"), vec![]).await;
    let hub_addr = listen_addr(&hub_net).await;
    let host_opts = |dir: &std::path::Path| Options {
        hosts: true,
        catalog_path: Some(dir.join("catalog.json")),
        timing: fast(),
        ..Default::default()
    };
    let relay = || ads::Options {
        timing: ads_fast(),
        ..Default::default()
    };
    let mut hosts = vec![Hosting::start(hub_net.clone(), hub_store, host_opts(&tmp.path().join("h0"))).await.unwrap()];
    let mut _relays = vec![Ads::start(hub_net, hosts[0].clone(), relay()).await.unwrap()];
    for i in 1..3 {
        let d = tmp.path().join(format!("h{i}"));
        let (n, s) = node(&d, vec![hub_addr.clone()]).await;
        let h = Hosting::start(n.clone(), s, host_opts(&d)).await.unwrap();
        _relays.push(Ads::start(n, h.clone(), relay()).await.unwrap());
        hosts.push(h);
    }
    let client = || Options {
        hosts: false,
        timing: fast(),
        ..Default::default()
    };
    let mut gateways = Vec::new();
    for (name, payout) in [("gwa", GW_A), ("gwb", GW_B)] {
        let d = tmp.path().join(name);
        let (n, s) = node(&d, vec![hub_addr.clone()]).await;
        let h = Hosting::start(n.clone(), s, client()).await.unwrap();
        let gw = Ads::start(
            n,
            h,
            ads::Options {
                signer: Some(libp2p::identity::Keypair::generate_ed25519()),
                data_dir: Some(d.join("ads")),
                serve: true,
                payout: Some(payout.into()),
                min_claim_micros: 0,
                timing: ads_fast(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        gateways.push(gw);
    }
    let (gw_a, gw_b) = (gateways[0].clone(), gateways[1].clone());
    let (an, ast) = node(&tmp.path().join("adv"), vec![hub_addr.clone()]).await;
    let adv_hosting = Hosting::start(an.clone(), ast, client()).await.unwrap();
    let adv_ads = Ads::start(an, adv_hosting.clone(), relay()).await.unwrap();
    let (pn, ps) = node(&tmp.path().join("pub"), vec![hub_addr.clone()]).await;
    let publisher = Hosting::start(pn, ps, client()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;

    // ── 1. a funded campaign and an opted-in show ────────────────────────
    let media_dir = tmp.path().join("adset");
    std::fs::create_dir_all(&media_dir).unwrap();
    std::fs::write(media_dir.join("spot.mp3"), (0..3_000).map(|i| (i % 211) as u8).collect::<Vec<u8>>()).unwrap();
    std::fs::write(media_dir.join("preroll.mp4"), (0..5_000).map(|i| (i % 199) as u8).collect::<Vec<u8>>()).unwrap();
    let adv_key = libp2p::identity::Keypair::generate_ed25519();
    let adv_owner = owner_hex(&adv_key).unwrap();
    let input = manage::collect_creatives(std::slice::from_ref(&media_dir)).unwrap();
    let creatives = manage::build_creative_dir(&input.files, &tmp.path().join("adv/creatives"), Some(3000)).unwrap();
    let budget_total = 16_000; // $0.016: eight impressions at $2 CPM
    let spec = CampaignSpec {
        name: "billed-launch".into(),
        placements: vec![Placement::PodcastPre, Placement::CtvPreroll],
        max_cpm_micros: 2 * MICROS,
        landing_url: "https://example.org/launch".into(),
        budget: Some(Budget {
            total_micros: budget_total,
            daily_micros: None,
            rail: "coinpay".into(),
            payer: Some(PAYER.into()),
            funding_ref: Some("escrow-test-1".into()),
        }),
        ..Default::default()
    };
    let record = manage::record_from_spec(&spec, &adv_owner, creatives, 1).sign(&adv_key).unwrap();
    let cid = record.campaign_id.clone();
    let bids = BTreeMap::from([(record.seq, record.max_cpm_micros)]);
    let rail = FakeRail::new();
    let shared = PayLedger::in_memory();
    let cap = PaymentsConfig::default().round_cap();
    let got = |payout: &str| rail.received(payout.split_once(':').unwrap().1);
    rail.fund(&adv_owner, MICROS); // the advertiser holds $1 on the rail
    adv_ads.publish_campaign(record.clone()).await.unwrap();
    {
        let (a, r) = (adv_ads.clone(), record.clone());
        wait_for("campaign at both gateways", 60, || {
            let (a, r, ga, gb) = (a.clone(), r.clone(), gw_a.clone(), gw_b.clone());
            async move {
                a.regossip(&r).await;
                ga.campaign(&r.campaign_id).await.is_some() && gb.campaign(&r.campaign_id).await.is_some()
            }
        })
        .await;
    }

    let pod_key = libp2p::identity::Keypair::generate_ed25519();
    let pod_owner = owner_hex(&pod_key).unwrap();
    let show_name = "paid-radio";
    let show_site = site_id_for(&pod_owner, show_name);
    {
        let src = tmp.path().join("show");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("ep.mp3"), (0..20_000).map(|i| (i % 7) as u8).collect::<Vec<u8>>()).unwrap();
        std::fs::write(
            src.join("show.toml"),
            format!("title = \"Paid Radio\"\ncategory = \"Technology\"\n[ads]\nplacements = [\"pre\"]\npayout = \"{SHOW}\"\n"),
        )
        .unwrap();
        std::fs::write(src.join("episodes.toml"), "[[episode]]\nfile = \"ep.mp3\"\ntitle = \"One\"\npub_date = \"2026-06-01\"\nduration = 10\n").unwrap();
        let out = tmp.path().join("built-show");
        let built = build_show(&load_input(&[src]).unwrap(), show_name, &show_site, None, &out).unwrap();
        assert!(built.ads);
        let site = build_from_dir(&out, show_name).unwrap();
        let rec = SiteRecord::sign(&pod_key, show_name, &Hash::of(&site.manifest.to_bytes()), 1, site.manifest.total_bytes(), vec![]).unwrap();
        publisher.publish_site(&site, rec.clone()).await.unwrap();
        let (p, h, a) = (publisher.clone(), adv_hosting.clone(), adv_ads.clone());
        wait_for("the advertiser can read the show's signed opt-in", 120, || {
            let (p, h, a, rec) = (p.clone(), h.clone(), a.clone(), rec.clone());
            async move {
                p.regossip(&rec).await;
                let Some(r) = h.record_for(&rec.site_id).await else {
                    return false;
                };
                a.show_opt_in(&h, &r).await.is_some_and(|o| o.payout.as_deref() == Some(SHOW))
            }
        })
        .await;
    }

    // ── 2. impressions on two gateways ───────────────────────────────────
    let http = reqwest::Client::builder().user_agent("LoopbackTV/1.0").build().unwrap();
    let base_a = serve(&gw_a).await;
    let base_b = serve(&gw_b).await;
    // The cleared price: one campaign, no floor, so it pays its $2 bid.
    let sel: serde_json::Value = http
        .get(format!("{base_a}/ads/v1/select?placement=ctv_preroll"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(sel["winner"]["cleared_cpm_usd"], 2.0);
    for i in 0..3 {
        assert!(watch(&http, &base_a, &format!("tv-a{i}")).await);
    }
    for i in 0..2 {
        assert!(watch(&http, &base_b, &format!("tv-b{i}")).await);
    }
    let creative = record.creatives.iter().find(|c| c.mime == "audio/mpeg").unwrap().hash.clone();
    for i in 0..2 {
        gw_b.record(Receipt {
            campaign_id: cid.clone(),
            creative: creative.clone(),
            placement: Placement::PodcastPre,
            event: Event::Impression,
            ts: ads::now_unix(),
            listener: format!("podcast-listener-{i}"),
            serving_id: format!("podcast-play-{i}"),
            site: Some(show_site.clone()),
            country: None,
            cpm_micros: 2 * MICROS,
            campaign_seq: record.seq,
        });
    }

    // ── 3. claims, verification, settlement, payout ──────────────────────
    let ca = gw_a.make_claims().await;
    let cb = gw_b.make_claims().await;
    assert_eq!(ca.len(), 1);
    assert_eq!(cb.len(), 1);
    assert_eq!((ca[0].impressions, ca[0].gross_micros), (3, 6_000));
    assert_eq!((cb[0].impressions, cb[0].gross_micros), (4, 8_000));

    let mut ledger = Ledger::new(&cid);
    let round = manage::settle_round(&adv_ads, &adv_hosting, &record, &bids, &mut ledger, Some(&adv_key), FEE, Duration::from_secs(6)).await;
    assert!(round.refused.is_empty(), "{:?}", round.refused);
    assert_eq!(round.settled.len(), 2, "both gateways settled");
    assert_eq!(ledger.spent_micros, 14_000);
    for (s, _) in &round.settled {
        assert!(s.verify());
        assert_eq!(s.payouts.iter().map(|p| p.micros).sum::<u64>(), s.gross_micros);
    }
    let paid = |key: &str| round.settled.iter().find(|(s, _)| s.gateway == key).unwrap().0.clone();
    let sa = paid(gw_a.gateway_key());
    let sb = paid(gw_b.gateway_key());
    assert_eq!(sa.gross_micros, 6_000);
    assert_eq!(sb.gross_micros, 8_000);
    let line = |s: &c0mpute_core::ads::billing::Settlement, r: Role| s.payouts.iter().filter(|p| p.role == r).map(|p| p.micros).sum::<u64>();
    assert_eq!((line(&sa, Role::Gateway), line(&sa, Role::Network), line(&sa, Role::Publisher)), (5_700, 300, 0));
    assert_eq!((line(&sb, Role::Gateway), line(&sb, Role::Network), line(&sb, Role::Publisher)), (4_600, 400, 3_000));
    assert_eq!(sb.payouts.iter().find(|p| p.role == Role::Publisher).unwrap().key, pod_owner);

    assert_eq!(ledger.pay(&rail, &shared, &adv_owner, cap).await, 5);
    assert_eq!(ledger.unpaid_micros(), 0);
    assert_eq!(rail.balance(&adv_owner).await.unwrap().unwrap(), MICROS - 14_000, "budget spent = money moved");
    assert_eq!(got(GW_A), 5_700);
    assert_eq!(got(GW_B), 4_600);
    assert_eq!(got(SHOW), 3_000);
    assert_eq!(got(FEE), 700);
    assert_eq!(ledger.pay(&rail, &shared, &adv_owner, cap).await, 0, "nothing is paid twice");
    // Every line went through the one shared payments path and its ledger.
    assert_eq!(shared.entries().len(), 5);
    assert!(shared.entries().keys().all(|k| k.starts_with("ads:")));
    // With payments off (the default) there is no rail at all.
    assert!(c0mpute_core::payments::rail_from_config(&PaymentsConfig::default()).unwrap().is_none());

    // ── 4. gateways learn the budget left; overspend is refused ──────────
    {
        let l = ledger.clone();
        let a = adv_ads.clone();
        wait_for("gateways learn their settlements", 30, || {
            let (a, l, ga, gb, cid) = (a.clone(), l.clone(), gw_a.clone(), gw_b.clone(), cid.clone());
            async move {
                manage::announce_settlements(&a, &l).await;
                let (va, vb) = (ga.settled(&cid), gb.settled(&cid));
                va.spent_micros == 14_000 && vb.spent_micros == 14_000 && va.through.is_some() && vb.through.is_some()
            }
        })
        .await;
    }
    assert!(gw_a.make_claims().await.is_empty(), "nothing left to claim");
    // $0.002 left: A fills one more slot, then its own unsettled spend
    // reaches the budget and it stops.
    assert!(watch(&http, &base_a, "tv-a9").await);
    assert!(!watch(&http, &base_a, "tv-a10").await, "A must stop at the budget");
    let sel: serde_json::Value = http
        .get(format!("{base_a}/ads/v1/select?placement=ctv_preroll"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(sel["rejected"][0]["reason"], "budget_exhausted");
    // B does not know about A's unsettled impression and serves one too.
    assert!(watch(&http, &base_b, "tv-b9").await);
    assert_eq!(gw_a.make_claims().await.len(), 1);
    assert_eq!(gw_b.make_claims().await.len(), 1);
    let round2 = manage::settle_round(&adv_ads, &adv_hosting, &record, &bids, &mut ledger, Some(&adv_key), FEE, Duration::from_secs(6)).await;
    assert_eq!(round2.settled.len(), 1, "{:?}", round2.refused);
    assert_eq!(round2.refused.len(), 1);
    assert_eq!(
        round2.refused[0].1,
        ClaimError::OverBudget {
            remaining_micros: 0,
            asked_micros: 2_000
        }
    );
    assert_eq!(ledger.spent_micros, budget_total);
    ledger.pay(&rail, &shared, &adv_owner, cap).await;
    assert_eq!(rail.balance(&adv_owner).await.unwrap().unwrap(), MICROS - budget_total, "never more than the budget");
    assert_eq!(
        got(GW_A) + got(GW_B) + got(SHOW) + got(FEE),
        budget_total
    );
    // The settlement chain links and every node can check it.
    assert_eq!(ledger.settlements.len(), 3);
    for w in ledger.settlements.windows(2) {
        assert_eq!(w[1].prev, w[0].hash());
    }

    for h in &hosts {
        h.stop();
    }
}
