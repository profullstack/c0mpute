//! Cross-node placement and retrieval (CIP-003 acceptance criteria).
//!
//! Two levels: a fast in-memory transport for the failure matrix, and a real
//! multi-node HTTP test that stands up actual gateway servers and talks to
//! them over the CIP-002 endpoints.

use std::sync::Arc;

use c0mpute_placement::transport::memory::MemoryTransport;
use c0mpute_placement::{
    BlockState, DistributedConfig, DistributedStorage, PeerCatalog, PeerInfo, PlacementError,
    PlacementPolicy, ShardTransport,
};
use c0mpute_proto::Hash;
use c0mpute_store::{ChunkStore, Storage, Tier};
use tokio::sync::RwLock;

fn tempdir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "c0mpute-placement-{tag}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn local_storage(tag: &str) -> Storage {
    Storage::new(ChunkStore::open(&tempdir(tag)).await.unwrap())
}

/// `count` healthy peers, each in its own failure domain.
fn healthy_peers(count: usize) -> Vec<PeerInfo> {
    (0..count)
        .map(|i| PeerInfo {
            peer_id: format!("peer{i}"),
            endpoint: format!("http://peer{i}.test:7780"),
            reputation: 0.95,
            uptime_30d: 0.995,
            free_bytes: 1 << 30,
            rtt_ms: 20 + i as u32,
            asn: Some(64500 + i as u32),
            region: None,
            ip_prefix: None,
        })
        .collect()
}

fn varied(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut state: u64 = 0xfeed_face_dead_beef;
    for _ in 0..len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.push((state & 0xff) as u8);
    }
    out
}

struct Net {
    storage: DistributedStorage,
    transport: MemoryTransport,
}

async fn net(tag: &str, peer_count: usize) -> Net {
    let transport = MemoryTransport::new();
    let catalog = Arc::new(RwLock::new(PeerCatalog::new(healthy_peers(peer_count))));
    let storage = DistributedStorage::new(
        local_storage(tag).await,
        Arc::new(transport.clone()),
        catalog,
    );
    Net { storage, transport }
}

// ------------------------------------------------------------------ placement

#[tokio::test]
async fn shards_land_on_distinct_peers() {
    let n = net("distinct", 20).await;
    let data = varied(100_000);

    let manifest = n.storage.put(&data, Tier::Standard).await.unwrap();
    assert_eq!(manifest.blocks.len(), 1);
    assert_eq!(manifest.blocks[0].shards.len(), 14);

    let hosts: std::collections::HashSet<_> = manifest.blocks[0]
        .shards
        .iter()
        .map(|s| s.host_hint.clone().unwrap())
        .collect();
    assert_eq!(hosts.len(), 14, "a block's shards must not share a peer");

    // And no peer is holding more than one shard of it.
    for host in &hosts {
        assert_eq!(n.transport.shard_count(host), 1);
    }
}

#[tokio::test]
async fn host_hints_are_recorded_and_reads_go_to_peers() {
    let n = net("hints", 20).await;
    let data = varied(50_000);
    let manifest = n.storage.put(&data, Tier::Standard).await.unwrap();

    assert!(
        manifest.blocks[0]
            .shards
            .iter()
            .all(|s| s.host_hint.is_some()),
        "every placed shard should record where it went"
    );

    let before = n.transport.get_calls();
    assert_eq!(n.storage.get(&manifest.object_hash).await.unwrap(), data);
    assert!(
        n.transport.get_calls() > before,
        "the read should have gone to peers, not to local disk"
    );
}

#[tokio::test]
async fn multi_block_objects_spread_across_the_network() {
    let n = net("spread", 40).await;
    // Three blocks at the 4 MiB default.
    let data = varied(4 * 1024 * 1024 * 3);
    let manifest = n.storage.put(&data, Tier::Standard).await.unwrap();
    assert_eq!(manifest.blocks.len(), 3);

    // Peers are chosen per block, so more than 14 distinct hosts are involved.
    let hosts: std::collections::HashSet<_> = manifest
        .blocks
        .iter()
        .flat_map(|b| b.shards.iter())
        .map(|s| s.host_hint.clone().unwrap())
        .collect();
    assert!(hosts.len() >= 14, "only {} hosts used", hosts.len());
    assert_eq!(n.storage.get(&manifest.object_hash).await.unwrap(), data);
}

