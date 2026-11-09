//! End-to-end private storage on loopback libp2p (DIP-0012 `private` tier).
//!
//! Alice seals a file and publishes it. Exactly three hosting nodes take
//! copies through the normal hosting replication. Then, from the operators'
//! side: no byte of plaintext or the file name exists anywhere in any node's
//! store; a gateway that holds nothing serves only ciphertext; a holder
//! passes a proof-of-storage challenge on the ciphertext. From the users'
//! side: Bob cannot read it, Alice shares it (a header-only new version),
//! Bob reads it through the gateway, Alice revokes him and he cannot read
//! the new version, and a capability link opens it with no identity at all.

use std::sync::Arc;
use std::time::Duration;

use c0mpute_core::hosting::{Hosting, Options, Timing};
use c0mpute_core::sealed_store::{self, BlobStore, GatewayStore, NetworkStore, ObjectRef, PutOptions};
use c0mpute_net::{ChunkSource, Libp2pNetwork, NetworkConfig};
use c0mpute_proto::Hash;
use c0mpute_sealed::StorageSecret;
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

async fn listen_addr(net: &Libp2pNetwork) -> libp2p::Multiaddr {
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

fn fast() -> Timing {
    Timing {
        announce_every: Duration::from_millis(700),
        records_every: Duration::from_millis(1500),
        repair_every: Duration::from_millis(500),
        holder_ttl: Duration::from_secs(4),
        fetch_timeout: Duration::from_secs(20),
    }
}

/// Every file under `dir`, recursively.
fn all_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            all_files(&p, out);
        } else {
            out.push(p);
        }
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn private_objects_replicate_as_ciphertext_and_only_key_holders_read_them() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("c0mpute_core=info,c0mpute_net=warn")
        .with_test_writer()
        .try_init();
    let tmp = tempdir().unwrap();

    let (hub_net, hub_store) = node(&tmp.path().join("h0"), vec![]).await;
    let hub_addr = listen_addr(&hub_net).await;
    let opts = |dir: &std::path::Path| Options {
        hosts: true,
        catalog_path: Some(dir.join("catalog.json")),
        timing: fast(),
        ..Default::default()
    };
    let mut hosts: Vec<Hosting> = vec![Hosting::start(hub_net.clone(), hub_store, opts(&tmp.path().join("h0"))).await.unwrap()];
    for i in 1..5 {
        let d = tmp.path().join(format!("h{i}"));
        let (n, s) = node(&d, vec![hub_addr.clone()]).await;
        hosts.push(Hosting::start(n, s, opts(&d)).await.unwrap());
    }
    let gd = tmp.path().join("gw");
    let (gn, gs) = node(&gd, vec![hub_addr.clone()]).await;
    let gateway = Hosting::start(gn, gs, Options { hosts: false, timing: fast(), ..Default::default() }).await.unwrap();
    let app = gateway.router();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    let gw_url = format!("http://{gw_addr}");

    // Alice's client node: seeds her objects, is not a copy.
    let pd = tmp.path().join("alice");
    let (pn, ps) = node(&pd, vec![hub_addr.clone()]).await;
    let alice_host = Hosting::start(pn, ps, Options { hosts: false, timing: fast(), ..Default::default() }).await.unwrap();
    let alice_net = NetworkStore::with_hosting(alice_host.clone(), &gw_url);
    tokio::time::sleep(Duration::from_secs(3)).await;

    let alice = StorageSecret::generate();
    let bob = StorageSecret::generate();
    const MARKER: &[u8] = b"PLAINTEXT-MARKER-7f3a";
    const FILE_NAME: &str = "medical-records-2026.pdf";
    let mut data = Vec::new();
    while data.len() < 3 * 1024 * 1024 + 123 {
        data.extend_from_slice(MARKER);
        data.extend_from_slice(b" some patient data that must stay private. ");
    }

    let put = sealed_store::put_bytes(
        &alice_net,
        &alice,
        &data,
        PutOptions {
            file_name: Some(FILE_NAME.into()),
            content_type: Some("application/pdf".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let site = put.site_id.clone();
    let copies = alice_net.wait_for_copies(Duration::from_secs(90), |_, _| {}).await;
    assert!(copies[&site] >= 3, "three network copies: {copies:?}");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let holders = hosts[0].holders(&site).await;
    assert_eq!(holders.len(), 3, "replicates to exactly the target: {holders:?}");

    // ── Operator view: nothing but ciphertext anywhere ────────────────
    let alice_pub = alice.public().0;
    let mut scanned = 0usize;
    let mut holder_dirs = 0usize;
    for (i, h) in hosts.iter().enumerate() {
        let dir = tmp.path().join(format!("h{i}"));
        let mut files = Vec::new();
        all_files(&dir, &mut files);
        if holders.iter().any(|p| p == h.peer_id()) {
            holder_dirs += 1;
        }
        for f in files {
            let bytes = std::fs::read(&f).unwrap();
            scanned += bytes.len();
            assert!(!contains(&bytes, MARKER), "plaintext on node h{i}: {}", f.display());
            assert!(!contains(&bytes, FILE_NAME.as_bytes()), "file name on node h{i}: {}", f.display());
            assert!(!contains(&bytes, b"application/pdf"), "content type on node h{i}: {}", f.display());
            assert!(!contains(&bytes, &alice_pub), "owner identity on node h{i}: {}", f.display());
        }
    }
    assert_eq!(holder_dirs, 3);
    assert!(scanned > 3 * 3 * 1024 * 1024, "the copies are really there ({scanned} bytes scanned)");

    // The gateway serves the object as opaque bytes, and the loader page
    // is static and locked down.
    let client = reqwest::Client::new();
    let served = client.get(format!("{gw_url}/s/{site}/o")).send().await.unwrap();
    assert_eq!(served.status(), 200);
    assert_eq!(served.headers()["content-type"], "application/octet-stream");
    let served = served.bytes().await.unwrap();
    assert!(!contains(&served, MARKER));
    assert_eq!(&served[..8], b"C0PRIV\x00\x01");
    let loader = client.get(format!("{gw_url}/p/{site}/")).send().await.unwrap();
    assert_eq!(loader.status(), 200);
    assert!(loader.headers()["content-security-policy"].to_str().unwrap().contains("default-src 'none'"));
    assert_eq!(loader.headers()["referrer-policy"], "no-referrer");
    let html = loader.text().await.unwrap();
    assert!(html.contains("c0mpute-private-v1 body") && !html.contains(&site), "static loader, nothing object-specific");

    // Proof-of-storage over the ciphertext.
    let rec = alice_host.record_for(&site).await.expect("record known");
    let challenger = libp2p::identity::Keypair::generate_ed25519();
    let receipt = alice_host.challenge_one(&challenger, &rec, &holders[0]).await.unwrap();
    assert!(receipt.ok && receipt.verify(), "holder answers a challenge on ciphertext");

    // ── User view ─────────────────────────────────────────────────────
    let via_gw = GatewayStore::new(&gw_url);
    let r = ObjectRef::parse(&sealed_store::plain_ref(&site)).unwrap();
    let got = sealed_store::get(&via_gw, &r, Some(&alice)).await.unwrap();
    assert_eq!(&*got.opened.content, &data);
    assert_eq!(got.opened.meta.file_name.as_deref(), Some(FILE_NAME));
    assert!(sealed_store::get(&via_gw, &r, Some(&bob)).await.is_err(), "bob has no slot");

    // Share with Bob: a new version that changes only the header.
    sealed_store::share(&alice_net, &alice, &site, &[bob.public()]).await.unwrap();
    let mut bob_read = None;
    for _ in 0..120 {
        if let Ok(f) = sealed_store::get(&via_gw, &r, Some(&bob)).await {
            bob_read = Some(f);
            break;
        }
        alice_net.wait_for_copies(Duration::from_secs(1), |_, _| {}).await;
    }
    let bob_read = bob_read.expect("bob reads the shared version through the gateway");
    assert_eq!(&*bob_read.opened.content, &data);

    // Revoke Bob: re-sealed under a new key, published as a new version.
    sealed_store::revoke(&alice_net, &alice, &site, &[bob.public()]).await.unwrap();
    let old_key = ObjectRef { site_id: site.clone(), key: Some(bob_read.dek.duplicate()), path: None };
    let mut locked_out = false;
    for _ in 0..120 {
        let alice_ok = sealed_store::get(&via_gw, &r, Some(&alice)).await.is_ok();
        let bob_fails = sealed_store::get(&via_gw, &r, Some(&bob)).await.is_err() && sealed_store::get(&via_gw, &old_key, None).await.is_err();
        if alice_ok && bob_fails {
            locked_out = true;
            break;
        }
        alice_net.wait_for_copies(Duration::from_secs(1), |_, _| {}).await;
    }
    assert!(locked_out, "after revocation bob's slot and his old key open nothing current");

    // A capability link (key in the fragment) needs no identity.
    let current = sealed_store::get(&via_gw, &r, Some(&alice)).await.unwrap();
    let link = sealed_store::browser_link(&gw_url, &site, &current.dek);
    let by_link = sealed_store::get(&via_gw, &ObjectRef::parse(&link).unwrap(), None).await.unwrap();
    assert_eq!(&*by_link.opened.content, &data);

    // Alice's client leaves; the network keeps serving her ciphertext.
    alice_host.stop();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let blob = via_gw.get(&site).await.unwrap();
    assert!(!contains(&blob, MARKER));
    assert_eq!(&*c0mpute_sealed::open_with_secret(&blob, &alice, Some(&site)).unwrap().0.content, &data);
}
