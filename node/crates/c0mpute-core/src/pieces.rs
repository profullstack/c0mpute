//! Moving a file bigger than one network message between peers.
//!
//! A libp2p chunk fetch answers with ONE message, and the fetch codec caps a
//! response at 10 MiB. Chunk bytes travel as a CBOR integer array (about 2x on
//! the wire for incompressible data), and the request itself times out after
//! 10 s, so anything much over 4 MiB cannot move as a single chunk.
//!
//! Instead the holder splits the file into fixed-size pieces, stores and
//! announces each piece, and the spec names the whole-file hash, its size and
//! the piece hashes in order. The receiver fetches the pieces a few at a time,
//! checks every piece, every piece size and the whole hash.
//!
//! `blender.render-frame` moves `.blend` files and frames in
//! [`crate::render::PIECE_BYTES`] (4 MiB) pieces; job inputs (`ocr.extract`
//! documents, `stt.transcribe` audio, `video.img2vid` images) use
//! [`INPUT_PIECE_BYTES`] (1 MiB, like hosting), which stays well inside the
//! codec cap and the request timeout on a slow uplink.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use c0mpute_net::{Libp2pNetwork, Network, PeerId};
use c0mpute_proto::{ChunkRequest, Hash};
use c0mpute_store::ChunkStore;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Piece size for job inputs a buyer sends a worker.
pub const INPUT_PIECE_BYTES: usize = 1024 * 1024;
/// Largest input that still fits one fetch response (measured on loopback:
/// 5.0 MiB of incompressible bytes arrives, 5.25 MiB does not). Buyers keep a
/// whole copy of inputs up to this size for workers that predate pieces.
pub const LEGACY_WHOLE_MAX_BYTES: usize = 4 * 1024 * 1024;
/// Per-piece fetch deadline.
pub const PIECE_FETCH_TIMEOUT: Duration = Duration::from_secs(60);
/// Pieces in flight at once.
const PIECE_FETCH_CONCURRENCY: usize = 4;

/// How many `piece_bytes` pieces a file of `size` bytes has (at least one).
pub fn piece_count(size: u64, piece_bytes: usize) -> usize {
    (size as usize).div_ceil(piece_bytes).max(1)
}

/// A file split into stored, announced pieces.
#[derive(Debug, Clone)]
pub struct Pieces {
    /// Hex hash of the whole file.
    pub hash: String,
    pub size: u64,
    /// Hex hashes of the pieces, in order.
    pub pieces: Vec<String>,
}

