//! Colours and the egui style.
//!
//! [`DARK`] and [`LIGHT`] hold the tokens of `docs/ui-spec.md` §7.1, and
//! [`palette`] gives the active one.  The old constants (`BG`, `TEAL`, …)
//! are the dark tokens and do not follow the theme, so the old pages stay
//! dark-only until they are rewritten.

use eframe::egui;
use egui::{Color32, CornerRadius, FontFamily, FontId, Stroke, Theme, Vec2};

/// Every text size in the window is multiplied by this, so the whole interface
/// can be made easier to read from one place.
pub(crate) const FONT_SCALE: f32 = 1.2;

/// A text size at the interface's reading scale.
pub(crate) fn ts(size: f32) -> f32 {
    size * FONT_SCALE
}

/// The colour tokens of one theme.  Both themes define every token; a
/// widget reads the active one through [`palette`], never a literal.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Palette {
    /// Console well, window.
    pub(crate) bg: Color32,
    /// Title bar, status bar.
    pub(crate) bar: Color32,
    /// Strips, setup panels.
    pub(crate) strip: Color32,
    /// Chain rows, buttons.
    pub(crate) slot: Color32,
    /// Meter tracks, scope.  Never under text.
    pub(crate) well: Color32,
    /// Sheets, drawers.
    pub(crate) sheet: Color32,
    /// Separators.
    pub(crate) line: Color32,
    /// Control borders.  Below 3:1 on purpose: a button is known by its
    /// label and fill, not its border.
    pub(crate) line2: Color32,
    /// Primary text.
    pub(crate) text: Color32,
    /// Secondary text.
    pub(crate) text2: Color32,
    /// Labels, hints.
    pub(crate) text3: Color32,
    /// A bypassed LED's ring, off states.
    pub(crate) led_off: Color32,
    /// Engaged states: running, selected, armed, muted, soloed.
    pub(crate) accent: Color32,
    /// The accent as text.
    pub(crate) accent_text: Color32,
    /// Selected fill.
    pub(crate) accent_bg: Color32,
    /// Warnings, always with an icon or a word.
    pub(crate) warn: Color32,
    /// Clip, recording, error icons.
    pub(crate) error: Color32,
    /// Error text.
    pub(crate) error_text: Color32,
    /// Meter below −6 dBFS.
    pub(crate) meter: Color32,
    /// Meter from −6 to −1 dBFS, limiter reduction.  Always `warn`.
    pub(crate) meter_hot: Color32,
    /// Path identity by the saved colour index: Blue, Violet, Green, Rose.
    pub(crate) path: [Color32; crate::audio::MAX_PATHS],
}

/// `0xRRGGBB`, so the tokens read like the table in the spec.
const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

pub(crate) const DARK: Palette = Palette {
    bg: hex(0x121211),
    bar: hex(0x0d0d0c),
    strip: hex(0x1a1918),
    slot: hex(0x222120),
    well: hex(0x0f0f0e),
    sheet: hex(0x1c1b19),
    line: hex(0x2a2825),
    line2: hex(0x3a3733),
    text: hex(0xebe7df),
    text2: hex(0xb0aa9f),
    text3: hex(0x948e83),
    led_off: hex(0x7d776e),
    accent: hex(0xff8f40),
    accent_text: hex(0xffb27a),
    accent_bg: hex(0x2a1a0e),
    warn: hex(0xffcc4d),
    error: hex(0xff5a4a),
    error_text: hex(0xff5a4a),
    meter: hex(0xcfc8bb),
    meter_hot: hex(0xffcc4d),
    path: [hex(0x7fb2d6), hex(0xc49bd8), hex(0x9cc48a), hex(0xe39aa6)],
};

pub(crate) const LIGHT: Palette = Palette {
    bg: hex(0xe9e6df),
    bar: hex(0xf4f2ed),
    strip: hex(0xfbfaf7),
    slot: hex(0xefede8),
    well: hex(0xe4e1da),
    sheet: hex(0xfdfcfa),
    line: hex(0xdcd8cf),
    line2: hex(0xc9c4b9),
    text: hex(0x1d1c1a),
    text2: hex(0x55514a),
    text3: hex(0x6b665d),
    led_off: hex(0x857f73),
    accent: hex(0xc55a0c),
    accent_text: hex(0xa8480a),
    accent_bg: hex(0xfbeee2),
    warn: hex(0x7a5700),
    error: hex(0xd93a2b),
    error_text: hex(0xb8291c),
    meter: hex(0x4a463f),
    meter_hot: hex(0x7a5700),
    path: [hex(0x3f7fae), hex(0x8d5bb0), hex(0x5f8f4a), hex(0xb5566a)],
};

impl Palette {
    pub(crate) fn of(theme: Theme) -> &'static Self {
        match theme {
            Theme::Dark => &DARK,
            Theme::Light => &LIGHT,
        }
    }
}

/// The palette of the theme egui draws with now.
#[allow(
    dead_code,
    reason = "the rewritten pages read it; the old ones use the constants"
)]
pub(crate) fn palette(ctx: &egui::Context) -> &'static Palette {
    Palette::of(ctx.theme())
}

