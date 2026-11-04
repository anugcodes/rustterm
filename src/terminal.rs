//! Terminal view: wraps alacritty_terminal::Term and renders it with egui.

use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, StdSyncHandler};
use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId, Modifiers, PointerButton, Pos2, Rect, Vec2};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

const DEFAULT_BG: Color32 = Color32::from_rgb(20, 20, 25);
const DEFAULT_FG: Color32 = Color32::from_rgb(220, 220, 220);

/// Cached layout for one terminal row, so unchanged rows don't rebuild
/// their `LayoutJob` (and re-tessellate) every frame.
#[derive(Clone)]
struct RowCache {
    text: String,
    runs: Vec<(usize, usize, Color32)>,
    galley: Arc<egui::text::Galley>,
}

struct BgSpan {
    start_col: usize,
    end_col: usize,
    color: Color32,
}

/// Merge per-cell background colours for one row into contiguous runs.
///
/// `cells` yields `(column, colour)` in column order, where `colour` is `None`
/// for the terminal's default background (nothing to paint). Adjacent cells
/// with the same non-default colour and contiguous columns become one run.
///
/// This is the single place background runs are built, so it is unit-tested —
/// an earlier inline version accidentally extended the *previous* run and made
/// backgrounds bleed across the row (nano/htop rendering bug).
fn bg_runs(cells: &[(usize, Option<Color32>)]) -> Vec<BgSpan> {
    let mut runs: Vec<BgSpan> = Vec::new();
    let mut current: Option<BgSpan> = None;

    for &(col, color) in cells {
        let Some(color) = color else {
            // Default background: close any open run.
            if let Some(run) = current.take() {
                runs.push(run);
            }
            continue;
        };

        if matches!(&current, Some(run) if run.color == color && run.end_col + 1 == col) {
            if let Some(run) = current.as_mut() {
                run.end_col = col;
            }
        } else {
            if let Some(run) = current.take() {
                runs.push(run);
            }
            current = Some(BgSpan {
                start_col: col,
                end_col: col,
                color,
            });
        }
    }

    if let Some(run) = current {
        runs.push(run);
    }
    runs
}

struct TermSize {
    cols: usize,
    rows: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// A selection endpoint in *grid* coordinates.
///
/// `line` is the value of `Indexed::point.line.0` — i.e. the viewport row with
/// the current scroll offset already subtracted — so the selection stays
/// anchored to the text when the user scrolls.
#[derive(Clone, Copy)]
struct SelPoint {
    line: i32,
    col: usize,
}

/// A command the terminal asks the app shell to carry out because it needs the
/// egui `Context` (clipboard) or the SSH channel, which `TerminalView` does not
/// own. Raised by the right-click context menu; see `TerminalView::take_action`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TermAction {
    Copy,
    Paste,
    SelectAll,
    ClearSelection,
}

/// A mouse text selection, stored as a drag anchor plus a moving cursor.
#[derive(Clone, Copy)]
struct Selection {
    anchor: SelPoint,
    cursor: SelPoint,
}

impl Selection {
    /// The endpoints ordered top-to-bottom, left-to-right (inclusive).
    fn ordered(self) -> (SelPoint, SelPoint) {
        if (self.anchor.line, self.anchor.col) <= (self.cursor.line, self.cursor.col) {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }
}

/// Painted width of the terminal's custom scrollbar thumb when idle.
const SCROLLBAR_WIDTH: f32 = 4.0;
/// Painted width while the pointer hovers the bar or drags it.
const SCROLLBAR_HOVER_WIDTH: f32 = 6.0;
/// Wider, invisible strip used for hit-testing so the thin thumb is easy to
/// grab with the pointer.
const SCROLLBAR_HIT_WIDTH: f32 = 12.0;

/// Geometry of the terminal's custom scrollbar for one frame.
///
/// The terminal paints its own scrollbar rather than using egui's
/// `ScrollArea`, so it also owns the pointer interaction: the thumb is drawn
/// on the right edge and a wider strip is registered with `ui.interact`.
struct ScrollbarGeometry {
    /// Invisible, widened strip that captures pointer input.
    hit_rect: Rect,
    /// Right edge the thumb is aligned to.
    right: f32,
    /// Top of the thumb; its height never changes.
    thumb_top: f32,
    /// Top of the track, cached for the pointer-to-offset conversion.
    track_top: f32,
    /// Distance the thumb can travel from top to bottom.
    travel: f32,
    /// Height of the thumb.
    thumb_height: f32,
    /// Total scrollback lines, to convert a thumb position into an offset.
    history_size: usize,
}

impl ScrollbarGeometry {
    fn new(rect: Rect, rows: usize, history_size: usize, display_offset: usize) -> Option<Self> {
        if history_size == 0 {
            return None;
        }

        let track_h = rect.height();
        let view_fraction = rows as f32 / (history_size + rows) as f32;
        let thumb_height = (track_h * view_fraction).max(16.0).min(track_h);
        let travel = (track_h - thumb_height).max(1.0);
        // display_offset == history_size is the top of the scrollback;
        // display_offset == 0 is the live bottom.
        let from_top = (history_size - display_offset) as f32 / history_size as f32;
        let thumb_y = rect.top() + from_top.clamp(0.0, 1.0) * travel;

        Some(Self {
            hit_rect: Rect::from_min_max(
                Pos2::new(rect.right() - SCROLLBAR_HIT_WIDTH, rect.top()),
                Pos2::new(rect.right(), rect.bottom()),
            ),
            right: rect.right(),
            thumb_top: thumb_y,
            track_top: rect.top(),
            travel,
            thumb_height,
            history_size,
        })
    }

    /// The visible thumb at the given width, aligned to the right edge.
    fn thumb_rect(&self, width: f32) -> Rect {
        Rect::from_min_size(
            Pos2::new(self.right - width, self.thumb_top),
            Vec2::new(width, self.thumb_height),
        )
    }

    /// Grab offset (within the thumb) to use when a drag starts at `pos`.
    ///
    /// Grabbing the thumb keeps its current position; clicking the empty track
    /// centres the thumb under the pointer.
    fn grab_offset(&self, pos: Pos2, width: f32) -> f32 {
        if self.thumb_rect(width).contains(pos) {
            pos.y - self.thumb_top
        } else {
            self.thumb_height * 0.5
        }
    }

