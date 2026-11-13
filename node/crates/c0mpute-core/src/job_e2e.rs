//! End-to-end encrypted jobs (DIP-0032).
//!
//! Job offers go to gossip topics that every subscriber and relay can read,
//! so an offer carries only what bidding needs. The spec, the inputs and the
//! result travel encrypted between the buyer and the one worker that won:
//!
//! ```text
//! buyer  -> gossip  offer   public_spec (model, engine, size buckets), buyer X25519 key B,
//!                           spec_hash = salted commitment to the spec
//! worker -> buyer   bid     worker X25519 key W, signed by the worker's libp2p identity
//! buyer  -> both    accept  names the winner, carries NO spec
//! winner -> buyer   rpc     `c0mpute.job.payload.v1`: the buyer answers only the winner,
//!                           with the spec sealed to W (point to point, never gossip)
//! winner -> chunk   result  sealed to the buyer; the receipt names the ciphertext hash
//! ```
//!
//! Keys: X25519(B, W) -> HKDF-SHA256 (salt = domain, info = label plus a
//! length-prefixed binding of job id, workload, both peer ids and both keys)
//! -> one AES-256-GCM key for the spec and one for the result. The sealed spec
//! has the same binding as associated data, so it cannot be moved to another
//! job, workload, buyer or worker. Both X25519 keys are fresh per auction and
//! per bid, so a later key compromise opens nothing old.
//!
//! Inputs too large for the spec (audio, images, documents, corpora, .blend
//! files) are sealed by the buyer under a random per-input key before they
//! are stored as chunks; the key rides inside the sealed spec. Results are
//! sealed by the worker under the result key before they are stored. Chunks
//! are therefore ciphertext on every node and on the wire, whoever fetches
//! them.
//!
//! What this does not hide: the winning worker sees the plaintext it computes
//! on (it is decrypted in memory and in the job's scratch directory, which is
//! removed when the job ends). Sensitive work uses private mode (own or
//! allowlisted nodes) or the `confidential` requirement (an attested TEE, see
//! [`AttestationVerifier`]).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use c0mpute_net::topics::{BidE2e, JOB_E2E_VERSION, JobAccept, JobBid, JobKey, JobOffer, OfferE2e};
use c0mpute_net::{Libp2pNetwork, PeerId, RpcHandler};
use hkdf::Hkdf;
use rand::RngCore;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroizing;

pub use c0mpute_net::topics::{CONFIDENTIAL_TAG, JOB_E2E_TAG};

/// Domain separator for every key, nonce context and signature here.
pub const DOMAIN: &[u8] = b"c0mpute/job-e2e/v1";
/// Request-response service the winner fetches its sealed spec from.
pub const PAYLOAD_SERVICE: &str = "c0mpute.job.payload.v1";
/// Largest sealed spec a worker accepts (the largest spec is 1 MiB).
pub const MAX_SEALED_SPEC: usize = 4 * 1024 * 1024;
/// Largest sealed spec pushed on the direct accept (one request carries at
/// most 1 MiB, and the accept is JSON with the spec in base64).
pub const MAX_PUSHED_SPEC: usize = 512 * 1024;
/// How long a buyer keeps a sealed spec for its winner to fetch.
const PAYLOAD_TTL: Duration = Duration::from_secs(10 * 60);
/// Plaintext bytes per sealed segment of a blob.
pub const BLOB_SEGMENT: usize = 64 * 1024;
const TAG_LEN: usize = 16;
const BLOB_MAGIC: &[u8; 8] = b"C0JOBE1\0";
/// Bytes before the first segment of a sealed blob: magic and salt.
pub const BLOB_HEADER: usize = 8 + 32;
/// Strings longer than this never go in a public spec, whatever the key.
const MAX_PUBLIC_STRING: usize = 128;

// ─── bindings and keys ───────────────────────────────────────────────────

/// Everything a job's keys and ciphertexts are bound to.
#[derive(Clone, Copy, Debug)]
pub struct Binding<'a> {
    pub job_id: &'a str,
    pub workload: &'a str,
    pub buyer_peer: &'a str,
    pub worker_peer: &'a str,
    /// Buyer X25519 public key, hex.
    pub buyer_key: &'a str,
    /// Worker X25519 public key, hex.
    pub worker_key: &'a str,
}

fn lp(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u32).to_be_bytes());
    out.extend_from_slice(b);
}

impl Binding<'_> {
    /// Unambiguous encoding: every field length-prefixed, under `label`.
    pub fn encode(&self, label: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(256);
        lp(&mut out, DOMAIN);
        lp(&mut out, label);
        for f in [
            self.job_id,
            self.workload,
            self.buyer_peer,
            self.worker_peer,
            self.buyer_key,
            self.worker_key,
        ] {
            lp(&mut out, f.as_bytes());
        }
        out
    }
}

/// The statement a worker signs with its libp2p identity to bind its X25519
/// key to one job (worker fields of `b` are the worker's own).
pub fn bid_statement(b: &Binding) -> Vec<u8> {
    b.encode(b"bid")
}

/// The two keys of one job.
pub struct JobKeys {
    pub spec: JobKey,
    pub result: JobKey,
}

pub fn parse_public(hex_key: &str) -> Result<PublicKey> {
    let b: [u8; 32] = hex::decode(hex_key)
        .ok()
        .and_then(|v| v.try_into().ok())
        .ok_or_else(|| anyhow!("X25519 key must be 32 bytes of hex"))?;
    Ok(PublicKey::from(b))
}

pub fn public_hex(secret: &StaticSecret) -> String {
    hex::encode(PublicKey::from(secret).as_bytes())
}

/// Derive the spec and result keys from our secret and the peer's public key.
pub fn derive(secret: &StaticSecret, peer_key_hex: &str, b: &Binding) -> Result<JobKeys> {
    let shared = secret.diffie_hellman(&parse_public(peer_key_hex)?);
    if !shared.was_contributory() {
        bail!("peer X25519 key is a low-order point");
    }
    let hk = Hkdf::<Sha256>::new(Some(DOMAIN), shared.as_bytes());
    let mut spec = [0u8; 32];
    let mut result = [0u8; 32];
    hk.expand(&b.encode(b"spec-key"), &mut spec)
        .map_err(|_| anyhow!("hkdf"))?;
    hk.expand(&b.encode(b"result-key"), &mut result)
        .map_err(|_| anyhow!("hkdf"))?;
    Ok(JobKeys {
        spec: JobKey(spec),
        result: JobKey(result),
    })
}

pub fn random_key() -> JobKey {
    let mut k = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut k);
    JobKey(k)
}

pub fn key_from_hex(h: &str) -> Result<JobKey> {
    let b: [u8; 32] = hex::decode(h)
        .ok()
        .and_then(|v| v.try_into().ok())
        .ok_or_else(|| anyhow!("input key must be 32 bytes of hex"))?;
    Ok(JobKey(b))
}

pub fn key_to_hex(k: &JobKey) -> String {
    hex::encode(k.0)
}

// ─── the sealed spec ─────────────────────────────────────────────────────

