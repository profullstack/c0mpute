//! End-to-end encrypted jobs (DIP-0032) on loopback libp2p: a buyer, two
//! workers and a passive node that subscribes to every job topic and records
//! everything gossip shows it.
//!
//! For every workload: the passive node sees no prompt, input or result; the
//! winner receives the exact spec; the loser cannot fetch the sealed spec;
//! the result chunk is ciphertext to anyone but the buyer; the buyer opens it.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use c0mpute_core::runner::AcceptedJob;
use c0mpute_core::{JobAuction, dispatch, gateway_util, job_e2e, pieces, run_auction};
use c0mpute_net::topics::{JobAccept, JobReceipt, JobStatus, job_topic};
use c0mpute_net::{ChunkSource, Libp2pNetwork, NetworkConfig, PeerId};
use c0mpute_proto::Hash;
use c0mpute_store::ChunkStore;
use libp2p::multiaddr::Protocol;
use serde_json::{Value, json};
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
    _dir: TempDir,
}

async fn node(bootstrap: Vec<libp2p::Multiaddr>) -> Node {
    let dir = tempdir().unwrap();
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
        _dir: dir,
    }
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

/// One spec per workload, every sensitive field marked `SECRET`.
fn specs() -> Vec<(&'static str, Value)> {
    vec![
        (
            "tts.synthesize",
            json!({"text": "SECRET tts line", "engine": "kokoro", "voice": "af_heart", "locale": "en-US", "speed": 1.0}),
        ),
        (
            "diffusion.txt2img",
            json!({"prompt": "SECRET castle", "model": "sd-turbo", "width": 512, "height": 512, "steps": 2}),
        ),
        (
            "music.txt2music",
            json!({"prompt": "SECRET song", "lyrics": "SECRET verse", "duration": 10}),
        ),
        (
            "video.txt2vid",
            json!({"model": "m", "prompt": "SECRET clip", "negative_prompt": "SECRET no", "frames": 8}),
        ),
        (
            "embed.text",
            json!({"v": 1, "model": "bge-small-en-v1.5", "texts": ["SECRET document one"]}),
        ),
        (
            "ocr.extract",
            json!({"v": 1, "input_hash": "SECRET", "format": "png", "lang": "eng"}),
        ),
        (
            "ffmpeg.transcode",
            json!({"input_url": "https://SECRET.example/a.mp4", "preset": "720p", "spec": {"codec": "h264", "bitrate_bps": 1, "width": 2, "height": 2, "keyframe_interval": 1, "extra_ffmpeg_args": ["drawtext=SECRET"]}}),
        ),
        (
            "blender.render-frame",
            json!({"v": 1, "input_hash": "SECRET", "input_size": 5, "input_pieces": ["SECRET"], "frame_start": 1, "frame_end": 1}),
        ),
        (
            "crawl.fetch",
            json!({"v": 1, "url": "https://SECRET.example/", "headers": {"cookie": "SECRET"}}),
        ),
        (
            "workflow.step",
            json!({"v": 1, "kind": "http_call", "config": {"url": "https://SECRET.example/"}, "input": {"note": "SECRET"}}),
        ),
        (
            "infernet.text.train",
            json!({"job": {"kind": "lora", "base_model": "org/m", "system_prompt": "SECRET"}, "dataset_hash": "SECRET"}),
        ),
        (
            "infernet.inference",
            json!({"v": 1, "model": "qwen2.5:0.5b", "messages": [{"role": "user", "content": "SECRET question"}]}),
        ),
        (
            "infernet.text.infer",
            json!({"adapter_hash": "SECRET", "base_model": "org/m", "messages": [{"role": "user", "content": "SECRET"}], "max_new_tokens": 8}),
        ),
        (
            "captcha.solve",
            json!({"v": 1, "kind": "text", "image": "SECRET"}),
        ),
    ]
}

const RESULT: &[u8] = b"{\"ok\":true,\"text\":\"RESULT-SECRET answer\"}";
const INPUT: &[u8] = b"INPUT-SECRET audio bytes";