    /// Convert a pointer y (offset by `grab`) into a display offset.
    fn display_offset_at(&self, pointer_y: f32, grab: f32) -> usize {
        let frac = ((pointer_y - grab - self.track_top) / self.travel).clamp(0.0, 1.0);
        ((1.0 - frac) * self.history_size as f32).round() as usize
    }
}

/// Map a pointer position to a grid point, clamped to the visible grid.
fn point_at(
    rect: Rect,
    pos: Pos2,
    display_offset: usize,
    cols: usize,
    rows: usize,
    char_width: f32,
    char_height: f32,
) -> SelPoint {
    let max_col = cols.saturating_sub(1) as f32;
    let max_row = rows.saturating_sub(1) as f32;
    let col = ((pos.x - rect.left()) / char_width)
        .floor()
        .clamp(0.0, max_col) as usize;
    let row = ((pos.y - rect.top()) / char_height)
        .floor()
        .clamp(0.0, max_row) as usize;
    SelPoint {
        line: row as i32 - display_offset as i32,
        col,
    }
}

/// xterm mouse button number for an egui pointer button.
fn mouse_button_number(button: PointerButton) -> u8 {
    match button {
        PointerButton::Primary => 0,
        PointerButton::Middle => 1,
        PointerButton::Secondary => 2,
        PointerButton::Extra1 => 8,
        PointerButton::Extra2 => 9,
    }
}

/// xterm mouse modifier bits: Shift = 4, Alt = 8, Ctrl = 16.
fn mouse_modifier_bits(m: Modifiers) -> u8 {
    (m.shift as u8) * 4 + (m.alt as u8) * 8 + (m.ctrl as u8) * 16
}

/// xterm mouse code for a wheel event: 64 = up, 65 = down.
///
/// The xterm spec defines the wheel as buttons 4 and 5 *plus 64*; sending the
/// bare 4/5 is decoded by ncurses as a left/middle **button click**, so the
/// wheel would select rows instead of scrolling.
fn wheel_button_code(up: bool) -> u8 {
    if up {
        64
    } else {
        65
    }
}

#[derive(Clone)]
struct EventProxy {
    input_tx: Arc<Mutex<mpsc::UnboundedSender<Vec<u8>>>>,
    title: Arc<Mutex<Option<String>>>,
    bell: Arc<AtomicBool>,
    grid_size: Arc<Mutex<(usize, usize, f32, f32)>>,
}

impl alacritty_terminal::event::EventListener for EventProxy {
    fn send_event(&self, event: alacritty_terminal::event::Event) {
        match event {
            alacritty_terminal::event::Event::PtyWrite(text) => {
                let _ = self.input_tx.lock().unwrap().send(text.into_bytes());
            }
            alacritty_terminal::event::Event::Title(title) => {
                if let Ok(mut slot) = self.title.lock() {
                    *slot = Some(title);
                }
            }
            alacritty_terminal::event::Event::Bell => {
                self.bell.store(true, Ordering::Relaxed);
            }
            alacritty_terminal::event::Event::TextAreaSizeRequest(format) => {
                if let Ok(size) = self.grid_size.lock() {
                    // let size_info = alacritty_terminal::term::SizeInfo {
                    //     width: size.0 as f32 * size.2,
                    //     height: size.1 as f32 * size.3,
                    //     cell_width: size.2,
                    //      cell_height: size.3,
                    //     padding_x: 0.0,
                    //     padding_y: 0.0,
                    //     dpr: 1.0,
                    // };

                    let size_info = alacritty_terminal::event::WindowSize {
                        num_lines: size.1 as u16,   // Number of rows
                        num_cols: size.0 as u16,    // Number of columns
                        cell_width: size.2 as u16,  // Individual cell pixel width
                        cell_height: size.3 as u16, // Individual cell pixel height
                    };
                    let reply = format(size_info);
                    let _ = self.input_tx.lock().unwrap().send(reply.into_bytes());
                }
            }
            alacritty_terminal::event::Event::ColorRequest(index, format) => {
                let color = indexed_color_to_egui(index as u8);
                // Rgb is located in vte::ansi
                let rgb = alacritty_terminal::vte::ansi::Rgb {
                    r: color.r(),
                    g: color.g(),
                    b: color.b(),
                };
                let reply = format(rgb);
                let _ = self.input_tx.lock().unwrap().send(reply.into_bytes());
            }
            other => {
                log::debug!("unhandled terminal event: {other:?}");
            }
        }
    }
}

/// Longest OSC 7 body kept. A real `file://` URL is far shorter; the cap stops
/// a broken or hostile stream from growing the buffer without bound.
const OSC7_MAX: usize = 4096;

/// Streaming scanner for OSC 7 (`ESC ] 7 ; file://host/path BEL|ST`).
///
/// `alacritty_terminal` does not expose OSC 7, so the directory is read from
/// the raw byte stream in parallel with the VTE parser. State is kept between
/// `feed` calls: a sequence split across two network chunks is still seen.
#[derive(Debug, Default)]
struct Osc7Scanner {
    state: Osc7State,
    /// Digits after `ESC ]`, up to the first `;`.
    code: Vec<u8>,
    /// The `file://…` body once the code is known to be 7.
    body: Vec<u8>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Osc7State {
    #[default]
    Ground,
    /// Saw `ESC`.
    Esc,
    /// Saw `ESC ]`.
    Osc,
    /// Reading the numeric code before `;`.
    Code,
    /// Reading the body after `ESC ] 7 ;`.
    Body,
    /// Saw `ESC` inside the body, waiting for `\` (the ST terminator).
    BodyEsc,
}

impl Osc7Scanner {
    /// Feed one byte; returns a decoded path when a complete sequence ends.
    fn advance(&mut self, byte: u8) -> Option<String> {
        match self.state {
            Osc7State::Ground => {
                if byte == 0x1b {
                    self.state = Osc7State::Esc;
                }
            }
            Osc7State::Esc => match byte {
                b']' => {
                    self.code.clear();
                    self.state = Osc7State::Osc;
                }
                0x1b => {}
                _ => self.state = Osc7State::Ground,
            },
            Osc7State::Osc => match byte {
                b'0'..=b'9' => {
                    self.code.push(byte);
                    self.state = Osc7State::Code;
                }
                0x07 => self.state = Osc7State::Ground,
                0x1b => self.state = Osc7State::Esc,
                _ => self.state = Osc7State::Ground,
            },
            Osc7State::Code => match byte {
                b'0'..=b'9' => {
                    // A huge code can never be 7; bail out early.
                    if self.code.len() >= 8 {
                        self.state = Osc7State::Ground;
                    } else {
                        self.code.push(byte);
                    }
                }
                b';' => {
                    if self.code == b"7" {
                        self.body.clear();
                        self.state = Osc7State::Body;
                    } else {
                        self.state = Osc7State::Ground;
                    }
                }
                0x07 => self.state = Osc7State::Ground,
                0x1b => self.state = Osc7State::Esc,
                _ => self.state = Osc7State::Ground,
            },
            Osc7State::Body => match byte {
                0x07 | 0x9c => return self.finish(),
                0x1b => self.state = Osc7State::BodyEsc,
                _ => {
                    if self.body.len() < OSC7_MAX {
                        self.body.push(byte);
                    } else {
                        // Too long to be a path we would act on.
                        self.state = Osc7State::Ground;
                    }
                }
            },
            Osc7State::BodyEsc => {
                if byte == b'\\' {
                    return self.finish();
                }
                // Some other escape aborted the OSC.
                self.state = Osc7State::Ground;
            }
        }
        None
    }

