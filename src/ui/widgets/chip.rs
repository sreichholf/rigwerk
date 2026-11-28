//! A toggle chip.  The caller owns the value; a click flips it.

use eframe::egui;
use egui::{CornerRadius, Sense, Stroke, Vec2};

use crate::ui::fonts::Face;
use crate::ui::theme::{font, palette, TypeRole};
use crate::ui::widgets::{palette_focus, row_info};

pub(crate) fn chip(ui: &mut egui::Ui, on: &mut bool, label: &str) -> egui::Response {
    let colours = palette(ui.ctx());
    let text_font = font(Face::Sans, TypeRole::Control);
    let text_w = ui
        .painter()
        .layout_no_wrap(label.to_owned(), text_font.clone(), colours.text)
        .size()
        .x;
    let (rect, mut response) =
        ui.allocate_exact_size(Vec2::new(text_w + 20.0, 32.0), Sense::click());
    let fill = if *on { colours.accent_bg } else { colours.slot };
    let stroke = if *on { colours.accent } else { colours.line2 };
    let ink = if *on {
        colours.accent_text
    } else {
        colours.text
    };
    ui.painter().rect_filled(rect, CornerRadius::same(3), fill);
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(3),
        Stroke::new(1.0, stroke),
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        label,
        text_font,
        ink,
    );
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    row_info(
        &response,
        egui::Role::Button,
        ui.is_enabled(),
        label,
        Some(*on),
        None,
    );
    palette_focus(ui, &response, rect);
    response
}

#[cfg(test)]
mod tests;
