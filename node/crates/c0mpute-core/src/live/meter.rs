//! Paying relays: bytes served per stream, signed receipts, and the bounds
//! that keep a lying relay from draining a streamer's balance.
//!
//! * Every node counts the bytes of each live stream it serves: segments a
//!   gateway sends to viewers over HTTP, and chunks it sends to other nodes
//!   over libp2p (the [`Tap`] on the node's chunk source).
//! * Every [`EPOCH_SECS`] the node signs one [`RelayReceipt`] per stream it
//!   served bytes of and gossips it on `c0mpute/jobs/live.relay`.
//! * A status aggregator checks the signature against the peer id, bounds
//!   the bytes by what the stream could have carried in one epoch (bitrate x
//!   epoch x viewer cap, from the stream's own signed records) and files it
//!   as a credited job of type `live.relay` ([`Book`]). `GET /jobs` then
//!   lists it like any other job, with
//!   `job_id = live:<stream_id>:<node>:<epoch>` and
//!   `paid_usd = bytes / 1e9 * $0.01` when the stream's record says `paid`,
//!   else 0.
//!
//! Receipts are self-reported. The bound is the node-side limit; the web
//! side adds a per-account spend cap and only bills streams it issued.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use anyhow::{Context, Result};
use c0mpute_net::Libp2pNetwork;
use c0mpute_proto::Hash;
use serde::{Deserialize, Serialize};

use super::playlist;
use super::record::{self, LiveRecord};

/// The workload type relay receipts are credited under.
pub const WORKLOAD: &str = "live.relay";
/// `t` of a [`RelayReceipt`] on the wire.
pub const RECEIPT_TAG: &str = "live_relay";
/// One receipt per stream per peer per this many seconds.
pub const EPOCH_SECS: u64 = 60;
/// What a relay earns per GB served of a paid stream (DIP-0019).
pub const USD_PER_GB: f64 = 0.01;
/// A GB here is 10^9 bytes.
pub const BYTES_PER_GB: f64 = 1e9;
/// Viewers one relay is believed to have served of a paid stream in one
/// epoch. Paid streams have no viewer cap; receipts still need a ceiling.
pub const PAID_VIEWER_BOUND: u64 = 1_000;
/// A receipt is never bounded below this (one small segment always fits).
pub const MIN_EPOCH_BYTES: u64 = 1024 * 1024;
/// Receipts for epochs older than this many epochs are dropped.
pub const MAX_EPOCH_AGE: u64 = 10;
const SIGNING_DOMAIN: &str = "c0mpute-live-receipt-v1";
/// Streams an aggregator tracks at once.
const MAX_BOOK_STREAMS: usize = 20_000;
/// Receipt ids an aggregator remembers at once (for de-duplication).
const MAX_BOOK_SEEN: usize = 500_000;
/// An aggregator forgets a stream with no record for this long.
const BOOK_STREAM_TTL_MS: u64 = 15 * 60 * 1000;

/// The gossip topic receipts travel on.
pub fn topic() -> String {
    c0mpute_net::topics::job_topic(WORKLOAD)
}

/// The epoch a wall-clock time (ms since 1970) falls in.
pub fn epoch_of(unix_ms: u64) -> u64 {
    unix_ms / 1000 / EPOCH_SECS
}

/// The row id status aggregators use for a node: the last 8 characters of
/// its peer id (the `node` field of `GET /jobs` rows).
pub fn short_node(peer: &str) -> String {
    let n = peer.len();
    peer.get(n.saturating_sub(8)..).unwrap_or(peer).to_string()
}

/// What `bytes` of a stream earn: $0.01 per GB when it is paid, else 0.
pub fn usd_for(bytes: u64, paid: bool) -> f64 {
    if paid { bytes as f64 / BYTES_PER_GB * USD_PER_GB } else { 0.0 }
}

/// Bits per second of the whole stream: every rendition's peak, summed.
pub fn bitrate_bps(rec: &LiveRecord) -> u64 {
    rec.body
        .renditions
        .iter()
        .map(|r| playlist::bandwidth(r).0)
        .fold(0u64, |a, b| a.saturating_add(b))
}

