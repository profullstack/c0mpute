//! A buyer that restarts mid-job (DIP-0032): the same identity re-derives
//! the job's key, collects the job from the worker still running it (no
//! re-auction, no second run) and opens the sealed result. A job the buyer
//! abandons is cancelled on the worker.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use c0mpute_core::job_e2e::InFlightJob;
use c0mpute_core::runner::{AcceptedJob, publish_receipt};
use c0mpute_core::{JobAuction, buyer, dispatch, gateway_util, job_control, pieces, run_auction};
use c0mpute_net::topics::JobStatus;
use c0mpute_net::{ChunkSource, Libp2pNetwork, NetworkConfig};
use c0mpute_proto::Hash;
use c0mpute_store::ChunkStore;
use libp2p::multiaddr::Protocol;
use serde_json::json;
use tempfile::{TempDir, tempdir};
use tokio::sync::mpsc;

const WORKLOAD: &str = "diffusion.txt2img";
const RESULT: &[u8] = b"PNG-ish sealed render result";

struct StoreSource(ChunkStore);

#[async_trait::async_trait]
impl ChunkSource for StoreSource {
    async fn read_chunk(&self, hash: &Hash) -> anyhow::Result<Vec<u8>> {
        self.0.get(hash).await
    }
}

async fn spawn(dir: &std::path::Path, bootstrap: Vec<libp2p::Multiaddr>, store: Option<ChunkStore>) -> Arc<Libp2pNetwork> {
    let mut cfg = NetworkConfig::for_dir(dir.join("id"))
        .with_listen(vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()])
        .with_bootstrap(bootstrap);
    if let Some(s) = store {
        cfg = cfg.with_local_source(Arc::new(StoreSource(s)));
    }
    Arc::new(Libp2pNetwork::spawn(cfg).await.unwrap())
}

async fn addr(net: &Libp2pNetwork) -> libp2p::Multiaddr {
    for _ in 0..50 {
        if let Ok(a) = net.listen_addrs().await
            && let Some(a) = a.into_iter().next()
        {
            return a.with(Protocol::P2p(net.peer_id()));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no listen addr");
}

struct Worker {
    net: Arc<Libp2pNetwork>,
    runs: Arc<AtomicUsize>,
    aborted: Arc<AtomicBool>,
    _dir: TempDir,
}

/// A worker whose "render" takes `work` and can be cancelled.
async fn worker(work: Duration) -> Worker {
    let dir = tempdir().unwrap();
    let store = ChunkStore::open(dir.path().join("store")).await.unwrap();
    let net = spawn(dir.path(), vec![], Some(store.clone())).await;
    let (tx, mut rx) = mpsc::channel::<AcceptedJob>(4);
    dispatch::run_worker_subscriber(net.clone(), WORKLOAD.into(), vec![], Some(tx));
    let runs = Arc::new(AtomicUsize::new(0));
    let aborted = Arc::new(AtomicBool::new(false));
    let (n, r, a) = (net.clone(), runs.clone(), aborted.clone());
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let (net, store, runs, aborted) = (n.clone(), store.clone(), r.clone(), a.clone());
            tokio::spawn(async move {
                runs.fetch_add(1, Ordering::SeqCst);
                struct Guard(Arc<AtomicBool>, bool);
                impl Drop for Guard {
                    fn drop(&mut self) {
                        if !self.1 {
                            self.0.store(true, Ordering::SeqCst);
                        }
                    }
                }
                let result = job_control::cancellable(&job.accept.job_id, async {
                    let mut g = Guard(aborted.clone(), false);
                    tokio::time::sleep(work).await;
                    g.1 = true;
                    pieces::publish_result_bytes(&net, &store, &job.accept, RESULT).await
                })
                .await;
                match result {
                    Ok(hash) => publish_receipt(&net, &job.accept, Some(hash), JobStatus::Completed).await,
                    Err(_) => publish_receipt(&net, &job.accept, None, JobStatus::Failed).await,
                }
            });
        }
    });
    Worker { net, runs, aborted, _dir: dir }
}

