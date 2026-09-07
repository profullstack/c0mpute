//! Known-answer vectors for the c0mpute v2 wire format.
//!
//! Phase 1 of the v2 direction has an acceptance criterion that job, offer
//! and receipt hashes reproduce *across implementations*. That is only
//! checkable against fixed answers, so this file pins them: given these
//! seeds and these payloads, an implementation must produce exactly these
//! DIDs, these canonical bytes, and these hashes.
//!
//! A failure here is not a flaky test. It means the wire format changed,
//! and every previously signed advert, offer and receipt on the network
//! now hashes differently. If the change is deliberate, the payload's
//! version identifier has to move with it.
//!
//! To regenerate the copies embedded in `docs/protocol/`:
//!
//! ```sh
//! cargo test -p c0mpute-envelope --test vectors -- --nocapture print_vectors
//! ```

use c0mpute_envelope::advert::{CpuSpec, Disclosure, ProviderCapabilities, Rate};
use c0mpute_envelope::job::{
    Bound, Economics, Execution, GpuRequirement, Isolation, JobInput, WorkloadRef,
};
use c0mpute_envelope::receipt::{SettlementRecord, ValidationOutcome, ValidationStatus};
use c0mpute_envelope::*;

/// Fixed seeds. Never use these for anything real.
const BUYER_SEED: [u8; 32] = [0x11; 32];
const PROVIDER_SEED: [u8; 32] = [0x77; 32];

const BUYER_DID: &str = "did:c0mpute:z6MktULudTtAsAhRegYPiZ6631RV3viv12qd4GQF8z1xB22S";
const PROVIDER_DID: &str = "did:c0mpute:z6MkswFb62xmEDrqnknM3TP112AiH6A5YETp7gc2Qz4Wqkar";

const ADVERT_HASH: &str = "blake3:cecfc7a08527ee50ab8945bb636afc940b5afff48a50cdfbcdee501edbebd2c2";
const JOB_ID: &str = "blake3:4a4323c24e72bb676e575acb33051450e930e499b7b68d6a41ad7d47f5551b35";
const OFFER_HASH: &str = "blake3:9e9151a1d682f3c60fb596b92dad5757be890f476cf979dbe8c924228494393f";
const RECEIPT_HASH: &str =
    "blake3:7c5ba273a840b6bfd859f24904e032229010cee3de67c4102e194d3c558e87df";
const ACCEPTANCE_HASH: &str =
    "blake3:8b5a7302ed135469555c185b0b7cbcb1ecc9266bfa0c88b25bf6f48f89f45b9a";

fn ts(s: &str) -> Timestamp {
    Timestamp::parse(s).unwrap()
}

fn buyer() -> SigningIdentity {
    SigningIdentity::from_seed(&BUYER_SEED)
}

fn provider() -> SigningIdentity {
    SigningIdentity::from_seed(&PROVIDER_SEED)
}

fn advert() -> ProviderAdvert {
    ProviderAdvert {
        sequence: 8472,
        issued_at: ts("2026-09-06T23:50:00.000Z"),
        expires_at: ts("2026-09-06T23:59:00.000Z"),
        capabilities: ProviderCapabilities {
            cpu: CpuSpec {
                arch: "x86_64".into(),
                cores: 32,
            },
            memory_gib: 128,
            gpus: vec![GpuSpec {
                vendor: "nvidia".into(),
                family: Some("ada".into()),
                vram_gib: 24,
                features: vec!["cuda".into(), "nvenc".into()],
                count: 1,
            }],
            storage_gib: 1200,
            bandwidth_mbps: 1000,
            workloads: vec!["infernet.inference".into(), "transcode.ffmpeg".into()],
        },
        pricing: vec![Rate {
            workload: "infernet.inference".into(),
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
            asn: None,
        }),
    }
}

fn job() -> JobManifest {
    JobManifest {
        workload: WorkloadRef {
            type_id: "infernet.inference".into(),
            version: Some(">=1 <2".into()),
        },
        requester: buyer().did().clone(),
        requirements: Requirements {
            gpu: Some(GpuRequirement {
                count: 1,
                vram_gib: Some(Bound::at_least(24)),
                features: vec!["cuda".into()],
            }),
            cpu_cores: None,
            memory_gib: Some(Bound::at_least(32)),
            region: vec!["us-west".into()],
            trust_tier: TrustTier::Standard,
            providers: vec![],
        },
        input: Some(JobInput {
            reference: ContentHash::of(b"prompts.jsonl"),
            encryption: Some("recipient-selected".into()),
            bytes: Some(4096),
        }),
        execution: Execution {
            timeout_seconds: 120,
            retry: 2,
            isolation: Isolation::Container,
            runtime_digest: Some(ContentHash::of(b"runtime image")),
            network: false,
        },
        economics: Economics {
            max_price: Money::new("0.10", "USD"),
            settlement: SettlementAdapter::coinpay(),
            escrow: true,
        },
        validation: ValidationPolicy {
            level: ValidationLevel::Spotcheck,
            redundancy: 1,
            spotcheck_percent: Some(5),
        },
        output: JobOutput {
            retention_seconds: 3600,
            max_bytes: 10_485_760,
        },
        announced_at: ts("2026-09-06T16:00:00.000Z"),
        deadline: ts("2026-09-06T16:10:00.000Z"),
    }
}

