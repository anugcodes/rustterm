//! State for the SFTP side panel.
//!
//! The panel is per session: each SSH tab owns an `SftpExplorer`, so switching
//! tabs shows that session's remote directory. All network work stays in
//! `ssh.rs`; this module tracks the current path, the last listing, clipboard
//! contents, dialogs and the in-flight request or file operation.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc as std_mpsc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::ssh::{RemoteEntry, SshSession};

/// Where a remote file should be opened locally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EditorChoice {
    /// `$VISUAL`, then `$EDITOR`, then the desktop default.
    #[default]
    Default,
    Zed,
    Gvim,
    VsCode,
    /// `xdg-open` / the desktop default application.
    System,
    /// A command typed by the user in the "Open with…" dialog.
    Custom,
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
            EditorChoice::Custom => "Custom command",
        }
    }
}

/// State for the "Open with…" dialog.
#[derive(Debug, Clone)]
pub struct EditorPicker {
    pub entry: RemoteEntry,
    pub choice: EditorChoice,
    pub custom: String,
    pub error: Option<String>,
}

/// Severity of a status-bar message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    Info,
    Success,
    Error,
}

/// One entry in the SFTP message log.
#[derive(Debug, Clone)]
pub struct StatusEntry {
    pub kind: StatusKind,
    pub text: String,
    pub at: String,
}

/// Latest message plus a bounded history for the status-bar popover.
#[derive(Debug, Default)]
pub struct StatusLog {
    current: Option<StatusEntry>,
    history: VecDeque<StatusEntry>,
}

impl StatusLog {
    const MAX_HISTORY: usize = 50;

    fn push(&mut self, kind: StatusKind, text: impl Into<String>) {
        let entry = StatusEntry {
            kind,
            text: text.into(),
            at: chrono::Local::now().format("%H:%M:%S").to_string(),
        };
        self.current = Some(entry.clone());
        self.history.push_back(entry);
        while self.history.len() > Self::MAX_HISTORY {
            self.history.pop_front();
        }
    }

    pub fn info(&mut self, text: impl Into<String>) {
        self.push(StatusKind::Info, text);
    }

    pub fn success(&mut self, text: impl Into<String>) {
        self.push(StatusKind::Success, text);
    }

    pub fn error(&mut self, text: impl Into<String>) {
        self.push(StatusKind::Error, text);
    }

    pub fn current(&self) -> Option<&StatusEntry> {
        self.current.as_ref()
    }

    /// Newest message first.
    pub fn history(&self) -> impl DoubleEndedIterator<Item = &StatusEntry> {
        self.history.iter().rev()
    }

    pub fn clear(&mut self) {
        self.current = None;
        self.history.clear();
    }

