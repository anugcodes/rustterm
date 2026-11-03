use super::*;

/// How long the local copies opened in an editor stay quiet after a filesystem
/// event before the save is uploaded. Editors emit several events per write.
const EDIT_DEBOUNCE: Duration = Duration::from_millis(150);

/// Safety pass while a file is open: catches events inotify can miss (network
/// mounts, watcher limits) and notices a deleted temporary copy.
const EDIT_SAFETY_INTERVAL: Duration = Duration::from_secs(5);

/// How long a missing local file is tolerated. Editors that save atomically
/// replace the file, so it is briefly absent.
const EDIT_GONE_GRACE: Duration = Duration::from_secs(12);

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
    pub entries: Vec<RemoteEntry>,
    pub choice: EditorChoice,
    pub custom: String,
    /// Re-upload the local copy whenever the editor saves it.
    pub auto_upload: bool,
    pub error: Option<String>,
}

/// Size and modification time of a local file. Cheap to read and enough to
/// confirm that an event really was a save.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FileStamp {
    modified: Option<SystemTime>,
    len: u64,
}

impl FileStamp {
    pub(super) fn of(path: &Path) -> Option<Self> {
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
pub(super) struct EditWatch {
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
pub(super) struct EditNotifier {
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
pub(super) struct Waker(Option<Wake>);

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
    pub(super) synced: bool,
}

impl EditJournal {
    pub(super) fn for_profile(profile: &str) -> Self {
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
    pub(super) fn mark_uploaded(&mut self, local: &Path, stamp: FileStamp) {
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

impl SftpExplorer {
    /// Open an already-downloaded copy and (optionally) watch it for saves.
    pub(super) fn launch_local_copy(
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
    pub(super) fn sync_journal(&mut self, ssh: &SshSession) -> bool {
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
    pub(super) fn poll_edits(&mut self, ssh: &SshSession) -> bool {
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

    /// Open the "Open with…" dialog for a file.
    pub fn begin_open_with(&mut self, entries: &[RemoteEntry]) -> bool {
        if entries.is_empty() {
            return true;
        }
        if entries.iter().any(|entry| entry.is_dir) {
            self.status
                .error("Folders cannot be opened in a local editor");
            return true;
        }
        if !self.can_start_operation() {
            return true;
        }
        self.editor_picker = Some(EditorPicker {
            entries: entries.to_vec(),
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
        if picker.entries.len() > 1 {
            self.open_many_in_editor(ssh, &picker.entries, picker.choice, custom);
        } else if let Some(entry) = picker.entries.first() {
            self.open_in_editor(ssh, entry, picker.choice, custom, picker.auto_upload);
        }
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

    /// Download several files and open each in `editor`.
    ///
    /// Auto-upload and the edit journal only track single files, so a batch
    /// opens copies without watching them.
    fn open_many_in_editor(
        &mut self,
        ssh: &SshSession,
        entries: &[RemoteEntry],
        editor: EditorChoice,
        custom: Option<String>,
    ) -> bool {
        if entries.iter().any(|entry| entry.is_dir || entry.is_symlink) {
            self.status
                .error("Only regular files can be opened in an editor");
            return true;
        }
        if !self.can_start_operation() {
            return true;
        }
        let mut files = Vec::with_capacity(entries.len());
        let mut locals = Vec::with_capacity(entries.len());
        for entry in entries {
            let local = match temp_download_path(&entry.name) {
                Ok(path) => path,
                Err(err) => {
                    self.status.error(err);
                    return true;
                }
            };
            files.push((entry.path.clone(), local.clone()));
            locals.push(local);
        }
        let Some(rx) = ssh.sftp_fetch_many(files) else {
            self.mark_unavailable(ssh);
            return true;
        };
        self.start_operation(
            format!("Downloading {} files…", entries.len()),
            OnSuccess::LaunchEditors {
                editor,
                custom,
                locals,
            },
            rx,
        );
        true
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

pub(super) fn launch_editor(
    choice: EditorChoice,
    custom: Option<&str>,
    path: &Path,
) -> Result<(), String> {
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
}
