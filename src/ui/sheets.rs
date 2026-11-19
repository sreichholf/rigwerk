//! Bottom sheets: the header every sheet shares (`docs/ui-spec.md` §4.3)
//! and the dispatch to each sheet's body.
//!
//! The shell draws the sheet's frame, height and drag edge.  This file
//! draws the header (the path dot and name, `/`, the item, *On* /
//! *Bypass* for a stage, ‹ › through the chain, ×) and picks the body.
//! A body that has not landed yet shows the classic inspector for the
//! same thing, so no control is lost; each such arm is one line, marked
//! `// Until <name>_sheet lands`.

pub(crate) mod diagnostics;
pub(crate) mod eq;
pub(crate) mod gate;

use eframe::egui;
use egui::{Align, CornerRadius, Layout, RichText, Stroke, Vec2};

use crate::app::screen::Sheet;
use crate::app::RigWerkApp;
use crate::ui::fonts::Face;
use crate::ui::theme::{font, palette, TypeRole};
use crate::ui::widgets::{row_info, segmented};

/// Buttons inside a sheet are 28 pt high (§4.5).
const SHEET_BUTTON_H: f32 = 28.0;

impl RigWerkApp {
    /// The open sheet's title, without the path: *Pre EQ*, an effect's
    /// name, an output's name.
    pub(crate) fn sheet_title(&self, sheet: Sheet) -> String {
        match sheet {
            Sheet::Diagnostics => "Diagnostics".to_owned(),
            Sheet::Scope => "Scope".to_owned(),
            Sheet::Eq { post: false, .. } => "Pre EQ".to_owned(),
            Sheet::Eq { post: true, .. } => "Post EQ".to_owned(),
            Sheet::Gate { .. } => "Gate".to_owned(),
            Sheet::Path { .. } => "Path".to_owned(),
            Sheet::Effect { path, index } => self
                .engine
                .plugin_views(path)
                .get(index)
                .map(|view| view.name.clone())
                .unwrap_or_else(|| "Effect".to_owned()),
            Sheet::Output { output } => self
                .output_device_rows()
                .get(output)
                .map(|row| row.name.clone())
                .unwrap_or_else(|| "Output".to_owned()),
        }
    }

    /// The header row: path dot and name, `/`, the item, its *On* /
    /// *Bypass*, then ‹ › and × on the right.
    pub(crate) fn sheet_header(&mut self, ui: &mut egui::Ui) {
        let Some(sheet) = self.chrome.sheet else {
            return;
        };
        let colours = palette(ui.ctx());
        let title = self.sheet_title(sheet);
        let path = sheet
            .path()
            .and_then(|slot| self.paths.get(slot).and_then(Option::as_ref))
            .map(|path| (path.colour, path.name.clone()));
        let previous = self.sheet_neighbour(false);
        let next = self.sheet_neighbour(true);
        let mut step = None;
        let mut close = false;
        let mut stage_on = None;
        ui.horizontal(|ui| {
            ui.set_min_height(SHEET_BUTTON_H);
            ui.spacing_mut().item_spacing.x = 8.0;
            if let Some((colour, name)) = &path {
                let (dot, _) = ui.allocate_exact_size(Vec2::splat(10.0), egui::Sense::hover());
                ui.painter().circle_filled(
                    dot.center(),
                    4.0,
                    colours.path[*colour as usize % colours.path.len()],
                );
                ui.label(
                    RichText::new(name)
                        .font(font(Face::SansMedium, TypeRole::Title))
                        .color(colours.text2),
                );
                ui.label(
                    RichText::new("/")
                        .font(font(Face::Sans, TypeRole::Title))
                        .color(colours.text3),
                );
            }
            let heading = ui.label(
                RichText::new(&title)
                    .font(font(Face::SansSemibold, TypeRole::Title))
                    .color(colours.text),
            );
            row_info(&heading, egui::Role::Heading, true, &title, None, None);
            if let Some((slot, stage)) = sheet.stage() {
                ui.add_space(8.0);
                let on = self.stage_on(slot, stage);
                let mut choice = usize::from(!on);
                if segmented(ui, &mut choice, &["On", "Bypass"]).changed() {
                    stage_on = Some((slot, stage, choice == 0));
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if sheet_button(ui, "×", "Close", true).clicked() {
                    close = true;
                }
                if sheet.stage().is_some() {
                    if sheet_button(ui, "›", "Next stage", next.is_some()).clicked() {
                        step = Some(true);
                    }
                    if sheet_button(ui, "‹", "Previous stage", previous.is_some()).clicked() {
                        step = Some(false);
                    }
                }
            });
        });
        if let Some((slot, stage, on)) = stage_on {
            self.set_stage_on(slot, stage, on);
        }
        if let Some(forward) = step {
            self.step_sheet(forward);
        }
        if close {
            self.close_sheet();
        }
    }

    /// The open sheet's body, at most `height` tall; it scrolls inside.
    pub(crate) fn sheet_body(&mut self, ui: &mut egui::Ui, height: f32) {
        let Some(sheet) = self.chrome.sheet else {
            return;
        };
        // The classic inspectors edit the selected path.
        if let Some(path) = sheet.path() {
            self.select_path(path);
        }
        let ctx = ui.ctx().clone();
        egui::ScrollArea::vertical()
            .id_salt(("sheet_body", format!("{sheet:?}")))
            .max_height(height)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                let width = ui.available_width();
                match sheet {
                    Sheet::Diagnostics => self.diagnostics_page(ui),
                    Sheet::Eq { path, post } => self.eq_sheet(ui, path, post),
                    Sheet::Gate { path } => self.gate_sheet(ui, path),
                    // Until effect_sheet lands
                    Sheet::Effect { index, .. } => self.effect_inspector(ui, index),
                    // Until path_sheet lands
                    Sheet::Path { path } => self.path_stage_panel(ui, path),
                    // Until output_sheet lands
                    Sheet::Output { output } => self.output_panels(ui, output),
                    // Until scope_sheet lands
                    Sheet::Scope => self.scope_card(ui, &ctx, width),
                }
            });
    }

    /// The Mixer's inspector for output `output` beside Signal Routing's map
    /// for it.
    fn output_panels(&mut self, ui: &mut egui::Ui, output: usize) {
        let width = ui.available_width();
        let left = (width * 0.4).clamp(280.0, 460.0);
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(Vec2::new(left, 0.0), Layout::top_down(Align::LEFT), |ui| {
                ui.set_width(left);
                self.output_device_panel(ui, output);
            });
            ui.add_space(16.0);
            ui.allocate_ui_with_layout(
                Vec2::new((width - left - 24.0).max(200.0), 0.0),
                Layout::top_down(Align::LEFT),
                |ui| self.routing_device_panel(ui, output),
            );
        });
    }
}

/// An outline button 28 pt high.  `name` is what it does, for screen
/// readers and hover, when the label is a glyph.
fn sheet_button(ui: &mut egui::Ui, label: &str, name: &str, enabled: bool) -> egui::Response {
    let colours = palette(ui.ctx());
    let button = egui::Button::new(
        RichText::new(label)
            .font(font(Face::Sans, TypeRole::Control))
            .color(if enabled { colours.text } else { colours.text3 }),
    )
    .fill(colours.slot)
    .stroke(Stroke::new(1.0, colours.line2))
    .corner_radius(CornerRadius::same(4))
    .min_size(Vec2::new(SHEET_BUTTON_H, SHEET_BUTTON_H));
    let response = ui.add_enabled(enabled, button);
    row_info(&response, egui::Role::Button, enabled, name, None, None);
    response.on_hover_text(name)
}

#[cfg(test)]
mod tests;
