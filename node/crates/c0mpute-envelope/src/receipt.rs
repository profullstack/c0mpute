//! `c0mpute.receipt/v1` — what actually happened, signed.
//!
//! A receipt is the network's durable record and the input to every
//! reputation calculation. It names the job, the offer that priced it, the
//! runtime that ran it, the input and output by hash, what the validator
//! concluded, and what was paid.
//!
//! **Reputation is derived, not stored.** There is no global score and no
//! authoritative table holding one. A peer that has collected receipts can
//! compute a provider's completion rate, dispute rate, and offer accuracy
//! for itself, and two peers weighting those differently are both correct.
//! That is what makes reputation survive an operator's database going
//! away, or an operator deciding it does not like you.
//!
//! ## Two signatures, two envelopes
//!
//! The v2 direction shows a receipt carrying a `signatures` object with a
//! provider and a buyer entry. This crate models the same fact as a
//! provider-sealed `Envelope<Receipt>` plus a buyer-sealed
//! [`ReceiptAcceptance`] naming that receipt by hash. Two envelopes rather
//! than one multi-signature blob, because:
//!
//! - the signatures are made at different times, by parties who may never
//!   be online together, and one arriving without the other is the normal
//!   case, not a malformed message;
//! - a buyer needs to be able to say *no* as well as yes, and a
//!   countersignature field has no way to express a rejection;
//! - both then reuse one signing and verification path, so there is no
//!   second, subtly different way to check a signature.

use serde::{Deserialize, Serialize};

use crate::Error;
use crate::advert::validate_workload_type;
use crate::envelope::Payload;
use crate::identity::Did;
use crate::types::{ContentHash, Money, SettlementAdapter, Timestamp, ValidationPolicy};

/// A provider's signed statement of what it did.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Receipt {
    /// The job, by [`crate::JobManifest::id`].
    pub job: ContentHash,
    pub buyer: Did,
    pub provider: Did,
    /// The independent validator, when one was used (levels L2 and up).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validator: Option<Did>,
    pub workload: String,
    /// The offer that set the price, by content hash. Ties the amount paid
    /// back to a signed quote instead of an assertion.
    pub offer: ContentHash,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_hash: Option<ContentHash>,
    /// Absent when the job did not complete — a failure is still a receipt,
    /// and a provider's failures are part of its record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_hash: Option<ContentHash>,
    /// The pinned runtime that produced the output. With the input hash,
    /// this is what makes a result reproducible by a third party.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_hash: Option<ContentHash>,
    pub started_at: Timestamp,
    pub completed_at: Timestamp,
    pub price: Money,
    pub validation: ValidationOutcome,
    pub settlement: SettlementRecord,
}

/// What the chosen validation policy concluded.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationOutcome {
    /// The policy the job asked for, repeated here so a reader of the
    /// receipt alone knows how much the result was actually checked.
    pub policy: ValidationPolicy,
    pub status: ValidationStatus,
    /// Free-text detail, e.g. which schema check failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ValidationStatus {
    /// The result passed and the job should settle.
    Accepted,
    /// The result failed validation. No settlement.
    Rejected,
    /// The parties disagree; settlement is held pending resolution.
    Disputed,
    /// The job did not produce a result to validate (timeout, crash,
    /// provider withdrawal).
    Failed,
}

/// How the money moved, or did not.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettlementRecord {
    pub adapter: SettlementAdapter,
    /// The adapter's own identifier for the transfer — a CoinPay receipt
    /// id, a Lightning payment hash, an invoice number. Opaque here on
    /// purpose: the protocol records *that* an adapter settled and how to
    /// look it up, and knows nothing about any adapter's internals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    #[serde(default)]
    pub settled: bool,
}

impl Receipt {
    /// Wall-clock execution time.
    pub fn duration_ms(&self) -> i64 {
        self.completed_at.unix_ms() - self.started_at.unix_ms()
    }

