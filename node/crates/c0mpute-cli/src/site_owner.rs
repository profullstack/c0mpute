//! Which key publishes (and owns) a site: the producer's account key, so the
//! same `--name` lands on the same site from any machine they use.
//!
//! A site's id is `site_id_for(owner, name)`. Until 0.3.73 the owner was the
//! publishing machine's buyer key, so the same name published from a second
//! machine silently became a second site with a new URL, and only the first
//! machine could ever update or remove the original. Now the owner is the
//! account key, derived on the producer's own machine from their c0mpute
//! identity (`chat.key`, the one `c0mpute dm` and private storage use). Copy
//! or restore that identity onto every machine you publish from. c0mpute.com
//! and node operators never hold it, so only the producer can change a site.
//!
//! Sites already live under a machine key keep their id (and every Moshpit
//! name and domain pointing at it) until the producer moves them with
//! `publish --move`.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use c0mpute_core::hosting::site;
use c0mpute_net::libp2p::identity::Keypair;
use c0mpute_secure_chat as chat;
use serde::{Deserialize, Serialize};

/// `--key` on `hosting publish` / `hosting rm`.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum KeyChoice {
    /// The site's existing owner if it has one, else your account key.
    #[default]
    Auto,
    /// Your account key (from your c0mpute identity): same site from any machine.
    Account,
    /// This machine's key (how sites were owned before 0.3.73).
    Machine,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OwnerKind {
    Account,
    Machine,
}

impl OwnerKind {
    pub fn label(self) -> &'static str {
        match self {
            OwnerKind::Account => "your account key",
            OwnerKind::Machine => "this machine's key",
        }
    }
}

/// A key a site can be signed with.
pub struct Owner {
    pub kind: OwnerKind,
    pub keypair: Keypair,
    pub owner_hex: String,
}

impl Owner {
    fn new(kind: OwnerKind, keypair: Keypair) -> Result<Self> {
        let owner_hex = site::owner_hex(&keypair).context("hosting needs an ed25519 key")?;
        Ok(Owner { kind, keypair, owner_hex })
    }

    pub fn site_id(&self, name: &str) -> String {
        site::site_id_for(&self.owner_hex, name)
    }

    /// `None` = this machine's publisher key (what the core functions default to).
    pub fn signer(&self) -> Option<&Keypair> {
        match self.kind {
            OwnerKind::Account => Some(&self.keypair),
            OwnerKind::Machine => None,
        }
    }
}

pub fn machine() -> Result<Owner> {
    Owner::new(OwnerKind::Machine, c0mpute_core::hosting::publisher_keypair()?)
}

/// Why the account key is not available right now.
pub enum NoAccount {
    /// No c0mpute identity on this machine.
    Missing,
    /// There is one, but no password to unlock it without a terminal.
    Locked,
}

/// Unlock the account key: `C0MPUTE_DM_PASSWORD`, else a prompt on a terminal.
pub fn account() -> Result<std::result::Result<Owner, NoAccount>> {
    let path = chat::key_file_path()?;
    if !path.exists() {
        return Ok(Err(NoAccount::Missing));
    }
    let pw = match std::env::var("C0MPUTE_DM_PASSWORD") {
        Ok(p) if !p.is_empty() => zeroize::Zeroizing::new(p),
        _ if std::io::stdin().is_terminal() => {
            zeroize::Zeroizing::new(rpassword::prompt_password("c0mpute identity password (signs your sites): ")?)
        }
        _ => return Ok(Err(NoAccount::Locked)),
    };
    let key = chat::decrypt_keyfile(&chat::load_keyfile(&path)?, &pw)?;
    Ok(Ok(Owner::new(OwnerKind::Account, c0mpute_core::hosting::account_keypair(&key)?)?))
}

fn account_required() -> Result<Owner> {
    match account()? {
        Ok(o) => Ok(o),
        Err(NoAccount::Missing) => bail!(
            "no c0mpute identity on this machine. Create one with `c0mpute storage key init --backup-to <file>`, \
             or copy your existing one here (`{}`) to publish the same sites you publish elsewhere",
            chat::key_file_path().map(|p| p.display().to_string()).unwrap_or_default()
        ),
        Err(NoAccount::Locked) => bail!("set C0MPUTE_DM_PASSWORD to unlock your c0mpute identity (no terminal to ask on)"),
    }
}

// ───────────────────────────────────────────────────────────── folder link

/// `<dir>/.c0mpute/site.json`: which site a folder was published as, so
/// publishing it again (from any machine) updates that site instead of
/// starting a new one. Hidden, so it is never published.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Link {
    pub name: String,
    pub site_id: String,
    pub owner: String,
    pub key: OwnerKind,
}

fn link_path(dir: &Path) -> PathBuf {
    dir.join(".c0mpute").join("site.json")
}

pub fn read_link(dir: &Path) -> Option<Link> {
    serde_json::from_slice(&std::fs::read(link_path(dir)).ok()?).ok()
}

