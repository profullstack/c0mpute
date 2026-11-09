//! Settlement for marketplace sites: what a payer owes each preferred holder
//! for a window of time, from the availability receipts it trusts.
//!
//! Pure functions, no I/O. The node runs them over its own receipts and
//! hands the result to a [`crate::payments::Rail`]; with payments off
//! (the default) it only logs what it would pay.
//!
//! The rule, per preferred holder of a site whose payer is this node:
//!
//! ```text
//! owed = price_per_gib_month × site_GiB × (window / 30 days) × passed / challenged
//! ```
//!
//! counting only receipts in the window signed by the site's owner or payer.
//! A holder that passed fewer than half its challenges, or was never
//! challenged, is owed nothing for the window. Amounts round down to the
//! micro-USD.

use std::collections::HashMap;

use super::proof::Receipt;
use super::site::SiteRecord;
use crate::payments::{Micros, Payee, Transfer};

/// The month in GiB-month: 30 days.
pub const MONTH_SECS: u64 = 30 * 24 * 60 * 60;
const GIB: u128 = 1024 * 1024 * 1024;

/// Owed for holding `size` bytes at `price` micro-USD per GiB-month for
/// `secs` seconds, having passed `passed` of `challenged` challenges.
pub fn owed(price: Micros, size: u64, secs: u64, passed: u64, challenged: u64) -> Micros {
    if challenged == 0 || passed * 2 < challenged {
        return 0;
    }
    let full = (price as u128 * size as u128).saturating_mul(secs as u128) / (GIB * MONTH_SECS as u128);
    let earned = full.saturating_mul(passed as u128) / challenged as u128;
    earned.min(u64::MAX as u128) as Micros
}

/// One holder's due for one site and window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Due {
    pub site_id: String,
    pub holder: String,
    pub payee: Option<Payee>,
    pub amount: Micros,
    pub passed: u64,
    pub challenged: u64,
    /// Idempotency key: same site, holder and window, same payment.
    pub key: String,
}

/// Everything `payer` owes for `[from, to)` (unix seconds).
pub fn dues(
    records: &[SiteRecord],
    receipts: &[Receipt],
    payouts: &HashMap<String, Payee>,
    payer: &str,
    from: u64,
    to: u64,
) -> Vec<Due> {
    if to <= from {
        return Vec::new();
    }
    let mut out = Vec::new();
    for rec in records {
        let Some(terms) = rec.terms() else {
            continue;
        };
        if rec.payer_key() != payer || !rec.verify() {
            continue;
        }
        let trusted = [rec.owner.as_str(), rec.payer_key()];
        for holder in &terms.preferred_hosts {
            let mut seen = std::collections::HashSet::new();
            let (mut passed, mut challenged) = (0u64, 0u64);
            for r in receipts {
                if r.site_id == rec.site_id
                    && r.holder == *holder
                    && (from..to).contains(&r.at)
                    && trusted.contains(&r.challenger.as_str())
                    && seen.insert(r.sig.clone())
                    && r.verify()
                {
                    challenged += 1;
                    if r.ok {
                        passed += 1;
                    }
                }
            }
            let amount = owed(terms.price_per_gib_month, rec.size, to - from, passed, challenged);
            out.push(Due {
                site_id: rec.site_id.clone(),
                holder: holder.clone(),
                payee: payouts.get(holder).cloned(),
                amount,
                passed,
                challenged,
                key: format!("c0mpute-hosting:{}:{}:{from}:{to}", rec.site_id, holder),
            });
        }
    }
    out.sort_by(|a, b| a.site_id.cmp(&b.site_id).then(a.holder.cmp(&b.holder)));
    out
}

