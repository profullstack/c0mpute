//! End-to-end encrypted repos: hosting nodes and gateways keep and serve the
//! repo, but only holders of the repo key can read it.
//!
//! * The **repo key** is 32 bytes. The owner never stores it: it is derived
//!   from the owner's identity key and the repo name (HKDF-SHA256), so any
//!   machine with the owner's `identity.key` can push and read. Anyone else
//!   reads with the key given to them: `c0mpute git share <repo>` prints a
//!   URL `c0mpute://<owner>/<repo>#key=<key>` (the fragment is never sent to
//!   a gateway; git keeps it in the remote URL).
//! * Every pack and index is sealed whole with AES-256-GCM, bound to the
//!   repo and file kind, and stored under a keyed name, so file names do not
//!   reveal git's pack hashes.
//! * The ref map keeps what a gateway checks in the clear (`repo`, `packs`,
//!   `prev`, `force`) and seals the rest (`head`, `refs`, and which git pack
//!   each stored pack is) in `sealed`. `refs` is empty and the dumb-HTTP
//!   files are not written, so gateways and old clients learn nothing
//!   beyond sizes, push times and the number of packs.
//! * The owner signature still covers everything (the record signs the
//!   manifest, the manifest pins every sealed file), so a gateway can neither
//!   read nor forge an encrypted repo.

use std::collections::BTreeMap;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use hkdf::Hkdf;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use zeroize::Zeroizing;

/// Prefix of every sealed file and of the sealed ref map.
const MAGIC: &[u8] = b"C0GE2E1";
const DOMAIN: &[u8] = b"c0mpute-git-e2e-v1";

/// The 32-byte key of one encrypted repo.
#[derive(Clone)]
pub struct RepoKey(Zeroizing<[u8; 32]>);

impl std::fmt::Debug for RepoKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RepoKey(..)")
    }
}

impl RepoKey {
    /// The owner's key for `<owner>/<repo>`, from the owner's identity.
    pub fn derive(keypair: &c0mpute_net::libp2p::identity::Keypair, repo: &str) -> Result<Self> {
        let ed = keypair
            .clone()
            .try_into_ed25519()
            .map_err(|_| anyhow::anyhow!("encrypted repos need an ed25519 identity"))?;
        let secret = Zeroizing::new(ed.to_bytes());
        let owner = hex::encode(ed.public().to_bytes());
        let hk = Hkdf::<Sha256>::new(Some(DOMAIN), &secret[..32]);
        let mut k = Zeroizing::new([0u8; 32]);
        hk.expand(format!("repo-key/{owner}/{repo}").as_bytes(), &mut k[..])
            .expect("32 bytes is a valid HKDF length");
        Ok(Self(k))
    }

    /// From its text form: base64url (43 chars) or 64 hex.
    pub fn from_text(s: &str) -> Result<Self> {
        let s = s.trim();
        let v = if s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit()) {
            hex::decode(s)?
        } else {
            B64URL.decode(s).context("repo key is not base64url")?
        };
        let a: [u8; 32] = v
            .try_into()
            .map_err(|_| anyhow::anyhow!("a repo key is 32 bytes"))?;
        Ok(Self(Zeroizing::new(a)))
    }

    pub fn to_text(&self) -> String {
        B64URL.encode(&self.0[..])
    }

    fn sub(&self, label: &str) -> Zeroizing<[u8; 32]> {
        let hk = Hkdf::<Sha256>::new(Some(DOMAIN), &self.0[..]);
        let mut k = Zeroizing::new([0u8; 32]);
        hk.expand(label.as_bytes(), &mut k[..])
            .expect("32 bytes is a valid HKDF length");
        k
    }

    fn cipher(&self) -> Aes256Gcm {
        Aes256Gcm::new_from_slice(&self.sub("seal")[..]).expect("32-byte key")
    }

    /// `MAGIC`, 12-byte random nonce, ciphertext + tag. `aad` binds the
    /// bytes to where they belong (repo and file kind).
    pub fn seal(&self, aad: &str, plain: &[u8]) -> Vec<u8> {
        let mut nonce = [0u8; 12];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        let ct = self
            .cipher()
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plain,
                    aad: aad.as_bytes(),
                },
            )
            .expect("AES-GCM encrypts any length we store");
        let mut out = Vec::with_capacity(MAGIC.len() + 12 + ct.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        out
    }

    pub fn open(&self, aad: &str, sealed: &[u8]) -> Result<Vec<u8>> {
        let rest = sealed
            .strip_prefix(MAGIC)
            .context("not an encrypted c0mpute git file")?;
        if rest.len() < 12 + 16 {
            bail!("encrypted file is truncated");
        }
        self.cipher()
            .decrypt(
                Nonce::from_slice(&rest[..12]),
                Payload {
                    msg: &rest[12..],
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| anyhow::anyhow!("wrong repo key (or the file was tampered with)"))
    }

    /// Stored name of git pack `git_name`: keyed, so it reveals nothing.
    pub fn pack_name(&self, git_name: &str) -> String {
        let k = self.sub("pack-name");
        let h = blake3::keyed_hash(&k, git_name.as_bytes());
        format!("pack-{}", &h.to_hex()[..40])
    }
}

