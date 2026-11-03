use super::*;

/// How a transfer treats a destination that already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictPolicy {
    /// Replace an existing file and merge into an existing folder.
    Overwrite,
    /// Write next to the existing entry under a unique name (`name (copy)`).
    KeepBoth,
}

/// Live progress of one upload or download.
///
/// The SSH task updates atomics per chunk and the UI reads them once per frame,
/// so progress reporting costs no allocations and needs no channel of its own.
/// The one-shot completion result still travels over the usual `std_mpsc`
/// receiver, which keeps the existing `Operation` plumbing intact.
#[derive(Debug)]
pub struct TransferProgress {
    label: String,
    bytes_done: AtomicU64,
    bytes_total: AtomicU64,
    items_done: AtomicU32,
    items_total: AtomicU32,
    cancel: AtomicBool,
}

impl TransferProgress {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            bytes_done: AtomicU64::new(0),
            bytes_total: AtomicU64::new(0),
            items_done: AtomicU32::new(0),
            items_total: AtomicU32::new(0),
            cancel: AtomicBool::new(false),
        }
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn set_total_bytes(&self, total: u64) {
        self.bytes_total.store(total, Ordering::Relaxed);
    }

    pub fn add_bytes(&self, bytes: u64) {
        self.bytes_done.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn bytes_done(&self) -> u64 {
        self.bytes_done.load(Ordering::Relaxed)
    }

    /// `None` while the pre-walk has not finished or the transfer is empty.
    pub fn total_bytes(&self) -> Option<u64> {
        match self.bytes_total.load(Ordering::Relaxed) {
            0 => None,
            total => Some(total),
        }
    }

    pub fn set_total_items(&self, total: u32) {
        self.items_total.store(total, Ordering::Relaxed);
    }

    pub fn item_done(&self) {
        self.items_done.fetch_add(1, Ordering::Relaxed);
    }

    pub fn items_done(&self) -> u32 {
        self.items_done.load(Ordering::Relaxed)
    }

    pub fn total_items(&self) -> Option<u32> {
        match self.items_total.load(Ordering::Relaxed) {
            0 => None,
            total => Some(total),
        }
    }

    /// Byte-based fraction in `0.0..=1.0`, or `None` when the size is unknown.
    pub fn fraction(&self) -> Option<f32> {
        let total = self.total_bytes()?;
        if total == 0 {
            return None;
        }
        Some((self.bytes_done() as f32 / total as f32).clamp(0.0, 1.0))
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// Copy `from` into `dest_dir`, giving the copy a unique name.
pub(crate) async fn copy_into(
    client: &SftpClient,
    from: &str,
    dest_dir: &str,
) -> Result<String, String> {
    let metadata = client
        .session
        .symlink_metadata(from)
        .await
        .map_err(|err| format!("could not inspect '{from}': {err}"))?;
    let is_dir = metadata.file_type().is_dir();

    // Copying a directory into itself would recurse forever while the tree
    // grows underneath the walk.
    let from_trimmed = from.trim_end_matches('/');
    let dest_trimmed = dest_dir.trim_end_matches('/');
    if is_dir
        && (dest_trimmed == from_trimmed || dest_trimmed.starts_with(&format!("{from_trimmed}/")))
    {
        return Err(format!(
            "cannot copy folder '{from}' into itself; choose a different destination"
        ));
    }

    let source_name = from_trimmed
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty() && *name != "." && *name != "..")
        .unwrap_or("copy");
    let destination = unique_copy_path(client, dest_dir, source_name).await?;
    if let Err(err) = copy_tree(client, from, &destination).await {
        // Do not leave a half-copied tree behind.
        let _ = remove_tree(client, &destination, is_dir).await;
        return Err(err);
    }
    Ok(destination)
}

pub(crate) async fn unique_copy_path(
    client: &SftpClient,
    dest_dir: &str,
    source_name: &str,
) -> Result<String, String> {
    for attempt in 1..1000 {
        let name = if attempt == 1 {
            format!("{source_name} (copy)")
        } else {
            format!("{source_name} (copy {attempt})")
        };
        let candidate = join_remote_path(dest_dir, &name);
        match client.session.try_exists(&candidate).await {
            Ok(false) => return Ok(candidate),
            Ok(true) => {}
            Err(err) => return Err(format!("could not check '{candidate}': {err}")),
        }
    }
    Err(format!(
        "could not find a free copy name for '{source_name}' in '{dest_dir}'"
    ))
}

/// Recursively copy a file, directory tree or symlink.
///
/// Files stream through `tokio::io::copy`; SFTP has no server-side copy
/// primitive. Symlinks are recreated with their original target.
pub(crate) async fn copy_tree(client: &SftpClient, from: &str, to: &str) -> Result<(), String> {
    let metadata = client
        .session
        .symlink_metadata(from)
        .await
        .map_err(|err| format!("could not inspect '{from}': {err}"))?;
    let file_type = metadata.file_type();

    if file_type.is_symlink() {
        let target = client
            .session
            .read_link(from)
            .await
            .map_err(|err| format!("could not read symlink '{from}': {err}"))?;
        // russh-sftp follows OpenSSH's argument order: target first, link path
        // second.
        return client
            .session
            .symlink(&target, to)
            .await
            .map_err(|err| format!("could not create symlink '{to}': {err}"));
    }

    if file_type.is_dir() {
        client
            .session
            .create_dir(to)
            .await
            .map_err(|err| format!("could not create folder '{to}': {err}"))?;
        let entries = client
            .session
            .read_dir(from)
            .await
            .map_err(|err| format!("could not read folder '{from}': {err}"))?;
        for entry in entries {
            let child_from = entry.path();
            let child_to = join_remote_path(to, &entry.file_name());
            Box::pin(copy_tree(client, &child_from, &child_to)).await?;
        }
        return Ok(());
    }

    copy_file(client, from, to).await
}

pub(crate) async fn copy_file(client: &SftpClient, from: &str, to: &str) -> Result<(), String> {
    let mut source = client
        .session
        .open(from)
        .await
        .map_err(|err| format!("could not open '{from}' for reading: {err}"))?;
    let mut destination = client
        .session
        .create(to)
        .await
        .map_err(|err| format!("could not create '{to}': {err}"))?;

    let mut buffer = vec![0u8; TRANSFER_CHUNK];
    let copied = loop {
        match source.read(&mut buffer).await {
            Ok(0) => break Ok(()),
            Ok(read) => {
                if let Err(err) = destination.write_all(&buffer[..read]).await {
                    break Err(err);
                }
            }
            Err(err) => break Err(err),
        }
    };
    let flushed = destination.flush().await;
    let closed_source = source.close().await;
    let closed_destination = destination.close().await;

    if let Err(err) = copied {
        let _ = client.session.remove_file(to).await;
        return Err(format!("could not copy '{from}' to '{to}': {err}"));
    }
    flushed.map_err(|err| format!("could not finish writing '{to}': {err}"))?;
    closed_source.map_err(|err| format!("could not close '{from}': {err}"))?;
    closed_destination.map_err(|err| format!("could not close '{to}': {err}"))?;
    Ok(())
}

pub(crate) async fn fetch_file(
    client: &SftpClient,
    remote: &str,
    local: &Path,
) -> Result<String, String> {
    // Reuse the chunked downloader so the editor path gets the same 64 KiB
    // streaming, temp-file cleanup and atomic rename as a panel download.
    let progress = TransferProgress::new(remote);
    let part = local_temp_path(local);
    if let Err(err) = download_file_inner(client, remote, local, &part, &progress).await {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(err);
    }
    Ok(local.display().to_string())
}

/// Download several files for the editor path. Stops at the first failure so a
/// partial batch never gets opened as if it were complete.
pub(crate) async fn fetch_files(
    client: &SftpClient,
    files: Vec<(String, PathBuf)>,
) -> Result<String, String> {
    let count = files.len();
    for (remote, local) in files {
        fetch_file(client, &remote, &local).await?;
    }
    Ok(format!("{count} files"))
}

/// Chunk size for streamed transfers. 64 KiB matches the SFTP read/write
/// pipeline well without making cancellation feel unresponsive.
const TRANSFER_CHUNK: usize = 64 * 1024;

/// Distinguishes concurrent transfers aimed at the same target. Without it a
/// manual transfer and an editor auto-upload share one `.part` path and
/// overwrite each other mid-flight.
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Temp sibling for a remote target. It lives in the same directory so the
/// final `rename` never crosses a filesystem boundary.
pub(crate) fn remote_temp_path(target: &str) -> String {
    format!(
        "{target}.rustterm-{}-{}.part",
        std::process::id(),
        TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// Temp sibling for a local download target.
pub(crate) fn local_temp_path(target: &Path) -> PathBuf {
    let mut name = target.as_os_str().to_os_string();
    name.push(format!(
        ".rustterm-{}-{}.part",
        std::process::id(),
        TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    PathBuf::from(name)
}

/// Message used when the SSH transport disappeared mid-transfer, instead of
/// leaking library strings such as `sender dropped` into the status bar.
pub(crate) const CONNECTION_LOST: &str =
    "the SSH connection closed during the transfer — reconnect and retry";

/// Whether an error means the transport is gone rather than one file failing.
pub(crate) fn is_connection_loss(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "sender dropped",
        "session closed",
        "channel closed",
        "broken pipe",
        "connection reset",
        "connection aborted",
        "not connected",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

pub(crate) fn normalise_transfer_error(message: String) -> String {
    if is_connection_loss(&message) {
        CONNECTION_LOST.to_string()
    } else if is_sftp_timeout(&message) {
        SFTP_TIMEOUT.to_string()
    } else {
        message
    }
}

/// Message shown when an SFTP request gets no answer in time.
pub(crate) const SFTP_TIMEOUT: &str =
    "the server did not answer the SFTP request (30 s timeout) — try Reconnect SFTP";

/// Whether an error is russh-sftp's request timeout rather than a real file
/// error. Slow network mounts can produce this on a healthy connection.
pub(crate) fn is_sftp_timeout(message: &str) -> bool {
    message.to_ascii_lowercase().contains("timeout")
}

/// Turn a chained connect error into one actionable sentence.
///
/// The raw library text stays useful as a tooltip, but on its own it hides the
/// cause ("TCP connection failed: Connection refused (os error 111)").
pub(crate) fn explain_connect_error(message: &str) -> String {
    let lower = message.to_ascii_lowercase();
    // A changed host key already carries a full warning; never shorten it.
    if lower.contains("host key changed") {
        return message.to_string();
    }
    if lower.contains("password: rejected by server") {
        return "Login failed — the server rejected this username or password.".to_string();
    }
    if lower.contains("connection refused") {
        return "Connection refused — nothing is listening on that host and port.".to_string();
    }
    if lower.contains("timed out") || lower.contains("timeout") {
        return "Connection timed out — the host did not answer. Check the host, port and network."
            .to_string();
    }
    if lower.contains("failed to lookup address")
        || lower.contains("name or service not known")
        || lower.contains("nodename nor servname")
    {
        return "Host not found — check the hostname.".to_string();
    }
    if lower.contains("network is unreachable") {
        return "Network unreachable — check this machine's connection.".to_string();
    }
    message.to_string()
}

pub(crate) struct UploadItem {
    local: PathBuf,
    remote: String,
    kind: LocalKind,
    size: u64,
}

pub(crate) enum LocalKind {
    File,
    Dir,
    Symlink(PathBuf),
}

/// Recursively describe a local tree before uploading it.
///
/// The pre-walk is what makes a progress bar possible: without it the byte
/// total is unknown until the transfer ends.
pub(crate) fn collect_upload(
    local: &Path,
    remote: &str,
    out: &mut Vec<UploadItem>,
) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(local)
        .map_err(|err| format!("could not inspect '{}': {err}", local.display()))?;
    let file_type = metadata.file_type();

    if file_type.is_symlink() {
        let target = std::fs::read_link(local)
            .map_err(|err| format!("could not read symlink '{}': {err}", local.display()))?;
        out.push(UploadItem {
            local: local.to_path_buf(),
            remote: remote.to_string(),
            kind: LocalKind::Symlink(target),
            size: 0,
        });
        return Ok(());
    }

    if file_type.is_dir() {
        out.push(UploadItem {
            local: local.to_path_buf(),
            remote: remote.to_string(),
            kind: LocalKind::Dir,
            size: 0,
        });
        let entries = std::fs::read_dir(local)
            .map_err(|err| format!("could not read '{}': {err}", local.display()))?;
        let mut children: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .collect();
        children.sort();
        for child in children {
            let name = child
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default();
            collect_upload(&child, &join_remote_path(remote, &name), out)?;
        }
        return Ok(());
    }

    out.push(UploadItem {
        local: local.to_path_buf(),
        remote: remote.to_string(),
        kind: LocalKind::File,
        size: metadata.len(),
    });
    Ok(())
}

pub(crate) async fn ensure_remote_dir(client: &SftpClient, path: &str) -> Result<(), String> {
    match client.session.try_exists(path).await {
        Ok(true) => Ok(()),
        Ok(false) => client
            .session
            .create_dir(path)
            .await
            .map_err(|err| format!("could not create folder '{path}': {err}")),
        Err(err) => Err(format!("could not check '{path}': {err}")),
    }
}

pub(crate) async fn upload_symlink(
    client: &SftpClient,
    target: &Path,
    remote: &str,
) -> Result<(), String> {
    let target = target.to_string_lossy().to_string();
    if client.session.try_exists(remote).await.unwrap_or(false) {
        let _ = client.session.remove_file(remote).await;
    }
    // Same reversed argument order as `copy_tree`.
    client
        .session
        .symlink(&target, remote)
        .await
        .map_err(|err| format!("could not create symlink '{remote}': {err}"))
}

pub(crate) async fn upload_file(
    client: &SftpClient,
    local: &Path,
    remote: &str,
    progress: &TransferProgress,
) -> Result<(), String> {
    let part = remote_temp_path(remote);
    let result = upload_file_inner(client, local, remote, &part, progress).await;
    if result.is_err() {
        let _ = client.session.remove_file(&part).await;
    }
    result
}

/// Move `part` onto `target` without ever leaving `target` missing.
///
/// OpenSSH's `rename` replaces an existing file, but not every server allows
/// that. In that case the old file is moved aside first and restored if the
/// new one cannot be put in place — the previous version is never deleted
/// before the replacement is known to have succeeded.
pub(crate) async fn replace_remote_file(
    client: &SftpClient,
    part: &str,
    target: &str,
) -> Result<(), String> {
    if let Err(first) = client.session.rename(part, target).await {
        if !client.session.try_exists(target).await.unwrap_or(false) {
            return Err(format!("could not finalise '{target}': {first}"));
        }
    } else {
        return Ok(());
    }

    let backup = remote_temp_path(target);
    client
        .session
        .rename(target, &backup)
        .await
        .map_err(|err| format!("could not move the existing '{target}' aside: {err}"))?;
    match client.session.rename(part, target).await {
        Ok(()) => {
            let _ = client.session.remove_file(&backup).await;
            Ok(())
        }
        Err(err) => {
            if client.session.rename(&backup, target).await.is_ok() {
                Err(format!("could not finalise '{target}': {err}"))
            } else {
                Err(format!(
                    "could not finalise '{target}': {err} (the previous file is kept at '{backup}')"
                ))
            }
        }
    }
}

pub(crate) async fn upload_file_inner(
    client: &SftpClient,
    local: &Path,
    remote: &str,
    part: &str,
    progress: &TransferProgress,
) -> Result<(), String> {
    log::trace!("upload_file_inner: {} -> {}", local.display(), remote);
    let mut source = tokio::fs::File::open(local)
        .await
        .map_err(|err| format!("could not open '{}': {err}", local.display()))?;
    let mut destination = client
        .session
        .create(part)
        .await
        .map_err(|err| format!("could not create '{part}': {err}"))?;

    let mut buffer = vec![0u8; TRANSFER_CHUNK];
    let mut failure: Option<String> = None;
    loop {
        if progress.is_cancelled() {
            failure = Some("Transfer cancelled".to_string());
            break;
        }
        let read = match source.read(&mut buffer).await {
            Ok(read) => read,
            Err(err) => {
                failure = Some(format!("could not read '{}': {err}", local.display()));
                break;
            }
        };
        if read == 0 {
            break;
        }
        if let Err(err) = destination.write_all(&buffer[..read]).await {
            failure = Some(format!("could not write '{remote}': {err}"));
            break;
        }
        progress.add_bytes(read as u64);
    }

    if failure.is_none() {
        if let Err(err) = destination.flush().await {
            failure = Some(format!("could not finish writing '{remote}': {err}"));
        }
    }
    // Always close, even after a write error: an unclosed handle keeps a
    // descriptor open on the server until the channel ends, which adds up over
    // a folder with many files.
    let closed = destination.close().await;
    if let Some(err) = failure {
        return Err(err);
    }
    closed.map_err(|err| format!("could not close '{part}': {err}"))?;

    // OpenSSH's rename replaces, but not every server does. `replace_remote_file`
    // moves the old file aside instead of deleting it, so a failed retry can
    // never leave the destination empty.
    replace_remote_file(client, part, remote).await?;
    Ok(())
}

pub(crate) async fn upload_items(
    client: &SftpClient,
    local: Vec<PathBuf>,
    remote_dir: &str,
    policy: ConflictPolicy,
    progress: &TransferProgress,
) -> Result<String, String> {
    let mut plan = Vec::new();
    for path in &local {
        let Some(name) = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
        else {
            return Err(format!("'{}' has no file name", path.display()));
        };
        let destination = join_remote_path(remote_dir, &name);
        let destination = match policy {
            ConflictPolicy::Overwrite => destination,
            ConflictPolicy::KeepBoth => match client.session.try_exists(&destination).await {
                Ok(true) => unique_copy_path(client, remote_dir, &name).await?,
                Ok(false) => destination,
                Err(err) => return Err(format!("could not check '{destination}': {err}")),
            },
        };
        let local_path = path.clone();
        // Walking a large local tree can take seconds. On the session's
        // current-thread runtime that would stall terminal I/O and the SFTP
        // reader, so keep the pre-walk off the async thread.
        let scanned = tokio::task::spawn_blocking(move || {
            let mut items = Vec::new();
            collect_upload(&local_path, &destination, &mut items)?;
            Ok::<_, String>(items)
        })
        .await
        .map_err(|err| format!("could not scan '{}': {err}", path.display()))??;
        plan.extend(scanned);
    }

    let total_bytes: u64 = plan
        .iter()
        .filter(|item| matches!(&item.kind, LocalKind::File))
        .map(|item| item.size)
        .sum();
    let total_files = plan
        .iter()
        .filter(|item| matches!(&item.kind, LocalKind::File))
        .count();
    progress.set_total_bytes(total_bytes);
    progress.set_total_items(total_files as u32);

    let mut failures = Vec::new();
    let mut uploaded = 0usize;
    for item in &plan {
        if progress.is_cancelled() {
            return Err("Transfer cancelled".to_string());
        }
        let result = match &item.kind {
            LocalKind::Dir => ensure_remote_dir(client, &item.remote).await,
            LocalKind::Symlink(target) => upload_symlink(client, target, &item.remote).await,
            LocalKind::File => upload_file(client, &item.local, &item.remote, progress).await,
        };
        match result {
            Ok(()) => {
                if matches!(&item.kind, LocalKind::File) {
                    progress.item_done();
                    uploaded += 1;
                }
            }
            Err(err) => {
                if is_connection_loss(&err) {
                    // The transport is gone; every later item would fail too.
                    return Err(err);
                }
                failures.push(err);
            }
        }
    }

    if progress.is_cancelled() {
        return Err("Transfer cancelled".to_string());
    }
    if !failures.is_empty() {
        return Err(summarise_failures(failures));
    }
    Ok(format!("{uploaded} file(s) to {remote_dir}"))
}

pub(crate) struct DownloadItem {
    remote: String,
    local: PathBuf,
    kind: RemoteKind,
    size: u64,
}

pub(crate) enum RemoteKind {
    File,
    Dir,
    Symlink(String),
}

/// Walk one remote entry into `out`, using attributes that are already known.
///
/// The panel's listing and every `READDIR` reply carry the file attributes, so
/// a download walk costs one request per directory instead of an extra `stat`
/// per entry — which is what made the first click time out on a slow network
/// mount.
pub(crate) async fn collect_download(
    client: &SftpClient,
    remote: &str,
    local: PathBuf,
    is_dir: bool,
    is_symlink: bool,
    size: u64,
    out: &mut Vec<DownloadItem>,
) -> Result<(), String> {
    if is_symlink {
        let target = client
            .session
            .read_link(remote)
            .await
            .map_err(|err| format!("could not read symlink '{remote}': {err}"))?;
        out.push(DownloadItem {
            remote: remote.to_string(),
            local,
            kind: RemoteKind::Symlink(target),
            size: 0,
        });
        return Ok(());
    }

    if is_dir {
        out.push(DownloadItem {
            remote: remote.to_string(),
            local: local.clone(),
            kind: RemoteKind::Dir,
            size: 0,
        });
        let entries = client
            .session
            .read_dir(remote)
            .await
            .map_err(|err| format!("could not read folder '{remote}': {err}"))?;
        let mut children: Vec<(String, String, bool, bool, u64)> = entries
            .map(|entry| {
                let metadata = entry.metadata();
                let file_type = metadata.file_type();
                (
                    entry.file_name(),
                    entry.path(),
                    file_type.is_dir(),
                    file_type.is_symlink(),
                    metadata.len(),
                )
            })
            .collect();
        children.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, path, child_is_dir, child_is_symlink, child_size) in children {
            Box::pin(collect_download(
                client,
                &path,
                local.join(&name),
                child_is_dir,
                child_is_symlink,
                child_size,
                out,
            ))
            .await?;
        }
        return Ok(());
    }

    out.push(DownloadItem {
        remote: remote.to_string(),
        local,
        kind: RemoteKind::File,
        size,
    });
    Ok(())
}

pub(crate) fn download_symlink(target: &str, local: &Path) -> Result<(), String> {
    if local.symlink_metadata().is_ok() {
        let _ = std::fs::remove_file(local);
    }
    std::os::unix::fs::symlink(target, local).map_err(|err| {
        format!(
            "could not create symlink '{}' -> '{target}': {err}",
            local.display()
        )
    })
}

pub(crate) async fn download_file(
    client: &SftpClient,
    remote: &str,
    local: &Path,
    progress: &TransferProgress,
) -> Result<(), String> {
    let part = local_temp_path(local);
    let result = download_file_inner(client, remote, local, &part, progress).await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&part).await;
    }
    result
}

pub(crate) async fn download_file_inner(
    client: &SftpClient,
    remote: &str,
    local: &Path,
    part: &Path,
    progress: &TransferProgress,
) -> Result<(), String> {
    log::trace!("download_file_inner: {} -> {}", remote, local.display());
    let mut source = client
        .session
        .open(remote)
        .await
        .map_err(|err| format!("could not open '{remote}': {err}"))?;
    let mut destination = tokio::fs::File::create(part)
        .await
        .map_err(|err| format!("could not create '{}': {err}", part.display()))?;

    let mut buffer = vec![0u8; TRANSFER_CHUNK];
    let mut failure: Option<String> = None;
    loop {
        if progress.is_cancelled() {
            failure = Some("Transfer cancelled".to_string());
            break;
        }
        let read = match source.read(&mut buffer).await {
            Ok(read) => read,
            Err(err) => {
                failure = Some(format!("could not read '{remote}': {err}"));
                break;
            }
        };
        if read == 0 {
            break;
        }
        if let Err(err) = destination.write_all(&buffer[..read]).await {
            failure = Some(format!("could not write '{}': {err}", part.display()));
            break;
        }
        progress.add_bytes(read as u64);
    }

    if failure.is_none() {
        if let Err(err) = destination.flush().await {
            failure = Some(format!(
                "could not finish writing '{}': {err}",
                part.display()
            ));
        }
    }
    drop(destination);
    // Fire-and-forget the close; don't block on server acknowledgment.
    tokio::spawn(async move {
        let _ = source.close().await;
    });
    if let Some(err) = failure {
        return Err(err);
    }
    // `rename` replaces an existing file on Unix, so an overwrite is atomic.
    std::fs::rename(part, local)
        .map_err(|err| format!("could not finalise '{}': {err}", local.display()))
}

pub(crate) fn unique_local_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    for attempt in 1..1000 {
        let candidate_name = if attempt == 1 {
            format!("{name} (copy)")
        } else {
            format!("{name} (copy {attempt})")
        };
        let candidate = dir.join(candidate_name);
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(name)
}

pub(crate) fn remote_file_name(path: &str) -> &str {
    path.rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or(path)
}

pub(crate) fn summarise_failures(mut failures: Vec<String>) -> String {
    if failures.len() == 1 {
        failures.remove(0)
    } else {
        let first = failures.first().cloned().unwrap_or_default();
        format!("{} items failed: {first}", failures.len())
    }
}

pub(crate) async fn download_items(
    client: &SftpClient,
    remote: Vec<RemoteEntry>,
    local_dir: &Path,
    policy: ConflictPolicy,
    progress: &TransferProgress,
) -> Result<String, String> {
    let mut plan = Vec::new();
    for entry in &remote {
        let name = remote_file_name(&entry.path).to_string();
        let local = local_dir.join(&name);
        let local = match policy {
            ConflictPolicy::Overwrite => local,
            ConflictPolicy::KeepBoth => {
                if local.exists() {
                    unique_local_path(local_dir, &name)
                } else {
                    local
                }
            }
        };
        Box::pin(collect_download(
            client,
            &entry.path,
            local,
            entry.is_dir,
            entry.is_symlink,
            entry.size,
            &mut plan,
        ))
        .await?;
    }

    let total_bytes: u64 = plan
        .iter()
        .filter(|item| matches!(&item.kind, RemoteKind::File))
        .map(|item| item.size)
        .sum();
    let total_files = plan
        .iter()
        .filter(|item| matches!(&item.kind, RemoteKind::File))
        .count();
    progress.set_total_bytes(total_bytes);
    progress.set_total_items(total_files as u32);

    let mut failures = Vec::new();
    let mut downloaded = 0usize;
    for item in &plan {
        if progress.is_cancelled() {
            return Err("Transfer cancelled".to_string());
        }
        let result = match &item.kind {
            RemoteKind::Dir => tokio::fs::create_dir_all(&item.local)
                .await
                .map_err(|err| format!("could not create '{}': {err}", item.local.display())),
            RemoteKind::Symlink(target) => download_symlink(target, &item.local),
            RemoteKind::File => download_file(client, &item.remote, &item.local, progress).await,
        };
        match result {
            Ok(()) => {
                if matches!(&item.kind, RemoteKind::File) {
                    progress.item_done();
                    downloaded += 1;
                }
            }
            Err(err) => {
                if is_connection_loss(&err) {
                    return Err(err);
                }
                failures.push(err);
            }
        }
    }

    if progress.is_cancelled() {
        return Err("Transfer cancelled".to_string());
    }
    if !failures.is_empty() {
        return Err(summarise_failures(failures));
    }
    Ok(format!("{downloaded} file(s) to {}", local_dir.display()))
}

pub(crate) async fn remove_tree(
    client: &SftpClient,
    path: &str,
    is_dir: bool,
) -> Result<(), String> {
    if !is_dir {
        return client
            .session
            .remove_file(path)
            .await
            .map_err(|err| format!("could not delete '{path}': {err}"));
    }

    let entries = client
        .session
        .read_dir(path)
        .await
        .map_err(|err| format!("could not read folder '{path}': {err}"))?;
    for entry in entries {
        let child = entry.path();
        // Use lstat: a readdir listing may follow symlinks, and recursively
        // deleting through a symlink would remove the target's contents.
        let child_is_dir = match client.session.symlink_metadata(&child).await {
            Ok(metadata) => metadata.file_type().is_dir(),
            Err(_) => false,
        };
        Box::pin(remove_tree(client, &child, child_is_dir)).await?;
    }
    client
        .session
        .remove_dir(path)
        .await
        .map_err(|err| format!("could not delete folder '{path}': {err}"))
}
