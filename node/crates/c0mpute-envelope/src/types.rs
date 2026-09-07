//! Value types shared by every c0mpute protocol payload.
//!
//! Each one exists to remove an ambiguity that would otherwise show up as
//! two nodes computing different hashes for the same logical object:
//! money is a decimal string rather than a float, timestamps have exactly
//! one legal spelling, and content hashes carry their algorithm.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

use crate::Error;

// ────────────────────────────────────────────────────────────────────────
// Content hashes
// ────────────────────────────────────────────────────────────────────────

/// A self-describing content hash: `blake3:<64 hex chars>`.
///
/// c0mpute hashes with BLAKE3, matching `c0mpute-proto::Hash` and the
/// existing chunk store. The algorithm prefix is on the wire so a future
/// migration does not require guessing at the length. (The v2 direction
/// PRD writes `sha256:` in its illustrative JSON; the repository already
/// content-addresses chunks with BLAKE3, and splitting the two would mean
/// two hash trees over the same bytes.)
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ContentHash {
    algo: String,
    hex: String,
}

impl ContentHash {
    /// Hash bytes with the default algorithm.
    pub fn of(bytes: &[u8]) -> Self {
        Self {
            algo: "blake3".into(),
            hex: hex::encode(blake3::hash(bytes).as_bytes()),
        }
    }

    pub fn parse(s: &str) -> Result<Self, Error> {
        let (algo, hex_part) = s
            .split_once(':')
            .ok_or_else(|| Error::Format(format!("content hash {s:?} must be `<algo>:<hex>`")))?;
        if algo.is_empty()
            || !algo
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        {
            return Err(Error::Format(format!(
                "content-hash algorithm {algo:?} must be lowercase alphanumeric"
            )));
        }
        if hex_part.is_empty()
            || hex_part.len() % 2 != 0
            || !hex_part
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        {
            return Err(Error::Format(format!(
                "content-hash digest {hex_part:?} must be non-empty lowercase hex"
            )));
        }
        Ok(Self {
            algo: algo.to_string(),
            hex: hex_part.to_string(),
        })
    }

    pub fn algorithm(&self) -> &str {
        &self.algo
    }

    pub fn to_wire(&self) -> String {
        format!("{}:{}", self.algo, self.hex)
    }

    /// The network-native content URI from the v2 direction, e.g.
    /// `c0://blake3:0f2c…`. Resolvable from a local cache, a c0mpute
    /// storage provider, or an HTTP gateway — the URI itself names no host,
    /// which is what makes it survive any one of them disappearing.
    pub fn to_c0_uri(&self) -> String {
        format!("c0://{}", self.to_wire())
    }

    /// Parse either the bare `blake3:…` form or a `c0://blake3:…` URI.
    pub fn parse_uri(s: &str) -> Result<Self, Error> {
        Self::parse(s.strip_prefix("c0://").unwrap_or(s))
    }
}

impl std::fmt::Display for ContentHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.algo, self.hex)
    }
}

impl std::fmt::Debug for ContentHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ContentHash({self})")
    }
}

impl Serialize for ContentHash {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_wire())
    }
}

impl<'de> Deserialize<'de> for ContentHash {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        ContentHash::parse(&s).map_err(serde::de::Error::custom)
    }
}

// ────────────────────────────────────────────────────────────────────────
// Money
// ────────────────────────────────────────────────────────────────────────

/// A price. The amount is a **decimal string**, never a float.
///
/// `0.1 + 0.2 != 0.3` in binary floating point, and JSON encoders disagree
/// about how to render a double. Either one is enough to make a buyer and a
/// provider hash the same offer differently. A string sidesteps both, and
/// [`crate::canonical`] refuses to sign a float anywhere in a payload.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Money {
    /// Plain decimal, e.g. `"0.042"`. No exponent, no thousands separator,
    /// no sign.
    pub amount: String,
    /// ISO-4217 code for fiat (`"USD"`) or the asset symbol for crypto
    /// (`"USDC"`). Compared case-sensitively; use upper case.
    pub currency: String,
}

