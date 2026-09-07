//! Buyer-side offer selection.
//!
//! This is where "the buyer decides" stops being a slogan. Nothing on the
//! network ranks these offers; this function does, on the buyer's machine,
//! under a policy the buyer named.
//!
//! ## Eligibility is not scoring
//!
//! By the time an offer reaches here it has already passed the hard gates
//! in [`crate::offers`] — right job, within cap, meets the deadline,
//! signed by an eligible provider. Those are correctness. What follows is
//! *preference*, and reasonable buyers disagree about it. Two nodes running
//! different policies over identical offers are both right; two nodes
//! running the *same* policy over identical offers must agree, which is
//! why every tie here breaks deterministically.
//!
//! ## Why not just take the cheapest
//!
//! Pure lowest-bid selection is how a market races to the bottom and
//! becomes unreliable: the cheapest quote often comes from the provider
//! most likely to disappear mid-job, and the buyer pays for that in
//! retries. [`SelectionPolicy::Balanced`] is the default for that reason —
//! it prices reputation alongside money.

use c0mpute_envelope::job::JobManifest;
use c0mpute_envelope::{Did, Money, TrustTier};

use crate::Error;
use crate::offers::OfferRecord;
use crate::reputation::ReputationLedger;

/// How to rank eligible offers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SelectionPolicy {
    /// Lowest price, full stop.
    Cheapest,
    /// Shortest quoted duration.
    Fastest,
    /// Price, speed and reputation together. The default.
    #[default]
    Balanced,
    /// Reputation and trust tier first, price last.
    Trusted,
    /// For allowlisted work: trust and reputation, then price. Requires
    /// the job to carry a provider allowlist — without one, "private" is
    /// a preference rather than a guarantee, and the caller almost
    /// certainly meant something else.
    Private,
}

impl SelectionPolicy {
    /// Weights for (price, speed, reputation, trust). Each is `0.0..=1.0`
    /// and they need not sum to 1 — scores are compared, not published.
    fn weights(self) -> Weights {
        match self {
            SelectionPolicy::Cheapest => Weights::new(1.0, 0.0, 0.0, 0.0),
            SelectionPolicy::Fastest => Weights::new(0.0, 1.0, 0.0, 0.0),
            SelectionPolicy::Balanced => Weights::new(0.4, 0.2, 0.4, 0.0),
            SelectionPolicy::Trusted => Weights::new(0.2, 0.0, 0.5, 0.3),
            SelectionPolicy::Private => Weights::new(0.2, 0.0, 0.3, 0.5),
        }
    }

