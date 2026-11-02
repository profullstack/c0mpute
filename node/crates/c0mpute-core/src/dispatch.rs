//! Job dispatch over gossipsub.
//!
//! Workers subscribe to `c0mpute/jobs/<workload-type>` for each
//! workload they support. On an inbound `JobOffer`:
//!
//!   1. Reject if older than `MAX_OFFER_AGE`.
//!   2. Reject if any `required_capabilities` aren't in our advertised
//!      tag set.
//!   3. Reject if the deadline can't be met.
//!   4. Reject if we are full: no runner, runner queue full, no free
//!      compute slot, or already holding as many open bids as we have free
//!      capacity (each open bid is a job we could be handed).
//!   5. Otherwise: publish a `JobBid` on the same topic, with an ETA from
//!      queue depth x recent job time and an `expires_at_ms`.
//!
//! After winning bid (received as `JobAccept`), the worker fetches the
//! manifest, runs the workload, publishes a `JobReceipt`. The receipt
//! flow lives in a follow-up commit — this module ships the offer/bid
//! plumbing.
//!
//! The buyer side (publishing offers + collecting bids + accepting one)
//! lives in `c0mpute job submit` and is not part of the worker daemon.

use std::sync::Arc;
use std::time::Duration;

use c0mpute_net::topics::{JobAccept, JobBid, JobOffer, job_topic};
use c0mpute_net::{GossipMessage, Libp2pNetwork};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};

use crate::pricing::{MarketLearner, PricingConfig};
use crate::runner::{AcceptedJob, WorkerLoad};

/// Reject job offers older than this — replay protection + freshness
/// guard.
pub const MAX_OFFER_AGE: Duration = Duration::from_secs(5 * 60);

/// Open bids remembered for learning the clearing price; reset when full
/// (auctions that never accept would otherwise pile up).
const MAX_OPEN_BIDS: usize = 1024;

/// How long a bid stands. The buyer picks a winner when its bid window
/// closes (8 s by default) and ignores bids past `expires_at_ms`; until
/// then the bid holds a unit of our capacity.
pub const BID_TTL: Duration = Duration::from_secs(60);

/// Clock-skew allowance: an accept that names us up to this long after our
/// bid expired is still honoured (the buyer judged expiry on its clock).
/// Capacity is held for the same span, so honouring it never overbooks.
const ACCEPT_GRACE_MS: u64 = 10_000;

/// ETA quoted before this workload has completed any job.
pub const DEFAULT_ETA_SECS: u64 = 60;

/// Upper bound on a quoted ETA.
const MAX_ETA_SECS: u64 = 24 * 60 * 60;

/// Spawn a per-workload subscriber. Listens on
/// `c0mpute/jobs/<workload_type>`:
///   - bids on every offer we're capable of fulfilling
///   - watches for accepts naming this peer-id, and forwards the
///     accepted job to `runner_tx` for execution.
pub fn run_worker_subscriber(
    net: Arc<Libp2pNetwork>,
    workload_type: String,
    advertised_tags: Vec<String>,
    runner_tx: Option<mpsc::Sender<AcceptedJob>>,
) {
    run_worker_subscriber_filtered(net, workload_type, advertised_tags, runner_tx, None)
}

/// Extra per-workload eligibility, checked after the capability tags and
/// before bidding (spec validation, "busy", etc.). `false` = don't bid.
pub type OfferFilter = Arc<dyn Fn(&JobOffer) -> bool + Send + Sync>;

/// [`run_worker_subscriber`] with an [`OfferFilter`].
pub fn run_worker_subscriber_filtered(
    net: Arc<Libp2pNetwork>,
    workload_type: String,
    advertised_tags: Vec<String>,
    runner_tx: Option<mpsc::Sender<AcceptedJob>>,
    filter: Option<OfferFilter>,
) {
    run_worker_subscriber_priced(net, workload_type, advertised_tags, runner_tx, filter, None)
}