/// The buyer's commitment to a spec, published as the offer's `spec_hash`:
/// keyed by a random salt, so the hash says nothing about a guessable spec.
pub struct Commitment {
    salt: [u8; 32],
    spec: Zeroizing<Vec<u8>>,
}

fn commit_hash(salt: &[u8], spec: &[u8]) -> String {
    let mut h = blake3::Hasher::new_derive_key("c0mpute/job-e2e/v1 spec commitment");
    h.update(salt);
    h.update(spec);
    h.finalize().to_hex().to_string()
}

impl Commitment {
    pub fn new(spec: &Value) -> Result<Self> {
        let mut salt = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut salt);
        Ok(Self {
            salt,
            spec: Zeroizing::new(serde_json::to_vec(spec)?),
        })
    }

    pub fn hash_hex(&self) -> String {
        commit_hash(&self.salt, &self.spec)
    }
}

fn aead(key: &JobKey) -> Aes256Gcm {
    Aes256Gcm::new_from_slice(&key.0).expect("32-byte key")
}

/// Seal the committed spec to the winner: `nonce || AES-256-GCM(salt || spec)`
/// with the binding as associated data.
pub fn seal_spec(keys: &JobKeys, b: &Binding, c: &Commitment) -> Result<Vec<u8>> {
    let mut plain = Zeroizing::new(Vec::with_capacity(32 + c.spec.len()));
    plain.extend_from_slice(&c.salt);
    plain.extend_from_slice(&c.spec);
    let mut nonce = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let aad = b.encode(b"spec");
    let ct = aead(&keys.spec)
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &plain,
                aad: &aad,
            },
        )
        .map_err(|_| anyhow!("seal spec"))?;
    let mut out = nonce.to_vec();
    out.extend(ct);
    Ok(out)
}

/// Open a sealed spec and check it against the offer's commitment. Returns
/// the spec bytes.
pub fn open_spec(
    keys: &JobKeys,
    b: &Binding,
    sealed: &[u8],
    commitment: &str,
) -> Result<Zeroizing<Vec<u8>>> {
    if sealed.len() < 12 + TAG_LEN + 32 || sealed.len() > MAX_SEALED_SPEC {
        bail!("sealed spec has an impossible size");
    }
    let aad = b.encode(b"spec");
    let plain = Zeroizing::new(
        aead(&keys.spec)
            .decrypt(
                Nonce::from_slice(&sealed[..12]),
                Payload {
                    msg: &sealed[12..],
                    aad: &aad,
                },
            )
            .map_err(|_| anyhow!("{SPEC_KEY_MISMATCH} (wrong key, wrong job or tampered)"))?,
    );
    let (salt, spec) = plain.split_at(32);
    if commit_hash(salt, spec) != commitment {
        bail!("sealed spec does not match the offer's commitment");
    }
    Ok(Zeroizing::new(spec.to_vec()))
}

// ─── sealed blobs (inputs and results) ───────────────────────────────────
//
// `magic(8) || salt(32) || segment*`. The blob key is HKDF(root, salt), so
// every blob has its own key even under one root; segment i of up to
// BLOB_SEGMENT plaintext bytes is AES-256-GCM with nonce
// `last(1) || 0(3) || i(8, BE)` and the header as associated data. The
// final segment carries the last flag, so truncation and reordering fail.

/// Size of `plain` bytes once sealed.
pub fn sealed_len(plain: u64) -> u64 {
    let segments = plain.div_ceil(BLOB_SEGMENT as u64).max(1);
    BLOB_HEADER as u64 + plain + segments * TAG_LEN as u64
}

fn blob_cipher(root: &JobKey, salt: &[u8]) -> Aes256Gcm {
    let hk = Hkdf::<Sha256>::new(Some(salt), &root.0);
    let mut k = Zeroizing::new([0u8; 32]);
    hk.expand(b"c0mpute/job-e2e/v1 blob", &mut *k)
        .expect("32-byte okm");
    Aes256Gcm::new_from_slice(&*k).expect("32-byte key")
}

fn seg_nonce(i: u64, last: bool) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[0] = u8::from(last);
    n[4..].copy_from_slice(&i.to_be_bytes());
    n
}

fn new_header() -> [u8; BLOB_HEADER] {
    let mut h = [0u8; BLOB_HEADER];
    h[..8].copy_from_slice(BLOB_MAGIC);
    rand::rngs::OsRng.fill_bytes(&mut h[8..]);
    h
}

/// Seal bytes held in memory.
pub fn seal_blob(root: &JobKey, plain: &[u8]) -> Result<Vec<u8>> {
    let header = new_header();
    let cipher = blob_cipher(root, &header[8..]);
    let mut out = Vec::with_capacity(sealed_len(plain.len() as u64) as usize);
    out.extend_from_slice(&header);
    let segments = plain.len().div_ceil(BLOB_SEGMENT).max(1);
    for i in 0..segments {
        let seg =
            &plain[(i * BLOB_SEGMENT).min(plain.len())..((i + 1) * BLOB_SEGMENT).min(plain.len())];
        let last = i + 1 == segments;
        let ct = cipher
            .encrypt(
                Nonce::from_slice(&seg_nonce(i as u64, last)),
                Payload {
                    msg: seg,
                    aad: &header,
                },
            )
            .map_err(|_| anyhow!("seal blob"))?;
        out.extend(ct);
    }
    Ok(out)
}

/// Open a sealed blob held in memory.
pub fn open_blob(root: &JobKey, sealed: &[u8]) -> Result<Vec<u8>> {
    if sealed.len() < BLOB_HEADER + TAG_LEN || &sealed[..8] != BLOB_MAGIC {
        bail!("not a sealed job blob");
    }
    let (header, body) = sealed.split_at(BLOB_HEADER);
    let cipher = blob_cipher(root, &header[8..]);
    let full = BLOB_SEGMENT + TAG_LEN;
    let segments = body.len().div_ceil(full).max(1);
    let mut out = Vec::with_capacity(body.len());
    for i in 0..segments {
        let seg = &body[i * full..((i + 1) * full).min(body.len())];
        if seg.len() < TAG_LEN {
            bail!("sealed blob is truncated");
        }
        let last = i + 1 == segments;
        let pt = cipher
            .decrypt(
                Nonce::from_slice(&seg_nonce(i as u64, last)),
                Payload {
                    msg: seg,
                    aad: header,
                },
            )
            .map_err(|_| anyhow!("sealed blob does not open (wrong key, tampered or truncated)"))?;
        out.extend(pt);
    }
    Ok(out)
}

/// Read until `buf` is full or the reader ends; returns bytes read.
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

