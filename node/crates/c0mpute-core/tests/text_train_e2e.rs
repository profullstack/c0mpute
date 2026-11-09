//! End-to-end: `infernet.text.train` peer to peer on loopback libp2p.
//!
//! A worker (`text::serve`) and a buyer (`text::train`) with no HTTP between
//! them: the buyer stores a multi-MiB corpus (with multi-turn conversations)
//! as pieces, the worker pulls them over libp2p, publishes training events on
//! the job topic, stores the adapter as pieces behind a manifest its receipt
//! names, and the buyer fetches and verifies it. A fake `infernet`
//! (tests/fixtures/fake-infernet-text, no PyTorch) stands in for the peft
//! runner; it writes a deterministic adapter derived from the corpus it was
//! handed, so every hash check is real. Skips where python3 is missing.

use std::path::PathBuf;
use std::time::Duration;

use c0mpute_core::temporal::node::{NetOptions, spawn_node_with_store};
use c0mpute_core::text::{self, TrainRequest};
use c0mpute_core::train::{LoraOptions, normalize_dataset};
use c0mpute_store::ChunkStore;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
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

/// About 3 MiB of records: text, prompt/completion and multi-turn chats.
fn dataset() -> Vec<u8> {
    let mut out = String::new();
    let mut i = 0u64;
    while out.len() < 3 * 1024 * 1024 {
        let filler: String = (0..40).map(|k| format!("w{}", (i * 31 + k) % 997)).collect::<Vec<_>>().join(" ");
        let rec = match i % 3 {
            0 => json!({"title": format!("Topic {i}"), "text": filler}),
            1 => json!({"prompt": format!("Q{i}?"), "completion": filler}),
            _ => json!({"messages": [
                {"role": "system", "content": "You are a pricing coach."},
                {"role": "user", "content": format!("I sell product {i}.")},
                {"role": "assistant", "content": "What does it replace?"},
                {"role": "user", "content": "Spreadsheets."},
                {"role": "assistant", "content": filler},
            ]}),
        };
        out.push_str(&rec.to_string());
        out.push('\n');
        i += 1;
    }
    out.into_bytes()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trains_peer_to_peer_with_pieces_and_conversations() {
    if !python3() {
        eprintln!("skipping: python3 is not installed (the fake infernet is a python script)");
        return;
    }
    let _ = tracing_subscriber::fmt().with_env_filter("c0mpute_core=info,c0mpute_net=warn").with_test_writer().try_init();
    let tmp = tempdir().unwrap();

    // ── worker ─────────────────────────────────────────────────────────
    let w = text::serve(&net(&tmp.path().join("w/id"), vec![]), fake_infernet(), tmp.path().join("w/cache")).await.expect("worker serves");
    assert!(w.tags.contains(&"infernet:text:train".to_string()));
    assert!(w.tags.contains(&text::MESSAGES_FORMAT_TAG.to_string()), "the runner reports the messages format: {:?}", w.tags);
    assert!(w.infer, "the fake answers chats (model generate)");
    assert!(w.tags.contains(&"infernet:text:infer".to_string()));
    let wid = w.net.peer_id().to_base58();
    let waddr = format!("{}/p2p/{wid}", w.net.listen_addrs().await.unwrap()[0]);

    // ── buyer: what `c0mpute train lora` does ──────────────────────────
    let raw = dataset();
    let (corpus, report) = normalize_dataset(&raw).unwrap();
    assert!(report.messages > 0 && report.text > 0 && report.prompt_completion > 0);
    assert!(corpus.len() > 2 * 1024 * 1024, "several pieces");
    let opts = LoraOptions { base_model: "HuggingFaceTB/SmolLM2-135M-Instruct".into(), name: "coach".into(), ..Default::default() };
    let job = opts.job(&report);
    let stage = tmp.path().join("stage");
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::write(stage.join("corpus.jsonl"), &corpus).unwrap();
    std::fs::write(stage.join("job.json"), serde_json::to_vec(&job).unwrap()).unwrap();

    let store = ChunkStore::open(tmp.path().join("b/store")).await.unwrap();
    let buyer = spawn_node_with_store(&net(&tmp.path().join("b/id"), vec![waddr]), store.clone()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;

    let req = TrainRequest {
        job_path: stage.join("job.json"),
        dataset_path: Some(stage.join("corpus.jsonl")),
        output: tmp.path().join("adapter"),
        workers: vec![],
        max_price_usd: 1.0,
        gpu: false,
        min_ram_gb: None,
        legacy_http: false,
        public_url: None,
        bind: "127.0.0.1:0".parse().unwrap(),
        timeout_s: 120,
        retries: 1,
        bid_window: Duration::from_secs(2),
    };
    let mut events: Vec<Value> = Vec::new();
    let out = text::train(buyer.clone(), &store, &req, |ev| events.push(ev.clone())).await.expect("trains");

    // Peer to peer both ways.
    assert_eq!(out["transport"], "pieces", "{out}");
    assert_eq!(out["receipt"]["worker_peer_id"], wid);
    let manifest_hash = out["receipt"]["output_hash"].as_str().unwrap();
    assert!(!manifest_hash.starts_with("sha256:"), "the receipt names the adapter manifest");

    // The adapter is exactly what the trainer derived from the corpus the
    // buyer sent: the corpus arrived intact.
    let seed = Sha256::digest(&corpus);
    let want: Vec<u8> = seed.iter().copied().cycle().take(2_621_440).collect();
    let got = std::fs::read(tmp.path().join("adapter/adapter_model.safetensors")).unwrap();
    assert_eq!(got.len(), want.len());
    assert!(got == want, "adapter bytes differ");
    assert_eq!(out["adapter_hash"], format!("sha256:{}", hex::encode(Sha256::digest(&want))));
    let m: Value = serde_json::from_slice(&std::fs::read(tmp.path().join("adapter/manifest.json")).unwrap()).unwrap();
    assert_eq!(m["dataset_sha256"], hex::encode(seed));
    assert_eq!(m["system_prompt"], "You are a pricing coach.");
    // Every conversation reached the trainer whole: five turns per chat,
    // two per prompt/completion pair.
    assert_eq!(m["conversation_turns"].as_u64().unwrap() as usize, report.messages * 5 + report.prompt_completion * 2);
    assert!(std::fs::read(tmp.path().join("adapter/adapter_config.json")).is_ok());

    // Progress came over gossip.
    let kinds: Vec<&str> = events.iter().filter_map(|e| e["type"].as_str()).collect();
    assert!(kinds.contains(&"meta"), "{kinds:?}");
    assert!(kinds.contains(&"checkpoint"), "{kinds:?}");
    assert_eq!(kinds.last(), Some(&"done"), "{kinds:?}");

    // The worker cleaned its job dir (corpus and adapter now live as pieces).
    let left = std::fs::read_dir(tmp.path().join("w/cache/text/jobs")).map(|d| d.count()).unwrap_or(0);
    assert_eq!(left, 0, "worker job dir removed");

    // A tampered manifest hash is refused (nothing is written).
    let bogus = c0mpute_proto::Hash::of(b"not a manifest").to_hex();
    let err = text::fetch_adapter_pieces(&buyer, Some(w.net.peer_id()), &bogus, &tmp.path().join("bad")).await;
    assert!(err.is_err());
    assert!(!tmp.path().join("bad").exists());

    // ── then chat with the adapter, also peer to peer ──────────────────
    // (`c0mpute infernet text chat`: adapter out as pieces, answer back as a chunk)
    let chat = text::ChatRequest {
        adapter_dir: tmp.path().join("adapter"),
        question: "How should I price it?".into(),
        search: None,
        max_new_tokens: 64,
        workers: vec![],
        max_price_usd: 1.0,
        legacy_http: false,
        public_url: None,
        bind: "127.0.0.1:0".parse().unwrap(),
        timeout_s: 60,
        bid_window: Duration::from_secs(2),
    };
    let ans = text::chat(buyer.clone(), &store, &chat).await.expect("chat answered");
    assert_eq!(ans["text"], "[HuggingFaceTB/SmolLM2-135M-Instruct] system=You are a pricing coach. | user=How should I price it?", "{ans}");
    assert_eq!(ans["adapter_hash"], out["adapter_hash"]);
    assert!(!ans["receipt"]["output_hash"].as_str().unwrap().starts_with("sha256:"), "answer came back as a chunk");
    // The worker cached the adapter it fetched as pieces.
    let cached = tmp.path().join("w/cache/text/adapters").join(out["adapter_hash"].as_str().unwrap().trim_start_matches("sha256:"));
    assert_eq!(std::fs::read(cached.join("adapter_model.safetensors")).unwrap(), want);
}

/// A corpus with conversations is never placed on a worker whose runner
/// cannot train them (it would silently drop them).
#[test]
fn conversations_need_a_runner_that_reports_messages() {
    let chat = br#"{"messages":[{"role":"user","content":"a"},{"role":"assistant","content":"b"}]}"#;
    let mut corpus = chat.to_vec();
    corpus.push(b'\n');
    let tags = text::required_tags(&corpus);
    let old_runner = text::worker_tags(&json!({"installed": true, "cuda": false}), 8 * 1024);
    assert!(!tags.iter().all(|t| old_runner.contains(t)), "an old runner lacks {}", text::MESSAGES_FORMAT_TAG);
    let new_runner = text::worker_tags(&json!({"installed": true, "cuda": false, "formats": ["text", "messages"]}), 8 * 1024);
    assert!(tags.iter().all(|t| new_runner.contains(t)));
}
