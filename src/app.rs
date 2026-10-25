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
use crate::settings::Settings;
use crate::sftp::{
    command_available, editor_program, format_age, format_owner, format_size, format_timestamp,
    EditorChoice, SftpDialogKind, SftpExplorer, StatusKind,
};
use crate::ssh::{HostKeyPrompt, RemoteEntry, SshConfig, SshSession, Wake};
use crate::terminal::{TermAction, TerminalView};
use crate::theme::{self, Theme, ThemeId};

use egui_extras::{Column, TableBuilder};

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

/// Small label above a form field.
fn field_label(ui: &mut egui::Ui, theme: &Theme, text: &str) {
    ui.label(egui::RichText::new(text).small().color(theme.palette.muted));
}

/// Muted, compact table header label.
fn table_header(ui: &mut egui::Ui, theme: &Theme, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .small()
            .strong()
            .color(theme.palette.muted),
    );
}

/// Latest SFTP message, operation state and message history.
fn sftp_status_bar(ui: &mut egui::Ui, theme: &Theme, explorer: &mut SftpExplorer) {
    let busy = explorer.busy_label().map(str::to_string);
    let current = explorer.status.current().cloned();

    ui.horizontal(|ui| {
        if let Some(label) = busy {
            ui.spinner();
            ui.add(
                egui::Label::new(egui::RichText::new(label).small().color(theme.palette.text))
                    .truncate(),
            );
        } else {
            let (color, icon, text) = match &current {
                Some(entry) => (
                    status_color(theme, entry.kind),
                    status_icon(entry.kind),
                    entry.text.as_str(),
                ),
                None => (theme.palette.muted, "•", "Ready"),
            };
            ui.label(egui::RichText::new(icon).color(color).size(9.0));
            ui.add(egui::Label::new(egui::RichText::new(text).small().color(color)).truncate())
                .on_hover_text(text);
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let response = ui.menu_button(egui::RichText::new("⋯").size(15.0), |ui| {
                ui.label(egui::RichText::new("Session messages").strong());
                ui.add_space(2.0);
                for entry in explorer.status.history() {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(&entry.at)
                                .small()
                                .color(theme.palette.muted),
                        );
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&entry.text)
                                    .small()
                                    .color(status_color(theme, entry.kind)),
                            )
                            .wrap(),
                        );
                    });
                }
                ui.separator();
                if ui.button("Clear messages").clicked() {
                    explorer.status.clear();
                    ui.close_menu();
                }
            });
            response.response.on_hover_text("Message history");
        });
    });
}

fn status_color(theme: &Theme, kind: StatusKind) -> egui::Color32 {
    match kind {
        StatusKind::Info => theme.palette.muted,
        StatusKind::Success => theme.palette.success,
        StatusKind::Error => theme.palette.danger,
    }
}

fn status_icon(kind: StatusKind) -> &'static str {
    match kind {
        StatusKind::Info => "•",
        StatusKind::Success => "✓",
        StatusKind::Error => "×",
    }
}

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
    /// Selected application theme.
    theme: ThemeId,
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
        let settings = Settings::load_default();

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
            theme: settings.theme,
        }
    }
}