pub fn write_link(dir: &Path, link: &Link) {
    let p = link_path(dir);
    let ok = p.parent().is_some_and(|d| std::fs::create_dir_all(d).is_ok())
        && serde_json::to_vec_pretty(link).is_ok_and(|b| std::fs::write(&p, b).is_ok());
    if !ok {
        eprintln!("note: could not write {} (publishing again still works, it just cannot double-check the owner)", p.display());
    }
}

// ───────────────────────────────────────────────────────────── network view

/// A site with this name on the gateway (any owner).
#[derive(Debug, Clone)]
pub struct Seen {
    pub site_id: String,
    pub owner: String,
    pub size: u64,
}

/// Sites named `name` that a gateway knows, live (non-empty) ones only.
/// `None` when the gateway cannot be asked.
pub async fn seen(gateway: &str, name: &str) -> Option<Vec<Seen>> {
    let url = format!("{}/hosting/v1/sites", gateway.trim_end_matches('/'));
    let v: serde_json::Value = reqwest::Client::new()
        .get(&url)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    Some(
        v["sites"]
            .as_array()?
            .iter()
            .filter(|s| s["name"].as_str() == Some(name))
            .map(|s| Seen {
                site_id: s["site_id"].as_str().unwrap_or_default().to_string(),
                owner: s["owner"].as_str().unwrap_or_default().to_string(),
                size: s["size"].as_u64().unwrap_or(0),
            })
            .filter(|s| s.size > 0)
            .collect(),
    )
}

// ───────────────────────────────────────────────────────────── choosing

pub struct Choice {
    pub owner: Owner,
    /// A live machine-owned site with this name to take down after a `--move`.
    pub move_from: Option<Owner>,
}

/// Pick the key `hosting publish` signs with. Refuses, with what to do,
/// instead of silently starting a second site under a new URL.
pub async fn for_publish(
    dir: &Path,
    name: &str,
    choice: KeyChoice,
    gateway: &str,
    moving: bool,
    new_site: bool,
) -> Result<Choice> {
    let machine = machine()?;
    let link = read_link(dir).filter(|l| l.name == name);
    let seen = seen(gateway, name).await;
    let machine_live = seen
        .as_ref()
        .is_some_and(|s| s.iter().any(|x| x.site_id == machine.site_id(name)));

    if moving {
        let account = account_required()?;
        if account.owner_hex == machine.owner_hex {
            bail!("--move: this machine's key is your account key already");
        }
        let from = (machine_live || link.as_ref().is_some_and(|l| l.owner == machine.owner_hex)).then_some(machine);
        return Ok(Choice { owner: account, move_from: from });
    }

    let owner = match (choice, &link) {
        (KeyChoice::Machine, _) => machine,
        (KeyChoice::Account, _) => account_required()?,
        // The folder says who owns it: keep that owner, or explain.
        (KeyChoice::Auto, Some(l)) if l.owner == machine.owner_hex => machine,
        (KeyChoice::Auto, Some(l)) => match account()? {
            Ok(a) if a.owner_hex == l.owner => a,
            other => {
                let why = match (l.key, other) {
                    (OwnerKind::Machine, _) => format!(
                        "it is owned by the key of the machine it was first published from ({}…), and only \
                         that machine can update it. Publish from that machine, or there run \
                         `c0mpute hosting publish {} --name {name} --move` once to hand it to your account \
                         key, which works from every machine",
                        &l.owner[..12.min(l.owner.len())],
                        dir.display()
                    ),
                    (OwnerKind::Account, Ok(_)) => format!(
                        "it belongs to a different c0mpute identity ({}…) than the one on this machine",
                        &l.owner[..12.min(l.owner.len())]
                    ),
                    (OwnerKind::Account, Err(NoAccount::Missing)) => format!(
                        "it is owned by your account key, and this machine has no c0mpute identity: copy yours \
                         here ({})",
                        chat::key_file_path().map(|p| p.display().to_string()).unwrap_or_default()
                    ),
                    (OwnerKind::Account, Err(NoAccount::Locked)) => {
                        "it is owned by your account key: set C0MPUTE_DM_PASSWORD to unlock it".to_string()
                    }
                };
                bail!(
                    "{} was published as site {} and {why}.\nPublishing it from here would make a separate \
                     site with a new URL; pass --new-site if that is really what you want.",
                    dir.display(),
                    l.site_id
                );
            }
        },
        // Already live under this machine's key: keep its id (and every
        // name and domain pointing at it).
        (KeyChoice::Auto, None) if machine_live => {
            eprintln!(
                "note: {name} is owned by this machine's key, so only this machine can update it. \
                 Add --move once to hand it to your account key (new site id: repoint names/domains)"
            );
            machine
        }
        (KeyChoice::Auto, None) => match account()? {
            Ok(a) => a,
            Err(NoAccount::Missing) => {
                eprintln!(
                    "note: signing with this machine's key, so only this machine can update {name}. \
                     Run `c0mpute storage key init --backup-to <file>` to publish with your account key \
                     from any machine"
                );
                machine
            }
            Err(NoAccount::Locked) => {
                eprintln!(
                    "note: your c0mpute identity is locked (no terminal, no C0MPUTE_DM_PASSWORD): signing \
                     with this machine's key"
                );
                machine
            }
        },
    };

    // The same name under somebody else's key is a different site.
    if !new_site && link.is_none() {
        match &seen {
            Some(s) => {
                let others: Vec<&Seen> = s.iter().filter(|x| x.owner != owner.owner_hex).collect();
                if !others.is_empty() && !s.iter().any(|x| x.owner == owner.owner_hex) {
                    let list = others
                        .iter()
                        .map(|x| format!("  {}  owner {}…  {} bytes", x.site_id, &x.owner[..12.min(x.owner.len())], x.size))
                        .collect::<Vec<_>>()
                        .join("\n");
                    let msg = format!(
                        "a site named {name} is already live under another key:\n{list}\n\
                         Publishing here signs with {} ({}…), which makes a separate site with its own URL \
                         ({}); it will not update the one above. If that one is yours, publish from the \
                         machine that made it (it can --move it to your account key).",
                        owner.kind.label(),
                        &owner.owner_hex[..12],
                        owner.site_id(name)
                    );
                    if std::io::stdin().is_terminal() {
                        eprintln!("{msg}");
                        eprint!("Publish a separate site anyway? [y/N] ");
                        let mut line = String::new();
                        std::io::stdin().read_line(&mut line)?;
                        if !line.trim().eq_ignore_ascii_case("y") {
                            bail!("not published");
                        }
                    } else {
                        bail!("{msg}\nPass --new-site to publish a separate site anyway.");
                    }
                }
            }
            None => eprintln!("note: could not ask {gateway} which sites are named {name}; skipping the duplicate check"),
        }
    }
    Ok(Choice { owner, move_from: None })
}

