//! Worker results bigger than one network message, on loopback libp2p.
//!
//! A chunk fetch answers with one message; the fetch codec caps it at 10 MiB
//! and chunk bytes travel as a CBOR integer array (about 2x), so a result
//! stored as a single chunk over ~5 MiB never reaches the buyer. Workers now
//! publish results over [`pieces::RESULT_SINGLE_CHUNK_MAX`] as 1 MiB pieces
//! plus a manifest ([`pieces::publish_result_bytes`] /
//! [`pieces::publish_result_file`]) and buyers follow it
//! ([`gateway_util::fetch_result`] / [`gateway_util::fetch_result_to_file`]).
//! Small results stay one chunk, and an older worker's single-chunk result
//! still fetches.

use std::sync::Arc;
use std::time::{Duration, Instant};

use c0mpute_core::gateway_util::{self, FetchError};
use c0mpute_core::pieces;
use c0mpute_net::{ChunkSource, Libp2pNetwork, Network, NetworkConfig};
use c0mpute_proto::Hash;
use c0mpute_store::ChunkStore;
use libp2p::multiaddr::Protocol;
use tempfile::tempdir;

const MIB: usize = 1024 * 1024;

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

/// A worker (with its store) and a buyer that bootstraps off it.
async fn pair(tmp: &std::path::Path) -> (Arc<Libp2pNetwork>, ChunkStore, Arc<Libp2pNetwork>) {
    let (worker, worker_store) = node(&tmp.join("w"), vec![]).await;
    let worker_addr = wait_for_listen(&worker)
        .await
        .with(Protocol::P2p(worker.peer_id()));
    let (buyer, _) = node(&tmp.join("b"), vec![worker_addr]).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    (worker, worker_store, buyer)
}

/// `head` followed by pseudo-random (incompressible) bytes, `len` in all.
fn noise(head: &[u8], len: usize) -> Vec<u8> {
    let mut out = head.to_vec();
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15 ^ len as u64;
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.truncate(len);
    out
}

fn wav(len: usize) -> Vec<u8> {
    noise(b"RIFF\0\0\0\0WAVEfmt ", len)
}

/// The bug: a 12 MiB TTS WAV stored the way every runner stored results
/// (one chunk) cannot reach the buyer. Published the new way it does, byte
/// for byte, through the same fetch the speech and jobs gateways use.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_12_mib_result_reaches_the_buyer() {
    let tmp = tempdir().unwrap();
    let (worker, worker_store, buyer) = pair(tmp.path()).await;
    let provider = worker.peer_id().to_base58();
    let result = wav(12 * MIB);

    // Before: one chunk. The fetch fails on the codec's response cap.
    let whole = worker_store.put(&result).await.unwrap();
    worker.announce(&whole).await.unwrap();
    let single = gateway_util::fetch_stored_result(
        &buyer,
        &whole.to_hex(),
        &provider,
        20 * MIB,
        Duration::from_secs(10),
    )
    .await;
    assert!(
        single.is_err(),
        "a 12 MiB single-chunk result unexpectedly arrived; the codec cap changed"
    );

    // After: pieces plus a manifest; the receipt names the manifest.
    let receipt_hash = pieces::publish_stored_bytes(&worker, &worker_store, &result)
        .await
        .unwrap();
    assert_ne!(receipt_hash, Hash::of(&result).to_hex());
    let got = gateway_util::fetch_stored_result(
        &buyer,
        &receipt_hash,
        &provider,
        20 * MIB,
        Duration::from_secs(120),
    )
    .await
    .expect("fetch the pieced result");
    assert_eq!(got.len(), result.len());
    assert!(got == result, "reassembled result differs");

    // The buyer's cap still holds for a pieced result.
    let over = gateway_util::fetch_stored_result(
        &buyer,
        &receipt_hash,
        &provider,
        8 * MIB,
        Duration::from_secs(30),
    )
    .await;
    assert!(matches!(over, Err(FetchError::Invalid(_))), "{over:?}");

    // What a buyer that predates pieces sees: the manifest, which is not a
    // WAV, so it rejects the result (as it already did for anything over
    // ~5 MiB, which never arrived at all).
    let old_buyer = gateway_util::fetch_verified(
        &buyer,
        &receipt_hash,
        &provider,
        20 * MIB,
        Duration::from_secs(30),
    )
    .await
    .unwrap();
    assert!(old_buyer.starts_with(pieces::MANIFEST_MAGIC));
}

