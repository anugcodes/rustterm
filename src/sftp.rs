//! State for the SFTP side panel.
//!
//! The panel is per session: each SSH tab owns an `SftpExplorer`, so switching
//! tabs shows that session's remote directory. All network work stays in
//! `ssh.rs`; this module only tracks the current path, the last listing and the
//! in-flight request.

use std::sync::mpsc as std_mpsc;
use std::time::SystemTime;

use crate::ssh::{RemoteEntry, SshSession};

/// State for the SFTP side panel.
#[derive(Debug, Default)]
pub struct SftpExplorer {
    /// Directory currently shown (remote, POSIX path).
    pub current: String,
    /// Directory that produced `entries`; differs from `current` while a new
    /// listing is in flight, so stale rows still resolve against the right path.
    pub listed_path: String,
    /// Editable copy shown in the path box.
    pub path_edit: String,
    pub entries: Vec<RemoteEntry>,
    pub selected: Option<usize>,
    pub loading: bool,
    pub error: Option<String>,
    /// Set when the server has no SFTP subsystem; disables the controls.
    pub unavailable: Option<String>,
    pending: Option<Job>,
}

#[derive(Debug)]
enum Job {
    Home(std_mpsc::Receiver<Result<String, String>>),
    List {
        path: String,
        rx: std_mpsc::Receiver<Result<Vec<RemoteEntry>, String>>,
    },
}

impl SftpExplorer {
    /// Create the panel state and start resolving the remote home directory.
    pub fn start(ssh: &SshSession) -> Self {
        let mut explorer = Self::default();
        if let Some(reason) = ssh.sftp_unavailable() {
            explorer.unavailable = Some(reason.to_string());
            return explorer;
        }
        explorer.load_home(ssh);
        explorer
    }

    /// Check the in-flight request. Returns `true` when the UI should redraw.
    pub fn poll(&mut self, ssh: &SshSession) -> bool {
        let Some(job) = self.pending.take() else {
            return false;
        };

        match job {
            Job::Home(rx) => match rx.try_recv() {
                Ok(Ok(home)) => {
                    self.current = home.clone();
                    self.path_edit = home.clone();
                    self.start_list(ssh, home);
                    true
                }
                Ok(Err(err)) => {
                    self.fail(err);
                    true
                }
                Err(std_mpsc::TryRecvError::Empty) => {
                    self.pending = Some(Job::Home(rx));
                    false
                }
                Err(std_mpsc::TryRecvError::Disconnected) => {
                    self.fail("the SFTP session closed while resolving the home directory");
                    true
                }
            },
            Job::List { path, rx } => match rx.try_recv() {
                Ok(Ok(mut entries)) => {
                    sort_entries(&mut entries);
                    self.entries = entries;
                    self.listed_path = path;
                    self.selected = None;
                    self.loading = false;
                    self.error = None;
                    true
                }
                Ok(Err(err)) => {
                    self.fail(err);
                    true
                }
                Err(std_mpsc::TryRecvError::Empty) => {
                    self.pending = Some(Job::List { path, rx });
                    false
                }
                Err(std_mpsc::TryRecvError::Disconnected) => {
                    self.fail("the SFTP session closed while listing the directory");
                    true
                }
            },
        }
    }

    /// Resolve `.` again (usually the login home directory).
    pub fn load_home(&mut self, ssh: &SshSession) {
        self.error = None;
        if let Some(rx) = ssh.sftp_home() {
            self.loading = true;
            self.pending = Some(Job::Home(rx));
        } else {
            self.mark_unavailable(ssh);
        }
    }

    /// Show `path`, replacing the current listing once the reply arrives.
    pub fn navigate(&mut self, ssh: &SshSession, path: impl Into<String>) {
        let path = path.into();
        let path = path.trim();
        if path.is_empty() {
            self.error = Some("Enter a remote path".to_string());
            return;
        }
        self.start_list(ssh, path.to_string());
    }

    /// Re-list the current directory.
    pub fn refresh(&mut self, ssh: &SshSession) {
        let path = if self.current.is_empty() {
            ".".to_string()
        } else {
            self.current.clone()
        };
        self.navigate(ssh, path);
    }

    /// Go to the parent of `current`.
    pub fn up(&mut self, ssh: &SshSession) {
        if self.current.is_empty() {
            return;
        }
        let parent = parent_path(&self.current);
        self.navigate(ssh, parent);
    }

    pub fn selected_entry(&self) -> Option<&RemoteEntry> {
        self.selected.and_then(|index| self.entries.get(index))
    }