/// Pick the key `hosting rm` signs with: the owner of the live site named
/// `name` (account first), or what `--key` says.
pub async fn for_rm(name: &str, choice: KeyChoice, gateway: &str) -> Result<Owner> {
    match choice {
        KeyChoice::Machine => return machine(),
        KeyChoice::Account => return account_required(),
        KeyChoice::Auto => {}
    }
    let machine = machine()?;
    let Some(seen) = seen(gateway, name).await else {
        eprintln!("note: could not ask {gateway} who owns {name}; removing this machine's copy (use --key account for yours)");
        return Ok(machine);
    };
    let machine_live = seen.iter().any(|x| x.site_id == machine.site_id(name));
    let account = match account()? {
        Ok(a) => Some(a),
        Err(_) => None,
    };
    let account_live = account
        .as_ref()
        .is_some_and(|a| seen.iter().any(|x| x.site_id == a.site_id(name)));
    match (account, account_live, machine_live) {
        (Some(_), true, true) => bail!(
            "{name} is live under both your account key and this machine's key: say which with --key account|machine"
        ),
        (Some(a), true, false) => Ok(a),
        (_, _, true) => Ok(machine),
        _ => bail!("no live site named {name} is owned by your account key or this machine's key"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner_of(seed: [u8; 32]) -> String {
        let key = chat::ChatKey::from_seed(seed);
        site::owner_hex(&c0mpute_core::hosting::account_keypair(&key).unwrap()).unwrap()
    }

    #[test]
    fn the_account_key_is_the_same_on_every_machine() {
        // Two machines holding the same identity sign as the same owner, so
        // one --name is one site id wherever it is published from.
        assert_eq!(owner_of([7; 32]), owner_of([7; 32]));
        assert_eq!(
            site::site_id_for(&owner_of([7; 32]), "alt2600"),
            site::site_id_for(&owner_of([7; 32]), "alt2600")
        );
        assert_ne!(owner_of([7; 32]), owner_of([8; 32]));
    }

    #[test]
    fn the_account_key_is_not_the_storage_root() {
        let key = chat::ChatKey::from_seed([9; 32]);
        assert_ne!(
            *key.derive_subkey(c0mpute_core::hosting::ACCOUNT_OWNER_LABEL),
            *key.derive_subkey(c0mpute_core::sealed_store::STORAGE_LABEL)
        );
    }

    #[test]
    fn a_folder_remembers_its_site_and_the_link_is_hidden() {
        let dir = std::env::temp_dir().join(format!("c0mpute-link-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(read_link(&dir).is_none());
        let l = Link {
            name: "alt2600".into(),
            site_id: "76eaa600f9373b7dd35b".into(),
            owner: "ab".repeat(32),
            key: OwnerKind::Account,
        };
        write_link(&dir, &l);
        let back = read_link(&dir).unwrap();
        assert_eq!((back.name.as_str(), back.key), ("alt2600", OwnerKind::Account));
        // Hidden: the publisher skips dot-folders, so it never ships.
        assert!(link_path(&dir).components().any(|c| c.as_os_str() == ".c0mpute"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
