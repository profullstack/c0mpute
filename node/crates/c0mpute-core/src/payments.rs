//! Payments: the one small interface every c0mpute feature that moves money
//! goes through (hosting settlement today; ads billing is meant to use the
//! same [`Rail`]).
//!
//! * Amounts are integer micro-USD ([`Micros`]), never floats.
//! * A [`Transfer`] carries an idempotency key; the [`Ledger`] records every
//!   key that was paid, so a retried settlement round never pays twice.
//! * Rails: [`FakeRail`] (in memory, for tests) and [`CoinPayRail`] (CoinPay
//!   `POST /api/payouts/create` with a business API key holding the
//!   `payouts:create` scope).
//!
//! Nothing here moves money unless the operator turns it on: [`PaymentsConfig`]
//! defaults to `enabled = false`, and [`rail_from_config`] returns `None`
//! until `enabled = true` **and** the CoinPay key is in the environment.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Micro-USD (1 USD = 1_000_000).
pub type Micros = u64;

pub const MICROS_PER_USD: u64 = 1_000_000;

/// Parse a decimal USD amount ("0.004", "12", "1.5") into micro-USD. At most
/// six decimals; no sign, no exponent.
pub fn parse_usd(s: &str) -> Result<Micros> {
    let s = s.trim();
    if s.is_empty() || s.len() > 24 {
        bail!("not a USD amount: {s:?}");
    }
    let (int, frac) = match s.split_once('.') {
        Some((i, f)) => (i, f),
        None => (s, ""),
    };
    if int.is_empty() && frac.is_empty() {
        bail!("not a USD amount: {s:?}");
    }
    if !int.chars().all(|c| c.is_ascii_digit()) || !frac.chars().all(|c| c.is_ascii_digit()) {
        bail!("not a USD amount: {s:?}");
    }
    if frac.len() > 6 {
        bail!("USD amounts have at most 6 decimals: {s:?}");
    }
    let whole: u64 = if int.is_empty() { 0 } else { int.parse().context("USD amount too large")? };
    let mut f = frac.to_string();
    while f.len() < 6 {
        f.push('0');
    }
    let frac_micros: u64 = f.parse().unwrap_or(0);
    whole
        .checked_mul(MICROS_PER_USD)
        .and_then(|w| w.checked_add(frac_micros))
        .context("USD amount too large")
}

/// Micro-USD as a canonical decimal string ("0.004", "12", "1.5").
pub fn format_usd(m: Micros) -> String {
    let whole = m / MICROS_PER_USD;
    let frac = m % MICROS_PER_USD;
    if frac == 0 {
        return whole.to_string();
    }
    let f = format!("{frac:06}");
    format!("{whole}.{}", f.trim_end_matches('0'))
}

/// Where a payee wants to be paid.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Payee {
    /// Wallet address on `coin`'s chain.
    pub wallet: String,
    /// CoinPay currency code, e.g. `USDC_POL`.
    #[serde(default = "default_coin")]
    pub coin: String,
}

pub fn default_coin() -> String {
    "USDC_POL".to_string()
}

impl Payee {
    /// Basic shape check; the rail validates the address for real.
    pub fn valid(&self) -> bool {
        let w = self.wallet.trim();
        (8..=128).contains(&w.len())
            && w.chars().all(|c| c.is_ascii_alphanumeric() || c == ':' || c == '_' || c == '-')
            && (2..=16).contains(&self.coin.len())
            && self.coin.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    }
}

/// One payment to make.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transfer {
    /// Same key, same payment: rails and the ledger refuse to pay a key twice.
    pub key: String,
    /// Who pays (hex ed25519 key of the payer identity; informational for
    /// the rail, which pays from the account its credentials belong to).
    pub payer: String,
    pub payee: Payee,
    pub amount: Micros,
    /// What it is for (goes into the rail's metadata).
    pub memo: String,
}

