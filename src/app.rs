//! Application shell: tab bar, connect form, terminal views and SFTP panel.
//!
//! Tabs: index 0 is *always* the "New Session" form; connected sessions live
//! at indices 1..N. Only one connect attempt may be in flight at a time.
//!
//! `SshSession::connect` blocks through auth, so it runs on a background
//! thread and the result is polled each frame (handoff flaw F1). Closing a tab
//! sends on `disconnect_tx` so the SSH connection is torn down cleanly
//! (handoff flaw F2).
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

use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;

use crate::input;
use crate::secrets;
use crate::sessions::{SessionProfile, SessionStore};
use crate::settings::{DisplayPref, Settings};
use crate::sftp::{
    command_available, editor_program, format_age, format_owner, format_permissions, format_size,
    format_timestamp, EditJournal, EditorChoice, SftpDialogKind, SftpExplorer, StatusKind,
};
use crate::ssh::{
    ConflictPolicy, HostKeyPrompt, RemoteEntry, ShellKind, SshConfig, SshSession, VncProbe,
    VncTarget, Wake,
};
use crate::terminal::{TermAction, TerminalView};
use crate::theme::{self, Theme, ThemeId};

const NEW_SESSION_TAB: usize = 0;
const DEFAULT_COLS: usize = 80;
const DEFAULT_ROWS: usize = 24;

/// Delay between frames while a connection attempt is still in progress. Once
/// connected, the SSH thread wakes the UI itself when output arrives.
const CONNECT_POLL_MS: u64 = 8;

/// Slow safety pass behind the file watcher. Saves arrive as inotify events and
/// wake the UI immediately; this timer only catches events the watcher missed.
const EDIT_SAFETY_MS: u64 = 5000;

/// Repaint delay while a save event waits out the debounce (150 ms + margin).
const EDIT_DEBOUNCE_MS: u64 = 170;

/// Upper bound on remote bytes decoded per session in a single frame. This
/// keeps a large burst from stalling the UI while still allowing tens of MiB/s
/// sustained. Applied per session, with the visible tab drained first.
const MAX_OUTPUT_PER_FRAME: usize = 256 * 1024;

/// How long a VNC tunnel with no viewer process and no connected client is
/// kept alive. Covers viewers that daemonise or hand off to an existing
/// instance (e.g. `remmina -c`) and never connect in the spawning process.
const VNC_TUNNEL_GRACE: Duration = Duration::from_secs(15);

/// Wake egui from a background thread, but coalesce calls that arrive within
/// ~8 ms so a fast link cannot flood the UI thread with repaint requests.
fn debounced_wake(ctx: egui::Context) -> Wake {
    let last = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    std::sync::Arc::new(move || {
        use std::sync::atomic::Ordering;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let prev = last.load(Ordering::Relaxed);
        if now.saturating_sub(prev) >= 8 {
            last.store(now, Ordering::Relaxed);
            ctx.request_repaint();
        }
    })
}

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

/// Column layout of the SFTP table, persisted per session.
///
/// Columns are ordered **Name, Modified, Size, Owner, Permissions** and each one
/// owns a fixed number of pixels on a table canvas that never changes width. The side
/// panel is only a viewport on top of that canvas: resizing it clips or reveals
/// the table instead of reflowing it. Dividers move in whole `COLUMN_STEP`
/// pixels rather than following the pointer freely.
#[derive(Debug, Clone, Copy)]
struct SftpColumns {
    name: f32,
    modified: f32,
    size: f32,
    owner: f32,
    permissions: f32,
}

/// Width of the table canvas, in pixels.
///
/// This is deliberately independent of the side panel: the panel may shrink
/// below it (the table then scrolls horizontally) or grow past it (blank space
/// appears to the right), but the columns themselves never move.
const TABLE_WIDTH: f32 = 560.0;

impl Default for SftpColumns {
    fn default() -> Self {
        // `Name` gets the largest share; the metadata columns split the rest.
        Self {
            name: TABLE_WIDTH * 0.42,
            modified: TABLE_WIDTH * 0.18,
            size: TABLE_WIDTH * 0.10,
            owner: TABLE_WIDTH * 0.14,
            permissions: TABLE_WIDTH * 0.16,
        }
    }
}

const NAME_MIN_WIDTH: f32 = 90.0;
const MIN_MODIFIED_WIDTH: f32 = 64.0;
const MIN_SIZE_WIDTH: f32 = 36.0;
const MIN_OWNER_WIDTH: f32 = 44.0;
const MIN_PERMISSIONS_WIDTH: f32 = 72.0;
/// A divider only ever moves by this many pixels per step.
const COLUMN_STEP: f32 = 24.0;

const COLUMN_MINS: [f32; 5] = [
    NAME_MIN_WIDTH,
    MIN_MODIFIED_WIDTH,
    MIN_SIZE_WIDTH,
    MIN_OWNER_WIDTH,
    MIN_PERMISSIONS_WIDTH,
];

impl SftpColumns {
    /// Pixel widths in column order.
    fn widths(self) -> [f32; 5] {
        [
            self.name,
            self.modified,
            self.size,
            self.owner,
            self.permissions,
        ]
        .map(|w| w.max(0.0))
    }

    fn from_widths(widths: [f32; 5]) -> Self {
        Self {
            name: widths[0],
            modified: widths[1],
            size: widths[2],
            owner: widths[3],
            permissions: widths[4],
        }
    }

    /// Move the divider left of column `boundary` to `offset` pixels from the
    /// table's left edge.
    ///
    /// The pointer is only advisory: the divider jumps to the nearest whole
    /// `COLUMN_STEP` from its current position, and only the two neighbours
    /// change, so every other column keeps its width.
    fn drag_divider(&mut self, boundary: usize, offset: f32) {
        if !(1..5).contains(&boundary) {
            return;
        }
        let widths = self.widths();
        let (left, right) = (boundary - 1, boundary);
        if widths[left] <= 0.0 || widths[right] <= 0.0 {
            return;
        }
        let current: f32 = widths[..boundary].iter().sum();
        // How far the divider may travel before either neighbour drops below
        // its minimum, expressed in whole steps so the grid is never broken.
        let left_slack = (widths[left] - COLUMN_MINS[left]).max(0.0);
        let right_slack = (widths[right] - COLUMN_MINS[right]).max(0.0);
        let min_steps = -(left_slack / COLUMN_STEP).floor();
        let max_steps = (right_slack / COLUMN_STEP).floor();
        if min_steps > max_steps {
            return;
        }
        let steps = ((offset - current) / COLUMN_STEP)
            .round()
            .clamp(min_steps, max_steps);
        let delta = steps * COLUMN_STEP;
        if delta == 0.0 {
            return;
        }
        let mut next = widths;
        next[left] += delta;
        next[right] -= delta;
        *self = Self::from_widths(next);
    }
}

/// Rectangles of the four columns in one table row, in column order.
struct SftpLayout {
    name: egui::Rect,
    modified: egui::Rect,
    size: egui::Rect,
    owner: egui::Rect,
    permissions: egui::Rect,
}

impl SftpLayout {
    fn new(rect: egui::Rect, columns: SftpColumns) -> Self {
        let widths = columns.widths();
        let mut x = rect.left();
        let mut cell = |width: f32| {
            let column = egui::Rect::from_x_y_ranges(x..=(x + width), rect.y_range());
            x += width;
            column
        };
        Self {
            name: cell(widths[0]),
            modified: cell(widths[1]),
            size: cell(widths[2]),
            owner: cell(widths[3]),
            permissions: cell(widths[4]),
        }
    }
}

/// Two compact lines describing the selected entry.
///
/// The table columns stay scannable; anything that would not fit there (the
/// full path, the exact timestamps, the entry type) lives here.
fn sftp_details_bar(ui: &mut egui::Ui, theme: &Theme, entry: &RemoteEntry, link: Option<&str>) {
    let mut second = String::new();
    if entry.is_dir {
        second.push_str("folder");
    } else {
        second.push_str(&format_size(entry.size));
    }
    if entry.is_symlink {
        match link {
            Some(target) => second.push_str(&format!(" → {target}")),
            None => second.push_str(" · symlink"),
        }
    }
    second.push_str(&format!(" · owner {}", format_owner(entry)));
    second.push_str(&format!(" · {}", format_timestamp(entry.modified)));
    if let Some(modified) = entry.modified {
        second.push_str(&format!(" ({})", format_age(modified)));
    }

    let font = theme::body_font(theme);
    let body = |ui: &mut egui::Ui, text: &str, color: egui::Color32| {
        ui.add(
            egui::Label::new(egui::RichText::new(text).font(font.clone()).color(color)).truncate(),
        )
        .on_hover_text(text);
    };
    body(ui, &entry.path, theme.palette.text);
    body(ui, &second, theme.palette.muted);
}

/// Two compact lines summarising a multi-row selection.
fn sftp_multi_details_bar(ui: &mut egui::Ui, theme: &Theme, entries: &[RemoteEntry]) {
    let folders = entries.iter().filter(|entry| entry.is_dir).count();
    let files = entries.len() - folders;
    let bytes: u64 = entries
        .iter()
        .filter(|entry| !entry.is_dir)
        .map(|entry| entry.size)
        .sum();
    let mut second = format!("{files} files · {}", format_size(bytes));
    if folders > 0 {
        second = format!("{folders} folders · {second}");
    }

    let font = theme::body_font(theme);
    let body = |ui: &mut egui::Ui, text: &str, color: egui::Color32| {
        ui.add(
            egui::Label::new(egui::RichText::new(text).font(font.clone()).color(color)).truncate(),
        )
        .on_hover_text(text);
    };
    body(
        ui,
        &format!("{} items selected", entries.len()),
        theme.palette.text,
    );
    body(ui, &second, theme.palette.muted);
}

/// Latest SFTP message, operation state, queue depth and message history.
fn sftp_status_bar(
    ui: &mut egui::Ui,
    theme: &Theme,
    explorer: &mut SftpExplorer,
    ssh: &SshSession,
) -> bool {
    let mut activity = false;
    let busy = explorer.busy_label().map(str::to_string);
    let queued = explorer.queued_transfers();
    let current = explorer.status.current().cloned();

    ui.horizontal(|ui| {
        if let Some(label) = busy {
            ui.spinner();
            // Queue depth rides along with the status text instead of living in
            // the transfer bar, where it could overlap the progress widgets.
            let text = if queued > 0 {
                format!("{label} (+{queued} queued)")
            } else {
                label
            };
            ui.add(
                egui::Label::new(egui::RichText::new(&text).small().color(theme.palette.text))
                    .truncate(),
            )
            .on_hover_text(text);
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
                theme::menu_style(ui);
                ui.label(egui::RichText::new("Session messages").strong());
                ui.add_space(2.0);
                // Cap the list so a long session cannot grow the menu past the
                // bottom of the window.
                egui::ScrollArea::vertical()
                    .max_height(320.0)
                    .show(ui, |ui| {
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
                    });
                ui.separator();
                if theme::menu_item(ui, "Clear messages", true, None).clicked() {
                    explorer.status.clear();
                    ui.close_menu();
                }
            });
            response.response.on_hover_text("Message history");
            if theme::icon_button(
                ui,
                theme,
                theme::icons::RECONNECT,
                "Reconnect SFTP without dropping the terminal",
                !explorer.is_busy(),
            )
            .clicked()
            {
                explorer.reconnect(ssh);
                activity = true;
            }
        });
    });
    activity
}

