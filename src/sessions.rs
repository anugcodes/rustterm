//! Saved connection profiles.
//!
//! Profiles live in `$XDG_CONFIG_HOME/rustterm/sessions.toml` (falling back to
//! `~/.config/rustterm/sessions.toml`). Only connection metadata is stored:
//! **passwords are never written to disk**. Keyring-backed password storage is
//! still tracked as F19 in the handoff.
//!
//! Loading is deliberately forgiving: a missing file is an empty list, and a
//! malformed file reports an error without preventing the app from starting.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// File name inside the `rustterm` config directory.
pub const FILE_NAME: &str = "sessions.toml";

/// One saved SSH endpoint. The password is never part of a profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionProfile {
    /// Human-readable name. Unique: saving with an existing name replaces it.
    pub name: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub user: String,
    /// Whether the password is stored in the OS keyring under this profile
    /// name. Always `false` for profiles written before keyring support; the
    /// password itself is never part of the TOML file.
    #[serde(default, skip_serializing_if = "is_false")]
    pub save_password: bool,
    /// Try public-key authentication before falling back to a password.
    #[serde(default, skip_serializing_if = "is_false")]
    pub use_key: bool,
    /// Private-key file for `use_key`. Empty means "auto-detect the usual
    /// `~/.ssh/id_*` files". `~` is expanded when connecting.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key_path: String,
    /// Offer identities from `$SSH_AUTH_SOCK` (ssh-agent).
    #[serde(default, skip_serializing_if = "is_false")]
    pub use_agent: bool,
}

fn default_port() -> u16 {
    22
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl SessionProfile {
    /// Validate user-supplied fields before they are written to disk.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.name.trim().is_empty() {
            if self.save_password {
                return Err("Profile name is required when saving the password");
            }
            return Err("Profile name is required");
        }
        if self.host.trim().is_empty() {
            return Err("Host is required");
        }
        if self.user.trim().is_empty() {
            return Err("User is required");
        }
        if self.port == 0 {
            return Err("Port must be between 1 and 65535");
        }
        Ok(())
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SessionsFile {
    #[serde(default)]
    sessions: Vec<SessionProfile>,
}

/// In-memory list plus its backing file.
#[derive(Debug)]
pub struct SessionStore {
    path: PathBuf,
    profiles: Vec<SessionProfile>,
    /// Read/parse problem to surface in the UI; `None` when all is well.
    error: Option<String>,
}

impl SessionStore {
    /// Load the platform default file. Never fails: problems are reported via
    /// [`SessionStore::error`].
    pub fn load_default() -> Self {
        match default_path() {
            Some(path) => Self::load(path),
            None => Self {
                path: PathBuf::new(),
                profiles: Vec::new(),
                error: Some(
                    "Cannot locate a config directory (set HOME or XDG_CONFIG_HOME)".to_string(),
                ),
            },
        }
    }

    /// Load from an explicit path. Used by tests and by `load_default`.
    pub fn load(path: PathBuf) -> Self {
        let mut store = Self {
            path,
            profiles: Vec::new(),
            error: None,
        };

        match fs::read_to_string(&store.path) {
            Ok(text) => match toml::from_str::<SessionsFile>(&text) {
                Ok(file) => store.profiles = file.sessions,
                Err(err) => {
                    store.error = Some(format!("Could not parse {}: {err}", store.path.display()));
                }
            },
            // First run: no file is not an error.
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                store.error = Some(format!("Could not read {}: {err}", store.path.display()));
            }
        }

        store
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn profiles(&self) -> &[SessionProfile] {
        &self.profiles
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Insert a profile, replacing any profile with the same name.
    ///
    /// Returns the index of the stored profile. The file is written first; on
    /// failure the in-memory list is left untouched.
    pub fn upsert(&mut self, profile: SessionProfile) -> io::Result<usize> {
        let mut next = self.profiles.clone();
        let index = match next.iter().position(|p| p.name == profile.name) {
            Some(i) => {
                next[i] = profile;
                i
            }
            None => {
                next.push(profile);
                next.len() - 1
            }
        };

        write_profiles(&self.path, &next)?;
        self.profiles = next;
        self.error = None;
        Ok(index)
    }

    /// Remove the profile at `index`. Returns the removed profile, if any.
    pub fn remove(&mut self, index: usize) -> io::Result<Option<SessionProfile>> {
        if index >= self.profiles.len() {
            return Ok(None);
        }

        let mut next = self.profiles.clone();
        let removed = next.remove(index);
        write_profiles(&self.path, &next)?;
        self.profiles = next;
        self.error = None;
        Ok(Some(removed))
    }
}

fn default_path() -> Option<PathBuf> {
    let config_dir = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".config"))
        })?;

    Some(config_dir.join("rustterm").join(FILE_NAME))
}

