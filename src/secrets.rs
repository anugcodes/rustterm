//! Optional password storage in the operating system's keyring.
//!
//! Passwords are keyed by the **profile name** under the service `rustterm`.
//! On Linux that is the Secret Service (GNOME Keyring / KWallet), so entries
//! show up in tools such as Seahorse as `rustterm` / `<profile name>`.
//!
//! Every operation returns a human-readable error instead of panicking: a
//! missing or locked keyring must never prevent the app from connecting with a
//! typed password.
//!
//! Builds with `--no-default-features` leave out the `keyring-secrets` feature
//! (and therefore the zbus/Secret Service dependency stack). The public API
//! stays the same: `save`/`load` report that keyring support was not compiled
//! in, and `delete` is a no-op.

#[cfg(feature = "keyring-secrets")]
pub use enabled::{delete, load, save};

#[cfg(not(feature = "keyring-secrets"))]
pub use disabled::{delete, load, save};

#[cfg(feature = "keyring-secrets")]
mod enabled {
    use keyring::Entry;

    const SERVICE: &str = "rustterm";

    fn entry(profile: &str) -> Result<Entry, String> {
        Entry::new(SERVICE, profile).map_err(|err| format!("system keyring is unavailable ({err})"))
    }

    /// Store or replace the password for `profile`.
    pub fn save(profile: &str, password: &str) -> Result<(), String> {
        entry(profile)?
            .set_password(password)
            .map_err(|err| format!("{err}"))
    }

    /// Read the password for `profile`.
    ///
    /// Returns `Ok(None)` when no entry exists, so a stale `save_password = true`
    /// flag can be recovered from without an error dialog.
    pub fn load(profile: &str) -> Result<Option<String>, String> {
        match entry(profile)?.get_password() {
            Ok(password) => Ok(Some(password)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(format!("{err}")),
        }
    }

    /// Delete the password for `profile`. Missing entries are not an error.
    pub fn delete(profile: &str) -> Result<(), String> {
        match entry(profile)?.delete_credential() {
            Ok(()) => Ok(()),
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(format!("{err}")),
        }
    }
}

#[cfg(not(feature = "keyring-secrets"))]
mod disabled {
    const MESSAGE: &str =
        "this build has no keyring support (rebuild with the `keyring-secrets` feature)";

    pub fn save(_profile: &str, _password: &str) -> Result<(), String> {
        Err(MESSAGE.to_string())
    }

    pub fn load(_profile: &str) -> Result<Option<String>, String> {
        Err(MESSAGE.to_string())
    }

    /// Nothing can be stored in this build, so there is nothing to delete.
    pub fn delete(_profile: &str) -> Result<(), String> {
        Ok(())
    }
}
