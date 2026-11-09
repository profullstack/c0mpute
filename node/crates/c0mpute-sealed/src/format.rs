//! The sealed object format: seal, open, rewrap.
//!
//! Construction (all keys 256-bit, all hashes SHA-256 unless noted):
//!
//! * `DEK`: 32 random bytes, drawn inside [`seal`] for every encryption. The
//!   API never accepts a caller-chosen DEK for new plaintext, so a DEK is
//!   never used for two different plaintexts. Sharing reuses the DEK only
//!   for the *same* ciphertext (it rewraps; it does not re-encrypt).
//! * `stream_id`: 16 random bytes per encryption.
//! * `commit = HKDF(ikm=DEK, salt=stream_id, info="c0mpute-private-v1 commit")`,
//!   stored in the header. AES-GCM is not key-committing, so without this a
//!   malicious sender could craft one ciphertext that opens to different
//!   plaintexts under different wrapped keys; with it, a blob admits exactly
//!   one DEK. Readers check it (constant time) before decrypting anything.
//! * `body_key = HKDF(DEK, stream_id, "c0mpute-private-v1 body")`.
//! * Stream: plaintext is cut into 64 KiB chunks; chunk `i` of `n` is
//!   `AES-256-GCM(body_key, nonce_i, aad_i, chunk)` with
//!   `nonce_i = 0x000000 || u64be(i) || last` (last = 1 only for `i = n-1`)
//!   and `aad_i = magic || stream_id || commit || u64be(i) || last`. The
//!   index in both nonce and AAD stops reordering; the last flag stops
//!   truncation and extension (the STREAM construction of Hoang, Reyhanitabar,
//!   Rogaway and Vizár). Nonces never repeat under one key: the key is unique
//!   per encryption and the index is unique per chunk.
//! * Wrap slot for recipient public key `R`: fresh ephemeral X25519 key `e`,
//!   `ss = X25519(e, R)` (rejected if non-contributory),
//!   `KEK = HKDF(ss, salt = e_pub || R, "c0mpute-private-v1 wrap")`,
//!   `slot = e_pub || AES-256-GCM(KEK, nonce=0^96, aad = magic||stream_id||commit, DEK)`.
//!   A zero nonce is safe because each KEK is used exactly once (it depends
//!   on a fresh ephemeral key). Slots carry no recipient identifier.
//! * Unused slots are 80 random bytes (top bit of the "public key" cleared,
//!   like a real encoding). A careful observer could still tell some dummy
//!   slots apart by checking curve membership, so the slot count is hidden
//!   from casual inspection only; see the threat model in docs.
//! * Authenticity: the stream starts with the metadata JSON and a 64-byte
//!   ed25519 signature by the per-object owner key over
//!   `"c0mpute-private-v1 object\n" site_id \n hex(stream_id) \n hex(commit)
//!   \n blake3(meta) \n blake3(content)`. Readers verify it, and check that
//!   the site id the metadata names is derived from that owner key (the same
//!   derivation the hosting layer uses). So a node or gateway cannot
//!   substitute a different object, even one encrypted to the reader's
//!   public key, and a recipient who knows the DEK cannot forge content the
//!   owner did not sign.

use std::collections::BTreeMap;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::Signer;
use hkdf::Hkdf;
use rand::RngCore;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use x25519_dalek::{EphemeralSecret, PublicKey};
use zeroize::Zeroizing;

use crate::SealError;
use crate::keys::{StoragePublic, StorageSecret};
use crate::pad::padme;

pub const MAGIC: [u8; 8] = *b"C0PRIV\x00\x01";
/// Key-wrap slots per object, always all present.
pub const MAX_RECIPIENTS: usize = 16;
const SLOT_LEN: usize = 32 + 32 + 16;
const FIXED_LEN: usize = 8 + 16 + 32 + 4;
const SLOTS_LEN: usize = MAX_RECIPIENTS * SLOT_LEN;
/// The owner's roster: who the object is wrapped to, encrypted under a key
/// only the owner can derive (recipients cannot see each other).
const ROSTER_PLAIN: usize = 1 + MAX_RECIPIENTS * 32;
const ROSTER_LEN: usize = 12 + ROSTER_PLAIN + 16;
/// Header size. Fixed, so rewrapping (share) never shifts the stream and
/// only the first network chunk of the object changes.
pub const HEADER_LEN: usize = FIXED_LEN + SLOTS_LEN + ROSTER_LEN;
/// Plaintext bytes per AEAD chunk.
pub const CHUNK_PLAINTEXT: usize = 64 * 1024;
const TAG_LEN: usize = 16;
const CHUNK_CIPHERTEXT: usize = CHUNK_PLAINTEXT + TAG_LEN;
/// Largest metadata block (a private site with thousands of paths fits).
pub const MAX_META: usize = 4 * 1024 * 1024;
/// Largest content in one sealed object. Bigger files are split into parts
/// by the caller ([`Kind::Multipart`]).
pub const MAX_CONTENT: u64 = 1 << 34;
const SIG_LEN: usize = 64;

const INFO_COMMIT: &[u8] = b"c0mpute-private-v1 commit";
const INFO_BODY: &[u8] = b"c0mpute-private-v1 body";
const INFO_WRAP: &[u8] = b"c0mpute-private-v1 wrap";
const INFO_ROSTER: &[u8] = b"c0mpute-private-v1 roster";
const SIG_DOMAIN: &str = "c0mpute-private-v1 object";

/// A data key. Zeroized on drop; not `Clone` (use [`DataKey::duplicate`]
/// deliberately when a copy is really needed).
pub struct DataKey(Zeroizing<[u8; 32]>);

impl DataKey {
    fn generate() -> Self {
        let mut k = Zeroizing::new([0u8; 32]);
        OsRng.fill_bytes(k.as_mut());
        Self(k)
    }

    /// Duplicate deliberately (e.g. to keep a site's link stable across
    /// versions; see [`seal_with_key`]).
    pub fn duplicate(&self) -> Self {
        Self(Zeroizing::new(*self.0))
    }

    /// base64url, for capability links (`#k=...`). Treat the result as a
    /// secret: anyone holding it can read the object.
    pub fn to_link_secret(&self) -> Zeroizing<String> {
        Zeroizing::new(URL_SAFE_NO_PAD.encode(self.0.as_ref()))
    }

