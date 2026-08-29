//! Integration tests for the storage HTTP API (CIP-002 acceptance criteria).
//!
//! Drives the real axum router in-process via `oneshot`, so these exercise
//! routing, headers, status codes and streaming — not just the storage engine
//! underneath.

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use c0mpute_gateway::auth::{AllowAll, SignedEnvelope, sign_envelope};
use c0mpute_gateway::storage_api::{self, Limits, StorageApiState};
use c0mpute_proto::Hash;
use c0mpute_store::{ChunkStore, Storage, Tier};
use ed25519_dalek::SigningKey;
use tower::ServiceExt;

const DID: &str = "did:coinpay:test";

fn tempdir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "c0mpute-api-test-{tag}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn storage_at(dir: &std::path::Path) -> Storage {
    Storage::new(ChunkStore::open(dir).await.unwrap())
}

async fn app_with(tag: &str, limits: Limits) -> (Router, Storage, std::path::PathBuf) {
    let dir = tempdir(tag);
    let storage = storage_at(&dir).await;
    let state = StorageApiState::new(storage.clone(), Arc::new(AllowAll), limits);
    (storage_api::router(state), storage, dir)
}

async fn app(tag: &str) -> (Router, Storage, std::path::PathBuf) {
    app_with(tag, Limits::default()).await
}

fn varied(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut state: u64 = 0x1234_5678_9abc_def0;
    for _ in 0..len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.push((state & 0xff) as u8);
    }
    out
}

fn put_req(hash: &Hash, body: Vec<u8>) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(format!("/storage/v1/objects/{}", hash.to_hex()))
        .header(header::CONTENT_LENGTH, body.len())
        .body(Body::from(body))
        .unwrap()
}

async fn body_bytes(resp: axum::response::Response) -> Vec<u8> {
    to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec()
}

// ------------------------------------------------------------------ round trip

