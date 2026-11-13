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
        let bytes = std::fs::read(&path).map_err(|e| unreadable(&path, e))?;
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

/// Persist `kp` as `<dir>/identity.key` (mode 600). Refuses to overwrite an
/// existing key: losing one is not recoverable.
pub fn save(dir: &Path, kp: &Keypair) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let encoded = kp.to_protobuf_encoding().context("encode keypair")?;
    write_locked_down(&dir.join(KEY_FILE), &encoded)
}

/// A read error on an identity file, with the fix when it is a permission
/// problem: almost always the file was created by `sudo c0mpute ...` (or a
/// root service with this HOME) and now belongs to root.
pub fn unreadable(path: &Path, e: std::io::Error) -> anyhow::Error {
    if e.kind() != std::io::ErrorKind::PermissionDenied {
        return anyhow::Error::new(e).context(format!("read {}", path.display()));
    }
    #[cfg(unix)]
    let owner = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path)
            .ok()
            .map(|m| format!(" (owned by uid {}, mode {:o})", m.uid(), m.mode() & 0o777))
            .unwrap_or_default()
    };
    #[cfg(not(unix))]
    let owner = String::new();
    let dir = path.parent().unwrap_or(path);
    anyhow::anyhow!(
        "cannot read {}{owner}: permission denied. It was probably created by running c0mpute \
         with sudo or by a root service using this home directory. Give it back to your user: \
         sudo chown -R \"$(id -un):$(id -gn)\" {}",
        path.display(),
        dir.display()
    )
}

const JOB_SECRET_FILE: &str = "job-e2e.secret";

/// The node's job-key secret (`<dir>/job-e2e.secret`, 32 random bytes, mode
/// 600 like the identity key), created on first use. A buyer derives each
/// job's X25519 key from it and the job id (DIP-0032), so after a restart
/// it can re-derive the key of a job still running on a worker. Deleting
/// the file rotates it (in-flight jobs then cannot be resumed).
pub fn load_or_create_job_secret(dir: &Path) -> Result<zeroize::Zeroizing<[u8; 32]>> {
    let path = dir.join(JOB_SECRET_FILE);
    match std::fs::read(&path) {
        Ok(bytes) => {
            let b: [u8; 32] = bytes.as_slice().try_into().with_context(|| format!("{} must hold 32 bytes", path.display()))?;
            return Ok(zeroize::Zeroizing::new(b));
        }
        // Present but unreadable: never rotate it behind the user's back.
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return Err(unreadable(&path, e)),
        Err(_) => {}
    }
    std::fs::create_dir_all(dir)?;
    let mut b = zeroize::Zeroizing::new([0u8; 32]);
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut *b);
    write_locked_down(&path, &*b)?;
    Ok(b)
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

    use std::io::Write;

    // The secret is written whole to a sibling file first and only then given
    // its name. Writing in place left a 0-byte key behind when the write
    // failed (a full disk), and every later start then died on "must hold 32
    // bytes" until someone deleted it.
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.tmp-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let staged = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new().create_new(true).write(true).mode(0o600).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()
    })();
    if let Err(e) = staged {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("write {}", path.display()));
    }
    // A hard link fails when the name is taken, so an existing key is never
    // overwritten (the `create_new` guarantee), and the name only ever shows
    // the complete file.
    let placed = match std::fs::hard_link(&tmp, path) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists && !path.exists() => {
            // A filesystem without hard links: rename is the next best thing.
            std::fs::rename(&tmp, path)
        }
        other => other,
    };
    let _ = std::fs::remove_file(&tmp);
    placed.with_context(|| format!("create {}", path.display()))
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

    #[cfg(unix)]
    #[test]
    fn a_secret_is_written_whole_private_and_never_over_an_existing_one() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("c0-identity-write-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("identity.key");
        write_locked_down(&path, b"first secret").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"first secret");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        // An existing key is refused and left as it was.
        assert!(write_locked_down(&path, b"second secret").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"first secret");
        // No scratch file is left beside it either way.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        // A write that cannot happen leaves no file at all (not an empty key).
        let gone = dir.join("missing").join("identity.key");
        assert!(write_locked_down(&gone, b"x").is_err());
        assert!(!gone.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

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

    /// A key the user cannot read (root-owned after `sudo c0mpute ...`) is an
    /// actionable error, and neither file is replaced.
    #[cfg(unix)]
    #[test]
    fn unreadable_identity_says_how_to_fix_it() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir();
        load_or_create(&dir).unwrap();
        load_or_create_job_secret(&dir).unwrap();
        let key = dir.join(KEY_FILE);
        let secret = dir.join(JOB_SECRET_FILE);
        let before = (std::fs::read(&key).unwrap(), std::fs::read(&secret).unwrap());
        for f in [&key, &secret] {
            std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o000)).unwrap();
        }
        if std::fs::read(&key).is_ok() {
            eprintln!("skipping: running as root, every file is readable");
            return;
        }
        let err = format!("{:#}", load_or_create(&dir).unwrap_err());
        assert!(err.contains("permission denied") && err.contains("sudo chown"), "{err}");
        let err = format!("{:#}", load_or_create_job_secret(&dir).unwrap_err());
        assert!(err.contains("sudo chown"), "{err}");
        for f in [&key, &secret] {
            std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert_eq!((std::fs::read(&key).unwrap(), std::fs::read(&secret).unwrap()), before, "nothing was rotated");
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
