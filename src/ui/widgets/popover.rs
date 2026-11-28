//! A popover anchored to a control.  It closes on a click outside or Esc.

use eframe::egui;
use egui::{CornerRadius, Margin, Stroke};

use crate::ui::theme::palette;

pub(crate) fn popover<R>(
    ui: &mut egui::Ui,
    anchor: &egui::Response,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> Option<R> {
    let colours = palette(ui.ctx());
    egui::Popup::menu(anchor)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .frame(
            egui::Frame::new()
                .fill(colours.sheet)
                .stroke(Stroke::new(1.0, colours.line))
                .corner_radius(CornerRadius::same(4))
                .inner_margin(Margin::same(8)),
        )
        .show(|ui| {
            ui.set_min_width(120.0);
            add(ui)
        })
        .map(|inner| inner.inner)
}

#[cfg(test)]
mod tests;