/// [`run_worker_subscriber_filtered`] that bids with the operator's
/// [`PricingConfig`] (dynamic with a floor, or fixed) instead of the buyer's
/// cap. `None` keeps the cap-price bid.
pub fn run_worker_subscriber_priced(
    net: Arc<Libp2pNetwork>,
    workload_type: String,
    advertised_tags: Vec<String>,
    runner_tx: Option<mpsc::Sender<AcceptedJob>>,
    filter: Option<OfferFilter>,
    pricing: Option<PricingConfig>,
) {
    run_worker_subscriber_with_load(
        net,
        workload_type,
        advertised_tags,
        runner_tx,
        filter,
        pricing,
        None,
    )
}

/// [`run_worker_subscriber_priced`] that also knows the node's compute
/// slots and recent job times ([`WorkerLoad`]), so it stops bidding when no
/// slot is free and quotes a measured ETA. Without a load it still caps
/// open bids to the runner queue's free places.
pub fn run_worker_subscriber_with_load(
    net: Arc<Libp2pNetwork>,
    workload_type: String,
    advertised_tags: Vec<String>,
    runner_tx: Option<mpsc::Sender<AcceptedJob>>,
    filter: Option<OfferFilter>,
    pricing: Option<PricingConfig>,
    load: Option<WorkerLoad>,
) {
    tokio::spawn(async move {
        let topic = job_topic(&workload_type);
        if let Err(e) = net.subscribe(&topic).await {
            warn!(err = %e, %topic, "dispatch: subscribe failed");
            return;
        }
        info!(%topic, "dispatch: subscribed for workload");

        let our_peer_id = net.peer_id().to_base58();
        // This node's bench score rides on every bid for smart routing;
        // re-read every 10 minutes so a fresh `c0mpute bench` shows up.
        let mut bench = BenchCache::default();
        let mut rx = net.messages();
        // What the market clears at, learned from accepts on auctions we
        // bid in, plus our open bids keyed by job_id. An accept is only
        // honoured when it matches a bid we actually placed.
        let mut market = MarketLearner::default();
        let mut open_bids: std::collections::HashMap<String, OpenBid> =
            std::collections::HashMap::new();
        loop {
            let msg: GossipMessage = match rx.recv().await {
                Ok(m) => m,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(skipped = n, "dispatch: broadcast lagged");
                    continue;
                }
                Err(_) => return,
            };
            if msg.topic != topic {
                continue;
            }
            // Try parse as JobOffer first, then JobAccept. (Bids and
            // receipts also flow on this topic; we ignore them — bids
            // are what we PUBLISH, receipts are for the buyer.)
            if let Ok(offer) = serde_json::from_slice::<JobOffer>(&msg.data) {
                if offer.workload_type != workload_type {
                    continue;
                }
                if let Some(f) = &filter {
                    if !f(&offer) {
                        debug!(job_id = %offer.job_id, "dispatch: offer filtered out");
                        continue;
                    }
                }
                // No runner means nothing could execute a win: don't bid.
                let Some(tx) = runner_tx.as_ref() else {
                    debug!(job_id = %offer.job_id, "dispatch: no runner wired, not bidding");
                    continue;
                };
                let now = unix_ms();
                open_bids.retain(|_, b| b.holds_capacity(now));
                let capacity = Capacity::of(tx, load.as_ref());
                if !capacity.can_bid(open_bids.len()) {
                    debug!(
                        job_id = %offer.job_id,
                        open_bids = open_bids.len(),
                        ?capacity,
                        "dispatch: at capacity, not bidding"
                    );
                    continue;
                }
                let eta = eta_seconds(
                    capacity.queued,
                    capacity.total_slots,
                    load.as_ref().and_then(WorkerLoad::recent_job_duration),
                );
                if let Some((price, expires_at_ms)) = handle_offer(
                    &net,
                    &topic,
                    &offer,
                    &advertised_tags,
                    pricing.as_ref(),
                    queue_load(tx),
                    &market,
                    eta,
                    bench.get(),
                    capacity.load_fraction(queue_load(tx)),
                )
                .await
                {
                    if open_bids.len() >= MAX_OPEN_BIDS {
                        open_bids.clear();
                    }
                    open_bids.insert(
                        offer.job_id.clone(),
                        OpenBid {
                            cap: offer.max_price_usd,
                            price,
                            buyer_peer_id: offer.buyer_peer_id.clone(),
                            spec_inline: offer.spec_inline.clone(),
                            external_ref: offer.external_ref.clone(),
                            expires_at_ms,
                        },
                    );
                }
                continue;
            }
            if let Ok(accept) = serde_json::from_slice::<JobAccept>(&msg.data) {
                if accept.workload_type != workload_type {
                    continue;
                }
                // Only the buyer that published the offer may accept it.
                // Gossipsub runs Signed + Strict, so `msg.source` is the
                // authenticated publisher and cannot be spoofed.
                if !crate::buyer::source_matches_claim(&msg, &accept.buyer_peer_id) {
                    warn!(job_id = %accept.job_id, "dispatch: discarding accept, gossip source is not the claimed buyer");
                    continue;
                }
                let won = accept.winning_bidder_peer_id == our_peer_id;
                let Some(bid) = open_bids.remove(&accept.job_id) else {
                    if won {
                        warn!(job_id = %accept.job_id, "dispatch: discarding accept for a job we never bid on");
                    }
                    continue;
                };
                if let Err(reason) = bid.matches(&accept, unix_ms()) {
                    warn!(job_id = %accept.job_id, reason, "dispatch: discarding accept that does not match our bid");
                    continue;
                }
                market.observe(bid.cap, bid.price, accept.agreed_price_usd, won);
                debug!(job_id = %accept.job_id, won, cleared = accept.agreed_price_usd, share = market.share, "dispatch: market price learned");
                if !won {
                    debug!(
                        job_id = %accept.job_id,
                        winner = %accept.winning_bidder_peer_id,
                        "dispatch: accept for someone else"
                    );
                    continue;
                }
                let Some(tx) = runner_tx.as_ref() else {
                    warn!(
                        job_id = %accept.job_id,
                        "dispatch: won bid but no runner wired for this workload"
                    );
                    continue;
                };
                info!(
                    job_id = %accept.job_id,
                    "dispatch: we won — forwarding to runner"
                );
                if let Err(e) = tx.send(AcceptedJob { accept }).await {
                    warn!(err = %e, "dispatch: runner channel closed");
                }
                continue;
            }
        }
    });
}

