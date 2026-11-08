//! Key management: hybrid classical + post-quantum keypairs, password-encrypted backups.
//!
//! A single 32-byte seed is HKDF-expanded into four keypairs:
//!   - X25519:      classical ECDH (classical half of hybrid KEM)
//!   - ML-KEM-768:  post-quantum KEM (NIST FIPS 203, quantum half of hybrid KEM)
//!   - Ed25519:     classical message signing
//!   - ML-DSA-65:   post-quantum signing (NIST FIPS 204)
//!
//! The seed is encrypted with Argon2id → AES-256-GCM and stored in
//! ~/.config/c0mpute/chat.key (chmod 600). The JSON blob is the user-facing
//! backup — no plaintext secret ever touches disk after keygen.

use std::fs;
use std::path::{Path, PathBuf};

use aes_gcm::{
    Aes256Gcm, Key, Nonce,
    aead::{Aead, AeadCore, KeyInit},
};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::prelude::*;
use ed25519_dalek::{SigningKey as Ed25519SigningKey, VerifyingKey as Ed25519VerifyingKey};
use hkdf::Hkdf;
use ml_dsa::{KeyExport as MlDsaKeyExport, Keypair as MlDsaKeypair, MlDsa65, SigningKey as MlDsaSigningKey, VerifyingKey as MlDsaVerifyingKey};
use ml_kem::{DecapsulationKey as MlKemDk, MlKem768};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use x25519_dalek::{PublicKey as X25519Public, StaticSecret as X25519Secret};
use zeroize::Zeroizing;

const ARGON2_MEM_KIB: u32 = 65_536; // 64 MiB
const ARGON2_ITERS: u32 = 3;
const ARGON2_PARA: u32 = 4;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyFile {
    pub v: u8,
    pub alg: String,
    pub argon2_mem_kib: u32,
    pub argon2_iters: u32,
    pub argon2_para: u32,
    /// Base64url random salt (32 bytes).
    pub salt: String,
    /// Base64url AES-GCM nonce (12 bytes).
    pub enc_nonce: String,
    /// Base64url AES-GCM ciphertext of the 32-byte seed.
    pub ciphertext: String,
    /// Base64url X25519 public key (32 bytes) — classical KEM half.
    pub pubkey_enc: String,
    /// Base64url ML-KEM-768 encapsulation key (1184 bytes) — PQ KEM half.
    pub pubkey_kem: String,
    /// Base64url Ed25519 verifying key (32 bytes) — classical signing.
    pub pubkey_sig: String,
    /// Base64url ML-DSA-65 verifying key (1952 bytes) — PQ signing.
    pub pubkey_ml_dsa: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub did: Option<String>,
    pub created_at: u64,
}

/// In-memory key material. Seed is zeroized on drop.
pub struct ChatKey {
    pub(crate) seed: Zeroizing<[u8; 32]>,
    // Classical
    pub x25519_secret: X25519Secret,
    pub x25519_public: X25519Public,
    pub ed25519_key: Ed25519SigningKey,
    pub ed25519_public: Ed25519VerifyingKey,
    // Post-quantum (NIST FIPS 203 + 204)
    pub ml_kem_dk: MlKemDk<MlKem768>,
    pub ml_dsa_key: MlDsaSigningKey<MlDsa65>,
    pub ml_dsa_public: MlDsaVerifyingKey<MlDsa65>,
}

impl ChatKey {
    pub fn generate() -> Self {
        let mut seed = Zeroizing::new([0u8; 32]);
        rand::RngCore::fill_bytes(&mut OsRng, seed.as_mut());
        Self::from_seed(*seed)
    }

    pub fn from_seed(seed: [u8; 32]) -> Self {
        let hk = Hkdf::<Sha256>::new(None, &seed);

        // X25519 (classical ECDH)
        let mut x25519_bytes = Zeroizing::new([0u8; 32]);
        hk.expand(b"secure-chat-x25519-v2", x25519_bytes.as_mut()).unwrap();
        let x25519_secret = X25519Secret::from(*x25519_bytes);
        let x25519_public = X25519Public::from(&x25519_secret);

        // Ed25519 (classical signing)
        let mut ed25519_bytes = Zeroizing::new([0u8; 32]);
        hk.expand(b"secure-chat-ed25519-v2", ed25519_bytes.as_mut()).unwrap();
        let ed25519_key = Ed25519SigningKey::from_bytes(&*ed25519_bytes);
        let ed25519_public = ed25519_key.verifying_key();

        // ML-KEM-768 (PQ KEM, FIPS 203): 64-byte seed = d || z
        let mut ml_kem_seed_bytes = Zeroizing::new([0u8; 64]);
        hk.expand(b"secure-chat-ml-kem-768-v2", ml_kem_seed_bytes.as_mut()).unwrap();
        let ml_kem_seed: ml_kem::Seed = (*ml_kem_seed_bytes).into();
        let ml_kem_dk = MlKemDk::<MlKem768>::from_seed(ml_kem_seed);

        // ML-DSA-65 (PQ signing, FIPS 204): 32-byte seed
        let mut ml_dsa_seed_bytes = Zeroizing::new([0u8; 32]);
        hk.expand(b"secure-chat-ml-dsa-65-v2", ml_dsa_seed_bytes.as_mut()).unwrap();
        let ml_dsa_seed: ml_dsa::Seed = (*ml_dsa_seed_bytes).into();
        let ml_dsa_key = MlDsaSigningKey::<MlDsa65>::from_seed(&ml_dsa_seed);
        let ml_dsa_public = ml_dsa_key.verifying_key();

        Self {
            seed: Zeroizing::new(seed),
            x25519_secret,
            x25519_public,
            ed25519_key,
            ed25519_public,
            ml_kem_dk,
            ml_dsa_key,
            ml_dsa_public,
        }
    }