    fn start_list(&mut self, ssh: &SshSession, path: String) {
        self.current = path.clone();
        self.path_edit = path.clone();
        self.error = None;

        if let Some(rx) = ssh.sftp_list(&path) {
            self.loading = true;
            self.pending = Some(Job::List { path, rx });
        } else {
            self.mark_unavailable(ssh);
        }
    }

    fn mark_unavailable(&mut self, ssh: &SshSession) {
        self.loading = false;
        self.unavailable = ssh
            .sftp_unavailable()
            .map(str::to_string)
            .or_else(|| Some("the SFTP session is no longer available".to_string()));
    }

    fn fail(&mut self, message: impl Into<String>) {
        self.loading = false;
        self.error = Some(message.into());
        // Keep the panel consistent with the listing that is still on screen.
        if !self.listed_path.is_empty() {
            self.current = self.listed_path.clone();
            self.path_edit = self.listed_path.clone();
        }
    }
}

/// POSIX path join for remote entries.
pub fn join_path(dir: &str, name: &str) -> String {
    if dir.is_empty() || dir == "/" {
        format!("/{name}")
    } else if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// Parent directory, clamped at `/`.
pub fn parent_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_string();
    }
    match trimmed.rfind('/') {
        Some(0) => "/".to_string(),
        Some(index) => trimmed[..index].to_string(),
        // Relative path such as `foo` or `.`.
        None => ".".to_string(),
    }
}

/// Directories first, then case-insensitive name.
pub fn sort_entries(entries: &mut [RemoteEntry]) {
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| {
                a.name
                    .to_ascii_lowercase()
                    .cmp(&b.name.to_ascii_lowercase())
            })
            .then_with(|| a.name.cmp(&b.name))
    });
}

pub fn format_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];

    if bytes < 1024 {
        return format!("{bytes} B");
    }

    let mut value = bytes as f64 / KIB;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }

    if value >= 10.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn format_age(when: SystemTime) -> String {
    let Ok(elapsed) = SystemTime::now().duration_since(when) else {
        return "in the future".to_string();
    };

    let secs = elapsed.as_secs();
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        86_400..=2_591_999 => format!("{}d ago", secs / 86_400),
        _ => format!("{}y ago", secs / 31_536_000),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, is_dir: bool) -> RemoteEntry {
        RemoteEntry {
            name: name.to_string(),
            path: format!("/tmp/{name}"),
            is_dir,
            is_symlink: false,
            size: 0,
            modified: None,
        }
    }

    #[test]
    fn joins_remote_paths() {
        assert_eq!(join_path("/home/user", "file"), "/home/user/file");
        assert_eq!(join_path("/home/user/", "file"), "/home/user/file");
        assert_eq!(join_path("/", "etc"), "/etc");
        assert_eq!(join_path("", "etc"), "/etc");
        assert_eq!(join_path(".", "file"), "./file");
    }

    #[test]
    fn walks_to_the_parent_and_stops_at_root() {
        assert_eq!(parent_path("/home/user"), "/home");
        assert_eq!(parent_path("/home/user/"), "/home");
        assert_eq!(parent_path("/home"), "/");
        assert_eq!(parent_path("/"), "/");
        assert_eq!(parent_path(""), "/");
        assert_eq!(parent_path("file"), ".");
        assert_eq!(parent_path("."), ".");
    }

    #[test]
    fn sorts_directories_first_then_names_case_insensitively() {
        let mut entries = vec![
            entry("zeta.txt", false),
            entry("Beta", true),
            entry("alpha.txt", false),
            entry("alpha", true),
        ];
        sort_entries(&mut entries);

        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["alpha", "Beta", "alpha.txt", "zeta.txt"]);
    }

    #[test]
    fn formats_human_sizes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KiB");
        assert_eq!(format_size(1536), "1.5 KiB");
        assert_eq!(format_size(10 * 1024), "10 KiB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MiB");
    }

    #[test]
    fn formats_relative_ages() {
        let now = SystemTime::now();
        assert_eq!(
            format_age(now - std::time::Duration::from_secs(5)),
            "just now"
        );
        assert_eq!(
            format_age(now - std::time::Duration::from_secs(120)),
            "2m ago"
        );
        assert_eq!(
            format_age(now - std::time::Duration::from_secs(3 * 3600)),
            "3h ago"
        );
        assert_eq!(
            format_age(now - std::time::Duration::from_secs(2 * 86_400)),
            "2d ago"
        );
    }
}
