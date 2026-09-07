//! The buyer-side market: who can do this job, what did they quote, and
//! which quote wins.
//!
//! This crate is the answer to "if nobody is in the middle, who decides?"
//! The answer is: whoever is running this code. A buyer's CLI, a gateway
//! the buyer explicitly delegated to, or an organization's private
//! scheduler all link the same logic and reach the same kind of decision
//! — and none of them is privileged over the others, because there is no
//! network-wide matcher for them to be privileged *by*.
//!
//! ```text
//! adverts ──> ProviderDirectory ──┐
//!                                 ├──> select(policy) ──> winning offer
//! offers  ──> OfferBook ──────────┤
//! receipts ─> ReputationLedger ───┘
//! ```
//!
//! Four pieces:
//!
//! - [`directory::ProviderDirectory`] — the newest signed advert per
//!   provider, expiry and supersession enforced.
//! - [`offers::OfferBook`] — verified offers for one job, one slot per
//!   provider, order-independent.
//! - [`reputation::ReputationLedger`] — provider statistics derived from
//!   signed receipts, rebuildable from a receipt log.
//! - [`policy::select`] — ranks eligible offers under a named policy.
//!
//! ## What makes this trustworthy without a trusted party
//!
//! Every input is a signed envelope, verified at the boundary as it
//! enters. Nothing downstream has to ask where a record came from: a
//! forged advert fails on insert, a receipt about someone else's work is
//! refused, and an offer that was edited in flight never reaches scoring.
//! An indexer that feeds this crate can be stale, partial or hostile and
//! the worst it achieves is showing you fewer providers than exist.
//!
//! ## Deliberate non-dependencies
//!
//! No transport, no HTTP, no settlement product — same rule as
//! `c0mpute-envelope` (DIP-0024). Time enters as a `Timestamp` parameter
//! rather than being read from the clock, which is what makes every
//! expiry and freshness path testable rather than a matter of waiting.

pub mod directory;
pub mod matching;
pub mod offers;
pub mod policy;
pub mod reputation;

#[cfg(test)]
mod test_support;

pub use directory::{Accepted as AdvertAccepted, ProviderDirectory, ProviderRecord};
pub use matching::{Mismatch, is_eligible, mismatches, summarize};
pub use offers::{Accepted as OfferAccepted, OfferBook, OfferRecord};
pub use policy::{ScoredOffer, Selection, SelectionPolicy, select};
pub use reputation::{ProviderStats, ReputationLedger};

/// Everything that can go wrong running a market.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A record failed verification, expiry, or its own validity rules.
    #[error(transparent)]
    Envelope(c0mpute_envelope::Error),

    /// A record verified, but does not say what the caller was told it
    /// said — a receipt signed by the wrong identity, an offer for another
    /// job, a quote paired with an unrelated receipt.
    #[error("untrusted input: {0}")]
    Untrusted(String),

    /// Selection was asked to choose from nothing.
    #[error("no eligible offers to choose from")]
    NoOffers,
}