/// Store `path` as `piece_bytes` pieces and announce each. Streams: the file
/// is never in memory whole.
pub async fn put_file_pieces(
    net: &Libp2pNetwork,
    store: &ChunkStore,
    path: &Path,
    max_bytes: u64,
    piece_bytes: usize,
) -> Result<Pieces> {
    let size = tokio::fs::metadata(path)
        .await
        .with_context(|| format!("read {}", path.display()))?
        .len();
    if size == 0 || size > max_bytes {
        bail!(
            "{} is {size} bytes; the limit is {max_bytes}",
            path.display()
        );
    }
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = blake3::Hasher::new();
    let mut pieces = Vec::with_capacity(piece_count(size, piece_bytes));
    let mut buf = vec![0u8; piece_bytes];
    let mut total = 0u64;
    loop {
        let mut filled = 0;
        while filled < piece_bytes {
            let n = file.read(&mut buf[filled..]).await?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        if filled == 0 {
            break;
        }
        let piece = &buf[..filled];
        hasher.update(piece);
        total += filled as u64;
        let hash = store.put(piece).await.context("store piece")?;
        net.announce(&hash).await.context("announce piece")?;
        pieces.push(hash.to_hex());
        if filled < piece_bytes {
            break;
        }
    }
    if total != size {
        bail!("{} changed while it was read", path.display());
    }
    Ok(Pieces {
        hash: hasher.finalize().to_hex().to_string(),
        size,
        pieces,
    })
}

/// Store in-memory `bytes` as `piece_bytes` pieces and announce each. A
/// one-piece input stores exactly one chunk whose hash is the whole hash, the
/// same chunk an older buyer stored. A multi-piece input up to
/// [`LEGACY_WHOLE_MAX_BYTES`] is also stored whole, so a worker that predates
/// pieces (and reads only the `_hash` field) can still fetch it in one message.
pub async fn put_bytes_pieces(
    net: &Libp2pNetwork,
    store: &ChunkStore,
    bytes: &[u8],
    piece_bytes: usize,
) -> Result<Pieces> {
    if bytes.is_empty() {
        bail!("cannot store an empty input");
    }
    let mut pieces = Vec::with_capacity(piece_count(bytes.len() as u64, piece_bytes));
    for piece in bytes.chunks(piece_bytes) {
        let hash = store.put(piece).await.context("store piece")?;
        net.announce(&hash).await.context("announce piece")?;
        pieces.push(hash.to_hex());
    }
    if pieces.len() > 1 && bytes.len() <= LEGACY_WHOLE_MAX_BYTES {
        let hash = store.put(bytes).await.context("store whole input")?;
        net.announce(&hash).await.context("announce whole input")?;
    }
    Ok(Pieces {
        hash: Hash::of(bytes).to_hex(),
        size: bytes.len() as u64,
        pieces,
    })
}

impl Pieces {
    /// The spec fields that name this input, under `prefix` (`input`,
    /// `audio`, `image`): `<prefix>_hash` always, plus `<prefix>_size` and
    /// `<prefix>_pieces` when there is more than one piece. A one-piece
    /// input therefore looks exactly like an older single-chunk spec, which
    /// every worker version can fetch.
    pub fn spec_fields(&self, prefix: &str) -> serde_json::Map<String, serde_json::Value> {
        let mut map = serde_json::Map::new();
        map.insert(format!("{prefix}_hash"), self.hash.clone().into());
        if self.pieces.len() > 1 {
            map.insert(format!("{prefix}_size"), self.size.into());
            map.insert(format!("{prefix}_pieces"), self.pieces.clone().into());
        }
        map
    }
}

/// Fetch one chunk: from `provider` first when known, else (or on failure)
/// from whoever the DHT says holds it. Checks the hash and `max_bytes`.
pub async fn fetch_chunk(
    net: &Libp2pNetwork,
    provider: Option<PeerId>,
    hash: Hash,
    max_bytes: usize,
    timeout: Duration,
) -> Result<Vec<u8>> {
    let request = ChunkRequest {
        chunk_hash: hash,
        shard_index: None,
    };
    let fetch = async {
        match provider {
            Some(peer) => match net.fetch_from_peer(peer, &request).await {
                Ok(bytes) => Ok(bytes),
                Err(_) => net.fetch(&request).await,
            },
            None => net.fetch(&request).await,
        }
    };
    let bytes = tokio::time::timeout(timeout, fetch)
        .await
        .context("timed out fetching a piece")??;
    if Hash::of(&bytes) != hash || bytes.len() > max_bytes {
        bail!("piece does not match its hash or is too big");
    }
    Ok(bytes)
}

/// Where verified pieces go.
enum Sink<'a> {
    File(&'a mut tokio::fs::File),
    Memory(&'a mut Vec<u8>),
}

/// Fetch every piece (a few at a time) and hand each, in order, to `sink`
/// after checking its hash and size. Checks the total size and whole hash.
async fn fetch_pieces_with(
    net: &Arc<Libp2pNetwork>,
    provider: Option<PeerId>,
    pieces: &[Hash],
    size: u64,
    whole: Hash,
    piece_bytes: usize,
    mut sink: Sink<'_>,
) -> Result<()> {
    if pieces.len() != piece_count(size, piece_bytes) {
        bail!("{} pieces cannot make {size} bytes", pieces.len());
    }
    let mut hasher = blake3::Hasher::new();
    let mut written = 0u64;
    for window in pieces.chunks(PIECE_FETCH_CONCURRENCY) {
        let mut set = tokio::task::JoinSet::new();
        for (k, hash) in window.iter().enumerate() {
            let net = net.clone();
            let hash = *hash;
            set.spawn(async move {
                let got =
                    fetch_chunk(&net, provider, hash, piece_bytes, PIECE_FETCH_TIMEOUT).await;
                (k, got)
            });
        }
        let mut got: Vec<Option<Vec<u8>>> = vec![None; window.len()];
        while let Some(joined) = set.join_next().await {
            let (k, bytes) = joined?;
            got[k] = Some(bytes?);
        }
        for bytes in got.into_iter().flatten() {
            written += bytes.len() as u64;
            let last = written >= size;
            if (!last && bytes.len() != piece_bytes) || written > size {
                bail!("pieces do not add up to {size} bytes");
            }
            hasher.update(&bytes);
            match &mut sink {
                Sink::File(file) => file.write_all(&bytes).await?,
                Sink::Memory(out) => out.extend_from_slice(&bytes),
            }
        }
    }
    if written != size || Hash(*hasher.finalize().as_bytes()) != whole {
        bail!("reassembled file does not match its hash");
    }
    Ok(())
}

/// [`fetch_pieces_with`] into a file: written to a temp name next to `dest`
/// and renamed into place only once every check passed.
pub async fn fetch_pieces_to_file(
    net: &Arc<Libp2pNetwork>,
    provider: Option<PeerId>,
    pieces: &[Hash],
    size: u64,
    whole: Hash,
    piece_bytes: usize,
    dest: &Path,
) -> Result<()> {
    let tmp = dest.with_extension(format!("part-{}", uuid::Uuid::new_v4()));
    let result: Result<()> = async {
        let mut file = tokio::fs::File::create(&tmp).await?;
        fetch_pieces_with(
            net,
            provider,
            pieces,
            size,
            whole,
            piece_bytes,
            Sink::File(&mut file),
        )
        .await?;
        file.flush().await?;
        tokio::fs::rename(&tmp, dest).await?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    result
}

/// A job input as a spec names it: either one chunk (every spec before
/// pieces existed, and any one-piece input) or a whole hash with its size
/// and [`INPUT_PIECE_BYTES`] piece hashes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputRef {
    Single(Hash),
    Pieces {
        whole: Hash,
        size: u64,
        pieces: Vec<Hash>,
    },
}

impl InputRef {
    /// Check a spec's `<prefix>_hash` / `_size` / `_pieces` fields. `size`
    /// and `pieces` come together or not at all; `what` names the field
    /// prefix in errors.
    pub fn parse(
        what: &str,
        hash: &str,
        size: Option<u64>,
        pieces: Option<&[String]>,
        max_bytes: usize,
    ) -> std::result::Result<Self, String> {
        let whole = Hash::from_hex(hash)
            .map_err(|_| format!("{what}_hash must be a hex content hash"))?;
        match (size, pieces) {
            (None, None) => Ok(Self::Single(whole)),
            (Some(size), Some(pieces)) => {
                if size == 0 || size > max_bytes as u64 {
                    return Err(format!(
                        "{what}_size must be between 1 byte and {max_bytes} bytes"
                    ));
                }
                let expected = piece_count(size, INPUT_PIECE_BYTES);
                if pieces.len() != expected {
                    return Err(format!(
                        "{what}_pieces must list {expected} pieces of {INPUT_PIECE_BYTES} bytes for {size} bytes"
                    ));
                }
                let pieces = pieces
                    .iter()
                    .map(|h| Hash::from_hex(h))
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(|_| format!("{what}_pieces must be hex content hashes"))?;
                Ok(Self::Pieces {
                    whole,
                    size,
                    pieces,
                })
            }
            _ => Err(format!("{what}_size and {what}_pieces go together")),
        }
    }

    /// Fetch and verify the whole input into memory.
    pub async fn fetch(
        &self,
        net: &Arc<Libp2pNetwork>,
        provider: Option<PeerId>,
        max_bytes: usize,
    ) -> Result<Vec<u8>> {
        match self {
            Self::Single(hash) => {
                let bytes = fetch_chunk(net, provider, *hash, max_bytes, PIECE_FETCH_TIMEOUT)
                    .await
                    .context("fetch the input")?;
                if bytes.is_empty() {
                    bail!("the input is empty");
                }
                Ok(bytes)
            }
            Self::Pieces {
                whole,
                size,
                pieces,
            } => {
                if *size as usize > max_bytes {
                    bail!("the input is over the {max_bytes}-byte limit");
                }
                let mut out = Vec::with_capacity(*size as usize);
                fetch_pieces_with(
                    net,
                    provider,
                    pieces,
                    *size,
                    *whole,
                    INPUT_PIECE_BYTES,
                    Sink::Memory(&mut out),
                )
                .await
                .context("fetch the input pieces")?;
                Ok(out)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        Hash::of(b).to_hex()
    }

    #[test]
    fn counts() {
        assert_eq!(piece_count(0, 4), 1);
        assert_eq!(piece_count(4, 4), 1);
        assert_eq!(piece_count(5, 4), 2);
        assert_eq!(piece_count(32 * 1024 * 1024, INPUT_PIECE_BYTES), 32);
    }

    #[test]
    fn input_refs() {
        let h = hex(b"x");
        assert_eq!(
            InputRef::parse("input", &h, None, None, 10).unwrap(),
            InputRef::Single(Hash::of(b"x"))
        );
        assert!(
            InputRef::parse("input", "zz", None, None, 10)
                .unwrap_err()
                .contains("input_hash")
        );
        let two = vec![h.clone(), h.clone()];
        let size = INPUT_PIECE_BYTES as u64 + 1;
        let max = 32 * 1024 * 1024;
        assert!(matches!(
            InputRef::parse("audio", &h, Some(size), Some(&two), max).unwrap(),
            InputRef::Pieces { size: s, ref pieces, .. } if s == size && pieces.len() == 2
        ));
        // Wrong count for the size.
        assert!(
            InputRef::parse("audio", &h, Some(size), Some(&two[..1]), max)
                .unwrap_err()
                .contains("2 pieces")
        );
        // Over the cap, zero, half-present, bad hex.
        assert!(InputRef::parse("a", &h, Some(max as u64 + 1), Some(&two), max).is_err());
        assert!(InputRef::parse("a", &h, Some(0), Some(&two), max).is_err());
        assert!(
            InputRef::parse("a", &h, Some(size), None, max)
                .unwrap_err()
                .contains("together")
        );
        assert!(InputRef::parse("a", &h, None, Some(&two), max).is_err());
        let bad = vec![h.clone(), "nope".into()];
        assert!(
            InputRef::parse("a", &h, Some(size), Some(&bad), max)
                .unwrap_err()
                .contains("hex")
        );
    }

    #[test]
    fn spec_fields_stay_single_chunk_when_small() {
        let one = Pieces {
            hash: hex(b"a"),
            size: 1,
            pieces: vec![hex(b"a")],
        };
        let f = one.spec_fields("input");
        assert_eq!(f.len(), 1);
        assert_eq!(f["input_hash"], hex(b"a"));
        let two = Pieces {
            hash: hex(b"ab"),
            size: 2,
            pieces: vec![hex(b"a"), hex(b"b")],
        };
        let f = two.spec_fields("audio");
        assert_eq!(f["audio_size"], 2);
        assert_eq!(f["audio_pieces"].as_array().unwrap().len(), 2);
    }
}
