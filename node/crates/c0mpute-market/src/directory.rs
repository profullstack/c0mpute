//! What this node currently believes about available capacity.
//!
//! The directory holds the newest signed advert per provider. It is a
//! **cache of other people's claims**, never a source of truth, and the
//! code is written so that stays true:
//!
//! - every advert is signature-verified on the way in, so it does not
//!   matter whether it arrived over gossip, from an indexer, or pasted
//!   from a file — an indexer that lies produces a rejected insert, not a
//!   bad match;
//! - adverts expire, and expiry is checked on read as well as on write,
//!   so a directory that has not been pruned still cannot hand out stale
//!   capacity;
//! - a lower sequence number never replaces a higher one, so replaying an
//!   old advert cannot roll a provider's claims backwards.
//!
//! Nothing here needs a network, a database, or a hosted service.

use std::collections::HashMap;

use c0mpute_envelope::advert::ProviderAdvert;
use c0mpute_envelope::job::JobManifest;
use c0mpute_envelope::{Did, Envelope, Timestamp};

use crate::Error;
use crate::matching::{Mismatch, mismatches};

/// One provider's current claim.
#[derive(Clone, Debug)]
pub struct ProviderRecord {
    pub provider: Did,
    pub advert: ProviderAdvert,
    /// Content hash of the sealed advert, so an offer can cite the exact
    /// claim it was made against.
    pub advert_hash: c0mpute_envelope::ContentHash,
}

/// What happened to an advert on the way in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Accepted {
    /// First advert seen from this provider.
    New,
    /// Replaced an older advert from the same provider.
    Superseded,
    /// Ignored: we already hold this sequence or a newer one.
    Stale,
}

/// The newest signed advert per provider.
#[derive(Debug, Default)]
pub struct ProviderDirectory {
    by_provider: HashMap<String, ProviderRecord>,
}

