//! A popover anchored to a control (`docs/ui-spec.md` §4.5).
//!
//! It opens when `anchor` is clicked, closes on a click outside or Esc, and
//! is drawn on `sheet` with a `line` stroke.  The closure paints the body.

use eframe::egui;
use egui::{CornerRadius, Margin, Stroke};

use crate::ui::theme::palette;

/// Shows `add` under `anchor` while the popover is open.  The anchor is
/// usually the button that was just drawn.
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