impl Money {
    pub fn new(amount: impl Into<String>, currency: impl Into<String>) -> Self {
        Self {
            amount: amount.into(),
            currency: currency.into(),
        }
    }

    /// Check that the amount is a well-formed non-negative decimal and the
    /// currency looks like a symbol.
    pub fn validate(&self) -> Result<(), Error> {
        parse_decimal(&self.amount)?;
        if self.currency.is_empty()
            || !self
                .currency
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        {
            return Err(Error::Format(format!(
                "currency {:?} must be an uppercase alphanumeric symbol",
                self.currency
            )));
        }
        Ok(())
    }

    /// Compare two amounts. Errors if the currencies differ — the protocol
    /// deliberately has no exchange-rate opinion, so a cross-currency
    /// comparison is a caller bug, not a silent `false`.
    pub fn cmp_amount(&self, other: &Money) -> Result<Ordering, Error> {
        if self.currency != other.currency {
            return Err(Error::Format(format!(
                "cannot compare {} with {}: no exchange rate at the protocol layer",
                self.currency, other.currency
            )));
        }
        let (a_int, a_frac) = parse_decimal(&self.amount)?;
        let (b_int, b_frac) = parse_decimal(&other.amount)?;
        Ok(a_int.cmp(&b_int).then_with(|| {
            let width = a_frac.len().max(b_frac.len());
            let pad = |s: &str| -> u128 {
                let padded = format!("{s:0<width$}");
                padded.parse().unwrap_or(0)
            };
            pad(&a_frac).cmp(&pad(&b_frac))
        }))
    }

    /// True when this amount is within (less than or equal to) `cap`.
    pub fn is_within(&self, cap: &Money) -> Result<bool, Error> {
        Ok(self.cmp_amount(cap)? != Ordering::Greater)
    }
}

/// Split a decimal string into (integer part, fractional digits).
fn parse_decimal(s: &str) -> Result<(u128, String), Error> {
    let bad = |why: &str| Error::Format(format!("amount {s:?} {why}"));
    if s.is_empty() {
        return Err(bad("is empty"));
    }
    let (int_part, frac_part) = match s.split_once('.') {
        Some((i, f)) => (i, f),
        None => (s, ""),
    };
    if int_part.is_empty() || !int_part.chars().all(|c| c.is_ascii_digit()) {
        return Err(bad("must have a digits-only integer part"));
    }
    if int_part.len() > 1 && int_part.starts_with('0') {
        return Err(bad("must not have leading zeros"));
    }
    if s.contains('.') && (frac_part.is_empty() || !frac_part.chars().all(|c| c.is_ascii_digit())) {
        return Err(bad("must have digits after the decimal point"));
    }
    if frac_part.len() > 30 {
        return Err(bad("has more than 30 fractional digits"));
    }
    let int_value: u128 = int_part
        .parse()
        .map_err(|_| bad("has too large an integer part"))?;
    Ok((int_value, frac_part.to_string()))
}

// ────────────────────────────────────────────────────────────────────────
// Timestamps
// ────────────────────────────────────────────────────────────────────────

/// An instant, spelled exactly one way: `YYYY-MM-DDTHH:MM:SS.sssZ`.
///
/// Always UTC, always millisecond precision. That is precisely what
/// JavaScript's `Date.prototype.toISOString()` emits, so an agent or
/// browser client produces canonical output without a library. Any other
/// spelling of the same instant — a `+00:00` offset, a dropped
/// milliseconds field, a lowercase `z` — is rejected rather than
/// normalized, because normalizing after signing would change the bytes
/// the signature covers.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp {
    text: String,
    unix_ms: i64,
}

