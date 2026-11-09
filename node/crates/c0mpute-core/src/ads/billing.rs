//! Billing and settlement: advertisers pay gateways and shows for verified
//! impressions, peer to peer, from the advertiser's own wallet.
//!
//! The money rail is the one c0mpute already names everywhere
//! ([`SettlementAdapter::COINPAY`](c0mpute_envelope::SettlementAdapter)):
//! the `coinpay` CLI that the `coinpay` plugin delegates to. Nothing here
//! holds anyone's funds and there is no ad server or clearing house:
//!
//! 1. **Budget.** The advertiser signs a [`Budget`] into the campaign record
//!    (total, optional daily cap, the wallet it pays from, an optional CoinPay
//!    escrow or payment id as proof of funds). A campaign without a budget
//!    still runs, unpaid, exactly as before.
//! 2. **Cleared price.** Every impression receipt carries the price it
//!    cleared at ([`crate::ads::select`]: second price, floored at the
//!    publisher's floor, never above the winner's bid) and the campaign
//!    version it was served under.
//! 3. **Claim.** A gateway signs a [`Claim`] over a contiguous range of its
//!    own receipt batches for one campaign, naming the hash of the last one.
//!    The batches are the proof: signed by the same gateway, hash-chained, and
//!    the range has to start exactly where its last settlement ended.
//! 4. **Verify.** [`verify_claim`] is a pure function of the campaign, the
//!    advertiser's [`Ledger`], the claim and its batches, so every node gets
//!    the same answer. It recomputes the amount itself (the gateway's own
//!    total is informational), dedupes plays, applies the frequency and daily
//!    caps, refuses self-dealing, and refuses a claim that would overspend.
//! 5. **Settle.** The advertiser's node signs a [`Settlement`] (chained per
//!    campaign, carrying the running total spent), pays each line through a
//!    [`PaymentRail`], and gossips the settlement so gateways learn how much
//!    budget is left and where their next claim starts.
//!
//! Split of every paid impression ([`split`]): the c0mpute network fee is 5%,
//! the same single fee c0mpute's market and Infernet take (the operators keep
//! 95%); of the gross, the show that carried the ad gets 75% and the gateway
//! that served it 20%. With no show (VAST to a player that called the gateway
//! directly), the gateway is the publisher and gets 95%.
//!
//! Real money moves only with `[ads] payouts = true` on the advertiser's node
//! and `c0mpute ads settle --pay`; the default rail refuses to pay.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use c0mpute_proto::Hash;
use serde::{Deserialize, Serialize};

use super::campaign::{CampaignRecord, MICROS};
use super::receipt::{Event, ReceiptBatch};

const CLAIM_DOMAIN: &str = "c0mpute-ads-claim-v1";
const SETTLE_DOMAIN: &str = "c0mpute-ads-settlement-v1";

/// Basis points in one.
pub const BPS: u64 = 10_000;
/// c0mpute's network fee: the same 5% the market and Infernet take.
pub const FEE_BPS: u64 = 500;
/// The show (publisher) that carried the ad.
pub const PUBLISHER_BPS: u64 = 7_500;
/// The gateway that served it (selection, stitching, bandwidth, receipts).
/// It also gets any rounding remainder, and the publisher share when there
/// is no show.
pub const GATEWAY_BPS: u64 = BPS - FEE_BPS - PUBLISHER_BPS;
/// Most batches one claim covers (500 x 100 receipts).
pub const MAX_BATCHES_PER_CLAIM: u64 = 500;
/// A gateway does not claim less than this by default ($1), so payouts are
/// worth a transaction.
pub const DEFAULT_MIN_CLAIM_MICROS: u64 = MICROS;
/// Receipts stamped further in the future than this are not paid.
const CLOCK_SKEW_SECS: i64 = 300;
/// Listener history kept for frequency caps across claims.
const LISTENER_HISTORY_SECS: i64 = 7 * 86_400;

/// The advertiser's money for a campaign, signed into the campaign record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    /// Most the campaign ever pays, USD millionths.
    pub total_micros: u64,
    /// Most it pays for impressions served on one UTC day.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_micros: Option<u64>,
    /// Settlement adapter; "coinpay" is the only one with a rail today.
    pub rail: String,
    /// The advertiser's paying wallet, `<CHAIN>:<address>`. A gateway or show
    /// paid to this address is the advertiser paying itself and is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payer: Option<String>,
    /// A CoinPay escrow or payment id that shows the budget is funded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub funding_ref: Option<String>,
}

impl Budget {
    pub fn check(&self) -> Result<()> {
        if self.total_micros == 0 {
            bail!("budget must be above zero");
        }
        if self.daily_micros == Some(0) {
            bail!("daily budget must be above zero");
        }
        if !valid_token(&self.rail, 32) {
            bail!("budget rail must be a settlement adapter name (coinpay)");
        }
        if let Some(p) = &self.payer
            && !valid_payout(p)
        {
            bail!("payer must be <CHAIN>:<address>, e.g. SOL:9xQe...");
        }
        if let Some(f) = &self.funding_ref
            && (f.is_empty() || f.len() > 128 || f.chars().any(|c| c.is_control() || c.is_whitespace()))
        {
            bail!("funding ref must be a CoinPay escrow or payment id");
        }
        Ok(())
    }
}

fn valid_token(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// A payout address: `<CHAIN>:<address>`, e.g. `SOL:9xQe...`, `ETH:0xabc...`,
/// `BCH:bitcoincash:q...`. The chain is what `coinpay wallet send --chain`
/// takes.
pub fn valid_payout(s: &str) -> bool {
    let Some((chain, addr)) = s.split_once(':') else {
        return false;
    };
    chain.len() >= 2
        && chain.len() <= 16
        && chain.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && addr.len() >= 8
        && addr.len() <= 128
        && addr.chars().all(|c| c.is_ascii_alphanumeric() || ":_-.".contains(c))
}

/// The second-price rule. `runner_up` is the best losing bid, `floor` the
/// publisher's floor. The winner pays the larger of the two, never more than
/// its own bid; an uncontested slot with no floor clears at the bid (there
/// is no lower price anyone stated).
pub fn cleared_cpm(winner_bid: u64, runner_up: Option<u64>, floor: u64) -> u64 {
    let p = runner_up.unwrap_or(0).max(floor).min(winner_bid);
    if p == 0 { winner_bid } else { p }
}

// ── claims ───────────────────────────────────────────────────────────────

/// A gateway's signed request to be paid for batches `from_seq..=to_seq` of
/// its receipt chain for one campaign.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    pub v: u8,
    pub campaign_id: String,
    /// Gateway key (hex), the same key that signed the batches.
    pub gateway: String,
    /// Where the gateway's share goes, `<CHAIN>:<address>`.
    pub payout: String,
    pub from_seq: u64,
    pub to_seq: u64,
    /// [`ReceiptBatch::hash`] of batch `to_seq`.
    pub head: String,
    /// The gateway's own count and total (informational: the verifier
    /// recomputes both).
    pub impressions: u64,
    pub gross_micros: u64,
    pub ts: i64,
    pub sig: String,
}

