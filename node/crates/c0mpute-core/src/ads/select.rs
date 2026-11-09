//! Picking an ad: a pure, deterministic function of the campaigns a node
//! knows, the request context and this gateway's frequency ledger.
//!
//! 1. Filters, each with a named reason: paused, outside its schedule, wrong
//!    placement, no creative for the placement, category / keyword / country
//!    mismatch, bid under the publisher's floor, frequency cap reached.
//! 2. The highest `max_cpm` wins. It pays the second price: the best losing
//!    bid or the publisher's floor, whichever is higher, never more than its
//!    own bid ([`super::billing::cleared_cpm`]). That price goes on the
//!    impression receipt and is what settlement pays.
//! 3. Equal bids: rendezvous hashing of (campaign id, slot key), so every
//!    gateway asked about the same slot gives the same answer, and different
//!    slots spread across tied campaigns.
//!
//! [`select`] returns the winner plus why every other campaign lost, so
//! `/ads/v1/select` and `c0mpute ads explain` can show the decision.

use std::collections::HashMap;

use c0mpute_proto::Hash;
use serde::Serialize;

use super::campaign::{CampaignRecord, Creative, FreqCap, Placement};

/// What a slot asks for.
#[derive(Clone, Debug, Default)]
pub struct AdContext {
    pub placement: Option<Placement>,
    /// The publisher's categories (lowercase).
    pub categories: Vec<String>,
    pub keywords: Vec<String>,
    /// ISO alpha-2, uppercase, when the gateway knows it.
    pub country: Option<String>,
    /// Stable, anonymous listener id, when there is one.
    pub listener: Option<String>,
    /// Unix seconds.
    pub now: i64,
    /// The publisher's floor, USD millionths per thousand.
    pub floor_micros: u64,
    /// Seeds the tie-break (e.g. site + episode + placement).
    pub slot_key: String,
    /// Campaigns already used in this response (one ad per campaign per break).
    pub exclude: Vec<String>,
    /// Longest creative the slot takes (podcast shows set this).
    pub max_duration_ms: Option<u64>,
    /// Campaigns whose budget this gateway knows is spent.
    pub exhausted: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reject {
    Paused,
    NotStarted,
    Ended,
    Placement,
    NoCreative,
    Category,
    Keyword,
    Geo,
    /// The campaign targets countries and the gateway does not know this one.
    GeoUnknown,
    BelowFloor,
    FrequencyCap,
    /// The campaign's budget is spent (settled plus this gateway's unsettled).
    BudgetExhausted,
    Excluded,
    /// Lost on price (or the tie-break) to the winner.
    Outbid,
}

#[derive(Clone, Debug, Serialize)]
pub struct Decision {
    pub campaign: CampaignRecord,
    pub creative: Creative,
    pub bid_micros: u64,
    /// What the winner pays (second price), USD millionths per thousand.
    pub cleared_cpm_micros: u64,
    /// The tie-break score (hex), for explanations.
    pub rank: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Explained {
    pub winner: Option<Decision>,
    /// (campaign id, name, reason) for every campaign that did not win.
    pub rejected: Vec<(String, String, Reject)>,
}

/// Impressions per (listener, campaign) this gateway has served.
#[derive(Default, Debug)]
pub struct FreqLedger {
    seen: HashMap<(String, String), Vec<i64>>,
}

/// Entries kept before the oldest listeners are forgotten.
const LEDGER_MAX: usize = 100_000;

impl FreqLedger {
    pub fn count(&self, listener: &str, campaign: &str, since: i64) -> usize {
        self.seen
            .get(&(listener.to_string(), campaign.to_string()))
            .map(|v| v.iter().filter(|t| **t >= since).count())
            .unwrap_or(0)
    }

    pub fn capped(&self, listener: &str, campaign: &str, cap: FreqCap, now: i64) -> bool {
        self.count(listener, campaign, now - cap.per_secs as i64) >= cap.max as usize
    }

