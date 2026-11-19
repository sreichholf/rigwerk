//! Small controls shared by several screens: cards, labels, buttons, tabs,
//! meters and sliders.
//!
//! The console kit (fader, meter, chain row, segmented control, chip, path
//! dot, popover) follows `docs/ui-spec.md` §4.  Those modules read
//! [`palette`] and [`font`]; the helpers below are shared by every screen.

mod chain_row;
mod chip;
mod dot;
mod fader;
mod meter;
mod popover;
mod segmented;
mod slider;

pub(crate) use chain_row::{
    add_effect_row, chain_event, chain_row, dashed_rect, effects_header, ChainEvent, ChainMark,
    ChainRow, EffectDrag, CHAIN_ROW_H,
};
pub(crate) use chip::chip;
pub(crate) use dot::path_dot;
pub(crate) use fader::{fader, fader_height, FaderRange};
pub(crate) use meter::{horizontal_meter, vertical_meter, MeterChannel};
pub(crate) use popover::popover;
pub(crate) use segmented::segmented;

pub(crate) use slider::{row_info, show_focus_ring, styled_slider};

#[cfg(test)]
pub(crate) use slider::nudge_slider;

use eframe::egui;
use egui::{
    Align, Color32, CornerRadius, FontFamily, FontId, Layout, Margin, Rect, RichText, Sense,
    Stroke, Vec2,
};

use crate::ui::theme::ts;
use crate::ui::theme::{palette, with_alpha, BORDER_SOFT, CARD_RAISED, DIM, MUTED, TEXT};

/// Accent ring on a focused control, from the active palette.
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

pub(crate) fn card<R>(
    ui: &mut egui::Ui,
    fill: Color32,
    border: Color32,
    inner: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    egui::Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0, border))
        .corner_radius(CornerRadius::same(13))
        .inner_margin(Margin::same(15))
        .show(ui, inner)
        .inner
}

pub(crate) fn micro_label(text: &str, color: Color32) -> RichText {
    RichText::new(text)
        .size(ts(9.5))
        .color(color)
        .family(FontFamily::Proportional)
        .strong()
}

pub(crate) fn mono(text: impl Into<String>, color: Color32, size: f32) -> RichText {
    RichText::new(text)
        .family(FontFamily::Monospace)
        .size(ts(size))
        .color(color)
}

pub(crate) fn status_dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
    ui.painter()
        .circle_filled(rect.center(), 4.0, with_alpha(color, 60));
    ui.painter().circle_filled(rect.center(), 2.2, color);
}

#[allow(dead_code, reason = "the header pill stays until the classic pages go")]
pub(crate) fn status_pill(ui: &mut egui::Ui, text: &str, color: Color32) {
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        FontId::new(ts(10.0), FontFamily::Proportional),
        color,
    );
    let (rect, _) = ui.allocate_exact_size(Vec2::new(galley.size().x + 30.0, 25.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(12), with_alpha(color, 22));
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(12),
        Stroke::new(1.0, with_alpha(color, 55)),
        egui::StrokeKind::Inside,
    );
    ui.painter()
        .circle_filled(egui::pos2(rect.left() + 12.0, rect.center().y), 3.2, color);
    ui.painter().galley(
        egui::pos2(rect.left() + 20.0, rect.center().y - galley.size().y / 2.0),
        galley,
        color,
    );
}

pub(crate) fn horizontal_line(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(rect, 0, color);
}

pub(crate) fn ellipsize(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        let mut output: String = text.chars().take(max.saturating_sub(1)).collect();
        output.push('…');
        output
    }
}

pub(crate) fn ghost_button(ui: &mut egui::Ui, label: &str, width: f32) -> bool {
    ui.add_sized(
        [width, 32.0],
        egui::Button::new(RichText::new(label).size(ts(12.0)).color(MUTED))
            .fill(CARD_RAISED)
            .stroke(Stroke::new(1.0, BORDER_SOFT))
            .corner_radius(CornerRadius::same(8)),
    )
    .clicked()
}

/// A [`tab_button`] that explains itself on hover.
pub(crate) fn hinted_tab(
    ui: &mut egui::Ui,
    label: &str,
    selected: bool,
    color: Color32,
    hint: &str,
) -> bool {
    tab_button(ui, label, selected, color)
        .on_hover_text(hint)
        .clicked()
}

pub(crate) fn tab_button(
    ui: &mut egui::Ui,
    label: &str,
    selected: bool,
    color: Color32,
) -> egui::Response {
    tab_button_sized(ui, label, selected, color, 8.2)
}

/// A [`tab_button`] at a larger font size, for choices such as the channel
/// mode.  The unselected state keeps a quiet outline, so an "off" choice still
/// looks like a button instead of plain text.
pub(crate) fn tab_button_sized(
    ui: &mut egui::Ui,
    label: &str,
    selected: bool,
    color: Color32,
    size: f32,
) -> egui::Response {
    ui.add(
        egui::Button::new(
            RichText::new(label)
                .size(ts(size))
                .color(if selected { color } else { DIM })
                .strong(),
        )
        .fill(if selected {
            with_alpha(color, 20)
        } else {
            Color32::TRANSPARENT
        })
        .stroke(Stroke::new(
            1.0,
            if selected {
                with_alpha(color, 60)
            } else {
                BORDER_SOFT
            },
        ))
        .corner_radius(CornerRadius::same(5))
        .min_size(Vec2::new(0.0, 18.0)),
    )
}

/// A labelled bar: the name on the left, a formatted value in monospace on the
/// right, and a 4 px bar underneath filled to `fraction`.
pub(crate) fn progress_line(
    ui: &mut egui::Ui,
    label: &str,
    value: &str,
    fraction: f32,
    color: Color32,
) {
    ui.horizontal(|ui| {
        ui.label(micro_label(label, DIM));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(mono(value, color, 9.0));
        });
    });
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 4.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(2), Color32::from_rgb(27, 34, 46));
    let filled = Rect::from_min_size(
        rect.min,
        Vec2::new(rect.width() * fraction.clamp(0.0, 1.0), rect.height()),
    );
    ui.painter()
        .rect_filled(filled, CornerRadius::same(2), with_alpha(color, 220));
}

#[allow(dead_code, reason = "the route line stays until the classic pages go")]
pub(crate) fn route_summary(ui: &mut egui::Ui, label: &str, value: &str, color: Color32) {
    ui.horizontal(|ui| {
        ui.label(micro_label(label, DIM));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                RichText::new(ellipsize(value, 28))
                    .size(ts(9.5))
                    .color(color),
            );
        });
    });
}

/// `text` shortened with "…" until it fits `max_width` in `font`.
pub(crate) fn fit_text(ui: &egui::Ui, text: &str, font: &FontId, max_width: f32) -> String {
    let width = |candidate: &str| {
        ui.painter()
            .layout_no_wrap(candidate.to_owned(), font.clone(), TEXT)
            .size()
            .x
    };
    if width(text) <= max_width {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    for keep in (0..chars.len()).rev() {
        let mut candidate: String = chars[..keep].iter().collect();
        candidate.push('…');
        if width(&candidate) <= max_width {
            return candidate;
        }
    }
    "…".to_owned()
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
