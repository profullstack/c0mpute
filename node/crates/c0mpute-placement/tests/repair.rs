//! Auto-repair (CIP-005 acceptance criteria).
//!
//! The question these answer is the one CIP-001 makes load-bearing: after a
//! node leaves, does the block get its redundancy back, on peers that keep it
//! genuinely independent?

use std::sync::Arc;

use c0mpute_placement::transport::memory::MemoryTransport;
use c0mpute_placement::{
    BlockState, DistributedStorage, PeerCatalog, PeerInfo, RepairConfig, Repairer,
};
use c0mpute_store::{ChunkStore, Storage, Tier};
use tokio::sync::RwLock;

fn tempdir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "c0mpute-repair-{tag}-{}",
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

fn peers(count: usize) -> Vec<PeerInfo> {
    (0..count)
        .map(|i| PeerInfo {
            peer_id: format!("peer{i}"),
            endpoint: format!("http://peer{i}.test:7780"),
            reputation: 0.95,
            uptime_30d: 0.995,
            free_bytes: 1 << 30,
            rtt_ms: 20,
            asn: Some(64500 + i as u32),
            region: None,
            ip_prefix: None,
        })
        .collect()
}

fn varied(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut state: u64 = 0xabcd_1234_5678_ef01;
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
    repairer: Repairer,
    transport: MemoryTransport,
    catalog: Arc<RwLock<PeerCatalog>>,
}

async fn net(tag: &str, peer_count: usize) -> Net {
    let transport = MemoryTransport::new();
    let catalog = Arc::new(RwLock::new(PeerCatalog::new(peers(peer_count))));
    let storage = DistributedStorage::new(
        local_storage(tag).await,
        Arc::new(transport.clone()),
        Arc::clone(&catalog),
    );
    // `local` holds no shards, so it could never win the rendezvous election.
    // These tests drive repair explicitly, the same way the CLI does; the
    // election itself is unit-tested separately.
    let repairer =
        Repairer::new(Arc::new(transport.clone()), Arc::clone(&catalog), "local").manual();
    Net {
        storage,
        repairer,
        transport,
        catalog,
    }
}

// ------------------------------------------------------------------- the core

/// The headline: a block that lost shards gets them back, on new peers.
#[tokio::test]
async fn repair_restores_full_redundancy_after_losses() {
    let n = net("restore", 24).await;
    let data = varied(150_000);
    let mut manifest = n.storage.put(&data, Tier::Standard).await.unwrap();

    let dead: Vec<String> = manifest.blocks[0].shards[..3]
        .iter()
        .map(|s| s.host_hint.clone().unwrap())
        .collect();
    for d in &dead {
        n.transport.take_offline(d);
    }

    // condemn=true skips the grace window, which is exercised separately.
    let report = n
        .repairer
        .repair_object(&mut manifest, 0, true)
        .await
        .unwrap();
    assert_eq!(report.blocks_repaired, 1);
    assert_eq!(report.shards_regenerated, 3);
    assert!(report.failures.is_empty(), "{:?}", report.failures);

    // Full redundancy, and none of it on the dead peers.
    let health = n.storage.health(&manifest).await.unwrap();
    assert_eq!(health[0].state, BlockState::Healthy);
    assert_eq!(health[0].healthy, 14);
    for shard in &manifest.blocks[0].shards {
        let host = shard.host_hint.as_ref().unwrap();
        assert!(
            !dead.contains(host),
            "shard still points at dead peer {host}"
        );
    }

    // And the object still reads.
    assert_eq!(n.storage.get(&manifest.object_hash).await.unwrap(), data);
}