    fn finish(&mut self) -> Option<String> {
        self.state = Osc7State::Ground;
        parse_osc7(&self.body)
    }
}

/// Decode an OSC 7 body (`file://host/path`, percent-encoded) into a path.
///
/// The host is ignored: the panel only ever browses the session's own server.
/// A bare absolute path is accepted too, because a few shells emit one.
fn parse_osc7(body: &[u8]) -> Option<String> {
    let body = std::str::from_utf8(body).ok()?;
    let path = if let Some(rest) = body.strip_prefix("file://") {
        // `file:///path` has an empty host; `file://host/path` has a name.
        let slash = rest.find('/')?;
        &rest[slash..]
    } else if body.starts_with('/') {
        body
    } else {
        return None;
    };
    let path = percent_decode(path.as_bytes());
    if path.is_empty() || !path.starts_with('/') {
        None
    } else {
        Some(path)
    }
}

/// Percent-decode a path, leaving malformed escapes as literal text.
fn percent_decode(bytes: &[u8]) -> String {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = hex_value(bytes[i + 1]);
            let lo = hex_value(bytes[i + 2]);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push(hi << 4 | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

pub struct TerminalView {
    term: Term<EventProxy>,
    processor: Processor<StdSyncHandler>,
    /// Shared handle to the current PTY input channel; swapped on reconnect.
    input_tx: Arc<Mutex<mpsc::UnboundedSender<Vec<u8>>>>,
    /// OSC 7 extractor; runs alongside the VTE parser.
    osc7: Osc7Scanner,
    /// Most recent directory reported by the shell, waiting for the app.
    pending_cwd: Option<String>,
    cols: usize,
    rows: usize,
    font_size: f32,
    cell_size: Option<(f32, f32)>,
    scroll_accum: f32,
    pending_resize: Option<(u32, u32)>,
    title: Arc<Mutex<Option<String>>>,
    bell: Arc<AtomicBool>,
    grid_size: Arc<Mutex<(usize, usize, f32, f32)>>,
    selection: Option<Selection>,
    /// Set by the context menu, consumed by the app shell via `take_action`.
    pending_action: Option<TermAction>,
    /// Tracks alt-screen transitions so a stale selection is dropped.
    last_alt_screen: bool,
    /// Per-row galley cache to avoid rebuilding LayoutJobs for unchanged rows.
    row_cache: Vec<Option<RowCache>>,
    /// Font size when the cache was last populated; cleared on change.
    last_font_size: f32,
    /// Bytes destined for the PTY, produced by the terminal itself (currently
    /// xterm mouse reports). Drained by the app shell via `take_input`.
    pending_input: Vec<u8>,
    /// Last motion report `(col, row, code)` so identical motion is not repeated.
    last_motion: Option<(usize, usize, u8)>,
    /// Offset inside the scrollbar thumb where a drag grabbed it, so the thumb
    /// does not jump under the pointer.
    scrollbar_grab: Option<f32>,
}

impl TerminalView {
    /// Swap the channel that keystrokes are written to. Used on reconnect, so
    /// the existing view (and its scrollback) keeps working against the new
    /// session instead of being replaced.
    pub fn set_input_tx(&mut self, input_tx: mpsc::UnboundedSender<Vec<u8>>) {
        *self.input_tx.lock().unwrap() = input_tx;
    }

    pub fn new(cols: usize, rows: usize, input_tx: mpsc::UnboundedSender<Vec<u8>>) -> Self {
        let title = Arc::new(Mutex::new(None));
        let bell = Arc::new(AtomicBool::new(false));
        let grid_size = Arc::new(Mutex::new((cols, rows, 8.4, 16.8)));
        let input_tx = Arc::new(Mutex::new(input_tx));
        let event_proxy = EventProxy {
            input_tx: Arc::clone(&input_tx),
            title: Arc::clone(&title),
            bell: Arc::clone(&bell),
            grid_size: Arc::clone(&grid_size),
        };

        let size = TermSize { cols, rows };
        let term = Term::new(TermConfig::default(), &size, event_proxy);
        Self {
            term,
            processor: Processor::new(),
            input_tx,
            osc7: Osc7Scanner::default(),
            pending_cwd: None,
            cols,
            rows,
            font_size: 14.0,
            cell_size: None,
            scroll_accum: 0.0,
            pending_resize: None,
            title,
            bell,
            grid_size,
            selection: None,
            pending_action: None,
            last_alt_screen: false,
            row_cache: Vec::new(),
            last_font_size: 14.0,
            pending_input: Vec::new(),
            last_motion: None,
            scrollbar_grab: None,
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.processor.advance(&mut self.term, byte);
            if let Some(cwd) = self.osc7.advance(byte) {
                self.pending_cwd = Some(cwd);
            }
        }
    }

    /// Take the latest working directory reported by the shell via OSC 7.
    pub fn take_cwd(&mut self) -> Option<String> {
        self.pending_cwd.take()
    }

    /// Whether the terminal is showing an alternate screen (vim, htop, …).
    /// Typing into the shell is wrong while one of those is running.
    pub fn is_alt_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        self.term.resize(TermSize { cols, rows });
        self.pending_resize = Some((cols as u32, rows as u32));
    }

    pub fn take_resize(&mut self) -> Option<(u32, u32)> {
        self.pending_resize.take()
    }

    pub fn take_title(&self) -> Option<String> {
        self.title.lock().ok().and_then(|mut slot| slot.take())
    }

    pub fn take_bell(&self) -> bool {
        self.bell.swap(false, Ordering::Relaxed)
    }

    /// Drop any active mouse selection (e.g. after the user types).
    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// Take the pending context-menu action, if any.
    pub fn take_action(&mut self) -> Option<TermAction> {
        self.pending_action.take()
    }

    /// Take any bytes the terminal queued for the PTY (xterm mouse reports).
    pub fn take_input(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending_input)
    }

    /// True when the remote program enabled DECCKM (application cursor keys).
    ///
    /// ncurses programs such as htop turn this on via terminfo's `smkx`
    /// capability and then only understand SS3 cursor keys (`ESC O A`).
    pub fn app_cursor_mode(&self) -> bool {
        self.term.mode().contains(TermMode::APP_CURSOR)
    }

    /// Select every cell on the visible screen.
    pub fn select_all(&mut self) {
        let offset = self.term.grid().display_offset() as i32;
        self.selection = Some(Selection {
            anchor: SelPoint {
                line: -offset,
                col: 0,
            },
            cursor: SelPoint {
                line: self.rows as i32 - 1 - offset,
                col: self.cols.saturating_sub(1),
            },
        });
    }

    /// The currently selected text, with trailing whitespace trimmed per line.
    ///
    /// Returns `None` when nothing (or only whitespace) is selected. Only the
    /// visible part of the grid is read, so a selection that has been scrolled
    /// off-screen yields whatever remains visible — acceptable for the MVP.
    pub fn selected_text(&self) -> Option<String> {
        let selection = self.selection?;
        let (start, end) = selection.ordered();

        let content = self.term.renderable_content();
        let mut text = String::new();
        let mut current_line: Option<i32> = None;

        for indexed in content.display_iter {
            let line = indexed.point.line.0;
            let col = indexed.point.column.0;
            if (line, col) < (start.line, start.col) || (line, col) > (end.line, end.col) {
                continue;
            }
            if indexed
                .cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            if current_line != Some(line) {
                if current_line.is_some() {
                    text.push('\n');
                }
                current_line = Some(line);
            }
            let ch = indexed.cell.c;
            text.push(if ch == '\0' { ' ' } else { ch });
        }

        if text.trim().is_empty() {
            return None;
        }

        let trimmed = text
            .lines()
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n");
        Some(trimmed)
    }

    pub fn scroll(&mut self, lines: i32) {
        if lines != 0 {
            self.term.scroll_display(Scroll::Delta(lines));
        }
    }

    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    /// Move the viewport to an absolute scrollback offset (0 = live bottom).
    fn scroll_to_offset(&mut self, offset: usize) {
        let current = self.term.grid().display_offset();
        let delta = offset as i64 - current as i64;
        if delta != 0 {
            self.term.scroll_display(Scroll::Delta(delta as i32));
        }
    }

    pub fn handle_scroll_event(&mut self, event: &egui::Event) -> bool {
        let egui::Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } = event
        else {
            return false;
        };
        if !modifiers.shift || modifiers.ctrl || modifiers.alt {
            return false;
        }

        let page = self.rows.saturating_sub(1).max(1) as i32;
        match key {
            egui::Key::PageUp => {
                self.scroll(page);
                true
            }
            egui::Key::PageDown => {
                self.scroll(-page);
                true
            }
            egui::Key::Home => {
                self.term.scroll_display(Scroll::Top);
                true
            }
            egui::Key::End => {
                self.term.scroll_display(Scroll::Bottom);
                true
            }
            _ => false,
        }
    }