fn offer(job_id: ContentHash, advert_hash: ContentHash) -> Offer {
    Offer {
        job: job_id,
        price: Money::new("0.042", "USD"),
        expected_duration_ms: 12_000,
        start_before: ts("2026-09-06T16:02:00.000Z"),
        expires_at: ts("2026-09-06T16:00:10.000Z"),
        capability_advert: Some(advert_hash),
        coordinator: false,
    }
}

fn receipt(job_id: ContentHash, offer_hash: ContentHash) -> Receipt {
    Receipt {
        job: job_id,
        buyer: buyer().did().clone(),
        provider: provider().did().clone(),
        validator: None,
        workload: "infernet.inference".into(),
        offer: offer_hash,
        input_hash: Some(ContentHash::of(b"prompts.jsonl")),
        output_hash: Some(ContentHash::of(b"completions.jsonl")),
        runtime_hash: Some(ContentHash::of(b"runtime image")),
        started_at: ts("2026-09-06T16:01:00.000Z"),
        completed_at: ts("2026-09-06T16:01:12.000Z"),
        price: Money::new("0.042", "USD"),
        validation: ValidationOutcome {
            policy: ValidationPolicy {
                level: ValidationLevel::Spotcheck,
                redundancy: 1,
                spotcheck_percent: Some(5),
            },
            status: ValidationStatus::Accepted,
            detail: None,
        },
        settlement: SettlementRecord {
            adapter: SettlementAdapter::coinpay(),
            reference: Some("cp_rcpt_01J8Z3".into()),
            settled: true,
        },
    }
}

// ────────────────────────────────────────────────────────────────────────
// Pinned answers
// ────────────────────────────────────────────────────────────────────────

#[test]
fn identities_derive_from_seeds() {
    assert_eq!(buyer().did().as_str(), BUYER_DID);
    assert_eq!(provider().did().as_str(), PROVIDER_DID);
}

#[test]
fn advert_hash_is_pinned() {
    let env = Envelope::seal(&provider(), advert()).unwrap();
    env.open().unwrap();
    assert_eq!(env.content_hash().unwrap().to_wire(), ADVERT_HASH);
}

#[test]
fn job_id_is_pinned() {
    assert_eq!(job().id().unwrap().to_wire(), JOB_ID);
}

#[test]
fn offer_hash_is_pinned() {
    let advert_env = Envelope::seal(&provider(), advert()).unwrap();
    let j = job();
    let o = offer(j.id().unwrap(), advert_env.content_hash().unwrap());
    o.check_against(&j).unwrap();
    let env = Envelope::seal(&provider(), o).unwrap();
    assert_eq!(env.content_hash().unwrap().to_wire(), OFFER_HASH);
}

#[test]
fn receipt_and_acceptance_hashes_are_pinned() {
    let offer_hash = ContentHash::parse(OFFER_HASH).unwrap();
    let receipt_env =
        Envelope::seal(&provider(), receipt(job().id().unwrap(), offer_hash)).unwrap();
    assert_eq!(receipt_env.content_hash().unwrap().to_wire(), RECEIPT_HASH);

    let acceptance_env = Envelope::seal(
        &buyer(),
        ReceiptAcceptance {
            receipt: receipt_env.content_hash().unwrap(),
            accepted: true,
            signed_at: ts("2026-09-06T16:01:20.000Z"),
            reason: None,
        },
    )
    .unwrap();
    assert_eq!(
        acceptance_env.content_hash().unwrap().to_wire(),
        ACCEPTANCE_HASH
    );
}

