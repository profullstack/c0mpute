//! Reputation, derived from signed receipts.
//!
//! There is no global score and no table anyone can edit. A node that has
//! collected receipts computes provider statistics for itself, and two
//! nodes weighting them differently are both correct. Feed the same
//! receipts to a fresh ledger and you get the same numbers back — which is
//! what makes a provider's record survive us, and survive anyone deciding
//! they do not like it.
//!
//! ## A new provider is not a bad provider
//!
//! Success rate is smoothed toward a neutral prior rather than computed
//! raw. Without that, a provider's first job is scored on a 0/0 record —
//! either 0.0, which makes joining impossible, or 1.0 after one success,
//! which makes a fresh key better than a provider with a thousand jobs and
//! two failures. Smoothing makes the first few jobs cheap to price and the
//! record meaningful once it exists.

use std::collections::HashMap;

use c0mpute_envelope::receipt::ValidationStatus;
use c0mpute_envelope::{Did, Envelope, Offer, Receipt};

use crate::Error;

/// Strength of the neutral prior, in imaginary jobs. Two is the standard
/// Laplace choice: one imaginary success and one imaginary failure, so an
/// unknown provider scores exactly 0.5.
const PRIOR_JOBS: f64 = 2.0;
const PRIOR_SUCCESSES: f64 = 1.0;

/// What one node has observed about one provider.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProviderStats {
    pub completed: u32,
    pub failed: u32,
    pub rejected: u32,
    pub disputed: u32,
    /// Summed wall-clock execution time over jobs where we hold both the
    /// offer and the receipt.
    pub actual_ms: u64,
    /// Summed quoted duration over those same jobs.
    pub quoted_ms: u64,
    pub quoted_samples: u32,
}

impl ProviderStats {
    /// Every receipt seen for this provider.
    pub fn jobs(&self) -> u32 {
        self.completed + self.failed + self.rejected + self.disputed
    }

    /// Smoothed success rate in `0.0..=1.0`. An unseen provider scores 0.5.
    pub fn success_rate(&self) -> f64 {
        let good = f64::from(self.completed) + PRIOR_SUCCESSES;
        let total = f64::from(self.jobs()) + PRIOR_JOBS;
        good / total
    }

    /// How well quotes predicted reality: `quoted / actual`.
    ///
    /// `1.0` means dead on, above means the provider beat its estimates,
    /// below means it overran them. `None` until we hold a matched
    /// offer/receipt pair.
    ///
    /// Hard to game: the quote is signed before the work and the receipt
    /// after it, and both are cited by hash.
    pub fn quote_accuracy(&self) -> Option<f64> {
        if self.quoted_samples == 0 || self.actual_ms == 0 {
            return None;
        }
        Some(self.quoted_ms as f64 / self.actual_ms as f64)
    }

    /// A single 0..1 summary for scoring. Success rate, with disputes
    /// weighted more heavily than plain failures: a job that failed is a
    /// bad day, a job the buyer disputed is a disagreement about whether
    /// the work was done at all.
    pub fn score(&self) -> f64 {
        let base = self.success_rate();
        let penalty = if self.jobs() == 0 {
            0.0
        } else {
            0.5 * f64::from(self.disputed) / f64::from(self.jobs())
        };
        (base - penalty).clamp(0.0, 1.0)
    }
}

/// Per-provider statistics accumulated from receipts this node holds.
#[derive(Debug, Default)]
pub struct ReputationLedger {
    by_provider: HashMap<String, ProviderStats>,
}

