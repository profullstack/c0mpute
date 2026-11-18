//! What the status aggregator tells an autoscaler (`GET /autoscale`).
//!
//! An operator who rents a GPU node by the hour wants it stopped while
//! nobody needs it and started again as soon as somebody does. Both halves
//! are visible on the gossip mesh the aggregator already listens to:
//!
//! - **demand**: every buyer publishes a `JobOffer` per auction, naming the
//!   model it wants (in the plaintext `public_spec` of an end-to-end offer, the
//!   inline spec of an open one, or a `...:model:<id>` tag). A requeued job is
//!   auctioned again, so a job that keeps finding no taker keeps showing up.
//! - **the node's own activity**: the accepts naming it as the winner and the
//!   receipts it publishes.
//!
//! Only counts and timestamps leave this module: no prompts (there are none in
//! an offer), no buyers, no job ids.

use std::collections::{HashMap, VecDeque};

use c0mpute_net::topics::JobOffer;
use serde::Serialize;

/// Offers remembered (each auction is one); anyone can gossip an offer, so
/// the table is bounded.
pub const MAX_OFFERS: usize = 50_000;
/// Longest window a caller can ask about.
pub const MAX_WINDOW_MS: u64 = 24 * 60 * 60 * 1000;
/// "Right now": offers this recent count as current demand.
pub const RECENT_MS: u64 = 2 * 60 * 1000;
/// An offer older than this with no accept found no taker in its bid window.
pub const NO_TAKER_AFTER_MS: u64 = 10 * 1000;
/// Workers whose last accept/receipt we remember.
pub const MAX_WORKERS: usize = 20_000;
/// Models named per caller request.
pub const MAX_MODELS: usize = 16;

#[derive(Debug, Clone)]
struct OfferSeen {
    job_id: String,
    external_ref: Option<String>,
    at_ms: u64,
    model: Option<String>,
    accepted: bool,
}

/// The model an offer asks for: `public_spec.model` (end-to-end offers), else
/// the inline spec's `model`, else a `:model:<id>` capability tag.
pub fn offer_model(offer: &JobOffer) -> Option<String> {
    let from_spec = |v: &serde_json::Value| v.get("model").and_then(|m| m.as_str()).map(str::to_string);
    offer
        .e2e
        .as_ref()
        .and_then(|e| from_spec(&e.public_spec))
        .or_else(|| offer.spec_inline.as_ref().and_then(from_spec))
        .or_else(|| {
            offer
                .required_capabilities
                .iter()
                .find_map(|t| t.split(":model:").nth(1).map(str::to_string))
        })
        .filter(|m| !m.is_empty() && m.len() <= 200)
}

/// Offers seen and per-worker activity. Not thread-safe by itself; the
/// aggregator keeps it behind a lock.
#[derive(Default)]
pub struct Demand {
    offers: VecDeque<OfferSeen>,
    /// worker peer id -> (last accept naming it, last receipt from it), ms.
    workers: HashMap<String, (Option<u64>, Option<u64>)>,
}

#[derive(Serialize, Debug, PartialEq, Default, Clone)]
pub struct ModelDemand {
    /// Auctions for this model in the window.
    pub offers_window: u64,
    /// Auctions in the last two minutes.
    pub offers_2m: u64,
    /// Auctions in the last two minutes that found no taker yet.
    pub waiting: u64,
    /// Auctions in the window that never found a taker (the job waited,
    /// was requeued, went to a slower node later, or gave up).
    pub unserved_window: u64,
    pub last_offer_ms: Option<u64>,
}

#[derive(Serialize, Debug, PartialEq, Default, Clone)]
pub struct NodeActivity {
    /// The board id (last 8 characters of the peer id).
    pub id: String,
    pub peer_id: Option<String>,
    pub online: bool,
    pub last_seen_s: Option<u64>,
    /// Jobs accepted for this node with no receipt yet.
    pub in_flight: u64,
    pub last_accept_ms: Option<u64>,
    pub last_receipt_ms: Option<u64>,
    /// Models the node's capability ad says it serves (`...:model:<id>` tags).
    pub serves: Vec<String>,
}

impl Demand {
    pub fn on_offer(&mut self, offer: &JobOffer, now: u64) {
        if self.offers.len() >= MAX_OFFERS {
            self.offers.pop_front();
        }
        // Filed at our clock: a buyer's clock can be anywhere.
        let at_ms = offer.published_at_ms.min(now).max(now.saturating_sub(60_000));
        self.offers.push_back(OfferSeen {
            job_id: offer.job_id.clone(),
            external_ref: offer.external_ref.clone(),
            at_ms,
            model: offer_model(offer),
            accepted: false,
        });
    }

