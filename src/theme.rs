//! Application-wide visual theme.
//!
//! All UI colours, spacing and padding come from here so new themes can be
//! added without touching the individual screens. The terminal keeps its own
//! ANSI palette; this module only styles the application chrome.
//!
//! The design language is deliberately flat and boxy: square corners, single
//! pixel borders, no drop shadows and no hover glow. Padding is uniform
//! (`Metrics::pad` for controls, `Metrics::card_padding` for frames).

use eframe::egui::{self, Color32, FontFamily, FontId, Margin, Rounding, Stroke, TextStyle, Vec2};

/// Identifiers for the built-in themes. Add a variant and a palette below to
/// introduce a new theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeId {
    #[default]
    Midnight,
    Graphite,
}

impl ThemeId {
    pub const ALL: [ThemeId; 2] = [ThemeId::Midnight, ThemeId::Graphite];

    pub fn id(self) -> &'static str {
        match self {
            ThemeId::Midnight => "midnight",
            ThemeId::Graphite => "graphite",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ThemeId::Midnight => "Midnight",
            ThemeId::Graphite => "Graphite",
        }
    }

    pub fn theme(self) -> &'static Theme {
        match self {
            ThemeId::Midnight => &MIDNIGHT,
            ThemeId::Graphite => &GRAPHITE,
        }
    }

    /// Parse a persisted theme id, falling back to the default.
    pub fn from_id(value: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|theme| theme.id() == value)
            .unwrap_or(ThemeId::Midnight)
    }
}

/// Colour tokens used by the application.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub bg: Color32,
    pub panel: Color32,
    pub surface: Color32,
    pub surface_hover: Color32,
    pub border: Color32,
    pub border_strong: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub accent: Color32,
    pub success: Color32,
    pub warning: Color32,
    pub danger: Color32,
    pub danger_soft: Color32,
    pub selection: Color32,
    pub stripe: Color32,
    pub input_bg: Color32,
}

/// Width shared by every context and dropdown menu. Keeping this in one place
/// stops menus from resizing themselves around their longest label.
pub const MENU_WIDTH: f32 = 220.0;
/// Row height of a menu entry. Deliberately shorter than `control_height` so
/// long menus stay compact.
pub const MENU_ROW_HEIGHT: f32 = 22.0;
/// Width shared by every modal dialog (rename, delete, host key, editor picker).
pub const DIALOG_WIDTH: f32 = 440.0;

/// Spacing and sizing tokens.
#[derive(Debug, Clone, Copy)]
pub struct Metrics {
    /// Uniform padding inside buttons, inputs and small frames.
    pub pad: f32,
    /// Gap between elements.
    pub gap: f32,
    pub card_padding: f32,
    pub modal_padding: f32,
    pub control_height: f32,
    pub row_height: f32,
    pub tab_min_width: f32,
    pub dialog_width: f32,
    pub body_size: f32,
    pub small_size: f32,
    pub heading_size: f32,
    pub mono_size: f32,
}

pub struct Theme {
    pub dark: bool,
    pub palette: Palette,
    pub metrics: Metrics,
}

const METRICS: Metrics = Metrics {
    pad: 6.0,
    gap: 6.0,
    card_padding: 10.0,
    modal_padding: 12.0,
    control_height: 26.0,
    row_height: 24.0,
    tab_min_width: 168.0,
    dialog_width: DIALOG_WIDTH,
    body_size: 13.5,
    small_size: 11.0,
    heading_size: 17.0,
    mono_size: 12.5,
};

pub static MIDNIGHT: Theme = Theme {
    dark: true,
    palette: Palette {
        bg: Color32::from_rgb(12, 15, 19),
        panel: Color32::from_rgb(17, 21, 26),
        surface: Color32::from_rgb(22, 27, 33),
        surface_hover: Color32::from_rgb(29, 35, 43),
        border: Color32::from_rgb(38, 45, 55),
        border_strong: Color32::from_rgb(54, 63, 76),
        text: Color32::from_rgb(224, 229, 235),
        muted: Color32::from_rgb(136, 147, 160),
        accent: Color32::from_rgb(74, 134, 232),
        success: Color32::from_rgb(78, 174, 120),
        warning: Color32::from_rgb(214, 162, 62),
        danger: Color32::from_rgb(226, 88, 79),
        danger_soft: Color32::from_rgb(54, 24, 23),
        selection: Color32::from_rgb(33, 56, 88),
        stripe: Color32::from_rgb(15, 18, 23),
        input_bg: Color32::from_rgb(9, 12, 15),
    },
    metrics: METRICS,
};