/// The whole chain, as a buyer and a provider actually walk it: advert,
/// job, offer against that job, receipt citing that offer, buyer
/// countersignature citing that receipt. Every link is a content hash, so
/// no participant has to be trusted to report the previous step honestly.
#[test]
fn the_full_chain_verifies_end_to_end() {
    let advert_env = Envelope::seal(&provider(), advert()).unwrap();
    let job_env = Envelope::seal(&buyer(), job()).unwrap();
    let j = job_env.open().unwrap();
    assert!(j.signed_by(&job_env.signer));

    let offer_env = Envelope::seal(
        &provider(),
        offer(j.id().unwrap(), advert_env.content_hash().unwrap()),
    )
    .unwrap();
    let o = offer_env.open().unwrap();
    o.check_against(j).unwrap();
    assert_eq!(
        o.capability_advert.as_ref().unwrap(),
        &advert_env.content_hash().unwrap()
    );

    let receipt_env = Envelope::seal(
        &provider(),
        receipt(j.id().unwrap(), offer_env.content_hash().unwrap()),
    )
    .unwrap();
    let r = receipt_env.open().unwrap();
    assert!(r.is_success());
    assert_eq!(r.job, j.id().unwrap());
    assert_eq!(r.offer, offer_env.content_hash().unwrap());
    // The price paid is the price that was signed for, not an assertion.
    assert_eq!(r.price, o.price);

    let acceptance_env = Envelope::seal(
        &buyer(),
        ReceiptAcceptance {
            receipt: receipt_env.content_hash().unwrap(),
            accepted: true,
            signed_at: ts("2026-09-06T16:01:20.000Z"),
            reason: None,
        },
    )
    .unwrap();
    let a = acceptance_env.open().unwrap();
    assert_eq!(a.receipt, receipt_env.content_hash().unwrap());
    assert_eq!(acceptance_env.signer.as_str(), BUYER_DID);
}

/// Nothing in the chain needs an index, a database, or a hosted endpoint
/// to be believed: a third party holding only the JSON can check it all.
#[test]
fn a_third_party_can_verify_from_json_alone() {
    let advert_json = Envelope::seal(&provider(), advert())
        .unwrap()
        .to_canonical_json()
        .unwrap();
    let job_json = Envelope::seal(&buyer(), job())
        .unwrap()
        .to_canonical_json()
        .unwrap();

    // Reconstructed from text, by someone who saw none of it happen.
    let advert_env: Envelope<ProviderAdvert> = Envelope::from_json(&advert_json).unwrap();
    let job_env: Envelope<JobManifest> = Envelope::from_json(&job_json).unwrap();

    assert_eq!(advert_env.signer.as_str(), PROVIDER_DID);
    assert!(advert_env.open().unwrap().runs("infernet.inference"));
    assert_eq!(job_env.open().unwrap().id().unwrap().to_wire(), JOB_ID);
}

/// Prints the vectors for `docs/protocol/`. Not an assertion — run it with
/// `--nocapture` when the documented examples need refreshing.
#[test]
fn print_vectors() {
    let advert_env = Envelope::seal(&provider(), advert()).unwrap();
    let j = job();
    let job_env = Envelope::seal(&buyer(), j.clone()).unwrap();
    let offer_env = Envelope::seal(
        &provider(),
        offer(j.id().unwrap(), advert_env.content_hash().unwrap()),
    )
    .unwrap();
    let receipt_env = Envelope::seal(
        &provider(),
        receipt(j.id().unwrap(), offer_env.content_hash().unwrap()),
    )
    .unwrap();
    let acceptance_env = Envelope::seal(
        &buyer(),
        ReceiptAcceptance {
            receipt: receipt_env.content_hash().unwrap(),
            accepted: true,
            signed_at: ts("2026-09-06T16:01:20.000Z"),
            reason: None,
        },
    )
    .unwrap();

    println!("\nbuyer    {}", buyer().did());
    println!("provider {}\n", provider().did());
    println!("job id {}\n", j.id().unwrap());
    for (label, json, hash) in [
        (
            "provider.advert/v1",
            advert_env.to_canonical_json().unwrap(),
            advert_env.content_hash().unwrap(),
        ),
        (
            "job/v2",
            job_env.to_canonical_json().unwrap(),
            job_env.content_hash().unwrap(),
        ),
        (
            "offer/v1",
            offer_env.to_canonical_json().unwrap(),
            offer_env.content_hash().unwrap(),
        ),
        (
            "receipt/v1",
            receipt_env.to_canonical_json().unwrap(),
            receipt_env.content_hash().unwrap(),
        ),
        (
            "receipt.acceptance/v1",
            acceptance_env.to_canonical_json().unwrap(),
            acceptance_env.content_hash().unwrap(),
        ),
    ] {
        println!("## {label}\n\n{json}\n\nenvelope hash: {hash}\n");
    }
}
