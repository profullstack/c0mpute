//! Owner keys this machine may push as, besides its own.
//!
//! A repo is signed by its owner key, and the network checks that signature,
//! so a push from a second machine needs the same key there. Copying
//! `buyer/identity.key` over works but replaces that machine's own identity
//! (and everything published under it). Instead, `c0mpute git key export`
//! prints the key as a token and `c0mpute git key import` adds it here, to
//! `<data_dir>/git/keys/<owner>/identity.key`, next to the machine's own.
//! A push signs with whichever key owns the repo's URL.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use c0mpute_net::libp2p::identity::Keypair;

use crate::hosting::site::owner_hex;

/// Prefix of an exported key: `c0gitkey1.<hex of the protobuf keypair>`.
pub const TOKEN_PREFIX: &str = "c0gitkey1.";

fn owner_of(kp: &Keypair) -> Result<String> {
    owner_hex(kp).context("only ed25519 keys own repos")
}

fn keys_dir() -> PathBuf {
    crate::config::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("git")
        .join("keys")
}

/// This machine's own key as an export token. Anyone holding it can push to
/// (and replace) every repo and site of this key: treat it like a password.
pub fn export_token(kp: &Keypair) -> Result<String> {
    let bytes = kp.to_protobuf_encoding().context("encode key")?;
    Ok(format!("{TOKEN_PREFIX}{}", hex::encode(bytes)))
}

/// Parse an export token (surrounding whitespace allowed).
pub fn parse_token(token: &str) -> Result<Keypair> {
    let hex_part = token
        .trim()
        .strip_prefix(TOKEN_PREFIX)
        .with_context(|| format!("not a c0mpute git key (expected {TOKEN_PREFIX}...)"))?;
    let bytes = hex::decode(hex_part).context("the key is not valid hex (copied whole?)")?;
    let kp = Keypair::from_protobuf_encoding(&bytes).context("not a valid key")?;
    owner_hex(&kp).context("only ed25519 keys own repos")?;
    Ok(kp)
}

/// Add a key from an export token. Returns its owner key. Importing this
/// machine's own key, or one already imported, is a no-op.
pub fn import(token: &str) -> Result<String> {
    import_into(&keys_dir(), token, own_owner().ok().as_deref())
}

fn import_into(dir: &Path, token: &str, own: Option<&str>) -> Result<String> {
    let kp = parse_token(token)?;
    let owner = owner_of(&kp)?;
    if own == Some(owner.as_str()) {
        return Ok(owner);
    }
    let d = dir.join(&owner);
    if d.join("identity.key").exists() {
        return Ok(owner);
    }
    std::fs::create_dir_all(&d).with_context(|| format!("create {}", d.display()))?;
    c0mpute_net::identity::save(&d, &kp)?;
    Ok(owner)
}

/// Owner keys imported on this machine (not counting its own), sorted.
pub fn imported() -> Vec<String> {
    imported_in(&keys_dir())
}

fn imported_in(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("identity.key").is_file())
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|o| super::valid_owner(o))
        .collect();
    out.sort();
    out
}

/// Forget an imported key. Returns whether there was one.
pub fn remove(owner: &str) -> Result<bool> {
    remove_in(&keys_dir(), owner)
}

fn remove_in(dir: &Path, owner: &str) -> Result<bool> {
    if !super::valid_owner(owner) {
        bail!("owner must be a 64-hex key, got {owner:?}");
    }
    let d = dir.join(owner);
    if !d.exists() {
        return Ok(false);
    }
    std::fs::remove_dir_all(&d).with_context(|| format!("remove {}", d.display()))?;
    Ok(true)
}

fn own_owner() -> Result<String> {
    owner_of(&crate::hosting::publisher_keypair()?)
}

/// The key to sign a push to a repo of `owner` with: this machine's own key,
/// or one imported with `c0mpute git key import`.
pub fn keypair_for(owner: &str) -> Result<Keypair> {
    let own = crate::hosting::publisher_keypair()?;
    keypair_in(&keys_dir(), own, owner)
}

fn keypair_in(dir: &Path, own: Keypair, owner: &str) -> Result<Keypair> {
    let me = owner_of(&own)?;
    if me == owner {
        return Ok(own);
    }
    let d = dir.join(owner);
    if d.join("identity.key").is_file() && super::valid_owner(owner) {
        return c0mpute_net::identity::load_or_create(&d);
    }
    bail!(
        "{} belongs to key {owner}; this machine's key is {me}. To push from here: run \
         `c0mpute git key export` on the machine that owns it, then `c0mpute git key import` here",
        super::url_for(owner, "*")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_machine_pushes_with_an_imported_key() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("keys");
        let laptop = Keypair::generate_ed25519();
        let desktop = Keypair::generate_ed25519();
        let laptop_owner = owner_hex(&laptop).unwrap();
        let desktop_owner = owner_hex(&desktop).unwrap();

        // Before the import the desktop cannot sign for the laptop's repos.
        let err = keypair_in(&dir, desktop.clone(), &laptop_owner).unwrap_err();
        assert!(err.to_string().contains("c0mpute git key import"), "{err}");

        let token = export_token(&laptop).unwrap();
        assert!(token.starts_with(TOKEN_PREFIX));
        let owner = import_into(&dir, &format!("  {token}\n"), Some(&desktop_owner)).unwrap();
        assert_eq!(owner, laptop_owner);
        // Importing twice is fine.
        import_into(&dir, &token, Some(&desktop_owner)).unwrap();
        assert_eq!(imported_in(&dir), vec![laptop_owner.clone()]);

        // Pushes to the laptop's repos sign with its key; its own repos with its own.
        let kp = keypair_in(&dir, desktop.clone(), &laptop_owner).unwrap();
        assert_eq!(owner_hex(&kp).unwrap(), laptop_owner);
        let kp = keypair_in(&dir, desktop.clone(), &desktop_owner).unwrap();
        assert_eq!(owner_hex(&kp).unwrap(), desktop_owner);

        // Importing the machine's own key stores nothing.
        import_into(&dir, &export_token(&desktop).unwrap(), Some(&desktop_owner)).unwrap();
        assert_eq!(imported_in(&dir).len(), 1);

        assert!(remove_in(&dir, &laptop_owner).unwrap());
        assert!(!remove_in(&dir, &laptop_owner).unwrap());
        assert!(keypair_in(&dir, desktop, &laptop_owner).is_err());
    }

    #[test]
    fn bad_tokens_are_refused() {
        assert!(parse_token("hello").is_err());
        assert!(parse_token("c0gitkey1.zz").is_err());
        assert!(parse_token("c0gitkey1.00ff").is_err());
    }
}
