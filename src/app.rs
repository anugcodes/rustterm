//! Application shell: tab bar, connect form, terminal views.
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
//! Repaint gating: egui is left to sleep when nothing has happened. A repaint
//! is only scheduled while a connect is pending, or in the few frames after
//! input/output (`poll_countdown`).

use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use eframe::egui;

use crate::input;
use crate::ssh::{HostKeyPrompt, SshConfig, SshSession};
use crate::terminal::{TermAction, TerminalView};

const NEW_SESSION_TAB: usize = 0;
const DEFAULT_COLS: usize = 80;
const DEFAULT_ROWS: usize = 24;

/// Delay between "activity" frames while polling for SSH output.
const REPAINT_DELAY_MS: u64 = 8;
/// Number of extra frames to keep polling after the last sign of activity,
/// so echo that lags behind a keystroke by a few frames is not dropped.
const POST_ACTIVITY_FRAMES: u8 = 3;

pub struct RustTermApp {
    sessions: Vec<ActiveSession>,
    /// 0 = New Session form; `i > 0` = `sessions[i - 1]`.
    active: usize,
    form: ConnectForm,
    connecting: Option<ConnectingState>,
    status: String,
    /// Frames left in the current "keep polling" window. Refreshed whenever
    /// output is read or input is sent; when it hits zero, egui sleeps.
    poll_countdown: u8,
}

struct ActiveSession {
    title: String,
    ssh: SshSession,
    terminal: TerminalView,
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
}

impl Default for ConnectForm {
    fn default() -> Self {
        Self {
            host: "localhost".to_string(),
            port: "22".to_string(),
            user: String::new(),
            password: String::new(),
        }
    }
}

impl Default for RustTermApp {
    fn default() -> Self {
        Self {
            sessions: Vec::new(),
            active: NEW_SESSION_TAB,
            form: ConnectForm::default(),
            connecting: None,
            status: String::new(),
            poll_countdown: 0,
        }
    }
}

impl RustTermApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        Self::default()
    }

    fn start_connect(&mut self) {
        if self.connecting.is_some() {
            return; // one connect attempt at a time
        }

        let host = self.form.host.trim().to_string();
        let port: u16 = match self.form.port.trim().parse() {
            Ok(p) => p,
            Err(_) => {
                self.status = "Port must be a number".to_string();
                return;
            }
        };
        let user = self.form.user.trim().to_string();
        if host.is_empty() || user.is_empty() {
            self.status = "Host and user are required".to_string();
            return;
        }

        let config = SshConfig {
            host,
            port,
            user,
            password: self.form.password.clone(),
        };
        let label = format!("{}@{}:{}", config.user, config.host, config.port);

        let (tx, rx) = std_mpsc::channel();
        let (prompt_tx, prompt_rx) = std_mpsc::channel();
        std::thread::spawn(move || {
            let result = SshSession::connect(config, Some(prompt_tx)).map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });

        self.connecting = Some(ConnectingState {
            rx,
            prompt_rx,
            prompt: None,
            label,
        });
        self.status.clear();
        self.active = NEW_SESSION_TAB;
        // Poll the connect result without waiting for the user to click.
        self.poll_countdown = POST_ACTIVITY_FRAMES;
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
                self.status = "Connect failed: worker thread died".to_string();
                true
            }
            Polled::Ready(Ok(ssh)) => {
                let state = self.connecting.take().expect("checked above");
                let input_tx = ssh.input_sender();
                self.sessions.push(ActiveSession {
                    title: state.label,
                    ssh,
                    terminal: TerminalView::new(DEFAULT_COLS, DEFAULT_ROWS, input_tx),
                    focus_requested: false,
                });
                // New session is the last tab; make it active.
                self.active = self.sessions.len();
                self.form.password.clear();
                true
            }
            Polled::Ready(Err(err)) => {
                self.connecting = None;
                self.status = format!("Connect failed: {err}");
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

    fn tabs_ui(&mut self, ui: &mut egui::Ui) {
        let mut switch_to: Option<usize> = None;
        let mut close: Option<usize> = None;

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

                ui.label("Password");
                ui.add(egui::TextEdit::singleline(&mut self.form.password).password(true));
                ui.end_row();
            });

        ui.add_space(8.0);

        let connecting = self.connecting.is_some();
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!connecting, egui::Button::new("Connect"))
                .clicked()
            {
                self.start_connect();
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

        if !self.status.is_empty() {
            ui.add_space(8.0);
            ui.colored_label(egui::Color32::LIGHT_RED, &self.status);
        }
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

        // Drain queued output before painting so the frame shows fresh data.
        while let Some(chunk) = session.ssh.try_read() {
            session.terminal.feed(&chunk);
            activity = true;
        }

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

        if let Some(title) = session.terminal.take_title() {
            session.title = title;
            // The tab bar was drawn earlier in this frame; request a repaint so
            // the new title is shown on the next one.
            activity = true;
        }
        if session.terminal.take_bell() {
            log::info!("terminal bell");
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
        if self.poll_connecting() {
            self.poll_countdown = POST_ACTIVITY_FRAMES;
        }
        // Any pending connect keeps us polling: its result — or a host-key
        // prompt — arrives on a background thread and has to be observed.
        if self.connecting.is_some() {
            self.poll_countdown = POST_ACTIVITY_FRAMES;
        }

        let mut activity = false;

        egui::TopBottomPanel::top("tab_bar").show(ctx, |ui| {
            self.tabs_ui(ui);
        });

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
            self.poll_countdown = POST_ACTIVITY_FRAMES;
        }

        if self.poll_countdown > 0 {
            self.poll_countdown -= 1;
            ctx.request_repaint_after(Duration::from_millis(REPAINT_DELAY_MS));
        }
        if activity {
            self.poll_countdown = POST_ACTIVITY_FRAMES;
            // Render immediately so new output (htop, tmux) appears without
            // waiting for the tail timer.
            ctx.request_repaint();
        } else if self.poll_countdown > 0 {
            self.poll_countdown -= 1;
            // Short tail so echo arriving a few frames later is still picked up.
            ctx.request_repaint_after(Duration::from_millis(8));
        }
        // Otherwise: no request → egui sleeps until the next input event.
    }
}