    /// Parse a `--policy` value.
    pub fn parse(s: &str) -> Result<Self, Error> {
        match s {
            "cheapest" => Ok(Self::Cheapest),
            "fastest" => Ok(Self::Fastest),
            "balanced" => Ok(Self::Balanced),
            "trusted" => Ok(Self::Trusted),
            "private" => Ok(Self::Private),
            other => Err(Error::Untrusted(format!(
                "unknown policy {other:?}; expected one of \
                 cheapest, fastest, balanced, trusted, private"
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cheapest => "cheapest",
            Self::Fastest => "fastest",
            Self::Balanced => "balanced",
            Self::Trusted => "trusted",
            Self::Private => "private",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Weights {
    price: f64,
    speed: f64,
    reputation: f64,
    trust: f64,
}

impl Weights {
    fn new(price: f64, speed: f64, reputation: f64, trust: f64) -> Self {
        Self {
            price,
            speed,
            reputation,
            trust,
        }
    }
}

/// A scored candidate, kept so a buyer can see *why* it lost.
#[derive(Clone, Debug)]
pub struct ScoredOffer {
    pub record: OfferRecord,
    pub total: f64,
    pub price: f64,
    pub speed: f64,
    pub reputation: f64,
    pub trust: f64,
}

/// The outcome of a selection.
#[derive(Clone, Debug)]
pub struct Selection {
    pub policy: SelectionPolicy,
    /// Best first.
    pub ranked: Vec<ScoredOffer>,
}

impl Selection {
    pub fn winner(&self) -> &ScoredOffer {
        &self.ranked[0]
    }

    pub fn runners_up(&self) -> &[ScoredOffer] {
        &self.ranked[1..]
    }

    /// One line explaining the choice, for `--json` output or a log.
    pub fn explain(&self) -> String {
        let w = self.winner();
        format!(
            "{} won under {} at {} {} ({}ms quoted, reputation {:.2}) — {} other offer(s)",
            w.record.provider,
            self.policy.as_str(),
            w.record.offer.price.amount,
            w.record.offer.price.currency,
            w.record.offer.expected_duration_ms,
            w.reputation,
            self.runners_up().len()
        )
    }
}

/// Rank `offers` and pick a winner.
///
/// `trust_of` supplies each provider's claimed tier from the directory;
/// providers missing from it score as `Community`.
pub fn select(
    policy: SelectionPolicy,
    offers: &[OfferRecord],
    job: &JobManifest,
    reputation: &ReputationLedger,
    trust_of: &dyn Fn(&Did) -> TrustTier,
) -> Result<Selection, Error> {
    if offers.is_empty() {
        return Err(Error::NoOffers);
    }
    if policy == SelectionPolicy::Private && job.requirements.providers.is_empty() {
        return Err(Error::Untrusted(
            "the private policy requires the job to carry a provider allowlist".into(),
        ));
    }

    let prices: Vec<f64> = offers
        .iter()
        .map(|o| amount(&o.offer.price))
        .collect::<Result<_, _>>()?;
    let durations: Vec<f64> = offers
        .iter()
        .map(|o| o.offer.expected_duration_ms as f64)
        .collect();

    let best_price = prices.iter().cloned().fold(f64::INFINITY, f64::min);
    let best_duration = durations.iter().cloned().fold(f64::INFINITY, f64::min);

    let w = policy.weights();
    let mut ranked: Vec<ScoredOffer> = offers
        .iter()
        .enumerate()
        .map(|(i, record)| {
            let price = ratio(best_price, prices[i]);
            let speed = ratio(best_duration, durations[i]);
            let rep = reputation.score(&record.provider);
            let trust = trust_score(trust_of(&record.provider));
            ScoredOffer {
                total: w.price * price + w.speed * speed + w.reputation * rep + w.trust * trust,
                price,
                speed,
                reputation: rep,
                trust,
                record: record.clone(),
            }
        })
        .collect();

    // Highest score first; ties broken by provider DID so the answer never
    // depends on iteration order.
    ranked.sort_by(|a, b| {
        b.total
            .partial_cmp(&a.total)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.record.provider.as_str().cmp(b.record.provider.as_str()))
    });

    Ok(Selection { policy, ranked })
}

/// Score a lower-is-better quantity as `best / value`, in `0.0..=1.0`.
///
/// **Not** min-max normalization, and the difference matters. Min-max
/// stretches whatever spread happens to be present across the full range,
/// so with two offers a one-cent gap and a hundred-dollar gap both produce
/// a 0.0-vs-1.0 split. Any price difference, however trivial, then
/// outweighs every other dimension — which in practice means a provider
/// that fails most of its jobs wins on being a cent cheaper, exactly the
/// race to the bottom `Balanced` exists to avoid.
///
/// A ratio is proportional and scale-invariant: an offer 20% dearer than
/// the best scores 0.83, whether the numbers are cents or thousands.
///
/// A free offer (`value <= 0`) dominates on that dimension rather than
/// dividing by zero.
fn ratio(best: f64, value: f64) -> f64 {
    if value <= 0.0 {
        return 1.0;
    }
    if !best.is_finite() || best <= 0.0 {
        return 0.0;
    }
    (best / value).clamp(0.0, 1.0)
}

fn trust_score(tier: TrustTier) -> f64 {
    match tier {
        TrustTier::Community => 0.0,
        TrustTier::Standard => 0.4,
        TrustTier::Verified => 0.8,
        TrustTier::Private | TrustTier::Enterprise => 1.0,
    }
}

/// Parse a validated decimal amount for scoring.
///
/// Floats are fine *here* — this is a ranking heuristic, and a rounding
/// error changes a preference rather than a correctness check. The
/// within-cap test that actually gates an offer uses exact decimal
/// comparison in [`crate::offers`].
fn amount(m: &Money) -> Result<f64, Error> {
    m.amount
        .parse::<f64>()
        .map_err(|_| Error::Untrusted(format!("unparseable amount {:?}", m.amount)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::offers::OfferBook;
    use crate::test_support::*;
    use c0mpute_envelope::receipt::ValidationStatus;

    /// Build a book from (seed, price, duration) triples.
    fn book_of(job: &JobManifest, rows: &[(u8, &str, u64)]) -> Vec<OfferRecord> {
        let mut book = OfferBook::for_job(job).unwrap();
        for (seed, price, dur) in rows {
            book.submit(&offer_from(*seed, job, price, *dur), job, &now())
                .unwrap();
        }
        book.live(&now())
    }

    fn all_standard(_: &Did) -> TrustTier {
        TrustTier::Standard
    }

    #[test]
    fn cheapest_picks_the_lowest_price() {
        let job = gpu_job();
        let offers = book_of(&job, &[(7, "0.09", 5_000), (8, "0.02", 60_000)]);
        let sel = select(
            SelectionPolicy::Cheapest,
            &offers,
            &job,
            &ReputationLedger::new(),
            &all_standard,
        )
        .unwrap();
        assert_eq!(sel.winner().record.offer.price.amount, "0.02");
    }

    #[test]
    fn fastest_picks_the_shortest_quote_even_when_dearer() {
        let job = gpu_job();
        let offers = book_of(&job, &[(7, "0.09", 5_000), (8, "0.02", 60_000)]);
        let sel = select(
            SelectionPolicy::Fastest,
            &offers,
            &job,
            &ReputationLedger::new(),
            &all_standard,
        )
        .unwrap();
        assert_eq!(sel.winner().record.offer.expected_duration_ms, 5_000);
    }

    #[test]
    fn balanced_rejects_a_cheap_provider_with_a_bad_record() {
        // The race-to-the-bottom case. Seed 8 is marginally cheaper and
        // has failed most of its jobs; a pure price policy takes it and
        // pays in retries.
        let job = gpu_job();
        let offers = book_of(&job, &[(7, "0.06", 12_000), (8, "0.05", 12_000)]);

        let mut led = ReputationLedger::new();
        for _ in 0..20 {
            led.record(&receipt_for(7, 7, ValidationStatus::Accepted, 5_000, None))
                .unwrap();
        }
        for _ in 0..20 {
            led.record(&receipt_for(8, 8, ValidationStatus::Failed, 5_000, None))
                .unwrap();
        }

        let cheap = select(
            SelectionPolicy::Cheapest,
            &offers,
            &job,
            &led,
            &all_standard,
        )
        .unwrap();
        assert_eq!(cheap.winner().record.provider, identity(8).did().clone());

        let balanced = select(
            SelectionPolicy::Balanced,
            &offers,
            &job,
            &led,
            &all_standard,
        )
        .unwrap();
        assert_eq!(
            balanced.winner().record.provider,
            identity(7).did().clone(),
            "balanced should pay 0.01 more to avoid a provider that fails"
        );
    }

    #[test]
    fn with_no_history_balanced_falls_back_to_price_and_speed() {
        // Every provider scores a neutral 0.5, so reputation cancels and
        // the cheaper, faster offer wins. A brand-new network must still
        // make sensible choices.
        let job = gpu_job();
        let offers = book_of(&job, &[(7, "0.09", 30_000), (8, "0.02", 5_000)]);
        let sel = select(
            SelectionPolicy::Balanced,
            &offers,
            &job,
            &ReputationLedger::new(),
            &all_standard,
        )
        .unwrap();
        assert_eq!(sel.winner().record.provider, identity(8).did().clone());
    }

    #[test]
    fn trusted_prefers_a_higher_tier_over_a_lower_price() {
        let job = gpu_job();
        let offers = book_of(&job, &[(7, "0.09", 12_000), (8, "0.02", 12_000)]);
        let seven = identity(7).did().clone();
        let tiers = move |d: &Did| {
            if *d == seven {
                TrustTier::Verified
            } else {
                TrustTier::Community
            }
        };
        let sel = select(
            SelectionPolicy::Trusted,
            &offers,
            &job,
            &ReputationLedger::new(),
            &tiers,
        )
        .unwrap();
        assert_eq!(sel.winner().record.provider, identity(7).did().clone());
    }

    #[test]
    fn the_private_policy_demands_an_allowlist() {
        let job = gpu_job(); // no allowlist
        let offers = book_of(&job, &[(7, "0.05", 12_000)]);
        assert!(matches!(
            select(
                SelectionPolicy::Private,
                &offers,
                &job,
                &ReputationLedger::new(),
                &all_standard
            ),
            Err(Error::Untrusted(_))
        ));

        let mut allowed = gpu_job();
        allowed.requirements.providers = vec![identity(7).did().clone()];
        let offers = book_of(&allowed, &[(7, "0.05", 12_000)]);
        select(
            SelectionPolicy::Private,
            &offers,
            &allowed,
            &ReputationLedger::new(),
            &all_standard,
        )
        .unwrap();
    }

    #[test]
    fn an_empty_book_is_an_error_not_a_panic() {
        let job = gpu_job();
        assert!(matches!(
            select(
                SelectionPolicy::Balanced,
                &[],
                &job,
                &ReputationLedger::new(),
                &all_standard
            ),
            Err(Error::NoOffers)
        ));
    }

    #[test]
    fn a_single_offer_wins_without_dividing_by_a_zero_spread() {
        let job = gpu_job();
        let offers = book_of(&job, &[(7, "0.05", 12_000)]);
        let sel = select(
            SelectionPolicy::Balanced,
            &offers,
            &job,
            &ReputationLedger::new(),
            &all_standard,
        )
        .unwrap();
        assert!(sel.winner().total.is_finite());
        assert_eq!(sel.runners_up().len(), 0);
    }

    #[test]
    fn identical_offers_rank_deterministically() {
        let job = gpu_job();
        let offers = book_of(&job, &[(7, "0.05", 12_000), (8, "0.05", 12_000)]);
        let run = || {
            select(
                SelectionPolicy::Balanced,
                &offers,
                &job,
                &ReputationLedger::new(),
                &all_standard,
            )
            .unwrap()
            .winner()
            .record
            .provider
            .clone()
        };
        assert_eq!(run(), run());

        // And reversing the input does not change the answer.
        let mut reversed = offers.clone();
        reversed.reverse();
        let other = select(
            SelectionPolicy::Balanced,
            &reversed,
            &job,
            &ReputationLedger::new(),
            &all_standard,
        )
        .unwrap();
        assert_eq!(run(), other.winner().record.provider);
    }

    #[test]
    fn ranking_keeps_every_candidate_for_inspection() {
        let job = gpu_job();
        let offers = book_of(
            &job,
            &[(7, "0.09", 5_000), (8, "0.02", 60_000), (9, "0.05", 9_000)],
        );
        let sel = select(
            SelectionPolicy::Balanced,
            &offers,
            &job,
            &ReputationLedger::new(),
            &all_standard,
        )
        .unwrap();
        assert_eq!(sel.ranked.len(), 3);
        assert_eq!(sel.runners_up().len(), 2);
        assert!(sel.winner().total >= sel.ranked[1].total);
        assert!(sel.explain().contains("won under balanced"));
    }

    #[test]
    fn policy_names_round_trip() {
        for p in [
            SelectionPolicy::Cheapest,
            SelectionPolicy::Fastest,
            SelectionPolicy::Balanced,
            SelectionPolicy::Trusted,
            SelectionPolicy::Private,
        ] {
            assert_eq!(SelectionPolicy::parse(p.as_str()).unwrap(), p);
        }
        assert!(SelectionPolicy::parse("whatever").is_err());
        assert_eq!(SelectionPolicy::default(), SelectionPolicy::Balanced);
    }
}
