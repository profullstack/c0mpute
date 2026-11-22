//! Objects sealed by the JavaScript library (`packages/sealed`,
//! `@profullstack/c0mpute-sealed`) must open with the Rust code that backs
//! `c0mpute storage get`, carry a hosting record a gateway accepts, and stay
//! manageable (share, revoke, rm) by their owner.
//!
//! Needs `node` and the package's dependencies (`npm install` in a copy of
//! packages/sealed named by `C0MPUTE_SEALED_JS_DIR`, or the monorepo's
//! `bun install`). Skipped when they are
//! missing unless `C0MPUTE_JS_INTEROP=1` (the sealed-js workflow sets it).

use std::path::{Path, PathBuf};
use std::process::Command;

use c0mpute_core::hosting::sealed_ingest::sealed_shape_ok;
use c0mpute_core::hosting::site::SiteRecord;
use c0mpute_core::sealed_store::{self, BlobStore, DirStore, ObjectRef};
use c0mpute_proto::Hash;
use c0mpute_sealed::StorageSecret;

/// The JS package: `C0MPUTE_SEALED_JS_DIR` (a copy with its dependencies
/// installed, as CI does), else packages/sealed in this tree.
fn package_dir() -> PathBuf {
    match std::env::var_os("C0MPUTE_SEALED_JS_DIR") {
        Some(d) => PathBuf::from(d),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../packages/sealed"),
    }
}

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| ((i * 7 + (i >> 8)) & 255) as u8).collect()
}

fn ready() -> bool {
    let required = std::env::var("C0MPUTE_JS_INTEROP").is_ok_and(|v| v == "1");
    let have_node = Command::new("node").arg("--version").output().is_ok_and(|o| o.status.success());
    let have_deps = Command::new("node")
        .args(["--input-type=module", "-e", "await import('@noble/curves/ed25519.js')"])
        .current_dir(package_dir())
        .output()
        .is_ok_and(|o| o.status.success());
    if have_node && have_deps {
        return true;
    }
    assert!(!required, "C0MPUTE_JS_INTEROP=1 but node or packages/sealed/node_modules is missing");
    eprintln!("skipping: node or packages/sealed/node_modules missing (npm ci in packages/sealed)");
    false
}

/// Run the JS fixture maker and load what it wrote into a DirStore.
fn js_upload(owner: &StorageSecret, size: usize, part: Option<usize>, dir: &Path) -> (DirStore, serde_json::Value) {
    let out = dir.join("js");
    let mut cmd = Command::new("node");
    cmd.arg(package_dir().join("test/make-fixture.mjs"))
        .arg(owner.public().to_share_string())
        .arg(&out)
        .arg(size.to_string());
    if let Some(p) = part {
        cmd.arg(p.to_string());
    }
    let run = cmd.output().expect("run node");
    assert!(run.status.success(), "make-fixture failed: {}", String::from_utf8_lossy(&run.stderr));
    let plan: serde_json::Value = serde_json::from_slice(&std::fs::read(out.join("plan.json")).unwrap()).unwrap();
    let blobs = dir.join("blobs");
    std::fs::create_dir_all(&blobs).unwrap();
    for id in plan["site_ids"].as_array().unwrap() {
        let id = id.as_str().unwrap();
        let blob = std::fs::read(out.join(format!("{id}.blob"))).unwrap();
        let record: SiteRecord = serde_json::from_slice(&std::fs::read(out.join(format!("{id}.record.json"))).unwrap()).unwrap();
        // What the ingest gateway checks before publishing.
        assert!(record.verify(), "record signature");
        assert_eq!(record.site_id, id);
        assert_eq!(record.size, blob.len() as u64);
        assert!(sealed_shape_ok(&blob));
        let built = sealed_store::built_site(&record.name, &blob);
        assert_eq!(Hash::of(&built.manifest.to_bytes()).to_hex(), record.manifest, "manifest the record signs");
        std::fs::write(blobs.join(id), &blob).unwrap();
    }
    (DirStore(blobs), plan)
}

#[tokio::test]
async fn js_sealed_file_opens_and_is_manageable_in_rust() {
    if !ready() {
        return;
    }
    let d = tempfile::tempdir().unwrap();
    let alice = StorageSecret::generate();
    let bob = StorageSecret::generate();
    let (store, plan) = js_upload(&alice, 300_000, None, d.path());
    let site_id = plan["site_id"].as_str().unwrap().to_string();
    let r = ObjectRef::parse(plan["ref"].as_str().unwrap()).unwrap();

    // `c0mpute storage get c0p1:...` with the owner's identity.
    let got = sealed_store::get(&store, &r, Some(&alice)).await.unwrap();
    assert_eq!(&*got.opened.content, &pattern(300_000));
    assert_eq!(got.opened.meta.file_name.as_deref(), Some("fixture.bin"));
    // The #k= capability the app got.
    let cap = ObjectRef::parse(&format!("c0p1:{site_id}#k={}", plan["key"].as_str().unwrap())).unwrap();
    assert_eq!(&*sealed_store::get(&store, &cap, None).await.unwrap().opened.content, &pattern(300_000));
    // Nobody else's key opens it.
    assert!(sealed_store::get(&store, &r, Some(&bob)).await.is_err());

    // The owner shares, revokes and deletes it (custody of the app's key).
    sealed_store::share(&store, &alice, &site_id, &[bob.public()]).await.unwrap();
    assert_eq!(&*sealed_store::get(&store, &r, Some(&bob)).await.unwrap().opened.content, &pattern(300_000));
    let (_, re) = sealed_store::revoke(&store, &alice, &site_id, &[bob.public()]).await.unwrap();
    assert_eq!(re.site_id, site_id);
    assert!(sealed_store::get(&store, &r, Some(&bob)).await.is_err());
    assert!(sealed_store::get(&store, &cap, None).await.is_err(), "old link dead after revoke");
    sealed_store::delete(&store, &alice, &site_id).await.unwrap();
    assert!(sealed_store::get(&store, &r, Some(&alice)).await.is_err());
}

#[tokio::test]
async fn js_sealed_multipart_reassembles_in_rust() {
    if !ready() {
        return;
    }
    let d = tempfile::tempdir().unwrap();
    let alice = StorageSecret::generate();
    let (store, plan) = js_upload(&alice, 250_000, Some(100_000), d.path());
    assert_eq!(plan["site_ids"].as_array().unwrap().len(), 4);
    let r = ObjectRef::parse(plan["ref"].as_str().unwrap()).unwrap();
    let got = sealed_store::get(&store, &r, Some(&alice)).await.unwrap();
    assert_eq!(got.opened.meta.kind, c0mpute_sealed::Kind::Multipart);
    assert_eq!(&*got.opened.content, &pattern(250_000));
    let gone = sealed_store::delete(&store, &alice, plan["site_id"].as_str().unwrap()).await.unwrap();
    assert_eq!(gone.len(), 4);
    let first = plan["site_ids"][0].as_str().unwrap();
    assert!(c0mpute_sealed::open(&store.get(first).await.unwrap(), &got.dek, None).is_err());
}
