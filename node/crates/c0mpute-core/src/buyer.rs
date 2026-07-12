//! Buyer side of the auction: publish a `JobOffer`, collect `JobBid`s,
//! pick a winner, publish `JobAccept`, wait for `JobReceipt`.
//!
//! Phase 1: inline-spec auction. The full workload spec ships in
//! `JobOffer.spec_inline` (and again in `JobAccept.spec_inline`) so
//! workers never need a separate manifest fetch. Phase 2 swaps this for
//! the chunk-store / request-response transfer keyed off `spec_hash`.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use c0mpute_net::topics::{
    JobAccept, JobBid, JobOffer, JobReceipt, JobStatus, job_topic,
};
use c0mpute_net::{GossipMessage, Libp2pNetwork};
use tokio::sync::broadcast;
use tokio::time::{Instant, sleep};
use tracing::{debug, info, warn};
use uuid::Uuid;

/// How long to keep collecting bids before picking a winner.
pub const DEFAULT_BID_WINDOW: Duration = Duration::from_secs(8);
/// How long to wait for the winner's receipt before giving up.
pub const DEFAULT_RECEIPT_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Auction inputs the buyer assembles before publishing the offer.
pub struct JobAuction {
    pub workload_type: String,
    pub spec_inline: serde_json::Value,
    pub required_capabilities: Vec<String>,
    pub max_price_usd: f64,
    pub deadline: Duration,
    pub bid_window: Duration,
    pub receipt_timeout: Duration,
}

impl JobAuction {
    pub fn new(workload_type: impl Into<String>, spec_inline: serde_json::Value) -> Self {
        Self {
            workload_type: workload_type.into(),
            spec_inline,
            required_capabilities: Vec::new(),
            max_price_usd: 1.0,
            deadline: Duration::from_secs(15 * 60),
            bid_window: DEFAULT_BID_WINDOW,
            receipt_timeout: DEFAULT_RECEIPT_TIMEOUT,
        }
    }

    pub fn with_required_capabilities(mut self, caps: Vec<String>) -> Self {
        self.required_capabilities = caps;
        self
    }

    pub fn with_max_price_usd(mut self, max: f64) -> Self {
        self.max_price_usd = max;
        self
    }

    pub fn with_deadline(mut self, d: Duration) -> Self {
        self.deadline = d;
        self
    }

    pub fn with_bid_window(mut self, d: Duration) -> Self {
        self.bid_window = d;
        self
    }
}

pub struct AuctionOutcome {
    pub job_id: String,
    pub accepted_bid: JobBid,
    pub receipt: JobReceipt,
}

