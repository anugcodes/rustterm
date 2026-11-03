//! State for the SFTP side panel.
//!
//! The panel is per session: each SSH tab owns an `SftpExplorer`, so switching
//! tabs shows that session's remote directory. All network work stays in
//! `ssh.rs`; this module tracks the current path, the last listing, clipboard
//! contents, dialogs and the in-flight request or file operation.

use std::collections::{HashMap, VecDeque};
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

mod paths;
pub use paths::*;

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
    /// Re-upload the local copy whenever the editor saves it.
    pub auto_upload: bool,
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

/// What a transfer does, independent of how conflicts are resolved.
#[derive(Debug)]
enum TransferKind {
    Upload {
        local: Vec<PathBuf>,
        remote_dir: String,
    },
    Download {
        remote: Vec<RemoteEntry>,
        local_dir: PathBuf,
    },
    /// One local file to an exact remote path (editor save).
    UploadOne { local: PathBuf, remote: String },
}

/// One transfer waiting for its turn.
#[derive(Debug)]
pub struct TransferRequest {
    pub label: String,
    kind: TransferKind,
    policy: ConflictPolicy,
}

/// A destination conflict that needs a decision before the transfer starts.
#[derive(Debug)]
pub struct ConflictPrompt {
    /// Names that already exist at the destination (top level only).
    pub names: Vec<String>,
    pub label: String,
    request: TransferRequest,
}

/// The transfer currently running.
#[derive(Debug)]
pub struct ActiveTransfer {
    pub progress: Arc<TransferProgress>,
    rx: std_mpsc::Receiver<Result<String, String>>,
    verb: &'static str,
    /// Local copy of an editor upload plus the metadata it had when the upload
    /// started, so success clears exactly the version that was sent.
    upload: Option<(PathBuf, FileStamp)>,
}

/// How long the local copies opened in an editor stay quiet after a filesystem
/// event before the save is uploaded. Editors emit several events per write.
const EDIT_DEBOUNCE: Duration = Duration::from_millis(150);

/// Safety pass while a file is open: catches events inotify can miss (network
/// mounts, watcher limits) and notices a deleted temporary copy.
const EDIT_SAFETY_INTERVAL: Duration = Duration::from_secs(5);

/// How long a missing local file is tolerated. Editors that save atomically
/// replace the file, so it is briefly absent.
const EDIT_GONE_GRACE: Duration = Duration::from_secs(12);

/// Size and modification time of a local file. Cheap to read and enough to
/// confirm that an event really was a save.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    modified: Option<SystemTime>,
    len: u64,
}

impl FileStamp {
    fn of(path: &Path) -> Option<Self> {
        let metadata = std::fs::metadata(path).ok()?;
        Some(Self {
            modified: metadata.modified().ok(),
            len: metadata.len(),
        })
    }
}

/// What one check of a watched file found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatchState {
    Unchanged,
    /// The file changed and should be uploaded.
    Changed,
    /// The local copy has been gone long enough to stop watching it.
    Gone,
}

/// A local copy of a remote file that is open in an editor.
#[derive(Debug)]
struct EditWatch {
    local: PathBuf,
    remote: String,
    name: String,
    /// State at the last upload (or at launch), to detect the next save.
    stamp: Option<FileStamp>,
    /// When the watcher last reported activity for this file.
    dirty_at: Option<Instant>,
    /// When the local copy was first found missing.
    missing_since: Option<Instant>,
}

impl EditWatch {
    fn new(local: PathBuf, remote: String, name: String) -> Self {
        let stamp = FileStamp::of(&local);
        Self {
            local,
            remote,
            name,
            stamp,
            dirty_at: None,
            missing_since: None,
        }
    }

    /// A filesystem event mentioned this file; start the quiet period.
    fn touch(&mut self, now: Instant) {
        self.dirty_at = Some(now);
        self.missing_since = None;
    }

    /// Whether a filesystem event has settled long enough to be uploaded.
    fn settled(&self, now: Instant) -> bool {
        self.dirty_at
            .is_some_and(|at| now.duration_since(at) >= EDIT_DEBOUNCE)
    }

    fn check(&mut self, now: Instant) -> WatchState {
        let Some(stamp) = FileStamp::of(&self.local) else {
            // A file that momentarily disappears (atomic save) is not an
            // error and is not uploaded; the next pass sees the replacement.
            let since = *self.missing_since.get_or_insert(now);
            if now.duration_since(since) >= EDIT_GONE_GRACE {
                return WatchState::Gone;
            }
            return WatchState::Unchanged;
        };
        self.missing_since = None;
        if Some(stamp) == self.stamp {
            self.dirty_at = None;
            return WatchState::Unchanged;
        }
        self.stamp = Some(stamp);
        self.dirty_at = None;
        WatchState::Changed
    }
}

