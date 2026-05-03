//! Gossipsub topic conventions for c0mpute.
//!
//! Three topic families today:
//!
//!   c0mpute/cap/v1                  — capability advertisements
//!                                     (workers broadcast what they can do)
//!   c0mpute/jobs/<workload-type>    — job dispatch
//!                                     (buyers post jobs; workers claim)
//!   c0mpute/heartbeat/v1            — liveness
//!                                     (every N seconds per worker)
//!
//! Topic identifiers are stable strings; workloads are namespaced by type
//! (e.g. `c0mpute/jobs/ffmpeg.transcode`, `c0mpute/jobs/infernet.inference`).

pub const CAPABILITY_TOPIC: &str = "c0mpute/cap/v1";
pub const HEARTBEAT_TOPIC: &str = "c0mpute/heartbeat/v1";

/// Build the topic name for a workload type.
pub fn job_topic(workload_type: &str) -> String {
    format!("c0mpute/jobs/{workload_type}")
}

/// Capability advertisement payload. Signed by the worker's CoinPay DID
/// once that lands; today we rely on gossipsub message authenticity
/// (libp2p signs each pubsub message with the publisher's identity key).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CapabilityAd {
    /// Worker's libp2p peer-id, hex-encoded.
    pub peer_id: String,
    /// Capability tags this worker advertises, e.g.
    /// `["c0mpute:transcode:h264:nvenc", "c0mpute:gpu:nvidia"]`.
    pub tags: Vec<String>,
    /// Free-disk / free-VRAM / region etc. — opaque JSON for now.
    pub hardware: serde_json::Value,
    /// Unix-ms when this ad was created. Older than ~5min = ignore.
    pub published_at_ms: u64,
}

impl CapabilityAd {
    pub fn now(peer_id: String, tags: Vec<String>, hardware: serde_json::Value) -> Self {
        Self {
            peer_id,
            tags,
            hardware,
            published_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        }
    }
}

// ────────────────────────────────────────────────────────────────────────
// Job dispatch (DIP-0011, DIP-0006)
// ────────────────────────────────────────────────────────────────────────

/// Buyer publishes this to `c0mpute/jobs/<workload-type>` to start the
/// auction. The actual job spec is opaque to the network — workers
/// evaluate by `workload_type` + `spec_hash` + price/deadline. Inputs +
/// secrets are referenced by URL or content hash inside `spec`,
/// optionally end-to-end encrypted.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct JobOffer {
    /// UUID minted by the buyer.
    pub job_id: String,
    /// `ffmpeg.transcode`, `infernet.inference`, etc.
    pub workload_type: String,
    /// Buyer's libp2p peer-id (so workers can reply via direct dial /
    /// request-response in Phase 3).
    pub buyer_peer_id: String,
    /// Buyer's CoinPay DID (proof of funds + reputation; checked by
    /// workers before bidding).
    pub buyer_did: Option<String>,
    /// Hash of the actual job manifest. The manifest itself is fetched
    /// out-of-band (signed URL, content-addressed network, etc.) once
    /// the auction is settled.
    pub spec_hash: String,
    /// Capability tags the worker MUST advertise to be eligible. The
    /// dispatcher filters by these before considering bids.
    pub required_capabilities: Vec<String>,
    /// Maximum price the buyer will pay, in USD-equivalent.
    pub max_price_usd: f64,
    /// Wall-clock deadline for the job (unix-ms).
    pub deadline_unix_ms: u64,
    /// When the offer was posted (unix-ms). Workers reject offers older
    /// than ~5 min.
    pub published_at_ms: u64,
}

/// Worker bid in response to a `JobOffer`. Sent on the same job topic;
/// the buyer collects bids until `bid_window_ms` elapses then accepts
/// the best one.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct JobBid {
    pub job_id: String,
    pub bidder_peer_id: String,
    /// Worker's CoinPay DID — buyer can look up reputation + stake.
    pub bidder_did: Option<String>,
    /// Worker's quote in USD-equivalent. Must be ≤ JobOffer.max_price_usd.
    pub price_usd: f64,
    /// Estimated seconds to completion.
    pub eta_seconds: u64,
    pub published_at_ms: u64,
}

/// Buyer's accept message. Names the winning bidder; other bidders see
/// this and stop tracking the job.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct JobAccept {
    pub job_id: String,
    pub buyer_peer_id: String,
    pub winning_bidder_peer_id: String,
    pub agreed_price_usd: f64,
    pub published_at_ms: u64,
}

/// Worker's completion receipt. Includes the output hash so the buyer
/// can verify against expectation, plus the signed evidence the
/// validator + CoinPay use to settle the escrow.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct JobReceipt {
    pub job_id: String,
    pub worker_peer_id: String,
    pub worker_did: Option<String>,
    pub buyer_peer_id: String,
    pub output_hash: Option<String>,
    pub status: JobStatus,
    pub completed_at_ms: u64,
    /// Signed by the worker's CoinPay DID. Verified by buyer + validator.
    /// Today this is opaque bytes; signature scheme lives in coinpay.
    #[serde(default)]
    pub signature: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Completed,
    Failed,
    Timeout,
}
