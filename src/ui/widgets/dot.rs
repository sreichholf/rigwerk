//! The path dot (`docs/ui-spec.md` §4.3 and the *Plays* marks in §3.3).
//!
//! Filled, the path is present or playing.  A ring is the same path with
//! nothing playing.  The colour is the saved path index.

use eframe::egui;
use egui::{Sense, Stroke, Vec2};

use crate::ui::theme::palette;
use crate::ui::widgets::row_info;

/// An 8 pt dot in the path colour.  `label` is the AccessKit name, for
/// example the path name or "Room plays".
pub(crate) fn path_dot(
    ui: &mut egui::Ui,
    colour_index: u8,
    filled: bool,
    label: &str,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::click());
    let colours = palette(ui.ctx());
    let colour = colours.path[colour_index as usize % colours.path.len()];
    if filled {
        ui.painter().circle_filled(rect.center(), 4.0, colour);
    } else {
        ui.painter()
            .circle_stroke(rect.center(), 3.5, Stroke::new(1.5, colour));
    }
    row_info(
        &response,
        egui::Role::Button,
        ui.is_enabled(),
        label,
        Some(filled),
        None,
    );
    response
}

#[cfg(test)]
mod tests;
