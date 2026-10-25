//! SSH session management.
//!
//! One OS thread + one current-thread tokio runtime per session.
//!
//! `SshSession::connect` is *synchronous through authentication*: it blocks
//! the caller on TCP connect + host-key verification + password auth + PTY +
//! shell request. The caller (the app) is expected to run it on a background
//! thread and surface the `Result` to the UI. This makes connect failures real
//! errors instead of text written into a phantom terminal (handoff flaw F1).
//!
//! Each session owns a `disconnect_tx`; sending on it asks the session thread
//! to call `session.disconnect(...)` and exit, so connections are no longer
//! leaked until the server times out (handoff flaw F2).
//!
//! Host keys (handoff flaw F3): `check_server_key` verifies against
//! `~/.ssh/known_hosts`. A known host with a matching key proceeds; an unknown
//! host asks the UI for a trust-on-first-use decision and records the answer;
//! a *changed* key is refused outright, with no way to click through it.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use russh::client::{self, Handle, Msg as ClientMsg};
// `PublicKey` lives in the `key` submodule; the known-hosts helpers are at the
// `keys` root. `HashAlg` does not exist in russh 0.45 — `fingerprint()` takes
// no arguments and hashes with SHA-256 internally.
use russh::keys::agent::client::AgentClient;
use russh::keys::key::PublicKey;
use russh::keys::{check_known_hosts, known_host_keys, learn_known_hosts, load_secret_key};
use russh::{compression, Channel, ChannelMsg};
use russh_sftp::client::{Config as SftpConfig, SftpSession};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, OnceCell};

/// Wakes the egui thread when asynchronous SSH data arrives.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    /// Used as a fallback after key/agent auth, or on its own when they are
    /// disabled. May be empty.
    pub password: String,
    /// Try private-key authentication (auto-detected `~/.ssh/id_*` when
    /// `key_path` is blank).
    pub use_key: bool,
    /// Explicit private-key file. `~` is expanded here.
    pub key_path: String,
    /// Offer identities from `$SSH_AUTH_SOCK`.
    pub use_agent: bool,
}

/// A pending trust-on-first-use decision.
///
/// `check_server_key` runs on the session thread and pauses the handshake until
/// the UI answers. `reply` is the only field the UI touches, through
/// `accept`/`reject`, so `app.rs` never has to name a tokio type.
pub struct HostKeyPrompt {
    pub host: String,
    pub port: u16,
    /// `PublicKey::fingerprint()` already returns the `SHA256:...` form.
    pub fingerprint: String,
    reply: oneshot::Sender<bool>,
}

impl HostKeyPrompt {
    pub fn accept(self) {
        let _ = self.reply.send(true);
    }

    pub fn reject(self) {
        let _ = self.reply.send(false);
    }
}

/// Why a host-key check refused the connection.
///
/// `check_server_key` can only return a bool, so the reason is parked in a slot
/// that `connect_and_auth` reads when `client::connect` comes back with russh's
/// generic handshake error.
enum HostKeyFailure {
    /// The host was unknown and the user declined.
    Rejected { fingerprint: String },
    /// A different key is already recorded for this host: possible MITM.
    Changed { fingerprint: String },
    /// `known_hosts` unreadable/unwritable, or another verification problem.
    Error { message: String },
}

pub struct SshSession {
    input_tx: mpsc::UnboundedSender<Vec<u8>>,
    resize_tx: mpsc::UnboundedSender<(u16, u16)>,
    disconnect_tx: mpsc::UnboundedSender<()>,
    output_rx: std_mpsc::Receiver<Vec<u8>>,
    /// Requests for the SFTP subsystem. Always present, even when SFTP failed
    /// to open, so callers can still get a meaningful error.
    sftp_tx: mpsc::UnboundedSender<SftpRequest>,
    /// Why SFTP could not be opened, if it could not.
    sftp_unavailable: Option<String>,
    /// Wakes egui when an SFTP reply is ready.
    wake: Option<Wake>,
}

/// One entry returned by the remote directory listing.
#[derive(Debug, Clone)]
pub struct RemoteEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    /// Best-effort owner name: `/etc/passwd` lookup, then the name supplied by
    /// the server, then the numeric UID.
    pub owner: String,
    pub modified: Option<SystemTime>,
}

impl SshSession {
    /// Connect, verify the host key, authenticate, request a PTY + shell, then
    /// spawn the I/O loop.
    ///
    /// **Blocks** the calling thread through the whole handshake — including,
    /// possibly, waiting for the user to answer a host-key prompt on
    /// `prompt_tx`. Call this from a background thread.
    pub fn connect(
        config: SshConfig,
        prompt_tx: Option<std_mpsc::Sender<HostKeyPrompt>>,
        wake: Option<Wake>,
    ) -> Result<Self> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to build tokio runtime")?;

        let (session, channel, sftp, sftp_error) = rt
            .block_on(connect_and_auth(&config, prompt_tx))
            .with_context(|| {
                format!("connect to {}@{}:{}", config.user, config.host, config.port)
            })?;
        // The handle is shared so the SFTP reconnect task can open a channel
        // while the session loop keeps ownership for terminal I/O.
        let session = Arc::new(session);

