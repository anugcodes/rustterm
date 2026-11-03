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
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use russh::client::{self, Handle, Msg as ClientMsg};
// `PublicKey` lives in the `key` submodule; the known-hosts helpers are at the
// `keys` root. `HashAlg` does not exist in russh 0.45 — `fingerprint()` takes
// no arguments and hashes with SHA-256 internally.
use crate::x11;
use russh::keys::agent::client::AgentClient;
use russh::keys::key::PublicKey;
use russh::keys::{check_known_hosts, known_host_keys, learn_known_hosts, load_secret_key};
use russh::{compression, Channel, ChannelMsg};
use russh_sftp::client::{Config as SftpConfig, SftpSession};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Notify, OnceCell};

mod connect;
mod session;
mod vnc;
use session::*;
use vnc::*;
mod transfer;
use connect::*;
pub use transfer::*;
/// Wakes the egui thread when asynchronous SSH data arrives.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// Bytes the UI may have queued before the session thread stops reading the
/// SSH channel. This is the terminal's only backpressure: without it a hidden
/// tab running `yes` grows the process without bound and starves the visible
/// tab of its per-frame drain budget.
const OUTPUT_HIGH_WATER: usize = 2 * 1024 * 1024;

/// Loopback viewers allowed through one VNC tunnel at a time. VNC uses two
/// connections per viewer in some clients; the rest are refused (and closed)
/// instead of spawning an unbounded number of tasks.
const VNC_MAX_CLIENTS: usize = 8;

#[derive(Clone)]
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
    /// Request X11 forwarding before the shell starts (`ssh -X`).
    pub x11: bool,
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
    /// Bytes queued for the UI but not yet read. The session thread stops
    /// reading the SSH channel while this is above [`OUTPUT_HIGH_WATER`], which
    /// is what bounds memory when a hidden tab floods output.
    output_backlog: Arc<AtomicUsize>,
    /// Signalled by the UI after it drains output, so a gated session resumes.
    output_space: Arc<Notify>,
    /// Requests for the SFTP subsystem. Always present, even when SFTP failed
    /// to open, so callers can still get a meaningful error.
    sftp_tx: mpsc::UnboundedSender<SftpRequest>,
    /// Remote-display requests (VNC probe/tunnel).
    display_tx: mpsc::UnboundedSender<DisplayRequest>,
    /// Why SFTP could not be opened, if it could not.
    sftp_unavailable: Option<String>,
    /// Wakes egui when an SFTP reply is ready.
    wake: Option<Wake>,
    /// False once the session thread has finished (normal exit or panic).
    alive: Arc<AtomicBool>,
    /// X11 forwarding state, shared with the handler and the proxies.
    x11: Arc<X11Shared>,
    /// Whether this session requested X11 forwarding at connect time.
    x11_requested: bool,
    /// Live viewer connections on the current VNC tunnel. The app uses this to
    /// tell "viewer exited" apart from "viewer handed off to a daemon".
    vnc_clients: Arc<AtomicUsize>,
}

/// Flips `alive` to false and wakes egui when the session thread ends,
/// including on panic, so a dead tab is noticed even while the UI is idle.
struct SessionGuard {
    alive: Arc<AtomicBool>,
    wake: Option<Wake>,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
        if let Some(wake) = &self.wake {
            wake();
        }
    }
}

/// A VNC server reachable from the SSH server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VncTarget {
    pub host: String,
    pub port: u16,
}

impl VncTarget {
    pub fn label(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// The VNC display number (`:1` for port 5901), when the port is in the
    /// standard range.
    pub fn display_number(&self) -> Option<u16> {
        (5900..=5999)
            .contains(&self.port)
            .then_some(self.port - 5900)
    }

    /// Parse the editable setting: `host`, `host:port`, or blank.
    pub fn parse(value: &str, default_port: u16) -> Option<Self> {
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        match value.rsplit_once(':') {
            Some((host, port)) if !host.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
                Some(Self {
                    host: host.to_string(),
                    port: port.parse().ok().filter(|port| *port > 0)?,
                })
            }
            _ => Some(Self {
                host: value.to_string(),
                port: default_port,
            }),
        }
    }
}

/// What a VNC probe found: the target to tunnel to and the account's login
/// shell, which decides how the bar sets `DISPLAY`.
#[derive(Debug, Clone)]
pub struct VncProbe {
    pub target: Option<VncTarget>,
    /// Login-shell path from `/etc/passwd`, or `$SHELL` as a fallback.
    pub shell: Option<String>,
}

/// What the UI asks the session thread to do for remote display.
enum DisplayAction {
    /// Find a VNC listener in the server's shell.
    Probe {
        auto_create: bool,
        reply: std_mpsc::Sender<Result<VncProbe, String>>,
    },
    /// Open a local TCP listener tunneled to `host:port`.
    Open {
        host: String,
        port: u16,
        reply: std_mpsc::Sender<Result<u16, String>>,
    },
    /// Close the tunnel and its listener.
    Close { reply: std_mpsc::Sender<()> },
}

pub(crate) struct DisplayRequest {
    action: DisplayAction,
    wake: Option<Wake>,
}

/// Live state of X11 forwarding for one session.
#[derive(Debug, Clone, Default)]
pub struct X11Status {
    /// The server accepted the `x11-req`; remote programs get a `DISPLAY`.
    pub forwarding: bool,
    /// Server-opened X11 channels currently being proxied.
    pub active: usize,
    /// Why forwarding could not start, or the last proxy failure.
    pub error: Option<String>,
}

/// State shared between the session thread, the SSH handler and the UI.
#[derive(Default)]
pub(crate) struct X11Shared {
    config: Mutex<Option<x11::X11Config>>,
    status: Mutex<X11Status>,
    wake: Option<Wake>,
}

impl X11Shared {
    fn new(wake: Option<Wake>) -> Arc<Self> {
        Arc::new(Self {
            wake,
            ..Self::default()
        })
    }

    fn config(&self) -> Option<x11::X11Config> {
        self.config.lock().ok().and_then(|slot| slot.clone())
    }

    fn set_config(&self, config: Option<x11::X11Config>) {
        if let Ok(mut slot) = self.config.lock() {
            *slot = config;
        }
    }

    fn status(&self) -> X11Status {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }

    fn mark_forwarding(&self, forwarding: bool, error: Option<String>) {
        if let Ok(mut status) = self.status.lock() {
            status.forwarding = forwarding;
            status.error = error;
        }
        self.wake_now();
    }

    fn opened(&self) {
        if let Ok(mut status) = self.status.lock() {
            status.active += 1;
        }
        self.wake_now();
    }

    fn closed(&self, error: Option<String>) {
        if let Ok(mut status) = self.status.lock() {
            status.active = status.active.saturating_sub(1);
            if error.is_some() {
                status.error = error;
            }
        }
        self.wake_now();
    }

    fn wake_now(&self) {
        if let Some(wake) = &self.wake {
            wake();
        }
    }
}

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

/// What a remote symlink ultimately points at.
#[derive(Debug, Clone)]
pub struct ResolvedLink {
    /// Whether the *target* (not the link itself) is a directory.
    pub is_dir: bool,
    /// The link's raw target, when the server reports it.
    pub target: Option<String>,
}

/// Result of installing or removing the shell hook.
#[derive(Debug, Clone)]
pub struct HookResult {
    /// Startup file that was edited.
    pub path: String,
    /// Backup created before the first edit, if there was a file to back up.
    pub backup: Option<String>,
    /// False when the file already had the requested state.
    pub changed: bool,
}

/// Shell families that can report their working directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    Bash,
    Zsh,
    Csh,
    Tcsh,
}

impl ShellKind {
    /// Map a login-shell path to a supported family.
    pub fn from_shell(shell: &str) -> Option<Self> {
        match shell.rsplit('/').next().unwrap_or(shell) {
            "bash" => Some(Self::Bash),
            "zsh" => Some(Self::Zsh),
            "csh" => Some(Self::Csh),
            "tcsh" => Some(Self::Tcsh),
            _ => None,
        }
    }

    /// Family name for messages.
    pub fn name(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Zsh => "zsh",
            Self::Csh => "csh",
            Self::Tcsh => "tcsh",
        }
    }

    /// Whether installing replaces the single `cwdcmd` alias csh provides.
    pub fn shares_cwdcmd(self) -> bool {
        matches!(self, Self::Csh | Self::Tcsh)
    }

    /// One-line version typed into a live shell by **Enable now**.
    ///
    /// It must stay on a single line so Enter runs the whole thing, and it is
    /// idempotent so a second click does not stack entries.
    pub fn session_hook(self) -> &'static str {
        match self {
            Self::Bash | Self::Zsh => concat!(
                "__rustterm_cwd() { printf '\\033]7;file://%s%s\\033\\\\' \"${HOSTNAME:-localhost}\" \"$PWD\"; }; ",
                "if [ -n \"$ZSH_VERSION\" ]; then ",
                "eval 'typeset -ga precmd_functions; (( ${precmd_functions[(Ie)__rustterm_cwd]} )) || precmd_functions+=(__rustterm_cwd)'; ",
                "else case \"$PROMPT_COMMAND\" in *__rustterm_cwd*) ;; *) ",
                "PROMPT_COMMAND=\"__rustterm_cwd${PROMPT_COMMAND:+;$PROMPT_COMMAND}\" ;; esac; fi"
            ),
            // csh has no prompt hook: `cwdcmd` runs after every directory
            // change. `$?prompt` keeps this quiet in non-interactive shells.
            Self::Csh | Self::Tcsh => {
                r#"if ( $?prompt ) alias cwdcmd 'printf "\033]7;file://%s\a" "$cwd"'; printf "\033]7;file://%s\a" "$cwd""#
            }
        }
    }

    /// Text typed at a prompt to point the next command at `display`.
    ///
    /// `setenv` persists, which is why csh/tcsh cannot use the sh-family
    /// `DISPLAY=:N` prefix. The trailing `; ` lets a command follow on the same
    /// line, and pressing Enter on its own still sets the variable.
    pub fn display_prefix(self, display: u16) -> String {
        match self {
            Self::Bash | Self::Zsh => format!("DISPLAY=:{display} "),
            Self::Csh | Self::Tcsh => format!("setenv DISPLAY :{display}; "),
        }
    }
}

/// Login shell plus the startup file that will receive the hook.
#[derive(Debug, Clone)]
pub struct ShellInfo {
    /// Shell path from `/etc/passwd`, as reported.
    pub shell: String,
    pub kind: ShellKind,
    /// Startup file the hook belongs in, chosen on the server.
    pub rc: String,
}
impl SshSession {
    /// Connect, verify the host key, authenticate, request a PTY + shell, then
    /// spawn the I/O loop.
    ///
    /// **Blocks** the calling thread through the whole handshake — including,
    /// possibly, waiting for the user to answer a host-key prompt on
    /// `prompt_tx`. Call this from a background thread.
    ///
    /// `cancel` aborts the attempt at any await point (dropping the socket and
    /// handshake). The returned error is never shown when it fires.
    pub fn connect(
        config: SshConfig,
        prompt_tx: Option<std_mpsc::Sender<HostKeyPrompt>>,
        wake: Option<Wake>,
        cancel: Option<oneshot::Receiver<()>>,
    ) -> Result<Self> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to build tokio runtime")?;

        let attempt = async {
            match cancel {
                Some(cancel) => {
                    tokio::select! {
                        _ = cancel => Err(anyhow!("connection cancelled")),
                        result = connect_and_auth(&config, prompt_tx, wake.clone()) => result,
                    }
                }
                None => connect_and_auth(&config, prompt_tx, wake.clone()).await,
            }
        };
        let (session, channel, sftp, sftp_error, x11) =
            rt.block_on(attempt).with_context(|| {
                format!("connect to {}@{}:{}", config.user, config.host, config.port)
            })?;
        // The handle is shared so the SFTP reconnect task can open a channel
        // while the session loop keeps ownership for terminal I/O.
        let session = Arc::new(session);

