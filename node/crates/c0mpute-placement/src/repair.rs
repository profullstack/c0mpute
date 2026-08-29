//! Auto-repair (CIP-005).
//!
//! CIP-001 bought c0mpute's cost advantage by spending the durability margin
//! Storj keeps: RS 10/14 tolerates four losses where RS 29/80 tolerates
//! fifty-one. That trade is only defensible if lost shards come back quickly,
//! which makes this module load-bearing rather than a follow-up — without it,
//! every object in the network trends toward unrecoverable on a schedule set
//! by node churn.
//!
//! Three things have to be right:
//!
//!   1. **Do not repair a node that is merely rebooting.** Consumer nodes flap.
//!      Treating a two-minute outage as data loss produces repair traffic
//!      proportional to flapping rather than to real churn, and that traffic is
//!      what pushes marginal nodes off the network — the reflexive failure
//!      that kills p2p storage networks. [`FailureTracker`] holds the line.
//!   2. **Do not have fourteen nodes repair the same block.** Elected by
//!      rendezvous hashing, so every holder computes the same answer with no
//!      coordination (DIP-0011).
//!   3. **Do not let repair concentrate a block.** Replacements are selected
//!      against the domains the survivors already occupy — see
//!      [`crate::select::PlacementContext`].

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use c0mpute_proto::Hash;
use c0mpute_store::erasure::{self, Shard};
use c0mpute_store::{BlockEntry, ObjectManifest, ShardEntry};
use futures::stream::{FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::distributed::BlockState;
use crate::peer::{PeerCatalog, PeerInfo};
use crate::select::{PlacementContext, PlacementPolicy, select_peers};
use crate::transport::ShardTransport;

/// How repair behaves.
#[derive(Clone, Debug)]
pub struct RepairConfig {
    /// Consecutive failed probes before a peer's shards are presumed lost.
    pub grace_probes: u32,
    /// ...spread over at least this long. Both must be satisfied, so a burst
    /// of six failures in one second does not condemn a peer.
    pub grace_window: Duration,
    /// Fraction of the network's blocks that may be degraded before repair is
    /// treated as a storm and rate-limited to priority order.
    pub storm_threshold: f32,
    /// Most blocks repaired in one pass.
    pub max_blocks_per_pass: usize,
    /// Defer to the rendezvous election before repairing.
    ///
    /// True for the background daemon, where every holder scans the same
    /// blocks and exactly one should act. False for an explicit
    /// `c0mpute storage repair`, where an operator — who is usually not one of
    /// the shard holders, and so could never win the election — has asked for
    /// the work directly. Election exists to stop fourteen nodes doing the
    /// same job, not to stop anyone doing it.
    pub honor_election: bool,
}

impl Default for RepairConfig {
    fn default() -> Self {
        Self {
            grace_probes: 6,
            grace_window: Duration::from_secs(2 * 60 * 60),
            storm_threshold: 0.05,
            max_blocks_per_pass: 64,
            honor_election: true,
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ------------------------------------------------------------------ election

/// Which holder should repair this block in this round.
///
/// Rendezvous hashing: every holder computes the same winner from the same
/// inputs, with no messages exchanged. If the winner is itself gone, the next
/// round picks someone else — no leader, no lease, no consensus.
///
/// Returns `None` when there are no candidates.
pub fn elect_repairer<'a>(
    block_hash: &Hash,
    round: u64,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    candidates
        .into_iter()
        .map(|peer_id| {
            let mut h = blake3::Hasher::new();
            h.update(block_hash.0.as_slice());
            h.update(&round.to_be_bytes());
            h.update(peer_id.as_bytes());
            (*h.finalize().as_bytes(), peer_id.to_string())
        })
        .min()
        .map(|(_, peer_id)| peer_id)
}

// ------------------------------------------------------------ flap tolerance

/// Distinguishes "unreachable right now" from "gone".
///
/// A shard is only presumed lost after `grace_probes` failures spread over at
/// least `grace_window`. Both conditions matter: the count alone would condemn
/// a peer from a burst of probes seconds apart, and the window alone would
/// condemn one from a single failure two hours ago.
#[derive(Debug, Default)]
pub struct FailureTracker {
    /// peer_id -> (consecutive failures, unix-ms of the first of them)
    failures: HashMap<String, (u32, u64)>,
}

impl FailureTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_reachable(&mut self, peer_id: &str) {
        // A single success clears the streak. Repairing a peer that just came
        // back is pure waste.
        self.failures.remove(peer_id);
    }

    pub fn record_unreachable_at(&mut self, peer_id: &str, at_ms: u64) {
        let e = self
            .failures
            .entry(peer_id.to_string())
            .or_insert((0, at_ms));
        e.0 += 1;
    }

    pub fn record_unreachable(&mut self, peer_id: &str) {
        self.record_unreachable_at(peer_id, now_ms());
    }

    pub fn presumed_gone_at(&self, peer_id: &str, cfg: &RepairConfig, now: u64) -> bool {
        match self.failures.get(peer_id) {
            None => false,
            Some((count, first)) => {
                *count >= cfg.grace_probes
                    && now.saturating_sub(*first) >= cfg.grace_window.as_millis() as u64
            }
        }
    }

    pub fn presumed_gone(&self, peer_id: &str, cfg: &RepairConfig) -> bool {
        self.presumed_gone_at(peer_id, cfg, now_ms())
    }

    pub fn consecutive_failures(&self, peer_id: &str) -> u32 {
        self.failures.get(peer_id).map(|(c, _)| *c).unwrap_or(0)
    }
}

// -------------------------------------------------------------- attestations

/// Proof that a repair happened.
///
/// Unsigned here: signing keys are CoinPay DIDs, which arrive with CIP-006
/// along with the reputation and payout plumbing these feed. Recorded now so
/// the shape is fixed and repairs are attributable from the first pass rather
/// than retrofitted.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepairAttestation {
    pub object: Hash,
    pub block: u32,
    pub block_hash: Hash,
    pub repairer: String,
    pub round: u64,
    pub shards_regenerated: Vec<u8>,
    pub sources: Vec<String>,
    pub destinations: Vec<String>,
    pub bytes_read: u64,
    pub completed_at_ms: u64,
}

// --------------------------------------------------------------------- plans

/// What one degraded block needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepairPlan {
    pub object: Hash,
    pub block: u32,
    pub state: BlockState,
    /// Shard indices to regenerate.
    pub missing: Vec<u8>,
    /// Peers still holding a shard of this block.
    pub survivors: Vec<String>,
}