        let (input_tx, input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (resize_tx, resize_rx) = mpsc::unbounded_channel::<(u16, u16)>();
        let (disconnect_tx, disconnect_rx) = mpsc::unbounded_channel::<()>();
        let (output_tx, output_rx) = std_mpsc::channel::<Vec<u8>>();
        let (sftp_tx, sftp_rx) = mpsc::unbounded_channel::<SftpRequest>();
        let session_wake = wake.clone();

        std::thread::Builder::new()
            .name(format!("ssh-{}@{}", config.user, config.host))
            .spawn(move || {
                rt.block_on(run_session(
                    session,
                    channel,
                    sftp,
                    SessionIo {
                        input_rx,
                        resize_rx,
                        disconnect_rx,
                        sftp_rx,
                        output_tx,
                    },
                    wake,
                ));
            })
            .context("failed to spawn SSH session thread")?;

        Ok(Self {
            input_tx,
            resize_tx,
            disconnect_tx,
            output_rx,
            sftp_tx,
            sftp_unavailable: sftp_error,
            wake: session_wake,
        })
    }

    /// Why SFTP is unavailable for this connection, if it is.
    pub fn sftp_unavailable(&self) -> Option<&str> {
        self.sftp_unavailable.as_deref()
    }

    /// Ask the SFTP subsystem for the session's home directory (`.`).
    pub fn sftp_home(&self) -> Option<std_mpsc::Receiver<Result<String, String>>> {
        let (reply, rx) = std_mpsc::channel();
        self.sftp_tx
            .send(SftpRequest {
                action: SftpAction::Home { reply },
                wake: self.wake.clone(),
            })
            .ok()?;
        Some(rx)
    }

    /// Ask for a directory listing. The reply arrives via std mpsc; egui is
    /// woken when it is ready.
    pub fn sftp_list(
        &self,
        path: &str,
    ) -> Option<std_mpsc::Receiver<Result<Vec<RemoteEntry>, String>>> {
        let (reply, rx) = std_mpsc::channel();
        self.sftp_tx
            .send(SftpRequest {
                action: SftpAction::List {
                    path: path.to_string(),
                    reply,
                },
                wake: self.wake.clone(),
            })
            .ok()?;
        Some(rx)
    }

    /// Open a fresh SFTP channel on the existing SSH connection.
    ///
    /// The terminal session is never touched: only the SFTP subsystem is
    /// reopened, which is enough to recover from a dropped channel or from a
    /// server that had no SFTP support at login time.
    pub fn sftp_reconnect(&self) -> Option<std_mpsc::Receiver<Result<(), String>>> {
        let (reply, rx) = std_mpsc::channel();
        self.sftp_tx
            .send(SftpRequest {
                action: SftpAction::Reconnect { reply },
                wake: self.wake.clone(),
            })
            .ok()?;
        Some(rx)
    }

    pub fn sftp_rename(
        &self,
        from: &str,
        to: &str,
    ) -> Option<std_mpsc::Receiver<Result<String, String>>> {
        let (reply, rx) = std_mpsc::channel();
        self.sftp_tx
            .send(SftpRequest {
                action: SftpAction::Rename {
                    from: from.to_string(),
                    to: to.to_string(),
                    reply,
                },
                wake: self.wake.clone(),
            })
            .ok()?;
        Some(rx)
    }

    pub fn sftp_delete(
        &self,
        path: &str,
        is_dir: bool,
    ) -> Option<std_mpsc::Receiver<Result<String, String>>> {
        let (reply, rx) = std_mpsc::channel();
        self.sftp_tx
            .send(SftpRequest {
                action: SftpAction::Delete {
                    path: path.to_string(),
                    is_dir,
                    reply,
                },
                wake: self.wake.clone(),
            })
            .ok()?;
        Some(rx)
    }

    pub fn sftp_create_dir(
        &self,
        path: &str,
    ) -> Option<std_mpsc::Receiver<Result<String, String>>> {
        let (reply, rx) = std_mpsc::channel();
        self.sftp_tx
            .send(SftpRequest {
                action: SftpAction::CreateDir {
                    path: path.to_string(),
                    reply,
                },
                wake: self.wake.clone(),
            })
            .ok()?;
        Some(rx)
    }

    /// Copy `from` into `dest_dir`, choosing a unique name if needed.
    ///
    /// SFTP has no server-side copy operation, so the copy is streamed. The
    /// reply contains the final destination path.
    pub fn sftp_copy(
        &self,
        from: &str,
        dest_dir: &str,
    ) -> Option<std_mpsc::Receiver<Result<String, String>>> {
        let (reply, rx) = std_mpsc::channel();
        self.sftp_tx
            .send(SftpRequest {
                action: SftpAction::Copy {
                    from: from.to_string(),
                    dest_dir: dest_dir.to_string(),
                    reply,
                },
                wake: self.wake.clone(),
            })
            .ok()?;
        Some(rx)
    }

