//! End-to-end: a temporal worker and a buyer on loopback libp2p. A fake
//! `infernet` (tests/fixtures/fake-infernet, same CLI and JSON shapes, no
//! PyTorch) stands in for Infernet so this runs anywhere with python3.
//! Covers capability filtering, the trusted-worker allowlist, single-node
//! training with artifact upload verified against the receipt, and sticky
//! encrypted inference sessions that only their opener can drive.

use std::path::PathBuf;
use std::time::Duration;

use c0mpute_core::temporal::client::{self, OpenRequest, TrainRequest};
use c0mpute_core::temporal::caps::Requirements;
use c0mpute_core::temporal::node::{self, NetOptions, ServeOptions};
use c0mpute_core::temporal::worker::ModelStore;
use serde_json::json;
use tempfile::tempdir;

fn fake_infernet() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-infernet")
}

fn bundle(dir: &std::path::Path) -> PathBuf {
    let h = format!("sha256:{}", "ab".repeat(32));
    let p = dir.join("bundle.json");
    let b = json!({"schema_version": "liquid-agent/v1", "kind": "dataset_bundle",
                   "manifest": {"dataset_hash": h}, "episodes": [{"episode_id": "ep_test1"}]});
    std::fs::write(&p, serde_json::to_vec(&b).unwrap()).unwrap();
    p
}

