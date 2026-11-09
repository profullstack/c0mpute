//! Proof of storage for hosted sites: anyone challenges a holder by fetching
//! one random chunk of the site straight from that peer, checks it against
//! its hash, and signs an availability [`Receipt`] saying whether it passed.
//!
//! Receipts are gossiped on the hosting topic, so every node sees the same
//! history. Two things read them:
//!
//! * placement: a preferred host whose recent receipts from the site's owner
//!   or payer are mostly failures loses its preference ([`demoted`]), and the
//!   normal rendezvous fill takes over;
//! * settlement: the payer pays preferred holders in proportion to the
//!   challenges they passed (`settle`).
//!
//! What this proves: the peer returned the right bytes for a chunk nobody
//! could predict, within the timeout. It does not prove the peer keeps its
//! own copy rather than relaying from another holder; relaying costs the
//! relayer bandwidth and latency, and the rest of the network still has the
//! data, so the site stays up either way.

use anyhow::Result;
use c0mpute_proto::Hash;
use serde::{Deserialize, Serialize};

use super::site::SiteManifest;

const SIGNING_DOMAIN: &str = "c0mpute-hosting-receipt-v1";

/// Receipts per (site, holder) a node keeps for placement decisions.
pub const KEEP_PER_HOLDER: usize = 16;
/// A preferred host is judged on its most recent this-many trusted receipts…
pub const RECENT: usize = 5;
/// …and loses preference when at least this many of them failed.
pub const FAIL_LIMIT: usize = 3;

/// "I asked `holder` for chunk `chunk` of `site_id` at `at`; it passed or not."
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub v: u8,
    pub site_id: String,
    /// Manifest (version) the chunk belongs to.
    pub manifest: String,
    /// The challenged peer (base58 peer id).
    pub holder: String,
    /// The chunk asked for (hex blake3).
    pub chunk: String,
    pub ok: bool,
    /// Round-trip of the fetch, ms (0 on a failure).
    pub rtt_ms: u32,
    /// Unix seconds.
    pub at: u64,
    /// Challenger's ed25519 public key, hex.
    pub challenger: String,
    pub sig: String,
}

/// The unsigned fields of a receipt.
pub struct Challenge<'a> {
    pub site_id: &'a str,
    pub manifest: &'a str,
    pub holder: &'a str,
    pub chunk: &'a str,
    pub ok: bool,
    pub rtt_ms: u32,
    pub at: u64,
}

impl Receipt {
    pub fn signing_bytes(&self) -> Vec<u8> {
        format!(
            "{SIGNING_DOMAIN}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
            self.site_id, self.manifest, self.holder, self.chunk, self.ok, self.rtt_ms, self.at, self.challenger
        )
        .into_bytes()
    }

    pub fn sign(keypair: &c0mpute_net::libp2p::identity::Keypair, c: Challenge<'_>) -> Result<Self> {
        let challenger = super::site::owner_hex(keypair).ok_or_else(|| anyhow::anyhow!("challenger needs an ed25519 key"))?;
        let mut r = Receipt {
            v: 1,
            site_id: c.site_id.to_string(),
            manifest: c.manifest.to_string(),
            holder: c.holder.to_string(),
            chunk: c.chunk.to_string(),
            ok: c.ok,
            rtt_ms: c.rtt_ms,
            at: c.at,
            challenger,
            sig: String::new(),
        };
        let sig = keypair
            .sign(&r.signing_bytes())
            .map_err(|e| anyhow::anyhow!("sign receipt: {e}"))?;
        r.sig = hex::encode(sig);
        Ok(r)
    }

    /// True when the fields are sane and the challenger signed them.
    pub fn verify(&self) -> bool {
        if self.v != 1
            || self.site_id.len() != 20
            || self.holder.is_empty()
            || self.holder.len() > 64
            || Hash::from_hex(&self.manifest).is_err()
            || Hash::from_hex(&self.chunk).is_err()
        {
            return false;
        }
        let (Ok(key), Ok(sig)) = (hex::decode(&self.challenger), hex::decode(&self.sig)) else {
            return false;
        };
        let Ok(public) = c0mpute_net::libp2p::identity::ed25519::PublicKey::try_from_bytes(&key) else {
            return false;
        };
        public.verify(&self.signing_bytes(), &sig)
    }
}

