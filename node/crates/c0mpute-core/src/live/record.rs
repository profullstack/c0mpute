//! What a live stream is on the wire: a signed [`LiveRecord`] from the
//! broadcaster's ed25519 key + stream name, carrying the current playlist
//! window as segment hashes, durations and media sequence numbers.
//!
//! Same trust model as hosting's site records: the stream id is derived from
//! the owner key and the name, so only the owner can publish a new window;
//! every segment is pinned by its blake3 hash (and each 1 MiB chunk of it by
//! its own hash), so a relay can verify every byte it forwards.

use anyhow::{Context, Result, bail};
use c0mpute_proto::Hash;
use serde::{Deserialize, Serialize};

const SIGNING_DOMAIN: &str = "c0mpute-live-v1";

/// Segments are carried over libp2p in chunks of this size (the chunk
/// protocol's comfortable size; see hosting's `FILE_CHUNK_BYTES`).
pub const CHUNK_BYTES: usize = 1024 * 1024;
/// Longest stream name.
pub const MAX_NAME_CHARS: usize = 63;
/// Most renditions (quality levels) in one stream.
pub const MAX_RENDITIONS: usize = 3;
/// Most segments in one rendition's window.
pub const MAX_WINDOW: usize = 20;
/// Most chunks in one segment (so 8 MiB is the largest segment).
pub const MAX_CHUNKS_PER_SEGMENT: usize = 8;
/// Longest segment, in milliseconds.
pub const MAX_SEGMENT_MS: u32 = 30_000;

/// One media segment of the live window.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    /// blake3 (hex) of the whole segment: its name in playlists and URLs.
    pub hash: String,
    /// blake3 (hex) of each [`CHUNK_BYTES`] piece, in order. A segment that
    /// fits in one chunk has `chunks == [hash]`.
    pub chunks: Vec<String>,
    pub size: u64,
    /// Duration in milliseconds (integers sign and round-trip exactly).
    pub dur_ms: u32,
    /// An `EXT-X-DISCONTINUITY` precedes this segment (encoder restart,
    /// input reconnect, codec change).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disc: bool,
}

/// One quality level: its own window of segments.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rendition {
    /// URL path component: 1-16 of a-z, 0-9.
    pub name: String,
    /// `RESOLUTION=` for the master playlist, e.g. `854x480`, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
    /// `EXT-X-MEDIA-SEQUENCE`: the sequence number of `segments[0]`.
    pub media_seq: u64,
    /// `EXT-X-DISCONTINUITY-SEQUENCE`: discontinuities that have left the window.
    #[serde(default)]
    pub disc_seq: u64,
    pub segments: Vec<Segment>,
}

/// The fields the owner signs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveBody {
    pub v: u8,
    pub stream_id: String,
    pub name: String,
    /// Owner's ed25519 public key, hex.
    pub owner: String,
    /// Version: strictly increasing; higher wins.
    pub seq: u64,
    /// Broadcaster's wall clock (ms since the epoch) when it signed this.
    pub ts_ms: u64,
    /// The broadcast is over: playlists get `EXT-X-ENDLIST`.
    #[serde(default)]
    pub ended: bool,
    pub renditions: Vec<Rendition>,
}

/// A signed live window, gossiped on `c0mpute/live/v1`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveRecord {
    #[serde(flatten)]
    pub body: LiveBody,
    /// ed25519 signature (hex) over [`LiveBody::signing_bytes`].
    pub sig: String,
}

