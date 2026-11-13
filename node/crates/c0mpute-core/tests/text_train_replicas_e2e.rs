//! End-to-end: replicated verification of `infernet.text.train` (`--replicas`)
//! on loopback libp2p. Three workers run the same job; one returns an adapter
//! from nowhere. The fake `infernet` (tests/fixtures/fake-infernet-text) writes
//! a real tiny LoRA safetensors file when FAKE_TEXT_VARIANT is set.
//!
//! Its own test binary on purpose: cargo runs test binaries one at a time, and
//! these workers would otherwise answer (via mDNS) auctions of the other text
//! tests running in parallel in the same process.

use std::path::PathBuf;
use std::time::Duration;

use c0mpute_core::temporal::node::{NetOptions, spawn_node_with_store};
use c0mpute_core::text::{self, TrainRequest};
use c0mpute_core::train::{LoraOptions, normalize_dataset};
use c0mpute_core::train_verify as text_verify;
use c0mpute_store::ChunkStore;
use serde_json::{Value, json};
use tempfile::tempdir;

fn fake_infernet() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-infernet-text")
}

fn python3() -> bool {
    std::process::Command::new("python3").arg("--version").output().is_ok_and(|o| o.status.success())
}

fn net(dir: &std::path::Path, bootstrap: Vec<String>) -> NetOptions {
    NetOptions { identity_dir: dir.to_path_buf(), listen: vec!["/ip4/127.0.0.1/tcp/0".into()], bootstrap, default_bootstrap: false }
}

/// A wrapper `infernet` that runs the fake with one replica variant.
fn variant_infernet(dir: &std::path::Path, variant: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).unwrap();
    let p = dir.join(format!("infernet-{variant}"));
    std::fs::write(&p, format!("#!/bin/sh\nFAKE_TEXT_VARIANT={variant} exec python3 {} \"$@\"\n", fake_infernet().display())).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

/// `--replicas 3`: the same job on three workers, one of which returns an
/// adapter from nowhere. The buyer keeps an honest one, drops the cheat and
/// benches its worker.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replicated_training_drops_the_worker_that_disagrees() {
    // The buyer serves its files from loopback here; a worker only fetches a
    // job URL on a private address when its operator allows it.
    // SAFETY: set before any node starts; the test owns its process env.
    unsafe {
        std::env::set_var("C0MPUTE_ALLOW_PRIVATE_JOB_URLS", "1");
    }
    if !python3() {
        eprintln!("skipping: python3 is not installed (the fake infernet is a python script)");
        return;
    }
    let _ = tracing_subscriber::fmt().with_env_filter("c0mpute_core=info,c0mpute_net=warn").with_test_writer().try_init();
    let tmp = tempdir().unwrap();
    let bin = tmp.path().join("bin");
    let mut workers = Vec::new();
    let mut addrs = Vec::new();
    for (i, variant) in ["honest-a", "cheat", "honest-b"].iter().enumerate() {
        let w = text::serve(&net(&tmp.path().join(format!("w{i}/id")), addrs.clone()), variant_infernet(&bin, variant), tmp.path().join(format!("w{i}/cache")))
            .await
            .expect("worker serves");
        let id = w.net.peer_id().to_base58();
        addrs.push(format!("{}/p2p/{id}", w.net.listen_addrs().await.unwrap()[0]));
        workers.push((variant.to_string(), id, w));
    }

    let (corpus, report) = normalize_dataset(b"{\"text\":\"one\"}\n{\"text\":\"two\"}\n{\"text\":\"three\"}\n").unwrap();
    let job = LoraOptions { base_model: "HuggingFaceTB/SmolLM2-135M-Instruct".into(), name: "r3".into(), ..Default::default() }.job(&report);
    let stage = tmp.path().join("stage");
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::write(stage.join("corpus.jsonl"), &corpus).unwrap();
    std::fs::write(stage.join("job.json"), serde_json::to_vec(&job).unwrap()).unwrap();
    let store = ChunkStore::open(tmp.path().join("b/store")).await.unwrap();
    let buyer = spawn_node_with_store(&net(&tmp.path().join("b/id"), addrs.clone()), store.clone()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;

    let penalty_file = tmp.path().join("train-penalties.json");
    let req = TrainRequest {
        job_path: stage.join("job.json"),
        dataset_path: Some(stage.join("corpus.jsonl")),
        output: tmp.path().join("adapter"),
        workers: workers.iter().map(|w| w.1.clone()).collect(),
        max_price_usd: 1.0,
        gpu: false,
        min_ram_gb: None,
        legacy_http: false,
        public_url: None,
        bind: "127.0.0.1:0".parse().unwrap(),
        timeout_s: 120,
        retries: 0,
        bid_window: Duration::from_secs(2),
        replicas: 3,
        replica_tolerance: text_verify::DEFAULT_TOLERANCE,
        penalty_file: Some(penalty_file.clone()),
    };
    let mut events: Vec<Value> = Vec::new();
    let out = text::train(buyer.clone(), &store, &req, |ev| events.push(ev.clone())).await.expect("a majority agrees");

    let r = &out["replicas"];
    assert_eq!(r["requested"], 3, "{out}");
    assert_eq!(r["returned"], 3, "{out}");
    let used: std::collections::HashSet<&str> = r["workers"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(used.len(), 3, "three distinct workers: {out}");
    let cheat = &workers.iter().find(|w| w.0 == "cheat").unwrap().1;
    assert_eq!(r["outliers"], json!([cheat]), "{out}");
    assert_ne!(out["receipt"]["worker_peer_id"].as_str().unwrap(), cheat.as_str());
    assert_eq!(r["agreeing"].as_array().unwrap().len(), 2);

    // The kept adapter is one of the honest ones, in the output directory.
    let kept = text_verify::Replica::from_dir("kept", &tmp.path().join("adapter"), None).unwrap();
    assert_eq!(kept.steps, Some(3));
    assert!(!tmp.path().join("adapter/.replicas").exists(), "scratch removed");

    // The cheat is benched for this buyer's next train jobs; nobody else is.
    let pb = c0mpute_core::penalty_box::PenaltyBox::persistent(penalty_file);
    assert_eq!(pb.benched(), vec![cheat.clone()]);

    // Every replica's progress arrived, tagged with its index.
    let replicas_seen: std::collections::HashSet<u64> = events.iter().filter_map(|e| e["replica"].as_u64()).collect();
    assert_eq!(replicas_seen.len(), 3, "{events:?}");
    assert_eq!(events.first().map(|e| e["type"].clone()), Some(json!("replicas")));
    assert!(text::render(&events[0], &mut None).unwrap().contains("$3 total"), "{:?}", text::render(&events[0], &mut None));
}