#[tokio::test]
async fn hot_tier_places_three_replicas_on_three_peers() {
    let n = net("hot", 10).await;
    let data = varied(10_000);
    let manifest = n.storage.put(&data, Tier::Hot).await.unwrap();

    assert_eq!(manifest.blocks[0].shards.len(), 3);
    let hosts: std::collections::HashSet<_> = manifest.blocks[0]
        .shards
        .iter()
        .map(|s| s.host_hint.clone().unwrap())
        .collect();
    assert_eq!(hosts.len(), 3);
    assert_eq!(n.storage.get(&manifest.object_hash).await.unwrap(), data);
}

// ------------------------------------------------------------------ durability

#[tokio::test]
async fn survives_losing_the_parity_budget() {
    let n = net("parity", 20).await;
    let data = varied(200_000);
    let manifest = n.storage.put(&data, Tier::Standard).await.unwrap();

    for shard in manifest.blocks[0].shards.iter().take(4) {
        n.transport.take_offline(shard.host_hint.as_ref().unwrap());
    }
    assert_eq!(
        n.storage.get(&manifest.object_hash).await.unwrap(),
        data,
        "RS 10/14 must tolerate 4 lost hosts"
    );
}

#[tokio::test]
async fn fails_clearly_past_the_parity_budget() {
    let n = net("lost", 20).await;
    let data = varied(200_000);
    let manifest = n.storage.put(&data, Tier::Standard).await.unwrap();

    for shard in manifest.blocks[0].shards.iter().take(5) {
        n.transport.take_offline(shard.host_hint.as_ref().unwrap());
    }
    let err = format!(
        "{:#}",
        n.storage.get(&manifest.object_hash).await.unwrap_err()
    );
    assert!(err.contains("need 10 shards"), "unhelpful error: {err}");
    assert!(err.contains("got 9"), "unhelpful error: {err}");
}

/// A peer serving wrong bytes must not corrupt the reconstruction — parity
/// covers it and the read still succeeds.
#[tokio::test]
async fn a_dishonest_peer_cannot_poison_a_read() {
    let n = net("dishonest", 20).await;
    let data = varied(120_000);
    let manifest = n.storage.put(&data, Tier::Standard).await.unwrap();

    for shard in manifest.blocks[0].shards.iter().take(3) {
        n.transport.make_corrupt(shard.host_hint.as_ref().unwrap());
    }
    assert_eq!(n.storage.get(&manifest.object_hash).await.unwrap(), data);
}

// ----------------------------------------------------------------- write path

/// Write acknowledges at k + ceil(parity/2) = 12 of 14, so two dead peers do
/// not fail the write — they leave it under-replicated for repair.
#[tokio::test]
async fn write_succeeds_at_quorum_with_two_peers_down() {
    let n = net("quorum", 20).await;
    n.transport.take_offline("peer0");
    n.transport.take_offline("peer1");

    let data = varied(80_000);
    let manifest = n.storage.put(&data, Tier::Standard).await.unwrap();
    let placed = manifest.blocks[0].shards.len();
    assert!(
        (12..=14).contains(&placed),
        "expected quorum placement, got {placed}"
    );
    assert_eq!(n.storage.get(&manifest.object_hash).await.unwrap(), data);
}

#[tokio::test]
async fn write_fails_below_quorum() {
    let n = net("noquorum", 16).await;
    // Only 16 peers, and 5 of them are dead: at most 11 placements, under the
    // quorum of 12.
    for i in 0..5 {
        n.transport.take_offline(&format!("peer{i}"));
    }
    let err = format!(
        "{:#}",
        n.storage
            .put(&varied(50_000), Tier::Standard)
            .await
            .unwrap_err()
    );
    assert!(err.contains("write quorum"), "unhelpful error: {err}");
}