impl Claim {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.sig = String::new();
        let mut out = format!("{CLAIM_DOMAIN}\n").into_bytes();
        out.extend(serde_json::to_vec(&unsigned).expect("claim serializes"));
        out
    }

    pub fn hash(&self) -> String {
        Hash::of(&self.signing_bytes()).to_hex()
    }

    pub fn verify(&self) -> bool {
        self.v == 1 && self.to_seq >= self.from_seq && crate::ads::verify_sig(&self.gateway, &self.signing_bytes(), &self.sig)
    }

    /// Build and sign a claim over `batches` (this gateway's, for one
    /// campaign) starting after `settled` (the last settled `(to_seq, head)`,
    /// or `None` when nothing is settled yet). Takes the longest run of
    /// batches whose estimated cost fits `remaining_micros`. `None` when there
    /// is nothing to claim or the first batch does not fit.
    pub fn build(
        keypair: &c0mpute_net::libp2p::identity::Keypair,
        campaign: &CampaignRecord,
        settled: Option<(u64, &str)>,
        batches: &[ReceiptBatch],
        remaining_micros: u64,
        payout: &str,
        now: i64,
    ) -> Option<Claim> {
        let gateway = crate::hosting::site::owner_hex(keypair)?;
        let mut by_seq: BTreeMap<u64, &ReceiptBatch> = BTreeMap::new();
        for b in batches {
            if b.gateway == gateway && b.campaign_id == campaign.campaign_id {
                by_seq.entry(b.seq).or_insert(b);
            }
        }
        let from = settled.map(|(s, _)| s + 1).unwrap_or(0);
        let mut prev = settled.map(|(_, h)| h.to_string()).unwrap_or_default();
        let mut cpm_sum: u128 = 0;
        let mut impressions = 0u64;
        let mut last: Option<(u64, String, u128, u64)> = None;
        let mut seq = from;
        while let Some(b) = by_seq.get(&seq) {
            if b.prev != prev || seq - from >= MAX_BATCHES_PER_CLAIM {
                break;
            }
            for r in &b.receipts {
                if r.event == Event::Impression && r.cpm_micros > 0 {
                    cpm_sum += r.cpm_micros.min(campaign.max_cpm_micros) as u128;
                    impressions += 1;
                }
            }
            if (cpm_sum / 1000) as u64 > remaining_micros {
                break;
            }
            prev = b.hash();
            last = Some((seq, prev.clone(), cpm_sum, impressions));
            seq += 1;
        }
        let (to, head, sum, n) = last?;
        let mut c = Claim {
            v: 1,
            campaign_id: campaign.campaign_id.clone(),
            gateway,
            payout: payout.to_string(),
            from_seq: from,
            to_seq: to,
            head,
            impressions: n,
            gross_micros: (sum / 1000) as u64,
            ts: now,
            sig: String::new(),
        };
        c.sig = hex::encode(keypair.sign(&c.signing_bytes()).ok()?);
        Some(c)
    }
}

/// What the advertiser knows about a show it may pay: the site owner's key
/// and the payout its signed opt-in names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Publisher {
    pub owner: String,
    pub payout: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Publisher,
    Gateway,
    Network,
}

/// One line of a settlement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Payout {
    pub role: Role,
    /// Recipient key (gateway or show owner); "" for the network fee.
    pub key: String,
    /// `<CHAIN>:<address>`; "" when no network fee address is configured
    /// (the fee is owed, not sent).
    pub address: String,
    pub micros: u64,
}

/// Why a claim was refused outright.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum ClaimError {
    BadSignature,
    WrongCampaign,
    /// The campaign has no budget: its ads run unpaid.
    NotBilled,
    /// The gateway is the advertiser, or pays out to the advertiser's wallet.
    SelfDealing,
    BadPayout,
    /// Starts before the gateway's last settled batch: already paid.
    AlreadySettled { next_seq: u64 },
    /// Starts after it: a batch in between is missing.
    Gap { next_seq: u64 },
    TooLarge,
    /// A batch in the range is missing, unsigned, or not the claimant's.
    MissingBatch { seq: u64 },
    /// Two different signed batches with the same seq.
    Equivocation { seq: u64 },
    /// The chain does not link (prev hash) or does not end at `head`.
    BrokenChain { seq: u64 },
    /// Paying it would spend more than the budget.
    OverBudget { remaining_micros: u64, asked_micros: u64 },
}

impl std::fmt::Display for ClaimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", serde_json::to_string(self).unwrap_or_default())
    }
}

/// Why an impression inside an accepted claim was not paid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Excluded {
    /// Served before billing (no cleared price on the receipt).
    Unpriced,
    /// No listener id: frequency caps cannot apply.
    NoListener,
    /// The play was already paid (here or in an earlier claim).
    Duplicate,
    OutsideSchedule,
    FutureTimestamp,
    /// Creative or placement that is not in the campaign.
    NotInCampaign,
    /// The show is not known to the verifier.
    UnknownShow,
    /// The show's opt-in names no payout address.
    ShowHasNoPayout,
    /// The show is the advertiser's own.
    SelfDealing,
    FrequencyCap,
    DailyCap,
}

/// The result of verifying a claim: what to pay and what changes in the
/// ledger.
#[derive(Clone, Debug, Serialize)]
pub struct Verified {
    pub claim: Claim,
    pub claim_hash: String,
    pub impressions: u64,
    pub excluded: BTreeMap<Excluded, u64>,
    pub gross_micros: u64,
    pub payouts: Vec<Payout>,
    #[serde(skip)]
    new_plays: Vec<String>,
    #[serde(skip)]
    new_listens: Vec<(String, i64)>,
    #[serde(skip)]
    day_cpm: BTreeMap<i64, u128>,
}

/// The campaign's max CPM in force for a receipt served under version `seq`:
/// the newest known version at or before it, else the oldest known.
pub fn bid_at(bids: &BTreeMap<u64, u64>, seq: u64) -> Option<u64> {
    bids.range(..=seq).next_back().or_else(|| bids.iter().next()).map(|(_, b)| *b)
}

/// Split `gross` into payouts. `by_show` is gross per paid show (key,
/// payout); the rest of `gross` was served without a show.
pub fn split(gross: u64, by_show: &BTreeMap<(String, String), u64>, gateway: &str, gateway_payout: &str, fee_payout: &str) -> Vec<Payout> {
    let fee = gross * FEE_BPS / BPS;
    let mut out = Vec::new();
    let mut to_shows = 0u64;
    for ((key, addr), g) in by_show {
        let m = g * PUBLISHER_BPS / BPS;
        to_shows += m;
        out.push(Payout {
            role: Role::Publisher,
            key: key.clone(),
            address: addr.clone(),
            micros: m,
        });
    }
    out.push(Payout {
        role: Role::Gateway,
        key: gateway.to_string(),
        address: gateway_payout.to_string(),
        micros: gross - fee - to_shows,
    });
    out.push(Payout {
        role: Role::Network,
        key: String::new(),
        address: fee_payout.to_string(),
        micros: fee,
    });
    out
}