    pub fn record(&mut self, listener: &str, campaign: &str, now: i64) {
        if self.seen.len() >= LEDGER_MAX {
            // Forget anything older than a day, then (if still full) everything.
            self.seen.retain(|_, v| {
                v.retain(|t| *t > now - 86_400);
                !v.is_empty()
            });
            if self.seen.len() >= LEDGER_MAX {
                self.seen.clear();
            }
        }
        let v = self.seen.entry((listener.to_string(), campaign.to_string())).or_default();
        v.push(now);
        if v.len() > 1000 {
            v.drain(..500);
        }
    }
}

fn rank(campaign_id: &str, slot_key: &str) -> String {
    Hash::of(format!("c0mpute-ads-rank\n{campaign_id}\n{slot_key}").as_bytes()).to_hex()
}

/// The creative a campaign runs in this placement: deterministic among
/// several by rendezvous on the slot key.
pub fn creative_for<'a>(
    c: &'a CampaignRecord,
    placement: Placement,
    slot_key: &str,
    max_duration_ms: Option<u64>,
) -> Option<&'a Creative> {
    c.creatives
        .iter()
        .filter(|cr| placement.accepts(cr.kind))
        .filter(|cr| max_duration_ms.is_none_or(|m| cr.duration_ms.unwrap_or(0) <= m))
        .max_by(|a, b| rank(&a.hash, slot_key).cmp(&rank(&b.hash, slot_key)))
}

fn why_not(c: &CampaignRecord, ctx: &AdContext, placement: Placement, freq: &FreqLedger) -> Option<Reject> {
    if ctx.exclude.contains(&c.campaign_id) {
        return Some(Reject::Excluded);
    }
    if c.paused {
        return Some(Reject::Paused);
    }
    if c.schedule.start.is_some_and(|s| ctx.now < s) {
        return Some(Reject::NotStarted);
    }
    if c.schedule.end.is_some_and(|e| ctx.now >= e) {
        return Some(Reject::Ended);
    }
    if !c.targeting.placements.contains(&placement) {
        return Some(Reject::Placement);
    }
    if creative_for(c, placement, &ctx.slot_key, ctx.max_duration_ms).is_none() {
        return Some(Reject::NoCreative);
    }
    let t = &c.targeting;
    if !t.categories.is_empty() && !t.categories.iter().any(|x| ctx.categories.contains(x)) {
        return Some(Reject::Category);
    }
    if !t.keywords.is_empty() && !t.keywords.iter().any(|x| ctx.keywords.contains(x)) {
        return Some(Reject::Keyword);
    }
    if !t.geo.is_empty() {
        match &ctx.country {
            None => return Some(Reject::GeoUnknown),
            Some(cc) if !t.geo.contains(cc) => return Some(Reject::Geo),
            _ => {}
        }
    }
    if ctx.exhausted.contains(&c.campaign_id) {
        return Some(Reject::BudgetExhausted);
    }
    if c.max_cpm_micros < ctx.floor_micros {
        return Some(Reject::BelowFloor);
    }
    if let (Some(cap), Some(l)) = (c.freq_cap, &ctx.listener)
        && freq.capped(l, &c.campaign_id, cap, ctx.now)
    {
        return Some(Reject::FrequencyCap);
    }
    None
}