/// Additional data for a repo's sealed files.
pub fn aad(owner: &str, repo: &str, what: &str) -> String {
    format!("c0mpute-git/{owner}/{repo}/{what}")
}

/// The secret half of an encrypted repo's ref map.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedRefs {
    pub head: Option<String>,
    pub refs: BTreeMap<String, String>,
    /// Stored pack name -> git's pack name (`pack-<sha>`).
    pub packs: BTreeMap<String, String>,
}

impl SealedRefs {
    pub fn seal(&self, key: &RepoKey, owner: &str, repo: &str) -> String {
        let json = serde_json::to_vec(self).expect("sealed refs serialize");
        B64URL.encode(key.seal(&aad(owner, repo, "refs"), &json))
    }

    pub fn open(key: &RepoKey, owner: &str, repo: &str, sealed: &str) -> Result<Self> {
        let raw = B64URL
            .decode(sealed.trim())
            .context("sealed ref map is not base64url")?;
        let json = key.open(&aad(owner, repo, "refs"), &raw)?;
        serde_json::from_slice(&json).context("sealed ref map")
    }
}

/// Split `c0mpute://...#key=K&e2e` into the URL and its fragment options:
/// `(url, key, e2e)`. `#e2e` asks for an encrypted repo; `#key=` implies it.
pub fn split_fragment(url: &str) -> Result<(&str, Option<RepoKey>, bool)> {
    let Some((base, frag)) = url.split_once('#') else {
        return Ok((url, None, false));
    };
    let mut key = None;
    let mut e2e = false;
    for part in frag.split('&').filter(|p| !p.is_empty()) {
        match part.split_once('=') {
            Some(("key", v)) => {
                key = Some(RepoKey::from_text(v)?);
                e2e = true;
            }
            None if part == "e2e" => e2e = true,
            _ => bail!("unknown option #{part} in {base}"),
        }
    }
    Ok((base, key, e2e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_round_trip_and_binding() {
        let k = RepoKey::from_text(&"ab".repeat(32)).unwrap();
        let s = k.seal("a", b"secret");
        assert_eq!(k.open("a", &s).unwrap(), b"secret");
        assert!(k.open("b", &s).is_err(), "aad binds the file");
        let other = RepoKey::from_text(&"cd".repeat(32)).unwrap();
        assert!(other.open("a", &s).is_err());
        assert_eq!(
            RepoKey::from_text(&k.to_text()).unwrap().to_text(),
            k.to_text()
        );
        let n = k.pack_name("pack-0000000000000000000000000000000000000000");
        assert!(crate::git::valid_pack_name(&n), "{n}");
        assert_ne!(
            n,
            other.pack_name("pack-0000000000000000000000000000000000000000")
        );
    }

    #[test]
    fn derive_is_per_repo_and_stable() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let a = RepoKey::derive(&kp, "a").unwrap();
        assert_eq!(a.to_text(), RepoKey::derive(&kp, "a").unwrap().to_text());
        assert_ne!(a.to_text(), RepoKey::derive(&kp, "b").unwrap().to_text());
    }

    #[test]
    fn sealed_refs_round_trip() {
        let k = RepoKey::from_text(&"11".repeat(32)).unwrap();
        let mut r = SealedRefs::default();
        r.head = Some("refs/heads/main".into());
        r.refs.insert("refs/heads/main".into(), "a".repeat(40));
        let s = r.seal(&k, "o", "r");
        assert_eq!(SealedRefs::open(&k, "o", "r", &s).unwrap(), r);
        assert!(SealedRefs::open(&k, "o", "other", &s).is_err());
    }

    #[test]
    fn fragments() {
        let k = RepoKey::from_text(&"22".repeat(32)).unwrap();
        let url = format!("c0mpute://x/y#key={}", k.to_text());
        let (base, key, e2e) = split_fragment(&url).unwrap();
        assert_eq!(base, "c0mpute://x/y");
        assert!(e2e && key.is_some());
        let (_, key, e2e) = split_fragment("c0mpute://x/y#e2e").unwrap();
        assert!(e2e && key.is_none());
        assert!(split_fragment("c0mpute://x/y#nope").is_err());
        assert_eq!(split_fragment("c0mpute://x/y").unwrap().0, "c0mpute://x/y");
    }
}