    /// Map a pointer position to a 0-based grid cell, clamped to the grid.
    fn pointer_cell(
        &self,
        pos: Pos2,
        rect: Rect,
        char_width: f32,
        char_height: f32,
    ) -> (usize, usize) {
        let col = ((pos.x - rect.left()) / char_width).floor().max(0.0);
        let row = ((pos.y - rect.top()) / char_height).floor().max(0.0);
        (
            (col as usize).min(self.cols.saturating_sub(1)),
            (row as usize).min(self.rows.saturating_sub(1)),
        )
    }

    /// Append one xterm mouse report to `pending_input`.
    ///
    /// `code` is the button/motion code with modifier bits already added. SGR
    /// (`?1006`) uses the `CSI < b ; x ; y M/m` form; the legacy form uses a
    /// fixed `CSI M` plus three offset bytes.
    fn encode_mouse(&mut self, sgr: bool, code: u8, col: usize, row: usize, release: bool) {
        let x = col as u32 + 1;
        let y = row as u32 + 1;
        if sgr {
            let final_byte = if release { 'm' } else { 'M' };
            self.pending_input
                .extend_from_slice(format!("\x1b[<{code};{x};{y}{final_byte}").as_bytes());
        } else {
            self.pending_input.extend_from_slice(&[
                0x1b,
                b'[',
                b'M',
                code.saturating_add(32),
                (x.min(223) as u8).saturating_add(32),
                (y.min(223) as u8).saturating_add(32),
            ]);
        }
    }

    /// Forward this frame's pointer activity to the remote app as xterm mouse
    /// reports (handoff F17). Only called while the app has reporting enabled.
    fn forward_mouse(&mut self, ui: &egui::Ui, rect: Rect, char_width: f32, char_height: f32) {
        let mode = *self.term.mode();
        let sgr = mode.contains(TermMode::SGR_MOUSE);
        let report_drag = mode.contains(TermMode::MOUSE_DRAG);
        let report_motion = mode.contains(TermMode::MOUSE_MOTION);

        // Secondary is intentionally excluded: it stays local for our menu.
        let buttons = [
            PointerButton::Primary,
            PointerButton::Middle,
            PointerButton::Extra1,
            PointerButton::Extra2,
        ];

        let (pos, mods, scroll_y, moving, presses, releases, any_down) = ui.input(|i| {
            let presses: Vec<PointerButton> = buttons
                .iter()
                .copied()
                .filter(|b| i.pointer.button_pressed(*b))
                .collect();
            let releases: Vec<PointerButton> = buttons
                .iter()
                .copied()
                .filter(|b| i.pointer.button_released(*b))
                .collect();
            (
                i.pointer.interact_pos().or_else(|| i.pointer.latest_pos()),
                i.modifiers,
                i.raw_scroll_delta.y,
                i.pointer.delta() != Vec2::ZERO,
                presses,
                releases,
                i.pointer.any_down(),
            )
        });

        let Some(pos) = pos else {
            return;
        };
        if !rect.contains(pos) {
            return;
        }
        let (col, row) = self.pointer_cell(pos, rect, char_width, char_height);
        let bits = mouse_modifier_bits(mods);

        // Wheel -> buttons 4 (up) / 5 (down), one report per scrolled line.
        // xterm adds 64 to the wheel button codes (64 = up, 65 = down); sending
        // the bare 4/5 would be decoded by ncurses as a left/middle *click*.
        if scroll_y != 0.0 {
            self.scroll_accum += scroll_y;
            let lines = (self.scroll_accum / char_height).trunc();
            if lines != 0.0 {
                self.scroll_accum -= lines * char_height;
                let code = wheel_button_code(lines > 0.0) + bits;
                for _ in 0..(lines.abs() as usize).min(20) {
                    self.encode_mouse(sgr, code, col, row, false);
                }
            }
        }

        for button in presses {
            self.encode_mouse(sgr, mouse_button_number(button) + bits, col, row, false);
        }
        for button in releases {
            if sgr {
                // SGR signals release with the original button code + 'm'.
                self.encode_mouse(sgr, mouse_button_number(button) + bits, col, row, true);
            } else {
                // Legacy encoding reports any release as button 3.
                self.encode_mouse(sgr, 3 + bits, col, row, false);
            }
        }

        // Motion: ?1002 while a button is held, ?1003 always.
        if moving && (report_motion || (report_drag && any_down)) {
            let held = ui.input(|i| {
                buttons
                    .iter()
                    .copied()
                    .find(|b| i.pointer.button_down(*b))
                    .map(mouse_button_number)
            });
            let code = held.unwrap_or(3) + 32 + bits;
            if self.last_motion != Some((col, row, code)) {
                self.last_motion = Some((col, row, code));
                self.encode_mouse(sgr, code, col, row, false);
            }
        }
    }

