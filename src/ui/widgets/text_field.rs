//! The one-line text field, [`CONTROL_H`] high, so it lines up with an
//! outline button.

use eframe::egui;
use egui::Vec2;

use super::{row_info, CONTROL_H};
use crate::ui::fonts::Face;
use crate::ui::theme::{font, TypeRole};

/// A text field.  Build it, then [`Self::show`] it.
pub(crate) struct TextField<'a> {
    text: &'a mut String,
    name: &'a str,
    hint: &'a str,
    width: f32,
    id: Option<egui::Id>,
    value: bool,
}

impl<'a> TextField<'a> {
    /// `name` is what screen readers call it.  The field fills the width
    /// left unless [`Self::width`] says otherwise.
    pub(crate) fn new(text: &'a mut String, name: &'a str) -> Self {
        Self {
            text,
            name,
            hint: "",
            width: f32::INFINITY,
            id: None,
            value: false,
        }
    }

    pub(crate) fn hint(mut self, hint: &'a str) -> Self {
        self.hint = hint;
        self
    }

    pub(crate) fn width(mut self, width: f32) -> Self {
        self.width = width;
        self
    }

    /// A fixed id, to keep the field's state apart or to ask for focus.
    pub(crate) fn id(mut self, id: egui::Id) -> Self {
        self.id = Some(id);
        self
    }

    /// Reports the text as the field's value to screen readers.
    pub(crate) fn report_value(mut self) -> Self {
        self.value = true;
        self
    }

    pub(crate) fn show(self, ui: &mut egui::Ui) -> egui::Response {
        let width = if self.width.is_finite() {
            self.width
        } else {
            ui.available_width()
        };
        let report = self.value;
        let text_font = font(Face::Sans, TypeRole::Body);
        let text_h = ui.fonts_mut(|fonts| fonts.row_height(&text_font));
        // Whole points of margin around the text; `min_size` makes up the rest.
        let margin_y = ((CONTROL_H - text_h) / 2.0).floor().clamp(0.0, 8.0) as i8;
        let mut edit = egui::TextEdit::singleline(&mut *self.text)
            .hint_text(self.hint)
            .desired_width(width)
            .min_size(Vec2::new(width, CONTROL_H))
            .margin(egui::Margin::symmetric(8, margin_y))
            .align(egui::Align2::LEFT_CENTER)
            .font(text_font);
        if let Some(id) = self.id {
            edit = edit.id(id);
        }
        let response = ui.add(edit);
        let value = report.then(|| self.text.clone());
        row_info(
            &response,
            egui::Role::TextInput,
            ui.is_enabled(),
            self.name,
            None,
            value,
        );
        response
    }
}

#[cfg(test)]
mod tests;
