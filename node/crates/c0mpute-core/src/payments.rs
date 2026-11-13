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
            return Err(NotPaid(format!("transfer {} already paid", t.key)).into());
        }
        let bal = s.balances.get(&t.payer).copied().unwrap_or(0);
        if bal < t.amount {
            return Err(NotPaid(format!("insufficient balance: {} < {}", format_usd(bal), format_usd(t.amount))).into());
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
        let res = match self
            .client
            .post(format!("{}/api/payouts/create", self.base))
            .bearer_auth(&self.api_key)
            .json(&self.payout_body(t))
            .send()
            .await
        {
            Ok(res) => res,
            // Never connected: nothing was sent, so nothing was paid.
            Err(e) if e.is_connect() => return Err(NotPaid(format!("CoinPay unreachable: {e}")).into()),
            // A timeout or a dropped connection: CoinPay may have paid.
            Err(e) => return Err(e).context("CoinPay payout request"),
        };
        let status = res.status();
        let body: serde_json::Value = res.json().await.unwrap_or(serde_json::Value::Null);
        if matches!(status.as_u16(), 502 | 503) {
            // The proxy had no backend to hand the request to (a CoinPay
            // deploy): nothing ran, so the next round may try again.
            return Err(NotPaid(format!("CoinPay payout answered {status}: service unavailable")).into());
        }
        if status.is_server_error() {
            // A 500 or a gateway timeout can hide a payout that went through.
            bail!("CoinPay payout answered {status}: {}", body["error"]);
        }
        if !status.is_success() || body["success"] != serde_json::Value::Bool(true) {
            return Err(NotPaid(format!("CoinPay payout answered {status}: {}", body["error"])).into());
        }
        Ok(Paid {
            key: t.key.clone(),
            rail_ref: body["payout"]["id"].as_str().unwrap_or("").to_string(),
        })
    }
}

/// A rail's error when the payment certainly did not happen (refused, or
/// never sent), so trying the same transfer again is safe. Any other error
/// from [`Rail::pay`] leaves the outcome unknown: the request may have been
/// processed before the answer was lost.
#[derive(Debug)]
pub struct NotPaid(pub String);

impl std::fmt::Display for NotPaid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NotPaid {}

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

/// Every transfer key this node has paid (or started paying), with the rail's
/// reference. A key is written to disk *before* its payment is sent and again
/// after, so neither a crash mid-payment nor an answer lost on the way back
/// ever pays twice.
#[derive(Default)]
pub struct Ledger {
    path: Option<PathBuf>,
    paid: Mutex<HashMap<String, LedgerEntry>>,
    /// Why this ledger must not be paid from: its file exists but could not
    /// be read back. An empty ledger here would pay everything again.
    broken: Option<String>,
    /// The last attempt to write a key before paying failed.
    stalled: std::sync::atomic::AtomicBool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub amount: Micros,
    pub payee: String,
    pub rail_ref: String,
    pub at: u64,
    /// The payment was sent and no answer recorded: it may or may not have
    /// gone through. Counts as paid (never retried automatically); an
    /// operator checks the rail for this key.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pending: bool,
}