// ------------------------------------------------------------------ diversity

/// The property the whole design rests on. Twenty healthy peers, but they all
/// sit behind two ASNs, so placement would be two correlated samples dressed
/// as fourteen independent ones. It must refuse.
#[tokio::test]
async fn refuses_to_place_on_a_network_without_diversity() {
    let transport = MemoryTransport::new();
    let mut peers = healthy_peers(20);
    for (i, p) in peers.iter_mut().enumerate() {
        p.asn = Some(if i < 10 { 100 } else { 200 });
    }
    let catalog = Arc::new(RwLock::new(PeerCatalog::new(peers)));
    let storage = DistributedStorage::new(
        local_storage("nodiversity").await,
        Arc::new(transport.clone()),
        catalog,
    );

    let err = format!(
        "{:#}",
        storage
            .put(&varied(50_000), Tier::Standard)
            .await
            .unwrap_err()
    );
    assert!(err.contains("diversity unsatisfiable"), "unhelpful: {err}");
    assert!(err.contains("7 distinct domains"), "unhelpful: {err}");

    // And nothing was written — a refused placement must not leave shards
    // scattered across the peers it did reach.
    assert_eq!(transport.put_calls(), 0);
}

#[tokio::test]
async fn a_too_small_network_is_an_error_not_a_silent_downgrade() {
    let n = net("tiny", 6).await;
    let err = format!(
        "{:#}",
        n.storage
            .put(&varied(10_000), Tier::Standard)
            .await
            .unwrap_err()
    );
    assert!(
        err.contains("not enough eligible peers"),
        "unhelpful: {err}"
    );
    assert!(err.contains("need 14"), "unhelpful: {err}");
}

/// An operator who knowingly runs a small network can relax the policy, but it
/// has to be deliberate.
#[tokio::test]
async fn policy_can_be_relaxed_explicitly() {
    let transport = MemoryTransport::new();
    let mut peers = healthy_peers(14);
    for p in peers.iter_mut() {
        p.asn = Some(1); // all one domain
    }
    let catalog = Arc::new(RwLock::new(PeerCatalog::new(peers)));
    let storage = DistributedStorage::new(
        local_storage("relaxed").await,
        Arc::new(transport.clone()),
        catalog,
    )
    .with_config(DistributedConfig {
        policy: Some(PlacementPolicy {
            max_per_domain: 14,
            ..PlacementPolicy::for_parity(4)
        }),
        keep_local_copy: false,
    });

    let data = varied(30_000);
    let manifest = storage.put(&data, Tier::Standard).await.unwrap();
    assert_eq!(storage.get(&manifest.object_hash).await.unwrap(), data);
}

// -------------------------------------------------------------------- health

#[tokio::test]
async fn health_reports_degradation_per_block() {
    let n = net("health", 20).await;
    let data = varied(60_000);
    let manifest = n.storage.put(&data, Tier::Standard).await.unwrap();

    let health = n.storage.health(&manifest).await.unwrap();
    assert_eq!(health.len(), 1);
    assert_eq!(health[0].healthy, 14);
    assert_eq!(health[0].state, BlockState::Healthy);

    for shard in manifest.blocks[0].shards.iter().take(3) {
        n.transport.take_offline(shard.host_hint.as_ref().unwrap());
    }
    let health = n.storage.health(&manifest).await.unwrap();
    assert_eq!(health[0].healthy, 11);
    assert_eq!(health[0].missing.len(), 3);
    assert_eq!(health[0].state, BlockState::Urgent);
    assert!(health[0].state.needs_repair());
}

// ------------------------------------------------------ real multi-node HTTP