impl RepairPlan {
    /// Order repairs by how close the block is to death.
    ///
    /// `Lost` sorts last despite being worst: it cannot be repaired, so
    /// spending a pass on it starves blocks that can still be saved.
    pub fn priority(&self) -> u8 {
        match self.state {
            BlockState::Critical => 0,
            BlockState::Urgent => 1,
            BlockState::Degraded => 2,
            BlockState::Healthy => 3,
            BlockState::Lost => 4,
        }
    }

    pub fn repairable(&self) -> bool {
        !matches!(self.state, BlockState::Healthy | BlockState::Lost)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepairReport {
    pub blocks_scanned: usize,
    pub blocks_repaired: usize,
    pub shards_regenerated: usize,
    pub blocks_lost: usize,
    pub blocks_skipped_storm: usize,
    pub attestations: Vec<RepairAttestation>,
    pub failures: Vec<String>,
}

// ------------------------------------------------------------------ repairer

pub struct Repairer {
    transport: Arc<dyn ShardTransport>,
    catalog: Arc<RwLock<PeerCatalog>>,
    config: RepairConfig,
    tracker: RwLock<FailureTracker>,
    /// This node's identity, for attestations and rendezvous election.
    local_peer_id: String,
}

impl Repairer {
    pub fn new(
        transport: Arc<dyn ShardTransport>,
        catalog: Arc<RwLock<PeerCatalog>>,
        local_peer_id: impl Into<String>,
    ) -> Self {
        Self {
            transport,
            catalog,
            config: RepairConfig::default(),
            tracker: RwLock::new(FailureTracker::new()),
            local_peer_id: local_peer_id.into(),
        }
    }

    pub fn with_config(mut self, config: RepairConfig) -> Self {
        self.config = config;
        self
    }

    pub fn config(&self) -> &RepairConfig {
        &self.config
    }

    /// Repair on request rather than on election. For `c0mpute storage repair`
    /// and for tests, where the caller is not one of the shard holders.
    pub fn manual(mut self) -> Self {
        self.config.honor_election = false;
        self
    }

    /// Probe every shard of an object and produce a plan per block.
    ///
    /// `condemn` decides whether an unreachable peer counts as gone. Pass
    /// `false` (the default path) to respect the grace window; a plan built
    /// mid-flap will list nothing as missing, which is the intended answer.
    pub async fn scan(&self, manifest: &ObjectManifest, condemn: bool) -> Result<Vec<RepairPlan>> {
        let catalog = self.catalog.read().await;
        let mut plans = Vec::with_capacity(manifest.blocks.len());
        let now = now_ms();

        for block in &manifest.blocks {
            let mut survivors = Vec::new();
            let mut missing = Vec::new();

            for shard in &block.shards {
                let Some(hint) = shard.host_hint.as_ref() else {
                    // Local-only shard from a single-node write; nothing to
                    // probe and nothing this loop can repair.
                    survivors.push(String::from("<local>"));
                    continue;
                };
                let Some(peer) = catalog.get(hint) else {
                    // Not in the catalog at all: treat as gone, since we have
                    // no way to reach it. CIP-003's DHT fallback would resolve
                    // this properly.
                    missing.push(shard.index);
                    continue;
                };

                let held = self
                    .transport
                    .has_shards(peer, &[shard.hash])
                    .await
                    .map(|v| v.first().copied().unwrap_or(false))
                    .unwrap_or(false);

                {
                    let mut tracker = self.tracker.write().await;
                    if held {
                        tracker.record_reachable(hint);
                    } else {
                        tracker.record_unreachable_at(hint, now);
                    }
                }

                if held {
                    survivors.push(hint.clone());
                } else {
                    let gone = condemn
                        || self
                            .tracker
                            .read()
                            .await
                            .presumed_gone_at(hint, &self.config, now);
                    if gone {
                        missing.push(shard.index);
                    } else {
                        // Unreachable but within its grace window. Counted as
                        // present on purpose: repairing a rebooting node is
                        // how a flap becomes a storm.
                        debug!(
                            peer = %hint,
                            block = block.index,
                            "unreachable but inside the grace window; not condemning"
                        );
                        survivors.push(hint.clone());
                    }
                }
            }

            let healthy = block.shards.len() - missing.len();
            plans.push(RepairPlan {
                object: manifest.object_hash,
                block: block.index,
                state: BlockState::classify(healthy, manifest.k as usize, manifest.parity as usize),
                missing,
                survivors,
            });
        }
        Ok(plans)
    }

    /// Should this node perform the repair for `plan` in `round`?
    pub fn elected(&self, plan: &RepairPlan, block_hash: &Hash, round: u64) -> bool {
        let candidates: Vec<&str> = plan
            .survivors
            .iter()
            .filter(|s| s.as_str() != "<local>")
            .map(String::as_str)
            .collect();
        // With no reachable holders there is nobody to elect; whoever noticed
        // takes it.
        if candidates.is_empty() {
            return true;
        }
        elect_repairer(block_hash, round, candidates)
            .map(|winner| winner == self.local_peer_id)
            .unwrap_or(true)
    }

    /// Repair every degraded block of an object.
    ///
    /// Returns a report rather than failing on the first problem: one
    /// unrepairable block must not stop the others from being saved.
    pub async fn repair_object(
        &self,
        manifest: &mut ObjectManifest,
        round: u64,
        condemn: bool,
    ) -> Result<RepairReport> {
        let mut plans = self.scan(manifest, condemn).await?;
        let mut report = RepairReport {
            blocks_scanned: plans.len(),
            ..Default::default()
        };

        // Worst-but-still-savable first.
        plans.sort_by_key(|p| (p.priority(), p.block));

        let degraded = plans.iter().filter(|p| p.repairable()).count();
        let storm = !plans.is_empty()
            && (degraded as f32 / plans.len() as f32) > self.config.storm_threshold;
        if storm {
            warn!(
                degraded,
                total = plans.len(),
                "repair storm: proceeding in strict priority order under a cap"
            );
        }

        let mut budget = self.config.max_blocks_per_pass;
        for plan in plans {
            if matches!(plan.state, BlockState::Lost) {
                report.blocks_lost += 1;
                warn!(
                    object = %plan.object, block = plan.block,
                    "block is LOST — fewer than k shards remain; repair cannot help"
                );
                continue;
            }
            if !plan.repairable() {
                continue;
            }
            if budget == 0 {
                report.blocks_skipped_storm += 1;
                continue;
            }
            budget -= 1;

            match self.repair_block(manifest, &plan, round).await {
                Ok(att) => {
                    report.shards_regenerated += att.shards_regenerated.len();
                    report.blocks_repaired += 1;
                    report.attestations.push(att);
                }
                Err(e) => {
                    warn!(object = %plan.object, block = plan.block, err = %format!("{e:#}"),
                          "block repair failed");
                    report.failures.push(format!("block {}: {e:#}", plan.block));
                }
            }
        }
        Ok(report)
    }

    /// Rebuild one block's missing shards onto fresh peers.
    pub async fn repair_block(
        &self,
        manifest: &mut ObjectManifest,
        plan: &RepairPlan,
        round: u64,
    ) -> Result<RepairAttestation> {
        let k = manifest.k as usize;
        let parity = manifest.parity as usize;
        let block_pos = manifest
            .blocks
            .iter()
            .position(|b| b.index == plan.block)
            .ok_or_else(|| anyhow!("block {} not in manifest", plan.block))?;
        let block: BlockEntry = manifest.blocks[block_pos].clone();

        if self.config.honor_election && !self.elected(plan, &block.hash, round) {
            bail!(
                "another holder is elected to repair block {} this round",
                plan.block
            );
        }

        // 1. Fetch k surviving shards.
        let (shards, sources, bytes_read) = self.fetch_k(&block, k, plan).await?;

        // 2. Reconstruct and verify. Repairing from bytes we have not checked
        //    would launder a corrupt block into fresh shards that all agree.
        let mut plaintext = erasure::decode(shards, k, parity, block.len as usize)?;
        plaintext.truncate(block.len as usize);
        let actual = Hash::of(&plaintext);
        if actual != block.hash {
            bail!(
                "refusing to repair block {}: reconstructed bytes hash to {actual}, manifest says {}",
                plan.block,
                block.hash
            );
        }

        // 3. Re-encode. Only the missing shards are kept — regenerating all n
        //    would rewrite healthy placements for nothing.
        let (all_shards, _) = erasure::encode(&plaintext, k, parity)?;

        // 4. Choose replacements, excluding current holders and counting their
        //    domains against the cap.
        let catalog = self.catalog.read().await;
        let holders: Vec<&PeerInfo> = plan
            .survivors
            .iter()
            .filter_map(|id| catalog.get(id))
            .collect();
        // Domains come from the survivors only: the dead shard's domain slot
        // is freed by the very move we are making.
        let mut ctx = PlacementContext::from_holders(holders);

        // Exclusions are broader than the survivors, for two reasons the
        // catalog cannot express on its own.
        //
        // First, every peer this block has ever pointed at — dead ones
        // included. A peer that just vanished still looks healthy in the
        // catalog, because reputation and uptime are periodic measurements,
        // not liveness. Without this, repair cheerfully places the
        // replacement back onto the node that just died.
        for shard in &block.shards {
            if let Some(host) = &shard.host_hint {
                ctx.exclude_peers.insert(host.clone());
            }
        }
        // Second, anything we failed to reach in this scan. It may be inside
        // its grace window and so not yet condemned, but it is plainly a bad
        // place to put a shard we are trying to rescue.
        {
            let tracker = self.tracker.read().await;
            for peer in catalog.peers() {
                if tracker.consecutive_failures(&peer.peer_id) > 0 {
                    ctx.exclude_peers.insert(peer.peer_id.clone());
                }
            }
        }
        let policy = PlacementPolicy::for_parity(parity);
        let shard_bytes = all_shards.first().map(|s| s.bytes.len()).unwrap_or(0) as u64;

        // Ask for spares. A peer that died in an earlier round is still in the
        // catalog looking healthy — reputation and uptime are periodic
        // measurements, and nothing probed it this pass because it holds none
        // of this block's shards. The first time we learn it is gone is when
        // the placement fails, so carry alternatives and fail over.
        //
        // Any subset of a valid selection is itself valid — the per-domain cap
        // is a maximum — so skipping a dead candidate cannot break diversity.
        let wanted = plan.missing.len();
        let with_spares = (wanted * 2 + 2).min(catalog.peers().len());
        let replacements =
            match select_peers(catalog.peers(), with_spares, shard_bytes, &policy, &ctx) {
                Ok(peers) => peers,
                // Not enough for spares; take exactly what is needed.
                Err(_) => select_peers(catalog.peers(), wanted, shard_bytes, &policy, &ctx)?,
            };
        drop(catalog);

        // 5. Place them, moving to the next candidate when one refuses.
        let mut destinations = Vec::new();
        let mut regenerated = Vec::new();
        let mut candidates = replacements.iter();
        let mut place_failures: Vec<String> = Vec::new();

        for shard_index in &plan.missing {
            let shard = all_shards
                .iter()
                .find(|s| s.index == *shard_index)
                .ok_or_else(|| anyhow!("re-encode produced no shard {shard_index}"))?;
            let hash = Hash::of(&shard.bytes);

            let mut placed_on: Option<&PeerInfo> = None;
            for peer in candidates.by_ref() {
                match self.transport.put_shard(peer, &hash, &shard.bytes).await {
                    Ok(()) => {
                        placed_on = Some(peer);
                        break;
                    }
                    Err(e) => {
                        warn!(peer = %peer.peer_id, err = %e,
                              "repair target refused; trying the next candidate");
                        place_failures.push(format!("{}: {e}", peer.peer_id));
                    }
                }
            }
            let Some(peer) = placed_on else {
                break;
            };

            // 6. Point the manifest at the new home.
            let entry = manifest.blocks[block_pos]
                .shards
                .iter_mut()
                .find(|e| e.index == *shard_index);
            match entry {
                Some(e) => {
                    e.hash = hash;
                    e.host_hint = Some(peer.peer_id.clone());
                }
                None => manifest.blocks[block_pos].shards.push(ShardEntry {
                    index: *shard_index,
                    hash,
                    host_hint: Some(peer.peer_id.clone()),
                }),
            }
            destinations.push(peer.peer_id.clone());
            regenerated.push(*shard_index);
        }
        manifest.blocks[block_pos].shards.sort_by_key(|e| e.index);

        if regenerated.is_empty() {
            bail!(
                "block {}: no replacement peer accepted a shard ({} refused: {})",
                plan.block,
                place_failures.len(),
                place_failures.join("; ")
            );
        }
        if regenerated.len() < plan.missing.len() {
            // Partial is still progress — the block is closer to healthy than
            // it was, and the next pass finishes the job. Reported rather than
            // swallowed so a network that is quietly out of room is visible.
            warn!(
                object = %plan.object, block = plan.block,
                regenerated = regenerated.len(), needed = plan.missing.len(),
                "partial repair: ran out of usable peers"
            );
        }

        info!(
            object = %plan.object, block = plan.block,
            regenerated = ?regenerated, destinations = ?destinations,
            "repaired block"
        );

        Ok(RepairAttestation {
            object: plan.object,
            block: plan.block,
            block_hash: block.hash,
            repairer: self.local_peer_id.clone(),
            round,
            shards_regenerated: regenerated,
            sources,
            destinations,
            bytes_read,
            completed_at_ms: now_ms(),
        })
    }

    /// Pull `k` shards from surviving holders, taking whichever answer first.
    async fn fetch_k(
        &self,
        block: &BlockEntry,
        k: usize,
        plan: &RepairPlan,
    ) -> Result<(Vec<Option<Shard>>, Vec<String>, u64)> {
        let catalog = self.catalog.read().await;
        let n = block.shards.len().max(k);
        let mut inflight = FuturesUnordered::new();

        for shard in &block.shards {
            if plan.missing.contains(&shard.index) {
                continue;
            }
            let Some(peer) = shard
                .host_hint
                .as_ref()
                .and_then(|h| catalog.get(h))
                .cloned()
            else {
                continue;
            };
            let transport = Arc::clone(&self.transport);
            let hash = shard.hash;
            let index = shard.index;
            inflight.push(async move {
                let res = transport.get_shard(&peer, &hash).await;
                (index, hash, peer.peer_id, res)
            });
        }
        drop(catalog);

        let mut received: Vec<Option<Shard>> = vec![None; n];
        let mut sources = Vec::new();
        let mut bytes_read = 0u64;
        let mut found = 0usize;

        while let Some((index, hash, peer_id, res)) = inflight.next().await {
            match res {
                Ok(bytes) => {
                    if Hash::of(&bytes) != hash {
                        warn!(peer = %peer_id, "served the wrong bytes during repair");
                        continue;
                    }
                    bytes_read += bytes.len() as u64;
                    received[index as usize] = Some(Shard { index, bytes });
                    sources.push(peer_id);
                    found += 1;
                    if found == k {
                        break;
                    }
                }
                Err(e) => debug!(peer = %peer_id, err = %e, "repair source unavailable"),
            }
        }

        if found < k {
            bail!(
                "cannot repair block {}: need {k} shards to reconstruct, reached {found}",
                plan.block
            );
        }
        Ok((received, sources, bytes_read))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn election_is_deterministic_and_agreed_by_everyone() {
        let block = Hash::of(b"block");
        let holders = ["a", "b", "c", "d", "e"];
        let first = elect_repairer(&block, 7, holders).unwrap();
        // Every holder computes the same winner, in any order.
        let mut reversed: Vec<&str> = holders.to_vec();
        reversed.reverse();
        assert_eq!(elect_repairer(&block, 7, reversed).unwrap(), first);
        assert!(holders.contains(&first.as_str()));
    }

    #[test]
    fn election_moves_on_when_the_round_advances() {
        let block = Hash::of(b"block");
        let holders = ["a", "b", "c", "d", "e", "f", "g", "h"];
        let winners: std::collections::HashSet<String> = (0..40)
            .filter_map(|r| elect_repairer(&block, r, holders))
            .collect();
        // A stuck winner would mean a dead repairer blocks the block forever.
        assert!(winners.len() > 1, "election never rotated: {winners:?}");
    }

    #[test]
    fn election_spreads_work_across_blocks() {
        let holders = ["a", "b", "c", "d", "e"];
        let mut counts: HashMap<String, usize> = HashMap::new();
        for i in 0..500u32 {
            let block = Hash::of(&i.to_be_bytes());
            if let Some(w) = elect_repairer(&block, 0, holders) {
                *counts.entry(w).or_default() += 1;
            }
        }
        assert_eq!(counts.len(), 5, "some holder never repairs anything");
        // Roughly even: no holder doing more than double its share.
        for (peer, n) in &counts {
            assert!(*n < 200, "{peer} elected {n} times of 500");
        }
    }

    #[test]
    fn election_of_nobody_is_none() {
        assert_eq!(elect_repairer(&Hash::of(b"b"), 0, Vec::<&str>::new()), None);
    }

    // ----------------------------------------------------------- flap window

    const HOUR: u64 = 60 * 60 * 1000;

    #[test]
    fn a_brief_outage_does_not_condemn_a_peer() {
        let cfg = RepairConfig::default();
        let mut t = FailureTracker::new();
        // Six failures, but all within a minute — a reboot, not a departure.
        for i in 0..6 {
            t.record_unreachable_at("a", i * 10_000);
        }
        assert!(!t.presumed_gone_at("a", &cfg, 60_000));
    }

    #[test]
    fn sustained_absence_does_condemn() {
        let cfg = RepairConfig::default();
        let mut t = FailureTracker::new();
        for i in 0..6 {
            t.record_unreachable_at("a", i * HOUR / 2);
        }
        assert!(t.presumed_gone_at("a", &cfg, 3 * HOUR));
    }

    #[test]
    fn too_few_probes_never_condemns_however_long_it_has_been() {
        let cfg = RepairConfig::default();
        let mut t = FailureTracker::new();
        t.record_unreachable_at("a", 0);
        assert!(!t.presumed_gone_at("a", &cfg, 100 * HOUR));
    }

    #[test]
    fn coming_back_clears_the_streak() {
        let cfg = RepairConfig::default();
        let mut t = FailureTracker::new();
        for i in 0..6 {
            t.record_unreachable_at("a", i * HOUR / 2);
        }
        assert!(t.presumed_gone_at("a", &cfg, 3 * HOUR));

        t.record_reachable("a");
        assert_eq!(t.consecutive_failures("a"), 0);
        assert!(!t.presumed_gone_at("a", &cfg, 3 * HOUR));
    }

    #[test]
    fn an_unseen_peer_is_not_gone() {
        let cfg = RepairConfig::default();
        assert!(!FailureTracker::new().presumed_gone_at("nobody", &cfg, HOUR));
    }

    // ------------------------------------------------------------- priority

    fn plan(state: BlockState) -> RepairPlan {
        RepairPlan {
            object: Hash::of(b"o"),
            block: 0,
            state,
            missing: vec![],
            survivors: vec![],
        }
    }

    #[test]
    fn critical_blocks_are_repaired_before_merely_degraded_ones() {
        let mut plans = [
            plan(BlockState::Degraded),
            plan(BlockState::Critical),
            plan(BlockState::Urgent),
        ];
        plans.sort_by_key(|p| p.priority());
        assert_eq!(plans[0].state, BlockState::Critical);
        assert_eq!(plans[1].state, BlockState::Urgent);
        assert_eq!(plans[2].state, BlockState::Degraded);
    }

    /// Lost blocks cannot be repaired, so they must not consume a pass that a
    /// still-savable block needs.
    #[test]
    fn lost_blocks_sort_last_and_are_not_repairable() {
        let mut plans = [plan(BlockState::Lost), plan(BlockState::Degraded)];
        plans.sort_by_key(|p| p.priority());
        assert_eq!(plans[0].state, BlockState::Degraded);
        assert!(!plan(BlockState::Lost).repairable());
        assert!(!plan(BlockState::Healthy).repairable());
        assert!(plan(BlockState::Critical).repairable());
    }
}