    pub fn from_link_secret(s: &str) -> Result<Self, SealError> {
        let bytes = Zeroizing::new(URL_SAFE_NO_PAD.decode(s.trim()).map_err(|_| SealError::WrongKey)?);
        let arr: [u8; 32] = bytes.as_slice().try_into().map_err(|_| SealError::WrongKey)?;
        Ok(Self(Zeroizing::new(arr)))
    }

    fn bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Debug for PartRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PartRef").field("site_id", &self.site_id).field("key", &"<redacted>").field("len", &self.len).finish()
    }
}

impl std::fmt::Debug for DataKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DataKey(<redacted>)")
    }
}

/// What an object is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// One file.
    File,
    /// A whole static site: `content` is the files concatenated, `files`
    /// maps paths to ranges.
    Site,
    /// A file too big for one object: `content` is empty, `parts` lists the
    /// pieces (each its own sealed object, with its key inside this one).
    Multipart,
    /// One piece of a multipart file.
    Part,
    /// An encrypted volume's directory (`c0mpute storage mount`).
    Volume,
}

/// A file inside a [`Kind::Site`] object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteFile {
    pub offset: u64,
    pub len: u64,
    pub content_type: String,
}

/// One piece of a [`Kind::Multipart`] object.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartRef {
    pub site_id: String,
    /// base64url DEK of the part (it lives only inside this encrypted meta).
    pub key: String,
    pub len: u64,
}

/// Object metadata, encrypted inside the stream. Nodes never see it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    pub v: u8,
    pub kind: Kind,
    /// The random name the object's network record is published under (and
    /// from which the owner re-derives its signing key).
    pub name: String,
    /// The hosting site id the object lives at: `site_id_for(owner, name)`.
    pub site_id: String,
    /// Per-object owner key (ed25519, hex). Unlinkable to the user.
    pub owner: String,
    /// Exact content length.
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// Unix seconds.
    pub created: u64,
    /// Monotonic version (milliseconds since the epoch at sealing, or one
    /// more than the previous version). Signed with the rest of the
    /// metadata, so readers that remember the last version they saw can
    /// refuse a node or gateway replaying an older one.
    pub version: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub files: BTreeMap<String, SiteFile>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<PartRef>,
}

/// The hosting site id for an owner key (hex) and name: 20 hex characters of
/// blake3. Must stay identical to `c0mpute_core::hosting::site::site_id_for`
/// (a test in c0mpute-core pins the two together).
pub fn site_id_for(owner_hex: &str, name: &str) -> String {
    let h = blake3::hash(format!("c0mpute-site\n{owner_hex}\n{name}").as_bytes());
    hex::encode(h.as_bytes())[..20].to_string()
}

/// Everything [`seal`] needs besides the content.
pub struct SealParams<'a> {
    pub kind: Kind,
    /// Random network name; see [`Meta::name`].
    pub name: &'a str,
    /// The owner. Signs with the per-object key derived from `name`, and
    /// encrypts the roster to itself.
    pub owner: &'a StorageSecret,
    pub file_name: Option<String>,
    pub content_type: Option<String>,
    pub files: BTreeMap<String, SiteFile>,
    pub parts: Vec<PartRef>,
    /// Who can open it with their storage key. Usually just the owner.
    pub recipients: &'a [StoragePublic],
    pub created: u64,
    /// See [`Meta::version`]. 0 means "now, in milliseconds".
    pub version: u64,
}

/// Milliseconds since the epoch: the default object version.
pub fn now_version() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(1)
        .max(1)
}

/// A freshly sealed object.
pub struct Sealed {
    pub blob: Vec<u8>,
    pub dek: DataKey,
    pub site_id: String,
}

/// A successfully opened object.
pub struct Opened {
    pub meta: Meta,
    pub content: Zeroizing<Vec<u8>>,
}

impl std::fmt::Debug for Opened {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print plaintext.
        f.debug_struct("Opened").field("meta", &self.meta).field("content_len", &self.content.len()).finish()
    }
}

fn hkdf32(ikm: &[u8], salt: &[u8], info: &[u8]) -> Zeroizing<[u8; 32]> {
    let mut out = Zeroizing::new([0u8; 32]);
    Hkdf::<Sha256>::new(Some(salt), ikm)
        .expand(info, out.as_mut())
        .expect("32 bytes is a valid HKDF length");
    out
}

fn commitment(dek: &DataKey, stream_id: &[u8; 16]) -> [u8; 32] {
    *hkdf32(dek.bytes(), stream_id, INFO_COMMIT)
}

fn chunk_nonce(index: u64, last: bool) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[3..11].copy_from_slice(&index.to_be_bytes());
    n[11] = last as u8;
    n
}

fn chunk_aad(stream_id: &[u8; 16], commit: &[u8; 32], index: u64, last: bool) -> [u8; 8 + 16 + 32 + 9] {
    let mut a = [0u8; 8 + 16 + 32 + 9];
    a[..8].copy_from_slice(&MAGIC);
    a[8..24].copy_from_slice(stream_id);
    a[24..56].copy_from_slice(commit);
    a[56..64].copy_from_slice(&index.to_be_bytes());
    a[64] = last as u8;
    a
}

fn slot_aad(stream_id: &[u8; 16], commit: &[u8; 32]) -> [u8; 8 + 16 + 32] {
    let mut a = [0u8; 8 + 16 + 32];
    a[..8].copy_from_slice(&MAGIC);
    a[8..24].copy_from_slice(stream_id);
    a[24..].copy_from_slice(commit);
    a
}

fn signing_message(site_id: &str, stream_id: &[u8; 16], commit: &[u8; 32], meta: &[u8], content: &[u8]) -> Vec<u8> {
    format!(
        "{SIG_DOMAIN}\n{site_id}\n{}\n{}\n{}\n{}",
        hex::encode(stream_id),
        hex::encode(commit),
        blake3::hash(meta).to_hex(),
        blake3::hash(content).to_hex()
    )
    .into_bytes()
}

