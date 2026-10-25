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
use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use russh::client::{self, Handle, Msg as ClientMsg};
// `PublicKey` lives in the `key` submodule; the known-hosts helpers are at the
// `keys` root. `HashAlg` does not exist in russh 0.45 — `fingerprint()` takes
// no arguments and hashes with SHA-256 internally.
use russh::keys::key::PublicKey;
use russh::keys::{check_known_hosts, known_host_keys, learn_known_hosts};
use russh::{compression, Channel, ChannelMsg};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};

/// Wakes the egui thread when asynchronous SSH data arrives.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
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

        let (session, channel) = rt
            .block_on(connect_and_auth(&config, prompt_tx))
            .with_context(|| {
                format!("connect to {}@{}:{}", config.user, config.host, config.port)
            })?;

        let (input_tx, input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (resize_tx, resize_rx) = mpsc::unbounded_channel::<(u16, u16)>();
        let (disconnect_tx, disconnect_rx) = mpsc::unbounded_channel::<()>();
        let (output_tx, output_rx) = std_mpsc::channel::<Vec<u8>>();

        std::thread::Builder::new()
            .name(format!("ssh-{}@{}", config.user, config.host))
            .spawn(move || {
                rt.block_on(run_session(
                    session,
                    channel,
                    input_rx,
                    resize_rx,
                    disconnect_rx,
                    output_tx,
                    wake,
                ));
            })
            .context("failed to spawn SSH session thread")?;

        Ok(Self {
            input_tx,
            resize_tx,
            disconnect_tx,
            output_rx,
        })
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
) -> Result<(Handle<ClientHandler>, Channel<ClientMsg>)> {
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

    let ok = session
        .authenticate_password(&config.user, &config.password)
        .await
        .context("password authentication request failed")?;
    if !ok {
        return Err(anyhow!("authentication rejected by server"));
    }

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

    Ok((session, channel))
}

async fn run_session(
    session: Handle<ClientHandler>,
    mut channel: Channel<ClientMsg>,
    mut input_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    mut resize_rx: mpsc::UnboundedReceiver<(u16, u16)>,
    mut disconnect_rx: mpsc::UnboundedReceiver<()>,
    output_tx: std_mpsc::Sender<Vec<u8>>,
    wake: Option<Wake>,
) {
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
