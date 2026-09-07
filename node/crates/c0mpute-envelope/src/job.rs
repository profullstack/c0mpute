//! `c0mpute.job/v2` — a buyer describes work rather than a machine.
//!
//! The manifest is the network's central portable abstraction. It says
//! what to run, what the provider must have, how much the buyer will pay,
//! how the result gets checked, and who settles it — and it says all of
//! that without naming a provider, a scheduler, or a host. That absence is
//! the point: the same manifest is schedulable by a local CLI, by a
//! managed gateway the buyer delegated to, or by a private organization
//! scheduler, and none of them is privileged over the others.
//!
//! ## Job identity
//!
//! A job's id is the content hash of its canonical manifest —
//! [`JobManifest::id`]. The v2 direction sketches an `id` field *inside*
//! the manifest, which forces every implementation to agree on which
//! fields to strip before hashing. Deriving the id from the whole manifest
//! removes that step and the class of bugs that lives in it: two nodes
//! that hold the same manifest cannot disagree about what to call it.

use serde::{Deserialize, Serialize};

use crate::Error;
use crate::advert::validate_workload_type;
use crate::canonical::canonicalize;
use crate::envelope::Payload;
use crate::identity::Did;
use crate::types::{ContentHash, Money, SettlementAdapter, Timestamp, TrustTier, ValidationPolicy};

/// Ceiling on `execution.retry`. Retries multiply spend against the same
/// escrow, so the manifest states a bound the buyer can see.
pub const MAX_RETRIES: u32 = 10;

/// A signed, immutable description of work to be done.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobManifest {
    pub workload: WorkloadRef,
    /// The buyer. Carried in the payload as well as the envelope so that a
    /// manifest quoted inside an offer or receipt still names its origin.
    /// [`JobManifest::signed_by`] checks the two agree.
    pub requester: Did,
    #[serde(default)]
    pub requirements: Requirements,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<JobInput>,
    pub execution: Execution,
    pub economics: Economics,
    #[serde(default)]
    pub validation: ValidationPolicy,
    #[serde(default)]
    pub output: JobOutput,
    pub announced_at: Timestamp,
    /// After this instant the job is not schedulable. Offers referencing
    /// it should be dropped.
    pub deadline: Timestamp,
}

/// Which typed workload to run, and which versions of it will do.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkloadRef {
    #[serde(rename = "type")]
    pub type_id: String,
    /// A semver range, e.g. `>=1 <2`. Absent means any version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// What a provider must have to be eligible.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Requirements {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu: Option<GpuRequirement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_cores: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "memoryGiB")]
    pub memory_gib: Option<Bound>,
    /// Acceptable regions. Empty means the buyer does not care — and note
    /// that a non-empty list can only match providers that chose to
    /// disclose a region.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub region: Vec<String>,
    #[serde(default = "default_tier")]
    pub trust_tier: TrustTier,
    /// Explicit provider allowlist. Required for the `private` trust tier,
    /// which means exactly "these identities and no others".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<Did>,
}

fn default_tier() -> TrustTier {
    TrustTier::Community
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuRequirement {
    #[serde(default = "one_u32")]
    pub count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "vramGiB")]
    pub vram_gib: Option<Bound>,
    /// Required runtime features, e.g. `cuda`, `nvenc`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
}

fn one_u32() -> u32 {
    1
}

/// An inclusive numeric bound. At least one end must be set.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bound {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<u32>,
}

impl Bound {
    pub fn at_least(min: u32) -> Self {
        Self {
            min: Some(min),
            max: None,
        }
    }

    pub fn contains(&self, v: u32) -> bool {
        self.min.is_none_or(|m| v >= m) && self.max.is_none_or(|m| v <= m)
    }

    fn validate(&self) -> Result<(), Error> {
        match (self.min, self.max) {
            (None, None) => Err(Error::Format("a bound must set min, max, or both".into())),
            (Some(lo), Some(hi)) if lo > hi => {
                Err(Error::Format(format!("bound min {lo} is above max {hi}")))
            }
            _ => Ok(()),
        }
    }
}

/// Where the job's input comes from.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobInput {
    /// Content address, e.g. `c0://blake3:…`.
    ///
    /// A content hash rather than a mutable URL: the provider can verify
    /// it fetched what the buyer signed for, and the input can be served
    /// by a cache, a storage provider, or an HTTP gateway without changing
    /// the manifest or its hash.
    #[serde(rename = "ref")]
    pub reference: ContentHash,
    /// How the bytes at `ref` are encrypted, if they are. Absent means
    /// plaintext — appropriate for public inputs, not for private work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encryption: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
}

