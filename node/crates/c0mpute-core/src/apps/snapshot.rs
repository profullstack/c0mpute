//! App state snapshots and the sealed secrets that unlock them.
//!
//! A snapshot is a tar of the app's state directory, cut into 1 MiB pieces.
//! Each piece is encrypted with AES-256-GCM under a key derived from the app
//! key, with the nonce derived from the piece itself (keyed blake3). The same
//! plaintext piece always encrypts to the same chunk, so a new snapshot shares
//! every unchanged piece with the last one and a standby fetches only what
//! changed. The cost is that equal pieces are visibly equal; nothing else
//! about the plaintext leaks, and only holders of the app key can read it.
//!
//! The app key and the app's secret environment travel together as
//! [`AppSecrets`], sealed per holder (X25519 → HKDF → AES-GCM). The record
//! carries only the blake3 of the blob, so a holder knows it got the right one.

use std::collections::BTreeMap;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, Result, anyhow, bail};
use c0mpute_proto::Hash;
use c0mpute_store::ChunkStore;
use rand::RngCore;
use serde::{Deserialize, Serialize};

/// Plaintext bytes per snapshot piece.
pub const PIECE_BYTES: usize = 1024 * 1024;
/// Largest state an app may keep (the tar), so a snapshot stays something a
/// standby can pull in a minute or two.
pub const MAX_STATE_BYTES: usize = 1024 * 1024 * 1024;

/// What the owner hands every holder, sealed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppSecrets {
    /// 32 bytes, hex: encrypts the app's state snapshots.
    pub app_key: String,
    /// Secret environment variables, set on the container only.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl AppSecrets {
    pub fn generate(env: BTreeMap<String, String>) -> Self {
        let mut k = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut k);
        AppSecrets {
            app_key: hex::encode(k),
            env,
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    /// The `secrets_hash` a record names.
    pub fn hash(&self) -> String {
        Hash::of(&self.to_bytes()).to_hex()
    }

    pub fn key(&self) -> Result<[u8; 32]> {
        hex::decode(&self.app_key)?
            .try_into()
            .map_err(|_| anyhow!("app key must be 32 bytes"))
    }
}

/// The session id a sealed-secrets message is bound to.
pub fn seal_session(app_id: &str, secrets_hash: &str) -> String {
    format!("apps:{app_id}:{secrets_hash}")
}

/// A snapshot's table of contents (stored as a chunk; its hash is the
/// [`super::placement::SnapRef::manifest`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapManifest {
    pub v: u8,
    pub app_id: String,
    pub epoch: u64,
    pub n: u64,
    pub taken_at: u64,
    /// Plaintext tar bytes.
    pub size: u64,
    /// Chunk hashes (hex) of the encrypted pieces, in order.
    pub pieces: Vec<String>,
}

impl SnapManifest {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    pub fn from_bytes(b: &[u8]) -> Result<Self> {
        let m: SnapManifest = serde_json::from_slice(b).context("snapshot manifest")?;
        if m.v != 1
            || m.size as usize > MAX_STATE_BYTES
            || m.pieces.len() > MAX_STATE_BYTES / PIECE_BYTES + 1
        {
            bail!("snapshot manifest out of bounds");
        }
        for p in &m.pieces {
            Hash::from_hex(p).context("snapshot piece hash")?;
        }
        Ok(m)
    }
}

fn snap_key(app_key: &[u8; 32]) -> [u8; 32] {
    blake3::derive_key("c0mpute apps snapshot v1", app_key)
}

/// Encrypt one piece deterministically: nonce = keyed blake3 of the piece.
pub fn seal_piece(app_key: &[u8; 32], plain: &[u8]) -> Result<Vec<u8>> {
    let key = snap_key(app_key);
    let tag = blake3::keyed_hash(&key, plain);
    let nonce = &tag.as_bytes()[..12];
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| anyhow!("key"))?;
    let ct = cipher
        .encrypt(Nonce::from_slice(nonce), plain)
        .map_err(|_| anyhow!("encrypt"))?;
    let mut out = nonce.to_vec();
    out.extend(ct);
    Ok(out)
}