    /// Borrow the ML-KEM-768 encapsulation (public) key.
    pub fn ml_kem_ek(&self) -> &ml_kem::EncapsulationKey<MlKem768> {
        self.ml_kem_dk.encapsulation_key()
    }

    /// Hex fingerprint of the X25519 public key — 8 colon-separated byte pairs.
    pub fn fingerprint(&self) -> String {
        self.x25519_public.as_bytes()[..8]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":")
    }

    /// Produce the password-encrypted `KeyFile` (backup blob).
    pub fn encrypt_to_keyfile(&self, password: &str, did: Option<&str>) -> anyhow::Result<KeyFile> {
        let mut salt = [0u8; 32];
        rand::RngCore::fill_bytes(&mut OsRng, &mut salt);

        let mut kek = Zeroizing::new([0u8; 32]);
        let params = Params::new(ARGON2_MEM_KIB, ARGON2_ITERS, ARGON2_PARA, Some(32))
            .map_err(|e| anyhow::anyhow!("argon2 params: {e}"))?;
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
            .hash_password_into(password.as_bytes(), &salt, kek.as_mut())
            .map_err(|e| anyhow::anyhow!("argon2: {e}"))?;

        let key = Key::<Aes256Gcm>::from_slice(kek.as_ref());
        let cipher = Aes256Gcm::new(key);
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ciphertext = cipher
            .encrypt(&nonce, self.seed.as_ref())
            .map_err(|e| anyhow::anyhow!("aes-gcm encrypt: {e}"))?;

        let kem_ek_bytes = self.ml_kem_dk.encapsulation_key().to_bytes();
        let ml_dsa_vk_bytes = self.ml_dsa_public.to_bytes();

        Ok(KeyFile {
            v: 2,
            alg: "argon2id-aes256gcm-hybrid-pqc-v2".into(),
            argon2_mem_kib: ARGON2_MEM_KIB,
            argon2_iters: ARGON2_ITERS,
            argon2_para: ARGON2_PARA,
            salt: BASE64_URL_SAFE_NO_PAD.encode(salt),
            enc_nonce: BASE64_URL_SAFE_NO_PAD.encode(nonce),
            ciphertext: BASE64_URL_SAFE_NO_PAD.encode(&ciphertext),
            pubkey_enc: BASE64_URL_SAFE_NO_PAD.encode(self.x25519_public.as_bytes()),
            pubkey_kem: BASE64_URL_SAFE_NO_PAD.encode(&kem_ek_bytes[..]),
            pubkey_sig: BASE64_URL_SAFE_NO_PAD.encode(self.ed25519_public.as_bytes()),
            pubkey_ml_dsa: BASE64_URL_SAFE_NO_PAD.encode(&ml_dsa_vk_bytes[..]),
            did: did.map(str::to_string),
            created_at: unix_now(),
        })
    }
}

/// Decrypt a `KeyFile` with the user's password and return the in-memory key.
pub fn decrypt_keyfile(file: &KeyFile, password: &str) -> anyhow::Result<ChatKey> {
    let salt = BASE64_URL_SAFE_NO_PAD.decode(&file.salt)?;
    let nonce_bytes = BASE64_URL_SAFE_NO_PAD.decode(&file.enc_nonce)?;
    let ciphertext = BASE64_URL_SAFE_NO_PAD.decode(&file.ciphertext)?;

    let mut kek = Zeroizing::new([0u8; 32]);
    let params =
        Params::new(file.argon2_mem_kib, file.argon2_iters, file.argon2_para, Some(32))
            .map_err(|e| anyhow::anyhow!("argon2 params: {e}"))?;
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(password.as_bytes(), &salt, kek.as_mut())
        .map_err(|e| anyhow::anyhow!("argon2: {e}"))?;

    let key = Key::<Aes256Gcm>::from_slice(kek.as_ref());
    let cipher = Aes256Gcm::new(key);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let seed_bytes = cipher
        .decrypt(nonce, ciphertext.as_ref())
        .map_err(|_| anyhow::anyhow!("decryption failed — wrong password?"))?;

    if seed_bytes.len() != 32 {
        anyhow::bail!("unexpected seed length: {}", seed_bytes.len());
    }
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&seed_bytes);
    Ok(ChatKey::from_seed(seed))
}

pub fn key_file_path() -> anyhow::Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("com", "c0mpute", "c0mpute")
        .ok_or_else(|| anyhow::anyhow!("cannot determine config dir"))?;
    Ok(dirs.config_dir().join("chat.key"))
}

pub fn load_keyfile(path: &Path) -> anyhow::Result<KeyFile> {
    let raw = fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
    Ok(serde_json::from_str(&raw)?)
}

pub fn save_keyfile(path: &Path, kf: &KeyFile) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(kf)?;
    write_secret_file(path, json.as_bytes())
}

/// Write `content` to `path` with 0o600 permissions (unix) or plain write (other).
fn write_secret_file(path: &Path, content: &[u8]) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(content)?;
    }
    #[cfg(not(unix))]
    {
        fs::write(path, content)?;
    }
    Ok(())
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
