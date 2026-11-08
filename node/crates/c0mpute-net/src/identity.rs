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
        let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        let kp =
            Keypair::from_protobuf_encoding(&bytes).context("decode persisted libp2p identity")?;
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
    std::fs::write(path, bytes).with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

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