/// The transfers to make: dues with something owed and a payout address.
pub fn transfers(dues: &[Due], payer: &str) -> Vec<Transfer> {
    dues.iter()
        .filter(|d| d.amount > 0)
        .filter_map(|d| {
            let payee = d.payee.clone().filter(|p| p.valid())?;
            Some(Transfer {
                key: d.key.clone(),
                payer: payer.to_string(),
                payee,
                amount: d.amount,
                memo: format!(
                    "c0mpute hosting: site {} held by {} ({}/{} challenges passed)",
                    d.site_id, d.holder, d.passed, d.challenged
                ),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hosting::proof::Challenge;
    use crate::hosting::site::HostingTerms;
    use c0mpute_proto::Hash;

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn owed_follows_price_size_time_and_passes() {
        // $1/GiB-month, 1 GiB, a whole month, every challenge passed = $1.
        assert_eq!(owed(1_000_000, 1024 * MIB, MONTH_SECS, 10, 10), 1_000_000);
        // Half the size, half the time = a quarter.
        assert_eq!(owed(1_000_000, 512 * MIB, MONTH_SECS / 2, 4, 4), 250_000);
        // 3 of 4 passed = three quarters.
        assert_eq!(owed(1_000_000, 1024 * MIB, MONTH_SECS, 3, 4), 750_000);
        // Under half passed, or never challenged: nothing.
        assert_eq!(owed(1_000_000, 1024 * MIB, MONTH_SECS, 1, 3), 0);
        assert_eq!(owed(1_000_000, 1024 * MIB, MONTH_SECS, 0, 0), 0);
        // Exactly half passes.
        assert_eq!(owed(1_000_000, 1024 * MIB, MONTH_SECS, 2, 4), 500_000);
        // Small sites round down, never up.
        assert_eq!(owed(4_000, MIB, 3600, 1, 1), 0);
        // No overflow at the extremes.
        assert!(owed(u64::MAX, u64::MAX, u64::MAX, 1, 1) > 0);
    }

    fn pid() -> String {
        c0mpute_net::libp2p::identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id()
            .to_base58()
    }

    #[test]
    fn dues_count_only_trusted_receipts_in_the_window() {
        let owner = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let owner_hex = crate::hosting::site::owner_hex(&owner).unwrap();
        let stranger = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let (a, b) = (pid(), pid());
        let rec = SiteRecord::sign_with_terms(
            &owner,
            "paid",
            &Hash::of(b"m"),
            1,
            256 * MIB,
            vec![],
            Some(HostingTerms {
                preferred_hosts: vec![a.clone(), b.clone()],
                price_per_gib_month: 8_000_000,
                payer: None,
            }),
        )
        .unwrap();
        let chunk = Hash::of(b"c").to_hex();
        let r = |kp: &c0mpute_net::libp2p::identity::Keypair, holder: &str, ok: bool, at: u64| {
            Receipt::sign(
                kp,
                Challenge {
                    site_id: &rec.site_id,
                    manifest: &rec.manifest,
                    holder,
                    chunk: &chunk,
                    ok,
                    rtt_ms: 1,
                    at,
                },
            )
            .unwrap()
        };
        let mut receipts = vec![
            r(&owner, &a, true, 100),
            r(&owner, &a, true, 200),
            r(&owner, &b, true, 100),
            r(&owner, &b, false, 200),
            r(&owner, &b, false, 300),
            // Outside the window, or not trusted: ignored.
            r(&owner, &a, false, 5_000_000),
            r(&stranger, &a, false, 150),
            r(&stranger, &b, true, 150),
        ];
        receipts.push(receipts[0].clone()); // a duplicate counts once
        let mut payouts = HashMap::new();
        payouts.insert(
            a.clone(),
            Payee {
                wallet: "0xaaaaaaaaaaaaaaaa".into(),
                coin: "USDC_POL".into(),
            },
        );
        let ds = dues(&[rec.clone()], &receipts, &payouts, &owner_hex, 0, MONTH_SECS);
        assert_eq!(ds.len(), 2);
        let da = ds.iter().find(|d| d.holder == a).unwrap();
        let db = ds.iter().find(|d| d.holder == b).unwrap();
        assert_eq!((da.passed, da.challenged), (2, 2));
        assert_eq!(da.amount, 2_000_000, "a quarter GiB, a month, $8/GiB-month, all passed");
        assert_eq!((db.passed, db.challenged), (1, 3));
        assert_eq!(db.amount, 0, "under half passed");
        // Only `a` is owed and has a payout address.
        let ts = transfers(&ds, &owner_hex);
        assert_eq!(ts.len(), 1);
        assert_eq!(ts[0].amount, 2_000_000);
        assert_eq!(ts[0].key, da.key);
        // Somebody else's sites are not ours to pay.
        assert!(dues(&[rec], &receipts, &payouts, &"c".repeat(64), 0, MONTH_SECS).is_empty());
    }
}