/// Check a claim against the campaign and the ledger, and work out what it
/// is worth. Pure: every node with the same inputs gets the same answer.
///
/// * `bids`: seq -> max CPM of every campaign version the verifier knows
///   (receipts never pay more than the bid they were served under).
/// * `publishers`: site id -> show owner and payout.
/// * `fee_payout`: the network fee address ("" = owed, not sent).
#[allow(clippy::too_many_arguments)]
pub fn verify_claim(
    campaign: &CampaignRecord,
    bids: &BTreeMap<u64, u64>,
    ledger: &Ledger,
    claim: &Claim,
    batches: &[ReceiptBatch],
    publishers: &HashMap<String, Publisher>,
    fee_payout: &str,
    now: i64,
) -> std::result::Result<Verified, ClaimError> {
    if !claim.verify() {
        return Err(ClaimError::BadSignature);
    }
    if claim.campaign_id != campaign.campaign_id || ledger.campaign_id != campaign.campaign_id {
        return Err(ClaimError::WrongCampaign);
    }
    let Some(budget) = &campaign.budget else {
        return Err(ClaimError::NotBilled);
    };
    if !valid_payout(&claim.payout) {
        return Err(ClaimError::BadPayout);
    }
    if claim.gateway == campaign.owner || budget.payer.as_deref() == Some(claim.payout.as_str()) {
        return Err(ClaimError::SelfDealing);
    }
    let settled = ledger.through.get(&claim.gateway);
    let next_seq = settled.map(|(s, _)| s + 1).unwrap_or(0);
    if claim.from_seq < next_seq {
        return Err(ClaimError::AlreadySettled { next_seq });
    }
    if claim.from_seq > next_seq {
        return Err(ClaimError::Gap { next_seq });
    }
    if claim.to_seq - claim.from_seq >= MAX_BATCHES_PER_CLAIM {
        return Err(ClaimError::TooLarge);
    }

    // The batches: the claimant's own, verified, one per seq, chained.
    let mut by_seq: BTreeMap<u64, &ReceiptBatch> = BTreeMap::new();
    for b in batches {
        if b.gateway != claim.gateway || b.campaign_id != claim.campaign_id || b.seq < claim.from_seq || b.seq > claim.to_seq {
            continue;
        }
        if !b.verify() {
            continue;
        }
        match by_seq.get(&b.seq) {
            Some(have) if have.sig != b.sig => return Err(ClaimError::Equivocation { seq: b.seq }),
            Some(_) => {}
            None => {
                by_seq.insert(b.seq, b);
            }
        }
    }
    let mut prev = settled.map(|(_, h)| h.clone()).unwrap_or_default();
    for seq in claim.from_seq..=claim.to_seq {
        let b = by_seq.get(&seq).ok_or(ClaimError::MissingBatch { seq })?;
        if b.prev != prev {
            return Err(ClaimError::BrokenChain { seq });
        }
        prev = b.hash();
    }
    if prev != claim.head {
        return Err(ClaimError::BrokenChain { seq: claim.to_seq });
    }

    // Price every impression.
    let mut excluded: BTreeMap<Excluded, u64> = BTreeMap::new();
    let mut plays: BTreeSet<String> = BTreeSet::new();
    let mut listens: Vec<(String, i64)> = Vec::new();
    let mut day_cpm: BTreeMap<i64, u128> = BTreeMap::new();
    // Gross per show (key, payout) in CPM units; `None` = no show.
    let mut cpm_by: BTreeMap<Option<(String, String)>, u128> = BTreeMap::new();
    let mut impressions = 0u64;
    let daily_cap = budget.daily_micros.map(|d| d as u128 * 1000);
    for b in by_seq.values() {
        for r in &b.receipts {
            if r.event != Event::Impression {
                continue;
            }
            let mut skip = |why: Excluded| *excluded.entry(why).or_default() += 1;
            if r.cpm_micros == 0 {
                skip(Excluded::Unpriced);
                continue;
            }
            if r.listener.is_empty() {
                skip(Excluded::NoListener);
                continue;
            }
            if ledger.plays.contains(&r.serving_id) || plays.contains(&r.serving_id) {
                skip(Excluded::Duplicate);
                continue;
            }
            if r.ts > now + CLOCK_SKEW_SECS || r.ts > claim.ts + CLOCK_SKEW_SECS {
                skip(Excluded::FutureTimestamp);
                continue;
            }
            if !campaign.schedule.contains(r.ts) {
                skip(Excluded::OutsideSchedule);
                continue;
            }
            if campaign.creative(&r.creative).is_none() || !campaign.targeting.placements.contains(&r.placement) {
                skip(Excluded::NotInCampaign);
                continue;
            }
            let show = match &r.site {
                None => None,
                Some(site) => match publishers.get(site) {
                    None => {
                        skip(Excluded::UnknownShow);
                        continue;
                    }
                    Some(p) if p.owner == campaign.owner => {
                        skip(Excluded::SelfDealing);
                        continue;
                    }
                    Some(p) => match &p.payout {
                        Some(addr) if valid_payout(addr) && budget.payer.as_deref() != Some(addr.as_str()) => {
                            Some((p.owner.clone(), addr.clone()))
                        }
                        Some(_) => {
                            skip(Excluded::SelfDealing);
                            continue;
                        }
                        None => {
                            skip(Excluded::ShowHasNoPayout);
                            continue;
                        }
                    },
                },
            };
            if let Some(cap) = campaign.freq_cap {
                let since = r.ts - cap.per_secs as i64;
                let seen = ledger
                    .listens
                    .get(&r.listener)
                    .map(|v| v.iter().filter(|t| **t > since && **t <= r.ts).count())
                    .unwrap_or(0)
                    + listens.iter().filter(|(l, t)| *l == r.listener && *t > since && *t <= r.ts).count();
                if seen >= cap.max as usize {
                    skip(Excluded::FrequencyCap);
                    continue;
                }
            }
            let cpm = r.cpm_micros.min(bid_at(bids, r.campaign_seq).unwrap_or(campaign.max_cpm_micros)) as u128;
            let day = r.ts.div_euclid(86_400);
            if let Some(cap) = daily_cap {
                let spent = ledger.day_cpm.get(&day).copied().unwrap_or(0) + day_cpm.get(&day).copied().unwrap_or(0);
                if spent + cpm > cap {
                    skip(Excluded::DailyCap);
                    continue;
                }
            }
            *day_cpm.entry(day).or_default() += cpm;
            *cpm_by.entry(show).or_default() += cpm;
            plays.insert(r.serving_id.clone());
            listens.push((r.listener.clone(), r.ts));
            impressions += 1;
        }
    }
    let mut by_show: BTreeMap<(String, String), u64> = BTreeMap::new();
    let mut gross = 0u64;
    for (show, cpm) in &cpm_by {
        let g = (cpm / 1000) as u64;
        gross += g;
        if let Some(s) = show {
            by_show.insert(s.clone(), g);
        }
    }
    let remaining = budget.total_micros.saturating_sub(ledger.spent_micros);
    if gross > remaining {
        return Err(ClaimError::OverBudget {
            remaining_micros: remaining,
            asked_micros: gross,
        });
    }
    let payouts = split(gross, &by_show, &claim.gateway, &claim.payout, fee_payout);
    Ok(Verified {
        claim: claim.clone(),
        claim_hash: claim.hash(),
        impressions,
        excluded,
        gross_micros: gross,
        payouts,
        new_plays: plays.into_iter().collect(),
        new_listens: listens,
        day_cpm,
    })
}

