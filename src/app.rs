//! Application shell: tab bar, connect form, terminal views and SFTP panel.
//!
//! Tabs: index 0 is *always* the "New Session" form; connected sessions live
//! at indices 1..N. Only one connect attempt may be in flight at a time.
//!
//! `SshSession::connect` blocks through auth, so it runs on a background
//! thread and the result is polled each frame (handoff flaw F1). Closing a tab
//! or pressing Disconnect sends on `disconnect_tx` so the SSH connection is
//! torn down cleanly (handoff flaw F2).
//!
//! Host-key verification (handoff flaw F3) runs on that same background thread
//! and may pause the handshake to ask the user about an unknown host. The
//! prompt arrives on its own channel, is rendered as a modal, and is answered
//! over a oneshot that the SSH handler is awaiting.
//!
//! Resize forwarding uses `TerminalView::take_resize()`.
//!
//! Repaint gating: egui is left to sleep when nothing has happened. The SSH
//! thread calls `request_repaint` when output arrives, while a timer is only
//! needed while a connection attempt is still in progress.

use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::time::Duration;

use eframe::egui;

use crate::input;
use crate::secrets;
use crate::sessions::{SessionProfile, SessionStore};
use crate::sftp::{format_age, format_size, join_path, SftpExplorer};
use crate::ssh::{HostKeyPrompt, SshConfig, SshSession, Wake};
use crate::terminal::{TermAction, TerminalView};

const NEW_SESSION_TAB: usize = 0;
const DEFAULT_COLS: usize = 80;
const DEFAULT_ROWS: usize = 24;

/// Delay between frames while a connection attempt is still in progress. Once
/// connected, the SSH thread wakes the UI itself when output arrives.
const CONNECT_POLL_MS: u64 = 8;

/// Upper bound on remote bytes decoded in a single frame. This keeps a large
/// burst from stalling the UI while still allowing tens of MiB/s sustained.
const MAX_OUTPUT_PER_FRAME: usize = 256 * 1024;

/// Whether this build includes the optional OS-keyring support.
const KEYRING_ENABLED: bool = cfg!(feature = "keyring-secrets");

pub struct RustTermApp {
    sessions: Vec<ActiveSession>,
    /// 0 = New Session form; `i > 0` = `sessions[i - 1]`.
    active: usize,
    form: ConnectForm,
    connecting: Option<ConnectingState>,
    /// Persisted profiles loaded at startup (F19: no passwords stored).
    store: SessionStore,
    /// Name used by the "Save profile" button.
    profile_name: String,
    /// Highlighted entry in the saved-session list.
    selected_profile: Option<usize>,
    status: StatusMessage,
    /// Whether the SFTP side panel is shown for connected sessions.
    show_sftp: bool,
}

/// One line of user feedback below the connect form.
#[derive(Default)]
struct StatusMessage {
    text: String,
    is_error: bool,
}

struct ActiveSession {
    title: String,
    ssh: SshSession,
    terminal: TerminalView,
    /// Per-session remote file browser. Each tab keeps its own directory.
    sftp: SftpExplorer,
    /// Set once so the terminal takes focus on first appearance.
    focus_requested: bool,
}

struct ConnectingState {
    rx: std_mpsc::Receiver<Result<SshSession, String>>,
    prompt_rx: std_mpsc::Receiver<HostKeyPrompt>,
    /// Taken out to draw the modal, put back if the user has not answered yet.
    prompt: Option<HostKeyPrompt>,
    label: String,
}

struct ConnectForm {
    host: String,
    port: String,
    user: String,
    password: String,
    /// Opt-in: store the password in the OS keyring for this profile.
    save_password: bool,
    /// Try private-key auth before the password.
    use_key: bool,
    /// Explicit key file; blank means auto-detect `~/.ssh/id_*`.
    key_path: String,
    /// Offer identities from `$SSH_AUTH_SOCK`.
    use_agent: bool,
}

impl Default for ConnectForm {
    fn default() -> Self {
        Self {
            host: "localhost".to_string(),
            port: "22".to_string(),
            user: String::new(),
            password: String::new(),
            save_password: false,
            use_key: false,
            key_path: String::new(),
            use_agent: false,
        }
    }
}

