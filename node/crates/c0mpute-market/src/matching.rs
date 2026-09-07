//! Does this provider satisfy this job's requirements?
//!
//! Eligibility is a hard filter, evaluated before price ever comes into
//! it. A provider that fails here is not a worse choice — it is not a
//! choice.
//!
//! Every rejection carries a [`Mismatch`] saying which requirement failed
//! and why. That is not decoration: "no providers found" is the single
//! most common thing a buyer will see on a young network, and being able
//! to answer *why* — nobody has 24 GiB of VRAM, or six providers do but
//! none disclosed a region — is the difference between a usable market
//! and a black box.

use c0mpute_envelope::advert::ProviderAdvert;
use c0mpute_envelope::job::JobManifest;
use c0mpute_envelope::{Did, TrustTier};

/// Why a provider is not eligible for a job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mismatch {
    /// The provider does not advertise this workload at all.
    WorkloadNotOffered { wanted: String },
    /// Not on the buyer's allowlist (the `private` trust tier).
    NotAllowlisted,
    /// The provider's claimed tier does not satisfy the job's.
    TrustTier { wanted: TrustTier, has: TrustTier },
    /// No disclosed region, or a region the buyer did not ask for.
    Region {
        wanted: Vec<String>,
        has: Option<String>,
    },
    /// Not enough GPUs, VRAM, or a missing GPU feature.
    Gpu(String),
    /// Not enough CPU cores.
    CpuCores { wanted: u32, has: u32 },
    /// Not enough memory.
    Memory { wanted: String, has: u32 },
}

impl std::fmt::Display for Mismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Mismatch::WorkloadNotOffered { wanted } => {
                write!(f, "does not offer workload {wanted}")
            }
            Mismatch::NotAllowlisted => {
                write!(f, "not on the job's provider allowlist")
            }
            Mismatch::TrustTier { wanted, has } => {
                write!(f, "claims trust tier {has:?}, job requires {wanted:?}")
            }
            Mismatch::Region { wanted, has } => match has {
                Some(r) => write!(f, "is in region {r}, job wants one of {wanted:?}"),
                None => write!(
                    f,
                    "discloses no region, and the job restricts to {wanted:?}"
                ),
            },
            Mismatch::Gpu(why) => write!(f, "GPU requirement unmet: {why}"),
            Mismatch::CpuCores { wanted, has } => {
                write!(f, "has {has} CPU cores, job needs {wanted}")
            }
            Mismatch::Memory { wanted, has } => {
                write!(f, "has {has} GiB of memory, job needs {wanted}")
            }
        }
    }
}

/// Check `advert` against `job`, returning every requirement it fails.
///
/// Returns all mismatches rather than the first, so a buyer looking at an
/// empty market can see the whole shape of the problem in one pass instead
/// of fixing one constraint at a time.
pub fn mismatches(advert: &ProviderAdvert, provider: &Did, job: &JobManifest) -> Vec<Mismatch> {
    let req = &job.requirements;
    let caps = &advert.capabilities;
    let mut out = Vec::new();

    if !advert.runs(&job.workload.type_id) {
        out.push(Mismatch::WorkloadNotOffered {
            wanted: job.workload.type_id.clone(),
        });
    }

    // An allowlist, when present, is absolute — it is checked before the
    // tier, because "one of these identities" is a stronger statement than
    // any tier claim and a provider cannot talk its way onto the list.
    if !req.providers.is_empty() && !req.providers.contains(provider) {
        out.push(Mismatch::NotAllowlisted);
    } else if !advert.trust.tier.satisfies(req.trust_tier) {
        out.push(Mismatch::TrustTier {
            wanted: req.trust_tier,
            has: advert.trust.tier,
        });
    }

    if !req.region.is_empty() {
        let has = advert.disclosure.as_ref().and_then(|d| d.region.as_deref());
        // Undisclosed is not a match. A provider that withheld its region
        // is not excluded from the network, only from jobs that constrain
        // on one — which is exactly the trade the disclosure opt-in makes.
        if !has.is_some_and(|r| req.region.iter().any(|w| w == r)) {
            out.push(Mismatch::Region {
                wanted: req.region.clone(),
                has: has.map(str::to_string),
            });
        }
    }

    if let Some(want) = &req.gpu {
        // Count GPUs that individually satisfy the VRAM and feature
        // requirements. Summing VRAM across cards would be wrong: a job
        // needing 24 GiB cannot run on two 12 GiB cards.
        let usable: u32 = caps
            .gpus
            .iter()
            .filter(|g| {
                want.vram_gib.is_none_or(|b| b.contains(g.vram_gib))
                    && want
                        .features
                        .iter()
                        .all(|f| g.features.iter().any(|has| has == f))
            })
            .map(|g| g.count)
            .sum();

        if usable < want.count {
            out.push(Mismatch::Gpu(if caps.gpus.is_empty() {
                "provider advertises no GPU".to_string()
            } else {
                format!(
                    "needs {} GPU(s) meeting the spec, provider has {usable}",
                    want.count
                )
            }));
        }
    }

    if let Some(cores) = req.cpu_cores {
        if caps.cpu.cores < cores {
            out.push(Mismatch::CpuCores {
                wanted: cores,
                has: caps.cpu.cores,
            });
        }
    }

    if let Some(mem) = &req.memory_gib {
        if !mem.contains(caps.memory_gib) {
            out.push(Mismatch::Memory {
                wanted: describe_bound(mem),
                has: caps.memory_gib,
            });
        }
    }

    out
}