fn write_profiles(path: &Path, profiles: &[SessionProfile]) -> io::Result<()> {
    if path.as_os_str().is_empty() {
        return Err(io::Error::other("no config directory available"));
    }

    let file = SessionsFile {
        sessions: profiles.to_vec(),
    };
    let body = toml::to_string_pretty(&file).map_err(io::Error::other)?;

    let mut text = String::from(
        "# rustterm saved sessions.\n\
         # Passwords are never stored here.\n\n",
    );
    text.push_str(&body);

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Write next to the target, then rename, so a crash cannot leave a
    // half-written profile file. `rename` is atomic within one filesystem.
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, text.as_bytes())?;
    if let Err(err) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

    /// Unique, self-cleaning temporary directory (no external crate needed).
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "rustterm-sessions-test-{}-{id}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }

        fn file(&self) -> PathBuf {
            self.0.join(FILE_NAME)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn profile(name: &str, host: &str) -> SessionProfile {
        SessionProfile {
            name: name.to_string(),
            host: host.to_string(),
            port: 22,
            user: "anurag".to_string(),
            save_password: false,
            use_key: false,
            key_path: String::new(),
            use_agent: false,
        }
    }

    #[test]
    fn missing_file_loads_as_empty_without_error() {
        let dir = TempDir::new();
        let store = SessionStore::load(dir.file());

        assert!(store.profiles().is_empty());
        assert!(store.error().is_none());
    }

    #[test]
    fn save_then_load_round_trips_profiles() {
        let dir = TempDir::new();
        let mut store = SessionStore::load(dir.file());

        store
            .upsert(SessionProfile {
                name: "legacy box".to_string(),
                host: "172.31.251.23".to_string(),
                port: 22,
                user: "anuragb".to_string(),
                save_password: false,
                use_key: false,
                key_path: String::new(),
                use_agent: false,
            })
            .unwrap();
        store.upsert(profile("local", "localhost")).unwrap();

        let reloaded = SessionStore::load(dir.file());
        assert_eq!(reloaded.profiles(), store.profiles());
        assert_eq!(reloaded.profiles().len(), 2);
        assert_eq!(reloaded.profiles()[0].name, "legacy box");
        assert!(reloaded.error().is_none());
    }

    #[test]
    fn upsert_replaces_a_profile_with_the_same_name() {
        let dir = TempDir::new();
        let mut store = SessionStore::load(dir.file());

        store.upsert(profile("work", "old.example")).unwrap();
        let index = store
            .upsert(SessionProfile {
                host: "new.example".to_string(),
                ..profile("work", "ignored")
            })
            .unwrap();

        assert_eq!(index, 0);
        assert_eq!(store.profiles().len(), 1);
        assert_eq!(store.profiles()[0].host, "new.example");

        let reloaded = SessionStore::load(dir.file());
        assert_eq!(reloaded.profiles()[0].host, "new.example");
    }

    #[test]
    fn written_file_is_documented_toml_without_passwords() {
        let dir = TempDir::new();
        let mut store = SessionStore::load(dir.file());
        store.upsert(profile("box", "example.com")).unwrap();

        let text = fs::read_to_string(dir.file()).unwrap();
        assert!(text.starts_with("# rustterm saved sessions."));
        assert!(text.contains("[[sessions]]"));
        assert!(text.contains("name = \"box\""));
        // No standalone password field; only `save_password` may appear.
        assert!(!text.lines().any(|line| line
            .trim_start()
            .to_ascii_lowercase()
            .starts_with("password")));
    }

    #[test]
    fn remove_deletes_and_persists() {
        let dir = TempDir::new();
        let mut store = SessionStore::load(dir.file());
        store.upsert(profile("a", "a.example")).unwrap();
        store.upsert(profile("b", "b.example")).unwrap();

        let removed = store.remove(0).unwrap().expect("profile exists");
        assert_eq!(removed.name, "a");
        assert_eq!(store.profiles().len(), 1);

        let reloaded = SessionStore::load(dir.file());
        assert_eq!(reloaded.profiles().len(), 1);
        assert_eq!(reloaded.profiles()[0].name, "b");

        assert!(store.remove(99).unwrap().is_none());
    }

    #[test]
    fn malformed_file_reports_error_but_keeps_running() {
        let dir = TempDir::new();
        fs::write(dir.file(), "this is not toml = = =").unwrap();

        let store = SessionStore::load(dir.file());
        assert!(store.profiles().is_empty());
        assert!(store.error().is_some());
    }

    #[test]
    fn profile_without_port_defaults_to_22() {
        let dir = TempDir::new();
        fs::write(
            dir.file(),
            "[[sessions]]\nname = \"x\"\nhost = \"h\"\nuser = \"u\"\n",
        )
        .unwrap();

        let store = SessionStore::load(dir.file());
        assert_eq!(store.profiles()[0].port, 22);
        assert!(store.error().is_none());
    }

    #[test]
    fn save_password_flag_round_trips_but_never_writes_the_secret() {
        let dir = TempDir::new();
        let mut store = SessionStore::load(dir.file());
        let mut with_secret = profile("box", "example.com");
        with_secret.save_password = true;
        store.upsert(with_secret).unwrap();

        let text = fs::read_to_string(dir.file()).unwrap();
        assert!(text.contains("save_password = true"));
        assert!(!text.lines().any(|line| line
            .trim_start()
            .to_ascii_lowercase()
            .starts_with("password")));

        let reloaded = SessionStore::load(dir.file());
        assert!(reloaded.profiles()[0].save_password);
    }

    #[test]
    fn profiles_saved_without_the_flag_default_to_no_password() {
        let dir = TempDir::new();
        fs::write(
            dir.file(),
            "[[sessions]]\nname = \"old\"\nhost = \"h\"\nuser = \"u\"\n",
        )
        .unwrap();

        let mut store = SessionStore::load(dir.file());
        assert!(store.error().is_none());
        assert!(!store.profiles()[0].save_password);
        // Key auth is opt-in too.
        assert!(!store.profiles()[0].use_key);
        assert!(store.profiles()[0].key_path.is_empty());
        assert!(!store.profiles()[0].use_agent);

        // `false` is omitted when re-saved, keeping old files tidy.
        store.upsert(store.profiles()[0].clone()).unwrap();
        assert!(!fs::read_to_string(dir.file())
            .unwrap()
            .contains("save_password"));
    }

    #[test]
    fn key_auth_settings_round_trip() {
        let dir = TempDir::new();
        let mut store = SessionStore::load(dir.file());
        let mut keyed = profile("keyed", "example.com");
        keyed.use_key = true;
        keyed.key_path = "~/.ssh/id_ed25519".to_string();
        keyed.use_agent = true;
        store.upsert(keyed).unwrap();

        let text = fs::read_to_string(dir.file()).unwrap();
        assert!(text.contains("use_key = true"));
        assert!(text.contains("key_path = \"~/.ssh/id_ed25519\""));
        assert!(text.contains("use_agent = true"));

        let reloaded = SessionStore::load(dir.file());
        let profile = &reloaded.profiles()[0];
        assert!(profile.use_key);
        assert_eq!(profile.key_path, "~/.ssh/id_ed25519");
        assert!(profile.use_agent);
    }

    #[test]
    fn blank_key_path_is_omitted_from_the_file() {
        let dir = TempDir::new();
        let mut store = SessionStore::load(dir.file());
        let mut keyed = profile("keyed", "example.com");
        keyed.use_key = true;
        store.upsert(keyed).unwrap();

        let text = fs::read_to_string(dir.file()).unwrap();
        assert!(text.contains("use_key = true"));
        assert!(!text.contains("key_path"));
        assert!(!text.contains("use_agent"));
    }

    #[test]
    fn validate_rejects_missing_fields_and_port_zero() {
        assert!(profile("name", "host").validate().is_ok());

        let mut bad = profile("", "host");
        assert_eq!(bad.validate(), Err("Profile name is required"));

        // Storing a password makes an explicit name mandatory.
        bad.save_password = true;
        assert_eq!(
            bad.validate(),
            Err("Profile name is required when saving the password")
        );

        bad = profile("name", "");
        assert_eq!(bad.validate(), Err("Host is required"));

        bad = profile("name", "host");
        bad.user.clear();
        assert_eq!(bad.validate(), Err("User is required"));

        bad = profile("name", "host");
        bad.port = 0;
        assert_eq!(bad.validate(), Err("Port must be between 1 and 65535"));
    }
}