/// What a rail answered for a payment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paid {
    pub key: String,
    /// The rail's own id for the payment.
    pub rail_ref: String,
}

/// A way to move money. Keep it small: ads billing and hosting settlement
/// both need only these two calls.
#[async_trait::async_trait]
pub trait Rail: Send + Sync {
    fn name(&self) -> &str;
    /// The payer's spendable balance, when the rail can tell. `None` means
    /// unknown (callers then rely on their own spending cap).
    async fn balance(&self, payer: &str) -> Result<Option<Micros>>;
    /// Pay one transfer. Must refuse a key it already paid.
    async fn pay(&self, t: &Transfer) -> Result<Paid>;
}

// ── fake rail ───────────────────────────────────────────────────────────

/// In-memory rail for tests: prepaid balances per payer, a list of payments,
/// and the same idempotency rule a real rail has.
#[derive(Default)]
pub struct FakeRail {
    inner: Mutex<FakeState>,
}

#[derive(Default)]
struct FakeState {
    balances: HashMap<String, Micros>,
    paid: Vec<Transfer>,
    keys: HashSet<String>,
}

impl FakeRail {
    pub fn new() -> Self {
        Self::default()
    }

    /// Prepay `amount` for `payer`.
    pub fn fund(&self, payer: &str, amount: Micros) {
        let mut s = self.inner.lock().unwrap();
        *s.balances.entry(payer.to_string()).or_default() += amount;
    }

    pub fn payments(&self) -> Vec<Transfer> {
        self.inner.lock().unwrap().paid.clone()
    }

    /// Total received by a wallet.
    pub fn received(&self, wallet: &str) -> Micros {
        self.inner
            .lock()
            .unwrap()
            .paid
            .iter()
            .filter(|t| t.payee.wallet == wallet)
            .map(|t| t.amount)
            .sum()
    }
}

#[async_trait::async_trait]
impl Rail for FakeRail {
    fn name(&self) -> &str {
        "fake"
    }

    async fn balance(&self, payer: &str) -> Result<Option<Micros>> {
        Ok(Some(*self.inner.lock().unwrap().balances.get(payer).unwrap_or(&0)))
    }

    async fn pay(&self, t: &Transfer) -> Result<Paid> {
        let mut s = self.inner.lock().unwrap();
        if s.keys.contains(&t.key) {
            bail!("transfer {} already paid", t.key);
        }
        let bal = s.balances.get(&t.payer).copied().unwrap_or(0);
        if bal < t.amount {
            bail!("insufficient balance: {} < {}", format_usd(bal), format_usd(t.amount));
        }
        s.balances.insert(t.payer.clone(), bal - t.amount);
        s.keys.insert(t.key.clone());
        s.paid.push(t.clone());
        Ok(Paid {
            key: t.key.clone(),
            rail_ref: format!("fake-{}", s.paid.len()),
        })
    }
}

// ── CoinPay rail ────────────────────────────────────────────────────────

/// CoinPay payouts from the payer's own CoinPay business wallet.
pub struct CoinPayRail {
    base: String,
    api_key: String,
    recipient_email: String,
    client: reqwest::Client,
}

impl CoinPayRail {
    pub fn new(base: &str, api_key: &str, recipient_email: &str) -> Result<Self> {
        if api_key.trim().is_empty() {
            bail!("CoinPay API key is empty");
        }
        if !recipient_email.contains('@') {
            bail!("payments.coinpay.recipient_email must be an email address");
        }
        Ok(Self {
            base: base.trim_end_matches('/').to_string(),
            api_key: api_key.trim().to_string(),
            recipient_email: recipient_email.to_string(),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .user_agent(concat!("c0mpute/", env!("CARGO_PKG_VERSION")))
                .build()?,
        })
    }

