//! Job inputs bigger than one network message, on loopback libp2p.
//!
//! A chunk fetch answers with one message; the fetch codec caps it at 10 MiB
//! and chunk bytes travel as a CBOR integer array (about 2x), so a single
//! chunk over ~5 MiB never arrives. Buyers now store inputs as
//! [`pieces::INPUT_PIECE_BYTES`] pieces and workers fetch and verify each
//! ([`pieces::InputRef`]), while a single-hash spec from an older buyer still
//! works.

use std::sync::Arc;
use std::time::Duration;

use c0mpute_core::{ocr, pieces};
use c0mpute_net::{ChunkSource, Libp2pNetwork, Network, NetworkConfig};
use c0mpute_proto::{ChunkRequest, Hash};
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

/// A worker and a buyer that bootstraps off it.
async fn pair(tmp: &std::path::Path) -> (Arc<Libp2pNetwork>, Arc<Libp2pNetwork>, ChunkStore) {
    let (worker, _ws) = node(&tmp.join("w"), vec![]).await;
    let worker_addr = wait_for_listen(&worker)
        .await
        .with(Protocol::P2p(worker.peer_id()));
    let (buyer, buyer_store) = node(&tmp.join("b"), vec![worker_addr]).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    (worker, buyer, buyer_store)
}

/// A PNG signature followed by pseudo-random bytes (incompressible, like a
/// real scan).
fn big_png(len: usize) -> Vec<u8> {
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// The bug this guards against: what buyers did before (one chunk) cannot
/// carry a 12 MiB input; what they do now (pieces) can, and the OCR worker's
/// fetch returns exactly the bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_12_mib_ocr_input_reaches_the_worker() {
    let tmp = tempdir().unwrap();
    let (worker, buyer, buyer_store) = pair(tmp.path()).await;
    let input = big_png(12 * 1024 * 1024);

    // Before: one chunk. The fetch fails on the codec's response cap.
    let whole = buyer_store.put(&input).await.unwrap();
    buyer.announce(&whole).await.unwrap();
    let single = tokio::time::timeout(
        Duration::from_secs(60),
        worker.fetch_from_peer(
            buyer.peer_id(),
            &ChunkRequest {
                chunk_hash: whole,
                shard_index: None,
            },
        ),
    )
    .await;
    assert!(
        !matches!(single, Ok(Ok(_))),
        "a 12 MiB single-chunk fetch unexpectedly worked; the codec cap changed"
    );

    // After: pieces, named in the spec the way `/v1/ocr` and `c0mpute ocr`
    // build it.
    let stored =
        pieces::put_bytes_pieces(&buyer, &buyer_store, &input, pieces::INPUT_PIECE_BYTES)
            .await
            .unwrap();
    assert_eq!(stored.hash, Hash::of(&input).to_hex());
    assert_eq!(stored.pieces.len(), 12);
    let mut spec = serde_json::json!({
        "v": 1,
        "input_provider_peer_id": buyer.peer_id().to_base58(),
        "format": "png",
        "lang": "eng",
    });
    spec.as_object_mut()
        .unwrap()
        .extend(stored.spec_fields("input"));
    let spec = ocr::parse_spec(&spec).expect("pieced spec parses");
    let got = ocr::fetch_input(&worker, &spec)
        .await
        .expect("fetch the pieced input");
    assert_eq!(got.len(), input.len());
    assert!(got == input, "reassembled input differs");

    // A piece list that does not add up to the whole hash is refused.
    let mut forged = spec.clone();
    forged.input_hash = Hash::of(b"something else").to_hex();
    assert!(ocr::fetch_input(&worker, &forged).await.is_err());
}

/// Older buyers send only `input_hash` for a single chunk; that still works,
/// and a one-piece input from a new buyer is byte-identical to it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn single_chunk_specs_still_work() {
    let tmp = tempdir().unwrap();
    let (worker, buyer, buyer_store) = pair(tmp.path()).await;
    let input = big_png(300 * 1024);
    let hash = buyer_store.put(&input).await.unwrap();
    buyer.announce(&hash).await.unwrap();
    let legacy = ocr::parse_spec(&serde_json::json!({
        "v": 1,
        "input_hash": hash.to_hex(),
        "input_provider_peer_id": buyer.peer_id().to_base58(),
        "format": "png",
    }))
    .unwrap();
    assert!(ocr::fetch_input(&worker, &legacy).await.unwrap() == input);

    let stored =
        pieces::put_bytes_pieces(&buyer, &buyer_store, &input, pieces::INPUT_PIECE_BYTES)
            .await
            .unwrap();
    let fields = stored.spec_fields("input");
    assert_eq!(fields.len(), 1, "one piece: no size or piece list");
    assert_eq!(fields["input_hash"], hash.to_hex());

    // A 3 MiB input goes out in pieces, but a worker that predates pieces
    // reads only `input_hash`: the buyer keeps a whole copy for it.
    let mid = big_png(3 * 1024 * 1024);
    let stored = pieces::put_bytes_pieces(&buyer, &buyer_store, &mid, pieces::INPUT_PIECE_BYTES)
        .await
        .unwrap();
    assert_eq!(stored.pieces.len(), 3);
    let old_worker_view = ocr::parse_spec(&serde_json::json!({
        "v": 1,
        "input_hash": stored.hash,
        "input_provider_peer_id": buyer.peer_id().to_base58(),
        "format": "png",
    }))
    .unwrap();
    assert!(ocr::fetch_input(&worker, &old_worker_view).await.unwrap() == mid);
}
