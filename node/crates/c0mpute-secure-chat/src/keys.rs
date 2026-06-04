//! Keypair generation, password-encrypted backups, and key loading.
//!
//! A single 32-byte seed is HKDF-expanded into two keypairs:
//!   - X25519 (crypto_box):  NaCl crypto_box DM encryption
//!   - Ed25519:              message envelope signing
//!
//! The seed is encrypted with Argon2id → AES-256-GCM and stored in
//! ~/.config/c0mpute/chat.key (chmod 600). The JSON blob is also the
//! user-facing backup — no plaintext secret ever touches disk after keygen.

use std::fs;
use std::path::{Path, PathBuf};

use aes_gcm::{
    Aes256Gcm, Key, Nonce,
    aead::{Aead, AeadCore, KeyInit},
};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::prelude::*;
use crypto_box::{PublicKey as EncPublicKey, SecretKey as EncSecretKey};
use ed25519_dalek::{SigningKey, VerifyingKey};
use hkdf::Hkdf;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
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
    /// Base64url-encoded random salt (32 bytes).
    pub salt: String,
    /// Base64url-encoded AES-GCM nonce (12 bytes).
    pub enc_nonce: String,
    /// Base64url-encoded AES-GCM ciphertext of the 32-byte seed.
    pub ciphertext: String,
    /// Base64url-encoded X25519 public key (32 bytes). Not secret.
    pub pubkey_enc: String,
    /// Base64url-encoded Ed25519 verifying key (32 bytes). Not secret.
    pub pubkey_sig: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub did: Option<String>,
    pub created_at: u64,
}

/// In-memory key material. Seed is zeroized on drop.
pub struct ChatKey {
    pub(crate) seed: Zeroizing<[u8; 32]>,
    pub enc_secret: EncSecretKey,
    pub enc_public: EncPublicKey,
    pub sig_key: SigningKey,
    pub sig_public: VerifyingKey,
}

impl ChatKey {
    pub fn generate() -> Self {
        let mut seed = Zeroizing::new([0u8; 32]);
        rand::RngCore::fill_bytes(&mut OsRng, seed.as_mut());
        Self::from_seed(*seed)
    }

    pub fn from_seed(seed: [u8; 32]) -> Self {
        let hk = Hkdf::<Sha256>::new(None, &seed);

        let mut enc_bytes = Zeroizing::new([0u8; 32]);
        hk.expand(b"secure-chat-enc-v1", enc_bytes.as_mut()).unwrap();
        let enc_secret = EncSecretKey::from(*enc_bytes);
        let enc_public = enc_secret.public_key();

        let mut sig_bytes = Zeroizing::new([0u8; 32]);
        hk.expand(b"secure-chat-sig-v1", sig_bytes.as_mut()).unwrap();
        let sig_key = SigningKey::from_bytes(&*sig_bytes);
        let sig_public = sig_key.verifying_key();

        Self {
            seed: Zeroizing::new(seed),
            enc_secret,
            enc_public,
            sig_key,
            sig_public,
        }
    }

    /// Hex fingerprint of the X25519 public key — 8 colon-separated byte pairs.
    pub fn fingerprint(&self) -> String {
        self.enc_public.as_bytes()[..8]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":")
    }

    /// Produce the password-encrypted `KeyFile` (backup blob).
    pub fn encrypt_to_keyfile(
        &self,
        password: &str,
        did: Option<&str>,
    ) -> anyhow::Result<KeyFile> {
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

        Ok(KeyFile {
            v: 1,
            alg: "argon2id-aes256gcm-v1".into(),
            argon2_mem_kib: ARGON2_MEM_KIB,
            argon2_iters: ARGON2_ITERS,
            argon2_para: ARGON2_PARA,
            salt: BASE64_URL_SAFE_NO_PAD.encode(salt),
            enc_nonce: BASE64_URL_SAFE_NO_PAD.encode(nonce),
            ciphertext: BASE64_URL_SAFE_NO_PAD.encode(&ciphertext),
            pubkey_enc: BASE64_URL_SAFE_NO_PAD.encode(self.enc_public.as_bytes()),
            pubkey_sig: BASE64_URL_SAFE_NO_PAD.encode(self.sig_public.as_bytes()),
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