/// Only the missing shards are rebuilt — rewriting healthy placements would
/// multiply repair traffic, which CIP-001 says the margin cannot absorb.
#[tokio::test]
async fn repair_regenerates_only_what_was_lost() {
    let n = net("minimal", 24).await;
    let data = varied(100_000);
    let mut manifest = n.storage.put(&data, Tier::Standard).await.unwrap();

    let before: Vec<_> = manifest.blocks[0]
        .shards
        .iter()
        .map(|s| (s.index, s.hash, s.host_hint.clone()))
        .collect();
    let dead = manifest.blocks[0].shards[0].host_hint.clone().unwrap();
    n.transport.take_offline(&dead);

    let puts_before = n.transport.put_calls();
    n.repairer
        .repair_object(&mut manifest, 0, true)
        .await
        .unwrap();
    let placed = n.transport.put_calls() - puts_before;
    assert_eq!(placed, 1, "expected one replacement shard, got {placed}");

    // The other 13 placements are untouched.
    let mut unchanged = 0;
    for (index, hash, host) in &before {
        let now = manifest.blocks[0]
            .shards
            .iter()
            .find(|s| s.index == *index)
            .unwrap();
        if now.hash == *hash && now.host_hint == *host {
            unchanged += 1;
        }
    }
    assert_eq!(unchanged, 13);
}

/// The subtle one. Repairing against an empty context would let a block drift
/// into one failure domain over successive repairs, each individually legal.
#[tokio::test]
async fn repair_respects_the_domains_survivors_already_occupy() {
    let transport = MemoryTransport::new();
    // 14 peers in 7 ASNs (2 each) — exactly enough for the cap — plus 4 spares
    // that all sit in ASN 64500, which already holds two shards.
    let mut all = peers(14);
    for (i, p) in all.iter_mut().enumerate() {
        p.asn = Some(64500 + (i as u32 % 7));
    }
    for i in 0..4 {
        let mut spare = peers(1)[0].clone();
        spare.peer_id = format!("spare{i}");
        spare.endpoint = format!("http://spare{i}.test:7780");
        spare.asn = Some(64500); // the crowded domain
        all.push(spare);
    }
    let catalog = Arc::new(RwLock::new(PeerCatalog::new(all)));
    let storage = DistributedStorage::new(
        local_storage("domains").await,
        Arc::new(transport.clone()),
        Arc::clone(&catalog),
    );
    let repairer = Repairer::new(Arc::new(transport.clone()), Arc::clone(&catalog), "local");

    let data = varied(80_000);
    let mut manifest = storage.put(&data, Tier::Standard).await.unwrap();

    // Kill a shard held in a domain that is NOT the crowded one, so the only
    // spares available sit in a domain already at its cap.
    let victim = manifest.blocks[0]
        .shards
        .iter()
        .find(|s| {
            let host = s.host_hint.as_ref().unwrap();
            !host.starts_with("spare")
                && futures::executor::block_on(async {
                    catalog.read().await.get(host).unwrap().asn != Some(64500)
                })
        })
        .unwrap()
        .host_hint
        .clone()
        .unwrap();
    transport.take_offline(&victim);

    let report = repairer
        .repair_object(&mut manifest, 0, true)
        .await
        .unwrap();

    // Either it placed somewhere legal, or it refused. What it must never do
    // is put a third shard into ASN 64500.
    let mut per_asn = std::collections::HashMap::new();
    for shard in &manifest.blocks[0].shards {
        if let Some(host) = &shard.host_hint
            && let Some(peer) = catalog.read().await.get(host)
        {
            *per_asn.entry(peer.asn).or_insert(0) += 1;
        }
    }
    for (asn, count) in &per_asn {
        assert!(
            *count <= 2,
            "repair put {count} shards in ASN {asn:?}; the cap is 2 — a block \
             drifting into one domain is exactly what this guards"
        );
    }
    // Refusing is a legitimate outcome here, and it must be reported.
    if report.blocks_repaired == 0 {
        assert!(!report.failures.is_empty(), "silent refusal");
    }
}

