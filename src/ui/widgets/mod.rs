//! Controls shared by several screens.  The outline button and the text
//! field share one height so they line up.

mod button;
mod chain_row;
mod chip;
mod db;
mod dot;
mod fader;
mod meter;
mod popover;
mod segmented;
mod slider;
mod text_field;

pub(crate) use button::{OutlineButton, CONTROL_H};
pub(crate) use chain_row::{
    add_effect_row, chain_event, chain_row, dashed_rect, effects_header, ChainEvent, ChainMark,
    ChainRow, EffectDrag, NamDrag, RowMenu, CHAIN_ROW_H,
};
pub(crate) use chip::chip;
pub(crate) use db::format_db;
pub(crate) use dot::path_dot;
pub(crate) use fader::{fader, fader_height, FaderRange};
pub(crate) use meter::{horizontal_meter, vertical_meter, MeterChannel};
pub(crate) use popover::popover;
pub(crate) use segmented::{segmented, segmented_hinted, segmented_with_tips};

pub(crate) use slider::{row_info, styled_slider, SliderStyle};
pub(crate) use text_field::TextField;

#[cfg(test)]
pub(crate) use slider::nudge_slider;

use eframe::egui;
use egui::{
    Align, Color32, CornerRadius, FontFamily, FontId, Layout, Rect, RichText, Sense, Stroke, Vec2,
};

use crate::audio::EngineState;
use crate::ui::fonts::Face;
use crate::ui::theme::ts;
use crate::ui::theme::{font, palette, with_alpha, Palette, TypeRole};

/// One setting row's height and its readout width: the path and output
/// sheets and the level rows share them so they read the same.
pub(crate) const ROW_H: f32 = 28.0;
pub(crate) const VALUE_W: f32 = 80.0;

// How many layouts [`text_width`] ran on this test thread, so a test can
// check that [`fit_text`] searches rather than measuring every prefix.
#[cfg(test)]
thread_local! {
    static LAYOUTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(crate) fn palette_focus(ui: &egui::Ui, response: &egui::Response, rect: Rect) {
    if response.has_focus() {
        ui.painter().rect_stroke(
            rect,
            CornerRadius::same(4),
            Stroke::new(1.5, palette(ui.ctx()).accent),
            egui::StrokeKind::Inside,
        );
    }
}

pub(crate) fn micro_label(text: &str, color: Color32) -> RichText {
    RichText::new(text)
        .font(font(Face::Sans, TypeRole::Secondary))
        .color(color)
        .strong()
}

pub(crate) fn mono(text: impl Into<String>, color: Color32, size: f32) -> RichText {
    RichText::new(text)
        .family(FontFamily::Monospace)
        .size(ts(size))
        .color(color)
}

pub(crate) fn engine_state_label(state: EngineState) -> &'static str {
    match state {
        EngineState::Preview => "Demo",
        EngineState::Running => "Running",
        EngineState::Starting => "Starting…",
        EngineState::Stopping => "Stopping…",
        EngineState::Error => "Error",
        EngineState::Idle => "Stopped",
    }
}

/// `ready` is true only while a live ASIO driver is open, so the enabled
/// half cannot be exercised headlessly.  The caller hides the chip when
/// the side is not ASIO.
pub(crate) fn asio_panel_chip(
    ui: &mut egui::Ui,
    ready: bool,
    label: &str,
    full_width: bool,
    disabled_hint: &str,
    hint: &str,
) -> bool {
    let mut button = OutlineButton::new(label).enabled(ready);
    if full_width {
        button = button.full_width();
    }
    let response = button.show(ui);
    if !ready {
        let _ = response.on_disabled_hover_text(disabled_hint);
        return false;
    }
    response.on_hover_text(hint).clicked()
}

pub(crate) fn status_dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
    ui.painter()
        .circle_filled(rect.center(), 4.0, with_alpha(color, 60));
    ui.painter().circle_filled(rect.center(), 2.2, color);
}

pub(crate) fn progress_line(
    ui: &mut egui::Ui,
    label: &str,
    value: &str,
    fraction: f32,
    color: Color32,
) {
    let colours = palette(ui.ctx());
    ui.horizontal(|ui| {
        ui.label(micro_label(label, colours.text3));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(mono(value, color, 9.0));
        });
    });
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 4.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(2), colours.well);
    let filled = Rect::from_min_size(
        rect.min,
        Vec2::new(rect.width() * fraction.clamp(0.0, 1.0), rect.height()),
    );
    ui.painter()
        .rect_filled(filled, CornerRadius::same(2), with_alpha(color, 220));
}

pub(crate) fn heading(ui: &mut egui::Ui, text: &str, colours: &Palette) {
    section(ui, text, colours, Face::SansMedium, 3.0);
}

