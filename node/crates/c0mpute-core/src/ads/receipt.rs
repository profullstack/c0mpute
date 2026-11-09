//! Impression receipts: what a gateway says it served, signed by that
//! gateway, in append-only per-campaign chains, so an advertiser can count
//! delivery from the gateways themselves with no ad server in between.
//!
//! * A gateway records one [`Receipt`] per event (impression, VAST quartile,
//!   click) and, every flush, signs a [`ReceiptBatch`] for each campaign that
//!   had events: `seq` counts up from 0 and `prev` is the hash of the batch
//!   before, so a gateway cannot quietly rewrite or drop history, and a
//!   reader can see gaps.
//! * Batches are kept in a JSONL log on the gateway, served at
//!   `/ads/v1/receipts`, and re-gossiped when an advertiser asks
//!   (`ReportWant`).
//! * [`Report::build`] (the advertiser's CLI) verifies every batch, checks
//!   each chain, dedupes events by serving id, and counts.
//!
//! What this does not do: prove a human heard or saw the ad. A receipt is a
//! gateway's signed claim; the chain makes the claim permanent and
//! attributable, which is what settlement will need later.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use c0mpute_proto::Hash;
use serde::{Deserialize, Serialize};

use super::campaign::Placement;

const SIGNING_DOMAIN: &str = "c0mpute-ads-receipts-v1";
/// Receipts per batch: keeps one batch inside a gossip message.
pub const MAX_RECEIPTS_PER_BATCH: usize = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    Impression,
    Start,
    FirstQuartile,
    Midpoint,
    ThirdQuartile,
    Complete,
    Click,
}

impl Event {
    /// From a VAST tracking event name or our own snake_case name.
    pub fn parse(s: &str) -> Option<Event> {
        Some(match s {
            "impression" => Event::Impression,
            "start" => Event::Start,
            "firstQuartile" | "first_quartile" => Event::FirstQuartile,
            "midpoint" => Event::Midpoint,
            "thirdQuartile" | "third_quartile" => Event::ThirdQuartile,
            "complete" => Event::Complete,
            "click" => Event::Click,
            _ => return None,
        })
    }

    pub fn vast_name(&self) -> &'static str {
        match self {
            Event::Impression => "impression",
            Event::Start => "start",
            Event::FirstQuartile => "firstQuartile",
            Event::Midpoint => "midpoint",
            Event::ThirdQuartile => "thirdQuartile",
            Event::Complete => "complete",
            Event::Click => "click",
        }
    }
}

/// One served event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub campaign_id: String,
    /// Creative hash.
    pub creative: String,
    pub placement: Placement,
    pub event: Event,
    /// Unix seconds.
    pub ts: i64,
    /// Anonymous listener id (hash), "" when unknown.
    #[serde(default)]
    pub listener: String,
    /// One ad play: every event of the same play shares it.
    pub serving_id: String,
    /// Publisher hosting site (podcast DAI), when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
}

/// A gateway's signed, chained batch of receipts for one campaign.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptBatch {
    pub v: u8,
    /// Gateway's ed25519 public key, hex.
    pub gateway: String,
    pub campaign_id: String,
    pub seq: u64,
    /// [`ReceiptBatch::hash`] of batch `seq - 1`; "" for seq 0.
    pub prev: String,
    pub receipts: Vec<Receipt>,
    pub sig: String,
}

impl ReceiptBatch {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.sig = String::new();
        let mut out = format!("{SIGNING_DOMAIN}\n").into_bytes();
        out.extend(serde_json::to_vec(&unsigned).expect("batch serializes"));
        out
    }

    /// What the next batch's `prev` names.
    pub fn hash(&self) -> String {
        Hash::of(&self.signing_bytes()).to_hex()
    }

    pub fn sign(
        keypair: &c0mpute_net::libp2p::identity::Keypair,
        campaign_id: &str,
        seq: u64,
        prev: String,
        receipts: Vec<Receipt>,
    ) -> Result<Self> {
        let gateway = crate::hosting::site::owner_hex(keypair).context("receipts need an ed25519 identity")?;
        let mut b = ReceiptBatch {
            v: 1,
            gateway,
            campaign_id: campaign_id.to_string(),
            seq,
            prev,
            receipts,
            sig: String::new(),
        };
        b.check_fields()?;
        b.sig = hex::encode(
            keypair
                .sign(&b.signing_bytes())
                .map_err(|e| anyhow::anyhow!("sign receipts: {e}"))?,
        );
        Ok(b)
    }

    fn check_fields(&self) -> Result<()> {
        if self.v != 1 {
            bail!("unsupported receipt batch version");
        }
        if self.receipts.is_empty() || self.receipts.len() > MAX_RECEIPTS_PER_BATCH {
            bail!("a batch carries 1-{MAX_RECEIPTS_PER_BATCH} receipts");
        }
        if self.receipts.iter().any(|r| r.campaign_id != self.campaign_id) {
            bail!("a batch is for one campaign");
        }
        if (self.seq == 0) != self.prev.is_empty() {
            bail!("only the first batch has no prev");
        }
        Ok(())
    }

    pub fn verify(&self) -> bool {
        self.check_fields().is_ok() && crate::ads::verify_sig(&self.gateway, &self.signing_bytes(), &self.sig)
    }
}

