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

use c0mpute_net::topics::{JobBid, JobOffer, job_topic};
use c0mpute_net::{GossipMessage, Libp2pNetwork};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

/// Reject job offers older than this — replay protection + freshness
/// guard.
pub const MAX_OFFER_AGE: Duration = Duration::from_secs(5 * 60);

/// Spawn a per-workload subscriber. Listens on
/// `c0mpute/jobs/<workload_type>` and (in this Phase 1 cut) just logs
/// what comes in. Phase 2 wires real bid evaluation + publication.
pub fn run_worker_subscriber(
    net: Arc<Libp2pNetwork>,
    workload_type: String,
    advertised_tags: Vec<String>,
) {
    tokio::spawn(async move {
        let topic = job_topic(&workload_type);
        if let Err(e) = net.subscribe(&topic).await {
            warn!(err = %e, %topic, "dispatch: subscribe failed");
            return;
        }
        info!(%topic, "dispatch: subscribed for workload");

        let mut rx = net.messages();
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
            // Try parse as JobOffer first, then JobBid (we'll see our
            // own bids back via gossipsub but ignore them).
            if let Ok(offer) = serde_json::from_slice::<JobOffer>(&msg.data) {
                handle_offer(&net, &topic, &offer, &advertised_tags).await;
            }
        }
    });
}

async fn handle_offer(
    net: &Libp2pNetwork,
    topic: &str,
    offer: &JobOffer,
    advertised_tags: &[String],
) {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    if now_ms.saturating_sub(offer.published_at_ms) > MAX_OFFER_AGE.as_millis() as u64 {
        debug!(job_id = %offer.job_id, "dispatch: ignoring stale offer");
        return;
    }
    if offer.deadline_unix_ms < now_ms {
        debug!(job_id = %offer.job_id, "dispatch: deadline already passed");
        return;
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
        return;
    }

    // Phase 1: publish a bid at the buyer's max price (cooperative
    // pricing). Phase 2 will compute a real per-worker price from
    // hardware + queue depth + reputation tier.
    let bid = JobBid {
        job_id: offer.job_id.clone(),
        bidder_peer_id: net.peer_id().to_base58(),
        bidder_did: None,
        price_usd: offer.max_price_usd,
        eta_seconds: 60, // placeholder; real ETA from the worker pool
        published_at_ms: now_ms,
    };
    let payload = match serde_json::to_vec(&bid) {
        Ok(b) => b,
        Err(e) => {
            warn!(err = %e, "dispatch: serialize bid");
            return;
        }
    };
    match net.publish(topic, payload).await {
        Ok(()) => info!(
            job_id = %offer.job_id,
            price = bid.price_usd,
            "dispatch: bid published"
        ),
        Err(e) => debug!(
            job_id = %offer.job_id,
            err = %e,
            "dispatch: bid publish failed (mesh forming?)"
        ),
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