/// Quieter beside the gate's knobs than [`heading`].
pub(crate) fn quiet_heading(ui: &mut egui::Ui, text: &str, colours: &Palette) {
    section(ui, text, colours, Face::Sans, 4.0);
}

fn section(ui: &mut egui::Ui, text: &str, colours: &Palette, face: Face, gap: f32) {
    ui.label(
        RichText::new(text)
            .font(font(face, TypeRole::Secondary))
            .color(colours.text3),
    );
    ui.add_space(gap);
}

pub(crate) fn level_row(
    ui: &mut egui::Ui,
    level: &mut f32,
    min_db: f32,
    max_db: f32,
    colour: Color32,
    hint: &str,
    colours: &Palette,
) {
    // The two floats are adjacent and unlabelled: a swapped pair would still
    // compile and quietly invert the slider.
    debug_assert!(min_db < max_db, "level range runs {min_db} to {max_db}");
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let slider_w = (ui.available_width() - VALUE_W - 8.0).max(80.0);
        styled_slider(
            ui,
            level,
            min_db,
            max_db,
            0.1,
            SliderStyle::named(colour, "Level")
                .value(format_db(*level, 1))
                .width(slider_w),
        )
        .on_hover_text(hint);
        ui.allocate_ui_with_layout(
            Vec2::new(VALUE_W, ROW_H),
            Layout::right_to_left(Align::Center),
            |ui| {
                ui.set_width(VALUE_W);
                ui.label(
                    RichText::new(format_db(*level, 1))
                        .font(font(Face::Mono, TypeRole::Secondary))
                        .color(colours.text),
                );
            },
        );
    });
}

pub(crate) fn text_width(ui: &egui::Ui, text: &str, font: &FontId) -> f32 {
    #[cfg(test)]
    LAYOUTS.with(|count| count.set(count.get() + 1));
    ui.painter()
        .layout_no_wrap(text.to_owned(), font.clone(), palette(ui.ctx()).text)
        .size()
        .x
}

/// A longer prefix plus the ellipsis is never narrower, so the longest
/// prefix that fits is a binary search.
pub(crate) fn fit_text(ui: &egui::Ui, text: &str, font: &FontId, max_width: f32) -> String {
    if text_width(ui, text, font) <= max_width {
        return text.to_owned();
    }
    // The first `low` characters fit with the ellipsis (or `low` is zero,
    // where only the ellipsis is left); the first `high` do not.
    let mut low = 0;
    let mut high = text.chars().count();
    while high - low > 1 {
        let keep = low.midpoint(high);
        if prefix_fits(ui, text, keep, font, max_width) {
            low = keep;
        } else {
            high = keep;
        }
    }
    let mut fitted = String::with_capacity(text.len() + 3);
    fitted.push_str(prefix(text, low));
    fitted.push('…');
    fitted
}

fn prefix(text: &str, keep: usize) -> &str {
    match text.char_indices().nth(keep) {
        Some((index, _)) => &text[..index],
        None => text,
    }
}

fn prefix_fits(ui: &egui::Ui, text: &str, keep: usize, font: &FontId, max_width: f32) -> bool {
    let prefix = prefix(text, keep);
    let mut candidate = String::with_capacity(prefix.len() + 3);
    candidate.push_str(prefix);
    candidate.push('…');
    text_width(ui, &candidate, font) <= max_width
}

#[cfg(test)]
pub(crate) fn run_frame(
    ctx: &egui::Context,
    input: egui::RawInput,
    add: impl FnMut(&mut egui::Ui),
) -> egui::FullOutput {
    let mut output = ctx.run_ui(input, add);
    output.textures_delta.clear();
    output
}

#[cfg(test)]
pub(crate) fn accesskit_frame(add: impl FnMut(&mut egui::Ui)) -> egui::accesskit::TreeUpdate {
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let output = run_frame(&ctx, test_input(None), add);
    output
        .platform_output
        .accesskit_update
        .expect("accesskit tree")
}

#[cfg(test)]
pub(crate) fn node_named<'a>(
    tree: &'a egui::accesskit::TreeUpdate,
    name: &str,
) -> &'a egui::accesskit::Node {
    tree.nodes
        .iter()
        .find(|(_, node)| node.label() == Some(name))
        .map(|(_, node)| node)
        .unwrap_or_else(|| panic!("no AccessKit node named {name}"))
}

#[cfg(test)]
pub(crate) fn test_input(key: Option<egui::Key>) -> egui::RawInput {
    let mut input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(800.0, 600.0),
        )),
        ..Default::default()
    };
    if let Some(key) = key {
        input.events.push(egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        });
    }
    input
}

#[cfg(test)]
mod tests;
