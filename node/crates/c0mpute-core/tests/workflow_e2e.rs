//! End-to-end: a `workflow.step` worker and a `c0mpute jobs serve` bridge on
//! loopback libp2p (direct dial, no mDNS reliance). An HTTP client posts to
//! the bridge; the bridge auctions the step, the real workflow runner wins
//! and runs it, and the bridge fetches the result by hash from the worker.
//! Covers an inline transform, a large spec carried by hash, auth, unknown
//! workloads, and a bidder allowlist that leaves no bids.

use std::sync::Arc;
use std::time::Duration;

use c0mpute_core::jobs_gateway::{self, JobsBridge};
use c0mpute_core::pricing::PricingConfig;
use c0mpute_core::{dispatch, workflow};
use c0mpute_net::{ChunkSource, Libp2pNetwork, NetworkConfig};
use c0mpute_proto::Hash;
use c0mpute_store::ChunkStore;
use libp2p::multiaddr::Protocol;
use serde_json::{Value, json};
use tempfile::tempdir;

struct StoreSource(ChunkStore);

#[async_trait::async_trait]
impl ChunkSource for StoreSource {
    async fn read_chunk(&self, hash: &Hash) -> anyhow::Result<Vec<u8>> {
        self.0.get(hash).await
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

async fn wait_for_listen(net: &Libp2pNetwork) -> libp2p::Multiaddr {
    for _ in 0..50 {
        if let Ok(addrs) = net.listen_addrs().await
            && let Some(a) = addrs.into_iter().next()
        {
            return a;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no listen addr after 2.5s");
}

const TOKEN: &str = "test-token-0123456789abcdef";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transform_step_round_trips_through_the_bridge() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("c0mpute_core=info,c0mpute_net=warn")
        .with_test_writer()
        .try_init();
    let tmp = tempdir().unwrap();

    // ── worker: the real workflow.step runner ──────────────────────────
    let (worker, worker_store) = node(&tmp.path().join("w"), vec![]).await;
    let worker_addr = wait_for_listen(&worker)
        .await
        .with(Protocol::P2p(worker.peer_id()));
    let load = workflow::worker_load();
    let runner = workflow::spawn_workflow_runner(worker.clone(), worker_store, load.clone());
    dispatch::run_worker_subscriber_with_load(
        worker.clone(),
        workflow::WORKLOAD.to_string(),
        vec![workflow::ROLE_TAG.to_string(), "c0mpute:cpu".to_string()],
        Some(runner),
        None,
        Some(PricingConfig::default()),
        Some(load),
    );

    // ── buyer: the bridge, dialing the worker directly ─────────────────
    let (buyer, buyer_store) = node(&tmp.path().join("b"), vec![worker_addr]).await;
    let bridge = JobsBridge::new(buyer.clone(), buyer_store, TOKEN.into(), 0.0, vec![])
        .unwrap()
        .with_bid_window(Duration::from_secs(2));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, jobs_gateway::router(bridge)).await });
    // gossipsub mesh warm-up.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let http = reqwest::Client::new();
    let base = format!("http://{addr}");

    let health: Value = http
        .get(format!("{base}/healthz"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["ok"], true);
    assert_eq!(health["peer_id"], buyer.peer_id().to_base58());
    assert_eq!(
        health["workloads"],
        json!([
            "workflow.step",
            "crawl.fetch",
            "ocr.extract",
            "embed.text",
            "blender.render-frame"
        ])
    );

    let post = |body: Value, token: &'static str| {
        let http = http.clone();
        let url = format!("{base}/v1/jobs");
        async move {
            let r = http
                .post(url)
                .bearer_auth(token)
                .json(&body)
                .send()
                .await
                .unwrap();
            let status = r.status().as_u16();
            (status, r.json::<Value>().await.unwrap())
        }
    };

    // Auth and workload checks come before any auction.
    let (status, _) = post(
        json!({"workload": "workflow.step", "spec": {}}),
        "wrong-token-0123456789abcdef",
    )
    .await;
    assert_eq!(status, 401);
    let (status, body) = post(json!({"workload": "ffmpeg.transcode", "spec": {}}), TOKEN).await;
    assert_eq!(status, 400);
    assert_eq!(body["error"], "unknown_workload");

    // Inline transform.
    let (status, body) = post(
        json!({
            "workload": "workflow.step",
            "spec": {"v": 1, "kind": "transform",
                     "config": {"expression": "items[?price > `5`].name"},
                     "input": {"items": [{"name": "a", "price": 9}, {"name": "b", "price": 1}]}},
            "timeout_ms": 30000
        }),
        TOKEN,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["worker"], worker.peer_id().to_base58());
    assert!(body["job_id"].is_string());
    assert!(body["price"].is_number());
    assert_eq!(body["output_hash"].as_str().unwrap().len(), 64);
    assert_eq!(body["result"]["v"], 1);
    assert_eq!(body["result"]["ok"], true, "{body}");
    assert_eq!(body["result"]["output"], json!(["a"]));

    // A bad spec still completes, as ok:false from the worker.
    let (status, body) = post(
        json!({"workload": "workflow.step", "spec": {"v": 2, "kind": "transform"}}),
        TOKEN,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["result"]["ok"], false);

    // A spec too big for a gossip message rides by hash.
    let big: Vec<String> = (0..2000).map(|i| format!("item-{i:040}")).collect();
    let (status, body) = post(
        json!({
            "workload": "workflow.step",
            "spec": {"v": 1, "kind": "transform", "config": {"expression": "length(@)"}, "input": big},
        }),
        TOKEN,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["result"]["output"], json!(2000), "{body}");

    // Only an unknown peer may bid: nobody does.
    let stranger = c0mpute_net::PeerId::random().to_base58();
    let (status, body) = post(
        json!({
            "workload": "workflow.step",
            "spec": {"v": 1, "kind": "transform", "config": {"expression": "@"}, "input": 1},
            "allow_peers": [stranger],
        }),
        TOKEN,
    )
    .await;
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"], "no_bids");
}
