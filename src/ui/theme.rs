//! Colours and the egui style.

use eframe::egui;
use egui::{Color32, CornerRadius, FontFamily, FontId, Stroke, Vec2};

/// Every text size in the window is multiplied by this, so the whole interface
/// can be made easier to read from one place.
pub(crate) const FONT_SCALE: f32 = 1.2;

/// A text size at the interface's reading scale.
pub(crate) fn ts(size: f32) -> f32 {
    size * FONT_SCALE
}

pub(crate) const BG: Color32 = Color32::from_rgb(8, 11, 17);
pub(crate) const SIDEBAR: Color32 = Color32::from_rgb(11, 15, 23);
pub(crate) const CARD: Color32 = Color32::from_rgb(16, 21, 31);
pub(crate) const CARD_RAISED: Color32 = Color32::from_rgb(21, 27, 40);
pub(crate) const CARD_DEEP: Color32 = Color32::from_rgb(12, 16, 25);
pub(crate) const BORDER: Color32 = Color32::from_rgb(34, 42, 57);
pub(crate) const BORDER_SOFT: Color32 = Color32::from_rgb(27, 34, 47);
pub(crate) const TEXT: Color32 = Color32::from_rgb(232, 237, 246);
pub(crate) const MUTED: Color32 = Color32::from_rgb(139, 151, 171);
pub(crate) const DIM: Color32 = Color32::from_rgb(89, 102, 122);
pub(crate) const TEAL: Color32 = Color32::from_rgb(56, 221, 191);
pub(crate) const TEAL_DARK: Color32 = Color32::from_rgb(16, 78, 72);
pub(crate) const BLUE: Color32 = Color32::from_rgb(112, 167, 255);
pub(crate) const AMBER: Color32 = Color32::from_rgb(255, 184, 91);
pub(crate) const RED: Color32 = Color32::from_rgb(255, 112, 112);
pub(crate) const GREEN: Color32 = Color32::from_rgb(96, 216, 144);

pub(crate) fn configure_style(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = Vec2::new(8.0, 8.0);
        style.spacing.button_padding = Vec2::new(12.0, 7.0);
        style.visuals = egui::Visuals::dark();
        style.visuals.override_text_color = Some(TEXT);
        style.visuals.panel_fill = BG;
        style.visuals.window_fill = CARD;
        style.visuals.window_stroke = Stroke::new(1.0, BORDER);
        style.visuals.extreme_bg_color = CARD_DEEP;
        style.visuals.faint_bg_color = CARD;
        style.visuals.code_bg_color = CARD_DEEP;
        style.visuals.selection.bg_fill = TEAL_DARK;
        style.visuals.selection.stroke = Stroke::new(1.0, TEAL);
        style.visuals.widgets.noninteractive.bg_fill = CARD;
        style.visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
        style.visuals.widgets.inactive.bg_fill = CARD_RAISED;
        style.visuals.widgets.inactive.weak_bg_fill = CARD_RAISED;
        style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER_SOFT);
        style.visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
        style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(27, 35, 49);
        style.visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(27, 35, 49);
        style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(50, 63, 82));
        style.visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, Color32::WHITE);
        style.visuals.widgets.active.bg_fill = TEAL_DARK;
        style.visuals.widgets.active.weak_bg_fill = TEAL_DARK;
        style.visuals.widgets.active.bg_stroke = Stroke::new(1.0, TEAL);
        style.visuals.widgets.active.fg_stroke = Stroke::new(1.0, TEXT);
        style.visuals.widgets.open.bg_fill = CARD_RAISED;
        style.visuals.widgets.open.fg_stroke = Stroke::new(1.0, TEXT);
        style.visuals.widgets.noninteractive.corner_radius = CornerRadius::same(8);
        style.visuals.widgets.inactive.corner_radius = CornerRadius::same(8);
        style.visuals.widgets.hovered.corner_radius = CornerRadius::same(8);
        style.visuals.widgets.active.corner_radius = CornerRadius::same(8);
        style.visuals.widgets.open.corner_radius = CornerRadius::same(8);
        style.visuals.window_corner_radius = CornerRadius::same(14);
        style.visuals.menu_corner_radius = CornerRadius::same(10);
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
    });
}

pub(crate) fn with_alpha(color: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha)
}
