//! One identity running twice ("twins"), on loopback libp2p.
//!
//! * Two swarms started from one copied identity dir advertise one peer id
//!   with two instance ids; the node board's registry counts both.
//! * A winner that answers "this accept is for my twin" does not fail the job
//!   or count as a compute fault: the buyer re-awards it to the next bidder at
//!   once and the job completes.

use std::sync::Arc;
use std::time::Duration;

use c0mpute_core::pricing::{PricingConfig, PricingMode};
use c0mpute_core::routing::SelectionPolicy;
use c0mpute_core::runner::AcceptedJob;
use c0mpute_core::{
    JobAuction, Registry, advertise_loop, buyer, dispatch, pieces, run_auction, runner,
};
use c0mpute_net::topics::{JobAccept, JobReceipt, JobStatus, job_topic};
use c0mpute_net::{ChunkSource, Libp2pNetwork, NetworkConfig, PeerId};
use c0mpute_proto::Hash;
use c0mpute_store::ChunkStore;
use libp2p::multiaddr::Protocol;
use serde_json::json;
use tempfile::{TempDir, tempdir};
use tokio::sync::mpsc;

struct StoreSource(ChunkStore);

#[async_trait::async_trait]
impl ChunkSource for StoreSource {
    async fn read_chunk(&self, hash: &Hash) -> anyhow::Result<Vec<u8>> {
        self.0.get(hash).await
    }
}

struct Node {
    net: Arc<Libp2pNetwork>,
    store: ChunkStore,
    dir: TempDir,
}

async fn node_in(dir: TempDir, bootstrap: Vec<libp2p::Multiaddr>) -> Node {
    let store = ChunkStore::open(dir.path().join("store")).await.unwrap();
    let net = Libp2pNetwork::spawn(
        NetworkConfig::for_dir(dir.path().join("id"))
            .with_listen(vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()])
            .with_local_source(Arc::new(StoreSource(store.clone())))
            .with_bootstrap(bootstrap),
    )
    .await
    .unwrap();
    Node {
        net: Arc::new(net),
        store,
        dir,
    }
}

async fn node(bootstrap: Vec<libp2p::Multiaddr>) -> Node {
    node_in(tempdir().unwrap(), bootstrap).await
}

/// A second node started from a copy of `of`'s identity dir.
async fn twin(of: &Node, bootstrap: Vec<libp2p::Multiaddr>) -> Node {
    let dir = tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("id")).unwrap();
    for e in std::fs::read_dir(of.dir.path().join("id"))
        .unwrap()
        .flatten()
    {
        std::fs::copy(e.path(), dir.path().join("id").join(e.file_name())).unwrap();
    }
    node_in(dir, bootstrap).await
}