pub static GRAPHITE: Theme = Theme {
    dark: true,
    palette: Palette {
        bg: Color32::from_rgb(22, 23, 25),
        panel: Color32::from_rgb(27, 29, 32),
        surface: Color32::from_rgb(33, 36, 40),
        surface_hover: Color32::from_rgb(41, 45, 50),
        border: Color32::from_rgb(52, 57, 63),
        border_strong: Color32::from_rgb(70, 76, 84),
        text: Color32::from_rgb(232, 234, 236),
        muted: Color32::from_rgb(155, 161, 169),
        accent: Color32::from_rgb(120, 158, 224),
        success: Color32::from_rgb(112, 192, 140),
        warning: Color32::from_rgb(219, 184, 126),
        danger: Color32::from_rgb(232, 116, 112),
        danger_soft: Color32::from_rgb(56, 29, 29),
        selection: Color32::from_rgb(49, 61, 78),
        stripe: Color32::from_rgb(25, 27, 30),
        input_bg: Color32::from_rgb(18, 20, 22),
    },
    metrics: METRICS,
};

/// Install the theme's visuals, spacing and text styles on the egui context.
pub fn apply(ctx: &egui::Context, theme: &Theme) {
    let p = &theme.palette;
    let m = &theme.metrics;

    let mut style = (*ctx.style()).clone();
    let mut visuals = if theme.dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };

    visuals.override_text_color = None;
    visuals.panel_fill = p.panel;
    visuals.window_fill = p.surface;
    visuals.window_stroke = Stroke::new(1.0_f32, p.border_strong);
    visuals.window_rounding = Rounding::ZERO;
    visuals.menu_rounding = Rounding::ZERO;
    visuals.faint_bg_color = p.stripe;
    visuals.extreme_bg_color = p.input_bg;
    visuals.code_bg_color = p.surface_hover;
    visuals.hyperlink_color = p.accent;
    visuals.warn_fg_color = p.warning;
    visuals.error_fg_color = p.danger;
    visuals.selection = egui::style::Selection {
        bg_fill: p.selection,
        // Also the text colour of a selected selectable label.
        stroke: Stroke::new(1.0_f32, p.text),
    };
    visuals.window_shadow = egui::epaint::Shadow::NONE;
    visuals.popup_shadow = egui::epaint::Shadow::NONE;
    // Tables draw their own stripes; global striping would tint unrelated
    // widgets (combo boxes, menus) as well.
    visuals.striped = false;
    visuals.button_frame = true;
    visuals.collapsing_header_frame = false;
    visuals.indent_has_left_vline = false;

    let square = |bg_fill, weak_bg_fill, bg_stroke, fg_stroke| egui::style::WidgetVisuals {
        bg_fill,
        weak_bg_fill,
        bg_stroke,
        rounding: Rounding::ZERO,
        fg_stroke,
        expansion: 0.0,
    };

    visuals.widgets.noninteractive = square(
        p.panel,
        p.panel,
        Stroke::new(1.0_f32, p.border),
        Stroke::new(1.0_f32, p.text),
    );
    visuals.widgets.inactive = square(
        p.surface,
        p.surface,
        Stroke::new(1.0_f32, p.border),
        Stroke::new(1.0_f32, p.text),
    );
    // Hover highlights the fill only: no accent-coloured border glow.
    visuals.widgets.hovered = square(
        p.surface_hover,
        p.surface_hover,
        Stroke::new(1.0_f32, p.border_strong),
        Stroke::new(1.0_f32, p.text),
    );
    visuals.widgets.active = square(
        p.surface_hover,
        p.surface_hover,
        Stroke::new(1.0_f32, p.accent),
        Stroke::new(1.0_f32, p.text),
    );
    visuals.widgets.open = square(
        p.surface,
        p.surface,
        Stroke::new(1.0_f32, p.border_strong),
        Stroke::new(1.0_f32, p.text),
    );

    style.visuals = visuals;
    style.spacing.item_spacing = Vec2::splat(m.gap);
    style.spacing.button_padding = Vec2::splat(m.pad);
    style.spacing.window_margin = Margin::same(m.card_padding);
    style.spacing.menu_margin = Margin::same(m.pad);
    style.spacing.menu_width = 240.0;
    style.spacing.indent = 12.0;
    style.spacing.interact_size = Vec2::new(36.0, m.control_height);
    style.spacing.combo_width = 180.0;
    style.spacing.text_edit_width = 260.0;
    // Thin scrollbars that only appear while the pointer is over the area, so
    // they do not add visual noise to the flat surfaces.
    style.spacing.scroll = egui::style::ScrollStyle::floating();
    style.spacing.scroll.bar_width = 8.0;
    style.spacing.scroll.floating_width = 4.0;
    style.animation_time = 0.1;
    style.text_styles = [
        (
            TextStyle::Heading,
            FontId::new(m.heading_size, FontFamily::Proportional),
        ),
        (
            TextStyle::Body,
            FontId::new(m.body_size, FontFamily::Proportional),
        ),
        (
            TextStyle::Monospace,
            FontId::new(m.mono_size, FontFamily::Monospace),
        ),
        (
            TextStyle::Button,
            FontId::new(m.body_size, FontFamily::Proportional),
        ),
        (
            TextStyle::Small,
            FontId::new(m.small_size, FontFamily::Proportional),
        ),
    ]
    .into();

    ctx.set_style(style);
}

