//! `c0mpute.offer/v1` — a provider quotes a price for one specific job.
//!
//! An offer is the moment the market actually happens, and it is where the
//! v2 direction's central architectural claim gets cashed out: the offer
//! is *signed by the provider* and *selected by the buyer*. Nothing in
//! between decides. A gateway may collect offers on a buyer's behalf, and
//! an indexer may have suggested which providers to ask, but neither can
//! manufacture an offer, and neither gets to pick.
//!
//! Offers are short-lived by construction. A quote is a promise about
//! capacity the provider has *now*; letting it linger would mean bidding
//! on hardware that is already busy.

use serde::{Deserialize, Serialize};

use crate::Error;
use crate::envelope::Payload;
use crate::job::JobManifest;
use crate::types::{ContentHash, Money, Timestamp};

/// A signed quote for one job.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Offer {
    /// The job being bid on, by [`JobManifest::id`].
    pub job: ContentHash,
    /// The price the provider will do it for. Binding, unlike the
    /// indicative rates in an advert.
    pub price: Money,
    /// How long the provider expects execution to take.
    pub expected_duration_ms: u64,
    /// The provider commits to starting before this instant, if awarded.
    pub start_before: Timestamp,
    /// After this, the quote is void. Must not be later than
    /// `start_before` — a quote you can accept after the promised start
    /// time is not a quote.
    pub expires_at: Timestamp,
    /// The advert this quote is backed by, by content hash. Lets a buyer
    /// tie the price to the capabilities it was offered against, and lets
    /// a receipt record which claim of capacity was relied on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_advert: Option<ContentHash>,
    /// Whether this provider is willing to act as the ephemeral job
    /// coordinator for a multi-provider workload (§15).
    ///
    /// Per-job and per-offer, never a standing role: a coordinator elected
    /// for one distributed-inference run has no authority over any other
    /// job, which is what keeps it from becoming a control plane.
    #[serde(default)]
    pub coordinator: bool,
}

impl Offer {
    /// Check this offer against the job it claims to bid on.
    ///
    /// Runs the constraints a buyer must not skip: right job, price within
    /// the cap, and enough time to finish before the deadline. Reputation,
    /// latency, and diversity are *policy* and live in the scheduler; the
    /// checks here are the ones where accepting a failing offer is simply
    /// a bug.
    pub fn check_against(&self, job: &JobManifest) -> Result<(), Error> {
        let job_id = job.id()?;
        if self.job != job_id {
            return Err(Error::Format(format!(
                "offer is for job {} but was checked against {job_id}",
                self.job
            )));
        }
        if !self.price.is_within(&job.economics.max_price)? {
            return Err(Error::Format(format!(
                "offer price {} {} exceeds the job's maximum of {} {}",
                self.price.amount,
                self.price.currency,
                job.economics.max_price.amount,
                job.economics.max_price.currency
            )));
        }
        let finish_by = self
            .start_before
            .unix_ms()
            .saturating_add(self.expected_duration_ms as i64);
        if finish_by > job.deadline.unix_ms() {
            return Err(Error::Format(format!(
                "offer would start as late as {} and run {}ms, finishing after the job deadline {}",
                self.start_before, self.expected_duration_ms, job.deadline
            )));
        }
        Ok(())
    }

    /// True when this offer is still acceptable at `now`.
    pub fn is_live_at(&self, now: &Timestamp) -> bool {
        !self.expires_at.is_expired_at(now)
    }
}

impl Payload for Offer {
    const TYPE: &'static str = "c0mpute.offer/v1";

    fn expires_at(&self) -> Option<&Timestamp> {
        Some(&self.expires_at)
    }