    /// Download a remote file to a local path (used by "open in editor").
    pub fn sftp_fetch(
        &self,
        remote: &str,
        local: PathBuf,
    ) -> Option<std_mpsc::Receiver<Result<String, String>>> {
        let (reply, rx) = std_mpsc::channel();
        self.sftp_tx
            .send(SftpRequest {
                action: SftpAction::Fetch {
                    remote: remote.to_string(),
                    local,
                    reply,
                },
                wake: self.wake.clone(),
            })
            .ok()?;
        Some(rx)
    }

    pub fn write(&self, bytes: Vec<u8>) {
        let _ = self.input_tx.send(bytes);
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.resize_tx.send((cols, rows));
    }

    /// Ask the session thread to disconnect and exit. Idempotent.
    pub fn disconnect(&self) {
        let _ = self.disconnect_tx.send(());
    }

    /// Drain one pending output chunk, if any.
    pub fn try_read(&self) -> Option<Vec<u8>> {
        self.output_rx.try_recv().ok()
    }

    /// Clone of the PTY input sender, for components that need to write bytes
    /// back to the remote — primarily the terminal's `EventProxy`, which answers
    /// DA1/DA2/DSR queries with `PtyWrite`.
    pub fn input_sender(&self) -> mpsc::UnboundedSender<Vec<u8>> {
        self.input_tx.clone()
    }
}

/// Build the SSH client configuration.
///
/// Keep russh's modern defaults first, but append two legacy fallbacks for old
/// OpenSSH servers. In particular, OpenSSH 5.3 only offers SHA-1 key exchange
/// (`diffie-hellman-group14-sha1`) and an `ssh-rsa` host key. Modern servers
/// still negotiate the preferred algorithms because the client's list order is
/// preserved.
fn client_config() -> client::Config {
    let mut preferred = russh::Preferred::DEFAULT.clone();

    let mut kex = preferred.kex.into_owned();
    kex.push(russh::kex::DH_G14_SHA1);
    preferred.kex = Cow::Owned(kex);

    let mut key = preferred.key.into_owned();
    key.push(russh::keys::key::SSH_RSA);
    preferred.key = Cow::Owned(key);

    // Prefer OpenSSH's "delayed" zlib (`zlib@openssh.com`), matching `ssh -C`.
    // `russh` only starts compressing after authentication for both zlib names,
    // so this behaves like OpenSSH's delayed mode. The client's list order is
    // what gets negotiated, so putting the legacy name first makes OpenSSH
    // servers choose it; plain `zlib` stays as a fallback for servers that do
    // not implement the OpenSSH extension.
    let mut compress = preferred.compression.into_owned();
    compress.retain(|name| *name != compression::ZLIB_LEGACY);
    compress.insert(0, compression::ZLIB_LEGACY);
    preferred.compression = Cow::Owned(compress);

    client::Config {
        preferred,
        ..client::Config::default()
    }
}

async fn connect_and_auth(
    config: &SshConfig,
    prompt_tx: Option<std_mpsc::Sender<HostKeyPrompt>>,
) -> Result<(
    Handle<ClientHandler>,
    Channel<ClientMsg>,
    Option<SftpSession>,
    Option<String>,
)> {
    let ssh_config = Arc::new(client_config());
    let addr = (config.host.as_str(), config.port);

    // The handler is moved into `client::connect`, so the reason for a
    // rejection has to live outside it.
    let failure: Arc<Mutex<Option<HostKeyFailure>>> = Arc::new(Mutex::new(None));
    let handler = ClientHandler {
        host: config.host.clone(),
        port: config.port,
        prompt_tx,
        failure: Arc::clone(&failure),
    };

    // `client::connect` does not set TCP_NODELAY. On high-latency links Nagle's
    // algorithm then delays small keystrokes by hundreds of milliseconds waiting
    // for an ACK. OpenSSH sets this itself; do the same explicitly.
    let socket = TcpStream::connect(addr)
        .await
        .context("TCP connection failed")?;
    socket
        .set_nodelay(true)
        .context("failed to enable TCP_NODELAY")?;

    let mut session = match client::connect_stream(ssh_config, socket, handler).await {
        Ok(session) => session,
        Err(err) => {
            let reason = failure.lock().ok().and_then(|mut slot| slot.take());
            return Err(match reason {
                Some(HostKeyFailure::Changed { fingerprint }) => anyhow!(
                    "HOST KEY CHANGED for {}:{} — the server presented a different \
                     key ({fingerprint}).\n\n\
                     This is what a man-in-the-middle attack looks like. If you know \
                     the host was reinstalled or rekeyed, remove its old entry from \
                     ~/.ssh/known_hosts and reconnect. Otherwise stop and investigate.",
                    config.host,
                    config.port,
                ),
                Some(HostKeyFailure::Rejected { fingerprint }) => anyhow!(
                    "host key for {}:{} was not accepted ({fingerprint})",
                    config.host,
                    config.port,
                ),
                Some(HostKeyFailure::Error { message }) => anyhow!(
                    "could not verify the host key for {}:{}: {message}",
                    config.host,
                    config.port,
                ),
                None => anyhow!("{err}").context("SSH handshake failed"),
            });
        }
    };

    authenticate(&mut session, config).await?;

    let channel = session
        .channel_open_session()
        .await
        .context("failed to open SSH channel")?;

    channel
        .request_pty(false, "xterm-256color", 80, 24, 0, 0, &[])
        .await
        .context("PTY request rejected")?;
    channel
        .request_shell(false)
        .await
        .context("shell request rejected")?;

    // SFTP is optional: a server may not provide the subsystem. Never fail the
    // whole connection because of it — the terminal is the primary feature.
    let (sftp, sftp_error) = match open_sftp(&session).await {
        Ok(sftp) => (Some(sftp), None),
        Err(err) => {
            log::warn!("SFTP unavailable: {err}");
            (None, Some(err))
        }
    };

    Ok((session, channel, sftp, sftp_error))
}

