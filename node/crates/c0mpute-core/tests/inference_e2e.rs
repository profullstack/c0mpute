//! End-to-end: `infernet.inference` peer to peer on loopback libp2p.
//!
//! A worker runs the real inference runner and bid filter in front of an
//! OpenAI-compatible runtime; a buyer auctions chat turns through the jobs
//! bridge (what `c0mpute chat --p2p` does) and gets answers fetched from the
//! worker and checked against its receipt. No c0mpute.com, no `infernet`
//! binary. The runtime is a stand-in on loopback, unless
//! `C0MPUTE_INFERENCE_LIVE_URL` names a real one (e.g. Ollama at
//! http://127.0.0.1:11434), in which case that one answers.

use std::sync::Arc;
use std::time::Duration;

use axum::routing::{get, post};
use axum::{Json, Router};
use c0mpute_core::inference;
use c0mpute_core::jobs_gateway::JobsBridge;
use c0mpute_core::pricing::PricingConfig;
use c0mpute_core::dispatch;
use c0mpute_net::{ChunkSource, Libp2pNetwork, NetworkConfig};
use c0mpute_proto::Hash;
use c0mpute_store::ChunkStore;
use libp2p::multiaddr::Protocol;
use serde_json::{Value, json};
use tempfile::tempdir;

struct StoreSource(ChunkStore);

/// The worker tests share process-wide state (the ready-model list, the
/// freed-slot announcement); run them one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