/// A bid we published and are waiting to hear back on.
#[derive(Clone, Debug)]
pub(crate) struct OpenBid {
    pub cap: f64,
    pub price: f64,
    pub buyer_peer_id: String,
    pub spec_inline: Option<serde_json::Value>,
    pub external_ref: Option<String>,
    pub expires_at_ms: u64,
}

impl OpenBid {
    /// Whether this bid still reserves capacity (and can still be won).
    pub(crate) fn holds_capacity(&self, now_ms: u64) -> bool {
        now_ms <= self.expires_at_ms.saturating_add(ACCEPT_GRACE_MS)
    }

    /// An accept is only honoured when it comes from the offer's buyer,
    /// carries exactly the spec and `external_ref` we bid on, stays within
    /// the cap, and arrives while the bid stands.
    pub(crate) fn matches(&self, accept: &JobAccept, now_ms: u64) -> Result<(), &'static str> {
        if accept.buyer_peer_id != self.buyer_peer_id {
            return Err("buyer differs from the offer");
        }
        if accept.spec_inline != self.spec_inline {
            return Err("spec differs from the offer we bid on");
        }
        if accept.external_ref != self.external_ref {
            return Err("external_ref differs from the offer we bid on");
        }
        if !self.holds_capacity(now_ms) {
            return Err("our bid expired before the accept arrived");
        }
        if !accept.agreed_price_usd.is_finite()
            || accept.agreed_price_usd < 0.0
            || accept.agreed_price_usd > self.cap
        {
            return Err("agreed price outside the offer cap");
        }
        Ok(())
    }
}

