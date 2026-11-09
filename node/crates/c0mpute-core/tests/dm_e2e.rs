//! Secure chat end to end on loopback libp2p, no coordinator:
//!
//! 1. Alice sends to Bob while Bob is offline: the sealed message lands on
//!    exactly three of four mailbox nodes (rendezvous on Bob's tag).
//! 2. Bob comes online, pulls, decrypts, acks: every copy is deleted.
//! 3. A blocked sender's message is dropped on receipt (and still deleted).
//! 4. Bob looks up Alice by fingerprint from her self-published key record.
//! 5. Bob's home node is up: Alice's next message goes straight into its
//!    spool, no mailbox involved, and Bob reads it from there.

use std::sync::Arc;
use std::time::Duration;

use c0mpute_core::dm::{Dm, Options, Paths, Timing};
use c0mpute_net::{Libp2pNetwork, NetworkConfig};
use c0mpute_secure_chat::mailbox::{Limits, Mailbox};
use c0mpute_secure_chat::sealed::{pull_tags, seal};
use c0mpute_secure_chat::state::{LocalState, Received};
use c0mpute_secure_chat::{ChatKey, HomeRecord, KeyRecord, unix_now};
use libp2p::multiaddr::Protocol;
use tempfile::tempdir;

async fn net(dir: &std::path::Path, bootstrap: Vec<libp2p::Multiaddr>) -> Arc<Libp2pNetwork> {
    Arc::new(
        Libp2pNetwork::spawn(
            NetworkConfig::for_dir(dir.join("id"))
                .with_listen(vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()])
                .with_bootstrap(bootstrap),
        )
        .await
        .unwrap(),
    )
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
        announce_every: Duration::from_millis(500),
        peer_ttl: Duration::from_secs(5),
        rpc_timeout: Duration::from_secs(10),
    }
}

