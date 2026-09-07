//! Fixtures shared by this crate's unit tests.
//!
//! Deterministic by construction: identities come from fixed seeds and
//! timestamps are literals, so a scoring change shows up as a diff in the
//! chosen provider rather than as an intermittent failure.

use c0mpute_envelope::advert::{
    CpuSpec, Disclosure, ProviderAdvert, ProviderCapabilities, ProviderTrust, Rate,
};
use c0mpute_envelope::job::{
    Bound, Economics, Execution, GpuRequirement, Isolation, JobManifest, JobOutput, Requirements,
    WorkloadRef,
};
use c0mpute_envelope::receipt::{SettlementRecord, ValidationOutcome, ValidationStatus};
use c0mpute_envelope::{
    ContentHash, Did, Envelope, GpuSpec, Money, Offer, Receipt, SettlementAdapter, SigningIdentity,
    Timestamp, TrustTier, ValidationPolicy,
};

pub const WORKLOAD: &str = "infernet.inference";

pub fn identity(seed: u8) -> SigningIdentity {
    SigningIdentity::from_seed(&[seed; 32])
}

pub fn ts(s: &str) -> Timestamp {
    Timestamp::parse(s).unwrap()
}

/// "Now" for every fixture. The job below opens at 16:00 and closes at
/// 16:10, so this sits comfortably inside the window.
pub fn now() -> Timestamp {
    ts("2026-09-06T16:00:05.000Z")
}

pub fn gpu(vram_gib: u32, count: u32, features: &[&str]) -> GpuSpec {
    GpuSpec {
        vendor: "nvidia".into(),
        family: Some("ada".into()),
        vram_gib,
        features: features.iter().map(|s| (*s).to_string()).collect(),
        count,
    }
}

/// A provider that satisfies [`gpu_job`].
pub fn gpu_provider() -> (Did, ProviderAdvert) {
    provider_seeded(7)
}

/// Same shape, distinct identity — for tests that need a market.
pub fn provider_seeded(seed: u8) -> (Did, ProviderAdvert) {
    let did = identity(seed).did().clone();
    let advert = ProviderAdvert {
        sequence: 1,
        issued_at: ts("2026-09-06T16:00:00.000Z"),
        expires_at: ts("2026-09-06T16:10:00.000Z"),
        capabilities: ProviderCapabilities {
            cpu: CpuSpec {
                arch: "x86_64".into(),
                cores: 32,
            },
            memory_gib: 128,
            gpus: vec![gpu(24, 1, &["cuda"])],
            storage_gib: 512,
            bandwidth_mbps: 1000,
            workloads: vec![WORKLOAD.into(), "transcode.ffmpeg".into()],
        },
        pricing: vec![Rate {
            workload: WORKLOAD.into(),
            unit: "1m-output-tokens".into(),
            price: Money::new("0.60", "USD"),
        }],
        trust: ProviderTrust {
            tier: TrustTier::Standard,
            credentials: vec![],
        },
        disclosure: Some(Disclosure {
            region: Some("us-west".into()),
            country: None,
            asn: Some(64500 + u32::from(seed)),
        }),
    };
    (did, advert)
}

pub fn sealed_advert(seed: u8) -> (Did, Envelope<ProviderAdvert>) {
    let id = identity(seed);
    let (did, advert) = provider_seeded(seed);
    (did, Envelope::seal(&id, advert).unwrap())
}

/// A job requiring one 24 GiB CUDA GPU, capped at $0.10.
pub fn gpu_job() -> JobManifest {
    JobManifest {
        workload: WorkloadRef {
            type_id: WORKLOAD.into(),
            version: None,
        },
        requester: identity(1).did().clone(),
        requirements: Requirements {
            gpu: Some(GpuRequirement {
                count: 1,
                vram_gib: Some(Bound::at_least(24)),
                features: vec!["cuda".into()],
            }),
            cpu_cores: None,
            memory_gib: Some(Bound::at_least(32)),
            region: vec![],
            trust_tier: TrustTier::Standard,
            providers: vec![],
        },
        input: None,
        execution: Execution {
            timeout_seconds: 120,
            retry: 0,
            isolation: Isolation::Container,
            runtime_digest: None,
            network: false,
        },
        economics: Economics {
            max_price: Money::new("0.10", "USD"),
            settlement: SettlementAdapter::coinpay(),
            escrow: true,
        },
        validation: ValidationPolicy::default(),
        output: JobOutput::default(),
        announced_at: ts("2026-09-06T16:00:00.000Z"),
        deadline: ts("2026-09-06T16:10:00.000Z"),
    }
}

/// An offer for `job` at `price`, taking `duration_ms`.
pub fn offer_from(seed: u8, job: &JobManifest, price: &str, duration_ms: u64) -> Envelope<Offer> {
    let offer = Offer {
        job: job.id().unwrap(),
        price: Money::new(price, "USD"),
        expected_duration_ms: duration_ms,
        start_before: ts("2026-09-06T16:01:00.000Z"),
        expires_at: ts("2026-09-06T16:00:30.000Z"),
        capability_advert: Some(ContentHash::of(b"advert")),
        coordinator: false,
    };
    Envelope::seal(&identity(seed), offer).unwrap()
}

/// A sealed receipt.
///
/// `claimed` is the provider the receipt names; `signer` is who actually
/// signs it. They are separate parameters so tests can build the forgery
/// case — a receipt about someone else's work — as easily as the honest one.
pub fn receipt_for(
    claimed: u8,
    signer: u8,
    status: ValidationStatus,
    duration_ms: i64,
    offer_hash: Option<ContentHash>,
) -> Envelope<Receipt> {
    let started = ts("2026-09-06T16:01:00.000Z");
    let completed = Timestamp::from_unix_ms(started.unix_ms() + duration_ms).unwrap();
    let accepted = status == ValidationStatus::Accepted;

    let receipt = Receipt {
        job: gpu_job().id().unwrap(),
        buyer: identity(1).did().clone(),
        provider: identity(claimed).did().clone(),
        validator: None,
        workload: WORKLOAD.into(),
        offer: offer_hash.unwrap_or_else(|| ContentHash::of(b"some offer")),
        input_hash: Some(ContentHash::of(b"in")),
        output_hash: accepted.then(|| ContentHash::of(b"out")),
        runtime_hash: Some(ContentHash::of(b"runtime")),
        started_at: started,
        completed_at: completed,
        price: Money::new("0.05", "USD"),
        validation: ValidationOutcome {
            policy: ValidationPolicy::default(),
            status,
            detail: None,
        },
        settlement: SettlementRecord {
            adapter: SettlementAdapter::coinpay(),
            reference: accepted.then(|| "cp_rcpt_test".to_string()),
            settled: accepted,
        },
    };
    Envelope::seal(&identity(signer), receipt).unwrap()
}