impl Default for RustTermApp {
    fn default() -> Self {
        let store = SessionStore::load_default();
        let status = match store.error() {
            Some(err) => StatusMessage {
                text: err.to_string(),
                is_error: true,
            },
            None => StatusMessage::default(),
        };

        Self {
            sessions: Vec::new(),
            active: NEW_SESSION_TAB,
            form: ConnectForm::default(),
            connecting: None,
            store,
            profile_name: String::new(),
            selected_profile: None,
            status,
            show_sftp: true,
        }
    }
}

impl RustTermApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        Self::default()
    }

    fn set_error(&mut self, text: impl Into<String>) {
        self.status.text = text.into();
        self.status.is_error = true;
    }

    fn set_info(&mut self, text: impl Into<String>) {
        self.status.text = text.into();
        self.status.is_error = false;
    }

    fn clear_status(&mut self) {
        self.status.text.clear();
        self.status.is_error = false;
    }

    fn start_connect(&mut self, ctx: &egui::Context) {
        if self.connecting.is_some() {
            return; // one connect attempt at a time
        }

        let host = self.form.host.trim().to_string();
        let port: u16 = match self.form.port.trim().parse() {
            Ok(p) => p,
            Err(_) => {
                self.set_error("Port must be a number");
                return;
            }
        };
        let user = self.form.user.trim().to_string();
        if host.is_empty() || user.is_empty() {
            self.set_error("Host and user are required");
            return;
        }
        if self.form.password.is_empty() && !self.form.use_key && !self.form.use_agent {
            self.set_error("Type a password or enable key/agent authentication");
            return;
        }

        let config = SshConfig {
            host,
            port,
            user,
            password: self.form.password.clone(),
            use_key: self.form.use_key,
            key_path: self.form.key_path.trim().to_string(),
            use_agent: self.form.use_agent,
        };
        let label = format!("{}@{}:{}", config.user, config.host, config.port);

        // The SSH thread wakes egui as soon as output arrives. Without this,
        // remote echo on a high-latency link was only displayed on the next
        // keyboard/mouse event (the previous 8 ms polling tail had long expired).
        let wake: Wake = {
            let ctx = ctx.clone();
            Arc::new(move || ctx.request_repaint())
        };

        let (tx, rx) = std_mpsc::channel();
        let (prompt_tx, prompt_rx) = std_mpsc::channel();
        std::thread::spawn(move || {
            let result = SshSession::connect(config, Some(prompt_tx), Some(wake))
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });

        self.connecting = Some(ConnectingState {
            rx,
            prompt_rx,
            prompt: None,
            label,
        });
        self.clear_status();
        self.active = NEW_SESSION_TAB;
    }

    /// Poll the background connect attempt.
    ///
    /// Returns `true` if the state changed this frame (prompt surfaced, session
    /// added, or error reported). The caller uses that to extend the polling
    /// window.
    fn poll_connecting(&mut self) -> bool {
        // 1. A host-key prompt arrives while the handshake is paused, so it can
        //    show up before the connect result does.
        if let Some(state) = self.connecting.as_mut() {
            if state.prompt.is_none() {
                if let Ok(prompt) = state.prompt_rx.try_recv() {
                    state.prompt = Some(prompt);
                    return true;
                }
            }
        }

        // 2. Has the background connect finished?
        enum Polled {
            Pending,
            Ready(Result<SshSession, String>),
            WorkerGone,
        }

        let polled = match self.connecting.as_ref() {
            None => return false,
            Some(state) => match state.rx.try_recv() {
                Ok(result) => Polled::Ready(result),
                Err(std_mpsc::TryRecvError::Empty) => Polled::Pending,
                Err(std_mpsc::TryRecvError::Disconnected) => Polled::WorkerGone,
            },
        };

        match polled {
            Polled::Pending => false,
            Polled::WorkerGone => {
                self.connecting = None;
                self.set_error("Connect failed: worker thread died");
                true
            }
            Polled::Ready(Ok(ssh)) => {
                let state = self.connecting.take().expect("checked above");
                let input_tx = ssh.input_sender();
                // Resolve the remote home before `ssh` is moved into the session.
                let sftp = SftpExplorer::start(&ssh);
                self.sessions.push(ActiveSession {
                    title: state.label,
                    ssh,
                    terminal: TerminalView::new(DEFAULT_COLS, DEFAULT_ROWS, input_tx),
                    sftp,
                    focus_requested: false,
                });
                // New session is the last tab; make it active.
                self.active = self.sessions.len();
                self.form.password.clear();
                true
            }
            Polled::Ready(Err(err)) => {
                self.connecting = None;
                self.set_error(format!("Connect failed: {err}"));
                true
            }
        }
    }

    /// Modal trust-on-first-use prompt for an unknown host key.
    ///
    /// The handshake is paused on the other side of `HostKeyPrompt`, so there
    /// is no timeout to race against here beyond the server's login grace time.
    fn host_key_prompt_ui(&mut self, ctx: &egui::Context) {
        let Some(prompt) = self
            .connecting
            .as_mut()
            .and_then(|state| state.prompt.take())
        else {
            return;
        };

        let mut answer: Option<bool> = None;

        egui::Window::new("Unknown host key")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_max_width(480.0);
                ui.label(format!(
                    "The authenticity of host '{}:{}' can't be established.",
                    prompt.host, prompt.port
                ));
                ui.add_space(6.0);
                ui.label("Host key fingerprint:");
                ui.monospace(&prompt.fingerprint);
                ui.add_space(8.0);
                ui.label("Accept this key and record it in ~/.ssh/known_hosts?");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Accept & save").clicked() {
                        answer = Some(true);
                    }
                    if ui.button("Reject").clicked() {
                        answer = Some(false);
                    }
                });
            });

        match answer {
            Some(true) => prompt.accept(),
            Some(false) => prompt.reject(),
            None => {
                // Still waiting: put it back so the next frame can draw it.
                if let Some(state) = self.connecting.as_mut() {
                    state.prompt = Some(prompt);
                }
            }
        }
    }

    /// Remove a session, disconnect its SSH, and fix up `active`/focus.
    fn close_session(&mut self, i: usize) {
        if i >= self.sessions.len() {
            return;
        }
        let removed = self.sessions.remove(i);
        removed.ssh.disconnect();

        if self.active == i + 1 {
            self.active = NEW_SESSION_TAB;
        } else if self.active > i + 1 {
            self.active -= 1;
        }
        self.reset_focus_for_active();
    }

    fn reset_focus_for_active(&mut self) {
        if let Some(idx) = self.active.checked_sub(1) {
            if let Some(s) = self.sessions.get_mut(idx) {
                s.focus_requested = false;
            }
        }
    }

    /// Drain output for *every* session, not just the visible tab.
    ///
    /// Leaving background sessions unread would stall their SSH window and
    /// make switching tabs dump a large backlog all at once. Titles/bells also
    /// arrive for background tabs, so process them here.
    ///
    /// At most [`MAX_OUTPUT_PER_FRAME`] bytes are decoded per frame. A huge
    /// burst (`cat` of a large file) would otherwise block painting for
    /// hundreds of milliseconds; the remainder is picked up on the next frame
    /// and a repaint is requested so it is not left waiting for a wake-up.
    fn pump_sessions(&mut self, ctx: &egui::Context) -> bool {
        let mut activity = false;
        let mut budget = MAX_OUTPUT_PER_FRAME;

        for session in &mut self.sessions {
            // SFTP replies arrive asynchronously; polling them for every tab
            // keeps a listing ready even if the sidebar is currently hidden.
            if session.sftp.poll(&session.ssh) {
                activity = true;
            }
            while budget > 0 {
                let Some(chunk) = session.ssh.try_read() else {
                    break;
                };
                budget = budget.saturating_sub(chunk.len());
                session.terminal.feed(&chunk);
                activity = true;
            }
            if let Some(title) = session.terminal.take_title() {
                session.title = title;
                activity = true;
            }
            if session.terminal.take_bell() {
                log::info!("terminal bell");
            }
        }

        if budget == 0 {
            // Work was left over: keep painting until every queue is empty.
            ctx.request_repaint();
        }

        activity
    }

    fn tabs_ui(&mut self, ui: &mut egui::Ui) {
        let mut switch_to: Option<usize> = None;
        let mut close: Option<usize> = None;

        ui.horizontal(|ui| {
            // Keep the Files toggle outside the scroll area so a long tab list
            // cannot hide it.
            if self.active != NEW_SESSION_TAB {
                if ui
                    .selectable_label(self.show_sftp, "Files")
                    .on_hover_text("Show or hide the remote file panel")
                    .clicked()
                {
                    self.show_sftp = !self.show_sftp;
                }
                ui.separator();
            }

            egui::ScrollArea::horizontal()
                .id_salt("tab_bar_scroll")
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        // New Session — always first, never closable.
                        if ui
                            .selectable_label(self.active == NEW_SESSION_TAB, "＋ New Session")
                            .clicked()
                        {
                            switch_to = Some(NEW_SESSION_TAB);
                        }

                        for (i, s) in self.sessions.iter().enumerate() {
                            let tab_index = i + 1;
                            let selected = self.active == tab_index;
                            ui.horizontal(|ui| {
                                if ui.selectable_label(selected, &s.title).clicked() {
                                    switch_to = Some(tab_index);
                                }
                                if ui
                                    .small_button("×")
                                    .on_hover_text("Close session")
                                    .clicked()
                                {
                                    close = Some(i);
                                }
                            });
                        }
                    });
                });
        });

        if let Some(tab) = switch_to {
            if tab != self.active {
                self.active = tab;
                self.reset_focus_for_active();
            }
        }

        if let Some(i) = close {
            self.close_session(i);
        }
    }

    /// Fill the connect form from a saved profile.
    fn load_profile(&mut self, index: usize) {
        let Some(profile) = self.store.profiles().get(index).cloned() else {
            return;
        };

        self.form.host = profile.host;
        self.form.port = profile.port.to_string();
        self.form.user = profile.user;
        self.form.save_password = KEYRING_ENABLED && profile.save_password;
        self.form.use_key = profile.use_key;
        self.form.key_path = profile.key_path.clone();
        self.form.use_agent = profile.use_agent;
        // The password is only ever read from the keyring, never from TOML.
        self.form.password.clear();
        self.profile_name = profile.name.clone();
        self.selected_profile = Some(index);
        self.clear_status();

        if profile.save_password {
            if KEYRING_ENABLED {
                match secrets::load(&profile.name) {
                    Ok(Some(password)) => self.form.password = password,
                    Ok(None) => self.set_error(format!(
                        "No password is stored for '{}'. Type one and press Save profile to store it.",
                        profile.name
                    )),
                    Err(err) => {
                        self.set_error(format!("Could not read the stored password: {err}"))
                    }
                }
            } else {
                self.set_error(format!(
                    "'{}' stores its password in the system keyring, but this build \
                     has no keyring support. Type the password manually.",
                    profile.name
                ));
            }
        }
    }

    fn save_profile(&mut self) {
        let host = self.form.host.trim().to_string();
        let user = self.form.user.trim().to_string();
        let port: u16 = match self.form.port.trim().parse() {
            Ok(port) => port,
            Err(_) => {
                self.set_error("Port must be a number");
                return;
            }
        };
        let requested = self.form.save_password;

        // A blank name falls back to user@host, but only when the password is
        // *not* being stored: keyring entries are identified by profile name,
        // so an explicit name is mandatory in that case.
        let mut name = self.profile_name.trim().to_string();
        if name.is_empty() && !requested && !host.is_empty() && !user.is_empty() {
            name = format!("{user}@{host}");
        }

        // Whether the previous version of this profile had a keyring entry.
        // Used to decide if unchecking the box needs a keyring call at all.
        let had_stored_password = self
            .store
            .profiles()
            .iter()
            .find(|existing| existing.name == name)
            .is_some_and(|existing| existing.save_password);

        // In a lean build the checkbox is never shown, so keep any flag that
        // is already on disk instead of silently dropping the metadata.
        let save_password = if KEYRING_ENABLED {
            requested
        } else {
            had_stored_password
        };

        let profile = SessionProfile {
            name,
            host,
            port,
            user,
            save_password,
            use_key: self.form.use_key,
            key_path: self.form.key_path.trim().to_string(),
            use_agent: self.form.use_agent,
        };
        if let Err(message) = profile.validate() {
            self.set_error(message);
            return;
        }

        if KEYRING_ENABLED && requested {
            if self.form.password.is_empty() {
                self.set_error("Type a password to store it in the system keyring");
                return;
            }
            // Store the secret first: if the keyring is unavailable, do not
            // write a profile that claims to have a stored password.
            if let Err(err) = secrets::save(&profile.name, &self.form.password) {
                self.set_error(format!(
                    "Could not store the password in the system keyring: {err}"
                ));
                return;
            }
        }

        let saved_name = profile.name.clone();
        match self.store.upsert(profile) {
            Ok(index) => {
                self.profile_name = saved_name.clone();
                self.selected_profile = Some(index);

                if KEYRING_ENABLED && !requested && had_stored_password {
                    if let Err(err) = secrets::delete(&saved_name) {
                        self.set_error(format!(
                            "Profile saved, but its keyring password could not be removed: {err}"
                        ));
                        return;
                    }
                }

                self.set_info(format!("Saved profile '{saved_name}'"));
            }
            Err(err) => self.set_error(format!("Could not save profile: {err}")),
        }
    }

    fn delete_selected_profile(&mut self) {
        let Some(index) = self.selected_profile else {
            return;
        };

        match self.store.remove(index) {
            Ok(Some(profile)) => {
                self.selected_profile = None;

                if KEYRING_ENABLED && profile.save_password {
                    if let Err(err) = secrets::delete(&profile.name) {
                        self.set_error(format!(
                            "Deleted profile '{}', but its keyring password could not be removed: {err}",
                            profile.name
                        ));
                        return;
                    }
                }

                self.set_info(format!("Deleted profile '{}'", profile.name));
            }
            Ok(None) => self.selected_profile = None,
            Err(err) => {
                // The profile is still on disk; leave it selected.
                self.set_error(format!("Could not delete profile: {err}"));
            }
        }
    }

    /// Name field + save button, drawn right below the connect form.
    fn save_profile_ui(&mut self, ui: &mut egui::Ui) {
        let mut save_clicked = false;
        let name_hint = if self.form.save_password {
            "required to save password"
        } else {
            "blank = user@host"
        };

        ui.horizontal(|ui| {
            ui.label("Profile name");
            ui.add(
                egui::TextEdit::singleline(&mut self.profile_name)
                    .hint_text(name_hint)
                    .desired_width(220.0),
            );
            if ui
                .button("Save profile")
                .on_hover_text(
                    "Saves host, port and user; if the keyring box is checked, \
                     the password too (requires a profile name).",
                )
                .clicked()
            {
                save_clicked = true;
            }
        });

        if save_clicked {
            self.save_profile();
            // The name field was drawn before a blank name was replaced with
            // `user@host`, so redraw to show the stored name.
            ui.ctx().request_repaint();
        }
    }

    /// Saved-profile list plus the delete control.
    fn saved_sessions_ui(&mut self, ui: &mut egui::Ui) {
        let profiles = self.store.profiles().to_vec();
        let mut delete_clicked = false;
        let mut load: Option<usize> = None;

        let empty_hint = "No saved sessions yet. Fill in the form above and press Save profile.";
        if profiles.is_empty() {
            ui.label(egui::RichText::new(empty_hint).weak());
            return;
        }

        ui.label(format!("Saved sessions ({}):", profiles.len()));
        egui::ScrollArea::vertical()
            .id_salt("saved_sessions")
            .max_height(140.0)
            .show(ui, |ui| {
                for (index, profile) in profiles.iter().enumerate() {
                    let text = format!(
                        "{}  —  {}@{}:{}",
                        profile.name, profile.user, profile.host, profile.port
                    );
                    let response = ui.selectable_label(self.selected_profile == Some(index), text);
                    if response.clicked() {
                        load = Some(index);
                    }
                    // No double-click shortcut: passwords are not stored, so a
                    // one-click connect would usually fail authentication.
                    response.on_hover_text("Click to load into the form");
                }
            });

        ui.horizontal(|ui| {
            let can_delete = self.selected_profile.is_some();
            if ui
                .add_enabled(can_delete, egui::Button::new("Delete selected"))
                .on_hover_text("Removes the profile from sessions.toml")
                .clicked()
            {
                delete_clicked = true;
            }
            ui.label(egui::RichText::new(format!("File: {}", self.store.path().display())).weak());
        });

        if let Some(index) = load {
            self.load_profile(index);
            // The form is drawn above this list, so it must be redrawn with
            // the newly loaded values (and any keyring error).
            ui.ctx().request_repaint();
        }
        if delete_clicked {
            self.delete_selected_profile();
            // Redraw the list without the removed entry.
            ui.ctx().request_repaint();
        }
    }

    fn new_session_ui(&mut self, ui: &mut egui::Ui) {
        ui.heading("New SSH session");
        ui.add_space(8.0);

        egui::Grid::new("connect_form")
            .num_columns(2)
            .spacing([10.0, 6.0])
            .show(ui, |ui| {
                ui.label("Host");
                ui.text_edit_singleline(&mut self.form.host);
                ui.end_row();

                ui.label("Port");
                ui.text_edit_singleline(&mut self.form.port);
                ui.end_row();

                ui.label("User");
                ui.text_edit_singleline(&mut self.form.user);
                ui.end_row();

                ui.label("Public key");
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.form.use_key, "Use");
                    ui.add_enabled(
                        self.form.use_key,
                        egui::TextEdit::singleline(&mut self.form.key_path)
                            .hint_text("blank = auto-detect ~/.ssh/id_*")
                            .desired_width(240.0),
                    );
                })
                .response
                .on_hover_text(
                    "Tried before the password. Leave the path blank to use \
                     ~/.ssh/id_ed25519, id_ecdsa, id_rsa or id_dsa.",
                );
                ui.end_row();

                ui.label("SSH agent");
                ui.checkbox(
                    &mut self.form.use_agent,
                    "Use identities from SSH_AUTH_SOCK",
                )
                .on_hover_text(
                    "Tried after a key file and before the password. Best for \
                     passphrase-protected keys loaded with ssh-add.",
                );
                ui.end_row();

                ui.label("Password");
                ui.add(egui::TextEdit::singleline(&mut self.form.password).password(true))
                    .on_hover_text("Used when key/agent authentication is disabled or fails.");
                ui.end_row();

                // Hidden entirely in lean builds (`--no-default-features`).
                if KEYRING_ENABLED {
                    ui.label("");
                    ui.checkbox(
                        &mut self.form.save_password,
                        "Save password in the system keyring",
                    )
                    .on_hover_text(
                        "Optional, per profile. Requires an explicit profile name. \
                         Stored by the OS keyring, never in sessions.toml.",
                    );
                    ui.end_row();
                }
            });

        ui.add_space(8.0);

        let connecting = self.connecting.is_some();
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!connecting, egui::Button::new("Connect"))
                .clicked()
            {
                self.start_connect(ui.ctx());
            }
            if let Some(state) = &self.connecting {
                ui.spinner();
                if state.prompt.is_some() {
                    ui.label("Waiting for host key confirmation…");
                } else {
                    ui.label(format!("Connecting to {}…", state.label));
                }
            }
        });

        ui.add_space(4.0);
        self.save_profile_ui(ui);

        if !self.status.text.is_empty() {
            ui.add_space(8.0);
            let color = if self.status.is_error {
                egui::Color32::LIGHT_RED
            } else {
                egui::Color32::LIGHT_GREEN
            };
            ui.colored_label(color, &self.status.text);
        }

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(6.0);
        self.saved_sessions_ui(ui);
    }

    /// Render the remote file browser for one session.
    ///
    /// Returns `true` when the panel started a request, changed selection or was
    /// hidden, so the caller can request another frame.
    fn sftp_ui(&mut self, ui: &mut egui::Ui, index: usize) -> bool {
        let Some(session) = self.sessions.get_mut(index) else {
            return false;
        };
        // Split the borrow: the explorer mutates while the session only answers
        // requests.
        let (ssh, explorer) = (&session.ssh, &mut session.sftp);
        let mut activity = false;
        let mut hide = false;

        ui.horizontal(|ui| {
            ui.heading("Remote files");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button("×")
                    .on_hover_text("Hide the file panel")
                    .clicked()
                {
                    hide = true;
                }
            });
        });

        if let Some(reason) = explorer.unavailable.clone() {
            ui.add_space(4.0);
            ui.colored_label(ui.visuals().warn_fg_color, "SFTP is unavailable");
            ui.label(reason);
        } else {
            ui.horizontal(|ui| {
                if ui
                    .button("Home")
                    .on_hover_text("Go to the login directory")
                    .clicked()
                {
                    explorer.load_home(ssh);
                    activity = true;
                }
                let can_up = !explorer.current.is_empty() && explorer.current != "/";
                if ui.add_enabled(can_up, egui::Button::new("Up")).clicked() {
                    explorer.up(ssh);
                    activity = true;
                }
                if ui.button("Refresh").clicked() {
                    explorer.refresh(ssh);
                    activity = true;
                }
                if explorer.loading {
                    ui.spinner();
                }
            });

            ui.horizontal(|ui| {
                ui.label("Path:");
                let edit = ui.add(
                    egui::TextEdit::singleline(&mut explorer.path_edit)
                        .desired_width(ui.available_width())
                        .hint_text("Remote path"),
                );
                if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    let path = explorer.path_edit.clone();
                    explorer.navigate(ssh, path);
                    activity = true;
                }
            });

            ui.horizontal(|ui| {
                if ui.button("Go").clicked() {
                    let path = explorer.path_edit.clone();
                    explorer.navigate(ssh, path);
                    activity = true;
                }
                if explorer.loading {
                    ui.weak("Loading…");
                }
            });

            if let Some(err) = &explorer.error {
                ui.colored_label(ui.visuals().error_fg_color, err);
            }

            ui.separator();

            let mut open: Option<String> = None;
            // Leave room for the footer; without this the scroll area eats the
            // whole panel and the selected-path line is clipped.
            let list_height = (ui.available_height() - 46.0).max(80.0);
            egui::ScrollArea::vertical()
                .id_salt(("sftp_entries", index))
                .auto_shrink([false, false])
                .max_height(list_height)
                .show(ui, |ui| {
                    if explorer.entries.is_empty() && !explorer.loading && explorer.error.is_none()
                    {
                        ui.weak("This directory is empty.");
                    }

                    for (i, entry) in explorer.entries.iter().enumerate() {
                        let icon = if entry.is_dir {
                            "📁"
                        } else if entry.is_symlink {
                            "🔗"
                        } else {
                            "📄"
                        };
                        let path = join_path(&explorer.listed_path, &entry.name);
                        let response = ui.selectable_label(
                            explorer.selected == Some(i),
                            format!("{icon} {}", entry.name),
                        );
                        if response.clicked() {
                            explorer.selected = Some(i);
                            activity = true;
                            if entry.is_dir {
                                open = Some(path.clone());
                            }
                        }

                        let mut details = path;
                        if entry.is_dir {
                            details.push_str("\ndirectory");
                        } else {
                            details.push_str(&format!("\n{}", format_size(entry.size)));
                        }
                        if entry.is_symlink {
                            details.push_str(" · symlink");
                        }
                        if let Some(modified) = entry.modified {
                            details.push_str(&format!("\nmodified {}", format_age(modified)));
                        }
                        response.on_hover_text(details);
                    }
                });

            // Navigate after the row loop: `navigate` needs the explorer mutably.
            if let Some(path) = open {
                explorer.navigate(ssh, path);
            }

            ui.separator();
            match explorer.selected_entry() {
                Some(entry) => {
                    ui.add(
                        egui::Label::new(egui::RichText::new(&entry.path).monospace()).truncate(),
                    );
                    let mut details = if entry.is_dir {
                        "directory".to_string()
                    } else {
                        format_size(entry.size)
                    };
                    if entry.is_symlink {
                        details.push_str(" · symlink");
                    }
                    if let Some(modified) = entry.modified {
                        details.push_str(&format!(" · {}", format_age(modified)));
                    }
                    ui.weak(details);
                }
                None => {
                    ui.weak(format!("{} entries", explorer.entries.len()));
                }
            }
        }

        if hide {
            self.show_sftp = false;
        }
        activity || hide
    }

    /// Render one session.
    ///
    /// Returns `(disconnect_clicked, had_activity)` — the caller closes the tab
    /// after the borrow ends, and uses `had_activity` to keep polling for
    /// output on the following frames.
    fn terminal_ui(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        index: usize,
    ) -> (bool, bool) {
        let session = &mut self.sessions[index];
        let mut activity = false;

        let response = session.terminal.ui(ui);

        // Right-click menu actions (handoff F14). Copy/Paste need the egui
        // `Context`; Select All / Clear act on the terminal directly.
        if let Some(action) = session.terminal.take_action() {
            match action {
                TermAction::Copy => {
                    if let Some(text) = session.terminal.selected_text() {
                        ctx.copy_text(text);
                    }
                }
                TermAction::Paste => {
                    // Ask the platform to read the clipboard; eframe pushes an
                    // `Event::Paste` next frame. Clear `focus_requested` so the
                    // terminal re-takes focus and that event is processed.
                    session.focus_requested = false;
                    ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaste);
                }
                TermAction::SelectAll => session.terminal.select_all(),
                TermAction::ClearSelection => session.terminal.clear_selection(),
            }
            activity = true;
        }

        // Terminal-generated input (xterm mouse reports, handoff F17).
        let mouse_input = session.terminal.take_input();
        if !mouse_input.is_empty() {
            session.ssh.write(mouse_input);
            activity = true;
        }

        // Focus: on first appearance, and on click. Never unconditionally per
        // frame — that fights the lock filter (handoff §9.4).
        if !session.focus_requested {
            response.request_focus();
            session.focus_requested = true;
        }
        if response.clicked() {
            response.request_focus();
        }

        if response.has_focus() {
            // Re-apply the lock filter every frame: egui clears it on focus change.
            ctx.memory_mut(|m| {
                m.set_focus_lock_filter(
                    response.id,
                    egui::EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: true,
                    },
                );
            });

            let events = ui.input(|i| i.events.clone());
            let mods = ui.input(|i| i.modifiers);
            // DECCKM (`?1h`): ncurses programs expect SS3 cursor keys.
            let app_cursor = session.terminal.app_cursor_mode();
            for event in &events {
                // Shift+PgUp/PgDn/Home/End scroll the local scrollback instead
                // of being encoded and sent to the remote program.
                if session.terminal.handle_scroll_event(event) {
                    activity = true; // viewport changed: repaint next frame
                    continue;
                }
                // Ctrl+Shift+C copies the mouse selection to the system
                // clipboard. egui reports it as `Event::Copy`, the same event
                // as Ctrl+C, so the current modifiers disambiguate the two.
                if input::is_copy_gesture(event, mods) {
                    if let Some(text) = session.terminal.selected_text() {
                        ctx.copy_text(text);
                    }
                    activity = true;
                    continue;
                }
                if let Some(bytes) = input::event_to_bytes(event, mods, app_cursor) {
                    session.ssh.write(bytes);
                    // Typing clears the selection and returns to the live view:
                    // input while scrolled back would otherwise go somewhere the
                    // user cannot see.
                    session.terminal.clear_selection();
                    session.terminal.scroll_to_bottom();
                    activity = true;
                }
            }
        }

        // Forward grid changes to the remote PTY. The terminal already tracks
        // whether the size changed — we just consume the notification.
        if let Some((cols, rows)) = session.terminal.take_resize() {
            session.ssh.resize(cols as u16, rows as u16);
        }

        ui.add_space(4.0);
        let mut disconnect_clicked = false;
        ui.horizontal(|ui| {
            if ui.button("Disconnect").clicked() {
                disconnect_clicked = true;
            }
        });
        (disconnect_clicked, activity)
    }
}

