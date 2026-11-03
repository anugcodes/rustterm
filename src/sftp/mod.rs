//! State for the SFTP side panel.
//!
//! The panel is per session: each SSH tab owns an `SftpExplorer`, so switching
//! tabs shows that session's remote directory. All network work stays in
//! `ssh.rs`; this module tracks the current path, the last listing, clipboard
//! contents, dialogs and the in-flight request or file operation.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use notify::Watcher as _;

use crate::ssh::{
    ConflictPolicy, HookResult, RemoteEntry, ResolvedLink, ShellInfo, ShellKind, SshSession,
    TransferProgress, Wake,
};

mod edit;
mod paths;
mod transfer;
pub use edit::*;
pub use paths::*;
pub use transfer::*;

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
    DeleteMany {
        /// Remote path and a directory hint for each selected entry.
        paths: Vec<(String, bool)>,
        names: Vec<String>,
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

/// Give the first shell prompt a moment to report its directory before
/// offering the shell-hook hint; otherwise it flashes on every connect.
const HOOK_HINT_DELAY: Duration = Duration::from_secs(3);

/// Upper bound on each history stack.
const HISTORY_LIMIT: usize = 100;

/// Why a listing was started. History is committed only when the reply
/// succeeds, so a failed navigation cannot corrupt the stacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NavIntent {
    /// First listing or reconnect: replace the directory silently.
    Initial,
    /// Explicit navigation: path box, Up, Home, or a folder click.
    User,
    /// Shell-driven following.
    Follow,
    /// Same directory, re-listed after a file operation.
    Refresh,
    /// Back/Forward buttons.
    Back,
    Forward,
}

/// Browser-style directory history for the panel.
#[derive(Debug, Default)]
struct SftpHistory {
    back: Vec<String>,
    forward: Vec<String>,
    current: String,
}

impl SftpHistory {
    fn can_back(&self) -> bool {
        !self.back.is_empty()
    }

    fn can_forward(&self) -> bool {
        !self.forward.is_empty()
    }

    fn back_target(&self) -> Option<String> {
        self.back.last().cloned()
    }

    fn forward_target(&self) -> Option<String> {
        self.forward.last().cloned()
    }

    /// Record a listing that actually succeeded.
    fn visit(&mut self, path: &str, intent: NavIntent) {
        match intent {
            NavIntent::Initial => self.current = path.to_string(),
            NavIntent::Refresh => {
                if self.current.is_empty() {
                    self.current = path.to_string();
                }
            }
            NavIntent::User | NavIntent::Follow => {
                if self.current == path {
                    return;
                }
                if !self.current.is_empty() {
                    let previous = std::mem::take(&mut self.current);
                    push_history(&mut self.back, previous);
                }
                self.current = path.to_string();
                self.forward.clear();
            }
            NavIntent::Back => {
                if self.back.last().map(String::as_str) == Some(path) {
                    self.back.pop();
                }
                if self.current != path {
                    let previous = std::mem::replace(&mut self.current, path.to_string());
                    push_history(&mut self.forward, previous);
                }
            }
            NavIntent::Forward => {
                if self.forward.last().map(String::as_str) == Some(path) {
                    self.forward.pop();
                }
                if self.current != path {
                    let previous = std::mem::replace(&mut self.current, path.to_string());
                    push_history(&mut self.back, previous);
                }
            }
        }
    }
}

fn push_history(stack: &mut Vec<String>, path: String) {
    if path.is_empty() || stack.last() == Some(&path) {
        return;
    }
    stack.push(path);
    if stack.len() > HISTORY_LIMIT {
        stack.remove(0);
    }
}

/// Consent modal for writing the shell hook into a startup file.
#[derive(Debug, Clone)]
pub struct HookPrompt {
    /// Startup file that will be edited.
    pub path: String,
    /// Login shell that selected it.
    pub shell: String,
    /// Family deciding the block body and the `cwdcmd` warning.
    pub kind: ShellKind,
}