/// A gateway's own receipt log: pending receipts, the chain heads, and the
/// batches already signed (JSONL per campaign under `dir`).
pub struct ReceiptLog {
    dir: Option<PathBuf>,
    pending: BTreeMap<String, Vec<Receipt>>,
    heads: HashMap<String, (u64, String)>,
    batches: HashMap<String, Vec<ReceiptBatch>>,
}

/// Batches kept in memory per campaign (older ones stay in the file).
const BATCHES_IN_MEMORY: usize = 2_000;

impl ReceiptLog {
    pub fn open(dir: Option<PathBuf>) -> Self {
        let mut log = ReceiptLog {
            dir,
            pending: BTreeMap::new(),
            heads: HashMap::new(),
            batches: HashMap::new(),
        };
        if let Some(d) = log.dir.clone()
            && let Ok(rd) = std::fs::read_dir(&d)
        {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                    continue;
                }
                let Some(cid) = p.file_stem().and_then(|s| s.to_str()).map(String::from) else {
                    continue;
                };
                let batches = read_jsonl(&p);
                if let Some(last) = batches.last() {
                    log.heads.insert(cid.clone(), (last.seq + 1, last.hash()));
                }
                let skip = batches.len().saturating_sub(BATCHES_IN_MEMORY);
                log.batches.insert(cid, batches.into_iter().skip(skip).collect());
            }
        }
        log
    }

    pub fn add(&mut self, r: Receipt) {
        self.pending.entry(r.campaign_id.clone()).or_default().push(r);
    }

    pub fn pending_len(&self) -> usize {
        self.pending.values().map(Vec::len).sum()
    }

    /// Sign everything pending into batches, append them to the log, and
    /// return them (to gossip).
    pub fn flush(&mut self, keypair: &c0mpute_net::libp2p::identity::Keypair) -> Vec<ReceiptBatch> {
        let pending = std::mem::take(&mut self.pending);
        let mut out = Vec::new();
        for (cid, receipts) in pending {
            for chunk in receipts.chunks(MAX_RECEIPTS_PER_BATCH) {
                let (seq, prev) = self.heads.get(&cid).cloned().unwrap_or((0, String::new()));
                let b = match ReceiptBatch::sign(keypair, &cid, seq, prev, chunk.to_vec()) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(err = %e, "ads: could not sign receipts");
                        continue;
                    }
                };
                self.heads.insert(cid.clone(), (seq + 1, b.hash()));
                if let Some(d) = &self.dir {
                    let _ = std::fs::create_dir_all(d);
                    if let Ok(mut f) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(d.join(format!("{cid}.jsonl")))
                    {
                        let _ = writeln!(f, "{}", serde_json::to_string(&b).unwrap_or_default());
                    }
                }
                let v = self.batches.entry(cid.clone()).or_default();
                v.push(b.clone());
                if v.len() > BATCHES_IN_MEMORY {
                    v.drain(..v.len() - BATCHES_IN_MEMORY);
                }
                out.push(b);
            }
        }
        out
    }

    /// Batches for a campaign with `seq >= since` (from the file when they
    /// have aged out of memory).
    pub fn batches(&self, campaign_id: &str, since: u64) -> Vec<ReceiptBatch> {
        let mem = self.batches.get(campaign_id).cloned().unwrap_or_default();
        if mem.first().is_none_or(|b| b.seq <= since) {
            return mem.into_iter().filter(|b| b.seq >= since).collect();
        }
        match &self.dir {
            Some(d) => read_jsonl(&d.join(format!("{campaign_id}.jsonl")))
                .into_iter()
                .filter(|b| b.seq >= since)
                .collect(),
            None => mem.into_iter().filter(|b| b.seq >= since).collect(),
        }
    }

    pub fn campaigns(&self) -> Vec<String> {
        self.heads.keys().cloned().collect()
    }
}

