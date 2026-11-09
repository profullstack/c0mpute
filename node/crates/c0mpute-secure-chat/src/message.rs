//! DM envelope: hybrid classical+PQ encrypt, decrypt, sign, verify.
//!
//! Encryption: X25519 ECDH + ML-KEM-768 encapsulation → HKDF-combined shared
//! secret → AES-256-GCM. Either algorithm alone is sufficient to break
//! confidentiality; both must be broken simultaneously for the message to be
//! read by an attacker.
//!
//! Signing: Ed25519 + ML-DSA-65 (FIPS 204). Both signatures must verify.
//!
//! Envelope v3 binds every field: the id is a hash over the header *and* the
//! hashes of both ciphertexts, the signatures cover the id, and verification
//! recomputes the id from the fields, so changing any byte (sender, recipient,
//! timestamp, TTL, nonce or ciphertext) fails verification. The header is also
//! the AES-GCM associated data.

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
    /// SHA-256 over the header and both ciphertext hashes (hex); see [`envelope_id`].
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
    /// Seconds a mailbox should keep this for an offline recipient.
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
    encrypt_dm_at(
        from_did,
        to_did,
        text,
        sender_x25519_secret,
        recipient_x25519_public,
        recipient_ml_kem_ek,
        sender_ed25519_key,
        sender_ml_dsa_key,
        ttl,
        unix_now(),
    )
}

/// Current envelope version.
pub const ENVELOPE_VERSION: u8 = 3;

fn header_aad(from: &str, to: &str, created_at: u64, ttl: u64) -> Vec<u8> {
    format!("c0mpute-dm-v3|{from}|{to}|{created_at}|{ttl}").into_bytes()
}

/// The id an envelope must carry: SHA-256 over its header and the SHA-256 of
/// each ciphertext field.
pub fn envelope_id(e: &DmEnvelope) -> String {
    let ct = hex::encode(Sha256::digest(e.ciphertext.as_bytes()));
    let kem = hex::encode(Sha256::digest(e.ml_kem_ct.as_bytes()));
    let canonical = format!(
        "c0mpute-dm-id-v3|{}|{}|{}|{}|{}|{}|{}|{}",
        e.kind, e.from, e.to, e.created_at, e.ttl, e.enc_nonce, kem, ct
    );
    hex::encode(Sha256::digest(canonical.as_bytes()))
}