/// What to do once the login shell is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookAction {
    /// Type the one-liner into the running shell.
    EnableNow,
    /// Offer to write the marked block into a startup file.
    Install,
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
    /// Follow the shell's working directory (OSC 7). Manual navigation turns
    /// this off; the Follow chip turns it back on.
    pub follow: bool,
    /// True while the path box has focus. Following waits so it cannot
    /// overwrite what the user is typing.
    pub path_focused: bool,
    /// Most recent directory reported by the shell.
    shell_cwd: Option<String>,
    /// Whether any OSC 7 report has arrived this session.
    saw_shell_cwd: bool,
    /// A shell directory the panel already failed to open; not retried on
    /// every prompt.
    follow_denied: Option<String>,
    /// Login name, for the `/etc/passwd` shell lookup.
    user: String,
    /// Resolved remote home directory, used to build startup-file paths.
    home: Option<String>,
    /// Login shell from `/etc/passwd`, with the startup file it reads.
    login_shell: Option<ShellInfo>,
    /// Set when a hook action was requested but the shell was still being
    /// detected.
    pending_hook: Option<HookAction>,
    /// Set when the hint was dismissed for this session.
    hook_hint_dismissed: bool,
    /// When the panel was created, so the hint waits for the first prompt.
    started_at: Option<Instant>,
    /// Consent modal for the shell hook.
    pub hook_prompt: Option<HookPrompt>,
    /// Symlink targets resolved so far, for the details strip.
    link_targets: HashMap<String, String>,
    /// Directory history behind Back/Forward.
    history: SftpHistory,
    pub entries: Vec<RemoteEntry>,
    /// Indices into `entries` that are selected in the table.
    pub selected: BTreeSet<usize>,
    /// Row a shift range extends from; also the keyboard cursor.
    pub selection_anchor: Option<usize>,
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
    /// Last "upload on save" answer; preselected next time.
    last_auto_upload: bool,
    /// Local copies opened in an editor, watched for saves.
    edits: Vec<EditWatch>,
    /// Inotify watcher shared by every open copy.
    notifier: Option<EditNotifier>,
    /// When the safety pass last compared timestamps.
    last_edit_safety: Option<Instant>,
    /// Wakes egui when a save event arrives.
    wake: Waker,
    /// Local copies that outlive this connection, keyed by profile.
    journal: EditJournal,
    /// Latest message and recent history, shown at the bottom of the panel.
    pub status: StatusLog,
    pending: Option<Job>,
    operation: Option<Operation>,
    /// Uploads/downloads, one running at a time.
    pub transfer: Option<ActiveTransfer>,
    /// A conflict question that must be answered before the transfer starts.
    pub conflict: Option<ConflictPrompt>,
    queue: VecDeque<TransferRequest>,
}

#[derive(Debug)]
enum Job {
    Home {
        intent: NavIntent,
        rx: std_mpsc::Receiver<Result<String, String>>,
    },
    List {
        path: String,
        intent: NavIntent,
        rx: std_mpsc::Receiver<Result<Vec<RemoteEntry>, String>>,
    },
    Reconnect(std_mpsc::Receiver<Result<(), String>>),
    /// A symlink is being resolved so it can be followed.
    Resolve {
        entry: RemoteEntry,
        rx: std_mpsc::Receiver<Result<ResolvedLink, String>>,
    },
    /// Login-shell lookup for the install dialog.
    Shell(std_mpsc::Receiver<Result<ShellInfo, String>>),
    /// Install/remove the startup-file hook.
    Hook {
        install: bool,
        rx: std_mpsc::Receiver<Result<HookResult, String>>,
    },
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
        remote: String,
        name: String,
        auto_upload: bool,
    },
    LaunchEditors {
        editor: EditorChoice,
        custom: Option<String>,
        locals: Vec<PathBuf>,
    },
}

