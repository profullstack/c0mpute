//! Buyer side of the auction: publish a `JobOffer`, collect `JobBid`s,
//! pick a winner, publish `JobAccept`, wait for `JobReceipt`.
//!
//! Phase 1: inline-spec auction. The full workload spec ships in
//! `JobOffer.spec_inline` (and again in `JobAccept.spec_inline`) so
//! workers never need a separate manifest fetch. Phase 2 swaps this for
//! the chunk-store / request-response transfer keyed off `spec_hash`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Result, bail};
use c0mpute_net::topics::{JobAccept, JobBid, JobOffer, JobReceipt, JobStatus, job_topic};
use c0mpute_net::{GossipMessage, Libp2pNetwork};
use tokio::sync::broadcast;
use tokio::time::{Instant, sleep};
use tracing::{debug, info, warn};
use uuid::Uuid;

/// How long to keep collecting bids before picking a winner.
pub const DEFAULT_BID_WINDOW: Duration = Duration::from_secs(8);
/// How long to wait for the winner's receipt before giving up.
pub const DEFAULT_RECEIPT_TIMEOUT: Duration = Duration::from_secs(15 * 60);

// Rotate equally priced bids so a burst of parallel chunks does not keep
// selecting the same earliest bidder when several workers have equal prices.
static NEXT_EQUAL_PRICE_BID: AtomicUsize = AtomicUsize::new(0);

/// Auction inputs the buyer assembles before publishing the offer.
pub struct JobAuction {
    pub workload_type: String,
    pub spec_inline: serde_json::Value,
    pub required_capabilities: Vec<String>,
    pub max_price_usd: f64,
    pub deadline: Duration,
    pub bid_window: Duration,
    pub receipt_timeout: Duration,
    /// When non-empty, only bids from these peer ids are considered (the
    /// buyer's trusted workers). Others are ignored, not rejected.
    pub allowed_bidders: Vec<String>,
    /// Bids from these peer ids are ignored. This is useful when retrying a
    /// job after a selected worker cannot return its output.
    pub excluded_bidders: Vec<String>,
    /// The job's id in the system that bills it (Infernet job id, CoinPay
    /// invoice id). Carried on the offer, accept and receipt; billing keys
    /// on it, not on the per-auction `job_id` (see
    /// [`c0mpute_net::topics::JobOffer::external_ref`]). Reuse the same
    /// value when re-auctioning a job.
    pub external_ref: Option<String>,
    /// How the winner is chosen. Default: the most capable worker that is
    /// not overloaded ([`crate::routing`]).
    pub policy: crate::routing::SelectionPolicy,
    /// Told `(job_id, winning bid)` the moment the accept is published, for
    /// workloads that exchange more with the winner before its receipt
    /// (`webhook.deliver` seals its request to the winner's key).
    pub accept_notify: Option<tokio::sync::mpsc::UnboundedSender<(String, JobBid)>>,
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
            allowed_bidders: Vec::new(),
            excluded_bidders: Vec::new(),
            external_ref: None,
            policy: crate::routing::SelectionPolicy::default(),
            accept_notify: None,
        }
    }

    pub fn with_accept_notify(mut self, tx: tokio::sync::mpsc::UnboundedSender<(String, JobBid)>) -> Self {
        self.accept_notify = Some(tx);
        self
    }

    pub fn with_policy(mut self, policy: crate::routing::SelectionPolicy) -> Self {
        self.policy = policy;
        self
    }

    pub fn with_external_ref(mut self, external_ref: Option<String>) -> Self {
        self.external_ref = external_ref;
        self
    }

    pub fn with_allowed_bidders(mut self, peers: Vec<String>) -> Self {
        self.allowed_bidders = peers;
        self
    }

    pub fn with_excluded_bidders(mut self, peers: Vec<String>) -> Self {
        self.excluded_bidders = peers;
        self
    }

    pub fn with_receipt_timeout(mut self, d: Duration) -> Self {
        self.receipt_timeout = d;
        self
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
    /// The auction's `external_ref`, as the buyer set it. Use this rather
    /// than `receipt.external_ref`, which workers older than 0.2.49 omit.
    pub external_ref: Option<String>,
}

