//! Application-wide visual theme.
//!
//! All UI colours, spacing and rounding come from here so new themes can be
//! added without touching the individual screens. The terminal keeps its own
//! ANSI palette; this module only styles the application chrome.

use eframe::egui::{
    self, Color32, FontFamily, FontId, Margin, Rounding, Shadow, Stroke, TextStyle, Vec2,
};

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
    pub text: Color32,
    pub muted: Color32,
    pub accent: Color32,
    pub accent_hover: Color32,
    pub success: Color32,
    pub warning: Color32,
    pub danger: Color32,
    pub danger_soft: Color32,
    pub selection: Color32,
    pub stripe: Color32,
    pub input_bg: Color32,
}

/// Spacing and sizing tokens.
#[derive(Debug, Clone, Copy)]
pub struct Metrics {
    pub radius: f32,
    pub card_radius: f32,
    pub modal_radius: f32,
    pub base_spacing: f32,
    pub card_padding: f32,
    pub modal_padding: f32,
    pub control_height: f32,
    pub row_height: f32,
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

const MIDNIGHT_METRICS: Metrics = Metrics {
    radius: 6.0,
    card_radius: 8.0,
    modal_radius: 10.0,
    base_spacing: 8.0,
    card_padding: 16.0,
    modal_padding: 20.0,
    control_height: 28.0,
    row_height: 26.0,
    body_size: 14.0,
    small_size: 11.5,
    heading_size: 20.0,
    mono_size: 13.0,
};

pub static MIDNIGHT: Theme = Theme {
    dark: true,
    palette: Palette {
        bg: Color32::from_rgb(15, 17, 21),
        panel: Color32::from_rgb(20, 23, 28),
        surface: Color32::from_rgb(25, 29, 36),
        surface_hover: Color32::from_rgb(32, 37, 45),
        border: Color32::from_rgb(42, 48, 58),
        text: Color32::from_rgb(230, 232, 236),
        muted: Color32::from_rgb(152, 161, 173),
        accent: Color32::from_rgb(91, 157, 255),
        accent_hover: Color32::from_rgb(124, 178, 255),
        success: Color32::from_rgb(70, 177, 123),
        warning: Color32::from_rgb(217, 164, 65),
        danger: Color32::from_rgb(229, 83, 75),
        danger_soft: Color32::from_rgba_premultiplied(57, 21, 19, 255),
        selection: Color32::from_rgb(36, 59, 92),
        stripe: Color32::from_rgb(22, 26, 32),
        input_bg: Color32::from_rgb(13, 16, 20),
    },
    metrics: MIDNIGHT_METRICS,
};

pub static GRAPHITE: Theme = Theme {
    dark: true,
    palette: Palette {
        bg: Color32::from_rgb(23, 24, 26),
        panel: Color32::from_rgb(29, 31, 34),
        surface: Color32::from_rgb(35, 38, 42),
        surface_hover: Color32::from_rgb(44, 48, 53),
        border: Color32::from_rgb(56, 61, 67),
        text: Color32::from_rgb(236, 237, 238),
        muted: Color32::from_rgb(163, 168, 174),
        accent: Color32::from_rgb(122, 162, 247),
        accent_hover: Color32::from_rgb(148, 183, 255),
        success: Color32::from_rgb(115, 201, 145),
        warning: Color32::from_rgb(226, 192, 141),
        danger: Color32::from_rgb(240, 113, 120),
        danger_soft: Color32::from_rgba_premultiplied(60, 28, 30, 255),
        selection: Color32::from_rgb(53, 64, 82),
        stripe: Color32::from_rgb(30, 33, 37),
        input_bg: Color32::from_rgb(20, 22, 25),
    },
    metrics: MIDNIGHT_METRICS,
};

/// Install the theme's visuals, spacing and text styles on the egui context.
pub fn apply(ctx: &egui::Context, theme: &Theme) {
    let p = &theme.palette;
    let m = &theme.metrics;
    let rounding = Rounding::same(m.radius);

    let mut style = (*ctx.style()).clone();
    let mut visuals = if theme.dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };

    visuals.override_text_color = None;
    visuals.panel_fill = p.panel;
    visuals.window_fill = p.surface;
    visuals.window_stroke = Stroke::new(1.0_f32, p.border);
    visuals.window_rounding = Rounding::same(m.modal_radius);
    visuals.menu_rounding = Rounding::same(m.card_radius);
    visuals.faint_bg_color = p.stripe;
    visuals.extreme_bg_color = p.input_bg;
    visuals.code_bg_color = p.surface_hover;
    visuals.hyperlink_color = p.accent;
    visuals.warn_fg_color = p.warning;
    visuals.error_fg_color = p.danger;
    visuals.selection = egui::style::Selection {
        bg_fill: p.selection,
        stroke: Stroke::new(1.0_f32, p.accent),
    };
    visuals.window_shadow = Shadow {
        offset: Vec2::new(0.0, 8.0),
        blur: 24.0,
        spread: 0.0,
        color: Color32::from_black_alpha(110),
    };
    visuals.popup_shadow = Shadow {
        offset: Vec2::new(0.0, 4.0),
        blur: 14.0,
        spread: 0.0,
        color: Color32::from_black_alpha(90),
    };
    visuals.striped = true;
    visuals.button_frame = true;
    visuals.collapsing_header_frame = false;
    visuals.indent_has_left_vline = true;

