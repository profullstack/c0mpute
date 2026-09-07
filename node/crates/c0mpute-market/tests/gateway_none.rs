//! The full market chain, with nothing in the middle.
//!
//! This is the protocol half of the v2 direction's `--gateway none`
//! release gate (§32, Test A) and of Test B, "no shared database". A buyer
//! and a provider are constructed as two independent parties that share
//! **no memory, no database, and no service** — every fact that crosses
//! between them crosses as JSON bytes through [`Wire`], the way it would
//! come off a gossip topic or a file.
//!
//! What this does prove:
//!
//! - discovery, quoting, award, execution and settlement need no third
//!   party to be *believed*;
//! - a hostile courier — an indexer, a relay, anything that touches the
//!   bytes in transit — can drop or delay messages but cannot forge them;
//! - a fourth party who saw none of it, holding only the JSON, reaches the
//!   same conclusions.
//!
//! What it does **not** prove: that the transport works. There is no
//! libp2p here, no DHT, no NAT traversal. Those are the rest of Phase 2,
//! and the real `--gateway none` gate needs them. This test pins the layer
//! underneath, so that when the transport lands the only new question is
//! whether packets arrive.

use c0mpute_envelope::advert::{
    CpuSpec, Disclosure, ProviderAdvert, ProviderCapabilities, ProviderTrust, Rate,
};
use c0mpute_envelope::job::{
    Bound, Economics, Execution, GpuRequirement, Isolation, JobManifest, JobOutput, Requirements,
    WorkloadRef,
};
use c0mpute_envelope::receipt::{SettlementRecord, ValidationOutcome, ValidationStatus};
use c0mpute_envelope::{
    ContentHash, Envelope, GpuSpec, Money, Offer, Receipt, ReceiptAcceptance, SettlementAdapter,
    SigningIdentity, Timestamp, TrustTier, ValidationPolicy,
};
use c0mpute_market::{OfferBook, ProviderDirectory, ReputationLedger, SelectionPolicy, select};

const WORKLOAD: &str = "infernet.inference";

/// The only thing the two parties share: bytes.
///
/// Deliberately not a channel of typed values. Everything is serialized
/// and re-parsed, so nothing can pass between the parties except what
/// would survive a real hop — no shared allocation, no implicit trust.
#[derive(Default)]
struct Wire {
    frames: Vec<String>,
}

impl Wire {
    fn send<T: c0mpute_envelope::Payload>(&mut self, envelope: &Envelope<T>) {
        self.frames.push(envelope.to_canonical_json().unwrap());
    }

    fn last(&self) -> &str {
        self.frames.last().expect("nothing sent")
    }
}

fn ts(s: &str) -> Timestamp {
    Timestamp::parse(s).unwrap()
}

/// The advert a provider publishes. It does not name its own signer —
/// the envelope does — so this needs no identity to build.
fn advert() -> ProviderAdvert {
    ProviderAdvert {
        sequence: 1,
        issued_at: ts("2026-09-06T16:00:00.000Z"),
        expires_at: ts("2026-09-06T16:10:00.000Z"),
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
                features: vec!["cuda".into()],
                count: 1,
            }],
            storage_gib: 512,
            bandwidth_mbps: 1000,
            workloads: vec![WORKLOAD.into()],
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
            asn: None,
        }),
    }
}

