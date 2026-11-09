//! Persistent libp2p identity.
//!
//! ed25519 keypair stored at `<config_dir>/identity.key` (mode 600).
//! Generated on first run; loaded thereafter. The peer-id derived from
//! this key IS the node's stable identity on the network. Rotating it
//! drops the node's reputation history; don't do it casually.

use std::path::Path;

use anyhow::{Context, Result};
use libp2p::identity::Keypair;
use tracing::info;

const KEY_FILE: &str = "identity.key";

/// Load the node's libp2p identity from `<dir>/identity.key`, generating
/// + persisting a fresh ed25519 keypair if the file doesn't exist.
pub fn load_or_create(dir: &Path) -> Result<Keypair> {
    let path = dir.join(KEY_FILE);
    if path.exists() {
        let bytes = std::fs::read(&path)
            .with_context(|| format!("read {}", path.display()))?;
        let kp = Keypair::from_protobuf_encoding(&bytes)
            .context("decode persisted libp2p identity")?;
        info!(peer_id = %kp.public().to_peer_id(), file = %path.display(), "loaded identity");
        return Ok(kp);
    }

    std::fs::create_dir_all(dir)?;
    let kp = Keypair::generate_ed25519();
    let encoded = kp.to_protobuf_encoding().context("encode keypair")?;
    write_locked_down(&path, &encoded)?;
    info!(peer_id = %kp.public().to_peer_id(), file = %path.display(), "generated new identity");
    Ok(kp)
}

/// The peer-id of the identity already stored in `<dir>/identity.key`,
/// without generating one. `None` when there is no (readable) key.
pub fn existing_peer_id(dir: &Path) -> Option<libp2p::PeerId> {
    let bytes = std::fs::read(dir.join(KEY_FILE)).ok()?;
    let kp = Keypair::from_protobuf_encoding(&bytes).ok()?;
    Some(kp.public().to_peer_id())
}

#[cfg(unix)]
fn write_locked_down(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut f = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    use std::io::Write;
    f.write_all(bytes)?;
    Ok(())
}

#[cfg(not(unix))]
fn write_locked_down(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes)
        .with_context(|| format!("write {}", path.display()))
}

/// The raw ed25519 public key behind a peer id, as lowercase hex, when the
/// peer id inlines one (every c0mpute identity does: ed25519 keys are small
/// enough to sit in an identity multihash). This is the key an OpenDisk
/// descriptor states as `ed25519:<hex>`.
pub fn ed25519_public_hex(peer: &libp2p::PeerId) -> Option<String> {
    let mh: &libp2p::multihash::Multihash<64> = peer.as_ref();
    if mh.code() != 0x00 {
        return None;
    }
    let public = libp2p::identity::PublicKey::try_decode_protobuf(mh.digest()).ok()?;
    let ed = public.try_into_ed25519().ok()?;
    Some(ed.to_bytes().iter().map(|b| format!("{b:02x}")).collect())
}

/// Check that `sig` is `peer`'s signature over `msg`, using the public key
/// the peer id inlines (identity multihash). Peer ids that hash their key
/// (RSA) cannot be checked this way and never verify.
pub fn verify_peer_signature(peer: &libp2p::PeerId, msg: &[u8], sig: &[u8]) -> bool {
    let mh: &libp2p::multihash::Multihash<64> = peer.as_ref();
    if mh.code() != 0x00 {
        return false;
    }
    let Ok(public) = libp2p::identity::PublicKey::try_decode_protobuf(mh.digest()) else {
        return false;
    };
    public.to_peer_id() == *peer && public.verify(msg, sig)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_signatures_verify_only_for_the_signer() {
        let kp = Keypair::generate_ed25519();
        let other = Keypair::generate_ed25519().public().to_peer_id();
        let peer = kp.public().to_peer_id();
        let sig = kp.sign(b"hello").unwrap();
        assert!(verify_peer_signature(&peer, b"hello", &sig));
        assert!(!verify_peer_signature(&peer, b"hellp", &sig));
        assert!(!verify_peer_signature(&other, b"hello", &sig));
    }

    #[test]
    fn peer_id_yields_its_ed25519_key() {
        let kp = Keypair::generate_ed25519();
        let peer = kp.public().to_peer_id();
        let got = ed25519_public_hex(&peer).expect("ed25519 peer id");
        let want: String = kp
            .public()
            .try_into_ed25519()
            .unwrap()
            .to_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(got, want);
        assert_eq!(got.len(), 64);
    }

    #[test]
    fn generate_and_reload_yields_same_peer_id() {
        let dir = tempdir();
        let kp1 = load_or_create(&dir).unwrap();
        let kp2 = load_or_create(&dir).unwrap();
        assert_eq!(kp1.public().to_peer_id(), kp2.public().to_peer_id());
    }

    fn tempdir() -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "c0mpute-identity-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