/// Open a second channel and start the SFTP client handshake (`SSH_FXP_INIT`).
async fn open_sftp(session: &Handle<ClientHandler>) -> Result<SftpSession, String> {
    let channel = session
        .channel_open_session()
        .await
        .map_err(|err| format!("could not open an SFTP channel: {err}"))?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|err| format!("could not request the SFTP subsystem: {err}"))?;

    SftpSession::new_with_config(
        channel.into_stream(),
        SftpConfig {
            request_timeout_secs: 10,
            ..SftpConfig::default()
        },
    )
    .await
    .map_err(|err| format!("SFTP handshake failed: {err}"))
}

/// Authenticate in the same order `ssh` users expect: key file(s), then
/// ssh-agent identities, then the typed password.
///
/// Every method is optional. Failures are accumulated so the UI can explain
/// *why* nothing worked (missing key, encrypted key, agent not running, …)
/// instead of a bare "authentication rejected". The password is never included
/// in any message.
async fn authenticate(session: &mut Handle<ClientHandler>, config: &SshConfig) -> Result<()> {
    let mut failures: Vec<String> = Vec::new();

    if config.use_key {
        let candidates = key_candidates(&config.key_path);
        if candidates.is_empty() {
            failures.push(if config.key_path.trim().is_empty() {
                "no key found in ~/.ssh (id_ed25519, id_ecdsa, id_rsa, id_dsa)".to_string()
            } else {
                format!("key file not found: {}", config.key_path.trim())
            });
        }

        for path in candidates {
            let display = path.display();
            let key = match load_secret_key(&path, None) {
                Ok(key) => key,
                Err(russh::keys::Error::KeyIsEncrypted) => {
                    failures.push(format!(
                        "{display}: key is passphrase-protected; add it to ssh-agent \
                         (ssh-add) and enable the agent option"
                    ));
                    continue;
                }
                Err(err) => {
                    failures.push(format!("{display}: {err}"));
                    continue;
                }
            };

            match session
                .authenticate_publickey(&config.user, Arc::new(key))
                .await
            {
                Ok(true) => return Ok(()),
                Ok(false) => failures.push(format!("{display}: key rejected by server")),
                Err(err) => failures.push(format!("{display}: {err}")),
            }
        }
    }

    if config.use_agent {
        match authenticate_with_agent(session, &config.user).await {
            Ok(true) => return Ok(()),
            Ok(false) => failures.push("ssh-agent: no identity was accepted".to_string()),
            Err(err) => failures.push(format!("ssh-agent: {err}")),
        }
    }

    if !config.password.is_empty() {
        match session
            .authenticate_password(&config.user, &config.password)
            .await
        {
            Ok(true) => return Ok(()),
            Ok(false) => failures.push("password: rejected by server".to_string()),
            Err(err) => return Err(anyhow!("password authentication request failed: {err}")),
        }
    }

    if failures.is_empty() {
        return Err(anyhow!(
            "no authentication method selected: enable a key or ssh-agent, or type a password"
        ));
    }
    Err(anyhow!("authentication failed: {}", failures.join("; ")))
}

/// Try every identity currently loaded in `$SSH_AUTH_SOCK`.
///
/// `authenticate_future` consumes the agent connection and hands it back, so a
/// single connection is reused for all identities — the agent never has to be
/// re-opened after a rejected key.
async fn authenticate_with_agent(session: &mut Handle<ClientHandler>, user: &str) -> Result<bool> {
    let mut agent = AgentClient::connect_env()
        .await
        .context("could not connect to $SSH_AUTH_SOCK (is ssh-agent running?)")?;
    let identities = agent
        .request_identities()
        .await
        .context("could not list ssh-agent identities")?;
    if identities.is_empty() {
        return Err(anyhow!("the agent has no identities (run ssh-add)"));
    }

    for key in identities {
        let (returned, result) = session.authenticate_future(user, key, agent).await;
        agent = returned;
        match result {
            Ok(true) => return Ok(true),
            Ok(false) => {}
            Err(err) => return Err(anyhow!("signing with an agent key failed: {err}")),
        }
    }
    Ok(false)
}

const DEFAULT_KEY_NAMES: [&str; 4] = ["id_ed25519", "id_ecdsa", "id_rsa", "id_dsa"];

