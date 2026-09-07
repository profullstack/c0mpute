//! `c0mpute.provider.advert/v1` — a provider says what it can run.
//!
//! An advert is a signed, short-lived, self-contained claim. Anyone
//! holding one can check who made it and whether it is still current
//! without asking a registry. That is what lets indexers cache adverts
//! while remaining non-authoritative: an indexer can be stale or lying
//! about *which* adverts exist, but it cannot forge one, and a buyer that
//! doubts the index can collect adverts straight off the network instead.
//!
//! Adverts expire fast on purpose. A residential provider that loses power
//! leaves its last advert sitting in every peer's cache; a short lifetime
//! is what stops that cache from looking like live capacity.

use serde::{Deserialize, Serialize};

use crate::Error;
use crate::envelope::Payload;
use crate::identity::Did;
use crate::types::{Money, Timestamp, TrustTier};

/// Longest lifetime an advert may claim.
///
/// Long enough that a provider is not re-signing constantly, short enough
/// that a dead provider drops out of the market within a coffee break.
pub const MAX_ADVERT_LIFETIME_MS: i64 = 15 * 60 * 1000;

/// A provider's signed capability advertisement.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderAdvert {
    /// Monotonic counter, incremented on every re-advert. A peer holding
    /// two adverts from the same provider keeps the higher one — see
    /// [`ProviderAdvert::supersedes`].
    pub sequence: u64,
    pub issued_at: Timestamp,
    pub expires_at: Timestamp,
    pub capabilities: ProviderCapabilities,
    /// Indicative rates. The binding number is the one in a signed
    /// `offer/v1`; this is what a buyer filters on before asking.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pricing: Vec<Rate>,
    pub trust: ProviderTrust,
    /// Opt-in disclosure. Absent means "not saying", never "unknown to the
    /// provider" — a buyer that requires a region simply cannot match a
    /// provider that withheld it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disclosure: Option<Disclosure>,
}

/// What a provider can execute, and how much of it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCapabilities {
    pub cpu: CpuSpec,
    #[serde(rename = "memoryGiB")]
    pub memory_gib: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gpus: Vec<GpuSpec>,
    #[serde(default)]
    #[serde(rename = "storageGiB")]
    pub storage_gib: u64,
    #[serde(default)]
    pub bandwidth_mbps: u32,
    /// Workload namespaces this provider accepts, e.g.
    /// `infernet.inference`, `transcode.ffmpeg`.
    ///
    /// Acceptance is per-provider policy: advertising `whisper.transcribe`
    /// and refusing `oci.container` is the normal, expected posture for a
    /// machine someone also uses for other things.
    pub workloads: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CpuSpec {
    /// `x86_64`, `aarch64`, …
    pub arch: String,
    pub cores: u32,
}

/// A GPU or other accelerator.
///
/// Described by family and feature strings rather than an exact model
/// name: it keeps the protocol from having to learn every new SKU, and it
/// is one less fingerprint on a home machine (§25).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuSpec {
    /// `nvidia`, `amd`, `intel`, `apple`, …
    pub vendor: String,
    /// Architecture family, e.g. `ada`, `rdna3`, `m3`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    #[serde(rename = "vramGiB")]
    pub vram_gib: u32,
    /// Runtime features, e.g. `cuda`, `nvenc`, `rocm`, `metal`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
    /// How many identical units. Defaults to 1.
    #[serde(default = "one_u32")]
    pub count: u32,
}

fn one_u32() -> u32 {
    1
}

/// An indicative price for one workload, in one unit.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rate {
    /// Workload namespace this rate applies to.
    pub workload: String,
    /// A unit from §19.2, e.g. `gpu-second`, `1m-output-tokens`,
    /// `video-minute`, `gb-month`. An open string: plugins define their
    /// own units, and the protocol does not need to know what they mean to
    /// carry them.
    pub unit: String,
    pub price: Money,
}

/// The trust position a provider claims for itself.
///
/// Claims, not proof. A buyer's policy decides which claims it credits and
/// which credentials it verifies; the network does not adjudicate.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderTrust {
    pub tier: TrustTier,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credentials: Vec<Credential>,
}

impl Default for ProviderTrust {
    fn default() -> Self {
        // Community is the honest default: a fresh key with no history.
        Self {
            tier: TrustTier::Community,
            credentials: Vec::new(),
        }
    }
}

/// An assertion some other identity has made about this provider.
///
/// Deliberately thin: a type, who said it, and where the evidence lives.
/// Binding the protocol to one verifiable-credential format now would age
/// worse than carrying a pointer.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Credential {
    /// e.g. `coinpay.wallet`, `org.membership`, `hardware.attestation`,
    /// `kyb`, `sla`.
    #[serde(rename = "type")]
    pub type_id: String,
    /// The identity that issued it.
    pub issuer: Did,
    /// Where the credential document itself can be fetched or checked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
}

