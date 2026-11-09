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
        return Err(Mismatch("piece does not match its hash or is too big").into());
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
        return Err(Mismatch("the piece count does not fit the size").into());
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
                return Err(Mismatch("pieces do not add up to the size").into());
            }
            hasher.update(&bytes);
            match &mut sink {
                Sink::File(file) => file.write_all(&bytes).await?,
                Sink::Memory(out) => out.extend_from_slice(&bytes),
            }
        }
    }
    if written != size || Hash(*hasher.finalize().as_bytes()) != whole {
        return Err(Mismatch("reassembled file does not match its hash").into());
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

    /// The ref for stored [`Pieces`] (one piece: a single chunk).
    pub fn of(p: &Pieces) -> Result<Self> {
        let whole = Hash::from_hex(&p.hash).context("piece hash")?;
        if p.pieces.len() <= 1 {
            return Ok(Self::Single(whole));
        }
        Ok(Self::Pieces {
            whole,
            size: p.size,
            pieces: p
                .pieces
                .iter()
                .map(|h| Hash::from_hex(h))
                .collect::<std::result::Result<Vec<_>, _>>()
                .context("piece hash")?,
        })
    }

    /// Fetch and verify the whole input into `dest` (streamed for pieces:
    /// never whole in memory), renamed into place only once every check passed.
    pub async fn fetch_to_file(
        &self,
        net: &Arc<Libp2pNetwork>,
        provider: Option<PeerId>,
        max_bytes: usize,
        dest: &Path,
    ) -> Result<()> {
        match self {
            Self::Single(_) => {
                let bytes = self.fetch(net, provider, max_bytes).await?;
                let tmp = dest.with_extension(format!("part-{}", uuid::Uuid::new_v4()));
                tokio::fs::write(&tmp, &bytes).await?;
                tokio::fs::rename(&tmp, dest).await?;
                Ok(())
            }
            Self::Pieces {
                whole,
                size,
                pieces,
            } => {
                if *size as usize > max_bytes {
                    bail!("the input is over the {max_bytes}-byte limit");
                }
                fetch_pieces_to_file(net, provider, pieces, *size, *whole, INPUT_PIECE_BYTES, dest)
                    .await
                    .context("fetch the input pieces")
            }
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

/// A piece check failed: the holder sent bytes that are not what was named.
/// Callers downcast to tell bad bytes from an unreachable holder.
#[derive(Debug, Clone, Copy)]
pub struct Mismatch(pub &'static str);

impl std::fmt::Display for Mismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Mismatch {}

// ─── worker results ──────────────────────────────────────────────────────
//
// A worker's result goes back to the buyer by the hash its receipt names.
// Results up to [`RESULT_SINGLE_CHUNK_MAX`] are that one chunk, exactly as
// before pieces existed, so every buyer version can fetch them. A bigger
// result is stored as [`RESULT_PIECE_BYTES`] pieces and the receipt names a
// small manifest chunk instead: [`MANIFEST_MAGIC`] followed by JSON
// `{v, size, hash, piece_bytes, pieces}`. The magic cannot begin a JSON,
// WAV, PNG or MP4 result, so a buyer tells the two apart from the first
// bytes. Buyers that predate this fetch the manifest and reject it as a
// malformed result.

/// Largest result a worker still returns as one chunk.
pub const RESULT_SINGLE_CHUNK_MAX: usize = 1024 * 1024;
/// Piece size for results over [`RESULT_SINGLE_CHUNK_MAX`].
pub const RESULT_PIECE_BYTES: usize = 1024 * 1024;
/// First bytes of a result manifest chunk.
pub const MANIFEST_MAGIC: &[u8] = b"C0MPUTE-PIECES/1\n";
/// Largest manifest a buyer accepts (a 2 GiB transcode output in 1 MiB
/// pieces is about 140 KiB of manifest).
pub const MAX_MANIFEST_BYTES: usize = 512 * 1024;
/// Smallest and largest piece size a manifest may name.
const MIN_MANIFEST_PIECE_BYTES: usize = 64 * 1024;
const MAX_MANIFEST_PIECE_BYTES: usize = 4 * 1024 * 1024;

/// A pieced result, as the manifest chunk describes it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResultManifest {
    pub v: u32,
    /// Bytes in the whole result.
    pub size: u64,
    /// Hex hash of the whole result.
    pub hash: String,
    pub piece_bytes: u64,
    /// Hex piece hashes, in order.
    pub pieces: Vec<String>,
}

/// A manifest after its checks: what to fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedManifest {
    pub whole: Hash,
    pub size: u64,
    pub piece_bytes: usize,
    pub pieces: Vec<Hash>,
}

impl ResultManifest {
    fn of(pieces: &Pieces, piece_bytes: usize) -> Self {
        Self {
            v: 1,
            size: pieces.size,
            hash: pieces.hash.clone(),
            piece_bytes: piece_bytes as u64,
            pieces: pieces.pieces.clone(),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = MANIFEST_MAGIC.to_vec();
        out.extend_from_slice(&serde_json::to_vec(self).expect("manifest serializes"));
        out
    }

    /// `None` when `bytes` is not a manifest at all (a plain single-chunk
    /// result); `Some(Err)` when it claims to be one but is malformed.
    pub fn decode(bytes: &[u8]) -> Option<Result<Self>> {
        let body = bytes.strip_prefix(MANIFEST_MAGIC)?;
        Some(
            serde_json::from_slice::<Self>(body)
                .map_err(|_| anyhow::Error::from(Mismatch("result manifest is not valid JSON"))),
        )
    }

    /// Check every field against the buyer's `max_bytes` cap.
    pub fn check(&self, max_bytes: usize) -> Result<CheckedManifest> {
        let bad = |why: &'static str| -> anyhow::Error { Mismatch(why).into() };
        if self.v != 1 {
            return Err(bad("unknown result manifest version"));
        }
        if self.size == 0 {
            return Err(bad("result manifest names an empty result"));
        }
        if self.size > max_bytes as u64 {
            return Err(bad("result is over the size limit"));
        }
        let piece_bytes = self.piece_bytes as usize;
        if !(MIN_MANIFEST_PIECE_BYTES..=MAX_MANIFEST_PIECE_BYTES).contains(&piece_bytes) {
            return Err(bad("result manifest piece size is out of range"));
        }
        if self.pieces.len() != piece_count(self.size, piece_bytes) {
            return Err(bad("the piece count does not fit the size"));
        }
        let whole =
            Hash::from_hex(&self.hash).map_err(|_| bad("result manifest hash is not hex"))?;
        let pieces = self
            .pieces
            .iter()
            .map(|h| Hash::from_hex(h))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| bad("result manifest piece hashes are not hex"))?;
        Ok(CheckedManifest {
            whole,
            size: self.size,
            piece_bytes,
            pieces,
        })
    }
}

async fn store_manifest(
    net: &Libp2pNetwork,
    store: &ChunkStore,
    pieces: &Pieces,
) -> Result<String> {
    let manifest = ResultManifest::of(pieces, RESULT_PIECE_BYTES).encode();
    let hash = store
        .put(&manifest)
        .await
        .context("store result manifest")?;
    net.announce(&hash)
        .await
        .context("announce result manifest")?;
    Ok(hash.to_hex())
}

/// Store and announce a worker result held in memory; returns the hash the
/// receipt names. Up to [`RESULT_SINGLE_CHUNK_MAX`] it is the result's own
/// hash (one chunk, readable by every buyer); above that it is a manifest
/// over [`RESULT_PIECE_BYTES`] pieces.
pub async fn publish_result_bytes(
    net: &Libp2pNetwork,
    store: &ChunkStore,
    bytes: &[u8],
) -> Result<String> {
    if bytes.is_empty() {
        bail!("cannot publish an empty result");
    }
    if bytes.len() <= RESULT_SINGLE_CHUNK_MAX {
        let hash = store.put(bytes).await.context("store result")?;
        net.announce(&hash).await.context("announce result")?;
        return Ok(hash.to_hex());
    }
    let mut pieces = Vec::with_capacity(piece_count(bytes.len() as u64, RESULT_PIECE_BYTES));
    for piece in bytes.chunks(RESULT_PIECE_BYTES) {
        let hash = store.put(piece).await.context("store result piece")?;
        net.announce(&hash).await.context("announce result piece")?;
        pieces.push(hash.to_hex());
    }
    let pieces = Pieces {
        hash: Hash::of(bytes).to_hex(),
        size: bytes.len() as u64,
        pieces,
    };
    store_manifest(net, store, &pieces).await
}

/// [`publish_result_bytes`] for a result on disk, streamed: a multi-GiB
/// transcode output is never in memory whole. Empty or over-`max_bytes`
/// files are refused. The file is left in place.
pub async fn publish_result_file(
    net: &Libp2pNetwork,
    store: &ChunkStore,
    path: &Path,
    max_bytes: u64,
) -> Result<String> {
    let size = tokio::fs::metadata(path)
        .await
        .with_context(|| format!("result file {} is missing", path.display()))?
        .len();
    if size == 0 {
        bail!("result {} is empty", path.display());
    }
    if size > max_bytes {
        bail!("result is {size} bytes, over the {max_bytes}-byte result limit");
    }
    if size as usize <= RESULT_SINGLE_CHUNK_MAX {
        let bytes = tokio::fs::read(path).await?;
        return publish_result_bytes(net, store, &bytes).await;
    }
    let pieces = put_file_pieces(net, store, path, max_bytes, RESULT_PIECE_BYTES).await?;
    store_manifest(net, store, &pieces).await
}

/// Fetch the pieces a checked manifest names into memory.
pub async fn fetch_manifest_pieces(
    net: &Arc<Libp2pNetwork>,
    provider: Option<PeerId>,
    m: &CheckedManifest,
) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(m.size as usize);
    fetch_pieces_with(
        net,
        provider,
        &m.pieces,
        m.size,
        m.whole,
        m.piece_bytes,
        Sink::Memory(&mut out),
    )
    .await?;
    Ok(out)
}

/// Fetch the pieces a checked manifest names straight to `dest`, a piece at
/// a time (renamed into place only once every check passed).
pub async fn fetch_manifest_pieces_to_file(
    net: &Arc<Libp2pNetwork>,
    provider: Option<PeerId>,
    m: &CheckedManifest,
    dest: &Path,
) -> Result<()> {
    fetch_pieces_to_file(
        net,
        provider,
        &m.pieces,
        m.size,
        m.whole,
        m.piece_bytes,
        dest,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_manifests_round_trip_and_check() {
        let p = Pieces {
            hash: hex(b"whole"),
            size: RESULT_PIECE_BYTES as u64 + 1,
            pieces: vec![hex(b"a"), hex(b"b")],
        };
        let m = ResultManifest::of(&p, RESULT_PIECE_BYTES);
        let bytes = m.encode();
        assert!(bytes.starts_with(MANIFEST_MAGIC));
        let back = ResultManifest::decode(&bytes).unwrap().unwrap();
        assert_eq!(back, m);
        let c = back.check(16 * 1024 * 1024).unwrap();
        assert_eq!(c.pieces.len(), 2);
        assert_eq!(c.whole, Hash::of(b"whole"));
        // Over the buyer's cap.
        assert!(back.check(RESULT_PIECE_BYTES).is_err());
        // Plain results are not manifests: JSON, WAV, PNG, MP4.
        for plain in [
            &b"{\"ok\":true}"[..],
            b"RIFF....WAVE",
            b"\x89PNG\r\n\x1a\n",
            b"\0\0\0\x18ftypmp42",
        ] {
            assert!(ResultManifest::decode(plain).is_none());
        }
        // Claims to be one but is not JSON.
        let mut junk = MANIFEST_MAGIC.to_vec();
        junk.extend_from_slice(b"nope");
        assert!(ResultManifest::decode(&junk).unwrap().is_err());
        // Wrong count, bad piece size, bad version, empty, bad hex.
        let mut w = m.clone();
        w.pieces.pop();
        assert!(w.check(usize::MAX).is_err());
        let mut w = m.clone();
        w.piece_bytes = 1;
        assert!(w.check(usize::MAX).is_err());
        let mut w = m.clone();
        w.v = 2;
        assert!(w.check(usize::MAX).is_err());
        let mut w = m.clone();
        w.size = 0;
        assert!(w.check(usize::MAX).is_err());
        let mut w = m.clone();
        w.pieces[0] = "zz".into();
        assert!(w.check(usize::MAX).is_err());
        // A 2 GiB result's manifest fits the manifest cap.
        let big = Pieces {
            hash: hex(b"x"),
            size: 2 * 1024 * 1024 * 1024,
            pieces: vec![hex(b"x"); 2048],
        };
        assert!(ResultManifest::of(&big, RESULT_PIECE_BYTES).encode().len() < MAX_MANIFEST_BYTES);
    }

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