impl ReputationLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a signed receipt.
    ///
    /// Verifies before counting: an unverified receipt is an unsupported
    /// claim about someone else's record, and counting one would recreate
    /// exactly the "trust me" property this design exists to remove.
    pub fn record(&mut self, receipt: &Envelope<Receipt>) -> Result<(), Error> {
        let r = receipt.open().map_err(Error::Envelope)?;
        // The receipt names its provider, and the envelope proves who
        // signed it. A receipt signed by someone other than the provider
        // or the validator is not evidence about the provider.
        if receipt.signer != r.provider {
            return Err(Error::Untrusted(format!(
                "receipt for provider {} was signed by {}",
                r.provider, receipt.signer
            )));
        }

        let s = self.entry(&r.provider);
        match r.validation.status {
            ValidationStatus::Accepted => s.completed += 1,
            ValidationStatus::Failed => s.failed += 1,
            ValidationStatus::Rejected => s.rejected += 1,
            ValidationStatus::Disputed => s.disputed += 1,
        }
        Ok(())
    }

    /// Record a receipt together with the offer that priced it, which also
    /// yields quote accuracy.
    ///
    /// The receipt must cite this exact offer by hash. Without that check
    /// a provider could be credited for beating an estimate it never gave.
    pub fn record_against_offer(
        &mut self,
        receipt: &Envelope<Receipt>,
        offer: &Envelope<Offer>,
    ) -> Result<(), Error> {
        let r = receipt.open().map_err(Error::Envelope)?;
        let o = offer.open().map_err(Error::Envelope)?;
        let offer_hash = offer.content_hash().map_err(Error::Envelope)?;
        if r.offer != offer_hash {
            return Err(Error::Untrusted(format!(
                "receipt cites offer {} but was given offer {offer_hash}",
                r.offer
            )));
        }
        if receipt.signer != offer.signer {
            return Err(Error::Untrusted(
                "receipt and offer were signed by different identities".into(),
            ));
        }

        let actual = r.duration_ms().max(0) as u64;
        let quoted = o.expected_duration_ms;
        self.record(receipt)?;

        let s = self.entry(&receipt.signer.clone());
        s.actual_ms += actual;
        s.quoted_ms += quoted;
        s.quoted_samples += 1;
        Ok(())
    }

    /// Statistics for a provider. An unseen provider gets the default —
    /// a 0.5 score, not a zero.
    pub fn stats(&self, provider: &Did) -> ProviderStats {
        self.by_provider
            .get(provider.as_str())
            .cloned()
            .unwrap_or_default()
    }

    /// Convenience: the 0..1 score used by scoring policies.
    pub fn score(&self, provider: &Did) -> f64 {
        self.stats(provider).score()
    }

    /// How many providers we hold any history for.
    pub fn len(&self) -> usize {
        self.by_provider.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_provider.is_empty()
    }

    fn entry(&mut self, provider: &Did) -> &mut ProviderStats {
        self.by_provider
            .entry(provider.as_str().to_string())
            .or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

    #[test]
    fn an_unseen_provider_scores_neutral_not_zero() {
        let led = ReputationLedger::new();
        let (did, _) = gpu_provider();
        assert_eq!(led.stats(&did), ProviderStats::default());
        assert!((led.score(&did) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn one_success_does_not_outrank_a_long_good_record() {
        // The reason smoothing exists: a fresh key with a single success
        // must not be worth more than a provider with a real history.
        let mut led = ReputationLedger::new();
        let newcomer = identity(30).did().clone();
        record_n(&mut led, 30, ValidationStatus::Accepted, 1);

        let veteran = identity(31).did().clone();
        record_n(&mut led, 31, ValidationStatus::Accepted, 200);
        record_n(&mut led, 31, ValidationStatus::Failed, 2);

        assert!(
            led.score(&veteran) > led.score(&newcomer),
            "veteran {} should beat newcomer {}",
            led.score(&veteran),
            led.score(&newcomer)
        );
    }

    #[test]
    fn failures_lower_the_score() {
        let mut led = ReputationLedger::new();
        let did = identity(32).did().clone();
        record_n(&mut led, 32, ValidationStatus::Accepted, 10);
        let good = led.score(&did);
        record_n(&mut led, 32, ValidationStatus::Failed, 10);
        assert!(led.score(&did) < good);
    }

    #[test]
    fn a_dispute_costs_more_than_a_failure() {
        let mut led_f = ReputationLedger::new();
        record_n(&mut led_f, 33, ValidationStatus::Accepted, 10);
        record_n(&mut led_f, 33, ValidationStatus::Failed, 2);

        let mut led_d = ReputationLedger::new();
        record_n(&mut led_d, 34, ValidationStatus::Accepted, 10);
        record_n(&mut led_d, 34, ValidationStatus::Disputed, 2);

        assert!(
            led_d.score(&identity(34).did().clone()) < led_f.score(&identity(33).did().clone())
        );
    }

    #[test]
    fn failures_are_counted_which_is_the_point_of_signing_them() {
        let mut led = ReputationLedger::new();
        record_n(&mut led, 35, ValidationStatus::Failed, 3);
        let s = led.stats(&identity(35).did().clone());
        assert_eq!(s.failed, 3);
        assert_eq!(s.jobs(), 3);
        assert!(s.score() < 0.5);
    }

    #[test]
    fn a_receipt_signed_by_someone_else_is_not_evidence() {
        // Trivially forgeable reputation would otherwise be one signature
        // away: sign a glowing receipt about a competitor, or about
        // yourself under another provider's name.
        let mut led = ReputationLedger::new();
        let receipt = receipt_for(
            /* claimed provider */ 7,
            /* signed by */ 8,
            ValidationStatus::Accepted,
            12_000,
            None,
        );
        assert!(matches!(led.record(&receipt), Err(Error::Untrusted(_))));
        assert!(led.is_empty());
    }

    #[test]
    fn a_tampered_receipt_is_rejected() {
        let mut led = ReputationLedger::new();
        let mut receipt = receipt_for(7, 7, ValidationStatus::Failed, 12_000, None);
        receipt.payload.validation.status = ValidationStatus::Accepted;
        assert!(led.record(&receipt).is_err());
        assert!(led.is_empty());
    }

    #[test]
    fn quote_accuracy_needs_a_matching_offer() {
        let mut led = ReputationLedger::new();
        let job = gpu_job();
        let offer = offer_from(7, &job, "0.05", 10_000);
        let receipt = receipt_for(
            7,
            7,
            ValidationStatus::Accepted,
            8_000,
            Some(offer.content_hash().unwrap()),
        );

        led.record_against_offer(&receipt, &offer).unwrap();
        let s = led.stats(&identity(7).did().clone());
        assert_eq!(s.completed, 1);
        // Quoted 10s, took 8s: beat the estimate, ratio 1.25.
        assert!((s.quote_accuracy().unwrap() - 1.25).abs() < 1e-9);
    }

    #[test]
    fn credit_cannot_be_claimed_against_an_unrelated_offer() {
        let mut led = ReputationLedger::new();
        let job = gpu_job();
        let real = offer_from(7, &job, "0.05", 10_000);
        let other = offer_from(7, &job, "0.09", 999_000);
        let receipt = receipt_for(
            7,
            7,
            ValidationStatus::Accepted,
            8_000,
            Some(real.content_hash().unwrap()),
        );

        // Pairing the receipt with a slower quote would manufacture a
        // flattering accuracy number.
        assert!(matches!(
            led.record_against_offer(&receipt, &other),
            Err(Error::Untrusted(_))
        ));
        assert!(led.is_empty());
    }

    #[test]
    fn stats_rebuild_identically_from_the_same_receipts() {
        // The portability claim: reputation is derived, so a node that
        // loses its ledger and replays its receipt log lands in exactly
        // the same place. Order must not matter either.
        let job = gpu_job();
        let offer = offer_from(7, &job, "0.05", 10_000);
        let oh = offer.content_hash().unwrap();
        let receipts = vec![
            receipt_for(7, 7, ValidationStatus::Accepted, 8_000, Some(oh.clone())),
            receipt_for(7, 7, ValidationStatus::Failed, 1_000, None),
            receipt_for(7, 7, ValidationStatus::Accepted, 9_000, None),
        ];

        let mut a = ReputationLedger::new();
        for r in &receipts {
            let _ = a.record(r);
        }

        let mut b = ReputationLedger::new();
        for r in receipts.iter().rev() {
            let _ = b.record(r);
        }

        let did = identity(7).did().clone();
        assert_eq!(a.stats(&did), b.stats(&did));
        assert_eq!(a.stats(&did).completed, 2);
        assert_eq!(a.stats(&did).failed, 1);
    }

    fn record_n(led: &mut ReputationLedger, seed: u8, status: ValidationStatus, n: u32) {
        for _ in 0..n {
            led.record(&receipt_for(seed, seed, status, 5_000, None))
                .unwrap();
        }
    }
}