impl RustTermApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let app = Self::default();
        theme::apply(&cc.egui_ctx, app.theme.theme());
        app
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

        ui.add_space(2.0);
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

            let scroll_width = (ui.available_width() - 40.0).max(120.0);
            ui.allocate_ui_with_layout(
                egui::vec2(scroll_width, ui.available_height()),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    egui::ScrollArea::horizontal()
                        .id_salt("tab_bar_scroll")
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                // New Session — always first, never closable.
                                if ui
                                    .selectable_label(
                                        self.active == NEW_SESSION_TAB,
                                        "＋ New Session",
                                    )
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
                },
            );

            ui.add_space(4.0);
            self.theme_menu_ui(ui);
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

    /// Small theme picker shown at the right edge of the tab bar.
    fn theme_menu_ui(&mut self, ui: &mut egui::Ui) {
        let mut chosen: Option<ThemeId> = None;
        ui.menu_button(egui::RichText::new("◐").size(16.0), |ui| {
            ui.label(egui::RichText::new("Theme").strong());
            ui.add_space(2.0);
            for id in ThemeId::ALL {
                if ui.selectable_label(self.theme == id, id.name()).clicked() {
                    chosen = Some(id);
                }
            }
            ui.separator();
            theme::muted(
                ui,
                self.theme.theme(),
                "More themes can be added in src/theme.rs.",
            );
        })
        .response
        .on_hover_text("Choose a theme");

        if let Some(id) = chosen {
            self.theme = id;
            theme::apply(ui.ctx(), id.theme());
            let settings = Settings { theme: id };
            if let Err(err) = settings.save_default() {
                log::warn!("could not save theme choice: {err}");
            }
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

    /// Name field + save button, drawn inside the Profile card.
    fn save_profile_ui(&mut self, ui: &mut egui::Ui, theme: &Theme) {
        let mut save_clicked = false;
        let name_hint = if self.form.save_password {
            "required to save password"
        } else {
            "blank = user@host"
        };

        field_label(ui, theme, "Profile name");
        ui.horizontal(|ui| {
            let button_width = 112.0_f32;
            let name_width =
                (ui.available_width() - button_width - ui.spacing().item_spacing.x).max(120.0);
            ui.add_sized(
                [name_width, theme.metrics.control_height],
                egui::TextEdit::singleline(&mut self.profile_name).hint_text(name_hint),
            );
            let button = egui::Button::new("Save profile")
                .min_size(egui::vec2(button_width, theme.metrics.control_height));
            if ui.add(button).clicked() {
                save_clicked = true;
            }
        })
        .response
        .on_hover_text(
            "Saves host, port and user; if the keyring box is checked, \
             the password too (requires a profile name).",
        );

        if save_clicked {
            self.save_profile();
            // The name field was drawn before a blank name was replaced with
            // `user@host`, so redraw to show the stored name.
            ui.ctx().request_repaint();
        }
    }

    /// Saved-profile list plus the delete control.
    fn saved_sessions_ui(&mut self, ui: &mut egui::Ui, theme: &Theme) {
        let profiles = self.store.profiles().to_vec();
        let mut delete_clicked = false;
        let mut load: Option<usize> = None;

        theme::section_label(ui, theme, "Saved sessions");
        if profiles.is_empty() {
            theme::muted(
                ui,
                theme,
                "No saved sessions yet. Fill in the form above and press Save profile.",
            );
            return;
        }

        theme::muted(ui, theme, format!("{} saved", profiles.len()));
        ui.add_space(4.0);
        egui::ScrollArea::vertical()
            .id_salt("saved_sessions")
            .max_height(170.0)
            .show(ui, |ui| {
                for (index, profile) in profiles.iter().enumerate() {
                    let selected = self.selected_profile == Some(index);
                    let text = format!(
                        "{}   ·   {}@{}:{}",
                        profile.name, profile.user, profile.host, profile.port
                    );
                    let response = ui.selectable_label(selected, text);
                    if response.clicked() {
                        load = Some(index);
                    }
                    // No double-click shortcut: passwords are not stored, so a
                    // one-click connect would usually fail authentication.
                    response.on_hover_text("Click to load into the form");
                }
            });

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let can_delete = self.selected_profile.is_some();
            if ui
                .add_enabled(can_delete, egui::Button::new("Delete selected"))
                .on_hover_text("Removes the profile from sessions.toml")
                .clicked()
            {
                delete_clicked = true;
            }
            ui.add(
                egui::Label::new(
                    egui::RichText::new(format!("File: {}", self.store.path().display()))
                        .small()
                        .color(theme.palette.muted),
                )
                .truncate(),
            );
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
        let theme = self.theme.theme();
        egui::ScrollArea::vertical()
            .id_salt("new_session_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(24.0);
                let width = 560.0_f32.min((ui.available_width() - 24.0).max(280.0));
                let leading = ((ui.available_width() - width) * 0.5).max(0.0);

                ui.horizontal(|ui| {
                    ui.add_space(leading);
                    ui.allocate_ui_with_layout(
                        egui::vec2(width, 0.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.set_width(width);
                            ui.label(
                                egui::RichText::new("New SSH session")
                                    .size(24.0)
                                    .strong()
                                    .color(theme.palette.text),
                            );
                            theme::muted(ui, theme, "Connect to a remote machine over SSH.");
                            ui.add_space(16.0);

                            theme::card_frame(theme).show(ui, |ui| {
                                ui.set_width(width - theme.metrics.card_padding * 2.0);
                                theme::section_label(ui, theme, "Connection");
                                ui.add_space(8.0);

                                ui.horizontal(|ui| {
                                    let port_width = 82.0_f32;
                                    let host_width = (ui.available_width()
                                        - port_width
                                        - ui.spacing().item_spacing.x)
                                        .max(140.0);
                                    ui.vertical(|ui| {
                                        field_label(ui, theme, "Host");
                                        ui.add_sized(
                                            [host_width, theme.metrics.control_height],
                                            egui::TextEdit::singleline(&mut self.form.host)
                                                .hint_text("example.com"),
                                        );
                                    });
                                    ui.vertical(|ui| {
                                        field_label(ui, theme, "Port");
                                        ui.add_sized(
                                            [port_width, theme.metrics.control_height],
                                            egui::TextEdit::singleline(&mut self.form.port)
                                                .hint_text("22"),
                                        );
                                    });
                                });

                                ui.add_space(8.0);
                                field_label(ui, theme, "User");
                                ui.add_sized(
                                    [ui.available_width(), theme.metrics.control_height],
                                    egui::TextEdit::singleline(&mut self.form.user)
                                        .hint_text("username"),
                                );

                                ui.add_space(8.0);
                                field_label(ui, theme, "Password");
                                ui.add_sized(
                                    [ui.available_width(), theme.metrics.control_height],
                                    egui::TextEdit::singleline(&mut self.form.password)
                                        .password(true)
                                        .hint_text("Password"),
                                )
                                .on_hover_text(
                                    "Used when key/agent authentication is disabled or fails.",
                                );

                                ui.add_space(8.0);
                                egui::CollapsingHeader::new(
                                    egui::RichText::new("Authentication").strong(),
                                )
                                .id_salt("auth_options")
                                .default_open(self.form.use_key || self.form.use_agent)
                                .show(ui, |ui| {
                                    ui.add_space(4.0);
                                    ui.checkbox(&mut self.form.use_key, "Use a private key")
                                        .on_hover_text(
                                            "Tried before the password. Leave the path blank to \
                                             use ~/.ssh/id_ed25519, id_ecdsa, id_rsa or id_dsa.",
                                        );
                                    ui.add_enabled(
                                        self.form.use_key,
                                        egui::TextEdit::singleline(&mut self.form.key_path)
                                            .hint_text("Key path (blank = auto-detect)")
                                            .desired_width(ui.available_width()),
                                    );
                                    ui.add_space(2.0);
                                    ui.checkbox(
                                        &mut self.form.use_agent,
                                        "Use SSH agent identities",
                                    )
                                    .on_hover_text(
                                        "Tried after a key file and before the password. Best for \
                                         passphrase-protected keys loaded with ssh-add.",
                                    );

                                    // Hidden entirely in lean builds
                                    // (`--no-default-features`).
                                    if KEYRING_ENABLED {
                                        ui.checkbox(
                                            &mut self.form.save_password,
                                            "Save password in the system keyring",
                                        )
                                        .on_hover_text(
                                            "Optional, per profile. Requires an explicit profile \
                                             name. Stored by the OS keyring, never in \
                                             sessions.toml.",
                                        );
                                    }
                                });

                                ui.add_space(12.0);
                                let connecting = self.connecting.is_some();
                                let connecting_label = self.connecting.as_ref().map(|state| {
                                    if state.prompt.is_some() {
                                        "Waiting for host key confirmation…".to_string()
                                    } else {
                                        format!("Connecting to {}…", state.label)
                                    }
                                });

                                ui.horizontal(|ui| {
                                    let button = egui::Button::new(
                                        egui::RichText::new("Connect")
                                            .strong()
                                            .color(egui::Color32::WHITE),
                                    )
                                    .fill(theme.palette.accent)
                                    .stroke(egui::Stroke::NONE)
                                    .min_size(egui::vec2(120.0, 32.0))
                                    .rounding(egui::Rounding::same(theme.metrics.radius));
                                    if ui.add_enabled(!connecting, button).clicked() {
                                        self.start_connect(ui.ctx());
                                    }
                                    if let Some(label) = connecting_label {
                                        ui.spinner();
                                        theme::muted(ui, theme, label);
                                    }
                                });

                                if !self.status.text.is_empty() {
                                    ui.add_space(10.0);
                                    let color = if self.status.is_error {
                                        theme.palette.danger
                                    } else {
                                        theme.palette.success
                                    };
                                    ui.horizontal(|ui| {
                                        ui.label(egui::RichText::new("●").color(color).size(10.0));
                                        ui.add(
                                            egui::Label::new(
                                                egui::RichText::new(&self.status.text).color(color),
                                            )
                                            .wrap(),
                                        );
                                    });
                                }
                            });

                            ui.add_space(14.0);
                            theme::card_frame(theme).show(ui, |ui| {
                                ui.set_width(width - theme.metrics.card_padding * 2.0);
                                theme::section_label(ui, theme, "Profile");
                                theme::muted(
                                    ui,
                                    theme,
                                    "Save this connection for one-click reuse.",
                                );
                                ui.add_space(6.0);
                                self.save_profile_ui(ui, theme);
                            });

                            ui.add_space(14.0);
                            theme::card_frame(theme).show(ui, |ui| {
                                ui.set_width(width - theme.metrics.card_padding * 2.0);
                                self.saved_sessions_ui(ui, theme);
                            });

                            ui.add_space(24.0);
                        },
                    );
                });
            });
    }

    /// Render the remote file browser for one session.
    ///
    /// Returns `true` when the panel started a request, changed selection or was
    /// hidden, so the caller can request another frame.
    /// Render the remote file browser for one session.
    ///
    /// Returns `true` when the panel started a request, changed selection or was
    /// hidden, so the caller can request another frame.
    fn sftp_ui(&mut self, ui: &mut egui::Ui, index: usize) -> bool {
        let theme = self.theme.theme();
        let Some(session) = self.sessions.get_mut(index) else {
            return false;
        };
        // Split the borrow: the explorer mutates while the session only answers
        // requests.
        let (ssh, explorer) = (&session.ssh, &mut session.sftp);
        let mut activity = false;
        let mut hide = false;

        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("Remote files")
                    .strong()
                    .size(15.0)
                    .color(theme.palette.text),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::icon_button(ui, theme, "×", "Hide the file panel", true).clicked() {
                    hide = true;
                }
            });
        });
        ui.add_space(2.0);

        let busy = explorer.is_busy();
        let selected = explorer.selected_entry().cloned();
        let can_up = !explorer.current.is_empty() && explorer.current != "/";
        let can_copy = selected.is_some() && !busy;
        let can_paste = explorer.clipboard.is_some() && !busy;

        ui.horizontal_wrapped(|ui| {
            if theme::icon_button(ui, theme, "⌂", "Go to the login directory", !busy).clicked() {
                explorer.load_home(ssh);
                activity = true;
            }
            if theme::icon_button(ui, theme, "↑", "Up one level", can_up && !busy).clicked() {
                explorer.up(ssh);
                activity = true;
            }
            if theme::icon_button(ui, theme, "↻", "Refresh this directory", !busy).clicked() {
                explorer.refresh(ssh);
                activity = true;
            }
            ui.separator();
            if theme::icon_button(ui, theme, "＋", "Create a folder here", !busy).clicked() {
                explorer.begin_new_folder();
                activity = true;
            }
            if theme::icon_button(ui, theme, "⧉", "Copy the selected entry", can_copy).clicked() {
                if let Some(entry) = &selected {
                    explorer.copy_entry(entry);
                }
                activity = true;
            }
            if theme::icon_button(ui, theme, "📋", "Paste the copied entry here", can_paste)
                .clicked()
            {
                explorer.paste(ssh);
                activity = true;
            }
            ui.separator();
            if theme::icon_toggle(
                ui,
                theme,
                "◉",
                "Show or hide dotfiles",
                explorer.show_hidden,
            )
            .clicked()
            {
                explorer.show_hidden = !explorer.show_hidden;
                if !explorer.show_hidden
                    && explorer
                        .selected_entry()
                        .is_some_and(|entry| entry.name.starts_with('.'))
                {
                    explorer.selected = None;
                }
                activity = true;
            }
            if theme::icon_button(
                ui,
                theme,
                "⟳",
                "Reconnect SFTP without dropping the terminal",
                !busy,
            )
            .clicked()
            {
                explorer.reconnect(ssh);
                activity = true;
            }
            if explorer.loading {
                ui.spinner();
            }
        });

        ui.horizontal(|ui| {
            let go_width = 28.0_f32;
            let width = (ui.available_width() - go_width - ui.spacing().item_spacing.x).max(80.0);
            let edit = ui.add_sized(
                [width, theme.metrics.control_height],
                egui::TextEdit::singleline(&mut explorer.path_edit).hint_text("Remote path"),
            );
            if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                let path = explorer.path_edit.clone();
                explorer.navigate(ssh, path);
                activity = true;
            }
            if theme::icon_button(ui, theme, "→", "Open this path", true).clicked() {
                let path = explorer.path_edit.clone();
                explorer.navigate(ssh, path);
                activity = true;
            }
        });

        // Fixed message bar at the bottom. Declared before the table so the
        // table gets the remaining height.
        egui::TopBottomPanel::bottom(egui::Id::new(("sftp_status", index)))
            .exact_height(30.0)
            .frame(
                egui::Frame::none()
                    .fill(theme.palette.panel)
                    .stroke(egui::Stroke::new(1.0_f32, theme.palette.border))
                    .inner_margin(egui::Margin::symmetric(8.0, 4.0)),
            )
            .show_inside(ui, |ui| {
                sftp_status_bar(ui, theme, explorer);
            });

        if let Some(reason) = explorer.unavailable.clone() {
            ui.add_space(16.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new("SFTP is unavailable")
                        .strong()
                        .color(theme.palette.warning),
                );
                ui.add_space(4.0);
                ui.add(
                    egui::Label::new(egui::RichText::new(reason).color(theme.palette.muted)).wrap(),
                );
                ui.add_space(8.0);
                if ui.button("Reconnect SFTP").clicked() {
                    explorer.reconnect(ssh);
                    activity = true;
                }
            });
            if hide {
                self.show_sftp = false;
            }
            return activity || hide;
        }

        let visible_indices: Vec<usize> = explorer
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| explorer.show_hidden || !entry.name.starts_with('.'))
            .map(|(index, _)| index)
            .collect();
        let has_clipboard = explorer.clipboard.is_some();
        let empty_message = if explorer.loading {
            "Loading…"
        } else if explorer.entries.is_empty() {
            "This directory is empty."
        } else if visible_indices.is_empty() {
            "Only hidden files here — enable Hidden to show them."
        } else if explorer.status.is_error() {
            "Nothing to show. See the message below."
        } else {
            "No entries."
        };

        // `RowAction` is applied after the row loop: the menu closures cannot
        // borrow the explorer mutably while the table is iterated.
        enum RowAction {
            Navigate(String),
            OpenWith(RemoteEntry),
            Rename(RemoteEntry),
            Delete(RemoteEntry),
            CopyPath(RemoteEntry),
            Paste(String),
        }
        let mut row_action: Option<RowAction> = None;
        let mut select_row: Option<usize> = None;

        let max_height = ui.available_height().max(80.0);
        let row_height = theme.metrics.row_height;
        TableBuilder::new(ui)
            .id_salt(("sftp_table", index))
            .striped(true)
            .resizable(true)
            .sense(egui::Sense::click())
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::remainder().at_least(120.0))
            .column(Column::initial(80.0).at_least(52.0).resizable(true))
            .column(Column::initial(112.0).at_least(88.0).resizable(true))
            .column(Column::initial(64.0).at_least(48.0).resizable(true))
            .min_scrolled_height(0.0)
            .max_scroll_height(max_height)
            .header(24.0, |mut header| {
                header.col(|ui| {
                    table_header(ui, theme, "Name");
                });
                header.col(|ui| {
                    table_header(ui, theme, "Owner");
                });
                header.col(|ui| {
                    table_header(ui, theme, "Modified");
                });
                header.col(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        table_header(ui, theme, "Size");
                    });
                });
            })
            .body(|mut body| {
                if visible_indices.is_empty() {
                    body.row(row_height, |mut row| {
                        row.col(|ui| {
                            theme::muted(ui, theme, empty_message);
                        });
                        row.col(|_| {});
                        row.col(|_| {});
                        row.col(|_| {});
                    });
                } else {
                    body.rows(row_height, visible_indices.len(), |mut row| {
                        let visible_index = row.index();
                        let entry_index = visible_indices[visible_index];
                        let entry = &explorer.entries[entry_index];
                        row.set_selected(explorer.selected == Some(entry_index));
                        let icon = if entry.is_dir {
                            "📁"
                        } else if entry.is_symlink {
                            "🔗"
                        } else {
                            "📄"
                        };

                        row.col(|ui| {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(icon).size(13.0));
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(&entry.name).color(theme.palette.text),
                                    )
                                    .truncate(),
                                )
                                .on_hover_text(&entry.name);
                            });
                        });
                        row.col(|ui| {
                            let owner = format_owner(entry);
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(owner)
                                        .small()
                                        .color(theme.palette.muted),
                                )
                                .truncate(),
                            )
                            .on_hover_text(owner);
                        });
                        row.col(|ui| {
                            let modified = format_timestamp(entry.modified);
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(&modified)
                                        .small()
                                        .color(theme.palette.muted),
                                )
                                .truncate(),
                            )
                            .on_hover_text(format!(
                                "{modified}\n{}",
                                entry
                                    .modified
                                    .map(format_age)
                                    .unwrap_or_else(|| "unknown".to_string())
                            ));
                        });
                        row.col(|ui| {
                            let size = if entry.is_dir {
                                "—".to_string()
                            } else {
                                format_size(entry.size)
                            };
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(
                                        egui::RichText::new(size)
                                            .small()
                                            .color(theme.palette.muted),
                                    );
                                },
                            );
                        });

                        let mut details = entry.path.clone();
                        if entry.is_dir {
                            details.push_str("\ndirectory");
                        } else {
                            details.push_str(&format!("\n{}", format_size(entry.size)));
                        }
                        if entry.is_symlink {
                            details.push_str(" · symlink");
                        }
                        details.push_str(&format!(
                            "\nowner {} · modified {}",
                            format_owner(entry),
                            format_timestamp(entry.modified)
                        ));
                        if let Some(modified) = entry.modified {
                            details.push_str(&format!("\n{}", format_age(modified)));
                        }

                        let response = row.response();
                        response.clone().on_hover_text(details);
                        if response.clicked() {
                            select_row = Some(entry_index);
                        }
                        if response.double_clicked() {
                            if entry.is_dir {
                                row_action = Some(RowAction::Navigate(entry.path.clone()));
                            } else {
                                row_action = Some(RowAction::OpenWith(entry.clone()));
                            }
                        }
                        if response.secondary_clicked() {
                            select_row = Some(entry_index);
                        }
                        response.context_menu(|ui| {
                            ui.label(egui::RichText::new(&entry.name).strong());
                            ui.separator();
                            if entry.is_dir {
                                if ui.button("Open").clicked() {
                                    row_action = Some(RowAction::Navigate(entry.path.clone()));
                                    ui.close_menu();
                                }
                                if has_clipboard && ui.button("Paste into this folder").clicked() {
                                    row_action = Some(RowAction::Paste(entry.path.clone()));
                                    ui.close_menu();
                                }
                            } else if ui.button("Open with…").clicked() {
                                row_action = Some(RowAction::OpenWith(entry.clone()));
                                ui.close_menu();
                            }
                            if ui.button("Rename…").clicked() {
                                row_action = Some(RowAction::Rename(entry.clone()));
                                ui.close_menu();
                            }
                            if ui.button("Copy path").clicked() {
                                row_action = Some(RowAction::CopyPath(entry.clone()));
                                ui.close_menu();
                            }
                            ui.separator();
                            if ui
                                .add(egui::Button::new(
                                    egui::RichText::new("Delete…").color(theme.palette.danger),
                                ))
                                .clicked()
                            {
                                row_action = Some(RowAction::Delete(entry.clone()));
                                ui.close_menu();
                            }
                        });
                    });
                }
            });

        if let Some(index) = select_row {
            explorer.selected = Some(index);
            activity = true;
        }
        if let Some(action) = row_action {
            match action {
                RowAction::Navigate(path) => explorer.navigate(ssh, path),
                RowAction::OpenWith(entry) => {
                    explorer.begin_open_with(&entry);
                }
                RowAction::Rename(entry) => {
                    explorer.begin_rename(&entry);
                }
                RowAction::Delete(entry) => {
                    explorer.begin_delete(&entry);
                }
                RowAction::CopyPath(entry) => {
                    ui.ctx().copy_text(entry.path.clone());
                    explorer
                        .status
                        .info(format!("Copied path for {}", entry.name));
                }
                RowAction::Paste(path) => {
                    explorer.paste_into(ssh, path);
                }
            }
            activity = true;
        }

        if hide {
            self.show_sftp = false;
        }
        activity || hide
    }

    /// Render rename / new-folder / delete dialogs for the active session.
    fn sftp_dialogs_ui(&mut self, ctx: &egui::Context, index: usize) -> bool {
        let theme = self.theme.theme();
        let Some(session) = self.sessions.get_mut(index) else {
            return false;
        };
        let (ssh, explorer) = (&session.ssh, &mut session.sftp);
        let Some(dialog) = explorer.dialog.as_mut() else {
            return false;
        };

        let mut submit = false;
        let mut cancel = false;
        let kind = dialog.kind.clone();

        let (title, subtitle) = match &kind {
            SftpDialogKind::Rename { .. } => ("Rename", "Give the entry a new name."),
            SftpDialogKind::NewFolder { .. } => {
                ("New folder", "Create a folder in this directory.")
            }
            SftpDialogKind::Delete { .. } => ("Delete", "Please confirm this action."),
        };

        egui::Window::new(title)
            .id(egui::Id::new(("sftp_dialog", index)))
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .frame(theme::modal_frame(theme))
            .show(ctx, |ui| {
                ui.set_width(400.0);
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(title)
                            .size(18.0)
                            .strong()
                            .color(theme.palette.text),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("×").on_hover_text("Cancel").clicked() {
                            cancel = true;
                        }
                    });
                });
                theme::muted(ui, theme, subtitle);
                ui.add_space(14.0);

                match &kind {
                    SftpDialogKind::Rename { .. } => {
                        field_label(ui, theme, "New name");
                        let edit = ui.add_sized(
                            [ui.available_width(), theme.metrics.control_height],
                            egui::TextEdit::singleline(&mut dialog.name)
                                .hint_text("File or folder name"),
                        );
                        if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            submit = true;
                        }
                        if let Some(err) = &dialog.error {
                            ui.add_space(4.0);
                            ui.colored_label(theme.palette.danger, err);
                        }
                    }
                    SftpDialogKind::NewFolder { parent } => {
                        field_label(ui, theme, "Folder name");
                        let edit = ui.add_sized(
                            [ui.available_width(), theme.metrics.control_height],
                            egui::TextEdit::singleline(&mut dialog.name).hint_text("New folder"),
                        );
                        if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            submit = true;
                        }
                        ui.add_space(4.0);
                        theme::muted(ui, theme, format!("Will be created in {parent}"));
                        if let Some(err) = &dialog.error {
                            ui.add_space(4.0);
                            ui.colored_label(theme.palette.danger, err);
                        }
                    }
                    SftpDialogKind::Delete { path, name, is_dir } => {
                        ui.label(
                            egui::RichText::new(format!("Delete “{name}”?"))
                                .size(16.0)
                                .strong()
                                .color(theme.palette.text),
                        );
                        ui.add_space(8.0);
                        theme::callout_frame(theme, theme.palette.danger_soft).show(ui, |ui| {
                            let warning = if *is_dir {
                                "This folder and everything inside it will be permanently deleted. This cannot be undone."
                            } else {
                                "This file will be permanently deleted. This cannot be undone."
                            };
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(warning)
                                        .color(theme.palette.danger),
                                )
                                .wrap(),
                            );
                        });
                        ui.add_space(8.0);
                        theme::muted(ui, theme, "Remote path");
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(path)
                                    .monospace()
                                    .color(theme.palette.muted),
                            )
                            .truncate(),
                        )
                        .on_hover_text(path);
                        if let Some(err) = &dialog.error {
                            ui.add_space(4.0);
                            ui.colored_label(theme.palette.danger, err);
                        }
                    }
                }

                ui.add_space(18.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let primary = match &kind {
                        SftpDialogKind::Rename { .. } => "Rename",
                        SftpDialogKind::NewFolder { .. } => "Create",
                        SftpDialogKind::Delete { .. } => "Delete",
                    };
                    let primary_button = if matches!(kind, SftpDialogKind::Delete { .. }) {
                        egui::Button::new(
                            egui::RichText::new(primary)
                                .strong()
                                .color(egui::Color32::WHITE),
                        )
                        .fill(theme.palette.danger)
                        .stroke(egui::Stroke::NONE)
                    } else {
                        egui::Button::new(
                            egui::RichText::new(primary)
                                .strong()
                                .color(egui::Color32::WHITE),
                        )
                        .fill(theme.palette.accent)
                        .stroke(egui::Stroke::NONE)
                    };
                    if ui
                        .add_sized(
                            [92.0, theme.metrics.control_height],
                            primary_button,
                        )
                        .clicked()
                    {
                        submit = true;
                    }
                    if ui
                        .add_sized(
                            [92.0, theme.metrics.control_height],
                            egui::Button::new("Cancel"),
                        )
                        .clicked()
                    {
                        cancel = true;
                    }
                });
            });

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            cancel = true;
        }
        if cancel {
            return explorer.cancel_dialog();
        }
        if submit {
            return explorer.submit_dialog(ssh);
        }
        false
    }

    /// Render the "Open with…" editor picker for the active session.
    fn editor_picker_ui(&mut self, ctx: &egui::Context, index: usize) -> bool {
        let theme = self.theme.theme();
        let Some(session) = self.sessions.get_mut(index) else {
            return false;
        };
        let (ssh, explorer) = (&session.ssh, &mut session.sftp);
        let Some(picker) = explorer.editor_picker.as_mut() else {
            return false;
        };

        let mut submit = false;
        let mut cancel = false;

        egui::Window::new("Open with")
            .id(egui::Id::new(("sftp_editor_picker", index)))
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .frame(theme::modal_frame(theme))
            .show(ctx, |ui| {
                ui.set_width(420.0);
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("Open with…")
                            .size(18.0)
                            .strong()
                            .color(theme.palette.text),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("×").on_hover_text("Cancel").clicked() {
                            cancel = true;
                        }
                    });
                });
                ui.add_space(4.0);
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(&picker.entry.name)
                            .strong()
                            .color(theme.palette.text),
                    )
                    .truncate(),
                )
                .on_hover_text(&picker.entry.path);
                theme::muted(
                    ui,
                    theme,
                    format!("Download and open {}", picker.entry.path),
                );
                ui.add_space(10.0);

                for choice in EditorChoice::ALL {
                    let program = editor_program(choice);
                    let available = command_available(&program);
                    let label = if available {
                        format!("{}  ·  {program}", choice.label())
                    } else {
                        format!("{}  ·  not found", choice.label())
                    };
                    ui.add_enabled_ui(available, |ui| {
                        ui.radio_value(&mut picker.choice, choice, label);
                    });
                }

                ui.radio_value(&mut picker.choice, EditorChoice::Custom, "Custom command…");
                ui.add_enabled(
                    picker.choice == EditorChoice::Custom,
                    egui::TextEdit::singleline(&mut picker.custom)
                        .hint_text("e.g. kate, nvim, /usr/local/bin/editor")
                        .desired_width(ui.available_width()),
                );
                if let Some(err) = &picker.error {
                    ui.add_space(4.0);
                    ui.colored_label(theme.palette.danger, err);
                }

                ui.add_space(18.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let open = egui::Button::new(
                        egui::RichText::new("Open")
                            .strong()
                            .color(egui::Color32::WHITE),
                    )
                    .fill(theme.palette.accent)
                    .stroke(egui::Stroke::NONE);
                    if ui
                        .add_sized([92.0, theme.metrics.control_height], open)
                        .clicked()
                    {
                        submit = true;
                    }
                    if ui
                        .add_sized(
                            [92.0, theme.metrics.control_height],
                            egui::Button::new("Cancel"),
                        )
                        .clicked()
                    {
                        cancel = true;
                    }
                });
            });

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            cancel = true;
        }
        if cancel {
            return explorer.cancel_open_with();
        }
        if submit {
            return explorer.submit_open_with(ssh);
        }
        false
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
                        .default_width(420.0)
                        .min_width(340.0)
                        .show(ctx, |ui| {
                            activity |= self.sftp_ui(ui, idx);
                        });
                    activity |= self.sftp_dialogs_ui(ctx, idx);
                    activity |= self.editor_picker_ui(ctx, idx);
                }
            }
        }

        let mut disconnect_requested = false;
        let theme = self.theme.theme();
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(theme.palette.bg))
            .show(ctx, |ui| {
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
