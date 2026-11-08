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
    /// `c0mpute bench` score (geometric mean of measured throughput against a
    /// fixed reference core, ×1000). `None` until the operator has run a full
    /// benchmark. Defaults on decode so pre-bench peers still parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bench_score: Option<u32>,
}

impl CapabilityAd {
    pub fn now(peer_id: String, tags: Vec<String>, hardware: serde_json::Value) -> Self {
        Self::with_score(peer_id, tags, hardware, None)
    }

    pub fn with_score(
        peer_id: String,
        tags: Vec<String>,
        hardware: serde_json::Value,
        bench_score: Option<u32>,
    ) -> Self {
        Self {
            peer_id,
            tags,
            hardware,
            bench_score,
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
//
// Wire compatibility: nodes on the network run mixed versions. Every field
// added after the first release is an `Option` with
// `#[serde(default, skip_serializing_if = "Option::is_none")]`, so an old
// node parses a new message (serde ignores unknown fields; none of these
// structs use `deny_unknown_fields`) and a new node parses an old one.

/// Longest `external_ref` a node accepts or forwards.
pub const MAX_EXTERNAL_REF_LEN: usize = 128;

/// An `external_ref` is 1 to [`MAX_EXTERNAL_REF_LEN`] printable ASCII
/// characters with no spaces (an Infernet job id, a CoinPay invoice id, a
/// UUID). Anything else is refused rather than echoed back.
pub fn valid_external_ref(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_EXTERNAL_REF_LEN && s.bytes().all(|b| b.is_ascii_graphic())
}

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
    /// Workload spec carried inline. Phase 1: the entire job (input URL
    /// + ffmpeg preset, etc.) lives here so workers don't need a
    /// separate manifest fetch. Phase 2 (chunk-store transfer) flips
    /// this back to None and uses `spec_hash` + Libp2p request-response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec_inline: Option<serde_json::Value>,
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
    /// The job's id in the system that bills it (an Infernet job id, a
    /// CoinPay invoice/escrow id), set by the buyer. Carried unchanged on
    /// the `JobAccept` and echoed on the `JobReceipt`, so one job is
    /// identified once across c0mpute, Infernet and CoinPay.
    ///
    /// Billing systems MUST key on `external_ref`, not `job_id`: `job_id` is
    /// minted per auction, and a job that is re-auctioned (worker failed,
    /// result unreachable) gets a new `job_id` but keeps its `external_ref`.
    /// Keying on `job_id` would bill a retried job twice. Optional; old
    /// buyers and workers omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_ref: Option<String>,
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
    /// Unix-ms after which the worker no longer stands behind this bid (it
    /// may have committed the capacity elsewhere). Buyers ignore expired
    /// bids when picking a winner. `None` (old workers) never expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
    /// The worker's `c0mpute bench` score (the same one its capability ad
    /// carries). Buyers route to the most capable worker. `None` when never
    /// benchmarked or from workers older than 0.2.49.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bench_score: Option<u32>,
    /// How busy the worker is when it bids: 0.0 idle .. 1.0 saturated.
    /// Buyers skip overloaded workers when a free one bids.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load: Option<f32>,
}

impl JobBid {
    /// Whether the bid can still be accepted at `now_ms`.
    pub fn is_live(&self, now_ms: u64) -> bool {
        self.expires_at_ms.is_none_or(|t| now_ms < t)
    }
}

/// Buyer's accept message. Names the winning bidder; other bidders see
/// this and stop tracking the job.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct JobAccept {
    pub job_id: String,
    pub buyer_peer_id: String,
    pub winning_bidder_peer_id: String,
    pub agreed_price_usd: f64,
    /// Workload type this accept is for (lets workers route to the
    /// right runner without rebuilding the topic from scratch).
    pub workload_type: String,
    /// Same shape as `JobOffer.spec_inline` — the actual workload
    /// spec, repeated here so the winning worker doesn't need to keep
    /// every offer it bid on cached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec_inline: Option<serde_json::Value>,
    pub published_at_ms: u64,
    /// Same value as `JobOffer.external_ref`. Billing keys on this; see
    /// [`JobOffer::external_ref`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_ref: Option<String>,
    /// The buyer's publicly dialable multiaddrs (`.../p2p/<buyer_peer_id>`).
    /// A winning worker behind NAT dials one of these before publishing its
    /// receipt, so the buyer can fetch the result over that connection; the
    /// buyer usually cannot dial a worker's random, firewalled port.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub buyer_addrs: Vec<String>,
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
    /// Echo of `JobAccept.external_ref`, so a billing system settling from
    /// the receipt alone sees the id it issued. Billing keys on this; see
    /// [`JobOffer::external_ref`]. `None` from workers older than 0.2.49.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_ref: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Completed,
    Failed,
    Timeout,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_offer_without_external_ref_still_parses() {
        let old = r#"{"job_id":"j","workload_type":"ffmpeg.transcode","buyer_peer_id":"b","buyer_did":null,
            "spec_hash":"h","required_capabilities":[],"max_price_usd":1.0,"deadline_unix_ms":2,"published_at_ms":1}"#;
        let offer: JobOffer = serde_json::from_str(old).unwrap();
        assert_eq!(offer.external_ref, None);
        // And a new offer omits the field entirely when unset, so the bytes
        // an old node sees are unchanged.
        assert!(
            !serde_json::to_string(&offer)
                .unwrap()
                .contains("external_ref")
        );
    }