/// A worker for `workload` whose runner records the spec it got, checks a
/// sealed input when the spec names one, seals `RESULT` back and publishes a
/// receipt.
fn serve(w: &Node, workload: &str, got: Arc<Mutex<Vec<(String, Value)>>>) {
    let (tx, mut rx) = mpsc::channel::<AcceptedJob>(8);
    dispatch::run_worker_subscriber(w.net.clone(), workload.to_string(), vec![], Some(tx));
    let (net, store) = (w.net.clone(), w.store.clone());
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let spec = job.accept.spec_inline.clone().unwrap();
            if let Some(h) = spec["audio_hash"].as_str() {
                let key = pieces::input_key(spec["audio_key"].as_str()).unwrap();
                let r = pieces::InputRef::parse("audio", h, None, None, 1 << 20).unwrap();
                let buyer: PeerId = job.accept.buyer_peer_id.parse().unwrap();
                let plain = r
                    .fetch_opened(&net, Some(buyer), 1 << 20, key.as_ref())
                    .await
                    .unwrap();
                assert_eq!(plain, INPUT, "the winner opens the sealed input");
            }
            got.lock().unwrap().push((job.accept.job_id.clone(), spec));
            // Long enough for the loser's attempt on the sealed spec.
            tokio::time::sleep(Duration::from_millis(800)).await;
            let hash = pieces::publish_result_bytes(&net, &store, &job.accept, RESULT)
                .await
                .unwrap();
            receipt(&net, &job.accept, hash).await;
        }
    });
}