fn seal_file_sync(root: &JobKey, src: &Path, dst: &Path) -> Result<u64> {
    let mut input = std::io::BufReader::new(
        std::fs::File::open(src).with_context(|| format!("open {}", src.display()))?,
    );
    let mut output = std::io::BufWriter::new(
        std::fs::File::create(dst).with_context(|| format!("create {}", dst.display()))?,
    );
    let header = new_header();
    let cipher = blob_cipher(root, &header[8..]);
    output.write_all(&header)?;
    let mut written = BLOB_HEADER as u64;
    let mut cur = Zeroizing::new(vec![0u8; BLOB_SEGMENT]);
    let mut next = Zeroizing::new(vec![0u8; BLOB_SEGMENT]);
    let mut cur_len = read_full(&mut input, &mut cur)?;
    let mut i = 0u64;
    loop {
        // Read ahead: a segment is the last one when nothing follows it.
        let next_len = if cur_len == BLOB_SEGMENT {
            read_full(&mut input, &mut next)?
        } else {
            0
        };
        let last = next_len == 0;
        let ct = cipher
            .encrypt(
                Nonce::from_slice(&seg_nonce(i, last)),
                Payload {
                    msg: &cur[..cur_len],
                    aad: &header,
                },
            )
            .map_err(|_| anyhow!("seal blob"))?;
        output.write_all(&ct)?;
        written += ct.len() as u64;
        if last {
            break;
        }
        std::mem::swap(&mut cur, &mut next);
        cur_len = next_len;
        i += 1;
    }
    output.flush()?;
    Ok(written)
}

fn open_file_sync(root: &JobKey, src: &Path, dst: &Path) -> Result<u64> {
    let mut input = std::io::BufReader::new(
        std::fs::File::open(src).with_context(|| format!("open {}", src.display()))?,
    );
    let mut header = [0u8; BLOB_HEADER];
    if read_full(&mut input, &mut header)? != BLOB_HEADER || &header[..8] != BLOB_MAGIC {
        bail!("not a sealed job blob");
    }
    let cipher = blob_cipher(root, &header[8..]);
    let mut output = std::io::BufWriter::new(
        std::fs::File::create(dst).with_context(|| format!("create {}", dst.display()))?,
    );
    let full = BLOB_SEGMENT + TAG_LEN;
    let mut cur = vec![0u8; full];
    let mut next = vec![0u8; full];
    let mut cur_len = read_full(&mut input, &mut cur)?;
    let mut i = 0u64;
    let mut written = 0u64;
    loop {
        let next_len = if cur_len == full {
            read_full(&mut input, &mut next)?
        } else {
            0
        };
        let last = next_len == 0;
        if cur_len < TAG_LEN {
            bail!("sealed blob is truncated");
        }
        let pt = Zeroizing::new(
            cipher
                .decrypt(
                    Nonce::from_slice(&seg_nonce(i, last)),
                    Payload {
                        msg: &cur[..cur_len],
                        aad: &header,
                    },
                )
                .map_err(|_| {
                    anyhow!("sealed blob does not open (wrong key, tampered or truncated)")
                })?,
        );
        output.write_all(&pt)?;
        written += pt.len() as u64;
        if last {
            break;
        }
        std::mem::swap(&mut cur, &mut next);
        cur_len = next_len;
        i += 1;
    }
    output.flush()?;
    Ok(written)
}

/// Seal the file at `src` into `dst`, streamed. Returns the sealed size.
pub async fn seal_file(root: &JobKey, src: &Path, dst: &Path) -> Result<u64> {
    let (root, src, dst) = (root.clone(), src.to_path_buf(), dst.to_path_buf());
    tokio::task::spawn_blocking(move || {
        let r = seal_file_sync(&root, &src, &dst);
        if r.is_err() {
            let _ = std::fs::remove_file(&dst);
        }
        r
    })
    .await?
}

/// Open the sealed file at `src` into `dst`, streamed. `dst` is removed when
/// any segment fails to open. Returns the plaintext size.
pub async fn open_file(root: &JobKey, src: &Path, dst: &Path) -> Result<u64> {
    let (root, src, dst) = (root.clone(), src.to_path_buf(), dst.to_path_buf());
    tokio::task::spawn_blocking(move || {
        let r = open_file_sync(&root, &src, &dst);
        if r.is_err() {
            let _ = std::fs::remove_file(&dst);
        }
        r
    })
    .await?
}

/// Open a blob in place on disk (`path` holds the sealed bytes on entry and
/// the plaintext on success).
pub async fn open_file_in_place(root: &JobKey, path: &Path) -> Result<u64> {
    let tmp = path.with_extension("opening");
    let n = open_file(root, path, &tmp).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(n)
}

// ─── public specs ────────────────────────────────────────────────────────

/// `n` rounded up to a power of two (at least 1 KiB), so sizes leak only
/// their order of magnitude.
pub fn bucket(n: u64) -> u64 {
    n.max(1024).next_power_of_two()
}

/// Spec keys whose (small, scalar) values go in a workload's public spec.
/// Everything else stays sealed: prompts, texts, messages, URLs, headers,
/// input hashes and keys, file names, client keys, tokens.
fn public_keys(workload: &str) -> &'static [&'static str] {
    match workload {
        "ffmpeg.transcode" => &["preset"],
        "tts.synthesize" => &["engine", "voice", "locale", "speed"],
        "stt.transcribe" => &["model", "language", "audio_format"],
        "diffusion.txt2img" => &["model", "width", "height", "steps"],
        "music.txt2music" => &["model", "duration", "steps"],
        "video.txt2vid" | "video.img2vid" => &[
            "model",
            "image_format",
            "frames",
            "fps",
            "steps",
            "width",
            "height",
            "guidance_scale",
        ],
        "embed.text" => &["v", "model"],
        "ocr.extract" => &["v", "format", "lang"],
        "crawl.fetch" => &["v", "timeout_ms"],
        "blender.render-frame" => &[
            "v",
            "frame_start",
            "frame_end",
            "engine",
            "device",
            "format",
            "resolution_pct",
            "samples",
            "frame_timeout_secs",
        ],
        "workflow.step" => &["v", "kind"],
        "infernet.inference" => &["v", "model", "max_tokens", "temperature"],
        "infernet.text.infer" => &["base_model", "max_new_tokens"],
        "infernet.temporal.infer" => &["model_hash", "ttl_s"],
        "webhook.deliver" => &["v", "max_attempts", "timeout_ms", "backoff_ms"],
        "gameserver.start" => &[
            "v",
            "game",
            "hours",
            "ram_gb",
            "cpus",
            "accept_eula",
            "backup_mins",
        ],
        "gameserver.stop" => &["v", "id"],
        "captcha.solve" => &["v", "kind"],
        "container.run" => crate::container_run::PUBLIC_KEYS,
        _ => &["v"],
    }
}

/// Size fields copied into the public spec as [`bucket`]s.
const SIZE_KEYS: &[&str] = &["audio_size", "image_size", "input_size", "corpus_size"];

fn public_scalar(v: &Value) -> bool {
    match v {
        Value::Bool(_) | Value::Number(_) => true,
        Value::String(s) => s.len() <= MAX_PUBLIC_STRING,
        _ => false,
    }
}

fn pick(obj: &Map<String, Value>, keys: &[&str]) -> Map<String, Value> {
    keys.iter()
        .filter_map(|k| {
            obj.get(*k)
                .filter(|v| public_scalar(v))
                .map(|v| ((*k).to_string(), v.clone()))
        })
        .collect()
}