impl SftpExplorer {
    /// Create the panel state and start resolving the remote home directory.
    pub fn start(ssh: &SshSession, profile: &str) -> Self {
        let user = profile.split('@').next().unwrap_or(profile).to_string();
        let mut explorer = Self {
            follow: true,
            user,
            started_at: Some(Instant::now()),
            last_auto_upload: true,
            journal: EditJournal::for_profile(profile),
            ..Self::default()
        };
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
        let mut activity = self.poll_transfer(ssh);
        activity |= self.poll_operation(ssh);
        activity |= self.poll_edits(ssh);
        // One reconnect-sync attempt per connection. `synced` is reset when the
        // journal is installed or when Reconnect SFTP succeeds.
        if !self.journal.synced && self.unavailable.is_none() && !self.journal.is_empty() {
            self.journal.synced = true;
            activity |= self.sync_journal(ssh);
        }
        // Entries that stayed offline past the window are no longer eligible.
        for name in self.journal.expire(Instant::now()) {
            self.status.info(format!(
                "{name} changed while offline but was not synced within 15 minutes"
            ));
        }
        let Some(job) = self.pending.take() else {
            return activity;
        };

        match job {
            Job::Home { intent, rx } => match rx.try_recv() {
                Ok(Ok(home)) => {
                    self.home = Some(home.clone());
                    self.current = home.clone();
                    self.path_edit = home.clone();
                    self.start_list(ssh, home, intent);
                    true
                }
                Ok(Err(err)) => {
                    self.fail(err);
                    true
                }
                Err(std_mpsc::TryRecvError::Empty) => {
                    self.pending = Some(Job::Home { intent, rx });
                    activity
                }
                Err(std_mpsc::TryRecvError::Disconnected) => {
                    self.fail("the SFTP session closed while resolving the home directory");
                    true
                }
            },
            Job::List { path, intent, rx } => match rx.try_recv() {
                Ok(Ok(mut entries)) => {
                    sort_entries(&mut entries);
                    let count = entries.len();
                    self.entries = entries;
                    // Only a successful listing becomes part of history.
                    self.history.visit(&path, intent);
                    self.listed_path = path;
                    self.clear_selection();
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
                    if intent == NavIntent::Follow {
                        // Do not retry an unreadable directory on every prompt.
                        self.follow_denied = Some(path.clone());
                    }
                    self.fail(err);
                    true
                }
                Err(std_mpsc::TryRecvError::Empty) => {
                    self.pending = Some(Job::List { path, intent, rx });
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
                    // The channel is back: pick up any edits made while it was
                    // down before the listing is refreshed.
                    self.journal.synced = false;
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
            Job::Resolve { entry, rx } => match rx.try_recv() {
                Ok(Ok(link)) => {
                    if let Some(target) = link.target {
                        self.link_targets.insert(entry.path.clone(), target);
                    }
                    if link.is_dir {
                        // Following a link is manual browsing; stop tracking
                        // the shell so the next prompt does not yank it away.
                        self.follow = false;
                        self.start_list(ssh, entry.path, NavIntent::User);
                    } else {
                        self.begin_open_with(std::slice::from_ref(&entry));
                    }
                    true
                }
                Ok(Err(err)) => {
                    self.status.error(err);
                    true
                }
                Err(std_mpsc::TryRecvError::Empty) => {
                    self.pending = Some(Job::Resolve { entry, rx });
                    activity
                }
                Err(std_mpsc::TryRecvError::Disconnected) => {
                    self.fail("the SFTP session closed while resolving a link");
                    true
                }
            },
            Job::Shell(rx) => match rx.try_recv() {
                Ok(Ok(info)) => {
                    let name = info.shell.clone();
                    self.login_shell = Some(info);
                    match self.pending_hook.take() {
                        Some(HookAction::Install) => {
                            self.open_hook_prompt();
                        }
                        Some(HookAction::EnableNow) => {
                            self.send_session_hook(ssh);
                        }
                        None => self.status.info(format!("Login shell: {name}")),
                    }
                    true
                }
                Ok(Err(err)) => {
                    self.pending_hook = None;
                    self.status.error(err);
                    true
                }
                Err(std_mpsc::TryRecvError::Empty) => {
                    self.pending = Some(Job::Shell(rx));
                    activity
                }
                Err(std_mpsc::TryRecvError::Disconnected) => {
                    self.fail("the SFTP session closed while reading /etc/passwd");
                    true
                }
            },
            Job::Hook { install, rx } => match rx.try_recv() {
                Ok(Ok(result)) => {
                    if install {
                        // Activate it in the current shell too, not only at the
                        // next login — even when the file was already correct.
                        self.send_session_hook(ssh);
                        self.follow = true;
                    }
                    let backup = result
                        .backup
                        .map(|path| format!(" (backup: {path})"))
                        .unwrap_or_default();
                    if result.changed {
                        let verb = if install {
                            "installed in"
                        } else {
                            "removed from"
                        };
                        self.status
                            .success(format!("Directory tracking {verb} {}{backup}", result.path));
                    } else {
                        let state = if install {
                            "already installed"
                        } else {
                            "was not installed"
                        };
                        self.status
                            .info(format!("Nothing to change: {} ({state})", result.path));
                    }
                    true
                }
                Ok(Err(err)) => {
                    self.status.error(err);
                    true
                }
                Err(std_mpsc::TryRecvError::Empty) => {
                    self.pending = Some(Job::Hook { install, rx });
                    activity
                }
                Err(std_mpsc::TryRecvError::Disconnected) => {
                    self.fail("the SFTP session closed while editing the startup file");
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
                if crate::ssh::is_sftp_timeout(&err) {
                    self.mark_unavailable_with(crate::ssh::SFTP_TIMEOUT);
                } else {
                    self.status.error(err);
                }
                true
            }
            Err(std_mpsc::TryRecvError::Empty) => {
                self.operation = Some(operation);
                false
            }
            Err(std_mpsc::TryRecvError::Disconnected) => {
                self.mark_unavailable_with("the SFTP session closed during the file operation");
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
                remote,
                name,
                auto_upload,
            } => self.launch_local_copy(editor, custom, local, remote, name, auto_upload),
            OnSuccess::LaunchEditors {
                editor,
                custom,
                locals,
            } => {
                let count = locals.len();
                for local in &locals {
                    if let Err(err) = launch_editor(editor, custom.as_deref(), local) {
                        self.status.error(err);
                        return;
                    }
                }
                self.status.success(format!("Opened {count} files"));
            }
        }
    }

    /// Resolve `.` again (usually the login home directory).
    ///
    /// Used by connect and reconnect, so it keeps following the shell.
    pub fn load_home(&mut self, ssh: &SshSession) {
        self.request_home(ssh, NavIntent::Initial);
    }

    /// Toolbar **Home**: explicit navigation, so following pauses.
    pub fn go_home(&mut self, ssh: &SshSession) {
        self.follow = false;
        self.request_home(ssh, NavIntent::User);
    }

    fn request_home(&mut self, ssh: &SshSession, intent: NavIntent) {
        self.status.info("Loading remote files…");
        if let Some(rx) = ssh.sftp_home() {
            self.loading = true;
            self.pending = Some(Job::Home { intent, rx });
        } else {
            self.mark_unavailable(ssh);
        }
    }

    /// Show `path`, replacing the current listing once the reply arrives.
    ///
    /// A user navigation stops shell following, so the panel never moves under
    /// the user while they browse.
    pub fn navigate(&mut self, ssh: &SshSession, path: impl Into<String>) {
        self.follow = false;
        self.navigate_inner(ssh, path.into(), NavIntent::User);
    }

    fn navigate_inner(&mut self, ssh: &SshSession, path: String, intent: NavIntent) {
        let path = path.trim();
        if path.is_empty() {
            self.status.error("Enter a remote path");
            return;
        }
        self.start_list(ssh, path.to_string(), intent);
    }

    /// Re-list the current directory. This is not a move, so it keeps
    /// following and does not touch history.
    pub fn refresh(&mut self, ssh: &SshSession) {
        let path = if self.current.is_empty() {
            ".".to_string()
        } else {
            self.current.clone()
        };
        self.start_list(ssh, path, NavIntent::Refresh);
    }

    /// Go to the parent of `current`.
    pub fn up(&mut self, ssh: &SshSession) {
        if self.current.is_empty() {
            return;
        }
        self.follow = false;
        let parent = parent_path(&self.current);
        self.start_list(ssh, parent, NavIntent::User);
    }

    pub fn can_back(&self) -> bool {
        self.history.can_back()
    }

    pub fn can_forward(&self) -> bool {
        self.history.can_forward()
    }

    /// Go to the previous directory. Manual navigation pauses following.
    pub fn back(&mut self, ssh: &SshSession) -> bool {
        let Some(target) = self.history.back_target() else {
            return false;
        };
        self.follow = false;
        self.start_list(ssh, target, NavIntent::Back);
        true
    }

    /// Go forward again after one or more **Back** steps.
    pub fn forward(&mut self, ssh: &SshSession) -> bool {
        let Some(target) = self.history.forward_target() else {
            return false;
        };
        self.follow = false;
        self.start_list(ssh, target, NavIntent::Forward);
        true
    }

    /// Follow a directory reported by the shell via OSC 7.
    ///
    /// The latest report is always remembered, even while following is off or
    /// the path box has focus, so clicking **Follow** can catch up instantly.
    pub fn follow_shell_cwd(&mut self, ssh: &SshSession, cwd: &str) {
        let Some(path) = normalize_shell_path(cwd) else {
            return;
        };
        self.saw_shell_cwd = true;
        if self.shell_cwd.as_deref() == Some(&path) {
            return;
        }
        self.shell_cwd = Some(path.clone());
        if !self.follow || self.unavailable.is_some() || self.path_focused {
            return;
        }
        if self.current == path || self.listed_path == path {
            return;
        }
        if self.follow_denied.as_deref() == Some(path.as_str()) {
            return;
        }
        self.navigate_inner(ssh, path, NavIntent::Follow);
    }

    /// Turn following on or off; turning it on re-syncs to the shell at once.
    pub fn set_follow(&mut self, ssh: &SshSession, follow: bool) {
        self.follow = follow;
        if !follow {
            self.status.info("Stopped following the shell");
            return;
        }
        match self.shell_cwd.clone() {
            Some(cwd) if cwd != self.current => {
                self.navigate_inner(ssh, cwd, NavIntent::Follow);
            }
            Some(_) => self.status.success("Following the shell again"),
            None => self
                .status
                .info("Following the shell — waiting for the first directory report"),
        }
    }

    /// Whether the panel should offer the shell-hook hint.
    pub fn wants_hook_hint(&self) -> bool {
        self.follow
            && !self.saw_shell_cwd
            && !self.hook_hint_dismissed
            && self.unavailable.is_none()
            && !self.current.is_empty()
            && self
                .started_at
                .is_some_and(|started| started.elapsed() >= HOOK_HINT_DELAY)
    }

    pub fn dismiss_hook_hint(&mut self) {
        self.hook_hint_dismissed = true;
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

    /// The selected entry when exactly one row is selected.
    pub fn selected_entry(&self) -> Option<&RemoteEntry> {
        if self.selected.len() != 1 {
            return None;
        }
        self.selected
            .iter()
            .next()
            .and_then(|&index| self.entries.get(index))
    }

    /// Every selected entry, in table order.
    pub fn selected_entries(&self) -> Vec<RemoteEntry> {
        self.selected
            .iter()
            .filter_map(|&index| self.entries.get(index))
            .cloned()
            .collect()
    }

    pub fn selection_count(&self) -> usize {
        self.selected.len()
    }

    /// Replace the selection with one row and move the anchor there.
    pub fn select_row(&mut self, index: usize) {
        self.selected.clear();
        self.selected.insert(index);
        self.selection_anchor = Some(index);
    }

    /// Ctrl-click: add the row, or remove it if it was already selected.
    pub fn toggle_row(&mut self, index: usize) {
        if !self.selected.remove(&index) {
            self.selected.insert(index);
        }
        self.selection_anchor = Some(index);
    }

    /// Shift-click: select the range from the anchor, replacing the selection.
    pub fn extend_selection(&mut self, index: usize) {
        let anchor = self.selection_anchor.unwrap_or(index);
        let (low, high) = if anchor <= index {
            (anchor, index)
        } else {
            (index, anchor)
        };
        self.selected = (low..=high).collect();
        self.selection_anchor.get_or_insert(anchor);
    }

    pub fn clear_selection(&mut self) {
        self.selected.clear();
        self.selection_anchor = None;
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

    /// Confirm deletion of every selected entry in one dialog.
    pub fn begin_delete_many(&mut self, entries: &[RemoteEntry]) -> bool {
        if entries.is_empty() {
            return true;
        }
        if entries.len() == 1 {
            return self.begin_delete(&entries[0]);
        }
        if !self.can_start_operation() {
            return true;
        }
        self.dialog = Some(SftpDialog {
            kind: SftpDialogKind::DeleteMany {
                paths: entries
                    .iter()
                    .map(|entry| (entry.path.clone(), entry.is_dir))
                    .collect(),
                names: entries.iter().map(|entry| entry.name.clone()).collect(),
            },
            name: format!("{} items", entries.len()),
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
            SftpDialogKind::DeleteMany { paths, names } => {
                let count = names.len();
                let Some(rx) = ssh.sftp_delete_many(paths) else {
                    dialog.error = Some("the SFTP session is no longer available".to_string());
                    self.dialog = Some(dialog);
                    self.mark_unavailable(ssh);
                    return true;
                };
                self.start_operation(
                    format!("Deleting {count} items…"),
                    OnSuccess::Refresh { verb: "Deleted" },
                    rx,
                );
            }
        }
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

    fn start_list(&mut self, ssh: &SshSession, path: String, intent: NavIntent) {
        self.current = path.clone();
        self.path_edit = path.clone();
        self.status.info(format!("Opening {path}…"));

        if let Some(rx) = ssh.sftp_list(&path) {
            self.loading = true;
            self.pending = Some(Job::List { path, intent, rx });
        } else {
            self.mark_unavailable(ssh);
        }
    }

    /// Resolve and open a symlink from the listing.
    ///
    /// A folder target is navigated to (keeping the link path in the
    /// breadcrumb); a file target goes through the normal "Open with…" picker.
    pub fn open_link(&mut self, ssh: &SshSession, entry: &RemoteEntry) -> bool {
        if !entry.is_symlink {
            return false;
        }
        if !self.can_start_operation() {
            return true;
        }
        // `pending` holds a single job; starting a resolve would drop a listing
        // reply and leave `loading` set forever.
        if self.loading {
            self.status.info("Wait for the current listing to finish");
            return false;
        }
        match ssh.sftp_resolve(&entry.path) {
            Some(rx) => {
                self.status.info(format!("Following {}…", entry.name));
                self.pending = Some(Job::Resolve {
                    entry: entry.clone(),
                    rx,
                });
            }
            None => self.mark_unavailable(ssh),
        }
        true
    }

    /// Target of a symlink resolved earlier, if any.
    pub fn link_target(&self, path: &str) -> Option<&str> {
        self.link_targets.get(path).map(String::as_str)
    }

    /// Ask for the login shell, then open the install dialog.
    pub fn begin_hook_prompt(&mut self, ssh: &SshSession) -> bool {
        self.request_shell(ssh, HookAction::Install)
    }

    /// Type the tracking snippet into the running shell.
    ///
    /// The shell is detected first when unknown, because csh/tcsh need a
    /// different snippet than bash/zsh.
    pub fn begin_session_hook(&mut self, ssh: &SshSession) -> bool {
        self.request_shell(ssh, HookAction::EnableNow)
    }

    fn request_shell(&mut self, ssh: &SshSession, action: HookAction) -> bool {
        if self.unavailable.is_some() {
            return false;
        }
        if self.login_shell.is_some() {
            return match action {
                HookAction::Install => self.open_hook_prompt(),
                HookAction::EnableNow => self.send_session_hook(ssh),
            };
        }
        // Same single-job slot as a listing: do not drop its reply.
        if self.loading {
            self.status.info("Wait for the current listing to finish");
            return false;
        }
        self.pending_hook = Some(action);
        match ssh.sftp_login_shell(&self.user) {
            Some(rx) => {
                self.pending = Some(Job::Shell(rx));
                self.status.info("Detecting your login shell…");
            }
            None => self.mark_unavailable(ssh),
        }
        true
    }

    fn open_hook_prompt(&mut self) -> bool {
        let Some(info) = self.login_shell.clone() else {
            return false;
        };
        self.hook_prompt = Some(HookPrompt {
            path: info.rc,
            shell: info.shell,
            kind: info.kind,
        });
        true
    }

    /// Apply the choice from the shell-hook dialog.
    pub fn submit_hook_prompt(&mut self, ssh: &SshSession, install: bool) -> bool {
        let Some(prompt) = self.hook_prompt.take() else {
            return false;
        };
        match ssh.sftp_edit_hook(prompt.path.clone(), prompt.kind, install) {
            Some(rx) => {
                if install {
                    self.status
                        .info(format!("Installing directory tracking in {}…", prompt.path));
                } else {
                    self.status
                        .info(format!("Removing directory tracking from {}…", prompt.path));
                }
                self.pending = Some(Job::Hook { install, rx });
            }
            None => self.mark_unavailable(ssh),
        }
        true
    }

    pub fn cancel_hook_prompt(&mut self) -> bool {
        self.hook_prompt.take().is_some()
    }

    /// Type the tracking one-liner into the current shell.
    ///
    /// Only useful at a prompt: the bytes go to whatever owns the terminal.
    pub fn send_session_hook(&mut self, ssh: &SshSession) -> bool {
        let Some(kind) = self.login_shell.as_ref().map(|info| info.kind) else {
            return false;
        };
        let mut line = String::from(kind.session_hook());
        line.push('\r');
        if ssh.input_sender().send(line.into_bytes()).is_ok() {
            self.status.info(format!(
                "Sent {} directory tracking to the shell — it applies at the next prompt",
                kind.name()
            ));
            true
        } else {
            self.status
                .error("the SSH session is no longer accepting input");
            false
        }
    }

    fn mark_unavailable(&mut self, ssh: &SshSession) {
        self.loading = false;
        let message = ssh
            .sftp_unavailable()
            .map(str::to_string)
            .unwrap_or_else(|| "the SFTP session is no longer available".to_string());
        self.mark_unavailable_with(message);
    }

    /// Mark the panel unusable with an explicit reason (transport failure).
    pub fn mark_unavailable_with(&mut self, message: impl Into<String>) {
        self.loading = false;
        let message = message.into();
        self.unavailable = Some(message.clone());
        self.status.error(message);
        // Start the 15-minute window for any edit that was open when the
        // connection dropped. The journal itself is kept.
        self.journal.mark_offline(Instant::now());
    }

    /// The whole SSH session is gone: drop transfers and stop offering Reconnect.
    pub fn mark_closed(&mut self) {
        self.transfer = None;
        self.queue.clear();
        self.conflict = None;
        self.pending = None;
        // Nothing can be uploaded any more, and polling local files would
        // only queue transfers that are guaranteed to fail.
        self.edits.clear();
        self.notifier = None;
        self.last_edit_safety = None;
        self.mark_unavailable_with("the SSH connection is closed");
    }

    fn fail(&mut self, message: impl Into<String>) {
        self.loading = false;
        let message = message.into();
        if crate::ssh::is_sftp_timeout(&message) {
            // A timeout is not a property of the path; the SFTP channel is
            // unresponsive. Offer the reconnect path instead of a dead end.
            self.unavailable = Some(crate::ssh::SFTP_TIMEOUT.to_string());
            self.status.error(crate::ssh::SFTP_TIMEOUT);
            return;
        }
        self.status.error(message);
        // Keep the panel consistent with the listing that is still on screen.
        if !self.listed_path.is_empty() {
            self.current = self.listed_path.clone();
            self.path_edit = self.listed_path.clone();
        }
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
            owner: "alice".to_string(),
            permissions: None,
            modified: None,
        }
    }

    #[test]
    fn selection_replaces_toggles_and_extends() {
        let mut explorer = SftpExplorer {
            entries: vec![entry("a", false), entry("b", false), entry("c", true)],
            ..SftpExplorer::default()
        };

        explorer.select_row(1);
        assert_eq!(explorer.selected_entries().len(), 1);
        assert_eq!(explorer.selected_entry().unwrap().name, "b");

        explorer.toggle_row(0);
        assert_eq!(explorer.selection_count(), 2);
        // More than one row: no single "selected entry".
        assert!(explorer.selected_entry().is_none());

        explorer.toggle_row(0);
        assert_eq!(explorer.selection_count(), 1);

        // Shift extends from the last plain click (row 1).
        explorer.select_row(1);
        explorer.extend_selection(2);
        assert_eq!(
            explorer
                .selected_entries()
                .iter()
                .map(|e| &e.name)
                .collect::<Vec<_>>(),
            ["b", "c"]
        );

        explorer.clear_selection();
        assert_eq!(explorer.selection_count(), 0);
        assert!(explorer.selection_anchor.is_none());
    }

    #[test]
    fn history_records_moves_and_drops_duplicates() {
        let mut history = SftpHistory::default();
        history.visit("/home", NavIntent::Initial);
        assert_eq!(history.current, "/home");
        assert!(!history.can_back());

        history.visit("/home/src", NavIntent::User);
        history.visit("/home/src", NavIntent::User);
        assert_eq!(history.back, ["/home"]);
        assert_eq!(history.current, "/home/src");

        // Refreshing is not a move.
        history.visit("/home/src", NavIntent::Refresh);
        assert_eq!(history.back, ["/home"]);
        assert!(!history.can_forward());
    }

    #[test]
    fn history_back_and_forward_swap_stacks() {
        let mut history = SftpHistory::default();
        history.visit("/a", NavIntent::Initial);
        history.visit("/b", NavIntent::User);
        history.visit("/c", NavIntent::User);

        let back = history.back_target().unwrap();
        assert_eq!(back, "/b");
        history.visit(&back, NavIntent::Back);
        assert_eq!(history.current, "/b");
        assert!(history.can_forward());
        assert_eq!(history.forward_target().as_deref(), Some("/c"));

        // A new move after going back clears the forward stack.
        history.visit("/d", NavIntent::User);
        assert!(!history.can_forward());
        assert_eq!(history.back, ["/a", "/b"]);
    }

    #[test]
    fn history_is_capped() {
        let mut history = SftpHistory::default();
        history.visit("/0", NavIntent::Initial);
        for step in 1..=(HISTORY_LIMIT + 20) {
            history.visit(&format!("/{step}"), NavIntent::User);
        }
        assert_eq!(history.back.len(), HISTORY_LIMIT);
        assert_eq!(history.current, format!("/{}", HISTORY_LIMIT + 20));
        // The oldest entries fell off, the newest one is still there.
        let expected = format!("/{}", HISTORY_LIMIT + 19);
        assert_eq!(
            history.back.last().map(String::as_str),
            Some(expected.as_str())
        );
    }

    #[test]
    fn session_hook_is_one_line_and_parses_in_bash() {
        let hook = ShellKind::Bash.session_hook();
        assert!(!hook.contains('\n'));
        let bash = Path::new("/bin/bash");
        if !bash.exists() {
            return;
        }
        let status = Command::new(bash)
            .arg("-n")
            .arg("-c")
            .arg(hook)
            .status()
            .unwrap();
        assert!(status.success(), "bash -n rejected the session hook");
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