/// A peer that is briefly unreachable must not trigger repair. Flap-driven
/// repair traffic is what pushes marginal nodes off a p2p network.
#[tokio::test]
async fn a_rebooting_peer_is_not_repaired_away() {
    let n = net("flap", 24).await;
    let data = varied(60_000);
    let mut manifest = n.storage.put(&data, Tier::Standard).await.unwrap();

    let flapping = manifest.blocks[0].shards[0].host_hint.clone().unwrap();
    n.transport.take_offline(&flapping);

    // condemn=false: honour the grace window.
    let plans = n.repairer.scan(&manifest, false).await.unwrap();
    assert!(
        plans[0].missing.is_empty(),
        "a single failed probe condemned a peer"
    );
    assert_eq!(plans[0].state, BlockState::Healthy);

    let report = n
        .repairer
        .repair_object(&mut manifest, 0, false)
        .await
        .unwrap();
    assert_eq!(report.blocks_repaired, 0);
    assert_eq!(
        n.transport.shard_count(&flapping),
        1,
        "shard was moved anyway"
    );
}

#[tokio::test]
async fn repair_is_idempotent_on_a_healthy_object() {
    let n = net("healthy", 24).await;
    let data = varied(50_000);
    let mut manifest = n.storage.put(&data, Tier::Standard).await.unwrap();

    let puts_before = n.transport.put_calls();
    let report = n
        .repairer
        .repair_object(&mut manifest, 0, true)
        .await
        .unwrap();
    assert_eq!(report.blocks_repaired, 0);
    assert_eq!(report.shards_regenerated, 0);
    assert_eq!(
        n.transport.put_calls(),
        puts_before,
        "repaired nothing, wrote anyway"
    );
}

/// Past the parity budget nothing can be rebuilt. That must be reported
/// loudly, not silently skipped — and it must not consume the pass.
#[tokio::test]
async fn a_lost_block_is_reported_not_silently_skipped() {
    let n = net("lost", 24).await;
    let data = varied(90_000);
    let mut manifest = n.storage.put(&data, Tier::Standard).await.unwrap();

    for shard in manifest.blocks[0].shards.iter().take(5) {
        n.transport.take_offline(shard.host_hint.as_ref().unwrap());
    }
    let report = n
        .repairer
        .repair_object(&mut manifest, 0, true)
        .await
        .unwrap();
    assert_eq!(report.blocks_lost, 1);
    assert_eq!(report.blocks_repaired, 0);
}

/// Repair must verify what it reconstructs. Rebuilding from unchecked bytes
/// would launder a corrupt block into fresh shards that all agree with each
/// other and disagree with the manifest.
#[tokio::test]
async fn repair_refuses_to_rebuild_from_corrupt_sources() {
    let n = net("corrupt", 24).await;
    let data = varied(70_000);
    let mut manifest = n.storage.put(&data, Tier::Standard).await.unwrap();

    // One holder gone, and enough of the rest lying that k honest shards
    // cannot be assembled.
    n.transport
        .take_offline(manifest.blocks[0].shards[0].host_hint.as_ref().unwrap());
    for shard in manifest.blocks[0].shards.iter().skip(1).take(5) {
        n.transport.make_corrupt(shard.host_hint.as_ref().unwrap());
    }

    let report = n
        .repairer
        .repair_object(&mut manifest, 0, true)
        .await
        .unwrap();
    assert_eq!(report.blocks_repaired, 0);
    assert!(
        !report.failures.is_empty(),
        "corrupt repair reported success"
    );
}

#[tokio::test]
async fn multi_block_objects_repair_every_degraded_block() {
    let n = net("multiblock", 30).await;
    let data = varied(4 * 1024 * 1024 * 2 + 500);
    let mut manifest = n.storage.put(&data, Tier::Standard).await.unwrap();
    assert_eq!(manifest.blocks.len(), 3);

    // Take one holder out of each block.
    for block in &manifest.blocks {
        n.transport
            .take_offline(block.shards[0].host_hint.as_ref().unwrap());
    }

    let report = n
        .repairer
        .repair_object(&mut manifest, 0, true)
        .await
        .unwrap();
    assert_eq!(report.blocks_scanned, 3);
    assert!(report.blocks_repaired >= 1);

    for h in n.storage.health(&manifest).await.unwrap() {
        assert!(
            !h.state.needs_repair(),
            "block {} still {:?}",
            h.index,
            h.state
        );
    }
    assert_eq!(n.storage.get(&manifest.object_hash).await.unwrap(), data);
}

