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

/// Single-line input with the text vertically centred in a fixed-height box.
fn text_field<'a>(theme: &Theme, text: &'a mut String) -> egui::TextEdit<'a> {
    egui::TextEdit::singleline(text)
        .vertical_align(egui::Align::Center)
        .margin(egui::Margin::symmetric(theme.metrics.pad, 2.0))
        .min_size(egui::vec2(0.0, theme.metrics.control_height))
}

/// Fixed-width form label, vertically centred against the input beside it.
fn form_label(ui: &mut egui::Ui, theme: &Theme, text: &str, width: f32) {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(width, theme.metrics.control_height),
        egui::Sense::hover(),
    );
    ui.painter().text(
        rect.left_center(),
        egui::Align2::LEFT_CENTER,
        text,
        theme::body_font(theme),
        theme.palette.muted,
    );
}

/// Fixed-width table cell label; returns its response for tooltips.
fn sized_label(
    ui: &mut egui::Ui,
    theme: &Theme,
    text: &str,
    width: f32,
    color: egui::Color32,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(width, theme.metrics.row_height),
        egui::Sense::hover(),
    );
    paint_cell(
        ui,
        rect,
        text,
        theme::body_font(theme),
        color,
        egui::Align::Min,
    );
    response
}

/// Laid-out galley for one table cell: one line, truncated with an ellipsis.
fn cell_galley(
    ui: &egui::Ui,
    text: &str,
    font: egui::FontId,
    color: egui::Color32,
    max_width: f32,
) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::single_section(
        text.to_owned(),
        egui::TextFormat {
            font_id: font,
            color,
            ..Default::default()
        },
    );
    job.wrap.max_width = max_width.max(8.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('…');
    ui.fonts(|fonts| fonts.layout_job(job))
}

/// Paint one table cell, left-, centre- or right-aligned inside `rect`.
fn paint_cell(
    ui: &egui::Ui,
    rect: egui::Rect,
    text: &str,
    font: egui::FontId,
    color: egui::Color32,
    align: egui::Align,
) {
    if rect.width() <= 0.0 {
        return;
    }
    let galley = cell_galley(ui, text, font, color, rect.width());
    let size = galley.size();
    let pos = match align {
        egui::Align::Min => egui::pos2(rect.left(), rect.center().y - size.y / 2.0),
        egui::Align::Max => egui::pos2(rect.right() - size.x, rect.center().y - size.y / 2.0),
        egui::Align::Center => egui::pos2(
            rect.center().x - size.x / 2.0,
            rect.center().y - size.y / 2.0,
        ),
    };
    ui.painter().galley(pos, galley, color);
}

/// One tab, drawn as a single box with the close button inside it.
///
/// Returns the tab response and whether its close button was clicked.
fn tab_button(
    ui: &mut egui::Ui,
    theme: &Theme,
    label: &str,
    selected: bool,
    closable: bool,
) -> (egui::Response, bool) {
    const MAX_LABEL_WIDTH: f32 = 190.0;
    let label_width = cell_galley(
        ui,
        label,
        theme::body_font(theme),
        theme.palette.text,
        MAX_LABEL_WIDTH,
    )
    .size()
    .x;
    let close_width = if closable { 22.0 } else { 0.0 };
    let width =
        (label_width + theme.metrics.pad * 2.0 + close_width).max(theme.metrics.tab_min_width);
    let height = theme.metrics.control_height + 4.0;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::click());

    let fill = if selected {
        theme.palette.surface
    } else if response.hovered() {
        theme.palette.surface_hover
    } else {
        theme.palette.panel
    };
    let border = if selected {
        theme.palette.accent
    } else {
        theme.palette.border
    };
    let painter = ui.painter().clone();
    painter.rect_filled(rect, egui::Rounding::ZERO, fill);
    painter.rect_stroke(
        rect,
        egui::Rounding::ZERO,
        egui::Stroke::new(1.0_f32, border),
    );

    let galley = cell_galley(
        ui,
        label,
        theme::body_font(theme),
        theme.palette.text,
        MAX_LABEL_WIDTH,
    );
    painter.galley(
        egui::pos2(
            rect.left() + theme.metrics.pad,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        theme.palette.text,
    );

    let mut close_clicked = false;
    if closable {
        let close_rect =
            egui::Rect::from_min_max(egui::pos2(rect.right() - close_width, rect.top()), rect.max);
        let hovered = ui.rect_contains_pointer(close_rect);
        if hovered {
            painter.rect_filled(close_rect, egui::Rounding::ZERO, theme.palette.danger_soft);
        }
        let colour = if hovered {
            theme.palette.danger
        } else {
            theme.palette.muted
        };
        painter.text(
            close_rect.center(),
            egui::Align2::CENTER_CENTER,
            "×",
            theme::body_font(theme),
            colour,
        );
        let close_response = ui
            .interact(close_rect, response.id.with("close"), egui::Sense::click())
            .on_hover_text("Close session");
        close_clicked = close_response.clicked();
    }

    (response, close_clicked)
}