/// What a worker needs to decide whether to bid on `spec`, and nothing more.
pub fn public_view(workload: &str, spec: &Value) -> Value {
    let mut out = Map::new();
    let size = serde_json::to_vec(spec)
        .map(|b| b.len() as u64)
        .unwrap_or(0);
    out.insert("spec_bytes".into(), bucket(size).into());
    let Some(obj) = spec.as_object() else {
        return Value::Object(out);
    };
    out.extend(pick(obj, public_keys(workload)));
    for k in SIZE_KEYS {
        if let Some(n) = obj.get(*k).and_then(Value::as_u64) {
            out.insert((*k).to_string(), bucket(n).into());
        }
    }
    match workload {
        "ffmpeg.transcode" => {
            if let Some(s) = obj.get("spec").and_then(Value::as_object) {
                let p = pick(
                    s,
                    &[
                        "codec",
                        "bitrate_bps",
                        "width",
                        "height",
                        "keyframe_interval",
                        "hardware_pref",
                    ],
                );
                out.insert("spec".into(), Value::Object(p));
            }
        }
        "infernet.text.train" => {
            if let Some(j) = obj.get("job").and_then(Value::as_object) {
                out.insert(
                    "job".into(),
                    Value::Object(pick(j, &["kind", "base_model"])),
                );
            }
        }
        "infernet.temporal.train" => {
            if let Some(j) = obj.get("train_job").and_then(Value::as_object) {
                let mut p = pick(j, &["schema_version", "kind", "model_type"]);
                if let Some(pr) = j.get("privacy").and_then(Value::as_object) {
                    p.insert(
                        "privacy".into(),
                        Value::Object(pick(pr, &["mode", "upload_approved"])),
                    );
                }
                out.insert("train_job".into(), Value::Object(p));
            }
        }
        "gameserver.stop" => {
            if let Some(t) = obj.get("token").and_then(Value::as_str) {
                out.insert(
                    "token_hash".into(),
                    format!("sha256:{}", hex::encode(Sha256::digest(t.as_bytes()))).into(),
                );
            }
        }
        _ => {}
    }
    Value::Object(out)
}

/// A buyer cannot claim one thing publicly and send another: every field it
/// published must equal the field recomputed from the sealed spec. (A subset
/// check, so builds whose public key lists differ still interoperate.)
pub fn public_view_consistent(workload: &str, spec: &Value, published: &Value) -> bool {
    let actual = public_view(workload, spec);
    fn subset(p: &Value, a: &Value) -> bool {
        match (p, a) {
            (Value::Object(p), Value::Object(a)) => p
                .iter()
                .all(|(k, v)| a.get(k).is_some_and(|av| subset(v, av))),
            _ => p == a,
        }
    }
    match published {
        Value::Null => true,
        Value::Object(_) => subset(published, &actual),
        _ => false,
    }
}

/// The spec an offer filter should judge: on an e2e offer only the public
/// spec exists at bid time; on a legacy offer the whole spec.
pub enum BidSpec<'a> {
    Full(&'a Value),
    Public(&'a Value),
}

pub fn bid_spec(offer: &JobOffer) -> Option<BidSpec<'_>> {
    match (&offer.e2e, &offer.spec_inline) {
        (Some(e), _) => Some(BidSpec::Public(&e.public_spec)),
        (None, Some(s)) => Some(BidSpec::Full(s)),
        (None, None) => None,
    }
}

/// A public spec with stand-ins for sealed fields that a filter's parser
/// requires (a prompt, a client key), for bid-time checks only: the real
/// spec is checked again by the same filter once it is opened.
pub fn with_placeholders(public: &Value, fill: &[(&str, Value)]) -> Value {
    let mut m = public.as_object().cloned().unwrap_or_default();
    for (k, v) in fill {
        m.entry((*k).to_string()).or_insert_with(|| v.clone());
    }
    Value::Object(m)
}

/// `offer` as a filter sees it once the sealed spec is open: the full spec in
/// `spec_inline`, no e2e part. Filters run on this again before a won job
/// starts, so every check they make on whole specs still holds.
pub fn opened_offer(offer: &JobOffer, spec: Value) -> JobOffer {
    let mut o = offer.clone();
    o.spec_inline = Some(spec);
    o.e2e = None;
    o
}

// ─── buyer ───────────────────────────────────────────────────────────────

/// What [`open_spec`] says when the spec was sealed to another key: on a
/// worker that bid, almost always a twin (another process with this peer id)
/// whose bid the buyer kept.
pub const SPEC_KEY_MISMATCH: &str = "sealed spec does not open";

/// Whether an error from opening a won job's spec is a key mismatch.
pub fn is_key_mismatch(err: &str) -> bool {
    err.contains(SPEC_KEY_MISMATCH)
}

/// The buyer's half of one e2e auction.
pub struct BuyerSession {
    secret: StaticSecret,
    pub public_hex: String,
    commitment: Commitment,
    pub attest_nonce: Option<String>,
}

/// The buyer's X25519 secret for `job_id`: derived from the node's job
/// secret and the job id, so a buyer that restarts mid-job re-derives it and
/// can still open the result. See DIP-0032 "Resuming after a restart".
fn job_buyer_secret(net: &Libp2pNetwork, job_id: &str) -> StaticSecret {
    let mut context = b"c0mpute/job-e2e/v1 buyer-key\0".to_vec();
    context.extend_from_slice(job_id.as_bytes());
    let bytes = net.derive_job_secret(&context);
    StaticSecret::from(*bytes)
}

/// What a buyer persists about a job a worker is running for it: enough to
/// re-derive the job's result key after a restart and to ask the worker
/// where the job stands. No secret: the buyer's key is re-derived from its
/// job secret, the worker's key is public.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InFlightJob {
    pub job_id: String,
    pub workload: String,
    pub buyer_peer: String,
    pub winner_peer: String,
    /// The winner's X25519 public key (hex), from its signed bid.
    pub worker_key: String,
    pub price_usd: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_ref: Option<String>,
    pub accepted_at_ms: u64,
}

/// Re-derive the result key of an in-flight job (after a restart).
pub fn resume_result_key(net: &Libp2pNetwork, job: &InFlightJob) -> Result<JobKey> {
    if net.peer_id().to_base58() != job.buyer_peer {
        bail!("this node is not the job's buyer");
    }
    let secret = job_buyer_secret(net, &job.job_id);
    let buyer_key = public_hex(&secret);
    let b = Binding {
        job_id: &job.job_id,
        workload: &job.workload,
        buyer_peer: &job.buyer_peer,
        worker_peer: &job.winner_peer,
        buyer_key: &buyer_key,
        worker_key: &job.worker_key,
    };
    Ok(derive(&secret, &job.worker_key, &b)?.result.clone())
}

impl BuyerSession {
    /// The session of one auction, its X25519 key derived from the node's
    /// job secret and `job_id` (re-derivable after a restart).
    pub fn for_job(net: &Libp2pNetwork, job_id: &str, spec: &Value, confidential: bool) -> Result<Self> {
        Self::with_secret(job_buyer_secret(net, job_id), spec, confidential)
    }

    /// A session with a random key (never resumed: canary probes).
    pub fn new(spec: &Value, confidential: bool) -> Result<Self> {
        Self::with_secret(StaticSecret::random_from_rng(rand::rngs::OsRng), spec, confidential)
    }