impl ProviderDirectory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Verify and record an advert.
    ///
    /// Errors if the envelope does not verify or the advert has expired.
    /// A stale-but-valid advert is not an error — it is the ordinary
    /// result of hearing the same provider twice.
    pub fn insert(
        &mut self,
        envelope: &Envelope<ProviderAdvert>,
        now: &Timestamp,
    ) -> Result<Accepted, Error> {
        // Verification happens here, once, at the boundary. Everything
        // downstream can then treat a ProviderRecord as something the
        // named provider actually signed.
        let advert = envelope.open_fresh(now).map_err(Error::Envelope)?;
        let provider = envelope.signer.clone();
        let key = provider.as_str().to_string();

        if let Some(existing) = self.by_provider.get(&key) {
            if !advert.supersedes(&existing.advert) {
                return Ok(Accepted::Stale);
            }
        }

        let outcome = if self.by_provider.contains_key(&key) {
            Accepted::Superseded
        } else {
            Accepted::New
        };

        self.by_provider.insert(
            key,
            ProviderRecord {
                provider,
                advert: advert.clone(),
                advert_hash: envelope.content_hash().map_err(Error::Envelope)?,
            },
        );
        Ok(outcome)
    }

    /// Providers whose advert is still valid at `now`.
    pub fn live(&self, now: &Timestamp) -> impl Iterator<Item = &ProviderRecord> {
        self.by_provider
            .values()
            .filter(move |r| !r.advert.expires_at.is_expired_at(now))
    }

    /// Providers eligible for `job`, in deterministic DID order.
    ///
    /// Ordering matters: two nodes running the same policy over the same
    /// adverts should reach the same answer, and a HashMap's iteration
    /// order would make that untrue in a way that only shows up
    /// occasionally.
    pub fn eligible(&self, job: &JobManifest, now: &Timestamp) -> Vec<ProviderRecord> {
        let mut out: Vec<ProviderRecord> = self
            .live(now)
            .filter(|r| mismatches(&r.advert, &r.provider, job).is_empty())
            .cloned()
            .collect();
        out.sort_by(|a, b| a.provider.as_str().cmp(b.provider.as_str()));
        out
    }

    /// [`ProviderDirectory::eligible`], plus why everyone else missed out.
    ///
    /// This is what turns "no providers found" from a dead end into a
    /// diagnosis.
    pub fn explain(
        &self,
        job: &JobManifest,
        now: &Timestamp,
    ) -> (Vec<ProviderRecord>, Vec<(Did, Vec<Mismatch>)>) {
        let mut eligible = Vec::new();
        let mut rejected = Vec::new();
        for r in self.live(now) {
            let ms = mismatches(&r.advert, &r.provider, job);
            if ms.is_empty() {
                eligible.push(r.clone());
            } else {
                rejected.push((r.provider.clone(), ms));
            }
        }
        eligible.sort_by(|a, b| a.provider.as_str().cmp(b.provider.as_str()));
        rejected.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        (eligible, rejected)
    }

    /// Look up one provider's current advert.
    pub fn get(&self, provider: &Did) -> Option<&ProviderRecord> {
        self.by_provider.get(provider.as_str())
    }

    /// Drop expired adverts. Optional — reads already ignore them — but
    /// worth running periodically so a long-lived process does not grow
    /// without bound on a churning network.
    pub fn prune(&mut self, now: &Timestamp) -> usize {
        let before = self.by_provider.len();
        self.by_provider
            .retain(|_, r| !r.advert.expires_at.is_expired_at(now));
        before - self.by_provider.len()
    }

    /// Total adverts held, including expired ones not yet pruned.
    pub fn len(&self) -> usize {
        self.by_provider.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_provider.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use c0mpute_envelope::Money;

    #[test]
    fn a_verified_advert_is_accepted_and_found() {
        let mut dir = ProviderDirectory::new();
        let (did, env) = sealed_advert(7);
        assert_eq!(dir.insert(&env, &now()).unwrap(), Accepted::New);
        assert_eq!(dir.len(), 1);
        assert_eq!(dir.get(&did).unwrap().provider, did);
        assert_eq!(dir.eligible(&gpu_job(), &now()).len(), 1);
    }

    #[test]
    fn a_forged_advert_is_rejected() {
        // Exactly the attack an untrusted indexer would attempt: hand us a
        // real provider's identity with claims it never made.
        let mut dir = ProviderDirectory::new();
        let (_, mut env) = sealed_advert(7);
        env.payload.capabilities.gpus = vec![gpu(80, 8, &["cuda"])];

        assert!(dir.insert(&env, &now()).is_err());
        assert!(dir.is_empty(), "a forgery must not reach the directory");
    }

    #[test]
    fn an_expired_advert_is_rejected_on_insert() {
        let mut dir = ProviderDirectory::new();
        let (_, env) = sealed_advert(7);
        let late = ts("2026-09-06T16:20:00.000Z");
        assert!(dir.insert(&env, &late).is_err());
        assert!(dir.is_empty());
    }

    #[test]
    fn an_advert_that_expires_while_held_stops_being_offered() {
        // The realistic failure: a provider goes offline and its last
        // advert sits in our cache. Reads must not surface it as capacity
        // even if nothing has pruned yet.
        let mut dir = ProviderDirectory::new();
        let (_, env) = sealed_advert(7);
        dir.insert(&env, &now()).unwrap();

        let later = ts("2026-09-06T16:11:00.000Z");
        assert_eq!(dir.len(), 1, "still held");
        assert_eq!(dir.live(&later).count(), 0, "but not live");
        assert!(dir.eligible(&gpu_job(), &later).is_empty());

        assert_eq!(dir.prune(&later), 1);
        assert!(dir.is_empty());
    }

    #[test]
    fn a_higher_sequence_supersedes() {
        let mut dir = ProviderDirectory::new();
        let (did, env) = sealed_advert(7);
        dir.insert(&env, &now()).unwrap();

        let id = identity(7);
        let (_, mut newer) = provider_seeded(7);
        newer.sequence = 2;
        newer.pricing[0].price = Money::new("0.20", "USD");
        let newer_env = Envelope::seal(&id, newer).unwrap();

        assert_eq!(
            dir.insert(&newer_env, &now()).unwrap(),
            Accepted::Superseded
        );
        assert_eq!(dir.len(), 1, "one record per provider");
        assert_eq!(
            dir.get(&did).unwrap().advert.pricing[0].price.amount,
            "0.20"
        );
    }

    #[test]
    fn replaying_an_older_advert_cannot_roll_a_provider_back() {
        let mut dir = ProviderDirectory::new();
        let id = identity(7);

        let (did, mut v2) = provider_seeded(7);
        v2.sequence = 2;
        v2.capabilities.workloads = vec![WORKLOAD.into()];
        dir.insert(&Envelope::seal(&id, v2).unwrap(), &now())
            .unwrap();

        // A perfectly valid, correctly signed, older advert.
        let (_, v1) = provider_seeded(7);
        let replayed = Envelope::seal(&id, v1).unwrap();
        assert_eq!(dir.insert(&replayed, &now()).unwrap(), Accepted::Stale);
        assert_eq!(dir.get(&did).unwrap().advert.sequence, 2);
    }

    #[test]
    fn re_inserting_the_same_sequence_is_stale_not_an_error() {
        let mut dir = ProviderDirectory::new();
        let (_, env) = sealed_advert(7);
        dir.insert(&env, &now()).unwrap();
        assert_eq!(dir.insert(&env, &now()).unwrap(), Accepted::Stale);
        assert_eq!(dir.len(), 1);
    }

    #[test]
    fn providers_are_tracked_independently() {
        let mut dir = ProviderDirectory::new();
        for seed in [7u8, 8, 9] {
            let (_, env) = sealed_advert(seed);
            dir.insert(&env, &now()).unwrap();
        }
        assert_eq!(dir.len(), 3);
        assert_eq!(dir.eligible(&gpu_job(), &now()).len(), 3);
    }

    #[test]
    fn eligible_order_is_deterministic() {
        let mut a = ProviderDirectory::new();
        let mut b = ProviderDirectory::new();
        for seed in [7u8, 8, 9, 10, 11] {
            let (_, env) = sealed_advert(seed);
            a.insert(&env, &now()).unwrap();
        }
        // Same adverts, opposite insertion order.
        for seed in [11u8, 10, 9, 8, 7] {
            let (_, env) = sealed_advert(seed);
            b.insert(&env, &now()).unwrap();
        }
        let ids = |d: &ProviderDirectory| -> Vec<String> {
            d.eligible(&gpu_job(), &now())
                .iter()
                .map(|r| r.provider.as_str().to_string())
                .collect()
        };
        assert_eq!(ids(&a), ids(&b));
    }

    #[test]
    fn explain_separates_the_eligible_from_the_rest() {
        let mut dir = ProviderDirectory::new();
        let (_, ok) = sealed_advert(7);
        dir.insert(&ok, &now()).unwrap();

        let (_, mut weak) = provider_seeded(8);
        weak.capabilities.gpus = vec![];
        dir.insert(&Envelope::seal(&identity(8), weak).unwrap(), &now())
            .unwrap();

        let (eligible, rejected) = dir.explain(&gpu_job(), &now());
        assert_eq!(eligible.len(), 1);
        assert_eq!(rejected.len(), 1);
        assert!(matches!(rejected[0].1[0], Mismatch::Gpu(_)));
    }

    #[test]
    fn the_advert_hash_is_recorded_so_an_offer_can_cite_it() {
        let mut dir = ProviderDirectory::new();
        let (did, env) = sealed_advert(7);
        dir.insert(&env, &now()).unwrap();
        assert_eq!(
            dir.get(&did).unwrap().advert_hash,
            env.content_hash().unwrap()
        );
    }
}
