//! Size padding: Padmé (Nikitin et al., "Reducing Metadata Leakage from
//! Encrypted Files and Communication with PURBs", PETS 2019).
//!
//! Padmé rounds a length L up so that only the top ~log2(log2(L)) bits stay
//! significant. Overhead is at most ~12% (less as objects grow), and an
//! observer learns O(log log L) bits about the true size instead of all of
//! them. Below [`MIN_PADDED`] everything rounds to the same bucket.

/// Every sealed stream is at least this long, so small objects (a password,
/// a short note) are indistinguishable from each other.
pub const MIN_PADDED: u64 = 4096;

/// The Padmé bucket for `len`, never below [`MIN_PADDED`].
pub fn padme(len: u64) -> u64 {
    if len <= MIN_PADDED {
        return MIN_PADDED;
    }
    // E = floor(log2 L), S = floor(log2 E) + 1, keep the top S bits.
    let e = 63 - len.leading_zeros() as u64;
    let s = (63 - e.leading_zeros() as u64) + 1;
    let last_bits = e - s;
    let mask = (1u64 << last_bits) - 1;
    (len + mask) & !mask
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_sizes_share_one_bucket() {
        for n in [0, 1, 100, 4095, 4096] {
            assert_eq!(padme(n), MIN_PADDED);
        }
    }

    #[test]
    fn padding_never_shrinks_and_overhead_is_bounded() {
        let mut n = 4097u64;
        while n < 1 << 34 {
            let p = padme(n);
            assert!(p >= n, "{n} -> {p}");
            assert!((p - n) as f64 / n as f64 <= 0.12, "{n} -> {p}");
            n = n * 3 / 2 + 7;
        }
    }

    #[test]
    fn buckets_are_coarse() {
        // Every length from 1 MiB to 1.1 MiB maps to very few buckets.
        let mut seen = std::collections::BTreeSet::new();
        for n in (1 << 20)..((1 << 20) + (1 << 20) / 10) {
            seen.insert(padme(n));
        }
        assert!(seen.len() <= 5, "{} buckets", seen.len());
    }
}