#[tokio::test]
async fn put_then_get_round_trips() {
    let (app, _s, _d) = app("roundtrip").await;
    let data = varied(100_000);
    let hash = Hash::of(&data);

    let resp = app
        .clone()
        .oneshot(put_req(&hash, data.clone()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app
        .oneshot(
            Request::get(format!("/storage/v1/objects/{}", hash.to_hex()))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_bytes(resp).await, data);
}

#[tokio::test]
async fn accepts_blake3_prefixed_hashes() {
    let (app, _s, _d) = app("prefix").await;
    let data = varied(2048);
    let hash = Hash::of(&data);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/storage/v1/objects/blake3:{}", hash.to_hex()))
                .header(header::CONTENT_LENGTH, data.len())
                .body(Body::from(data.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn put_is_idempotent() {
    let (app, _s, _d) = app("idempotent").await;
    let data = varied(5_000);
    let hash = Hash::of(&data);

    let first = app
        .clone()
        .oneshot(put_req(&hash, data.clone()))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::CREATED);

    let second = app.oneshot(put_req(&hash, data)).await.unwrap();
    assert_eq!(second.status(), StatusCode::OK, "re-PUT should not rewrite");
}

// ------------------------------------------------------------ commit & verify

/// The property that makes the store trustworthy without trusting the
/// uploader: bytes must hash to the hash the client committed to in the URL.
#[tokio::test]
async fn wrong_committed_hash_is_422_and_stores_nothing() {
    let (app, storage, dir) = app("badhash").await;
    let data = varied(9_000);
    let lie = Hash::of(b"a completely different object");

    let resp = app.oneshot(put_req(&lie, data.clone())).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

    assert!(!storage.has(&lie).await);
    assert!(!storage.has(&Hash::of(&data)).await);
    let mut shards = 0;
    for e in walkdir(&dir.join("shards")) {
        if e.is_file() {
            shards += 1;
        }
    }
    assert_eq!(shards, 0, "rejected write left {shards} shards behind");
}

/// Regression: a rejected PUT must not damage an object that already holds
/// the same bytes.
///
/// Shards are content-addressed, so re-uploading an existing object's content
/// under a wrong committed hash produces identical shard hashes. Rolling back
/// every hash the failed write touched deleted the good object's shards —
/// one malformed request causing real data loss. Found by driving the server
/// with curl; the unit tests missed it because nothing was stored first.
#[tokio::test]
async fn rejected_put_does_not_destroy_an_existing_object() {
    let (app, storage, _d) = app("rollback-safety").await;
    let data = varied(300_000);
    let hash = Hash::of(&data);

    let resp = app
        .clone()
        .oneshot(put_req(&hash, data.clone()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Same bytes, wrong committed hash.
    let lie = Hash::of(b"something else entirely");
    let resp = app
        .clone()
        .oneshot(put_req(&lie, data.clone()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // The good object must still be fully readable.
    assert!(storage.has(&hash).await);
    let resp = app
        .oneshot(
            Request::get(format!("/storage/v1/objects/{}", hash.to_hex()))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        body_bytes(resp).await,
        data,
        "a rejected write destroyed an intact object"
    );
}

#[tokio::test]
async fn shard_put_verifies_its_hash() {
    let (app, _s, _d) = app("shardverify").await;
    let bytes = varied(1024);
    let real = Hash::of(&bytes);
    let lie = Hash::of(b"not it");

    let bad = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/storage/v1/shards/{}", lie.to_hex()))
                .body(Body::from(bytes.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let good = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/storage/v1/shards/{}", real.to_hex()))
                .body(Body::from(bytes))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(good.status(), StatusCode::CREATED);

    let head = app
        .oneshot(
            Request::builder()
                .method("HEAD")
                .uri(format!("/storage/v1/shards/{}", real.to_hex()))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(head.status(), StatusCode::OK);
}

// ----------------------------------------------------------------- durability

#[tokio::test]
async fn survives_parity_budget_of_shard_loss() {
    let (app, storage, _d) = app("parity").await;
    let data = varied(200_000);
    let hash = Hash::of(&data);
    app.clone()
        .oneshot(put_req(&hash, data.clone()))
        .await
        .unwrap();

    let manifest = storage.read_manifest(&hash).await.unwrap();
    // Standard = RS 10/14; four losses are inside the parity budget.
    for shard in manifest.blocks[0].shards.iter().take(4) {
        storage.chunk_store().delete(&shard.hash).await.unwrap();
    }
    let resp = app
        .clone()
        .oneshot(
            Request::get(format!("/storage/v1/objects/{}", hash.to_hex()))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_bytes(resp).await, data);

    // A fifth loss is unrecoverable.
    storage
        .chunk_store()
        .delete(&manifest.blocks[0].shards[4].hash)
        .await
        .unwrap();
    let resp = app
        .oneshot(
            Request::get(format!("/storage/v1/objects/{}", hash.to_hex()))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // The body streams, so the failure surfaces as a truncated stream rather
    // than a status code — the status is already sent by then. What must not
    // happen is silently returning wrong bytes.
    let got = to_bytes(resp.into_body(), usize::MAX).await;
    match got {
        Err(_) => {}
        Ok(b) => assert_ne!(
            b.as_ref(),
            data.as_slice(),
            "returned corrupt data as success"
        ),
    }
}

// --------------------------------------------------------------------- ranges

#[tokio::test]
async fn range_requests_return_exact_bytes() {
    let (app, _s, _d) = app("range").await;
    let data = varied(3_000_000);
    let hash = Hash::of(&data);
    app.clone()
        .oneshot(put_req(&hash, data.clone()))
        .await
        .unwrap();

    for (spec, start, len) in [
        ("bytes=1000000-1004095", 1_000_000usize, 4096usize),
        ("bytes=0-0", 0, 1),
        ("bytes=2999000-", 2_999_000, 1000),
        ("bytes=-500", 2_999_500, 500),
    ] {
        let resp = app
            .clone()
            .oneshot(
                Request::get(format!("/storage/v1/objects/{}", hash.to_hex()))
                    .header(header::RANGE, spec)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT, "spec {spec}");
        let cr = resp
            .headers()
            .get(header::CONTENT_RANGE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(cr.ends_with("/3000000"), "content-range was {cr}");
        assert_eq!(
            body_bytes(resp).await,
            &data[start..start + len],
            "spec {spec}"
        );
    }
}

#[tokio::test]
async fn unsatisfiable_range_is_416() {
    let (app, _s, _d) = app("range416").await;
    let data = varied(1000);
    let hash = Hash::of(&data);
    app.clone().oneshot(put_req(&hash, data)).await.unwrap();

    let resp = app
        .oneshot(
            Request::get(format!("/storage/v1/objects/{}", hash.to_hex()))
                .header(header::RANGE, "bytes=5000-6000")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
}

// ----------------------------------------------------------------- metadata

#[tokio::test]
async fn head_reports_length_and_tier_without_a_body() {
    let (app, _s, _d) = app("head").await;
    let data = varied(4321);
    let hash = Hash::of(&data);
    app.clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/storage/v1/objects/{}", hash.to_hex()))
                .header(header::CONTENT_LENGTH, data.len())
                .header(storage_api::TIER_HEADER, "critical")
                .body(Body::from(data.clone()))
                .unwrap(),
        )
        .await
        .unwrap();

    let resp = app
        .oneshot(
            Request::builder()
                .method("HEAD")
                .uri(format!("/storage/v1/objects/{}", hash.to_hex()))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CONTENT_LENGTH).unwrap(),
        &data.len().to_string()
    );
    assert_eq!(resp.headers().get("x-c0mpute-tier").unwrap(), "critical");
    assert!(body_bytes(resp).await.is_empty());
}

#[tokio::test]
async fn manifest_endpoint_describes_the_layout() {
    let (app, _s, _d) = app("manifest").await;
    let data = varied(50_000);
    let hash = Hash::of(&data);
    app.clone().oneshot(put_req(&hash, data)).await.unwrap();

    let resp = app
        .oneshot(
            Request::get(format!("/storage/v1/manifests/{}", hash.to_hex()))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let m: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(m["version"], 2);
    assert_eq!(m["tier"], "standard");
    assert_eq!(m["k"], 10);
    assert_eq!(m["parity"], 4);
    assert_eq!(m["blocks"].as_array().unwrap().len(), 1);
    assert_eq!(m["blocks"][0]["shards"].as_array().unwrap().len(), 14);
}

#[tokio::test]
async fn tier_header_selects_redundancy() {
    for (tier, shards) in [("hot", 3), ("standard", 14), ("critical", 32)] {
        let (app, storage, _d) = app(&format!("tier-{tier}")).await;
        let data = varied(10_000);
        let hash = Hash::of(&data);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/storage/v1/objects/{}", hash.to_hex()))
                    .header(header::CONTENT_LENGTH, data.len())
                    .header(storage_api::TIER_HEADER, tier)
                    .body(Body::from(data))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let m = storage.read_manifest(&hash).await.unwrap();
        assert_eq!(m.tier, tier.parse::<Tier>().unwrap());
        assert_eq!(m.shard_count(), shards, "tier {tier}");
    }
}

#[tokio::test]
async fn unknown_tier_is_400() {
    let (app, _s, _d) = app("badtier").await;
    let data = varied(100);
    let hash = Hash::of(&data);
    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/storage/v1/objects/{}", hash.to_hex()))
                .header(header::CONTENT_LENGTH, data.len())
                .header(storage_api::TIER_HEADER, "glacier")
                .body(Body::from(data))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// -------------------------------------------------------------------- errors

#[tokio::test]
async fn missing_content_length_is_400() {
    let (app, _s, _d) = app("nolen").await;
    let data = varied(100);
    let hash = Hash::of(&data);
    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/storage/v1/objects/{}", hash.to_hex()))
                .body(Body::from(data))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn unknown_object_is_404() {
    let (app, _s, _d) = app("missing").await;
    let hash = Hash::of(b"never stored");
    for path in ["objects", "manifests"] {
        let resp = app
            .clone()
            .oneshot(
                Request::get(format!("/storage/v1/{path}/{}", hash.to_hex()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "path {path}");
    }
}

#[tokio::test]
async fn malformed_hash_is_400() {
    let (app, _s, _d) = app("badhex").await;
    let resp = app
        .oneshot(
            Request::get("/storage/v1/objects/not-a-hash")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn object_over_the_limit_is_413() {
    let (app, _s, _d) = app_with(
        "toolarge",
        Limits {
            max_object_bytes: 1024,
            disk_budget_bytes: None,
        },
    )
    .await;
    let data = varied(5000);
    let hash = Hash::of(&data);
    let resp = app.oneshot(put_req(&hash, data)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn exhausted_disk_budget_is_507() {
    let (app, _s, _d) = app_with(
        "nospace",
        Limits {
            max_object_bytes: u64::MAX,
            // 10 KiB of raw disk; a 50 KiB object at 1.4x needs 70 KiB.
            disk_budget_bytes: Some(10_240),
        },
    )
    .await;
    let data = varied(50_000);
    let hash = Hash::of(&data);
    let resp = app.oneshot(put_req(&hash, data)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::INSUFFICIENT_STORAGE);
}

#[tokio::test]
async fn delete_frees_the_object_and_its_budget() {
    let (app, storage, _d) = app("delete").await;
    let data = varied(20_000);
    let hash = Hash::of(&data);
    app.clone().oneshot(put_req(&hash, data)).await.unwrap();
    assert!(storage.has(&hash).await);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/storage/v1/objects/{}", hash.to_hex()))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert!(!storage.has(&hash).await);

    let resp = app
        .oneshot(
            Request::get("/storage/v1/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let s: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(s["used_bytes"], 0);
}

// ----------------------------------------------------------------------- auth

fn signed_app(tag: &str, sk: &SigningKey) -> (Router, Storage, std::path::PathBuf) {
    let dir = tempdir(tag);
    let storage = futures::executor::block_on(async { storage_at(&dir).await });
    let auth = SignedEnvelope::new().with_key(DID, sk.verifying_key());
    let state = StorageApiState::new(storage.clone(), Arc::new(auth), Limits::default());
    (storage_api::router(state), storage, dir)
}

#[tokio::test]
async fn writes_require_an_envelope_but_reads_do_not() {
    let sk = SigningKey::from_bytes(&[42u8; 32]);
    let (app, _s, _d) = signed_app("auth", &sk);
    let data = varied(3_000);
    let hash = Hash::of(&data);
    let path = format!("/storage/v1/objects/{}", hash.to_hex());

    // Unsigned write: refused.
    let resp = app
        .clone()
        .oneshot(put_req(&hash, data.clone()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Signed write: accepted.
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let env = sign_envelope(DID, &sk, "PUT", &path, ts, &hash.to_hex());
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(&path)
                .header(header::CONTENT_LENGTH, data.len())
                .header("x-coinpay-auth", env)
                .body(Body::from(data.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Read needs nothing: the hash is the capability.
    let resp = app
        .clone()
        .oneshot(Request::get(&path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_bytes(resp).await, data);

    // Unsigned delete: refused.
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(&path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// An envelope minted for one object must not authorize a write to another.
#[tokio::test]
async fn envelope_cannot_be_replayed_against_another_object() {
    let sk = SigningKey::from_bytes(&[42u8; 32]);
    let (app, _s, _d) = signed_app("replay", &sk);
    let a = varied(1_000);
    let b = varied(2_000);
    let (ha, hb) = (Hash::of(&a), Hash::of(&b));
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let env_for_a = sign_envelope(
        DID,
        &sk,
        "PUT",
        &format!("/storage/v1/objects/{}", ha.to_hex()),
        ts,
        &ha.to_hex(),
    );

    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/storage/v1/objects/{}", hb.to_hex()))
                .header(header::CONTENT_LENGTH, b.len())
                .header("x-coinpay-auth", env_for_a)
                .body(Body::from(b))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ------------------------------------------------------------------ streaming

/// Memory must be bounded by block size, not object size. The CIP's headline
/// figure is 1 GiB under 200 MB RSS; this runs a smaller object so the suite
/// stays fast, and asserts the same property.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn large_object_streams_without_buffering_it_all() {
    fn rss_bytes() -> u64 {
        let s = std::fs::read_to_string("/proc/self/statm").unwrap();
        let pages: u64 = s.split_whitespace().nth(1).unwrap().parse().unwrap();
        pages * 4096
    }

    let (app, _s, _d) = app("bigstream").await;
    let size = 64 * 1024 * 1024;
    let data = varied(size);
    let hash = Hash::of(&data);

    let before = rss_bytes();
    let resp = app
        .clone()
        .oneshot(put_req(&hash, data.clone()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Read it back a range at a time; a whole-object read would legitimately
    // allocate the whole object, which is what `read_stream` exists to avoid.
    let resp = app
        .oneshot(
            Request::get(format!("/storage/v1/objects/{}", hash.to_hex()))
                .header(header::RANGE, "bytes=60000000-60001023")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        body_bytes(resp).await,
        &data[60_000_000..60_001_024],
        "range read of a large object"
    );

    let growth = rss_bytes().saturating_sub(before);
    // Generous: the point is that it is not proportional to the 64 MiB object.
    assert!(
        growth < 48 * 1024 * 1024,
        "RSS grew {growth} bytes writing+reading a {size}-byte object; \
         streaming is not bounding memory"
    );
}

fn walkdir(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walkdir(&p));
        } else {
            out.push(p);
        }
    }
    out
}
