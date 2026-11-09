//! The video buyer path end to end on loopback, with a fake runner.
//!
//! `C0MPUTE_DOCKER` points the node's container CLI at a shell script that
//! behaves like `docker run <runner>`: it finds the job directory the node
//! bind-mounted at /job, checks `job.json` is there, and writes a real (tiny)
//! H.264 MP4 as `out.mp4`. Everything else is the shipping code: the worker's
//! video runner and bid filter, the auction, the store and announce, and the
//! buyer gateway's `POST /v1/videos`, which answers `video/mp4`.
//!
//! The same fake stands in for `docker pull/tag/build` to prove
//! `c0mpute video setup` builds from source when the registry will not serve
//! the pinned image.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use c0mpute_core::pricing::PricingConfig;
use c0mpute_core::{dispatch, penalty_box::PenaltyBox, speech_gateway, video};
use c0mpute_net::{ChunkSource, Libp2pNetwork, NetworkConfig};
use c0mpute_proto::Hash;
use c0mpute_store::ChunkStore;
use libp2p::multiaddr::Protocol;
use tempfile::tempdir;

const TOKEN: &str = "video-e2e-token-0123456789abcdef";
const FAKE_DOCKER: &str = r#"#!/bin/sh
# Stand-in for the docker CLI (see video_e2e.rs).
echo "$*" >> "$FAKE_DIR/log"
case "$1" in
  run)
    jd=""
    for a in "$@"; do
      case "$a" in *:/job) jd="${a%:/job}" ;; esac
    done
    [ -n "$jd" ] && [ -f "$jd/job.json" ] || exit 3
    if grep -q BIGCLIP "$jd/job.json"; then cp "$FAKE_DIR/big.mp4" "$jd/out.mp4"; else cp "$FAKE_DIR/tiny.mp4" "$jd/out.mp4"; fi
    exit 0 ;;
  pull) [ -f "$FAKE_DIR/pull-ok" ] && exit 0; echo "denied" >&2; exit 1 ;;
  build) [ -f "$5" ] && [ -d "$6" ] || exit 4; exit 0 ;;
  *) exit 0 ;;
esac
"#;

struct StoreSource(ChunkStore);

#[async_trait::async_trait]
impl ChunkSource for StoreSource {
    async fn read_chunk(&self, hash: &Hash) -> anyhow::Result<Vec<u8>> {
        self.0.get(hash).await
    }
}

fn tiny_mp4() -> Vec<u8> {
    std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny.mp4")).unwrap()
}

/// The tiny clip plus a top-level `free` box: still a valid MP4, `len` bytes.
fn big_mp4(len: usize) -> Vec<u8> {
    let mut v = tiny_mp4();
    let pad = len - v.len();
    v.extend_from_slice(&(pad as u32).to_be_bytes());
    v.extend_from_slice(b"free");
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    while v.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.extend_from_slice(&x.to_le_bytes());
    }
    v.truncate(len);
    v
}

/// Install the fake docker once per test binary.
fn fake_docker() -> &'static PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = tempdir().unwrap().keep();
        std::fs::write(dir.join("tiny.mp4"), tiny_mp4()).unwrap();
        std::fs::write(dir.join("big.mp4"), big_mp4(6 * 1024 * 1024 + 123)).unwrap();
        let bin = dir.join("docker");
        std::fs::write(&bin, FAKE_DOCKER).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // SAFETY: set once, before any thread of this binary reads them.
        unsafe {
            std::env::set_var(c0mpute_core::container::ENV, &bin);
            std::env::set_var("FAKE_DIR", &dir);
        }
        dir
    })
}