    /// An accept: the offer found a taker, and the winner is busy.
    pub fn on_accept(&mut self, job_id: &str, worker: &str, now: u64) {
        // Accepts follow their offer within seconds: look at the recent end.
        for o in self.offers.iter_mut().rev().take(5_000) {
            if o.job_id == job_id {
                o.accepted = true;
                break;
            }
        }
        self.touch(worker, now, true);
    }

    pub fn on_receipt(&mut self, worker: &str, now: u64) {
        self.touch(worker, now, false);
    }

    fn touch(&mut self, worker: &str, now: u64, accept: bool) {
        if worker.is_empty() || worker.len() > 128 {
            return;
        }
        if !self.workers.contains_key(worker) && self.workers.len() >= MAX_WORKERS {
            // Forget the quietest worker to make room.
            if let Some(oldest) = self
                .workers
                .iter()
                .min_by_key(|(_, (a, r))| a.unwrap_or(0).max(r.unwrap_or(0)))
                .map(|(k, _)| k.clone())
            {
                self.workers.remove(&oldest);
            }
        }
        let e = self.workers.entry(worker.to_string()).or_default();
        if accept {
            e.0 = Some(now);
        } else {
            e.1 = Some(now);
        }
    }

    pub fn evict(&mut self, now: u64) {
        let cutoff = now.saturating_sub(MAX_WINDOW_MS);
        while self.offers.front().is_some_and(|o| o.at_ms < cutoff) {
            self.offers.pop_front();
        }
    }

    /// Demand for each of `models` over the last `window_ms`.
    pub fn models(&self, models: &[String], window_ms: u64, now: u64) -> HashMap<String, ModelDemand> {
        let since = now.saturating_sub(window_ms);
        let recent = now.saturating_sub(RECENT_MS);
        // A job re-auctioned under a new job id keeps its external ref: once
        // any of its auctions was accepted, it found a taker.
        let served: std::collections::HashSet<&str> = self
            .offers
            .iter()
            .filter(|o| o.accepted && o.at_ms >= since)
            .filter_map(|o| o.external_ref.as_deref())
            .collect();
        let mut out: HashMap<String, ModelDemand> = models.iter().map(|m| (m.clone(), ModelDemand::default())).collect();
        for o in self.offers.iter().rev() {
            if o.at_ms < since {
                break;
            }
            let Some(m) = o.model.as_ref() else { continue };
            let Some(d) = out.get_mut(m) else { continue };
            let taken = o.accepted || o.external_ref.as_deref().is_some_and(|r| served.contains(r));
            d.offers_window += 1;
            d.last_offer_ms = Some(d.last_offer_ms.unwrap_or(0).max(o.at_ms));
            let aged = now.saturating_sub(o.at_ms) >= NO_TAKER_AFTER_MS;
            if o.at_ms >= recent {
                d.offers_2m += 1;
                if aged && !taken {
                    d.waiting += 1;
                }
            }
            if aged && !taken {
                d.unserved_window += 1;
            }
        }
        out
    }

    /// Last accept/receipt for the worker whose peer id ends with `id` (a full
    /// peer id or the 8-character board id).
    pub fn worker(&self, id: &str) -> Option<(String, Option<u64>, Option<u64>)> {
        self.workers
            .iter()
            .filter(|(p, _)| p.as_str() == id || (id.len() >= 8 && p.ends_with(id)))
            .max_by_key(|(_, (a, r))| a.unwrap_or(0).max(r.unwrap_or(0)))
            .map(|(p, (a, r))| (p.clone(), *a, *r))
    }
}

/// Parse `models=a,b` (at most [`MAX_MODELS`], each 1..=200 characters).
pub fn parse_models(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|m| !m.is_empty() && m.len() <= 200)
        .take(MAX_MODELS)
        .map(str::to_string)
        .collect()
}

/// `window_s` in ms, default 20 min, at most a day, at least a minute.
pub fn parse_window_ms(raw: Option<&str>) -> u64 {
    raw.and_then(|v| v.parse::<u64>().ok())
        .map(|s| s.saturating_mul(1000))
        .unwrap_or(20 * 60 * 1000)
        .clamp(60_000, MAX_WINDOW_MS)
}