fn read_jsonl(p: &Path) -> Vec<ReceiptBatch> {
    std::fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<ReceiptBatch>(l).ok())
        .collect()
}

/// The advertiser's count for one campaign.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Report {
    pub campaign_id: String,
    /// Event -> count (deduped by serving id).
    pub events: BTreeMap<String, u64>,
    pub by_placement: BTreeMap<String, u64>,
    /// Impressions per gateway key.
    pub by_gateway: BTreeMap<String, u64>,
    /// Impressions per publisher site.
    pub by_site: BTreeMap<String, u64>,
    pub unique_listeners: u64,
    pub batches: u64,
    /// Batches dropped: bad signature, wrong campaign, or a broken chain.
    pub rejected_batches: u64,
    /// (gateway, first missing seq) where a chain has a hole.
    pub gaps: Vec<(String, u64)>,
}

impl Report {
    pub fn build<'a, I: IntoIterator<Item = &'a ReceiptBatch>>(campaign_id: &str, batches: I) -> Report {
        let mut r = Report {
            campaign_id: campaign_id.to_string(),
            ..Default::default()
        };
        // Verify, dedupe by (gateway, seq), group per gateway.
        let mut chains: BTreeMap<String, BTreeMap<u64, &ReceiptBatch>> = BTreeMap::new();
        for b in batches {
            if b.campaign_id != campaign_id || !b.verify() {
                r.rejected_batches += 1;
                continue;
            }
            chains.entry(b.gateway.clone()).or_default().entry(b.seq).or_insert(b);
        }
        let mut seen_events: HashSet<(String, Event)> = HashSet::new();
        let mut listeners: HashSet<String> = HashSet::new();
        for (gw, chain) in &chains {
            // Each batch must link to the one before it when we have both.
            let mut expect = 0u64;
            let mut prev_hash: Option<String> = None;
            for (seq, b) in chain {
                if *seq != expect {
                    r.gaps.push((gw.clone(), expect));
                    prev_hash = None;
                }
                if let Some(p) = &prev_hash
                    && *p != b.prev
                {
                    r.rejected_batches += 1;
                    prev_hash = None;
                    expect = seq + 1;
                    continue;
                }
                prev_hash = Some(b.hash());
                expect = seq + 1;
                r.batches += 1;
                for rc in &b.receipts {
                    if !seen_events.insert((rc.serving_id.clone(), rc.event)) {
                        continue;
                    }
                    *r.events.entry(rc.event.vast_name().to_string()).or_default() += 1;
                    if rc.event == Event::Impression {
                        *r.by_placement.entry(rc.placement.as_str().to_string()).or_default() += 1;
                        *r.by_gateway.entry(gw.clone()).or_default() += 1;
                        if let Some(s) = &rc.site {
                            *r.by_site.entry(s.clone()).or_default() += 1;
                        }
                        if !rc.listener.is_empty() {
                            listeners.insert(rc.listener.clone());
                        }
                    }
                }
            }
        }
        r.unique_listeners = listeners.len() as u64;
        r
    }

    pub fn impressions(&self) -> u64 {
        self.events.get("impression").copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use c0mpute_net::libp2p::identity::Keypair;

    fn receipt(cid: &str, ev: Event, serving: &str, listener: &str) -> Receipt {
        Receipt {
            campaign_id: cid.into(),
            creative: "ab".repeat(32),
            placement: Placement::PodcastPre,
            event: ev,
            ts: 1,
            listener: listener.into(),
            serving_id: serving.into(),
            site: Some("site1".into()),
            country: None,
        }
    }

    #[test]
    fn batches_sign_chain_and_aggregate() {
        let gw = Keypair::generate_ed25519();
        let tmp = tempfile::tempdir().unwrap();
        let mut log = ReceiptLog::open(Some(tmp.path().to_path_buf()));
        log.add(receipt("c1", Event::Impression, "s1", "l1"));
        log.add(receipt("c1", Event::Impression, "s2", "l1"));
        log.add(receipt("c2", Event::Impression, "s3", "l2"));
        let first = log.flush(&gw);
        assert_eq!(first.len(), 2, "one batch per campaign");
        assert!(first.iter().all(|b| b.verify() && b.seq == 0 && b.prev.is_empty()));
        // A duplicate event (same serving id) and a quartile.
        log.add(receipt("c1", Event::Impression, "s1", "l1"));
        log.add(receipt("c1", Event::Complete, "s1", "l1"));
        let second = log.flush(&gw);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].seq, 1);
        assert_eq!(second[0].prev, first.iter().find(|b| b.campaign_id == "c1").unwrap().hash());