/// Running transfer: byte bar, byte count, file count and cancel.
///
/// A separate fixed-height row keeps the progress widgets stable; queue depth
/// lives in the status bar above so it cannot overlap them.
fn sftp_transfer_bar(ui: &mut egui::Ui, theme: &Theme, explorer: &mut SftpExplorer) {
    let info = explorer.transfer_progress().map(|progress| {
        (
            progress.fraction(),
            progress.bytes_done(),
            progress.total_bytes(),
            progress.items_done(),
            progress.total_items(),
            progress.is_cancelled(),
        )
    });

    ui.horizontal(|ui| {
        let Some((fraction, done, total, items_done, items_total, cancelled)) = info else {
            return;
        };
        if cancelled {
            ui.label(
                egui::RichText::new("cancelling…")
                    .small()
                    .color(theme.palette.warning),
            );
        } else if let Some(fraction) = fraction {
            ui.add(
                egui::ProgressBar::new(fraction)
                    .desired_width(110.0)
                    .desired_height(8.0)
                    .fill(theme.palette.accent),
            );
            let size = match total {
                Some(total) => format!("{} / {}", format_size(done), format_size(total)),
                None => format_size(done),
            };
            ui.label(egui::RichText::new(size).small().color(theme.palette.muted));
        } else {
            ui.spinner();
            ui.label(
                egui::RichText::new(format_size(done))
                    .small()
                    .color(theme.palette.muted),
            );
        }
        if let Some(items_total) = items_total {
            ui.label(
                egui::RichText::new(format!("{items_done}/{items_total} files"))
                    .small()
                    .color(theme.palette.muted),
            );
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if theme::icon_button(
                ui,
                theme,
                theme::icons::CLOSE,
                "Cancel this transfer",
                !cancelled,
            )
            .clicked()
            {
                explorer.cancel_transfer();
            }
        });
    });
    // Progress is polled, not pushed: keep frames coming while a transfer runs.
    ui.ctx().request_repaint_after(Duration::from_millis(100));
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

fn local_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn default_download_directory() -> PathBuf {
    let home = local_home();
    let downloads = home.join("Downloads");
    if downloads.is_dir() {
        downloads
    } else {
        home
    }
}

fn pick_download_folder() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("Download to folder")
        .set_directory(default_download_directory())
        .pick_folder()
}

fn pick_upload_files() -> Option<Vec<PathBuf>> {
    rfd::FileDialog::new()
        .set_title("Upload files")
        .set_directory(local_home())
        .pick_files()
}

fn pick_upload_folder() -> Option<Vec<PathBuf>> {
    rfd::FileDialog::new()
        .set_title("Upload folder")
        .set_directory(local_home())
        .pick_folder()
        .map(|path| vec![path])
}

/// Which column the SFTP table is ordered by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum SftpSortKey {
    #[default]
    Name,
    Modified,
    Size,
    Owner,
    Permissions,
}

/// Current sort of the SFTP table, kept per session.
#[derive(Debug, Clone, Copy)]
struct SftpSort {
    key: SftpSortKey,
    ascending: bool,
}

impl Default for SftpSort {
    fn default() -> Self {
        Self {
            key: SftpSortKey::Name,
            ascending: true,
        }
    }
}

impl SftpSort {
    /// Clicking a header selects it, or flips the direction if already active.
    fn toggle(&mut self, key: SftpSortKey) {
        if self.key == key {
            self.ascending = !self.ascending;
        } else {
            self.key = key;
            self.ascending = true;
        }
    }

    fn arrow(&self) -> &'static str {
        if self.ascending {
            theme::icons::SORT_ASC
        } else {
            theme::icons::SORT_DESC
        }
    }
}

/// Case-insensitive ordering without allocating: `to_lowercase()` in a
/// comparator allocated twice per comparison, which is O(n log n) allocations
/// on every frame. `flat_map(char::to_lowercase)` produces the same sequence
/// of characters without building a string.
fn cmp_lower(a: &str, b: &str) -> std::cmp::Ordering {
    a.chars()
        .flat_map(char::to_lowercase)
        .cmp(b.chars().flat_map(char::to_lowercase))
}

/// Indices of `entries` in display order: hidden files filtered out, folders
/// first, then the active sort key.
fn sorted_indices(entries: &[RemoteEntry], show_hidden: bool, sort: SftpSort) -> Vec<usize> {
    log::trace!("sorted_indices: {} entries", entries.len());
    let mut indices: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| show_hidden || !entry.name.starts_with('.'))
        .map(|(index, _)| index)
        .collect();
    indices.sort_by(|&a, &b| {
        let (a, b) = (&entries[a], &entries[b]);
        // Folders always stay grouped above files, whatever the key.
        b.is_dir.cmp(&a.is_dir).then_with(|| {
            let order = match sort.key {
                SftpSortKey::Name => cmp_lower(&a.name, &b.name),
                SftpSortKey::Modified => a.modified.cmp(&b.modified),
                SftpSortKey::Size => a.size.cmp(&b.size),
                SftpSortKey::Owner => cmp_lower(&a.owner, &b.owner),
                SftpSortKey::Permissions => a.permissions.cmp(&b.permissions),
            };
            let order = if sort.ascending {
                order
            } else {
                order.reverse()
            };
            // Stable, predictable order for equal keys.
            order.then_with(|| cmp_lower(&a.name, &b.name))
        })
    });
    indices
}

/// Draw the SFTP column header and its resize handles.
///
/// The handles live only in the header row, never on the file rows. Each one
/// adjusts the column that starts at its divider, so the divider always
/// follows the pointer. Clicking a column name sorts by it. Returns `true` when
/// the sort changed.
fn sftp_header(
    ui: &mut egui::Ui,
    theme: &Theme,
    columns: &mut SftpColumns,
    sort: &mut SftpSort,
    index: usize,
    rect: egui::Rect,
    width: f32,
) -> bool {
    let rect = egui::Rect::from_min_size(rect.min, egui::vec2(width, rect.height()));
    let layout = SftpLayout::new(rect, *columns);
    let font = theme::body_font(theme);
    let painter = ui.painter().clone();
    painter.rect_filled(rect, egui::Rounding::ZERO, theme.palette.panel);

    let mut sort_changed = false;
    // (cell rect, left inset, sort key, label). The inset keeps the label clear
    // of the type icon and of the divider handles.
    for (cell, inset, key, label) in [
        (layout.name, 22.0, SftpSortKey::Name, "Name"),
        (layout.modified, 6.0, SftpSortKey::Modified, "Modified"),
        (layout.size, 6.0, SftpSortKey::Size, "Size"),
        (layout.owner, 6.0, SftpSortKey::Owner, "Owner"),
        (
            layout.permissions,
            6.0,
            SftpSortKey::Permissions,
            "Permissions",
        ),
    ] {
        if cell.width() <= inset + 4.0 {
            continue;
        }
        let label_rect = egui::Rect::from_min_max(
            egui::pos2(cell.left() + inset, cell.top()),
            egui::pos2(cell.right() - 2.0, cell.bottom()),
        );
        let active = sort.key == key;
        let text = if active {
            format!("{label} {}", sort.arrow())
        } else {
            label.to_string()
        };
        let align = if key == SftpSortKey::Size {
            egui::Align::Max
        } else {
            egui::Align::Min
        };
        paint_cell(
            ui,
            label_rect,
            &text,
            font.clone(),
            if active {
                theme.palette.text
            } else {
                theme.palette.muted
            },
            align,
        );

        let response = ui.interact(
            label_rect,
            egui::Id::new(("sftp_sort", index, label)),
            egui::Sense::click(),
        );
        if response
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .clicked()
        {
            sort.toggle(key);
            sort_changed = true;
        }
        if active {
            painter.hline(
                cell.x_range(),
                rect.bottom() - 1.0,
                egui::Stroke::new(1.0_f32, theme.palette.border_strong),
            );
        }
    }

    // A divider is only drawn — and only draggable — in front of a column that
    // is currently visible. `boundary` is the column index it starts.
    let mut separators: Vec<(usize, f32)> = Vec::new();
    for (boundary, column) in [
        (1_usize, layout.modified),
        (2, layout.size),
        (3, layout.owner),
        (4, layout.permissions),
    ] {
        if column.width() > 0.0 {
            separators.push((boundary, column.left()));
            painter.vline(
                column.left(),
                rect.y_range(),
                egui::Stroke::new(1.0_f32, theme.palette.border),
            );
        }
    }
    painter.hline(
        rect.x_range(),
        rect.bottom(),
        egui::Stroke::new(1.0_f32, theme.palette.border_strong),
    );

    for (boundary, x) in separators {
        let handle = egui::Rect::from_min_max(
            egui::pos2(x - 3.0, rect.top()),
            egui::pos2(x + 3.0, rect.bottom()),
        );
        let response = ui.interact(
            handle,
            egui::Id::new(("sftp_col_resize", index, boundary)),
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
        // Snapshots to fixed steps, so the divider jumps one notch at a time.
        columns.drag_divider(boundary, pointer.x - rect.left());
    }

    sort_changed
}

pub struct RustTermApp {
    sessions: Vec<ActiveSession>,
    /// Edit journals detached from closed tabs, waiting for a session with the
    /// same profile so unsynced local changes can be uploaded.
    detached_edits: Vec<EditJournal>,
    /// 0 = New Session form; `i > 0` = `sessions[i - 1]`.
    active: usize,
    form: ConnectForm,
    connecting: Option<ConnectingState>,
    /// A reconnect requested from inside a terminal tab's bottom bar; processed
    /// in `update` once the borrow ends.
    pending_reconnect: Option<(usize, bool)>,
    /// Persisted profiles loaded at startup (F19: no passwords stored).
    store: SessionStore,
    /// Name used when "Save as profile" is checked.
    profile_name: String,
    /// Whether Connect should also store the form as a profile.
    save_as_profile: bool,
    status: StatusMessage,
    /// Whether the SFTP side panel is shown for connected sessions.
    show_sftp: bool,
    /// Persisted preferences: theme, panel history and remote display.
    settings: Settings,
    /// Whether the Settings window is open.
    settings_open: bool,
}

/// One line of user feedback below the connect form.
#[derive(Default)]
struct StatusMessage {
    text: String,
    is_error: bool,
}

struct ActiveSession {
    title: String,
    /// Connection parameters used to open this session; kept so a reconnect
    /// can re-open it with the same credentials (and a different X11 flag).
    config: SshConfig,
    ssh: SshSession,
    terminal: TerminalView,
    /// In-flight reconnect attempt, if any.
    reconnect: Option<ConnectingState>,
    /// Per-session remote file browser. Each tab keeps its own directory.
    sftp: SftpExplorer,
    /// Resized column widths of the file table.
    sftp_columns: SftpColumns,
    /// Sort order of the file table.
    sftp_sort: SftpSort,
    /// Remote-display state (X11 status is read from `ssh`; VNC is here).
    display: DisplayUi,
    /// Set once so the terminal takes focus on first appearance.
    focus_requested: bool,
    /// Set when the session thread exits; the tab then shows a closed banner
    /// instead of a stale, unresponsive terminal.
    connection_lost: bool,
}

/// Which remote-display path the user chose for this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplayChoice {
    X11,
    Vnc,
}

/// Per-session VNC probe/tunnel state shown in the bottom bar.
#[derive(Default)]
struct DisplayUi {
    /// The Ask preference is showing its small chooser.
    ask: bool,
    /// A probe or tunnel request is in flight.
    busy: bool,
    probe: Option<std_mpsc::Receiver<Result<VncProbe, String>>>,
    open: Option<std_mpsc::Receiver<Result<u16, String>>>,
    /// Local loopback port the external viewer connects to.
    tunnel_port: Option<u16>,
    /// VNC server the tunnel points at.
    target: Option<VncTarget>,
    /// Login shell found by the probe, which decides the `DISPLAY` syntax.
    shell: Option<ShellKind>,
    message: Option<String>,
    /// External viewer process, kept so Stop can close it.
    viewer: Option<std::process::Child>,
    /// When the viewer process was last seen exiting. The tunnel is stopped
    /// only after [`VNC_TUNNEL_GRACE`] with no connected client, so a viewer
    /// that hands off to an existing instance is not cut off.
    viewer_exited_at: Option<Instant>,
}

impl Drop for DisplayUi {
    fn drop(&mut self) {
        // Closing the tab or the app must not leave an orphaned viewer.
        if let Some(mut child) = self.viewer.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct ConnectingState {
    rx: std_mpsc::Receiver<Result<SshSession, String>>,
    prompt_rx: std_mpsc::Receiver<HostKeyPrompt>,
    /// Taken out to draw the modal, put back if the user has not answered yet.
    prompt: Option<HostKeyPrompt>,
    label: String,
    config: SshConfig,
    /// Fires to abort the attempt at any await point in `SshSession::connect`.
    cancel_tx: Option<tokio::sync::oneshot::Sender<()>>,
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
            detached_edits: Vec::new(),
            active: NEW_SESSION_TAB,
            form: ConnectForm::default(),
            connecting: None,
            pending_reconnect: None,
            store,
            profile_name: String::new(),
            save_as_profile: false,
            status,
            show_sftp: true,
            settings,
            settings_open: false,
        }
    }
}

