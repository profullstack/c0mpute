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
//!   5. Private mode ([`crate::private`]): reject unless the signed
//!      publisher is the offer's buyer and that buyer is trusted.
//!   6. Otherwise: send a `JobBid` straight to the buyer
//!      ([`Libp2pNetwork::send_direct`]) and publish it on the same topic,
//!      with an ETA from queue depth x recent job time and an `expires_at_ms`.
//!      Either path reaching the buyer is enough; the gossip mesh alone lost
//!      every bid from NAT'd workers while it re-formed (bonita, 2026-11-09:
//!      57 of 57 bids published, 0 received).
//!
//! A worker that is draining for an update ([`c0mpute_update::draining`])
//! does not bid at all.
//!
//! End-to-end encrypted offers ([`crate::job_e2e`], DIP-0032) carry only a
//! public spec: filters judge that at bid time, and the bid carries a fresh
//! X25519 key signed by this node's identity. On winning, the worker fetches
//! the sealed spec from the buyer point to point, opens it, checks it
//! against the offer's commitment and public spec, runs the workload's filter
//! again on the whole spec, and only then hands the job to the runner (with
//! the key its result is sealed back under).
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
const ACCEPT_GRACE_MS: u64 = 10_000;

/// How long past the buyer's bid window a bid keeps reserving capacity. The
/// buyer picks its winner when the window closes; after that a bid we never
/// heard back on is almost certainly lost, and holding the slot for the full
/// [`BID_TTL`] stopped nodes bidding at all ("at capacity" with every slot
/// free). A late accept is still honoured; the runner queues it.
const RESERVE_GRACE_MS: u64 = 10_000;

/// Bid window assumed for offers from buyers that do not state one.
const DEFAULT_OFFER_WINDOW_MS: u64 = 8_000;

/// Longest a bid waits on its direct delivery to the buyer.
const DIRECT_BID_TIMEOUT: Duration = Duration::from_secs(3);