impl Timestamp {
    /// Build from milliseconds since the Unix epoch.
    pub fn from_unix_ms(unix_ms: i64) -> Result<Self, Error> {
        let (days, ms_of_day) = (
            unix_ms.div_euclid(86_400_000),
            unix_ms.rem_euclid(86_400_000),
        );
        let (y, m, d) = civil_from_days(days);
        let (ms, rest) = (ms_of_day % 1000, ms_of_day / 1000);
        let (s, rest) = (rest % 60, rest / 60);
        let (min, h) = (rest % 60, rest / 60);
        Ok(Self {
            text: format!("{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}.{ms:03}Z"),
            unix_ms,
        })
    }

    /// Parse the one legal spelling.
    pub fn parse(s: &str) -> Result<Self, Error> {
        let bad = |why: &str| {
            Error::Format(format!(
                "timestamp {s:?} {why}; the only legal form is YYYY-MM-DDTHH:MM:SS.sssZ (UTC)"
            ))
        };
        let b = s.as_bytes();
        if b.len() != 24 {
            return Err(bad("has the wrong length"));
        }
        if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
            return Err(bad("has misplaced separators"));
        }
        if b[19] != b'.' || b[23] != b'Z' {
            return Err(bad("must end with .sssZ"));
        }
        let num = |range: std::ops::Range<usize>| -> Result<i64, Error> {
            let part = &s[range];
            if !part.chars().all(|c| c.is_ascii_digit()) {
                return Err(bad("has a non-digit where a number belongs"));
            }
            part.parse().map_err(|_| bad("has an unparseable number"))
        };
        let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
        let (h, mi, sec, ms) = (num(11..13)?, num(14..16)?, num(17..19)?, num(20..23)?);

        if !(1..=12).contains(&mo) {
            return Err(bad("has a month outside 1-12"));
        }
        if d < 1 || d > days_in_month(y, mo) {
            return Err(bad("has a day that does not exist in that month"));
        }
        if h > 23 || mi > 59 || sec > 59 {
            return Err(bad("has an out-of-range time field"));
        }

        let unix_ms =
            days_from_civil(y, mo, d) * 86_400_000 + h * 3_600_000 + mi * 60_000 + sec * 1_000 + ms;
        Ok(Self {
            text: s.to_string(),
            unix_ms,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn unix_ms(&self) -> i64 {
        self.unix_ms
    }

    /// True when this instant is at or before `now`.
    pub fn is_expired_at(&self, now: &Timestamp) -> bool {
        self.unix_ms <= now.unix_ms
    }
}

impl std::fmt::Display for Timestamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

impl std::fmt::Debug for Timestamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Timestamp({})", self.text)
    }
}

impl Serialize for Timestamp {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.text)
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Timestamp::parse(&s).map_err(serde::de::Error::custom)
    }
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(y) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days from 1970-01-01 to the given civil date (Howard Hinnant's
/// `days_from_civil`, which is exact for the proleptic Gregorian calendar).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`].
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ────────────────────────────────────────────────────────────────────────
// Trust, settlement, validation
// ────────────────────────────────────────────────────────────────────────

/// Buyer-selectable provider trust requirement (v2 direction §13).
///
/// A scheduling constraint, not a network-wide identity policy: the
/// network never refuses a `Community` provider, individual buyers do.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustTier {
    /// Cryptographic identity only. No KYC, public reputation, low-value work.
    /// The default: a fresh key with no history is exactly this.
    #[default]
    Community,
    /// Established receipt history and a minimum success rate.
    Standard,
    /// Provider credential, plus hardware verification where available.
    Verified,
    /// Allowlisted provider set, encrypted inputs, restricted telemetry.
    Private,
    /// Organization verification, SLA, jurisdiction controls, audited pool.
    Enterprise,
}

impl TrustTier {
    /// Whether a provider at `self` satisfies a job asking for `required`.
    ///
    /// Ordering is by strength, so a Verified provider can take Standard
    /// work. `Private` and `Enterprise` are *not* reachable by merely being
    /// strong — they mean "on this buyer's list", so they only satisfy
    /// themselves.
    pub fn satisfies(self, required: TrustTier) -> bool {
        match required {
            TrustTier::Private | TrustTier::Enterprise => self == required,
            _ => self >= required,
        }
    }
}