        // The log survives a restart: the chain continues.
        let mut again = ReceiptLog::open(Some(tmp.path().to_path_buf()));
        assert_eq!(again.batches("c1", 0).len(), 2);
        again.add(receipt("c1", Event::Click, "s2", "l1"));
        let third = again.flush(&gw);
        assert_eq!(third[0].seq, 2);
        assert_eq!(third[0].prev, second[0].hash());

        let all = again.batches("c1", 0);
        let rep = Report::build("c1", &all);
        assert_eq!(rep.impressions(), 2, "s1 counted once");
        assert_eq!(rep.events["complete"], 1);
        assert_eq!(rep.events["click"], 1);
        assert_eq!(rep.unique_listeners, 1);
        assert_eq!(rep.by_site["site1"], 2);
        assert_eq!(rep.batches, 3);
        assert_eq!(rep.rejected_batches, 0);
        assert!(rep.gaps.is_empty());

        // A second gateway's batches add up; a missing middle batch is a gap.
        let gw2 = Keypair::generate_ed25519();
        let mut log2 = ReceiptLog::open(None);
        for s in ["t1", "t2", "t3"] {
            log2.add(receipt("c1", Event::Impression, s, s));
            log2.flush(&gw2);
        }
        let mut b2 = log2.batches("c1", 0);
        b2.remove(1);
        let mut combined = all.clone();
        combined.extend(b2);
        let rep = Report::build("c1", &combined);
        assert_eq!(rep.impressions(), 4);
        assert_eq!(rep.by_gateway.len(), 2);
        assert_eq!(rep.gaps.len(), 1);
        assert_eq!(rep.gaps[0].1, 1);
    }

    #[test]
    fn tampered_batches_are_rejected() {
        let gw = Keypair::generate_ed25519();
        let mut log = ReceiptLog::open(None);
        log.add(receipt("c1", Event::Impression, "s1", "l1"));
        let mut b = log.flush(&gw).remove(0);
        assert!(b.verify());
        b.receipts.push(receipt("c1", Event::Impression, "fake", "l9"));
        assert!(!b.verify(), "inflated batch");
        let rep = Report::build("c1", [&b]);
        assert_eq!(rep.impressions(), 0);
        assert_eq!(rep.rejected_batches, 1);
        // A batch for another campaign does not count toward this one.
        let mut log = ReceiptLog::open(None);
        log.add(receipt("c2", Event::Impression, "s1", "l1"));
        let other = log.flush(&gw).remove(0);
        assert_eq!(Report::build("c1", [&other]).rejected_batches, 1);
        // A rewritten history (same seq, different prev) breaks the chain.
        let mut log = ReceiptLog::open(None);
        log.add(receipt("c3", Event::Impression, "a", "l"));
        let b0 = log.flush(&gw).remove(0);
        let forged = ReceiptBatch::sign(&gw, "c3", 1, "00".repeat(32), vec![receipt("c3", Event::Impression, "b", "l")]).unwrap();
        let rep = Report::build("c3", [&b0, &forged]);
        assert_eq!(rep.impressions(), 1);
        assert_eq!(rep.rejected_batches, 1);
    }

    #[test]
    fn large_flushes_split_and_fit_gossip() {
        let gw = Keypair::generate_ed25519();
        let mut log = ReceiptLog::open(None);
        for i in 0..250 {
            log.add(receipt("c1", Event::Impression, &format!("serving-{i:032}"), &format!("{i:016}")));
        }
        let out = log.flush(&gw);
        assert_eq!(out.len(), 3);
        for b in &out {
            assert!(serde_json::to_vec(b).unwrap().len() < 60 * 1024);
        }
        assert_eq!(Report::build("c1", &out).impressions(), 250);
    }
}