/// Start an auction from a buyer on `id_dir` and return the in-flight record
/// as soon as the accept is out, with the auction task (to abort = "crash").
async fn start(
    id_dir: &std::path::Path,
    w: &Worker,
) -> (InFlightJob, tokio::task::JoinHandle<anyhow::Result<c0mpute_core::AuctionOutcome>>) {
    let buyer = spawn(id_dir, vec![addr(&w.net).await], None).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let seen: Arc<Mutex<Option<InFlightJob>>> = Arc::default();
    let s = seen.clone();
    let auction = JobAuction::new(WORKLOAD, json!({"prompt": "a private castle", "model": "m", "width": 64, "height": 64, "steps": 1}))
        .with_allowed_bidders(vec![w.net.peer_id().to_base58()])
        .with_bid_window(Duration::from_secs(2))
        .with_receipt_timeout(Duration::from_secs(120))
        .with_on_accept(Arc::new(move |j| *s.lock().unwrap() = Some(j.clone())));
    let task = tokio::spawn(run_auction(buyer, auction));
    for _ in 0..200 {
        if let Some(j) = seen.lock().unwrap().clone() {
            return (j, task);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no accept");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restarted_buyer_collects_the_running_job_without_a_second_run() {
    // SAFETY: set before any node starts; the test owns its process env.
    unsafe { std::env::set_var("C0MPUTE_DISABLE_MDNS", "1") };
    let w = worker(Duration::from_secs(6)).await;
    let buyer_dir = tempdir().unwrap();
    let (job, task) = start(buyer_dir.path(), &w).await;
    assert!(!serde_json::to_string(&job).unwrap().contains("castle"), "the record holds no spec");

    // The gateway "restarts" mid-render: the auction and its node are gone.
    task.abort();
    let _ = task.await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Same identity (and job secret) on disk: a new process.
    let buyer = spawn(buyer_dir.path(), vec![addr(&w.net).await], None).await;
    assert_eq!(buyer.peer_id().to_base58(), job.buyer_peer);
    tokio::time::sleep(Duration::from_secs(1)).await;
    let outcome = buyer::resume_auction(buyer.clone(), &job, Duration::from_secs(60)).await.expect("resumed");
    assert_eq!(outcome.receipt.status, JobStatus::Completed);
    assert_eq!(outcome.job_id, job.job_id);
    let opened = gateway_util::fetch_result(&buyer, &outcome, 1 << 20, Duration::from_secs(20)).await.unwrap();
    assert_eq!(opened, RESULT, "the re-derived key opens the sealed result");
    assert_eq!(w.runs.load(Ordering::SeqCst), 1, "no second render");

    // Asked again after it finished: the worker hands over the receipt.
    let again = buyer::resume_auction(buyer.clone(), &job, Duration::from_secs(10)).await.expect("done job");
    assert_eq!(again.receipt.output_hash, outcome.receipt.output_hash);

    // Another node with a different identity cannot collect it.
    let stranger_dir = tempdir().unwrap();
    let stranger = spawn(stranger_dir.path(), vec![addr(&w.net).await], None).await;
    assert!(buyer::resume_auction(stranger, &job, Duration::from_secs(5)).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_abandoned_job_is_cancelled_on_the_worker() {
    // SAFETY: as above.
    unsafe { std::env::set_var("C0MPUTE_DISABLE_MDNS", "1") };
    let w = worker(Duration::from_secs(120)).await;
    let buyer_dir = tempdir().unwrap();
    let (job, task) = start(buyer_dir.path(), &w).await;
    task.abort();
    let _ = task.await;
    let buyer = spawn(buyer_dir.path(), vec![addr(&w.net).await], None).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    // Wait until the worker actually started the render.
    for _ in 0..100 {
        if w.runs.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    buyer::cancel_job(&buyer, &job).await;
    for _ in 0..100 {
        if w.aborted.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(w.aborted.load(Ordering::SeqCst), "the render stopped");
    let Err(err) = buyer::resume_auction(buyer, &job, Duration::from_secs(5)).await else {
        panic!("a cancelled job must not resume");
    };
    assert!(format!("{err:#}").contains("Cancelled"), "{err:#}");
}
