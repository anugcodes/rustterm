use super::*;

/// What a transfer does, independent of how conflicts are resolved.
#[derive(Debug)]
pub(super) enum TransferKind {
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
    pub(super) kind: TransferKind,
    pub(super) policy: ConflictPolicy,
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

impl SftpExplorer {
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

    pub(super) fn enqueue_or_ask(
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

    pub(super) fn poll_transfer(&mut self, ssh: &SshSession) -> bool {
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
}
