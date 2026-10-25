//! State for the SFTP side panel.
//!
//! The panel is per session: each SSH tab owns an `SftpExplorer`, so switching
//! tabs shows that session's remote directory. All network work stays in
//! `ssh.rs`; this module tracks the current path, the last listing, clipboard
//! contents, dialogs and the in-flight request or file operation.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc as std_mpsc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::ssh::{RemoteEntry, SshSession};

/// Where a remote file should be opened locally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorChoice {
    /// `$VISUAL`, then `$EDITOR`, then the desktop default.
    Default,
    Zed,
    Gvim,
    VsCode,
    /// `xdg-open` / the desktop default application.
    System,
}

impl EditorChoice {
    pub const ALL: [EditorChoice; 5] = [
        EditorChoice::Default,
        EditorChoice::Zed,
        EditorChoice::Gvim,
        EditorChoice::VsCode,
        EditorChoice::System,
    ];

    pub fn label(self) -> &'static str {
        match self {
            EditorChoice::Default => "Default editor",
            EditorChoice::Zed => "Zed",
            EditorChoice::Gvim => "gvim",
            EditorChoice::VsCode => "VS Code",
            EditorChoice::System => "Desktop default",
        }
    }
}

/// A remote path saved for "Paste".
#[derive(Debug, Clone)]
pub struct ClipboardEntry {
    pub path: String,
    pub name: String,
    pub is_dir: bool,
}

/// Which dialog is open over the panel.
#[derive(Debug, Clone)]
pub enum SftpDialogKind {
    Rename {
        from: String,
    },
    NewFolder {
        parent: String,
    },
    Delete {
        path: String,
        name: String,
        is_dir: bool,
    },
}

/// Rename / new-folder / delete dialog state.
#[derive(Debug, Clone)]
pub struct SftpDialog {
    pub kind: SftpDialogKind,
    /// Editable name (unused by the delete confirmation).
    pub name: String,
    pub error: Option<String>,
}

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
    /// Set when the server has no SFTP subsystem; disables the controls until a
    /// reconnect succeeds.
    pub unavailable: Option<String>,
    /// Show dotfiles in the listing. Defaults to hidden.
    pub show_hidden: bool,
    /// Last "Copy" action, waiting for "Paste".
    pub clipboard: Option<ClipboardEntry>,
    pub dialog: Option<SftpDialog>,
    /// Last successful operation, shown under the toolbar.
    pub notice: Option<String>,
    pending: Option<Job>,
    operation: Option<Operation>,
}

#[derive(Debug)]
enum Job {
    Home(std_mpsc::Receiver<Result<String, String>>),
    List {
        path: String,
        rx: std_mpsc::Receiver<Result<Vec<RemoteEntry>, String>>,
    },
    Reconnect(std_mpsc::Receiver<Result<(), String>>),
}

#[derive(Debug)]
struct Operation {
    label: String,
    on_success: OnSuccess,
    rx: std_mpsc::Receiver<Result<String, String>>,
}

#[derive(Debug)]
enum OnSuccess {
    /// Refresh the listing after the operation and report `verb` + the path or
    /// destination returned by the SSH side.
    Refresh { verb: &'static str },
    LaunchEditor {
        editor: EditorChoice,
        local: PathBuf,
        name: String,
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

    /// Check the in-flight request or operation. Returns `true` when the UI
    /// should redraw.
    pub fn poll(&mut self, ssh: &SshSession) -> bool {
        let activity = self.poll_operation(ssh);
        let Some(job) = self.pending.take() else {
            return activity;
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
                    activity
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
                    activity
                }
                Err(std_mpsc::TryRecvError::Disconnected) => {
                    self.fail("the SFTP session closed while listing the directory");
                    true
                }
            },
            Job::Reconnect(rx) => match rx.try_recv() {
                Ok(Ok(())) => {
                    self.unavailable = None;
                    self.error = None;
                    self.notice = Some("SFTP reconnected".to_string());
                    self.load_home(ssh);
                    true
                }
                Ok(Err(err)) => {
                    self.loading = false;
                    self.unavailable = Some(err.clone());
                    self.error = Some(err);
                    true
                }
                Err(std_mpsc::TryRecvError::Empty) => {
                    self.pending = Some(Job::Reconnect(rx));
                    activity
                }
                Err(std_mpsc::TryRecvError::Disconnected) => {
                    self.loading = false;
                    let message = "the SSH session closed while reconnecting SFTP";
                    self.unavailable = Some(message.to_string());
                    self.error = Some(message.to_string());
                    true
                }
            },
        }
    }

