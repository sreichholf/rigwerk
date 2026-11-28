//! The outline button, [`CONTROL_H`] high, so it lines up with a text field.
//! A button with its own meaning keeps its own look.

use eframe::egui;
use egui::{CornerRadius, RichText, Stroke, Vec2};

use super::{fit_text, row_info};
use crate::ui::fonts::Face;
use crate::ui::theme::{font, palette, TypeRole};

/// Height of an outline button and a text field.
pub(crate) const CONTROL_H: f32 = 28.0;

/// An outline button.  Build it, then [`Self::show`] it.
pub(crate) struct OutlineButton<'a> {
    label: &'a str,
    name: Option<&'a str>,
    enabled: bool,
    full_width: bool,
}

impl<'a> OutlineButton<'a> {
    pub(crate) fn new(label: &'a str) -> Self {
        Self {
            label,
            name: None,
            enabled: true,
            full_width: false,
        }
    }

    /// What it does, for screen readers, when the label is a glyph or
    /// shows a value.  Add hover text on the response where it helps.
    pub(crate) fn name(mut self, name: &'a str) -> Self {
        self.name = Some(name);
        self
    }

    pub(crate) fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Fills the width left, shortening the label with "…" to fit.
    pub(crate) fn full_width(mut self) -> Self {
        self.full_width = true;
        self
    }

    pub(crate) fn show(self, ui: &mut egui::Ui) -> egui::Response {
        let colours = palette(ui.ctx());
        let text_font = font(Face::Sans, TypeRole::Control);
        let mut width = CONTROL_H;
        let mut shown = self.label.to_owned();
        if self.full_width {
            width = ui.available_width();
            shown = fit_text(ui, self.label, &text_font, (width - 16.0).max(8.0));
        }
        let colour = if self.enabled {
            colours.text
        } else {
            colours.text3
        };
        let button = egui::Button::new(RichText::new(shown).font(text_font.clone()).color(colour))
            .fill(colours.slot)
            .stroke(Stroke::new(1.0, colours.line2))
            .corner_radius(CornerRadius::same(4))
            .min_size(Vec2::new(width, CONTROL_H));
        // The theme's padding is for taller buttons: fit the text into
        // the one height instead.
        let text_h = ui.fonts_mut(|fonts| fonts.row_height(&text_font));
        let response = ui
            .scope(|ui| {
                ui.spacing_mut().button_padding.y = ((CONTROL_H - text_h) / 2.0).max(0.0);
                ui.add_enabled(self.enabled, button)
            })
            .inner;
        let name = self.name.unwrap_or(self.label);
        // A name that is not the label: the label is the value it shows.
        let value = (name != self.label && !self.label.is_empty()).then(|| self.label.to_owned());
        row_info(
            &response,
            egui::Role::Button,
            self.enabled,
            name,
            None,
            value,
        );
        response
    }
}

#[cfg(test)]
mod tests;
