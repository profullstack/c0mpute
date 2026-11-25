//! DIP-0036 step 4: a hosting worker's own content policy.
//!
//! A node with the hosting role and a `[moderation]` table (the one its
//! gateway uses, see [`crate::moderation`]) does not hold a copy of a site
//! that has a file a subscribed labeler labelled with a `refuse` action:
//!
//! * a new copy is checked once its chunks are here and before it is held
//!   ([`Hosting::policy_admit`]): a refused one is never announced and its
//!   chunks are deleted;
//! * copies already held are checked again every `recheck_secs`
//!   ([`Hosting::policy_round`]), since a label can arrive after the copy
//!   was taken: a refused one is let go the way a surplus copy is (announce
//!   first, delete once peers have seen it);
//! * a refusal is lifted the same way, when the labels behind it are
//!   retracted or expire.
//!
//! Workers store blake3 chunks, and labels name a file's SHA-256, so the
//! worker puts each file back together from its local chunks and hashes it.
//! Hashes go to the labelers' `/query` in batches; with a scan token, web
//! files small enough are sent to `/scan` first, as the gateway does.
//!
//! The node says what it refuses on the hosting gossip
//! ([`super::HostingMsg::PolicyRefusal`]), with every holding announcement.
//! To the others a refusing peer is not a holder of that version, not a
//! candidate for it (the next ranked node takes the copy) and so not
//! challenged for it: a refusal is never a failed proof. Site owners see how
//! many replicas refused, and under which labeler and value, never which
//! peers ([`Hosting::refusals`]).
//!
//! Not covered: striped sites (over [`super::site::STRIPE_ABOVE`]), which a
//! worker holds in groups and so cannot put back into files; sites this
//! node's own key owns or pays for.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use c0mpute_proto::Hash;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{debug, info};

use super::replicas::RefusalTally;
use super::site::{FileEntry, Manifest, SiteRecord};
use super::{Hosting, HostingMsg, stripe};
use crate::moderation::{self, Gate, Hit};

/// Refusals one node announces (and keeps from any one peer): one gossip
/// message, well under gossipsub's 64 KiB.
pub const MAX_REFUSALS: usize = 128;
/// Refusals a node remembers itself. Past [`MAX_REFUSALS`] the most recent
/// are the ones announced.
const MAX_REFUSED: usize = 10_000;
/// Web files of one site sent to a labeler's `/scan` (the rest, and every
/// other file, are asked about by hash).
const SCANS_PER_SITE: usize = 64;
/// Held sites checked per round.
const CHECKS_PER_ROUND: usize = 16;

/// "I will not hold this version of this site: `labeler` labelled a file of
/// it `val`, and my policy refuses that." Gossiped by the refusing node
/// itself (gossip messages are signed by their source).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    pub site_id: String,
    /// Manifest (version) refused. A new version is judged afresh.
    pub manifest: String,
    /// DID of the labeler whose label caused it.
    pub labeler: String,
    /// The label value.
    pub val: String,
}

impl Refusal {
    /// Fields a peer may put in the map.
    pub fn sane(&self) -> bool {
        let printable =
            |s: &str, max: usize| !s.is_empty() && s.len() <= max && s.bytes().all(|b| (0x21..=0x7e).contains(&b));
        self.site_id.len() == 20
            && self.site_id.bytes().all(|b| b.is_ascii_hexdigit())
            && Hash::from_hex(&self.manifest).is_ok()
            && printable(&self.labeler, 128)
            && printable(&self.val, 64)
    }
}

/// Why [`Hosting::replicate`] did not hold a copy: not a failure to retry.
#[derive(Debug)]
pub struct PolicyRefused {
    pub labeler: String,
    pub val: String,
}

impl std::fmt::Display for PolicyRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "refused under this node's content policy: a file is labelled {} by {}",
            self.val, self.labeler
        )
    }
}

impl std::error::Error for PolicyRefused {}

struct Refused {
    refusal: Refusal,
    /// SHA-256 of the files the refusing labels name: what is asked about
    /// again to see whether the refusal still stands.
    flagged: Vec<[u8; 32]>,
    since: Instant,
    checked: Instant,
}