async fn node(dir: &Path, bootstrap: Vec<libp2p::Multiaddr>) -> (Arc<Libp2pNetwork>, ChunkStore) {
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

/// A video worker (the real runner over the fake docker), a buyer node, and
/// the buyer gateway on a loopback port. Returns the gateway URL.
async fn network(tmp: &Path) -> (String, Arc<Libp2pNetwork>) {
    fake_docker();
    let (worker, worker_store) = node(&tmp.join("w"), vec![]).await;
    let worker_addr = wait_for_listen(&worker).await.with(Protocol::P2p(worker.peer_id()));
    let load = c0mpute_core::WorkerLoad::new(Arc::new(tokio::sync::Semaphore::new(1)), 1);
    let runner = video::spawn_video_runner(
        worker.clone(),
        worker_store,
        tmp.join("cache"),
        video::WORKLOAD_TXT2VID.to_string(),
        video::RUNNER_IMAGE.to_string(),
        load.clone(),
    );
    dispatch::run_worker_subscriber_with_load(
        worker.clone(),
        video::WORKLOAD_TXT2VID.to_string(),
        vec![video::ROLE_TAG.to_string(), video::model_tag("wan2.2-ti2v-5b"), "c0mpute:gpu:nvidia".to_string()],
        Some(runner),
        None,
        Some(PricingConfig::default()),
        Some(load),
    );

    let (buyer, buyer_store) = node(&tmp.join("b"), vec![worker_addr]).await;
    let app = speech_gateway::router(buyer, buyer_store, TOKEN.into(), 100.0, PenaltyBox::new());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::time::sleep(Duration::from_secs(2)).await;
    (url, worker)
}

async fn post_video(url: &str, prompt: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{url}/v1/videos"))
        .bearer_auth(TOKEN)
        .json(&serde_json::json!({
            "model": "wan2.2-ti2v-5b",
            "prompt": prompt,
            "frames": 9,
            "steps": 2,
            "width": 64,
            "height": 64,
        }))
        .timeout(Duration::from_secs(180))
        .send()
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn post_v1_videos_returns_the_runner_clip() {
    let _ = tracing_subscriber::fmt().with_env_filter("c0mpute_core=info,c0mpute_net=warn").with_test_writer().try_init();
    let tmp = tempdir().unwrap();
    let (url, worker) = network(tmp.path()).await;

    // No token, no job.
    let anon = reqwest::Client::new().post(format!("{url}/v1/videos")).json(&serde_json::json!({"model": "m", "prompt": "p"})).send().await.unwrap();
    assert_eq!(anon.status(), 401);

    let res = post_video(&url, "a neon alley in the rain").await;
    assert_eq!(res.status(), 200, "{:?}", res.text().await);
    assert_eq!(res.headers()["content-type"], "video/mp4");
    assert_eq!(res.headers()["x-c0mpute-worker-peer-id"], worker.peer_id().to_base58().as_str());
    let body = res.bytes().await.unwrap();
    assert!(body.as_ref() == tiny_mp4().as_slice(), "the gateway returns exactly the runner's out.mp4");

    // The node ran the runner with the GPU, the job dir and the weights cache.
    let log = std::fs::read_to_string(fake_docker().join("log")).unwrap();
    let run = log.lines().find(|l| l.starts_with("run ")).expect("docker run was called");
    assert!(run.contains("--gpus all") && run.contains(":/job") && run.contains(":/weights"), "{run}");
    assert!(run.contains(video::RUNNER_IMAGE) && run.contains("txt2vid"), "{run}");

    // An invalid job is refused before any auction.
    let bad = reqwest::Client::new()
        .post(format!("{url}/v1/videos"))
        .bearer_auth(TOKEN)
        .json(&serde_json::json!({"model": "wan2.2-ti2v-5b", "prompt": "x", "frames": 9999}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
}

/// A clip over one network message (6 MiB): the worker publishes it as
/// pieces and the gateway fetches and reassembles them (fix/large-results).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clip_over_5_mib_reaches_the_buyer() {
    let tmp = tempdir().unwrap();
    let (url, _) = network(tmp.path()).await;
    let res = post_video(&url, "BIGCLIP a long take").await;
    assert_eq!(res.status(), 200, "{:?}", res.text().await);
    let body = res.bytes().await.unwrap();
    let want = std::fs::read(fake_docker().join("big.mp4")).unwrap();
    assert_eq!(body.len(), want.len());
    assert!(body.as_ref() == want.as_slice());
}

/// `c0mpute video setup`: the pinned pull is tried first and tagged as the
/// local name; when the registry refuses it, the runner is built from a
/// clone of the source repo instead.
#[tokio::test]
async fn setup_pulls_the_pinned_digest_or_builds_from_source() {
    let dir = fake_docker().clone();
    let git_ok = std::process::Command::new("git").arg("--version").output().is_ok_and(|o| o.status.success());
    if !git_ok {
        eprintln!("skipping: git is not installed");
        return;
    }
    // A local "source repo" laid out like this one.
    let src = tempdir().unwrap();
    std::fs::create_dir_all(src.path().join("plugins/video/runner")).unwrap();
    std::fs::write(src.path().join("plugins/video/runner/Dockerfile"), "FROM scratch\n").unwrap();
    std::fs::write(src.path().join("plugins/video/module.toml"), "[module]\n").unwrap();
    let git = |args: &[&str]| {
        let ok = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid", "-c", "init.defaultBranch=master"])
            .args(args)
            .current_dir(src.path())
            .output()
            .unwrap();
        assert!(ok.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&ok.stderr));
    };
    git(&["init", "-q"]);
    git(&["add", "."]);
    git(&["commit", "-qm", "runner"]);
    // SAFETY: only this test reads it.
    unsafe { std::env::set_var("C0MPUTE_VIDEO_SOURCE_REPO", format!("file://{}", src.path().display())) };

    let scratch = tempdir().unwrap();
    let said = std::sync::Mutex::new(Vec::<String>::new());
    let say = |l: &str| said.lock().unwrap().push(l.to_string());

    // The registry refuses the pull: build from source.
    let _ = std::fs::remove_file(dir.join("pull-ok"));
    let got = video::setup(false, scratch.path(), &say).await.expect("setup falls back to a build");
    assert_eq!(got, video::SetupSource::Built);
    let log = std::fs::read_to_string(dir.join("log")).unwrap();
    assert!(log.contains(&format!("pull {}", video::RUNNER_PULL_REF)), "{log}");
    let build = log.lines().find(|l| l.starts_with("build ")).expect("docker build ran");
    assert!(build.contains(&format!("-t {}", video::RUNNER_IMAGE)) && build.contains("plugins/video/runner/Dockerfile"), "{build}");
    assert!(said.lock().unwrap().iter().any(|l| l.contains("building it from source")));
    assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 0, "the scratch checkout is removed");

    // The pull works: tag the pinned digest as the local name, no build.
    std::fs::write(dir.join("pull-ok"), "").unwrap();
    let got = video::setup(false, scratch.path(), &say).await.unwrap();
    assert_eq!(got, video::SetupSource::Pulled);
    let log = std::fs::read_to_string(dir.join("log")).unwrap();
    assert!(log.contains(&format!("tag {} {}", video::RUNNER_PULL_REF, video::RUNNER_IMAGE)), "{log}");
    let _ = std::fs::remove_file(dir.join("pull-ok"));
}
