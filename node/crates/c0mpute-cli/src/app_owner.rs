//! `--key` for `c0mpute apps ...` and how the CLI opens the c0mpute identity
//! an app's account key is derived from (see `c0mpute_core::apps::owner`).

use std::io::IsTerminal;

use anyhow::Result;
use c0mpute_core::apps::owner::{self, NoAccount};
use c0mpute_secure_chat as chat;

/// `--key` on `apps deploy` / `logs` / `destroy`.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum KeyArg {
    /// The app's existing owner if it was deployed from here, else your account key.
    #[default]
    Auto,
    /// Your account key (from your c0mpute identity): same app from any machine.
    Account,
    /// This machine's key (how apps were owned before the account key).
    Machine,
}

impl From<KeyArg> for owner::KeyChoice {
    fn from(k: KeyArg) -> Self {
        match k {
            KeyArg::Auto => owner::KeyChoice::Auto,
            KeyArg::Account => owner::KeyChoice::Account,
            KeyArg::Machine => owner::KeyChoice::Machine,
        }
    }
}

/// `auto` / `account` / `machine` from an agent's tool call (anything else is `auto`).
pub fn key_from_str(s: Option<&str>) -> owner::KeyChoice {
    match s {
        Some("account") => owner::KeyChoice::Account,
        Some("machine") => owner::KeyChoice::Machine,
        _ => owner::KeyChoice::Auto,
    }
}

/// Open the c0mpute identity: `C0MPUTE_DM_PASSWORD`, else a prompt when
/// there is a terminal to ask on.
pub fn unlock() -> Result<std::result::Result<chat::ChatKey, NoAccount>> {
    match owner::unlock_from_env()? {
        Err(NoAccount::Locked) if std::io::stdin().is_terminal() => {
            let pw = zeroize::Zeroizing::new(rpassword::prompt_password(
                "c0mpute identity password (signs your apps): ",
            )?);
            let path = chat::key_file_path()?;
            Ok(Ok(chat::decrypt_keyfile(&chat::load_keyfile(&path)?, &pw)?))
        }
        other => Ok(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_arguments_map_to_the_core_choice() {
        assert_eq!(
            owner::KeyChoice::from(KeyArg::default()),
            owner::KeyChoice::Auto
        );
        assert_eq!(
            owner::KeyChoice::from(KeyArg::Account),
            owner::KeyChoice::Account
        );
        assert_eq!(
            owner::KeyChoice::from(KeyArg::Machine),
            owner::KeyChoice::Machine
        );
        assert_eq!(key_from_str(Some("account")), owner::KeyChoice::Account);
        assert_eq!(key_from_str(Some("machine")), owner::KeyChoice::Machine);
        assert_eq!(key_from_str(Some("nonsense")), owner::KeyChoice::Auto);
        assert_eq!(key_from_str(None), owner::KeyChoice::Auto);
    }
}