/// Column widths of the SFTP table, persisted per session.
///
/// `Name` takes whatever is left, so only the three metadata columns need to
/// be stored.
#[derive(Debug, Clone, Copy)]
struct SftpColumns {
    owner: f32,
    modified: f32,
    size: f32,
}

impl Default for SftpColumns {
    fn default() -> Self {
        Self {
            owner: 64.0,
            modified: 122.0,
            size: 52.0,
        }
    }
}

const NAME_MIN_WIDTH: f32 = 80.0;

impl SftpColumns {
    fn name_width(self, total: f32) -> f32 {
        (total - self.owner - self.modified - self.size).max(NAME_MIN_WIDTH)
    }
}

/// Rectangle of every column in one table row.
struct SftpLayout {
    owner: egui::Rect,
    modified: egui::Rect,
    size: egui::Rect,
}

impl SftpLayout {
    fn new(rect: egui::Rect, columns: SftpColumns) -> Self {
        let owner_x = rect.left() + columns.name_width(rect.width());
        let modified_x = owner_x + columns.owner;
        let size_x = modified_x + columns.modified;
        Self {
            owner: egui::Rect::from_x_y_ranges(owner_x..=modified_x, rect.y_range()),
            modified: egui::Rect::from_x_y_ranges(modified_x..=size_x, rect.y_range()),
            size: egui::Rect::from_x_y_ranges(size_x..=rect.right(), rect.y_range()),
        }
    }
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
            let response = ui.menu_button(egui::RichText::new("☰").size(14.0), |ui| {
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
        StatusKind::Success => "✔",
        StatusKind::Error => "×",
    }
}

/// Draw the SFTP column header and its resize handles.
///
/// The handles live only in the header row, never on the file rows. Each one
/// adjusts the column that starts at its divider, so the divider always
/// follows the pointer.
fn sftp_header(
    ui: &mut egui::Ui,
    theme: &Theme,
    columns: &mut SftpColumns,
    index: usize,
    rect: egui::Rect,
    width: f32,
) {
    let rect = egui::Rect::from_min_size(rect.min, egui::vec2(width, rect.height()));
    let layout = SftpLayout::new(rect, *columns);
    let font = theme::body_font(theme);
    let painter = ui.painter().clone();
    painter.rect_filled(rect, egui::Rounding::ZERO, theme.palette.panel);

    let name_rect = egui::Rect::from_min_max(
        egui::pos2(rect.left() + 22.0, rect.top()),
        egui::pos2(layout.owner.left() - 2.0, rect.bottom()),
    );
    paint_cell(
        ui,
        name_rect,
        "Name",
        font.clone(),
        theme.palette.muted,
        egui::Align::Min,
    );
    paint_cell(
        ui,
        layout.owner.shrink2(egui::vec2(4.0, 0.0)),
        "Owner",
        font.clone(),
        theme.palette.muted,
        egui::Align::Min,
    );
    paint_cell(
        ui,
        layout.modified.shrink2(egui::vec2(4.0, 0.0)),
        "Modified",
        font.clone(),
        theme.palette.muted,
        egui::Align::Min,
    );
    paint_cell(
        ui,
        layout.size.shrink2(egui::vec2(4.0, 0.0)),
        "Size",
        font,
        theme.palette.muted,
        egui::Align::Max,
    );

    let separators = [
        ("owner", layout.owner.left()),
        ("modified", layout.modified.left()),
        ("size", layout.size.left()),
    ];
    for (_, x) in separators {
        painter.vline(
            x,
            rect.y_range(),
            egui::Stroke::new(1.0_f32, theme.palette.border),
        );
    }
    painter.hline(
        rect.x_range(),
        rect.bottom(),
        egui::Stroke::new(1.0_f32, theme.palette.border_strong),
    );

    let total = rect.width();
    for (which, x) in separators {
        let handle = egui::Rect::from_min_max(
            egui::pos2(x - 3.0, rect.top()),
            egui::pos2(x + 3.0, rect.bottom()),
        );
        let response = ui.interact(
            handle,
            egui::Id::new(("sftp_col_resize", index, which)),
            egui::Sense::click_and_drag(),
        );
        if response.hovered() || response.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        }
        if !response.dragged() {
            continue;
        }
        let Some(pointer) = response.interact_pointer_pos() else {
            continue;
        };
        match which {
            "owner" => {
                let max = (total - NAME_MIN_WIDTH - columns.modified - columns.size).max(56.0);
                columns.owner =
                    (rect.right() - pointer.x - columns.modified - columns.size).clamp(56.0, max);
            }
            "modified" => {
                let max = (total - NAME_MIN_WIDTH - columns.owner - columns.size).max(74.0);
                columns.modified = (rect.right() - pointer.x - columns.size).clamp(74.0, max);
            }
            _ => {
                columns.size = (rect.right() - pointer.x).clamp(44.0, 140.0);
            }
        }
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
    /// Name used when "Save as profile" is checked.
    profile_name: String,
    /// Whether Connect should also store the form as a profile.
    save_as_profile: bool,
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
    /// Resized column widths of the file table.
    sftp_columns: SftpColumns,
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
            save_as_profile: false,
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
                    sftp_columns: SftpColumns::default(),
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

        let theme = self.theme.theme();
        egui::Window::new("Unknown host key")
            .id(egui::Id::new("host_key_prompt"))
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .default_size(egui::vec2(480.0, 1.0))
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .frame(theme::modal_frame(theme))
            .show(ctx, |ui| {
                ui.set_width(440.0);
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("Unknown host key")
                            .size(18.0)
                            .strong()
                            .color(theme.palette.text),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("×").on_hover_text("Reject").clicked() {
                            answer = Some(false);
                        }
                    });
                });
                ui.add_space(theme.metrics.pad);
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(format!(
                            "The authenticity of host '{}:{}' can't be established.",
                            prompt.host, prompt.port
                        ))
                        .color(theme.palette.text),
                    )
                    .wrap(),
                );
                ui.add_space(theme.metrics.pad);
                theme::muted(ui, theme, "Host key fingerprint");
                egui::Frame::none()
                    .fill(theme.palette.input_bg)
                    .stroke(egui::Stroke::new(1.0_f32, theme.palette.border))
                    .inner_margin(egui::Margin::symmetric(theme.metrics.pad, 4.0))
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&prompt.fingerprint)
                                    .monospace()
                                    .color(theme.palette.text),
                            )
                            .wrap(),
                        );
                    });
                ui.add_space(theme.metrics.pad);
                ui.add(
                    egui::Label::new(
                        egui::RichText::new("Accept this key and record it in ~/.ssh/known_hosts?")
                            .color(theme.palette.text),
                    )
                    .wrap(),
                );
                ui.add_space(theme.metrics.card_padding);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let accept = theme::primary_button(theme, "Accept & save")
                        .min_size(egui::vec2(128.0, theme.metrics.control_height));
                    if ui.add(accept).clicked() {
                        answer = Some(true);
                    }
                    let reject = egui::Button::new("Reject")
                        .min_size(egui::vec2(92.0, theme.metrics.control_height));
                    if ui.add(reject).clicked() {
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
        let theme = self.theme.theme();
        let mut switch_to: Option<usize> = None;
        let mut close: Option<usize> = None;

        ui.horizontal(|ui| {
            // The theme picker is drawn first in a right-to-left layout so it
            // stays pinned to the far edge no matter how many tabs are open.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.theme_menu_ui(ui);
                ui.add_space(2.0);
                ui.separator();
                ui.add_space(2.0);

                // Keep the Files toggle outside the scroll area so a long tab
                // list cannot hide it.
                if self.active != NEW_SESSION_TAB {
                    if ui
                        .selectable_label(self.show_sftp, "Files")
                        .on_hover_text("Show or hide the remote file panel")
                        .clicked()
                    {
                        self.show_sftp = !self.show_sftp;
                    }
                    ui.add_space(4.0);
                }

                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    egui::ScrollArea::horizontal()
                        .id_salt("tab_bar_scroll")
                        .auto_shrink([false, true])
                        .scroll_bar_visibility(
                            egui::containers::scroll_area::ScrollBarVisibility::AlwaysHidden,
                        )
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 4.0;
                                let (new_tab, _) =
                                    tab_button(ui, theme, "+ New Session", self.active == 0, false);
                                if new_tab.clicked() {
                                    switch_to = Some(NEW_SESSION_TAB);
                                }

                                for (i, session) in self.sessions.iter().enumerate() {
                                    let tab_index = i + 1;
                                    let (tab, close_clicked) = tab_button(
                                        ui,
                                        theme,
                                        &session.title,
                                        self.active == tab_index,
                                        true,
                                    );
                                    if close_clicked || tab.middle_clicked() {
                                        close = Some(i);
                                    } else if tab.clicked() {
                                        switch_to = Some(tab_index);
                                    }
                                }
                            });
                        });
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

    /// Small theme picker shown at the right edge of the tab bar.
    fn theme_menu_ui(&mut self, ui: &mut egui::Ui) {
        let mut chosen: Option<ThemeId> = None;
        ui.menu_button("🎨 Theme", |ui| {
            for id in ThemeId::ALL {
                if ui.selectable_label(self.theme == id, id.name()).clicked() {
                    chosen = Some(id);
                    ui.close_menu();
                }
            }
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
        self.save_as_profile = false;
        self.clear_status();

        if profile.save_password {
            if KEYRING_ENABLED {
                match secrets::load(&profile.name) {
                    Ok(Some(password)) => self.form.password = password,
                    Ok(None) => self.set_error(format!(
                        "No password is stored for '{}'. Type one and tick Save as profile again.",
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

    /// Store the current form as a profile. Returns `true` on success.
    fn save_profile(&mut self) -> bool {
        let host = self.form.host.trim().to_string();
        let user = self.form.user.trim().to_string();
        let port: u16 = match self.form.port.trim().parse() {
            Ok(port) => port,
            Err(_) => {
                self.set_error("Port must be a number");
                return false;
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
            return false;
        }

        if KEYRING_ENABLED && requested {
            if self.form.password.is_empty() {
                self.set_error("Type a password to store it in the system keyring");
                return false;
            }
            // Store the secret first: if the keyring is unavailable, do not
            // write a profile that claims to have a stored password.
            if let Err(err) = secrets::save(&profile.name, &self.form.password) {
                self.set_error(format!(
                    "Could not store the password in the system keyring: {err}"
                ));
                return false;
            }
        }

        let saved_name = profile.name.clone();
        match self.store.upsert(profile) {
            Ok(_) => {
                self.profile_name = saved_name.clone();

                if KEYRING_ENABLED && !requested && had_stored_password {
                    if let Err(err) = secrets::delete(&saved_name) {
                        self.set_error(format!(
                            "Profile saved, but its keyring password could not be removed: {err}"
                        ));
                        return true;
                    }
                }

                self.set_info(format!("Saved profile '{saved_name}'"));
                true
            }
            Err(err) => {
                self.set_error(format!("Could not save profile: {err}"));
                false
            }
        }
    }

    /// Remove the profile at `index`, including any stored password.
    fn delete_profile(&mut self, index: usize) {
        match self.store.remove(index) {
            Ok(Some(profile)) => {
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
            Ok(None) => {}
            Err(err) => {
                // The profile is still on disk.
                self.set_error(format!("Could not delete profile: {err}"));
            }
        }
    }

    /// Minimal saved-profile table: one row per profile with Load / Delete.
    fn saved_profiles_ui(&mut self, ui: &mut egui::Ui, theme: &Theme) {
        let profiles = self.store.profiles().to_vec();
        theme::section_label(ui, theme, &format!("Saved profiles ({})", profiles.len()))
            .on_hover_text(format!("Stored in {}", self.store.path().display()));
        if profiles.is_empty() {
            theme::muted(
                ui,
                theme,
                "Nothing saved yet. Tick “Save as profile” above to keep this connection.",
            );
            return;
        }

        let row_height = theme.metrics.row_height;
        let load_width = 48.0;
        let name_width = 132.0;
        ui.spacing_mut().button_padding = egui::vec2(theme.metrics.pad, 2.0);
        let mut load: Option<usize> = None;
        let mut delete: Option<usize> = None;

        // Header row, same font size as the values below it.
        ui.horizontal(|ui| {
            ui.add_space(load_width + ui.spacing().item_spacing.x);
            sized_label(ui, theme, "Name", name_width, theme.palette.muted);
            let target_width =
                (ui.available_width() - theme.metrics.control_height - 4.0).max(60.0);
            sized_label(ui, theme, "Target", target_width, theme.palette.muted);
        });

        egui::ScrollArea::vertical()
            .id_salt("saved_profiles")
            .max_height((row_height + 8.0) * 4.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for (index, profile) in profiles.iter().enumerate() {
                    ui.horizontal(|ui| {
                        let load_button =
                            egui::Button::new("Load").min_size(egui::vec2(load_width, row_height));
                        if ui.add(load_button).clicked() {
                            load = Some(index);
                        }

                        sized_label(ui, theme, &profile.name, name_width, theme.palette.text)
                            .on_hover_text(&profile.name);

                        let target = format!("{}@{}:{}", profile.user, profile.host, profile.port);
                        let target_width =
                            (ui.available_width() - theme.metrics.control_height - 4.0).max(60.0);
                        sized_label(ui, theme, &target, target_width, theme.palette.muted)
                            .on_hover_text(&target);

                        if theme::icon_button(ui, theme, "×", "Delete this profile", true).clicked()
                        {
                            delete = Some(index);
                        }
                    });
                }
            });

        if let Some(index) = load {
            self.load_profile(index);
            // The form was drawn above this table.
            ui.ctx().request_repaint();
        }
        if let Some(index) = delete {
            self.delete_profile(index);
            ui.ctx().request_repaint();
        }
    }

    fn new_session_ui(&mut self, ui: &mut egui::Ui) {
        let theme = self.theme.theme();
        egui::ScrollArea::vertical()
            .id_salt("new_session_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(theme.metrics.card_padding);
                let width = 600.0_f32.min((ui.available_width() - 24.0).max(320.0));
                let leading = ((ui.available_width() - width) * 0.5).max(0.0);

                let page_height = ui.available_height().max(320.0);
                ui.horizontal(|ui| {
                    ui.add_space(leading);
                    ui.allocate_ui_with_layout(
                        egui::vec2(width, page_height),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.set_width(width);
                            ui.label(
                                egui::RichText::new("New SSH session")
                                    .size(theme.metrics.heading_size)
                                    .color(theme.palette.text),
                            );
                            theme::muted(ui, theme, "Connect to a remote machine over SSH.");
                            ui.add_space(theme.metrics.card_padding);

                            theme::card_frame(theme).show(ui, |ui| {
                                ui.set_width(width - theme.metrics.card_padding * 2.0);
                                // Compact rows: checkboxes and labels use the
                                // interaction height, inputs set their own.
                                ui.spacing_mut().interact_size.y = 20.0;
                                let control = theme.metrics.control_height;
                                let label_width = ui
                                    .painter()
                                    .layout_no_wrap(
                                        "Password".to_owned(),
                                        theme::body_font(theme),
                                        theme.palette.muted,
                                    )
                                    .size()
                                    .x
                                    + 8.0;
                                let gap = ui.spacing().item_spacing.x;

                                // Host and port share one row to save space.
                                ui.horizontal(|ui| {
                                    let port_width = 62.0;
                                    let port_label_width = ui
                                        .painter()
                                        .layout_no_wrap(
                                            "Port".to_owned(),
                                            theme::body_font(theme),
                                            theme.palette.muted,
                                        )
                                        .size()
                                        .x
                                        + 8.0;
                                    let host_width = (ui.available_width()
                                        - label_width
                                        - port_label_width
                                        - port_width
                                        - gap * 3.0)
                                        .max(120.0);
                                    form_label(ui, theme, "Host", label_width);
                                    ui.add_sized(
                                        [host_width, control],
                                        text_field(theme, &mut self.form.host)
                                            .hint_text("example.com"),
                                    );
                                    form_label(ui, theme, "Port", port_label_width);
                                    ui.add_sized(
                                        [port_width, control],
                                        text_field(theme, &mut self.form.port).hint_text("22"),
                                    );
                                });

                                ui.horizontal(|ui| {
                                    form_label(ui, theme, "User", label_width);
                                    let field_width = ui.available_width();
                                    ui.add_sized(
                                        [field_width, control],
                                        text_field(theme, &mut self.form.user)
                                            .hint_text("username"),
                                    );
                                });

                                ui.horizontal(|ui| {
                                    form_label(ui, theme, "Password", label_width);
                                    let field_width = ui.available_width();
                                    ui.add_sized(
                                        [field_width, control],
                                        text_field(theme, &mut self.form.password)
                                            .password(true)
                                            .hint_text("Optional with a key or agent"),
                                    )
                                    .on_hover_text(
                                        "Used when key/agent authentication is disabled or fails.",
                                    );
                                });

                                ui.add_space(2.0);
                                ui.checkbox(&mut self.form.use_key, "Use a private key")
                                    .on_hover_text(
                                        "Tried before the password. Leave the path blank to use \
                                         ~/.ssh/id_ed25519, id_ecdsa, id_rsa or id_dsa.",
                                    );
                                if self.form.use_key {
                                    ui.horizontal(|ui| {
                                        form_label(ui, theme, "", label_width);
                                        let field_width = ui.available_width();
                                        ui.add_sized(
                                            [field_width, control],
                                            text_field(theme, &mut self.form.key_path)
                                                .hint_text("Key path (blank = auto-detect)"),
                                        );
                                    });
                                }
                                ui.checkbox(&mut self.form.use_agent, "Use SSH agent identities")
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
                                        "Optional, per profile. Requires an explicit profile name. \
                                         Stored by the OS keyring, never in sessions.toml.",
                                    );
                                }

                                ui.add_space(4.0);
                                ui.horizontal(|ui| {
                                    ui.checkbox(&mut self.save_as_profile, "Save as profile")
                                        .on_hover_text(
                                            "Also stores host, port, user and auth settings in \
                                             sessions.toml when you connect.",
                                        );
                                    let field_width = ui.available_width();
                                    ui.add_enabled_ui(self.save_as_profile, |ui| {
                                        ui.add_sized(
                                            [field_width, control],
                                            text_field(theme, &mut self.profile_name)
                                                .hint_text("Profile name (blank = user@host)"),
                                        );
                                    });
                                });

                                ui.add_space(6.0);
                                let connecting = self.connecting.is_some();
                                let mut connect_clicked = false;
                                ui.horizontal(|ui| {
                                    let button = theme::primary_button(theme, "Connect")
                                        .min_size(egui::vec2(104.0, control));
                                    if ui.add_enabled(!connecting, button).clicked() {
                                        connect_clicked = true;
                                    }
                                    if let Some(state) = &self.connecting {
                                        ui.spinner();
                                        let label = if state.prompt.is_some() {
                                            "Waiting for host key confirmation…".to_string()
                                        } else {
                                            format!("Connecting to {}…", state.label)
                                        };
                                        theme::muted(ui, theme, label);
                                    }
                                });

                                if connect_clicked {
                                    // Saving first keeps a failing profile out of
                                    // the connection attempt.
                                    if !self.save_as_profile || self.save_profile() {
                                        self.start_connect(ui.ctx());
                                    }
                                }

                                if !self.status.text.is_empty() {
                                    let color = if self.status.is_error {
                                        theme.palette.danger
                                    } else {
                                        theme.palette.success
                                    };
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(&self.status.text)
                                                .size(theme.metrics.small_size)
                                                .color(color),
                                        )
                                        .wrap(),
                                    );
                                }

                                ui.add_space(theme.metrics.card_padding);
                                ui.separator();
                                ui.add_space(2.0);
                                self.saved_profiles_ui(ui, theme);
                            });

                            ui.add_space(theme.metrics.card_padding);
                        },
                    );
                });
            });
    }

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
        let columns = &mut session.sftp_columns;
        let mut activity = false;
        let mut hide = false;

        // Never let a wide child grow the panel's stored width.
        let content_width = ui.available_width();
        ui.set_max_width(content_width);
        ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);

        let busy = explorer.is_busy();
        let selected = explorer.selected_entry().cloned();
        let can_up = !explorer.current.is_empty() && explorer.current != "/";
        let can_copy = selected.is_some() && !busy;
        let can_paste = explorer.clipboard.is_some() && !busy;

        ui.horizontal(|ui| {
            if theme::icon_button(ui, theme, "🏠", "Go to the login directory", !busy).clicked() {
                explorer.load_home(ssh);
                activity = true;
            }
            if theme::icon_button(ui, theme, "⬆", "Up one level", can_up && !busy).clicked() {
                explorer.up(ssh);
                activity = true;
            }
            if theme::icon_button(ui, theme, "🔄", "Refresh this directory", !busy).clicked() {
                explorer.refresh(ssh);
                activity = true;
            }
            if theme::icon_button(ui, theme, "➕", "Create a folder here", !busy).clicked() {
                explorer.begin_new_folder();
                activity = true;
            }
            if theme::icon_button(ui, theme, "🗐", "Copy the selected entry", can_copy).clicked()
            {
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
            if theme::icon_toggle(
                ui,
                theme,
                "👁",
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
                "⚡",
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

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::icon_button(ui, theme, "×", "Hide the file panel", true).clicked() {
                    hide = true;
                }
            });
        });

        ui.horizontal(|ui| {
            let go_width = theme.metrics.control_height;
            let width = (ui.available_width() - go_width - 4.0).max(80.0);
            let edit = ui.add_sized(
                [width, theme.metrics.control_height],
                text_field(theme, &mut explorer.path_edit).hint_text("Remote path"),
            );
            if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                let path = explorer.path_edit.clone();
                explorer.navigate(ssh, path);
                activity = true;
            }
            if theme::icon_button(ui, theme, "➡", "Open this path", true).clicked() {
                let path = explorer.path_edit.clone();
                explorer.navigate(ssh, path);
                activity = true;
            }
        });

        // Fixed message bar at the bottom. Declared before the table so the
        // table gets the remaining height.
        egui::TopBottomPanel::bottom(egui::Id::new(("sftp_status", index)))
            .exact_height(theme.metrics.row_height + 4.0)
            .frame(
                egui::Frame::none()
                    .fill(theme.palette.panel)
                    .stroke(egui::Stroke::new(1.0_f32, theme.palette.border))
                    .inner_margin(egui::Margin::symmetric(4.0, 2.0)),
            )
            .show_inside(ui, |ui| {
                sftp_status_bar(ui, theme, explorer);
            });

        if let Some(reason) = explorer.unavailable.clone() {
            ui.add_space(theme.metrics.card_padding);
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new("SFTP is unavailable").color(theme.palette.warning));
                ui.add(
                    egui::Label::new(egui::RichText::new(reason).color(theme.palette.muted)).wrap(),
                );
                ui.add_space(4.0);
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

        let row_height = theme.metrics.row_height;
        let (header_rect, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), row_height),
            egui::Sense::hover(),
        );

        let row_count = visible_indices.len().max(1);
        let output = egui::ScrollArea::vertical()
            .id_salt(("sftp_scroll", index))
            .auto_shrink([false, false])
            .show_rows(ui, row_height, row_count, |ui, range| {
                for visible_index in range {
                    let (rect, response) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), row_height),
                        egui::Sense::click(),
                    );

                    if visible_indices.is_empty() {
                        paint_cell(
                            ui,
                            rect.shrink2(egui::vec2(4.0, 0.0)),
                            empty_message,
                            theme::body_font(theme),
                            theme.palette.muted,
                            egui::Align::Min,
                        );
                        continue;
                    }

                    let entry_index = visible_indices[visible_index];
                    let entry = &explorer.entries[entry_index];
                    let is_selected = explorer.selected == Some(entry_index);
                    let fill = if is_selected {
                        theme.palette.selection
                    } else if response.hovered() {
                        theme.palette.surface_hover
                    } else if visible_index % 2 == 1 {
                        theme.palette.stripe
                    } else {
                        theme.palette.panel
                    };
                    ui.painter().rect_filled(rect, egui::Rounding::ZERO, fill);

                    let layout = SftpLayout::new(rect, *columns);
                    let font = theme::body_font(theme);
                    let icon = if entry.is_dir {
                        "🗀"
                    } else if entry.is_symlink {
                        "🔗"
                    } else {
                        "🗋"
                    };
                    let icon_rect = egui::Rect::from_min_max(
                        egui::pos2(rect.left() + 4.0, rect.top()),
                        egui::pos2(rect.left() + 22.0, rect.bottom()),
                    );
                    paint_cell(
                        ui,
                        icon_rect,
                        icon,
                        theme::body_font(theme),
                        theme.palette.muted,
                        egui::Align::Center,
                    );
                    let name_rect = egui::Rect::from_min_max(
                        egui::pos2(rect.left() + 22.0, rect.top()),
                        egui::pos2(layout.owner.left() - 2.0, rect.bottom()),
                    );
                    paint_cell(
                        ui,
                        name_rect,
                        &entry.name,
                        font.clone(),
                        theme.palette.text,
                        egui::Align::Min,
                    );
                    paint_cell(
                        ui,
                        layout.owner.shrink2(egui::vec2(4.0, 0.0)),
                        format_owner(entry),
                        font.clone(),
                        theme.palette.muted,
                        egui::Align::Min,
                    );
                    paint_cell(
                        ui,
                        layout.modified.shrink2(egui::vec2(4.0, 0.0)),
                        &format_timestamp(entry.modified),
                        font.clone(),
                        theme.palette.muted,
                        egui::Align::Min,
                    );
                    let size = if entry.is_dir {
                        "—".to_string()
                    } else {
                        format_size(entry.size)
                    };
                    paint_cell(
                        ui,
                        layout.size.shrink2(egui::vec2(4.0, 0.0)),
                        &size,
                        font,
                        theme.palette.muted,
                        egui::Align::Max,
                    );

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

                    let response = response.on_hover_text(details);
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
                        ui.label(egui::RichText::new(&entry.name));
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
                            .add(
                                egui::Button::new(
                                    egui::RichText::new("Delete…").color(theme.palette.danger),
                                )
                                .rounding(egui::Rounding::ZERO),
                            )
                            .clicked()
                        {
                            row_action = Some(RowAction::Delete(entry.clone()));
                            ui.close_menu();
                        }
                    });
                }
            });

        sftp_header(
            ui,
            theme,
            columns,
            index,
            header_rect,
            output.inner_rect.width(),
        );

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
            .default_size(egui::vec2(424.0, 1.0))
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
                ui.add_space(theme.metrics.pad);

                match &kind {
                    SftpDialogKind::Rename { .. } => {
                        field_label(ui, theme, "New name");
                        let edit = ui.add_sized(
                            [ui.available_width(), theme.metrics.control_height],
                            text_field(theme, &mut dialog.name).hint_text("File or folder name"),
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
                            text_field(theme, &mut dialog.name).hint_text("New folder"),
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

                ui.add_space(theme.metrics.card_padding);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let primary = match &kind {
                        SftpDialogKind::Rename { .. } => "Rename",
                        SftpDialogKind::NewFolder { .. } => "Create",
                        SftpDialogKind::Delete { .. } => "Delete",
                    };
                    let primary_button = if matches!(kind, SftpDialogKind::Delete { .. }) {
                        theme::danger_button(theme, primary)
                    } else {
                        theme::primary_button(theme, primary)
                    }
                    .min_size(egui::vec2(92.0, theme.metrics.control_height));
                    if ui.add(primary_button).clicked() {
                        submit = true;
                    }
                    let cancel_button = egui::Button::new("Cancel")
                        .min_size(egui::vec2(92.0, theme.metrics.control_height));
                    if ui.add(cancel_button).clicked() {
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
            .default_size(egui::vec2(424.0, 1.0))
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
                    let open = theme::primary_button(theme, "Open")
                        .min_size(egui::vec2(92.0, theme.metrics.control_height));
                    if ui.add(open).clicked() {
                        submit = true;
                    }
                    let cancel_button = egui::Button::new("Cancel")
                        .min_size(egui::vec2(92.0, theme.metrics.control_height));
                    if ui.add(cancel_button).clicked() {
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
        let theme = self.theme.theme();
        let mut activity = false;

        // Keep the action row visible without taking height away from the
        // terminal: it is declared as a bottom panel before the terminal.
        let mut disconnect_clicked = false;
        egui::TopBottomPanel::bottom(egui::Id::new(("terminal_actions", index)))
            .exact_height(theme.metrics.control_height + 8.0)
            .frame(
                egui::Frame::none()
                    .fill(theme.palette.bg)
                    .inner_margin(egui::Margin::symmetric(4.0, 4.0)),
            )
            .show_inside(ui, |ui| {
                if ui.button("Disconnect").clicked() {
                    disconnect_clicked = true;
                }
            });

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
        let theme = self.theme.theme();

        egui::TopBottomPanel::top("tab_bar")
            .frame(theme::panel_frame(theme))
            .show(ctx, |ui| {
                self.tabs_ui(ui);
            });

        // The remote file panel belongs to the active session and sits left of
        // the terminal. It must be declared before the central panel so egui can
        // reserve its width. The cap keeps the terminal visible: the panel can
        // never take more than a little over half of the window.
        if self.show_sftp {
            if let Some(idx) = self.active.checked_sub(1) {
                if idx < self.sessions.len() {
                    let max_width = (ctx.screen_rect().width() * 0.55).clamp(320.0, 720.0);
                    egui::SidePanel::left("sftp_panel")
                        .resizable(true)
                        .default_width(450.0)
                        .min_width(340.0)
                        .max_width(max_width)
                        .frame(theme::panel_frame(theme))
                        .show(ctx, |ui| {
                            activity |= self.sftp_ui(ui, idx);
                        });
                    activity |= self.sftp_dialogs_ui(ctx, idx);
                    activity |= self.editor_picker_ui(ctx, idx);
                }
            }
        }

        let mut disconnect_requested = false;
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