/// How the workload runs on the provider.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Execution {
    pub timeout_seconds: u32,
    #[serde(default)]
    pub retry: u32,
    #[serde(default)]
    pub isolation: Isolation,
    /// Digest of the runtime image or plugin artifact. Pinning it by
    /// digest is what makes a receipt reproducible: "this input, through
    /// exactly this code, produced this output".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_digest: Option<ContentHash>,
    /// Whether the workload may reach the network while it runs. Off by
    /// default — a provider running untrusted work should have to opt in.
    #[serde(default)]
    pub network: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Isolation {
    /// In-process, for first-party typed plugins the provider already
    /// trusts.
    Process,
    /// Rootless container. The default for anything else.
    #[default]
    Container,
    /// Full virtual machine.
    Vm,
}

/// Price, settlement rail, and escrow.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Economics {
    /// The most the buyer will pay. Offers above it are not eligible.
    pub max_price: Money,
    /// Which settlement implementation settles this job. Named, not
    /// assumed: the protocol carries the choice so that adopting c0mpute
    /// does not mean adopting one payment product.
    pub settlement: SettlementAdapter,
    #[serde(default)]
    pub escrow: bool,
}

/// Limits on what comes back.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobOutput {
    /// How long the provider should keep the result available.
    pub retention_seconds: u32,
    /// Cap on the result size, so a provider knows what it is agreeing to
    /// store and transfer before it bids.
    pub max_bytes: u64,
}

impl Default for JobOutput {
    fn default() -> Self {
        Self {
            retention_seconds: 3600,
            max_bytes: 10 * 1024 * 1024,
        }
    }
}

impl JobManifest {
    /// The job's id: the content hash of the canonical manifest.
    pub fn id(&self) -> Result<ContentHash, Error> {
        Ok(ContentHash::of(&canonicalize(self)?))
    }

    /// Whether the manifest's stated requester matches the identity that
    /// sealed it. An envelope proves *someone* signed the manifest; this
    /// proves it was the buyer the manifest names.
    pub fn signed_by(&self, signer: &Did) -> bool {
        &self.requester == signer
    }

    /// Whether the job is still schedulable at `now`.
    pub fn is_open_at(&self, now: &Timestamp) -> bool {
        !self.deadline.is_expired_at(now)
    }
}

impl Payload for JobManifest {
    const TYPE: &'static str = "c0mpute.job/v2";

    fn expires_at(&self) -> Option<&Timestamp> {
        Some(&self.deadline)
    }