/// Explicit path if configured, otherwise the usual `~/.ssh/id_*` files that
/// actually exist. Missing *default* keys are simply skipped; an explicit path
/// is always returned so its load error is reported.
fn key_candidates(configured: &str) -> Vec<PathBuf> {
    let configured = configured.trim();
    if !configured.is_empty() {
        return vec![expand_tilde(configured)];
    }

    let Some(home) = home_dir() else {
        return Vec::new();
    };
    let ssh = home.join(".ssh");
    DEFAULT_KEY_NAMES
        .iter()
        .map(|name| ssh.join(name))
        .filter(|path| path.is_file())
        .collect()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Expand a leading `~` / `~/`. `~user` is left untouched.
fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        return home_dir().unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

/// Channels owned by one session thread: terminal input/output plus the SFTP
/// request queue. Bundled so `run_session` keeps a readable signature.
struct SessionIo {
    input_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    resize_rx: mpsc::UnboundedReceiver<(u16, u16)>,
    disconnect_rx: mpsc::UnboundedReceiver<()>,
    sftp_rx: mpsc::UnboundedReceiver<SftpRequest>,
    output_tx: std_mpsc::Sender<Vec<u8>>,
}

async fn run_session(
    session: Arc<Handle<ClientHandler>>,
    mut channel: Channel<ClientMsg>,
    sftp: Option<SftpSession>,
    io: SessionIo,
    wake: Option<Wake>,
) {
    let SessionIo {
        mut input_rx,
        mut resize_rx,
        mut disconnect_rx,
        mut sftp_rx,
        output_tx,
    } = io;
    // The client is behind a lock so a reconnect can swap in a fresh SFTP
    // channel while the terminal loop keeps running. Locks are only held long
    // enough to clone the `Arc`; all I/O happens without one.
    let sftp: SharedSftp = Arc::new(Mutex::new(sftp.map(|sftp| Arc::new(SftpClient::new(sftp)))));

    loop {
        tokio::select! {
            maybe_bytes = input_rx.recv() => {
                match maybe_bytes {
                    Some(bytes) => {
                        // russh 0.45's Channel::data takes AsyncRead; a byte slice satisfies it.
                        if channel.data(&bytes[..]).await.is_err() {
                            break;
                        }
                    }
                    None => break, // app dropped the sender
                }
            }
            maybe_size = resize_rx.recv() => {
                match maybe_size {
                    Some((cols, rows)) => {
                        let _ = channel
                            .window_change(cols as u32, rows as u32, 0, 0)
                            .await;
                    }
                    None => break,
                }
            }
            // Fires on explicit disconnect AND on sender drop. Both mean stop.
            _ = disconnect_rx.recv() => break,
            maybe_request = sftp_rx.recv() => {
                let Some(request) = maybe_request else { break };
                let SftpRequest { action, wake: request_wake } = request;
                match action {
                    SftpAction::Reconnect { reply } => {
                        let session = session.clone();
                        let shared = Arc::clone(&sftp);
                        // Open the new channel off the terminal loop: the SFTP
                        // handshake must not freeze keystrokes or output.
                        tokio::spawn(async move {
                            let result = reconnect_sftp(&session, &shared).await;
                            let _ = reply.send(result);
                            if let Some(wake) = request_wake {
                                wake();
                            }
                        });
                    }
                    action => {
                        let shared = Arc::clone(&sftp);
                        tokio::spawn(async move {
                            let client = shared.lock().ok().and_then(|slot| slot.clone());
                            match client {
                                Some(client) => action.run(&client).await,
                                None => {
                                    action.fail("SFTP support is not available on this server")
                                }
                            }
                            if let Some(wake) = request_wake {
                                wake();
                            }
                        });
                    }
                }
            }
            msg = channel.wait() => {
                match msg {
                    Some(ChannelMsg::Data { data }) => {
                        if output_tx.send(data.to_vec()).is_err() {
                            break;
                        }
                        if let Some(wake) = wake.as_ref() {
                            wake();
                        }
                    }
                    Some(ChannelMsg::ExtendedData { data, .. }) => {
                        if output_tx.send(data.to_vec()).is_err() {
                            break;
                        }
                        if let Some(wake) = wake.as_ref() {
                            wake();
                        }
                    }
                    Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
                    _ => {}
                }
            }
        }
    }

    // Best-effort graceful disconnect on every exit path.
    let _ = session
        .disconnect(russh::Disconnect::ByApplication, "", "en")
        .await;
    if let Some(wake) = wake.as_ref() {
        wake();
    }
}

/// A request from the UI to the SFTP task.
struct SftpRequest {
    action: SftpAction,
    /// Wakes egui once the reply has been sent (the UI only repaints on
    /// demand, so without this a finished listing could sit invisible until
    /// the next mouse/key event).
    wake: Option<Wake>,
}

/// The live SFTP channel, swappable by a reconnect.
type SharedSftp = Arc<Mutex<Option<Arc<SftpClient>>>>;

/// An open SFTP channel plus the owner-name cache shared by all listings on
/// that connection.
struct SftpClient {
    session: SftpSession,
    /// Loaded on the first listing rather than during connection setup, so a
    /// slow `/etc/passwd` read can never delay terminal output.
    owners: OnceCell<HashMap<u32, String>>,
}

impl SftpClient {
    fn new(session: SftpSession) -> Self {
        Self {
            session,
            owners: OnceCell::new(),
        }
    }
}

fn owner_name(owners: &HashMap<u32, String>, uid: Option<u32>, user: Option<&str>) -> String {
    if let Some(uid) = uid {
        if let Some(name) = owners.get(&uid) {
            return name.clone();
        }
    }
    if let Some(user) = user {
        let user = user.trim();
        if !user.is_empty() {
            return user.to_string();
        }
    }
    uid.map(|uid| uid.to_string()).unwrap_or_default()
}

enum SftpAction {
    Home {
        reply: std_mpsc::Sender<Result<String, String>>,
    },
    List {
        path: String,
        reply: std_mpsc::Sender<Result<Vec<RemoteEntry>, String>>,
    },
    Reconnect {
        reply: std_mpsc::Sender<Result<(), String>>,
    },
    Rename {
        from: String,
        to: String,
        reply: std_mpsc::Sender<Result<String, String>>,
    },
    Delete {
        path: String,
        is_dir: bool,
        reply: std_mpsc::Sender<Result<String, String>>,
    },
    CreateDir {
        path: String,
        reply: std_mpsc::Sender<Result<String, String>>,
    },
    Copy {
        from: String,
        dest_dir: String,
        reply: std_mpsc::Sender<Result<String, String>>,
    },
    Fetch {
        remote: String,
        local: PathBuf,
        reply: std_mpsc::Sender<Result<String, String>>,
    },
}

impl SftpAction {
    async fn run(self, client: &SftpClient) {
        match self {
            SftpAction::Home { reply } => {
                let _ = reply.send(remote_home(&client.session).await);
            }
            SftpAction::List { path, reply } => {
                let _ = reply.send(remote_list(client, &path).await);
            }
            SftpAction::Reconnect { reply } => {
                // Reconnect needs the SSH handle, so `run_session` handles it
                // before dispatching an action here.
                let _ = reply.send(Err("SFTP reconnect was not dispatched correctly".into()));
            }
            SftpAction::Rename { from, to, reply } => {
                let result = client
                    .session
                    .rename(&from, &to)
                    .await
                    .map(|()| to.clone())
                    .map_err(|err| format!("could not rename '{from}' to '{to}': {err}"));
                let _ = reply.send(result);
            }
            SftpAction::Delete {
                path,
                is_dir,
                reply,
            } => {
                // Re-check the type: the listing may be stale, and the hint
                // could be wrong for a symlink or a replaced file.
                let is_dir = match client.session.symlink_metadata(&path).await {
                    Ok(metadata) => metadata.file_type().is_dir(),
                    Err(_) => is_dir,
                };
                let result = remove_tree(client, &path, is_dir)
                    .await
                    .map(|()| path.clone());
                let _ = reply.send(result);
            }
            SftpAction::CreateDir { path, reply } => {
                let result = client
                    .session
                    .create_dir(&path)
                    .await
                    .map(|()| path.clone())
                    .map_err(|err| format!("could not create folder '{path}': {err}"));
                let _ = reply.send(result);
            }
            SftpAction::Copy {
                from,
                dest_dir,
                reply,
            } => {
                let result = copy_into(client, &from, &dest_dir).await;
                let _ = reply.send(result);
            }
            SftpAction::Fetch {
                remote,
                local,
                reply,
            } => {
                let result = fetch_file(client, &remote, &local).await;
                let _ = reply.send(result);
            }
        }
    }

    /// Reply with an error without touching the SFTP session.
    fn fail(self, message: &str) {
        let send = |reply: std_mpsc::Sender<Result<String, String>>| {
            let _ = reply.send(Err(message.to_string()));
        };
        match self {
            SftpAction::Home { reply } => {
                let _ = reply.send(Err(message.to_string()));
            }
            SftpAction::List { reply, .. } => {
                let _ = reply.send(Err(message.to_string()));
            }
            SftpAction::Reconnect { reply } => {
                let _ = reply.send(Err(message.to_string()));
            }
            SftpAction::Rename { reply, .. }
            | SftpAction::Delete { reply, .. }
            | SftpAction::CreateDir { reply, .. }
            | SftpAction::Copy { reply, .. }
            | SftpAction::Fetch { reply, .. } => send(reply),
        }
    }
}

/// Replace the shared SFTP channel with a freshly opened one.
async fn reconnect_sftp(
    session: &Handle<ClientHandler>,
    shared: &SharedSftp,
) -> Result<(), String> {
    let session = open_sftp(session).await?;
    let client = Arc::new(SftpClient::new(session));
    let old = match shared.lock() {
        Ok(mut slot) => slot.replace(client),
        Err(_) => return Err("the SFTP state lock is poisoned".to_string()),
    };
    if let Some(old) = old {
        // In-flight requests still hold a clone; closing the channel is still
        // the right signal that this generation of the connection is done.
        let _ = old.session.close().await;
    }
    Ok(())
}

async fn load_owner_names(sftp: &SftpSession) -> HashMap<u32, String> {
    let mut file = match sftp.open("/etc/passwd").await {
        Ok(file) => file,
        Err(_) => return HashMap::new(),
    };
    let mut contents = Vec::new();
    let result = file.read_to_end(&mut contents).await;
    let _ = file.close().await;
    if result.is_err() {
        return HashMap::new();
    }
    parse_passwd(&String::from_utf8_lossy(&contents))
}

fn parse_passwd(contents: &str) -> HashMap<u32, String> {
    contents
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let mut fields = line.split(':');
            let name = fields.next()?.trim();
            let _password = fields.next()?;
            let uid = fields.next()?.trim().parse().ok()?;
            if name.is_empty() {
                return None;
            }
            Some((uid, name.to_string()))
        })
        .collect()
}