/// An OpenAI-compatible runtime that echoes the whole conversation it got.
async fn fake_runtime() -> String {
    let app = Router::new()
        .route("/v1/models", get(|| async { Json(json!({"object": "list", "data": [{"id": "tiny:1b"}]})) }))
        .route(
            "/v1/chat/completions",
            post(|Json(b): Json<Value>| async move {
                let turns: Vec<String> = b["messages"].as_array().unwrap().iter().map(|m| format!("{}={}", m["role"].as_str().unwrap(), m["content"].as_str().unwrap())).collect();
                Json(json!({"choices": [{"message": {"role": "assistant", "content": format!("[{}] {}", b["model"].as_str().unwrap(), turns.join(" | "))}, "finish_reason": "stop"}],
                            "usage": {"prompt_tokens": 5, "completion_tokens": 5}}))
            }),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chat_turns_are_answered_peer_to_peer() {
    let _serial = SERIAL.lock().await;
    let _ = tracing_subscriber::fmt().with_env_filter("c0mpute_core=info,c0mpute_net=warn").with_test_writer().try_init();
    // Keep the loopback buyer and worker off the live LAN network.
    unsafe { std::env::set_var("C0MPUTE_DISABLE_MDNS", "1") };
    let live = std::env::var("C0MPUTE_INFERENCE_LIVE_URL").ok().filter(|s| !s.is_empty());
    let base = match &live {
        Some(u) => u.trim_end_matches('/').to_string(),
        None => fake_runtime().await,
    };
    let tmp = tempdir().unwrap();

    // ── worker: refresh loop, runner, bid filter ───────────────────────
    tokio::spawn(inference::refresh_loop(base.clone()));
    let mut models = Vec::new();
    for _ in 0..50 {
        models = inference::ready_models();
        if !models.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!models.is_empty(), "the runtime at {base} lists no chat models");
    let model = models[0].clone();
    let (worker, worker_store) = node(&tmp.path().join("w"), vec![]).await;
    let worker_addr = wait_for_listen(&worker).await.with(Protocol::P2p(worker.peer_id()));
    let load = inference::worker_load(1);
    let runner = inference::spawn_inference_runner(worker.clone(), worker_store, base.clone(), load.clone());
    let mut tags = vec![inference::ROLE_TAG.to_string(), "c0mpute:cpu".to_string()];
    tags.extend(models.iter().map(|m| inference::model_tag(m)));
    dispatch::run_worker_subscriber_with_load(worker.clone(), inference::WORKLOAD.to_string(), tags, Some(runner), Some(inference::offer_filter()), Some(PricingConfig { min_usd: 0.0, ..PricingConfig::default() }), Some(load));

    // ── buyer ──────────────────────────────────────────────────────────
    let (buyer, buyer_store) = node(&tmp.path().join("b"), vec![worker_addr]).await;
    // This paid inference scenario needs a cap above the default node floor.
    let bridge = JobsBridge::new(buyer.clone(), buyer_store, "test-token-0123456789abcdef".into(), 1.0, vec![]).unwrap().with_bid_window(Duration::from_secs(2));
    tokio::time::sleep(Duration::from_secs(2)).await;

    // A multi-turn conversation, any model.
    let spec = json!({"v": 1, "max_tokens": 32, "temperature": 0.0, "messages": [
        {"role": "system", "content": "Answer in one word."},
        {"role": "user", "content": "Name a primary color."},
        {"role": "assistant", "content": "Red."},
        {"role": "user", "content": "Another one?"},
    ]});
    let out = bridge.submit_once(inference::WORKLOAD, spec, Duration::from_secs(240)).await.expect("answered");
    assert_eq!(out["worker"], worker.peer_id().to_base58());
    let r = &out["result"];
    assert!(inference::valid_result(r), "{r}");
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["model"], model.as_str());
    let text = r["text"].as_str().unwrap();
    assert!(!text.trim().is_empty());
    if live.is_none() {
        // The worker forwarded every turn, in order, to the runtime.
        assert_eq!(text, "[tiny:1b] system=Answer in one word. | user=Name a primary color. | assistant=Red. | user=Another one?");
    } else {
        eprintln!("live answer from {model}: {text}");
    }

    // Asking for the model by name works; a model nobody serves gets no bid.
    let named = json!({"v": 1, "model": model, "messages": [{"role": "user", "content": "hi"}], "max_tokens": 8});
    let out = bridge.submit_once(inference::WORKLOAD, named, Duration::from_secs(240)).await.expect("named model");
    assert_eq!(out["result"]["model"], model.as_str());
    let missing = json!({"v": 1, "model": "not-served:9b", "messages": [{"role": "user", "content": "hi"}]});
    let err = bridge.submit_once(inference::WORKLOAD, missing, Duration::from_secs(8)).await.unwrap_err().to_string();
    assert!(err.contains("no_bids") || err.contains("timeout"), "{err}");
    // An invalid spec never leaves the buyer.
    let bad = json!({"v": 1, "messages": [{"role": "tool", "content": "x"}]});
    assert!(bridge.submit_once(inference::WORKLOAD, bad, Duration::from_secs(5)).await.is_err());
}

/// Like [`fake_runtime`], but each answer takes `delay` and the runtime
/// records how many it ever ran at once.
async fn slow_runtime(delay: Duration, peak: Arc<std::sync::atomic::AtomicUsize>) -> String {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let now = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/v1/models", get(|| async { Json(json!({"object": "list", "data": [{"id": "tiny:1b"}]})) }))
        .route(
            "/v1/chat/completions",
            post(move |Json(b): Json<Value>| {
                let (now, peak) = (now.clone(), peak.clone());
                async move {
                    let n = now.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(n, Ordering::SeqCst);
                    tokio::time::sleep(delay).await;
                    now.fetch_sub(1, Ordering::SeqCst);
                    let last = b["messages"].as_array().unwrap().last().unwrap()["content"].as_str().unwrap().to_string();
                    Json(json!({"choices": [{"message": {"role": "assistant", "content": format!("[tiny:1b] user={last}")}, "finish_reason": "stop"}],
                                "usage": {"prompt_tokens": 5, "completion_tokens": 5}}))
                }
            }),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}")
}

/// The only capable worker is full: the jobs it cannot take wait in the
/// bridge's queue and are re-auctioned as its slot frees (it re-advertises
/// when a job ends), instead of failing with no_bids. The runtime never runs
/// more than the worker's one slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_full_worker_gets_the_queued_jobs_in_turn() {
    let _serial = SERIAL.lock().await;
    let _ = tracing_subscriber::fmt().with_env_filter("c0mpute_core=info,c0mpute_net=warn").with_test_writer().try_init();
    unsafe { std::env::set_var("C0MPUTE_DISABLE_MDNS", "1") };
    let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let base = slow_runtime(Duration::from_millis(1500), peak.clone()).await;
    let tmp = tempdir().unwrap();
    tokio::spawn(inference::refresh_loop(base.clone()));
    for _ in 0..50 {
        if !inference::ready_models().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (worker, worker_store) = node(&tmp.path().join("w"), vec![]).await;
    let worker_addr = wait_for_listen(&worker).await.with(Protocol::P2p(worker.peer_id()));
    let load = inference::worker_load(1);
    let runner = inference::spawn_inference_runner(worker.clone(), worker_store, base.clone(), load.clone());
    let tags = vec![inference::ROLE_TAG.to_string(), inference::ACCEPT_TAG.to_string(), "c0mpute:cpu".to_string(), inference::model_tag("tiny:1b")];
    tokio::spawn(c0mpute_core::advertise_loop(worker.clone(), tags.clone(), json!({}), None, Duration::from_secs(60)));
    dispatch::run_worker_subscriber_with_load(worker.clone(), inference::WORKLOAD.to_string(), tags, Some(runner), Some(inference::offer_filter()), Some(PricingConfig { min_usd: 0.0, ..PricingConfig::default() }), Some(load));

    let (buyer, buyer_store) = node(&tmp.path().join("b"), vec![worker_addr]).await;
    buyer.subscribe(c0mpute_net::topics::CAPABILITY_TOPIC).await.unwrap();
    let bridge = JobsBridge::new(buyer.clone(), buyer_store, "test-token-0123456789abcdef".into(), 1.0, vec![]).unwrap().with_bid_window(Duration::from_secs(1));
    tokio::time::sleep(Duration::from_secs(2)).await;

    let mut jobs = Vec::new();
    for i in 0..3 {
        let bridge = bridge.clone();
        jobs.push(tokio::spawn(async move {
            let spec = json!({"v": 1, "model": "tiny:1b", "max_tokens": 8, "messages": [{"role": "user", "content": format!("job {i}")}]});
            bridge.submit_once(inference::WORKLOAD, spec, Duration::from_secs(90)).await
        }));
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    for (i, j) in jobs.into_iter().enumerate() {
        let out = j.await.unwrap().unwrap_or_else(|e| panic!("job {i} was not answered: {e}"));
        assert_eq!(out["worker"], worker.peer_id().to_base58());
        assert_eq!(out["result"]["text"], format!("[tiny:1b] user=job {i}"));
    }
    assert_eq!(peak.load(std::sync::atomic::Ordering::SeqCst), 1, "never more answers at once than the worker's slots");
}

/// Nobody runs the asked-for model at all: the job fails at once with
/// `no_capable_node` instead of waiting out its deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_no_node_can_run_fails_at_once() {
    unsafe { std::env::set_var("C0MPUTE_DISABLE_MDNS", "1") };
    let tmp = tempdir().unwrap();
    let (buyer, buyer_store) = node(&tmp.path().join("b"), vec![]).await;
    let bridge = JobsBridge::new(buyer.clone(), buyer_store, "test-token-0123456789abcdef".into(), 1.0, vec![])
        .unwrap()
        .with_bid_window(Duration::from_millis(500))
        .with_registry_grace(Duration::ZERO);
    let spec = json!({"v": 1, "model": "nobody-serves:70b", "messages": [{"role": "user", "content": "hi"}]});
    let t = std::time::Instant::now();
    let err = bridge.submit_once(inference::WORKLOAD, spec, Duration::from_secs(120)).await.unwrap_err().to_string();
    assert!(err.contains("no_capable_node"), "{err}");
    assert!(t.elapsed() < Duration::from_secs(30), "failed after {:?}, not at once", t.elapsed());
}

/// The only worker is busy: a job that cannot get a slot within its requeues
/// ends with "Network is busy" (requeued at the back each time), while the
/// job behind it in line is answered as the slot frees.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requeues_run_out_with_network_busy_and_others_proceed() {
    let _serial = SERIAL.lock().await;
    let _ = tracing_subscriber::fmt().with_env_filter("c0mpute_core=debug,c0mpute_net=warn").with_test_writer().try_init();
    unsafe { std::env::set_var("C0MPUTE_DISABLE_MDNS", "1") };
    let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let base = slow_runtime(Duration::from_secs(4), peak.clone()).await;
    let tmp = tempdir().unwrap();
    tokio::spawn(inference::refresh_loop(base.clone()));
    for _ in 0..50 {
        if !inference::ready_models().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (worker, worker_store) = node(&tmp.path().join("w"), vec![]).await;
    let worker_addr = wait_for_listen(&worker).await.with(Protocol::P2p(worker.peer_id()));
    let load = inference::worker_load(1);
    let runner = inference::spawn_inference_runner(worker.clone(), worker_store, base.clone(), load.clone());
    let tags = vec![inference::ROLE_TAG.to_string(), inference::ACCEPT_TAG.to_string(), "c0mpute:cpu".to_string(), inference::model_tag("tiny:1b")];
    tokio::spawn(c0mpute_core::advertise_loop(worker.clone(), tags.clone(), json!({}), None, Duration::from_secs(60)));
    dispatch::run_worker_subscriber_with_load(worker.clone(), inference::WORKLOAD.to_string(), tags, Some(runner), Some(inference::offer_filter()), Some(PricingConfig { min_usd: 0.0, ..PricingConfig::default() }), Some(load));

    let (buyer, buyer_store) = node(&tmp.path().join("b"), vec![worker_addr]).await;
    let bridge = JobsBridge::new(buyer.clone(), buyer_store, "test-token-0123456789abcdef".into(), 1.0, vec![])
        .unwrap()
        .with_bid_window(Duration::from_secs(1))
        .with_max_requeues(1);
    tokio::time::sleep(Duration::from_secs(2)).await;

    let mut jobs = Vec::new();
    for i in 0..3 {
        let bridge = bridge.clone();
        jobs.push(tokio::spawn(async move {
            let spec = json!({"v": 1, "model": "tiny:1b", "max_tokens": 8, "messages": [{"role": "user", "content": format!("job {i}")}]});
            bridge.submit_once(inference::WORKLOAD, spec, Duration::from_secs(120)).await
        }));
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let (mut answered, mut busy) = (0, 0);
    for j in jobs {
        match j.await.unwrap() {
            Ok(out) => {
                assert_eq!(out["result"]["ok"], true);
                answered += 1;
            }
            Err(e) => {
                let e = e.to_string();
                assert!(e.contains("network_busy") && e.contains("Network is busy. Run a node or wait."), "{e}");
                busy += 1;
            }
        }
    }
    assert_eq!((answered, busy), (2, 1), "one runs, one gets the freed slot, the last runs out of requeues");
    assert_eq!(peak.load(std::sync::atomic::Ordering::SeqCst), 1);
}
