//! DM envelope: hybrid classical+PQ encrypt, decrypt, sign, verify.
//!
//! Encryption: X25519 ECDH + ML-KEM-768 encapsulation → HKDF-combined shared
//! secret → AES-256-GCM. Either algorithm alone is sufficient to break
//! confidentiality; both must be broken simultaneously for the message to be
//! read by an attacker.
//!
//! Signing: Ed25519 + ML-DSA-65 (FIPS 204). Both signatures must verify.

use aes_gcm::{
    Aes256Gcm, Key, Nonce,
    aead::{Aead, AeadCore, KeyInit},
};
use base64::prelude::*;
use ed25519_dalek::{
    Signature as Ed25519Sig, Signer as Ed25519Signer, SigningKey, Verifier as Ed25519Verifier,
    VerifyingKey,
};
use hkdf::Hkdf;
use ml_dsa::{
    MlDsa65, Signature as MlDsaSig, SigningKey as MlDsaSigningKey, Signer as MlDsaSigner,
    VerifyingKey as MlDsaVerifyingKey, Verifier as MlDsaVerifier,
};
use ml_kem::{Decapsulate, Encapsulate, EncapsulationKey, DecapsulationKey, MlKem768};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey as X25519Public, StaticSecret as X25519Secret};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DmEnvelope {
    pub v: u8,
    /// SHA-256 of `from|to|created_at|enc_nonce` (hex).
    pub id: String,
    pub kind: String,
    pub from: String,
    pub to: String,
    /// Base64url AES-256-GCM ciphertext.
    pub ciphertext: String,
    /// Base64url AES-256-GCM nonce (12 bytes).
    pub enc_nonce: String,
    /// Base64url ML-KEM-768 ciphertext (1088 bytes) for PQ shared-secret half.
    pub ml_kem_ct: String,
    pub created_at: u64,
    /// Seconds the relay should keep this for an offline recipient (max 604800).
    pub ttl: u64,
    /// Base64url Ed25519 signature (64 bytes) over `id`.
    pub sig: String,
    /// Base64url ML-DSA-65 signature (3309 bytes) over `id`.
    pub ml_dsa_sig: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DmPlaintext {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
}

pub fn encrypt_dm(
    from_did: &str,
    to_did: &str,
    text: &str,
    sender_x25519_secret: &X25519Secret,
    recipient_x25519_public: &X25519Public,
    recipient_ml_kem_ek: &EncapsulationKey<MlKem768>,
    sender_ed25519_key: &SigningKey,
    sender_ml_dsa_key: &MlDsaSigningKey<MlDsa65>,
    ttl: u64,
) -> anyhow::Result<DmEnvelope> {
    let created_at = unix_now();

    let plaintext_bytes = serde_json::to_vec(&DmPlaintext {
        text: text.to_string(),
        thread_id: None,
        reply_to: None,
    })?;

    // Hybrid KEM: X25519 ECDH (classical) + ML-KEM-768 (post-quantum)
    let ecdh_ss = sender_x25519_secret.diffie_hellman(recipient_x25519_public);
    let (ml_kem_ct, ml_kem_ss) = recipient_ml_kem_ek.encapsulate();

    // Combine both shared secrets via HKDF: attacker needs both to break encryption
    let mut combined = [0u8; 64];
    combined[..32].copy_from_slice(ecdh_ss.as_bytes());
    combined[32..].copy_from_slice(ml_kem_ss.as_ref());
    let hk = Hkdf::<Sha256>::new(None, &combined);
    let mut sym_key = [0u8; 32];
    hk.expand(b"hybrid-dm-key-v2", &mut sym_key).unwrap();

    // AES-256-GCM encryption
    let aes_key = Key::<Aes256Gcm>::from_slice(&sym_key);
    let cipher = Aes256Gcm::new(aes_key);
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(&nonce, plaintext_bytes.as_ref())
        .map_err(|e| anyhow::anyhow!("aes-gcm encrypt: {e}"))?;

    let ml_kem_ct_b64 = BASE64_URL_SAFE_NO_PAD.encode(&ml_kem_ct[..]);
    let enc_nonce_b64 = BASE64_URL_SAFE_NO_PAD.encode(nonce.as_slice());
    let ciphertext_b64 = BASE64_URL_SAFE_NO_PAD.encode(&ciphertext);

    // Message ID (replay-resistance)
    let canonical = format!("{from_did}|{to_did}|{created_at}|{enc_nonce_b64}");
    let id = hex::encode(Sha256::digest(canonical.as_bytes()));

    // Ed25519 signature (classical)
    let ed_sig: Ed25519Sig = Ed25519Signer::sign(sender_ed25519_key, id.as_bytes());
    let sig_b64 = BASE64_URL_SAFE_NO_PAD.encode(ed_sig.to_bytes());

    // ML-DSA-65 signature (post-quantum, FIPS 204)
    let ml_dsa_sig: MlDsaSig<MlDsa65> =
        MlDsaSigner::try_sign(sender_ml_dsa_key, id.as_bytes())
            .map_err(|e| anyhow::anyhow!("ml-dsa sign: {e}"))?;
    let ml_dsa_sig_encoded = ml_dsa_sig.encode();
    let ml_dsa_sig_b64 = BASE64_URL_SAFE_NO_PAD.encode(&ml_dsa_sig_encoded[..]);

    Ok(DmEnvelope {
        v: 2,
        id,
        kind: "dm".into(),
        from: from_did.to_string(),
        to: to_did.to_string(),
        ciphertext: ciphertext_b64,
        enc_nonce: enc_nonce_b64,
        ml_kem_ct: ml_kem_ct_b64,
        created_at,
        ttl,
        sig: sig_b64,
        ml_dsa_sig: ml_dsa_sig_b64,
    })
}

pub fn decrypt_dm(
    envelope: &DmEnvelope,
    recipient_x25519_secret: &X25519Secret,
    sender_x25519_public: &X25519Public,
    recipient_ml_kem_dk: &DecapsulationKey<MlKem768>,
) -> anyhow::Result<DmPlaintext> {
    let ciphertext = BASE64_URL_SAFE_NO_PAD.decode(&envelope.ciphertext)?;
    let nonce_bytes = BASE64_URL_SAFE_NO_PAD.decode(&envelope.enc_nonce)?;
    let ml_kem_ct_bytes = BASE64_URL_SAFE_NO_PAD.decode(&envelope.ml_kem_ct)?;

    anyhow::ensure!(nonce_bytes.len() == 12, "invalid AES-GCM nonce length");

    // Hybrid KEM decapsulation
    let ecdh_ss = recipient_x25519_secret.diffie_hellman(sender_x25519_public);
    let ml_kem_ss = recipient_ml_kem_dk
        .decapsulate_slice(&ml_kem_ct_bytes)
        .map_err(|_| anyhow::anyhow!("invalid ml-kem ciphertext length (expected 1088 bytes)"))?;

    // Reconstruct the same combined symmetric key
    let mut combined = [0u8; 64];
    combined[..32].copy_from_slice(ecdh_ss.as_bytes());
    combined[32..].copy_from_slice(ml_kem_ss.as_ref());
    let hk = Hkdf::<Sha256>::new(None, &combined);
    let mut sym_key = [0u8; 32];
    hk.expand(b"hybrid-dm-key-v2", &mut sym_key).unwrap();

    // AES-256-GCM decryption
    let aes_key = Key::<Aes256Gcm>::from_slice(&sym_key);
    let cipher = Aes256Gcm::new(aes_key);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let plaintext_bytes = cipher
        .decrypt(nonce, ciphertext.as_ref())
        .map_err(|_| anyhow::anyhow!("decryption failed — wrong key or corrupted message"))?;

    Ok(serde_json::from_slice(&plaintext_bytes)?)
}

/// Verify both the Ed25519 and ML-DSA-65 signatures on a `DmEnvelope`.
/// Both must pass for the message to be considered authentic.
pub fn verify_envelope_sig(
    envelope: &DmEnvelope,
    sender_ed25519_public: &VerifyingKey,
    sender_ml_dsa_public: &MlDsaVerifyingKey<MlDsa65>,
) -> anyhow::Result<()> {
    // Ed25519 signature (classical)
    let sig_bytes = BASE64_URL_SAFE_NO_PAD.decode(&envelope.sig)?;
    anyhow::ensure!(sig_bytes.len() == 64, "invalid ed25519 signature length");
    let mut arr = [0u8; 64];
    arr.copy_from_slice(&sig_bytes);
    let ed_sig = Ed25519Sig::from_bytes(&arr);
    sender_ed25519_public
        .verify(envelope.id.as_bytes(), &ed_sig)
        .map_err(|e| anyhow::anyhow!("ed25519 signature verification failed: {e}"))?;

    // ML-DSA-65 signature (post-quantum)
    let ml_dsa_sig_bytes = BASE64_URL_SAFE_NO_PAD.decode(&envelope.ml_dsa_sig)?;
    let ml_dsa_sig_arr: ml_dsa::EncodedSignature<MlDsa65> = ml_dsa_sig_bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid ml-dsa signature length"))?;
    let ml_dsa_sig = ml_dsa::Signature::<MlDsa65>::decode(&ml_dsa_sig_arr)
        .ok_or_else(|| anyhow::anyhow!("malformed ml-dsa signature"))?;
    MlDsaVerifier::verify(sender_ml_dsa_public, envelope.id.as_bytes(), &ml_dsa_sig)
        .map_err(|e| anyhow::anyhow!("ml-dsa signature verification failed: {e}"))?;

    Ok(())
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::ChatKey;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let alice = ChatKey::generate();
        let bob = ChatKey::generate();

        let envelope = encrypt_dm(
            "did:coinpay:user:alice",
            "did:coinpay:user:bob",
            "hello from alice",
            &alice.x25519_secret,
            &bob.x25519_public,
            bob.ml_kem_ek(),
            &alice.ed25519_key,
            &alice.ml_dsa_key,
            86400,
        )
        .unwrap();

        verify_envelope_sig(&envelope, &alice.ed25519_public, &alice.ml_dsa_public).unwrap();

        let plain = decrypt_dm(
            &envelope,
            &bob.x25519_secret,
            &alice.x25519_public,
            &bob.ml_kem_dk,
        )
        .unwrap();
        assert_eq!(plain.text, "hello from alice");
    }

    #[test]
    fn wrong_x25519_key_fails() {
        let alice = ChatKey::generate();
        let bob = ChatKey::generate();
        let eve = ChatKey::generate();

        let envelope = encrypt_dm(
            "did:coinpay:user:alice",
            "did:coinpay:user:bob",
            "secret",
            &alice.x25519_secret,
            &bob.x25519_public,
            bob.ml_kem_ek(),
            &alice.ed25519_key,
            &alice.ml_dsa_key,
            86400,
        )
        .unwrap();

        // Eve uses wrong X25519 key — ECDH diverges, symmetric key differs.
        let result = decrypt_dm(&envelope, &eve.x25519_secret, &alice.x25519_public, &bob.ml_kem_dk);
        assert!(result.is_err());
    }

    #[test]
    fn wrong_ml_kem_key_fails() {
        let alice = ChatKey::generate();
        let bob = ChatKey::generate();
        let eve = ChatKey::generate();

        let envelope = encrypt_dm(
            "did:coinpay:user:alice",
            "did:coinpay:user:bob",
            "secret",
            &alice.x25519_secret,
            &bob.x25519_public,
            bob.ml_kem_ek(),
            &alice.ed25519_key,
            &alice.ml_dsa_key,
            86400,
        )
        .unwrap();

        // Eve uses wrong ML-KEM key — decapsulation gives wrong shared secret.
        let result = decrypt_dm(&envelope, &bob.x25519_secret, &alice.x25519_public, &eve.ml_kem_dk);
        assert!(result.is_err());
    }
}