impl RustTermApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let app = Self::default();
        theme::apply(&cc.egui_ctx, app.settings.theme.theme());
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
            x11: self.settings.x11,
        };
        let label = format!("{}@{}:{}", config.user, config.host, config.port);

        let config_for_state = config.clone();

        // The SSH thread wakes egui as soon as output arrives. Without this,
        // remote echo on a high-latency link was only displayed on the next
        // keyboard/mouse event (the previous 8 ms polling tail had long expired).
        let wake: Wake = debounced_wake(ctx.clone());

        let (tx, rx) = std_mpsc::channel();
        let (prompt_tx, prompt_rx) = std_mpsc::channel();
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let result = SshSession::connect(config, Some(prompt_tx), Some(wake), Some(cancel_rx))
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });

        self.connecting = Some(ConnectingState {
            rx,
            prompt_rx,
            prompt: None,
            label,
            config: config_for_state,
            cancel_tx: Some(cancel_tx),
        });
        self.clear_status();
        self.active = NEW_SESSION_TAB;
    }

    /// Abort an in-flight connection attempt.
    ///
    /// The UI hides the "Connecting…" state immediately; the worker thread
    /// notices the cancel signal at its next await and drops the socket.
    fn cancel_connect(&mut self) {
        if let Some(mut state) = self.connecting.take() {
            if let Some(tx) = state.cancel_tx.take() {
                let _ = tx.send(());
            }
            self.set_info("Connection cancelled");
        }
    }

    /// Reconnect one session, optionally enabling X11 forwarding.
    ///
    /// Keeps the tab and its title; on success the new SSH session replaces
    /// the old one, the terminal's input channel is swapped and a divider is
    /// written into the scrollback, and SFTP restarts at the home directory.
    fn begin_reconnect(&mut self, index: usize, ctx: &egui::Context, enable_x11: bool) {
        let Some(session) = self.sessions.get(index) else {
            return;
        };
        if session.reconnect.is_some() {
            return; // one reconnect at a time per tab
        }
        let mut config = session.config.clone();
        config.x11 = enable_x11 || self.settings.x11;
        let label = session.title.clone();

        let wake: Wake = debounced_wake(ctx.clone());
        let (tx, rx) = std_mpsc::channel();
        let (_prompt_tx, prompt_rx) = std_mpsc::channel();
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let result = SshSession::connect(config, None, Some(wake), Some(cancel_rx))
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });

        let session = &mut self.sessions[index];
        session.reconnect = Some(ConnectingState {
            rx,
            prompt_rx,
            prompt: None,
            label,
            config: session.config.clone(),
            cancel_tx: Some(cancel_tx),
        });
        session.display.message = Some("Reconnecting…".to_string());
    }

    /// Poll an in-flight reconnect attempt for one session.
    fn poll_reconnecting(&mut self, index: usize) -> bool {
        // First, surface a host-key prompt if one arrived.
        if let Some(state) = self.sessions[index].reconnect.as_mut() {
            if state.prompt.is_none() {
                if let Ok(prompt) = state.prompt_rx.try_recv() {
                    state.prompt = Some(prompt);
                    return true;
                }
            }
        }

        enum Polled {
            Pending,
            Ready(Result<SshSession, String>),
            Gone,
        }
        let polled = match self.sessions[index].reconnect.as_ref() {
            None => return false,
            Some(state) => match state.rx.try_recv() {
                Ok(r) => Polled::Ready(r),
                Err(std_mpsc::TryRecvError::Empty) => Polled::Pending,
                Err(std_mpsc::TryRecvError::Disconnected) => Polled::Gone,
            },
        };

        match polled {
            Polled::Pending => false,
            Polled::Gone => {
                if let Some(s) = self.sessions.get_mut(index) {
                    s.reconnect = None;
                    s.display.message = Some("Reconnect failed: worker thread died".to_string());
                }
                true
            }
            Polled::Ready(Ok(new_ssh)) => {
                let input_tx = new_ssh.input_sender();
                let state = self.sessions[index]
                    .reconnect
                    .take()
                    .expect("checked above");
                let session = &mut self.sessions[index];
                session.ssh = new_ssh;
                session.terminal.set_input_tx(input_tx);
                session
                    .terminal
                    .feed(b"\r\n\r\n--- reconnected ---\r\n\r\n");
                session.connection_lost = false;
                // Keep unsynced editor copies: hand the old journal to the new
                // explorer. Use the journal's own profile key because the tab
                // title may have been changed by the remote shell's OSC title.
                let journal = session.sftp.take_journal();
                let profile = journal.profile().to_string();
                session.sftp = SftpExplorer::start(&session.ssh, &profile);
                session.sftp.install_journal(journal);
                session.display.message = Some("Reconnected".to_string());
                // Keep the stored config's x11 flag in sync with what was used.
                session.config.x11 = state.config.x11;
                true
            }
            Polled::Ready(Err(err)) => {
                if let Some(s) = self.sessions.get_mut(index) {
                    s.reconnect = None;
                    s.display.message = Some(format!("Reconnect failed: {err}"));
                }
                true
            }
        }
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
                let mut sftp = SftpExplorer::start(&ssh, &state.label);
                // A new tab to the same host picks up edits left unsynced by a
                // dropped session.
                if let Some(pos) = self
                    .detached_edits
                    .iter()
                    .position(|journal| journal.profile() == state.label)
                {
                    sftp.install_journal(self.detached_edits.remove(pos));
                }
                self.sessions.push(ActiveSession {
                    title: state.label,
                    config: state.config,
                    ssh,
                    terminal: TerminalView::new(DEFAULT_COLS, DEFAULT_ROWS, input_tx),
                    reconnect: None,
                    sftp,
                    sftp_columns: SftpColumns::default(),
                    sftp_sort: SftpSort::default(),
                    display: DisplayUi::default(),
                    focus_requested: false,
                    connection_lost: false,
                });
                // New session is the last tab; make it active.
                self.active = self.sessions.len();
                self.form.password.clear();
                true
            }
            Polled::Ready(Err(err)) => {
                self.connecting = None;
                log::warn!("connect failed: {err}");
                self.set_error(format!(
                    "Connect failed: {}",
                    crate::ssh::explain_connect_error(&err)
                ));
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

        let theme = self.settings.theme.theme();
        egui::Window::new("Unknown host key")
            .id(egui::Id::new("host_key_prompt"))
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .default_size(theme::dialog_size(theme))
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .frame(theme::modal_frame(theme))
            .show(ctx, |ui| {
                ui.set_width(theme::dialog_inner_width(theme));
                ui.horizontal(|ui| {
                    theme::modal_heading(ui, theme, "Unknown host key");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if theme::modal_close(ui) {
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
        let mut removed = self.sessions.remove(i);
        removed.ssh.disconnect();
        // Keep any unsynced editor copies for a later session to the same host.
        let journal = removed.sftp.take_journal();
        if !journal.is_empty() {
            self.detached_edits.push(journal);
        }

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
    /// At most [`MAX_OUTPUT_PER_FRAME`] bytes are decoded per session per
    /// frame, starting with the visible tab. A huge burst (`cat` of a large
    /// file) would otherwise block painting for hundreds of milliseconds; the
    /// remainder is picked up on the next frame and a repaint is requested so
    /// it is not left waiting for a wake-up. Draining the visible tab first
    /// means a flooding background tab cannot starve it.
    fn pump_sessions(&mut self, ctx: &egui::Context) -> bool {
        let mut activity = false;
        let mut exhausted = false;
        // File-save events arrive on a watcher thread; give each panel a way to
        // wake egui instead of relying on a polling timer.
        let wake: Wake = debounced_wake(ctx.clone());

        // Journals from closed tabs are only useful for a short while.
        let now = Instant::now();
        let had_detached = !self.detached_edits.is_empty();
        self.detached_edits.retain_mut(|journal| {
            for name in journal.expire(now) {
                log::info!("dropped stale edit journal for {name}");
            }
            !journal.is_empty()
        });
        if had_detached && !self.detached_edits.is_empty() {
            // Wake up occasionally so an idle app still expires old entries.
            ctx.request_repaint_after(Duration::from_secs(60));
        }

        let count = self.sessions.len();
        let first = self.active.checked_sub(1).filter(|index| *index < count);
        for offset in 0..count {
            let index = match first {
                Some(first) => (first + offset) % count,
                None => offset,
            };
            let session = &mut self.sessions[index];
            let mut budget = MAX_OUTPUT_PER_FRAME;
            // A dead thread used to be invisible: reads returned `None`, input
            // vanished and the tab looked alive forever. Detect it once here.
            if !session.connection_lost && !session.ssh.is_alive() {
                session.connection_lost = true;
                session.sftp.mark_closed();
                activity = true;
            }
            // SFTP replies arrive asynchronously; polling them for every tab
            // keeps a listing ready even if the sidebar is currently hidden.
            if !session.connection_lost {
                session.sftp.attach_wake(wake.clone());
                if session.sftp.poll(&session.ssh) {
                    activity = true;
                }
                if session.sftp.transfer.is_some() {
                    // The bar is polled; keep frames coming while bytes flow.
                    ctx.request_repaint_after(Duration::from_millis(100));
                }
                if session.sftp.pending_edit_settle() {
                    // An event arrived; wake once the debounce has elapsed so
                    // the upload does not wait for the slow safety pass.
                    ctx.request_repaint_after(Duration::from_millis(EDIT_DEBOUNCE_MS));
                }
                if session.sftp.watches_edits() {
                    // Save events wake egui through the watcher itself; this
                    // slow tick is only the safety pass.
                    ctx.request_repaint_after(Duration::from_millis(EDIT_SAFETY_MS));
                }
            }
            while budget > 0 {
                let Some(chunk) = session.ssh.try_read() else {
                    break;
                };
                budget = budget.saturating_sub(chunk.len());
                session.terminal.feed(&chunk);
                activity = true;
            }
            if budget == 0 {
                exhausted = true;
            }
            // The shell reports its directory through OSC 7; following is
            // decided in the explorer, which also ignores it while paused.
            if let Some(cwd) = session.terminal.take_cwd() {
                if !session.connection_lost {
                    session.sftp.follow_shell_cwd(&session.ssh, &cwd);
                    activity = true;
                }
            }
            if let Some(title) = session.terminal.take_title() {
                session.title = title;
                activity = true;
            }
            if session.terminal.take_bell() {
                log::info!("terminal bell");
            }
        }

        if exhausted {
            // Work was left over: keep painting until every queue is empty.
            ctx.request_repaint();
        }

        activity
    }

    fn tabs_ui(&mut self, ui: &mut egui::Ui) {
        let theme = self.settings.theme.theme();
        let mut switch_to: Option<usize> = None;
        let mut close: Option<usize> = None;

        ui.horizontal(|ui| {
            // The theme picker is drawn first in a right-to-left layout so it
            // stays pinned to the far edge no matter how many tabs are open.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .selectable_label(self.settings_open, "Settings")
                    .on_hover_text("Appearance, file panel and remote display")
                    .clicked()
                {
                    self.settings_open = !self.settings_open;
                }
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

    /// Dedicated Settings window: appearance, file panel and remote display.
    fn settings_window_ui(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let theme = self.settings.theme.theme();
        let before = self.settings.clone();
        let mut close = false;

        egui::Window::new("Settings")
            .id(egui::Id::new("settings_window"))
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .default_size(egui::vec2(theme::DIALOG_WIDTH + 40.0, 0.0))
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .frame(theme::modal_frame(theme))
            .show(ctx, |ui| {
                ui.set_width(theme::dialog_inner_width(theme) + 40.0);
                ui.horizontal(|ui| {
                    theme::modal_heading(ui, theme, "Settings");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if theme::modal_close(ui) {
                            close = true;
                        }
                    });
                });
                ui.add_space(theme.metrics.pad);

                ui.label(egui::RichText::new("Appearance").strong());
                ui.horizontal_wrapped(|ui| {
                    for id in ThemeId::ALL {
                        if ui
                            .selectable_label(self.settings.theme == id, id.name())
                            .clicked()
                        {
                            self.settings.theme = id;
                        }
                    }
                });

                ui.add_space(theme.metrics.card_padding);
                ui.separator();
                ui.add_space(theme.metrics.card_padding);

                ui.label(egui::RichText::new("File panel").strong());
                ui.checkbox(
                    &mut self.settings.sftp_history,
                    "Show Back/Forward history buttons",
                );

                ui.add_space(theme.metrics.card_padding);
                ui.separator();
                ui.add_space(theme.metrics.card_padding);

                ui.label(egui::RichText::new("Remote display").strong());
                theme::muted(
                    ui,
                    theme,
                    "Both are off by default. A session checks the server before offering either.",
                );
                ui.checkbox(&mut self.settings.x11, "X11 forwarding (ssh -X)");
                ui.checkbox(&mut self.settings.vnc, "VNC over SSH");
                if self.settings.x11 && self.settings.vnc {
                    ui.add_space(4.0);
                    for pref in DisplayPref::ALL {
                        ui.radio_value(&mut self.settings.display_pref, pref, pref.label());
                    }
                    theme::muted(ui, theme, self.settings.display_pref.hint());
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label("VNC server");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.vnc_host)
                            .hint_text("detect in shell, else host[:port]")
                            .desired_width(220.0),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label("VNC port");
                    ui.add(egui::DragValue::new(&mut self.settings.vnc_port).range(1..=65535));
                    theme::muted(ui, theme, "5900 is display :0");
                });
                if self.settings.vnc {
                    ui.checkbox(
                        &mut self.settings.vnc_auto_create,
                        "Create a VNC session if none exists",
                    );
                    theme::muted(
                        ui,
                        theme,
                        "Runs `vncserver` on the server when no session is found.",
                    );
                }
            });

        if close {
            self.settings_open = false;
        }
        if self.settings != before {
            if self.settings.theme != before.theme {
                theme::apply(ctx, self.settings.theme.theme());
            }
            if let Err(err) = self.settings.save_default() {
                log::warn!("could not save settings: {err}");
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

                        if theme::icon_button(
                            ui,
                            theme,
                            theme::icons::CLOSE,
                            "Delete this profile",
                            true,
                        )
                        .clicked()
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
        let theme = self.settings.theme.theme();
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
                                let mut cancel_clicked = false;
                                let connecting_label = self.connecting.as_ref().map(|state| {
                                    if state.prompt.is_some() {
                                        "Waiting for host key confirmation…".to_string()
                                    } else {
                                        format!("Connecting to {}…", state.label)
                                    }
                                });
                                ui.horizontal(|ui| {
                                    let button = theme::primary_button(theme, "Connect")
                                        .min_size(egui::vec2(104.0, control));
                                    if ui.add_enabled(!connecting, button).clicked() {
                                        connect_clicked = true;
                                    }
                                    if let Some(label) = connecting_label {
                                        ui.spinner();
                                        theme::muted(ui, theme, label);
                                        if theme::icon_button(
                                            ui,
                                            theme,
                                            theme::icons::CLOSE,
                                            "Cancel this connection attempt",
                                            true,
                                        )
                                        .clicked()
                                        {
                                            cancel_clicked = true;
                                        }
                                    }
                                });
                                if cancel_clicked {
                                    self.cancel_connect();
                                }

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
        let theme = self.settings.theme.theme();
        let show_history = self.settings.sftp_history;
        let Some(session) = self.sessions.get_mut(index) else {
            return false;
        };
        // "Enable now" types into the terminal, so it is only safe at a prompt.
        let at_prompt = !session.terminal.is_alt_screen();
        // Split the borrow: the explorer mutates while the session only answers
        // requests.
        let (ssh, explorer) = (&session.ssh, &mut session.sftp);
        let columns = &mut session.sftp_columns;
        let sort = &mut session.sftp_sort;
        let mut activity = false;

        // Never let a wide child grow the panel's stored width.
        let content_width = ui.available_width();
        ui.set_max_width(content_width);
        ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);

        let busy = explorer.is_busy();
        let selected = explorer.selected_entry().cloned();
        let selected_count = explorer.selection_count();
        let can_up = !explorer.current.is_empty() && explorer.current != "/";
        let can_copy = selected.is_some() && !busy;
        let can_paste = explorer.clipboard.is_some() && !busy;
        let download_tooltip = if selected_count > 1 {
            format!("Download the {selected_count} selected entries…")
        } else {
            "Download the selected entry…".to_string()
        };

        ui.horizontal_wrapped(|ui| {
            if theme::icon_button(
                ui,
                theme,
                theme::icons::HOME,
                "Go to the login directory",
                !busy,
            )
            .clicked()
            {
                explorer.go_home(ssh);
                activity = true;
            }
            if theme::icon_button(ui, theme, theme::icons::UP, "Up one level", can_up && !busy)
                .clicked()
            {
                explorer.up(ssh);
                activity = true;
            }
            if theme::icon_button(
                ui,
                theme,
                theme::icons::REFRESH,
                "Refresh this directory",
                !busy,
            )
            .clicked()
            {
                explorer.refresh(ssh);
                activity = true;
            }
            if theme::icon_button(
                ui,
                theme,
                theme::icons::NEW_FOLDER,
                "Create a folder here",
                !busy,
            )
            .clicked()
            {
                explorer.begin_new_folder();
                activity = true;
            }
            if theme::icon_button(
                ui,
                theme,
                theme::icons::COPY,
                "Copy the selected entry",
                can_copy,
            )
            .clicked()
            {
                if let Some(entry) = &selected {
                    explorer.copy_entry(entry);
                }
                activity = true;
            }
            if theme::icon_button(
                ui,
                theme,
                theme::icons::PASTE,
                "Paste the copied entry here",
                can_paste,
            )
            .clicked()
            {
                explorer.paste(ssh);
                activity = true;
            }
            if theme::icon_button(
                ui,
                theme,
                theme::icons::DOWNLOAD,
                &download_tooltip,
                selected_count > 0 && !busy,
            )
            .clicked()
            {
                if let Some(dir) = pick_download_folder() {
                    let entries = explorer.selected_entries();
                    explorer.download_entries(ssh, &entries, dir);
                }
                activity = true;
            }
            let upload = theme::icon_menu_button(
                ui,
                theme::icons::UPLOAD,
                "Upload files or folders here…",
                |ui| {
                    theme::menu_style(ui);
                    if theme::menu_item(ui, "Upload files…", true, None).clicked() {
                        if let Some(paths) = pick_upload_files() {
                            explorer.upload_paths(ssh, paths);
                        }
                        ui.close_menu();
                    }
                    if theme::menu_item(ui, "Upload folder…", true, None).clicked() {
                        if let Some(paths) = pick_upload_folder() {
                            explorer.upload_paths(ssh, paths);
                        }
                        ui.close_menu();
                    }
                },
            );
            if upload.clicked() {
                activity = true;
            }
            if theme::icon_toggle(
                ui,
                theme,
                theme::icons::HIDDEN,
                "Show or hide dotfiles",
                explorer.show_hidden,
            )
            .clicked()
            {
                explorer.show_hidden = !explorer.show_hidden;
                if !explorer.show_hidden {
                    // Hidden rows cannot stay selected once they leave the
                    // table.
                    let entries = &explorer.entries;
                    explorer.selected.retain(|&index| {
                        entries.get(index).is_some_and(|e| !e.name.starts_with('.'))
                    });
                    let anchor_hidden = explorer.selection_anchor.is_some_and(|index| {
                        entries
                            .get(index)
                            .is_some_and(|entry| entry.name.starts_with('.'))
                    });
                    if anchor_hidden {
                        explorer.selection_anchor = None;
                    }
                }
                activity = true;
            }
            let follow = theme::icon_toggle(
                ui,
                theme,
                theme::icons::FOLLOW,
                "Follow the shell's folder — right-click to set up tracking",
                explorer.follow,
            );
            follow.context_menu(|ui| {
                theme::menu_style(ui);
                if theme::menu_item(ui, "Set up directory tracking…", true, None).clicked() {
                    if explorer.begin_hook_prompt(ssh) {
                        explorer.dismiss_hook_hint();
                    }
                    ui.close_menu();
                }
            });
            if follow.clicked() {
                let next = !explorer.follow;
                explorer.set_follow(ssh, next);
                activity = true;
            }
        });

        ui.horizontal(|ui| {
            let control = theme.metrics.control_height;
            if show_history {
                if theme::icon_button(
                    ui,
                    theme,
                    theme::icons::BACK,
                    "Back to the previous folder",
                    explorer.can_back() && !busy,
                )
                .clicked()
                {
                    explorer.back(ssh);
                    activity = true;
                }
                if theme::icon_button(
                    ui,
                    theme,
                    theme::icons::FORWARD,
                    "Forward to the next folder",
                    explorer.can_forward() && !busy,
                )
                .clicked()
                {
                    explorer.forward(ssh);
                    activity = true;
                }
            }

            // Reserve room for Go so it stays inside the panel at any width.
            let width = (ui.available_width() - control - 8.0).max(80.0);
            let edit = ui.add_sized(
                [width, control],
                text_field(theme, &mut explorer.path_edit).hint_text("Remote path"),
            );
            // Following waits while the box has focus, so a prompt cannot
            // overwrite a path that is being typed.
            explorer.path_focused = edit.has_focus();
            if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                let path = explorer.path_edit.clone();
                explorer.navigate(ssh, path);
                activity = true;
            }
            if theme::icon_button(ui, theme, theme::icons::GO, "Open this path", true).clicked() {
                let path = explorer.path_edit.clone();
                explorer.navigate(ssh, path);
                activity = true;
            }
        });

        if explorer.wants_hook_hint() {
            egui::Frame::none()
                .fill(theme.palette.surface)
                .stroke(egui::Stroke::new(1.0_f32, theme.palette.border))
                .inner_margin(egui::Margin::symmetric(6.0, 3.0))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("Track the shell's folder?")
                                .size(11.0)
                                .color(theme.palette.muted),
                        );
                        if ui
                            .add_enabled(at_prompt, egui::Button::new("Enable now").small())
                            .on_hover_text(if at_prompt {
                                "At a shell prompt, runs a one-line snippet in this shell. It applies from the next prompt and is not saved."
                            } else {
                                "Not available while a full-screen program is running"
                            })
                            .clicked()
                        {
                            explorer.begin_session_hook(ssh);
                            explorer.dismiss_hook_hint();
                            activity = true;
                        }
                        if ui
                            .add(egui::Button::new("Install…").small())
                            .on_hover_text(
                                "Adds a marked block to your startup file so tracking survives new sessions.",
                            )
                            .clicked()
                        {
                            if explorer.begin_hook_prompt(ssh) {
                                explorer.dismiss_hook_hint();
                            }
                            activity = true;
                        }
                        if ui
                            .add(egui::Button::new(theme::icons::CLOSE).small())
                            .on_hover_text("Dismiss for this session")
                            .clicked()
                        {
                            explorer.dismiss_hook_hint();
                            activity = true;
                        }
                    });
                });
        }

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
                activity |= sftp_status_bar(ui, theme, explorer, ssh);
            });

        if explorer.transfer.is_some() || explorer.queued_transfers() > 0 {
            egui::TopBottomPanel::bottom(egui::Id::new(("sftp_transfer", index)))
                .exact_height(theme.metrics.row_height + 6.0)
                .frame(
                    egui::Frame::none()
                        .fill(theme.palette.surface)
                        .stroke(egui::Stroke::new(1.0_f32, theme.palette.border))
                        .inner_margin(egui::Margin::symmetric(4.0, 2.0)),
                )
                .show_inside(ui, |ui| {
                    sftp_transfer_bar(ui, theme, explorer);
                });
        }

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
            return activity;
        }

        let visible_indices = sorted_indices(&explorer.entries, explorer.show_hidden, *sort);
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
            FollowLink(RemoteEntry),
            Open(Vec<RemoteEntry>),
            Rename(RemoteEntry),
            Delete(Vec<RemoteEntry>),
            CopyPath(RemoteEntry),
            Paste(String),
            Download(Vec<RemoteEntry>),
            UploadHere(String, bool),
        }
        let mut row_action: Option<RowAction> = None;
        let mut select_row: Option<usize> = None;
        let mut toggle_row: Option<usize> = None;
        let mut extend_row: Option<usize> = None;

        // Selected-entry summary. Always reserved so selecting a row never
        // reflows the table under the pointer.
        egui::TopBottomPanel::bottom(egui::Id::new(("sftp_details", index)))
            .exact_height(theme.metrics.row_height * 2.0 + 6.0)
            .frame(
                egui::Frame::none()
                    .fill(theme.palette.surface)
                    .stroke(egui::Stroke::new(1.0_f32, theme.palette.border))
                    .inner_margin(egui::Margin::symmetric(6.0, 2.0)),
            )
            .show_inside(ui, |ui| {
                if let Some(entry) = &selected {
                    let link = explorer.link_target(&entry.path);
                    sftp_details_bar(ui, theme, entry, link);
                } else if selected_count > 1 {
                    sftp_multi_details_bar(ui, theme, &explorer.selected_entries());
                } else {
                    theme::muted(ui, theme, "Select an entry to see its details.");
                }
            });

        let row_height = theme.metrics.row_height;
        let table_width = TABLE_WIDTH;
        let table_output = egui::ScrollArea::horizontal()
            .id_salt(("sftp_hscroll", index))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // The table never reflows: it is always painted at `table_width`
                // and the panel is only the window onto it.
                ui.set_width(table_width);
                let (header_rect, _) = ui
                    .allocate_exact_size(egui::vec2(table_width, row_height), egui::Sense::hover());

                let row_count = visible_indices.len().max(1);
                egui::ScrollArea::vertical()
                    .id_salt(("sftp_scroll", index))
                    .auto_shrink([false, false])
                    .show_rows(ui, row_height, row_count, |ui, range| {
                        for visible_index in range {
                            let (rect, response) = ui.allocate_exact_size(
                                egui::vec2(table_width, row_height),
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
                            let is_selected = explorer.selected.contains(&entry_index);
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
                                egui::pos2(layout.name.right() - 2.0, rect.bottom()),
                            );
                            paint_cell(
                                ui,
                                name_rect,
                                &entry.name,
                                font.clone(),
                                theme.palette.text,
                                egui::Align::Min,
                            );
                            if layout.modified.width() > 0.0 {
                                paint_cell(
                                    ui,
                                    layout.modified.shrink2(egui::vec2(4.0, 0.0)),
                                    &format_timestamp(entry.modified),
                                    font.clone(),
                                    theme.palette.muted,
                                    egui::Align::Min,
                                );
                            }
                            if layout.size.width() > 0.0 {
                                let size = if entry.is_dir {
                                    "—".to_string()
                                } else {
                                    format_size(entry.size)
                                };
                                paint_cell(
                                    ui,
                                    layout.size.shrink2(egui::vec2(4.0, 0.0)),
                                    &size,
                                    font.clone(),
                                    theme.palette.muted,
                                    egui::Align::Max,
                                );
                            }
                            if layout.owner.width() > 0.0 {
                                paint_cell(
                                    ui,
                                    layout.owner.shrink2(egui::vec2(4.0, 0.0)),
                                    format_owner(entry),
                                    font.clone(),
                                    theme.palette.muted,
                                    egui::Align::Min,
                                );
                            }
                            if layout.permissions.width() > 0.0 {
                                paint_cell(
                                    ui,
                                    layout.permissions.shrink2(egui::vec2(4.0, 0.0)),
                                    &format_permissions(entry),
                                    font,
                                    theme.palette.muted,
                                    egui::Align::Min,
                                );
                            }

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
                            let modifiers = ui.input(|i| i.modifiers);
                            if response.clicked() {
                                if modifiers.ctrl || modifiers.command {
                                    toggle_row = Some(entry_index);
                                } else if modifiers.shift {
                                    extend_row = Some(entry_index);
                                } else {
                                    select_row = Some(entry_index);
                                }
                            }
                            if response.double_clicked() {
                                if entry.is_symlink {
                                    row_action = Some(RowAction::FollowLink(entry.clone()));
                                } else if entry.is_dir {
                                    row_action = Some(RowAction::Navigate(entry.path.clone()));
                                } else {
                                    row_action = Some(RowAction::Open(vec![entry.clone()]));
                                }
                            }
                            // A right-click selects the row it landed on
                            // (keeping an existing multi-selection intact) and
                            // opens the menu in the same frame.
                            if response.secondary_clicked() && !is_selected {
                                select_row = Some(entry_index);
                            }
                            response.context_menu(|ui| {
                                // A right-click outside the selection acts on
                                // the clicked row only; inside it, on the whole
                                // selection.
                                let targets: Vec<RemoteEntry> = if is_selected {
                                    explorer.selected_entries()
                                } else {
                                    vec![entry.clone()]
                                };
                                let many = targets.len() > 1;
                                let all_files = targets
                                    .iter()
                                    .all(|entry| !entry.is_dir && !entry.is_symlink);

                                theme::menu_style(ui);
                                // The header is the only variable-length text, so it is
                                // truncated; fixed-width items alone would still let a
                                // long filename widen the whole popup.
                                if many {
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(format!(
                                                "{} items selected",
                                                targets.len()
                                            ))
                                            .color(theme.palette.text),
                                        )
                                        .truncate(),
                                    );
                                } else {
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(&entry.name)
                                                .color(theme.palette.text),
                                        )
                                        .truncate(),
                                    )
                                    .on_hover_text(&entry.path);
                                }
                                ui.separator();
                                if !many && entry.is_symlink {
                                    if theme::menu_item(ui, "Follow link", true, None).clicked() {
                                        row_action = Some(RowAction::FollowLink(entry.clone()));
                                        ui.close_menu();
                                    }
                                    ui.separator();
                                }
                                if !many && entry.is_dir {
                                    if theme::menu_item(ui, "Open", true, None).clicked() {
                                        row_action = Some(RowAction::Navigate(entry.path.clone()));
                                        ui.close_menu();
                                    }
                                    if theme::menu_item(
                                        ui,
                                        "Paste into this folder",
                                        has_clipboard,
                                        None,
                                    )
                                    .clicked()
                                    {
                                        row_action = Some(RowAction::Paste(entry.path.clone()));
                                        ui.close_menu();
                                    }
                                    if theme::menu_item(ui, "Upload files here…", true, None)
                                        .clicked()
                                    {
                                        row_action =
                                            Some(RowAction::UploadHere(entry.path.clone(), false));
                                        ui.close_menu();
                                    }
                                    if theme::menu_item(ui, "Upload folder here…", true, None)
                                        .clicked()
                                    {
                                        row_action =
                                            Some(RowAction::UploadHere(entry.path.clone(), true));
                                        ui.close_menu();
                                    }
                                }
                                // Opening locally is only offered when every
                                // selected row is a regular file: a folder has
                                // nothing to open in an editor.
                                if many {
                                    if all_files {
                                        let label = format!("Open {} files with…", targets.len());
                                        if theme::menu_item(ui, &label, true, None).clicked() {
                                            row_action = Some(RowAction::Open(targets.clone()));
                                            ui.close_menu();
                                        }
                                    }
                                } else if !entry.is_dir
                                    && theme::menu_item(ui, "Open with…", true, None).clicked()
                                {
                                    row_action = Some(RowAction::Open(targets.clone()));
                                    ui.close_menu();
                                }

                                let download = if many {
                                    format!("Download {} items…", targets.len())
                                } else {
                                    "Download…".to_string()
                                };
                                if theme::menu_item(ui, &download, true, None).clicked() {
                                    row_action = Some(RowAction::Download(targets.clone()));
                                    ui.close_menu();
                                }
                                if !many {
                                    if theme::menu_item(ui, "Rename…", true, None).clicked() {
                                        row_action = Some(RowAction::Rename(entry.clone()));
                                        ui.close_menu();
                                    }
                                    if theme::menu_item(ui, "Copy path", true, None).clicked() {
                                        row_action = Some(RowAction::CopyPath(entry.clone()));
                                        ui.close_menu();
                                    }
                                }
                                ui.separator();
                                let delete = if many {
                                    format!("Delete {} items…", targets.len())
                                } else {
                                    "Delete…".to_string()
                                };
                                if theme::menu_item(ui, &delete, true, Some(theme.palette.danger))
                                    .clicked()
                                {
                                    row_action = Some(RowAction::Delete(targets.clone()));
                                    ui.close_menu();
                                }
                            });
                        }
                    });

                if sftp_header(ui, theme, columns, sort, index, header_rect, table_width) {
                    activity = true;
                }
            });

        if let Some(index) = select_row {
            explorer.select_row(index);
            activity = true;
        }
        if let Some(index) = toggle_row {
            explorer.toggle_row(index);
            activity = true;
        }
        if let Some(index) = extend_row {
            explorer.extend_selection(index);
            activity = true;
        }

        // Arrow keys move the selection while the table is hovered and no
        // text field has focus, so typing a path is never hijacked.
        if !visible_indices.is_empty()
            && ui.rect_contains_pointer(table_output.inner_rect)
            && ui.memory(|memory| memory.focused().is_none())
        {
            let extend = ui.input(|input| input.modifiers.shift);
            let delta = ui.input(|input| {
                if input.key_pressed(egui::Key::ArrowDown) {
                    Some(1)
                } else if input.key_pressed(egui::Key::ArrowUp) {
                    Some(-1)
                } else {
                    None
                }
            });
            if let Some(delta) = delta {
                let position = explorer
                    .selection_anchor
                    .and_then(|anchor| visible_indices.iter().position(|&i| i == anchor));
                let next = match position {
                    Some(position) => (position as isize + delta)
                        .clamp(0, visible_indices.len() as isize - 1)
                        as usize,
                    None if delta > 0 => 0,
                    None => visible_indices.len() - 1,
                };
                let entry_index = visible_indices[next];
                if extend {
                    explorer.extend_selection(entry_index);
                } else {
                    explorer.select_row(entry_index);
                }
                activity = true;
            }
        }
        if let Some(action) = row_action {
            match action {
                RowAction::Navigate(path) => explorer.navigate(ssh, path),
                RowAction::FollowLink(entry) => {
                    explorer.open_link(ssh, &entry);
                }
                RowAction::Open(entries) => {
                    explorer.begin_open_with(&entries);
                }
                RowAction::Rename(entry) => {
                    explorer.begin_rename(&entry);
                }
                RowAction::Delete(entries) => {
                    explorer.begin_delete_many(&entries);
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
                RowAction::Download(entries) => {
                    if let Some(dir) = pick_download_folder() {
                        explorer.download_entries(ssh, &entries, dir);
                    }
                }
                RowAction::UploadHere(dir, folder) => {
                    let paths = if folder {
                        pick_upload_folder()
                    } else {
                        pick_upload_files()
                    };
                    if let Some(paths) = paths {
                        explorer.upload_into(ssh, dir, paths);
                    }
                }
            }
            activity = true;
        }

        // Desktop drag & drop: dropped paths upload into the visible directory.
        if !busy && ui.rect_contains_pointer(ui.max_rect()) {
            let dropped: Vec<PathBuf> = ui.ctx().input(|input| {
                input
                    .raw
                    .dropped_files
                    .iter()
                    .filter_map(|file| file.path.clone())
                    .collect()
            });
            if !dropped.is_empty() {
                explorer.upload_paths(ssh, dropped);
                activity = true;
            }
        }

        activity
    }

    /// Render rename / new-folder / delete dialogs for the active session.
    fn sftp_dialogs_ui(&mut self, ctx: &egui::Context, index: usize) -> bool {
        let theme = self.settings.theme.theme();
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

        let (title, subtitle): (String, Option<&str>) = match &kind {
            // Name the entry being renamed so the heading identifies it; the
            // name itself is in the editable field below, not repeated here.
            SftpDialogKind::Rename { from } => {
                let name = from.rsplit('/').next().unwrap_or(from);
                (format!("Rename “{name}”"), None)
            }
            SftpDialogKind::NewFolder { .. } => (
                "New folder".to_string(),
                Some("Create a folder in this directory."),
            ),
            // The heading names the entry, so the body does not repeat
            // "Delete \"name\"?" a second time.
            SftpDialogKind::Delete { name, .. } => (format!("Delete “{name}”"), None),
            SftpDialogKind::DeleteMany { names, .. } => {
                (format!("Delete {} items", names.len()), None)
            }
        };

        egui::Window::new(&title)
            .id(egui::Id::new(("sftp_dialog", index)))
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .default_size(theme::dialog_size(theme))
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .frame(theme::modal_frame(theme))
            .show(ctx, |ui| {
                ui.set_width(theme::dialog_inner_width(theme));
                ui.horizontal(|ui| {
                    theme::modal_heading(ui, theme, &title);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if theme::modal_close(ui) {
                            cancel = true;
                        }
                    });
                });
                if let Some(subtitle) = subtitle {
                    theme::muted(ui, theme, subtitle);
                }
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
                        theme::muted(ui, theme, "Will be created in");
                        theme::modal_path(ui, theme, parent);
                        if let Some(err) = &dialog.error {
                            ui.add_space(4.0);
                            ui.colored_label(theme.palette.danger, err);
                        }
                    }
                    SftpDialogKind::Delete { path, is_dir, .. } => {
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
                        theme::modal_path(ui, theme, path);
                        if let Some(err) = &dialog.error {
                            ui.add_space(4.0);
                            ui.colored_label(theme.palette.danger, err);
                        }
                    }
                    SftpDialogKind::DeleteMany { names, .. } => {
                        theme::callout_frame(theme, theme.palette.danger_soft).show(ui, |ui| {
                            let warning = format!(
                                "{} selected entries will be permanently deleted, including everything inside selected folders. This cannot be undone.",
                                names.len()
                            );
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(warning)
                                        .color(theme.palette.danger),
                                )
                                .wrap(),
                            );
                        });
                        ui.add_space(8.0);
                        for name in names.iter().take(6) {
                            theme::muted(ui, theme, name);
                        }
                        if names.len() > 6 {
                            theme::muted(ui, theme, format!("…and {} more", names.len() - 6));
                        }
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
                        SftpDialogKind::Delete { .. } | SftpDialogKind::DeleteMany { .. } => {
                            "Delete"
                        }
                    };
                    let primary_button = if matches!(
                        kind,
                        SftpDialogKind::Delete { .. } | SftpDialogKind::DeleteMany { .. }
                    ) {
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

    /// Consent dialog for writing directory tracking into a startup file.
    fn sftp_hook_dialog_ui(&mut self, ctx: &egui::Context, index: usize) -> bool {
        let theme = self.settings.theme.theme();
        let Some(session) = self.sessions.get_mut(index) else {
            return false;
        };
        let (ssh, explorer) = (&session.ssh, &mut session.sftp);
        let Some(prompt) = explorer.hook_prompt.clone() else {
            return false;
        };

        let mut install = false;
        let mut remove = false;
        let mut copy = false;
        let mut cancel = false;
        let control = theme.metrics.control_height;

        egui::Window::new("Directory tracking")
            .id(egui::Id::new(("sftp_hook", index)))
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .default_size(theme::dialog_size(theme))
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .frame(theme::modal_frame(theme))
            .show(ctx, |ui| {
                ui.set_width(theme::dialog_inner_width(theme));
                ui.horizontal(|ui| {
                    theme::modal_heading(ui, theme, "Directory tracking");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if theme::modal_close(ui) {
                            cancel = true;
                        }
                    });
                });
                theme::muted(
                    ui,
                    theme,
                    "Let the file panel follow the shell's folder as you change directory.",
                );
                ui.add_space(theme.metrics.pad);

                theme::muted(ui, theme, "Startup file");
                theme::modal_path(ui, theme, &prompt.path);
                ui.add_space(4.0);
                theme::muted(ui, theme, format!("Detected shell: {}", prompt.shell));
                ui.add_space(6.0);
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(
                            "Install adds a marked block, keeping a one-time backup beside the file. Remove deletes the block and leaves the rest of the file untouched.",
                        )
                        .color(theme.palette.muted)
                        .size(theme.metrics.small_size),
                    )
                    .wrap(),
                );
                if prompt.kind.shares_cwdcmd() {
                    ui.add_space(6.0);
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(
                                "csh has a single cwdcmd alias, so Install replaces any alias you already have. The backup keeps the original; Copy snippet lets you add it yourself instead.",
                            )
                            .color(theme.palette.warning)
                            .size(theme.metrics.small_size),
                        )
                        .wrap(),
                    );
                }

                ui.add_space(theme.metrics.card_padding);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(
                            theme::primary_button(theme, "Install")
                                .min_size(egui::vec2(92.0, control)),
                        )
                        .clicked()
                    {
                        install = true;
                    }
                    if ui
                        .add(egui::Button::new("Remove").min_size(egui::vec2(92.0, control)))
                        .on_hover_text("Delete the marked block from this file")
                        .clicked()
                    {
                        remove = true;
                    }
                    if ui
                        .add(egui::Button::new("Cancel").min_size(egui::vec2(92.0, control)))
                        .clicked()
                    {
                        cancel = true;
                    }
                    if ui
                        .add(egui::Button::new("Copy snippet").min_size(egui::vec2(108.0, control)))
                        .on_hover_text("Copy the one-line snippet to add by hand")
                        .clicked()
                    {
                        copy = true;
                    }
                });
            });

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            cancel = true;
        }
        if copy {
            ctx.copy_text(prompt.kind.session_hook().to_string());
        }
        if cancel {
            return explorer.cancel_hook_prompt();
        }
        if install || remove {
            return explorer.submit_hook_prompt(ssh, install);
        }
        false
    }

    /// Render the "Open with…" editor picker for the active session.
    fn editor_picker_ui(&mut self, ctx: &egui::Context, index: usize) -> bool {
        let theme = self.settings.theme.theme();
        let Some(session) = self.sessions.get_mut(index) else {
            return false;
        };
        let (ssh, explorer) = (&session.ssh, &mut session.sftp);
        let Some(picker) = explorer.editor_picker.as_mut() else {
            return false;
        };

        let mut submit = false;
        let mut cancel = false;
        let count = picker.entries.len();
        let heading = if count == 1 {
            format!("Open “{}”", picker.entries[0].name)
        } else {
            format!("Open {count} files")
        };

        egui::Window::new("Open with")
            .id(egui::Id::new(("sftp_editor_picker", index)))
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .default_size(theme::dialog_size(theme))
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .frame(theme::modal_frame(theme))
            .show(ctx, |ui| {
                ui.set_width(theme::dialog_inner_width(theme));
                ui.horizontal(|ui| {
                    theme::modal_heading(ui, theme, &heading);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if theme::modal_close(ui) {
                            cancel = true;
                        }
                    });
                });
                ui.add_space(4.0);
                if count == 1 {
                    theme::muted(ui, theme, "Download this file, then open it in:");
                    theme::modal_path(ui, theme, &picker.entries[0].path);
                } else {
                    theme::muted(ui, theme, "Download these files, then open each in:");
                    theme::modal_path(ui, theme, &picker.entries[0].path);
                    theme::muted(ui, theme, format!("…and {} more", count - 1));
                }
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
                if count == 1 {
                    ui.add_space(6.0);
                    ui.checkbox(
                        &mut picker.auto_upload,
                        "Upload changes automatically when the file is saved",
                    );
                }

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

    /// Ask how to resolve existing destinations before a transfer starts.
    fn sftp_conflict_ui(&mut self, ctx: &egui::Context, index: usize) -> bool {
        let theme = self.settings.theme.theme();
        let Some(session) = self.sessions.get_mut(index) else {
            return false;
        };
        let (ssh, explorer) = (&session.ssh, &mut session.sftp);
        let Some(prompt) = explorer.conflict.as_ref() else {
            return false;
        };
        let names = prompt.names.clone();
        let label = prompt.label.clone();

        let mut choice: Option<Option<ConflictPolicy>> = None;

        egui::Window::new("Items already exist")
            .id(egui::Id::new(("sftp_conflict", index)))
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .default_size(theme::dialog_size(theme))
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .frame(theme::modal_frame(theme))
            .show(ctx, |ui| {
                ui.set_width(theme::dialog_inner_width(theme));
                let heading = if names.len() == 1 {
                    format!("Replace “{}”", names[0])
                } else {
                    format!("Replace {} items", names.len())
                };
                ui.horizontal(|ui| {
                    theme::modal_heading(ui, theme, &heading);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if theme::modal_close(ui) {
                            choice = Some(None);
                        }
                    });
                });
                theme::muted(
                    ui,
                    theme,
                    format!("{label} — the transfer has not started yet."),
                );
                ui.add_space(theme.metrics.pad);
                for name in names.iter().take(5) {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(format!("• {name}"))
                                .small()
                                .color(theme.palette.muted),
                        )
                        .truncate(),
                    )
                    .on_hover_text(name);
                }
                if names.len() > 5 {
                    theme::muted(ui, theme, format!("and {} more", names.len() - 5));
                }
                ui.add_space(8.0);
                theme::callout_frame(theme, theme.palette.danger_soft).show(ui, |ui| {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(
                                "Replace overwrites the existing files. Keep both writes the \
                                 new copy next to them as “name (copy)”.",
                            )
                            .color(theme.palette.danger),
                        )
                        .wrap(),
                    );
                });
                ui.add_space(theme.metrics.card_padding);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let replace = theme::primary_button(theme, "Replace")
                        .min_size(egui::vec2(92.0, theme.metrics.control_height));
                    if ui.add(replace).clicked() {
                        choice = Some(Some(ConflictPolicy::Overwrite));
                    }
                    let keep = egui::Button::new("Keep both")
                        .min_size(egui::vec2(92.0, theme.metrics.control_height));
                    if ui.add(keep).clicked() {
                        choice = Some(Some(ConflictPolicy::KeepBoth));
                    }
                    let cancel = egui::Button::new("Cancel")
                        .min_size(egui::vec2(92.0, theme.metrics.control_height));
                    if ui.add(cancel).clicked() {
                        choice = Some(None);
                    }
                });
            });

        if ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
            choice = Some(None);
        }
        match choice {
            Some(policy) => explorer.resolve_conflict(ssh, policy),
            None => false,
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
        let settings = self.settings.clone();
        let theme = settings.theme.theme();
        let session = &mut self.sessions[index];
        let mut activity = false;
        poll_display(session, &settings, ctx);

        // The bar is contextual: a lost connection needs a way out, and the
        // remote-display controls appear based on what the session actually
        // negotiated, not on the live checkboxes.
        let mut disconnect_clicked = false;
        let session_x11 = session.ssh.x11_requested();
        let show_display = session_x11
            || settings.x11
            || settings.vnc
            || session.display.tunnel_port.is_some()
            || session.display.busy;
        if session.connection_lost || show_display || session.display.tunnel_port.is_some() {
            egui::TopBottomPanel::bottom(egui::Id::new(("terminal_actions", index)))
                .exact_height(theme.metrics.control_height + 8.0)
                .frame(
                    egui::Frame::none()
                        .fill(theme.palette.bg)
                        .inner_margin(egui::Margin::symmetric(4.0, 4.0)),
                )
                .show_inside(ui, |ui| {
                    ui.horizontal(|ui| {
                        if session.connection_lost {
                            ui.label(
                                egui::RichText::new(
                                    "Connection lost — the server closed this session",
                                )
                                .small()
                                .color(theme.palette.danger),
                            );
                            if ui.button("Close tab").clicked() {
                                disconnect_clicked = true;
                            }
                            return;
                        }

                        if (settings.x11 || settings.vnc)
                            && ui
                                .button("Display")
                                .on_hover_text("Start or open the remote display")
                                .clicked()
                        {
                            begin_display(session, &settings);
                        }
                        // X11 state belongs to the session, not the checkbox.
                        if session_x11 {
                            let status = session.ssh.x11_status();
                            if status.forwarding {
                                let text = match status.active {
                                    0 => "X11: forwarding".to_string(),
                                    1 => "X11: 1 connection".to_string(),
                                    active => format!("X11: {active} connections"),
                                };
                                ui.label(
                                    egui::RichText::new(text)
                                        .small()
                                        .color(theme.palette.success),
                                );
                            } else if let Some(err) = status.error {
                                ui.label(
                                    egui::RichText::new("X11: unavailable")
                                        .small()
                                        .color(theme.palette.warning),
                                )
                                .on_hover_text(err.clone());
                                ui.label(
                                    egui::RichText::new(err).small().color(theme.palette.muted),
                                );
                            }
                        } else if settings.x11 {
                            ui.label(
                                egui::RichText::new("X11: reconnect to enable")
                                    .small()
                                    .color(theme.palette.muted),
                            );
                            if ui
                                .small_button("Reconnect")
                                .on_hover_text("Reconnect with X11 forwarding enabled")
                                .clicked()
                            {
                                self.pending_reconnect = Some((index, true));
                            }
                        }
                        if let Some(port) = session.display.tunnel_port {
                            ui.separator();
                            // Show both ends: the remote display the tunnel
                            // points at, and the local port a viewer uses.
                            let remote = match session
                                .display
                                .target
                                .as_ref()
                                .and_then(|target| target.display_number())
                            {
                                Some(display) => format!(":{display}"),
                                None => session
                                    .display
                                    .target
                                    .as_ref()
                                    .map(|target| target.label())
                                    .unwrap_or_default(),
                            };
                            ui.label(
                                egui::RichText::new(format!("VNC {remote} → 127.0.0.1:{port}"))
                                    .small()
                                    .color(theme.palette.text),
                            );
                            // Point the next command at the VNC desktop. The text
                            // depends on the login shell: csh/tcsh need `setenv`,
                            // bash/zsh use the temporary `DISPLAY=` prefix, and
                            // `env` is the fallback when detection failed.
                            if let Some(display) = session
                                .display
                                .target
                                .as_ref()
                                .and_then(|target| target.display_number())
                            {
                                // Typing is only useful at a shell prompt; while
                                // an alternate screen (vim, htop) is up the text
                                // would go to that program instead.
                                let at_prompt = !session.terminal.is_alt_screen();
                                let prefix = match session.display.shell {
                                    Some(kind) => kind.display_prefix(display),
                                    None => format!("env DISPLAY=:{display} "),
                                };
                                if ui
                                    .add_enabled(
                                        at_prompt,
                                        egui::Button::new(format!("Run on :{display}")),
                                    )
                                    .on_hover_text(if at_prompt {
                                        format!("Type \"{prefix}\" into the shell")
                                    } else {
                                        "Not available while a full-screen program is running"
                                            .to_string()
                                    })
                                    .clicked()
                                {
                                    session.ssh.write(prefix.as_bytes().to_vec());
                                    session.display.message =
                                        Some(format!("Typed \"{prefix}\" — now type the command"));
                                }
                            }
                            if ui
                                .small_button("Open")
                                .on_hover_text("Open the viewer again")
                                .clicked()
                            {
                                launch_vnc_viewer(&mut session.display, port);
                            }
                            if ui
                                .small_button("Copy")
                                .on_hover_text("Copy the tunnel address")
                                .clicked()
                            {
                                ui.ctx().copy_text(format!("127.0.0.1:{port}"));
                                session.display.message = Some("Tunnel address copied".to_string());
                            }
                            if ui
                                .small_button("Stop")
                                .on_hover_text("Close the tunnel and the viewer")
                                .clicked()
                            {
                                stop_vnc(session);
                            }
                        }
                        if session.display.busy {
                            ui.spinner();
                        }
                        if let Some(message) = &session.display.message {
                            ui.label(
                                egui::RichText::new(message)
                                    .small()
                                    .color(theme.palette.muted),
                            )
                            .on_hover_text(message);
                        }
                    });
                });
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

        // The "Ask every time" chooser, shown when both options are enabled.
        if session.display.ask {
            let status = session.ssh.x11_status();
            let mut choice: Option<DisplayChoice> = None;
            let mut cancel = false;
            let control = theme.metrics.control_height;
            egui::Window::new("Remote display")
                .id(egui::Id::new(("display_choice", index)))
                .title_bar(false)
                .collapsible(false)
                .resizable(false)
                .default_size(theme::dialog_size(theme))
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .frame(theme::modal_frame(theme))
                .show(ctx, |ui| {
                    ui.set_width(theme::dialog_inner_width(theme));
                    ui.horizontal(|ui| {
                        theme::modal_heading(ui, theme, "Remote display");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if theme::modal_close(ui) {
                                cancel = true;
                            }
                        });
                    });
                    theme::muted(
                        ui,
                        theme,
                        "Both X11 and VNC are enabled. Choose one for this session.",
                    );
                    ui.add_space(theme.metrics.pad);
                    if status.forwarding {
                        ui.label(
                            egui::RichText::new("X11 forwarding is active")
                                .small()
                                .color(theme.palette.success),
                        );
                    } else {
                        ui.label(
                            egui::RichText::new("X11 forwarding is not available")
                                .small()
                                .color(theme.palette.muted),
                        );
                        if let Some(err) = &status.error {
                            theme::muted(ui, theme, err);
                        }
                    }
                    ui.add_space(theme.metrics.card_padding);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(
                                theme::primary_button(theme, "VNC")
                                    .min_size(egui::vec2(92.0, control)),
                            )
                            .clicked()
                        {
                            choice = Some(DisplayChoice::Vnc);
                        }
                        if ui
                            .add(egui::Button::new("X11").min_size(egui::vec2(92.0, control)))
                            .clicked()
                        {
                            choice = Some(DisplayChoice::X11);
                        }
                        if ui
                            .add(egui::Button::new("Cancel").min_size(egui::vec2(92.0, control)))
                            .clicked()
                        {
                            cancel = true;
                        }
                    });
                });
            if ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
                cancel = true;
            }
            if let Some(choice) = choice {
                apply_display_choice(session, &settings, choice);
            } else if cancel {
                session.display.ask = false;
            }
        }

        (disconnect_clicked, activity)
    }
}

