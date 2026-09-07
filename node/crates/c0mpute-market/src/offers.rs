//! Offers collected for one job.
//!
//! The book holds at most one live offer per provider and verifies every
//! one on the way in. Three checks are hard gates rather than preferences,
//! because accepting an offer that fails any of them is a bug, not a bad
//! trade:
//!
//!   1. it is signed, and by the provider it claims to be from;
//!   2. it is for *this* job, within the price cap, and can finish before
//!      the deadline (`Offer::check_against`);
//!   3. it has not expired.
//!
//! Price is compared exactly, through decimal-string arithmetic, never as
//! a float. Scoring later uses floats — a ranking heuristic can afford
//! rounding — but the question "is this within my cap" cannot.

use std::collections::HashMap;

use c0mpute_envelope::job::JobManifest;
use c0mpute_envelope::{Did, Envelope, Offer, Timestamp};

use crate::Error;

/// A verified offer, with the identity that signed it.
#[derive(Clone, Debug)]
pub struct OfferRecord {
    pub provider: Did,
    pub offer: Offer,
    /// Content hash of the sealed offer — what a receipt cites to prove
    /// the price charged is the price quoted.
    pub offer_hash: c0mpute_envelope::ContentHash,
}

/// Live offers for one job, one per provider.
#[derive(Debug)]
pub struct OfferBook {
    job_id: c0mpute_envelope::ContentHash,
    by_provider: HashMap<String, OfferRecord>,
}

impl OfferBook {
    /// Open a book for `job`.
    pub fn for_job(job: &JobManifest) -> Result<Self, Error> {
        Ok(Self {
            job_id: job.id().map_err(Error::Envelope)?,
            by_provider: HashMap::new(),
        })
    }

    /// Verify an offer and record it if it beats what that provider has
    /// already quoted.
    ///
    /// Keeping the provider's **best** offer rather than its most recent
    /// makes the book independent of arrival order, so two buyers who
    /// heard the same offers in different sequences select the same
    /// winner. A provider improving its quote is legitimate; a provider
    /// worsening it after the fact should not un-quote the better price.
    pub fn submit(
        &mut self,
        envelope: &Envelope<Offer>,
        job: &JobManifest,
        now: &Timestamp,
    ) -> Result<Accepted, Error> {
        let offer = envelope.open_fresh(now).map_err(Error::Envelope)?;
        offer.check_against(job).map_err(Error::Envelope)?;

        // check_against already compares the job id, but only against the
        // manifest handed in. Comparing to the book's own id stops a
        // caller from mixing books and jobs.
        if offer.job != self.job_id {
            return Err(Error::Untrusted(format!(
                "offer is for job {} but this book is for {}",
                offer.job, self.job_id
            )));
        }

        let provider = envelope.signer.clone();
        let key = provider.as_str().to_string();
        let record = OfferRecord {
            provider,
            offer: offer.clone(),
            offer_hash: envelope.content_hash().map_err(Error::Envelope)?,
        };

        match self.by_provider.get(&key) {
            Some(existing) if !improves_on(&record, existing)? => Ok(Accepted::NotBetter),
            Some(_) => {
                self.by_provider.insert(key, record);
                Ok(Accepted::Improved)
            }
            None => {
                self.by_provider.insert(key, record);
                Ok(Accepted::New)
            }
        }
    }

    /// Offers still live at `now`, in deterministic provider order.
    pub fn live(&self, now: &Timestamp) -> Vec<OfferRecord> {
        let mut out: Vec<OfferRecord> = self
            .by_provider
            .values()
            .filter(|r| r.offer.is_live_at(now))
            .cloned()
            .collect();
        out.sort_by(|a, b| a.provider.as_str().cmp(b.provider.as_str()));
        out
    }

    /// Restrict to providers that are also eligible per the directory.
    ///
    /// An offer from an ineligible provider is not an attack — a provider
    /// may bid on work it cannot do, or its advert may have lapsed between
    /// quoting and now — but it must not win.
    pub fn live_from(&self, eligible: &[Did], now: &Timestamp) -> Vec<OfferRecord> {
        self.live(now)
            .into_iter()
            .filter(|r| eligible.contains(&r.provider))
            .collect()
    }

    pub fn len(&self) -> usize {
        self.by_provider.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_provider.is_empty()
    }
}

/// What happened to a submitted offer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Accepted {
    /// First offer from this provider.
    New,
    /// Replaced this provider's previous, worse offer.
    Improved,
    /// Kept the provider's existing offer instead.
    NotBetter,
}

