//! End-to-end `container.run` on loopback libp2p with a real docker: a buyer
//! packs a tiny build context, a local `containers` worker builds it and
//! returns the image (which `docker load`s with the reported id), then builds
//! and runs a command with an input directory and brings an output back, then
//! reports a failing command's exit code and log. A worker that does not
//! trust the buyer never bids.
//!
//! Needs docker with buildx (`C0MPUTE_DOCKER` may name a wrapper such as
//! `sudo docker`); where it is missing the test says so and passes.

use std::sync::Arc;
use std::time::Duration;

use c0mpute_core::container_run::{self, BuildSpec, BuyerPolicy, ContainersConfig, Request};
use c0mpute_core::dispatch;
use c0mpute_core::pricing::PricingConfig;
use c0mpute_net::{ChunkSource, Libp2pNetwork, NetworkConfig};
use c0mpute_proto::Hash;
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

fn docker(args: &[&str]) -> String {
    let out = c0mpute_core::container::command_sync()
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "docker {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn builds_runs_and_returns_results() {
    // Run only against this test's loopback worker, not dev2 workers found by mDNS.
    unsafe { std::env::set_var("C0MPUTE_DISABLE_MDNS", "1") };
    let rt = tokio::task::spawn_blocking(|| container_run::runtime().clone())
        .await
        .unwrap();
    if !rt.ready() {
        eprintln!("skipping: {}", rt.reason.unwrap_or_default());
        return;
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter("c0mpute_core=info,c0mpute_net=warn")
        .with_test_writer()
        .try_init();
    let tmp = tempdir().unwrap();
    let cfg = ContainersConfig {
        work_dir: Some(tmp.path().join("work")),
        max_jobs: 2,
        min_free_gb: 1,
        egress_guard: false,
        ..Default::default()
    };
    std::fs::create_dir_all(cfg.work_dir()).unwrap();

    let (wnet, wstore) = node(&tmp.path().join("w"), vec![]).await;
    let waddr = wait_for_listen(&wnet)
        .await
        .with(Protocol::P2p(wnet.peer_id()));
    let load = container_run::worker_load(&cfg);
    let runner =
        container_run::spawn_container_runner(wnet.clone(), wstore, cfg.clone(), load.clone());
    dispatch::run_worker_subscriber_with_load(
        wnet.clone(),
        container_run::WORKLOAD.to_string(),
        vec![
            container_run::ROLE_TAG.to_string(),
            "c0mpute:cpu".to_string(),
        ],
        Some(runner),
        Some(container_run::offer_filter(cfg.clone())),
        Some(PricingConfig::default()),
        Some(load),
    );
    let (bnet, bstore) = node(&tmp.path().join("b"), vec![waddr]).await;
    let buyer_id = bnet.peer_id().to_base58();
    let worker_id = wnet.peer_id().to_base58();
    container_run::install_policy(BuyerPolicy {
        open: false,
        trusted: [buyer_id.clone()].into(),
    });
    tokio::time::sleep(Duration::from_secs(3)).await;
    let progress: container_run::Progress = Arc::new(|l: &str| eprintln!("  | {l}"));

    // A tiny context.
    let ctx = tmp.path().join("ctx");
    std::fs::create_dir_all(ctx.join("skip")).unwrap();
    std::fs::write(
        ctx.join("Dockerfile"),
        "FROM busybox:1.36\nARG MSG=unset\nRUN echo \"$MSG\" > /msg\nCOPY hello.txt /hello.txt\n",
    )
    .unwrap();
    std::fs::write(ctx.join("hello.txt"), "hello from the context\n").unwrap();
    std::fs::write(ctx.join("skip/big.bin"), vec![7u8; 1 << 20]).unwrap();
    std::fs::write(ctx.join(".dockerignore"), "skip\n").unwrap();
    let ctx_tar = tmp.path().join("ctx.tar.gz");
    container_run::pack_dir(&ctx, &ctx_tar, true).unwrap();
    assert!(
        std::fs::metadata(&ctx_tar).unwrap().len() < 64 * 1024,
        ".dockerignore'd file was packed"
    );

    let build = BuildSpec {
        dockerfile: "Dockerfile".into(),
        build_args: [("MSG".to_string(), "built remotely".to_string())].into(),
        target: None,
    };

    // 1. Build: the image comes back and loads with the reported id.
    let out = tmp.path().join("out1");
    let outcome = container_run::submit(
        bnet.clone(),
        bstore.clone(),
        Request {
            context: Some(ctx_tar.clone()),
            build: Some(build.clone()),
            builders: vec![worker_id.clone()],
            max_price_usd: 1.0,
            timeout_secs: Some(600),
            ..Default::default()
        },
        &out,
        progress.clone(),
    )
    .await
    .expect("build job");
    assert!(outcome.ok(), "build failed: {}", outcome.result);
    assert_eq!(outcome.worker, worker_id);
    let id = outcome.result["image"]["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("sha256:"), "{id}");
    let image = outcome.image.clone().expect("image file");
    let loaded = docker(&["load", "--input", &image.display().to_string()]);
    let name = loaded
        .lines()
        .find_map(|l| l.strip_prefix("Loaded image: "))
        .expect("loaded name")
        .to_string();
    assert_eq!(
        docker(&["image", "inspect", "--format", "{{.Id}}", &name]),
        id,
        "loaded id matches the reported id"
    );
    assert_eq!(
        docker(&["run", "--rm", &name, "cat", "/msg"]),
        "built remotely"
    );
    let _ = docker(&["image", "rm", "-f", &name]);

    // 2. Build, then run with an input dir; the output comes back.
    let input = tmp.path().join("in");
    std::fs::create_dir_all(&input).unwrap();
    std::fs::write(input.join("in.txt"), "input file\n").unwrap();
    let in_tar = tmp.path().join("in.tar.gz");
    container_run::pack_dir(&input, &in_tar, false).unwrap();
    let out = tmp.path().join("out2");
    let outcome = container_run::submit(
        bnet.clone(),
        bstore.clone(),
        Request {
            context: Some(ctx_tar.clone()),
            build: Some(build.clone()),
            cmd: vec![
                "sh".into(),
                "-c".into(),
                "mkdir -p out && cat /msg in.txt /hello.txt > out/m.txt && echo ran-it".into(),
            ],
            input: Some(in_tar.clone()),
            outputs: vec!["out".into(), "nope".into()],
            builders: vec![worker_id.clone()],
            max_price_usd: 1.0,
            timeout_secs: Some(600),
            memory_mb: Some(256),
            cpus: Some(1.0),
            ..Default::default()
        },
        &out,
        progress.clone(),
    )
    .await
    .expect("run job");
    assert!(outcome.ok(), "run failed: {}", outcome.result);
    assert_eq!(outcome.result["exit_code"], 0);
    assert!(
        outcome.result["log_tail"]
            .as_str()
            .unwrap()
            .contains("ran-it")
    );
    assert_eq!(outcome.outputs.len(), 1, "{}", outcome.result);
    let unpack = tmp.path().join("unpacked");
    std::fs::create_dir_all(&unpack).unwrap();
    let st = std::process::Command::new("tar")
        .args([
            "-xzf",
            &outcome.outputs[0].1.display().to_string(),
            "-C",
            &unpack.display().to_string(),
        ])
        .status()
        .unwrap();
    assert!(st.success());
    let m = std::fs::read_to_string(unpack.join("out/m.txt")).unwrap();
    assert_eq!(m, "built remotely\ninput file\nhello from the context\n");
    assert!(
        outcome.result["outputs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["path"] == "nope" && o["missing"] == true)
    );

    // 3. A failing command: ok false, its exit code and log.
    let outcome = container_run::submit(
        bnet.clone(),
        bstore.clone(),
        Request {
            context: Some(ctx_tar.clone()),
            build: Some(build.clone()),
            cmd: vec!["sh".into(), "-c".into(), "echo boom; exit 3".into()],
            builders: vec![worker_id.clone()],
            max_price_usd: 1.0,
            timeout_secs: Some(600),
            ..Default::default()
        },
        &tmp.path().join("out3"),
        progress.clone(),
    )
    .await
    .expect("failing job still returns a result");
    assert!(!outcome.ok());
    assert_eq!(outcome.result["exit_code"], 3);
    assert!(
        outcome.result["log_tail"]
            .as_str()
            .unwrap()
            .contains("boom")
    );

    // Nothing of the jobs is left in docker.
    let left = docker(&[
        "ps",
        "-a",
        "--filter",
        "label=c0mpute.job",
        "--format",
        "{{.Names}}",
    ]);
    assert!(left.is_empty(), "containers left behind: {left}");

    // 4. A worker that does not trust this buyer never bids.
    container_run::install_policy(BuyerPolicy::default());
    let refused = container_run::submit(
        bnet.clone(),
        bstore.clone(),
        Request {
            context: Some(ctx_tar.clone()),
            build: Some(build),
            builders: vec![worker_id.clone()],
            max_price_usd: 1.0,
            timeout_secs: Some(60),
            ..Default::default()
        },
        &tmp.path().join("out4"),
        progress,
    )
    .await;
    assert!(refused.is_err(), "an untrusted buyer got a build");

    // And a buyer with no trusted builders refuses to send anything.
    let none = container_run::submit(
        bnet,
        bstore,
        Request {
            context: Some(ctx_tar),
            ..Default::default()
        },
        &tmp.path().join("out5"),
        Arc::new(|_: &str| {}),
    )
    .await;
    assert!(
        none.unwrap_err()
            .to_string()
            .contains("no trusted builders")
    );
}