/// Which settlement implementation a job uses.
///
/// An open string rather than a closed enum: the protocol must not make
/// adoption contingent on one payment product. CoinPay is the default and
/// best-integrated adapter, not a requirement (v2 direction §8.3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SettlementAdapter(pub String);

impl SettlementAdapter {
    /// First-party: CoinPay DID escrow and multi-asset settlement.
    pub const COINPAY: &'static str = "coinpay";
    /// HTTP 402 pay-per-request.
    pub const X402: &'static str = "x402";
    /// Bitcoin Lightning.
    pub const LIGHTNING: &'static str = "lightning";
    /// Out-of-band invoicing; the receipt records terms, not a transfer.
    pub const INVOICE: &'static str = "invoice";
    /// Prepaid credits held by a managed gateway the buyer selected.
    pub const GATEWAY_CREDITS: &'static str = "gateway-credits";
    /// Internal accounting inside a private network (§24).
    pub const PRIVATE: &'static str = "private";

    pub fn coinpay() -> Self {
        Self(Self::COINPAY.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn validate(&self) -> Result<(), Error> {
        if self.0.is_empty()
            || !self
                .0
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(Error::Format(format!(
                "settlement adapter {:?} must be lowercase kebab-case",
                self.0
            )));
        }
        Ok(())
    }
}

/// How hard a result gets checked before it is accepted and paid for.
///
/// The levels map to v2 direction §12. They are a buyer's economic choice:
/// duplicate execution costs roughly double, so a cheap image generation
/// and a high-value deterministic transform should not pay the same
/// verification tax.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ValidationLevel {
    /// L0 — the requester accepts whatever comes back.
    Requester,
    /// L1 — deterministic or schema validation the buyer can run alone.
    Schema,
    /// L2 — a sampled fraction runs on a second, independent provider.
    Spotcheck,
    /// L3 — run on N providers and compare.
    Quorum,
    /// L4 — hardware or runtime attestation.
    Attested,
    /// L5 — a workload-specific proof scheme.
    Proof,
}

/// A job's full validation policy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationPolicy {
    pub level: ValidationLevel,
    /// How many independent providers execute the job. `1` is the normal
    /// case; quorum validation needs at least 3 to break a tie.
    #[serde(default = "one")]
    pub redundancy: u32,
    /// For `Spotcheck`: the percentage of jobs duplicated, 0-100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spotcheck_percent: Option<u32>,
}

fn one() -> u32 {
    1
}

impl Default for ValidationPolicy {
    fn default() -> Self {
        Self {
            level: ValidationLevel::Schema,
            redundancy: 1,
            spotcheck_percent: None,
        }
    }
}

