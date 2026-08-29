//! Storage tiers and their redundancy parameters (CIP-001).
//!
//! The tier picks `(k, parity)`, and `(k, parity)` is the cost of goods:
//! the expansion factor `n/k` is how many raw GB a provider is paid for per
//! usable GB sold. See `docs/prds/001-storage-program.md` for the pricing that
//! falls out of these numbers.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Redundancy tier. Determines `(k, parity)` and therefore expansion factor,
/// failure tolerance, and repair amplification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// 3-copy replication (k=1, parity=2). 3.0x expansion, 1x repair
    /// amplification, tolerates 2 losses. For small, hot, frequently
    /// rewritten data — filesystem metadata and inline small files.
    Hot,
    /// Reed-Solomon 10/14. 1.4x expansion, tolerates 4 losses. The default.
    #[default]
    Standard,
    /// Reed-Solomon 20/32. 1.6x expansion, tolerates 12 losses. For
    /// irreplaceable data with long retention.
    Critical,
}

impl Tier {
    /// Data shards. Also the repair amplification factor: rebuilding one lost
    /// shard requires reading `k` shards.
    pub const fn k(self) -> usize {
        match self {
            Tier::Hot => 1,
            Tier::Standard => 10,
            Tier::Critical => 20,
        }
    }

    /// Parity shards. Also the number of simultaneous losses tolerated.
    pub const fn parity(self) -> usize {
        match self {
            Tier::Hot => 2,
            Tier::Standard => 4,
            Tier::Critical => 12,
        }
    }

    /// Total shards per block.
    pub const fn n(self) -> usize {
        self.k() + self.parity()
    }

    /// Raw bytes stored per usable byte.
    pub fn expansion(self) -> f64 {
        self.n() as f64 / self.k() as f64
    }

    /// Retail price in USD per usable GB-month (CIP-001).
    pub const fn price_usd_per_gb_month(self) -> f64 {
        match self {
            Tier::Hot => 0.006,
            Tier::Standard => 0.0035,
            Tier::Critical => 0.005,
        }
    }

    /// Recover the tier from `(k, parity)` read off an older manifest that
    /// predates the tier field.
    pub fn from_params(k: u8, parity: u8) -> Option<Self> {
        [Tier::Hot, Tier::Standard, Tier::Critical]
            .into_iter()
            .find(|t| t.k() == k as usize && t.parity() == parity as usize)
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Tier::Hot => "hot",
            Tier::Standard => "standard",
            Tier::Critical => "critical",
        })
    }
}

impl FromStr for Tier {
    type Err = TierParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "hot" => Ok(Tier::Hot),
            "standard" | "" => Ok(Tier::Standard),
            "critical" => Ok(Tier::Critical),
            other => Err(TierParseError(other.to_string())),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("unknown storage tier `{0}` (expected hot, standard, or critical)")]
pub struct TierParseError(pub String);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expansion_factors_match_cip_001() {
        assert_eq!(Tier::Hot.expansion(), 3.0);
        assert!((Tier::Standard.expansion() - 1.4).abs() < 1e-9);
        assert!((Tier::Critical.expansion() - 1.6).abs() < 1e-9);
    }

    #[test]
    fn standard_is_the_default() {
        assert_eq!(Tier::default(), Tier::Standard);
        assert_eq!(Tier::Standard.k(), 10);
        assert_eq!(Tier::Standard.n(), 14);
    }

    #[test]
    fn parses_and_displays() {
        assert_eq!("hot".parse::<Tier>().unwrap(), Tier::Hot);
        assert_eq!("CRITICAL".parse::<Tier>().unwrap(), Tier::Critical);
        assert_eq!(Tier::Standard.to_string(), "standard");
        assert!("glacier".parse::<Tier>().is_err());
    }

    #[test]
    fn recovers_tier_from_legacy_params() {
        assert_eq!(Tier::from_params(10, 4), Some(Tier::Standard));
        assert_eq!(Tier::from_params(20, 12), Some(Tier::Critical));
        assert_eq!(Tier::from_params(7, 3), None);
    }

    /// The cheaper tier must genuinely be cheaper to supply, or the pricing in
    /// CIP-001 is upside down.
    #[test]
    fn standard_costs_less_to_supply_than_hot() {
        let payout = 0.0015_f64; // $/raw GB-month
        assert!(Tier::Standard.expansion() * payout < Tier::Hot.expansion() * payout);
    }
}