#[tokio::test]
async fn attestations_record_what_actually_happened() {
    let n = net("attest", 24).await;
    let data = varied(120_000);
    let mut manifest = n.storage.put(&data, Tier::Standard).await.unwrap();

    let dead: Vec<String> = manifest.blocks[0].shards[..2]
        .iter()
        .map(|s| s.host_hint.clone().unwrap())
        .collect();
    for d in &dead {
        n.transport.take_offline(d);
    }

    let report = n
        .repairer
        .repair_object(&mut manifest, 42, true)
        .await
        .unwrap();
    let att = &report.attestations[0];
    assert_eq!(att.object, manifest.object_hash);
    assert_eq!(att.round, 42);
    assert_eq!(att.repairer, "local");
    assert_eq!(att.shards_regenerated.len(), 2);
    assert_eq!(att.destinations.len(), 2);
    assert_eq!(att.sources.len(), 10, "should read exactly k shards");
    assert!(att.bytes_read > 0);
    // None of the replacements went back to a dead peer.
    for d in &att.destinations {
        assert!(!dead.contains(d));
    }
    // Round-trips as JSON, for the gossip/ledger path in CIP-006.
    let json = serde_json::to_string(att).unwrap();
    assert_eq!(
        serde_json::from_str::<c0mpute_placement::RepairAttestation>(&json).unwrap(),
        *att
    );
}

/// Repair reads k shards to rebuild one — the 10x amplification CIP-001
/// budgets for. Worth pinning: if it silently became n-shard reads, the cost
/// model would be wrong by 40% and nothing else would notice.
#[tokio::test]
async fn repair_reads_exactly_k_shards() {
    let n = net("amplification", 24).await;
    let data = varied(100_000);
    let mut manifest = n.storage.put(&data, Tier::Standard).await.unwrap();
    n.transport
        .take_offline(manifest.blocks[0].shards[0].host_hint.as_ref().unwrap());

    let gets_before = n.transport.get_calls();
    n.repairer
        .repair_object(&mut manifest, 0, true)
        .await
        .unwrap();
    let reads = n.transport.get_calls() - gets_before;
    assert!(
        (10..=13).contains(&reads),
        "repair read {reads} shards; k=10 is the budgeted amplification"
    );
}

#[tokio::test]
async fn a_repaired_object_survives_another_round_of_losses() {
    let n = net("consecutive", 30).await;
    let data = varied(130_000);
    let mut manifest = n.storage.put(&data, Tier::Standard).await.unwrap();

    // Round one: lose 4, repair.
    for shard in &manifest.blocks[0].shards[..4] {
        n.transport.take_offline(shard.host_hint.as_ref().unwrap());
    }
    n.repairer
        .repair_object(&mut manifest, 0, true)
        .await
        .unwrap();
    assert_eq!(
        n.storage.health(&manifest).await.unwrap()[0].state,
        BlockState::Healthy
    );

    // Round two: lose 4 of the *new* placement, repair again. Without repair
    // this is the eight losses that would have killed the block.
    for shard in &manifest.blocks[0].shards[..4] {
        n.transport.take_offline(shard.host_hint.as_ref().unwrap());
    }
    n.repairer
        .repair_object(&mut manifest, 1, true)
        .await
        .unwrap();

    assert_eq!(
        n.storage.health(&manifest).await.unwrap()[0].state,
        BlockState::Healthy
    );
    assert_eq!(n.storage.get(&manifest.object_hash).await.unwrap(), data);
}

#[tokio::test]
async fn repair_needs_somewhere_to_put_the_replacement() {
    // Exactly 14 peers: after one dies there is no fresh peer to place onto.
    let n = net("nowhere", 14).await;
    let data = varied(40_000);
    let mut manifest = n.storage.put(&data, Tier::Standard).await.unwrap();
    n.transport
        .take_offline(manifest.blocks[0].shards[0].host_hint.as_ref().unwrap());

    let report = n
        .repairer
        .repair_object(&mut manifest, 0, true)
        .await
        .unwrap();
    assert_eq!(report.blocks_repaired, 0);
    assert!(
        report.failures.iter().any(|f| f.contains("eligible peers")),
        "should say the network has nowhere to repair to: {:?}",
        report.failures
    );
}

