//! Small persisted UI preferences.
//!
//! Settings live next to the saved sessions in
//! `$XDG_CONFIG_HOME/rustterm/settings.toml` (falling back to
//! `~/.config/rustterm/settings.toml`). Loading is forgiving: a missing or
//! malformed file simply means "use the defaults".

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::theme::ThemeId;

pub const FILE_NAME: &str = "settings.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub theme: ThemeId,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeId::default(),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SettingsFile {
    #[serde(default)]
    theme: Option<String>,
}

impl Settings {
    pub fn load_default() -> Self {
        default_path()
            .and_then(|path| Self::load_from(&path).ok())
            .unwrap_or_default()
    }

    pub fn load_from(path: &Path) -> Result<Self, String> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => return Err(format!("could not read '{}': {err}", path.display())),
        };
        let file: SettingsFile = toml::from_str(&text)
            .map_err(|err| format!("could not parse '{}': {err}", path.display()))?;
        let theme = file
            .theme
            .as_deref()
            .map(ThemeId::from_id)
            .unwrap_or_default();
        Ok(Self { theme })
    }

    pub fn save_default(&self) -> Result<(), String> {
        let path = default_path().ok_or_else(|| {
            "cannot locate a config directory (set HOME or XDG_CONFIG_HOME)".to_string()
        })?;
        self.save_to(&path)
    }

    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        let file = SettingsFile {
            theme: Some(self.theme.id().to_string()),
        };
        let body = toml::to_string_pretty(&file)
            .map_err(|err| format!("could not encode settings: {err}"))?;
        let text = format!("# rustterm settings.\n\n{body}");

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("could not create '{}': {err}", parent.display()))?;
        }
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, text.as_bytes())
            .map_err(|err| format!("could not write '{}': {err}", tmp.display()))?;
        if let Err(err) = fs::rename(&tmp, path) {
            let _ = fs::remove_file(&tmp);
            return Err(format!("could not save '{}': {err}", path.display()));
        }
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

    fn temp_file() -> PathBuf {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "rustterm-settings-{}-{stamp}-{id}.toml",
            std::process::id()
        ))
    }

    #[test]
    fn round_trips_the_selected_theme() {
        let path = temp_file();
        let settings = Settings {
            theme: ThemeId::Graphite,
        };
        settings.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path).unwrap(), settings);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn missing_and_unknown_theme_fall_back_to_default() {
        let missing = temp_file();
        assert_eq!(Settings::load_from(&missing).unwrap(), Settings::default());

        let path = temp_file();
        std::fs::write(&path, "theme = \"not-a-theme\"\n").unwrap();
        assert_eq!(Settings::load_from(&path).unwrap().theme, ThemeId::Midnight);
        let _ = std::fs::remove_file(path);
    }
}