fn job_for(buyer: &SigningIdentity) -> JobManifest {
    JobManifest {
        workload: WorkloadRef {
            type_id: WORKLOAD.into(),
            version: Some(">=1 <2".into()),
        },
        requester: buyer.did().clone(),
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
        input: Some(c0mpute_envelope::job::JobInput {
            reference: ContentHash::of(b"prompts.jsonl"),
            encryption: None,
            bytes: Some(4096),
        }),
        execution: Execution {
            timeout_seconds: 120,
            retry: 0,
            isolation: Isolation::Container,
            runtime_digest: Some(ContentHash::of(b"runtime image")),
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

#[test]
fn a_job_completes_with_no_third_party_anywhere() {
    let now = ts("2026-09-06T16:00:05.000Z");

    // Two parties. Separate keys, separate state, nothing shared.
    let buyer = SigningIdentity::from_seed(&[0x11; 32]);
    let provider = SigningIdentity::from_seed(&[0x77; 32]);

    let mut buyer_directory = ProviderDirectory::new();
    let mut buyer_reputation = ReputationLedger::new();
    let mut wire = Wire::default();

    // ── 1. the provider advertises ───────────────────────────────────
    let advert = Envelope::seal(&provider, advert()).unwrap();
    wire.send(&advert);

    // The buyer learns of it from bytes, and verifies before believing.
    let heard: Envelope<ProviderAdvert> = Envelope::from_json(wire.last()).unwrap();
    buyer_directory.insert(&heard, &now).unwrap();
    assert_eq!(buyer_directory.len(), 1);

    // ── 2. the buyer announces a job ─────────────────────────────────
    let job = job_for(&buyer);
    let job_id = job.id().unwrap();
    let sealed_job = Envelope::seal(&buyer, job.clone()).unwrap();
    wire.send(&sealed_job);

    // The buyer's own client does the matching. No matcher was consulted.
    let eligible = buyer_directory.eligible(&job, &now);
    assert_eq!(eligible.len(), 1, "the provider qualifies");
    assert_eq!(eligible[0].provider, *provider.did());

    // ── 3. the provider quotes ───────────────────────────────────────
    let seen_job: Envelope<JobManifest> = Envelope::from_json(wire.last()).unwrap();
    let seen = seen_job.open().unwrap();
    assert!(
        seen.signed_by(&seen_job.signer),
        "the manifest names the identity that signed it"
    );

    let offer = Offer {
        job: seen.id().unwrap(),
        price: Money::new("0.042", "USD"),
        expected_duration_ms: 12_000,
        start_before: ts("2026-09-06T16:01:00.000Z"),
        expires_at: ts("2026-09-06T16:00:30.000Z"),
        capability_advert: Some(advert.content_hash().unwrap()),
        coordinator: false,
    };
    let sealed_offer = Envelope::seal(&provider, offer).unwrap();
    wire.send(&sealed_offer);

    // ── 4. the buyer collects and selects ────────────────────────────
    let mut book = OfferBook::for_job(&job).unwrap();
    let heard_offer: Envelope<Offer> = Envelope::from_json(wire.last()).unwrap();
    book.submit(&heard_offer, &job, &now).unwrap();

    let eligible_dids: Vec<_> = eligible.iter().map(|r| r.provider.clone()).collect();
    let candidates = book.live_from(&eligible_dids, &now);
    let selection = select(
        SelectionPolicy::Balanced,
        &candidates,
        &job,
        &buyer_reputation,
        &|_| TrustTier::Standard,
    )
    .unwrap();

    let winner = selection.winner();
    assert_eq!(winner.record.provider, *provider.did());
    assert_eq!(winner.record.offer.price.amount, "0.042");

    // The quote is tied back to the capabilities it was made against.
    assert_eq!(
        winner.record.offer.capability_advert.as_ref().unwrap(),
        &advert.content_hash().unwrap()
    );

    // ── 5. the provider runs the work and signs for it ───────────────
    let receipt = Receipt {
        job: job_id.clone(),
        buyer: buyer.did().clone(),
        provider: provider.did().clone(),
        validator: None,
        workload: WORKLOAD.into(),
        offer: sealed_offer.content_hash().unwrap(),
        input_hash: Some(ContentHash::of(b"prompts.jsonl")),
        output_hash: Some(ContentHash::of(b"completions.jsonl")),
        runtime_hash: Some(ContentHash::of(b"runtime image")),
        started_at: ts("2026-09-06T16:01:00.000Z"),
        completed_at: ts("2026-09-06T16:01:12.000Z"),
        price: winner.record.offer.price.clone(),
        validation: ValidationOutcome {
            policy: ValidationPolicy::default(),
            status: ValidationStatus::Accepted,
            detail: None,
        },
        settlement: SettlementRecord {
            adapter: SettlementAdapter::coinpay(),
            reference: Some("cp_rcpt_demo".into()),
            settled: true,
        },
    };
    let sealed_receipt = Envelope::seal(&provider, receipt).unwrap();
    wire.send(&sealed_receipt);

    // ── 6. the buyer checks and countersigns ─────────────────────────
    let heard_receipt: Envelope<Receipt> = Envelope::from_json(wire.last()).unwrap();
    let r = heard_receipt.open().unwrap();

    // The two facts a buyer must be able to establish alone.
    assert_eq!(r.job, job_id, "receipt is for the job we announced");
    assert_eq!(
        r.offer,
        sealed_offer.content_hash().unwrap(),
        "receipt cites the offer we accepted"
    );
    assert_eq!(
        r.price, winner.record.offer.price,
        "charged exactly what was quoted"
    );
    assert!(r.is_success());

    let acceptance = Envelope::seal(
        &buyer,
        ReceiptAcceptance {
            receipt: heard_receipt.content_hash().unwrap(),
            accepted: true,
            signed_at: ts("2026-09-06T16:01:20.000Z"),
            reason: None,
        },
    )
    .unwrap();
    wire.send(&acceptance);

    // ── 7. reputation, derived locally ───────────────────────────────
    buyer_reputation
        .record_against_offer(&heard_receipt, &sealed_offer)
        .unwrap();
    let stats = buyer_reputation.stats(provider.did());
    assert_eq!(stats.completed, 1);
    // Quoted 12s, took 12s.
    assert!((stats.quote_accuracy().unwrap() - 1.0).abs() < 1e-9);

    // ── 8. a fourth party, holding only the bytes ────────────────────
    // Never online, no database, no access to either party's state.
    let auditor_advert: Envelope<ProviderAdvert> = Envelope::from_json(&wire.frames[0]).unwrap();
    let auditor_job: Envelope<JobManifest> = Envelope::from_json(&wire.frames[1]).unwrap();
    let auditor_offer: Envelope<Offer> = Envelope::from_json(&wire.frames[2]).unwrap();
    let auditor_receipt: Envelope<Receipt> = Envelope::from_json(&wire.frames[3]).unwrap();
    let auditor_acceptance: Envelope<ReceiptAcceptance> =
        Envelope::from_json(&wire.frames[4]).unwrap();

    auditor_advert.open().unwrap();
    let aj = auditor_job.open().unwrap();
    let ao = auditor_offer.open().unwrap();
    let ar = auditor_receipt.open().unwrap();
    let aa = auditor_acceptance.open().unwrap();

    assert_eq!(ao.job, aj.id().unwrap());
    assert_eq!(ar.offer, auditor_offer.content_hash().unwrap());
    assert_eq!(ar.price, ao.price, "quoted price equals charged price");
    assert_eq!(aa.receipt, auditor_receipt.content_hash().unwrap());
    assert_eq!(auditor_offer.signer, auditor_receipt.signer);
    assert_eq!(auditor_acceptance.signer, *buyer.did());
}

#[test]
fn a_hostile_courier_can_delay_but_not_forge() {
    let now = ts("2026-09-06T16:00:05.000Z");
    let buyer = SigningIdentity::from_seed(&[0x11; 32]);
    let provider = SigningIdentity::from_seed(&[0x77; 32]);
    let job = job_for(&buyer);

    // Everything an intermediary sits in front of, rewritten in its
    // favour. Each attempt is a realistic thing a malicious indexer,
    // relay or gateway would try.
    let mut directory = ProviderDirectory::new();

    // (a) inflate a provider's capabilities to win more work
    let mut fat = Envelope::seal(&provider, advert()).unwrap();
    fat.payload.capabilities.gpus[0].vram_gib = 80;
    assert!(directory.insert(&fat, &now).is_err());

    // (b) undercut a quote to steer the award
    let honest_offer = Offer {
        job: job.id().unwrap(),
        price: Money::new("0.042", "USD"),
        expected_duration_ms: 12_000,
        start_before: ts("2026-09-06T16:01:00.000Z"),
        expires_at: ts("2026-09-06T16:00:30.000Z"),
        capability_advert: None,
        coordinator: false,
    };
    let mut cut = Envelope::seal(&provider, honest_offer).unwrap();
    cut.payload.price = Money::new("0.001", "USD");
    let mut book = OfferBook::for_job(&job).unwrap();
    assert!(book.submit(&cut, &job, &now).is_err());

    // (c) re-point an offer at a different, more expensive job
    let mut dearer = job_for(&buyer);
    dearer.economics.max_price = Money::new("9.00", "USD");
    let cross = Envelope::seal(
        &provider,
        Offer {
            job: dearer.id().unwrap(),
            price: Money::new("0.042", "USD"),
            expected_duration_ms: 12_000,
            start_before: ts("2026-09-06T16:01:00.000Z"),
            expires_at: ts("2026-09-06T16:00:30.000Z"),
            capability_advert: None,
            coordinator: false,
        },
    )
    .unwrap();
    assert!(book.submit(&cross, &job, &now).is_err());

    // (d) manufacture reputation for a provider it favours
    let mut ledger = ReputationLedger::new();
    let flattering = Envelope::seal(
        &buyer, // not the provider
        Receipt {
            job: job.id().unwrap(),
            buyer: buyer.did().clone(),
            provider: provider.did().clone(),
            validator: None,
            workload: WORKLOAD.into(),
            offer: ContentHash::of(b"whatever"),
            input_hash: None,
            output_hash: Some(ContentHash::of(b"out")),
            runtime_hash: None,
            started_at: ts("2026-09-06T16:01:00.000Z"),
            completed_at: ts("2026-09-06T16:01:01.000Z"),
            price: Money::new("0.042", "USD"),
            validation: ValidationOutcome {
                policy: ValidationPolicy::default(),
                status: ValidationStatus::Accepted,
                detail: None,
            },
            settlement: SettlementRecord {
                adapter: SettlementAdapter::coinpay(),
                reference: None,
                settled: true,
            },
        },
    )
    .unwrap();
    assert!(ledger.record(&flattering).is_err());

    // Nothing the courier touched got through.
    assert!(directory.is_empty());
    assert!(book.is_empty());
    assert!(ledger.is_empty());
}

#[test]
fn withholding_messages_degrades_the_market_without_corrupting_it() {
    // The one thing an intermediary *can* do is show you less than exists.
    // The buyer should end up with fewer choices, never a wrong one.
    let now = ts("2026-09-06T16:00:05.000Z");
    let buyer = SigningIdentity::from_seed(&[0x11; 32]);
    let job = job_for(&buyer);

    let mut directory = ProviderDirectory::new();
    let mut book = OfferBook::for_job(&job).unwrap();

    // Three providers advertise; a censoring indexer relays only one.
    let providers: Vec<SigningIdentity> = (0..3)
        .map(|i| SigningIdentity::from_seed(&[0x77 + i; 32]))
        .collect();

    let relayed = &providers[0];
    let advert = Envelope::seal(relayed, advert()).unwrap();
    directory.insert(&advert, &now).unwrap();

    book.submit(
        &Envelope::seal(
            relayed,
            Offer {
                job: job.id().unwrap(),
                price: Money::new("0.09", "USD"), // the worst price of the three
                expected_duration_ms: 12_000,
                start_before: ts("2026-09-06T16:01:00.000Z"),
                expires_at: ts("2026-09-06T16:00:30.000Z"),
                capability_advert: Some(advert.content_hash().unwrap()),
                coordinator: false,
            },
        )
        .unwrap(),
        &job,
        &now,
    )
    .unwrap();

    let eligible = directory.eligible(&job, &now);
    let dids: Vec<_> = eligible.iter().map(|r| r.provider.clone()).collect();
    let selection = select(
        SelectionPolicy::Cheapest,
        &book.live_from(&dids, &now),
        &job,
        &ReputationLedger::new(),
        &|_| TrustTier::Standard,
    )
    .unwrap();

    // The buyer gets a worse deal than the full market would have given —
    // and still a valid, verifiable, within-cap one. That is the correct
    // failure mode for a non-authoritative index, and the reason a buyer
    // who suspects it can go to the network directly.
    assert_eq!(selection.ranked.len(), 1);
    assert_eq!(selection.winner().record.offer.price.amount, "0.09");
    assert!(
        selection
            .winner()
            .record
            .offer
            .price
            .is_within(&job.economics.max_price)
            .unwrap()
    );
}