    pub fn is_error(&self) -> bool {
        self.current
            .as_ref()
            .is_some_and(|entry| entry.kind == StatusKind::Error)
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
    /// Set when the server has no SFTP subsystem; disables the controls until a
    /// reconnect succeeds.
    pub unavailable: Option<String>,
    /// Show dotfiles in the listing. Defaults to hidden.
    pub show_hidden: bool,
    /// Last "Copy" action, waiting for "Paste".
    pub clipboard: Option<ClipboardEntry>,
    pub dialog: Option<SftpDialog>,
    /// State for the "Open with…" modal.
    pub editor_picker: Option<EditorPicker>,
    /// Last editor chosen for this session; preselected next time.
    last_editor: EditorChoice,
    /// Latest message and recent history, shown at the bottom of the panel.
    pub status: StatusLog,
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
        custom: Option<String>,
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
            explorer.status.error(reason);
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
                    let count = entries.len();
                    self.entries = entries;
                    self.listed_path = path;
                    self.selected = None;
                    self.loading = false;
                    // Keep an operation result visible; otherwise report the
                    // number of entries for this navigation.
                    let keep_success = self
                        .status
                        .current()
                        .is_some_and(|entry| entry.kind == StatusKind::Success);
                    if !keep_success {
                        self.status.info(format!("{count} entries"));
                    }
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
                    self.load_home(ssh);
                    self.status.success("SFTP reconnected");
                    true
                }
                Ok(Err(err)) => {
                    self.loading = false;
                    self.unavailable = Some(err.clone());
                    self.status.error(err);
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
                    self.status.error(message);
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
                self.status.error(err);
                true
            }
            Err(std_mpsc::TryRecvError::Empty) => {
                self.operation = Some(operation);
                false
            }
            Err(std_mpsc::TryRecvError::Disconnected) => {
                self.status
                    .error("the SFTP session closed during the file operation");
                true
            }
        }
    }

    fn finish_operation(&mut self, on_success: OnSuccess, detail: String, ssh: &SshSession) {
        match on_success {
            OnSuccess::Refresh { verb } => {
                self.status.success(if detail.is_empty() {
                    verb.to_string()
                } else {
                    format!("{verb} {detail}")
                });
                self.refresh(ssh);
            }
            OnSuccess::LaunchEditor {
                editor,
                custom,
                local,
                name,
            } => match launch_editor(editor, custom.as_deref(), &local) {
                Ok(()) => {
                    self.status.success(format!(
                        "Opened {name} in {} (downloaded to {})",
                        editor_label(editor, custom.as_deref()),
                        local.display()
                    ));
                }
                Err(err) => {
                    self.status
                        .error(format!("{err} (the download is at {})", local.display()));
                }
            },
        }
    }

    /// Resolve `.` again (usually the login home directory).
    pub fn load_home(&mut self, ssh: &SshSession) {
        self.status.info("Loading remote files…");
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
            self.status.error("Enter a remote path");
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
        self.status.info("Reconnecting SFTP…");
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
        self.status
            .info(format!("Copied {} — use Paste to duplicate it", entry.name));
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
            self.status.error("Copy a file or folder first");
            return true;
        };
        if let Err(message) = copy_target_allowed(&clipboard, &dest_dir) {
            self.status.error(message);
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

    /// Open the "Open with…" dialog for a file.
    pub fn begin_open_with(&mut self, entry: &RemoteEntry) -> bool {
        if entry.is_dir {
            self.status
                .error("Folders cannot be opened in a local editor");
            return true;
        }
        if !self.can_start_operation() {
            return true;
        }
        self.editor_picker = Some(EditorPicker {
            entry: entry.clone(),
            choice: self.last_editor,
            custom: String::new(),
            error: None,
        });
        true
    }

    pub fn cancel_open_with(&mut self) -> bool {
        self.editor_picker.take().is_some()
    }

    /// Validate the "Open with…" dialog and start the download/launch.
    pub fn submit_open_with(&mut self, ssh: &SshSession) -> bool {
        let Some(mut picker) = self.editor_picker.take() else {
            return false;
        };
        if picker.choice == EditorChoice::Custom && picker.custom.trim().is_empty() {
            picker.error = Some("Enter a command, for example: kate".to_string());
            self.editor_picker = Some(picker);
            return true;
        }
        self.last_editor = picker.choice;
        let custom = (picker.choice == EditorChoice::Custom).then(|| picker.custom.clone());
        self.open_in_editor(ssh, &picker.entry, picker.choice, custom);
        true
    }

    /// Download `entry` to a private temp directory and launch `editor`.
    pub fn open_in_editor(
        &mut self,
        ssh: &SshSession,
        entry: &RemoteEntry,
        editor: EditorChoice,
        custom: Option<String>,
    ) -> bool {
        if entry.is_dir {
            self.status
                .error("Folders cannot be opened in a local editor");
            return true;
        }
        if !self.can_start_operation() {
            return true;
        }
        let local = match temp_download_path(&entry.name) {
            Ok(path) => path,
            Err(err) => {
                self.status.error(err);
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
                custom,
                local,
                name: entry.name.clone(),
            },
            rx,
        );
        true
    }

    fn can_start_operation(&mut self) -> bool {
        if let Some(label) = self.busy_label() {
            self.status.error(format!(
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
        self.status.info(format!("Opening {path}…"));

        if let Some(rx) = ssh.sftp_list(&path) {
            self.loading = true;
            self.pending = Some(Job::List { path, rx });
        } else {
            self.mark_unavailable(ssh);
        }
    }

    fn mark_unavailable(&mut self, ssh: &SshSession) {
        self.loading = false;
        let message = ssh
            .sftp_unavailable()
            .map(str::to_string)
            .unwrap_or_else(|| "the SFTP session is no longer available".to_string());
        self.unavailable = Some(message.clone());
        self.status.error(message);
    }

    fn fail(&mut self, message: impl Into<String>) {
        self.loading = false;
        self.status.error(message);
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

fn launch_editor(choice: EditorChoice, custom: Option<&str>, path: &Path) -> Result<(), String> {
    let (program, args) = editor_command(choice, custom);
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

pub fn editor_label(choice: EditorChoice, custom: Option<&str>) -> String {
    match choice {
        EditorChoice::Custom => custom.unwrap_or("Custom command").trim().to_string(),
        _ => choice.label().to_string(),
    }
}

/// Program name used for an editor choice. Useful for availability checks and
/// for showing the resolved default editor in the picker.
pub fn editor_program(choice: EditorChoice) -> String {
    editor_command(choice, None).0
}

pub fn editor_command(choice: EditorChoice, custom: Option<&str>) -> (String, Vec<String>) {
    match choice {
        EditorChoice::Default => default_editor(),
        EditorChoice::Zed => ("zed".to_string(), Vec::new()),
        EditorChoice::Gvim => ("gvim".to_string(), Vec::new()),
        EditorChoice::VsCode => ("code".to_string(), Vec::new()),
        EditorChoice::System => ("xdg-open".to_string(), Vec::new()),
        EditorChoice::Custom => parse_command(custom.unwrap_or_default()),
    }
}

fn parse_command(value: &str) -> (String, Vec<String>) {
    let mut parts = value.split_whitespace();
    let program = parts.next().unwrap_or_default().to_string();
    let args = parts.map(str::to_string).collect();
    (program, args)
}

fn default_editor() -> (String, Vec<String>) {
    for variable in ["VISUAL", "EDITOR"] {
        if let Ok(value) = std::env::var(variable) {
            let (program, args) = parse_command(&value);
            if !program.is_empty() {
                return (program, args);
            }
        }
    }
    ("xdg-open".to_string(), Vec::new())
}

/// Whether `program` can be found on this machine.
pub fn command_available(program: &str) -> bool {
    if program.is_empty() {
        return false;
    }
    let path = Path::new(program);
    if path.components().count() > 1 {
        return path.is_file();
    }
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|directory| directory.join(program).is_file())
        })
        .unwrap_or(false)
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
        assert_eq!(editor_command(EditorChoice::Zed, None).0, "zed");
        assert_eq!(editor_command(EditorChoice::Gvim, None).0, "gvim");
        assert_eq!(editor_command(EditorChoice::VsCode, None).0, "code");
        assert_eq!(editor_command(EditorChoice::System, None).0, "xdg-open");
        // The default always resolves to something spawnable.
        assert!(!editor_command(EditorChoice::Default, None).0.is_empty());
    }

    #[test]
    fn parses_custom_editor_commands() {
        assert_eq!(
            editor_command(EditorChoice::Custom, Some("kate --new")),
            ("kate".to_string(), vec!["--new".to_string()])
        );
        assert_eq!(
            editor_command(EditorChoice::Custom, Some("  ")),
            (String::new(), Vec::new())
        );
        assert!(!command_available("rustterm-editor-that-does-not-exist"));
    }

    #[test]
    fn status_log_keeps_only_the_latest_entries() {
        let mut status = StatusLog::default();
        for index in 0..(StatusLog::MAX_HISTORY + 5) {
            status.info(format!("message {index}"));
        }
        assert_eq!(status.history().count(), StatusLog::MAX_HISTORY);
        assert_eq!(status.current().unwrap().text, "message 54");
        status.error("broken");
        assert!(status.is_error());
        status.clear();
        assert!(status.current().is_none());
        assert_eq!(status.history().count(), 0);
    }
}