impl eframe::App for RustTermApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_connecting();
        // Poll reconnect attempts for every tab.
        let n = self.sessions.len();
        for i in 0..n {
            self.poll_reconnecting(i);
        }
        // Dispatch a reconnect requested from a tab's bottom bar.
        if let Some((index, enable_x11)) = self.pending_reconnect.take() {
            if index < self.sessions.len() {
                self.begin_reconnect(index, ctx, enable_x11);
            }
        }
        // Escape aborts a connection attempt as well as the ✕ button. While a
        // host-key prompt is up, Esc is left to the modal.
        if self
            .connecting
            .as_ref()
            .is_some_and(|state| state.prompt.is_none())
            && ctx.input(|input| input.key_pressed(egui::Key::Escape))
        {
            self.cancel_connect();
        }

        // Read background tabs too: SSH flow control must keep moving even when
        // another session is visible. New output is what wakes egui, so this
        // happens before the UI is painted.
        let mut activity = self.pump_sessions(ctx);
        let theme = self.settings.theme.theme();

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
                        .default_width(560.0)
                        .min_width(260.0)
                        .max_width(max_width)
                        .frame(theme::panel_frame(theme))
                        .show(ctx, |ui| {
                            activity |= self.sftp_ui(ui, idx);
                        });
                    activity |= self.sftp_dialogs_ui(ctx, idx);
                    activity |= self.sftp_hook_dialog_ui(ctx, idx);
                    activity |= self.sftp_conflict_ui(ctx, idx);
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
        self.settings_window_ui(ctx);

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