    fn validate(&self) -> Result<(), Error> {
        self.price.validate()?;
        if self.expected_duration_ms == 0 {
            return Err(Error::Format(
                "an offer must estimate a non-zero duration; a free instant is not a quote".into(),
            ));
        }
        if self.expires_at > self.start_before {
            return Err(Error::Format(format!(
                "offer expiresAt ({}) must not be after startBefore ({}); \
                 a quote acceptable after its promised start time is not binding",
                self.expires_at, self.start_before
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::Envelope;
    use crate::identity::SigningIdentity;
    use crate::job::{Economics, Execution, JobManifest, JobOutput, Requirements, WorkloadRef};
    use crate::types::SettlementAdapter;

    fn ts(s: &str) -> Timestamp {
        Timestamp::parse(s).unwrap()
    }

    fn provider() -> SigningIdentity {
        SigningIdentity::from_seed(&[77u8; 32])
    }

    fn job() -> JobManifest {
        JobManifest {
            workload: WorkloadRef {
                type_id: "transcode.ffmpeg".into(),
                version: None,
            },
            requester: SigningIdentity::from_seed(&[11u8; 32]).did().clone(),
            requirements: Requirements::default(),
            input: None,
            execution: Execution {
                timeout_seconds: 120,
                retry: 0,
                isolation: Default::default(),
                runtime_digest: None,
                network: false,
            },
            economics: Economics {
                max_price: Money::new("0.10", "USD"),
                settlement: SettlementAdapter::coinpay(),
                escrow: true,
            },
            validation: Default::default(),
            output: JobOutput::default(),
            announced_at: ts("2026-09-06T16:00:00.000Z"),
            deadline: ts("2026-09-06T16:10:00.000Z"),
        }
    }

    fn offer_for(job: &JobManifest) -> Offer {
        Offer {
            job: job.id().unwrap(),
            price: Money::new("0.042", "USD"),
            expected_duration_ms: 12_000,
            start_before: ts("2026-09-06T16:02:00.000Z"),
            expires_at: ts("2026-09-06T16:00:10.000Z"),
            capability_advert: Some(ContentHash::of(b"advert")),
            coordinator: false,
        }
    }

    #[test]
    fn a_well_formed_offer_seals_and_checks_out() {
        let j = job();
        let o = offer_for(&j);
        o.validate().unwrap();
        o.check_against(&j).unwrap();
        let env = Envelope::seal(&provider(), o).unwrap();
        assert_eq!(env.open().unwrap().price.amount, "0.042");
    }

    #[test]
    fn an_offer_for_a_different_job_is_rejected() {
        let j = job();
        let mut other = job();
        other.economics.max_price = Money::new("0.20", "USD");
        let o = offer_for(&other);
        assert!(o.check_against(&j).is_err());
    }

    #[test]
    fn an_over_cap_offer_is_rejected_by_value_not_by_string() {
        let j = job(); // cap is "0.10"
        let mut o = offer_for(&j);
        // Lexically "0.09" > "0.10"; a string comparison would wrongly
        // reject this, and wrongly accept "0.9".
        o.price = Money::new("0.09", "USD");
        o.check_against(&j).unwrap();

        o.price = Money::new("0.9", "USD");
        assert!(o.check_against(&j).is_err());
    }

    #[test]
    fn an_offer_that_cannot_finish_before_the_deadline_is_rejected() {
        let j = job();
        let mut o = offer_for(&j);
        // Starts at 16:02, runs 9 minutes, deadline is 16:10.
        o.expected_duration_ms = 9 * 60 * 1000;
        assert!(o.check_against(&j).is_err());
    }

    #[test]
    fn an_offer_in_the_wrong_currency_is_rejected_rather_than_converted() {
        let j = job();
        let mut o = offer_for(&j);
        o.price = Money::new("0.042", "USDC");
        assert!(
            o.check_against(&j).is_err(),
            "the protocol has no exchange rate and must not guess one"
        );
    }

    #[test]
    fn an_offer_acceptable_after_its_promised_start_is_rejected() {
        let j = job();
        let mut o = offer_for(&j);
        o.expires_at = ts("2026-09-06T16:03:00.000Z"); // after start_before
        assert!(o.validate().is_err());
    }

    #[test]
    fn a_zero_duration_offer_is_rejected() {
        let j = job();
        let mut o = offer_for(&j);
        o.expected_duration_ms = 0;
        assert!(o.validate().is_err());
    }

    #[test]
    fn offers_go_stale() {
        let j = job();
        let o = offer_for(&j);
        assert!(o.is_live_at(&ts("2026-09-06T16:00:05.000Z")));
        assert!(!o.is_live_at(&ts("2026-09-06T16:00:11.000Z")));

        let env = Envelope::seal(&provider(), o).unwrap();
        assert!(matches!(
            env.open_fresh(&ts("2026-09-06T16:05:00.000Z")).unwrap_err(),
            Error::Expired { .. }
        ));
    }

    #[test]
    fn a_tampered_price_does_not_survive_the_signature() {
        let j = job();
        let mut env = Envelope::seal(&provider(), offer_for(&j)).unwrap();
        env.payload.price = Money::new("0.001", "USD");
        assert!(matches!(env.open().unwrap_err(), Error::BadSignature));
    }

    #[test]
    fn coordinator_willingness_is_off_by_default() {
        let json = r#"{"job":"blake3:00","price":{"amount":"1","currency":"USD"},
                       "expectedDurationMs":10,"startBefore":"2026-09-06T16:02:00.000Z",
                       "expiresAt":"2026-09-06T16:00:10.000Z"}"#;
        let o: Offer = serde_json::from_str(json).unwrap();
        assert!(!o.coordinator);
        assert!(o.capability_advert.is_none());
    }
}
