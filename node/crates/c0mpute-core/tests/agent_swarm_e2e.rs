//! Agent swarms (DIP 0034) over real libp2p on loopback: a private swarm's
//! events reach a member by gossip, a late joiner catches up by sync, and a
//! node outside the swarm learns nothing.

use std::sync::Arc;
use std::time::Duration;

use c0mpute_core::agent_swarm::event::{Body, JobPost, TaskClaim, TaskReview, TaskSpec, TaskSubmit, Verdict, make_task_id};
use c0mpute_core::agent_swarm::SwarmService;
use c0mpute_net::{Libp2pNetwork, NetworkConfig};
use libp2p::multiaddr::Protocol;
use tempfile::{TempDir, tempdir};

struct Node {
    svc: Arc<SwarmService>,
    net: Arc<Libp2pNetwork>,
    _dir: TempDir,
}

async fn node(bootstrap: Vec<libp2p::Multiaddr>) -> Node {
    let dir = tempdir().unwrap();
    let net = Arc::new(
        Libp2pNetwork::spawn(
            NetworkConfig::for_dir(dir.path().join("id"))
                .with_listen(vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()])
                .with_bootstrap(bootstrap),
        )
        .await
        .unwrap(),
    );
    let svc = SwarmService::start(net.clone(), &dir.path().join("swarm"), false).await.unwrap();
    Node { svc, net, _dir: dir }
}

async fn addr(net: &Libp2pNetwork) -> libp2p::Multiaddr {
    for _ in 0..100 {
        if let Ok(addrs) = net.listen_addrs().await
            && let Some(a) = addrs.into_iter().next()
        {
            return a.with(Protocol::P2p(net.peer_id()));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no listen addr");
}

async fn wait_for(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    for _ in 0..secs * 10 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

fn quiet() {
    // SAFETY: set before any node starts; the test owns its process env.
    unsafe {
        std::env::set_var("C0MPUTE_DISABLE_MDNS", "1");
        std::env::set_var("C0MPUTE_DISABLE_RELAY", "1");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_private_swarm_runs_a_job_end_to_end() {
    quiet();
    let a = node(vec![]).await;
    let a_addr = addr(&a.net).await;
    let b = node(vec![a_addr.clone()]).await;
    let outsider = node(vec![a_addr.clone()]).await;

    let link = a.svc.create("team").await.unwrap();
    let topic = b.svc.join(&link, None).await.unwrap();
    assert_eq!(a.svc.resolve("team").unwrap(), topic);
    // Let the dials land and gossipsub exchange subscriptions.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let job = a
        .svc
        .publish(
            &topic,
            Body::JobPost(JobPost {
                title: "Write a haiku about p2p".into(),
                desc: String::new(),
                tasks: vec![TaskSpec { key: "draft".into(), title: "Draft it".into(), desc: String::new(), needs: vec![] }],
                bounty_usd: String::new(),
                owners: vec![],
            }),
        )
        .await
        .unwrap();
    // Gossip, or the 30 s sync round at the latest.
    wait_for("b to see the job", 45, || b.svc.state(&topic).is_ok_and(|s| s.jobs.contains_key(&job.id))).await;

    let draft = make_task_id(&job.id, "draft");
    b.svc.publish(&topic, Body::TaskClaim(TaskClaim { task: draft.clone(), lease_secs: 60 })).await.unwrap();
    let sub = b
        .svc
        .publish(&topic, Body::TaskSubmit(TaskSubmit { task: draft.clone(), content: "packets drift like leaves".into(), payee: None }))
        .await
        .unwrap();
    wait_for("a to see the result", 45, || a.svc.state(&topic).is_ok_and(|s| s.submissions.contains_key(&sub.id))).await;
    a.svc
        .publish(&topic, Body::TaskReview(TaskReview { submission: sub.id.clone(), verdict: Verdict::Accept, note: String::new() }))
        .await
        .unwrap();
    wait_for("b to see the task done", 45, || b.svc.state(&topic).is_ok_and(|s| s.task_done(&draft))).await;

    // A late joiner catches up on the whole log by sync.
    let c = node(vec![a_addr]).await;
    c.svc.join(&link, None).await.unwrap();
    wait_for("c to catch up", 60, || c.svc.state(&topic).is_ok_and(|s| s.task_done(&draft))).await;

    // The outsider relays gossip on nothing it can read and holds nothing.
    assert!(outsider.svc.resolve(&topic).is_err());
    assert!(outsider.svc.state(&topic).is_err());
}