/// Build the 16 slots: one per recipient, the rest random.
fn build_slots(dek: &DataKey, stream_id: &[u8; 16], commit: &[u8; 32], recipients: &[StoragePublic]) -> Result<Vec<u8>, SealError> {
    if recipients.len() > MAX_RECIPIENTS {
        return Err(SealError::TooManyRecipients(MAX_RECIPIENTS));
    }
    let aad = slot_aad(stream_id, commit);
    let mut slots: Vec<[u8; SLOT_LEN]> = Vec::with_capacity(MAX_RECIPIENTS);
    for r in recipients {
        let eph = EphemeralSecret::random_from_rng(OsRng);
        let eph_pub = PublicKey::from(&eph);
        let rpub = PublicKey::from(r.0);
        let ss = eph.diffie_hellman(&rpub);
        if !ss.was_contributory() {
            return Err(SealError::BadMeta("recipient key is a low-order point".into()));
        }
        let mut salt = [0u8; 64];
        salt[..32].copy_from_slice(eph_pub.as_bytes());
        salt[32..].copy_from_slice(&r.0);
        let kek = hkdf32(ss.as_bytes(), &salt, INFO_WRAP);
        let cipher = Aes256Gcm::new_from_slice(kek.as_ref()).expect("32-byte key");
        let wrapped = cipher
            .encrypt(Nonce::from_slice(&[0u8; 12]), Payload { msg: dek.bytes(), aad: &aad })
            .map_err(|_| SealError::Tampered)?;
        let mut slot = [0u8; SLOT_LEN];
        slot[..32].copy_from_slice(eph_pub.as_bytes());
        slot[32..].copy_from_slice(&wrapped);
        slots.push(slot);
    }
    while slots.len() < MAX_RECIPIENTS {
        // A real, fresh ephemeral public key followed by random bytes, so a
        // dummy slot looks exactly like a real one (always a curve point).
        let mut slot = [0u8; SLOT_LEN];
        OsRng.fill_bytes(&mut slot[32..]);
        let eph = EphemeralSecret::random_from_rng(OsRng);
        slot[..32].copy_from_slice(PublicKey::from(&eph).as_bytes());
        slots.push(slot);
    }
    // Shuffle so the owner's slot is not always first.
    for i in (1..slots.len()).rev() {
        let j = (OsRng.next_u32() as usize) % (i + 1);
        slots.swap(i, j);
    }
    Ok(slots.concat())
}

fn roster_key(owner: &StorageSecret, stream_id: &[u8; 16]) -> Zeroizing<[u8; 32]> {
    hkdf32(owner.local_index_key(), stream_id, INFO_ROSTER)
}

fn seal_roster(owner: &StorageSecret, stream_id: &[u8; 16], commit: &[u8; 32], recipients: &[StoragePublic]) -> Result<Vec<u8>, SealError> {
    let mut plain = [0u8; ROSTER_PLAIN];
    plain[0] = recipients.len() as u8;
    for (i, r) in recipients.iter().enumerate() {
        plain[1 + i * 32..1 + (i + 1) * 32].copy_from_slice(&r.0);
    }
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut nonce);
    let key = roster_key(owner, stream_id);
    let cipher = Aes256Gcm::new_from_slice(key.as_ref()).expect("32-byte key");
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce), Payload { msg: &plain, aad: &slot_aad(stream_id, commit) })
        .map_err(|_| SealError::Tampered)?;
    let mut out = Vec::with_capacity(ROSTER_LEN);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    debug_assert_eq!(out.len(), ROSTER_LEN);
    Ok(out)
}

fn write_header(stream_id: &[u8; 16], commit: &[u8; 32], slots: &[u8], roster: &[u8]) -> Vec<u8> {
    let mut h = Vec::with_capacity(HEADER_LEN);
    h.extend_from_slice(&MAGIC);
    h.extend_from_slice(stream_id);
    h.extend_from_slice(commit);
    h.extend_from_slice(&(MAX_RECIPIENTS as u16).to_be_bytes());
    h.extend_from_slice(&[0u8; 2]);
    h.extend_from_slice(slots);
    h.extend_from_slice(roster);
    debug_assert_eq!(h.len(), HEADER_LEN);
    h
}

/// Seal `content`. Draws a fresh DEK and stream id; the result is ready to
/// publish. `content` and the DEK never leave this process.
pub fn seal(content: &[u8], p: &SealParams<'_>) -> Result<Sealed, SealError> {
    seal_with_key(content, p, DataKey::generate())
}

/// Seal `content` under an existing DEK, with a fresh random stream id.
///
/// Used to publish a new version of an object (a private site) without
/// changing its capability link. Safe because nothing is ever encrypted
/// directly under the DEK: the body key and the commitment are derived from
/// the DEK *and* the 128-bit random stream id, so every encryption gets its
/// own body key and its own nonce space. Never pass a DEK obtained from
/// someone else's object.
pub fn seal_with_key(content: &[u8], p: &SealParams<'_>, dek: DataKey) -> Result<Sealed, SealError> {
    if content.len() as u64 > MAX_CONTENT {
        return Err(SealError::TooLarge);
    }
    let signer = p.owner.object_signing_key(p.name);
    let owner_hex = hex::encode(signer.verifying_key().as_bytes());
    let site_id = site_id_for(&owner_hex, p.name);
    let meta = Meta {
        v: 1,
        kind: p.kind,
        name: p.name.to_string(),
        site_id: site_id.clone(),
        owner: owner_hex,
        size: content.len() as u64,
        file_name: p.file_name.clone(),
        content_type: p.content_type.clone(),
        created: p.created,
        version: if p.version == 0 { now_version() } else { p.version },
        files: p.files.clone(),
        parts: p.parts.clone(),
    };
    let meta_bytes = Zeroizing::new(serde_json::to_vec(&meta).map_err(|e| SealError::BadMeta(e.to_string()))?);
    if meta_bytes.len() > MAX_META {
        return Err(SealError::BadMeta("metadata too large".into()));
    }

    let mut stream_id = [0u8; 16];
    OsRng.fill_bytes(&mut stream_id);
    let commit = commitment(&dek, &stream_id);

    let sig = signer.sign(&signing_message(&site_id, &stream_id, &commit, &meta_bytes, content));

    // Plaintext stream: meta_len | meta | sig | content | zero padding.
    let unpadded = 4 + meta_bytes.len() as u64 + SIG_LEN as u64 + content.len() as u64;
    let padded = padme(unpadded) as usize;
    let mut plain = Zeroizing::new(Vec::with_capacity(padded));
    plain.extend_from_slice(&(meta_bytes.len() as u32).to_be_bytes());
    plain.extend_from_slice(&meta_bytes);
    plain.extend_from_slice(&sig.to_bytes());
    plain.extend_from_slice(content);
    plain.resize(padded, 0);

    let slots = build_slots(&dek, &stream_id, &commit, p.recipients)?;
    let roster = seal_roster(p.owner, &stream_id, &commit, p.recipients)?;
    let mut blob = write_header(&stream_id, &commit, &slots, &roster);
    blob.reserve(padded + padded.div_ceil(CHUNK_PLAINTEXT) * TAG_LEN);

    let body_key = hkdf32(dek.bytes(), &stream_id, INFO_BODY);
    let cipher = Aes256Gcm::new_from_slice(body_key.as_ref()).expect("32-byte key");
    let n = plain.len().div_ceil(CHUNK_PLAINTEXT);
    for (i, chunk) in plain.chunks(CHUNK_PLAINTEXT).enumerate() {
        let last = i + 1 == n;
        let ct = cipher
            .encrypt(
                Nonce::from_slice(&chunk_nonce(i as u64, last)),
                Payload { msg: chunk, aad: &chunk_aad(&stream_id, &commit, i as u64, last) },
            )
            .map_err(|_| SealError::Tampered)?;
        blob.extend_from_slice(&ct);
    }
    Ok(Sealed { blob, dek, site_id })
}