/// Run the full buyer-side auction synchronously: subscribe → publish
/// offer → collect bids → publish accept → wait for receipt.
///
/// Returns once the receipt arrives (or the timeout expires). The caller
/// is responsible for any further verification of `output_hash`.
pub async fn run_auction(net: Arc<Libp2pNetwork>, auction: JobAuction) -> Result<AuctionOutcome> {
    if let Some(r) = auction.external_ref.as_deref()
        && !c0mpute_net::topics::valid_external_ref(r)
    {
        bail!(
            "external_ref must be 1 to {} printable ASCII characters without spaces",
            c0mpute_net::topics::MAX_EXTERNAL_REF_LEN
        );
    }
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
        external_ref: auction.external_ref.clone(),
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
                    if bid.job_id == job_id && bid.bidder_peer_id != offer.buyer_peer_id {
                        if !auction.allowed_bidders.is_empty()
                            && !auction.allowed_bidders.contains(&bid.bidder_peer_id)
                        {
                            debug!(%job_id, bidder = %bid.bidder_peer_id, "buyer: bid from a worker outside the allowlist ignored");
                            continue;
                        }
                        if auction.excluded_bidders.contains(&bid.bidder_peer_id) {
                            debug!(%job_id, bidder = %bid.bidder_peer_id, "buyer: bid from an excluded worker ignored");
                            continue;
                        }
                        if !bid.price_usd.is_finite()
                            || bid.price_usd < 0.0
                            || bid.price_usd > offer.max_price_usd
                        {
                            warn!(%job_id, bidder = %bid.bidder_peer_id, price = bid.price_usd, cap = offer.max_price_usd, "buyer: discarding bid outside the offer cap");
                            continue;
                        }
                        if !bid.is_live(unix_ms()) {
                            debug!(%job_id, bidder = %bid.bidder_peer_id, "buyer: bid already expired, ignored");
                            continue;
                        }
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

    let received = bids.len();
    // A bid that expired while the window was open is no longer backed by
    // the worker's capacity; accepting it would overbook that worker.
    retain_live_bids(&mut bids, unix_ms());
    info!(%job_id, count = bids.len(), expired = received - bids.len(), "buyer: bid window closed");
    if bids.is_empty() {
        if received > 0 {
            bail!(
                "all {received} bids expired before the {:?} bid window closed",
                auction.bid_window
            );
        }
        bail!("no bids received within {:?}", auction.bid_window);
    }

    // ── pick winner: smart routing (most capable, not overloaded) or
    //    cheapest, per auction.policy; exact ties rotate across chunks. ─
    let tied = crate::routing::rank(&mut bids, auction.policy, auction.deadline.as_secs());
    if tied > 1 {
        let offset = NEXT_EQUAL_PRICE_BID.fetch_add(1, Ordering::Relaxed) % tied;
        bids[..tied].rotate_left(offset);
    }
    let winner = bids.remove(0);
    info!(
        %job_id,
        winner = %winner.bidder_peer_id,
        price = winner.price_usd,
        bench = ?winner.bench_score,
        load = ?winner.load,
        eta = winner.eta_seconds,
        policy = ?auction.policy,
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
        external_ref: auction.external_ref.clone(),
    };
    publish_json(&net, &topic, &accept).await?;
    if let Some(tx) = &auction.accept_notify {
        let _ = tx.send((job_id.clone(), winner.clone()));
    }

    // ── wait for receipt ────────────────────────────────────────────
    let recv_deadline = Instant::now() + auction.receipt_timeout;
    while Instant::now() < recv_deadline {
        let remaining = recv_deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(msg)) if msg.topic == topic => {
                if let Ok(receipt) = serde_json::from_slice::<JobReceipt>(&msg.data) {
                    if receipt.job_id == job_id && receipt.worker_peer_id == winner.bidder_peer_id {
                        if !source_matches_claim(&msg, &receipt.worker_peer_id) {
                            warn!(
                                %job_id,
                                claimed = %receipt.worker_peer_id,
                                "buyer: discarding receipt, gossip source does not match claimed worker_peer_id"
                            );
                            continue;
                        }
                        if !receipt_ref_matches(
                            auction.external_ref.as_deref(),
                            receipt.external_ref.as_deref(),
                        ) {
                            warn!(%job_id, "buyer: discarding receipt with a different external_ref");
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
                            external_ref: auction.external_ref.clone(),
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
pub(crate) fn source_matches_claim(msg: &GossipMessage, claimed_peer_id: &str) -> bool {
    match &msg.source {
        Some(source) => source.to_base58() == claimed_peer_id,
        None => false,
    }
}

/// Drop bids whose `expires_at_ms` has passed (bids from old workers carry
/// none and never expire).
pub(crate) fn retain_live_bids(bids: &mut Vec<JobBid>, now_ms: u64) {
    bids.retain(|bid| bid.is_live(now_ms));
}

/// A receipt's `external_ref` must be ours when present; workers older than
/// 0.2.49 do not echo it, so absence is accepted.
pub(crate) fn receipt_ref_matches(ours: Option<&str>, theirs: Option<&str>) -> bool {
    theirs.is_none() || theirs == ours
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

#[cfg(test)]
mod tests {
    use super::*;

    fn bid(peer: &str, expires_at_ms: Option<u64>) -> JobBid {
        JobBid {
            job_id: "j".into(),
            bidder_peer_id: peer.into(),
            bidder_did: None,
            price_usd: 0.1,
            eta_seconds: 60,
            published_at_ms: 0,
            expires_at_ms,
            bench_score: None,
            load: None,
        }
    }

    #[test]
    fn expired_bids_are_ignored() {
        let mut bids = vec![bid("old", None), bid("live", Some(2_000)), bid("stale", Some(1_000))];
        retain_live_bids(&mut bids, 1_500);
        let peers: Vec<_> = bids.iter().map(|b| b.bidder_peer_id.as_str()).collect();
        assert_eq!(peers, ["old", "live"]);
    }

    #[test]
    fn receipt_external_ref_must_match_when_echoed() {
        assert!(receipt_ref_matches(Some("inf:1"), Some("inf:1")));
        assert!(receipt_ref_matches(Some("inf:1"), None), "old worker");
        assert!(receipt_ref_matches(None, None));
        assert!(!receipt_ref_matches(Some("inf:1"), Some("inf:2")));
        assert!(!receipt_ref_matches(None, Some("inf:2")));
    }

    #[test]
    fn external_ref_builder() {
        let a = JobAuction::new("ffmpeg.transcode", serde_json::json!({}))
            .with_external_ref(Some("coinpay:inv_9".into()));
        assert_eq!(a.external_ref.as_deref(), Some("coinpay:inv_9"));
        assert_eq!(JobAuction::new("x", serde_json::json!({})).external_ref, None);
    }
}