async fn receipt(net: &Libp2pNetwork, accept: &JobAccept, hash: String) {
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
    };
    let topic = job_topic(&accept.workload_type);
    let bytes = serde_json::to_vec(&r).unwrap();
    let buyer: PeerId = accept.buyer_peer_id.parse().unwrap();
    let _ = net
        .send_direct(buyer, &topic, &bytes, Duration::from_secs(5))
        .await;
    let _ = net.publish(&topic, bytes).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gossip_shows_no_plaintext_and_only_the_winner_reads_the_job() {
    // SAFETY: set before any node starts; the test owns its process env.
    unsafe { std::env::set_var("C0MPUTE_DISABLE_MDNS", "1") };
    let _ = tracing_subscriber::fmt()
        .with_env_filter("c0mpute_core=info,c0mpute_net=warn")
        .with_test_writer()
        .try_init();

    let a = node(vec![]).await;
    let a_addr = addr(&a.net).await;
    let b = node(vec![a_addr.clone()]).await;
    let passive = node(vec![a_addr.clone()]).await;
    let buyer = node(vec![a_addr.clone(), addr(&b.net).await]).await;

    // The passive node joins every job topic and keeps every byte it sees.
    let seen: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
    let accepts: Arc<Mutex<Vec<JobAccept>>> = Arc::default();
    let got: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
    for (w, _) in specs() {
        passive.net.subscribe(&job_topic(w)).await.unwrap();
        serve(&a, w, got.clone());
        serve(&b, w, got.clone());
    }
    {
        let (seen, accepts) = (seen.clone(), accepts.clone());
        let mut rx = passive.net.messages();
        tokio::spawn(async move {
            while let Ok(m) = rx.recv().await {
                if let Ok(acc) = serde_json::from_slice::<JobAccept>(&m.data)
                    && !acc.winning_bidder_peer_id.is_empty()
                {
                    accepts.lock().unwrap().push(acc);
                }
                seen.lock().unwrap().push(m.data);
            }
        });
    }
    tokio::time::sleep(Duration::from_secs(2)).await;

    let workers = vec![a.net.peer_id().to_base58(), b.net.peer_id().to_base58()];
    let mut cases = specs();
    // stt carries its audio as a sealed input stored on the buyer.
    let (audio, audio_key) =
        pieces::put_bytes_pieces_sealed(&buyer.net, &buyer.store, INPUT, pieces::INPUT_PIECE_BYTES)
            .await
            .unwrap();
    let mut stt = json!({"audio_provider_peer_id": buyer.net.peer_id().to_base58(), "audio_format": "wav", "model": "base"});
    stt.as_object_mut()
        .unwrap()
        .extend(audio.sealed_spec_fields("audio", &audio_key));
    cases.push(("stt.transcribe", stt));
    serve(&a, "stt.transcribe", got.clone());
    serve(&b, "stt.transcribe", got.clone());
    passive
        .net
        .subscribe(&job_topic("stt.transcribe"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;

    for (workload, spec) in cases {
        // The loser tries to fetch the sealed spec as soon as the accept is
        // gossiped; the buyer serves it only to the winner.
        let loser_try = {
            let (accepts, a_net, b_net, buyer_peer) = (
                accepts.clone(),
                a.net.clone(),
                b.net.clone(),
                buyer.net.peer_id(),
            );
            let workload = workload.to_string();
            tokio::spawn(async move {
                for _ in 0..100 {
                    let acc = accepts
                        .lock()
                        .unwrap()
                        .iter()
                        .find(|x| x.workload_type == workload)
                        .cloned();
                    if let Some(acc) = acc {
                        let loser = if acc.winning_bidder_peer_id == a_net.peer_id().to_base58() {
                            b_net
                        } else {
                            a_net
                        };
                        return Some(
                            loser
                                .rpc(
                                    buyer_peer,
                                    job_e2e::PAYLOAD_SERVICE,
                                    acc.job_id.into_bytes(),
                                )
                                .await,
                        );
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                None
            })
        };
        let auction = JobAuction::new(workload, spec.clone())
            .with_allowed_bidders(workers.clone())
            .with_max_price_usd(0.01)
            .with_bid_window(Duration::from_secs(2))
            .with_receipt_timeout(Duration::from_secs(30));
        let outcome = run_auction(buyer.net.clone(), auction)
            .await
            .unwrap_or_else(|e| panic!("{workload}: {e:#}"));
        assert_eq!(outcome.receipt.status, JobStatus::Completed, "{workload}");

        // The winner got exactly the spec; nobody else got it.
        let jobs: Vec<_> = got
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, _)| *id == outcome.job_id)
            .cloned()
            .collect();
        assert_eq!(jobs.len(), 1, "{workload}: exactly one worker ran the job");
        assert_eq!(
            jobs[0].1, spec,
            "{workload}: the winner opened the exact spec"
        );

        // The loser was refused the sealed spec.
        let refused = loser_try.await.unwrap().expect("accept seen on gossip");
        assert!(
            refused.is_err(),
            "{workload}: the loser fetched the sealed spec"
        );

        // The buyer opens the result.
        let opened =
            gateway_util::fetch_result(&buyer.net, &outcome, 1 << 20, Duration::from_secs(20))
                .await
                .unwrap();
        assert_eq!(opened, RESULT, "{workload}");

        // Anyone else who fetches the result chunk gets ciphertext.
        let hash = outcome.receipt.output_hash.clone().unwrap();
        let winner = outcome.accepted_bid.bidder_peer_id.clone();
        let stolen = gateway_util::fetch_stored_result(
            &passive.net,
            &hash,
            &winner,
            1 << 20,
            Duration::from_secs(20),
        )
        .await
        .unwrap();
        assert!(
            !contains(&stolen, b"RESULT-SECRET"),
            "{workload}: result chunk is plaintext"
        );
        assert!(job_e2e::open_blob(&job_e2e::random_key(), &stolen).is_err());
    }

    // The sealed stt input is ciphertext to the passive node too.
    let stolen = gateway_util::fetch_stored_result(
        &passive.net,
        &audio.hash,
        &buyer.net.peer_id().to_base58(),
        1 << 20,
        Duration::from_secs(20),
    )
    .await
    .unwrap();
    assert!(!contains(&stolen, b"INPUT-SECRET"));

    // Everything gossip carried: offers, bids, accepts, receipts. No secret.
    let seen = seen.lock().unwrap();
    assert!(
        seen.len() >= 3 * 15,
        "the passive node saw the auctions ({} messages)",
        seen.len()
    );
    for m in seen.iter() {
        for needle in [
            &b"SECRET"[..],
            b"RESULT-SECRET",
            b"INPUT-SECRET",
            b"tts line",
            b"castle",
            b"question",
        ] {
            assert!(
                !contains(m, needle),
                "gossip leaked {:?}: {}",
                String::from_utf8_lossy(needle),
                String::from_utf8_lossy(m)
            );
        }
    }
    // And it did see what bidding needs.
    assert!(
        seen.iter().any(|m| contains(m, b"qwen2.5:0.5b")),
        "public spec carries the model"
    );
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// Live check (DIP-0032): join the real network as a passive node, subscribe
/// to the job topics, record every gossip message for
/// `C0MPUTE_CAPTURE_SECS` (default 120) into `C0MPUTE_CAPTURE_FILE`, and fail
/// if any contains `C0MPUTE_CAPTURE_NEEDLE`. Run it while a real job with
/// that needle in its prompt goes through:
/// `cargo test -p c0mpute-core --test e2e_jobs_e2e live_capture -- --ignored`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "joins the live network"]
async fn live_capture_shows_no_plaintext() {
    let needle = std::env::var("C0MPUTE_CAPTURE_NEEDLE").expect("C0MPUTE_CAPTURE_NEEDLE");
    let secs: u64 = std::env::var("C0MPUTE_CAPTURE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(120);
    let out = std::env::var("C0MPUTE_CAPTURE_FILE").unwrap_or_else(|_| "capture.jsonl".into());
    let dir = tempdir().unwrap();
    let net = Libp2pNetwork::spawn(
        NetworkConfig::for_dir(dir.path().join("id"))
            .with_listen(vec!["/ip4/0.0.0.0/tcp/0".parse().unwrap()])
            .with_bootstrap(c0mpute_net::bootstrap::default_seeds()),
    )
    .await
    .unwrap();
    let topics = [
        "infernet.inference",
        "diffusion.txt2img",
        "tts.synthesize",
        "stt.transcribe",
        "embed.text",
        "infernet.text.infer",
    ];
    for t in topics {
        net.subscribe(&job_topic(t)).await.unwrap();
    }
    let mut rx = net.messages();
    // Optionally buy a real job from a separate buyer node meanwhile:
    // C0MPUTE_CAPTURE_BUY=infernet.inference sends a chat with the needle in
    // the prompt (C0MPUTE_CAPTURE_MODEL picks the model).
    let buyer_task = std::env::var("C0MPUTE_CAPTURE_BUY").ok().map(|workload| {
        let needle = needle.clone();
        tokio::spawn(async move {
            let bdir = tempdir().unwrap();
            let buyer = Arc::new(
                Libp2pNetwork::spawn(
                    NetworkConfig::for_dir(bdir.path().join("id"))
                        .with_listen(vec!["/ip4/0.0.0.0/tcp/0".parse().unwrap()])
                        .with_bootstrap(c0mpute_net::bootstrap::default_seeds()),
                )
                .await
                .unwrap(),
            );
            tokio::time::sleep(Duration::from_secs(20)).await;
            let spec = match workload.as_str() {
                "embed.text" => json!({"v": 1, "texts": [format!("{needle} private document")]}),
                _ => {
                    let mut s = json!({"v": 1, "messages": [{"role": "user", "content": format!("{needle}: reply with the single word ok")}], "max_tokens": 16});
                    if let Ok(m) = std::env::var("C0MPUTE_CAPTURE_MODEL") {
                        s["model"] = json!(m);
                    }
                    s
                }
            };
            let caps = match workload.as_str() {
                "embed.text" => vec![c0mpute_core::embed::ROLE_TAG.to_string()],
                _ => vec![c0mpute_core::inference::ROLE_TAG.to_string()],
            };
            let auction = JobAuction::new(workload.clone(), spec)
                .with_required_capabilities(caps)
                .with_max_price_usd(0.05)
                .with_bid_window(Duration::from_secs(10))
                .with_receipt_timeout(Duration::from_secs(90));
            let o = run_auction(buyer.clone(), auction).await;
            match o {
                Ok(o) => {
                    let r = gateway_util::fetch_result(&buyer, &o, 1 << 20, Duration::from_secs(30)).await;
                    eprintln!(
                        "BUYER: job {} won by {} status {:?} output_hash {:?}; opened result: {}",
                        o.job_id,
                        o.accepted_bid.bidder_peer_id,
                        o.receipt.status,
                        o.receipt.output_hash,
                        r.map(|b| String::from_utf8_lossy(&b).chars().take(300).collect::<String>()).unwrap_or_else(|e| format!("{e:?}"))
                    );
                }
                Err(e) => eprintln!("BUYER: auction failed: {e:#}"),
            }
        })
    });
    let mut f = std::fs::File::create(&out).unwrap();
    let (mut n, mut leaks, mut offers) = (0usize, 0usize, 0usize);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let m = match tokio::time::timeout_at(deadline, rx.recv()).await {
            Err(_) => break,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(_)) => break,
            Ok(Ok(m)) => m,
        };
        n += 1;
        if contains(&m.data, needle.as_bytes()) {
            leaks += 1;
        }
        if let Ok(o) = serde_json::from_slice::<c0mpute_net::topics::JobOffer>(&m.data) {
            offers += 1;
            eprintln!(
                "offer {} {} e2e={} spec_inline={}",
                o.workload_type,
                o.job_id,
                o.e2e.is_some(),
                o.spec_inline.is_some()
            );
        }
        use std::io::Write;
        writeln!(
            f,
            "{}\t{}\t{}",
            m.topic,
            m.source.map(|p| p.to_base58()).unwrap_or_default(),
            String::from_utf8_lossy(&m.data)
        )
        .unwrap();
    }
    if let Some(t) = buyer_task {
        let _ = t.await;
    }
    eprintln!("captured {n} messages ({offers} offers) on {topics:?}; {leaks} contain the needle");
    assert_eq!(leaks, 0, "plaintext on gossip");
}