struct Header<'a> {
    stream_id: [u8; 16],
    commit: [u8; 32],
    slots: &'a [u8],
    roster: &'a [u8],
    stream: &'a [u8],
}

fn parse(blob: &[u8]) -> Result<Header<'_>, SealError> {
    if blob.len() < HEADER_LEN + TAG_LEN || blob[..8] != MAGIC {
        return Err(SealError::NotSealed);
    }
    let count = u16::from_be_bytes([blob[56], blob[57]]) as usize;
    if count != MAX_RECIPIENTS {
        return Err(SealError::NotSealed);
    }
    let mut stream_id = [0u8; 16];
    stream_id.copy_from_slice(&blob[8..24]);
    let mut commit = [0u8; 32];
    commit.copy_from_slice(&blob[24..56]);
    Ok(Header {
        stream_id,
        commit,
        slots: &blob[FIXED_LEN..FIXED_LEN + SLOTS_LEN],
        roster: &blob[FIXED_LEN + SLOTS_LEN..HEADER_LEN],
        stream: &blob[HEADER_LEN..],
    })
}

/// Find and unwrap the slot addressed to `secret`.
fn unwrap_slot(h: &Header<'_>, secret: &StorageSecret) -> Result<DataKey, SealError> {
    let me = secret.public();
    let aad = slot_aad(&h.stream_id, &h.commit);
    let mut found: Option<DataKey> = None;
    // Try every slot (no early exit), so timing does not say which slot is
    // ours to an observer of the decryptor.
    for slot in h.slots.chunks_exact(SLOT_LEN) {
        let mut eph = [0u8; 32];
        eph.copy_from_slice(&slot[..32]);
        let ss = secret.x25519().diffie_hellman(&PublicKey::from(eph));
        if !ss.was_contributory() {
            continue;
        }
        let mut salt = [0u8; 64];
        salt[..32].copy_from_slice(&eph);
        salt[32..].copy_from_slice(&me.0);
        let kek = hkdf32(ss.as_bytes(), &salt, INFO_WRAP);
        let cipher = Aes256Gcm::new_from_slice(kek.as_ref()).expect("32-byte key");
        if let Ok(pt) = cipher.decrypt(Nonce::from_slice(&[0u8; 12]), Payload { msg: &slot[32..], aad: &aad }) {
            let pt = Zeroizing::new(pt);
            if pt.len() == 32 && found.is_none() {
                let mut k = Zeroizing::new([0u8; 32]);
                k.copy_from_slice(&pt);
                let candidate = DataKey(k);
                // Only a key that matches the commitment counts, so a slot
                // injected into the (unauthenticated) header cannot shadow
                // the real one.
                if bool::from(commitment(&candidate, &h.stream_id).ct_eq(&h.commit)) {
                    found = Some(candidate);
                }
            }
        }
    }
    found.ok_or(SealError::NoSlot)
}

/// Decrypt the stream with a DEK and verify everything. `expected_site_id`
/// pins the object the caller asked for; pass it whenever known.
pub fn open(blob: &[u8], dek: &DataKey, expected_site_id: Option<&str>) -> Result<Opened, SealError> {
    let h = parse(blob)?;
    open_parsed(&h, dek, expected_site_id)
}

/// Open with the reader's storage secret (trial-decrypts the slots).
/// Returns the DEK too, for rewrapping or building capability links.
pub fn open_with_secret(blob: &[u8], secret: &StorageSecret, expected_site_id: Option<&str>) -> Result<(Opened, DataKey), SealError> {
    let h = parse(blob)?;
    let dek = unwrap_slot(&h, secret)?;
    let opened = open_parsed(&h, &dek, expected_site_id)?;
    Ok((opened, dek))
}