/// Process finished VNC probes and tunnel replies for one session.
fn poll_display(session: &mut ActiveSession, settings: &Settings, ctx: &egui::Context) {
    if let Some(rx) = session.display.probe.take() {
        match rx.try_recv() {
            Ok(Ok(probe)) => {
                // Remember the login shell for the Run on :N button; an
                // unsupported shell simply keeps the `env` fallback.
                session.display.shell = probe.shell.as_deref().and_then(ShellKind::from_shell);
                if let Some(found) = probe.target {
                    session.display.target = Some(found.clone());
                    start_vnc_tunnel(session, found);
                } else {
                    // Nothing was detected on the server: fall back to the
                    // configured address, which is the only way to reach a VNC
                    // server rustterm cannot see (a different host, for
                    // example).
                    let target = settings_vnc_target(settings);
                    session.display.message =
                        Some(format!("No VNC session found — using {}", target.label()));
                    session.display.target = Some(target.clone());
                    start_vnc_tunnel(session, target);
                }
            }
            Ok(Err(err)) => {
                // The probe reported a problem it cannot work around (for
                // example a missing ~/.vnc/passwd). Do not tunnel blindly.
                session.display.busy = false;
                session.display.message = Some(err);
            }
            Err(std_mpsc::TryRecvError::Empty) => session.display.probe = Some(rx),
            Err(std_mpsc::TryRecvError::Disconnected) => {
                session.display.busy = false;
                session.display.message =
                    Some("the SSH session closed while probing for VNC".to_string());
            }
        }
    }

    if let Some(rx) = session.display.open.take() {
        match rx.try_recv() {
            Ok(Ok(port)) => {
                session.display.busy = false;
                session.display.tunnel_port = Some(port);
                let opened = launch_vnc_viewer(&mut session.display, port);
                if !opened && settings.x11 && session.ssh.x11_status().forwarding {
                    session.display.message = Some(format!(
                        "No VNC viewer installed — X11 forwarding is active (tunnel: 127.0.0.1:{port})"
                    ));
                }
            }
            Ok(Err(err)) => {
                session.display.busy = false;
                session.display.message = Some(err);
            }
            Err(std_mpsc::TryRecvError::Empty) => session.display.open = Some(rx),
            Err(std_mpsc::TryRecvError::Disconnected) => {
                session.display.busy = false;
                session.display.message =
                    Some("the SSH session closed while opening the VNC tunnel".to_string());
            }
        }
    }

    // A viewer that exits is not necessarily finished: `remmina -c` and
    // similar hand off to an already-running instance and return immediately.
    // Stop the tunnel only after a grace period with no connected client.
    if session
        .display
        .viewer
        .as_mut()
        .is_some_and(|child| child.try_wait().ok().flatten().is_some())
    {
        session.display.viewer = None;
        session.display.viewer_exited_at = Some(Instant::now());
    }
    if let Some(exited) = session.display.viewer_exited_at {
        if session.display.tunnel_port.is_none() {
            session.display.viewer_exited_at = None;
        } else if session.ssh.vnc_clients() > 0 {
            // The viewer handed off to a running instance; keep the tunnel.
            session.display.viewer_exited_at = None;
        } else {
            let waited = exited.elapsed();
            if waited >= VNC_TUNNEL_GRACE {
                session.display.viewer_exited_at = None;
                let _ = session.ssh.vnc_close();
                session.display.tunnel_port = None;
                session.display.target = None;
                session.display.message = Some("Viewer closed — VNC tunnel stopped".to_string());
            } else {
                ctx.request_repaint_after(VNC_TUNNEL_GRACE - waited);
            }
        }
    }
}

