//! Job dispatch over gossipsub.
//!
//! Workers subscribe to `c0mpute/jobs/<workload-type>` for each
//! workload they support. On an inbound `JobOffer`:
//!
//!   1. Reject if older than `MAX_OFFER_AGE`.
//!   2. Reject if any `required_capabilities` aren't in our advertised
//!      tag set.
//!   3. Reject if the deadline can't be met.
//!   4. Otherwise: publish a `JobBid` on the same topic.
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
use crate::runner::AcceptedJob;

/// Reject job offers older than this — replay protection + freshness
/// guard.
pub const MAX_OFFER_AGE: Duration = Duration::from_secs(5 * 60);

/// Open bids remembered for learning the clearing price; reset when full
/// (auctions that never accept would otherwise pile up).
const MAX_OPEN_BIDS: usize = 1024;

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
    tokio::spawn(async move {
        let topic = job_topic(&workload_type);
        if let Err(e) = net.subscribe(&topic).await {
            warn!(err = %e, %topic, "dispatch: subscribe failed");
            return;
        }
        info!(%topic, "dispatch: subscribed for workload");

        let our_peer_id = net.peer_id().to_base58();
        let mut rx = net.messages();
        // What the market clears at, learned from accepts on auctions we
        // bid in, plus our open bids (job_id -> cap, our price).
        let mut market = MarketLearner::default();
        let mut open_bids: std::collections::HashMap<String, (f64, f64)> =
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
                let load = runner_tx.as_ref().map(queue_load).unwrap_or(0.0);
                if let Some(price) = handle_offer(
                    &net,
                    &topic,
                    &offer,
                    &advertised_tags,
                    pricing.as_ref(),
                    load,
                    &market,
                )
                .await
                {
                    if open_bids.len() >= MAX_OPEN_BIDS {
                        open_bids.clear();
                    }
                    open_bids.insert(offer.job_id.clone(), (offer.max_price_usd, price));
                }
                continue;
            }
            if let Ok(accept) = serde_json::from_slice::<JobAccept>(&msg.data) {
                if accept.workload_type != workload_type {
                    continue;
                }
                let won = accept.winning_bidder_peer_id == our_peer_id;
                if let Some((cap, ours)) = open_bids.remove(&accept.job_id) {
                    market.observe(cap, ours, accept.agreed_price_usd, won);
                    debug!(job_id = %accept.job_id, won, cleared = accept.agreed_price_usd, share = market.share, "dispatch: market price learned");
                }
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

/// How full the runner's queue is: 0.0 empty, 1.0 full.
fn queue_load(tx: &mpsc::Sender<AcceptedJob>) -> f64 {
    let max = tx.max_capacity();
    if max == 0 {
        return 0.0;
    }
    (max - tx.capacity()) as f64 / max as f64
}

async fn handle_offer(
    net: &Libp2pNetwork,
    topic: &str,
    offer: &JobOffer,
    advertised_tags: &[String],
    pricing: Option<&PricingConfig>,
    load: f64,
    market: &MarketLearner,
) -> Option<f64> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    if now_ms.saturating_sub(offer.published_at_ms) > MAX_OFFER_AGE.as_millis() as u64 {
        debug!(job_id = %offer.job_id, "dispatch: ignoring stale offer");
        return None;
    }
    if offer.deadline_unix_ms < now_ms {
        debug!(job_id = %offer.job_id, "dispatch: deadline already passed");
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
    let bid = JobBid {
        job_id: offer.job_id.clone(),
        bidder_peer_id: net.peer_id().to_base58(),
        bidder_did: None,
        price_usd,
        eta_seconds: 60, // placeholder; real ETA from the worker pool
        published_at_ms: now_ms,
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
                "dispatch: bid published"
            );
            Some(price_usd)
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
/// configured roles. Today: `Transcode` role → `ffmpeg.transcode`.
/// `infernet.inference` is handled by the infernet peer binary, not
/// this daemon.
pub fn workload_types_from_roles(roles: &[c0mpute_proto::Role]) -> Vec<String> {
    let mut types = Vec::new();
    if roles.contains(&c0mpute_proto::Role::Transcode) {
        types.push("ffmpeg.transcode".to_string());
    }
    types
}