    fn validate(&self) -> Result<(), Error> {
        validate_workload_type(&self.workload.type_id)?;

        if self.deadline <= self.announced_at {
            return Err(Error::Format(format!(
                "job deadline ({}) must be after announcedAt ({})",
                self.deadline, self.announced_at
            )));
        }

        if self.execution.timeout_seconds == 0 {
            return Err(Error::Format(
                "execution timeout must be greater than 0".into(),
            ));
        }
        let window_ms = self.deadline.unix_ms() - self.announced_at.unix_ms();
        let timeout_ms = i64::from(self.execution.timeout_seconds) * 1000;
        if timeout_ms > window_ms {
            return Err(Error::Format(format!(
                "execution timeout of {}s cannot fit in the {}ms window before the deadline",
                self.execution.timeout_seconds, window_ms
            )));
        }
        if self.execution.retry > MAX_RETRIES {
            return Err(Error::Format(format!(
                "retry count {} exceeds the maximum of {MAX_RETRIES}",
                self.execution.retry
            )));
        }

        self.economics.max_price.validate()?;
        self.economics.settlement.validate()?;
        self.validation.validate()?;

        if let Some(gpu) = &self.requirements.gpu {
            if gpu.count == 0 {
                return Err(Error::Format(
                    "a GPU requirement must ask for at least one GPU".into(),
                ));
            }
            if let Some(b) = &gpu.vram_gib {
                b.validate()?;
            }
        }
        if let Some(b) = &self.requirements.memory_gib {
            b.validate()?;
        }
        if self.requirements.cpu_cores == Some(0) {
            return Err(Error::Format(
                "a CPU requirement must ask for at least one core".into(),
            ));
        }

        // `private` means "this allowlist"; without one it silently
        // degrades into "any provider that claims the private tier",
        // which is the opposite of what the buyer asked for.
        if self.requirements.trust_tier == TrustTier::Private
            && self.requirements.providers.is_empty()
        {
            return Err(Error::Format(
                "the private trust tier requires an explicit provider allowlist".into(),
            ));
        }

        if self.output.max_bytes == 0 {
            return Err(Error::Format(
                "output maxBytes must be greater than 0".into(),
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
        SigningIdentity::from_seed(&[42u8; 32])
    }

    fn manifest() -> JobManifest {
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

    #[test]
    fn a_well_formed_manifest_validates_and_seals() {
        let m = manifest();
        m.validate().unwrap();
        let env = Envelope::seal(&buyer(), m).unwrap();
        let opened = env.open().unwrap();
        assert!(opened.signed_by(&env.signer));
    }

    #[test]
    fn job_id_is_the_content_hash_of_the_manifest() {
        let a = manifest().id().unwrap();
        let b = manifest().id().unwrap();
        assert_eq!(a, b, "the same manifest must have the same id everywhere");

        let mut changed = manifest();
        changed.economics.max_price = Money::new("0.11", "USD");
        assert_ne!(changed.id().unwrap(), a, "any change is a different job");
    }

    #[test]
    fn job_id_does_not_depend_on_json_key_order() {
        let m = manifest();
        let id = m.id().unwrap();
        // Round-trip through an untyped Value, which reorders nothing
        // predictably, then back into a manifest.
        let shuffled = serde_json::to_string(&serde_json::to_value(&m).unwrap()).unwrap();
        let reparsed: JobManifest = serde_json::from_str(&shuffled).unwrap();
        assert_eq!(reparsed.id().unwrap(), id);
    }

    #[test]
    fn signed_by_catches_a_manifest_sealed_by_someone_else() {
        let m = manifest();
        let impostor = SigningIdentity::from_seed(&[43u8; 32]);
        let env = Envelope::seal(&impostor, m).unwrap();
        // The envelope is authentic — the impostor really did sign it —
        // but it is not the requester the manifest names.
        let opened = env.open().unwrap();
        assert!(!opened.signed_by(&env.signer));
    }

    #[test]
    fn deadline_must_follow_announcement() {
        let mut m = manifest();
        m.deadline = m.announced_at.clone();
        assert!(m.validate().is_err());
    }

    #[test]
    fn a_timeout_that_cannot_fit_before_the_deadline_is_rejected() {
        let mut m = manifest();
        m.execution.timeout_seconds = 3600; // 1h into a 10min window
        assert!(m.validate().is_err());
    }

    #[test]
    fn zero_timeout_is_rejected() {
        let mut m = manifest();
        m.execution.timeout_seconds = 0;
        assert!(m.validate().is_err());
    }

    #[test]
    fn excessive_retries_are_rejected() {
        let mut m = manifest();
        m.execution.retry = MAX_RETRIES + 1;
        assert!(m.validate().is_err());
    }

    #[test]
    fn the_private_tier_requires_an_allowlist() {
        let mut m = manifest();
        m.requirements.trust_tier = TrustTier::Private;
        assert!(m.validate().is_err());

        m.requirements.providers = vec![SigningIdentity::from_seed(&[9u8; 32]).did().clone()];
        m.validate().unwrap();
    }

    #[test]
    fn an_impossible_bound_is_rejected() {
        let mut m = manifest();
        m.requirements.memory_gib = Some(Bound {
            min: Some(64),
            max: Some(32),
        });
        assert!(m.validate().is_err());

        m.requirements.memory_gib = Some(Bound {
            min: None,
            max: None,
        });
        assert!(m.validate().is_err());
    }

    #[test]
    fn bounds_match_inclusively() {
        assert!(Bound::at_least(24).contains(24));
        assert!(Bound::at_least(24).contains(48));
        assert!(!Bound::at_least(24).contains(23));
        let range = Bound {
            min: Some(8),
            max: Some(16),
        };
        assert!(range.contains(8) && range.contains(16));
        assert!(!range.contains(17));
    }

    #[test]
    fn network_access_is_off_unless_asked_for() {
        let json = r#"{"timeoutSeconds":60}"#;
        let e: Execution = serde_json::from_str(json).unwrap();
        assert!(
            !e.network,
            "untrusted workloads must not get the network by default"
        );
        assert_eq!(e.isolation, Isolation::Container);
        assert_eq!(e.retry, 0);
    }

    #[test]
    fn a_manifest_past_its_deadline_is_closed() {
        let m = manifest();
        assert!(m.is_open_at(&ts("2026-09-06T16:05:00.000Z")));
        assert!(!m.is_open_at(&ts("2026-09-06T16:11:00.000Z")));
    }

    #[test]
    fn manifest_round_trips_through_canonical_json() {
        let env = Envelope::seal(&buyer(), manifest()).unwrap();
        let json = env.to_canonical_json().unwrap();
        assert!(json.contains("\"maxPrice\""));
        let back: Envelope<JobManifest> = Envelope::from_json(&json).unwrap();
        assert_eq!(
            back.open().unwrap().id().unwrap(),
            env.payload.id().unwrap()
        );
    }

    #[test]
    fn settlement_is_named_not_assumed() {
        let mut m = manifest();
        m.economics.settlement = SettlementAdapter("lightning".into());
        m.validate().unwrap();
        let env = Envelope::seal(&buyer(), m).unwrap();
        assert!(env.to_canonical_json().unwrap().contains("\"lightning\""));
    }
}