fn settings_vnc_target(settings: &Settings) -> VncTarget {
    VncTarget::parse(&settings.vnc_host, settings.vnc_port).unwrap_or_else(|| VncTarget {
        host: "127.0.0.1".to_string(),
        port: settings.vnc_port,
    })
}

/// Start a probe, then tunnel to whatever it finds (or to the setting).
fn start_vnc(session: &mut ActiveSession, settings: &Settings) {
    session.display.busy = true;
    session.display.message = Some("Looking for a VNC server on the SSH host…".to_string());
    match session.ssh.vnc_probe(settings.vnc_auto_create) {
        Some(rx) => session.display.probe = Some(rx),
        None => {
            session.display.busy = false;
            session.display.message = Some("the SSH session is no longer available".to_string());
        }
    }
}

fn start_vnc_tunnel(session: &mut ActiveSession, target: VncTarget) {
    match session.ssh.vnc_open(&target) {
        Some(rx) => {
            session.display.busy = true;
            session.display.open = Some(rx);
            session.display.message = Some(format!("Opening a tunnel to {}…", target.label()));
        }
        None => {
            session.display.busy = false;
            session.display.message = Some("the SSH session is no longer available".to_string());
        }
    }
}

/// Start or open the remote display, honouring the Ask/Prefer setting.
fn begin_display(session: &mut ActiveSession, settings: &Settings) {
    let choice = match (settings.x11, settings.vnc, settings.display_pref) {
        (true, true, DisplayPref::Ask) => {
            session.display.ask = true;
            return;
        }
        (true, true, DisplayPref::X11) => DisplayChoice::X11,
        (true, true, DisplayPref::Vnc) => DisplayChoice::Vnc,
        (true, false, _) => DisplayChoice::X11,
        (false, true, _) => DisplayChoice::Vnc,
        (false, false, _) => return,
    };
    apply_display_choice(session, settings, choice);
}