    fn measure_cell(&mut self, ui: &egui::Ui) -> (f32, f32) {
        if let Some(size) = self.cell_size {
            return size;
        }
        let font_id = FontId::monospace(self.font_size);
        let (w, h) = ui.fonts(|f| {
            let mut job = LayoutJob::default();
            job.append(
                "MMMMMMMMMM",
                0.0,
                TextFormat {
                    font_id: font_id.clone(),
                    ..Default::default()
                },
            );
            let galley = f.layout_job(job);
            (galley.rect.width() / 10.0, f.row_height(&font_id))
        });
        if w > 0.0 && h > 0.0 {
            self.cell_size = Some((w, h));
            (w, h)
        } else {
            (self.font_size * 0.6, self.font_size * 1.2)
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) -> egui::Response {
        let available = ui.available_size();
        let (char_width, char_height) = self.measure_cell(ui);

        let new_cols = ((available.x / char_width).floor() as usize).max(20);
        let new_rows = ((available.y / char_height).floor() as usize).max(5);
        self.resize(new_cols, new_rows);

        // Dropping the selection when the screen buffer flips (alt-screen
        // enter/leave, e.g. vim/htop) avoids stale highlights over new content.
        let alt_screen = self.term.mode().contains(TermMode::ALT_SCREEN);
        if alt_screen != self.last_alt_screen {
            self.selection = None;
            self.last_alt_screen = alt_screen;
        }

        if let Ok(mut size) = self.grid_size.lock() {
            *size = (self.cols, self.rows, char_width, char_height);
        }

        let (rect, response) = ui.allocate_exact_size(available, egui::Sense::click_and_drag());

        // Right-click menu (MobaXterm-style). Actions that need the clipboard
        // or the SSH channel are recorded and carried out by `app.rs`.
        response.context_menu(|ui| {
            crate::theme::menu_style(ui);
            let has_selection = self.selection.is_some();
            if crate::theme::menu_item(ui, "Copy", has_selection, None).clicked() {
                self.pending_action = Some(TermAction::Copy);
                ui.close_menu();
            }
            if crate::theme::menu_item(ui, "Paste", true, None).clicked() {
                self.pending_action = Some(TermAction::Paste);
                ui.close_menu();
            }
            ui.separator();
            if crate::theme::menu_item(ui, "Select All", true, None).clicked() {
                self.pending_action = Some(TermAction::SelectAll);
                ui.close_menu();
            }
            if crate::theme::menu_item(ui, "Clear selection", has_selection, None).clicked() {
                self.pending_action = Some(TermAction::ClearSelection);
                ui.close_menu();
            }
        });

        // Mouse routing (handoff F17). When the remote app has enabled mouse
        // reporting (htop, vim with `:set mouse=a`, tmux, mc, lazygit, …) pointer
        // events are forwarded to it as xterm mouse reports. Holding Shift
        // forces local selection instead — the usual terminal convention. The
        // right button is always local, so the context menu stays reachable.
        let term_mode = *self.term.mode();
        let reporting = term_mode.intersects(TermMode::MOUSE_MODE);
        let forward_mouse = reporting && !ui.input(|i| i.modifiers.shift);

        // Custom scrollbar interaction. The terminal paints its own bar (see the
        // paint block below); registering a wider strip on top of the selection
        // surface lets the pointer drag or click it. egui resolves overlapping
        // widgets back-to-front, so this widget wins inside its strip while the
        // rest of the terminal still handles text selection. It is skipped while
        // the pointer is forwarded to a mouse-aware remote app.
        let history_size = self.term.grid().history_size();
        let scrollbar = ScrollbarGeometry::new(
            rect,
            self.rows,
            history_size,
            self.term.grid().display_offset(),
        );
        let scrollbar_response = if forward_mouse {
            None
        } else {
            scrollbar.as_ref().map(|sb| {
                ui.interact(
                    sb.hit_rect,
                    ui.id().with("terminal_scrollbar"),
                    egui::Sense::click_and_drag(),
                )
            })
        };

        // Grow the bar while it is hovered or dragged. The right edge stays
        // fixed, so the thumb widens leftwards like a normal scrollbar.
        let scrollbar_active = scrollbar_response
            .as_ref()
            .is_some_and(|r| r.hovered() || r.dragged() || r.is_pointer_button_down_on());
        let grow = ui.ctx().animate_bool_with_time(
            ui.id().with("terminal_scrollbar_hover"),
            scrollbar_active,
            0.12,
        );
        let scrollbar_width = SCROLLBAR_WIDTH + (SCROLLBAR_HOVER_WIDTH - SCROLLBAR_WIDTH) * grow;

        if let (Some(sb), Some(sbr)) = (&scrollbar, &scrollbar_response) {
            // egui surrenders keyboard focus when a press lands on a widget
            // covering the focused one, which the scrollbar strip does. Take it
            // back so clicking or dragging the bar never interrupts typing.
            if sbr.is_pointer_button_down_on() || sbr.drag_started() || sbr.clicked() {
                response.request_focus();
            }
            if sbr.drag_started() || sbr.clicked() {
                if let Some(pos) = sbr.interact_pointer_pos() {
                    self.scrollbar_grab = Some(sb.grab_offset(pos, scrollbar_width));
                }
            }
            if sbr.dragged() || sbr.clicked() {
                if let Some(pos) = sbr.interact_pointer_pos() {
                    let grab = self.scrollbar_grab.unwrap_or(sb.thumb_height * 0.5);
                    self.scroll_to_offset(sb.display_offset_at(pos.y, grab));
                }
            }
            if sbr.drag_stopped() {
                self.scrollbar_grab = None;
            }
        }

        if forward_mouse {
            self.forward_mouse(ui, rect, char_width, char_height);
        } else {
            // `contains_pointer` rather than `hovered` so the wheel still works
            // over the scrollbar strip, which sits on top of the terminal.
            if response.contains_pointer() {
                let dy = ui.input(|i| i.raw_scroll_delta.y);
                if dy != 0.0 {
                    self.scroll_accum += dy;
                    let lines = (self.scroll_accum / char_height).trunc();
                    if lines != 0.0 {
                        self.scroll_accum -= lines * char_height;
                        self.scroll(lines as i32);
                    }
                }
            }

            // Mouse text selection (handoff F14). A drag anchors at the press
            // point and extends to the pointer; a plain click clears it.
            if response.drag_started() {
                if let Some(pos) = response.interact_pointer_pos() {
                    let offset = self.term.grid().display_offset();
                    let point = point_at(
                        rect,
                        pos,
                        offset,
                        self.cols,
                        self.rows,
                        char_width,
                        char_height,
                    );
                    self.selection = Some(Selection {
                        anchor: point,
                        cursor: point,
                    });
                }
            }
            if response.dragged() {
                if let Some(pos) = response.interact_pointer_pos() {
                    let offset = self.term.grid().display_offset();
                    let point = point_at(
                        rect,
                        pos,
                        offset,
                        self.cols,
                        self.rows,
                        char_width,
                        char_height,
                    );
                    if let Some(selection) = self.selection.as_mut() {
                        selection.cursor = point;
                    }
                }
            }
            if response.clicked() {
                self.selection = None;
            }
        }

        if ui.is_rect_visible(rect) {
            let painter = ui.painter_at(rect);
            painter.rect_filled(rect, 0.0, DEFAULT_BG);

            let font_id = FontId::monospace(self.font_size);
            let rows = self.rows;
            let cols = self.cols;

            // Keep the row cache sized to the viewport and invalidate it when
            // the font changes (galleys are font-specific).
            if self.row_cache.len() != rows {
                self.row_cache = vec![None; rows];
            }
            if (self.font_size - self.last_font_size).abs() > f32::EPSILON {
                self.row_cache.fill(None);
                self.last_font_size = self.font_size;
            }

            let content = self.term.renderable_content();
            let display_offset = content.display_offset;
            let cursor_point = content.cursor.point;
            let history_size = self.term.grid().history_size();

            let mut row: Option<usize> = None;
            let mut text = String::with_capacity(cols);
            let mut runs: Vec<(usize, usize, Color32)> = Vec::new();
            let mut cols_emitted = 0usize;
            let mut row_bgs: Vec<(usize, Option<Color32>)> = Vec::new();

            let flush_row = |painter: &egui::Painter,
                             rect: Rect,
                             row: usize,
                             char_height: f32,
                             char_width: f32,
                             bg_runs: &[BgSpan],
                             galley: Option<Arc<egui::text::Galley>>| {
                let y = rect.top() + row as f32 * char_height;
                for span in bg_runs {
                    let x0 = rect.left() + span.start_col as f32 * char_width;
                    let x1 = rect.left() + (span.end_col + 1) as f32 * char_width;
                    painter.rect_filled(
                        Rect::from_min_max(Pos2::new(x0, y), Pos2::new(x1, y + char_height)),
                        0.0,
                        span.color,
                    );
                }
                if let Some(galley) = galley {
                    painter.galley(Pos2::new(rect.left(), y), galley, Color32::PLACEHOLDER);
                }
            };

            for indexed in content.display_iter {
                let line = indexed.point.line.0 + display_offset as i32;
                if line < 0 || line as usize >= rows {
                    continue;
                }
                let line = line as usize;
                let col = indexed.point.column.0;
                if col >= cols {
                    continue;
                }

                if row != Some(line) {
                    if let Some(prev) = row {
                        let spans = bg_runs(&row_bgs);
                        let galley = if runs.is_empty() {
                            None
                        } else {
                            let cache_hit = self
                                .row_cache
                                .get(prev)
                                .and_then(|c| c.as_ref())
                                .map(|c| c.text == text && c.runs == *runs)
                                .unwrap_or(false);
                            if cache_hit {
                                Some(self.row_cache[prev].as_ref().unwrap().galley.clone())
                            } else {
                                let mut job = LayoutJob::default();
                                job.wrap.max_width = f32::INFINITY;
                                job.break_on_newline = false;
                                for (start, end, color) in &runs {
                                    job.append(
                                        &text[*start..*end],
                                        0.0,
                                        TextFormat {
                                            font_id: font_id.clone(),
                                            color: *color,
                                            ..Default::default()
                                        },
                                    );
                                }
                                let g = ui.fonts(|f| f.layout_job(job));
                                if self.row_cache.len() > prev {
                                    self.row_cache[prev] = Some(RowCache {
                                        text: text.clone(),
                                        runs: runs.clone(),
                                        galley: g.clone(),
                                    });
                                }
                                Some(g)
                            }
                        };
                        flush_row(
                            &painter,
                            rect,
                            prev,
                            char_height,
                            char_width,
                            &spans,
                            galley,
                        );
                    }
                    row = Some(line);
                    text.clear();
                    runs.clear();
                    cols_emitted = 0;
                    row_bgs.clear();
                }

                let cell = indexed.cell;

                // Record the background for every cell — including wide-char
                // spacers — so a coloured run stays contiguous under wide glyphs.
                let bg = if cell.flags.contains(Flags::INVERSE) {
                    cell.fg
                } else {
                    cell.bg
                };
                let bg_color = color_to_egui(bg);
                let is_default_bg =
                    matches!(bg, Color::Named(NamedColor::Background)) || bg_color == DEFAULT_BG;
                row_bgs.push((col, if is_default_bg { None } else { Some(bg_color) }));

                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }

                let color = if cell.flags.contains(Flags::INVERSE) {
                    color_to_egui(cell.bg)
                } else {
                    color_to_egui(cell.fg)
                };

                while cols_emitted < col {
                    push_char(&mut text, &mut runs, ' ', color);
                    cols_emitted += 1;
                }

                let mut ch = cell.c;
                if ch == '\0' || cell.flags.contains(Flags::HIDDEN) {
                    ch = ' ';
                }
                push_char(&mut text, &mut runs, ch, color);
                cols_emitted += if cell.flags.contains(Flags::WIDE_CHAR) {
                    2
                } else {
                    1
                };
            }

            if let Some(prev) = row {
                let spans = bg_runs(&row_bgs);
                let galley = if runs.is_empty() {
                    None
                } else {
                    let cache_hit = self
                        .row_cache
                        .get(prev)
                        .and_then(|c| c.as_ref())
                        .map(|c| c.text == text && c.runs == *runs)
                        .unwrap_or(false);
                    if cache_hit {
                        Some(self.row_cache[prev].as_ref().unwrap().galley.clone())
                    } else {
                        let mut job = LayoutJob::default();
                        job.wrap.max_width = f32::INFINITY;
                        job.break_on_newline = false;
                        for (start, end, color) in &runs {
                            job.append(
                                &text[*start..*end],
                                0.0,
                                TextFormat {
                                    font_id: font_id.clone(),
                                    color: *color,
                                    ..Default::default()
                                },
                            );
                        }
                        let g = ui.fonts(|f| f.layout_job(job));
                        if self.row_cache.len() > prev {
                            self.row_cache[prev] = Some(RowCache {
                                text: text.clone(),
                                runs: runs.clone(),
                                galley: g.clone(),
                            });
                        }
                        Some(g)
                    }
                };
                flush_row(
                    &painter,
                    rect,
                    prev,
                    char_height,
                    char_width,
                    &spans,
                    galley,
                );
            }

            // Selection highlight, painted over the backgrounds (and text) so it
            // stays visible on colour-rich screens such as htop and tmux.
            if let Some(selection) = self.selection {
                let (start, end) = selection.ordered();
                let start_row = start.line + display_offset as i32;
                let end_row = end.line + display_offset as i32;
                let last_row = rows as i32 - 1;
                let highlight = Color32::from_rgba_unmultiplied(80, 120, 200, 110);
                for view_row in start_row.max(0)..=end_row.min(last_row) {
                    let (first_col, last_col) = if start_row == end_row {
                        (start.col, end.col)
                    } else if view_row == start_row {
                        (start.col, cols - 1)
                    } else if view_row == end_row {
                        (0, end.col)
                    } else {
                        (0, cols - 1)
                    };
                    let x0 = rect.left() + first_col as f32 * char_width;
                    let x1 = rect.left() + (last_col + 1) as f32 * char_width;
                    let y0 = rect.top() + view_row as f32 * char_height;
                    painter.rect_filled(
                        Rect::from_min_max(Pos2::new(x0, y0), Pos2::new(x1, y0 + char_height)),
                        0.0,
                        highlight,
                    );
                }
            }

            if display_offset == 0 {
                let cx = rect.left() + cursor_point.column.0 as f32 * char_width;
                let cy = rect.top() + cursor_point.line.0 as f32 * char_height;
                painter.rect_filled(
                    Rect::from_min_size(Pos2::new(cx, cy), Vec2::new(char_width, char_height)),
                    0.0,
                    Color32::from_rgba_unmultiplied(100, 140, 255, 100),
                );
            }

            if let Some(sb) = ScrollbarGeometry::new(rect, rows, history_size, display_offset) {
                // Show the thumb while scrolled back, while the terminal is
                // hovered, or while the pointer is interacting with the bar.
                let bar_visible = display_offset > 0
                    || response.hovered()
                    || scrollbar_response
                        .as_ref()
                        .is_some_and(|r| r.hovered() || r.dragged());
                if bar_visible {
                    painter.rect_filled(
                        sb.thumb_rect(scrollbar_width),
                        2.0,
                        Color32::from_rgba_unmultiplied(150, 150, 165, 140),
                    );
                }
            }
        }

        response
    }
}

fn push_char(text: &mut String, runs: &mut Vec<(usize, usize, Color32)>, ch: char, color: Color32) {
    let start = text.len();
    text.push(ch);
    let end = text.len();

    if let Some(last) = runs.last_mut() {
        if last.2 == color && last.1 == start {
            last.1 = end;
            return;
        }
    }
    runs.push((start, end, color));
}

#[allow(clippy::too_many_arguments)]
#[allow(dead_code)]
fn paint_row(
    ui: &egui::Ui,
    painter: &egui::Painter,
    rect: Rect,
    row: usize,
    char_height: f32,
    text: &str,
    runs: &[(usize, usize, Color32)],
    font_id: &FontId,
) {
    if runs.is_empty() {
        return;
    }

    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    job.break_on_newline = false;

    for &(start, end, color) in runs {
        job.append(
            &text[start..end],
            0.0,
            TextFormat {
                font_id: font_id.clone(),
                color,
                ..Default::default()
            },
        );
    }

    let galley = ui.fonts(|f| f.layout_job(job));
    painter.galley(
        Pos2::new(rect.left(), rect.top() + row as f32 * char_height),
        galley,
        Color32::PLACEHOLDER,
    );
}

fn color_to_egui(color: Color) -> Color32 {
    match color {
        Color::Named(named) => named_color_to_egui(named),
        Color::Spec(rgb) => Color32::from_rgb(rgb.r, rgb.g, rgb.b),
        Color::Indexed(idx) => indexed_color_to_egui(idx),
    }
}

fn named_color_to_egui(named: NamedColor) -> Color32 {
    use NamedColor::*;
    match named {
        Black => Color32::from_rgb(0, 0, 0),
        Red => Color32::from_rgb(205, 49, 49),
        Green => Color32::from_rgb(13, 188, 121),
        Yellow => Color32::from_rgb(229, 229, 16),
        Blue => Color32::from_rgb(36, 114, 200),
        Magenta => Color32::from_rgb(188, 63, 188),
        Cyan => Color32::from_rgb(17, 168, 205),
        White => Color32::from_rgb(229, 229, 229),
        BrightBlack => Color32::from_rgb(102, 102, 102),
        BrightRed => Color32::from_rgb(241, 76, 76),
        BrightGreen => Color32::from_rgb(35, 209, 139),
        BrightYellow => Color32::from_rgb(245, 245, 67),
        BrightBlue => Color32::from_rgb(59, 142, 234),
        BrightMagenta => Color32::from_rgb(214, 112, 214),
        BrightCyan => Color32::from_rgb(41, 184, 219),
        BrightWhite => Color32::from_rgb(255, 255, 255),
        // The terminal's *default* foreground and background must differ (light
        // vs dark). Mapping both to the same grey made reverse-video cells
        // white-on-white — invisible text in nano/htop/etc.
        Foreground | BrightForeground => DEFAULT_FG,
        Background => DEFAULT_BG,
        Cursor => Color32::from_rgb(100, 140, 255),
        // "Dim" variants are approximated by their base colour.
        DimBlack => Color32::from_rgb(0, 0, 0),
        DimRed => Color32::from_rgb(205, 49, 49),
        DimGreen => Color32::from_rgb(13, 188, 121),
        DimYellow => Color32::from_rgb(229, 229, 16),
        DimBlue => Color32::from_rgb(36, 114, 200),
        DimMagenta => Color32::from_rgb(188, 63, 188),
        DimCyan => Color32::from_rgb(17, 168, 205),
        DimWhite => Color32::from_rgb(229, 229, 229),
        DimForeground => DEFAULT_FG,
    }
}

fn indexed_color_to_egui(idx: u8) -> Color32 {
    // Standard xterm 256-colour palette: 0-15 are the system colours, 16-231 a
    // 6x6x6 cube on the canonical channel values, 232-255 a 24-step grey ramp.
    const CUBE: [u8; 6] = [0, 95, 135, 175, 215, 255];

    if idx < 16 {
        let named = match idx {
            0 => NamedColor::Black,
            1 => NamedColor::Red,
            2 => NamedColor::Green,
            3 => NamedColor::Yellow,
            4 => NamedColor::Blue,
            5 => NamedColor::Magenta,
            6 => NamedColor::Cyan,
            7 => NamedColor::White,
            8 => NamedColor::BrightBlack,
            9 => NamedColor::BrightRed,
            10 => NamedColor::BrightGreen,
            11 => NamedColor::BrightYellow,
            12 => NamedColor::BrightBlue,
            13 => NamedColor::BrightMagenta,
            14 => NamedColor::BrightCyan,
            _ => NamedColor::BrightWhite,
        };
        named_color_to_egui(named)
    } else if idx < 232 {
        let i = (idx - 16) as usize;
        Color32::from_rgb(CUBE[i / 36], CUBE[(i % 36) / 6], CUBE[i % 6])
    } else {
        let level = 8 + (idx - 232) * 10;
        Color32::from_rgb(level, level, level)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb(r: u8, g: u8, b: u8) -> Color32 {
        Color32::from_rgb(r, g, b)
    }

    #[test]
    fn bg_runs_merges_contiguous_same_colour() {
        let cells = [
            (0, None),
            (1, Some(rgb(255, 0, 0))),
            (2, Some(rgb(255, 0, 0))),
            (3, None),
        ];
        let runs = bg_runs(&cells);
        assert_eq!(runs.len(), 1);
        assert_eq!((runs[0].start_col, runs[0].end_col), (1, 2));
    }

    /// Regression for the nano/htop bug: two runs of the *same* colour separated
    /// by another colour must stay separate. The old inline code extended the
    /// previously pushed run, stretching backgrounds across unrelated cells.
    #[test]
    fn bg_runs_does_not_merge_same_colour_across_a_gap() {
        let red = Some(rgb(255, 0, 0));
        let white = Some(rgb(255, 255, 255));
        let cells = [(1, red), (2, red), (3, white), (4, red), (5, red)];
        let runs = bg_runs(&cells);
        assert_eq!(runs.len(), 3);
        assert_eq!(
            (runs[0].start_col, runs[0].end_col, runs[0].color),
            (1, 2, rgb(255, 0, 0))
        );
        assert_eq!(
            (runs[1].start_col, runs[1].end_col, runs[1].color),
            (3, 3, rgb(255, 255, 255))
        );
        assert_eq!(
            (runs[2].start_col, runs[2].end_col, runs[2].color),
            (4, 5, rgb(255, 0, 0))
        );
    }

    #[test]
    fn bg_runs_splits_non_contiguous_columns() {
        let cells = [(0, Some(rgb(255, 0, 0))), (2, Some(rgb(255, 0, 0)))];
        assert_eq!(bg_runs(&cells).len(), 2);
    }

    #[test]
    fn bg_runs_empty_when_all_default() {
        let cells = [(0, None), (1, None)];
        assert!(bg_runs(&cells).is_empty());
    }

    #[test]
    fn scrollbar_thumb_at_bottom_maps_to_live_offset() {
        let track = Rect::from_min_size(Pos2::ZERO, Vec2::new(100.0, 200.0));
        let sb = ScrollbarGeometry::new(track, 24, 100, 0).unwrap();
        // The thumb sits at the bottom when display_offset == 0.
        assert!((sb.thumb_rect(SCROLLBAR_WIDTH).bottom() - track.bottom()).abs() < 0.01);
        // Dragging the thumb to the bottom must return to the live view.
        assert_eq!(sb.display_offset_at(track.bottom(), sb.thumb_height), 0);
    }

    #[test]
    fn scrollbar_thumb_at_top_maps_to_oldest_line() {
        let track = Rect::from_min_size(Pos2::ZERO, Vec2::new(100.0, 200.0));
        let sb = ScrollbarGeometry::new(track, 24, 100, 100).unwrap();
        assert!((sb.thumb_rect(SCROLLBAR_WIDTH).top() - track.top()).abs() < 0.01);
        assert_eq!(sb.display_offset_at(track.top(), 0.0), 100);
    }

    #[test]
    fn scrollbar_grab_keeps_thumb_under_pointer() {
        let track = Rect::from_min_size(Pos2::ZERO, Vec2::new(100.0, 200.0));
        let sb = ScrollbarGeometry::new(track, 24, 100, 50).unwrap();
        let pos = sb.thumb_rect(SCROLLBAR_WIDTH).center();
        let grab = sb.grab_offset(pos, SCROLLBAR_WIDTH);
        assert_eq!(sb.display_offset_at(pos.y, grab), 50);
    }

    /// Reverse video with default colours must not be white-on-white.
    #[test]
    fn default_background_differs_from_default_foreground() {
        let bg = named_color_to_egui(NamedColor::Background);
        let fg = named_color_to_egui(NamedColor::Foreground);
        assert_ne!(bg, fg);
        assert!(bg.r() < fg.r(), "default bg must be darker than default fg");
    }

    #[test]
    fn indexed_greyscale_ramp_is_neutral() {
        assert_eq!(indexed_color_to_egui(232), Color32::from_rgb(8, 8, 8));
        assert_eq!(indexed_color_to_egui(255), Color32::from_rgb(238, 238, 238));
    }

    fn test_view() -> TerminalView {
        let (tx, _rx) = mpsc::unbounded_channel();
        TerminalView::new(80, 24, tx)
    }

    #[test]
    fn mouse_button_numbers_and_modifier_bits() {
        assert_eq!(mouse_button_number(PointerButton::Primary), 0);
        assert_eq!(mouse_button_number(PointerButton::Middle), 1);
        assert_eq!(mouse_button_number(PointerButton::Secondary), 2);
        assert_eq!(mouse_modifier_bits(Modifiers::NONE), 0);
        let all = Modifiers {
            shift: true,
            alt: true,
            ctrl: true,
            ..Default::default()
        };
        assert_eq!(mouse_modifier_bits(all), 28);
    }

    #[test]
    fn sgr_mouse_press_uses_1_based_coords() {
        let mut view = test_view();
        // Left press at grid (col 9, row 4) -> ESC [ < 0 ; 10 ; 5 M
        view.encode_mouse(true, 0, 9, 4, false);
        assert_eq!(view.take_input(), b"\x1b[<0;10;5M".to_vec());
    }

    #[test]
    fn sgr_mouse_release_uses_lowercase_m() {
        let mut view = test_view();
        view.encode_mouse(true, 0, 9, 4, true);
        assert_eq!(view.take_input(), b"\x1b[<0;10;5m".to_vec());
    }

    #[test]
    fn legacy_mouse_offsets_values_by_32() {
        let mut view = test_view();
        view.encode_mouse(false, 0, 9, 4, false);
        assert_eq!(view.take_input(), vec![0x1b, b'[', b'M', 32, 42, 37]);
    }

    #[test]
    fn take_input_drains_the_buffer() {
        let mut view = test_view();
        view.encode_mouse(true, 4, 0, 0, false);
        assert!(!view.take_input().is_empty());
        assert!(view.take_input().is_empty());
    }

    /// The wheel must be 64/65, not 4/5 — ncurses decodes 4/5 as a left or
    /// middle *click*, which made htop select rows instead of scrolling.
    #[test]
    fn wheel_button_codes_are_64_and_65() {
        assert_eq!(wheel_button_code(true), 64);
        assert_eq!(wheel_button_code(false), 65);
    }

    fn osc7(body: &str, bell: bool) -> Vec<u8> {
        let mut bytes = b"\x1b]7;".to_vec();
        bytes.extend_from_slice(body.as_bytes());
        if bell {
            bytes.push(0x07);
        } else {
            bytes.extend_from_slice(b"\x1b\\");
        }
        bytes
    }

    fn scan(bytes: &[u8]) -> Option<String> {
        let mut scanner = Osc7Scanner::default();
        bytes.iter().find_map(|&byte| scanner.advance(byte))
    }

    #[test]
    fn osc7_is_decoded_with_both_terminators() {
        assert_eq!(
            scan(&osc7("file://host/home/me", true)).as_deref(),
            Some("/home/me")
        );
        assert_eq!(
            scan(&osc7("file://host/home/me", false)).as_deref(),
            Some("/home/me")
        );
        // An empty host is the common form from many shells.
        assert_eq!(scan(&osc7("file:///tmp", true)).as_deref(), Some("/tmp"));
    }

    #[test]
    fn osc7_survives_every_chunk_split() {
        let sequence = osc7("file://host/home/me/my%20dir", true);
        for split in 0..=sequence.len() {
            let mut scanner = Osc7Scanner::default();
            let mut found = None;
            for &byte in &sequence[..split] {
                found = scanner.advance(byte).or(found);
            }
            for &byte in &sequence[split..] {
                found = scanner.advance(byte).or(found);
            }
            assert_eq!(
                found.as_deref(),
                Some("/home/me/my dir"),
                "split at byte {split}"
            );
        }
    }

    #[test]
    fn osc7_percent_decodes_utf8_and_keeps_bad_escapes() {
        assert_eq!(
            scan(&osc7("file://h/tmp/caf%C3%A9", true)).as_deref(),
            Some("/tmp/café")
        );
        assert_eq!(
            scan(&osc7("file://h/a%zzb", true)).as_deref(),
            Some("/a%zzb")
        );
        // A bare absolute path is accepted as a lenient fallback.
        assert_eq!(scan(&osc7("/var/log", true)).as_deref(), Some("/var/log"));
    }

    #[test]
    fn osc7_ignores_other_osc_sequences() {
        // Window title and clipboard: neither is a directory.
        assert_eq!(scan(b"\x1b]0;my title\x07"), None);
        assert_eq!(scan(b"\x1b]52;c;Zm9v\x07"), None);
        // OSC 7 without a path is dropped rather than navigating somewhere odd.
        assert_eq!(scan(&osc7("file://host", true)), None);
        assert_eq!(scan(&osc7("not-a-url", true)), None);
        // Plain text that merely mentions the sequence is not enough.
        assert_eq!(scan(b"echo ESC ] 7 ; file://host/tmp"), None);
    }

    #[test]
    fn osc7_rejects_an_oversized_body() {
        let mut body = String::from("file://h/");
        body.push_str(&"a".repeat(OSC7_MAX + 100));
        let mut sequence = b"\x1b]7;".to_vec();
        sequence.extend_from_slice(body.as_bytes());
        sequence.push(0x07);
        assert_eq!(scan(&sequence), None);
    }

    #[test]
    fn terminal_view_reports_the_shell_directory_once() {
        let mut view = test_view();
        view.feed(&osc7("file://host/home/me/project", true));
        assert_eq!(view.take_cwd().as_deref(), Some("/home/me/project"));
        assert_eq!(view.take_cwd(), None);

        // A later prompt moves it again, and a split chunk still works.
        let sequence = osc7("file://host/srv", false);
        let (first, second) = sequence.split_at(5);
        view.feed(first);
        view.feed(second);
        assert_eq!(view.take_cwd().as_deref(), Some("/srv"));
    }
}