    fn poll_operation(&mut self, ssh: &SshSession) -> bool {
        let Some(operation) = self.operation.take() else {
            return false;
        };

        match operation.rx.try_recv() {
            Ok(Ok(detail)) => {
                self.finish_operation(operation.on_success, detail, ssh);
                true
            }
            Ok(Err(err)) => {
                self.error = Some(err);
                true
            }
            Err(std_mpsc::TryRecvError::Empty) => {
                self.operation = Some(operation);
                false
            }
            Err(std_mpsc::TryRecvError::Disconnected) => {
                self.error = Some("the SFTP session closed during the file operation".to_string());
                true
            }
        }
    }

    fn finish_operation(&mut self, on_success: OnSuccess, detail: String, ssh: &SshSession) {
        self.error = None;
        match on_success {
            OnSuccess::Refresh { verb } => {
                self.notice = Some(if detail.is_empty() {
                    verb.to_string()
                } else {
                    format!("{verb} {detail}")
                });
                self.refresh(ssh);
            }
            OnSuccess::LaunchEditor {
                editor,
                local,
                name,
            } => match launch_editor(editor, &local) {
                Ok(()) => {
                    self.notice = Some(format!(
                        "Opened {name} in {} (downloaded to {})",
                        editor.label(),
                        local.display()
                    ));
                }
                Err(err) => {
                    self.error = Some(format!("{err} (the download is at {})", local.display()));
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

    /// Reopen the SFTP channel without touching the terminal.
    pub fn reconnect(&mut self, ssh: &SshSession) {
        self.error = None;
        self.notice = None;
        self.loading = true;
        // A stale reply from the old channel must not overwrite the result.
        self.pending = ssh.sftp_reconnect().map(Job::Reconnect);
        if self.pending.is_none() {
            self.mark_unavailable(ssh);
        }
    }

    pub fn selected_entry(&self) -> Option<&RemoteEntry> {
        self.selected.and_then(|index| self.entries.get(index))
    }

    pub fn is_busy(&self) -> bool {
        self.operation.is_some()
    }

    pub fn busy_label(&self) -> Option<&str> {
        self.operation.as_ref().map(|op| op.label.as_str())
    }

    /// Remember a remote entry for a later "Paste".
    pub fn copy_entry(&mut self, entry: &RemoteEntry) -> bool {
        self.clipboard = Some(ClipboardEntry {
            path: entry.path.clone(),
            name: entry.name.clone(),
            is_dir: entry.is_dir,
        });
        self.error = None;
        self.notice = Some(format!("Copied {} — use Paste to duplicate it", entry.name));
        true
    }

    /// Paste the clipboard into the current directory.
    pub fn paste(&mut self, ssh: &SshSession) -> bool {
        let destination = if self.listed_path.is_empty() {
            self.current.clone()
        } else {
            self.listed_path.clone()
        };
        self.paste_into(ssh, destination)
    }

    /// Paste the clipboard into a specific directory (the "Paste into this
    /// folder" row action).
    pub fn paste_into(&mut self, ssh: &SshSession, dest_dir: String) -> bool {
        if !self.can_start_operation() {
            return true;
        }
        let Some(clipboard) = self.clipboard.clone() else {
            self.error = Some("Copy a file or folder first".to_string());
            return true;
        };
        if let Err(message) = copy_target_allowed(&clipboard, &dest_dir) {
            self.error = Some(message);
            return true;
        }
        let Some(rx) = ssh.sftp_copy(&clipboard.path, &dest_dir) else {
            self.mark_unavailable(ssh);
            return true;
        };
        self.start_operation(
            format!("Copying {}…", clipboard.name),
            OnSuccess::Refresh { verb: "Copied to" },
            rx,
        );
        true
    }

    pub fn begin_rename(&mut self, entry: &RemoteEntry) -> bool {
        if !self.can_start_operation() {
            return true;
        }
        self.dialog = Some(SftpDialog {
            kind: SftpDialogKind::Rename {
                from: entry.path.clone(),
            },
            name: entry.name.clone(),
            error: None,
        });
        true
    }

    pub fn begin_delete(&mut self, entry: &RemoteEntry) -> bool {
        if !self.can_start_operation() {
            return true;
        }
        self.dialog = Some(SftpDialog {
            kind: SftpDialogKind::Delete {
                path: entry.path.clone(),
                name: entry.name.clone(),
                is_dir: entry.is_dir,
            },
            name: entry.name.clone(),
            error: None,
        });
        true
    }

    pub fn begin_new_folder(&mut self) -> bool {
        let parent = if self.current.is_empty() {
            self.listed_path.clone()
        } else {
            self.current.clone()
        };
        self.begin_new_folder_in(parent)
    }

    /// Open "New folder" for a specific parent directory (row context menu).
    pub fn begin_new_folder_in(&mut self, parent: String) -> bool {
        if !self.can_start_operation() {
            return true;
        }
        self.dialog = Some(SftpDialog {
            kind: SftpDialogKind::NewFolder { parent },
            name: "new folder".to_string(),
            error: None,
        });
        true
    }

    pub fn cancel_dialog(&mut self) -> bool {
        if self.dialog.take().is_some() {
            return true;
        }
        false
    }

    /// Validate the open dialog and send the corresponding SFTP operation.
    ///
    /// Validation errors stay in the dialog; a successfully dispatched
    /// operation closes it.
    pub fn submit_dialog(&mut self, ssh: &SshSession) -> bool {
        let Some(mut dialog) = self.dialog.take() else {
            return false;
        };
        let name = dialog.name.trim().to_string();

        match dialog.kind.clone() {
            SftpDialogKind::Rename { from } => {
                if let Err(message) = validate_name(&name) {
                    dialog.error = Some(message);
                    self.dialog = Some(dialog);
                    return true;
                }
                let to = join_path(&parent_path(&from), &name);
                if to == from {
                    dialog.error = Some("The name is unchanged".to_string());
                    self.dialog = Some(dialog);
                    return true;
                }
                let Some(rx) = ssh.sftp_rename(&from, &to) else {
                    dialog.error = Some("the SFTP session is no longer available".to_string());
                    self.dialog = Some(dialog);
                    self.mark_unavailable(ssh);
                    return true;
                };
                self.start_operation(
                    format!("Renaming to {name}…"),
                    OnSuccess::Refresh { verb: "Renamed to" },
                    rx,
                );
            }
            SftpDialogKind::NewFolder { parent } => {
                if let Err(message) = validate_name(&name) {
                    dialog.error = Some(message);
                    self.dialog = Some(dialog);
                    return true;
                }
                let path = join_path(&parent, &name);
                let Some(rx) = ssh.sftp_create_dir(&path) else {
                    dialog.error = Some("the SFTP session is no longer available".to_string());
                    self.dialog = Some(dialog);
                    self.mark_unavailable(ssh);
                    return true;
                };
                self.start_operation(
                    format!("Creating {name}…"),
                    OnSuccess::Refresh { verb: "Created" },
                    rx,
                );
            }
            SftpDialogKind::Delete { path, name, is_dir } => {
                let Some(rx) = ssh.sftp_delete(&path, is_dir) else {
                    dialog.error = Some("the SFTP session is no longer available".to_string());
                    self.dialog = Some(dialog);
                    self.mark_unavailable(ssh);
                    return true;
                };
                self.start_operation(
                    format!("Deleting {name}…"),
                    OnSuccess::Refresh { verb: "Deleted" },
                    rx,
                );
            }
        }
        true
    }

    /// Download `entry` to a private temp directory and launch `editor`.
    pub fn open_in_editor(
        &mut self,
        ssh: &SshSession,
        entry: &RemoteEntry,
        editor: EditorChoice,
    ) -> bool {
        if entry.is_dir {
            self.error = Some("Folders cannot be opened in a local editor".to_string());
            return true;
        }
        if !self.can_start_operation() {
            return true;
        }
        let local = match temp_download_path(&entry.name) {
            Ok(path) => path,
            Err(err) => {
                self.error = Some(err);
                return true;
            }
        };
        let Some(rx) = ssh.sftp_fetch(&entry.path, local.clone()) else {
            self.mark_unavailable(ssh);
            return true;
        };
        self.start_operation(
            format!("Downloading {}…", entry.name),
            OnSuccess::LaunchEditor {
                editor,
                local,
                name: entry.name.clone(),
            },
            rx,
        );
        true
    }

    fn can_start_operation(&mut self) -> bool {
        if let Some(label) = self.busy_label() {
            self.error = Some(format!(
                "Wait for the current operation ({label}) to finish"
            ));
            return false;
        }
        true
    }

    fn start_operation(
        &mut self,
        label: String,
        on_success: OnSuccess,
        rx: std_mpsc::Receiver<Result<String, String>>,
    ) {
        self.error = None;
        self.notice = None;
        self.dialog = None;
        self.operation = Some(Operation {
            label,
            on_success,
            rx,
        });
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

/// Local timestamp shown on every row.
pub fn format_timestamp(when: Option<SystemTime>) -> String {
    match when {
        Some(when) => {
            let local: chrono::DateTime<chrono::Local> = when.into();
            local.format("%Y-%m-%d %H:%M").to_string()
        }
        None => "unknown".to_string(),
    }
}

/// Owner text for a row.
pub fn format_owner(entry: &RemoteEntry) -> &str {
    if entry.owner.is_empty() {
        "unknown"
    } else {
        &entry.owner
    }
}

/// Reject names that cannot be a single path component.
pub fn validate_name(name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Enter a name".to_string());
    }
    if name == "." || name == ".." {
        return Err("That name is reserved".to_string());
    }
    if name.contains('/') {
        return Err("A name cannot contain '/'".to_string());
    }
    if name.contains('\0') {
        return Err("A name cannot contain a NUL byte".to_string());
    }
    Ok(())
}

fn copy_target_allowed(clipboard: &ClipboardEntry, dest_dir: &str) -> Result<(), String> {
    if !clipboard.is_dir {
        return Ok(());
    }
    let source = clipboard.path.trim_end_matches('/');
    let destination = dest_dir.trim_end_matches('/');
    if destination == source || destination.starts_with(&format!("{source}/")) {
        return Err(format!(
            "cannot copy folder '{}' into itself",
            clipboard.name
        ));
    }
    Ok(())
}

fn temp_download_path(name: &str) -> Result<PathBuf, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let directory =
        std::env::temp_dir().join(format!("rustterm-edit-{}-{stamp}", std::process::id()));
    std::fs::create_dir_all(&directory)
        .map_err(|err| format!("could not create '{}': {err}", directory.display()))?;

    let safe = name
        .rsplit('/')
        .next()
        .unwrap_or(name)
        .replace(['/', '\\'], "_");
    let safe = if safe.is_empty() { "download" } else { &safe };
    Ok(directory.join(safe))
}

fn launch_editor(choice: EditorChoice, path: &Path) -> Result<(), String> {
    let (program, args) = editor_command(choice);
    let mut child = Command::new(&program)
        .args(args)
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| format!("could not launch {program}: {err}"))?;

    // Reap the child without blocking the UI. Editors that fork (VS Code, Zed)
    // exit here almost immediately; terminal editors keep the thread alive
    // until they are closed.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

fn editor_command(choice: EditorChoice) -> (String, Vec<String>) {
    match choice {
        EditorChoice::Default => default_editor(),
        EditorChoice::Zed => ("zed".to_string(), Vec::new()),
        EditorChoice::Gvim => ("gvim".to_string(), Vec::new()),
        EditorChoice::VsCode => ("code".to_string(), Vec::new()),
        EditorChoice::System => ("xdg-open".to_string(), Vec::new()),
    }
}

fn default_editor() -> (String, Vec<String>) {
    for variable in ["VISUAL", "EDITOR"] {
        if let Ok(value) = std::env::var(variable) {
            let mut parts = value.split_whitespace();
            if let Some(program) = parts.next() {
                return (
                    program.to_string(),
                    parts.map(str::to_string).collect::<Vec<_>>(),
                );
            }
        }
    }
    ("xdg-open".to_string(), Vec::new())
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
            owner: "alice".to_string(),
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

    #[test]
    fn formats_local_timestamps_and_unknown_values() {
        assert_eq!(format_timestamp(None), "unknown");
        let stamp = format_timestamp(Some(UNIX_EPOCH));
        assert_eq!(stamp.len(), "1970-01-01 00:00".len());
        assert!(stamp.contains('-'));
        assert!(stamp.contains(':'));
    }

    #[test]
    fn validates_single_component_names() {
        assert!(validate_name("report.txt").is_ok());
        assert!(validate_name("with space").is_ok());
        assert!(validate_name("  trimmed  ").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("   ").is_err());
        assert!(validate_name(".").is_err());
        assert!(validate_name("..").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("nul\0byte").is_err());
    }

    #[test]
    fn refuses_to_copy_a_folder_into_itself() {
        let folder = ClipboardEntry {
            path: "/srv/data".to_string(),
            name: "data".to_string(),
            is_dir: true,
        };
        assert!(copy_target_allowed(&folder, "/srv").is_ok());
        assert!(copy_target_allowed(&folder, "/srv/data").is_err());
        assert!(copy_target_allowed(&folder, "/srv/data/child").is_err());

        let file = ClipboardEntry {
            path: "/srv/data.txt".to_string(),
            name: "data.txt".to_string(),
            is_dir: false,
        };
        assert!(copy_target_allowed(&file, "/srv/data.txt").is_ok());
    }

    #[test]
    fn resolves_explicit_editors_and_default_fallback() {
        assert_eq!(editor_command(EditorChoice::Zed).0, "zed");
        assert_eq!(editor_command(EditorChoice::Gvim).0, "gvim");
        assert_eq!(editor_command(EditorChoice::VsCode).0, "code");
        assert_eq!(editor_command(EditorChoice::System).0, "xdg-open");
        // The default always resolves to something spawnable.
        assert!(!editor_command(EditorChoice::Default).0.is_empty());
    }
}
