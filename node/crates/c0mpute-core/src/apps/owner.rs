//! Which key owns an app: the producer's account key, so the same name is the
//! same app from any machine they use (as hosting sites since 0.3.73).
//!
//! An app's id is `app_id_for(owner, name)`, and holders let only the owner's
//! libp2p peer hand out secrets and read logs. The owner used to be the
//! deploying machine's buyer key, so only that machine could ever redeploy,
//! read logs or delete. Now it is the account key ([`crate::hosting::account_keypair`],
//! derived on the producer's machine from their c0mpute identity, `chat.key`):
//!
//! * records are signed with it, so the id is the same everywhere;
//! * for owner-only calls the CLI joins the network *as* that key, so a
//!   holder's `peer == owner_peer()` check holds with no protocol change;
//! * the app key (which encrypts state snapshots) is derived from the same
//!   identity per app name ([`derive_app_key`]), so any machine holding the
//!   identity can rebuild it. Secret env values are not: they live on the
//!   machine they were set on and sealed on the holders.
//!
//! An app already deployed under a machine key keeps its id and stays under
//! that machine's control until the producer moves it (`apps deploy --move`).

use anyhow::{Context, Result, bail};
use c0mpute_net::libp2p::identity::Keypair;
use c0mpute_secure_chat::{self as chat, ChatKey};
use zeroize::Zeroizing;

use super::record::{AppRecord, app_id_for};
use crate::hosting::{self, site};

/// HKDF label (with the app name appended) that turns a producer's c0mpute
/// identity seed into an app's key. Distinct from the label of the account
/// key itself ([`hosting::ACCOUNT_OWNER_LABEL`]) and of private storage.
pub const APP_KEY_LABEL: &[u8] = b"c0mpute-apps-app-key-v1";

/// The key that encrypts the state snapshots of the app `name` owned by this
/// identity. Same identity + same name = same key on every machine.
pub fn derive_app_key(key: &ChatKey, name: &str) -> Zeroizing<[u8; 32]> {
    let mut label = APP_KEY_LABEL.to_vec();
    label.push(b'\n');
    label.extend_from_slice(name.as_bytes());
    key.derive_subkey(&label)
}

/// `--key` on `apps deploy` / `logs` / `destroy`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum KeyChoice {
    /// The app's existing owner if it was deployed from here, else the account key.
    #[default]
    Auto,
    /// The account key (from the c0mpute identity): same app from any machine.
    Account,
    /// This machine's key (how apps were owned before the account key).
    Machine,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

/// A key an app can be signed with (and the peer its owner calls come from).
pub struct AppOwner {
    pub kind: OwnerKind,
    pub keypair: Keypair,
    /// ed25519 public key, hex: the record's `owner`.
    pub owner_hex: String,
    /// The identity app keys are derived from (account owners only).
    identity: Option<ChatKey>,
}

impl AppOwner {
    /// This machine's key, `<data_dir>/buyer/identity.key`.
    pub fn machine() -> Result<Self> {
        Self::from_machine_key(hosting::publisher_keypair()?)
    }

    pub fn from_machine_key(keypair: Keypair) -> Result<Self> {
        let owner_hex = site::owner_hex(&keypair).context("apps are signed with an ed25519 key")?;
        Ok(AppOwner {
            kind: OwnerKind::Machine,
            keypair,
            owner_hex,
            identity: None,
        })
    }

    /// The account key of an unlocked c0mpute identity.
    pub fn account(key: ChatKey) -> Result<Self> {
        let keypair = hosting::account_keypair(&key)?;
        let owner_hex = site::owner_hex(&keypair).context("apps are signed with an ed25519 key")?;
        Ok(AppOwner {
            kind: OwnerKind::Account,
            keypair,
            owner_hex,
            identity: Some(key),
        })
    }

    pub fn app_id(&self, name: &str) -> String {
        app_id_for(&self.owner_hex, name)
    }

    /// The libp2p peer (base58) holders accept owner-only calls from.
    pub fn peer_id(&self) -> String {
        self.keypair.public().to_peer_id().to_base58()
    }

    /// The derived app key (hex) of `name`. `None` for a machine owner, whose
    /// app keys are random and kept in its owner file.
    pub fn app_key(&self, name: &str) -> Option<String> {
        self.identity
            .as_ref()
            .map(|k| hex::encode(*derive_app_key(k, name)))
    }

