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

// ────────────────────────────────────────────────────────────────────────
// End-to-end encrypted jobs (DIP-0032)
// ────────────────────────────────────────────────────────────────────────
//
// Gossip topics are readable by every subscriber and relay. An e2e offer
// carries no spec, only what bidding needs (`OfferE2e::public_spec`) and the
// buyer's fresh X25519 key. A bid carries the worker's fresh X25519 key,
// signed by the worker's libp2p identity. After the award the buyer seals
// the spec to the winner alone and hands it over point to point
// (request-response); the result comes back sealed to the buyer.

/// Version of the e2e job envelope this build speaks.
pub const JOB_E2E_VERSION: u8 = 1;

/// Capability tag every e2e-capable worker satisfies. Buyers list it in
/// `required_capabilities`, so a worker older than e2e (which checks that it
/// advertises every required tag) never bids on an e2e offer.
pub const JOB_E2E_TAG: &str = "c0mpute:e2e:v1";

/// Capability tag for a worker running inside an attested TEE (AMD SEV-SNP,
/// Intel TDX). An offer that requires it only matches workers that attach
/// attestation evidence to their bid; see [`BidE2e::attestation`].
pub const CONFIDENTIAL_TAG: &str = "c0mpute:confidential";

/// The e2e part of a [`JobOffer`].
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct OfferE2e {
    /// [`JOB_E2E_VERSION`].
    pub v: u8,
    /// The buyer's X25519 public key for this one auction, lowercase hex.
    pub buyer_key: String,
    /// What a worker needs to decide whether to bid (model, engine, size
    /// buckets...). Never prompts, inputs, URLs or file names.
    #[serde(default)]
    pub public_spec: serde_json::Value,
    /// Present when the buyer requires an attested TEE: the challenge the
    /// worker's attestation report must bind (with its X25519 key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attest_nonce: Option<String>,
    /// The buyer's listen addresses, so the winner can dial it for the
    /// sealed spec even when they share no public address (same host or
    /// LAN). A worker only uses one in an address class it listens in too.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addrs: Vec<String>,
}

/// The e2e part of a [`JobBid`].
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BidE2e {
    /// The worker's X25519 public key for this one job, lowercase hex.
    pub worker_key: String,
    /// The worker's libp2p identity signature (base64) over the binding of
    /// `worker_key` to this job, buyer and buyer key. The buyer checks it
    /// against `bidder_peer_id` before sealing anything to the key.
    pub sig: String,
    /// Remote attestation evidence (format named inside), for offers that
    /// require [`CONFIDENTIAL_TAG`]. Its report data binds `worker_key`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attestation: Option<serde_json::Value>,
    /// The worker's listen addresses, for the buyer's direct accept (same
    /// rule as [`OfferE2e::addrs`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addrs: Vec<String>,
}

/// A 32-byte job key held in memory only: never serialized, never printed,
/// wiped on drop.
#[derive(Clone, PartialEq, Eq)]
pub struct JobKey(pub [u8; 32]);

impl std::fmt::Debug for JobKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("JobKey(<redacted>)")
    }
}

impl Drop for JobKey {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.0);
    }
}

/// Buyer publishes this to `c0mpute/jobs/<workload-type>` to start the
/// auction. An e2e offer (`e2e` set, `spec_inline` empty) shows the network
/// only what bidding needs; see the e2e section above.
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
    /// Public multiaddrs the buyer can be dialed on, each ending in
    /// `/p2p/<buyer_peer_id>`. A worker delivers its bid straight to the
    /// buyer over one of them (`Libp2pNetwork::send_direct`) as well as on
    /// the gossip mesh, which drops bids while it re-forms. Optional; old
    /// buyers omit it and workers then dial the buyer by peer id.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub buyer_addrs: Vec<String>,
    /// How long the buyer collects bids. A worker reserves capacity for a
    /// bid about this long (not the full bid TTL), so losing an auction it
    /// never hears the result of does not stop it bidding on the next one.
    /// Optional; old buyers omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bid_window_ms: Option<u64>,
    /// End-to-end encryption (DIP-0032). When set, `spec_inline` is empty
    /// and `spec_hash` is a salted commitment to the spec, not its hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub e2e: Option<OfferE2e>,
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
    /// The worker's signed X25519 key, on bids for e2e offers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub e2e: Option<BidE2e>,
    /// The bidding process's instance id ([`crate::Libp2pNetwork::instance_id`]).
    /// Two processes running one identity (a copied identity dir, or a
    /// system and a user service on one box) bid under one peer id with
    /// different instance ids; buyers keep the newest and name the winner's
    /// instance on the accept. `None` from workers older than 0.3.18.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
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
    /// [`JOB_E2E_VERSION`] when the job is e2e: `spec_inline` is empty and
    /// the winner fetches the sealed spec from the buyer point to point.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub e2e: Option<u8>,
    /// The spec sealed to the winner (base64), only on the accept the buyer
    /// sends the winner directly (request-response, never gossip), and only
    /// when it fits one request. Without it the winner fetches it from the
    /// buyer over `c0mpute.job.payload.v1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sealed_spec: Option<String>,
    /// Worker side only, set once the sealed spec is opened: the key the
    /// result is sealed to the buyer with. Never serialized.
    #[serde(skip)]
    pub result_key: Option<JobKey>,
    /// The winning bid's [`JobBid::instance`]. A process with the winner's
    /// peer id but another instance id is a twin and must not run the job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub winning_instance: Option<String>,
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
    /// The reporting process's instance id (see [`JobBid::instance`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// Why a Failed receipt is not a compute fault: [`FAULT_IDENTITY_TWIN`]
    /// when the job was awarded to another process running this identity.
    /// Buyers re-award such a job instead of failing it or benching the peer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fault: Option<String>,
}

/// [`JobReceipt::fault`]: the accept (or its sealed spec) belongs to another
/// process with this peer id, not to the one answering.
pub const FAULT_IDENTITY_TWIN: &str = "identity_twin";

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
        assert!(!serde_json::to_string(&offer).unwrap().contains("external_ref"));
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
            e2e: None,
            instance: None,
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
            instance: None,
            fault: None,
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
            e2e: None,
            instance: None,
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