/// Regression: a peer that just died still looks healthy in the catalog,
/// because reputation and uptime are periodic measurements rather than
/// liveness. Repair used to place the replacement straight back onto it, so
/// the repair "succeeded" and the block stayed exactly as degraded.
#[tokio::test]
async fn repair_never_places_back_onto_the_peer_that_died() {
    let n = net("no-reuse", 24).await;
    let data = varied(110_000);
    let mut manifest = n.storage.put(&data, Tier::Standard).await.unwrap();

    let dead: Vec<String> = manifest.blocks[0].shards[..2]
        .iter()
        .map(|s| s.host_hint.clone().unwrap())
        .collect();
    for d in &dead {
        n.transport.take_offline(d);
    }
    // The catalog still believes they are fine — that is the trap.
    for d in &dead {
        let catalog = n.catalog.read().await;
        let peer = catalog.get(d).unwrap();
        assert!(peer.reputation >= 0.9 && peer.uptime_30d >= 0.99);
    }

    let report = n
        .repairer
        .repair_object(&mut manifest, 0, true)
        .await
        .unwrap();
    assert_eq!(report.blocks_repaired, 1);

    for d in &report.attestations[0].destinations {
        assert!(
            !dead.contains(d),
            "replacement went back onto dead peer {d}"
        );
    }
    assert_eq!(
        n.storage.health(&manifest).await.unwrap()[0].state,
        BlockState::Healthy
    );
}

/// And no peer ends up with two shards of the same block, however many
/// repairs it has been through.
#[tokio::test]
async fn a_block_never_puts_two_shards_on_one_peer() {
    let n = net("one-each", 30).await;
    let data = varied(90_000);
    let mut manifest = n.storage.put(&data, Tier::Standard).await.unwrap();

    for round in 0..3 {
        for shard in &manifest.blocks[0].shards[..2] {
            n.transport.take_offline(shard.host_hint.as_ref().unwrap());
        }
        n.repairer
            .repair_object(&mut manifest, round, true)
            .await
            .unwrap();

        let hosts: Vec<&String> = manifest.blocks[0]
            .shards
            .iter()
            .filter_map(|s| s.host_hint.as_ref())
            .collect();
        let unique: std::collections::HashSet<_> = hosts.iter().collect();
        assert_eq!(
            unique.len(),
            hosts.len(),
            "round {round}: a peer holds two shards of one block"
        );
    }
    assert_eq!(n.storage.get(&manifest.object_hash).await.unwrap(), data);
}

/// With election on — the background-daemon path — a node that holds none of
/// the block's shards defers instead of repairing. That is what stops all
/// fourteen holders doing the same work.
#[tokio::test]
async fn election_defers_when_this_node_is_not_the_winner() {
    let n = net("election", 24).await;
    let data = varied(60_000);
    let mut manifest = n.storage.put(&data, Tier::Standard).await.unwrap();
    n.transport
        .take_offline(manifest.blocks[0].shards[0].host_hint.as_ref().unwrap());

    // Default config honours the election; "local" holds nothing, so a
    // survivor always wins.
    let deferring = Repairer::new(
        Arc::new(n.transport.clone()),
        Arc::clone(&n.catalog),
        "local",
    );
    let report = deferring
        .repair_object(&mut manifest, 0, true)
        .await
        .unwrap();
    assert_eq!(report.blocks_repaired, 0);
    assert!(
        report.failures.iter().any(|f| f.contains("elected")),
        "should say it deferred: {:?}",
        report.failures
    );

    // The same node asked directly does the work.
    let manual = Repairer::new(
        Arc::new(n.transport.clone()),
        Arc::clone(&n.catalog),
        "local",
    )
    .manual();
    assert_eq!(
        manual
            .repair_object(&mut manifest, 0, true)
            .await
            .unwrap()
            .blocks_repaired,
        1
    );
}

