//! How a worker prices its bids: the operator's listing.
//!
//! The operator sets a floor (`min_usd`, per job) and a mode:
//!
//! - `dynamic` (the default): an idle node quotes [`DYNAMIC_IDLE_SHARE`] of
//!   the buyer's cap to win work, and the quote rises toward the full cap
//!   as the node's job queue fills. Never below the floor.
//! - `fixed`: always quote `fixed_usd` (raised to the floor if it is lower).
//!
//! Either way the node does not bid when its price would exceed the buyer's
//! cap. The listing rides in the capability ad's hardware blob, so the node
//! board and c0mpute.com/market show it.

use serde::{Deserialize, Serialize};

/// Share of the buyer's cap an idle node quotes in dynamic mode.
pub const DYNAMIC_IDLE_SHARE: f64 = 0.6;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PricingMode {
    #[default]
    Dynamic,
    Fixed,
}

impl PricingMode {
    pub fn as_str(self) -> &'static str {
        match self {
            PricingMode::Dynamic => "dynamic",
            PricingMode::Fixed => "fixed",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PricingConfig {
    #[serde(default)]
    pub mode: PricingMode,
    /// The lowest price per job this node accepts, in USD. 0 = no floor.
    #[serde(default)]
    pub min_usd: f64,
    /// The price per job in `fixed` mode, in USD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed_usd: Option<f64>,
}

impl PricingConfig {
    /// Reject negative or non-finite numbers and a fixed mode with no price.
    pub fn validate(&self) -> Result<(), String> {
        let ok = |v: f64| v.is_finite() && v >= 0.0;
        if !ok(self.min_usd) {
            return Err(format!(
                "min price must be a number >= 0, got {}",
                self.min_usd
            ));
        }
        match (self.mode, self.fixed_usd) {
            (PricingMode::Fixed, None) => Err("fixed mode needs a price (--fixed <usd>)".into()),
            (_, Some(f)) if !ok(f) => Err(format!("fixed price must be a number >= 0, got {f}")),
            _ => Ok(()),
        }
    }

    /// The price this node bids on a job capped at `max_usd`, with its queue
    /// `load` between 0.0 (idle) and 1.0 (full). `None` = do not bid.
    pub fn quote(&self, max_usd: f64, load: f64) -> Option<f64> {
        if !max_usd.is_finite() || max_usd < 0.0 {
            return None;
        }
        let floor = self.min_usd.max(0.0);
        let price = match self.mode {
            PricingMode::Fixed => self.fixed_usd.unwrap_or(max_usd).max(floor),
            PricingMode::Dynamic => {
                let load = load.clamp(0.0, 1.0);
                let share = DYNAMIC_IDLE_SHARE + (1.0 - DYNAMIC_IDLE_SHARE) * load;
                (max_usd * share).max(floor)
            }
        };
        let price = (price * 1e6).round() / 1e6;
        (price <= max_usd + 1e-9).then_some(price.min(max_usd))
    }

    /// The public listing carried in the capability ad.
    pub fn listing(&self) -> serde_json::Value {
        let mut v = serde_json::json!({ "mode": self.mode.as_str(), "min_usd": self.min_usd });
        if let (PricingMode::Fixed, Some(f)) = (self.mode, self.fixed_usd) {
            v["fixed_usd"] = serde_json::Value::from(f);
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dynamic(min: f64) -> PricingConfig {
        PricingConfig {
            mode: PricingMode::Dynamic,
            min_usd: min,
            fixed_usd: None,
        }
    }

    #[test]
    fn idle_dynamic_node_undercuts_the_cap() {
        assert_eq!(dynamic(0.0).quote(1.0, 0.0), Some(0.6));
    }

    #[test]
    fn dynamic_price_rises_with_load() {
        let p = dynamic(0.0);
        assert_eq!(p.quote(1.0, 0.5), Some(0.8));
        assert_eq!(p.quote(1.0, 1.0), Some(1.0));
        assert_eq!(p.quote(1.0, 7.0), Some(1.0), "load is clamped");
    }

    #[test]
    fn never_quotes_under_the_floor() {
        assert_eq!(dynamic(0.75).quote(1.0, 0.0), Some(0.75));
    }

    #[test]
    fn no_bid_when_the_floor_is_over_the_cap() {
        assert_eq!(dynamic(1.5).quote(1.0, 0.0), None);
    }

    #[test]
    fn fixed_quotes_its_price_and_respects_the_cap() {
        let p = PricingConfig {
            mode: PricingMode::Fixed,
            min_usd: 0.0,
            fixed_usd: Some(0.3),
        };
        assert_eq!(p.quote(1.0, 1.0), Some(0.3));
        assert_eq!(p.quote(0.2, 0.0), None);
        let floored = PricingConfig { min_usd: 0.5, ..p };
        assert_eq!(floored.quote(1.0, 0.0), Some(0.5));
    }

    #[test]
    fn validate_catches_bad_input() {
        assert!(dynamic(-1.0).validate().is_err());
        assert!(dynamic(f64::NAN).validate().is_err());
        let p = PricingConfig {
            mode: PricingMode::Fixed,
            min_usd: 0.0,
            fixed_usd: None,
        };
        assert!(p.validate().is_err());
        assert!(dynamic(0.02).validate().is_ok());
    }

    #[test]
    fn default_config_parses_from_an_empty_section() {
        let p: PricingConfig = toml::from_str("").unwrap();
        assert_eq!(p, PricingConfig::default());
        assert_eq!(p.mode, PricingMode::Dynamic);
        let p: PricingConfig =
            toml::from_str("mode = \"fixed\"\nmin_usd = 0.1\nfixed_usd = 0.2").unwrap();
        assert_eq!(p.quote(1.0, 0.0), Some(0.2));
    }

    #[test]
    fn listing_is_public_and_small() {
        assert_eq!(
            dynamic(0.02).listing(),
            serde_json::json!({"mode": "dynamic", "min_usd": 0.02})
        );
    }
}