/// Cheaper wins; equal price, faster wins; still equal, the lower offer
/// hash wins so the answer never depends on which arrived first.
fn improves_on(new: &OfferRecord, existing: &OfferRecord) -> Result<bool, Error> {
    use std::cmp::Ordering;
    let by_price = new
        .offer
        .price
        .cmp_amount(&existing.offer.price)
        .map_err(Error::Envelope)?;
    Ok(match by_price {
        Ordering::Less => true,
        Ordering::Greater => false,
        Ordering::Equal => match new
            .offer
            .expected_duration_ms
            .cmp(&existing.offer.expected_duration_ms)
        {
            Ordering::Less => true,
            Ordering::Greater => false,
            Ordering::Equal => new.offer_hash.to_wire() < existing.offer_hash.to_wire(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use c0mpute_envelope::Money;

    #[test]
    fn a_valid_offer_is_accepted() {
        let job = gpu_job();
        let mut book = OfferBook::for_job(&job).unwrap();
        let env = offer_from(7, &job, "0.05", 12_000);
        assert_eq!(book.submit(&env, &job, &now()).unwrap(), Accepted::New);
        assert_eq!(book.live(&now()).len(), 1);
    }

    #[test]
    fn a_tampered_offer_is_rejected() {
        let job = gpu_job();
        let mut book = OfferBook::for_job(&job).unwrap();
        let mut env = offer_from(7, &job, "0.05", 12_000);
        env.payload.price = Money::new("0.01", "USD");
        assert!(book.submit(&env, &job, &now()).is_err());
        assert!(book.is_empty());
    }

    #[test]
    fn an_over_cap_offer_is_rejected() {
        // The cap is "0.10". "0.9" is over it numerically, and *under* it
        // lexically — the case a string comparison gets wrong.
        let job = gpu_job();
        let mut book = OfferBook::for_job(&job).unwrap();
        let env = offer_from(7, &job, "0.9", 12_000);
        assert!(book.submit(&env, &job, &now()).is_err());

        let ok = offer_from(7, &job, "0.09", 12_000);
        book.submit(&ok, &job, &now()).unwrap();
    }

    #[test]
    fn an_offer_that_cannot_meet_the_deadline_is_rejected() {
        let job = gpu_job();
        let mut book = OfferBook::for_job(&job).unwrap();
        // Starts by 16:01, runs 10 minutes, deadline is 16:10.
        let env = offer_from(7, &job, "0.05", 10 * 60 * 1000);
        assert!(book.submit(&env, &job, &now()).is_err());
    }

    #[test]
    fn an_expired_offer_is_rejected() {
        let job = gpu_job();
        let mut book = OfferBook::for_job(&job).unwrap();
        let env = offer_from(7, &job, "0.05", 12_000);
        let late = ts("2026-09-06T16:05:00.000Z");
        assert!(book.submit(&env, &job, &late).is_err());
    }

    #[test]
    fn an_offer_for_another_job_is_rejected() {
        let job = gpu_job();
        let mut other = gpu_job();
        other.economics.max_price = Money::new("5.00", "USD");

        let mut book = OfferBook::for_job(&job).unwrap();
        let env = offer_from(7, &other, "0.05", 12_000);
        assert!(book.submit(&env, &job, &now()).is_err());
    }

    #[test]
    fn a_provider_holds_one_slot_and_may_improve_it() {
        let job = gpu_job();
        let mut book = OfferBook::for_job(&job).unwrap();
        book.submit(&offer_from(7, &job, "0.08", 12_000), &job, &now())
            .unwrap();
        assert_eq!(
            book.submit(&offer_from(7, &job, "0.05", 12_000), &job, &now())
                .unwrap(),
            Accepted::Improved
        );
        assert_eq!(book.len(), 1, "still one slot");
        assert_eq!(book.live(&now())[0].offer.price.amount, "0.05");
    }

    #[test]
    fn a_provider_cannot_walk_back_a_better_quote() {
        let job = gpu_job();
        let mut book = OfferBook::for_job(&job).unwrap();
        book.submit(&offer_from(7, &job, "0.05", 12_000), &job, &now())
            .unwrap();
        assert_eq!(
            book.submit(&offer_from(7, &job, "0.09", 12_000), &job, &now())
                .unwrap(),
            Accepted::NotBetter
        );
        assert_eq!(book.live(&now())[0].offer.price.amount, "0.05");
    }

    #[test]
    fn the_book_is_independent_of_arrival_order() {
        let job = gpu_job();
        let prices = ["0.08", "0.05", "0.09"];

        let mut forward = OfferBook::for_job(&job).unwrap();
        for p in prices {
            let _ = forward.submit(&offer_from(7, &job, p, 12_000), &job, &now());
        }
        let mut reverse = OfferBook::for_job(&job).unwrap();
        for p in prices.iter().rev() {
            let _ = reverse.submit(&offer_from(7, &job, p, 12_000), &job, &now());
        }

        assert_eq!(
            forward.live(&now())[0].offer.price.amount,
            reverse.live(&now())[0].offer.price.amount
        );
        assert_eq!(forward.live(&now())[0].offer.price.amount, "0.05");
    }

    #[test]
    fn equal_prices_are_broken_by_speed() {
        let job = gpu_job();
        let mut book = OfferBook::for_job(&job).unwrap();
        book.submit(&offer_from(7, &job, "0.05", 20_000), &job, &now())
            .unwrap();
        book.submit(&offer_from(7, &job, "0.05", 9_000), &job, &now())
            .unwrap();
        assert_eq!(book.live(&now())[0].offer.expected_duration_ms, 9_000);
    }

    #[test]
    fn several_providers_each_get_a_slot() {
        let job = gpu_job();
        let mut book = OfferBook::for_job(&job).unwrap();
        for seed in [7u8, 8, 9] {
            book.submit(&offer_from(seed, &job, "0.05", 12_000), &job, &now())
                .unwrap();
        }
        assert_eq!(book.len(), 3);
    }

    #[test]
    fn live_from_filters_out_ineligible_providers() {
        let job = gpu_job();
        let mut book = OfferBook::for_job(&job).unwrap();
        for seed in [7u8, 8] {
            book.submit(&offer_from(seed, &job, "0.05", 12_000), &job, &now())
                .unwrap();
        }
        let only_seven = vec![identity(7).did().clone()];
        assert_eq!(book.live_from(&only_seven, &now()).len(), 1);
        assert_eq!(book.live_from(&[], &now()).len(), 0);
    }

    #[test]
    fn offers_drop_out_when_they_expire() {
        let job = gpu_job();
        let mut book = OfferBook::for_job(&job).unwrap();
        book.submit(&offer_from(7, &job, "0.05", 12_000), &job, &now())
            .unwrap();
        // The fixture's offers expire at 16:00:30.
        assert_eq!(book.live(&ts("2026-09-06T16:00:29.000Z")).len(), 1);
        assert_eq!(book.live(&ts("2026-09-06T16:00:31.000Z")).len(), 0);
    }
}