fn open_parsed(h: &Header<'_>, dek: &DataKey, expected_site_id: Option<&str>) -> Result<Opened, SealError> {
    let want = commitment(dek, &h.stream_id);
    if !bool::from(want.ct_eq(&h.commit)) {
        return Err(SealError::WrongKey);
    }
    let body_key = hkdf32(dek.bytes(), &h.stream_id, INFO_BODY);
    let cipher = Aes256Gcm::new_from_slice(body_key.as_ref()).expect("32-byte key");

    let n = h.stream.len().div_ceil(CHUNK_CIPHERTEXT);
    if n == 0 {
        return Err(SealError::Tampered);
    }
    // The final chunk must hold its tag and at least one byte; a stream that
    // ends in a shorter sliver was cut mid-chunk.
    let tail = h.stream.len() - (n - 1) * CHUNK_CIPHERTEXT;
    if tail < TAG_LEN + 1 {
        return Err(SealError::Tampered);
    }
    let mut plain = Zeroizing::new(Vec::with_capacity(h.stream.len()));
    for (i, ct) in h.stream.chunks(CHUNK_CIPHERTEXT).enumerate() {
        let last = i + 1 == n;
        let pt = cipher
            .decrypt(
                Nonce::from_slice(&chunk_nonce(i as u64, last)),
                Payload { msg: ct, aad: &chunk_aad(&h.stream_id, &h.commit, i as u64, last) },
            )
            .map_err(|_| SealError::Tampered)?;
        let pt = Zeroizing::new(pt);
        plain.extend_from_slice(&pt);
    }

    if plain.len() < 4 {
        return Err(SealError::BadMeta("stream too short".into()));
    }
    let meta_len = u32::from_be_bytes([plain[0], plain[1], plain[2], plain[3]]) as usize;
    if meta_len > MAX_META || 4 + meta_len + SIG_LEN > plain.len() {
        return Err(SealError::BadMeta("bad metadata length".into()));
    }
    let meta_bytes = &plain[4..4 + meta_len];
    let meta: Meta = serde_json::from_slice(meta_bytes).map_err(|e| SealError::BadMeta(e.to_string()))?;
    if meta.v != 1 {
        return Err(SealError::BadMeta(format!("unsupported version {}", meta.v)));
    }
    let sig_at = 4 + meta_len;
    let content_at = sig_at + SIG_LEN;
    let size = usize::try_from(meta.size).map_err(|_| SealError::BadMeta("size".into()))?;
    if content_at.checked_add(size).is_none_or(|end| end > plain.len()) {
        return Err(SealError::BadMeta("size exceeds stream".into()));
    }
    let content = &plain[content_at..content_at + size];

    // Who signed it, and is that the owner the site id commits to?
    let owner_bytes: [u8; 32] = hex::decode(&meta.owner)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or(SealError::BadSignature)?;
    let owner = ed25519_dalek::VerifyingKey::from_bytes(&owner_bytes).map_err(|_| SealError::BadSignature)?;
    if site_id_for(&meta.owner, &meta.name) != meta.site_id {
        return Err(SealError::BadSignature);
    }
    let sig_bytes: [u8; 64] = plain[sig_at..content_at].try_into().expect("64 bytes");
    let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
    owner
        .verify_strict(&signing_message(&meta.site_id, &h.stream_id, &h.commit, meta_bytes, content), &sig)
        .map_err(|_| SealError::BadSignature)?;
    if let Some(expected) = expected_site_id
        && expected != meta.site_id
    {
        return Err(SealError::WrongObject {
            expected: expected.to_string(),
            got: meta.site_id.clone(),
        });
    }
    for f in meta.files.values() {
        if f.offset.checked_add(f.len).is_none_or(|end| end > meta.size) {
            return Err(SealError::BadMeta("site file out of range".into()));
        }
    }
    // Padding is authenticated, and must be zeros.
    if plain[content_at + size..].iter().any(|b| *b != 0) {
        return Err(SealError::BadMeta("non-zero padding".into()));
    }
    Ok(Opened {
        content: Zeroizing::new(content.to_vec()),
        meta,
    })
}

/// Replace the key slots (share with, or stop wrapping to, recipients)
/// without touching the ciphertext stream. `dek` must be this object's DEK.
///
/// This is *not* revocation: anyone who already unwrapped the DEK can still
/// read this ciphertext. Revoking means re-sealing the content under a new
/// DEK (a fresh [`seal`]) and publishing that version.
pub fn rewrap(blob: &[u8], dek: &DataKey, owner: &StorageSecret, recipients: &[StoragePublic]) -> Result<Vec<u8>, SealError> {
    let h = parse(blob)?;
    if !bool::from(commitment(dek, &h.stream_id).ct_eq(&h.commit)) {
        return Err(SealError::WrongKey);
    }
    let slots = build_slots(dek, &h.stream_id, &h.commit, recipients)?;
    let roster = seal_roster(owner, &h.stream_id, &h.commit, recipients)?;
    let mut out = write_header(&h.stream_id, &h.commit, &slots, &roster);
    out.extend_from_slice(h.stream);
    Ok(out)
}

/// Who an object is wrapped to. Only the owner (the secret that sealed it)
/// can read this; anyone else gets [`SealError::NoSlot`].
pub fn roster(blob: &[u8], owner: &StorageSecret) -> Result<Vec<StoragePublic>, SealError> {
    let h = parse(blob)?;
    let key = roster_key(owner, &h.stream_id);
    let cipher = Aes256Gcm::new_from_slice(key.as_ref()).expect("32-byte key");
    let plain = cipher
        .decrypt(Nonce::from_slice(&h.roster[..12]), Payload { msg: &h.roster[12..], aad: &slot_aad(&h.stream_id, &h.commit) })
        .map_err(|_| SealError::NoSlot)?;
    let n = plain[0] as usize;
    if n > MAX_RECIPIENTS || plain.len() != ROSTER_PLAIN {
        return Err(SealError::BadMeta("bad roster".into()));
    }
    Ok((0..n)
        .map(|i| {
            let mut k = [0u8; 32];
            k.copy_from_slice(&plain[1 + i * 32..1 + (i + 1) * 32]);
            StoragePublic(k)
        })
        .collect())
}

/// The DEK from a slot addressed to `secret`, without decrypting the body
/// (e.g. to rewrap a large object).
pub fn unwrap_dek(blob: &[u8], secret: &StorageSecret) -> Result<DataKey, SealError> {
    let h = parse(blob)?;
    let dek = unwrap_slot(&h, secret)?;
    if !bool::from(commitment(&dek, &h.stream_id).ct_eq(&h.commit)) {
        return Err(SealError::WrongKey);
    }
    Ok(dek)
}

/// A random object name for the network record: 26 base32-ish characters
/// from 128 random bits, valid as a hosting site name.
pub fn random_name() -> String {
    let mut b = [0u8; 16];
    OsRng.fill_bytes(&mut b);
    base32_name(&b)
}