// ── settlements ──────────────────────────────────────────────────────────

/// The advertiser's signed statement that a claim was accepted and how it is
/// paid. Chained per campaign (`seq`, `prev`), with the running total spent,
/// so anyone can see how much budget is left.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settlement {
    pub v: u8,
    pub campaign_id: String,
    /// Advertiser key (hex): the campaign owner.
    pub owner: String,
    pub seq: u64,
    /// Hash of settlement `seq - 1`; "" for the first.
    pub prev: String,
    /// The claim, by [`Claim::hash`].
    pub claim: String,
    pub gateway: String,
    pub from_seq: u64,
    pub to_seq: u64,
    /// Hash of the gateway's batch `to_seq`: its next claim chains from here.
    pub head: String,
    pub impressions: u64,
    pub gross_micros: u64,
    /// Total spent by the campaign after this settlement.
    pub spent_micros: u64,
    pub budget_micros: u64,
    pub payouts: Vec<Payout>,
    pub rail: String,
    pub ts: i64,
    pub sig: String,
}

impl Settlement {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.sig = String::new();
        let mut out = format!("{SETTLE_DOMAIN}\n").into_bytes();
        out.extend(serde_json::to_vec(&unsigned).expect("settlement serializes"));
        out
    }

    pub fn hash(&self) -> String {
        Hash::of(&self.signing_bytes()).to_hex()
    }

    /// Signed by `owner`, and the lines add up to the gross.
    pub fn verify(&self) -> bool {
        self.v == 1
            && self.payouts.iter().map(|p| p.micros).sum::<u64>() == self.gross_micros
            && self.spent_micros <= self.budget_micros
            && crate::ads::verify_sig(&self.owner, &self.signing_bytes(), &self.sig)
    }
}

/// One paid (or owed) line, with the rail's reference.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineStatus {
    pub settlement: u64,
    pub line: usize,
    /// The rail's transfer id once sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The advertiser's books for one campaign: what was settled, with whom,
/// and what is left. Kept next to the campaign (`ledger.json`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Ledger {
    pub campaign_id: String,
    pub spent_micros: u64,
    pub settlements: Vec<Settlement>,
    /// Per gateway: last settled batch seq and its hash.
    pub through: BTreeMap<String, (u64, String)>,
    /// Plays (serving ids) already paid.
    pub plays: BTreeSet<String>,
    /// Paid impressions per listener (unix seconds), for frequency caps.
    pub listens: BTreeMap<String, Vec<i64>>,
    /// Paid CPM units (micros x 1000) per UTC day, for the daily cap.
    pub day_cpm: BTreeMap<i64, u128>,
    pub lines: Vec<LineStatus>,
}

impl Ledger {
    pub fn new(campaign_id: &str) -> Self {
        Ledger {
            campaign_id: campaign_id.to_string(),
            ..Default::default()
        }
    }

    pub fn remaining(&self, budget: &Budget) -> u64 {
        budget.total_micros.saturating_sub(self.spent_micros)
    }

    /// Record a verified claim as the next settlement, signed by the
    /// campaign owner (`None`: unsigned, for a dry run on a scratch copy).
    /// Money moves later, in [`Ledger::pay`].
    pub fn settle(
        &mut self,
        campaign: &CampaignRecord,
        v: Verified,
        owner: Option<&c0mpute_net::libp2p::identity::Keypair>,
        now: i64,
    ) -> Result<Settlement> {
        let budget = campaign.budget.as_ref().context("campaign has no budget")?;
        if let Some(kp) = owner
            && crate::hosting::site::owner_hex(kp).as_deref() != Some(campaign.owner.as_str())
        {
            bail!("only the campaign owner settles its claims");
        }
        let expect = self.through.get(&v.claim.gateway).map(|(s, _)| s + 1).unwrap_or(0);
        if v.claim.from_seq != expect {
            bail!("claim no longer starts at batch {expect}: verify it again");
        }
        let spent = self.spent_micros + v.gross_micros;
        if spent > budget.total_micros {
            bail!("settling would overspend the budget");
        }
        let mut s = Settlement {
            v: 1,
            campaign_id: campaign.campaign_id.clone(),
            owner: campaign.owner.clone(),
            seq: self.settlements.len() as u64,
            prev: self.settlements.last().map(Settlement::hash).unwrap_or_default(),
            claim: v.claim_hash.clone(),
            gateway: v.claim.gateway.clone(),
            from_seq: v.claim.from_seq,
            to_seq: v.claim.to_seq,
            head: v.claim.head.clone(),
            impressions: v.impressions,
            gross_micros: v.gross_micros,
            spent_micros: spent,
            budget_micros: budget.total_micros,
            payouts: v.payouts.clone(),
            rail: budget.rail.clone(),
            ts: now,
            sig: String::new(),
        };
        if let Some(kp) = owner {
            s.sig = hex::encode(kp.sign(&s.signing_bytes()).map_err(|e| anyhow::anyhow!("sign settlement: {e}"))?);
        }
        self.spent_micros = spent;
        self.through.insert(s.gateway.clone(), (s.to_seq, s.head.clone()));
        self.plays.extend(v.new_plays);
        for (l, t) in v.new_listens {
            self.listens.entry(l).or_default().push(t);
        }
        let horizon = now - LISTENER_HISTORY_SECS;
        self.listens.retain(|_, v| {
            v.retain(|t| *t > horizon);
            !v.is_empty()
        });
        for (d, c) in v.day_cpm {
            *self.day_cpm.entry(d).or_default() += c;
        }
        for (i, p) in s.payouts.iter().enumerate() {
            if p.micros > 0 {
                self.lines.push(LineStatus {
                    settlement: s.seq,
                    line: i,
                    reference: None,
                    error: None,
                });
            }
        }
        self.settlements.push(s.clone());
        Ok(s)
    }

