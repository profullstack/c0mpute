//! End-to-end marketplace hosting on loopback libp2p: a publisher names two
//! of five hosting nodes as preferred (paid) hosts in a v2 record; both take
//! copies and the site settles at three copies with no coordinator; the
//! publisher challenges every holder (random chunk, hash-verified, signed
//! receipts on gossip); a settlement round pays exactly the two preferred
//! holders on a fake rail, and a second round pays nothing twice.

use std::sync::Arc;
use std::time::Duration;

use c0mpute_core::hosting::site::{HostingTerms, SiteRecord, build_from_dir, owner_hex};
use c0mpute_core::hosting::{ChallengeScope, Hosting, Options, Timing};
use c0mpute_core::payments::{FakeRail, Ledger, Outcome, Payee};
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

fn wallet(i: usize) -> String {
    format!("0x{:040x}", 0xa11ce + i)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preferred_hosts_hold_pass_challenges_and_get_paid() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("c0mpute_core=info,c0mpute_net=warn")
        .with_test_writer()
        .try_init();
    let tmp = tempdir().unwrap();

    // Five hosting nodes, each asking $0.004/GiB-month with a payout wallet.
    let host_opts = |dir: &std::path::Path, i: usize| Options {
        hosts: true,
        // Plenty of (pretend) disk: the test box may be below the free-disk floor.
        disk_override: Some((1 << 40, 2 << 40)),
        catalog_path: Some(dir.join("catalog.json")),
        timing: fast(),
        price: Some(4_000),
        payout: Some(Payee {
            wallet: wallet(i),
            coin: "USDC_POL".into(),
        }),
        ..Default::default()
    };
    let (hub_net, hub_store) = node(&tmp.path().join("h0"), vec![]).await;
    let hub_addr = listen_addr(&hub_net).await;
    let mut hosts: Vec<Hosting> = vec![Hosting::start(hub_net, hub_store, host_opts(&tmp.path().join("h0"), 0)).await.unwrap()];
    for i in 1..5 {
        let d = tmp.path().join(format!("h{i}"));
        let (n, s) = node(&d, vec![hub_addr.clone()]).await;
        hosts.push(Hosting::start(n, s, host_opts(&d, i)).await.unwrap());
    }

    // The publisher: owns and pays for the site, challenges its holders.
    let owner = libp2p::identity::Keypair::generate_ed25519();
    let owner_key = owner_hex(&owner).unwrap();
    let pd = tmp.path().join("pub");
    let (pn, ps) = node(&pd, vec![hub_addr.clone()]).await;
    let publisher = Hosting::start(
        pn,
        ps,
        Options {
            hosts: false,
            timing: fast(),
            challenger: Some(owner.clone()),
            challenge: ChallengeScope::Own,
            // Rounds are driven by the test.
            challenge_every: Duration::from_secs(3600),
            receipts_path: Some(pd.join("receipts.jsonl")),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // A gateway that challenges everything with its own key: its receipts
    // travel on gossip but do not count for the owner's settlement.
    let gd = tmp.path().join("gw");
    let (gn, gs) = node(&gd, vec![hub_addr.clone()]).await;
    let gateway = Hosting::start(
        gn,
        gs,
        Options {
            hosts: false,
            timing: fast(),
            challenger: Some(libp2p::identity::Keypair::generate_ed25519()),
            challenge: ChallengeScope::All,
            challenge_every: Duration::from_secs(3600),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_secs(3)).await;

    // Two of the five are preferred, at $1/GiB-month.
    let preferred: Vec<String> = vec![hosts[1].peer_id().to_string(), hosts[3].peer_id().to_string()];
    let site_dir = tmp.path().join("site");
    std::fs::create_dir_all(&site_dir).unwrap();
    std::fs::write(site_dir.join("index.html"), b"<h1>paid hosting</h1>").unwrap();
    let big: Vec<u8> = (0..(5 * 1024 * 1024)).map(|i| (i % 253) as u8).collect();
    std::fs::write(site_dir.join("big.bin"), &big).unwrap();
    let built = build_from_dir(&site_dir, "market").unwrap();
    let rec = SiteRecord::sign_with_terms(
        &owner,
        "market",
        &Hash::of(&built.manifest.to_bytes()),
        1,
        built.manifest.total_bytes(),
        vec![],
        Some(HostingTerms {
            preferred_hosts: preferred.clone(),
            price_per_gib_month: 1_000_000,
            payer: None,
        }),
    )
    .unwrap();
    assert_eq!(rec.v, 2);
    let site = rec.site_id.clone();
    publisher.publish_site(&built, rec.clone()).await.unwrap();

    // Both preferred hosts hold copies and the site settles at exactly three.
    let (p, s, r, pref) = (publisher.clone(), site.clone(), rec.clone(), preferred.clone());
    wait_for("preferred copies and three in total", 90, || {
        let (p, s, r, pref) = (p.clone(), s.clone(), r.clone(), pref.clone());
        async move {
            p.regossip(&r).await;
            let hs = p.holders(&s).await;
            hs.len() == 3 && pref.iter().all(|x| hs.contains(x))
        }
    })
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let holders = hosts[0].holders(&site).await;
    assert_eq!(holders.len(), 3, "stays at three: {holders:?}");
    for x in &preferred {
        assert!(holders.contains(x), "preferred {x} holds a copy");
    }
    assert_eq!(publisher.preferred(&site).await, preferred);

    // A challenge round: every holder passes. (A copy released moments ago
    // may still be listed by the challenger; it keeps serving through its
    // grace period, so it passes too rather than failing.)
    let receipts = publisher.challenge_round().await;
    assert!(receipts.len() >= 3, "one challenge per holder: {receipts:?}");
    assert!(receipts.iter().all(|r| r.ok && r.verify() && r.challenger == owner_key), "{receipts:?}");
    for x in &preferred {
        assert!(receipts.iter().any(|r| &r.holder == x), "preferred {x} was challenged");
    }
    let pd2 = pd.clone();
    assert_eq!(
        std::fs::read_to_string(pd2.join("receipts.jsonl")).unwrap().lines().count(),
        receipts.len(),
        "receipts are kept on disk for settlement"
    );
    // The receipts reach other nodes over gossip.
    let h0 = hosts[0].clone();
    let (s, x) = (site.clone(), preferred[0].clone());
    wait_for("receipts on gossip", 20, || {
        let (h0, s, x) = (h0.clone(), s.clone(), x.clone());
        async move { !h0.receipts_for(&s, &x).await.is_empty() }
    })
    .await;

    // The gateway's receipts arrive too, but are not the payer's.
    let g_receipts = gateway.challenge_round().await;
    assert!(!g_receipts.is_empty());

    // Settlement on a fake rail, prepaid by the owner.
    let rail = FakeRail::new();
    rail.fund(&owner_key, 10_000_000);
    let ledger = Ledger::in_memory();
    let now = c0mpute_core::payments::now_secs();
    let month = c0mpute_core::hosting::settle::MONTH_SECS;
    let (from, to) = (now + 1 - month, now + 1);
    let (dues, outcomes) = publisher.settle_round(&rail, &ledger, from, to, 5_000_000).await;
    assert_eq!(dues.len(), 2, "dues for the two preferred hosts only: {dues:?}");
    let expected = c0mpute_core::hosting::settle::owed(1_000_000, rec.size, month, 1, 1);
    assert!(expected > 4_000, "5 MiB at $1/GiB-month for a month is ~$0.0049: {expected}");
    for d in &dues {
        assert_eq!((d.passed, d.challenged), (1, 1), "gateway receipts do not count: {d:?}");
        assert_eq!(d.amount, expected);
    }
    assert_eq!(outcomes.len(), 2);
    assert!(outcomes.iter().all(|(_, o)| matches!(o, Outcome::Paid(_))), "{outcomes:?}");
    let idx = |peer: &str| hosts.iter().position(|h| h.peer_id() == peer).unwrap();
    for x in &preferred {
        assert_eq!(rail.received(&wallet(idx(x))), expected, "preferred {x} paid");
    }
    let free = holders.iter().find(|h| !preferred.contains(h)).unwrap();
    assert_eq!(rail.received(&wallet(idx(free))), 0, "the free copy is not paid");
    assert_eq!(rail.payments().len(), 2);

    // Same window again: nothing pays twice.
    let (_, again) = publisher.settle_round(&rail, &ledger, from, to, 5_000_000).await;
    assert!(again.iter().all(|(_, o)| *o == Outcome::AlreadyPaid), "{again:?}");
    assert_eq!(rail.payments().len(), 2);
}
