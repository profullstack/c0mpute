//! App uploads end to end on loopback: an object sealed the way an app seals
//! it (fresh signing key, custody for the owner) is streamed to a gateway
//! with a ticket, published, served back, and opened by its owner.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use c0mpute_core::hosting::sealed_ingest::{self, Ticket};
use c0mpute_core::hosting::{Hosting, Options, Timing};
use c0mpute_core::sealed_store::{self, GatewayStore, ObjectRef};
use c0mpute_net::{ChunkSource, Libp2pNetwork, NetworkConfig};
use c0mpute_proto::Hash;
use c0mpute_sealed::{Kind, SealParams, StorageSecret};
use c0mpute_store::ChunkStore;
use libp2p::identity::Keypair;
use zeroize::Zeroizing;

struct StoreSource(ChunkStore);

#[async_trait::async_trait]
impl ChunkSource for StoreSource {
    async fn read_chunk(&self, hash: &Hash) -> anyhow::Result<Vec<u8>> {
        self.0.get(hash).await
    }
}

fn ticket(k: &Keypair, t: &Ticket) -> String {
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(t).unwrap());
    let sig = k.sign(format!("c0mpute-sealed-ticket-v1\n{payload}").as_bytes()).unwrap();
    format!("{payload}.{}", URL_SAFE_NO_PAD.encode(sig))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn app_upload_is_published_and_opens_for_its_owner() {
    unsafe { std::env::set_var("C0MPUTE_DISABLE_MDNS", "1") };
    let tmp = tempfile::tempdir().unwrap();
    let store = ChunkStore::open(tmp.path().join("store")).await.unwrap();
    let net = Libp2pNetwork::spawn(
        NetworkConfig::for_dir(tmp.path().join("id"))
            .with_listen(vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()])
            .with_local_source(Arc::new(StoreSource(store.clone()))),
    )
    .await
    .unwrap();
    let gateway = Hosting::start(
        Arc::new(net),
        store,
        Options {
            hosts: true,
            disk_override: Some((1 << 40, 2 << 40)),
            timing: Timing {
                announce_every: Duration::from_millis(700),
                records_every: Duration::from_millis(1500),
                repair_every: Duration::from_millis(500),
                holder_ttl: Duration::from_secs(4),
                fetch_timeout: Duration::from_secs(20),
            },
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let issuer = Keypair::generate_ed25519();
    let issuer_hex = hex::encode(issuer.public().try_into_ed25519().unwrap().to_bytes());
    assert!(sealed_ingest::configure(&issuer_hex, tmp.path().join("ingest")));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = gateway.router();
    tokio::spawn(async move { axum::serve(listener, app).await });
    let base = format!("http://{addr}");

    // Seal like the JS library: fresh key, custody wrapped to alice.
    let alice = StorageSecret::generate();
    let mut seed = Zeroizing::new([0u8; 32]);
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, seed.as_mut());
    let name = c0mpute_sealed::random_name();
    let content: Vec<u8> = (0..2_500_000u32).map(|i| (i % 249) as u8).collect();
    let stranger = StorageSecret::generate();
    let sealed = c0mpute_sealed::seal_custodial(
        &content,
        &SealParams {
            kind: Kind::File,
            name: &name,
            owner: &stranger,
            file_name: Some("rec.webm".into()),
            content_type: Some("video/webm".into()),
            files: Default::default(),
            parts: vec![],
            recipients: &[alice.public()],
            created: 1,
            version: 0,
        },
        &seed,
        Some(c0mpute_sealed::wrap_custody(&seed, &alice.public()).unwrap()),
        None,
    )
    .unwrap();
    let blob = sealed.blob;
    let site = sealed.site_id.clone();
    let built = sealed_store::built_site(&name, &blob);
    let record = sealed_store::sign_record_with_seed(seed.clone(), &name, &built, 1_000).unwrap();
    let url = format!("{base}/storage/v1/sealed/{site}");
    let http = reqwest::Client::new();
    let exp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + 600;
    let t = ticket(&issuer, &Ticket { v: 1, upload: "up_test000001".into(), sub: "x".into(), max_bytes: blob.len() as u64, max_objects: 1, exp });

    // No ticket, wrong issuer: refused.
    let r = http.put(format!("{url}?offset=0")).body(blob[..1000].to_vec()).send().await.unwrap();
    assert_eq!(r.status(), 401);
    let other = ticket(&Keypair::generate_ed25519(), &Ticket { v: 1, upload: "up_test000001".into(), sub: "x".into(), max_bytes: 1 << 30, max_objects: 1, exp });
    let r = http.put(format!("{url}?offset=0")).bearer_auth(&other).body(blob[..1000].to_vec()).send().await.unwrap();
    assert_eq!(r.status(), 401);
    // Plaintext is refused.
    let r = http.put(format!("{url}?offset=0")).bearer_auth(&t).body(b"not sealed at all".to_vec()).send().await.unwrap();
    assert_eq!(r.status(), 400);
    // CORS preflight.
    let r = http.request(reqwest::Method::OPTIONS, &url).send().await.unwrap();
    assert_eq!(r.headers()["access-control-allow-origin"], "*");

    // Upload in pieces; a wrong offset says where to resume.
    let mid = 1_200_000;
    let r = http.put(format!("{url}?offset=0")).bearer_auth(&t).body(blob[..mid].to_vec()).send().await.unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let r = http.put(format!("{url}?offset=0")).bearer_auth(&t).body(blob[..mid].to_vec()).send().await.unwrap();
    assert_eq!(r.status(), 409);
    assert_eq!(r.json::<serde_json::Value>().await.unwrap()["received"], mid as u64);
    // Finishing early is refused (size mismatch).
    let r = http.post(format!("{url}/finish")).bearer_auth(&t).json(&serde_json::json!({ "record": record })).send().await.unwrap();
    assert_eq!(r.status(), 400);
    let r = http.put(format!("{url}?offset={mid}")).bearer_auth(&t).body(blob[mid..].to_vec()).send().await.unwrap();
    assert_eq!(r.status(), 200);
    // More than the ticket allows: refused.
    let r = http.put(format!("{url}?offset={}", blob.len())).bearer_auth(&t).body(vec![1u8; 10]).send().await.unwrap();
    assert_eq!(r.status(), 413);

    // A record for another manifest is refused; the right one publishes.
    let wrong = sealed_store::sign_record_with_seed(seed.clone(), &name, &sealed_store::built_site(&name, &blob[..blob.len() - 1]), 999).unwrap();
    let r = http.post(format!("{url}/finish")).bearer_auth(&t).json(&serde_json::json!({ "record": wrong })).send().await.unwrap();
    assert_eq!(r.status(), 400);
    let r = http.post(format!("{url}/finish")).bearer_auth(&t).json(&serde_json::json!({ "record": record })).send().await.unwrap();
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["status"], "published", "{body}");
    let s: serde_json::Value = http.get(&url).send().await.unwrap().json().await.unwrap();
    assert_eq!(s["published"], true);

    // Served back as ciphertext; the owner opens it with `storage get`'s code.
    let gw = GatewayStore::new(&base);
    let got = sealed_store::get(&gw, &ObjectRef::parse(&format!("c0p1:{site}")).unwrap(), Some(&alice)).await.unwrap();
    assert_eq!(&*got.opened.content, &content);
    assert_eq!(got.opened.meta.file_name.as_deref(), Some("rec.webm"));
}