    /// Send every unpaid line that has an address. Idempotent per line: a
    /// line with a reference is never sent again, and the rail gets
    /// `<settlement hash>:<line>` as an idempotency key. Returns lines sent.
    pub fn pay(&mut self, rail: &dyn PaymentRail, payer: Option<&str>) -> usize {
        let mut sent = 0;
        for st in self.lines.iter_mut().filter(|l| l.reference.is_none()) {
            let Some(s) = self.settlements.get(st.settlement as usize) else {
                continue;
            };
            let Some(p) = s.payouts.get(st.line) else {
                continue;
            };
            if p.address.is_empty() {
                st.error = Some("no address: owed, not sent".into());
                continue;
            }
            let key = format!("{}:{}", s.hash(), st.line);
            match rail.pay(payer, &p.address, p.micros, &key) {
                Ok(r) => {
                    st.reference = Some(r);
                    st.error = None;
                    sent += 1;
                }
                Err(e) => st.error = Some(e.to_string()),
            }
        }
        sent
    }

    /// Lines still owed (unsent), in micros.
    pub fn unpaid_micros(&self) -> u64 {
        self.lines
            .iter()
            .filter(|l| l.reference.is_none())
            .filter_map(|l| self.settlements.get(l.settlement as usize)?.payouts.get(l.line))
            .map(|p| p.micros)
            .sum()
    }

