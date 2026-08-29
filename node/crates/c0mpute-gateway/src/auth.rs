//! Signed-request auth for storage writes (CIP-002, DIP-0007).
//!
//! Reads of public objects need no auth — knowing a blake3 hash is itself the
//! capability, and `private`-tier objects are ciphertext (CIP-011), so
//! confidentiality comes from the key rather than an ACL. Writes are
//! authenticated so a stranger cannot fill an operator's disk.
//!
//! The envelope is an ed25519 signature over a canonical string. Identity is
//! a CoinPay DID; this module verifies the signature against a keyring and
//! leaves DID *resolution* (fetching a DID's current key from CoinPay) to the
//! caller, which is what lets the same code serve tests, a local dev node, and
//! a production node with a live registry.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

/// Header carrying the signed-request envelope.
pub const AUTH_HEADER: &str = "x-coinpay-auth";

/// Requests older than this are rejected, to bound replay.
pub const MAX_CLOCK_SKEW_SECS: u64 = 300;

/// Domain separator, so a storage envelope can never be replayed against
/// another c0mpute surface that adopts the same scheme.
const SIGNING_DOMAIN: &str = "c0mpute-storage-v1";

/// The authenticated caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub did: String,
}

impl Identity {
    /// The identity used when authorization is disabled.
    pub fn anonymous() -> Self {
        Self {
            did: "anonymous".to_string(),
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AuthError {
    #[error("missing {AUTH_HEADER} header")]
    Missing,
    #[error("malformed auth envelope: {0}")]
    Malformed(String),
    #[error("unknown signer {0}")]
    UnknownSigner(String),
    #[error("signature does not verify")]
    BadSignature,
    #[error("request timestamp is {0}s outside the allowed skew")]
    Expired(u64),
}

/// The envelope, base64url-encoded into the header value.
#[derive(Debug, Serialize, Deserialize)]
pub struct Envelope {
    pub did: String,
    /// Unix seconds.
    pub ts: u64,
    /// base64url ed25519 signature over [`canonical_string`].
    pub sig: String,
}

/// The exact bytes a client signs.
///
/// Binding the method, path and body hash means a captured envelope cannot be
/// replayed against a different object or a different verb.
pub fn canonical_string(method: &str, path: &str, ts: u64, body_hash_hex: &str) -> String {
    format!("{SIGNING_DOMAIN}\n{method}\n{path}\n{ts}\n{body_hash_hex}")
}

/// What a request needs to present to be authorized.
pub struct AuthRequest<'a> {
    pub method: &'a str,
    pub path: &'a str,
    /// Hex blake3 of the body the client committed to. For storage writes this
    /// is the object hash in the URL, so it costs nothing to bind.
    pub body_hash_hex: &'a str,
    pub header: Option<&'a str>,
}

pub trait Authorizer: Send + Sync + std::fmt::Debug {
    fn authorize(&self, req: &AuthRequest<'_>) -> Result<Identity, AuthError>;
}

/// Accepts everything. For local development and single-operator nodes that
/// are not exposed to the network.
#[derive(Debug, Default, Clone, Copy)]
pub struct AllowAll;

impl Authorizer for AllowAll {
    fn authorize(&self, _req: &AuthRequest<'_>) -> Result<Identity, AuthError> {
        Ok(Identity::anonymous())
    }
}

/// Verifies ed25519 signed-request envelopes against a keyring of DIDs.
#[derive(Debug, Default)]
pub struct SignedEnvelope {
    keys: HashMap<String, VerifyingKey>,
    /// Overridable for tests; `None` means "read the system clock".
    now_override: Option<u64>,
}

impl SignedEnvelope {
    pub fn new() -> Self {
        Self::default()
    }

    /// Trust `did` when it signs with `key`.
    pub fn with_key(mut self, did: impl Into<String>, key: VerifyingKey) -> Self {
        self.keys.insert(did.into(), key);
        self
    }

    #[cfg(test)]
    fn at_time(mut self, now: u64) -> Self {
        self.now_override = Some(now);
        self
    }

    fn now(&self) -> u64 {
        self.now_override.unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        })
    }
}

impl Authorizer for SignedEnvelope {
    fn authorize(&self, req: &AuthRequest<'_>) -> Result<Identity, AuthError> {
        let raw = req.header.ok_or(AuthError::Missing)?;
        let decoded = URL_SAFE_NO_PAD
            .decode(raw)
            .map_err(|e| AuthError::Malformed(e.to_string()))?;
        let env: Envelope =
            serde_json::from_slice(&decoded).map_err(|e| AuthError::Malformed(e.to_string()))?;

        let now = self.now();
        let skew = now.abs_diff(env.ts);
        if skew > MAX_CLOCK_SKEW_SECS {
            return Err(AuthError::Expired(skew - MAX_CLOCK_SKEW_SECS));
        }

        let key = self
            .keys
            .get(&env.did)
            .ok_or_else(|| AuthError::UnknownSigner(env.did.clone()))?;

        let sig_bytes = URL_SAFE_NO_PAD
            .decode(&env.sig)
            .map_err(|e| AuthError::Malformed(e.to_string()))?;
        let sig_arr: [u8; 64] = sig_bytes
            .try_into()
            .map_err(|_| AuthError::Malformed("signature is not 64 bytes".into()))?;
        let sig = Signature::from_bytes(&sig_arr);

        let msg = canonical_string(req.method, req.path, env.ts, req.body_hash_hex);
        key.verify(msg.as_bytes(), &sig)
            .map_err(|_| AuthError::BadSignature)?;

        Ok(Identity { did: env.did })
    }
}

/// Build a header value for `canonical_string`, for clients and tests.
pub fn sign_envelope(
    did: &str,
    signing_key: &ed25519_dalek::SigningKey,
    method: &str,
    path: &str,
    ts: u64,
    body_hash_hex: &str,
) -> String {
    use ed25519_dalek::Signer;
    let msg = canonical_string(method, path, ts, body_hash_hex);
    let sig = signing_key.sign(msg.as_bytes());
    let env = Envelope {
        did: did.to_string(),
        ts,
        sig: URL_SAFE_NO_PAD.encode(sig.to_bytes()),
    };
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(&env).expect("envelope serializes"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    const DID: &str = "did:coinpay:test";
    const NOW: u64 = 1_756_512_000;

    fn keypair() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn verifier(sk: &SigningKey) -> SignedEnvelope {
        SignedEnvelope::new()
            .with_key(DID, sk.verifying_key())
            .at_time(NOW)
    }

    #[test]
    fn allow_all_lets_anything_through() {
        let req = AuthRequest {
            method: "PUT",
            path: "/storage/v1/objects/abc",
            body_hash_hex: "abc",
            header: None,
        };
        assert_eq!(AllowAll.authorize(&req).unwrap(), Identity::anonymous());
    }

    #[test]
    fn valid_envelope_authorizes() {
        let sk = keypair();
        let header = sign_envelope(DID, &sk, "PUT", "/o/abc", NOW, "abc");
        let req = AuthRequest {
            method: "PUT",
            path: "/o/abc",
            body_hash_hex: "abc",
            header: Some(&header),
        };
        assert_eq!(verifier(&sk).authorize(&req).unwrap().did, DID);
    }

    #[test]
    fn missing_header_is_rejected() {
        let sk = keypair();
        let req = AuthRequest {
            method: "PUT",
            path: "/o/abc",
            body_hash_hex: "abc",
            header: None,
        };
        assert_eq!(
            verifier(&sk).authorize(&req).unwrap_err(),
            AuthError::Missing
        );
    }

    /// An envelope signed for one object must not work for another — this is
    /// the property that stops a captured header being reused.
    #[test]
    fn envelope_is_bound_to_its_path_and_body() {
        let sk = keypair();
        let header = sign_envelope(DID, &sk, "PUT", "/o/abc", NOW, "abc");

        let wrong_path = AuthRequest {
            method: "PUT",
            path: "/o/different",
            body_hash_hex: "abc",
            header: Some(&header),
        };
        assert_eq!(
            verifier(&sk).authorize(&wrong_path).unwrap_err(),
            AuthError::BadSignature
        );

        let wrong_body = AuthRequest {
            method: "PUT",
            path: "/o/abc",
            body_hash_hex: "deadbeef",
            header: Some(&header),
        };
        assert_eq!(
            verifier(&sk).authorize(&wrong_body).unwrap_err(),
            AuthError::BadSignature
        );

        let wrong_method = AuthRequest {
            method: "DELETE",
            path: "/o/abc",
            body_hash_hex: "abc",
            header: Some(&header),
        };
        assert_eq!(
            verifier(&sk).authorize(&wrong_method).unwrap_err(),
            AuthError::BadSignature
        );
    }

    #[test]
    fn unknown_signer_is_rejected() {
        let sk = keypair();
        let other = SigningKey::from_bytes(&[9u8; 32]);
        let header = sign_envelope("did:coinpay:stranger", &other, "PUT", "/o/abc", NOW, "abc");
        let req = AuthRequest {
            method: "PUT",
            path: "/o/abc",
            body_hash_hex: "abc",
            header: Some(&header),
        };
        assert!(matches!(
            verifier(&sk).authorize(&req).unwrap_err(),
            AuthError::UnknownSigner(_)
        ));
    }

    #[test]
    fn stale_envelope_is_rejected() {
        let sk = keypair();
        let old = NOW - MAX_CLOCK_SKEW_SECS - 60;
        let header = sign_envelope(DID, &sk, "PUT", "/o/abc", old, "abc");
        let req = AuthRequest {
            method: "PUT",
            path: "/o/abc",
            body_hash_hex: "abc",
            header: Some(&header),
        };
        assert!(matches!(
            verifier(&sk).authorize(&req).unwrap_err(),
            AuthError::Expired(_)
        ));
    }

    /// A clock ahead of ours by less than the skew budget is fine; well ahead
    /// is not. Rejecting only the past would let a client mint far-future
    /// envelopes that never expire.
    #[test]
    fn future_envelope_beyond_skew_is_rejected() {
        let sk = keypair();
        let near = sign_envelope(DID, &sk, "PUT", "/o/abc", NOW + 60, "abc");
        let req = AuthRequest {
            method: "PUT",
            path: "/o/abc",
            body_hash_hex: "abc",
            header: Some(&near),
        };
        assert!(verifier(&sk).authorize(&req).is_ok());

        let far = sign_envelope(DID, &sk, "PUT", "/o/abc", NOW + 10_000, "abc");
        let req = AuthRequest {
            method: "PUT",
            path: "/o/abc",
            body_hash_hex: "abc",
            header: Some(&far),
        };
        assert!(matches!(
            verifier(&sk).authorize(&req).unwrap_err(),
            AuthError::Expired(_)
        ));
    }

    #[test]
    fn garbage_header_is_malformed_not_a_panic() {
        let sk = keypair();
        for bad in ["!!!!", "", "aGVsbG8"] {
            let req = AuthRequest {
                method: "PUT",
                path: "/o/abc",
                body_hash_hex: "abc",
                header: Some(bad),
            };
            assert!(verifier(&sk).authorize(&req).is_err());
        }
    }
}
