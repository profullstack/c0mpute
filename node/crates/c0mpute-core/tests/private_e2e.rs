//! Private mode end to end: one worker, two buyers on loopback. The worker
//! trusts only one of them; the stranger's auction gets no bid, the trusted
//! buyer's job runs. Its own test binary because the policy is process-wide.

use std::sync::Arc;
use std::time::Duration;

use c0mpute_core::dispatch;
use c0mpute_core::private::{self, PrivateConfig, PrivatePolicy};
use c0mpute_core::runner::{AcceptedJob, TranscodeJobInline};
use c0mpute_core::{JobAuction, run_auction};
use c0mpute_net::topics::{JobReceipt, JobStatus, job_topic};
use c0mpute_net::{Libp2pNetwork, NetworkConfig};
use c0mpute_proto::{Codec, TranscodeSpec};
use libp2p::multiaddr::Protocol;
use tempfile::tempdir;
use tokio::sync::mpsc;

const WORKLOAD: &str = "ffmpeg.transcode";
const TAG: &str = "c0mpute:role:transcode";

#[tokio::test]
async fn private_worker_serves_only_trusted_buyers() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("c0mpute_core=info,c0mpute_net=warn")
        .with_test_writer()
        .try_init();

    let worker_dir = tempdir().unwrap();
    let worker = Arc::new(
        Libp2pNetwork::spawn(
            NetworkConfig::for_dir(worker_dir.path().to_path_buf())
                .with_listen(vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()]),
        )
        .await
        .unwrap(),
    );
    let worker_full = wait_for_listen(&worker)
        .await
        .with(Protocol::P2p(worker.peer_id()));

    let trusted_dir = tempdir().unwrap();
    let trusted = spawn_buyer(trusted_dir.path(), worker_full.clone()).await;
    let stranger_dir = tempdir().unwrap();
    let stranger = spawn_buyer(stranger_dir.path(), worker_full).await;

    private::install(PrivatePolicy::from_config(
        &PrivateConfig {
            enabled: true,
            buyers: vec![trusted.peer_id().to_base58()],
        },
        [],
    ));

    let (runner_tx, mut runner_rx) = mpsc::channel::<AcceptedJob>(8);
    dispatch::run_worker_subscriber(
        worker.clone(),
        WORKLOAD.into(),
        vec![TAG.into()],
        Some(runner_tx),
    );
    let w = worker.clone();
    tokio::spawn(async move {
        while let Some(job) = runner_rx.recv().await {
            let receipt = JobReceipt {
                job_id: job.accept.job_id.clone(),
                worker_peer_id: w.peer_id().to_base58(),
                worker_did: None,
                buyer_peer_id: job.accept.buyer_peer_id.clone(),
                output_hash: Some("deadbeef".repeat(8)),
                status: JobStatus::Completed,
                completed_at_ms: 0,
                signature: None,
                external_ref: job.accept.external_ref.clone(),
            };
            tokio::time::sleep(Duration::from_millis(100)).await;
            w.publish(
                &job_topic(&job.accept.workload_type),
                serde_json::to_vec(&receipt).unwrap(),
            )
            .await
            .ok();
        }
    });
    tokio::time::sleep(Duration::from_secs(1)).await;

    // A stranger gets no bid from a private worker.
    let refused = run_auction(stranger.clone(), auction()).await;
    assert!(
        refused.is_err(),
        "private worker must not bid for a stranger"
    );

    // The trusted buyer's job runs on it.
    let outcome = run_auction(trusted.clone(), auction())
        .await
        .expect("trusted auction completes");
    assert_eq!(outcome.receipt.status, JobStatus::Completed);
    assert_eq!(
        outcome.accepted_bid.bidder_peer_id,
        worker.peer_id().to_base58()
    );

    // Back to the open market: the same stranger is now served, so the
    // refusal above was the policy, not a mesh that had not formed.
    private::install(PrivatePolicy::public());
    let served = run_auction(stranger.clone(), auction())
        .await
        .expect("public worker serves strangers");
    assert_eq!(served.receipt.status, JobStatus::Completed);
}

fn auction() -> JobAuction {
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
    JobAuction::new(WORKLOAD, serde_json::to_value(&inline).unwrap())
        .with_required_capabilities(vec![TAG.into()])
        .with_max_price_usd(0.05)
        .with_bid_window(Duration::from_secs(4))
}

async fn spawn_buyer(dir: &std::path::Path, boot: libp2p::Multiaddr) -> Arc<Libp2pNetwork> {
    Arc::new(
        Libp2pNetwork::spawn(
            NetworkConfig::for_dir(dir.to_path_buf())
                .with_listen(vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()])
                .with_bootstrap(vec![boot]),
        )
        .await
        .unwrap(),
    )
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