/// Location and network facts a provider chose to publish.
///
/// Every field is optional and every field is a choice. Publishing none of
/// it is a supported way to run a provider; a buyer that needs
/// jurisdiction control simply will not match you.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Disclosure {
    /// Coarse region, e.g. `us-west`, `eu-central`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// ISO country code, for jurisdiction-constrained work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    /// Autonomous system number, for provider-diversity scoring.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asn: Option<u32>,
}

impl ProviderAdvert {
    /// Whether this advert replaces `other`. Both must come from the same
    /// provider; the caller checks that, because an advert does not name
    /// its own signer (the envelope does).
    pub fn supersedes(&self, other: &ProviderAdvert) -> bool {
        self.sequence > other.sequence
    }

    /// Whether this provider can run `workload`.
    pub fn runs(&self, workload: &str) -> bool {
        self.capabilities.workloads.iter().any(|w| w == workload)
    }
}

impl Payload for ProviderAdvert {
    const TYPE: &'static str = "c0mpute.provider.advert/v1";

    fn expires_at(&self) -> Option<&Timestamp> {
        Some(&self.expires_at)
    }

    fn validate(&self) -> Result<(), Error> {
        if self.expires_at <= self.issued_at {
            return Err(Error::Format(format!(
                "advert expiresAt ({}) must be after issuedAt ({})",
                self.expires_at, self.issued_at
            )));
        }
        let lifetime = self.expires_at.unix_ms() - self.issued_at.unix_ms();
        if lifetime > MAX_ADVERT_LIFETIME_MS {
            return Err(Error::Format(format!(
                "advert lifetime of {lifetime}ms exceeds the {MAX_ADVERT_LIFETIME_MS}ms maximum; \
                 stale capacity is worse than a re-advert"
            )));
        }

        let caps = &self.capabilities;
        if caps.cpu.cores == 0 {
            return Err(Error::Format(
                "a provider must advertise at least 1 CPU core".into(),
            ));
        }
        if caps.cpu.arch.is_empty() {
            return Err(Error::Format("CPU architecture must not be empty".into()));
        }
        if caps.workloads.is_empty() {
            return Err(Error::Format(
                "a provider must advertise at least one workload; an advert with none is noise"
                    .into(),
            ));
        }
        for w in &caps.workloads {
            validate_workload_type(w)?;
        }
        let mut sorted = caps.workloads.clone();
        sorted.sort();
        sorted.dedup();
        if sorted.len() != caps.workloads.len() {
            return Err(Error::Format("advertised workloads must be unique".into()));
        }
        for gpu in &caps.gpus {
            if gpu.vendor.is_empty() {
                return Err(Error::Format("GPU vendor must not be empty".into()));
            }
            if gpu.count == 0 {
                return Err(Error::Format("GPU count must be at least 1".into()));
            }
        }

        for rate in &self.pricing {
            validate_workload_type(&rate.workload)?;
            if rate.unit.is_empty() {
                return Err(Error::Format("a rate must name a pricing unit".into()));
            }
            rate.price.validate()?;
            if !caps.workloads.contains(&rate.workload) {
                return Err(Error::Format(format!(
                    "priced workload {:?} is not in the advertised workload list",
                    rate.workload
                )));
            }
        }

        for cred in &self.trust.credentials {
            if cred.type_id.is_empty() {
                return Err(Error::Format("a credential must have a type".into()));
            }
        }
        Ok(())
    }
}

