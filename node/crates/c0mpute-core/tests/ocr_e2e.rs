//! End-to-end `ocr.extract` on loopback libp2p: the buyer stores a PDF in its
//! own chunk store and announces it, a jobs bridge auctions the job, the real
//! OCR runner wins, fetches the input by hash from the buyer, runs Tesseract,
//! and the bridge fetches and checks the result.
//!
//! Needs `tesseract` (with `eng`) and poppler-utils; where they are missing
//! the test says so and passes, like the other runtime tests.

use std::sync::Arc;
use std::time::Duration;

use c0mpute_core::jobs_gateway::JobsBridge;
use c0mpute_core::pricing::PricingConfig;
use c0mpute_core::{dispatch, ocr};
use c0mpute_net::{ChunkSource, Libp2pNetwork, Network, NetworkConfig};
use c0mpute_proto::Hash;
use c0mpute_store::ChunkStore;
use libp2p::multiaddr::Protocol;
use serde_json::json;
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

/// A one-page PDF with `line` set in 36pt Helvetica.
fn text_pdf(line: &str) -> Vec<u8> {
    padded_pdf(line, 0)
}

/// [`text_pdf`] with about `pad` bytes of comment lines after the header, so
/// the document is big without changing what it shows.
fn padded_pdf(line: &str, pad: usize) -> Vec<u8> {
    let content = format!("BT /F1 36 Tf 72 700 Td ({line}) Tj ET");
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>".to_string(),
        format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
    ];
    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut x: u32 = 0x1234_5678;
    while pdf.len() < pad {
        pdf.push(b'%');
        for _ in 0..1023 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            pdf.push(b'!' + (x % 90) as u8);
        }
        pdf.push(b'\n');
    }
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
    }
    let xref = pdf.len();
    pdf.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for off in offsets {
        pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    pdf
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pdf_ocr_round_trips_through_the_bridge() {
    let rt = ocr::runtime();
    if !rt.ready() || !rt.has_lang("eng") || !rt.pdf {
        eprintln!("skipping: tesseract with eng and poppler-utils are not installed");
        return;
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter("c0mpute_core=info,c0mpute_net=warn")
        .with_test_writer()
        .try_init();
    // Keep this loopback OCR job off the live LAN network.
    unsafe { std::env::set_var("C0MPUTE_DISABLE_MDNS", "1") };
    let tmp = tempdir().unwrap();

    // ── worker: the real OCR runner, with its bid filter ───────────────
    let (worker, worker_store) = node(&tmp.path().join("w"), vec![]).await;
    let worker_addr = wait_for_listen(&worker)
        .await
        .with(Protocol::P2p(worker.peer_id()));
    let load = c0mpute_core::WorkerLoad::new(Arc::new(tokio::sync::Semaphore::new(2)), 2);
    let runner = ocr::spawn_ocr_runner(
        worker.clone(),
        worker_store,
        tmp.path().join("cache"),
        load.clone(),
    );
    dispatch::run_worker_subscriber_with_load(
        worker.clone(),
        ocr::WORKLOAD.to_string(),
        vec![ocr::ROLE_TAG.to_string(), "c0mpute:cpu".to_string()],
        Some(runner),
        Some(ocr::offer_filter()),
        Some(PricingConfig { min_usd: 0.0, ..PricingConfig::default() }),
        Some(load),
    );

    // ── buyer: stores the document, then auctions the job ──────────────
    let (buyer, buyer_store) = node(&tmp.path().join("b"), vec![worker_addr]).await;
    let pdf = text_pdf("INVOICE 7731 PAID");
    let hash = buyer_store.put(&pdf).await.unwrap();
    buyer.announce(&hash).await.unwrap();
    let buyer_store_for_big = buyer_store.clone();
    let bridge = JobsBridge::new(
        buyer.clone(),
        buyer_store,
        "test-token-0123456789abcdef".into(),
        1.0,
        vec![],
    )
    .unwrap()
    .with_bid_window(Duration::from_secs(2));
    tokio::time::sleep(Duration::from_secs(2)).await;

    let spec = json!({
        "v": 1,
        "input_hash": hash.to_hex(),
        "input_provider_peer_id": buyer.peer_id().to_base58(),
        "format": "pdf",
        "lang": "eng",
    });
    let out = bridge
        .submit_once(ocr::WORKLOAD, spec, Duration::from_secs(120))
        .await
        .expect("ocr job");
    assert_eq!(out["worker"], worker.peer_id().to_base58());
    let result = &out["result"];
    assert!(ocr::valid_result(result), "{result}");
    assert_eq!(result["engine"], "tesseract");
    assert_eq!(result["pages"][0]["page"], 1);
    let text = result["text"].as_str().unwrap().to_uppercase();
    assert!(text.contains("INVOICE") && text.contains("7731"), "{text}");

    // A language the worker lacks: its filter keeps it from bidding at all.
    let missing = json!({
        "v": 1,
        "input_hash": hash.to_hex(),
        "input_provider_peer_id": buyer.peer_id().to_base58(),
        "format": "pdf",
        "lang": "zzz",
    });
    let err = bridge
        .submit_once(ocr::WORKLOAD, missing, Duration::from_secs(10))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no_bids") || err.contains("timeout"), "{err}");

    // A 12 MiB document: far over one network message, so it travels as
    // pieces (stored the way `/v1/ocr` and `c0mpute ocr` store it).
    let big = padded_pdf("LEDGER 5150 BIG", 12 * 1024 * 1024);
    assert!(big.len() > 12 * 1024 * 1024 && big.len() < ocr::MAX_INPUT_BYTES);
    let stored = c0mpute_core::pieces::put_bytes_pieces(
        &buyer,
        &buyer_store_for_big,
        &big,
        c0mpute_core::pieces::INPUT_PIECE_BYTES,
    )
    .await
    .unwrap();
    assert!(stored.pieces.len() > 1);
    let mut spec = json!({
        "v": 1,
        "input_provider_peer_id": buyer.peer_id().to_base58(),
        "format": "pdf",
        "lang": "eng",
    });
    spec.as_object_mut()
        .unwrap()
        .extend(stored.spec_fields("input"));
    let out = bridge
        .submit_once(ocr::WORKLOAD, spec, Duration::from_secs(180))
        .await
        .expect("12 MiB ocr job");
    let text = out["result"]["text"].as_str().unwrap().to_uppercase();
    assert!(text.contains("LEDGER") && text.contains("5150"), "{text}");
}