/// Whether `advert` satisfies every requirement in `job`.
pub fn is_eligible(advert: &ProviderAdvert, provider: &Did, job: &JobManifest) -> bool {
    mismatches(advert, provider, job).is_empty()
}

fn describe_bound(b: &c0mpute_envelope::job::Bound) -> String {
    match (b.min, b.max) {
        (Some(lo), Some(hi)) => format!("{lo}-{hi}"),
        (Some(lo), None) => format!("at least {lo}"),
        (None, Some(hi)) => format!("at most {hi}"),
        (None, None) => "any".into(),
    }
}

/// A short human summary of why a set of providers was rejected, for the
/// "no eligible providers" case.
pub fn summarize(rejections: &[(Did, Vec<Mismatch>)]) -> String {
    if rejections.is_empty() {
        return "no providers known".into();
    }
    let mut counts: Vec<(String, usize)> = Vec::new();
    for (_, ms) in rejections {
        for m in ms {
            let key = match m {
                Mismatch::WorkloadNotOffered { .. } => "workload not offered",
                Mismatch::NotAllowlisted => "not allowlisted",
                Mismatch::TrustTier { .. } => "trust tier too low",
                Mismatch::Region { .. } => "region",
                Mismatch::Gpu(_) => "GPU",
                Mismatch::CpuCores { .. } => "CPU cores",
                Mismatch::Memory { .. } => "memory",
            };
            match counts.iter_mut().find(|(k, _)| k == key) {
                Some((_, n)) => *n += 1,
                None => counts.push((key.to_string(), 1)),
            }
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let parts: Vec<String> = counts.iter().map(|(k, n)| format!("{k}: {n}")).collect();
    format!(
        "{} provider(s) rejected — {}",
        rejections.len(),
        parts.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

    #[test]
    fn a_matching_provider_is_eligible() {
        let (did, advert) = gpu_provider();
        assert!(is_eligible(&advert, &did, &gpu_job()));
        assert_eq!(mismatches(&advert, &did, &gpu_job()), vec![]);
    }

    #[test]
    fn a_provider_that_does_not_offer_the_workload_is_rejected() {
        let (did, mut advert) = gpu_provider();
        advert.capabilities.workloads = vec!["whisper.transcribe".into()];
        let ms = mismatches(&advert, &did, &gpu_job());
        assert!(matches!(ms[0], Mismatch::WorkloadNotOffered { .. }));
    }

    #[test]
    fn vram_is_not_summed_across_cards() {
        // Two 12 GiB cards do not satisfy a 24 GiB requirement: a model
        // that does not fit on one card does not fit on two by addition.
        let (did, mut advert) = gpu_provider();
        advert.capabilities.gpus = vec![gpu(12, 2, &["cuda"])];
        let ms = mismatches(&advert, &did, &gpu_job());
        assert!(matches!(ms[0], Mismatch::Gpu(_)), "got {ms:?}");
    }

    #[test]
    fn multiple_qualifying_cards_do_count_toward_the_requested_count() {
        let (did, mut advert) = gpu_provider();
        advert.capabilities.gpus = vec![gpu(24, 4, &["cuda"])];
        let mut job = gpu_job();
        job.requirements.gpu.as_mut().unwrap().count = 3;
        assert!(is_eligible(&advert, &did, &job));

        job.requirements.gpu.as_mut().unwrap().count = 5;
        assert!(!is_eligible(&advert, &did, &job));
    }

    #[test]
    fn a_missing_gpu_feature_disqualifies_the_card() {
        let (did, mut advert) = gpu_provider();
        advert.capabilities.gpus = vec![gpu(24, 1, &["rocm"])];
        assert!(!is_eligible(&advert, &did, &gpu_job()));
    }

    #[test]
    fn no_gpu_at_all_reports_that_specifically() {
        let (did, mut advert) = gpu_provider();
        advert.capabilities.gpus = vec![];
        let ms = mismatches(&advert, &did, &gpu_job());
        assert_eq!(ms, vec![Mismatch::Gpu("provider advertises no GPU".into())]);
    }

    #[test]
    fn an_undisclosed_region_cannot_match_a_region_constraint() {
        let (did, mut advert) = gpu_provider();
        advert.disclosure = None;
        let mut job = gpu_job();
        job.requirements.region = vec!["us-west".into()];
        let ms = mismatches(&advert, &did, &job);
        assert!(matches!(ms[0], Mismatch::Region { has: None, .. }));
    }

    #[test]
    fn no_region_constraint_accepts_an_undisclosed_provider() {
        let (did, mut advert) = gpu_provider();
        advert.disclosure = None;
        let mut job = gpu_job();
        job.requirements.region = vec![];
        assert!(is_eligible(&advert, &did, &job));
    }

    #[test]
    fn a_weaker_trust_tier_is_rejected() {
        let (did, mut advert) = gpu_provider();
        advert.trust.tier = TrustTier::Community;
        let mut job = gpu_job();
        job.requirements.trust_tier = TrustTier::Verified;
        let ms = mismatches(&advert, &did, &job);
        assert!(matches!(ms[0], Mismatch::TrustTier { .. }));
    }

    #[test]
    fn the_allowlist_beats_a_tier_claim() {
        // A provider can claim any tier it likes. It cannot claim its way
        // onto someone else's allowlist.
        let (did, mut advert) = gpu_provider();
        advert.trust.tier = TrustTier::Private;
        let mut job = gpu_job();
        job.requirements.trust_tier = TrustTier::Private;
        job.requirements.providers = vec![identity(99).did().clone()];

        let ms = mismatches(&advert, &did, &job);
        assert_eq!(ms, vec![Mismatch::NotAllowlisted]);

        job.requirements.providers = vec![did.clone()];
        assert!(is_eligible(&advert, &did, &job));
    }

    #[test]
    fn insufficient_cpu_or_memory_is_rejected() {
        let (did, mut advert) = gpu_provider();
        advert.capabilities.cpu.cores = 2;
        advert.capabilities.memory_gib = 8;
        let mut job = gpu_job();
        job.requirements.cpu_cores = Some(16);

        let ms = mismatches(&advert, &did, &job);
        assert!(ms.contains(&Mismatch::CpuCores { wanted: 16, has: 2 }));
        assert!(ms.iter().any(|m| matches!(m, Mismatch::Memory { .. })));
    }

    #[test]
    fn every_failing_requirement_is_reported_not_just_the_first() {
        let (did, mut advert) = gpu_provider();
        advert.capabilities.workloads = vec!["other.thing".into()];
        advert.capabilities.gpus = vec![];
        advert.capabilities.cpu.cores = 1;
        let mut job = gpu_job();
        job.requirements.cpu_cores = Some(8);

        let ms = mismatches(&advert, &did, &job);
        assert!(ms.len() >= 3, "expected several mismatches, got {ms:?}");
    }

    #[test]
    fn summarize_counts_the_common_reasons() {
        let (did, mut advert) = gpu_provider();
        advert.capabilities.gpus = vec![];
        let ms = mismatches(&advert, &did, &gpu_job());
        let s = summarize(&[(did.clone(), ms.clone()), (did, ms)]);
        assert!(s.contains("2 provider(s) rejected"), "{s}");
        assert!(s.contains("GPU"), "{s}");
    }

    #[test]
    fn summarize_handles_an_empty_network() {
        assert_eq!(summarize(&[]), "no providers known");
    }
}