/// How full the runner's queue is: 0.0 empty, 1.0 full.
fn queue_load(tx: &mpsc::Sender<AcceptedJob>) -> f64 {
    let max = tx.max_capacity();
    if max == 0 {
        return 0.0;
    }
    (max - tx.capacity()) as f64 / max as f64
}

/// The local `c0mpute bench` score, read from disk at most every 10 minutes.
#[derive(Default)]
struct BenchCache {
    score: Option<u32>,
    read_at: Option<std::time::Instant>,
}

impl BenchCache {
    fn get(&mut self) -> Option<u32> {
        let stale = self.read_at.is_none_or(|t| t.elapsed() > Duration::from_secs(600));
        if stale {
            self.score = crate::capabilities::local_bench_score();
            self.read_at = Some(std::time::Instant::now());
        }
        self.score
    }
}

/// What this workload can take on right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Capacity {
    /// Accepted jobs waiting in the runner channel for a compute slot.
    pub queued: usize,
    /// Free places in the runner channel.
    pub queue_free: usize,
    /// Free node-wide compute slots; `None` when this subscriber has no
    /// slot accounting (the queue alone then bounds it).
    pub free_slots: Option<usize>,
    pub total_slots: usize,
}

impl Capacity {
    fn of(tx: &mpsc::Sender<AcceptedJob>, load: Option<&WorkerLoad>) -> Self {
        let queue_free = tx.capacity();
        Self {
            queued: tx.max_capacity().saturating_sub(queue_free),
            queue_free,
            free_slots: load.map(WorkerLoad::free_slots),
            total_slots: load.map(WorkerLoad::total_slots).unwrap_or(1),
        }
    }

    /// How many jobs we could start now without overbooking: free slots not
    /// already spoken for by queued jobs, never more than the queue holds.
    pub fn budget(&self) -> usize {
        let idle = self
            .free_slots
            .map(|free| free.saturating_sub(self.queued))
            .unwrap_or(usize::MAX);
        idle.min(self.queue_free)
    }

    /// How busy this workload's node is, 0.0 idle .. 1.0 saturated, for
    /// smart routing on the buyer: busy slots plus queued jobs over total
    /// slots, or the runner queue's fill when there is no slot accounting.
    pub fn load_fraction(&self, queue_fill: f64) -> f32 {
        match self.free_slots {
            Some(free) if self.total_slots > 0 => {
                let busy = self.total_slots.saturating_sub(free) + self.queued;
                (busy as f32 / self.total_slots as f32).clamp(0.0, 1.0)
            }
            _ => (queue_fill as f32).clamp(0.0, 1.0),
        }
    }

    /// Bid only while open bids (each a job we could be handed) stay under
    /// the budget.
    pub fn can_bid(&self, open_bids: usize) -> bool {
        open_bids < self.budget()
    }
}