fn net(dir: &std::path::Path, bootstrap: Vec<String>) -> NetOptions {
    NetOptions { identity_dir: dir.to_path_buf(), listen: vec!["/ip4/127.0.0.1/tcp/0".into()], bootstrap, default_bootstrap: false }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn train_then_serve_a_sticky_session() {
    let _ = tracing_subscriber::fmt().with_env_filter("c0mpute_core=info,c0mpute_net=warn").with_test_writer().try_init();
    let tmp = tempdir().unwrap();

    // ── worker: accepts training ───────────────────────────────────────
    let store = ModelStore::new(tmp.path().join("w/models"));
    let w = node::serve(&ServeOptions {
        net: net(&tmp.path().join("w/id"), vec![]),
        infernet: fake_infernet(),
        models: vec![],
        train: true,
        max_sessions: 8,
        store: store.clone(),
        cache_root: tmp.path().join("w/cache"),
    })
    .await
    .expect("worker serves");
    assert!(w.tags.contains(&"infernet:temporal:train".to_string()));
    assert!(!w.tags.contains(&"infernet:temporal:infer".to_string()), "no models, so no infer tag");
    let wid = w.net.peer_id().to_base58();
    let waddr = format!("{}/p2p/{wid}", w.net.listen_addrs().await.unwrap()[0]);

    let buyer = node::spawn_node(&net(&tmp.path().join("b/id"), vec![waddr.clone()])).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;

    let mk = |families: Vec<&str>, workers: Vec<String>, req: Requirements| TrainRequest {
        dataset_path: bundle(tmp.path()),
        output: tmp.path().join("out"),
        families: families.into_iter().map(String::from).collect(),
        workers,
        max_price_usd: 0.1,
        requirements: req,
        public_url: None,
        bind: "127.0.0.1:0".parse().unwrap(),
        timeout_s: 60,
        retries: 0,
        hyperparameters: json!({}),
        bid_window: Duration::from_secs(2),
    };

    // A worker outside the allowlist is never chosen.
    let err = client::train(buyer.clone(), &mk(vec!["cfc"], vec!["12D3KooWNotTrusted".into()], Requirements::default())).await;
    assert!(err.is_err(), "allowlist excluded the only worker");
    // Requirements the worker cannot meet: no bid.
    let err = client::train(buyer.clone(), &mk(vec!["cfc"], vec![], Requirements { gpu: true, ..Default::default() })).await;
    assert!(err.is_err(), "CPU worker must not bid on a GPU job");

    // The real thing: CfC + GRU baseline on the trusted worker.
    let out = client::train(buyer.clone(), &mk(vec!["cfc", "gru"], vec![wid.clone()], Requirements { min_ram_gb: Some(8), ..Default::default() }))
        .await
        .expect("training completes");
    let results = out["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    for r in results {
        let dir = PathBuf::from(r["model_dir"].as_str().unwrap());
        let got = c0mpute_core::temporal::worker::file_sha256(&dir.join("model.safetensors")).unwrap();
        assert_eq!(r["model_hash"].as_str().unwrap(), got);
        assert_eq!(r["worker_peer_id"], json!(wid));
    }
    assert_eq!(out["receipts"].as_array().unwrap().len(), 2);
    assert_eq!(out["selected"], json!("gru"), "fake scores: gru 0.9 > cfc 0.8, so the baseline stays");
    let cfc_hash = results[0]["model_hash"].as_str().unwrap().to_string();
    assert!(store.dir_for(&cfc_hash).join("manifest.json").exists(), "worker kept the model it trained");

    // ── a second worker process serving that model for inference ───────
    let s = node::serve(&ServeOptions {
        net: net(&tmp.path().join("s/id"), vec![waddr.clone()]),
        infernet: fake_infernet(),
        models: vec![cfc_hash.clone()],
        train: false,
        max_sessions: 8,
        store: store.clone(),
        cache_root: tmp.path().join("s/cache"),
    })
    .await
    .expect("infer worker serves");
    let sid_peer = s.net.peer_id().to_base58();
    assert!(s.tags.iter().any(|t| t.ends_with(cfc_hash.trim_start_matches("sha256:"))));
    let saddr = format!("{}/p2p/{sid_peer}", s.net.listen_addrs().await.unwrap()[0]);
    // The training worker does not subscribe to the infer topic, so it cannot
    // relay it: session clients dial the infer worker (no mDNS on CI).
    let buyer = node::spawn_node(&net(&tmp.path().join("b2/id"), vec![saddr.clone()])).await.unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;

    let sf = client::open_session(
        buyer.clone(),
        &OpenRequest { model_hash: cfc_hash.clone(), session_id: "s1".into(), ttl_s: 60, account_scope: "acct".into(), workers: vec![], max_price_usd: 0.01, bid_window: Duration::from_secs(2) },
    )
    .await
    .expect("session opens");
    assert_eq!(sf.worker_peer_id, sid_peer, "only the infer worker serves this model");

    let step = json!({"observation": {"app": "mail"}, "dt_ms": 0});
    let mut sf2 = sf.clone();
    for i in 0..3 {
        let req = client::predict_request(&step, &sf2).unwrap();
        let r = client::call(buyer.clone(), &sf2, json!({"op": "predict", "request": req}), Duration::from_secs(20)).await.unwrap();
        assert_eq!(r["ok"], json!(true), "{r}");
        assert_eq!(r["result"]["sequence_index"], json!(i));
        assert_eq!(r["result"]["session_id"], json!("s1"), "client sees its own session id");
        sf2.next_seq += 1;
    }
    // Replaying an old index is refused by the runtime, not silently re-run.
    let mut stale = sf.clone();
    stale.next_seq = 1;
    let req = client::predict_request(&step, &stale).unwrap();
    let r = client::call(buyer.clone(), &stale, json!({"op": "predict", "request": req}), Duration::from_secs(20)).await.unwrap();
    assert_eq!(r["error"]["code"], json!("E_SEQ_LATE"));

    // Another peer that copied the session file cannot drive the session.
    let thief = node::spawn_node(&net(&tmp.path().join("t/id"), vec![saddr, waddr])).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let r = client::call(thief, &sf, json!({"op": "reset"}), Duration::from_secs(5)).await;
    assert!(r.is_err(), "worker ignores calls from a peer that did not open the session");

    let r = client::call(buyer.clone(), &sf, json!({"op": "close_session"}), Duration::from_secs(20)).await.unwrap();
    assert_eq!(r["ok"], json!(true));
    let r = client::call(buyer.clone(), &sf, json!({"op": "reset"}), Duration::from_secs(5)).await;
    assert!(r.is_err(), "closed session no longer answers");
}