    pub fn owns(&self, rec: &AppRecord) -> bool {
        rec.owner == self.owner_hex
    }
}

/// Why the account key is not available right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoAccount {
    /// No c0mpute identity on this machine.
    Missing,
    /// There is one, but nothing to unlock it with.
    Locked,
}

/// How a caller unlocks the c0mpute identity (the CLI may prompt).
pub type Unlock<'a> = &'a (dyn Fn() -> Result<std::result::Result<ChatKey, NoAccount>> + Sync);

/// Unlock with `C0MPUTE_DM_PASSWORD` only (no terminal: `git push`, agents).
pub fn unlock_from_env() -> Result<std::result::Result<ChatKey, NoAccount>> {
    let path = chat::key_file_path()?;
    if !path.exists() {
        return Ok(Err(NoAccount::Missing));
    }
    match std::env::var("C0MPUTE_DM_PASSWORD") {
        Ok(p) if !p.is_empty() => {
            let pw = Zeroizing::new(p);
            Ok(Ok(chat::decrypt_keyfile(&chat::load_keyfile(&path)?, &pw)?))
        }
        _ => Ok(Err(NoAccount::Locked)),
    }
}

fn identity_path() -> String {
    chat::key_file_path()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

pub fn account_required(unlock: Unlock<'_>) -> Result<AppOwner> {
    match unlock()? {
        Ok(k) => AppOwner::account(k),
        Err(NoAccount::Missing) => bail!(
            "no c0mpute identity on this machine. Create one with `c0mpute storage key init --backup-to <file>`, \
             or copy your existing one here (`{}`) to control the same apps you deploy elsewhere",
            identity_path()
        ),
        Err(NoAccount::Locked) => {
            bail!("set C0MPUTE_DM_PASSWORD to unlock your c0mpute identity (no terminal to ask on)")
        }
    }
}

fn short(owner: &str) -> &str {
    &owner[..12.min(owner.len())]
}

/// The account key, which must be the one that owns `rec`.
fn account_owning(rec: &AppRecord, unlock: Unlock<'_>) -> Result<AppOwner> {
    let a = account_required(unlock).with_context(|| {
        format!(
            "{} ({}) was deployed here under an account key ({}…)",
            rec.name,
            rec.app_id,
            short(&rec.owner)
        )
    })?;
    if !a.owns(rec) {
        bail!(
            "{} ({}) belongs to a different c0mpute identity ({}…) than the one on this machine ({}…)",
            rec.name,
            rec.app_id,
            short(&rec.owner),
            short(&a.owner_hex)
        );
    }
    Ok(a)
}

/// Pick the key `apps deploy` signs with. `existing` is the record this
/// machine last deployed under that name, if any. An app that is already
/// someone's keeps its owner (and so its id); it never silently becomes a
/// second app under another key.
pub fn choose(
    name: &str,
    choice: KeyChoice,
    moving: bool,
    existing: Option<&AppRecord>,
    machine: AppOwner,
    unlock: Unlock<'_>,
) -> Result<AppOwner> {
    let on_machine = existing.is_some_and(|r| machine.owns(r));
    if moving {
        let account = account_required(unlock)?;
        match existing {
            Some(r) if account.owns(r) => {
                bail!("--move: {name} is already owned by your account key")
            }
            Some(r) if !on_machine => bail!(
                "--move: {name} was deployed here under another key ({}…), not this machine's",
                short(&r.owner)
            ),
            _ => {}
        }
        return Ok(account);
    }
    match (choice, existing) {
        (KeyChoice::Machine, Some(r)) if !on_machine => bail!(
            "{name} is deployed here under an account key ({}…); --key machine would start a separate app with its own id",
            short(&r.owner)
        ),
        (KeyChoice::Machine, _) => Ok(machine),
        (KeyChoice::Account, Some(_)) if on_machine => bail!(
            "{name} is owned by this machine's key. Deploy it once with --move to hand it to your account key \
             (new app id: repoint its domains)"
        ),
        (KeyChoice::Account | KeyChoice::Auto, Some(r)) if !on_machine => account_owning(r, unlock),
        (KeyChoice::Account, _) => account_required(unlock),
        // Already deployed under this machine's key: keep its id.
        (KeyChoice::Auto, Some(_)) => {
            eprintln!(
                "note: {name} is owned by this machine's key, so only this machine can redeploy it, read its logs \
                 or delete it. Add --move once to hand it to your account key (new app id: repoint its domains)"
            );
            Ok(machine)
        }
        (KeyChoice::Auto, None) => match unlock()? {
            Ok(k) => AppOwner::account(k),
            Err(NoAccount::Missing) => {
                eprintln!(
                    "note: signing with this machine's key, so only this machine can control {name}. Run \
                     `c0mpute storage key init --backup-to <file>` to deploy with your account key from any machine"
                );
                Ok(machine)
            }
            Err(NoAccount::Locked) => {
                eprintln!(
                    "note: your c0mpute identity is locked (no terminal, no C0MPUTE_DM_PASSWORD): signing with \
                     this machine's key"
                );
                Ok(machine)
            }
        },
    }
}

/// Pick the key (and app id) an owner-only call on `name_or_id` is made
/// with. `existing` is the record this machine deployed under that name and
/// `legacy` the machine-owned one a `--move` left behind, if any.
pub fn resolve(
    name_or_id: &str,
    choice: KeyChoice,
    existing: Option<&AppRecord>,
    legacy: Option<&AppRecord>,
    machine: AppOwner,
    unlock: Unlock<'_>,
) -> Result<(AppOwner, String)> {
    let is_id = super::record::valid_app_id(name_or_id);
    let id_under = |o: &AppOwner| {
        if is_id {
            name_or_id.to_string()
        } else {
            o.app_id(name_or_id)
        }
    };
    let on_machine = existing.filter(|r| machine.owns(r));
    match choice {
        KeyChoice::Machine => {
            let id = match legacy.filter(|r| machine.owns(r)).or(on_machine) {
                Some(r) => r.app_id.clone(),
                None => id_under(&machine),
            };
            Ok((machine, id))
        }
        KeyChoice::Auto if on_machine.is_some() => {
            let id = on_machine.map(|r| r.app_id.clone()).unwrap_or_default();
            Ok((machine, id))
        }
        KeyChoice::Auto | KeyChoice::Account => match existing {
            Some(r) if !machine.owns(r) => Ok((account_owning(r, unlock)?, r.app_id.clone())),
            _ if choice == KeyChoice::Account => {
                let a = account_required(unlock)?;
                let id = id_under(&a);
                Ok((a, id))
            }
            // Never deployed from here: the account key if there is one.
            _ => match unlock()? {
                Ok(k) => {
                    let a = AppOwner::account(k)?;
                    let id = id_under(&a);
                    Ok((a, id))
                }
                Err(_) => {
                    let id = id_under(&machine);
                    Ok((machine, id))
                }
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::super::record::tests::spec;
    use super::*;

    fn machine() -> AppOwner {
        AppOwner::from_machine_key(Keypair::generate_ed25519()).unwrap()
    }

    fn account(seed: u8) -> AppOwner {
        AppOwner::account(ChatKey::from_seed([seed; 32])).unwrap()
    }

    fn unlocked() -> Result<std::result::Result<ChatKey, NoAccount>> {
        Ok(Ok(ChatKey::from_seed([7; 32])))
    }
    fn other_identity() -> Result<std::result::Result<ChatKey, NoAccount>> {
        Ok(Ok(ChatKey::from_seed([8; 32])))
    }
    fn missing() -> Result<std::result::Result<ChatKey, NoAccount>> {
        Ok(Err(NoAccount::Missing))
    }
    fn locked() -> Result<std::result::Result<ChatKey, NoAccount>> {
        Ok(Err(NoAccount::Locked))
    }
    fn never() -> Result<std::result::Result<ChatKey, NoAccount>> {
        panic!("the identity must not be asked for")
    }

    fn signed(o: &AppOwner, name: &str) -> AppRecord {
        AppRecord::sign(&o.keypair, spec(name), 1).unwrap()
    }

    #[test]
    fn the_account_owner_and_app_key_are_the_same_on_every_machine() {
        // Two machines holding the same identity: one owner, one app id, one
        // libp2p peer for owner calls, one app key per app name.
        let (a, b) = (account(7), account(7));
        assert_eq!(a.owner_hex, b.owner_hex);
        assert_eq!(a.app_id("api"), b.app_id("api"));
        assert_eq!(a.peer_id(), b.peer_id());
        assert_eq!(a.app_key("api"), b.app_key("api"));
        assert_eq!(a.app_key("api").unwrap().len(), 64);
        // Per app, per identity.
        assert_ne!(a.app_key("api"), a.app_key("api2"));
        let c = account(8);
        assert_ne!(a.owner_hex, c.owner_hex);
        assert_ne!(a.app_id("api"), c.app_id("api"));
        assert_ne!(a.app_key("api"), c.app_key("api"));
        // A machine owner has no derived key (its app keys are random, on disk).
        assert!(machine().app_key("api").is_none());
    }

    #[test]
    fn the_app_key_is_not_another_derived_key() {
        let key = ChatKey::from_seed([9; 32]);
        let app = derive_app_key(&key, "api");
        assert_ne!(*app, *key.derive_subkey(hosting::ACCOUNT_OWNER_LABEL));
        assert_ne!(*app, *key.derive_subkey(crate::sealed_store::STORAGE_LABEL));
        assert_ne!(*app, *key.derive_subkey(APP_KEY_LABEL));
        // The app's sites share the account key with hosting: one owner.
        assert_eq!(
            account(9).owner_hex,
            site::owner_hex(&hosting::account_keypair(&key).unwrap()).unwrap()
        );
    }

    #[test]
    fn holders_accept_owner_calls_from_the_account_peer_only() {
        // The check a holder makes for logs and sealed secrets is
        // `rec.owner_peer() == connecting peer`. A record signed by the
        // account key names the peer the CLI joins the network as.
        let a = account(7);
        let rec = signed(&a, "api");
        assert!(rec.verify());
        assert_eq!(rec.owner_peer().as_deref(), Some(a.peer_id().as_str()));
        assert_eq!(
            rec.owner_peer(),
            Some(account(7).peer_id()),
            "any machine with the identity"
        );
        let m = machine();
        assert_ne!(
            rec.owner_peer(),
            Some(m.peer_id()),
            "not the deploying machine's key"
        );
        assert_ne!(
            rec.owner_peer(),
            Some(account(8).peer_id()),
            "not another identity"
        );
        // And a machine-owned (legacy) record still names the machine's peer.
        assert_eq!(signed(&m, "api").owner_peer(), Some(m.peer_id()));
    }

    #[test]
    fn a_new_app_is_signed_by_the_account_key() {
        let o = choose("api", KeyChoice::Auto, false, None, machine(), &unlocked).unwrap();
        assert_eq!(o.kind, OwnerKind::Account);
        assert_eq!(o.owner_hex, account(7).owner_hex);
        // No identity (or a locked one): the machine key, as before.
        let m = machine();
        let hex = m.owner_hex.clone();
        let o = choose("api", KeyChoice::Auto, false, None, m, &missing).unwrap();
        assert_eq!((o.kind, o.owner_hex), (OwnerKind::Machine, hex));
        assert_eq!(
            choose("api", KeyChoice::Auto, false, None, machine(), &locked)
                .unwrap()
                .kind,
            OwnerKind::Machine
        );
        // Asked for by name, it is required.
        assert!(choose("api", KeyChoice::Account, false, None, machine(), &missing).is_err());
        assert!(choose("api", KeyChoice::Account, false, None, machine(), &locked).is_err());
        assert_eq!(
            choose("api", KeyChoice::Machine, false, None, machine(), &never)
                .unwrap()
                .kind,
            OwnerKind::Machine
        );
    }

    #[test]
    fn a_machine_owned_app_keeps_its_id_until_it_is_moved() {
        let m = machine();
        let rec = signed(&m, "api");
        let again = || AppOwner::from_machine_key(m.keypair.clone()).unwrap();
        // Redeploying needs no identity and signs the same id.
        let o = choose("api", KeyChoice::Auto, false, Some(&rec), again(), &never).unwrap();
        assert_eq!(o.kind, OwnerKind::Machine);
        assert_eq!(o.app_id("api"), rec.app_id);
        assert_eq!(
            AppRecord::sign(&o.keypair, spec("api"), 2).unwrap().app_id,
            rec.app_id
        );
        // --key account does not quietly fork it.
        let e = choose(
            "api",
            KeyChoice::Account,
            false,
            Some(&rec),
            again(),
            &unlocked,
        )
        .err()
        .unwrap();
        assert!(e.to_string().contains("--move"), "{e}");
        // Its owner calls still come from the machine's peer.
        let (o, id) = resolve("api", KeyChoice::Auto, Some(&rec), None, again(), &never).unwrap();
        assert_eq!(
            (o.kind, id.as_str()),
            (OwnerKind::Machine, rec.app_id.as_str())
        );
        assert_eq!(rec.owner_peer(), Some(o.peer_id()));
    }

    #[test]
    fn move_hands_the_app_to_the_account_key_under_a_new_id() {
        let m = machine();
        let rec = signed(&m, "api");
        let again = || AppOwner::from_machine_key(m.keypair.clone()).unwrap();
        let o = choose("api", KeyChoice::Auto, true, Some(&rec), again(), &unlocked).unwrap();
        assert_eq!(o.kind, OwnerKind::Account);
        let moved = signed(&o, "api");
        assert!(moved.verify());
        assert_ne!(moved.app_id, rec.app_id, "the id follows the owner");
        assert_eq!(moved.app_id, account(7).app_id("api"));
        // It needs the identity, and happens once.
        assert!(choose("api", KeyChoice::Auto, true, Some(&rec), again(), &locked).is_err());
        assert!(
            choose(
                "api",
                KeyChoice::Auto,
                true,
                Some(&moved),
                again(),
                &unlocked
            )
            .is_err()
        );
        // Nothing deployed from here yet: --move is just an account deploy.
        assert_eq!(
            choose("api", KeyChoice::Auto, true, None, again(), &unlocked)
                .unwrap()
                .kind,
            OwnerKind::Account
        );
        // After the move the account key is used; the old app is reachable
        // with --key machine until it is destroyed.
        let (o, id) = resolve(
            "api",
            KeyChoice::Auto,
            Some(&moved),
            Some(&rec),
            again(),
            &unlocked,
        )
        .unwrap();
        assert_eq!((o.kind, id), (OwnerKind::Account, moved.app_id.clone()));
        let (o, id) = resolve(
            "api",
            KeyChoice::Machine,
            Some(&moved),
            Some(&rec),
            again(),
            &never,
        )
        .unwrap();
        assert_eq!((o.kind, id), (OwnerKind::Machine, rec.app_id.clone()));
    }

    #[test]
    fn an_account_owned_app_is_never_forked_by_another_key() {
        let rec = signed(&account(7), "api");
        // The same identity: fine, from any machine.
        let o = choose(
            "api",
            KeyChoice::Auto,
            false,
            Some(&rec),
            machine(),
            &unlocked,
        )
        .unwrap();
        assert_eq!(o.app_id("api"), rec.app_id);
        // A different identity, none, or a locked one: refused, not forked.
        assert!(
            choose(
                "api",
                KeyChoice::Auto,
                false,
                Some(&rec),
                machine(),
                &other_identity
            )
            .is_err()
        );
        assert!(
            choose(
                "api",
                KeyChoice::Auto,
                false,
                Some(&rec),
                machine(),
                &missing
            )
            .is_err()
        );
        assert!(
            choose(
                "api",
                KeyChoice::Auto,
                false,
                Some(&rec),
                machine(),
                &locked
            )
            .is_err()
        );
        assert!(
            choose(
                "api",
                KeyChoice::Machine,
                false,
                Some(&rec),
                machine(),
                &never
            )
            .is_err()
        );
        assert!(
            resolve(
                "api",
                KeyChoice::Auto,
                Some(&rec),
                None,
                machine(),
                &other_identity
            )
            .is_err()
        );
    }

    #[test]
    fn owner_calls_work_from_a_machine_that_never_deployed_the_app() {
        // A second machine with the identity: the name alone gives the id.
        let (o, id) = resolve("api", KeyChoice::Auto, None, None, machine(), &unlocked).unwrap();
        assert_eq!(o.kind, OwnerKind::Account);
        assert_eq!(id, account(7).app_id("api"));
        assert_eq!(signed(&account(7), "api").owner_peer(), Some(o.peer_id()));
        // An app id is passed through.
        let raw = "0123456789abcdef0123";
        assert_eq!(
            resolve(raw, KeyChoice::Auto, None, None, machine(), &unlocked)
                .unwrap()
                .1,
            raw
        );
        assert_eq!(
            resolve(raw, KeyChoice::Machine, None, None, machine(), &never)
                .unwrap()
                .1,
            raw
        );
        // No identity: this machine's key, as before.
        let (o, _) = resolve("api", KeyChoice::Auto, None, None, machine(), &missing).unwrap();
        assert_eq!(o.kind, OwnerKind::Machine);
        assert!(resolve("api", KeyChoice::Account, None, None, machine(), &missing).is_err());
    }
}