impl Ledger {
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// Open the ledger at `path`. A file that is not there is a new, empty
    /// ledger. A file that is there but cannot be read or parsed is NOT: it
    /// marks the ledger unusable and [`pay_all`] refuses to pay from it
    /// (same rule as `ads::billing::Ledger::load`).
    pub fn open(path: PathBuf) -> Self {
        let (paid, broken) = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(paid) => (paid, None),
                Err(e) => (HashMap::new(), Some(format!("payments ledger {} is corrupt ({e}); not paying until it is repaired or removed", path.display()))),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (HashMap::new(), None),
            Err(e) => (HashMap::new(), Some(format!("payments ledger {} is unreadable ({e}); not paying", path.display()))),
        };
        if let Some(why) = &broken {
            tracing::error!("{why}");
        }
        Self {
            path: Some(path),
            paid: Mutex::new(paid),
            broken,
            stalled: Default::default(),
        }
    }

    /// True when the last [`pay_all`] stopped because a key could not be
    /// written: the caller must not treat that round as settled.
    pub fn stalled(&self) -> bool {
        self.stalled.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// `Err` with the reason when this ledger must not be paid from.
    pub fn usable(&self) -> std::result::Result<(), String> {
        match &self.broken {
            Some(why) => Err(why.clone()),
            None => Ok(()),
        }
    }

    pub fn contains(&self, key: &str) -> bool {
        self.paid.lock().unwrap().contains_key(key)
    }

    pub fn entries(&self) -> HashMap<String, LedgerEntry> {
        self.paid.lock().unwrap().clone()
    }

    /// Put `key` in the ledger and on disk. `Err` when the disk write failed;
    /// the entry then stays in memory only.
    fn put(&self, t: &Transfer, rail_ref: &str, pending: bool) -> std::io::Result<()> {
        let snapshot = {
            let mut m = self.paid.lock().unwrap();
            m.insert(
                t.key.clone(),
                LedgerEntry {
                    amount: t.amount,
                    payee: t.payee.wallet.clone(),
                    rail_ref: rail_ref.to_string(),
                    at: now_secs(),
                    pending,
                },
            );
            serde_json::to_vec_pretty(&*m).unwrap_or_default()
        };
        self.persist(&snapshot)
    }

    /// Forget `key` (its payment certainly did not happen).
    fn forget(&self, key: &str) -> std::io::Result<()> {
        let snapshot = {
            let mut m = self.paid.lock().unwrap();
            m.remove(key);
            serde_json::to_vec_pretty(&*m).unwrap_or_default()
        };
        self.persist(&snapshot)
    }

    fn persist(&self, snapshot: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        let Some(p) = &self.path else { return Ok(()) };
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = p.with_extension("tmp");
        let written = (|| {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(snapshot)?;
            f.sync_all()?;
            std::fs::rename(&tmp, p)
        })();
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        written
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
///
/// The ledger is the only thing between a retry and a second payment, so it
/// fails closed: nothing is paid from a ledger that could not be read, each
/// key is on disk before its payment is sent, and the round stops at the
/// first key that cannot be written.
pub async fn pay_all(rail: &dyn Rail, ledger: &Ledger, transfers: &[Transfer], cap: Micros) -> Vec<(Transfer, Outcome)> {
    let mut out = Vec::new();
    let mut spent: Micros = 0;
    let mut balances: HashMap<String, Option<Micros>> = HashMap::new();
    let mut stopped: Option<String> = ledger.usable().err();
    for t in transfers {
        if ledger.contains(&t.key) {
            out.push((t.clone(), Outcome::AlreadyPaid));
            continue;
        }
        if t.amount == 0 {
            continue;
        }
        if let Some(why) = &stopped {
            out.push((t.clone(), Outcome::Failed(why.clone())));
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
        // On disk first: if we die (or the answer is lost) after the rail
        // took the payment, the next round finds the key and does not repeat.
        let wrote = ledger.put(t, "", true);
        ledger.stalled.store(wrote.is_err(), std::sync::atomic::Ordering::Relaxed);
        if let Err(e) = wrote {
            ledger.paid.lock().unwrap().remove(&t.key);
            let why = format!("payments ledger is not writable ({e}); stopping this round before paying");
            tracing::error!(key = %t.key, "{why}");
            out.push((t.clone(), Outcome::Failed(why.clone())));
            stopped = Some(why);
            continue;
        }
        match rail.pay(t).await {
            Ok(p) => {
                if let Err(e) = ledger.put(t, &p.rail_ref, false) {
                    // The pending entry is on disk, so this key is still safe.
                    tracing::warn!(key = %t.key, err = %e, "payments: paid, but the ledger kept only the pending mark");
                }
                spent += t.amount;
                if let Some(Some(b)) = balances.get_mut(&t.payer) {
                    *b -= t.amount;
                }
                out.push((t.clone(), Outcome::Paid(p)));
            }
            Err(e) if e.downcast_ref::<NotPaid>().is_some() => {
                // Certainly not paid: free the key so a later round retries.
                if let Err(io) = ledger.forget(&t.key) {
                    tracing::warn!(key = %t.key, err = %io, "payments: could not clear a refused payment from the ledger file");
                }
                out.push((t.clone(), Outcome::Failed(e.to_string())));
            }
            Err(e) => {
                // Outcome unknown. The pending entry stays, so this key is
                // never sent again on its own: paying twice is worse than a
                // payout an operator has to look up.
                tracing::error!(key = %t.key, payee = %t.payee.wallet, amount = %format_usd(t.amount), err = %format!("{e:#}"), "payments: no clear answer from the rail; the transfer is marked pending and will not be retried. Check the rail for this key.");
                out.push((t.clone(), Outcome::Failed(format!("{e:#} (outcome unknown; left pending, not retried)"))));
            }
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

    /// A rail whose next answers are scripted.
    struct ScriptedRail(Mutex<Vec<std::result::Result<&'static str, anyhow::Error>>>, Mutex<Vec<String>>);

    impl ScriptedRail {
        fn new(answers: Vec<std::result::Result<&'static str, anyhow::Error>>) -> Self {
            Self(Mutex::new(answers), Mutex::new(Vec::new()))
        }
        fn sent(&self) -> Vec<String> {
            self.1.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl Rail for ScriptedRail {
        fn name(&self) -> &str {
            "scripted"
        }
        async fn balance(&self, _payer: &str) -> Result<Option<Micros>> {
            Ok(None)
        }
        async fn pay(&self, t: &Transfer) -> Result<Paid> {
            self.1.lock().unwrap().push(t.key.clone());
            let next = self.0.lock().unwrap().remove(0);
            next.map(|r| Paid { key: t.key.clone(), rail_ref: r.to_string() })
        }
    }

    #[tokio::test]
    async fn a_corrupt_ledger_pays_nothing_and_a_missing_one_is_new() {
        let dir = tempfile::tempdir().unwrap();
        let missing = Ledger::open(dir.path().join("none.json"));
        assert!(missing.usable().is_ok());
        let path = dir.path().join("ledger.json");
        std::fs::write(&path, b"{ this is not a ledger").unwrap();
        let ledger = Ledger::open(path.clone());
        assert!(ledger.usable().is_err());
        let rail = ScriptedRail::new(vec![Ok("r1")]);
        let out = pay_all(&rail, &ledger, &[t("a", "payer", "wallet-aaaaaaaa", 400)], 10_000).await;
        assert!(matches!(&out[0].1, Outcome::Failed(why) if why.contains("corrupt")), "{out:?}");
        assert!(rail.sent().is_empty(), "nothing is sent from a ledger that could not be read");
        assert_eq!(std::fs::read(&path).unwrap(), b"{ this is not a ledger", "the broken file is left for the operator");
    }

    #[tokio::test]
    async fn an_unanswered_payment_is_never_sent_twice_but_a_refused_one_is_retried() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.json");
        let ts = vec![t("lost", "payer", "wallet-aaaaaaaa", 400), t("refused", "payer", "wallet-bbbbbbbb", 400)];
        let rail = ScriptedRail::new(vec![
            Err(anyhow::anyhow!("operation timed out")),
            Err(NotPaid("CoinPay payout answered 400: bad wallet".into()).into()),
            Ok("r-refused-second-try"),
        ]);
        let ledger = Ledger::open(path.clone());
        let out = pay_all(&rail, &ledger, &ts, 10_000).await;
        assert!(matches!(&out[0].1, Outcome::Failed(why) if why.contains("outcome unknown")), "{out:?}");
        assert!(matches!(&out[1].1, Outcome::Failed(_)));
        // A fresh process (the ledger re-read from disk) retries only the refusal.
        let ledger = Ledger::open(path);
        assert!(ledger.entries()["lost"].pending);
        let again = pay_all(&rail, &ledger, &ts, 10_000).await;
        assert_eq!(again[0].1, Outcome::AlreadyPaid);
        assert!(matches!(again[1].1, Outcome::Paid(_)));
        assert_eq!(rail.sent(), vec!["lost", "refused", "refused"]);
        assert!(!ledger.entries()["refused"].pending);
    }

    #[tokio::test]
    async fn a_ledger_that_cannot_be_written_stops_the_round_before_paying() {
        let dir = tempfile::tempdir().unwrap();
        // A new ledger whose directory then cannot be made (a file has its
        // name): every write fails, as on a full or read-only disk.
        let blocker = dir.path().join("not-a-dir");
        let ledger = Ledger::open(blocker.join("ledger.json"));
        assert!(ledger.usable().is_ok());
        std::fs::write(&blocker, b"x").unwrap();
        let rail = ScriptedRail::new(vec![Ok("r1"), Ok("r2")]);
        let ts = vec![t("a", "payer", "wallet-aaaaaaaa", 400), t("b", "payer", "wallet-bbbbbbbb", 400)];
        let out = pay_all(&rail, &ledger, &ts, 10_000).await;
        assert!(out.iter().all(|(_, o)| matches!(o, Outcome::Failed(why) if why.contains("not writable"))), "{out:?}");
        assert!(rail.sent().is_empty(), "no payment without its key on disk first");
        assert!(ledger.stalled(), "the caller can tell the round did not settle");
        assert!(!ledger.contains("a"), "and the key is free for the next round");
    }

    #[tokio::test]
    async fn the_key_is_on_disk_before_the_payment_is_sent() {
        struct Peek(PathBuf);
        #[async_trait::async_trait]
        impl Rail for Peek {
            fn name(&self) -> &str {
                "peek"
            }
            async fn balance(&self, _payer: &str) -> Result<Option<Micros>> {
                Ok(None)
            }
            async fn pay(&self, t: &Transfer) -> Result<Paid> {
                let on_disk: HashMap<String, LedgerEntry> = serde_json::from_slice(&std::fs::read(&self.0)?)?;
                assert!(on_disk[&t.key].pending, "pending mark written first");
                Ok(Paid { key: t.key.clone(), rail_ref: "r".into() })
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.json");
        let ledger = Ledger::open(path.clone());
        let out = pay_all(&Peek(path.clone()), &ledger, &[t("a", "payer", "wallet-aaaaaaaa", 400)], 10_000).await;
        assert!(matches!(out[0].1, Outcome::Paid(_)), "{out:?}");
        let on_disk: HashMap<String, LedgerEntry> = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(!on_disk["a"].pending);
        assert_eq!(on_disk["a"].rail_ref, "r");
    }

    #[test]
    fn payee_shape() {
        assert!(payee("0xabcdef0123456789").valid());
        assert!(!payee("short").valid());
        assert!(!payee("0xabc def0123456789").valid());
    }
}