/// Inotify watcher behind the save detection.
///
/// One watcher is shared by every open copy. It watches the *parent directory*
/// of each file because most editors save by writing a temporary file and
/// renaming it over the original, which would silently break a watch on the
/// file itself.
struct EditNotifier {
    watcher: notify::RecommendedWatcher,
    events: std_mpsc::Receiver<notify::Event>,
    dirs: std::collections::HashSet<PathBuf>,
}

impl std::fmt::Debug for EditNotifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EditNotifier")
            .field("dirs", &self.dirs)
            .finish()
    }
}

impl EditNotifier {
    fn new(wake: Option<Wake>) -> Option<Self> {
        let (tx, events) = std_mpsc::channel();
        let watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
            if let Ok(event) = result {
                let _ = tx.send(event);
                if let Some(wake) = &wake {
                    wake();
                }
            }
        })
        .ok()?;
        Some(Self {
            watcher,
            events,
            dirs: std::collections::HashSet::new(),
        })
    }

    /// Watch the folder containing `local`, once per folder.
    fn watch_parent(&mut self, local: &Path) -> bool {
        let Some(dir) = local.parent() else {
            return false;
        };
        let dir = dir.to_path_buf();
        if self.dirs.contains(&dir) {
            return true;
        }
        if self
            .watcher
            .watch(&dir, notify::RecursiveMode::NonRecursive)
            .is_err()
        {
            return false;
        }
        self.dirs.insert(dir);
        true
    }
}

/// The wake callback handed to the SFTP panel, kept behind a manual `Debug`
/// impl so `SftpExplorer` can still derive it.
#[derive(Default)]
struct Waker(Option<Wake>);

impl std::fmt::Debug for Waker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Waker")
    }
}

/// How long unsynced local edits remain eligible after the connection drops.
const EDIT_SYNC_WINDOW: Duration = Duration::from_secs(15 * 60);

/// One local copy that may still need to be uploaded.
#[derive(Debug, Clone)]
struct PendingEdit {
    local: PathBuf,
    remote: String,
    name: String,
    /// Stamp the server is known to have, from the last successful upload.
    uploaded: Option<FileStamp>,
    /// When the connection was lost, if it is currently down.
    offline_since: Option<Instant>,
}

/// Local copies opened in an editor that outlive a connection.
///
/// `EditWatch` only lives while the SFTP panel is alive. This journal is kept
/// by the app and handed to the next session for the same profile, so a save
/// made while offline is picked up on reconnect instead of being lost.
#[derive(Debug, Default)]
pub struct EditJournal {
    profile: String,
    entries: Vec<PendingEdit>,
    /// Whether the entries were already offered to the current connection.
    synced: bool,
}

impl EditJournal {
    fn for_profile(profile: &str) -> Self {
        Self {
            profile: profile.to_string(),
            ..Self::default()
        }
    }

