//! End-to-end `blender.render-frame` on loopback libp2p: Blender builds a tiny
//! scene, the buyer publishes it in pieces, two real render workers bid, and
//! `render::render_frames` splits three frames into jobs, runs them in
//! parallel, fetches and checks every frame, and writes them out. Then the
//! jobs bridge runs one frame and returns it inline.
//!
//! Needs `blender`; where it is missing the test says so and passes, like the
//! other runtime tests.

use std::sync::Arc;
use std::time::Duration;

use c0mpute_core::jobs_gateway::JobsBridge;
use c0mpute_core::pricing::PricingConfig;
use c0mpute_core::{dispatch, render};
use c0mpute_net::{ChunkSource, Libp2pNetwork, NetworkConfig};
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

async fn worker(dir: &std::path::Path) -> (Arc<Libp2pNetwork>, libp2p::Multiaddr) {
    let (net, store) = node(dir, vec![]).await;
    let addr = wait_for_listen(&net)
        .await
        .with(Protocol::P2p(net.peer_id()));
    let load = c0mpute_core::WorkerLoad::new(Arc::new(tokio::sync::Semaphore::new(2)), 2);
    let runner = render::spawn_render_runner(net.clone(), store, dir.join("cache"), load.clone());
    let mut tags = render::runtime().tags();
    tags.push("c0mpute:cpu".to_string());
    dispatch::run_worker_subscriber_with_load(
        net.clone(),
        render::WORKLOAD.to_string(),
        tags,
        Some(runner),
        Some(render::offer_filter()),
        Some(PricingConfig::default()),
        Some(load),
    );
    (net, addr)
}

/// Save the default scene (cube, camera, light) at 64x48, Cycles, to `path`,
/// with a packed 1600x1600 noise image so the file spans several pieces.
fn make_scene(path: &std::path::Path) {
    let expr = format!(
        "import bpy, random\ns = bpy.context.scene\ns.render.engine = 'CYCLES'\n\
         s.render.resolution_x = 64\ns.render.resolution_y = 48\n\
         s.render.resolution_percentage = 100\ns.frame_start = 1\ns.frame_end = 3\n\
         s.cycles.samples = 4\n\
         img = bpy.data.images.new('noise', 1600, 1600)\n\
         img.pixels.foreach_set([random.random() for _ in range(1600 * 1600 * 4)])\n\
         img.pack()\nimg.use_fake_user = True\n\
         bpy.ops.wm.save_as_mainfile(filepath={:?})\n",
        path.to_string_lossy()
    );
    let status = std::process::Command::new("blender")
        .args(["--background", "--factory-startup", "--python-expr", &expr])
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(
        status.success() && path.exists(),
        "could not build the test scene"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn frames_render_across_workers() {
    let rt = tokio::task::spawn_blocking(render::runtime).await.unwrap();
    if !rt.ready() {
        eprintln!("skipping: blender is not installed");
        return;
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter("c0mpute_core=info,c0mpute_net=warn")
        .with_test_writer()
        .try_init();
    let tmp = tempdir().unwrap();
    let blend = tmp.path().join("scene.blend");
    make_scene(&blend);
    let size = std::fs::metadata(&blend).unwrap().len();
    assert!(
        render::piece_count(size) >= 2,
        "the test scene should span several pieces ({size} bytes)"
    );

    let (w1, a1) = worker(&tmp.path().join("w1")).await;
    let (w2, a2) = worker(&tmp.path().join("w2")).await;
    let (buyer, buyer_store) = node(&tmp.path().join("b"), vec![a1, a2]).await;
    tokio::time::sleep(Duration::from_secs(3)).await;

    let out = tmp.path().join("frames");
    let job = render::RenderJob {
        blend: blend.clone(),
        frames: render::parse_frames("1-3", 100).unwrap(),
        batch: 1,
        engine: render::Engine::Cycles,
        device: render::Device::Cpu,
        format: render::Format::Png,
        resolution_pct: None,
        samples: Some(4),
        frame_timeout: Duration::from_secs(300),
        max_price_usd: 0.0,
        concurrency: 3,
        attempts: 3,
        out_dir: out.clone(),
        allowed_bidders: Vec::new(),
        bid_window: Duration::from_secs(2),
    };
    let report = render::render_frames(buyer.clone(), &buyer_store, job, |line| {
        eprintln!("render: {line}")
    })
    .await
    .unwrap();
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(
        report.written.iter().map(|f| f.frame).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    let peers = [w1.peer_id().to_base58(), w2.peer_id().to_base58()];
    for f in &report.written {
        assert!(peers.contains(&f.worker), "{}", f.worker);
        let bytes = std::fs::read(&f.path).unwrap();
        let info = render::inspect_image(&bytes).unwrap();
        assert_eq!(
            (info.format, info.width, info.height),
            (render::Format::Png, 64, 48)
        );
        assert_eq!(
            f.path,
            render::frame_path(&out, f.frame, render::Format::Png)
        );
    }

    // The jobs bridge: one frame at half size, as JPEG, returned inline.
    let pieces = render::put_file_pieces(&buyer, &buyer_store, &blend, render::MAX_INPUT_BYTES)
        .await
        .unwrap();
    let bridge = JobsBridge::new(
        buyer.clone(),
        buyer_store,
        "test-token-0123456789abcdef".into(),
        0.0,
        vec![],
    )
    .unwrap()
    .with_bid_window(Duration::from_secs(2));
    let spec = json!({
        "v": 1,
        "input_hash": pieces.hash,
        "input_size": pieces.size,
        "input_pieces": pieces.pieces,
        "input_provider_peer_id": buyer.peer_id().to_base58(),
        "frame_start": 2,
        "frame_end": 2,
        "format": "JPEG",
        "resolution_pct": 50,
        "samples": 2,
        "device": "CPU",
    });
    let res = bridge
        .submit_once(render::WORKLOAD, spec.clone(), Duration::from_secs(240))
        .await
        .expect("bridge render job");
    let frame = &res["result"]["frames"][0];
    assert_eq!(frame["frame"], 2);
    assert_eq!(frame["width"], 32);
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(frame["data_base64"].as_str().unwrap())
        .unwrap();
    let info = render::inspect_image(&bytes).unwrap();
    assert_eq!(
        (info.format, info.width, info.height),
        (render::Format::Jpeg, 32, 24)
    );

    // A GPU-only engine: these CPU workers' filter keeps them from bidding.
    if rt.gpu().is_none() {
        let mut eevee = spec;
        eevee["engine"] = json!("EEVEE");
        eevee["device"] = json!("AUTO");
        let err = bridge
            .submit_once(render::WORKLOAD, eevee, Duration::from_secs(10))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no_bids") || err.contains("timeout"), "{err}");
    }
}