pub fn open_piece(app_key: &[u8; 32], sealed: &[u8]) -> Result<Vec<u8>> {
    if sealed.len() < 12 + 16 {
        bail!("snapshot piece too short");
    }
    let key = snap_key(app_key);
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| anyhow!("key"))?;
    let plain = cipher
        .decrypt(Nonce::from_slice(&sealed[..12]), &sealed[12..])
        .map_err(|_| anyhow!("snapshot piece does not open (wrong app key or tampered)"))?;
    // The nonce must be the one this plaintext implies, or the piece was not
    // made by a holder of this key.
    if blake3::keyed_hash(&key, &plain).as_bytes()[..12] != sealed[..12] {
        bail!("snapshot piece nonce mismatch");
    }
    Ok(plain)
}

/// Encrypt `tar` into the store; returns the manifest hash and manifest.
pub async fn store(
    store: &ChunkStore,
    app_key: &[u8; 32],
    app_id: &str,
    epoch: u64,
    n: u64,
    taken_at: u64,
    tar: &[u8],
) -> Result<(Hash, SnapManifest)> {
    if tar.len() > MAX_STATE_BYTES {
        bail!(
            "state is {} bytes, over the {} byte limit",
            tar.len(),
            MAX_STATE_BYTES
        );
    }
    let mut pieces = Vec::with_capacity(tar.len() / PIECE_BYTES + 1);
    for piece in tar.chunks(PIECE_BYTES) {
        let sealed = seal_piece(app_key, piece)?;
        pieces.push(store.put(&sealed).await?.to_hex());
    }
    let m = SnapManifest {
        v: 1,
        app_id: app_id.to_string(),
        epoch,
        n,
        taken_at,
        size: tar.len() as u64,
        pieces,
    };
    let h = store.put(&m.to_bytes()).await?;
    Ok((h, m))
}

/// Reassemble a snapshot whose chunks are all in the local store.
pub async fn load(
    store: &ChunkStore,
    app_key: &[u8; 32],
    manifest: &SnapManifest,
) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(manifest.size as usize);
    for p in &manifest.pieces {
        let h = Hash::from_hex(p)?;
        let sealed = store.get(&h).await?;
        out.extend(open_piece(app_key, &sealed)?);
    }
    if out.len() as u64 != manifest.size {
        bail!(
            "snapshot is {} bytes, manifest says {}",
            out.len(),
            manifest.size
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pieces_are_deterministic_and_bound_to_the_key() {
        let s = AppSecrets::generate(BTreeMap::new());
        let k = s.key().unwrap();
        let a = seal_piece(&k, b"hello state").unwrap();
        let b = seal_piece(&k, b"hello state").unwrap();
        assert_eq!(a, b, "same plaintext, same chunk: snapshots dedupe");
        assert_ne!(a, seal_piece(&k, b"hello statf").unwrap());
        assert_eq!(open_piece(&k, &a).unwrap(), b"hello state");
        let other = AppSecrets::generate(BTreeMap::new()).key().unwrap();
        assert!(open_piece(&other, &a).is_err());
        let mut t = a.clone();
        *t.last_mut().unwrap() ^= 1;
        assert!(open_piece(&k, &t).is_err());
    }

    #[tokio::test]
    async fn store_and_load_share_unchanged_pieces() {
        let dir = std::env::temp_dir().join(format!("c0mpute-snap-{}", rand::random::<u64>()));
        let st = ChunkStore::open(&dir).await.unwrap();
        let k = AppSecrets::generate(BTreeMap::new()).key().unwrap();
        let mut tar = vec![7u8; PIECE_BYTES * 2 + 100];
        let (h1, m1) = store(&st, &k, "app", 1, 1, 0, &tar).await.unwrap();
        tar[PIECE_BYTES * 2 + 5] = 9; // touch only the last piece
        let (h2, m2) = store(&st, &k, "app", 1, 2, 0, &tar).await.unwrap();
        assert_ne!(h1, h2);
        assert_eq!(
            m1.pieces[..2],
            m2.pieces[..2],
            "unchanged pieces are the same chunks"
        );
        assert_ne!(m1.pieces[2], m2.pieces[2]);
        let back = SnapManifest::from_bytes(&st.get(&h2).await.unwrap()).unwrap();
        assert_eq!(load(&st, &k, &back).await.unwrap(), tar);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn secrets_hash_changes_with_content() {
        let s = AppSecrets::generate(BTreeMap::from([("A".into(), "1".into())]));
        let mut t = s.clone();
        t.env.insert("A".into(), "2".into());
        assert_ne!(s.hash(), t.hash());
        assert_eq!(s.hash(), Hash::of(&s.to_bytes()).to_hex());
    }
}