    /// The JSON body for `POST /api/payouts/create`.
    pub fn payout_body(&self, t: &Transfer) -> serde_json::Value {
        serde_json::json!({
            "recipient_email": self.recipient_email,
            "recipient_wallet": t.payee.wallet,
            "cryptocurrency": t.payee.coin,
            "amount_usd": format_usd(t.amount),
            "metadata": {
                "source": "c0mpute",
                "idempotency_key": t.key,
                "payer": t.payer,
                "memo": t.memo,
            },
        })
    }
}

#[async_trait::async_trait]
impl Rail for CoinPayRail {
    fn name(&self) -> &str {
        "coinpay"
    }

    async fn balance(&self, _payer: &str) -> Result<Option<Micros>> {
        // CoinPay has no balance call for a business API key; callers cap
        // spending with `payments.max_usd_per_round`.
        Ok(None)
    }

    async fn pay(&self, t: &Transfer) -> Result<Paid> {
        let res = self
            .client
            .post(format!("{}/api/payouts/create", self.base))
            .bearer_auth(&self.api_key)
            .json(&self.payout_body(t))
            .send()
            .await
            .context("CoinPay payout request")?;
        let status = res.status();
        let body: serde_json::Value = res.json().await.unwrap_or(serde_json::Value::Null);
        if !status.is_success() || body["success"] != serde_json::Value::Bool(true) {
            bail!("CoinPay payout answered {status}: {}", body["error"]);
        }
        Ok(Paid {
            key: t.key.clone(),
            rail_ref: body["payout"]["id"].as_str().unwrap_or("").to_string(),
        })
    }
}

// ── config ──────────────────────────────────────────────────────────────

/// `[payments]` in config.toml. Off by default: no money moves until an
/// operator sets `enabled = true` and puts the CoinPay key in the
/// environment variable `coinpay.api_key_env` names.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaymentsConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Only `coinpay` is a real rail.
    #[serde(default = "default_rail")]
    pub rail: String,
    /// Most this node pays out in one settlement round, USD (decimal string).
    #[serde(default = "default_round_cap")]
    pub max_usd_per_round: String,
    #[serde(default)]
    pub coinpay: CoinPayConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CoinPayConfig {
    #[serde(default = "default_coinpay_url")]
    pub url: String,
    /// The environment variable holding a CoinPay business API key with the
    /// `payouts:create` scope. The key itself never goes in config.toml.
    #[serde(default = "default_key_env")]
    pub api_key_env: String,
    /// CoinPay requires a recipient email on every payout; this is the
    /// address its payout records are filed under (yours).
    #[serde(default)]
    pub recipient_email: String,
}

fn default_rail() -> String {
    "coinpay".into()
}
fn default_round_cap() -> String {
    "10".into()
}
fn default_coinpay_url() -> String {
    "https://coinpayportal.com".into()
}
fn default_key_env() -> String {
    "C0MPUTE_COINPAY_PAYOUT_KEY".into()
}

impl Default for CoinPayConfig {
    fn default() -> Self {
        Self {
            url: default_coinpay_url(),
            api_key_env: default_key_env(),
            recipient_email: String::new(),
        }
    }
}

impl Default for PaymentsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            rail: default_rail(),
            max_usd_per_round: default_round_cap(),
            coinpay: CoinPayConfig::default(),
        }
    }
}

impl PaymentsConfig {
    pub fn round_cap(&self) -> Micros {
        parse_usd(&self.max_usd_per_round).unwrap_or(10 * MICROS_PER_USD)
    }
}

/// The rail this node pays with, or `None` when payments are off. Errors only
/// when payments are on but misconfigured, so an operator sees why.
pub fn rail_from_config(cfg: &PaymentsConfig) -> Result<Option<std::sync::Arc<dyn Rail>>> {
    if !cfg.enabled {
        return Ok(None);
    }
    match cfg.rail.as_str() {
        "coinpay" => {
            let key = std::env::var(&cfg.coinpay.api_key_env).unwrap_or_default();
            if key.trim().is_empty() {
                bail!(
                    "payments.enabled = true but ${} is not set (a CoinPay business API key with payouts:create)",
                    cfg.coinpay.api_key_env
                );
            }
            let rail = CoinPayRail::new(&cfg.coinpay.url, &key, &cfg.coinpay.recipient_email)?;
            Ok(Some(std::sync::Arc::new(rail)))
        }
        other => bail!("unknown payments.rail {other:?} (only \"coinpay\")"),
    }
}