impl eframe::App for RustTermApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_connecting();

        // Read background tabs too: SSH flow control must keep moving even when
        // another session is visible. New output is what wakes egui, so this
        // happens before the UI is painted.
        let mut activity = self.pump_sessions(ctx);

        egui::TopBottomPanel::top("tab_bar").show(ctx, |ui| {
            self.tabs_ui(ui);
        });

        // The remote file panel belongs to the active session and sits left of
        // the terminal. It must be declared before the central panel so egui can
        // reserve its width.
        if self.show_sftp {
            if let Some(idx) = self.active.checked_sub(1) {
                if idx < self.sessions.len() {
                    egui::SidePanel::left("sftp_panel")
                        .resizable(true)
                        .default_width(300.0)
                        .min_width(220.0)
                        .show(ctx, |ui| {
                            activity |= self.sftp_ui(ui, idx);
                        });
                }
            }
        }

        let mut disconnect_requested = false;
        egui::CentralPanel::default().show(ctx, |ui| {
            if self.active == NEW_SESSION_TAB {
                self.new_session_ui(ui);
            } else if let Some(idx) = self.active.checked_sub(1) {
                if idx < self.sessions.len() {
                    let (disc, act) = self.terminal_ui(ui, ctx, idx);
                    disconnect_requested = disc;
                    activity |= act;
                } else {
                    self.active = NEW_SESSION_TAB;
                }
            }
        });

        self.host_key_prompt_ui(ctx);

        if disconnect_requested {
            if let Some(idx) = self.active.checked_sub(1) {
                self.close_session(idx);
            }
        }

        if activity {
            // Local terminal actions (selection, mouse reports) also need an
            // immediate repaint. Remote output already calls `request_repaint`
            // from the SSH thread.
            ctx.request_repaint();
        }

        // Only connection setup needs a polling timer. Established sessions
        // wake the UI on demand and otherwise let egui sleep at zero CPU.
        if self.connecting.is_some() {
            ctx.request_repaint_after(Duration::from_millis(CONNECT_POLL_MS));
        }
    }
}