/// Run the full buyer-side auction synchronously: subscribe → publish
/// offer → collect bids → publish accept → wait for receipt.
///
/// Returns once the receipt arrives (or the timeout expires). The caller
/// is responsible for any further verification of `output_hash`.
pub async fn run_auction(
    net: Arc<Libp2pNetwork>,
    auction: JobAuction,
) -> Result<AuctionOutcome> {
    let topic = job_topic(&auction.workload_type);
    net.subscribe(&topic).await?;

    // Subscribe BEFORE publishing so we don't miss bids that arrive
    // during the publish round-trip.
    let mut rx: broadcast::Receiver<GossipMessage> = net.messages();

    let job_id = Uuid::new_v4().to_string();
    let now_ms = unix_ms();
    let offer = JobOffer {
        job_id: job_id.clone(),
        workload_type: auction.workload_type.clone(),
        buyer_peer_id: net.peer_id().to_base58(),
        buyer_did: None,
        spec_hash: blake3_hex(&auction.spec_inline.to_string()),
        spec_inline: Some(auction.spec_inline.clone()),
        required_capabilities: auction.required_capabilities.clone(),
        max_price_usd: auction.max_price_usd,
        deadline_unix_ms: now_ms + auction.deadline.as_millis() as u64,
        published_at_ms: now_ms,
    };

    info!(
        %job_id,
        workload = %auction.workload_type,
        "buyer: publishing offer"
    );
    publish_json(&net, &topic, &offer).await?;

    // ── collect bids ────────────────────────────────────────────────
    let deadline = Instant::now() + auction.bid_window;
    let mut bids: Vec<JobBid> = Vec::new();
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(msg)) if msg.topic == topic => {
                if let Ok(bid) = serde_json::from_slice::<JobBid>(&msg.data) {
                    if bid.job_id == job_id
                        && bid.bidder_peer_id != offer.buyer_peer_id
                    {
                        if !source_matches_claim(&msg, &bid.bidder_peer_id) {
                            warn!(
                                %job_id,
                                claimed = %bid.bidder_peer_id,
                                "buyer: discarding bid, gossip source does not match claimed bidder_peer_id"
                            );
                            continue;
                        }
                        debug!(
                            %job_id,
                            bidder = %bid.bidder_peer_id,
                            price = bid.price_usd,
                            "buyer: bid received"
                        );
                        bids.push(bid);
                    }
                }
            }
            Ok(Ok(_)) => continue,
            Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
                warn!(skipped = n, "buyer: bid stream lagged");
            }
            Ok(Err(_)) => break,
            Err(_) => break, // timeout: bid window closed
        }
    }

    info!(%job_id, count = bids.len(), "buyer: bid window closed");
    if bids.is_empty() {
        bail!("no bids received within {:?}", auction.bid_window);
    }

    // ── pick winner: lowest price, tie-break on earliest published. ─
    bids.sort_by(|a, b| {
        a.price_usd
            .partial_cmp(&b.price_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.published_at_ms.cmp(&b.published_at_ms))
    });
    let winner = bids.remove(0);
    info!(
        %job_id,
        winner = %winner.bidder_peer_id,
        price = winner.price_usd,
        "buyer: winner chosen"
    );

    // ── publish accept ──────────────────────────────────────────────
    let accept = JobAccept {
        job_id: job_id.clone(),
        buyer_peer_id: offer.buyer_peer_id.clone(),
        winning_bidder_peer_id: winner.bidder_peer_id.clone(),
        agreed_price_usd: winner.price_usd,
        workload_type: auction.workload_type.clone(),
        spec_inline: Some(auction.spec_inline.clone()),
        published_at_ms: unix_ms(),
    };
    publish_json(&net, &topic, &accept).await?;

    // ── wait for receipt ────────────────────────────────────────────
    let recv_deadline = Instant::now() + auction.receipt_timeout;
    while Instant::now() < recv_deadline {
        let remaining = recv_deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(msg)) if msg.topic == topic => {
                if let Ok(receipt) = serde_json::from_slice::<JobReceipt>(&msg.data) {
                    if receipt.job_id == job_id
                        && receipt.worker_peer_id == winner.bidder_peer_id
                    {
                        if !source_matches_claim(&msg, &receipt.worker_peer_id) {
                            warn!(
                                %job_id,
                                claimed = %receipt.worker_peer_id,
                                "buyer: discarding receipt, gossip source does not match claimed worker_peer_id"
                            );
                            continue;
                        }
                        info!(
                            %job_id,
                            status = ?receipt.status,
                            output_hash = ?receipt.output_hash,
                            "buyer: receipt received"
                        );
                        return Ok(AuctionOutcome {
                            job_id,
                            accepted_bid: winner,
                            receipt,
                        });
                    }
                }
            }
            Ok(Ok(_)) => continue,
            Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
                warn!(skipped = n, "buyer: receipt stream lagged");
            }
            Ok(Err(_)) => break,
            Err(_) => break,
        }
    }

    bail!(
        "no receipt from winner {} within {:?}",
        winner.bidder_peer_id,
        auction.receipt_timeout
    );
}

async fn publish_json<T: serde::Serialize>(
    net: &Libp2pNetwork,
    topic: &str,
    value: &T,
) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    // First publish on a fresh topic can race the gossipsub mesh
    // forming. Retry briefly if no peers are subscribed yet.
    for attempt in 0..3 {
        match net.publish(topic, bytes.clone()).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                debug!(attempt, err = %e, "publish retry");
                sleep(Duration::from_millis(200 * (attempt + 1))).await;
            }
        }
    }
    net.publish(topic, bytes).await
}

/// Gossipsub gives us a cryptographically-authenticated `source: Option<PeerId>`
/// (see `c0mpute_net::GossipMessage`) that cannot be forged by another peer —
/// `capabilities.rs`'s ad registry already keys off it for the same reason.
/// `JobBid.bidder_peer_id` / `JobReceipt.worker_peer_id` are just self-reported
/// strings inside the signed-later-but-not-yet payload, so without this check
/// any peer can claim to be any other peer_id in a bid or completion receipt.
fn source_matches_claim(msg: &GossipMessage, claimed_peer_id: &str) -> bool {
    match &msg.source {
        Some(source) => source.to_base58() == claimed_peer_id,
        None => false,
    }
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn blake3_hex(s: &str) -> String {
    let h = blake3::hash(s.as_bytes());
    hex::encode(h.as_bytes())
}

// Dummy use to suppress "unused" if `JobStatus` isn't referenced elsewhere
// in this module — it's part of the receipt we return.
#[allow(dead_code)]
fn _status_link(s: JobStatus) -> JobStatus {
    s
}
