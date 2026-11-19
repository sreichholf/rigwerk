//! Bottom sheets: the header every sheet shares (`docs/ui-spec.md` §4.3)
//! and the dispatch to each sheet's body.
//!
//! The shell draws the sheet's frame, height and drag edge.  This file
//! draws the header (the path dot and name, `/`, the item, *On* /
//! *Bypass* for a stage, ‹ › through the chain, ×) and picks the body.

pub(crate) mod amp;
pub(crate) mod diagnostics;
pub(crate) mod effect;
pub(crate) mod eq;
pub(crate) mod gate;
pub(crate) mod output;
pub(crate) mod path;
pub(crate) mod scope;

use eframe::egui;
use egui::{Align, Layout, RichText, Vec2};

use crate::app::screen::Sheet;
use crate::app::RigWerkApp;
use crate::ui::fonts::Face;
use crate::ui::theme::{font, palette, TypeRole};
use crate::ui::widgets::{row_info, segmented, OutlineButton, CONTROL_H};

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
            Sheet::Amp { .. } => "Amp".to_owned(),
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
            ui.set_min_height(CONTROL_H);
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
                if OutlineButton::new("×")
                    .name("Close")
                    .show(ui)
                    .on_hover_text("Close")
                    .clicked()
                {
                    close = true;
                }
                if sheet.stage().is_some() {
                    if OutlineButton::new("›")
                        .name("Next stage")
                        .enabled(next.is_some())
                        .show(ui)
                        .on_hover_text("Next stage")
                        .clicked()
                    {
                        step = Some(true);
                    }
                    if OutlineButton::new("‹")
                        .name("Previous stage")
                        .enabled(previous.is_some())
                        .show(ui)
                        .on_hover_text("Previous stage")
                        .clicked()
                    {
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
        // The sheet edits its own path.
        if let Some(path) = sheet.path() {
            self.select_path(path);
        }
        egui::ScrollArea::vertical()
            .id_salt(("sheet_body", format!("{sheet:?}")))
            .max_height(height)
            .auto_shrink([false, true])
            .show(ui, |ui| match sheet {
                Sheet::Diagnostics => self.diagnostics_page(ui),
                Sheet::Eq { path, post } => self.eq_sheet(ui, path, post),
                Sheet::Gate { path } => self.gate_sheet(ui, path),
                Sheet::Amp { path } => self.amp_sheet(ui, path),
                Sheet::Effect { path, index } => self.effect_sheet(ui, path, index),
                Sheet::Path { path } => self.path_sheet(ui, path),
                Sheet::Output { output } => self.output_sheet(ui, output),
                Sheet::Scope => self.scope_sheet(ui),
            });
    }
}

#[cfg(test)]
mod tests;
