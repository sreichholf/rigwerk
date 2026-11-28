//! Colours and the egui style.  Every control reads [`palette`], never a
//! literal.

use eframe::egui;
use egui::{Color32, CornerRadius, FontFamily, FontId, Stroke, Theme, Vec2};

pub(crate) use crate::ui::fonts::Face;

/// One multiplier, so the whole interface scales together.
pub(crate) const FONT_SCALE: f32 = 1.2;

pub(crate) fn ts(size: f32) -> f32 {
    size * FONT_SCALE
}

/// A step of the type scale (`docs/ui-spec.md` §7.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TypeRole {
    GridHeader,
    Secondary,
    Body,
    Control,
    Title,
    Heading,
    TunerString,
    TunerCents,
    /// The largest text in the window.
    TunerNote,
}

/// Points at 100 %.  The interface size zooms the result.
pub(crate) fn type_size(role: TypeRole) -> f32 {
    match role {
        TypeRole::GridHeader => 11.0,
        TypeRole::Secondary => 12.0,
        TypeRole::Body => 13.0,
        TypeRole::Control => 14.0,
        TypeRole::Title => 15.0,
        TypeRole::Heading => 18.0,
        TypeRole::TunerString => 22.0,
        TypeRole::TunerCents => 44.0,
        TypeRole::TunerNote => 104.0,
    }
}

pub(crate) fn font(face: Face, role: TypeRole) -> FontId {
    FontId::new(type_size(role), face.family())
}

/// Both themes define every token.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Palette {
    pub(crate) bg: Color32,
    pub(crate) bar: Color32,
    pub(crate) strip: Color32,
    pub(crate) slot: Color32,
    /// Never under text.
    pub(crate) well: Color32,
    pub(crate) sheet: Color32,
    pub(crate) line: Color32,
    /// Below 3:1 on purpose: a button is known by its label and fill, not
    /// its border.
    pub(crate) line2: Color32,
    pub(crate) text: Color32,
    pub(crate) text2: Color32,
    pub(crate) text3: Color32,
    pub(crate) led_off: Color32,
    pub(crate) accent: Color32,
    pub(crate) accent_text: Color32,
    pub(crate) accent_bg: Color32,
    /// Always with an icon or a word.
    pub(crate) warn: Color32,
    pub(crate) error: Color32,
    pub(crate) error_text: Color32,
    /// The palette's green is a path's identity, so it is not dark enough
    /// for text in every theme.
    pub(crate) ok_text: Color32,
    pub(crate) meter: Color32,
    /// Always `warn`.
    pub(crate) meter_hot: Color32,
    /// Same order as [`PATH_COLOUR_NAMES`].
    pub(crate) path: [Color32; crate::audio::MAX_PATHS],
}

pub(crate) const PATH_COLOUR_NAMES: [&str; crate::audio::MAX_PATHS] =
    ["Blue", "Violet", "Green", "Rose"];

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
    accent: hex(0x19c2a9),
    accent_text: hex(0x5fe3cf),
    accent_bg: hex(0x102e29),
    warn: hex(0xffcc4d),
    error: hex(0xff5a4a),
    error_text: hex(0xff5a4a),
    ok_text: hex(0x9cc48a),
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
    accent: hex(0x0d7d6f),
    accent_text: hex(0x0b6f63),
    accent_bg: hex(0xdff3ef),
    warn: hex(0x7a5700),
    error: hex(0xd93a2b),
    error_text: hex(0xb8291c),
    ok_text: hex(0x446b33),
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

pub(crate) fn palette(ctx: &egui::Context) -> &'static Palette {
    Palette::of(ctx.theme())
}

/// Installs both themes.  `app` picks which one is drawn.
pub(crate) fn configure_style(ctx: &egui::Context) {
    crate::ui::fonts::install(ctx);
    for theme in [Theme::Dark, Theme::Light] {
        ctx.set_style_of(theme, style(theme));
    }
    install_hover_cursor(ctx);
}

/// Anything that can be clicked is a hand.  A fader, a slider, a curve
/// handle and the sheet edge set their own cursor, and that choice wins.
pub(crate) fn hover_cursor(sense: egui::Sense, enabled: bool) -> Option<egui::CursorIcon> {
    if enabled && sense.senses_click() {
        Some(egui::CursorIcon::PointingHand)
    } else {
        None
    }
}

fn install_hover_cursor(ctx: &egui::Context) {
    let key = egui::Id::unique("rigwerk-hover-cursor");
    if ctx.data(|data| data.get_temp::<bool>(key).unwrap_or(false)) {
        return;
    }
    ctx.data_mut(|data| data.insert_temp(key, true));
    ctx.on_end_pass(
        "hover cursor",
        std::sync::Arc::new(|ui| {
            let ctx = ui.ctx().clone();
            if ctx.output(|output| output.cursor_icon) != egui::CursorIcon::Default {
                return;
            }
            let ids: Vec<egui::Id> =
                ctx.interaction_snapshot(|snap| snap.hovered.iter().copied().collect());
            let hand = ids.iter().any(|id| {
                ctx.read_response(*id)
                    .and_then(|response| hover_cursor(response.sense, response.enabled()))
                    .is_some()
            });
            if hand {
                ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
            }
        }),
    );
}

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
    // Bare egui buttons, combos, checkboxes and sliders read these.  Match
    // the type scale so they do not sit above the painted controls
    // (segmented, Driver panel) that already use `font()`.
    style.text_styles = [
        (
            egui::TextStyle::Heading,
            FontId::new(type_size(TypeRole::Heading), FontFamily::Proportional),
        ),
        (
            egui::TextStyle::Body,
            FontId::new(type_size(TypeRole::Body), FontFamily::Proportional),
        ),
        (
            egui::TextStyle::Button,
            FontId::new(type_size(TypeRole::Control), FontFamily::Proportional),
        ),
        (
            egui::TextStyle::Small,
            FontId::new(type_size(TypeRole::Secondary), FontFamily::Proportional),
        ),
        (
            egui::TextStyle::Monospace,
            FontId::new(type_size(TypeRole::Secondary), FontFamily::Monospace),
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