    #[test]
    fn new_fields_round_trip_and_old_shapes_ignore_them() {
        let bid = JobBid {
            job_id: "j".into(),
            bidder_peer_id: "w".into(),
            bidder_did: None,
            price_usd: 0.1,
            eta_seconds: 30,
            published_at_ms: 1,
            expires_at_ms: Some(1000),
            bench_score: None,
            load: None,
        };
        let json = serde_json::to_value(&bid).unwrap();
        assert_eq!(json["expires_at_ms"], 1000);
        // A struct without the new field (an old node) parses it fine.
        #[derive(serde::Deserialize)]
        #[allow(dead_code)]
        struct OldBid {
            job_id: String,
            bidder_peer_id: String,
            bidder_did: Option<String>,
            price_usd: f64,
            eta_seconds: u64,
            published_at_ms: u64,
        }
        let _: OldBid = serde_json::from_value(json).unwrap();

        let receipt = JobReceipt {
            job_id: "j".into(),
            worker_peer_id: "w".into(),
            worker_did: None,
            buyer_peer_id: "b".into(),
            output_hash: None,
            status: JobStatus::Completed,
            completed_at_ms: 0,
            signature: None,
            external_ref: Some("infernet:job_42".into()),
        };
        let back: JobReceipt =
            serde_json::from_slice(&serde_json::to_vec(&receipt).unwrap()).unwrap();
        assert_eq!(back.external_ref.as_deref(), Some("infernet:job_42"));
    }

    #[test]
    fn bid_expiry() {
        let mut bid = JobBid {
            job_id: "j".into(),
            bidder_peer_id: "w".into(),
            bidder_did: None,
            price_usd: 0.1,
            eta_seconds: 30,
            published_at_ms: 1,
            expires_at_ms: None,
            bench_score: None,
            load: None,
        };
        assert!(bid.is_live(u64::MAX), "old bids never expire");
        bid.expires_at_ms = Some(100);
        assert!(bid.is_live(99));
        assert!(!bid.is_live(100));
    }

    #[test]
    fn external_ref_validation() {
        assert!(valid_external_ref("infernet:job_42"));
        assert!(valid_external_ref("6f1c3e2a-8a39-4a0e-9a59-2c3b1f0e7d11"));
        assert!(!valid_external_ref(""));
        assert!(!valid_external_ref("has space"));
        assert!(!valid_external_ref("line\nbreak"));
        assert!(!valid_external_ref(&"x".repeat(MAX_EXTERNAL_REF_LEN + 1)));
    }
}