        let (input_tx, input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (resize_tx, resize_rx) = mpsc::unbounded_channel::<(u16, u16)>();
        let (disconnect_tx, disconnect_rx) = mpsc::unbounded_channel::<()>();
        let (output_tx, output_rx) = std_mpsc::channel::<Vec<u8>>();
        let output_backlog = Arc::new(AtomicUsize::new(0));
        let output_space = Arc::new(Notify::new());
        let vnc_clients = Arc::new(AtomicUsize::new(0));
        let (sftp_tx, sftp_rx) = mpsc::unbounded_channel::<SftpRequest>();
        let (display_tx, display_rx) = mpsc::unbounded_channel::<DisplayRequest>();
        let session_wake = wake.clone();
        let alive = Arc::new(AtomicBool::new(true));
        let alive_thread = Arc::clone(&alive);
        let guard_wake = session_wake.clone();
        // The loop needs its own handles; the originals stay with `SshSession`.
        let thread_backlog = Arc::clone(&output_backlog);
        let thread_space = Arc::clone(&output_space);
        let thread_vnc_clients = Arc::clone(&vnc_clients);

        std::thread::Builder::new()
            .name(format!("ssh-{}@{}", config.user, config.host))
            .spawn(move || {
                let _guard = SessionGuard {
                    alive: alive_thread,
                    wake: guard_wake,
                };
                rt.block_on(run_session(
                    session,
                    channel,
                    sftp,
                    thread_vnc_clients,
                    SessionIo {
                        input_rx,
                        resize_rx,
                        disconnect_rx,
                        sftp_rx,
                        display_rx,
                        output_tx,
                        output_backlog: thread_backlog,
                        output_space: thread_space,
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
            output_backlog,
            output_space,
            sftp_tx,
            display_tx,
            sftp_unavailable: sftp_error,
            wake: session_wake,
            alive,
            x11,
            x11_requested: config.x11,
            vnc_clients,
        })
    }

    /// Whether the session thread is still running. A false value means the
    /// connection is gone: input would be discarded and SFTP requests cannot
    /// be answered, so the UI must show a closed state instead of a stale tab.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// Why SFTP is unavailable for this connection, if it is.
    pub fn sftp_unavailable(&self) -> Option<&str> {
        self.sftp_unavailable.as_deref()
    }

    /// Current X11 forwarding state (accepted, active channels, last error).
    pub fn x11_status(&self) -> X11Status {
        self.x11.status()
    }

    /// Whether this session requested X11 forwarding at connect time.
    pub fn x11_requested(&self) -> bool {
        self.x11_requested
    }

    /// Number of viewer connections currently using the VNC tunnel.
    pub fn vnc_clients(&self) -> usize {
        self.vnc_clients.load(Ordering::Relaxed)
    }

    /// Look for a VNC listener on the server, creating one if requested.
    ///
    /// The reply also carries the login shell so the bar can type the right
    /// `DISPLAY` command for it.
    pub fn vnc_probe(
        &self,
        auto_create: bool,
    ) -> Option<std_mpsc::Receiver<Result<VncProbe, String>>> {
        let (reply, rx) = std_mpsc::channel();
        self.display_tx
            .send(DisplayRequest {
                action: DisplayAction::Probe { auto_create, reply },
                wake: self.wake.clone(),
            })
            .ok()?;
        Some(rx)
    }

    /// Open a local listener whose connections are tunneled to `target`.
    ///
    /// Returns the local port the viewer should connect to.
    pub fn vnc_open(&self, target: &VncTarget) -> Option<std_mpsc::Receiver<Result<u16, String>>> {
        let (reply, rx) = std_mpsc::channel();
        self.display_tx
            .send(DisplayRequest {
                action: DisplayAction::Open {
                    host: target.host.clone(),
                    port: target.port,
                    reply,
                },
                wake: self.wake.clone(),
            })
            .ok()?;
        Some(rx)
    }

    /// Stop the VNC tunnel and close its local listener.
    pub fn vnc_close(&self) -> Option<std_mpsc::Receiver<()>> {
        let (reply, rx) = std_mpsc::channel();
        self.display_tx
            .send(DisplayRequest {
                action: DisplayAction::Close { reply },
                wake: self.wake.clone(),
            })
            .ok()?;
        Some(rx)
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

    /// Resolve a symlink to learn whether its target is a folder.
    ///
    /// `stat` follows the link (unlike the listing's `lstat`), and the raw
    /// target is returned for display. A dangling link comes back as an error
    /// that names the target.
    pub fn sftp_resolve(
        &self,
        path: &str,
    ) -> Option<std_mpsc::Receiver<Result<ResolvedLink, String>>> {
        let (reply, rx) = std_mpsc::channel();
        self.sftp_tx
            .send(SftpRequest {
                action: SftpAction::Resolve {
                    path: path.to_string(),
                    reply,
                },
                wake: self.wake.clone(),
            })
            .ok()?;
        Some(rx)
    }

    /// Ask for the login shell of `user` from `/etc/passwd`.
    pub fn sftp_login_shell(
        &self,
        user: &str,
    ) -> Option<std_mpsc::Receiver<Result<ShellInfo, String>>> {
        let (reply, rx) = std_mpsc::channel();
        self.sftp_tx
            .send(SftpRequest {
                action: SftpAction::LoginShell {
                    user: user.to_string(),
                    reply,
                },
                wake: self.wake.clone(),
            })
            .ok()?;
        Some(rx)
    }

    /// Add or remove the marked directory-tracking block in a startup file.
    pub fn sftp_edit_hook(
        &self,
        rc: String,
        kind: ShellKind,
        install: bool,
    ) -> Option<std_mpsc::Receiver<Result<HookResult, String>>> {
        let (reply, rx) = std_mpsc::channel();
        self.sftp_tx
            .send(SftpRequest {
                action: SftpAction::EditHook {
                    rc,
                    kind,
                    install,
                    reply,
                },
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

    /// Upload one local file to exactly `remote`.
    ///
    /// The editor download keeps a randomised local name, so its destination
    /// cannot be derived from the file name the way [`Self::sftp_upload`]
    /// does. Used by the "upload on save" watcher.
    pub fn sftp_upload_one(
        &self,
        local: PathBuf,
        remote: String,
        progress: Arc<TransferProgress>,
    ) -> Option<std_mpsc::Receiver<Result<String, String>>> {
        let (reply, rx) = std_mpsc::channel();
        self.sftp_tx
            .send(SftpRequest {
                action: SftpAction::UploadOne {
                    local,
                    remote,
                    progress,
                    reply,
                },
                wake: self.wake.clone(),
            })
            .ok()?;
        Some(rx)
    }

    /// Upload local files or folders into `remote_dir`.
    ///
    /// Progress is reported through `progress`; the reply arrives on the usual
    /// receiver. The transfer itself runs off the terminal select loop.
    pub fn sftp_upload(
        &self,
        local: Vec<PathBuf>,
        remote_dir: &str,
        policy: ConflictPolicy,
        progress: Arc<TransferProgress>,
    ) -> Option<std_mpsc::Receiver<Result<String, String>>> {
        let (reply, rx) = std_mpsc::channel();
        self.sftp_tx
            .send(SftpRequest {
                action: SftpAction::Upload {
                    local,
                    remote_dir: remote_dir.to_string(),
                    policy,
                    progress,
                    reply,
                },
                wake: self.wake.clone(),
            })
            .ok()?;
        Some(rx)
    }

    /// Download remote files or folders into `local_dir`.
    pub fn sftp_download(
        &self,
        remote: Vec<RemoteEntry>,
        local_dir: PathBuf,
        policy: ConflictPolicy,
        progress: Arc<TransferProgress>,
    ) -> Option<std_mpsc::Receiver<Result<String, String>>> {
        let (reply, rx) = std_mpsc::channel();
        self.sftp_tx
            .send(SftpRequest {
                action: SftpAction::Download {
                    remote,
                    local_dir,
                    policy,
                    progress,
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
    ///
    /// Reading a chunk releases that much of the session's backlog and wakes
    /// the session thread if it had been gated by the high-water mark.
    pub fn try_read(&self) -> Option<Vec<u8>> {
        let chunk = self.output_rx.try_recv().ok()?;
        // The sender adds each chunk before queueing it, so this cannot
        // underflow; the debug assertion catches an accounting mistake early.
        let before = self
            .output_backlog
            .fetch_sub(chunk.len(), Ordering::Relaxed);
        debug_assert!(before >= chunk.len(), "output backlog accounting underflow");
        if before >= OUTPUT_HIGH_WATER {
            self.output_space.notify_one();
        }
        Some(chunk)
    }

    /// Clone of the PTY input sender, for components that need to write bytes
    /// back to the remote — primarily the terminal's `EventProxy`, which answers
    /// DA1/DA2/DSR queries with `PtyWrite`.
    pub fn input_sender(&self) -> mpsc::UnboundedSender<Vec<u8>> {
        self.input_tx.clone()
    }
}

pub(crate) struct SftpRequest {
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
pub(crate) struct SftpClient {
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
    /// Follow a symlink and report what it points at.
    Resolve {
        path: String,
        reply: std_mpsc::Sender<Result<ResolvedLink, String>>,
    },
    /// Look up the account's login shell in `/etc/passwd`.
    LoginShell {
        user: String,
        reply: std_mpsc::Sender<Result<ShellInfo, String>>,
    },
    /// Install or remove the directory-tracking block in `rc`.
    EditHook {
        rc: String,
        kind: ShellKind,
        install: bool,
        reply: std_mpsc::Sender<Result<HookResult, String>>,
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
    Upload {
        local: Vec<PathBuf>,
        remote_dir: String,
        policy: ConflictPolicy,
        progress: Arc<TransferProgress>,
        reply: std_mpsc::Sender<Result<String, String>>,
    },
    UploadOne {
        local: PathBuf,
        remote: String,
        progress: Arc<TransferProgress>,
        reply: std_mpsc::Sender<Result<String, String>>,
    },
    Download {
        /// Known entries from the listing, so the first request is not delayed
        /// by a redundant `stat` on a slow network mount.
        remote: Vec<RemoteEntry>,
        local_dir: PathBuf,
        policy: ConflictPolicy,
        progress: Arc<TransferProgress>,
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
            SftpAction::Resolve { path, reply } => {
                let result = resolve_link(client, &path).await;
                let _ = reply.send(result);
            }
            SftpAction::LoginShell { user, reply } => {
                let result = login_shell(&client.session, &user).await;
                let _ = reply.send(result);
            }
            SftpAction::EditHook {
                rc,
                kind,
                install,
                reply,
            } => {
                let result = edit_hook(client, &rc, kind, install).await;
                let _ = reply.send(result);
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
            SftpAction::Upload {
                local,
                remote_dir,
                policy,
                progress,
                reply,
            } => {
                let result = upload_items(client, local, &remote_dir, policy, &progress)
                    .await
                    .map_err(normalise_transfer_error);
                let _ = reply.send(result);
            }
            SftpAction::UploadOne {
                local,
                remote,
                progress,
                reply,
            } => {
                // A single-file upload (editor save) has no pre-walk to size
                // it, so read the length here for a real progress bar. This
                // must not move into `upload_file`, which also serves folder
                // uploads that already set an aggregate total.
                if let Ok(metadata) = tokio::fs::metadata(&local).await {
                    progress.set_total_bytes(metadata.len());
                }
                let result = upload_file(client, &local, &remote, &progress)
                    .await
                    .map(|()| remote)
                    .map_err(normalise_transfer_error);
                let _ = reply.send(result);
            }
            SftpAction::Download {
                remote,
                local_dir,
                policy,
                progress,
                reply,
            } => {
                let result = download_items(client, remote, &local_dir, policy, &progress)
                    .await
                    .map_err(normalise_transfer_error);
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
            SftpAction::Resolve { reply, .. } => {
                let _ = reply.send(Err(message.to_string()));
            }
            SftpAction::LoginShell { reply, .. } => {
                let _ = reply.send(Err(message.to_string()));
            }
            SftpAction::EditHook { reply, .. } => {
                let _ = reply.send(Err(message.to_string()));
            }
            SftpAction::Rename { reply, .. }
            | SftpAction::Delete { reply, .. }
            | SftpAction::CreateDir { reply, .. }
            | SftpAction::Copy { reply, .. }
            | SftpAction::Fetch { reply, .. }
            | SftpAction::Upload { reply, .. }
            | SftpAction::UploadOne { reply, .. }
            | SftpAction::Download { reply, .. } => send(reply),
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

/// Follow a symlink: `stat` (not `lstat`) so the target's type is reported.
async fn resolve_link(client: &SftpClient, path: &str) -> Result<ResolvedLink, String> {
    match client.session.metadata(path).await {
        Ok(metadata) => {
            let target = client.session.read_link(path).await.ok();
            Ok(ResolvedLink {
                is_dir: metadata.file_type().is_dir(),
                target,
            })
        }
        Err(err) => {
            // A dangling link is the common failure, and naming the target is
            // far more useful than the raw "no such file".
            match client.session.read_link(path).await {
                Ok(target) => Err(format!("broken symlink '{path}' -> '{target}'")),
                Err(_) => Err(format!("could not follow '{path}': {err}")),
            }
        }
    }
}

/// Read the seventh field of the account's `/etc/passwd` line.
async fn login_shell(sftp: &SftpSession, user: &str) -> Result<ShellInfo, String> {
    let contents = sftp
        .read("/etc/passwd")
        .await
        .map_err(|err| format!("could not read /etc/passwd: {err}"))?;
    let (shell, home) = shell_from_passwd(&String::from_utf8_lossy(&contents), user)
        .ok_or_else(|| format!("/etc/passwd has no login shell for '{user}'"))?;
    let kind = ShellKind::from_shell(&shell).ok_or_else(|| {
        format!(
            "Directory tracking is not available for {shell}; bash, zsh, csh and tcsh are supported"
        )
    })?;
    let home = if home.is_empty() {
        remote_home(sftp).await?
    } else {
        home
    };
    let rc = hook_rc_path(sftp, &home, kind).await;
    Ok(ShellInfo { shell, kind, rc })
}

/// Returns the login shell path and the account's home directory.
fn shell_from_passwd(contents: &str, user: &str) -> Option<(String, String)> {
    contents.lines().find_map(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() < 7 || fields[0].trim() != user {
            return None;
        }
        let shell = fields[6].trim();
        if shell.is_empty() {
            None
        } else {
            Some((shell.to_string(), fields[5].trim().to_string()))
        }
    })
}

/// Pick the startup file the shell actually reads.
///
/// tcsh reads `~/.tcshrc` *instead of* `~/.cshrc` when it exists, so an
/// existing file wins; otherwise the block goes in `~/.cshrc`, which both
/// shells read.
async fn hook_rc_path(sftp: &SftpSession, home: &str, kind: ShellKind) -> String {
    let file = match kind {
        ShellKind::Bash => ".bashrc",
        ShellKind::Zsh => ".zshrc",
        ShellKind::Csh => ".cshrc",
        ShellKind::Tcsh => {
            let tcshrc = join_home(home, ".tcshrc");
            if sftp.metadata(&tcshrc).await.is_ok() {
                ".tcshrc"
            } else {
                ".cshrc"
            }
        }
    };
    join_home(home, file)
}

fn join_home(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// Markers around the shell hook, so re-installing replaces the old block and
/// Remove can find it again.
pub(crate) const HOOK_BEGIN: &str = "# >>> rustterm directory tracking >>>";
pub(crate) const HOOK_END: &str = "# <<< rustterm directory tracking <<<";

/// Emits OSC 7 before every prompt. Works in bash (`PROMPT_COMMAND`) and zsh
/// (`precmd_functions`); the zsh-only syntax sits inside `eval`, so bash never
/// has to parse it.
const BASH_HOOK_BODY: &str = r#"__rustterm_cwd() {
    printf '\033]7;file://%s%s\033\\' "${HOSTNAME:-localhost}" "$PWD"
}
if [ -n "$ZSH_VERSION" ]; then
    eval 'typeset -ga precmd_functions; (( ${precmd_functions[(Ie)__rustterm_cwd]} )) || precmd_functions+=(__rustterm_cwd)'
else
    case "$PROMPT_COMMAND" in
        *__rustterm_cwd*) ;;
        *) PROMPT_COMMAND="__rustterm_cwd${PROMPT_COMMAND:+;$PROMPT_COMMAND}" ;;
    esac
fi"#;

/// csh/tcsh run `cwdcmd` after every directory change. Both shells only have
/// one such alias, so installing replaces whatever was there; the dialog
/// warns about that and the file backup keeps the original.
const CSH_HOOK_BODY: &str = r#"if ( $?prompt ) then
    alias cwdcmd 'printf "\033]7;file://%s\a" "$cwd"'
    printf "\033]7;file://%s\a" "$cwd"
endif"#;

fn hook_body(kind: ShellKind) -> &'static str {
    match kind {
        ShellKind::Bash | ShellKind::Zsh => BASH_HOOK_BODY,
        ShellKind::Csh | ShellKind::Tcsh => CSH_HOOK_BODY,
    }
}

fn hook_block(kind: ShellKind) -> String {
    format!("{HOOK_BEGIN}\n{}\n{HOOK_END}\n", hook_body(kind))
}

/// Remove a marked block, leaving the rest of the file untouched.
///
/// A file with only one of the two markers is returned unchanged rather than
/// truncated: a half-written block is not ours to guess at.
fn strip_hook_block(text: &str) -> String {
    if !text.contains(HOOK_BEGIN) || !text.contains(HOOK_END) {
        return text.to_string();
    }
    let mut out = String::new();
    let mut inside = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']).trim();
        if trimmed == HOOK_BEGIN {
            inside = true;
            continue;
        }
        if inside {
            if trimmed == HOOK_END {
                inside = false;
            }
            continue;
        }
        out.push_str(line);
    }
    // Drop the blank line the block was separated with.
    while out.ends_with("\n\n") {
        out.pop();
    }
    out
}

/// Replace any existing block with a fresh one, so installing twice is safe.
fn install_hook_block(text: &str, kind: ShellKind) -> String {
    let mut base = strip_hook_block(text);
    if !base.is_empty() {
        if !base.ends_with('\n') {
            base.push('\n');
        }
        // One blank line between the user's file and our block.
        base.push('\n');
    }
    base.push_str(&hook_block(kind));
    base
}

/// Add or remove the hook in `rc`, creating a one-time backup before writing.
async fn edit_hook(
    client: &SftpClient,
    rc: &str,
    kind: ShellKind,
    install: bool,
) -> Result<HookResult, String> {
    let existing = match client.session.metadata(rc).await {
        Ok(_) => client
            .session
            .read(rc)
            .await
            .map_err(|err| format!("could not read '{rc}': {err}"))?,
        // A missing startup file is normal: it is created below.
        Err(_) => Vec::new(),
    };
    let text = String::from_utf8_lossy(&existing).into_owned();
    let next = if install {
        install_hook_block(&text, kind)
    } else {
        strip_hook_block(&text)
    };
    if next == text {
        return Ok(HookResult {
            path: rc.to_string(),
            backup: None,
            changed: false,
        });
    }

    // Back up the original once; a second install must not replace the backup
    // with an already-modified file. `write` opens with WRITE only, so it fails
    // with "No such file" when the file does not exist and never truncates
    // when it does; use `create` (CREATE | TRUNCATE | WRITE) instead.
    let backup = format!("{rc}.rustterm-backup");
    let mut backup_path = None;
    if install
        && !existing.is_empty()
        && client
            .session
            .metadata(&backup)
            .await
            .map_err(|err| err.to_string())
            .map(|_| ())
            .is_err()
    {
        // Only treat "not found" as "create the backup"; other errors are
        // reported verbatim.
        match client.session.metadata(&backup).await {
            Ok(_) => {}
            Err(err) if err.to_string().to_lowercase().contains("no such") => {
                let mut file = client
                    .session
                    .create(&backup)
                    .await
                    .map_err(|err| format!("could not create '{backup}': {err}"))?;
                file.write_all(&existing)
                    .await
                    .map_err(|err| format!("could not write '{backup}': {err}"))?;
                file.flush()
                    .await
                    .map_err(|err| format!("could not finish writing '{backup}': {err}"))?;
                file.close()
                    .await
                    .map_err(|err| format!("could not close '{backup}': {err}"))?;
                backup_path = Some(backup.clone());
            }
            Err(err) => {
                return Err(format!("could not stat '{backup}': {err}"));
            }
        }
    }

    let mut file = client
        .session
        .create(rc)
        .await
        .map_err(|err| format!("could not create '{rc}': {err}"))?;
    file.write_all(next.as_bytes())
        .await
        .map_err(|err| format!("could not write '{rc}': {err}"))?;
    file.flush()
        .await
        .map_err(|err| format!("could not finish writing '{rc}': {err}"))?;
    file.close()
        .await
        .map_err(|err| format!("could not close '{rc}': {err}"))?;
    Ok(HookResult {
        path: rc.to_string(),
        backup: backup_path,
        changed: true,
    })
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

pub(crate) struct ClientHandler {
    host: String,
    port: u16,
    /// `None` means no UI is listening; unknown hosts are then refused rather
    /// than silently trusted.
    prompt_tx: Option<std_mpsc::Sender<HostKeyPrompt>>,
    failure: Arc<Mutex<Option<HostKeyFailure>>>,
    x11: Arc<X11Shared>,
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

    /// Proxy one server-opened X11 channel to the local X server.
    async fn server_channel_open_x11(
        &mut self,
        channel: Channel<ClientMsg>,
        _originator_address: &str,
        _originator_port: u32,
        _session: &mut russh::client::Session,
    ) -> std::result::Result<(), Self::Error> {
        let Some(config) = self.x11.config() else {
            // Forwarding was not granted; drop the channel.
            return Ok(());
        };
        let shared = Arc::clone(&self.x11);
        tokio::spawn(async move {
            shared.opened();
            let error = x11::proxy(channel, config.display, config.auth).await.err();
            shared.closed(error);
        });
        Ok(())
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

    #[test]
    fn transfer_progress_reports_bytes_and_fraction() {
        let progress = TransferProgress::new("Uploading notes.txt…");
        assert_eq!(progress.label(), "Uploading notes.txt…");
        // Before the pre-walk finishes the size is unknown, not zero.
        assert_eq!(progress.total_bytes(), None);
        assert_eq!(progress.fraction(), None);

        progress.set_total_bytes(100);
        progress.add_bytes(40);
        assert_eq!(progress.bytes_done(), 40);
        assert!((progress.fraction().unwrap() - 0.4).abs() < 1e-6);

        // Progress is clamped, so an over-report never exceeds 100%.
        progress.add_bytes(100);
        assert_eq!(progress.fraction(), Some(1.0));
    }

    #[test]
    fn transfer_progress_counts_items_and_cancels() {
        let progress = TransferProgress::new("Downloading");
        assert_eq!(progress.total_items(), None);
        progress.set_total_items(3);
        progress.item_done();
        assert_eq!(progress.items_done(), 1);
        assert_eq!(progress.total_items(), Some(3));

        assert!(!progress.is_cancelled());
        progress.cancel();
        assert!(progress.is_cancelled());
    }

    #[test]
    fn strips_directory_components_from_remote_paths() {
        assert_eq!(remote_file_name("/home/alice/report.txt"), "report.txt");
        assert_eq!(remote_file_name("/home/alice/"), "alice");
        assert_eq!(remote_file_name("report.txt"), "report.txt");
        assert_eq!(remote_file_name("/"), "/");
    }

    #[test]
    fn temp_paths_are_unique_siblings() {
        let remote_a = remote_temp_path("/home/alice/notes.txt");
        let remote_b = remote_temp_path("/home/alice/notes.txt");
        assert_ne!(remote_a, remote_b);
        assert!(remote_a.starts_with("/home/alice/notes.txt."));
        assert!(remote_a.ends_with(".part"));

        let local_a = local_temp_path(Path::new("/tmp/notes.txt"));
        let local_b = local_temp_path(Path::new("/tmp/notes.txt"));
        assert_ne!(local_a, local_b);
        assert_eq!(local_a.parent(), Some(Path::new("/tmp")));
        assert!(local_a.to_string_lossy().starts_with("/tmp/notes.txt."));
        assert!(local_a.to_string_lossy().ends_with(".part"));
    }

    #[test]
    fn summarises_single_and_multiple_failures() {
        assert_eq!(summarise_failures(vec!["boom".to_string()]), "boom");
        let summary = summarise_failures(vec!["a".to_string(), "b".into(), "c".into()]);
        assert_eq!(summary, "3 items failed: a");
    }

    #[test]
    fn picks_the_next_free_copy_name() {
        let dir = std::env::temp_dir().join(format!("rustterm-copy-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Nothing exists yet, so the original name wins.
        assert_eq!(unique_local_path(&dir, "notes.txt"), dir.join("notes.txt"));
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();
        assert_eq!(
            unique_local_path(&dir, "notes.txt"),
            dir.join("notes.txt (copy)")
        );
        std::fs::write(dir.join("notes.txt (copy)"), b"x").unwrap();
        assert_eq!(
            unique_local_path(&dir, "notes.txt"),
            dir.join("notes.txt (copy 2)")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn client_config_advertises_no_compression() {
        // russh 0.45's zlib compressor corrupts incompressible packets; the
        // client must not offer a compression algorithm until we upgrade.
        let config = client_config();
        assert_eq!(config.preferred.compression.len(), 1);
        assert_eq!(config.preferred.compression[0], compression::NONE);
    }

    #[test]
    fn classifies_transport_loss_from_library_errors() {
        assert!(is_connection_loss("could not write 'x': sender dropped"));
        assert!(is_connection_loss("session closed"));
        assert!(is_connection_loss("Broken pipe"));
        assert!(!is_connection_loss("permission denied"));
        assert_eq!(
            normalise_transfer_error("could not write 'x': sender dropped".to_string()),
            CONNECTION_LOST
        );
        assert_eq!(
            normalise_transfer_error("permission denied".to_string()),
            "permission denied"
        );
    }

    #[test]
    fn explains_common_connect_failures() {
        assert_eq!(
            explain_connect_error(
                "connect to u@h:22: connect to u@h:22: authentication failed: password: rejected by server"
            ),
            "Login failed — the server rejected this username or password."
        );
        assert_eq!(
            explain_connect_error("TCP connection failed: Connection refused (os error 111)"),
            "Connection refused — nothing is listening on that host and port."
        );
        assert_eq!(
            explain_connect_error(
                "failed to lookup address information: Name or service not known"
            ),
            "Host not found — check the hostname."
        );
        assert_eq!(
            explain_connect_error("timed out after 15s connecting to h:22"),
            "Connection timed out — the host did not answer. Check the host, port and network."
        );
        // A changed host key is already actionable and must not be shortened.
        let changed = "HOST KEY CHANGED for h:22 — the server presented a different key";
        assert_eq!(explain_connect_error(changed), changed);
    }

    #[test]
    fn classifies_sftp_timeouts() {
        assert!(is_sftp_timeout("could not inspect '/x.tar': Timeout"));
        assert!(!is_sftp_timeout("permission denied"));
        assert_eq!(
            normalise_transfer_error("could not inspect '/x.tar': Timeout".to_string()),
            SFTP_TIMEOUT
        );
    }

    #[test]
    fn keepalive_is_enabled() {
        let config = client_config();
        assert_eq!(config.keepalive_interval, Some(Duration::from_secs(15)));
        assert_eq!(config.keepalive_max, 3);
    }

    #[test]
    fn finds_the_login_shell_in_passwd() {
        let passwd = "\
root:x:0:0:root:/root:/bin/bash
alice:x:1000:1000:Alice:/home/alice:/usr/bin/zsh
empty:x:1001:1001::/home/empty:
short:x:1
";
        assert_eq!(
            shell_from_passwd(passwd, "alice"),
            Some(("/usr/bin/zsh".into(), "/home/alice".into()))
        );
        assert_eq!(
            shell_from_passwd(passwd, "root"),
            Some(("/bin/bash".into(), "/root".into()))
        );
        assert_eq!(shell_from_passwd(passwd, "empty"), None);
        assert_eq!(shell_from_passwd(passwd, "short"), None);
        assert_eq!(shell_from_passwd(passwd, "nobody"), None);
    }

    #[test]
    fn recognises_supported_shells() {
        assert_eq!(ShellKind::from_shell("/bin/bash"), Some(ShellKind::Bash));
        assert_eq!(ShellKind::from_shell("/usr/bin/zsh"), Some(ShellKind::Zsh));
        assert_eq!(ShellKind::from_shell("/bin/csh"), Some(ShellKind::Csh));
        assert_eq!(
            ShellKind::from_shell("/usr/local/bin/tcsh"),
            Some(ShellKind::Tcsh)
        );
        assert_eq!(ShellKind::from_shell("/bin/sh"), None);
        assert_eq!(ShellKind::from_shell("/bin/fish"), None);
        assert_eq!(ShellKind::from_shell("bash"), Some(ShellKind::Bash));
    }

    #[test]
    fn display_prefix_matches_the_shell() {
        assert_eq!(ShellKind::Bash.display_prefix(1), "DISPLAY=:1 ");
        assert_eq!(ShellKind::Zsh.display_prefix(12), "DISPLAY=:12 ");
        assert_eq!(ShellKind::Csh.display_prefix(1), "setenv DISPLAY :1; ");
        assert_eq!(ShellKind::Tcsh.display_prefix(2), "setenv DISPLAY :2; ");
    }

    #[test]
    fn login_shell_output_ignores_startup_noise() {
        assert_eq!(
            parse_login_shell("banner from .cshrc\n/bin/csh\n").as_deref(),
            Some("/bin/csh")
        );
        assert_eq!(
            parse_login_shell("/usr/bin/zsh\r\n").as_deref(),
            Some("/usr/bin/zsh")
        );
        assert_eq!(parse_login_shell(""), None);
        assert_eq!(parse_login_shell("banner only\n"), None);
    }

    #[test]
    fn hook_block_installs_once_and_can_be_removed() {
        let original = "# my bashrc\nexport EDITOR=vim\n";
        let installed = install_hook_block(original, ShellKind::Bash);
        assert!(installed.starts_with(original.trim_end()));
        assert_eq!(installed.matches(HOOK_BEGIN).count(), 1);
        assert_eq!(installed.matches(HOOK_END).count(), 1);
        assert!(installed.contains("__rustterm_cwd"));

        // Installing again must replace, not append a second block.
        let again = install_hook_block(&installed, ShellKind::Bash);
        assert_eq!(again, installed);
        assert_eq!(again.matches(HOOK_BEGIN).count(), 1);

        // Removing restores the original text (modulo the blank separator).
        let restored = strip_hook_block(&installed);
        assert_eq!(restored, original);
    }

    #[test]
    fn csh_block_installs_once_and_can_be_removed() {
        let original = "set prompt = '\\% '\n";
        let installed = install_hook_block(original, ShellKind::Csh);
        assert!(installed.starts_with(original.trim_end()));
        assert!(installed.contains("alias cwdcmd"));
        assert_eq!(installed.matches(HOOK_BEGIN).count(), 1);

        // tcsh and csh share the body; installing twice is still idempotent.
        let again = install_hook_block(&installed, ShellKind::Tcsh);
        assert_eq!(again, installed);

        assert_eq!(strip_hook_block(&installed), original);
    }

    #[test]
    fn hook_block_creates_a_file_that_only_has_the_block() {
        let installed = install_hook_block("", ShellKind::Bash);
        assert!(installed.starts_with(HOOK_BEGIN));
        assert_eq!(strip_hook_block(&installed), "");

        let installed = install_hook_block("", ShellKind::Csh);
        assert!(installed.starts_with(HOOK_BEGIN));
        assert_eq!(strip_hook_block(&installed), "");
    }

    #[test]
    fn hook_block_leaves_partial_markers_alone() {
        let broken = format!("keep me\n{HOOK_BEGIN}\nhalf a block\n");
        assert_eq!(strip_hook_block(&broken), broken);
        // A file without markers is returned untouched.
        assert_eq!(strip_hook_block("plain\n"), "plain\n");
    }

    #[test]
    fn session_hooks_are_one_line() {
        for kind in [
            ShellKind::Bash,
            ShellKind::Zsh,
            ShellKind::Csh,
            ShellKind::Tcsh,
        ] {
            let line = kind.session_hook();
            assert!(!line.contains('\n'), "{} hook spans lines", kind.name());
            assert!(!line.is_empty());
        }
    }

    #[test]
    fn hook_snippet_is_valid_bash() {
        let bash = std::path::Path::new("/bin/bash");
        if !bash.exists() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("rustterm-hook-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("hook.sh");
        std::fs::write(&script, format!("{}\n", hook_body(ShellKind::Bash))).unwrap();
        let status = std::process::Command::new(bash)
            .arg("-n")
            .arg(&script)
            .status()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(status.success(), "bash -n rejected the hook snippet");
    }

    #[test]
    fn csh_snippets_parse_when_csh_is_installed() {
        // csh is not on every machine; the live server check covers those.
        let Some(csh) = ["/bin/csh", "/usr/bin/csh", "/bin/tcsh", "/usr/bin/tcsh"]
            .into_iter()
            .find(|path| std::path::Path::new(path).exists())
        else {
            return;
        };
        let dir = std::env::temp_dir().join(format!("rustterm-csh-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("hook.csh");
        std::fs::write(&script, format!("{}\n", hook_body(ShellKind::Csh))).unwrap();
        let status = std::process::Command::new(csh)
            .arg("-f")
            .arg(&script)
            .status()
            .unwrap();
        let one_line = std::process::Command::new(csh)
            .arg("-f")
            .arg("-c")
            .arg(ShellKind::Csh.session_hook())
            .status()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(status.success(), "{csh} rejected the hook snippet");
        assert!(one_line.success(), "{csh} rejected the session hook");
    }

    #[test]
    fn parses_vncserver_list_output() {
        // TigerVNC
        let tiger = "\
TigerVNC server sessions:

X DISPLAY #     PROCESS ID
:1              12345
:2              12346
";
        assert_eq!(parse_vncserver_list(tiger), Some(1));

        // TightVNC / TurboVNC print the same table shape.
        let turbo = "\
     DISPLAY #    PID
    :3         22222
";
        assert_eq!(parse_vncserver_list(turbo), Some(3));

        // No sessions.
        assert_eq!(parse_vncserver_list(""), None);
        assert_eq!(
            parse_vncserver_list("TigerVNC server sessions:\n\nX DISPLAY #     PROCESS ID\n"),
            None
        );
        // A header line that merely contains a colon is not a display.
        assert_eq!(
            parse_vncserver_list("Creating VNC server for user@host:\n"),
            None
        );
    }

    #[test]
    fn parses_vncserver_create_output() {
        let tiger = "New 'host:1 (user)' desktop is host:1.\n\
                      Log file is /home/user/.vnc/host:1.log\n";
        assert_eq!(parse_vncserver_create(tiger), Some(1));

        let tight = "Desktop 'host:12' started on port 5912\n";
        assert_eq!(parse_vncserver_create(tight), None);

        assert_eq!(parse_vncserver_create("command not found\n"), None);
    }

    #[test]
    fn vnc_display_number_follows_the_port() {
        let target = VncTarget {
            host: "127.0.0.1".into(),
            port: 5901,
        };
        assert_eq!(target.display_number(), Some(1));
        let custom = VncTarget {
            host: "10.0.0.1".into(),
            port: 6000,
        };
        assert_eq!(custom.display_number(), None);
    }

    #[test]
    fn finds_the_lowest_vnc_listener() {
        let ss = "\
State  Recv-Q Send-Q Local Address:Port Peer Address:Port
LISTEN 0      5      0.0.0.0:5901       0.0.0.0:*
LISTEN 0      5      [::]:5903          [::]:*
LISTEN 0      5      0.0.0.0:22          0.0.0.0:*
";
        assert_eq!(
            parse_vnc_listener(ss),
            Some(VncTarget {
                host: "127.0.0.1".into(),
                port: 5901
            })
        );

        // A very old netstat prints the state as the last column.
        let netstat =
            "tcp 0 0 127.0.0.1:5902 0.0.0.0:* LISTEN\ntcp 0 0 1.2.3.4:5904 0.0.0.0:* LISTEN\n";
        assert_eq!(
            parse_vnc_listener(netstat),
            Some(VncTarget {
                host: "127.0.0.1".into(),
                port: 5902
            })
        );

        // Non-VNC ports and established connections are ignored.
        assert_eq!(parse_vnc_listener("LISTEN 0 5 0.0.0.0:22 0.0.0.0:*"), None);
        assert_eq!(
            parse_vnc_listener("tcp 0 0 127.0.0.1:59234 127.0.0.1:5000 ESTABLISHED"),
            None
        );
        assert_eq!(parse_vnc_listener(""), None);
    }

    #[test]
    fn vnc_setting_accepts_host_and_host_port_forms() {
        assert_eq!(VncTarget::parse("", 5900), None);
        assert_eq!(
            VncTarget::parse("vnc.example", 5900),
            Some(VncTarget {
                host: "vnc.example".into(),
                port: 5900
            })
        );
        assert_eq!(
            VncTarget::parse("10.0.0.4:5903", 5900),
            Some(VncTarget {
                host: "10.0.0.4".into(),
                port: 5903
            })
        );
    }
}