/// A node id the caller may pass: a peer id or the 8-character board id,
/// base58 characters only.
pub fn valid_node_id(id: &str) -> bool {
    (8..=128).contains(&id.len()) && id.chars().all(|c| c.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn offer(job: &str, model: &str, at: u64, ext: Option<&str>) -> JobOffer {
        serde_json::from_value(json!({
            "job_id": job,
            "workload_type": "infernet.inference",
            "buyer_peer_id": "buyer",
            "buyer_did": null,
            "spec_hash": "h",
            "spec_inline": {"model": model},
            "required_capabilities": ["infernet:inference"],
            "max_price_usd": 0.01,
            "deadline_unix_ms": at + 60_000,
            "published_at_ms": at,
            "external_ref": ext,
        }))
        .unwrap()
    }

    #[test]
    fn model_from_e2e_public_spec_inline_or_tag() {
        let mut o = offer("j", "a:9b", 0, None);
        assert_eq!(offer_model(&o).as_deref(), Some("a:9b"));
        o.spec_inline = None;
        o.required_capabilities = vec!["infernet:inference".into(), "infernet:inference:model:b:1b".into()];
        assert_eq!(offer_model(&o).as_deref(), Some("b:1b"));
        o.e2e = Some(serde_json::from_value(json!({"v": 1, "buyer_key": "k", "public_spec": {"model": "c:4b"}})).unwrap());
        assert_eq!(offer_model(&o).as_deref(), Some("c:4b"));
    }

    #[test]
    fn waiting_and_unserved_count_offers_without_a_taker() {
        let now = 10_000_000;
        let mut d = Demand::default();
        let m = vec!["m:9b".to_string(), "other".to_string()];
        // Taken at once.
        d.on_offer(&offer("j1", "m:9b", now - 60_000, None), now - 60_000);
        d.on_accept("j1", "12D3KooWxxxxxxxxsdNT3kCN", now - 59_000);
        // No taker yet, 30 s old: waiting.
        d.on_offer(&offer("j2", "m:9b", now - 30_000, Some("r2")), now - 30_000);
        // Just published: not waiting yet (still in its bid window).
        d.on_offer(&offer("j3", "m:9b", now - 2_000, None), now - 2_000);
        // Requeued job: first auction found nobody, second was taken.
        d.on_offer(&offer("j4", "m:9b", now - 15 * 60_000, Some("r4")), now - 15 * 60_000);
        d.on_offer(&offer("j5", "m:9b", now - 14 * 60_000, Some("r4")), now - 14 * 60_000);
        d.on_accept("j5", "other-worker", now);
        // Outside a 20 min window.
        d.on_offer(&offer("j6", "m:9b", now - 30 * 60_000, None), now - 30 * 60_000);
        let got = d.models(&m, 20 * 60_000, now);
        let g = &got["m:9b"];
        assert_eq!(g.offers_window, 5);
        assert_eq!(g.offers_2m, 3);
        assert_eq!(g.waiting, 1);
        assert_eq!(g.unserved_window, 1);
        assert_eq!(g.last_offer_ms, Some(now - 2_000));
        assert_eq!(got["other"], ModelDemand::default());
    }

    #[test]
    fn worker_activity_by_board_id() {
        let mut d = Demand::default();
        d.on_accept("x", "12D3KooWxxxxxxxxsdNT3kCN", 5);
        d.on_receipt("12D3KooWxxxxxxxxsdNT3kCN", 9);
        let (p, a, r) = d.worker("sdNT3kCN").unwrap();
        assert_eq!(p, "12D3KooWxxxxxxxxsdNT3kCN");
        assert_eq!((a, r), (Some(5), Some(9)));
        assert!(d.worker("nope1234").is_none());
    }

    #[test]
    fn query_parsing_is_bounded() {
        assert_eq!(parse_models(Some(" a , ,b")), vec!["a", "b"]);
        assert_eq!(parse_models(Some(&"m,".repeat(40))).len(), MAX_MODELS);
        assert_eq!(parse_window_ms(None), 1_200_000);
        assert_eq!(parse_window_ms(Some("5")), 60_000);
        assert_eq!(parse_window_ms(Some("999999999")), MAX_WINDOW_MS);
        assert!(valid_node_id("sdNT3kCN"));
        assert!(!valid_node_id("sdNT"));
        assert!(!valid_node_id("sdNT3kCN;rm"));
    }

    #[test]
    fn bounded_offer_table() {
        let mut d = Demand::default();
        for i in 0..(MAX_OFFERS + 10) {
            d.on_offer(&offer(&format!("j{i}"), "m", 1_000, None), 1_000);
        }
        assert_eq!(d.offers.len(), MAX_OFFERS);
    }
}