/// Viewers a single relay may claim to have served in one epoch: the paid
/// ceiling for a paid stream, else the binding's `max_viewers` or 25. (A
/// stream with no binding has no viewer cap on gateways, and earns nothing,
/// so its bound only keeps the reported bytes sane.)
pub fn viewer_bound(rec: &LiveRecord) -> u64 {
    if rec.is_paid() {
        PAID_VIEWER_BOUND
    } else {
        rec.max_viewers.filter(|n| *n > 0).unwrap_or(record::FREE_MAX_VIEWERS) as u64
    }
}

/// The most bytes one relay can have served of a stream in one epoch:
/// bitrate x epoch x viewers, never under [`MIN_EPOCH_BYTES`].
pub fn max_epoch_bytes(bitrate_bps: u64, viewers: u64) -> u64 {
    (bitrate_bps / 8)
        .saturating_mul(EPOCH_SECS)
        .saturating_mul(viewers.max(1))
        .max(MIN_EPOCH_BYTES)
}

/// "I served this many bytes of this stream in this epoch", signed with the
/// relay's libp2p identity key (its peer id carries the public key).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayReceipt {
    /// Always [`RECEIPT_TAG`].
    pub t: String,
    pub v: u8,
    pub stream_id: String,
    /// The relay's full peer id (base58).
    pub peer: String,
    pub bytes: u64,
    /// `unix_seconds / 60` of the minute the bytes were served in.
    pub epoch: u64,
    /// ed25519 signature (hex) over [`RelayReceipt::signing_bytes`].
    pub sig: String,
}

impl RelayReceipt {
    pub fn signing_bytes(stream_id: &str, peer: &str, bytes: u64, epoch: u64) -> Vec<u8> {
        format!("{SIGNING_DOMAIN}\n{stream_id}\n{peer}\n{bytes}\n{epoch}").into_bytes()
    }

    fn build(stream_id: &str, peer: String, bytes: u64, epoch: u64, sig: Vec<u8>) -> Self {
        RelayReceipt {
            t: RECEIPT_TAG.to_string(),
            v: 1,
            stream_id: stream_id.to_string(),
            peer,
            bytes,
            epoch,
            sig: hex::encode(sig),
        }
    }

    /// Sign with this node's identity.
    pub fn sign(net: &Libp2pNetwork, stream_id: &str, bytes: u64, epoch: u64) -> Result<Self> {
        let peer = net.peer_id().to_base58();
        let sig = net.sign(&Self::signing_bytes(stream_id, &peer, bytes, epoch))?;
        Ok(Self::build(stream_id, peer, bytes, epoch, sig))
    }

    /// Sign with a bare keypair (tests, tools).
    pub fn sign_with(
        keypair: &c0mpute_net::libp2p::identity::Keypair,
        stream_id: &str,
        bytes: u64,
        epoch: u64,
    ) -> Result<Self> {
        let peer = keypair.public().to_peer_id().to_base58();
        let sig = keypair
            .sign(&Self::signing_bytes(stream_id, &peer, bytes, epoch))
            .map_err(|e| anyhow::anyhow!("sign receipt: {e}"))?;
        Ok(Self::build(stream_id, peer, bytes, epoch, sig))
    }

    /// Well formed, and signed by the peer it names.
    pub fn verify(&self) -> bool {
        if self.t != RECEIPT_TAG || self.v != 1 || !record::valid_stream_id(&self.stream_id) {
            return false;
        }
        let Ok(peer) = self.peer.parse::<c0mpute_net::PeerId>() else {
            return false;
        };
        let Ok(sig) = hex::decode(&self.sig) else {
            return false;
        };
        c0mpute_net::identity::verify_peer_signature(
            &peer,
            &Self::signing_bytes(&self.stream_id, &self.peer, self.bytes, self.epoch),
            &sig,
        )
    }

    /// `live:<stream_id>:<node>:<epoch>`, `<node>` being [`short_node`] of
    /// the peer: the id of the credited job this receipt becomes. Always
    /// under 128 characters.
    pub fn job_id(&self) -> String {
        job_id(&self.stream_id, &self.peer, self.epoch)
    }
}

/// See [`RelayReceipt::job_id`].
pub fn job_id(stream_id: &str, peer: &str, epoch: u64) -> String {
    let sid = stream_id.get(..64).unwrap_or(stream_id);
    format!("live:{sid}:{}:{epoch}", short_node(peer))
}

/// Parse a receipt off the wire: `None` unless it is one and it verifies.
pub fn parse_receipt(data: &[u8]) -> Option<RelayReceipt> {
    let r: RelayReceipt = serde_json::from_slice(data).ok()?;
    r.verify().then_some(r)
}