fn apply_display_choice(session: &mut ActiveSession, settings: &Settings, choice: DisplayChoice) {
    session.display.ask = false;
    match choice {
        DisplayChoice::X11 => {
            let status = session.ssh.x11_status();
            if status.forwarding {
                session.display.message = Some(match status.active {
                    0 => "X11 forwarding is active — start a program on the server".to_string(),
                    1 => "X11 forwarding is active (1 connection)".to_string(),
                    active => format!("X11 forwarding is active ({active} connections)"),
                });
                return;
            }
            if settings.vnc {
                session.display.message = Some(match status.error {
                    Some(err) => format!("X11 unavailable ({err}) — trying VNC"),
                    None => "X11 unavailable — trying VNC".to_string(),
                });
                start_vnc(session, settings);
            } else {
                session.display.message = Some(match status.error {
                    Some(err) => err,
                    None => "X11 was not requested for this session — reconnect to enable it"
                        .to_string(),
                });
            }
        }
        DisplayChoice::Vnc => {
            if let Some(port) = session.display.tunnel_port {
                launch_vnc_viewer(&mut session.display, port);
            } else {
                start_vnc(session, settings);
            }
        }
    }
}

fn stop_vnc(session: &mut ActiveSession) {
    if let Some(mut child) = session.display.viewer.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let _ = session.ssh.vnc_close();
    session.display.tunnel_port = None;
    session.display.target = None;
    session.display.busy = false;
    session.display.viewer_exited_at = None;
    session.display.message = Some("VNC tunnel closed".to_string());
}