/// The end-to-end case: five real gateway servers, shards pushed over the
/// CIP-002 HTTP endpoints, and an object reconstructed from them.
///
/// Uses `hot` (n=3) so a five-node testnet is enough; the failure matrix above
/// covers RS 10/14 on the in-memory transport.
#[tokio::test]
async fn places_and_reads_across_real_http_nodes() {
    use axum::Router;
    use c0mpute_gateway::storage_api::{self, StorageApiState};
    use c0mpute_placement::HttpTransport;

    async fn spawn_node(tag: &str) -> (String, Storage) {
        let storage = local_storage(tag).await;
        let state = StorageApiState::local(storage.clone());
        let app: Router = storage_api::router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), storage)
    }

    let mut peers = Vec::new();
    let mut stores = Vec::new();
    for i in 0..5 {
        let (endpoint, store) = spawn_node(&format!("http-node{i}")).await;
        peers.push(PeerInfo {
            peer_id: format!("node{i}"),
            endpoint,
            reputation: 1.0,
            uptime_30d: 1.0,
            free_bytes: 1 << 30,
            rtt_ms: 1,
            asn: Some(64500 + i),
            region: None,
            ip_prefix: None,
        });
        stores.push(store);
    }

    let catalog = Arc::new(RwLock::new(PeerCatalog::new(peers.clone())));
    let client = DistributedStorage::new(
        local_storage("http-client").await,
        Arc::new(HttpTransport::default()),
        catalog,
    );

    let data = varied(250_000);
    let manifest = client.put(&data, Tier::Hot).await.unwrap();
    assert_eq!(manifest.blocks[0].shards.len(), 3);

    // The shards really are on three different servers' disks.
    let mut holders = 0;
    for store in &stores {
        for shard in &manifest.blocks[0].shards {
            if store.chunk_store().has(&shard.hash).await {
                holders += 1;
                break;
            }
        }
    }
    assert_eq!(holders, 3, "shards should be spread over three real nodes");

    assert_eq!(client.get(&manifest.object_hash).await.unwrap(), data);

    // The client itself holds no shard bytes — only the manifest.
    for shard in &manifest.blocks[0].shards {
        assert!(
            !client.local().chunk_store().has(&shard.hash).await,
            "the writer should not keep a redundant local copy by default"
        );
    }
}

/// A shard PUT to a real node under the wrong hash is rejected by the
/// receiver, so a corrupted transfer can never become a stored bad shard.
#[tokio::test]
async fn real_nodes_reject_shards_that_do_not_match_their_hash() {
    use axum::Router;
    use c0mpute_gateway::storage_api::{self, StorageApiState};
    use c0mpute_placement::HttpTransport;

    let storage = local_storage("http-reject").await;
    let app: Router = storage_api::router(StorageApiState::local(storage.clone()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let peer = PeerInfo {
        peer_id: "n".into(),
        endpoint: format!("http://{addr}"),
        reputation: 1.0,
        uptime_30d: 1.0,
        free_bytes: 1 << 30,
        rtt_ms: 1,
        asn: Some(1),
        region: None,
        ip_prefix: None,
    };

    let transport = HttpTransport::default();
    let bytes = b"the real bytes".to_vec();
    let wrong = Hash::of(b"a different shard");

    let err = transport
        .put_shard(&peer, &wrong, &bytes)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("rejected shard"), "unexpected: {err}");
    assert!(!storage.chunk_store().has(&wrong).await);

    // The honest write works.
    let right = Hash::of(&bytes);
    transport.put_shard(&peer, &right, &bytes).await.unwrap();
    assert!(storage.chunk_store().has(&right).await);
    assert_eq!(transport.get_shard(&peer, &right).await.unwrap(), bytes);
}

#[tokio::test]
async fn placement_error_types_are_distinguishable() {
    // Callers (the CLI, and CIP-005's repair loop) need to tell "grow the
    // network" apart from "this network can never be diverse enough".
    let policy = PlacementPolicy::for_parity(4);
    let few = healthy_peers(3);
    assert!(matches!(
        c0mpute_placement::select(&few, 14, 1024, &policy).unwrap_err(),
        PlacementError::InsufficientPeers { .. }
    ));

    let mut same_domain = healthy_peers(20);
    for p in same_domain.iter_mut() {
        p.asn = Some(42);
    }
    assert!(matches!(
        c0mpute_placement::select(&same_domain, 14, 1024, &policy).unwrap_err(),
        PlacementError::DiversityUnsatisfiable { .. }
    ));
}