async fn wait_for<F, Fut>(what: &str, secs: u64, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while tokio::time::Instant::now() < deadline {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn offline_store_and_forward_then_direct_delivery() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("c0mpute_core=info,c0mpute_net=warn")
        .with_test_writer()
        .try_init();
    let tmp = tempdir().unwrap();
    let opts = |dir: &std::path::Path, serve: bool| Options {
        serve_mailbox: serve,
        paths: Some(Paths::under(dir)),
        timing: fast(),
        ..Default::default()
    };

    // Four mailbox nodes around a hub.
    let hub_dir = tmp.path().join("m0");
    let hub_net = net(&hub_dir, vec![]).await;
    let hub_addr = listen_addr(&hub_net).await;
    let mut mailboxes = vec![Dm::start(hub_net, opts(&hub_dir, true)).await.unwrap()];
    for i in 1..4 {
        let d = tmp.path().join(format!("m{i}"));
        let n = net(&d, vec![hub_addr.clone()]).await;
        mailboxes.push(Dm::start(n, opts(&d, true)).await.unwrap());
    }

    // Alice: a client node, no mailbox.
    let alice_key = ChatKey::generate();
    let bob_key = ChatKey::generate();
    let ad = tmp.path().join("alice");
    let alice = Dm::start(net(&ad, vec![hub_addr.clone()]).await, opts(&ad, false)).await.unwrap();
    assert_eq!(alice.wait_for_mailboxes(4, Duration::from_secs(20)).await, 4);
    // Every mailbox should see the full set before placement is compared.
    for m in &mailboxes {
        let m = m.clone();
        wait_for("mailbox gossip", 20, || {
            let m = m.clone();
            async move { m.mailboxes().await.len() == 4 }
        })
        .await;
    }

    // 1. Bob is offline (no node at all): store-and-forward.
    let now = unix_now();
    let s = seal(&alice_key, &bob_key.public_identity(), "are you there, bob?", 86_400, now).unwrap();
    let rep = alice.send(&s, &bob_key.address(), 86_400).await.unwrap();
    assert!(rep.direct.is_none());
    assert_eq!(rep.stored_on.len(), 3, "{rep:?}");
    let expected = c0mpute_secure_chat::placement::place(&s.msg.tag, &alice.mailboxes().await, 3);
    let mut stored: Vec<String> = rep.stored_on.clone();
    stored.sort();
    let mut exp = expected.clone();
    exp.sort();
    assert_eq!(stored, exp, "stored on the rendezvous-ranked nodes");
    let holding: usize = mailboxes.iter().map(|m| m.mailbox_count()).sum();
    assert_eq!(holding, 3);

    // A blocked sender's message too.
    let mallory_key = ChatKey::generate();
    let spam = seal(&mallory_key, &bob_key.public_identity(), "spam", 86_400, now).unwrap();
    alice.send(&spam, &bob_key.address(), 86_400).await.unwrap();
    assert_eq!(mailboxes.iter().map(|m| m.mailbox_count()).sum::<usize>(), 6);

    // Alice publishes her key record so others can look her up.
    let published = alice
        .publish_key(&KeyRecord::sign(&alice_key, now, 86_400).unwrap())
        .await
        .unwrap();
    assert!(published >= 1);

    // 2. Bob comes online and pulls.
    let bd = tmp.path().join("bob");
    let bob = Dm::start(net(&bd, vec![hub_addr.clone()]).await, opts(&bd, false)).await.unwrap();
    assert_eq!(bob.wait_for_mailboxes(4, Duration::from_secs(20)).await, 4);
    let mut state = LocalState::default();
    state.blocked.insert(mallory_key.address());
    let fetched = bob.fetch(&bob_key.address()).await;
    assert_eq!(fetched.len(), 2, "both messages found");
    let mut texts = Vec::new();
    let mut blocked = 0;
    for f in &fetched {
        let got = state.receive(&bob_key, &f.msg, "mailbox", unix_now()).unwrap();
        match &got {
            Received::New(o) => texts.push((o.text.clone(), o.sender_address.clone())),
            Received::Blocked(_) => blocked += 1,
        }
        assert_eq!(f.holders.len(), 3);
        assert_eq!(bob.ack(f, got.ack()).await, 3, "every copy deleted");
    }
    assert_eq!(texts, vec![("are you there, bob?".to_string(), alice_key.address())]);
    assert_eq!(blocked, 1);
    assert_eq!(mailboxes.iter().map(|m| m.mailbox_count()).sum::<usize>(), 0);
    assert!(bob.fetch(&bob_key.address()).await.is_empty());
    // A replayed copy (say a mailbox kept one) is refused.
    assert!(state.receive(&bob_key, &fetched[0].msg, "mailbox", unix_now()).is_err());

    // 3. Lookup by fingerprint: resolved from the key record, verified.
    let found = bob.lookup(&alice_key.public_identity().fingerprint()).await.expect("lookup");
    assert_eq!(found.address(), alice_key.address());
    assert!(bob.lookup(&ChatKey::generate().public_identity().fingerprint()).await.is_none());

    // 4. Bob's home node: a running node on Bob's machine.
    let hd = tmp.path().join("bob-home");
    let home = Dm::start(net(&hd, vec![hub_addr.clone()]).await, opts(&hd, false)).await.unwrap();
    let rec = HomeRecord::sign(&bob_key, home.peer_id(), unix_now(), 3600).unwrap();
    home.add_local_home(&rec).await.unwrap();
    // A home record for someone else's peer is refused.
    let wrong = HomeRecord::sign(&bob_key, mailboxes[0].peer_id(), unix_now(), 3600).unwrap();
    assert!(home.add_local_home(&wrong).await.is_err());
    {
        let alice = alice.clone();
        let addr = bob_key.address();
        wait_for("bob's presence to reach alice", 20, || {
            let alice = alice.clone();
            let addr = addr.clone();
            async move { alice.find_home(&addr).await.is_some() }
        })
        .await;
    }
    let s2 = seal(&alice_key, &bob_key.public_identity(), "direct this time", 86_400, unix_now()).unwrap();
    let rep = alice.send(&s2, &bob_key.address(), 86_400).await.unwrap();
    assert_eq!(rep.direct.as_deref(), Some(home.peer_id()), "{rep:?}");
    assert!(rep.stored_on.is_empty());
    assert_eq!(mailboxes.iter().map(|m| m.mailbox_count()).sum::<usize>(), 0);
    // Bob's local client reads the home node's spool.
    let now = unix_now();
    let mut spool = Mailbox::open(&Paths::under(&hd).spool, Limits::default(), now);
    let mut direct = Vec::new();
    for tag in pull_tags(&bob_key.address(), now) {
        for (id, bytes) in spool.fetch(&tag, now) {
            let msg = c0mpute_secure_chat::SealedMessage::parse(&bytes).unwrap();
            let got = state.receive(&bob_key, &msg, "direct", now).unwrap();
            assert!(spool.ack(&tag, &id, got.ack()));
            if let Received::New(o) = got {
                direct.push(o.text);
            }
        }
    }
    assert_eq!(direct, vec!["direct this time".to_string()]);
    assert_eq!(spool.count(), 0);

    // Someone with no home node: back to the mailboxes.
    let carol = ChatKey::generate();
    let s3 = seal(&alice_key, &carol.public_identity(), "x", 3600, unix_now()).unwrap();
    let rep = alice.send(&s3, &carol.address(), 3600).await.unwrap();
    assert!(rep.direct.is_none());
    assert_eq!(rep.stored_on.len(), 3);
}