/// 128 bits as 26 lowercase base32 characters (a valid hosting site name).
pub(crate) fn base32_name(b: &[u8; 16]) -> String {
    const ALPHA: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut out = String::with_capacity(26);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for &byte in b {
        acc = (acc << 8) | byte as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHA[((acc >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHA[((acc << (5 - bits)) & 31) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params<'a>(owner: &'a StorageSecret, name: &'a str, rcpts: &'a [StoragePublic]) -> SealParams<'a> {
        SealParams {
            kind: Kind::File,
            name,
            owner,
            file_name: Some("secret-report.pdf".into()),
            content_type: Some("application/pdf".into()),
            files: BTreeMap::new(),
            parts: Vec::new(),
            recipients: rcpts,
            created: 1_700_000_000,
            version: 0,
        }
    }

    fn sample(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 + i / 251) as u8).collect()
    }

    struct Fixture {
        alice: StorageSecret,
        name: String,
    }

    fn fixture() -> Fixture {
        Fixture {
            alice: StorageSecret::generate(),
            name: random_name(),
        }
    }

    #[test]
    fn round_trip_many_sizes() {
        let f = fixture();
        let me = [f.alice.public()];
        for len in [0, 1, 100, CHUNK_PLAINTEXT - 1, CHUNK_PLAINTEXT, CHUNK_PLAINTEXT + 1, 3 * CHUNK_PLAINTEXT + 17, 1 << 20] {
            let data = sample(len);
            let s = seal(&data, &params(&f.alice, &f.name, &me)).unwrap();
            let (o, _dek) = open_with_secret(&s.blob, &f.alice, Some(&s.site_id)).unwrap();
            assert_eq!(&*o.content, &data, "len {len}");
            assert_eq!(o.meta.file_name.as_deref(), Some("secret-report.pdf"));
            assert_eq!(o.meta.size, len as u64);
            // And with the DEK directly (capability links).
            let o2 = open(&s.blob, &s.dek, None).unwrap();
            assert_eq!(&*o2.content, &data);
        }
    }

    #[test]
    fn ciphertext_hides_plaintext_and_name() {
        let f = fixture();
        let me = [f.alice.public()];
        let data = b"TOP-SECRET-MARKER ".repeat(5000);
        let s = seal(&data, &params(&f.alice, &f.name, &me)).unwrap();
        let hay = &s.blob;
        for needle in [&b"TOP-SECRET-MARKER"[..], b"secret-report.pdf", b"application/pdf", f.name.as_bytes()] {
            assert!(!hay.windows(needle.len()).any(|w| w == needle), "{:?} leaked", String::from_utf8_lossy(needle));
        }
        // The owner's storage public key does not appear either.
        assert!(!hay.windows(32).any(|w| w == f.alice.public().0));
    }

    #[test]
    fn sizes_are_padded_to_buckets() {
        let f = fixture();
        let me = [f.alice.public()];
        let a = seal(&sample(10), &params(&f.alice, &f.name, &me)).unwrap();
        let b = seal(&sample(2000), &params(&f.alice, &f.name, &me)).unwrap();
        assert_eq!(a.blob.len(), b.blob.len());
        let c = seal(&sample(1_000_000), &params(&f.alice, &f.name, &me)).unwrap();
        let d = seal(&sample(1_010_000), &params(&f.alice, &f.name, &me)).unwrap();
        assert_eq!(c.blob.len(), d.blob.len());
    }

    #[test]
    fn same_plaintext_twice_gives_unrelated_ciphertext() {
        // No convergent encryption: equal files must not be detectable.
        let f = fixture();
        let me = [f.alice.public()];
        let data = sample(200_000);
        let a = seal(&data, &params(&f.alice, &f.name, &me)).unwrap();
        let b = seal(&data, &params(&f.alice, &f.name, &me)).unwrap();
        assert_ne!(a.blob[HEADER_LEN..HEADER_LEN + 64], b.blob[HEADER_LEN..HEADER_LEN + 64]);
        assert_ne!(blake3::hash(&a.blob), blake3::hash(&b.blob));
    }

    #[test]
    fn wrong_key_fails() {
        let f = fixture();
        let me = [f.alice.public()];
        let s = seal(b"hello", &params(&f.alice, &f.name, &me)).unwrap();
        let mallory = StorageSecret::generate();
        assert_eq!(open_with_secret(&s.blob, &mallory, None).unwrap_err(), SealError::NoSlot);
        let wrong = DataKey::generate();
        assert_eq!(open(&s.blob, &wrong, None).unwrap_err(), SealError::WrongKey);
    }

    #[test]
    fn tampered_chunk_fails() {
        let f = fixture();
        let me = [f.alice.public()];
        let s = seal(&sample(300_000), &params(&f.alice, &f.name, &me)).unwrap();
        for at in [HEADER_LEN + 5, HEADER_LEN + CHUNK_CIPHERTEXT + 100, s.blob.len() - 1] {
            let mut bad = s.blob.clone();
            bad[at] ^= 1;
            assert_eq!(open(&bad, &s.dek, None).unwrap_err(), SealError::Tampered, "flip at {at}");
        }
        // Flipping the commitment is a wrong-key error, the stream id a tamper.
        let mut bad = s.blob.clone();
        bad[30] ^= 1;
        assert_eq!(open(&bad, &s.dek, None).unwrap_err(), SealError::WrongKey);
        let mut bad = s.blob.clone();
        bad[10] ^= 1;
        assert!(open(&bad, &s.dek, None).is_err());
    }

    #[test]
    fn chunk_reorder_fails() {
        let f = fixture();
        let me = [f.alice.public()];
        let s = seal(&sample(4 * CHUNK_PLAINTEXT), &params(&f.alice, &f.name, &me)).unwrap();
        let mut bad = s.blob.clone();
        let a = HEADER_LEN;
        let b = HEADER_LEN + CHUNK_CIPHERTEXT;
        let first: Vec<u8> = bad[a..b].to_vec();
        let second: Vec<u8> = bad[b..b + CHUNK_CIPHERTEXT].to_vec();
        bad[a..b].copy_from_slice(&second);
        bad[b..b + CHUNK_CIPHERTEXT].copy_from_slice(&first);
        assert_eq!(open(&bad, &s.dek, None).unwrap_err(), SealError::Tampered);
    }

    #[test]
    fn truncation_and_extension_fail() {
        let f = fixture();
        let me = [f.alice.public()];
        let s = seal(&sample(4 * CHUNK_PLAINTEXT), &params(&f.alice, &f.name, &me)).unwrap();
        // Drop the last whole chunk: the new final chunk was not sealed as last.
        let stream_len = s.blob.len() - HEADER_LEN;
        let n = stream_len.div_ceil(CHUNK_CIPHERTEXT);
        let cut = HEADER_LEN + (n - 1) * CHUNK_CIPHERTEXT;
        assert_eq!(open(&s.blob[..cut], &s.dek, None).unwrap_err(), SealError::Tampered);
        // Cut mid-chunk.
        assert_eq!(open(&s.blob[..s.blob.len() - 7], &s.dek, None).unwrap_err(), SealError::Tampered);
        // Append a copy of a chunk.
        let mut ext = s.blob.clone();
        ext.extend_from_slice(&s.blob[HEADER_LEN..HEADER_LEN + CHUNK_CIPHERTEXT]);
        assert_eq!(open(&ext, &s.dek, None).unwrap_err(), SealError::Tampered);
        // Only the header.
        assert!(open(&s.blob[..HEADER_LEN], &s.dek, None).is_err());
    }

    #[test]
    fn sharing_adds_a_reader_without_touching_the_stream() {
        let f = fixture();
        let bob = StorageSecret::generate();
        let s = seal(b"shared plans", &params(&f.alice, &f.name, &[f.alice.public()])).unwrap();
        assert!(open_with_secret(&s.blob, &bob, None).is_err());
        let (_, dek) = open_with_secret(&s.blob, &f.alice, None).unwrap();
        let shared = rewrap(&s.blob, &dek, &f.alice, &[f.alice.public(), bob.public()]).unwrap();
        assert_eq!(shared.len(), s.blob.len());
        assert_eq!(shared[HEADER_LEN..], s.blob[HEADER_LEN..], "stream unchanged");
        assert_eq!(shared[..FIXED_LEN], s.blob[..FIXED_LEN], "header id unchanged");
        assert_eq!(roster(&shared, &f.alice).unwrap(), vec![f.alice.public(), bob.public()]);
        assert_eq!(roster(&shared, &bob).unwrap_err(), SealError::NoSlot, "recipients cannot read the roster");
        let (o, _) = open_with_secret(&shared, &bob, Some(&s.site_id)).unwrap();
        assert_eq!(&*o.content, b"shared plans");
        assert!(open_with_secret(&shared, &f.alice, None).is_ok());
        // Unsharing (rewrap without bob) stops new unwraps by bob.
        let unshared = rewrap(&shared, &dek, &f.alice, &[f.alice.public()]).unwrap();
        assert_eq!(open_with_secret(&unshared, &bob, None).unwrap_err(), SealError::NoSlot);
    }

    #[test]
    fn revocation_by_rekey_locks_out_the_old_key() {
        let f = fixture();
        let bob = StorageSecret::generate();
        let both = [f.alice.public(), bob.public()];
        let v1 = seal(b"v1 contents", &params(&f.alice, &f.name, &both)).unwrap();
        let (_, bob_dek) = open_with_secret(&v1.blob, &bob, None).unwrap();
        // Revoke bob: re-seal (new DEK, new stream id) for alice only.
        let v2 = seal(b"v1 contents", &params(&f.alice, &f.name, &[f.alice.public()])).unwrap();
        assert_eq!(v2.site_id, v1.site_id, "same object, new version");
        assert_eq!(open_with_secret(&v2.blob, &bob, None).unwrap_err(), SealError::NoSlot);
        assert_eq!(open(&v2.blob, &bob_dek, None).unwrap_err(), SealError::WrongKey);
        assert!(open_with_secret(&v2.blob, &f.alice, None).is_ok());
    }

    #[test]
    fn rewrap_with_wrong_dek_is_refused() {
        let f = fixture();
        let s = seal(b"x", &params(&f.alice, &f.name, &[f.alice.public()])).unwrap();
        assert_eq!(rewrap(&s.blob, &DataKey::generate(), &f.alice, &[f.alice.public()]).unwrap_err(), SealError::WrongKey);
    }

    #[test]
    fn substituted_object_is_rejected() {
        // Mallory knows alice's public key and seals her own content to it,
        // under her own owner key, and serves it for alice's site id.
        let f = fixture();
        let real = seal(b"real", &params(&f.alice, &f.name, &[f.alice.public()])).unwrap();
        let mallory = StorageSecret::generate();
        let fake = seal(b"fake", &params(&mallory, &f.name, &[f.alice.public()])).unwrap();
        let err = open_with_secret(&fake.blob, &f.alice, Some(&real.site_id)).unwrap_err();
        assert!(matches!(err, SealError::WrongObject { .. }), "{err:?}");
    }

    #[test]
    fn recipient_cannot_forge_content_under_the_owner_signature() {
        // Bob holds the DEK. He re-encrypts different content with it, the
        // same stream id and commitment, and copies alice's signature.
        let f = fixture();
        let bob = StorageSecret::generate();
        let s = seal(b"original", &params(&f.alice, &f.name, &[f.alice.public(), bob.public()])).unwrap();
        let (_, dek) = open_with_secret(&s.blob, &bob, None).unwrap();
        let h = parse(&s.blob).unwrap();
        let body_key = hkdf32(dek.bytes(), &h.stream_id, INFO_BODY);
        let cipher = Aes256Gcm::new_from_slice(body_key.as_ref()).unwrap();
        // Decrypt the genuine stream, swap the content, re-encrypt.
        let n = h.stream.len().div_ceil(CHUNK_CIPHERTEXT);
        let mut plain = Vec::new();
        for (i, ct) in h.stream.chunks(CHUNK_CIPHERTEXT).enumerate() {
            let last = i + 1 == n;
            plain.extend(
                cipher
                    .decrypt(Nonce::from_slice(&chunk_nonce(i as u64, last)), Payload { msg: ct, aad: &chunk_aad(&h.stream_id, &h.commit, i as u64, last) })
                    .unwrap(),
            );
        }
        let meta_len = u32::from_be_bytes(plain[..4].try_into().unwrap()) as usize;
        let at = 4 + meta_len + SIG_LEN;
        plain[at..at + 8].copy_from_slice(b"FORGED!!");
        let mut forged = s.blob[..HEADER_LEN].to_vec();
        for (i, chunk) in plain.chunks(CHUNK_PLAINTEXT).enumerate() {
            let last = i + 1 == n;
            forged.extend(
                cipher
                    .encrypt(Nonce::from_slice(&chunk_nonce(i as u64, last)), Payload { msg: chunk, aad: &chunk_aad(&h.stream_id, &h.commit, i as u64, last) })
                    .unwrap(),
            );
        }
        assert_eq!(open_with_secret(&forged, &f.alice, None).unwrap_err(), SealError::BadSignature);
    }

    #[test]
    fn too_many_recipients_is_refused() {
        let f = fixture();
        let many: Vec<StoragePublic> = (0..MAX_RECIPIENTS + 1).map(|_| StorageSecret::generate().public()).collect();
        assert_eq!(
            seal(b"x", &params(&f.alice, &f.name, &many)).err().unwrap(),
            SealError::TooManyRecipients(MAX_RECIPIENTS)
        );
    }

    #[test]
    fn low_order_recipient_key_is_refused() {
        let f = fixture();
        let zero = StoragePublic([0u8; 32]);
        assert!(seal(b"x", &params(&f.alice, &f.name, &[zero])).is_err());
    }

    #[test]
    fn link_secret_round_trips() {
        let f = fixture();
        let s = seal(b"by link", &params(&f.alice, &f.name, &[])).unwrap();
        let link = s.dek.to_link_secret();
        let dek = DataKey::from_link_secret(&link).unwrap();
        assert_eq!(&*open(&s.blob, &dek, Some(&s.site_id)).unwrap().content, b"by link");
        assert!(DataKey::from_link_secret("short").is_err());
    }

    #[test]
    fn site_objects_check_their_file_table() {
        let f = fixture();
        let mut files = BTreeMap::new();
        files.insert("index.html".to_string(), SiteFile { offset: 0, len: 5, content_type: "text/html".into() });
        let mut p = params(&f.alice, &f.name, &[]);
        p.kind = Kind::Site;
        p.files = files;
        let s = seal(b"<p>hi", &p).unwrap();
        let o = open(&s.blob, &s.dek, None).unwrap();
        assert_eq!(o.meta.kind, Kind::Site);
        assert_eq!(o.meta.files["index.html"].len, 5);
    }

    #[test]
    fn random_names_are_valid_and_distinct() {
        let a = random_name();
        let b = random_name();
        assert_ne!(a, b);
        assert_eq!(a.len(), 26);
        assert!(a.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
    }

    #[test]
    fn new_version_under_the_same_key_keeps_the_link_but_not_the_keystream() {
        let f = fixture();
        let v1 = seal(b"site v1", &params(&f.alice, &f.name, &[f.alice.public()])).unwrap();
        let link = v1.dek.to_link_secret();
        let v2 = seal_with_key(b"site v2", &params(&f.alice, &f.name, &[f.alice.public()]), v1.dek.duplicate()).unwrap();
        let dek = DataKey::from_link_secret(&link).unwrap();
        assert_eq!(&*open(&v2.blob, &dek, Some(&v1.site_id)).unwrap().content, b"site v2");
        // Fresh stream id, so a fresh body key and commitment.
        assert_ne!(v1.blob[8..56], v2.blob[8..56]);
    }

    #[test]
    fn roster_lists_recipients_for_the_owner_only() {
        let f = fixture();
        let bob = StorageSecret::generate();
        let s = seal(b"x", &params(&f.alice, &f.name, &[f.alice.public(), bob.public()])).unwrap();
        assert_eq!(roster(&s.blob, &f.alice).unwrap(), vec![f.alice.public(), bob.public()]);
        assert!(roster(&s.blob, &bob).is_err());
        let mut bad = s.blob.clone();
        bad[HEADER_LEN - 3] ^= 1;
        assert!(roster(&bad, &f.alice).is_err());
    }

    #[test]
    fn injected_slot_cannot_shadow_the_real_one() {
        let f = fixture();
        let s = seal(b"real", &params(&f.alice, &f.name, &[f.alice.public()])).unwrap();
        // Replace the whole slot table with a wrap of a different key to
        // alice first, followed by the original slots.
        let h = parse(&s.blob).unwrap();
        let fake = build_slots(&DataKey::generate(), &h.stream_id, &h.commit, &[f.alice.public()]).unwrap();
        let mut bad = s.blob.clone();
        // Overwrite every slot but alice's real one with fake-key slots.
        for i in 0..MAX_RECIPIENTS {
            let at = FIXED_LEN + i * SLOT_LEN;
            let probe_ok = {
                let mut t = s.blob.clone();
                t[at..at + SLOT_LEN].copy_from_slice(&fake[..SLOT_LEN]);
                open_with_secret(&t, &f.alice, None).is_ok()
            };
            if probe_ok {
                bad[at..at + SLOT_LEN].copy_from_slice(&fake[..SLOT_LEN]);
            }
        }
        assert_eq!(&*open_with_secret(&bad, &f.alice, None).unwrap().0.content, b"real");
    }

    #[test]
    fn dummy_slots_carry_curve_points() {
        let f = fixture();
        let s = seal(b"x", &params(&f.alice, &f.name, &[])).unwrap();
        // With no recipients every slot is a dummy: each starts with a valid
        // X25519 public key (top bit clear in the canonical encoding).
        for i in 0..MAX_RECIPIENTS {
            assert_eq!(s.blob[FIXED_LEN + i * SLOT_LEN + 31] & 0x80, 0);
        }
    }

    #[test]
    fn versions_are_signed() {
        let f = fixture();
        let mut p = params(&f.alice, &f.name, &[]);
        p.version = 42;
        let s = seal(b"x", &p).unwrap();
        assert_eq!(open(&s.blob, &s.dek, None).unwrap().meta.version, 42);
    }

    #[test]
    fn garbage_is_not_sealed() {
        let dek = DataKey::generate();
        assert_eq!(open(b"hello", &dek, None).unwrap_err(), SealError::NotSealed);
        assert_eq!(open(&vec![0u8; HEADER_LEN + 100], &dek, None).unwrap_err(), SealError::NotSealed);
    }
}
