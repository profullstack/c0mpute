//! Reed-Solomon erasure coding for the c0mpute storage plugin.
//!
//! Defaults: k=10, n=14 (4 parity shards). Any 10 of 14 shards
//! reconstruct the original. See dips/0012-storage-plugin.md.

use anyhow::{Context, Result, bail};
use reed_solomon_erasure::galois_8::ReedSolomon;

/// Default data shard count (DIP-0012).
pub const DEFAULT_K: usize = 10;
/// Default total shard count (DIP-0012).
pub const DEFAULT_N: usize = 14;
/// Number of parity shards under the default scheme.
pub const DEFAULT_PARITY: usize = DEFAULT_N - DEFAULT_K;

/// One Reed-Solomon shard: index 0..n and the encoded bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shard {
    pub index: u8,
    pub bytes: Vec<u8>,
}

/// Encode `data` into k+parity shards. Returns the shards plus the
/// original byte length (so the decoder can trim padding).
pub fn encode(data: &[u8], k: usize, parity: usize) -> Result<(Vec<Shard>, usize)> {
    if k == 0 || parity == 0 {
        bail!("erasure encode: k and parity must be > 0");
    }
    let n = k + parity;

    // RS requires equal-length shards; pad the last data shard with
    // zeros. Empty input still gets at least one byte per shard so
    // the underlying RS impl doesn't panic on zero-length chunks.
    let shard_len = data.len().div_ceil(k).max(1);
    let mut buffers: Vec<Vec<u8>> = (0..n).map(|_| vec![0u8; shard_len]).collect();
    if !data.is_empty() {
        for (i, chunk) in data.chunks(shard_len).enumerate() {
            buffers[i][..chunk.len()].copy_from_slice(chunk);
        }
    }

    let rs = ReedSolomon::new(k, parity).context("ReedSolomon::new")?;
    rs.encode(&mut buffers).context("ReedSolomon::encode")?;

    let shards = buffers
        .into_iter()
        .enumerate()
        .map(|(i, b)| Shard {
            index: i as u8,
            bytes: b,
        })
        .collect();
    Ok((shards, data.len()))
}

/// Decode the original bytes from any subset of shards.
///
/// `received` is `Vec<Option<Shard>>` of length n; `None` means lost.
/// At least k slots must be `Some`. The implementation hands the
/// shards to the RS crate as `Vec<Option<Vec<u8>>>`, which
/// reconstructs missing positions in place.
pub fn decode(
    received: Vec<Option<Shard>>,
    k: usize,
    parity: usize,
    original_len: usize,
) -> Result<Vec<u8>> {
    let n = k + parity;
    if received.len() != n {
        bail!("erasure decode: expected {n} positions, got {}", received.len());
    }
    let surviving = received.iter().filter(|s| s.is_some()).count();
    if surviving < k {
        bail!("erasure decode: need at least {k} shards, got {surviving}");
    }

    let mut shards: Vec<Option<Vec<u8>>> =
        received.into_iter().map(|s| s.map(|s| s.bytes)).collect();

    let rs = ReedSolomon::new(k, parity).context("ReedSolomon::new")?;
    rs.reconstruct(&mut shards)
        .context("ReedSolomon::reconstruct")?;

    let mut out = Vec::with_capacity(original_len);
    for shard in shards.iter().take(k) {
        let bytes = shard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("RS reconstruct left data slot empty"))?;
        out.extend_from_slice(bytes);
    }
    out.truncate(original_len);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(data: &[u8], drop: &[u8]) -> Vec<u8> {
        let (shards, len) = encode(data, DEFAULT_K, DEFAULT_PARITY).unwrap();
        let mut received: Vec<Option<Shard>> = shards.into_iter().map(Some).collect();
        for &i in drop {
            received[i as usize] = None;
        }
        decode(received, DEFAULT_K, DEFAULT_PARITY, len).unwrap()
    }

    #[test]
    fn round_trip_no_loss() {
        let data = b"hello c0mpute storage";
        assert_eq!(round_trip(data, &[]), data);
    }

    #[test]
    fn survives_four_shard_loss() {
        // 4 lost = exactly the parity budget for RS 10/14.
        let data = b"the quick brown fox jumps over the lazy dog".repeat(100);
        // Drop 2 data shards + 2 parity shards.
        assert_eq!(round_trip(&data, &[1, 5, 10, 13]), data);
    }

    #[test]
    fn fails_on_five_shard_loss() {
        let data = b"will this survive?".repeat(100);
        let (shards, len) = encode(&data, DEFAULT_K, DEFAULT_PARITY).unwrap();
        let mut received: Vec<Option<Shard>> = shards.into_iter().map(Some).collect();
        for i in [0, 1, 2, 3, 4] {
            received[i] = None;
        }
        let err = decode(received, DEFAULT_K, DEFAULT_PARITY, len).unwrap_err();
        assert!(err.to_string().contains("at least"));
    }

    #[test]
    fn empty_data_roundtrips() {
        let data: &[u8] = b"";
        let (shards, len) = encode(data, DEFAULT_K, DEFAULT_PARITY).unwrap();
        assert_eq!(len, 0);
        let received: Vec<Option<Shard>> = shards.into_iter().map(Some).collect();
        let out = decode(received, DEFAULT_K, DEFAULT_PARITY, len).unwrap();
        assert_eq!(out, b"");
    }
}