// ── ledger ──────────────────────────────────────────────────────────────

/// Every transfer key this node has paid, with the rail's reference. Written
/// to disk after each payment so a crash between rounds never pays twice.
#[derive(Default)]
pub struct Ledger {
    path: Option<PathBuf>,
    paid: Mutex<HashMap<String, LedgerEntry>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub amount: Micros,
    pub payee: String,
    pub rail_ref: String,
    pub at: u64,
}

impl Ledger {
    pub fn in_memory() -> Self {
        Self::default()
    }

    pub fn open(path: PathBuf) -> Self {
        let paid = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self {
            path: Some(path),
            paid: Mutex::new(paid),
        }
    }

    pub fn contains(&self, key: &str) -> bool {
        self.paid.lock().unwrap().contains_key(key)
    }

    pub fn entries(&self) -> HashMap<String, LedgerEntry> {
        self.paid.lock().unwrap().clone()
    }

    fn record(&self, t: &Transfer, paid: &Paid) {
        let snapshot = {
            let mut m = self.paid.lock().unwrap();
            m.insert(
                t.key.clone(),
                LedgerEntry {
                    amount: t.amount,
                    payee: t.payee.wallet.clone(),
                    rail_ref: paid.rail_ref.clone(),
                    at: now_secs(),
                },
            );
            serde_json::to_vec_pretty(&*m).unwrap_or_default()
        };
        if let Some(p) = &self.path {
            if let Some(d) = p.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            let tmp = p.with_extension("tmp");
            if std::fs::write(&tmp, &snapshot).is_ok() {
                let _ = std::fs::rename(&tmp, p);
            }
        }
    }
}

/// What happened to one transfer in [`pay_all`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Paid(Paid),
    AlreadyPaid,
    /// Skipped: over the round cap or the payer's balance.
    OverBudget,
    Failed(String),
}