/// Proportional font used for body text.
pub fn body_font(theme: &Theme) -> FontId {
    FontId::new(theme.metrics.body_size, FontFamily::Proportional)
}

/// Frame used for the SFTP side panel and the tab strip.
pub fn panel_frame(theme: &Theme) -> egui::Frame {
    egui::Frame::none()
        .fill(theme.palette.panel)
        .inner_margin(Margin::same(theme.metrics.pad))
}

/// Frame used for grouped content on the New Session page and other cards.
pub fn card_frame(theme: &Theme) -> egui::Frame {
    egui::Frame::none()
        .fill(theme.palette.surface)
        .stroke(Stroke::new(1.0_f32, theme.palette.border))
        .rounding(Rounding::ZERO)
        .inner_margin(Margin::same(theme.metrics.card_padding))
}

/// Frame used for modal dialogs.
pub fn modal_frame(theme: &Theme) -> egui::Frame {
    egui::Frame::none()
        .fill(theme.palette.surface)
        .stroke(Stroke::new(1.0_f32, theme.palette.border_strong))
        .rounding(Rounding::ZERO)
        .inner_margin(Margin::same(theme.metrics.modal_padding))
}

/// Default size for a modal window. The height of `1.0` lets the dialog size
/// itself to its content, like every other modal in the app.
pub fn dialog_size(theme: &Theme) -> Vec2 {
    Vec2::new(theme.metrics.dialog_width, 1.0)
}

/// Content width inside a modal (window width minus the frame margins).
pub fn dialog_inner_width(theme: &Theme) -> f32 {
    theme.metrics.dialog_width - 2.0 * theme.metrics.modal_padding
}

/// Apply the shared geometry to a context or dropdown menu.
///
/// Call this as the first statement inside every menu closure. The global
/// `Spacing::menu_width` is not applied to context menus by egui, so the width
/// is set explicitly here. Vertical spacing is tightened for compact rows.
pub fn menu_style(ui: &mut egui::Ui) {
    ui.set_min_width(MENU_WIDTH);
    ui.spacing_mut().item_spacing.y = 1.0;
    ui.spacing_mut().button_padding.y = 2.0;
    ui.spacing_mut().interact_size.y = MENU_ROW_HEIGHT;
}

/// Menu row width that fills the popup minus its margins.
fn menu_row_width(ui: &egui::Ui) -> f32 {
    MENU_WIDTH - 2.0 * ui.spacing().menu_margin.left
}

