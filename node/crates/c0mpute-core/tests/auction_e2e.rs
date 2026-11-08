//! End-to-end auction test: two libp2p nodes (buyer + worker) on
//! loopback. The worker's runner is a stub that immediately publishes a
//! `Completed` receipt — the test validates the offer → bid → accept →
//! receipt round-trip without depending on ffmpeg.

use std::sync::Arc;
use std::time::Duration;

use c0mpute_core::dispatch;
use c0mpute_core::runner::{AcceptedJob, TranscodeJobInline};
use c0mpute_core::{JobAuction, run_auction};
use c0mpute_net::topics::{JobReceipt, JobStatus, job_topic};
use c0mpute_net::{Libp2pNetwork, NetworkConfig};
use c0mpute_proto::{Codec, TranscodeSpec};
use libp2p::multiaddr::Protocol;
use tempfile::tempdir;
use tokio::sync::mpsc;

#[tokio::test]
async fn auction_round_trip_publishes_receipt() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("c0mpute_core=info,c0mpute_net=warn")
        .with_test_writer()
        .try_init();

    // Worker — listens first so the buyer can bootstrap to it.
    let worker_dir = tempdir().expect("tempdir");
    let worker: Arc<Libp2pNetwork> = Arc::new(
        Libp2pNetwork::spawn(
            NetworkConfig::for_dir(worker_dir.path().to_path_buf())
                .with_listen(vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()]),
        )
        .await
        .expect("spawn worker"),
    );
    let worker_addr = wait_for_listen(&worker).await;
    let worker_full = worker_addr.with(Protocol::P2p(worker.peer_id()));

    let buyer_dir = tempdir().expect("tempdir");
    let buyer: Arc<Libp2pNetwork> = Arc::new(
        Libp2pNetwork::spawn(
            NetworkConfig::for_dir(buyer_dir.path().to_path_buf())
                .with_listen(vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()])
                .with_bootstrap(vec![worker_full]),
        )
        .await
        .expect("spawn buyer"),
    );

    // Wire the worker dispatch + a stub runner that emits a Completed
    // receipt for whatever offer it wins.
    let advertised_tags = vec!["c0mpute:role:transcode".to_string()];
    let workload_type = "ffmpeg.transcode".to_string();
    let (runner_tx, mut runner_rx) = mpsc::channel::<AcceptedJob>(8);
    dispatch::run_worker_subscriber(
        worker.clone(),
        workload_type.clone(),
        advertised_tags,
        Some(runner_tx),
    );

    let worker_for_runner = worker.clone();
    tokio::spawn(async move {
        while let Some(job) = runner_rx.recv().await {
            let receipt = JobReceipt {
                job_id: job.accept.job_id.clone(),
                worker_peer_id: worker_for_runner.peer_id().to_base58(),
                worker_did: None,
                buyer_peer_id: job.accept.buyer_peer_id.clone(),
                output_hash: Some("deadbeef".repeat(8)),
                status: JobStatus::Completed,
                completed_at_ms: 0,
                signature: None,
                external_ref: job.accept.external_ref.clone(),
            };
            let topic = job_topic(&job.accept.workload_type);
            let payload = serde_json::to_vec(&receipt).unwrap();
            // Brief wait so the gossipsub mesh has time to fan the
            // receipt back to the buyer.
            tokio::time::sleep(Duration::from_millis(100)).await;
            worker_for_runner.publish(&topic, payload).await.ok();
        }
    });

    // gossipsub mesh warm-up.
    tokio::time::sleep(Duration::from_secs(1)).await;

    let inline = TranscodeJobInline {
        input_url: "https://example.invalid/never-fetched".into(),
        preset: "video-720p".into(),
        spec: TranscodeSpec {
            codec: Codec::H264,
            bitrate_bps: 2_500_000,
            width: 1280,
            height: 720,
            keyframe_interval: 60,
            hardware_pref: None,
            extra_ffmpeg_args: vec![],
        },
    };
    let auction = JobAuction::new(workload_type, serde_json::to_value(&inline).unwrap())
        .with_required_capabilities(vec!["c0mpute:role:transcode".into()])
        .with_max_price_usd(0.05)
        .with_bid_window(Duration::from_secs(5))
        .with_external_ref(Some("infernet:job_e2e".into()));

    let outcome = run_auction(buyer.clone(), auction)
        .await
        .expect("auction completes");
    assert_eq!(outcome.receipt.status, JobStatus::Completed);
    assert_eq!(
        outcome.accepted_bid.bidder_peer_id,
        worker.peer_id().to_base58()
    );
    assert!(outcome.receipt.output_hash.is_some());
    // The billing id travels offer -> accept -> worker -> receipt.
    assert_eq!(
        outcome.receipt.external_ref.as_deref(),
        Some("infernet:job_e2e")
    );
    assert_eq!(outcome.external_ref.as_deref(), Some("infernet:job_e2e"));
    // Bids now carry an expiry and an ETA (default before any job ran).
    assert!(outcome.accepted_bid.expires_at_ms.is_some());
    assert_eq!(outcome.accepted_bid.eta_seconds, 60);
}

async fn wait_for_listen(net: &Libp2pNetwork) -> libp2p::Multiaddr {
    for _ in 0..50 {
        if let Ok(addrs) = net.listen_addrs().await {
            if let Some(a) = addrs.into_iter().next() {
                return a;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no listen addr after 2.5s");
}
