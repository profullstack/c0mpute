//! A site above the single-chunk manifest limit (here 9,000 files) is
//! published with a parted manifest and served file by file: the node loads
//! only the parts the requests touch, `/dir/` resolves to its index, and a
//! missing path gets the site's 404 page.

use std::sync::Arc;
use std::time::Duration;

use c0mpute_core::hosting::site::{INLINE_FILES, Manifest, SiteRecord, build_from_dir};
use c0mpute_core::hosting::{Hosting, Options};
use c0mpute_net::{ChunkSource, Libp2pNetwork, NetworkConfig};
use c0mpute_proto::Hash;
use c0mpute_store::ChunkStore;
use tempfile::tempdir;

struct StoreSource(ChunkStore);

#[async_trait::async_trait]
impl ChunkSource for StoreSource {
    async fn read_chunk(&self, hash: &Hash) -> anyhow::Result<Vec<u8>> {
        self.0.get(hash).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_nine_thousand_file_site_publishes_and_serves() {
    // Loopback only: never publish test sites to a live network via mDNS.
    unsafe { std::env::set_var("C0MPUTE_DISABLE_MDNS", "1") };
    let tmp = tempdir().unwrap();
    let site = tmp.path().join("site");
    for i in 0..9_000 {
        let d = site.join(format!("d{}", i % 30));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(format!("p{i}.html")), format!("page {i}")).unwrap();
    }
    std::fs::create_dir_all(site.join("docs")).unwrap();
    std::fs::write(site.join("docs/index.html"), "docs home").unwrap();
    std::fs::write(site.join("404.html"), "custom 404").unwrap();

    let built = build_from_dir(&site, "big").unwrap();
    assert!(built.manifest.files.len() > INLINE_FILES);
    let stored = built.stored();
    assert!(
        !stored.parts.is_empty(),
        "a large site gets a parted manifest"
    );
    assert!(matches!(
        Manifest::from_bytes(&stored.root).unwrap(),
        Manifest::Parted(_)
    ));

    let store = ChunkStore::open(tmp.path().join("store")).await.unwrap();
    let net = Libp2pNetwork::spawn(
        NetworkConfig::for_dir(tmp.path().join("id"))
            .with_listen(vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()])
            .with_local_source(Arc::new(StoreSource(store.clone()))),
    )
    .await
    .unwrap();
    let h = Hosting::start(Arc::new(net), store, Options::default())
        .await
        .unwrap();
    let kp = libp2p::identity::Keypair::generate_ed25519();
    let rec = SiteRecord::sign(
        &kp,
        "big",
        &built.manifest_hash(),
        1,
        built.manifest.total_bytes(),
        vec![],
    )
    .unwrap();
    h.publish_site(&built, rec.clone()).await.unwrap();

    let (_, body) = h
        .read_file(&rec, "d7/p8197.html")
        .await
        .unwrap()
        .expect("file in a late part");
    assert_eq!(body, b"page 8197");
    let (_, body) = h
        .read_file(&rec, "d0/p0.html")
        .await
        .unwrap()
        .expect("file in the first part");
    assert_eq!(body, b"page 0");
    assert!(h.read_file(&rec, "d0/nope.html").await.unwrap().is_none());

    let body_of = |r: axum::response::Response| async move {
        let status = r.status().as_u16();
        let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        (status, String::from_utf8_lossy(&b).into_owned())
    };
    assert_eq!(
        body_of(h.serve(&rec, "/d3/p4503").await).await,
        (200, "page 4503".into()),
        "extensionless path finds .html"
    );
    assert_eq!(
        body_of(h.serve(&rec, "/docs/").await).await,
        (200, "docs home".into())
    );
    assert_eq!(
        body_of(h.serve(&rec, "/missing/thing").await).await,
        (404, "custom 404".into())
    );
    let _ = Duration::from_secs(0);
}