impl ValidationPolicy {
    pub fn validate(&self) -> Result<(), Error> {
        if self.redundancy == 0 {
            return Err(Error::Format(
                "validation redundancy must be at least 1".into(),
            ));
        }
        if self.level == ValidationLevel::Quorum && self.redundancy < 3 {
            return Err(Error::Format(
                "quorum validation needs redundancy of at least 3 to resolve a disagreement".into(),
            ));
        }
        if let Some(pct) = self.spotcheck_percent {
            if pct > 100 {
                return Err(Error::Format(format!(
                    "spotcheck percent {pct} must be 0-100"
                )));
            }
            if self.level != ValidationLevel::Spotcheck {
                return Err(Error::Format(
                    "spotcheck_percent only applies to the spotcheck validation level".into(),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── content hashes ──────────────────────────────────────────────────

    #[test]
    fn content_hash_round_trips() {
        let h = ContentHash::of(b"hello c0mpute");
        let parsed = ContentHash::parse(&h.to_wire()).unwrap();
        assert_eq!(h, parsed);
        assert_eq!(h.algorithm(), "blake3");
    }

    #[test]
    fn content_hash_is_stable_for_the_same_bytes() {
        assert_eq!(ContentHash::of(b"abc"), ContentHash::of(b"abc"));
        assert_ne!(ContentHash::of(b"abc"), ContentHash::of(b"abd"));
    }

    #[test]
    fn c0_uri_round_trips() {
        let h = ContentHash::of(b"payload");
        let uri = h.to_c0_uri();
        assert!(uri.starts_with("c0://blake3:"));
        assert_eq!(ContentHash::parse_uri(&uri).unwrap(), h);
        assert_eq!(ContentHash::parse_uri(&h.to_wire()).unwrap(), h);
    }

    #[test]
    fn content_hash_rejects_malformed_input() {
        for bad in [
            "deadbeef",
            "blake3:",
            "blake3:XYZ",
            "blake3:abc",
            ":abc",
            "BLAKE3:ab",
        ] {
            assert!(ContentHash::parse(bad).is_err(), "{bad} should be rejected");
        }
    }

    // ── money ───────────────────────────────────────────────────────────

    #[test]
    fn money_validates_plain_decimals() {
        for ok in ["0", "0.042", "1", "12.5", "1000000.000001"] {
            Money::new(ok, "USD").validate().unwrap();
        }
    }

    #[test]
    fn money_rejects_floats_dressed_as_strings() {
        for bad in [
            "", "1e5", "-1", "+1", "1.", ".5", "01", "1.2.3", "1,5", "abc",
        ] {
            assert!(
                Money::new(bad, "USD").validate().is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn money_rejects_a_malformed_currency() {
        assert!(Money::new("1", "usd").validate().is_err());
        assert!(Money::new("1", "").validate().is_err());
    }

    #[test]
    fn money_compares_by_value_not_string_length() {
        let a = Money::new("0.10", "USD");
        let b = Money::new("0.1", "USD");
        assert_eq!(a.cmp_amount(&b).unwrap(), Ordering::Equal);

        let cheap = Money::new("0.042", "USD");
        let cap = Money::new("0.10", "USD");
        // Lexically "0.042" > "0.10"; numerically it is not.
        assert_eq!(cheap.cmp_amount(&cap).unwrap(), Ordering::Less);
        assert!(cheap.is_within(&cap).unwrap());
        assert!(!cap.is_within(&cheap).unwrap());
    }

    #[test]
    fn money_compares_large_integer_parts() {
        let a = Money::new("9", "USD");
        let b = Money::new("10", "USD");
        assert_eq!(a.cmp_amount(&b).unwrap(), Ordering::Less);
    }

    #[test]
    fn money_refuses_cross_currency_comparison() {
        let usd = Money::new("1", "USD");
        let usdc = Money::new("1", "USDC");
        assert!(usd.cmp_amount(&usdc).is_err());
    }

    // ── timestamps ──────────────────────────────────────────────────────

    #[test]
    fn timestamp_round_trips_through_unix_ms() {
        let t = Timestamp::parse("2026-09-06T16:02:00.000Z").unwrap();
        let again = Timestamp::from_unix_ms(t.unix_ms()).unwrap();
        assert_eq!(t.as_str(), again.as_str());
    }

    #[test]
    fn timestamp_epoch_is_zero() {
        assert_eq!(
            Timestamp::parse("1970-01-01T00:00:00.000Z")
                .unwrap()
                .unix_ms(),
            0
        );
        assert_eq!(
            Timestamp::from_unix_ms(0).unwrap().as_str(),
            "1970-01-01T00:00:00.000Z"
        );
    }

    #[test]
    fn timestamp_handles_leap_days() {
        let leap = Timestamp::parse("2024-02-29T12:00:00.000Z").unwrap();
        assert_eq!(
            Timestamp::from_unix_ms(leap.unix_ms()).unwrap().as_str(),
            "2024-02-29T12:00:00.000Z"
        );
        // 2100 is not a leap year: century rule.
        assert!(Timestamp::parse("2100-02-29T00:00:00.000Z").is_err());
        assert!(Timestamp::parse("2000-02-29T00:00:00.000Z").is_ok());
    }

    #[test]
    fn timestamp_rejects_every_other_spelling_of_the_same_instant() {
        for bad in [
            "2026-09-06T16:02:00Z",        // no milliseconds
            "2026-09-06T16:02:00+00:00",   // offset instead of Z
            "2026-09-06T16:02:00.000z",    // lowercase z
            "2026-09-06 16:02:00.000Z",    // space separator
            "2026-09-06T16:02:00.000000Z", // microseconds
            "2026-13-06T16:02:00.000Z",    // month 13
            "2026-09-31T16:02:00.000Z",    // September has 30 days
            "2026-09-06T24:00:00.000Z",    // hour 24
            "2026-09-06T16:60:00.000Z",    // minute 60
        ] {
            assert!(Timestamp::parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn timestamp_orders_chronologically() {
        let early = Timestamp::parse("2026-09-06T16:02:00.000Z").unwrap();
        let late = Timestamp::parse("2026-09-06T16:02:00.001Z").unwrap();
        assert!(early < late);
        assert!(early.is_expired_at(&late));
        assert!(!late.is_expired_at(&early));
        assert!(early.is_expired_at(&early), "expiry is inclusive");
    }

    // ── trust ───────────────────────────────────────────────────────────

    #[test]
    fn stronger_tiers_satisfy_weaker_requirements() {
        assert!(TrustTier::Verified.satisfies(TrustTier::Standard));
        assert!(TrustTier::Standard.satisfies(TrustTier::Community));
        assert!(!TrustTier::Community.satisfies(TrustTier::Standard));
    }

    #[test]
    fn allowlist_tiers_only_satisfy_themselves() {
        // Being "stronger" cannot buy your way onto an allowlist.
        assert!(!TrustTier::Enterprise.satisfies(TrustTier::Private));
        assert!(!TrustTier::Verified.satisfies(TrustTier::Enterprise));
        assert!(TrustTier::Private.satisfies(TrustTier::Private));
        assert!(TrustTier::Enterprise.satisfies(TrustTier::Enterprise));
        // But they still clear the ordinary tiers.
        assert!(TrustTier::Enterprise.satisfies(TrustTier::Standard));
    }

    #[test]
    fn trust_tier_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&TrustTier::Standard).unwrap(),
            "\"standard\""
        );
    }

    // ── settlement + validation ─────────────────────────────────────────

    #[test]
    fn settlement_adapter_is_an_open_string() {
        SettlementAdapter::coinpay().validate().unwrap();
        SettlementAdapter("some-future-rail".into())
            .validate()
            .unwrap();
        assert!(SettlementAdapter("CoinPay".into()).validate().is_err());
        assert!(SettlementAdapter(String::new()).validate().is_err());
    }

    #[test]
    fn settlement_adapter_serializes_as_a_bare_string() {
        assert_eq!(
            serde_json::to_string(&SettlementAdapter::coinpay()).unwrap(),
            "\"coinpay\""
        );
    }

    #[test]
    fn quorum_needs_enough_providers_to_break_a_tie() {
        let mut p = ValidationPolicy {
            level: ValidationLevel::Quorum,
            redundancy: 2,
            spotcheck_percent: None,
        };
        assert!(p.validate().is_err());
        p.redundancy = 3;
        p.validate().unwrap();
    }

    #[test]
    fn validation_policy_rejects_nonsense() {
        let zero = ValidationPolicy {
            level: ValidationLevel::Schema,
            redundancy: 0,
            spotcheck_percent: None,
        };
        assert!(zero.validate().is_err());

        let misplaced = ValidationPolicy {
            level: ValidationLevel::Schema,
            redundancy: 1,
            spotcheck_percent: Some(10),
        };
        assert!(misplaced.validate().is_err());

        let too_much = ValidationPolicy {
            level: ValidationLevel::Spotcheck,
            redundancy: 1,
            spotcheck_percent: Some(101),
        };
        assert!(too_much.validate().is_err());
    }
}