struct Checked {
    manifest: String,
    /// SHA-256 of every file, kept so a re-check does not read the site
    /// again. Empty when the site could not be read.
    digests: Arc<Vec<[u8; 32]>>,
    at: Instant,
}

/// What this node refuses, and what it has checked.
#[derive(Default)]
pub(super) struct State {
    refused: HashMap<String, Refused>,
    checked: HashMap<String, Checked>,
    /// A non-empty refusal list is out on the gossip (so an empty one must
    /// follow once it is lifted).
    announced: bool,
}

/// A site's files, hashed.
struct Hashed {
    digests: Vec<[u8; 32]>,
    /// (name for the labeler, bytes) of the files to send to `/scan`.
    scans: Vec<(&'static str, Vec<u8>)>,
}

/// The label this node refuses a site for, and the files it names.
async fn verdict(gate: &Arc<Gate>, digests: &[[u8; 32]], scans: &[(&'static str, Vec<u8>)]) -> Option<(Hit, Vec<[u8; 32]>)> {
    // Scanned files land in the gate's cache by SHA-256; the hash pass
    // below reads them from there.
    for (name, bytes) in scans {
        let _ = gate.decide_patiently(name, bytes).await;
    }
    let decisions = gate.decide_hashes(digests).await;
    let mut first: Option<Hit> = None;
    let mut flagged = Vec::new();
    for (d, decision) in digests.iter().zip(&decisions) {
        if let Some(hit) = moderation::refusing(decision) {
            flagged.push(*d);
            first.get_or_insert_with(|| hit.clone());
        }
    }
    first.map(|h| (h, flagged))
}

impl Hosting {
    /// This node's policy for the copies it holds, if it has one: only a
    /// node that holds copies for the network has copies to refuse, and the
    /// operator can keep `[moderation]` to the gateway (`refuse_replicas`).
    fn policy(&self) -> Option<Arc<Gate>> {
        if !self.0.hosts {
            return None;
        }
        self.0.moderation.clone().filter(|g| g.refuses_replicas())
    }

    /// The policy that applies to a copy of `rec` on this node, if any.
    fn policy_gate(&self, rec: &SiteRecord) -> Option<Arc<Gate>> {
        if stripe::is_striped(rec.size) || self.policy_exempt(rec) {
            return None;
        }
        self.policy()
    }

    /// Sites this node's own key owns or pays for: an operator's policy is
    /// about what it holds for others.
    fn policy_exempt(&self, rec: &SiteRecord) -> bool {
        let mine = |k: &str| rec.owner == k || rec.payer_key() == k;
        self.0.challenger_hex.as_deref().is_some_and(mine) || self.0.extra_challengers.iter().any(|(_, k)| mine(k))
    }

    /// The operator's `[moderation]` gate this node was started with (the
    /// gateway layers the same one over what it serves).
    pub fn moderation(&self) -> Option<Arc<Gate>> {
        self.0.moderation.clone()
    }

    /// Whether this node refuses this version of the site right now.
    pub(super) fn policy_refuses(&self, rec: &SiteRecord) -> bool {
        self.0
            .policy
            .lock()
            .expect("policy")
            .refused
            .get(&rec.site_id)
            .is_some_and(|r| r.refusal.manifest == rec.manifest)
    }

    /// How many replicas refused the current version of `site_id` under
    /// their operators' policy, per labeler and label value, as far as this
    /// node has heard. Which peers refused is not given out.
    pub async fn refusals(&self, site_id: &str) -> Vec<RefusalTally> {
        self.0.map.read().await.refusal_tally(site_id)
    }

    /// Every file of a site as (path, entry).
    async fn policy_files(&self, rec: &SiteRecord, m: &Manifest) -> Result<Vec<(String, FileEntry)>> {
        match m {
            Manifest::Inline(m) => Ok(m.files.iter().map(|(p, f)| (p.clone(), f.clone())).collect()),
            Manifest::Parted(r) => {
                let mut out = Vec::new();
                for pr in &r.parts {
                    let part = self.part(&rec.site_id, pr).await?;
                    out.extend(part.files.iter().map(|(p, f)| (p.clone(), f.clone())));
                }
                Ok(out)
            }
        }
    }

    /// SHA-256 of every file of a site, from the chunks in the local store.
    async fn policy_hash(&self, gate: &Gate, rec: &SiteRecord, m: &Manifest) -> Result<Hashed> {
        let files = self.policy_files(rec, m).await?;
        let (scans, max) = (gate.scans(), gate.max_scan_bytes());
        let mut out = Hashed {
            digests: Vec::with_capacity(files.len()),
            scans: Vec::new(),
        };
        for (path, f) in files {
            if f.chunks.is_empty() {
                continue;
            }
            let name = moderation::judged_name(&f.content_type)
                .filter(|_| scans && out.scans.len() < SCANS_PER_SITE && f.size <= max as u64);
            let mut keep = name.map(|_| Vec::new());
            let mut hasher = Sha256::new();
            for c in &f.chunks {
                let hash = Hash::from_hex(c).with_context(|| format!("chunk hash of {path}"))?;
                let bytes = self
                    .0
                    .store
                    .get(&hash)
                    .await
                    .with_context(|| format!("chunk of {path}"))?;
                hasher.update(&bytes);
                if let Some(k) = &mut keep {
                    k.extend_from_slice(&bytes);
                }
                // The size is the publisher's word.
                if keep.as_ref().is_some_and(|k| k.len() > max) {
                    keep = None;
                }
            }
            out.digests.push(hasher.finalize().into());
            if let (Some(name), Some(bytes)) = (name, keep) {
                out.scans.push((name, bytes));
            }
        }
        out.digests.sort();
        out.digests.dedup();
        Ok(out)
    }

    /// The check a new copy passes before it is held: every chunk of `rec`
    /// is in the store. A refused copy is recorded, announced and deleted,
    /// and the error is a [`PolicyRefused`]. Fails open: a site that cannot
    /// be read back, or labelers that do not answer, do not stop the copy
    /// (the periodic check asks again).
    pub(super) async fn policy_admit(&self, rec: &SiteRecord, manifest: &Manifest) -> Result<()> {
        let Some(gate) = self.policy_gate(rec) else {
            return Ok(());
        };
        let hashed = match self.policy_hash(&gate, rec, manifest).await {
            Ok(h) => h,
            Err(e) => {
                debug!(site = %rec.site_id, err = %e, "hosting: policy check could not read the site; holding it unchecked");
                return Ok(());
            }
        };
        match verdict(&gate, &hashed.digests, &hashed.scans).await {
            Some((hit, flagged)) => {
                self.policy_refuse(rec, &hit, flagged).await;
                Err(PolicyRefused {
                    labeler: hit.labeler,
                    val: hit.val,
                }
                .into())
            }
            None => {
                self.policy_note_checked(rec, Arc::new(hashed.digests));
                Ok(())
            }
        }
    }

    fn policy_note_checked(&self, rec: &SiteRecord, digests: Arc<Vec<[u8; 32]>>) {
        self.0.policy.lock().expect("policy").checked.insert(
            rec.site_id.clone(),
            Checked {
                manifest: rec.manifest.clone(),
                digests,
                at: Instant::now(),
            },
        );
    }

    /// Stop holding (or never start holding) `rec`: remember why, tell the
    /// network, delete the chunks. A copy that was held and announced keeps
    /// its chunks for one holder TTL, as a released surplus copy does, so a
    /// challenger that still lists this node is answered meanwhile.
    async fn policy_refuse(&self, rec: &SiteRecord, hit: &Hit, flagged: Vec<[u8; 32]>) {
        let held = {
            let mut cat = self.0.catalog.write().await;
            cat.stripes.remove(&rec.site_id);
            cat.sites.remove(&rec.site_id)
        };
        {
            let mut st = self.0.policy.lock().expect("policy");
            st.checked.remove(&rec.site_id);
            if st.refused.len() >= MAX_REFUSED && !st.refused.contains_key(&rec.site_id) {
                let oldest = st.refused.iter().min_by_key(|(_, r)| r.since).map(|(k, _)| k.clone());
                if let Some(k) = oldest {
                    st.refused.remove(&k);
                }
            }
            let now = Instant::now();
            st.refused.insert(
                rec.site_id.clone(),
                Refused {
                    refusal: Refusal {
                        site_id: rec.site_id.clone(),
                        manifest: rec.manifest.clone(),
                        labeler: hit.labeler.clone(),
                        val: hit.val.clone(),
                    },
                    flagged,
                    since: now,
                    checked: now,
                },
            );
        }
        info!(
            site = %rec.site_id,
            name = %rec.name,
            labeler = %hit.labeler,
            label = %hit.val,
            held = held.is_some(),
            "hosting: refusing a copy under this node's content policy (DIP-0036)"
        );
        if held.is_some() {
            self.save_catalog().await;
        }
        self.announce_holding().await;
        self.announce_refusals().await;
        match held {
            Some(old) => {
                let h = self.clone();
                let rec = rec.clone();
                let grace = self.0.timing.holder_ttl;
                tokio::spawn(async move {
                    tokio::time::sleep(grace).await;
                    // Taken back in the meantime (the refusal was lifted).
                    if h.0.catalog.read().await.sites.contains_key(&rec.site_id) {
                        return;
                    }
                    h.collect_garbage(&old, None).await;
                    if old.manifest != rec.manifest {
                        h.collect_garbage(&rec, None).await;
                    }
                });
            }
            None => self.collect_garbage(rec, None).await,
        }
    }

    /// Gossip what this node refuses (and count it in its own map). Sent
    /// with every holding announcement while there is something to say, and
    /// once more, empty, when there no longer is.
    pub(super) async fn announce_refusals(&self) {
        let list: Vec<Refusal> = {
            let mut st = self.0.policy.lock().expect("policy");
            if st.refused.is_empty() && !st.announced {
                return;
            }
            let mut v: Vec<(Instant, Refusal)> = st.refused.values().map(|r| (r.since, r.refusal.clone())).collect();
            v.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.site_id.cmp(&b.1.site_id)));
            v.truncate(MAX_REFUSALS);
            st.announced = !v.is_empty();
            v.into_iter().map(|(_, r)| r).collect()
        };
        self.0
            .map
            .write()
            .await
            .apply_refusals(&self.0.me, list.clone(), Instant::now());
        self.publish(&HostingMsg::PolicyRefusal { refusals: list }).await;
    }

    pub(super) fn spawn_policy(&self) {
        if self.policy().is_none() {
            return;
        }
        info!("hosting: this node refuses copies of sites its [moderation] labelers flag (DIP-0036)");
        let h = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(super::jitter(h.0.timing.repair_every)).await;
                if h.is_stopped() {
                    return;
                }
                h.policy_round().await;
            }
        });
    }

    /// One pass: lift refusals whose labels are gone, then check held sites
    /// that are due (never checked, or last checked `recheck_secs` ago).
    pub async fn policy_round(&self) {
        let Some(gate) = self.policy() else {
            return;
        };
        let every = gate.recheck();
        let due: Vec<(String, Vec<[u8; 32]>)> = {
            let st = self.0.policy.lock().expect("policy");
            st.refused
                .iter()
                .filter(|(_, r)| r.checked.elapsed() >= every)
                .map(|(s, r)| (s.clone(), r.flagged.clone()))
                .collect()
        };
        let mut lifted = false;
        for (site, flagged) in due {
            let still = gate
                .decide_hashes(&flagged)
                .await
                .iter()
                .any(|d| moderation::refusing(d).is_some());
            let mut st = self.0.policy.lock().expect("policy");
            if still {
                if let Some(r) = st.refused.get_mut(&site) {
                    r.checked = Instant::now();
                }
            } else if st.refused.remove(&site).is_some() {
                lifted = true;
                info!(%site, "hosting: content-policy refusal lifted (its labels are gone)");
            }
        }
        if lifted {
            self.announce_refusals().await;
        }
        let held: Vec<SiteRecord> = {
            let cat = self.0.catalog.read().await;
            cat.sites
                .values()
                .filter(|r| !cat.stripes.contains_key(&r.site_id))
                .cloned()
                .collect()
        };
        {
            let ids: HashSet<&String> = held.iter().map(|r| &r.site_id).collect();
            self.0
                .policy
                .lock()
                .expect("policy")
                .checked
                .retain(|s, _| ids.contains(s));
        }
        let mut checks = 0;
        for rec in held {
            if checks >= CHECKS_PER_ROUND || self.is_stopped() {
                break;
            }
            let Some(gate) = self.policy_gate(&rec) else {
                continue;
            };
            let known = self
                .0
                .policy
                .lock()
                .expect("policy")
                .checked
                .get(&rec.site_id)
                .filter(|c| c.manifest == rec.manifest)
                .map(|c| (c.digests.clone(), c.at));
            let (digests, scans) = match known {
                Some((_, at)) if at.elapsed() < every => continue,
                Some((digests, _)) => (digests, Vec::new()),
                None => {
                    let hashed = match self.manifest(&rec).await {
                        Ok(m) => self.policy_hash(&gate, &rec, &m).await,
                        Err(e) => Err(e),
                    };
                    match hashed {
                        Ok(h) => (Arc::new(h.digests), h.scans),
                        Err(e) => {
                            debug!(site = %rec.site_id, err = %e, "hosting: policy check could not read a held site");
                            // Not again until the next re-check is due.
                            self.policy_note_checked(&rec, Arc::new(Vec::new()));
                            continue;
                        }
                    }
                }
            };
            checks += 1;
            match verdict(&gate, &digests, &scans).await {
                Some((hit, flagged)) => self.policy_refuse(&rec, &hit, flagged).await,
                None => self.policy_note_checked(&rec, digests),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_sane_refusals_are_kept_and_the_error_is_typed() {
        let good = Refusal {
            site_id: "0123456789abcdef0123".into(),
            manifest: Hash::of(b"m").to_hex(),
            labeler: "did:web:threatcrush.com".into(),
            val: "phishing".into(),
        };
        assert!(good.sane());
        let bad = |f: &dyn Fn(&mut Refusal)| {
            let mut r = good.clone();
            f(&mut r);
            r.sane()
        };
        assert!(!bad(&|r| r.site_id = "short".into()));
        assert!(!bad(&|r| r.site_id = "0123456789abcdef012/".into()));
        assert!(!bad(&|r| r.manifest = "nothex".into()));
        assert!(!bad(&|r| r.labeler = String::new()));
        assert!(!bad(&|r| r.labeler = "did web".into()));
        assert!(!bad(&|r| r.val = "x\ny".into()));
        assert!(!bad(&|r| r.val = "v".repeat(65)));

        // What `replicate` returns is told apart from a failed copy.
        let e: anyhow::Error = PolicyRefused {
            labeler: good.labeler.clone(),
            val: good.val.clone(),
        }
        .into();
        assert!(e.is::<PolicyRefused>());
        assert!(e.to_string().contains("phishing") && e.to_string().contains("did:web:threatcrush.com"));
        assert!(!anyhow::anyhow!("no holders").is::<PolicyRefused>());
    }

    #[test]
    fn a_refusal_is_one_small_gossip_message_old_nodes_skip() {
        let r = Refusal {
            site_id: "0123456789abcdef0123".into(),
            manifest: Hash::of(b"m").to_hex(),
            labeler: format!("did:web:{}", "l".repeat(120)),
            val: "v".repeat(64),
        };
        assert!(r.sane());
        let msg = HostingMsg::PolicyRefusal {
            refusals: vec![r.clone(); MAX_REFUSALS],
        };
        let bytes = serde_json::to_vec(&msg).unwrap();
        assert!(bytes.len() < 64 * 1024, "{} bytes", bytes.len());
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["t"], "policy_refusal");
        match serde_json::from_slice::<HostingMsg>(&bytes).unwrap() {
            HostingMsg::PolicyRefusal { refusals } => {
                assert_eq!(refusals.len(), MAX_REFUSALS);
                assert_eq!(refusals[0], r);
            }
            other => panic!("{other:?}"),
        }
        // A node that predates the variant fails to parse it and moves on
        // (the gossip reader skips what it cannot read), as with any tag it
        // does not know.
        assert!(serde_json::from_slice::<HostingMsg>(br#"{"t":"something_newer","x":1}"#).is_err());
    }
}