async fn addr(net: &Libp2pNetwork) -> libp2p::Multiaddr {
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

fn quiet() {
    // SAFETY: set before any node starts; the test owns its process env.
    unsafe { std::env::set_var("C0MPUTE_DISABLE_MDNS", "1") };
    let _ = tracing_subscriber::fmt()
        .with_env_filter("c0mpute_core=info,c0mpute_net=warn")
        .with_test_writer()
        .try_init();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_swarms_sharing_a_key_show_as_two_instances_of_one_peer() {
    quiet();
    let board = node(vec![]).await;
    let board_addr = addr(&board.net).await;
    let registry = Registry::new();
    registry.run(board.net.clone());

    // Twins reach the board through different peers, as two machines do. (A
    // peer that already has a connection to a peer id only gossips with the
    // first one, so two twins dialing one peer would hide one of them.)
    let mut relays = Vec::new();
    for _ in 0..2 {
        let r = node(vec![board_addr.clone()]).await;
        Registry::new().run(r.net.clone()); // joins the capability topic
        relays.push(r);
    }
    let a = node(vec![addr(&relays[0].net).await]).await;
    let b = twin(&a, vec![addr(&relays[1].net).await]).await;
    assert_eq!(a.net.peer_id(), b.net.peer_id(), "one identity");
    assert_ne!(a.net.instance_id(), b.net.instance_id(), "two processes");

    for n in [&a, &b] {
        tokio::spawn(advertise_loop(
            n.net.clone(),
            vec!["c0mpute:cpu".into()],
            json!({"version": "test"}),
            None,
            Duration::from_millis(500),
        ));
    }

    let peer = a.net.peer_id();
    let mut seen = 0;
    for _ in 0..80 {
        seen = registry
            .instance_counts()
            .await
            .into_iter()
            .find(|(p, _)| *p == peer)
            .map(|(_, n)| n)
            .unwrap_or(0);
        if seen >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(seen, 2, "the board heard both processes behind one peer id");
    // Each ad names its own instance.
    let ad = registry
        .list()
        .await
        .into_iter()
        .find(|(p, _)| *p == peer)
        .unwrap()
        .1;
    let inst = ad.hardware["instance"].as_str().unwrap().to_string();
    assert!(inst == a.net.instance_id() || inst == b.net.instance_id());
}

/// A worker for `workload` bidding a fixed `price`. Its runner hands each
/// won job to `on_job`.
fn worker<F, Fut>(w: &Node, workload: &str, price: f64, on_job: F)
where
    F: Fn(Arc<Libp2pNetwork>, ChunkStore, JobAccept) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let (tx, mut rx) = mpsc::channel::<AcceptedJob>(8);
    let pricing = PricingConfig {
        mode: PricingMode::Fixed,
        min_usd: 0.0,
        fixed_usd: Some(price),
    };
    dispatch::run_worker_subscriber_priced(
        w.net.clone(),
        workload.to_string(),
        vec![],
        Some(tx),
        None,
        Some(pricing),
    );
    let (net, store) = (w.net.clone(), w.store.clone());
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            tokio::spawn(on_job(net.clone(), store.clone(), job.accept));
        }
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_twin_fault_is_re_awarded_to_the_next_bidder_without_failing_the_job() {
    quiet();
    let hub = node(vec![]).await;
    let hub_addr = addr(&hub.net).await;
    let cheap = node(vec![hub_addr.clone()]).await;
    let honest = node(vec![hub_addr.clone()]).await;
    let buyer_node = node(vec![
        hub_addr.clone(),
        addr(&cheap.net).await,
        addr(&honest.net).await,
    ])
    .await;
    let workload = "tts.synthesize";

    // The cheapest bidder wins, but the accept turns out to be its twin's:
    // it answers with the twin fault (what a process with this peer id and
    // another instance says).
    worker(&cheap, workload, 0.001, |net, _store, accept| async move {
        runner::publish_twin_fault(&net, &accept).await;
    });
    // The next bidder does the work.
    worker(&honest, workload, 0.002, |net, store, accept| async move {
        let hash = pieces::publish_result_bytes(&net, &store, &accept, b"{\"ok\":true}")
            .await
            .unwrap();
        let r = JobReceipt {
            job_id: accept.job_id.clone(),
            worker_peer_id: net.peer_id().to_base58(),
            worker_did: None,
            buyer_peer_id: accept.buyer_peer_id.clone(),
            output_hash: Some(hash),
            status: JobStatus::Completed,
            completed_at_ms: 0,
            signature: None,
            external_ref: None,
            instance: Some(net.instance_id().to_string()),
            fault: None,
        };
        let topic = job_topic(&accept.workload_type);
        let bytes = serde_json::to_vec(&r).unwrap();
        let buyer: PeerId = accept.buyer_peer_id.parse().unwrap();
        let _ = net
            .send_direct(buyer, &topic, &bytes, Duration::from_secs(5))
            .await;
        let _ = net.publish(&topic, bytes).await;
    });
    tokio::time::sleep(Duration::from_secs(2)).await;

    let auction = JobAuction::new(
        workload,
        json!({"text": "hello", "engine": "kokoro", "voice": "af_heart", "locale": "en-US", "speed": 1.0}),
    )
    .with_policy(SelectionPolicy::Cheapest)
    .with_max_price_usd(0.01)
    .with_receipt_timeout(Duration::from_secs(30));
    let out = tokio::time::timeout(
        Duration::from_secs(60),
        run_auction(buyer_node.net.clone(), auction),
    )
    .await
    .expect("auction finished")
    .expect("the job completes after the re-award");
    assert_eq!(out.receipt.status, JobStatus::Completed);
    assert_eq!(
        out.accepted_bid.bidder_peer_id,
        honest.net.peer_id().to_base58(),
        "the result came from the bidder the job was re-awarded to"
    );
    // One twin fault is not a compute fault: the peer is not kept back yet.
    let cheap_id = cheap.net.peer_id().to_base58();
    assert!(!buyer::identity_faulty(&cheap_id));
    // It is after it keeps happening.
    for _ in 0..buyer::IDENTITY_FAULT_BENCH {
        buyer::note_identity_fault(&cheap_id);
    }
    assert!(buyer::identity_faulty(&cheap_id));
}