/// The id a stream with this owner and name always has: 20 hex characters.
pub fn stream_id_for(owner_hex: &str, name: &str) -> String {
    let h = Hash::of(format!("c0mpute-live\n{owner_hex}\n{name}").as_bytes()).to_hex();
    h[..20].to_string()
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= MAX_NAME_CHARS
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

pub fn valid_rendition_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 16 && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

pub fn valid_stream_id(id: &str) -> bool {
    id.len() == 20 && id.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// Hex ed25519 public key of a keypair, when it is ed25519.
pub fn owner_hex(keypair: &c0mpute_net::libp2p::identity::Keypair) -> Option<String> {
    crate::hosting::site::owner_hex(keypair)
}

/// Split a segment into chunks and describe it.
pub fn describe_segment(bytes: &[u8], dur_ms: u32, disc: bool) -> (Segment, Vec<Vec<u8>>) {
    let pieces: Vec<Vec<u8>> = if bytes.is_empty() {
        vec![Vec::new()]
    } else {
        bytes.chunks(CHUNK_BYTES).map(|c| c.to_vec()).collect()
    };
    let seg = Segment {
        hash: Hash::of(bytes).to_hex(),
        chunks: pieces.iter().map(|p| Hash::of(p).to_hex()).collect(),
        size: bytes.len() as u64,
        dur_ms,
        disc,
    };
    (seg, pieces)
}

impl LiveBody {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut out = format!("{SIGNING_DOMAIN}\n").into_bytes();
        out.extend(serde_json::to_vec(self).expect("live body serializes"));
        out
    }

    fn check(&self) -> Result<()> {
        if self.v != 1 {
            bail!("unsupported live record version {}", self.v);
        }
        if !valid_name(&self.name) {
            bail!("stream name must be 1-63 of a-z, 0-9 and '-'");
        }
        if self.stream_id != stream_id_for(&self.owner, &self.name) {
            bail!("stream id does not match owner and name");
        }
        if self.renditions.is_empty() || self.renditions.len() > MAX_RENDITIONS {
            bail!("a stream has 1-{MAX_RENDITIONS} renditions");
        }
        let mut names = std::collections::HashSet::new();
        for r in &self.renditions {
            if !valid_rendition_name(&r.name) || !names.insert(r.name.as_str()) {
                bail!("bad or duplicate rendition name {:?}", r.name);
            }
            if r.segments.len() > MAX_WINDOW {
                bail!("rendition {} has more than {MAX_WINDOW} segments", r.name);
            }
            if let Some(res) = &r.resolution
                && (res.len() > 16 || !res.chars().all(|c| c.is_ascii_digit() || c == 'x'))
            {
                bail!("bad resolution {res:?}");
            }
            for s in &r.segments {
                if Hash::from_hex(&s.hash).is_err()
                    || s.chunks.is_empty()
                    || s.chunks.len() > MAX_CHUNKS_PER_SEGMENT
                    || s.chunks.iter().any(|c| Hash::from_hex(c).is_err())
                    || s.chunks.len() != (s.size as usize).div_ceil(CHUNK_BYTES).max(1)
                    || s.dur_ms == 0
                    || s.dur_ms > MAX_SEGMENT_MS
                {
                    bail!("bad segment {}", s.hash);
                }
            }
        }
        Ok(())
    }
}

impl LiveRecord {
    /// Sign a body with the owner's libp2p (ed25519) keypair. Fills in
    /// `owner` and `stream_id` from the key and name.
    pub fn sign(keypair: &c0mpute_net::libp2p::identity::Keypair, mut body: LiveBody) -> Result<Self> {
        let owner = owner_hex(keypair).context("live streams need an ed25519 identity")?;
        body.stream_id = stream_id_for(&owner, &body.name);
        body.owner = owner;
        body.check()?;
        let sig = keypair
            .sign(&body.signing_bytes())
            .map_err(|e| anyhow::anyhow!("sign live record: {e}"))?;
        Ok(LiveRecord {
            body,
            sig: hex::encode(sig),
        })
    }

    /// True when the fields are sane, the id matches, and the owner signed it.
    pub fn verify(&self) -> bool {
        if self.body.check().is_err() {
            return false;
        }
        let (Ok(key), Ok(sig)) = (hex::decode(&self.body.owner), hex::decode(&self.sig)) else {
            return false;
        };
        let Ok(public) = c0mpute_net::libp2p::identity::ed25519::PublicKey::try_from_bytes(&key) else {
            return false;
        };
        public.verify(&self.body.signing_bytes(), &sig)
    }

    pub fn stream_id(&self) -> &str {
        &self.body.stream_id
    }

    pub fn rendition(&self, name: &str) -> Option<&Rendition> {
        self.body.renditions.iter().find(|r| r.name == name)
    }

    /// Every segment in the window, any rendition.
    pub fn segments(&self) -> impl Iterator<Item = &Segment> {
        self.body.renditions.iter().flat_map(|r| r.segments.iter())
    }
}

/// Should `new` replace `current`? Only a verified record with a higher
/// `seq` for the same stream.
pub fn supersedes(current: Option<&LiveRecord>, new: &LiveRecord) -> bool {
    if !new.verify() {
        return false;
    }
    match current {
        Some(cur) => cur.body.stream_id == new.body.stream_id && new.body.seq > cur.body.seq,
        None => true,
    }
}

/// The broadcaster's rolling window for one rendition: media sequence and
/// discontinuity sequence bookkeeping per the HLS spec (RFC 8216 6.2.2).
#[derive(Clone, Debug)]
pub struct Window {
    pub rendition: Rendition,
    pub size: usize,
}

impl Window {
    pub fn new(name: &str, resolution: Option<String>, size: usize) -> Self {
        Self {
            rendition: Rendition {
                name: name.to_string(),
                resolution,
                media_seq: 0,
                disc_seq: 0,
                segments: Vec::new(),
            },
            size: size.clamp(1, MAX_WINDOW),
        }
    }

    /// Add the newest segment; slide the oldest out when the window is full.
    /// Returns the segments that left the window.
    pub fn push(&mut self, seg: Segment) -> Vec<Segment> {
        self.rendition.segments.push(seg);
        let mut gone = Vec::new();
        while self.rendition.segments.len() > self.size {
            let old = self.rendition.segments.remove(0);
            self.rendition.media_seq += 1;
            // A discontinuity tag that leaves the playlist bumps the
            // discontinuity sequence so players keep timelines aligned.
            if old.disc {
                self.rendition.disc_seq += 1;
            }
            gone.push(old);
        }
        gone
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(name: &str, seq: u64, segs: usize) -> LiveBody {
        let mut w = Window::new("src", None, 6);
        for i in 0..segs {
            let (s, _) = describe_segment(format!("segment {i}").as_bytes(), 4000, false);
            w.push(s);
        }
        LiveBody {
            v: 1,
            stream_id: String::new(),
            name: name.into(),
            owner: String::new(),
            seq,
            ts_ms: 1,
            ended: false,
            renditions: vec![w.rendition],
        }
    }

    #[test]
    fn sign_verify_and_tamper() {
        let k = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let rec = LiveRecord::sign(&k, body("show", 1, 3)).unwrap();
        assert!(rec.verify());
        assert_eq!(rec.body.stream_id, stream_id_for(&rec.body.owner, "show"));
        assert!(valid_stream_id(&rec.body.stream_id));
        // round trip through JSON keeps the signature valid
        let back: LiveRecord = serde_json::from_slice(&serde_json::to_vec(&rec).unwrap()).unwrap();
        assert!(back.verify());

        let mut bad = rec.clone();
        bad.body.renditions[0].segments[0].dur_ms = 5000;
        assert!(!bad.verify(), "edited duration");
        let mut bad = rec.clone();
        bad.body.seq += 1;
        assert!(!bad.verify(), "edited seq");
        let mut bad = rec.clone();
        bad.body.name = "other".into();
        assert!(!bad.verify(), "renamed");
        // someone else's key cannot publish under this stream id
        let k2 = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let mut forged = LiveRecord::sign(&k2, body("show", 99, 3)).unwrap();
        assert_ne!(forged.body.stream_id, rec.body.stream_id);
        forged.body.stream_id = rec.body.stream_id.clone();
        assert!(!forged.verify());
    }

    #[test]
    fn ordering_by_seq() {
        let k = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let r1 = LiveRecord::sign(&k, body("show", 10, 2)).unwrap();
        let r2 = LiveRecord::sign(&k, body("show", 11, 3)).unwrap();
        assert!(supersedes(None, &r1));
        assert!(supersedes(Some(&r1), &r2));
        assert!(!supersedes(Some(&r2), &r1), "older loses");
        assert!(!supersedes(Some(&r2), &r2), "equal loses");
        let other = LiveRecord::sign(&k, body("other", 12, 1)).unwrap();
        assert!(!supersedes(Some(&r2), &other), "different stream");
    }

    #[test]
    fn rejects_bad_fields() {
        let k = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        assert!(LiveRecord::sign(&k, body("Bad Name", 1, 1)).is_err());
        let mut b = body("ok", 1, 1);
        b.renditions[0].segments[0].dur_ms = 0;
        assert!(LiveRecord::sign(&k, b).is_err());
        let mut b = body("ok", 1, 1);
        b.renditions.clear();
        assert!(LiveRecord::sign(&k, b).is_err());
        let mut b = body("ok", 1, 1);
        b.renditions[0].segments[0].chunks.push(Hash::of(b"x").to_hex());
        assert!(LiveRecord::sign(&k, b).is_err(), "chunk count must match size");
    }

    #[test]
    fn window_slides_media_and_discontinuity_sequence() {
        let mut w = Window::new("src", None, 3);
        let seg = |i: u32, disc: bool| describe_segment(&i.to_le_bytes(), 2000, disc).0;
        assert!(w.push(seg(0, false)).is_empty());
        w.push(seg(1, false));
        w.push(seg(2, true)); // encoder restarted before segment 2
        assert_eq!(w.rendition.media_seq, 0);
        let gone = w.push(seg(3, false));
        assert_eq!(gone.len(), 1);
        assert_eq!(w.rendition.media_seq, 1);
        assert_eq!(w.rendition.disc_seq, 0);
        w.push(seg(4, false));
        assert_eq!(w.rendition.disc_seq, 0, "disc segment still in window");
        w.push(seg(5, false)); // segment 2 (disc) leaves
        assert_eq!(w.rendition.media_seq, 3);
        assert_eq!(w.rendition.disc_seq, 1);
        assert_eq!(w.rendition.segments.len(), 3);
    }

    #[test]
    fn big_segments_split_into_chunks() {
        let bytes = vec![7u8; CHUNK_BYTES * 2 + 10];
        let (s, pieces) = describe_segment(&bytes, 4000, false);
        assert_eq!(s.chunks.len(), 3);
        assert_eq!(pieces.len(), 3);
        assert_eq!(pieces.concat(), bytes);
        let small = describe_segment(b"tiny", 1000, false).0;
        assert_eq!(small.chunks, vec![small.hash.clone()]);
    }

    #[test]
    fn worst_case_record_fits_gossipsub() {
        let k = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let mut renditions = Vec::new();
        for r in 0..MAX_RENDITIONS {
            let mut w = Window::new(&format!("r{r}aaaaaaaaaaaaa"), Some("1920x1080".into()), MAX_WINDOW);
            for i in 0..MAX_WINDOW {
                let h = Hash::of(format!("{r}-{i}").as_bytes()).to_hex();
                w.push(Segment {
                    hash: h.clone(),
                    chunks: vec![h; MAX_CHUNKS_PER_SEGMENT],
                    size: (CHUNK_BYTES * MAX_CHUNKS_PER_SEGMENT) as u64,
                    dur_ms: MAX_SEGMENT_MS,
                    disc: true,
                });
            }
            w.rendition.media_seq = u64::MAX / 2;
            renditions.push(w.rendition);
        }
        let rec = LiveRecord::sign(
            &k,
            LiveBody {
                v: 1,
                stream_id: String::new(),
                name: "a".repeat(MAX_NAME_CHARS),
                owner: String::new(),
                seq: u64::MAX,
                ts_ms: u64::MAX,
                ended: true,
                renditions,
            },
        )
        .unwrap();
        let msg = serde_json::to_vec(&super::super::LiveMsg::Record { record: rec }).unwrap();
        assert!(msg.len() < 60 * 1024, "{} bytes", msg.len());
    }
}