    fn with_secret(secret: StaticSecret, spec: &Value, confidential: bool) -> Result<Self> {
        let attest_nonce = confidential.then(|| {
            let mut n = [0u8; 32];
            rand::rngs::OsRng.fill_bytes(&mut n);
            hex::encode(n)
        });
        Ok(Self {
            public_hex: public_hex(&secret),
            secret,
            commitment: Commitment::new(spec)?,
            attest_nonce,
        })
    }

    pub fn spec_hash(&self) -> String {
        self.commitment.hash_hex()
    }

    pub fn offer_part(&self, workload: &str, spec: &Value, addrs: Vec<String>) -> OfferE2e {
        OfferE2e {
            v: JOB_E2E_VERSION,
            buyer_key: self.public_hex.clone(),
            public_spec: public_view(workload, spec),
            attest_nonce: self.attest_nonce.clone(),
            addrs,
        }
    }

    fn binding<'a>(
        &'a self,
        job_id: &'a str,
        workload: &'a str,
        buyer_peer: &'a str,
        bid: &'a JobBid,
        e: &'a BidE2e,
    ) -> Binding<'a> {
        Binding {
            job_id,
            workload,
            buyer_peer,
            worker_peer: &bid.bidder_peer_id,
            buyer_key: &self.public_hex,
            worker_key: &e.worker_key,
        }
    }

    /// A bid is usable only with an X25519 key its bidder signed for this
    /// job and buyer key (and, for confidential work, attestation evidence
    /// that binds the key and verifies).
    pub fn check_bid(
        &self,
        job_id: &str,
        workload: &str,
        buyer_peer: &str,
        bid: &JobBid,
    ) -> Result<()> {
        let e = bid.e2e.as_ref().context("bid carries no e2e key")?;
        parse_public(&e.worker_key)?;
        let peer: PeerId = bid
            .bidder_peer_id
            .parse()
            .map_err(|_| anyhow!("bidder peer id"))?;
        let sig = B64
            .decode(&e.sig)
            .map_err(|_| anyhow!("bid key signature is not base64"))?;
        let stmt = bid_statement(&self.binding(job_id, workload, buyer_peer, bid, e));
        if !c0mpute_net::identity::verify_peer_signature(&peer, &stmt, &sig) {
            bail!("bid key signature does not verify against the bidder's peer id");
        }
        if let Some(nonce) = &self.attest_nonce {
            let evidence = e
                .attestation
                .as_ref()
                .context("confidential job: bid has no attestation")?;
            let rd = report_data(job_id, &bid.bidder_peer_id, &e.worker_key, nonce);
            attestation_verifier()
                .context("confidential job: no attestation verifier is configured on this buyer")?
                .verify(evidence, &rd)
                .context("attestation does not verify")?;
        }
        Ok(())
    }

    /// Seal the spec to the winner. Returns the sealed bytes and the key its
    /// result will be sealed back with.
    pub fn seal_for(
        &self,
        job_id: &str,
        workload: &str,
        buyer_peer: &str,
        winner: &JobBid,
    ) -> Result<(Vec<u8>, JobKey)> {
        let e = winner.e2e.as_ref().context("winner has no e2e key")?;
        let b = self.binding(job_id, workload, buyer_peer, winner, e);
        let keys = derive(&self.secret, &e.worker_key, &b)?;
        let sealed = seal_spec(&keys, &b, &self.commitment)?;
        Ok((sealed, keys.result.clone()))
    }
}

struct PendingPayload {
    winner: PeerId,
    sealed: Arc<Vec<u8>>,
    until: Instant,
}

fn payloads() -> &'static Mutex<HashMap<String, PendingPayload>> {
    static P: OnceLock<Mutex<HashMap<String, PendingPayload>>> = OnceLock::new();
    P.get_or_init(Default::default)
}

/// Key of one held sealed spec: a job re-awarded after a twin fault holds one
/// per winner.
fn payload_key(job_id: &str, winner: &PeerId) -> String {
    format!("{job_id}/{winner}")
}

/// Removes a job's sealed spec when the auction ends.
pub struct PayloadGuard(String, #[allow(dead_code)] crate::pieces::PushSender);

impl Drop for PayloadGuard {
    fn drop(&mut self) {
        if let Ok(mut p) = payloads().lock() {
            p.remove(&self.0);
        }
    }
}

/// Hold `sealed` for `winner` to fetch over [`PAYLOAD_SERVICE`].
pub fn hold_payload(
    net: &Libp2pNetwork,
    job_id: &str,
    winner: PeerId,
    sealed: Vec<u8>,
) -> PayloadGuard {
    net.register_rpc(PAYLOAD_SERVICE, Arc::new(PayloadHandler));
    // The winner may not be dialable; let it (and only it) push its result
    // back, for as long as this award is held.
    crate::pieces::serve_result_push(net);
    let pusher = crate::pieces::expect_result_push(winner);
    if let Ok(mut p) = payloads().lock() {
        let now = Instant::now();
        p.retain(|_, v| v.until > now);
        p.insert(
            payload_key(job_id, &winner),
            PendingPayload {
                winner,
                sealed: Arc::new(sealed),
                until: now + PAYLOAD_TTL,
            },
        );
    }
    PayloadGuard(payload_key(job_id, &winner), pusher)
}

/// Serves sealed specs, each only to the peer that won it. The caller's peer
/// id comes from the noise handshake, so it cannot be claimed falsely.
struct PayloadHandler;

#[async_trait::async_trait]
impl RpcHandler for PayloadHandler {
    async fn handle(&self, peer: PeerId, body: Vec<u8>) -> std::result::Result<Vec<u8>, String> {
        let job_id = std::str::from_utf8(&body).map_err(|_| "bad job id".to_string())?;
        let p = payloads().lock().map_err(|_| "unavailable".to_string())?;
        match p.get(&payload_key(job_id, &peer)) {
            Some(x) if x.winner == peer && x.until > Instant::now() => {
                Ok(x.sealed.as_ref().clone())
            }
            // Same answer for "no such job" and "not yours".
            _ => Err("no sealed spec for this peer".into()),
        }
    }
}

// ─── worker ──────────────────────────────────────────────────────────────

/// The worker's half of one bid: its fresh X25519 secret, kept until the
/// accept (or the bid lapses).
pub struct WorkerSession {
    secret: StaticSecret,
    pub public_hex: String,
}

impl std::fmt::Debug for WorkerSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WorkerSession({})", self.public_hex)
    }
}

impl WorkerSession {
    #[cfg(test)]
    pub(crate) fn for_tests() -> Self {
        let secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
        Self {
            public_hex: public_hex(&secret),
            secret,
        }
    }