/// The chunk to ask for, picked by `nonce` (the challenger passes fresh
/// randomness, so a holder cannot keep only the chunks it expects).
pub fn pick_chunk(manifest: &SiteManifest, nonce: &[u8]) -> Option<String> {
    let all = manifest.chunk_hashes();
    if all.is_empty() {
        return None;
    }
    let h = Hash::of(nonce).0;
    let n = u64::from_le_bytes(h[..8].try_into().expect("8 bytes"));
    Some(all[(n % all.len() as u64) as usize].clone())
}

/// Whether a holder has lost preference: of its most recent [`RECENT`]
/// receipts signed by a `trusted` key (the site's owner or payer), at least
/// [`FAIL_LIMIT`] failed. `receipts` may be in any order and may include
/// untrusted ones; those are ignored, so a stranger cannot demote anybody.
pub fn demoted(receipts: &[Receipt], trusted: &[&str]) -> bool {
    let mut mine: Vec<&Receipt> = receipts
        .iter()
        .filter(|r| trusted.contains(&r.challenger.as_str()))
        .collect();
    mine.sort_by(|a, b| b.at.cmp(&a.at).then(a.sig.cmp(&b.sig)));
    mine.iter().take(RECENT).filter(|r| !r.ok).count() >= FAIL_LIMIT
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hosting::site::FileEntry;
    use std::collections::BTreeMap;

    fn manifest(n: usize) -> SiteManifest {
        let mut files = BTreeMap::new();
        for i in 0..n {
            files.insert(
                format!("f{i}"),
                FileEntry {
                    size: 1,
                    content_type: "text/plain".into(),
                    chunks: vec![Hash::of(&[i as u8]).to_hex()],
                },
            );
        }
        SiteManifest {
            v: 1,
            name: "s".into(),
            files,
        }
    }

    fn receipt(kp: &c0mpute_net::libp2p::identity::Keypair, ok: bool, at: u64) -> Receipt {
        let m = Hash::of(b"m").to_hex();
        let c = Hash::of(b"c").to_hex();
        Receipt::sign(
            kp,
            Challenge {
                site_id: "0123456789abcdef0123",
                manifest: &m,
                holder: "peer",
                chunk: &c,
                ok,
                rtt_ms: 5,
                at,
            },
        )
        .unwrap()
    }

    #[test]
    fn receipts_verify_and_reject_tampering() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let r = receipt(&kp, true, 100);
        assert!(r.verify());
        let back: Receipt = serde_json::from_slice(&serde_json::to_vec(&r).unwrap()).unwrap();
        assert!(back.verify());
        let mut t = r.clone();
        t.ok = false;
        assert!(!t.verify(), "a pass cannot be turned into a fail");
        let mut t = r.clone();
        t.holder = "other".into();
        assert!(!t.verify());
        let mut t = r.clone();
        t.at = 101;
        assert!(!t.verify());
    }

    #[test]
    fn chunk_choice_follows_the_nonce() {
        let m = manifest(50);
        let picks: std::collections::HashSet<String> =
            (0u32..64).filter_map(|i| pick_chunk(&m, &i.to_le_bytes())).collect();
        assert!(picks.len() > 20, "random nonces spread over the site: {}", picks.len());
        assert_eq!(pick_chunk(&m, b"x"), pick_chunk(&m, b"x"));
        assert!(pick_chunk(&manifest(0), b"x").is_none());
    }

    #[test]
    fn only_trusted_recent_failures_demote() {
        let owner = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let stranger = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let owner_hex = crate::hosting::site::owner_hex(&owner).unwrap();
        let trusted = [owner_hex.as_str()];
        // Strangers' failures do nothing.
        let noise: Vec<Receipt> = (0..10).map(|i| receipt(&stranger, false, i)).collect();
        assert!(!demoted(&noise, &trusted));
        // Two recent owner failures: still preferred. Three: demoted.
        let mut rs = vec![receipt(&owner, true, 1), receipt(&owner, false, 2), receipt(&owner, false, 3)];
        assert!(!demoted(&rs, &trusted));
        rs.push(receipt(&owner, false, 4));
        assert!(demoted(&rs, &trusted));
        // Passing again pushes the failures out of the window.
        for at in 5..10 {
            rs.push(receipt(&owner, true, at));
        }
        assert!(!demoted(&rs, &trusted));
    }
}