/// Accepts already handed to the runner, remembered so the same accept
/// arriving twice (direct and over gossip) runs once.
const MAX_SEEN_ACCEPTS: usize = 256;

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
        let mut seen_accepts: std::collections::VecDeque<String> =
            std::collections::VecDeque::new();
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
                // Plaintext offers are served for one release more (DIP-0032).
                if offer.e2e.is_none() && !crate::job_e2e::serves_plaintext_offers() {
                    debug!(job_id = %offer.job_id, "dispatch: plaintext offer refused (e2e only)");
                    continue;
                }
                // Private mode: only trusted buyers (by signed publisher).
                if !crate::private::current().admits(&msg, &offer, &our_peer_id) {
                    debug!(job_id = %offer.job_id, buyer = %offer.buyer_peer_id, "dispatch: private mode, buyer not trusted");
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
                // Restarting into an update: finish what we hold, take nothing new.
                if c0mpute_update::draining() {
                    debug!(job_id = %offer.job_id, "dispatch: draining for an update, not bidding");
                    continue;
                }
                let now = unix_ms();
                open_bids.retain(|_, b| b.honourable(now));
                // The buyer re-sent an offer we already bid on (it heard
                // nothing): re-send the same bid, without reserving more.
                if let Some(bid) = open_bids.get(&offer.job_id) {
                    if bid.reserves(now) {
                        if let Some(payload) = bid.payload.clone() {
                            spawn_deliver_bid(net.clone(), topic.clone(), &offer, payload);
                        }
                        continue;
                    }
                }
                let reserving = open_bids.values().filter(|b| b.reserves(now)).count();
                let capacity = Capacity::of(tx, load.as_ref());
                if !capacity.can_bid(reserving) {
                    debug!(
                        job_id = %offer.job_id,
                        open_bids = reserving,
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
                if let Some((price, expires_at_ms, payload, session)) = handle_offer(
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
                    // Idle-donation workloads stand down while a bid stands.
                    crate::activity::note_bid(expires_at_ms);
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
                            reserve_until_ms: reserve_until(&offer, now, expires_at_ms),
                            payload: Some(payload),
                            e2e: session.map(|s| (Arc::new(s), Arc::new(offer.clone()))),
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
                // The same accept arrives directly and over gossip.
                if seen_accepts.contains(&accept.job_id) {
                    continue;
                }
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
                if seen_accepts.len() >= MAX_SEEN_ACCEPTS {
                    seen_accepts.pop_front();
                }
                seen_accepts.push_back(accept.job_id.clone());
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
                crate::activity::job_started(&accept.job_id);
                if let Some((session, offer)) = bid.e2e {
                    // Fetching the sealed spec takes a round trip to the
                    // buyer: never in the loop that reads offers.
                    spawn_open_e2e(net.clone(), tx.clone(), filter.clone(), session, offer, accept);
                    continue;
                }
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
    /// Until when this bid counts against our capacity (see
    /// [`RESERVE_GRACE_MS`]); never past `expires_at_ms + ACCEPT_GRACE_MS`.
    pub reserve_until_ms: u64,
    /// The serialized bid, re-sent if the buyer re-sends its offer.
    pub payload: Option<Vec<u8>>,
    /// For a bid on an e2e offer: our X25519 secret for it and the offer.
    pub e2e: Option<(Arc<crate::job_e2e::WorkerSession>, Arc<JobOffer>)>,
}

impl OpenBid {
    /// Whether an accept for this bid would still be honoured.
    pub(crate) fn honourable(&self, now_ms: u64) -> bool {
        now_ms <= self.expires_at_ms.saturating_add(ACCEPT_GRACE_MS)
    }

    /// Whether this bid still counts against our capacity.
    pub(crate) fn reserves(&self, now_ms: u64) -> bool {
        now_ms <= self.reserve_until_ms && self.honourable(now_ms)
    }

    /// An accept is only honoured when it comes from the offer's buyer,
    /// carries exactly the spec and `external_ref` we bid on, stays within
    /// the cap, and arrives while the bid stands.
    pub(crate) fn matches(&self, accept: &JobAccept, now_ms: u64) -> Result<(), &'static str> {
        if accept.buyer_peer_id != self.buyer_peer_id {
            return Err("buyer differs from the offer");
        }
        if self.e2e.is_some() {
            // The spec comes sealed after the accept, never on it.
            if accept.e2e != Some(c0mpute_net::topics::JOB_E2E_VERSION) || accept.spec_inline.is_some() {
                return Err("e2e offer answered by a plaintext accept");
            }
        } else if accept.spec_inline != self.spec_inline {
            return Err("spec differs from the offer we bid on");
        }
        if accept.external_ref != self.external_ref {
            return Err("external_ref differs from the offer we bid on");
        }
        if !self.honourable(now_ms) {
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
        let stale = self
            .read_at
            .is_none_or(|t| t.elapsed() > Duration::from_secs(600));
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

/// Until when a bid placed at `now_ms` on `offer` reserves capacity: the
/// buyer's bid window (as stated on the offer, else the default) plus
/// [`RESERVE_GRACE_MS`], never past the bid's own honour period.
pub(crate) fn reserve_until(offer: &JobOffer, now_ms: u64, expires_at_ms: u64) -> u64 {
    let window = offer
        .bid_window_ms
        .unwrap_or(DEFAULT_OFFER_WINDOW_MS)
        .min(BID_TTL.as_millis() as u64);
    // The window runs from when the buyer published, not from when we saw it.
    let window_end = offer.published_at_ms.max(now_ms.saturating_sub(window)) + window;
    (window_end + RESERVE_GRACE_MS).min(expires_at_ms.saturating_add(ACCEPT_GRACE_MS))
}

/// Send a bid to the buyer over both paths, in the background so the offer
/// loop never waits on a slow dial: straight to the buyer (dialing one of the
/// public addresses it put on the offer, else by peer id), and on the gossip
/// topic. Logs which path worked.
fn spawn_deliver_bid(net: Arc<Libp2pNetwork>, topic: String, offer: &JobOffer, payload: Vec<u8>) {
    let job_id = offer.job_id.clone();
    let buyer = offer.buyer_peer_id.parse::<c0mpute_net::PeerId>().ok();
    let addrs = crate::reachability::dialable_buyer_addrs(&offer.buyer_addrs, &offer.buyer_peer_id);
    tokio::spawn(async move {
        let gossip = net.publish(&topic, payload.clone()).await;
        let mut direct = Err(anyhow::anyhow!("buyer peer id does not parse"));
        if let Some(buyer) = buyer {
            for addr in addrs {
                net.add_peer_address(buyer, without_p2p(addr)).await;
            }
            for attempt in 0..2 {
                let started = std::time::Instant::now();
                direct = net
                    .send_direct(buyer, &topic, &payload, DIRECT_BID_TIMEOUT)
                    .await;
                // A quick failure (dial refused, no route yet) is worth one
                // more try inside the buyer's window; a timeout is not.
                if direct.is_ok() || attempt == 1 || started.elapsed() > Duration::from_secs(1) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        }
        match (&gossip, &direct) {
            (_, Ok(())) => info!(%job_id, gossip = gossip.is_ok(), "dispatch: bid delivered to the buyer"),
            (Ok(()), Err(e)) => info!(%job_id, direct_err = %e, "dispatch: bid published on gossip only"),
            (Err(g), Err(d)) => warn!(%job_id, gossip_err = %g, direct_err = %d, "dispatch: bid not delivered on either path"),
        }
    });
}

/// A multiaddr without its trailing `/p2p/<id>` (Kad keys addresses by peer).
fn without_p2p(mut addr: c0mpute_net::Multiaddr) -> c0mpute_net::Multiaddr {
    if matches!(addr.iter().last(), Some(c0mpute_net::Protocol::P2p(_))) {
        addr.pop();
    }
    addr
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

async fn handle_offer(
    net: &Arc<Libp2pNetwork>,
    topic: &str,
    offer: &JobOffer,
    advertised_tags: &[String],
    pricing: Option<&PricingConfig>,
    load: f64,
    market: &MarketLearner,
    eta_seconds: u64,
    bench_score: Option<u32>,
    bid_load: f32,
) -> Option<(f64, u64, Vec<u8>, Option<crate::job_e2e::WorkerSession>)> {
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
        .filter(|c| !crate::job_e2e::implicit_tag(c) && !advertised_tags.iter().any(|a| a == *c))
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
    // A fresh X25519 key for this one job, signed by our identity.
    let (session, e2e) = match offer.e2e.as_ref() {
        Some(_) => match crate::job_e2e::WorkerSession::for_offer(net, offer) {
            Ok((s, e)) => (Some(s), Some(e)),
            Err(e) => {
                debug!(job_id = %offer.job_id, err = %e, "dispatch: cannot bid on this e2e offer");
                return None;
            }
        },
        None => (None, None),
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
        e2e,
    };
    let payload = match serde_json::to_vec(&bid) {
        Ok(b) => b,
        Err(e) => {
            warn!(err = %e, "dispatch: serialize bid");
            return None;
        }
    };
    info!(
        job_id = %offer.job_id,
        price = bid.price_usd,
        eta_s = bid.eta_seconds,
        "dispatch: bid placed"
    );
    spawn_deliver_bid(net.clone(), topic.to_string(), offer, payload.clone());
    Some((price_usd, expires_at_ms, payload, session))
}

/// Won an e2e job: fetch and open the sealed spec, check it, re-run the
/// workload's filter on the whole spec, then hand the job to the runner with
/// its result key. Any failure ends the job with a Failed receipt, so the
/// buyer re-auctions at once instead of waiting out its timeout. Nothing
/// about the spec's content is logged.
fn spawn_open_e2e(
    net: Arc<Libp2pNetwork>,
    tx: mpsc::Sender<AcceptedJob>,
    filter: Option<OfferFilter>,
    session: Arc<crate::job_e2e::WorkerSession>,
    offer: Arc<JobOffer>,
    mut accept: JobAccept,
) {
    tokio::spawn(async move {
        let opened = session.open_accept(&net, &offer, &accept).await;
        drop(session);
        let refused = match opened {
            Ok((spec, key)) => {
                let full = crate::job_e2e::opened_offer(&offer, spec);
                if filter.as_ref().is_none_or(|f| f(&full)) {
                    accept.spec_inline = full.spec_inline;
                    accept.result_key = Some(key);
                    if let Err(e) = tx.send(AcceptedJob { accept }).await {
                        warn!(err = %e, "dispatch: runner channel closed");
                    }
                    return;
                }
                "the sealed spec does not pass this workload's checks".to_string()
            }
            Err(e) => format!("{e:#}"),
        };
        warn!(job_id = %accept.job_id, reason = %refused, "dispatch: won e2e job refused");
        crate::runner::publish_receipt(&net, &accept, None, c0mpute_net::topics::JobStatus::Failed).await;
    });
}

/// Workload types this worker should listen for, derived from its
/// configured roles. Each modality maps to a separate job topic.
/// `infernet.inference` is served natively from the box's own
/// OpenAI-compatible runtime ([`crate::inference`]).
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
    if roles.contains(&c0mpute_proto::Role::WorkflowStep) {
        types.push(crate::workflow::WORKLOAD.to_string());
    }
    if roles.contains(&c0mpute_proto::Role::Video) {
        types.push(crate::video::WORKLOAD_TXT2VID.to_string());
        types.push(crate::video::WORKLOAD_IMG2VID.to_string());
    }
    if roles.contains(&c0mpute_proto::Role::Diffusion) {
        types.push(crate::diffusion::WORKLOAD_TXT2IMG.to_string());
    }
    if roles.contains(&c0mpute_proto::Role::Music) {
        types.push(crate::music::WORKLOAD_TXT2MUSIC.to_string());
    }
    if roles.contains(&c0mpute_proto::Role::Crawl) {
        types.push(crate::crawl::WORKLOAD.to_string());
    }
    if roles.contains(&c0mpute_proto::Role::Ocr) {
        types.push(crate::ocr::WORKLOAD.to_string());
    }
    if roles.contains(&c0mpute_proto::Role::Gameserver) {
        types.push(crate::gameserver::WORKLOAD_START.to_string());
        types.push(crate::gameserver::WORKLOAD_STOP.to_string());
    }
    if roles.contains(&c0mpute_proto::Role::Embed) {
        types.push(crate::embed::WORKLOAD.to_string());
    }
    if roles.contains(&c0mpute_proto::Role::Captcha) {
        types.push(crate::captcha::WORKLOAD.to_string());
    }
    if roles.contains(&c0mpute_proto::Role::Inference) {
        types.push(crate::inference::WORKLOAD.to_string());
    }
    if roles.contains(&c0mpute_proto::Role::Render) {
        types.push(crate::render::WORKLOAD.to_string());
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
            reserve_until_ms: NOW + 18_000,
            payload: None,
            e2e: None,
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
            buyer_addrs: Vec::new(),
            e2e: None,
            result_key: None,
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
    fn e2e_bids_take_only_e2e_accepts_without_a_spec() {
        let mut b = bid();
        b.spec_inline = None;
        let offer = offer_at(NOW, None);
        // A session is only needed as a marker here.
        let session = std::sync::Arc::new(test_session());
        b.e2e = Some((session, std::sync::Arc::new(offer)));
        let mut a = accept();
        // A plaintext accept for an e2e bid (a downgrade) is refused.
        assert!(b.matches(&a, NOW).is_err());
        a.spec_inline = None;
        assert!(b.matches(&a, NOW).is_err(), "missing e2e version");
        a.e2e = Some(c0mpute_net::topics::JOB_E2E_VERSION);
        assert!(b.matches(&a, NOW).is_ok());
        // Any spec on an e2e accept is refused, even with the version set.
        a.spec_inline = Some(serde_json::json!({"x": 1}));
        assert!(b.matches(&a, NOW).is_err());
    }

    fn test_session() -> crate::job_e2e::WorkerSession {
        crate::job_e2e::WorkerSession::for_tests()
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
        assert!(
            b.matches(&accept(), b.expires_at_ms + ACCEPT_GRACE_MS)
                .is_ok()
        );
        assert!(
            b.matches(&accept(), b.expires_at_ms + ACCEPT_GRACE_MS + 1)
                .is_err()
        );
        assert!(!b.honourable(b.expires_at_ms + ACCEPT_GRACE_MS + 1));
    }

    #[test]
    fn a_bid_stops_reserving_capacity_after_the_buyers_window() {
        let b = bid();
        assert!(b.reserves(NOW));
        assert!(b.reserves(b.reserve_until_ms));
        assert!(!b.reserves(b.reserve_until_ms + 1));
        // ...but an accept arriving later is still honoured (the runner
        // queues it).
        assert!(b.matches(&accept(), b.reserve_until_ms + 1).is_ok());
    }

    fn offer_at(published_at_ms: u64, window: Option<u64>) -> JobOffer {
        JobOffer {
            job_id: "j".into(),
            workload_type: "diffusion.txt2img".into(),
            buyer_peer_id: "buyer".into(),
            buyer_did: None,
            spec_hash: String::new(),
            spec_inline: None,
            required_capabilities: vec![],
            max_price_usd: 1.0,
            deadline_unix_ms: u64::MAX,
            published_at_ms,
            external_ref: None,
            buyer_addrs: Vec::new(),
            bid_window_ms: window,
            e2e: None,
        }
    }

    #[test]
    fn reservation_follows_the_offers_bid_window() {
        let expires = NOW + BID_TTL.as_millis() as u64;
        // Stated 4 s window, seen 1 s after publishing: reserved until the
        // window closes plus the grace.
        assert_eq!(
            reserve_until(&offer_at(NOW - 1_000, Some(4_000)), NOW, expires),
            NOW + 3_000 + RESERVE_GRACE_MS
        );
        // Old buyers state none: the default window.
        assert_eq!(
            reserve_until(&offer_at(NOW, None), NOW, expires),
            NOW + DEFAULT_OFFER_WINDOW_MS + RESERVE_GRACE_MS
        );
        // An absurd window or a future publish time never reserves past the
        // bid's own honour period.
        assert_eq!(
            reserve_until(&offer_at(NOW, Some(u64::MAX)), NOW, expires),
            expires + ACCEPT_GRACE_MS
        );
        assert_eq!(
            reserve_until(&offer_at(u64::MAX / 2, Some(4_000)), NOW, expires),
            expires + ACCEPT_GRACE_MS
        );
    }

    #[test]
    fn p2p_suffix_is_stripped_for_kad() {
        let a: c0mpute_net::Multiaddr = "/ip4/1.2.3.4/tcp/5/p2p/12D3KooWS79f5QBjruwxK97TpSBiwTZ9gG7LoVUAeybDT9ic7cgu"
            .parse()
            .unwrap();
        assert_eq!(without_p2p(a).to_string(), "/ip4/1.2.3.4/tcp/5");
        let b: c0mpute_net::Multiaddr = "/ip4/1.2.3.4/tcp/5".parse().unwrap();
        assert_eq!(without_p2p(b.clone()), b);
    }

    fn cap(queued: usize, queue_free: usize, free_slots: Option<usize>) -> Capacity {
        Capacity {
            queued,
            queue_free,
            free_slots,
            total_slots: 4,
        }
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
        assert!(
            !c.can_bid(3),
            "three open bids already cover three free slots"
        );
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
