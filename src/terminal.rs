//! Terminal view: wraps alacritty_terminal::Term and renders it with egui.
//!
//! This is the riskiest part of the project; we reuse alacritty's VT engine
//! and only implement the rendering glue.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use egui::{Color32, FontId, Galley, Pos2, Rect, Stroke, Vec2};

/// Simple dimensions implementation for Term.
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

/// Event listener stub. We don't need most events for the scaffold.
struct EventProxy;

impl alacritty_terminal::event::EventListener for EventProxy {
    fn send_event(&self, _event: alacritty_terminal::event::Event) {}
}

/// The terminal state + rendering.
pub struct TerminalView {
    term: Term<EventProxy>,
    processor: Processor<StdSyncHandler>,
    cols: usize,
    rows: usize,
    font_size: f32,
    scroll_offset: i32,
}

impl TerminalView {
    pub fn new(cols: usize, rows: usize) -> Self {
        let size = TermSize { cols, rows };
        let config = TermConfig::default();
        let term = Term::new(config, &size, EventProxy);
        let processor = Processor::new();
        Self {
            term,
            processor,
            cols,
            rows,
            font_size: 14.0,
            scroll_offset: 0,
        }
    }

    /// Feed raw bytes from the SSH channel into the VT parser.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.processor.advance(&mut self.term, bytes);
    }

    /// Resize the terminal grid.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        let size = TermSize { cols, rows };
        self.term.resize(size);
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Render the terminal grid into the given egui UI, returning a response
    /// so the caller can attach keyboard focus.
    pub fn ui(&mut self, ui: &mut egui::Ui) -> egui::Response {
        let available = ui.available_size();
        let char_width = self.font_size * 0.6;
        let char_height = self.font_size * 1.2;

        // Compute grid size from available pixels.
        let new_cols = ((available.x / char_width).floor() as usize).max(20);
        let new_rows = ((available.y / char_height).floor() as usize).max(5);
        self.resize(new_cols, new_rows);

        let (rect, response) = ui.allocate_exact_size(available, egui::Sense::click_and_drag());

        if ui.is_rect_visible(rect) {
            let painter = ui.painter_at(rect);
            painter.rect_filled(rect, 0.0, Color32::from_rgb(20, 20, 25));

            let font_id = FontId::monospace(self.font_size);
            let grid = self.term.grid();

            for row in 0..self.rows {
                for col in 0..self.cols {
                    let cell = &grid[alacritty_terminal::index::Line(row as i32)
                        + alacritty_terminal::index::Column(col)];

                    let ch = cell.c;
                    if ch == ' ' || ch == '\0' {
                        continue;
                    }

                    let x = rect.left() + col as f32 * char_width;
                    let y = rect.top() + row as f32 * char_height;

                    let color = cell.fg;
                    let egui_color = match color {
                        alacritty_terminal::vte::ansi::Color::Named(
                            alacritty_terminal::vte::ansi::NamedColor::Foreground,
                        ) => Color32::from_rgb(220, 220, 220),
                        alacritty_terminal::vte::ansi::Color::Named(
                            alacritty_terminal::vte::ansi::NamedColor::Background,
                        ) => Color32::from_rgb(20, 20, 25),
                        alacritty_terminal::vte::ansi::Color::Named(named) => {
                            named_color_to_egui(named)
                        }
                        alacritty_terminal::vte::ansi::Color::Spec(rgb) => {
                            Color32::from_rgb(rgb.r, rgb.g, rgb.b)
                        }
                        alacritty_terminal::vte::ansi::Color::Indexed(idx) => {
                            indexed_color_to_egui(idx)
                        }
                    };

                    painter.text(
                        Pos2::new(x, y),
                        egui::Align2::LEFT_TOP,
                        ch,
                        font_id.clone(),
                        egui_color,
                    );
                }
            }

            // Cursor: simple block at the current cursor position.
            let cursor = self.term.grid().cursor.point;
            let cx = rect.left() + cursor.column.0 as f32 * char_width;
            let cy = rect.top() + cursor.line.0 as f32 * char_height;
            painter.rect_filled(
                Rect::from_min_size(Pos2::new(cx, cy), Vec2::new(char_width, char_height)),
                0.0,
                Color32::from_rgba_unmultiplied(100, 140, 255, 100),
            );
        }

        response
    }
}

fn named_color_to_egui(named: alacritty_terminal::vte::ansi::NamedColor) -> Color32 {
    use alacritty_terminal::vte::ansi::NamedColor::*;
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
        Foreground | Background => Color32::from_rgb(220, 220, 220),
        Cursor => Color32::from_rgb(100, 140, 255),
        _ => Color32::from_rgb(220, 220, 220),
    }
}

fn indexed_color_to_egui(idx: u8) -> Color32 {
    // Basic 256-color approximation; alacritty provides a palette if you need exact.
    if idx < 16 {
        let named = match idx {
            0 => alacritty_terminal::vte::ansi::NamedColor::Black,
            1 => alacritty_terminal::vte::ansi::NamedColor::Red,
            2 => alacritty_terminal::vte::ansi::NamedColor::Green,
            3 => alacritty_terminal::vte::ansi::NamedColor::Yellow,
            4 => alacritty_terminal::vte::ansi::NamedColor::Blue,
            5 => alacritty_terminal::vte::ansi::NamedColor::Magenta,
            6 => alacritty_terminal::vte::ansi::NamedColor::Cyan,
            7 => alacritty_terminal::vte::ansi::NamedColor::White,
            8 => alacritty_terminal::vte::ansi::NamedColor::BrightBlack,
            9 => alacritty_terminal::vte::ansi::NamedColor::BrightRed,
            10 => alacritty_terminal::vte::ansi::NamedColor::BrightGreen,
            11 => alacritty_terminal::vte::ansi::NamedColor::BrightYellow,
            12 => alacritty_terminal::vte::ansi::NamedColor::BrightBlue,
            13 => alacritty_terminal::vte::ansi::NamedColor::BrightMagenta,
            14 => alacritty_terminal::vte::ansi::NamedColor::BrightCyan,
            _ => alacritty_terminal::vte::ansi::NamedColor::BrightWhite,
        };
        named_color_to_egui(named)
    } else {
        // Simple fallback for the 256-color cube.
        let r = ((idx - 16) / 36) * 51;
        let g = (((idx - 16) % 36) / 6) * 51;
        let b = ((idx - 16) % 6) * 51;
        Color32::from_rgb(r, g, b)
    }
}