/// One menu entry with a fixed row width, so the hover highlight spans the
/// whole popup and the menu never grows around a long label.
pub fn menu_item(
    ui: &mut egui::Ui,
    text: impl Into<egui::RichText>,
    enabled: bool,
    color: Option<Color32>,
) -> egui::Response {
    let mut text = text.into();
    if let Some(color) = color {
        text = text.color(color);
    }
    let button = egui::Button::new(text)
        .min_size(Vec2::new(menu_row_width(ui), MENU_ROW_HEIGHT))
        .rounding(Rounding::ZERO);
    ui.add_enabled(enabled, button)
}

/// A menu entry with a persistent selected state (theme picker).
pub fn menu_selectable(
    ui: &mut egui::Ui,
    text: impl Into<egui::WidgetText>,
    selected: bool,
) -> egui::Response {
    ui.add_sized(
        Vec2::new(menu_row_width(ui), MENU_ROW_HEIGHT),
        egui::SelectableLabel::new(selected, text),
    )
}

/// Tinted frame used for warnings and other callouts inside a dialog.
pub fn callout_frame(theme: &Theme, fill: Color32) -> egui::Frame {
    egui::Frame::none()
        .fill(fill)
        .stroke(Stroke::new(1.0_f32, theme.palette.border))
        .rounding(Rounding::ZERO)
        .inner_margin(Margin::same(theme.metrics.pad))
}

/// Small uppercase section heading used inside cards.
pub fn section_label(ui: &mut egui::Ui, theme: &Theme, text: &str) -> egui::Response {
    ui.label(
        egui::RichText::new(text.to_uppercase())
            .color(theme.palette.muted)
            .size(theme.metrics.small_size),
    )
}

/// Muted explanatory text.
pub fn muted(ui: &mut egui::Ui, theme: &Theme, text: impl Into<String>) {
    ui.label(
        egui::RichText::new(text.into())
            .color(theme.palette.muted)
            .size(theme.metrics.small_size),
    );
}

/// Filled accent button used for the primary action of a screen.
pub fn primary_button(theme: &Theme, text: &str) -> egui::Button<'static> {
    egui::Button::new(
        egui::RichText::new(text.to_owned())
            .color(egui::Color32::WHITE)
            .size(theme.metrics.body_size),
    )
    .fill(theme.palette.accent)
    .stroke(Stroke::NONE)
    .rounding(Rounding::ZERO)
}

/// Destructive variant of [`primary_button`].
pub fn danger_button(theme: &Theme, text: &str) -> egui::Button<'static> {
    egui::Button::new(
        egui::RichText::new(text.to_owned())
            .color(egui::Color32::WHITE)
            .size(theme.metrics.body_size),
    )
    .fill(theme.palette.danger)
    .stroke(Stroke::NONE)
    .rounding(Rounding::ZERO)
}

/// A compact icon button with a tooltip.
pub fn icon_button(
    ui: &mut egui::Ui,
    theme: &Theme,
    icon: &str,
    tooltip: &str,
    enabled: bool,
) -> egui::Response {
    let button = egui::Button::new(egui::RichText::new(icon).size(13.0))
        .min_size(Vec2::splat(theme.metrics.control_height))
        .rounding(Rounding::ZERO);
    ui.add_enabled(enabled, button).on_hover_text(tooltip)
}

/// A compact toggle button used for the hidden-files switch.
pub fn icon_toggle(
    ui: &mut egui::Ui,
    theme: &Theme,
    icon: &str,
    tooltip: &str,
    selected: bool,
) -> egui::Response {
    let button = egui::Button::new(egui::RichText::new(icon).size(13.0))
        .selected(selected)
        .min_size(Vec2::splat(theme.metrics.control_height))
        .rounding(Rounding::ZERO);
    ui.add(button).on_hover_text(tooltip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_ids_round_trip_and_fall_back() {
        for id in ThemeId::ALL {
            assert_eq!(ThemeId::from_id(id.id()), id);
            assert!(!id.name().is_empty());
        }
        assert_eq!(ThemeId::from_id("does-not-exist"), ThemeId::Midnight);
    }

    #[test]
    fn palettes_have_contrasting_text() {
        for id in ThemeId::ALL {
            let theme = id.theme();
            assert_ne!(theme.palette.text, theme.palette.surface);
            assert_ne!(theme.palette.panel, theme.palette.surface);
        }
    }
}
