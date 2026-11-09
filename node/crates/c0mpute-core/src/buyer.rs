//! Buyer side of the auction: publish a `JobOffer`, collect `JobBid`s,
//! pick a winner, publish `JobAccept`, wait for `JobReceipt`.
//!
//! Every auction is end-to-end encrypted ([`crate::job_e2e`], DIP-0032): the
//! offer carries only a public spec and a fresh buyer X25519 key, bids carry
//! signed worker keys, the accept carries no spec, and the winner alone
//! fetches the spec sealed to its key over request-response. The outcome
//! holds the key that opens the worker's sealed result.

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
    /// Bids from these peer ids are kept back and used only when nobody else
    /// bid: workers benched for an earlier failure. A bench deprioritizes; it
    /// must not turn an auction its only capable bidder could win into "no
    /// bids" (dev2's gateway, 2026-11-09: 114 bids from two benched nodes
    /// dropped in six minutes while image and song auctions failed).
    pub fallback_bidders: Vec<String>,
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
    /// Require an attested TEE ([`c0mpute_net::topics::CONFIDENTIAL_TAG`]):
    /// only bids whose attestation verifies are considered, and the spec is
    /// sealed to the attested key only.
    pub confidential: bool,
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
            fallback_bidders: Vec::new(),
            external_ref: None,
            policy: crate::routing::SelectionPolicy::default(),
            accept_notify: None,
            confidential: false,
        }
    }

    pub fn with_confidential(mut self, confidential: bool) -> Self {
        self.confidential = confidential;
        self
    }

    pub fn with_accept_notify(
        mut self,
        tx: tokio::sync::mpsc::UnboundedSender<(String, JobBid)>,
    ) -> Self {
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

    pub fn with_fallback_bidders(mut self, peers: Vec<String>) -> Self {
        self.fallback_bidders = peers;
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

/// Why an auction ended without a result, for callers that map the outcome
/// to a status (the jobs bridge answers 503 for [`Self::NoBids`] and 504 for
/// [`Self::NoReceipt`]). Recover it with `err.downcast_ref::<AuctionFailure>()`.
#[derive(Debug, Clone)]
pub enum AuctionFailure {
    /// No usable bid arrived inside the bid window.
    NoBids(String),
    /// A winner was accepted but its receipt did not arrive in time.
    NoReceipt(String),
}

impl std::fmt::Display for AuctionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoBids(m) | Self::NoReceipt(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for AuctionFailure {}

pub struct AuctionOutcome {
    pub job_id: String,
    pub accepted_bid: JobBid,
    pub receipt: JobReceipt,
    /// The auction's `external_ref`, as the buyer set it. Use this rather
    /// than `receipt.external_ref`, which workers older than 0.2.49 omit.
    pub external_ref: Option<String>,
    /// Opens the result the worker sealed to us (the chunk the receipt's
    /// `output_hash` names is ciphertext). Never leaves this process.
    pub result_key: c0mpute_net::topics::JobKey,
}

impl AuctionOutcome {
    /// Open the sealed result bytes fetched by the receipt's hash.
    pub fn open_result(&self, sealed: &[u8]) -> Result<Vec<u8>> {
        crate::job_e2e::open_blob(&self.result_key, sealed)
    }

    /// Open a sealed result fetched to a file, in place.
    pub async fn open_result_file(&self, path: &std::path::Path) -> Result<u64> {
        crate::job_e2e::open_file_in_place(&self.result_key, path).await
    }
}

/// The offer's required tags: the caller's, plus the e2e tag (so pre-e2e
/// workers never bid) and the TEE tag for confidential work.
fn e2e_required_capabilities(caps: &[String], confidential: bool) -> Vec<String> {
    let mut caps = caps.to_vec();
    let mut want = vec![c0mpute_net::topics::JOB_E2E_TAG];
    if confidential {
        want.push(c0mpute_net::topics::CONFIDENTIAL_TAG);
    }
    for t in want {
        if !caps.iter().any(|c| c == t) {
            caps.push(t.to_string());
        }
    }
    caps
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
    let session = crate::job_e2e::BuyerSession::new(&auction.spec_inline, auction.confidential)?;
    let offer = JobOffer {
        job_id: job_id.clone(),
        workload_type: auction.workload_type.clone(),
        buyer_peer_id: net.peer_id().to_base58(),
        buyer_did: None,
        // A salted commitment: says nothing about the spec to observers.
        spec_hash: session.spec_hash(),
        spec_inline: None,
        required_capabilities: e2e_required_capabilities(&auction.required_capabilities, auction.confidential),
        max_price_usd: auction.max_price_usd,
        deadline_unix_ms: now_ms + auction.deadline.as_millis() as u64,
        published_at_ms: now_ms,
        external_ref: auction.external_ref.clone(),
        // Workers deliver bids straight to us over one of these as well as
        // on the gossip mesh, which drops them while it re-forms.
        buyer_addrs: offer_buyer_addrs(&net).await,
        bid_window_ms: Some(auction.bid_window.as_millis() as u64),
        e2e: Some(session.offer_part(&auction.workload_type, &auction.spec_inline)),
    };

    info!(
        %job_id,
        workload = %auction.workload_type,
        "buyer: publishing offer"
    );
    // An offer no peer could hear is an auction nobody bid on: report it the
    // same way, so a queued caller waits and re-offers instead of failing.
    if let Err(e) = publish_json(&net, &topic, &offer).await {
        crate::auction_stats::record_auction(&auction.workload_type, 0);
        return Err(AuctionFailure::NoBids(format!("the offer reached no peer: {e}")).into());
    }

    // ── collect bids ────────────────────────────────────────────────
    let deadline = Instant::now() + auction.bid_window;
    // Halfway through a silent window, publish the offer once more: a worker
    // that missed it (mesh re-forming) gets a second chance, and one that bid
    // re-sends the same bid without reserving more capacity.
    let mut republish_at = Some(Instant::now() + auction.bid_window / 2);
    let mut bids: Vec<JobBid> = Vec::new();
    let mut fallback: Vec<JobBid> = Vec::new();
    while Instant::now() < deadline {
        if let Some(at) = republish_at
            && bids.is_empty()
            && Instant::now() >= at
        {
            republish_at = None;
            debug!(%job_id, "buyer: no bids yet, re-publishing the offer");
            if let Err(e) = publish_json(&net, &topic, &offer).await {
                debug!(%job_id, err = %e, "buyer: offer re-publish failed");
            }
        }
        let until = match republish_at {
            Some(at) if bids.is_empty() => at.min(deadline),
            _ => deadline,
        };
        let remaining = until.saturating_duration_since(Instant::now());
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
                        // The same bid arrives directly and over gossip.
                        if bids.iter().chain(fallback.iter()).any(|b| b.bidder_peer_id == bid.bidder_peer_id) {
                            continue;
                        }
                        if auction.fallback_bidders.contains(&bid.bidder_peer_id) {
                            debug!(%job_id, bidder = %bid.bidder_peer_id, "buyer: bid from a benched worker kept as a fallback");
                            fallback.push(bid);
                            continue;
                        }
                        // Only a key the bidder signed for this job (and, for
                        // confidential work, attested) may receive the spec.
                        if let Err(e) = session.check_bid(&job_id, &auction.workload_type, &offer.buyer_peer_id, &bid) {
                            debug!(%job_id, bidder = %bid.bidder_peer_id, err = %e, "buyer: bid without a usable e2e key ignored");
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
            // Timeout: the window closed, or it is time to re-publish.
            Err(_) => continue,
        }
    }

    // Nobody else bid: a benched worker beats no worker at all.
    if bids.is_empty() && !fallback.is_empty() {
        info!(%job_id, count = fallback.len(), "buyer: only benched workers bid; using them");
        bids = fallback;
    }
    let received = bids.len();
    // A bid that expired while the window was open is no longer backed by
    // the worker's capacity; accepting it would overbook that worker.
    retain_live_bids(&mut bids, unix_ms());
    crate::auction_stats::record_auction(&auction.workload_type, bids.len());
    info!(%job_id, count = bids.len(), expired = received - bids.len(), "buyer: bid window closed");
    if bids.is_empty() {
        if received > 0 {
            return Err(AuctionFailure::NoBids(format!(
                "all {received} bids expired before the {:?} bid window closed",
                auction.bid_window
            ))
            .into());
        }
        return Err(AuctionFailure::NoBids(format!(
            "no bids received within {:?}",
            auction.bid_window
        ))
        .into());
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

    // ── seal the spec to the winner alone ───────────────────────────
    let winner_peer: c0mpute_net::PeerId = winner
        .bidder_peer_id
        .parse()
        .map_err(|e| anyhow::anyhow!("winner peer id: {e}"))?;
    let (sealed, result_key) =
        session.seal_for(&job_id, &auction.workload_type, &offer.buyer_peer_id, &winner)?;
    drop(session);
    // Held until this function returns (receipt or timeout); served only to
    // the winner over request-response, never on gossip.
    let _payload = crate::job_e2e::hold_payload(&net, &job_id, winner_peer, sealed);

    // ── publish accept ──────────────────────────────────────────────
    let accept = JobAccept {
        job_id: job_id.clone(),
        buyer_peer_id: offer.buyer_peer_id.clone(),
        winning_bidder_peer_id: winner.bidder_peer_id.clone(),
        agreed_price_usd: winner.price_usd,
        workload_type: auction.workload_type.clone(),
        spec_inline: None,
        published_at_ms: unix_ms(),
        external_ref: auction.external_ref.clone(),
        buyer_addrs: crate::reachability::buyer_external_addrs(
            std::env::var("C0MPUTE_P2P_EXTERNAL").ok().as_deref(),
            &offer.buyer_peer_id,
        ),
        e2e: Some(c0mpute_net::topics::JOB_E2E_VERSION),
        result_key: None,
    };
    // Tell the winner directly as well as on the topic: an accept lost on
    // the mesh used to leave the buyer waiting out the whole receipt timeout.
    let accept_bytes = serde_json::to_vec(&accept)?;
    let direct = async {
        match winner.bidder_peer_id.parse::<c0mpute_net::PeerId>() {
            Ok(peer) => {
                net.send_direct(peer, &topic, &accept_bytes, DIRECT_ACCEPT_TIMEOUT)
                    .await
            }
            Err(e) => Err(anyhow::anyhow!("winner peer id: {e}")),
        }
    };
    let (direct, gossip) = tokio::join!(direct, publish_json(&net, &topic, &accept));
    match (&direct, gossip) {
        (Ok(()), _) => debug!(%job_id, "buyer: accept delivered to the winner"),
        (Err(e), Ok(())) => debug!(%job_id, err = %e, "buyer: accept on gossip only"),
        (Err(e), Err(g)) => {
            warn!(%job_id, direct_err = %e, gossip_err = %g, "buyer: accept not delivered");
            return Err(g);
        }
    }
    if let Some(tx) = &auction.accept_notify {
        let _ = tx.send((job_id.clone(), winner.clone()));
    }

    // ── wait for receipt ────────────────────────────────────────────
    let receipt_timeout = receipt_wait(auction.receipt_timeout, &winner);
    let recv_deadline = Instant::now() + receipt_timeout;
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
                            result_key,
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

    Err(AuctionFailure::NoReceipt(format!(
        "no receipt from winner {} within {:?}",
        winner.bidder_peer_id, receipt_timeout
    ))
    .into())
}

/// [`run_auction`] that waits in a queue instead of failing when nobody
/// bids: re-offers with the gateway's backoff ([`crate::buyer_queue`], 5 s
/// doubling to 30 s) until a worker takes it or `max_wait` runs out, then
/// fails with `no worker for <workload> within <max_wait>`. `make` builds
/// each round's auction (a fresh job id each time); `on_wait` is told how
/// long it has waited before each pause, for a "queued" line.
pub async fn run_auction_queued(
    net: Arc<Libp2pNetwork>,
    make: impl Fn() -> JobAuction,
    max_wait: Duration,
    mut on_wait: impl FnMut(Duration),
) -> Result<AuctionOutcome> {
    let queue = crate::buyer_queue::QueueConfig::default();
    let started = Instant::now();
    let mut round = 0u32;
    loop {
        let auction = make();
        let workload = auction.workload_type.clone();
        match run_auction(net.clone(), auction).await {
            Err(e) if matches!(e.downcast_ref::<AuctionFailure>(), Some(AuctionFailure::NoBids(_))) => {
                let waited = started.elapsed();
                if waited >= max_wait {
                    bail!(crate::buyer_queue::expired_message(&workload, None, max_wait));
                }
                on_wait(waited);
                sleep(queue.backoff(round, max_wait - waited)).await;
                round += 1;
            }
            other => return other,
        }
    }
}

/// Canary probe: publish an offer for `workload` exactly as a buyer would,
/// count the distinct live bids that arrive within `window`, and accept none
/// (a bid reserves a worker only for the window plus a few seconds). Returns
/// the bid count; nothing runs anywhere.
pub async fn probe_auction(
    net: Arc<Libp2pNetwork>,
    workload: &str,
    spec_inline: serde_json::Value,
    required_capabilities: Vec<String>,
    window: Duration,
) -> Result<usize> {
    let topic = job_topic(workload);
    net.subscribe(&topic).await?;
    let mut rx = net.messages();
    let job_id = Uuid::new_v4().to_string();
    let now_ms = unix_ms();
    let session = crate::job_e2e::BuyerSession::new(&spec_inline, false)?;
    let offer = JobOffer {
        job_id: job_id.clone(),
        workload_type: workload.to_string(),
        buyer_peer_id: net.peer_id().to_base58(),
        buyer_did: None,
        spec_hash: session.spec_hash(),
        spec_inline: None,
        required_capabilities: e2e_required_capabilities(&required_capabilities, false),
        max_price_usd: 0.0,
        deadline_unix_ms: now_ms + 10 * 60 * 1000,
        published_at_ms: now_ms,
        external_ref: None,
        buyer_addrs: offer_buyer_addrs(&net).await,
        bid_window_ms: Some(window.as_millis() as u64),
        e2e: Some(session.offer_part(workload, &spec_inline)),
    };
    publish_json(&net, &topic, &offer).await?;
    let deadline = Instant::now() + window;
    let mut bidders: Vec<String> = Vec::new();
    while let Ok(next) =
        tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), rx.recv()).await
    {
        let msg = match next {
            Ok(m) => m,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => break,
        };
        if msg.topic != topic {
            continue;
        }
        if let Ok(bid) = serde_json::from_slice::<JobBid>(&msg.data)
            && bid.job_id == job_id
            && bid.is_live(unix_ms())
            && source_matches_claim(&msg, &bid.bidder_peer_id)
            && !bidders.contains(&bid.bidder_peer_id)
        {
            bidders.push(bid.bidder_peer_id);
        }
    }
    debug!(%job_id, workload, bids = bidders.len(), "buyer: canary probe closed");
    Ok(bidders.len())
}

/// Longest the buyer waits on the direct delivery of its accept.
const DIRECT_ACCEPT_TIMEOUT: Duration = Duration::from_secs(5);

/// Public addresses a worker can dial us on: `C0MPUTE_P2P_EXTERNAL` plus the
/// addresses the swarm confirmed as external, each ending in our peer id.
/// Workers keep only public IPs from these (`reachability::dialable_buyer_addrs`).
async fn offer_buyer_addrs(net: &Libp2pNetwork) -> Vec<String> {
    let me = net.peer_id().to_base58();
    let mut addrs = crate::reachability::buyer_external_addrs(
        std::env::var("C0MPUTE_P2P_EXTERNAL").ok().as_deref(),
        &me,
    );
    for addr in net.advertised_addrs().await {
        let s = addr.to_string();
        let full = if s.contains("/p2p/") { s } else { format!("{s}/p2p/{me}") };
        if !addrs.contains(&full)
            && !crate::reachability::dialable_buyer_addrs(std::slice::from_ref(&full), &me).is_empty()
        {
            addrs.push(full);
        }
    }
    addrs.truncate(crate::reachability::MAX_BUYER_ADDRS);
    addrs
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

/// Bench score a workload's receipt timeout is calibrated for (dev2's image
/// worker scores ~5,500 and draws a 512x512 image in ~2 min).
pub const REFERENCE_BENCH_SCORE: u32 = 5_000;

/// How long to wait for `winner`'s receipt: the auction's timeout, longer
/// for a winner that says it needs longer (2.5x its quoted ETA) or that
/// benchmarks slower than [`REFERENCE_BENCH_SCORE`] (scaled by how much
/// slower), never more than 4x the timeout. A slow CPU node (bench 1,606)
/// needed ~7 min per image against a 6 min timeout: it was benched for
/// "never delivered", the job re-queued and went straight back to it.
pub fn receipt_wait(base: Duration, winner: &JobBid) -> Duration {
    let by_eta = Duration::from_secs(winner.eta_seconds.saturating_mul(5) / 2);
    let by_bench = match winner.bench_score {
        Some(score) if score > 0 && score < REFERENCE_BENCH_SCORE => {
            base.mul_f64(REFERENCE_BENCH_SCORE as f64 / score as f64)
        }
        _ => base,
    };
    base.max(by_eta).max(by_bench).min(base.saturating_mul(4))
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
            e2e: None,
        }
    }

    #[test]
    fn expired_bids_are_ignored() {
        let mut bids = vec![
            bid("old", None),
            bid("live", Some(2_000)),
            bid("stale", Some(1_000)),
        ];
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
        assert_eq!(
            JobAuction::new("x", serde_json::json!({})).external_ref,
            None
        );
    }

    #[test]
    fn slow_or_busy_winners_get_longer_to_deliver() {
        let base = Duration::from_secs(360);
        let mut b = bid("w", None);
        b.eta_seconds = 60;
        b.bench_score = Some(5_560);
        assert_eq!(receipt_wait(base, &b), base, "a reference-speed node: the plain timeout");
        b.bench_score = Some(1_606);
        let slow = receipt_wait(base, &b);
        assert!(slow > Duration::from_secs(1100) && slow < Duration::from_secs(1200), "{slow:?}");
        b.bench_score = None;
        b.eta_seconds = 400;
        assert_eq!(receipt_wait(base, &b), Duration::from_secs(1000), "2.5x its own ETA");
        b.eta_seconds = 1_000_000;
        assert_eq!(receipt_wait(base, &b), base * 4, "capped");
    }

}