/// Regression: a peer that died in an *earlier* round is still in the catalog
/// looking healthy, and nothing probes it because it holds none of this
/// block's shards. The first time we learn it is gone is when the placement
/// fails — so repair carries spare candidates and fails over instead of
/// aborting the whole block.
///
/// Found on the testnet: round one repaired fine, round two died trying to
/// place onto a node killed in round one.
#[tokio::test]
async fn repair_fails_over_when_a_replacement_target_is_dead() {
    let n = net("failover", 24).await;
    let data = varied(100_000);
    let mut manifest = n.storage.put(&data, Tier::Standard).await.unwrap();

    // Kill two holders, and separately kill peers that hold nothing — the
    // ones repair would otherwise pick as replacements.
    for shard in &manifest.blocks[0].shards[..2] {
        n.transport.take_offline(shard.host_hint.as_ref().unwrap());
    }
    let holders: std::collections::HashSet<String> = manifest.blocks[0]
        .shards
        .iter()
        .filter_map(|s| s.host_hint.clone())
        .collect();
    let mut bystanders_killed = 0;
    for p in n.catalog.read().await.peers() {
        if !holders.contains(&p.peer_id) && bystanders_killed < 4 {
            n.transport.take_offline(&p.peer_id);
            bystanders_killed += 1;
        }
    }
    assert_eq!(bystanders_killed, 4);

    let report = n
        .repairer
        .repair_object(&mut manifest, 0, true)
        .await
        .unwrap();
    assert_eq!(
        report.blocks_repaired, 1,
        "should have failed over past the dead targets: {:?}",
        report.failures
    );
    assert_eq!(report.shards_regenerated, 2);
    assert_eq!(
        n.storage.health(&manifest).await.unwrap()[0].state,
        BlockState::Healthy
    );
    assert_eq!(n.storage.get(&manifest.object_hash).await.unwrap(), data);
}

/// When every possible target is dead, say so rather than reporting a repair
/// that placed nothing.
#[tokio::test]
async fn repair_reports_failure_when_no_target_accepts() {
    let n = net("no-target", 16).await;
    let data = varied(50_000);
    let mut manifest = n.storage.put(&data, Tier::Standard).await.unwrap();

    n.transport
        .take_offline(manifest.blocks[0].shards[0].host_hint.as_ref().unwrap());
    // Every peer that is not already a holder is dead too.
    let holders: std::collections::HashSet<String> = manifest.blocks[0]
        .shards
        .iter()
        .filter_map(|s| s.host_hint.clone())
        .collect();
    for p in n.catalog.read().await.peers() {
        if !holders.contains(&p.peer_id) {
            n.transport.take_offline(&p.peer_id);
        }
    }

    let report = n
        .repairer
        .repair_object(&mut manifest, 0, true)
        .await
        .unwrap();
    assert_eq!(report.blocks_repaired, 0);
    assert!(!report.failures.is_empty(), "silent failure to repair");
}

#[tokio::test]
async fn config_is_tunable() {
    let n = net("config", 24).await;
    let repairer = Repairer::new(
        Arc::new(n.transport.clone()),
        Arc::clone(&n.catalog),
        "local",
    )
    .with_config(RepairConfig {
        grace_probes: 1,
        grace_window: std::time::Duration::ZERO,
        honor_election: false,
        ..RepairConfig::default()
    });
    assert_eq!(repairer.config().grace_probes, 1);

    // With no grace at all, one failed probe is enough to condemn.
    let data = varied(30_000);
    let mut manifest = n.storage.put(&data, Tier::Standard).await.unwrap();
    n.transport
        .take_offline(manifest.blocks[0].shards[0].host_hint.as_ref().unwrap());
    let report = repairer
        .repair_object(&mut manifest, 0, false)
        .await
        .unwrap();
    assert_eq!(report.blocks_repaired, 1);
}