    visuals.widgets.noninteractive = egui::style::WidgetVisuals {
        bg_fill: p.panel,
        weak_bg_fill: p.panel,
        bg_stroke: Stroke::new(1.0_f32, p.border),
        rounding,
        fg_stroke: Stroke::new(1.0_f32, p.text),
        expansion: 0.0,
    };
    visuals.widgets.inactive = egui::style::WidgetVisuals {
        bg_fill: p.surface,
        weak_bg_fill: p.surface,
        bg_stroke: Stroke::new(1.0_f32, p.border),
        rounding,
        fg_stroke: Stroke::new(1.0_f32, p.text),
        expansion: 0.0,
    };
    visuals.widgets.hovered = egui::style::WidgetVisuals {
        bg_fill: p.surface_hover,
        weak_bg_fill: p.surface_hover,
        bg_stroke: Stroke::new(1.0_f32, p.accent_hover),
        rounding,
        fg_stroke: Stroke::new(1.0_f32, p.text),
        expansion: 0.0,
    };
    visuals.widgets.active = egui::style::WidgetVisuals {
        bg_fill: p.surface_hover,
        weak_bg_fill: p.surface_hover,
        bg_stroke: Stroke::new(1.0_f32, p.accent_hover),
        rounding,
        fg_stroke: Stroke::new(1.0_f32, p.text),
        expansion: 0.0,
    };
    visuals.widgets.open = egui::style::WidgetVisuals {
        bg_fill: p.surface_hover,
        weak_bg_fill: p.surface_hover,
        bg_stroke: Stroke::new(1.0_f32, p.border),
        rounding,
        fg_stroke: Stroke::new(1.0_f32, p.text),
        expansion: 0.0,
    };

    style.visuals = visuals;
    style.spacing.item_spacing = Vec2::new(m.base_spacing, m.base_spacing * 0.75);
    style.spacing.button_padding = Vec2::new(10.0, 5.0);
    style.spacing.window_margin = Margin::same(10.0);
    style.spacing.menu_margin = Margin::same(6.0);
    style.spacing.menu_width = 280.0;
    style.spacing.indent = 18.0;
    style.spacing.interact_size.y = m.control_height;
    style.spacing.scroll.bar_width = 8.0;
    style.spacing.scroll.floating = false;
    style.animation_time = 0.12;
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

/// Frame used for grouped content on the New Session page and other cards.
pub fn card_frame(theme: &Theme) -> egui::Frame {
    egui::Frame::none()
        .fill(theme.palette.surface)
        .stroke(Stroke::new(1.0_f32, theme.palette.border))
        .rounding(Rounding::same(theme.metrics.card_radius))
        .inner_margin(Margin::same(theme.metrics.card_padding))
}

/// Frame used for modal dialogs.
pub fn modal_frame(theme: &Theme) -> egui::Frame {
    egui::Frame::none()
        .fill(theme.palette.surface)
        .stroke(Stroke::new(1.0_f32, theme.palette.border))
        .rounding(Rounding::same(theme.metrics.modal_radius))
        .inner_margin(Margin::same(theme.metrics.modal_padding))
        .shadow(Shadow {
            offset: Vec2::new(0.0, 10.0),
            blur: 30.0,
            spread: 0.0,
            color: Color32::from_black_alpha(120),
        })
}

/// Tinted frame used for warnings and other callouts inside a dialog.
pub fn callout_frame(theme: &Theme, fill: Color32) -> egui::Frame {
    egui::Frame::none()
        .fill(fill)
        .stroke(Stroke::new(1.0_f32, theme.palette.border))
        .rounding(Rounding::same(theme.metrics.radius))
        .inner_margin(Margin::symmetric(10.0, 8.0))
}

/// Section heading used inside cards.
pub fn section_label(ui: &mut egui::Ui, theme: &Theme, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .strong()
            .color(theme.palette.text)
            .size(theme.metrics.body_size),
    );
}

/// Muted explanatory text.
pub fn muted(ui: &mut egui::Ui, theme: &Theme, text: impl Into<String>) {
    ui.label(
        egui::RichText::new(text.into())
            .color(theme.palette.muted)
            .size(theme.metrics.small_size),
    );
}

/// A compact icon button with a tooltip.
pub fn icon_button(
    ui: &mut egui::Ui,
    theme: &Theme,
    icon: &str,
    tooltip: &str,
    enabled: bool,
) -> egui::Response {
    let button = egui::Button::new(egui::RichText::new(icon).size(15.0))
        .min_size(Vec2::new(28.0, 26.0))
        .rounding(Rounding::same(theme.metrics.radius));
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
    let button = egui::Button::new(egui::RichText::new(icon).size(15.0))
        .selected(selected)
        .min_size(Vec2::new(28.0, 26.0))
        .rounding(Rounding::same(theme.metrics.radius));
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
