//! DM envelope: encrypt, decrypt, sign, verify.
//!
//! Wire format: a signed JSON envelope wrapping NaCl crypto_box ciphertext.
//! The plaintext is a JSON object; outer fields (from/to DID, timestamps) are
//! visible to relays but the content is opaque.

use base64::prelude::*;
use crypto_box::{
    PublicKey as EncPublicKey, SecretKey as EncSecretKey, SalsaBox,
    aead::{Aead, AeadCore},
};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DmEnvelope {
    pub v: u8,
    /// SHA-256 of `from|to|created_at|enc_nonce` (hex).
    pub id: String,
    pub kind: String,
    pub from: String,
    pub to: String,
    /// Base64url NaCl SalsaBox ciphertext.
    pub ciphertext: String,
    /// Base64url 24-byte XSalsa20 nonce.
    pub enc_nonce: String,
    pub created_at: u64,
    /// Seconds the relay should keep this for an offline recipient (max 604800).
    pub ttl: u64,
    /// Base64url Ed25519 signature over `id` bytes (hex-string as bytes).
    pub sig: String,
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
    sender_enc_secret: &EncSecretKey,
    recipient_enc_public: &EncPublicKey,
    sender_sig_key: &SigningKey,
    ttl: u64,
) -> anyhow::Result<DmEnvelope> {
    let created_at = unix_now();

    let plaintext_bytes = serde_json::to_vec(&DmPlaintext {
        text: text.to_string(),
        thread_id: None,
        reply_to: None,
    })?;

    let salsa = SalsaBox::new(recipient_enc_public, sender_enc_secret);
    let nonce = SalsaBox::generate_nonce(&mut OsRng);
    let ciphertext = salsa
        .encrypt(&nonce, plaintext_bytes.as_ref())
        .map_err(|e| anyhow::anyhow!("encrypt: {e}"))?;

    let enc_nonce_b64 = BASE64_URL_SAFE_NO_PAD.encode(nonce.as_slice());
    let ciphertext_b64 = BASE64_URL_SAFE_NO_PAD.encode(&ciphertext);

    let canonical = format!("{from_did}|{to_did}|{created_at}|{enc_nonce_b64}");
    let id = hex::encode(Sha256::digest(canonical.as_bytes()));

    let sig: Signature = sender_sig_key.sign(id.as_bytes());
    let sig_b64 = BASE64_URL_SAFE_NO_PAD.encode(sig.to_bytes());

    Ok(DmEnvelope {
        v: 1,
        id,
        kind: "dm".into(),
        from: from_did.to_string(),
        to: to_did.to_string(),
        ciphertext: ciphertext_b64,
        enc_nonce: enc_nonce_b64,
        created_at,
        ttl,
        sig: sig_b64,
    })
}

pub fn decrypt_dm(
    envelope: &DmEnvelope,
    recipient_enc_secret: &EncSecretKey,
    sender_enc_public: &EncPublicKey,
) -> anyhow::Result<DmPlaintext> {
    let ciphertext = BASE64_URL_SAFE_NO_PAD.decode(&envelope.ciphertext)?;
    let nonce_bytes = BASE64_URL_SAFE_NO_PAD.decode(&envelope.enc_nonce)?;

    anyhow::ensure!(nonce_bytes.len() == 24, "invalid nonce length");
    let nonce = crypto_box::Nonce::from_slice(&nonce_bytes);

    let salsa = SalsaBox::new(sender_enc_public, recipient_enc_secret);
    let plaintext_bytes = salsa
        .decrypt(nonce, ciphertext.as_ref())
        .map_err(|_| anyhow::anyhow!("decryption failed — wrong key or corrupted message"))?;

    Ok(serde_json::from_slice(&plaintext_bytes)?)
}

pub fn verify_envelope_sig(
    envelope: &DmEnvelope,
    sender_sig_public: &VerifyingKey,
) -> anyhow::Result<()> {
    let sig_bytes = BASE64_URL_SAFE_NO_PAD.decode(&envelope.sig)?;
    anyhow::ensure!(sig_bytes.len() == 64, "invalid signature length");
    let mut arr = [0u8; 64];
    arr.copy_from_slice(&sig_bytes);
    let sig = Signature::from_bytes(&arr);
    sender_sig_public
        .verify(envelope.id.as_bytes(), &sig)
        .map_err(|e| anyhow::anyhow!("signature verification failed: {e}"))
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
            &alice.enc_secret,
            &bob.enc_public,
            &alice.sig_key,
            86400,
        )
        .unwrap();

        verify_envelope_sig(&envelope, &alice.sig_public).unwrap();

        let plain = decrypt_dm(&envelope, &bob.enc_secret, &alice.enc_public).unwrap();
        assert_eq!(plain.text, "hello from alice");
    }

    #[test]
    fn wrong_key_fails() {
        let alice = ChatKey::generate();
        let bob = ChatKey::generate();
        let eve = ChatKey::generate();

        let envelope = encrypt_dm(
            "did:coinpay:user:alice",
            "did:coinpay:user:bob",
            "secret",
            &alice.enc_secret,
            &bob.enc_public,
            &alice.sig_key,
            86400,
        )
        .unwrap();

        // Eve can't decrypt what was encrypted for Bob.
        let result = decrypt_dm(&envelope, &eve.enc_secret, &alice.enc_public);
        assert!(result.is_err());
    }
}