    /// Whether this receipt counts as a successful job for reputation.
    pub fn is_success(&self) -> bool {
        self.validation.status == ValidationStatus::Accepted && self.output_hash.is_some()
    }

    /// Whether the provider's quoted duration held up. Offer accuracy is
    /// one of the more useful reputation inputs precisely because it is
    /// cheap to compute and hard to fake: the quote was signed before the
    /// work, the receipt after it.
    pub fn beat_estimate(&self, quoted_ms: u64) -> bool {
        self.duration_ms() <= quoted_ms as i64
    }
}

impl Payload for Receipt {
    const TYPE: &'static str = "c0mpute.receipt/v1";

    // Receipts do not expire. They are the historical record; a two-year-old
    // receipt is exactly as valid a statement about the past as a fresh one.

    fn validate(&self) -> Result<(), Error> {
        validate_workload_type(&self.workload)?;
        self.price.validate()?;
        self.settlement.adapter.validate()?;
        self.validation.policy.validate()?;

        if self.completed_at < self.started_at {
            return Err(Error::Format(format!(
                "receipt completedAt ({}) is before startedAt ({})",
                self.completed_at, self.started_at
            )));
        }
        if self.buyer == self.provider {
            return Err(Error::Format(
                "buyer and provider must be different identities".into(),
            ));
        }
        if self.validation.status == ValidationStatus::Accepted && self.output_hash.is_none() {
            return Err(Error::Format(
                "an accepted result must record an output hash".into(),
            ));
        }
        if self.settlement.settled && self.validation.status != ValidationStatus::Accepted {
            return Err(Error::Format(format!(
                "settlement recorded for a result whose validation status is {:?}; \
                 only accepted work settles",
                self.validation.status
            )));
        }
        Ok(())
    }
}