/// Seconds until a job won now would finish: the queue ahead of it drains
/// `total_slots` at a time, then it runs, each wave taking about the recent
/// average job time ([`DEFAULT_ETA_SECS`] before any job has completed).
pub(crate) fn eta_seconds(queued: usize, total_slots: usize, recent: Option<Duration>) -> u64 {
    let per_job = recent
        .map(|d| d.as_secs_f64())
        .filter(|s| *s > 0.0)
        .unwrap_or(DEFAULT_ETA_SECS as f64);
    let waves = (queued / total_slots.max(1)) as f64 + 1.0;
    ((per_job * waves).ceil() as u64).clamp(1, MAX_ETA_SECS)
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

async fn handle_offer(
    net: &Libp2pNetwork,
    topic: &str,
    offer: &JobOffer,
    advertised_tags: &[String],
    pricing: Option<&PricingConfig>,
    load: f64,
    market: &MarketLearner,
    eta_seconds: u64,
    bench_score: Option<u32>,
    bid_load: f32,
) -> Option<(f64, u64)> {
    let now_ms = unix_ms();
    if now_ms.saturating_sub(offer.published_at_ms) > MAX_OFFER_AGE.as_millis() as u64 {
        debug!(job_id = %offer.job_id, "dispatch: ignoring stale offer");
        return None;
    }
    if offer.deadline_unix_ms < now_ms {
        debug!(job_id = %offer.job_id, "dispatch: deadline already passed");
        return None;
    }
    if offer
        .external_ref
        .as_deref()
        .is_some_and(|r| !c0mpute_net::topics::valid_external_ref(r))
    {
        debug!(job_id = %offer.job_id, "dispatch: malformed external_ref, not bidding");
        return None;
    }
    // Capability check.
    let missing: Vec<&String> = offer
        .required_capabilities
        .iter()
        .filter(|c| !advertised_tags.iter().any(|a| a == *c))
        .collect();
    if !missing.is_empty() {
        debug!(
            job_id = %offer.job_id,
            ?missing,
            "dispatch: not eligible (missing capabilities)"
        );
        return None;
    }

    // The operator's listing prices the bid (dynamic by queue load, never
    // under their floor); without one, bid the buyer's cap.
    let price_usd = match pricing {
        Some(p) => match p.quote_in_market(offer.max_price_usd, load, market) {
            Some(price) => price,
            None => {
                debug!(job_id = %offer.job_id, cap = offer.max_price_usd, min = p.min_usd, "dispatch: cap under our floor, not bidding");
                return None;
            }
        },
        None => offer.max_price_usd,
    };
    let expires_at_ms = now_ms + BID_TTL.as_millis() as u64;
    let bid = JobBid {
        job_id: offer.job_id.clone(),
        bidder_peer_id: net.peer_id().to_base58(),
        bidder_did: None,
        price_usd,
        eta_seconds,
        published_at_ms: now_ms,
        expires_at_ms: Some(expires_at_ms),
        bench_score,
        load: Some(bid_load),
    };
    let payload = match serde_json::to_vec(&bid) {
        Ok(b) => b,
        Err(e) => {
            warn!(err = %e, "dispatch: serialize bid");
            return None;
        }
    };
    match net.publish(topic, payload).await {
        Ok(()) => {
            info!(
                job_id = %offer.job_id,
                price = bid.price_usd,
                eta_s = bid.eta_seconds,
                "dispatch: bid published"
            );
            Some((price_usd, expires_at_ms))
        }
        Err(e) => {
            debug!(
                job_id = %offer.job_id,
                err = %e,
                "dispatch: bid publish failed (mesh forming?)"
            );
            None
        }
    }
}

/// Workload types this worker should listen for, derived from its
/// configured roles. Each modality maps to a separate job topic.
/// `infernet.inference` is handled by the infernet peer binary, not
/// this daemon.
pub fn workload_types_from_roles(roles: &[c0mpute_proto::Role]) -> Vec<String> {
    let mut types = Vec::new();
    if roles.contains(&c0mpute_proto::Role::Transcode) {
        types.push("ffmpeg.transcode".to_string());
    }
    if roles.contains(&c0mpute_proto::Role::Tts) {
        types.push("tts.synthesize".to_string());
    }
    if roles.contains(&c0mpute_proto::Role::Stt) {
        types.push("stt.transcribe".to_string());
    }
    types
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bid() -> OpenBid {
        OpenBid {
            cap: 1.0,
            price: 0.6,
            buyer_peer_id: "buyer".into(),
            spec_inline: Some(serde_json::json!({"input_url": "https://example.com/a.mp4"})),
            external_ref: Some("infernet:job_42".into()),
            expires_at_ms: NOW + 60_000,
        }
    }

    const NOW: u64 = 1_000_000;

    fn accept() -> JobAccept {
        JobAccept {
            job_id: "j".into(),
            buyer_peer_id: "buyer".into(),
            winning_bidder_peer_id: "us".into(),
            agreed_price_usd: 0.6,
            workload_type: "ffmpeg.transcode".into(),
            spec_inline: Some(serde_json::json!({"input_url": "https://example.com/a.mp4"})),
            published_at_ms: 0,
            external_ref: Some("infernet:job_42".into()),
        }
    }

    #[test]
    fn matching_accept_is_honoured() {
        assert!(bid().matches(&accept(), NOW).is_ok());
    }

    #[test]
    fn accept_from_another_buyer_is_rejected() {
        let mut a = accept();
        a.buyer_peer_id = "mallory".into();
        assert!(bid().matches(&a, NOW).is_err());
    }

    #[test]
    fn accept_with_swapped_spec_is_rejected() {
        let mut a = accept();
        a.spec_inline = Some(serde_json::json!({"input_url": "http://169.254.169.254/"}));
        assert!(bid().matches(&a, NOW).is_err());
    }

    #[test]
    fn accept_over_cap_or_nan_is_rejected() {
        let mut a = accept();
        a.agreed_price_usd = 1.5;
        assert!(bid().matches(&a, NOW).is_err());
        a.agreed_price_usd = f64::NAN;
        assert!(bid().matches(&a, NOW).is_err());
    }

    #[test]
    fn accept_with_other_external_ref_is_rejected() {
        let mut a = accept();
        a.external_ref = Some("infernet:job_43".into());
        assert!(bid().matches(&a, NOW).is_err());
        a.external_ref = None;
        assert!(bid().matches(&a, NOW).is_err());
    }

    #[test]
    fn accept_after_bid_expiry_is_rejected() {
        let b = bid();
        assert!(b.matches(&accept(), b.expires_at_ms + ACCEPT_GRACE_MS).is_ok());
        assert!(b.matches(&accept(), b.expires_at_ms + ACCEPT_GRACE_MS + 1).is_err());
        assert!(!b.holds_capacity(b.expires_at_ms + ACCEPT_GRACE_MS + 1));
    }

    fn cap(queued: usize, queue_free: usize, free_slots: Option<usize>) -> Capacity {
        Capacity { queued, queue_free, free_slots, total_slots: 4 }
    }

    #[test]
    fn no_bid_when_no_compute_slot_is_free() {
        assert!(!cap(0, 16, Some(0)).can_bid(0));
        // Two slots free, but two jobs already queued for them.
        assert!(!cap(2, 14, Some(2)).can_bid(0));
    }

    #[test]
    fn no_bid_when_runner_queue_is_full() {
        assert!(!cap(16, 0, Some(4)).can_bid(0));
        assert!(!cap(8, 0, None).can_bid(0));
    }

    #[test]
    fn open_bids_are_capped_to_free_capacity() {
        let c = cap(0, 16, Some(3));
        assert_eq!(c.budget(), 3);
        assert!(c.can_bid(2));
        assert!(!c.can_bid(3), "three open bids already cover three free slots");
        // Without slot accounting the queue alone caps it.
        let q = cap(5, 3, None);
        assert!(q.can_bid(2));
        assert!(!q.can_bid(3));
    }

    #[test]
    fn eta_scales_with_queue_depth_and_recent_job_time() {
        assert_eq!(eta_seconds(0, 4, None), DEFAULT_ETA_SECS);
        assert_eq!(eta_seconds(0, 4, Some(Duration::from_secs(12))), 12);
        // Four slots: 4 queued jobs are one wave ahead of this one.
        assert_eq!(eta_seconds(4, 4, Some(Duration::from_secs(12))), 24);
        assert_eq!(eta_seconds(9, 4, Some(Duration::from_secs(10))), 30);
        assert_eq!(eta_seconds(3, 1, None), 4 * DEFAULT_ETA_SECS);
        assert_eq!(eta_seconds(0, 0, Some(Duration::from_millis(1))), 1);
        assert_eq!(eta_seconds(usize::MAX, 1, None), MAX_ETA_SECS);
    }
}