/// Start an installed external VNC viewer against the tunnel.
fn launch_vnc_viewer(display: &mut DisplayUi, port: u16) -> bool {
    let Some((program, args)) = vnc_viewer_command(port) else {
        display.message = Some(format!(
            "VNC tunnel ready at 127.0.0.1:{port} — install a viewer or copy this address"
        ));
        return false;
    };
    match std::process::Command::new(program)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => {
            display.viewer = Some(child);
            display.message = Some(format!("VNC viewer opened at 127.0.0.1:{port}"));
            true
        }
        Err(err) => {
            display.message = Some(format!("could not start {program}: {err}"));
            false
        }
    }
}

/// First viewer on `PATH`, with the argument form it understands.
fn vnc_viewer_command(port: u16) -> Option<(&'static str, Vec<String>)> {
    // TigerVNC and tightvncviewer accept `host::port` for a raw TCP port.
    if command_available("vncviewer") {
        return Some(("vncviewer", vec![format!("127.0.0.1::{port}")]));
    }
    if command_available("xtigervncviewer") {
        return Some(("xtigervncviewer", vec![format!("127.0.0.1::{port}")]));
    }
    if command_available("gvncviewer") {
        return Some(("gvncviewer", vec![format!("127.0.0.1:{port}")]));
    }
    if command_available("remmina") {
        return Some((
            "remmina",
            vec!["-c".to_string(), format!("vnc://127.0.0.1:{port}")],
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{
        cmp_lower, sftp_header, sorted_indices, RemoteEntry, SftpColumns, SftpLayout, SftpSort,
        SftpSortKey, COLUMN_STEP, MIN_MODIFIED_WIDTH, MIN_OWNER_WIDTH, MIN_PERMISSIONS_WIDTH,
        MIN_SIZE_WIDTH, NAME_MIN_WIDTH, TABLE_WIDTH,
    };
    use crate::theme::ThemeId;
    use std::time::{Duration, SystemTime};

    fn entry(name: &str, is_dir: bool, size: u64, owner: &str, age_secs: u64) -> RemoteEntry {
        RemoteEntry {
            name: name.to_string(),
            path: format!("/remote/{name}"),
            is_dir,
            is_symlink: false,
            size,
            owner: owner.to_string(),
            permissions: None,
            modified: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(age_secs)),
        }
    }

    fn folder(name: &str) -> RemoteEntry {
        entry(name, true, 0, "root", 100)
    }

    #[test]
    fn layout_orders_the_columns_left_to_right() {
        let rect = egui::Rect::from_min_size(egui::pos2(10.0, 0.0), egui::vec2(TABLE_WIDTH, 20.0));
        let layout = SftpLayout::new(rect, SftpColumns::default());
        assert!((layout.name.left() - 10.0).abs() < 0.01);
        assert!((layout.name.right() - layout.modified.left()).abs() < 0.01);
        assert!((layout.modified.right() - layout.size.left()).abs() < 0.01);
        assert!((layout.size.right() - layout.owner.left()).abs() < 0.01);
        assert!((layout.owner.right() - layout.permissions.left()).abs() < 0.01);
        assert!((layout.permissions.right() - rect.right()).abs() < 0.01);
    }

    /// The panel is only a viewport: the same columns must land on the same
    /// pixels no matter how wide the space they are drawn in.
    #[test]
    fn panel_width_does_not_move_the_columns() {
        for width in [120.0, 340.0, 520.0, 900.0] {
            let rect = egui::Rect::from_min_size(egui::pos2(10.0, 0.0), egui::vec2(width, 20.0));
            let layout = SftpLayout::new(rect, SftpColumns::default());
            let expected = SftpColumns::default().widths();
            assert!(
                (layout.name.width() - expected[0]).abs() < 0.01,
                "w {width}"
            );
            assert!(
                (layout.owner.width() - expected[3]).abs() < 0.01,
                "w {width}"
            );
            assert!(
                (layout.permissions.width() - expected[4]).abs() < 0.01,
                "w {width}"
            );
            // The table itself keeps its canvas width; it is merely clipped.
            assert!(
                (layout.permissions.right() - rect.left() - TABLE_WIDTH).abs() < 0.01,
                "w {width}"
            );
        }
    }

    /// Paints the real header through egui at every width, so the resize
    /// handles and sort targets cannot panic.
    #[test]
    fn header_paints_at_every_panel_width() {
        let theme = ThemeId::Midnight.theme();
        let mut columns = SftpColumns::default();
        let mut sort = SftpSort::default();
        let ctx = egui::Context::default();
        for width in [90.0, 160.0, 320.0, 480.0, 720.0] {
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let rect =
                        egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(width, 24.0));
                    sftp_header(ui, theme, &mut columns, &mut sort, 0, rect, width);
                });
            });
        }
    }

    #[test]
    fn default_layout_shares_the_canvas_between_five_columns() {
        let w = SftpColumns::default().widths();
        assert!((w[0] - TABLE_WIDTH * 0.42).abs() < 0.01, "name {}", w[0]);
        assert!(w[1] >= MIN_MODIFIED_WIDTH);
        assert!(w[2] >= MIN_SIZE_WIDTH);
        assert!(w[3] >= MIN_OWNER_WIDTH);
        assert!(w[4] >= MIN_PERMISSIONS_WIDTH);
        assert!((w.iter().sum::<f32>() - TABLE_WIDTH).abs() < 0.01);
    }

    #[test]
    fn dividers_move_in_24_pixel_steps() {
        let mut columns = SftpColumns::default();
        let before = columns.widths();
        // Dragging the Name/Modified divider 23 px right lands on 24 px.
        columns.drag_divider(1, before[0] + 23.0);
        let after = columns.widths();
        assert!(
            (after[0] - before[0] - COLUMN_STEP).abs() < 0.01,
            "name moved by {}",
            after[0] - before[0]
        );
        // Every move is a whole step, never an arbitrary pixel.
        for offset in [37.0, 151.0, 233.0, 401.0] {
            let mut probe = SftpColumns::default();
            let prior = probe.widths();
            probe.drag_divider(1, offset);
            let moved = (probe.widths()[0] - prior[0]) / COLUMN_STEP;
            assert!(
                (moved - moved.round()).abs() < 0.01,
                "offset {offset}: moved {moved} steps"
            );
        }
    }

    #[test]
    fn dividers_stop_at_the_column_minimum() {
        let mut columns = SftpColumns::default();
        // Drag the Name/Modified divider far right: Modified cannot collapse.
        columns.drag_divider(1, TABLE_WIDTH);
        let w = columns.widths();
        assert!(w[1] >= MIN_MODIFIED_WIDTH - 0.01, "modified {}", w[1]);
        // And far left: the name keeps a readable width.
        columns.drag_divider(1, 0.0);
        let w = columns.widths();
        assert!(w[0] >= NAME_MIN_WIDTH - 0.01, "name {}", w[0]);
        assert!(w.iter().all(|&width| width > 0.0), "no column is hidden");
    }

    #[test]
    fn dragging_never_changes_the_canvas_width() {
        let mut columns = SftpColumns::default();
        for boundary in [1, 2, 3, 1, 2] {
            columns.drag_divider(boundary, 900.0);
            let total: f32 = columns.widths().iter().sum();
            assert!((total - TABLE_WIDTH).abs() < 0.01, "total {total}");
        }
        // A boundary outside 1..4 is ignored.
        let before = columns.widths();
        columns.drag_divider(0, 100.0);
        columns.drag_divider(4, 100.0);
        assert_eq!(columns.widths(), before);
    }

    fn names(entries: &[RemoteEntry], shown: &[usize]) -> Vec<String> {
        shown.iter().map(|&i| entries[i].name.clone()).collect()
    }

    #[test]
    fn sorting_keeps_folders_first_then_orders_by_key() {
        let entries = vec![
            entry("beta.txt", false, 300, "bob", 30),
            folder("Docs"),
            entry("Alpha.txt", false, 100, "alice", 10),
            entry("z.bin", false, 200, "carol", 20),
        ];

        let ascending = SftpSort::default();
        let shown = sorted_indices(&entries, false, ascending);
        assert_eq!(
            names(&entries, &shown),
            ["Docs", "Alpha.txt", "beta.txt", "z.bin"]
        );

        let descending = SftpSort {
            key: SftpSortKey::Name,
            ascending: false,
        };
        let shown = sorted_indices(&entries, false, descending);
        // Folders stay on top; only the files reverse.
        assert_eq!(
            names(&entries, &shown),
            ["Docs", "z.bin", "beta.txt", "Alpha.txt"]
        );

        let by_size = SftpSort {
            key: SftpSortKey::Size,
            ascending: false,
        };
        let shown = sorted_indices(&entries, false, by_size);
        assert_eq!(
            names(&entries, &shown),
            ["Docs", "beta.txt", "z.bin", "Alpha.txt"]
        );

        let by_owner = SftpSort {
            key: SftpSortKey::Owner,
            ascending: true,
        };
        let shown = sorted_indices(&entries, false, by_owner);
        assert_eq!(
            names(&entries, &shown),
            ["Docs", "Alpha.txt", "beta.txt", "z.bin"]
        );

        let by_age = SftpSort {
            key: SftpSortKey::Modified,
            ascending: true,
        };
        let shown = sorted_indices(&entries, false, by_age);
        assert_eq!(
            names(&entries, &shown),
            ["Docs", "Alpha.txt", "z.bin", "beta.txt"]
        );
    }

    #[test]
    fn case_insensitive_order_matches_lowercase_strings() {
        let names = ["Zeta", "alpha", "İstanbul", "istanbul", "ALPHA"];
        let mut by_helper = names;
        by_helper.sort_by(|a, b| cmp_lower(a, b));
        let mut by_string = names;
        by_string.sort_by_key(|name| name.to_lowercase());
        assert_eq!(by_helper, by_string);
    }

    #[test]
    fn sorting_skips_hidden_files_unless_enabled() {
        let entries = vec![
            entry(".bashrc", false, 10, "root", 1),
            entry("visible.txt", false, 20, "root", 2),
        ];
        let sort = SftpSort::default();
        assert_eq!(
            names(&entries, &sorted_indices(&entries, false, sort)),
            ["visible.txt"]
        );
        assert_eq!(
            names(&entries, &sorted_indices(&entries, true, sort)),
            [".bashrc", "visible.txt"]
        );
    }

    #[test]
    fn sort_click_toggles_direction_then_switches_key() {
        let mut sort = SftpSort::default();
        assert_eq!(sort.key, SftpSortKey::Name);
        assert!(sort.ascending);
        sort.toggle(SftpSortKey::Name);
        assert!(sort.key == SftpSortKey::Name && !sort.ascending);
        sort.toggle(SftpSortKey::Size);
        assert!(sort.key == SftpSortKey::Size && sort.ascending);
    }
}