// What the old pages draw with: the dark tokens, whatever the theme.
pub(crate) const BG: Color32 = DARK.bg;
pub(crate) const SIDEBAR: Color32 = DARK.bar;
pub(crate) const CARD: Color32 = DARK.strip;
pub(crate) const CARD_RAISED: Color32 = DARK.slot;
pub(crate) const CARD_DEEP: Color32 = DARK.well;
pub(crate) const BORDER: Color32 = DARK.line2;
pub(crate) const BORDER_SOFT: Color32 = DARK.line;
pub(crate) const TEXT: Color32 = DARK.text;
pub(crate) const MUTED: Color32 = DARK.text2;
/// Secondary text, `text3`: about 5.4:1 on CARD and quieter than MUTED.
pub(crate) const DIM: Color32 = DARK.text3;
pub(crate) const TEAL: Color32 = DARK.accent;
/// Outputs: their meters, traces and buttons.  The palette has no blue
/// outside the path colours, so this is the meter colour.
pub(crate) const BLUE: Color32 = DARK.meter;
pub(crate) const AMBER: Color32 = DARK.warn;
pub(crate) const RED: Color32 = DARK.error;
/// OK states.  The palette's only green is the Green path colour.
pub(crate) const GREEN: Color32 = DARK.path[2];

/// Path palette.  The index is what `app` stores; this is the colour a row
/// paints.  Same order as the saved `colour` field.
pub(crate) const PATH_COLOURS: [Color32; crate::audio::MAX_PATHS] = DARK.path;

pub(crate) fn path_colour(index: u8) -> Color32 {
    PATH_COLOURS[index as usize % crate::audio::MAX_PATHS]
}

pub(crate) fn log_colour(tone: crate::app::LogTone) -> Color32 {
    use crate::app::LogTone;
    match tone {
        LogTone::Muted => MUTED,
        LogTone::Blue => BLUE,
        LogTone::Amber => AMBER,
        LogTone::Teal => TEAL,
        LogTone::Green => GREEN,
        LogTone::Red => RED,
    }
}

/// Installs the fonts and both egui styles.
pub(crate) fn configure_style(ctx: &egui::Context) {
    crate::ui::fonts::install(ctx);
    ctx.set_theme(egui::ThemePreference::Dark);
    for theme in [Theme::Dark, Theme::Light] {
        ctx.set_style_of(theme, style(theme));
    }
}

/// egui's style for `theme`, coloured from its palette.
fn style(theme: Theme) -> egui::Style {
    let p = Palette::of(theme);
    let mut style = theme.default_style();
    style.spacing.item_spacing = Vec2::new(8.0, 8.0);
    style.spacing.button_padding = Vec2::new(12.0, 7.0);
    let visuals = &mut style.visuals;
    visuals.override_text_color = Some(p.text);
    visuals.hyperlink_color = p.accent_text;
    visuals.warn_fg_color = p.warn;
    visuals.error_fg_color = p.error_text;
    visuals.panel_fill = p.bg;
    visuals.window_fill = p.sheet;
    visuals.window_stroke = Stroke::new(1.0, p.line);
    visuals.extreme_bg_color = p.well;
    visuals.faint_bg_color = p.strip;
    visuals.code_bg_color = p.well;
    visuals.selection.bg_fill = p.accent_bg;
    visuals.selection.stroke = Stroke::new(1.0, p.accent);
    visuals.text_cursor.stroke.color = p.accent;
    let widgets = &mut visuals.widgets;
    widgets.noninteractive.bg_fill = p.strip;
    widgets.noninteractive.weak_bg_fill = p.strip;
    widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.line);
    widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    widgets.inactive.bg_fill = p.slot;
    widgets.inactive.weak_bg_fill = p.slot;
    widgets.inactive.bg_stroke = Stroke::new(1.0, p.line2);
    widgets.inactive.fg_stroke = Stroke::new(1.0, p.text);
    widgets.hovered.bg_fill = p.line;
    widgets.hovered.weak_bg_fill = p.line;
    widgets.hovered.bg_stroke = Stroke::new(1.0, p.led_off);
    widgets.hovered.fg_stroke = Stroke::new(1.0, p.text);
    widgets.active.bg_fill = p.accent_bg;
    widgets.active.weak_bg_fill = p.accent_bg;
    widgets.active.bg_stroke = Stroke::new(1.0, p.accent);
    widgets.active.fg_stroke = Stroke::new(1.0, p.text);
    widgets.open.bg_fill = p.slot;
    widgets.open.weak_bg_fill = p.slot;
    widgets.open.bg_stroke = Stroke::new(1.0, p.line2);
    widgets.open.fg_stroke = Stroke::new(1.0, p.text);
    for widget in [
        &mut widgets.noninteractive,
        &mut widgets.inactive,
        &mut widgets.hovered,
        &mut widgets.active,
        &mut widgets.open,
    ] {
        widget.corner_radius = CornerRadius::same(8);
    }
    visuals.window_corner_radius = CornerRadius::same(14);
    visuals.menu_corner_radius = CornerRadius::same(10);
    style.text_styles = [
        (
            egui::TextStyle::Heading,
            FontId::new(ts(27.0), FontFamily::Proportional),
        ),
        (
            egui::TextStyle::Body,
            FontId::new(ts(13.5), FontFamily::Proportional),
        ),
        (
            egui::TextStyle::Button,
            FontId::new(ts(13.0), FontFamily::Proportional),
        ),
        (
            egui::TextStyle::Small,
            FontId::new(ts(10.5), FontFamily::Proportional),
        ),
        (
            egui::TextStyle::Monospace,
            FontId::new(ts(11.5), FontFamily::Monospace),
        ),
    ]
    .into();
    style
}

pub(crate) fn with_alpha(color: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha)
}

#[cfg(test)]
mod tests;