/// Pick the campaign for `ctx`, explaining every loser.
pub fn select<'a, I>(campaigns: I, ctx: &AdContext, freq: &FreqLedger) -> Explained
where
    I: IntoIterator<Item = &'a CampaignRecord>,
{
    let mut rejected = Vec::new();
    let Some(placement) = ctx.placement else {
        return Explained { winner: None, rejected };
    };
    let mut eligible: Vec<(&CampaignRecord, String)> = Vec::new();
    for c in campaigns {
        match why_not(c, ctx, placement, freq) {
            Some(r) => rejected.push((c.campaign_id.clone(), c.name.clone(), r)),
            None => eligible.push((c, rank(&c.campaign_id, &ctx.slot_key))),
        }
    }
    // Highest bid, then highest rank; campaign id last so the order is total.
    eligible.sort_by(|(a, ra), (b, rb)| {
        b.max_cpm_micros
            .cmp(&a.max_cpm_micros)
            .then_with(|| rb.cmp(ra))
            .then_with(|| a.campaign_id.cmp(&b.campaign_id))
    });
    let runner_up = eligible.get(1).map(|(c, _)| c.max_cpm_micros);
    let mut it = eligible.into_iter();
    let winner = it.next().and_then(|(c, r)| {
        let creative = creative_for(c, placement, &ctx.slot_key, ctx.max_duration_ms)?.clone();
        Some(Decision {
            campaign: c.clone(),
            creative,
            bid_micros: c.max_cpm_micros,
            cleared_cpm_micros: super::billing::cleared_cpm(c.max_cpm_micros, runner_up, ctx.floor_micros),
            rank: r,
        })
    });
    for (c, _) in it {
        rejected.push((c.campaign_id.clone(), c.name.clone(), Reject::Outbid));
    }
    rejected.sort_by(|a, b| a.0.cmp(&b.0));
    Explained { winner, rejected }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ads::campaign::tests::sample;
    use crate::ads::campaign::{MICROS, Schedule};
    use c0mpute_net::libp2p::identity::Keypair;

    fn ctx(p: Placement) -> AdContext {
        AdContext {
            placement: Some(p),
            now: 1_000_000,
            slot_key: "site/ep1/pre".into(),
            ..Default::default()
        }
    }

    fn campaigns(n: usize, cpm: u64) -> Vec<CampaignRecord> {
        (0..n)
            .map(|i| {
                let kp = Keypair::generate_ed25519();
                let mut c = sample(&kp, &format!("c{i}"), 1);
                c.max_cpm_micros = cpm;
                c.sign(&kp).unwrap()
            })
            .collect()
    }

    #[test]
    fn highest_bid_wins_and_ties_are_deterministic() {
        let mut cs = campaigns(4, 2 * MICROS);
        let kp = Keypair::generate_ed25519();
        let mut rich = sample(&kp, "rich", 1);
        rich.max_cpm_micros = 5 * MICROS;
        let rich = rich.sign(&kp).unwrap();
        cs.push(rich.clone());
        let freq = FreqLedger::default();
        let e = select(&cs, &ctx(Placement::PodcastPre), &freq);
        let w = e.winner.unwrap();
        assert_eq!(w.campaign.campaign_id, rich.campaign_id);
        assert_eq!(w.creative.kind, crate::ads::campaign::CreativeKind::Audio);
        assert_eq!(e.rejected.len(), 4);
        assert!(e.rejected.iter().all(|r| r.2 == Reject::Outbid));
        // Second price: $5 beats four $2 bids and pays $2.
        assert_eq!(w.cleared_cpm_micros, 2 * MICROS);
        let mut floored = ctx(Placement::PodcastPre);
        floored.floor_micros = 3 * MICROS;
        let w = select(&cs, &floored, &freq).winner.unwrap();
        assert_eq!(w.cleared_cpm_micros, 3 * MICROS, "the floor beats the runner-up");
        assert_eq!(select([&rich], &ctx(Placement::PodcastPre), &freq).winner.unwrap().cleared_cpm_micros, 5 * MICROS);

        // Without the rich one: a tie among four, decided by rendezvous. The
        // same answer whatever order the campaigns arrive in.
        cs.pop();
        let a = select(&cs, &ctx(Placement::PodcastPre), &freq).winner.unwrap();
        let mut rev = cs.clone();
        rev.reverse();
        let b = select(&rev, &ctx(Placement::PodcastPre), &freq).winner.unwrap();
        assert_eq!(a.campaign.campaign_id, b.campaign.campaign_id);
        // Different slots spread over the tied campaigns.
        let winners: std::collections::HashSet<String> = (0..64)
            .map(|i| {
                let mut c = ctx(Placement::PodcastPre);
                c.slot_key = format!("slot-{i}");
                select(&cs, &c, &freq).winner.unwrap().campaign.campaign_id
            })
            .collect();
        assert!(winners.len() >= 3, "{winners:?}");
    }

    #[test]
    fn filters_name_their_reason() {
        let kp = Keypair::generate_ed25519();
        let freq = FreqLedger::default();
        let base = sample(&kp, "f", 1);
        let check = |c: CampaignRecord, cx: AdContext, want: Reject| {
            let c = c.sign(&kp).unwrap();
            let e = select([&c], &cx, &freq);
            assert!(e.winner.is_none(), "{want:?} should reject");
            assert_eq!(e.rejected[0].2, want);
        };

        let mut c = base.clone();
        c.paused = true;
        check(c, ctx(Placement::PodcastPre), Reject::Paused);

        let mut c = base.clone();
        c.schedule = Schedule { start: Some(2_000_000), end: None };
        check(c, ctx(Placement::PodcastPre), Reject::NotStarted);

        let mut c = base.clone();
        c.schedule = Schedule { start: None, end: Some(1_000_000) };
        check(c, ctx(Placement::PodcastPre), Reject::Ended);

        check(base.clone(), ctx(Placement::PodcastPost), Reject::Placement);

        let mut c = base.clone();
        c.targeting.categories = vec!["technology".into()];
        let mut cx = ctx(Placement::PodcastPre);
        cx.categories = vec!["comedy".into()];
        check(c.clone(), cx.clone(), Reject::Category);
        cx.categories.push("technology".into());
        assert!(select([&c.clone().sign(&kp).unwrap()], &cx, &freq).winner.is_some());

        let mut c = base.clone();
        c.targeting.keywords = vec!["rust".into()];
        check(c, ctx(Placement::PodcastPre), Reject::Keyword);

        let mut c = base.clone();
        c.targeting.geo = vec!["US".into(), "CA".into()];
        check(c.clone(), ctx(Placement::PodcastPre), Reject::GeoUnknown);
        let mut cx = ctx(Placement::PodcastPre);
        cx.country = Some("DE".into());
        check(c.clone(), cx.clone(), Reject::Geo);
        cx.country = Some("CA".into());
        assert!(select([&c.sign(&kp).unwrap()], &cx, &freq).winner.is_some());

        let mut cx = ctx(Placement::PodcastPre);
        cx.floor_micros = 3 * MICROS;
        check(base.clone(), cx, Reject::BelowFloor);

        let signed = base.clone().sign(&kp).unwrap();
        let mut cx = ctx(Placement::PodcastPre);
        cx.exclude = vec![signed.campaign_id.clone()];
        check(base.clone(), cx, Reject::Excluded);

        let mut cx = ctx(Placement::PodcastPre);
        cx.exhausted = vec![signed.campaign_id.clone()];
        check(base.clone(), cx, Reject::BudgetExhausted);

        // No placement: nothing selected.
        let mut cx = ctx(Placement::PodcastPre);
        cx.placement = None;
        assert!(select([&signed], &cx, &freq).winner.is_none());
    }

    #[test]
    fn frequency_cap_per_listener() {
        let kp = Keypair::generate_ed25519();
        let mut c = sample(&kp, "capped", 1);
        c.freq_cap = Some(FreqCap { max: 2, per_secs: 3600 });
        let c = c.sign(&kp).unwrap();
        let mut freq = FreqLedger::default();
        let mut cx = ctx(Placement::PodcastPre);
        cx.listener = Some("listener-a".into());
        for _ in 0..2 {
            assert!(select([&c], &cx, &freq).winner.is_some());
            freq.record("listener-a", &c.campaign_id, cx.now);
        }
        let e = select([&c], &cx, &freq);
        assert!(e.winner.is_none());
        assert_eq!(e.rejected[0].2, Reject::FrequencyCap);
        // Another listener is unaffected; so is the same one an hour later.
        cx.listener = Some("listener-b".into());
        assert!(select([&c], &cx, &freq).winner.is_some());
        cx.listener = Some("listener-a".into());
        cx.now += 3601;
        assert!(select([&c], &cx, &freq).winner.is_some());
        // Without a listener id the cap cannot apply.
        cx.now -= 3601;
        cx.listener = None;
        assert!(select([&c], &cx, &freq).winner.is_some());
    }

    #[test]
    fn ctv_gets_video_and_podcast_gets_audio() {
        let kp = Keypair::generate_ed25519();
        let c = sample(&kp, "both", 1).sign(&kp).unwrap();
        let freq = FreqLedger::default();
        let v = select([&c], &ctx(Placement::CtvPreroll), &freq).winner.unwrap();
        assert_eq!(v.creative.mime, "video/mp4");
        let a = select([&c], &ctx(Placement::PodcastPre), &freq).winner.unwrap();
        assert_eq!(a.creative.mime, "audio/mpeg");
        // A show that takes at most 3 s of ad cannot run this 5 s spot.
        let mut short = ctx(Placement::PodcastPre);
        short.max_duration_ms = Some(3000);
        assert_eq!(select([&c], &short, &freq).rejected[0].2, Reject::NoCreative);
    }
}