    pub fn load(path: &std::path::Path, campaign_id: &str) -> Result<Ledger> {
        match std::fs::read(path) {
            Ok(b) => {
                let l: Ledger = serde_json::from_slice(&b).with_context(|| format!("parse {}", path.display()))?;
                if l.campaign_id != campaign_id {
                    bail!("{} is the ledger of another campaign", path.display());
                }
                Ok(l)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Ledger::new(campaign_id)),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, path: &std::path::Path) -> Result<()> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

// ── rails ────────────────────────────────────────────────────────────────

/// Moves money. `pay` sends `micros` USD millionths to `to`
/// (`<CHAIN>:<address>`) and returns the rail's transfer id.
pub trait PaymentRail: Send + Sync {
    fn name(&self) -> &str;
    fn pay(&self, from: Option<&str>, to: &str, micros: u64, idempotency_key: &str) -> Result<String>;
}

/// The default: refuses to move money.
pub struct DisabledRail;

impl PaymentRail for DisabledRail {
    fn name(&self) -> &str {
        "disabled"
    }
    fn pay(&self, _: Option<&str>, _: &str, _: u64, _: &str) -> Result<String> {
        bail!("payouts are disabled: set `[ads] payouts = true` and pass --pay to send real money")
    }
}

/// An in-memory rail for tests and dry runs: accounts with balances, every
/// transfer logged, idempotency keys honoured, overdrafts refused.
#[derive(Default)]
pub struct FakeRail {
    pub balances: Mutex<HashMap<String, u64>>,
    pub transfers: Mutex<Vec<(String, String, u64, String)>>,
    keys: Mutex<HashMap<String, String>>,
}

impl FakeRail {
    pub fn fund(&self, account: &str, micros: u64) {
        *self.balances.lock().unwrap().entry(account.to_string()).or_default() += micros;
    }
    pub fn balance(&self, account: &str) -> u64 {
        self.balances.lock().unwrap().get(account).copied().unwrap_or(0)
    }
}

impl PaymentRail for FakeRail {
    fn name(&self) -> &str {
        "fake"
    }
    fn pay(&self, from: Option<&str>, to: &str, micros: u64, key: &str) -> Result<String> {
        if let Some(r) = self.keys.lock().unwrap().get(key) {
            return Ok(r.clone());
        }
        let from = from.context("fake rail needs a payer account")?;
        let mut bal = self.balances.lock().unwrap();
        let have = bal.get(from).copied().unwrap_or(0);
        if have < micros {
            bail!("insufficient funds: {from} has {have}, needs {micros}");
        }
        bal.insert(from.to_string(), have - micros);
        *bal.entry(to.to_string()).or_default() += micros;
        let mut t = self.transfers.lock().unwrap();
        let r = format!("fake-{}", t.len());
        t.push((from.to_string(), to.to_string(), micros, r.clone()));
        self.keys.lock().unwrap().insert(key.to_string(), r.clone());
        Ok(r)
    }
}

/// The real rail: `coinpay wallet send --chain <CHAIN> --to <address>
/// --amount-fiat <usd> --fiat USD` from the advertiser's own CoinPay wallet
/// (the `coinpay` plugin's CLI; it prompts for the wallet password unless
/// one is configured there). Nothing in c0mpute holds the funds.
pub struct CoinpayCliRail {
    pub bin: PathBuf,
    pub wallet_file: Option<PathBuf>,
}

impl CoinpayCliRail {
    /// `coinpay` on PATH (or `~/.c0mpute/bin`).
    pub fn find() -> Option<Self> {
        let bin = crate::ads::stitch::find_tool("coinpay")?;
        Some(CoinpayCliRail { bin, wallet_file: None })
    }

    /// The argv for one payout (without the binary).
    pub fn args(&self, to: &str, micros: u64) -> Result<Vec<String>> {
        if !valid_payout(to) {
            bail!("not a payout address: {to:?}");
        }
        let (chain, addr) = to.split_once(':').expect("checked");
        let mut a = vec![
            "wallet".into(),
            "send".into(),
            "--chain".into(),
            chain.into(),
            "--to".into(),
            addr.into(),
            "--amount-fiat".into(),
            usd_decimal(micros),
            "--fiat".into(),
            "USD".into(),
        ];
        if let Some(w) = &self.wallet_file {
            a.push("--wallet-file".into());
            a.push(w.display().to_string());
        }
        Ok(a)
    }
}

impl PaymentRail for CoinpayCliRail {
    fn name(&self) -> &str {
        "coinpay"
    }
    fn pay(&self, _from: Option<&str>, to: &str, micros: u64, key: &str) -> Result<String> {
        let out = std::process::Command::new(&self.bin)
            .args(self.args(to, micros)?)
            .stdin(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .output()
            .with_context(|| format!("run {}", self.bin.display()))?;
        if !out.status.success() {
            bail!("coinpay wallet send failed ({}) for {key}", out.status);
        }
        let text = String::from_utf8_lossy(&out.stdout);
        // The CLI prints the transaction hash; keep the last token that looks
        // like one, else the whole (trimmed) output.
        let r = text
            .split_whitespace()
            .rev()
            .find(|t| t.len() >= 16 && t.chars().all(|c| c.is_ascii_alphanumeric()))
            .map(String::from)
            .unwrap_or_else(|| text.trim().chars().take(200).collect());
        Ok(r)
    }
}

/// 1_234_500 -> "1.2345" (USD, trailing zeros dropped).
pub fn usd_decimal(micros: u64) -> String {
    let s = format!("{}.{:06}", micros / MICROS, micros % MICROS);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ads::campaign::tests::sample;
    use crate::ads::campaign::{FreqCap, Placement, Schedule};
    use crate::ads::receipt::{Receipt, ReceiptLog};
    use c0mpute_net::libp2p::identity::Keypair;

    const NOW: i64 = 1_800_000_000;

    fn billed(kp: &Keypair, total: u64) -> CampaignRecord {
        let mut r = sample(kp, "billed", 1);
        r.budget = Some(Budget {
            total_micros: total,
            daily_micros: None,
            rail: "coinpay".into(),
            payer: Some("SOL:advertiserwallet1".into()),
            funding_ref: None,
        });
        r.sign(kp).unwrap()
    }

    fn imp(c: &CampaignRecord, serving: &str, listener: &str, cpm: u64, site: Option<&str>) -> Receipt {
        Receipt {
            campaign_id: c.campaign_id.clone(),
            creative: c.creatives[0].hash.clone(),
            placement: Placement::PodcastPre,
            event: Event::Impression,
            ts: NOW - 60,
            listener: listener.into(),
            serving_id: serving.into(),
            site: site.map(String::from),
            country: None,
            cpm_micros: cpm,
            campaign_seq: c.seq,
        }
    }

    fn bids(c: &CampaignRecord) -> BTreeMap<u64, u64> {
        BTreeMap::from([(c.seq, c.max_cpm_micros)])
    }

    fn shows(owner: &str) -> HashMap<String, Publisher> {
        HashMap::from([(
            "show1".to_string(),
            Publisher {
                owner: owner.into(),
                payout: Some("SOL:showwallet0001".into()),
            },
        )])
    }

    /// One gateway flushes `per_batch` receipts per batch.
    fn chain(gw: &Keypair, receipts: Vec<Receipt>, per_batch: usize) -> Vec<ReceiptBatch> {
        let mut log = ReceiptLog::open(None);
        let mut out = Vec::new();
        for chunk in receipts.chunks(per_batch) {
            for r in chunk {
                log.add(r.clone());
            }
            out.extend(log.flush(gw));
        }
        out
    }

    #[test]
    fn second_price() {
        assert_eq!(cleared_cpm(5_000_000, Some(3_000_000), 0), 3_000_000);
        assert_eq!(cleared_cpm(5_000_000, Some(3_000_000), 4_000_000), 4_000_000, "floor beats runner-up");
        assert_eq!(cleared_cpm(5_000_000, Some(5_000_000), 0), 5_000_000, "tie pays the bid");
        assert_eq!(cleared_cpm(5_000_000, None, 1_000_000), 1_000_000, "uncontested pays the floor");
        assert_eq!(cleared_cpm(5_000_000, None, 0), 5_000_000, "no floor, no rival: the bid");
        assert_eq!(cleared_cpm(2_000_000, Some(9_000_000), 0), 2_000_000, "never above the bid");
    }

    #[test]
    fn split_adds_up_and_follows_the_fee_convention() {
        let by = BTreeMap::from([(("show".to_string(), "SOL:showwallet0001".to_string()), 1_000_000u64)]);
        let p = split(1_000_000, &by, "gw", "SOL:gatewaywallet1", "SOL:networkfee0001");
        let get = |r: Role| p.iter().find(|x| x.role == r).unwrap().micros;
        assert_eq!(get(Role::Network), 50_000, "5% network fee");
        assert_eq!(get(Role::Publisher), 750_000);
        assert_eq!(get(Role::Gateway), 200_000);
        // No show: the gateway is the publisher (95%).
        let p = split(1_000_000, &BTreeMap::new(), "gw", "SOL:gatewaywallet1", "");
        assert_eq!(p.iter().find(|x| x.role == Role::Gateway).unwrap().micros, 950_000);
        assert_eq!(p.iter().find(|x| x.role == Role::Network).unwrap().address, "");
        // Odd amounts: rounding goes to the gateway and nothing is lost.
        for g in [1u64, 7, 333, 999_999, 12_345_679] {
            let by = BTreeMap::from([
                (("a".to_string(), "SOL:showwallet000a".to_string()), g / 3),
                (("b".to_string(), "SOL:showwallet000b".to_string()), g / 3),
            ]);
            let p = split(g, &by, "gw", "SOL:gatewaywallet1", "");
            assert_eq!(p.iter().map(|x| x.micros).sum::<u64>(), g);
        }
        assert_eq!(FEE_BPS + PUBLISHER_BPS + GATEWAY_BPS, BPS);
    }

    #[test]
    fn verify_prices_dedupes_and_settles() {
        let adv = Keypair::generate_ed25519();
        let gw = Keypair::generate_ed25519();
        let show_owner = crate::hosting::site::owner_hex(&Keypair::generate_ed25519()).unwrap();
        let c = billed(&adv, 10 * MICROS);
        // 4 impressions at $2 CPM ($0.002 each) on show1, a duplicate play,
        // a quartile (not billed) and an unpriced old receipt.
        let mut rs: Vec<Receipt> = (0..4).map(|i| imp(&c, &format!("s{i}"), &format!("l{i}"), 2 * MICROS, Some("show1"))).collect();
        rs.push(imp(&c, "s0", "l0", 2 * MICROS, Some("show1")));
        let mut q = imp(&c, "s1", "l1", 2 * MICROS, Some("show1"));
        q.event = Event::Complete;
        rs.push(q);
        rs.push(imp(&c, "old", "l9", 0, Some("show1")));
        let batches = chain(&gw, rs, 3);
        let claim = Claim::build(&gw, &c, None, &batches, u64::MAX, "SOL:gatewaywallet1", NOW).unwrap();
        assert_eq!((claim.from_seq, claim.to_seq), (0, 2));
        let mut ledger = Ledger::new(&c.campaign_id);
        let v = verify_claim(&c, &bids(&c), &ledger, &claim, &batches, &shows(&show_owner), "SOL:networkfee0001", NOW).unwrap();
        assert_eq!(v.impressions, 4);
        assert_eq!(v.gross_micros, 8_000);
        assert_eq!(v.excluded[&Excluded::Duplicate], 1);
        assert_eq!(v.excluded[&Excluded::Unpriced], 1);
        assert_eq!(v.payouts.iter().map(|p| p.micros).sum::<u64>(), 8_000);
        let s = ledger.settle(&c, v, Some(&adv), NOW).unwrap();
        assert!(s.verify());
        assert_eq!(ledger.spent_micros, 8_000);
        assert_eq!(ledger.through[&claim.gateway].0, 2);

        // The same claim again: already settled.
        assert_eq!(
            verify_claim(&c, &bids(&c), &ledger, &claim, &batches, &shows(&show_owner), "", NOW).unwrap_err(),
            ClaimError::AlreadySettled { next_seq: 3 }
        );
        // Paying through the fake rail moves exactly the gross.
        let rail = FakeRail::default();
        rail.fund("SOL:advertiserwallet1", MICROS);
        assert_eq!(ledger.pay(&rail, Some("SOL:advertiserwallet1")), 3);
        assert_eq!(rail.balance("SOL:advertiserwallet1"), MICROS - 8_000);
        assert_eq!(rail.balance("SOL:showwallet0001"), 6_000);
        assert_eq!(rail.balance("SOL:gatewaywallet1"), 1_600);
        assert_eq!(rail.balance("SOL:networkfee0001"), 400);
        assert_eq!(ledger.pay(&rail, Some("SOL:advertiserwallet1")), 0, "never paid twice");
        assert_eq!(ledger.unpaid_micros(), 0);

        // A later batch whose play was already paid in this claim is a duplicate.
        let mut log = ReceiptLog::open(None);
        for b in &batches {
            for r in &b.receipts {
                log.add(r.clone());
            }
            log.flush(&gw);
        }
        log.add(imp(&c, "s2", "l2", 2 * MICROS, Some("show1")));
        log.add(imp(&c, "s9", "l9", 2 * MICROS, None));
        log.flush(&gw);
        let all = log.batches(&c.campaign_id, 0);
        let (to, head) = ledger.through[&claim.gateway].clone();
        let next = Claim::build(&gw, &c, Some((to, &head)), &all, u64::MAX, "SOL:gatewaywallet1", NOW).unwrap();
        let v = verify_claim(&c, &bids(&c), &ledger, &next, &all, &shows(&show_owner), "", NOW).unwrap();
        assert_eq!(v.impressions, 1);
        assert_eq!(v.excluded[&Excluded::Duplicate], 1);
        // No show: the gateway gets 95%, the fee is owed (no address).
        assert_eq!(v.payouts.iter().find(|p| p.role == Role::Gateway).unwrap().micros, 1_900);
        let s2 = ledger.settle(&c, v, Some(&adv), NOW).unwrap();
        assert_eq!(s2.prev, s.hash());
        assert_eq!(s2.spent_micros, 10_000);
        ledger.pay(&rail, Some("SOL:advertiserwallet1"));
        assert_eq!(ledger.unpaid_micros(), 100, "network fee owed, not sent");
    }

    #[test]
    fn chain_and_signature_attacks_are_refused() {
        let adv = Keypair::generate_ed25519();
        let gw = Keypair::generate_ed25519();
        let other = Keypair::generate_ed25519();
        let c = billed(&adv, 10 * MICROS);
        let rs: Vec<Receipt> = (0..6).map(|i| imp(&c, &format!("s{i}"), &format!("l{i}"), MICROS, None)).collect();
        let batches = chain(&gw, rs.clone(), 2);
        let ledger = Ledger::new(&c.campaign_id);
        let ok = Claim::build(&gw, &c, None, &batches, u64::MAX, "SOL:gatewaywallet1", NOW).unwrap();
        let v = |claim: &Claim, b: &[ReceiptBatch]| verify_claim(&c, &bids(&c), &ledger, claim, b, &HashMap::new(), "", NOW);
        assert!(v(&ok, &batches).is_ok());

        // Tampered claim.
        let mut t = ok.clone();
        t.to_seq += 1;
        assert_eq!(v(&t, &batches).unwrap_err(), ClaimError::BadSignature);
        // A missing batch in the middle.
        let mut holes = batches.clone();
        holes.remove(1);
        assert_eq!(v(&ok, &holes).unwrap_err(), ClaimError::MissingBatch { seq: 1 });
        // Batches signed by someone else do not count for this gateway.
        let theirs = chain(&other, rs.clone(), 2);
        assert_eq!(v(&ok, &theirs).unwrap_err(), ClaimError::MissingBatch { seq: 0 });
        // Two different signed batches with the same seq.
        let mut forked = batches.clone();
        forked.push(ReceiptBatch::sign(&gw, &c.campaign_id, 1, batches[0].hash(), vec![rs[5].clone()]).unwrap());
        assert_eq!(v(&ok, &forked).unwrap_err(), ClaimError::Equivocation { seq: 1 });
        // A rewritten history that does not link.
        let mut rewritten = vec![batches[0].clone()];
        rewritten.push(ReceiptBatch::sign(&gw, &c.campaign_id, 1, "00".repeat(32), vec![rs[2].clone()]).unwrap());
        rewritten.push(batches[2].clone());
        assert_eq!(v(&ok, &rewritten).unwrap_err(), ClaimError::BrokenChain { seq: 1 });
        // Starting after the settled point is a gap.
        let mut gap = ok.clone();
        gap.from_seq = 1;
        gap.sig = hex::encode(gw.sign(&gap.signing_bytes()).unwrap());
        assert_eq!(v(&gap, &batches).unwrap_err(), ClaimError::Gap { next_seq: 0 });
        // A head that is not the last batch.
        let mut head = ok.clone();
        head.head = batches[0].hash();
        head.sig = hex::encode(gw.sign(&head.signing_bytes()).unwrap());
        assert_eq!(v(&head, &batches).unwrap_err(), ClaimError::BrokenChain { seq: 2 });
        // An unbilled campaign.
        let free = sample(&adv, "free", 1).sign(&adv).unwrap();
        let fc = Claim { campaign_id: free.campaign_id.clone(), ..ok.clone() };
        assert_eq!(
            verify_claim(&free, &bids(&free), &Ledger::new(&free.campaign_id), &fc, &[], &HashMap::new(), "", NOW).unwrap_err(),
            ClaimError::BadSignature
        );
        let mut fc = fc;
        fc.sig = hex::encode(gw.sign(&fc.signing_bytes()).unwrap());
        assert_eq!(
            verify_claim(&free, &bids(&free), &Ledger::new(&free.campaign_id), &fc, &[], &HashMap::new(), "", NOW).unwrap_err(),
            ClaimError::NotBilled
        );
    }

    #[test]
    fn self_dealing_is_refused() {
        let adv = Keypair::generate_ed25519();
        let c = billed(&adv, 10 * MICROS);
        let rs = vec![imp(&c, "s0", "l0", MICROS, None)];
        let ledger = Ledger::new(&c.campaign_id);
        // The advertiser running the gateway that claims.
        let own = chain(&adv, rs.clone(), 10);
        let claim = Claim::build(&adv, &c, None, &own, u64::MAX, "SOL:gatewaywallet1", NOW).unwrap();
        assert_eq!(
            verify_claim(&c, &bids(&c), &ledger, &claim, &own, &HashMap::new(), "", NOW).unwrap_err(),
            ClaimError::SelfDealing
        );
        // A gateway paying out to the advertiser's own wallet.
        let gw = Keypair::generate_ed25519();
        let b = chain(&gw, rs, 10);
        let claim = Claim::build(&gw, &c, None, &b, u64::MAX, "SOL:advertiserwallet1", NOW).unwrap();
        assert_eq!(
            verify_claim(&c, &bids(&c), &ledger, &claim, &b, &HashMap::new(), "", NOW).unwrap_err(),
            ClaimError::SelfDealing
        );
        // A show owned by the advertiser: its impressions are not paid.
        let b = chain(&gw, vec![imp(&c, "s1", "l1", MICROS, Some("show1")), imp(&c, "s2", "l2", MICROS, None)], 10);
        let claim = Claim::build(&gw, &c, None, &b, u64::MAX, "SOL:gatewaywallet1", NOW).unwrap();
        let v = verify_claim(&c, &bids(&c), &ledger, &claim, &b, &shows(&c.owner), "", NOW).unwrap();
        assert_eq!(v.impressions, 1);
        assert_eq!(v.excluded[&Excluded::SelfDealing], 1);
        // A show with no payout address, and one nobody knows.
        let mut s = shows("someone");
        s.get_mut("show1").unwrap().payout = None;
        let v = verify_claim(&c, &bids(&c), &ledger, &claim, &b, &s, "", NOW).unwrap();
        assert_eq!(v.excluded[&Excluded::ShowHasNoPayout], 1);
        let v = verify_claim(&c, &bids(&c), &ledger, &claim, &b, &HashMap::new(), "", NOW).unwrap();
        assert_eq!(v.excluded[&Excluded::UnknownShow], 1);
    }

    #[test]
    fn caps_and_budget() {
        let adv = Keypair::generate_ed25519();
        let gw = Keypair::generate_ed25519();
        let mut c = sample(&adv, "capped", 1);
        c.freq_cap = Some(FreqCap { max: 2, per_secs: 3600 });
        c.budget = Some(Budget {
            total_micros: 10_000,
            daily_micros: Some(4_000),
            rail: "coinpay".into(),
            payer: None,
            funding_ref: None,
        });
        let c = c.sign(&adv).unwrap();
        // One listener 3 times (cap 2/hour), $1 CPM each = $0.001.
        let mut rs: Vec<Receipt> = (0..3).map(|i| imp(&c, &format!("f{i}"), "same", MICROS, None)).collect();
        // Five more listeners: the daily cap ($0.004) stops after two of them.
        rs.extend((0..5).map(|i| imp(&c, &format!("d{i}"), &format!("u{i}"), MICROS, None)));
        // Served outside the schedule? Not in this campaign; a receipt for a
        // creative it never had.
        let mut alien = imp(&c, "x", "ux", MICROS, None);
        alien.creative = "cd".repeat(32);
        rs.push(alien);
        // A price above the bid is clamped to the bid ($2).
        let mut greedy = imp(&c, "g", "ug", 50 * MICROS, None);
        greedy.ts = NOW - 86_400 * 2; // another day
        rs.push(greedy);
        // A receipt from the future.
        let mut future = imp(&c, "fut", "uf", MICROS, None);
        future.ts = NOW + 3600;
        rs.push(future);
        let b = chain(&gw, rs, 100);
        let claim = Claim::build(&gw, &c, None, &b, u64::MAX, "SOL:gatewaywallet1", NOW).unwrap();
        let ledger = Ledger::new(&c.campaign_id);
        let v = verify_claim(&c, &bids(&c), &ledger, &claim, &b, &HashMap::new(), "", NOW).unwrap();
        assert_eq!(v.excluded[&Excluded::FrequencyCap], 1);
        assert_eq!(v.excluded[&Excluded::DailyCap], 3);
        assert_eq!(v.excluded[&Excluded::NotInCampaign], 1);
        assert_eq!(v.excluded[&Excluded::FutureTimestamp], 1);
        assert_eq!(v.impressions, 5, "2 capped-listener + 2 more on day one + the greedy one");
        assert_eq!(v.gross_micros, 4_000 + 2_000, "greedy clamped to the $2 bid");
        // Against a $0.005 total budget the same claim is refused outright.
        let mut c2 = c.clone();
        c2.budget.as_mut().unwrap().total_micros = 5_000;
        assert!(matches!(
            verify_claim(&c2, &bids(&c2), &ledger, &claim, &b, &HashMap::new(), "", NOW),
            Err(ClaimError::OverBudget { remaining_micros: 5_000, asked_micros: 6_000 })
        ));
        // The gateway's own builder stops before the budget runs out.
        let small = Claim::build(&gw, &c, None, &chain(&gw, (0..8).map(|i| imp(&c, &format!("z{i}"), &format!("z{i}"), MICROS, None)).collect(), 1), 5_000, "SOL:gatewaywallet1", NOW).unwrap();
        assert_eq!(small.to_seq, 4, "five $0.001 batches fit $0.005");
    }

    #[test]
    fn bid_history_and_schedule() {
        let bids = BTreeMap::from([(10u64, 2_000_000u64), (20, 1_000_000)]);
        assert_eq!(bid_at(&bids, 15), Some(2_000_000));
        assert_eq!(bid_at(&bids, 25), Some(1_000_000));
        assert_eq!(bid_at(&bids, 5), Some(2_000_000), "older than anything known: the oldest");
        assert_eq!(bid_at(&BTreeMap::new(), 5), None);
        let s = Schedule { start: Some(10), end: Some(20) };
        assert!(s.contains(10) && !s.contains(20));
    }

    #[test]
    fn payouts_budget_and_rail_args() {
        assert!(valid_payout("SOL:9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin"));
        assert!(valid_payout("BCH:bitcoincash:qpm2qsznhks23z7629mms6s4cwef74vcwvy22gdx6a"));
        assert!(valid_payout("USDC_POL:0x52908400098527886E0F7030069857D2E4169EE7"));
        for bad in ["", "SOL", "sol:abcdefgh", "SOL:short", "SOL:has space x", ":abcdefghij"] {
            assert!(!valid_payout(bad), "{bad:?}");
        }
        let b = Budget {
            total_micros: 1,
            daily_micros: None,
            rail: "coinpay".into(),
            payer: Some("nope".into()),
            funding_ref: None,
        };
        assert!(b.check().is_err());
        assert_eq!(usd_decimal(1_234_500), "1.2345");
        assert_eq!(usd_decimal(5 * MICROS), "5");
        assert_eq!(usd_decimal(1), "0.000001");
        let rail = CoinpayCliRail {
            bin: "coinpay".into(),
            wallet_file: None,
        };
        assert_eq!(
            rail.args("SOL:9xQeWvG816bUx9", 2_500_000).unwrap(),
            ["wallet", "send", "--chain", "SOL", "--to", "9xQeWvG816bUx9", "--amount-fiat", "2.5", "--fiat", "USD"]
        );
        assert!(rail.args("not an address", 1).is_err());
        assert!(DisabledRail.pay(None, "SOL:9xQeWvG816bUx9", 1, "k").is_err());
        let fake = FakeRail::default();
        fake.fund("a", 10);
        assert!(fake.pay(Some("a"), "b", 11, "k1").is_err(), "overdraft refused");
        assert_eq!(fake.pay(Some("a"), "b", 10, "k2").unwrap(), "fake-0");
        assert_eq!(fake.pay(Some("a"), "b", 10, "k2").unwrap(), "fake-0", "idempotent");
        assert_eq!(fake.balance("b"), 10);
    }
}