/// Small results are still one chunk under their own hash, readable by every
/// buyer; and a result from a worker that predates pieces (one chunk, any
/// size that ever worked) still fetches with the new buyer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn small_and_legacy_results_stay_single_chunk() {
    let tmp = tempdir().unwrap();
    let (worker, worker_store, buyer) = pair(tmp.path()).await;
    let provider = worker.peer_id().to_base58();

    // At the threshold: one chunk, receipt hash = content hash.
    let small = noise(b"{\"text\":\"", pieces::RESULT_SINGLE_CHUNK_MAX);
    let h = pieces::publish_stored_bytes(&worker, &worker_store, &small)
        .await
        .unwrap();
    assert_eq!(h, Hash::of(&small).to_hex());
    let old_buyer =
        gateway_util::fetch_verified(&buyer, &h, &provider, 10 * MIB, Duration::from_secs(20))
            .await
            .unwrap();
    assert!(old_buyer == small);
    let new_buyer =
        gateway_util::fetch_stored_result(&buyer, &h, &provider, 10 * MIB, Duration::from_secs(20))
            .await
            .unwrap();
    assert!(new_buyer == small);
    // A single chunk over the buyer's cap is refused.
    assert!(matches!(
        gateway_util::fetch_stored_result(&buyer, &h, &provider, 1024, Duration::from_secs(20)).await,
        Err(FetchError::Invalid(_))
    ));

    // One byte over: pieced.
    let two = noise(b"{", pieces::RESULT_SINGLE_CHUNK_MAX + 1);
    let h = pieces::publish_stored_bytes(&worker, &worker_store, &two)
        .await
        .unwrap();
    assert_ne!(h, Hash::of(&two).to_hex());
    assert!(
        gateway_util::fetch_stored_result(&buyer, &h, &provider, 10 * MIB, Duration::from_secs(30))
            .await
            .unwrap()
            == two
    );

    // An older worker's 4 MiB result: one chunk, as runners used to store it.
    let legacy = wav(4 * MIB);
    let lh = worker_store.put(&legacy).await.unwrap();
    worker.announce(&lh).await.unwrap();
    let got = gateway_util::fetch_stored_result(
        &buyer,
        &lh.to_hex(),
        &provider,
        20 * MIB,
        Duration::from_secs(30),
    )
    .await
    .unwrap();
    assert!(got == legacy);
}

/// A manifest whose pieces do not add up to its whole hash is refused as an
/// invalid result, not retried as a timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forged_manifest_is_refused() {
    let tmp = tempdir().unwrap();
    let (worker, worker_store, buyer) = pair(tmp.path()).await;
    let provider = worker.peer_id().to_base58();
    let a = noise(b"a", pieces::RESULT_PIECE_BYTES);
    let b = noise(b"b", 10);
    let mut pieces_hex = vec![];
    for p in [&a, &b] {
        let h = worker_store.put(p).await.unwrap();
        worker.announce(&h).await.unwrap();
        pieces_hex.push(h.to_hex());
    }
    let manifest = pieces::ResultManifest {
        v: 1,
        size: (a.len() + b.len()) as u64,
        hash: Hash::of(b"something else").to_hex(),
        piece_bytes: pieces::RESULT_PIECE_BYTES as u64,
        pieces: pieces_hex,
    };
    let mh = worker_store.put(&manifest.encode()).await.unwrap();
    worker.announce(&mh).await.unwrap();
    let got = gateway_util::fetch_stored_result(
        &buyer,
        &mh.to_hex(),
        &provider,
        10 * MIB,
        Duration::from_secs(30),
    )
    .await;
    assert!(matches!(got, Err(FetchError::Invalid(_))), "{got:?}");
}

/// A transcode-sized output: published from disk (streamed into 1 MiB
/// pieces) and fetched straight to a file (a piece at a time).
async fn file_round_trip(len: usize) {
    let tmp = tempdir().unwrap();
    let (worker, worker_store, buyer) = pair(tmp.path()).await;
    let provider = worker.peer_id().to_base58();
    let src = tmp.path().join("output.mp4");
    let mut expected = blake3::Hasher::new();
    {
        use std::io::Write;
        let mut f = std::io::BufWriter::new(std::fs::File::create(&src).unwrap());
        let mut left = len;
        let mut i = 0u8;
        while left > 0 {
            let n = left.min(8 * MIB);
            let block = noise(&[i], n);
            expected.update(&block);
            f.write_all(&block).unwrap();
            left -= n;
            i = i.wrapping_add(1);
        }
    }
    // Over the worker's cap is refused before anything is stored.
    assert!(
        pieces::publish_stored_file(&worker, &worker_store, &src, len as u64 - 1)
            .await
            .is_err()
    );
    let started = Instant::now();
    let h = pieces::publish_stored_file(&worker, &worker_store, &src, len as u64)
        .await
        .unwrap();
    let stored = started.elapsed();
    let dest = tmp.path().join("got.mp4");
    let started = Instant::now();
    let written = gateway_util::fetch_stored_result_to_file(
        &buyer,
        &h,
        &provider,
        len,
        Duration::from_secs(30),
        &dest,
    )
    .await
    .unwrap();
    let fetched = started.elapsed();
    assert_eq!(written, len as u64);
    let mut got = blake3::Hasher::new();
    std::io::copy(&mut std::fs::File::open(&dest).unwrap(), &mut got).unwrap();
    assert_eq!(got.finalize(), expected.finalize());
    // No temp file left next to the output.
    let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains("part-"))
        .collect();
    assert!(leftovers.is_empty());
    eprintln!(
        "{} MiB result: stored in {stored:?}, fetched to disk in {fetched:?}",
        len / MIB
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_48_mib_output_streams_disk_to_disk() {
    file_round_trip(48 * MIB).await;
}

/// Multi-hundred-MiB transcode output. Slow in a debug build, so run on
/// demand: `cargo test --test large_result_e2e -- --ignored`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn a_300_mib_output_streams_disk_to_disk() {
    file_round_trip(300 * MIB).await;
}