    pub fn profile(&self) -> &str {
        &self.profile
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Re-open the local copy for `remote` if it still has unsynced changes.
    fn local_copy(&self, remote: &str) -> Option<PathBuf> {
        self.entries
            .iter()
            .find(|entry| {
                entry.remote == remote
                    && entry.local.exists()
                    && FileStamp::of(&entry.local) != entry.uploaded
            })
            .map(|entry| entry.local.clone())
    }

    /// Remember a file that was just opened in an editor.
    ///
    /// Re-opening the same remote path keeps the previously uploaded stamp, so
    /// changes made while the connection was down still count as unsynced.
    fn record(&mut self, local: &Path, remote: &str, name: &str) {
        if let Some(entry) = self.entries.iter_mut().find(|seen| seen.local == local) {
            entry.remote = remote.to_string();
            entry.name = name.to_string();
            return;
        }
        // A fresh download supersedes any older copy of the same remote path.
        self.entries.retain(|entry| entry.remote != remote);
        self.entries.push(PendingEdit {
            local: local.to_path_buf(),
            remote: remote.to_string(),
            name: name.to_string(),
            uploaded: FileStamp::of(local),
            offline_since: None,
        });
    }

    /// Record the exact version the server received.
    ///
    /// A save that landed while the upload was running leaves the file's
    /// current stamp different from `stamp`, so the change stays pending and is
    /// retried instead of being silently dropped.
    fn mark_uploaded(&mut self, local: &Path, stamp: FileStamp) {
        if let Some(entry) = self.entries.iter_mut().find(|seen| seen.local == local) {
            entry.uploaded = Some(stamp);
            entry.offline_since = None;
        }
    }

    fn forget(&mut self, local: &Path) {
        self.entries.retain(|entry| entry.local != local);
    }

    /// Mark every entry as waiting for a working connection.
    pub fn mark_offline(&mut self, now: Instant) {
        for entry in &mut self.entries {
            entry.offline_since.get_or_insert(now);
        }
    }

    /// Drop entries that have been offline longer than `EDIT_SYNC_WINDOW`.
    /// Returns their names so the caller can tell the user.
    pub fn expire(&mut self, now: Instant) -> Vec<String> {
        let mut expired = Vec::new();
        self.entries.retain(|entry| {
            let stale = entry
                .offline_since
                .is_some_and(|since| now.duration_since(since) >= EDIT_SYNC_WINDOW);
            if stale {
                expired.push(entry.name.clone());
            }
            !stale
        });
        expired
    }

    /// Files whose local copy differs from what the server was last given.
    ///
    /// A file that no longer exists locally is not pending; it is dropped.
    fn pending(&mut self, now: Instant) -> Vec<(PathBuf, String, String)> {
        self.entries.retain(|entry| entry.local.exists());
        for entry in &mut self.entries {
            entry.offline_since.get_or_insert(now);
        }
        self.entries
            .iter()
            .filter(|entry| FileStamp::of(&entry.local) != entry.uploaded)
            .map(|entry| {
                (
                    entry.local.clone(),
                    entry.remote.clone(),
                    entry.name.clone(),
                )
            })
            .collect()
    }
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
                        self.begin_open_with(&entry);
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
        }
    }

    /// Open an already-downloaded copy and (optionally) watch it for saves.
    fn launch_local_copy(
        &mut self,
        editor: EditorChoice,
        custom: Option<String>,
        local: PathBuf,
        remote: String,
        name: String,
        auto_upload: bool,
    ) {
        let label = editor_label(editor, custom.as_deref());
        match launch_editor(editor, custom.as_deref(), &local) {
            Ok(()) => {
                if auto_upload {
                    // Watch from the state right after launch, so an editor
                    // that touches the file on open does not trigger an
                    // immediate re-upload. Opening the same copy twice replaces
                    // the earlier watch instead of uploading it twice per save.
                    self.watch_edit(local.clone(), remote, name.clone());
                    self.status
                        .success(format!("Opened {name} in {label} — saving uploads it back"));
                } else {
                    self.status.success(format!(
                        "Opened {name} in {label} (local copy: {})",
                        local.display()
                    ));
                }
            }
            Err(err) => {
                self.status
                    .error(format!("{err} (the download is at {})", local.display()));
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

    pub fn selected_entry(&self) -> Option<&RemoteEntry> {
        self.selected.and_then(|index| self.entries.get(index))
    }

    pub fn is_busy(&self) -> bool {
        self.operation.is_some() || self.transfer.is_some() || !self.queue.is_empty()
    }

    pub fn busy_label(&self) -> Option<&str> {
        self.operation
            .as_ref()
            .map(|op| op.label.as_str())
            .or_else(|| self.transfer_label())
    }

    // ---- Transfers -------------------------------------------------------

    /// Queue a download of `entries` into `local_dir`.
    ///
    /// Existing local files are detected synchronously; when any exist the UI
    /// shows a conflict prompt before anything is written.
    pub fn download_entries(
        &mut self,
        ssh: &SshSession,
        entries: &[RemoteEntry],
        local_dir: PathBuf,
    ) -> bool {
        if entries.is_empty() {
            self.status.error("Select a file or folder first");
            return true;
        }
        let conflicts: Vec<String> = entries
            .iter()
            .map(|entry| entry.name.clone())
            .filter(|name| local_dir.join(name).exists())
            .collect();
        let label = if entries.len() == 1 {
            format!("Downloading {}…", entries[0].name)
        } else {
            format!("Downloading {} items…", entries.len())
        };
        let request = TransferRequest {
            label,
            kind: TransferKind::Download {
                remote: entries.to_vec(),
                local_dir,
            },
            policy: ConflictPolicy::Overwrite,
        };
        self.enqueue_or_ask(ssh, request, conflicts);
        true
    }

    /// Queue an upload of local `paths` into the current remote directory.
    pub fn upload_paths(&mut self, ssh: &SshSession, paths: Vec<PathBuf>) -> bool {
        let remote_dir = if self.current.is_empty() {
            self.listed_path.clone()
        } else {
            self.current.clone()
        };
        self.upload_into(ssh, remote_dir, paths)
    }

    /// Queue an upload into a specific directory (row menu "Upload here…").
    pub fn upload_into(
        &mut self,
        ssh: &SshSession,
        remote_dir: String,
        paths: Vec<PathBuf>,
    ) -> bool {
        if paths.is_empty() {
            return true;
        }
        if remote_dir.is_empty() {
            self.status.error("Open a remote folder first");
            return true;
        }
        let names: Vec<String> = paths
            .iter()
            .filter_map(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().to_string())
            })
            .collect();
        // The current listing is a reliable-enough conflict source for the
        // top-level items; nested conflicts are handled by the policy.
        let conflicts: Vec<String> = if remote_dir == self.current {
            names
                .iter()
                .filter(|name| self.entries.iter().any(|entry| &entry.name == *name))
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        let label = if paths.len() == 1 {
            format!(
                "Uploading {}…",
                names.first().cloned().unwrap_or_else(|| "item".to_string())
            )
        } else {
            format!("Uploading {} items…", paths.len())
        };
        let request = TransferRequest {
            label,
            kind: TransferKind::Upload {
                local: paths,
                remote_dir,
            },
            policy: ConflictPolicy::Overwrite,
        };
        self.enqueue_or_ask(ssh, request, conflicts);
        true
    }

    /// Answer the conflict prompt. `None` cancels the pending transfer.
    pub fn resolve_conflict(&mut self, ssh: &SshSession, policy: Option<ConflictPolicy>) -> bool {
        let Some(prompt) = self.conflict.take() else {
            return false;
        };
        match policy {
            Some(policy) => {
                let mut request = prompt.request;
                request.policy = policy;
                self.queue.push_back(request);
                self.start_next_transfer(ssh);
            }
            None => self.status.info("Transfer cancelled"),
        }
        true
    }

    /// Cancel the running transfer, if any, and drop everything still queued.
    pub fn cancel_transfer(&mut self) -> bool {
        let queued = self.queue.len();
        self.queue.clear();
        if let Some(transfer) = &self.transfer {
            transfer.progress.cancel();
            if queued > 0 {
                self.status
                    .info(format!("Cancelling 1 running and {queued} queued"));
            }
            return true;
        }
        if queued > 0 {
            self.status
                .info(format!("Cancelled {queued} queued transfer(s)"));
            return true;
        }
        false
    }

    pub fn transfer_progress(&self) -> Option<&TransferProgress> {
        self.transfer
            .as_ref()
            .map(|transfer| transfer.progress.as_ref())
    }

    pub fn transfer_label(&self) -> Option<&str> {
        self.transfer
            .as_ref()
            .map(|transfer| transfer.progress.label())
    }

    pub fn queued_transfers(&self) -> usize {
        self.queue.len()
    }

    fn enqueue_or_ask(
        &mut self,
        ssh: &SshSession,
        request: TransferRequest,
        conflicts: Vec<String>,
    ) {
        if conflicts.is_empty() {
            self.queue.push_back(request);
            self.start_next_transfer(ssh);
        } else {
            self.conflict = Some(ConflictPrompt {
                names: conflicts,
                label: request.label.clone(),
                request,
            });
        }
    }

    fn start_next_transfer(&mut self, ssh: &SshSession) -> bool {
        if self.transfer.is_some() {
            return false;
        }
        let Some(request) = self.queue.pop_front() else {
            return false;
        };
        let progress = Arc::new(TransferProgress::new(request.label.clone()));
        // Capture the stamp *before* the transfer starts. If the file changes
        // at any point after this, the journal keeps it pending rather than
        // recording a version the server never received.
        let upload = match &request.kind {
            TransferKind::UploadOne { local, .. } => {
                FileStamp::of(local).map(|stamp| (local.clone(), stamp))
            }
            _ => None,
        };
        let (verb, receiver) = match &request.kind {
            TransferKind::Upload { local, remote_dir } => (
                "Uploaded",
                ssh.sftp_upload(
                    local.clone(),
                    remote_dir,
                    request.policy,
                    Arc::clone(&progress),
                ),
            ),
            TransferKind::Download { remote, local_dir } => (
                "Downloaded",
                ssh.sftp_download(
                    remote.clone(),
                    local_dir.clone(),
                    request.policy,
                    Arc::clone(&progress),
                ),
            ),
            TransferKind::UploadOne { local, remote } => (
                "Uploaded",
                ssh.sftp_upload_one(local.clone(), remote.clone(), Arc::clone(&progress)),
            ),
        };
        let Some(rx) = receiver else {
            self.mark_unavailable(ssh);
            return false;
        };
        self.transfer = Some(ActiveTransfer {
            progress,
            rx,
            verb,
            upload,
        });
        true
    }

    fn poll_transfer(&mut self, ssh: &SshSession) -> bool {
        let Some(active) = self.transfer.take() else {
            return false;
        };
        match active.rx.try_recv() {
            Ok(Ok(detail)) => {
                if let Some((local, stamp)) = &active.upload {
                    // Clear only the version that was sent. A save made while
                    // the upload was running keeps its journal entry pending.
                    self.journal.mark_uploaded(local, *stamp);
                }
                self.status.success(format!("{} {detail}", active.verb));
                self.refresh(ssh);
                self.start_next_transfer(ssh);
                true
            }
            Ok(Err(err)) => {
                if err == "Transfer cancelled" {
                    self.status.info("Transfer cancelled");
                    self.start_next_transfer(ssh);
                } else if err == crate::ssh::CONNECTION_LOST {
                    // The transport is gone: every queued transfer would fail
                    // the same way, and Reconnect SFTP is the only recovery.
                    self.queue.clear();
                    self.mark_unavailable_with(err);
                } else if crate::ssh::is_sftp_timeout(&err) {
                    self.queue.clear();
                    self.mark_unavailable_with(crate::ssh::SFTP_TIMEOUT);
                } else {
                    self.status.error(err);
                    self.start_next_transfer(ssh);
                }
                true
            }
            Err(std_mpsc::TryRecvError::Empty) => {
                self.transfer = Some(active);
                false
            }
            Err(std_mpsc::TryRecvError::Disconnected) => {
                self.queue.clear();
                self.mark_unavailable_with(
                    "the SSH connection closed during the transfer — reconnect and retry",
                );
                true
            }
        }
    }

    /// Whether any opened file is being watched for saves.
    ///
    /// The UI keeps a slow safety repaint alive while this is true. Save events
    /// themselves wake egui through the watcher callback.
    pub fn watches_edits(&self) -> bool {
        !self.edits.is_empty()
    }

    /// Whether a save event is still inside its quiet period. The UI uses this
    /// to schedule one short repaint instead of a permanent timer.
    pub fn pending_edit_settle(&self) -> bool {
        self.edits.iter().any(|watch| watch.dirty_at.is_some())
    }

    /// Give the panel a way to wake egui when a save event arrives.
    pub fn attach_wake(&mut self, wake: Wake) {
        if self.wake.0.is_none() {
            self.wake.0 = Some(wake);
        }
    }

    /// Detach the journal so it can survive this connection.
    pub fn take_journal(&mut self) -> EditJournal {
        let mut journal = std::mem::take(&mut self.journal);
        journal.mark_offline(Instant::now());
        journal
    }

    /// Adopt the journal of an earlier session for the same profile.
    pub fn install_journal(&mut self, journal: EditJournal) {
        if journal.profile == self.journal.profile {
            self.journal = journal;
            self.journal.synced = false;
        }
    }

    /// Upload every journal entry whose local copy changed while offline.
    fn sync_journal(&mut self, ssh: &SshSession) -> bool {
        let pending = self.journal.pending(Instant::now());
        let mut activity = false;
        for (local, remote, name) in pending {
            // The live watch saw the same save while the channel was down;
            // align it so the safety pass does not upload it a second time.
            if let Some(watch) = self.edits.iter_mut().find(|watch| watch.local == local) {
                watch.stamp = FileStamp::of(&local);
                watch.dirty_at = None;
            }
            self.status.info(format!(
                "Uploading {name} (changes kept from before the disconnect)…"
            ));
            let request = TransferRequest {
                label: format!("Uploading {name}…"),
                kind: TransferKind::UploadOne { local, remote },
                policy: ConflictPolicy::Overwrite,
            };
            self.enqueue_or_ask(ssh, request, Vec::new());
            activity = true;
        }
        activity
    }

    /// Start watching a local copy that an editor has opened.
    fn watch_edit(&mut self, local: PathBuf, remote: String, name: String) {
        let watch = EditWatch::new(local.clone(), remote.clone(), name.clone());
        match self.edits.iter_mut().find(|seen| seen.local == local) {
            Some(seen) => *seen = watch,
            None => self.edits.push(watch),
        }
        self.journal.record(&local, &remote, &name);
        if self.notifier.is_none() {
            match EditNotifier::new(self.wake.0.clone()) {
                Some(notifier) => self.notifier = Some(notifier),
                None => {
                    log::warn!("file watcher unavailable; falling back to the safety pass");
                    self.status.info(
                        "File watching is unavailable — saves are detected by a periodic check",
                    );
                }
            }
        }
        if let Some(notifier) = &mut self.notifier {
            if !notifier.watch_parent(&local) {
                log::warn!("could not watch {}", local.display());
            }
        }
    }

    /// Drain watcher events, then upload every save that has settled.
    ///
    /// Events are the primary trigger; the `EDIT_SAFETY_INTERVAL` pass covers
    /// watcher misses and notices deleted temporary copies. Returns `true` when
    /// the UI should redraw.
    fn poll_edits(&mut self, ssh: &SshSession) -> bool {
        if self.edits.is_empty() {
            return false;
        }
        // While the channel is down, the journal keeps the changes and the
        // reconnect sync uploads them; trying now would only fail every save.
        if self.unavailable.is_some() {
            return false;
        }
        let now = Instant::now();

        // 1. Mark every file named by a watcher event as dirty. Events are
        //    matched by path, so unrelated activity in the folder is ignored.
        if let Some(notifier) = &self.notifier {
            while let Ok(event) = notifier.events.try_recv() {
                for path in &event.paths {
                    for watch in &mut self.edits {
                        if watch.local == *path {
                            watch.touch(now);
                        }
                    }
                }
            }
        }

        // 2. The safety pass runs on its own slower clock.
        let safety_due = self
            .last_edit_safety
            .is_none_or(|last| now.duration_since(last) >= EDIT_SAFETY_INTERVAL);
        if safety_due {
            self.last_edit_safety = Some(now);
        }

        // An editor may save several times in a row; each change found here is
        // queued in order, so the last save always wins.
        let mut changed: Vec<(PathBuf, String, String)> = Vec::new();
        let mut gone: Vec<PathBuf> = Vec::new();
        for watch in &mut self.edits {
            if !safety_due && !watch.settled(now) {
                continue;
            }
            match watch.check(now) {
                WatchState::Changed => {
                    changed.push((
                        watch.local.clone(),
                        watch.remote.clone(),
                        watch.name.clone(),
                    ));
                }
                WatchState::Gone => gone.push(watch.local.clone()),
                WatchState::Unchanged => {}
            }
        }
        // Deleting the temp copy (or closing an editor that removes it) ends
        // the watch, so the UI can stop its safety timer.
        if !gone.is_empty() {
            for local in &gone {
                self.journal.forget(local);
            }
            self.edits.retain(|watch| !gone.contains(&watch.local));
            if self.edits.is_empty() {
                self.notifier = None;
                self.last_edit_safety = None;
            }
        }

        let mut activity = !changed.is_empty();
        for (local, remote, name) in changed {
            self.status
                .info(format!("Uploading {name} (saved in the editor)…"));
            let request = TransferRequest {
                label: format!("Uploading {name}…"),
                kind: TransferKind::UploadOne { local, remote },
                policy: ConflictPolicy::Overwrite,
            };
            self.enqueue_or_ask(ssh, request, Vec::new());
            activity = true;
        }
        activity
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
            auto_upload: self.last_auto_upload,
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
        self.last_auto_upload = picker.auto_upload;
        let custom = (picker.choice == EditorChoice::Custom).then(|| picker.custom.clone());
        self.open_in_editor(
            ssh,
            &picker.entry,
            picker.choice,
            custom,
            picker.auto_upload,
        );
        true
    }

    /// Download `entry` to a private temp directory and launch `editor`.
    pub fn open_in_editor(
        &mut self,
        ssh: &SshSession,
        entry: &RemoteEntry,
        editor: EditorChoice,
        custom: Option<String>,
        auto_upload: bool,
    ) -> bool {
        if entry.is_dir {
            self.status
                .error("Folders cannot be opened in a local editor");
            return true;
        }
        if !self.can_start_operation() {
            return true;
        }
        // A copy with unsynced changes from an earlier connection is opened
        // as-is; downloading over it would throw those changes away.
        if let Some(local) = self.journal.local_copy(&entry.path) {
            self.status.info(format!(
                "Opening {} with changes that have not reached the server",
                entry.name
            ));
            self.launch_local_copy(
                editor,
                custom,
                local,
                entry.path.clone(),
                entry.name.clone(),
                auto_upload,
            );
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
                remote: entry.path.clone(),
                name: entry.name.clone(),
                auto_upload,
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

/// POSIX path join for remote entries.
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

    #[test]
    fn cancelling_stops_the_running_transfer_and_clears_the_queue() {
        let mut explorer = SftpExplorer::default();
        let (tx, rx) = std_mpsc::channel();
        let progress = Arc::new(TransferProgress::new("Uploading notes.txt"));
        explorer.transfer = Some(ActiveTransfer {
            progress: Arc::clone(&progress),
            rx,
            verb: "Uploaded",
            upload: None,
        });
        for name in ["a.txt", "b.txt"] {
            explorer.queue.push_back(TransferRequest {
                label: format!("Uploading {name}"),
                kind: TransferKind::UploadOne {
                    local: PathBuf::from(name),
                    remote: format!("/remote/{name}"),
                },
                policy: ConflictPolicy::Overwrite,
            });
        }
        let _keep_tx = tx;

        assert!(explorer.cancel_transfer());
        assert!(progress.is_cancelled());
        assert_eq!(explorer.queued_transfers(), 0);
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

    /// Small RAII temp directory so the watch tests do not need a new crate.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!("rustterm-{tag}-{unique}"));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn watch_for(dir: &TempDir, name: &str) -> EditWatch {
        let local = dir.0.join(name);
        EditWatch::new(local, format!("/remote/{name}"), name.to_string())
    }

    #[test]
    fn edit_watch_reports_each_save_once() {
        let dir = TempDir::new("edit-watch");
        std::fs::write(dir.0.join("notes.txt"), "one").unwrap();
        let mut watch = watch_for(&dir, "notes.txt");
        let now = Instant::now();

        assert_eq!(watch.check(now), WatchState::Unchanged, "no save yet");

        std::fs::write(dir.0.join("notes.txt"), "two").unwrap();
        assert_eq!(watch.check(now), WatchState::Changed, "the write is a save");
        assert_eq!(watch.check(now), WatchState::Unchanged, "uploaded once");

        // Editors that write and rename (or touch the file) change the stamp
        // without changing the length.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(dir.0.join("notes.txt"), "TWO!").unwrap();
        assert_eq!(watch.check(now), WatchState::Changed, "a second save");

        // A vanished file is tolerated for a moment (atomic save)...
        std::fs::remove_file(dir.0.join("notes.txt")).unwrap();
        assert_eq!(watch.check(now), WatchState::Unchanged);
        // ...and the watch is dropped once it stays gone.
        assert_eq!(
            watch.check(now + EDIT_GONE_GRACE + Duration::from_secs(1)),
            WatchState::Gone
        );
    }

    #[test]
    fn save_events_wait_for_the_debounce() {
        let dir = TempDir::new("edit-debounce");
        std::fs::write(dir.0.join("a.txt"), "a").unwrap();
        let mut watch = watch_for(&dir, "a.txt");
        let now = Instant::now();
        assert!(!watch.settled(now), "nothing happened yet");

        watch.touch(now);
        assert!(!watch.settled(now), "an event just arrived");
        assert!(watch.settled(now + EDIT_DEBOUNCE), "quiet period elapsed");
        assert_eq!(watch.check(now + EDIT_DEBOUNCE), WatchState::Unchanged);
        assert!(!watch.settled(now + EDIT_DEBOUNCE), "settled check clears");
    }

    #[test]
    fn watcher_reports_a_write_to_the_watched_file() {
        let dir = TempDir::new("edit-notify");
        let local = dir.0.join("live.txt");
        std::fs::write(&local, "one").unwrap();

        let mut notifier = EditNotifier::new(None).expect("watch support");
        assert!(notifier.watch_parent(&local));
        std::fs::write(&local, "two").unwrap();

        // inotify delivery is asynchronous; retry briefly.
        let mut seen = false;
        for _ in 0..100 {
            while let Ok(event) = notifier.events.try_recv() {
                if event.paths.iter().any(|path| path == &local) {
                    seen = true;
                }
            }
            if seen {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(seen, "no event for {}", local.display());
    }

    #[test]
    fn journal_syncs_only_files_changed_since_the_last_upload() {
        let dir = TempDir::new("edit-journal");
        let local = dir.0.join("notes.txt");
        std::fs::write(&local, "one").unwrap();
        let mut journal = EditJournal::for_profile("me@host:22");
        let now = Instant::now();
        journal.record(&local, "/remote/notes.txt", "notes.txt");
        assert!(journal.pending(now).is_empty(), "nothing changed yet");

        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&local, "two").unwrap();
        let pending = journal.pending(now);
        assert_eq!(pending.len(), 1, "the save is pending");
        assert_eq!(pending[0].2, "notes.txt");

        // Once the server has the new version, nothing is pending any more.
        journal.mark_uploaded(&local, FileStamp::of(&local).unwrap());
        assert!(journal.pending(now).is_empty());
    }

    #[test]
    fn journal_keeps_a_save_made_during_an_upload() {
        let dir = TempDir::new("edit-journal-during-upload");
        let local = dir.0.join("notes.txt");
        std::fs::write(&local, "one").unwrap();
        let mut journal = EditJournal::for_profile("me@host:22");
        let now = Instant::now();
        journal.record(&local, "/remote/notes.txt", "notes.txt");

        // Version the upload actually read.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&local, "two-two").unwrap();
        let sent = FileStamp::of(&local).unwrap();

        // The user saves again while that upload is still in flight.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&local, "three-three-three").unwrap();

        journal.mark_uploaded(&local, sent);
        assert_eq!(
            journal.pending(now).len(),
            1,
            "a save made during the upload must stay pending"
        );

        journal.mark_uploaded(&local, FileStamp::of(&local).unwrap());
        assert!(journal.pending(now).is_empty());
    }

    #[test]
    fn journal_keeps_changes_when_the_file_is_reopened() {
        let dir = TempDir::new("edit-journal-reopen");
        let local = dir.0.join("notes.txt");
        std::fs::write(&local, "one").unwrap();
        let mut journal = EditJournal::for_profile("me@host:22");
        let now = Instant::now();
        journal.record(&local, "/remote/notes.txt", "notes.txt");

        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&local, "two").unwrap();
        // Re-opening the same path must not adopt the changed file as the
        // "uploaded" state, or the offline edit would be lost.
        journal.record(&local, "/remote/notes.txt", "notes.txt");
        assert_eq!(journal.pending(now).len(), 1);
    }

    #[test]
    fn journal_expires_after_the_window() {
        let dir = TempDir::new("edit-journal-expiry");
        let local = dir.0.join("notes.txt");
        std::fs::write(&local, "one").unwrap();
        let mut journal = EditJournal::for_profile("me@host:22");
        let now = Instant::now();
        journal.record(&local, "/remote/notes.txt", "notes.txt");
        journal.mark_offline(now);

        // Still inside the window.
        assert!(journal.expire(now + EDIT_SYNC_WINDOW / 2).is_empty());
        // Past it.
        let expired = journal.expire(now + EDIT_SYNC_WINDOW + Duration::from_secs(1));
        assert_eq!(expired, ["notes.txt"]);
        assert!(journal.is_empty());
    }

    #[test]
    fn journal_ignores_files_that_vanished_locally() {
        let dir = TempDir::new("edit-journal-gone");
        let local = dir.0.join("notes.txt");
        std::fs::write(&local, "one").unwrap();
        let mut journal = EditJournal::for_profile("me@host:22");
        journal.record(&local, "/remote/notes.txt", "notes.txt");
        std::fs::remove_file(&local).unwrap();
        assert!(journal.pending(Instant::now()).is_empty());
        assert!(journal.is_empty(), "a deleted copy is dropped");
    }

    #[test]
    fn taking_a_journal_keeps_its_profile_and_pending_changes() {
        let dir = TempDir::new("edit-journal-detach");
        let local = dir.0.join("notes.txt");
        std::fs::write(&local, "one").unwrap();
        let mut explorer = SftpExplorer {
            journal: EditJournal::for_profile("me@host:22"),
            ..SftpExplorer::default()
        };
        explorer
            .journal
            .record(&local, "/remote/notes.txt", "notes.txt");

        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&local, "two-two").unwrap();

        let mut taken = explorer.take_journal();
        assert_eq!(taken.profile(), "me@host:22");
        assert_eq!(taken.pending(Instant::now()).len(), 1);
    }

    #[test]
    fn journal_is_only_installed_for_the_same_profile() {
        let mut explorer = SftpExplorer::default();
        let journal = EditJournal::for_profile("me@host:22");
        explorer.install_journal(journal);
        // The explorer's own journal has an empty profile, so nothing is adopted.
        assert!(explorer.journal.is_empty());
        assert_eq!(explorer.journal.profile(), "");
    }

    #[test]
    fn journal_offers_a_local_copy_only_while_changes_are_unsynced() {
        let dir = TempDir::new("edit-journal-reuse");
        let local = dir.0.join("notes.txt");
        std::fs::write(&local, "one").unwrap();
        let mut journal = EditJournal::for_profile("me@host:22");
        journal.record(&local, "/remote/notes.txt", "notes.txt");
        assert!(journal.local_copy("/remote/notes.txt").is_none());

        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&local, "two").unwrap();
        assert_eq!(journal.local_copy("/remote/notes.txt"), Some(local.clone()));

        journal.mark_uploaded(&local, FileStamp::of(&local).unwrap());
        assert!(journal.local_copy("/remote/notes.txt").is_none());
    }

    #[test]
    fn watchers_are_tracked_until_the_session_ends() {
        let dir = TempDir::new("edit-list");
        std::fs::write(dir.0.join("a.txt"), "a").unwrap();
        let mut explorer = SftpExplorer::default();
        assert!(!explorer.watches_edits());
        explorer.edits.push(watch_for(&dir, "a.txt"));
        assert!(explorer.watches_edits());
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