/// [`encrypt_dm`] with an explicit creation time (seconds since the epoch).
#[allow(clippy::too_many_arguments)]
pub fn encrypt_dm_at(
    from_did: &str,
    to_did: &str,
    text: &str,
    sender_x25519_secret: &X25519Secret,
    recipient_x25519_public: &X25519Public,
    recipient_ml_kem_ek: &EncapsulationKey<MlKem768>,
    sender_ed25519_key: &SigningKey,
    sender_ml_dsa_key: &MlDsaSigningKey<MlDsa65>,
    ttl: u64,
    created_at: u64,
) -> anyhow::Result<DmEnvelope> {
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
    let aad = header_aad(from_did, to_did, created_at, ttl);
    let ciphertext = cipher
        .encrypt(
            &nonce,
            aes_gcm::aead::Payload {
                msg: plaintext_bytes.as_ref(),
                aad: &aad,
            },
        )
        .map_err(|e| anyhow::anyhow!("aes-gcm encrypt: {e}"))?;

    let ml_kem_ct_b64 = BASE64_URL_SAFE_NO_PAD.encode(&ml_kem_ct[..]);
    let enc_nonce_b64 = BASE64_URL_SAFE_NO_PAD.encode(nonce.as_slice());
    let ciphertext_b64 = BASE64_URL_SAFE_NO_PAD.encode(&ciphertext);

    let mut env = DmEnvelope {
        v: ENVELOPE_VERSION,
        id: String::new(),
        kind: "dm".into(),
        from: from_did.to_string(),
        to: to_did.to_string(),
        ciphertext: ciphertext_b64,
        enc_nonce: enc_nonce_b64,
        ml_kem_ct: ml_kem_ct_b64,
        created_at,
        ttl,
        sig: String::new(),
        ml_dsa_sig: String::new(),
    };
    // Message ID (replay protection + what the signatures cover).
    let id = envelope_id(&env);

    // Ed25519 signature (classical)
    let ed_sig: Ed25519Sig = Ed25519Signer::sign(sender_ed25519_key, id.as_bytes());
    let sig_b64 = BASE64_URL_SAFE_NO_PAD.encode(ed_sig.to_bytes());

    // ML-DSA-65 signature (post-quantum, FIPS 204)
    let ml_dsa_sig: MlDsaSig<MlDsa65> =
        MlDsaSigner::try_sign(sender_ml_dsa_key, id.as_bytes())
            .map_err(|e| anyhow::anyhow!("ml-dsa sign: {e}"))?;
    let ml_dsa_sig_encoded = ml_dsa_sig.encode();
    let ml_dsa_sig_b64 = BASE64_URL_SAFE_NO_PAD.encode(&ml_dsa_sig_encoded[..]);

    env.id = id;
    env.sig = sig_b64;
    env.ml_dsa_sig = ml_dsa_sig_b64;
    Ok(env)
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
    anyhow::ensure!(envelope.v == ENVELOPE_VERSION, "unsupported envelope version {}", envelope.v);

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
    let aad = header_aad(&envelope.from, &envelope.to, envelope.created_at, envelope.ttl);
    let plaintext_bytes = cipher
        .decrypt(
            nonce,
            aes_gcm::aead::Payload {
                msg: ciphertext.as_ref(),
                aad: &aad,
            },
        )
        .map_err(|_| anyhow::anyhow!("decryption failed — wrong key or corrupted message"))?;

    Ok(serde_json::from_slice(&plaintext_bytes)?)
}

/// Verify both the Ed25519 and ML-DSA-65 signatures on a `DmEnvelope`, after
/// recomputing its id from the fields. Both must pass for the message to be
/// considered authentic.
pub fn verify_envelope_sig(
    envelope: &DmEnvelope,
    sender_ed25519_public: &VerifyingKey,
    sender_ml_dsa_public: &MlDsaVerifyingKey<MlDsa65>,
) -> anyhow::Result<()> {
    anyhow::ensure!(envelope.v == ENVELOPE_VERSION, "unsupported envelope version {}", envelope.v);
    anyhow::ensure!(envelope_id(envelope) == envelope.id, "envelope id does not match its fields (tampered)");
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

    #[test]
    fn tampered_fields_fail_verification() {
        let alice = ChatKey::generate();
        let bob = ChatKey::generate();
        let env = encrypt_dm(
            "alice", "bob", "pay 5", &alice.x25519_secret, &bob.x25519_public, bob.ml_kem_ek(),
            &alice.ed25519_key, &alice.ml_dsa_key, 3600,
        )
        .unwrap();
        let ok = |e: &DmEnvelope| verify_envelope_sig(e, &alice.ed25519_public, &alice.ml_dsa_public).is_ok();
        assert!(ok(&env));
        let mut t = env.clone();
        t.created_at += 1;
        assert!(!ok(&t));
        let mut t = env.clone();
        t.to = "eve".into();
        assert!(!ok(&t));
        let mut t = env.clone();
        t.ttl = 1;
        assert!(!ok(&t));
        // Flip one ciphertext character.
        let mut t = env.clone();
        let mut c: Vec<char> = t.ciphertext.chars().collect();
        c[3] = if c[3] == 'A' { 'B' } else { 'A' };
        t.ciphertext = c.into_iter().collect();
        assert!(!ok(&t));
        // Even with the id recomputed, the signatures no longer match.
        t.id = envelope_id(&t);
        assert!(!ok(&t));
        // And the AEAD rejects the header change on decrypt.
        let mut t = env.clone();
        t.created_at += 1;
        assert!(decrypt_dm(&t, &bob.x25519_secret, &alice.x25519_public, &bob.ml_kem_dk).is_err());
    }
}
