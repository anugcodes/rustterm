//! Small persisted UI preferences.
//!
//! Settings live next to the saved sessions in
//! `$XDG_CONFIG_HOME/rustterm/settings.toml` (falling back to
//! `~/.config/rustterm/settings.toml`). Loading is forgiving: a missing or
//! malformed file simply means "use the defaults", and fields added by newer
//! versions fall back one by one when an older file is read.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::theme::ThemeId;

pub const FILE_NAME: &str = "settings.toml";

/// Default port for the VNC viewer, matching the usual `:0` display.
pub const DEFAULT_VNC_PORT: u16 = 5900;

/// Which remote-display path to take when both are enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DisplayPref {
    /// Ask each time a session is used.
    #[default]
    Ask,
    /// Start X11 forwarding when the server allows it.
    X11,
    /// Open the VNC tunnel when the server has a viewer.
    Vnc,
}

impl DisplayPref {
    pub const ALL: [DisplayPref; 3] = [DisplayPref::Ask, DisplayPref::X11, DisplayPref::Vnc];

    pub fn id(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::X11 => "x11",
            Self::Vnc => "vnc",
        }
    }

    pub fn from_id(value: &str) -> Self {
        match value {
            "x11" => Self::X11,
            "vnc" => Self::Vnc,
            _ => Self::Ask,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Ask => "Ask every time",
            Self::X11 => "Prefer X11",
            Self::Vnc => "Prefer VNC",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Self::Ask => "Both enabled: choose at use time.",
            Self::X11 => "Use X11 when the server supports it; fall back to VNC.",
            Self::Vnc => "Use VNC when available; fall back to X11.",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub theme: ThemeId,
    /// Show the browser-style Back/Forward buttons in the file panel.
    pub sftp_history: bool,
    /// Offer X11 forwarding (off by default).
    pub x11: bool,
    /// Offer the VNC-over-SSH tunnel (off by default).
    pub vnc: bool,
    /// What to do when both remote-display options are enabled.
    pub display_pref: DisplayPref,
    /// VNC server address used when it cannot be detected in the shell.
    pub vnc_host: String,
    /// VNC server port (5900 = display :0).
    pub vnc_port: u16,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeId::default(),
            sftp_history: false,
            x11: false,
            vnc: false,
            display_pref: DisplayPref::default(),
            vnc_host: String::new(),
            vnc_port: DEFAULT_VNC_PORT,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SettingsFile {
    #[serde(default)]
    theme: Option<String>,
    #[serde(default)]
    sftp_history: Option<bool>,
    #[serde(default)]
    x11: Option<bool>,
    #[serde(default)]
    vnc: Option<bool>,
    #[serde(default)]
    display_pref: Option<String>,
    #[serde(default)]
    vnc_host: Option<String>,
    #[serde(default)]
    vnc_port: Option<u16>,
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
        let defaults = Self::default();
        let theme = file
            .theme
            .as_deref()
            .map(ThemeId::from_id)
            .unwrap_or(defaults.theme);
        Ok(Self {
            theme,
            sftp_history: file.sftp_history.unwrap_or(defaults.sftp_history),
            x11: file.x11.unwrap_or(defaults.x11),
            vnc: file.vnc.unwrap_or(defaults.vnc),
            display_pref: file
                .display_pref
                .as_deref()
                .map(DisplayPref::from_id)
                .unwrap_or(defaults.display_pref),
            vnc_host: file.vnc_host.unwrap_or(defaults.vnc_host),
            vnc_port: file
                .vnc_port
                .filter(|port| *port > 0)
                .unwrap_or(defaults.vnc_port),
        })
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
            sftp_history: Some(self.sftp_history),
            x11: Some(self.x11),
            vnc: Some(self.vnc),
            display_pref: Some(self.display_pref.id().to_string()),
            vnc_host: Some(self.vnc_host.clone()),
            vnc_port: Some(self.vnc_port),
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
    fn round_trips_every_setting() {
        let path = temp_file();
        let settings = Settings {
            theme: ThemeId::Graphite,
            sftp_history: true,
            x11: true,
            vnc: true,
            display_pref: DisplayPref::Vnc,
            vnc_host: "10.0.0.5".into(),
            vnc_port: 5902,
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

    #[test]
    fn old_file_keeps_defaults_for_new_fields() {
        let path = temp_file();
        std::fs::write(&path, "theme = \"graphite\"\n").unwrap();
        let settings = Settings::load_from(&path).unwrap();
        assert_eq!(settings.theme, ThemeId::Graphite);
        assert!(!settings.sftp_history);
        assert!(!settings.x11);
        assert!(!settings.vnc);
        assert_eq!(settings.display_pref, DisplayPref::Ask);
        assert!(settings.vnc_host.is_empty());
        assert_eq!(settings.vnc_port, DEFAULT_VNC_PORT);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn zero_vnc_port_falls_back_to_the_default() {
        let path = temp_file();
        std::fs::write(&path, "vnc_port = 0\n").unwrap();
        assert_eq!(
            Settings::load_from(&path).unwrap().vnc_port,
            DEFAULT_VNC_PORT
        );
        let _ = std::fs::remove_file(path);
    }
}