/// The buyer's countersignature on a receipt — or its refusal.
///
/// Sealed by the buyer in its own envelope, naming the provider's sealed
/// receipt by content hash.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceiptAcceptance {
    /// Content hash of the provider's sealed `Envelope<Receipt>`, from
    /// [`crate::Envelope::content_hash`].
    pub receipt: ContentHash,
    /// True to accept, false to dispute.
    pub accepted: bool,
    pub signed_at: Timestamp,
    /// Why, when disputing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl Payload for ReceiptAcceptance {
    const TYPE: &'static str = "c0mpute.receipt.acceptance/v1";

    fn validate(&self) -> Result<(), Error> {
        if !self.accepted && self.reason.as_ref().is_none_or(|r| r.trim().is_empty()) {
            return Err(Error::Format(
                "a disputed receipt must state a reason; an unexplained dispute cannot be \
                 weighed against the provider's record"
                    .into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::Envelope;
    use crate::identity::SigningIdentity;
    use crate::types::{ValidationLevel, ValidationPolicy};

    fn ts(s: &str) -> Timestamp {
        Timestamp::parse(s).unwrap()
    }

    fn buyer() -> SigningIdentity {
        SigningIdentity::from_seed(&[11u8; 32])
    }

    fn provider() -> SigningIdentity {
        SigningIdentity::from_seed(&[77u8; 32])
    }

    fn receipt() -> Receipt {
        Receipt {
            job: ContentHash::of(b"job manifest"),
            buyer: buyer().did().clone(),
            provider: provider().did().clone(),
            validator: None,
            workload: "infernet.inference".into(),
            offer: ContentHash::of(b"sealed offer"),
            input_hash: Some(ContentHash::of(b"input")),
            output_hash: Some(ContentHash::of(b"output")),
            runtime_hash: Some(ContentHash::of(b"runtime")),
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
                reference: Some("cp_rcpt_01J8…".into()),
                settled: true,
            },
        }
    }

    #[test]
    fn a_well_formed_receipt_seals_and_opens() {
        let r = receipt();
        r.validate().unwrap();
        let env = Envelope::seal(&provider(), r).unwrap();
        let opened = env.open().unwrap();
        assert!(opened.is_success());
        assert_eq!(opened.duration_ms(), 12_000);
        assert!(opened.beat_estimate(12_000));
        assert!(!opened.beat_estimate(11_999));
    }

    #[test]
    fn receipts_do_not_expire() {
        let env = Envelope::seal(&provider(), receipt()).unwrap();
        // Years later, the record still verifies.
        env.open_fresh(&ts("2031-01-01T00:00:00.000Z")).unwrap();
    }

    #[test]
    fn a_failure_is_still_a_valid_receipt() {
        let mut r = receipt();
        r.output_hash = None;
        r.validation.status = ValidationStatus::Failed;
        r.validation.detail = Some("provider timed out".into());
        r.settlement.settled = false;
        r.settlement.reference = None;
        r.validate().unwrap();
        assert!(!r.is_success());
    }

    #[test]
    fn an_accepted_result_must_have_an_output() {
        let mut r = receipt();
        r.output_hash = None;
        assert!(r.validate().is_err());
    }

    #[test]
    fn settlement_cannot_be_recorded_for_unaccepted_work() {
        let mut r = receipt();
        r.validation.status = ValidationStatus::Rejected;
        r.output_hash = None;
        // settled is still true from the fixture.
        assert!(r.validate().is_err());
    }

    #[test]
    fn completion_cannot_precede_start() {
        let mut r = receipt();
        r.completed_at = ts("2026-09-06T16:00:00.000Z");
        assert!(r.validate().is_err());
    }

    #[test]
    fn a_self_dealt_receipt_is_rejected() {
        let mut r = receipt();
        r.provider = r.buyer.clone();
        assert!(r.validate().is_err());
    }

    #[test]
    fn a_tampered_price_does_not_survive_the_signature() {
        let mut env = Envelope::seal(&provider(), receipt()).unwrap();
        env.payload.price = Money::new("999", "USD");
        assert!(matches!(env.open().unwrap_err(), Error::BadSignature));
    }

    #[test]
    fn the_buyer_countersigns_the_providers_sealed_receipt() {
        let sealed = Envelope::seal(&provider(), receipt()).unwrap();
        let acceptance = ReceiptAcceptance {
            receipt: sealed.content_hash().unwrap(),
            accepted: true,
            signed_at: ts("2026-09-06T16:01:20.000Z"),
            reason: None,
        };
        let counter = Envelope::seal(&buyer(), acceptance).unwrap();

        let opened = counter.open().unwrap();
        assert_eq!(opened.receipt, sealed.content_hash().unwrap());
        assert_eq!(counter.signer.as_str(), buyer().did().as_str());
    }

    #[test]
    fn a_countersignature_does_not_transfer_to_another_receipt() {
        let sealed = Envelope::seal(&provider(), receipt()).unwrap();
        let mut other = receipt();
        other.price = Money::new("0.500", "USD");
        let other_sealed = Envelope::seal(&provider(), other).unwrap();

        let counter = Envelope::seal(
            &buyer(),
            ReceiptAcceptance {
                receipt: sealed.content_hash().unwrap(),
                accepted: true,
                signed_at: ts("2026-09-06T16:01:20.000Z"),
                reason: None,
            },
        )
        .unwrap();

        // The acceptance names one receipt hash and only that one.
        assert_ne!(
            counter.open().unwrap().receipt,
            other_sealed.content_hash().unwrap()
        );
    }

    #[test]
    fn a_dispute_must_say_why() {
        let bare = ReceiptAcceptance {
            receipt: ContentHash::of(b"r"),
            accepted: false,
            signed_at: ts("2026-09-06T16:01:20.000Z"),
            reason: None,
        };
        assert!(bare.validate().is_err());

        let blank = ReceiptAcceptance {
            reason: Some("   ".into()),
            ..bare.clone()
        };
        assert!(blank.validate().is_err());

        let explained = ReceiptAcceptance {
            reason: Some("output failed schema validation".into()),
            ..bare
        };
        explained.validate().unwrap();
    }
}