    /// A fresh key for a bid on `offer`, signed by our identity, plus TEE
    /// evidence when the offer asks for it.
    pub fn for_offer(net: &Libp2pNetwork, offer: &JobOffer) -> Result<(Self, BidE2e)> {
        let e = offer.e2e.as_ref().context("offer is not e2e")?;
        if e.v != JOB_E2E_VERSION {
            bail!("offer speaks e2e v{}, this node v{JOB_E2E_VERSION}", e.v);
        }
        parse_public(&e.buyer_key)?;
        let secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let public_hex = public_hex(&secret);
        let me = net.peer_id().to_base58();
        let b = Binding {
            job_id: &offer.job_id,
            workload: &offer.workload_type,
            buyer_peer: &offer.buyer_peer_id,
            worker_peer: &me,
            buyer_key: &e.buyer_key,
            worker_key: &public_hex,
        };
        let sig = B64.encode(net.sign(&bid_statement(&b))?);
        let attestation = match &e.attest_nonce {
            Some(nonce) => {
                let rd = report_data(&offer.job_id, &me, &public_hex, nonce);
                Some(
                    attestation_provider()
                        .context("no TEE attestation provider on this node")?
                        .evidence(&rd)?,
                )
            }
            None => None,
        };
        Ok((
            Self {
                secret,
                public_hex: public_hex.clone(),
            },
            BidE2e {
                worker_key: public_hex,
                sig,
                attestation,
                addrs: Vec::new(),
            },
        ))
    }

    /// Fetch the sealed spec from the buyer (point to point), open it, check
    /// it against the offer's commitment and public spec, and return the spec
    /// with the key the result must be sealed with.
    pub async fn open_accept(
        &self,
        net: &Libp2pNetwork,
        offer: &JobOffer,
        accept: &JobAccept,
    ) -> Result<(Value, JobKey)> {
        let e = offer.e2e.as_ref().context("offer is not e2e")?;
        let me = net.peer_id().to_base58();
        let b = Binding {
            job_id: &offer.job_id,
            workload: &offer.workload_type,
            buyer_peer: &offer.buyer_peer_id,
            worker_peer: &me,
            buyer_key: &e.buyer_key,
            worker_key: &self.public_hex,
        };
        let sealed = match &accept.sealed_spec {
            // Pushed on the direct accept (its source was checked: the buyer).
            Some(b64) => B64
                .decode(b64)
                .map_err(|_| anyhow!("pushed sealed spec is not base64"))?,
            None => {
                // The addresses the buyer gave on its offer (same host or LAN).
                if let Ok(buyer) = accept.buyer_peer_id.parse::<PeerId>() {
                    let own = net.listen_addrs().await.unwrap_or_default();
                    for a in crate::reachability::hint_addrs(&e.addrs, &own) {
                        net.add_peer_address(buyer, a).await;
                    }
                }
                fetch_sealed(net, accept).await?
            }
        };
        let keys = derive(&self.secret, &e.buyer_key, &b)?;
        let bytes = open_spec(&keys, &b, &sealed, &offer.spec_hash)?;
        let spec: Value = serde_json::from_slice(&bytes).context("sealed spec is not JSON")?;
        if !public_view_consistent(&offer.workload_type, &spec, &e.public_spec) {
            bail!("sealed spec differs from what the offer published");
        }
        Ok((spec, keys.result.clone()))
    }
}

async fn fetch_sealed(net: &Libp2pNetwork, accept: &JobAccept) -> Result<Vec<u8>> {
    let buyer: PeerId = accept
        .buyer_peer_id
        .parse()
        .map_err(|_| anyhow!("buyer peer id"))?;
    for addr in
        crate::reachability::dialable_buyer_addrs(&accept.buyer_addrs, &accept.buyer_peer_id)
    {
        let mut a = addr;
        if matches!(a.iter().last(), Some(c0mpute_net::Protocol::P2p(_))) {
            a.pop();
        }
        net.add_peer_address(buyer, a).await;
    }
    // A buyer heard of only through gossip relayed by other nodes (no
    // public address on its offer, e.g. on a LAN) is found through the DHT.
    if let Err(e) = net.reach(buyer, Duration::from_secs(10)).await {
        tracing::debug!(%buyer, err = %e, "e2e: buyer not reachable yet; trying the fetch anyway");
    }
    let mut last = anyhow!("not tried");
    for attempt in 0..4u32 {
        match net
            .rpc(buyer, PAYLOAD_SERVICE, accept.job_id.as_bytes().to_vec())
            .await
        {
            Ok(b) if b.len() <= MAX_SEALED_SPEC => return Ok(b),
            Ok(_) => bail!("sealed spec from the buyer is over the limit"),
            Err(e) => last = e,
        }
        tokio::time::sleep(Duration::from_millis(500 * u64::from(attempt + 1))).await;
    }
    Err(last.context("fetch the sealed spec from the buyer"))
}

// ─── results ─────────────────────────────────────────────────────────────

/// The key an e2e job's results are sealed with; `None` for a legacy job.
pub fn result_key(accept: &JobAccept) -> Result<Option<&JobKey>> {
    match (&accept.e2e, &accept.result_key) {
        (Some(_), Some(k)) => Ok(Some(k)),
        (Some(_), None) => bail!("e2e job without a result key"),
        (None, _) => Ok(None),
    }
}

/// Seal a result for the buyer when the job is e2e; a legacy job's result is
/// stored as it is.
pub fn seal_result(accept: &JobAccept, plain: &[u8]) -> Result<Option<Vec<u8>>> {
    result_key(accept)?.map(|k| seal_blob(k, plain)).transpose()
}

/// For a result file: the path to store (a sealed copy next to `path` for an
/// e2e job, removed by the caller with [`SealedCopy`]'s drop) and its limit.
pub async fn sealed_result_file(
    accept: &JobAccept,
    path: &Path,
    max_bytes: u64,
) -> Result<(SealedCopy, u64)> {
    match result_key(accept)? {
        Some(k) => {
            let sealed = path.with_extension("sealed");
            seal_file(k, path, &sealed).await?;
            Ok((
                SealedCopy {
                    path: sealed,
                    temp: true,
                },
                sealed_len(max_bytes),
            ))
        }
        None => Ok((
            SealedCopy {
                path: path.to_path_buf(),
                temp: false,
            },
            max_bytes,
        )),
    }
}

/// A file to store; removed on drop when it is a sealed temporary copy.
pub struct SealedCopy {
    pub path: std::path::PathBuf,
    temp: bool,
}

