use crate::input::event_to_bytes;
use crate::ssh::{SshConfig, SshSession};
use crate::terminal::TerminalView;
use egui::{Color32, RichText};
use std::time::Duration;

/// Top-level application state.
pub struct RustTermApp {
    // Connection form.
    host: String,
    port: String,
    username: String,
    password: String,

    // Active session (None = show connect form / idle).
    session: Option<ActiveSession>,

    // Status / error text.
    status: String,
    error: Option<String>,

    // Whether a connect is in flight.
    connecting: bool,

    // Receiver for the SSH session result (connect runs on a background thread).
    pending: Option<std::sync::mpsc::Receiver<anyhow::Result<SshSession>>>,
}

struct ActiveSession {
    view: TerminalView,
    session: SshSession,
    title: String,
    /// Whether we have requested initial keyboard focus yet. We request
    /// focus once, then only on click; re-requesting every frame fights
    /// the focus-lock filter and causes arrow-key flicker.
    focus_requested: bool,
}

impl RustTermApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        Self {
            host: "localhost".to_owned(),
            port: "22".to_owned(),
            username: std::env::var("USER").unwrap_or_else(|_| "root".to_owned()),
            password: String::new(),
            session: None,
            status: "Enter connection details and click Connect.".to_owned(),
            error: None,
            connecting: false,
            pending: None,
        }
    }

    fn start_connect(&mut self, ctx: &egui::Context) {
        let port: u16 = match self.port.parse() {
            Ok(p) => p,
            Err(_) => {
                self.error = Some("Invalid port number.".to_owned());
                return;
            }
        };

        let config = SshConfig {
            host: self.host.clone(),
            port,
            username: self.username.clone(),
            password: self.password.clone(),
        };

        self.connecting = true;
        self.error = None;
        self.status = format!("Connecting to {}:{}…", config.host, config.port);

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = SshSession::connect(config);
            tx.send(result).ok();
        });

        self.pending = Some(rx);
        ctx.request_repaint();
    }

    fn poll_pending(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.pending else {
            return;
        };

        match rx.try_recv() {
            Ok(Ok(session)) => {
                let view = TerminalView::new(80, 24);
                self.session = Some(ActiveSession {
                    view,
                    session,
                    title: format!("{}@{}", self.username, self.host),
                    focus_requested: false,
                });
                self.connecting = false;
                self.status = "Connected.".to_owned();
                self.pending = None;
            }
            Ok(Err(e)) => {
                self.error = Some(format!("{e:#}"));
                self.connecting = false;
                self.status = "Connection failed.".to_owned();
                self.pending = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(Duration::from_millis(50));
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.error = Some("Connection thread died unexpectedly.".to_owned());
                self.connecting = false;
                self.pending = None;
            }
        }
    }

    fn connect_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.heading("rustterm");
        ui.label("Lightweight SSH client. Enter details to connect.");
        ui.separator();

        egui::Grid::new("connect_form")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                ui.label("Host");
                ui.text_edit_singleline(&mut self.host);
                ui.end_row();

                ui.label("Port");
                ui.text_edit_singleline(&mut self.port);
                ui.end_row();

                ui.label("Username");
                ui.text_edit_singleline(&mut self.username);
                ui.end_row();

                ui.label("Password");
                ui.add(egui::TextEdit::singleline(&mut self.password).password(true));
                ui.end_row();
            });

        ui.add_space(8.0);

        if self.connecting {
            ui.spinner();
            ui.label("Connecting…");
        } else if ui.button("Connect").clicked() {
            self.start_connect(ctx);
        }

        if let Some(err) = &self.error {
            ui.add_space(8.0);
            ui.colored_label(Color32::from_rgb(220, 80, 80), err);
        }

        ui.add_space(8.0);
        ui.label(RichText::new(self.status.as_str()).weak());
    }

    fn terminal_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // Defer the disconnect so we don't mutate `self.session` while it is
        // borrowed by `active`.
        let mut disconnect = false;

        if let Some(active) = &mut self.session {
            // Drain pending SSH output into the terminal parser.
            for chunk in active.session.drain_output() {
                active.view.feed(&chunk);
            }

            // Forward any grid resize to the remote pty.
            if let Some((cols, rows)) = active.view.take_resize() {
                active.session.resize(cols, rows);
            }

            // Header bar.
            ui.horizontal(|ui| {
                ui.label(RichText::new(active.title.as_str()).strong());
                ui.separator();
                if ui.button("Disconnect").clicked() {
                    disconnect = true;
                }
            });
            ui.separator();

            // Terminal area fills the rest of the window.
            let response = active.view.ui(ui);

            // Request focus on first appearance and on click only.
            // Do NOT re-request every frame: that fights the lock filter
            // and makes arrow keys flicker through the Disconnect button.
            if !active.focus_requested {
                response.request_focus();
                active.focus_requested = true;
            }
            if response.clicked() {
                response.request_focus();
            }

            // Install the lock filter every frame, after the focus request.
            // It is only consulted while this widget has focus, and it
            // stops egui from turning Tab / arrows / Escape into global
            // focus navigation.
            ui.memory_mut(|m| {
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

            if response.has_focus() {
                let events = ui.input(|i| i.events.clone());
                let mods = ui.input(|i| i.modifiers);

                for event in events {
                    if let Some(bytes) = event_to_bytes(&event, mods) {
                        active.session.write(bytes);
                    }
                }
            }

            // Keep repainting so SSH output shows up promptly.
            ctx.request_repaint_after(Duration::from_millis(30));
        }

        if disconnect {
            self.session = None;
            self.status = "Disconnected.".to_owned();
        }
    }
}

impl eframe::App for RustTermApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_pending(ctx);

        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(Color32::from_rgb(28, 28, 32)))
            .show(ctx, |ui| {
                if self.session.is_some() {
                    self.terminal_ui(ui, ctx);
                } else {
                    self.connect_ui(ui, ctx);
                }
            });
    }
}