/// Check a workload namespace: lowercase, dot-separated, at least two
/// segments (`transcode.ffmpeg`, not `transcode`).
///
/// The two-segment rule keeps the namespace from filling with bare verbs
/// that different plugins would each read differently.
pub fn validate_workload_type(s: &str) -> Result<(), Error> {
    let bad = |why: &str| Error::Format(format!("workload type {s:?} {why}"));
    if s.is_empty() {
        return Err(bad("must not be empty"));
    }
    let segments: Vec<&str> = s.split('.').collect();
    if segments.len() < 2 {
        return Err(bad("must be namespaced, e.g. `transcode.ffmpeg`"));
    }
    for seg in segments {
        if seg.is_empty() {
            return Err(bad("has an empty segment"));
        }
        if !seg
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(bad("must be lowercase alphanumeric with dashes"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::Envelope;
    use crate::identity::SigningIdentity;

    fn ts(s: &str) -> Timestamp {
        Timestamp::parse(s).unwrap()
    }

    fn advert() -> ProviderAdvert {
        ProviderAdvert {
            sequence: 8472,
            issued_at: ts("2026-09-06T23:50:00.000Z"),
            expires_at: ts("2026-09-06T23:59:00.000Z"),
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
                    features: vec!["cuda".into(), "nvenc".into()],
                    count: 1,
                }],
                storage_gib: 1200,
                bandwidth_mbps: 1000,
                workloads: vec!["infernet.inference".into(), "transcode.ffmpeg".into()],
            },
            pricing: vec![Rate {
                workload: "transcode.ffmpeg".into(),
                unit: "video-minute".into(),
                price: Money::new("0.004", "USD"),
            }],
            trust: ProviderTrust {
                tier: TrustTier::Standard,
                credentials: vec![],
            },
            disclosure: Some(Disclosure {
                region: Some("us-west".into()),
                country: None,
                asn: Some(7922),
            }),
        }
    }

    #[test]
    fn a_well_formed_advert_validates_and_seals() {
        let a = advert();
        a.validate().unwrap();
        let env = Envelope::seal(&SigningIdentity::from_seed(&[1u8; 32]), a).unwrap();
        let opened = env.open().unwrap();
        assert!(opened.runs("transcode.ffmpeg"));
        assert!(!opened.runs("oci.container"));
    }

    #[test]
    fn advert_round_trips_through_canonical_json() {
        let env = Envelope::seal(&SigningIdentity::from_seed(&[2u8; 32]), advert()).unwrap();
        let json = env.to_canonical_json().unwrap();
        assert!(
            json.contains("\"expiresAt\""),
            "wire form is camelCase: {json}"
        );
        let back: Envelope<ProviderAdvert> = Envelope::from_json(&json).unwrap();
        back.open().unwrap();
        assert_eq!(back.content_hash().unwrap(), env.content_hash().unwrap());
    }

    #[test]
    fn a_long_lived_advert_is_rejected() {
        let mut a = advert();
        a.expires_at = ts("2026-09-07T23:59:00.000Z");
        assert!(a.validate().is_err(), "a 24h advert should not be allowed");
    }

    #[test]
    fn expiry_must_follow_issuance() {
        let mut a = advert();
        a.expires_at = a.issued_at.clone();
        assert!(a.validate().is_err());
    }

    #[test]
    fn an_advert_with_no_workloads_is_rejected() {
        let mut a = advert();
        a.capabilities.workloads.clear();
        a.pricing.clear();
        assert!(a.validate().is_err());
    }

    #[test]
    fn duplicate_workloads_are_rejected() {
        let mut a = advert();
        a.capabilities.workloads = vec!["transcode.ffmpeg".into(), "transcode.ffmpeg".into()];
        assert!(a.validate().is_err());
    }

    #[test]
    fn pricing_a_workload_you_do_not_run_is_rejected() {
        let mut a = advert();
        a.pricing[0].workload = "whisper.transcribe".into();
        assert!(a.validate().is_err());
    }

    #[test]
    fn a_float_price_cannot_reach_the_wire() {
        // The type system already forbids it; this guards the JSON path,
        // where a hand-written or foreign-implementation advert could try.
        let json = r#"{"workload":"transcode.ffmpeg","unit":"video-minute",
                       "price":{"amount":0.004,"currency":"USD"}}"#;
        assert!(serde_json::from_str::<Rate>(json).is_err());
    }

    #[test]
    fn higher_sequence_supersedes() {
        let old = advert();
        let mut new = advert();
        new.sequence = old.sequence + 1;
        assert!(new.supersedes(&old));
        assert!(!old.supersedes(&new));
        assert!(!old.supersedes(&old), "equal sequence is not a replacement");
    }

    #[test]
    fn an_expired_advert_fails_open_fresh_but_still_verifies() {
        let env = Envelope::seal(&SigningIdentity::from_seed(&[3u8; 32]), advert()).unwrap();
        let later = ts("2026-09-07T00:00:00.000Z");
        assert!(matches!(
            env.open_fresh(&later).unwrap_err(),
            crate::Error::Expired { .. }
        ));
        env.open().unwrap();
    }

    #[test]
    fn workload_namespaces_must_be_namespaced_and_lowercase() {
        for ok in [
            "transcode.ffmpeg",
            "infernet.inference",
            "oci.container",
            "a.b.c",
        ] {
            validate_workload_type(ok).unwrap();
        }
        for bad in [
            "",
            "transcode",
            "Transcode.FFmpeg",
            "transcode.",
            ".ffmpeg",
            "a..b",
            "a b.c",
        ] {
            assert!(
                validate_workload_type(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn disclosure_is_optional() {
        let mut a = advert();
        a.disclosure = None;
        a.validate().unwrap();
        let env = Envelope::seal(&SigningIdentity::from_seed(&[4u8; 32]), a).unwrap();
        let json = env.to_canonical_json().unwrap();
        assert!(
            !json.contains("disclosure"),
            "an undisclosed field should not appear on the wire at all: {json}"
        );
    }
}