/// Reports every chunk a node serves over libp2p to its live-stream
/// service, which credits the ones that belong to a live stream. Created
/// before the network (the chunk source holds it), attached once the
/// service exists.
#[derive(Default)]
pub struct Tap {
    tx: OnceLock<tokio::sync::mpsc::UnboundedSender<(Hash, u64)>>,
}

impl Tap {
    /// A chunk left this node over libp2p. Free until a service attaches.
    pub fn served(&self, hash: &Hash, len: usize) {
        if let Some(tx) = self.tx.get() {
            let _ = tx.send((*hash, len as u64));
        }
    }

    /// The receiving end, once.
    pub(crate) fn attach(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<(Hash, u64)>> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.tx.set(tx).ok().map(|_| rx)
    }
}

/// One receipt an aggregator accepted, ready to file as a credited job.
#[derive(Clone, Debug, PartialEq)]
pub struct Credit {
    pub job_id: String,
    /// Full peer id of the relay.
    pub peer: String,
    /// Bytes credited (the receipt's, bounded).
    pub bytes: u64,
    pub paid_usd: f64,
    pub completed_at_ms: u64,
}

struct BookEntry {
    paid: bool,
    /// Highest whole-stream bitrate any record showed.
    peak_bps: u64,
    viewers: u64,
    seq: u64,
    bound: bool,
    last_ms: u64,
}

/// The aggregator's side: what it knows of each live stream from the
/// stream's signed records, and which receipts it already credited.
#[derive(Default)]
pub struct Book {
    streams: HashMap<String, BookEntry>,
    seen: HashMap<String, u64>,
    /// When set, only records signed by these owner keys (hex ed25519) can
    /// make a stream paid: the hosted ingest nodes' keys.
    paid_owners: Option<HashSet<String>>,
}

impl Book {
    pub fn new(paid_owners: Option<HashSet<String>>) -> Self {
        Book {
            paid_owners,
            ..Default::default()
        }
    }

    /// `C0MPUTE_LIVE_PAID_OWNERS`: comma-separated owner keys (hex) whose
    /// streams may be paid. Unset: any stream whose record says `paid`.
    pub fn from_env() -> Self {
        let owners = std::env::var("C0MPUTE_LIVE_PAID_OWNERS").ok().map(|v| {
            v.split(',')
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect::<HashSet<String>>()
        });
        Self::new(owners.filter(|o| !o.is_empty()))
    }

    /// A live record arrived. Returns true when it was taken.
    pub fn on_record(&mut self, rec: &LiveRecord, now_ms: u64) -> bool {
        if !rec.verify() {
            return false;
        }
        let id = rec.stream_id().to_string();
        if !self.streams.contains_key(&id) && self.streams.len() >= MAX_BOOK_STREAMS {
            return false;
        }
        let bound = rec.bind_sig.is_some();
        if let Some(e) = self.streams.get(&id)
            && !(rec.body.seq > e.seq || (rec.body.seq == e.seq && bound && !e.bound))
        {
            return false;
        }
        let paid = rec.is_paid() && self.paid_owners.as_ref().is_none_or(|o| o.contains(&rec.body.owner));
        let peak = self.streams.get(&id).map_or(0, |e| e.peak_bps).max(bitrate_bps(rec));
        self.streams.insert(
            id,
            BookEntry {
                paid,
                peak_bps: peak,
                viewers: viewer_bound(rec),
                seq: rec.body.seq,
                bound,
                last_ms: now_ms,
            },
        );
        true
    }

    /// A relay receipt arrived. `Some` when it is signed, recent, for a
    /// stream this aggregator has a record of, and not already credited.
    /// The bytes are cut to what the stream could have carried.
    pub fn on_receipt(&mut self, r: &RelayReceipt, now_ms: u64) -> Option<Credit> {
        if r.bytes == 0 || !r.verify() {
            return None;
        }
        let now_epoch = epoch_of(now_ms);
        if r.epoch > now_epoch || r.epoch + MAX_EPOCH_AGE < now_epoch {
            return None;
        }
        let entry = self.streams.get(&r.stream_id)?;
        let job_id = r.job_id();
        if self.seen.contains_key(&job_id) || self.seen.len() >= MAX_BOOK_SEEN {
            return None;
        }
        let bytes = r.bytes.min(max_epoch_bytes(entry.peak_bps, entry.viewers));
        let paid_usd = usd_for(bytes, entry.paid);
        self.seen.insert(job_id.clone(), r.epoch);
        Some(Credit {
            job_id,
            peer: r.peer.clone(),
            bytes,
            paid_usd,
            completed_at_ms: ((r.epoch + 1) * EPOCH_SECS * 1000).min(now_ms),
        })
    }