impl Drop for SealedCopy {
    fn drop(&mut self) {
        if self.temp {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

// ─── confidential computing (TEE attestation) ────────────────────────────

/// 64 bytes a TEE report must carry as its report data / REPORTDATA: binds
/// the attestation to this job, this worker and its X25519 key, and the
/// buyer's fresh nonce.
pub fn report_data(job_id: &str, worker_peer: &str, worker_key: &str, nonce: &str) -> [u8; 64] {
    let mut h = blake3::Hasher::new_derive_key("c0mpute/job-e2e/v1 attestation report data");
    for f in [job_id, worker_peer, worker_key, nonce] {
        h.update(&(f.len() as u32).to_be_bytes());
        h.update(f.as_bytes());
    }
    let mut out = [0u8; 64];
    h.finalize_xof().fill(&mut out);
    out
}

/// Buyer side: checks a worker's attestation evidence (an AMD SEV-SNP report
/// with its VCEK chain, an Intel TDX quote...) against vendor roots and the
/// buyer's measurement policy, and that its report data equals
/// `report_data`. Only then is the spec sealed to the attested key.
pub trait AttestationVerifier: Send + Sync {
    fn verify(&self, evidence: &Value, report_data: &[u8; 64]) -> Result<()>;
}

/// Worker side: produces attestation evidence over `report_data` from the
/// TEE this node runs in.
pub trait AttestationProvider: Send + Sync {
    fn evidence(&self, report_data: &[u8; 64]) -> Result<Value>;
}

static VERIFIER: RwLock<Option<Arc<dyn AttestationVerifier>>> = RwLock::new(None);
static PROVIDER: RwLock<Option<Arc<dyn AttestationProvider>>> = RwLock::new(None);

pub fn set_attestation_verifier(v: Option<Arc<dyn AttestationVerifier>>) {
    if let Ok(mut g) = VERIFIER.write() {
        *g = v;
    }
}

pub fn set_attestation_provider(p: Option<Arc<dyn AttestationProvider>>) {
    if let Ok(mut g) = PROVIDER.write() {
        *g = p;
    }
}

pub fn attestation_verifier() -> Option<Arc<dyn AttestationVerifier>> {
    VERIFIER.read().ok().and_then(|g| g.clone())
}

pub fn attestation_provider() -> Option<Arc<dyn AttestationProvider>> {
    PROVIDER.read().ok().and_then(|g| g.clone())
}

/// Capability tags a worker satisfies by speaking this protocol rather than
/// by configuration: [`JOB_E2E_TAG`] always, [`CONFIDENTIAL_TAG`] only with
/// an attestation provider.
pub fn implicit_tag(tag: &str) -> bool {
    tag == JOB_E2E_TAG || (tag == CONFIDENTIAL_TAG && attestation_provider().is_some())
}

/// Whether this build still serves plaintext (pre-e2e) offers. The window
/// closes in the release after the one that introduced e2e jobs;
/// `C0MPUTE_REJECT_PLAINTEXT_OFFERS=1` closes it early on one node.
pub fn serves_plaintext_offers() -> bool {
    !std::env::var("C0MPUTE_REJECT_PLAINTEXT_OFFERS").is_ok_and(|v| !v.is_empty() && v != "0")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn binding<'a>(bk: &'a str, wk: &'a str) -> Binding<'a> {
        Binding {
            job_id: "job-1",
            workload: "infernet.inference",
            buyer_peer: "B",
            worker_peer: "W",
            buyer_key: bk,
            worker_key: wk,
        }
    }

    #[test]
    fn both_sides_derive_the_same_keys_and_bindings_separate_them() {
        let b = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let w = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let (bp, wp) = (public_hex(&b), public_hex(&w));
        let kb = derive(&b, &wp, &binding(&bp, &wp)).unwrap();
        let kw = derive(&w, &bp, &binding(&bp, &wp)).unwrap();
        assert_eq!(kb.spec, kw.spec);
        assert_eq!(kb.result, kw.result);
        assert_ne!(kb.spec, kb.result);
        let mut other = binding(&bp, &wp);
        other.job_id = "job-2";
        assert_ne!(derive(&b, &wp, &other).unwrap().spec, kb.spec);
    }

    #[test]
    fn low_order_keys_are_refused() {
        let b = StaticSecret::random_from_rng(rand::rngs::OsRng);
        assert!(derive(&b, &"00".repeat(32), &binding("x", "y")).is_err());
    }

    #[test]
    fn sealed_spec_opens_only_for_the_winner_and_only_unmodified() {
        let spec = json!({"v": 1, "messages": [{"role": "user", "content": "the secret prompt"}]});
        let c = Commitment::new(&spec).unwrap();
        let b = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let w = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let loser = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let (bp, wp, lp_) = (public_hex(&b), public_hex(&w), public_hex(&loser));
        let bind = binding(&bp, &wp);
        let sealed = seal_spec(&derive(&b, &wp, &bind).unwrap(), &bind, &c).unwrap();
        assert!(!String::from_utf8_lossy(&sealed).contains("secret prompt"));
        let opened = open_spec(
            &derive(&w, &bp, &bind).unwrap(),
            &bind,
            &sealed,
            &c.hash_hex(),
        )
        .unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&opened).unwrap(), spec);
        // The loser (or anyone else) derives a different key.
        let lbind = binding(&bp, &lp_);
        assert!(
            open_spec(
                &derive(&loser, &bp, &lbind).unwrap(),
                &lbind,
                &sealed,
                &c.hash_hex()
            )
            .is_err()
        );
        assert!(
            open_spec(
                &derive(&loser, &bp, &bind).unwrap(),
                &bind,
                &sealed,
                &c.hash_hex()
            )
            .is_err()
        );
        // Tampering anywhere fails.
        for i in [0, 12, sealed.len() - 1] {
            let mut t = sealed.clone();
            t[i] ^= 1;
            assert!(open_spec(&derive(&w, &bp, &bind).unwrap(), &bind, &t, &c.hash_hex()).is_err());
        }
        // Moved to another job: the associated data no longer matches.
        let mut moved = bind;
        moved.job_id = "job-2";
        assert!(
            open_spec(
                &derive(&w, &bp, &bind).unwrap(),
                &moved,
                &sealed,
                &c.hash_hex()
            )
            .is_err()
        );
        // A different commitment is refused even with the right key.
        let other = Commitment::new(&spec).unwrap();
        assert!(
            open_spec(
                &derive(&w, &bp, &bind).unwrap(),
                &bind,
                &sealed,
                &other.hash_hex()
            )
            .is_err()
        );
    }

    #[test]
    fn commitment_hides_the_spec() {
        let spec = json!({"text": "hi"});
        let a = Commitment::new(&spec).unwrap();
        let b = Commitment::new(&spec).unwrap();
        assert_ne!(
            a.hash_hex(),
            b.hash_hex(),
            "salted: equal specs commit differently"
        );
        assert_ne!(
            a.hash_hex(),
            blake3::hash(spec.to_string().as_bytes())
                .to_hex()
                .to_string()
        );
    }

    #[test]
    fn blobs_round_trip_at_segment_edges_and_reject_tampering() {
        let k = random_key();
        for n in [
            0,
            1,
            BLOB_SEGMENT - 1,
            BLOB_SEGMENT,
            BLOB_SEGMENT + 1,
            3 * BLOB_SEGMENT,
        ] {
            let plain: Vec<u8> = (0..n).map(|i| (i % 251) as u8).collect();
            let sealed = seal_blob(&k, &plain).unwrap();
            assert_eq!(sealed.len() as u64, sealed_len(n as u64), "n={n}");
            assert_eq!(open_blob(&k, &sealed).unwrap(), plain, "n={n}");
            assert!(open_blob(&random_key(), &sealed).is_err());
            if n > BLOB_SEGMENT {
                // Drop the final segment: the new last one lacks the flag.
                let cut = BLOB_HEADER + (BLOB_SEGMENT + TAG_LEN);
                assert!(open_blob(&k, &sealed[..cut]).is_err(), "truncation n={n}");
            }
            let mut t = sealed.clone();
            let last = t.len() - 1;
            t[last] ^= 0x80;
            assert!(open_blob(&k, &t).is_err());
        }
        // Same plaintext, same root: different ciphertext (fresh salt).
        assert_ne!(seal_blob(&k, b"x").unwrap(), seal_blob(&k, b"x").unwrap());
    }

    #[tokio::test]
    async fn files_seal_and_open_streamed() {
        let dir = tempfile::tempdir().unwrap();
        let k = random_key();
        for n in [0usize, 10, BLOB_SEGMENT, 2 * BLOB_SEGMENT + 7] {
            let src = dir.path().join("in");
            let plain: Vec<u8> = (0..n).map(|i| (i * 7 % 256) as u8).collect();
            std::fs::write(&src, &plain).unwrap();
            let sealed = dir.path().join("sealed");
            let size = seal_file(&k, &src, &sealed).await.unwrap();
            assert_eq!(size, sealed_len(n as u64));
            // File and memory formats are the same.
            assert_eq!(
                open_blob(&k, &std::fs::read(&sealed).unwrap()).unwrap(),
                plain
            );
            let out = dir.path().join("out");
            assert_eq!(open_file(&k, &sealed, &out).await.unwrap(), n as u64);
            assert_eq!(std::fs::read(&out).unwrap(), plain);
            let mut bytes = std::fs::read(&sealed).unwrap();
            let mid = bytes.len() / 2;
            bytes[mid] ^= 1;
            std::fs::write(&sealed, &bytes).unwrap();
            assert!(open_file(&k, &sealed, &out).await.is_err());
            assert!(!out.exists(), "a failed open leaves no partial plaintext");
        }
    }

    #[test]
    fn public_views_carry_no_content() {
        let cases = [
            (
                "infernet.inference",
                json!({"v": 1, "model": "qwen2.5:0.5b", "messages": [{"role": "user", "content": "SECRET-1"}], "max_tokens": 64}),
            ),
            (
                "tts.synthesize",
                json!({"text": "SECRET-1", "engine": "kokoro", "voice": "af_heart", "locale": "en-US", "speed": 1.0, "persona": "SECRET-1"}),
            ),
            (
                "stt.transcribe",
                json!({"audio_hash": "SECRET-1", "audio_size": 12345, "audio_pieces": ["SECRET-1"], "audio_key": "SECRET-1", "model": "base", "audio_format": "wav"}),
            ),
            (
                "diffusion.txt2img",
                json!({"prompt": "SECRET-1", "model": "sd", "width": 512, "height": 512, "steps": 20, "seed": 7}),
            ),
            (
                "music.txt2music",
                json!({"prompt": "SECRET-1", "lyrics": "SECRET-1", "duration": 30}),
            ),
            (
                "video.img2vid",
                json!({"model": "m", "prompt": "SECRET-1", "negative_prompt": "SECRET-1", "image_hash": "SECRET-1", "image_key": "SECRET-1", "frames": 16}),
            ),
            (
                "embed.text",
                json!({"v": 1, "model": "e5", "texts": ["SECRET-1"]}),
            ),
            (
                "ocr.extract",
                json!({"v": 1, "input_hash": "SECRET-1", "input_key": "SECRET-1", "format": "png", "lang": "eng"}),
            ),
            (
                "crawl.fetch",
                json!({"v": 1, "url": "https://SECRET-1.example/", "headers": {"cookie": "SECRET-1"}}),
            ),
            (
                "blender.render-frame",
                json!({"v": 1, "input_hash": "SECRET-1", "input_size": 9, "input_pieces": ["SECRET-1"], "frame_start": 1, "frame_end": 2}),
            ),
            (
                "workflow.step",
                json!({"v": 1, "kind": "http_call", "config": {"url": "https://SECRET-1/"}, "input": "SECRET-1"}),
            ),
            (
                "ffmpeg.transcode",
                json!({"input_url": "https://SECRET-1/a.mp4", "preset": "720p", "spec": {"codec": "h264", "bitrate_bps": 1, "width": 1, "height": 1, "keyframe_interval": 1, "extra_ffmpeg_args": ["drawtext=SECRET-1"]}}),
            ),
            (
                "infernet.text.train",
                json!({"job": {"kind": "lora", "base_model": "org/m", "data": "SECRET-1"}, "dataset_hash": "SECRET-1", "dataset_url": "https://SECRET-1"}),
            ),
            (
                "infernet.text.infer",
                json!({"adapter_hash": "SECRET-1", "base_model": "org/m", "messages": [{"role": "user", "content": "SECRET-1"}], "max_new_tokens": 9}),
            ),
            (
                "gameserver.stop",
                json!({"v": 1, "id": "srv", "token": "SECRET-1"}),
            ),
            (
                "gameserver.start",
                json!({"v": 1, "game": "minecraft", "hours": 1, "ram_gb": 2, "cpus": 1.0, "stop_hash": "SECRET-1", "client_pubkey": "SECRET-1", "backup_pubkey": "SECRET-1", "world": "SECRET-1", "server_name": "SECRET-1"}),
            ),
            (
                "captcha.solve",
                json!({"v": 1, "kind": "text", "image": "SECRET-1"}),
            ),
            (
                "webhook.deliver",
                json!({"v": 1, "client_pubkey": "SECRET-1", "max_attempts": 3, "timeout_ms": 1000, "backoff_ms": 100}),
            ),
            (
                "infernet.temporal.infer",
                json!({"model_hash": "sha256:00", "session_id": "SECRET-1", "ttl_s": 60, "account_scope": "SECRET-1", "client_pubkey": "SECRET-1"}),
            ),
            (
                "infernet.temporal.train",
                json!({"train_job": {"kind": "train_job", "dataset_hash": "SECRET-1", "privacy": {"mode": "trusted_worker"}}, "dataset_url": "https://SECRET-1", "dataset_hash": "SECRET-1"}),
            ),
            ("unknown.workload", json!({"v": 1, "anything": "SECRET-1"})),
        ];
        for (w, spec) in cases {
            let v = public_view(w, &spec);
            assert!(!v.to_string().contains("SECRET-1"), "{w}: {v}");
            assert!(public_view_consistent(w, &spec, &v), "{w}");
        }
        let v = public_view("infernet.inference", &cases_model());
        assert_eq!(v["model"], "qwen2.5:0.5b");
        assert_eq!(
            public_view("stt.transcribe", &json!({"audio_size": 12345}))["audio_size"],
            16384
        );
    }

    fn cases_model() -> Value {
        json!({"v": 1, "model": "qwen2.5:0.5b", "messages": []})
    }

    #[test]
    fn a_buyer_cannot_publish_one_spec_and_seal_another() {
        let spec = json!({"v": 1, "model": "big-model", "messages": []});
        let claimed = public_view(
            "infernet.inference",
            &json!({"v": 1, "model": "small-model", "messages": []}),
        );
        assert!(!public_view_consistent(
            "infernet.inference",
            &spec,
            &claimed
        ));
        assert!(public_view_consistent(
            "infernet.inference",
            &spec,
            &json!({})
        ));
        assert!(!public_view_consistent(
            "infernet.inference",
            &spec,
            &json!("x")
        ));
    }

    #[test]
    fn report_data_binds_every_field() {
        let a = report_data("j", "w", "k", "n");
        assert_ne!(a, report_data("j", "w", "k", "m"));
        assert_ne!(a, report_data("j", "w", "kk", "n"));
        assert_ne!(
            report_data("ab", "c", "k", "n"),
            report_data("a", "bc", "k", "n")
        );
    }
}