async fn remote_home(sftp: &SftpSession) -> Result<String, String> {
    sftp.canonicalize(".")
        .await
        .map_err(|err| format!("could not resolve the remote home directory: {err}"))
}

async fn remote_list(client: &SftpClient, path: &str) -> Result<Vec<RemoteEntry>, String> {
    let dir = client
        .session
        .read_dir(path)
        .await
        .map_err(|err| format!("could not list '{path}': {err}"))?;

    // Load `/etc/passwd` once per SFTP connection. Servers that hide the file
    // produce an empty map and the UI falls back to the UID.
    let owners = client
        .owners
        .get_or_init(|| load_owner_names(&client.session))
        .await;

    let mut entries = Vec::new();
    for entry in dir {
        let metadata = entry.metadata();
        let file_type = metadata.file_type();
        entries.push(RemoteEntry {
            name: entry.file_name(),
            path: entry.path(),
            is_dir: file_type.is_dir(),
            is_symlink: file_type.is_symlink(),
            size: metadata.len(),
            owner: owner_name(owners, metadata.uid, metadata.user.as_deref()),
            modified: metadata.modified().ok(),
        });
    }
    Ok(entries)
}

/// Copy `from` into `dest_dir`, giving the copy a unique name.
async fn copy_into(client: &SftpClient, from: &str, dest_dir: &str) -> Result<String, String> {
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

async fn unique_copy_path(
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
async fn copy_tree(client: &SftpClient, from: &str, to: &str) -> Result<(), String> {
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

async fn copy_file(client: &SftpClient, from: &str, to: &str) -> Result<(), String> {
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

    let copied = tokio::io::copy(&mut source, &mut destination).await;
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

async fn fetch_file(client: &SftpClient, remote: &str, local: &Path) -> Result<String, String> {
    let mut source = client
        .session
        .open(remote)
        .await
        .map_err(|err| format!("could not open '{remote}' for reading: {err}"))?;
    let mut destination = tokio::fs::File::create(local)
        .await
        .map_err(|err| format!("could not create '{}': {err}", local.display()))?;

    let copied = tokio::io::copy(&mut source, &mut destination).await;
    let flushed = destination.flush().await;
    let closed_source = source.close().await;
    drop(destination);

    if let Err(err) = copied {
        let _ = tokio::fs::remove_file(local).await;
        return Err(format!("could not download '{remote}': {err}"));
    }
    flushed.map_err(|err| format!("could not finish writing '{}': {err}", local.display()))?;
    closed_source.map_err(|err| format!("could not close '{remote}': {err}"))?;
    Ok(local.display().to_string())
}

async fn remove_tree(client: &SftpClient, path: &str, is_dir: bool) -> Result<(), String> {
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

/// POSIX path join for remote paths. Mirrors `sftp::join_path` without making
/// the transport module depend on the UI state module.
fn join_remote_path(dir: &str, name: &str) -> String {
    if dir.is_empty() || dir == "/" {
        format!("/{name}")
    } else if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

struct ClientHandler {
    host: String,
    port: u16,
    /// `None` means no UI is listening; unknown hosts are then refused rather
    /// than silently trusted.
    prompt_tx: Option<std_mpsc::Sender<HostKeyPrompt>>,
    failure: Arc<Mutex<Option<HostKeyFailure>>>,
}

impl ClientHandler {
    fn fail(&self, reason: HostKeyFailure) {
        if let Ok(mut slot) = self.failure.lock() {
            *slot = Some(reason);
        }
    }
}

#[async_trait]
impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> std::result::Result<bool, Self::Error> {
        // 1. Already known and matching?
        match check_known_hosts(&self.host, self.port, server_public_key) {
            Ok(true) => return Ok(true),
            // Unknown host or an RSA algorithm-name mismatch: fall through.
            Ok(false) => {}
            // A *different* key is recorded for this host. Refuse, and do not
            // offer a way to accept it from the client.
            Err(russh::keys::Error::KeyChanged { .. }) => {
                self.fail(HostKeyFailure::Changed {
                    fingerprint: fingerprint(server_public_key),
                });
                return Ok(false);
            }
            // Unreadable known_hosts or another verification problem: fail
            // closed.
            Err(err) => {
                self.fail(HostKeyFailure::Error {
                    message: format!("known_hosts: {err}"),
                });
                return Ok(false);
            }
        }

        // `check_known_hosts` compares algorithm names as well as keys. It
        // always parses an RSA entry as `rsa-sha2-256`, but an old server that
        // negotiated `ssh-rsa` presents the same key with the `ssh-rsa` name.
        // Compare the key material for RSA entries before showing a TOFU
        // prompt, otherwise the prompt would reappear on every connection.
        match known_rsa_key_status(&self.host, self.port, server_public_key) {
            Ok(Some(true)) => return Ok(true),
            Ok(Some(false)) => {
                self.fail(HostKeyFailure::Changed {
                    fingerprint: fingerprint(server_public_key),
                });
                return Ok(false);
            }
            // No RSA entry for this host: genuinely unknown.
            Ok(None) => {}
            Err(err) => {
                self.fail(HostKeyFailure::Error {
                    message: format!("known_hosts: {err}"),
                });
                return Ok(false);
            }
        }

        // 2. Unknown host. Ask the user, if anyone is listening.
        let fp = fingerprint(server_public_key);
        let Some(prompt_tx) = self.prompt_tx.clone() else {
            self.fail(HostKeyFailure::Rejected { fingerprint: fp });
            return Ok(false);
        };

        let (reply, answer) = oneshot::channel();
        let prompt = HostKeyPrompt {
            host: self.host.clone(),
            port: self.port,
            fingerprint: fp.clone(),
            reply,
        };
        if prompt_tx.send(prompt).is_err() {
            // UI went away between the check and the prompt.
            self.fail(HostKeyFailure::Rejected { fingerprint: fp });
            return Ok(false);
        }

        // The handshake pauses here until the UI answers.
        match answer.await {
            Ok(true) => match record_host_key(&self.host, self.port, server_public_key) {
                Ok(()) => Ok(true),
                Err(message) => {
                    // Accepting without recording would prompt again next time
                    // and hide the write failure; report it instead.
                    self.fail(HostKeyFailure::Error { message });
                    Ok(false)
                }
            },
            _ => {
                self.fail(HostKeyFailure::Rejected { fingerprint: fp });
                Ok(false)
            }
        }
    }
}

/// `russh_keys::key::PublicKey::fingerprint()` takes no arguments in 0.45 —
/// it hashes the key with SHA-256 internally and returns the `SHA256:...` string.
fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint()
}

/// Compare RSA host-key material while ignoring the signature algorithm name.
///
/// `known_hosts` entries are always parsed with `rsa-sha2-256` as the algorithm,
/// whereas an OpenSSH 5.x server negotiated with `ssh-rsa` reports `ssh-rsa`.
/// `PublicKey` equality already ignores that hash field, so direct comparison
/// works once the RSA entries for the host are collected.
///
/// Returns `Ok(None)` when no RSA key is recorded for the host, `Ok(Some(true))`
/// when one matches, and `Ok(Some(false))` when RSA keys exist but none match
/// (a changed key).
fn known_rsa_key_status(
    host: &str,
    port: u16,
    server_key: &PublicKey,
) -> std::result::Result<Option<bool>, russh::keys::Error> {
    if !matches!(server_key, PublicKey::RSA { .. }) {
        return Ok(None);
    }

    let mut saw_rsa = false;
    for (_, recorded) in known_host_keys(host, port)? {
        if matches!(recorded, PublicKey::RSA { .. }) {
            saw_rsa = true;
            if &recorded == server_key {
                return Ok(Some(true));
            }
        }
    }

    Ok(if saw_rsa { Some(false) } else { None })
}

/// Record a host key in `~/.ssh/known_hosts`.
///
/// russh appends to the file but does not create `~/.ssh`, which a brand-new
/// account may not have. Creating the directory first is idempotent and turns a
/// confusing failure into a working first connect.
fn record_host_key(host: &str, port: u16, key: &PublicKey) -> std::result::Result<(), String> {
    if let Ok(home) = std::env::var("HOME") {
        let _ = std::fs::create_dir_all(PathBuf::from(home).join(".ssh"));
    }
    learn_known_hosts(host, port, key)
        .map_err(|err| format!("could not record the host key in known_hosts: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_owner_names_from_passwd() {
        let passwd = "\
root:x:0:0:root:/root:/bin/bash
# comment lines are ignored
alice:x:1000:1000:Alice:/home/alice:/bin/bash
invalid line
bad:x:not-a-number:0::/:/bin/sh
bob:x:1001:1001::/home/bob:/bin/sh
";
        let owners = parse_passwd(passwd);
        assert_eq!(owners.get(&0).map(String::as_str), Some("root"));
        assert_eq!(owners.get(&1000).map(String::as_str), Some("alice"));
        assert_eq!(owners.get(&1001).map(String::as_str), Some("bob"));
        assert!(!owners.contains_key(&999));
    }

    #[test]
    fn joins_remote_paths() {
        assert_eq!(join_remote_path("/home/user", "file"), "/home/user/file");
        assert_eq!(join_remote_path("/home/user/", "file"), "/home/user/file");
        assert_eq!(join_remote_path("/", "etc"), "/etc");
        assert_eq!(join_remote_path("", "etc"), "/etc");
    }
}