/// Pay `transfers` in order through `rail`, skipping keys the ledger already
/// has, and stopping at `cap` (and at the payer's balance when the rail
/// knows it).
pub async fn pay_all(rail: &dyn Rail, ledger: &Ledger, transfers: &[Transfer], cap: Micros) -> Vec<(Transfer, Outcome)> {
    let mut out = Vec::new();
    let mut spent: Micros = 0;
    let mut balances: HashMap<String, Option<Micros>> = HashMap::new();
    for t in transfers {
        if ledger.contains(&t.key) {
            out.push((t.clone(), Outcome::AlreadyPaid));
            continue;
        }
        if t.amount == 0 {
            continue;
        }
        if spent.saturating_add(t.amount) > cap {
            out.push((t.clone(), Outcome::OverBudget));
            continue;
        }
        if !balances.contains_key(&t.payer) {
            let b = rail.balance(&t.payer).await.unwrap_or(None);
            balances.insert(t.payer.clone(), b);
        }
        if let Some(Some(b)) = balances.get(&t.payer)
            && *b < t.amount
        {
            out.push((t.clone(), Outcome::OverBudget));
            continue;
        }
        match rail.pay(t).await {
            Ok(p) => {
                ledger.record(t, &p);
                spent += t.amount;
                if let Some(Some(b)) = balances.get_mut(&t.payer) {
                    *b -= t.amount;
                }
                out.push((t.clone(), Outcome::Paid(p)));
            }
            Err(e) => out.push((t.clone(), Outcome::Failed(e.to_string()))),
        }
    }
    out
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payee(w: &str) -> Payee {
        Payee {
            wallet: w.into(),
            coin: default_coin(),
        }
    }

    fn t(key: &str, payer: &str, w: &str, amount: Micros) -> Transfer {
        Transfer {
            key: key.into(),
            payer: payer.into(),
            payee: payee(w),
            amount,
            memo: "test".into(),
        }
    }

    #[test]
    fn usd_round_trips() {
        assert_eq!(parse_usd("0.004").unwrap(), 4_000);
        assert_eq!(parse_usd("12").unwrap(), 12_000_000);
        assert_eq!(parse_usd("1.5").unwrap(), 1_500_000);
        assert_eq!(parse_usd(".25").unwrap(), 250_000);
        assert!(parse_usd("-1").is_err());
        assert!(parse_usd("1e3").is_err());
        assert!(parse_usd("0.0000001").is_err());
        assert!(parse_usd("").is_err());
        assert!(parse_usd(".").is_err());
        assert_eq!(format_usd(4_000), "0.004");
        assert_eq!(format_usd(12_000_000), "12");
        assert_eq!(format_usd(1_500_000), "1.5");
        for m in [0, 1, 999_999, 1_000_001, 123_456_789] {
            assert_eq!(parse_usd(&format_usd(m)).unwrap(), m);
        }
    }

    #[test]
    fn payments_are_off_by_default() {
        let cfg = PaymentsConfig::default();
        assert!(!cfg.enabled);
        assert!(rail_from_config(&cfg).unwrap().is_none());
        let on = PaymentsConfig {
            enabled: true,
            coinpay: CoinPayConfig {
                api_key_env: "C0MPUTE_TEST_KEY_THAT_IS_NEVER_SET".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(rail_from_config(&on).is_err(), "on without a key is an error, not a silent rail");
    }

    #[test]
    fn coinpay_body_carries_the_idempotency_key() {
        let rail = CoinPayRail::new("https://coinpay.invalid/", "k", "ops@example.org").unwrap();
        let b = rail.payout_body(&t("k1", "aa", "0xabcdef0123456789", 1_234_567));
        assert_eq!(b["amount_usd"], "1.234567");
        assert_eq!(b["recipient_wallet"], "0xabcdef0123456789");
        assert_eq!(b["cryptocurrency"], "USDC_POL");
        assert_eq!(b["metadata"]["idempotency_key"], "k1");
    }

    #[tokio::test]
    async fn fake_rail_pays_once_within_balance_and_cap() {
        let rail = FakeRail::new();
        rail.fund("payer", 1_000);
        let ledger = Ledger::in_memory();
        let ts = vec![
            t("a", "payer", "wallet-aaaaaaaa", 400),
            t("b", "payer", "wallet-bbbbbbbb", 400),
            t("c", "payer", "wallet-cccccccc", 400),
        ];
        let out = pay_all(&rail, &ledger, &ts, 10_000).await;
        assert!(matches!(out[0].1, Outcome::Paid(_)));
        assert!(matches!(out[1].1, Outcome::Paid(_)));
        assert_eq!(out[2].1, Outcome::OverBudget, "balance runs out");
        // Same round again: nothing pays twice.
        rail.fund("payer", 1_000);
        let again = pay_all(&rail, &ledger, &ts, 10_000).await;
        assert_eq!(again[0].1, Outcome::AlreadyPaid);
        assert_eq!(again[1].1, Outcome::AlreadyPaid);
        assert!(matches!(again[2].1, Outcome::Paid(_)));
        assert_eq!(rail.received("wallet-aaaaaaaa"), 400);
        // The cap stops a round.
        let capped = pay_all(&rail, &ledger, &[t("d", "payer", "wallet-dddddddd", 500)], 499).await;
        assert_eq!(capped[0].1, Outcome::OverBudget);
    }

    #[test]
    fn payee_shape() {
        assert!(payee("0xabcdef0123456789").valid());
        assert!(!payee("short").valid());
        assert!(!payee("0xabc def0123456789").valid());
    }
}