    /// Forget quiet streams and receipt ids too old to be replayed.
    pub fn evict(&mut self, now_ms: u64) {
        self.streams.retain(|_, e| now_ms.saturating_sub(e.last_ms) <= BOOK_STREAM_TTL_MS);
        let now_epoch = epoch_of(now_ms);
        self.seen.retain(|_, epoch| *epoch + MAX_EPOCH_AGE + 1 >= now_epoch);
    }

    pub fn streams(&self) -> usize {
        self.streams.len()
    }
}

/// Bytes served per stream since the last receipt, on a relay.
#[derive(Default)]
pub(crate) struct Served(std::sync::Mutex<HashMap<String, u64>>);

impl Served {
    pub(crate) fn add(&self, stream_id: &str, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let e = m.entry(stream_id.to_string()).or_insert(0);
        *e = e.saturating_add(bytes);
    }

    pub(crate) fn take(&self) -> HashMap<String, u64> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }

    pub(crate) fn peek(&self, stream_id: &str) -> u64 {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).get(stream_id).copied().unwrap_or(0)
    }
}

/// Sign one receipt per stream for `epoch`, each cut to its bound. `bounds`
/// gives a stream's most bytes per epoch; a stream missing from it (already
/// forgotten) is bounded by [`MIN_EPOCH_BYTES`].
pub(crate) fn receipts_for(
    net: &Libp2pNetwork,
    served: HashMap<String, u64>,
    bounds: &HashMap<String, u64>,
    epoch: u64,
) -> Vec<RelayReceipt> {
    let mut out = Vec::new();
    for (stream_id, bytes) in served {
        let bytes = bytes.min(bounds.get(&stream_id).copied().unwrap_or(MIN_EPOCH_BYTES));
        if bytes == 0 {
            continue;
        }
        match RelayReceipt::sign(net, &stream_id, bytes, epoch).context("sign relay receipt") {
            Ok(r) => out.push(r),
            Err(e) => tracing::debug!(err = %e, "live: receipt not signed"),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::record::{Binding, LiveBody, Window, describe_segment};
    use c0mpute_net::libp2p::identity::Keypair;

    /// A record whose single rendition runs at about `kbps`.
    fn rec(k: &Keypair, name: &str, seq: u64, kbps: u64, binding: Option<&Binding>) -> LiveRecord {
        let mut w = Window::new("src", None, 6);
        for i in 0..3u8 {
            // 4 s segments of kbps * 500 bytes: kbps * 1000 bits per second.
            let bytes = vec![i; (kbps * 500) as usize];
            w.push(describe_segment(&bytes, 4000, false).0);
        }
        LiveRecord::sign_bound(
            k,
            LiveBody {
                v: 1,
                stream_id: String::new(),
                name: name.into(),
                owner: String::new(),
                seq,
                ts_ms: 1,
                ended: false,
                renditions: vec![w.rendition],
            },
            binding,
        )
        .unwrap()
    }

    fn paid() -> Binding {
        Binding {
            account: "acct_1".into(),
            paid: true,
            max_viewers: None,
        }
    }

    const NOW: u64 = 1_800_000_000_000;

    #[test]
    fn receipt_math() {
        assert_eq!(usd_for(1_000_000_000, true), 0.01);
        assert!((usd_for(2_500_000_000, true) - 0.025).abs() < 1e-12);
        assert_eq!(usd_for(123, true), 123.0 / 1e9 * 0.01);
        assert_eq!(usd_for(5_000_000_000, false), 0.0, "an unpaid stream pays nothing");
        // 1 Mbit/s for 60 s to 25 viewers.
        assert_eq!(max_epoch_bytes(1_000_000, 25), 125_000 * 60 * 25);
        assert_eq!(max_epoch_bytes(0, 25), MIN_EPOCH_BYTES);
        assert_eq!(max_epoch_bytes(u64::MAX, u64::MAX), u64::MAX, "saturates");
        assert_eq!(epoch_of(120_000), 2);
        assert_eq!(epoch_of(179_999), 2);
    }

    #[test]
    fn receipts_sign_verify_and_name_their_job() {
        let k = Keypair::generate_ed25519();
        let sid = "0123456789abcdef0123";
        let r = RelayReceipt::sign_with(&k, sid, 4_000_000, 29_000_000).unwrap();
        assert!(r.verify());
        let peer = k.public().to_peer_id().to_base58();
        assert_eq!(r.peer, peer);
        let node = &peer[peer.len() - 8..];
        assert_eq!(r.job_id(), format!("live:{sid}:{node}:29000000"));
        assert!(r.job_id().len() < 128);
        assert_eq!(short_node(&peer), node);
        // The wire form is the five fields plus the tag, version and signature.
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(v["t"], "live_relay");
        assert_eq!(v["stream_id"], sid);
        assert_eq!(v["bytes"], 4_000_000);
        assert_eq!(v["epoch"], 29_000_000);
        assert_eq!(parse_receipt(&serde_json::to_vec(&r).unwrap()), Some(r.clone()));

        let mut bad = r.clone();
        bad.bytes += 1;
        assert!(!bad.verify(), "inflated bytes");
        let mut bad = r.clone();
        bad.epoch += 1;
        assert!(!bad.verify(), "moved to another epoch");
        let mut bad = r.clone();
        bad.peer = Keypair::generate_ed25519().public().to_peer_id().to_base58();
        assert!(!bad.verify(), "someone else's receipt");
        let mut bad = r.clone();
        bad.stream_id = "not-a-stream".into();
        assert!(!bad.verify());
        assert_eq!(parse_receipt(b"{\"job_id\":\"x\"}"), None);
        // A long stream id is cut so the job id stays short.
        assert!(job_id(&"a".repeat(200), &peer, u64::MAX).len() < 128);
    }

    #[test]
    fn a_paid_stream_credits_bytes_per_gb() {
        let owner = Keypair::generate_ed25519();
        let relay = Keypair::generate_ed25519();
        let mut book = Book::new(None);
        // 8 Mbit/s paid stream.
        let record = rec(&owner, "show", 10, 8_000, Some(&paid()));
        assert!(book.on_record(&record, NOW));
        let epoch = epoch_of(NOW) - 1;
        let r = RelayReceipt::sign_with(&relay, record.stream_id(), 3_000_000_000, epoch).unwrap();
        let c = book.on_receipt(&r, NOW).expect("credited");
        assert_eq!(c.bytes, 3_000_000_000);
        assert!((c.paid_usd - 0.03).abs() < 1e-12);
        assert_eq!(c.job_id, r.job_id());
        assert_eq!(c.peer, r.peer);
        assert_eq!(c.completed_at_ms, (epoch + 1) * 60_000);
        // The same receipt again (gossip echo, replay) is not credited twice.
        assert_eq!(book.on_receipt(&r, NOW), None);
        // Another relay, same stream and epoch, is its own job.
        let other = Keypair::generate_ed25519();
        let r2 = RelayReceipt::sign_with(&other, record.stream_id(), 1_000_000_000, epoch).unwrap();
        assert!((book.on_receipt(&r2, NOW).unwrap().paid_usd - 0.01).abs() < 1e-12);
    }

    #[test]
    fn receipts_are_bounded_by_bitrate_epoch_and_viewers() {
        let owner = Keypair::generate_ed25519();
        let relay = Keypair::generate_ed25519();
        let mut book = Book::new(None);
        // 1 Mbit/s paid stream: at most 125 kB/s x 60 s x 1000 viewers.
        let record = rec(&owner, "show", 10, 1_000, Some(&paid()));
        book.on_record(&record, NOW);
        let bound = max_epoch_bytes(bitrate_bps(&record), PAID_VIEWER_BOUND);
        assert_eq!(bound, 125_000 * 60 * 1_000);
        let r = RelayReceipt::sign_with(&relay, record.stream_id(), u64::MAX / 2, epoch_of(NOW)).unwrap();
        let c = book.on_receipt(&r, NOW).unwrap();
        assert_eq!(c.bytes, bound, "a fantasy receipt is cut to the bound");
        assert!((c.paid_usd - bound as f64 / 1e9 * 0.01).abs() < 1e-9);

        // An unpaid stream: 25 viewers, and nothing is owed whatever it says.
        let free = rec(&owner, "free", 10, 1_000, None);
        book.on_record(&free, NOW);
        let r = RelayReceipt::sign_with(&relay, free.stream_id(), u64::MAX / 2, epoch_of(NOW)).unwrap();
        let c = book.on_receipt(&r, NOW).unwrap();
        assert_eq!(c.bytes, 125_000 * 60 * 25);
        assert_eq!(c.paid_usd, 0.0);
    }

    #[test]
    fn receipts_need_a_known_stream_a_recent_epoch_and_bytes() {
        let owner = Keypair::generate_ed25519();
        let relay = Keypair::generate_ed25519();
        let mut book = Book::new(None);
        let record = rec(&owner, "show", 10, 1_000, Some(&paid()));
        let now_epoch = epoch_of(NOW);
        let r = RelayReceipt::sign_with(&relay, record.stream_id(), 1000, now_epoch).unwrap();
        assert_eq!(book.on_receipt(&r, NOW), None, "no record of the stream yet");
        book.on_record(&record, NOW);
        let old = RelayReceipt::sign_with(&relay, record.stream_id(), 1000, now_epoch - MAX_EPOCH_AGE - 1).unwrap();
        assert_eq!(book.on_receipt(&old, NOW), None, "too old");
        let future = RelayReceipt::sign_with(&relay, record.stream_id(), 1000, now_epoch + 1).unwrap();
        assert_eq!(book.on_receipt(&future, NOW), None, "from the future");
        let zero = RelayReceipt::sign_with(&relay, record.stream_id(), 0, now_epoch).unwrap();
        assert_eq!(book.on_receipt(&zero, NOW), None);
        let mut forged = r.clone();
        forged.bytes = 2000;
        assert_eq!(book.on_receipt(&forged, NOW), None, "bad signature");
        assert!(book.on_receipt(&r, NOW).is_some());
        // A quiet stream is forgotten, and old ids with it.
        book.evict(NOW + BOOK_STREAM_TTL_MS + 60_000);
        assert_eq!(book.streams(), 0);
    }

    #[test]
    fn paid_can_be_limited_to_known_ingest_owners() {
        let ingest = Keypair::generate_ed25519();
        let stranger = Keypair::generate_ed25519();
        let relay = Keypair::generate_ed25519();
        let ingest_hex = record::owner_hex(&ingest).unwrap();
        let mut book = Book::new(Some([ingest_hex].into_iter().collect()));
        let good = rec(&ingest, "show", 10, 1_000, Some(&paid()));
        let fake = rec(&stranger, "show", 10, 1_000, Some(&paid()));
        book.on_record(&good, NOW);
        book.on_record(&fake, NOW);
        let e = epoch_of(NOW);
        let r = RelayReceipt::sign_with(&relay, good.stream_id(), 1_000_000_000, e).unwrap();
        assert!(book.on_receipt(&r, NOW).unwrap().paid_usd > 0.0);
        let r = RelayReceipt::sign_with(&relay, fake.stream_id(), 1_000_000_000, e).unwrap();
        assert_eq!(book.on_receipt(&r, NOW).unwrap().paid_usd, 0.0, "a stranger cannot declare itself paid");
    }

    #[test]
    fn a_stream_that_runs_out_of_balance_stops_paying() {
        let owner = Keypair::generate_ed25519();
        let relay = Keypair::generate_ed25519();
        let mut book = Book::new(None);
        let record = rec(&owner, "show", 10, 8_000, Some(&paid()));
        book.on_record(&record, NOW);
        let free = Binding {
            account: "acct_1".into(),
            paid: false,
            max_viewers: Some(25),
        };
        assert!(book.on_record(&rec(&owner, "show", 11, 8_000, Some(&free)), NOW));
        assert!(!book.on_record(&record, NOW), "the older paid record does not come back");
        let r = RelayReceipt::sign_with(&relay, record.stream_id(), 1_000_000_000, epoch_of(NOW)).unwrap();
        assert_eq!(book.on_receipt(&r, NOW).unwrap().paid_usd, 0.0);
    }

    #[test]
    fn served_counters_add_and_drain() {
        let s = Served::default();
        s.add("a", 10);
        s.add("a", 5);
        s.add("b", 1);
        s.add("c", 0);
        assert_eq!(s.peek("a"), 15);
        let m = s.take();
        assert_eq!(m.len(), 2);
        assert_eq!(m["a"], 15);
        assert_eq!(s.peek("a"), 0);
    }
}
