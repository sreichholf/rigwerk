//! Bottom sheets.  The shell draws the frame, height and drag edge.

pub(crate) mod cab;
pub(crate) mod catalog;
pub(crate) mod diagnostics;
pub(crate) mod effect;
pub(crate) mod eq;
pub(crate) mod gate;
pub(crate) mod match_paths;
pub(crate) mod nam;
pub(crate) mod output;
pub(crate) mod path;
pub(crate) mod pitch;
pub(crate) mod scope;

use eframe::egui;
use egui::{Align, Layout, RichText, Vec2};

use crate::app::chain::ChainStage;
use crate::app::screen::Sheet;
use crate::app::RigWerkApp;
use crate::ui::fonts::Face;
use crate::ui::theme::{font, palette, TypeRole};
use crate::ui::widgets::{row_info, segmented, OutlineButton, CONTROL_H};

impl RigWerkApp {
    pub(crate) fn sheet_title(&self, sheet: Sheet) -> String {
        match sheet {
            Sheet::Diagnostics => "Diagnostics".to_owned(),
            Sheet::Scope => "Scope".to_owned(),
            Sheet::Pitch { .. } => "Pitch".to_owned(),
            Sheet::Eq { post: false, .. } => "Pre EQ".to_owned(),
            Sheet::Eq { post: true, .. } => "Post EQ".to_owned(),
            Sheet::Gate { .. } => "Gate".to_owned(),
            Sheet::Nam { .. } => "NAM".to_owned(),
            Sheet::Cab { .. } => "Cab".to_owned(),
            Sheet::Path { .. } => "Path".to_owned(),
            Sheet::MatchPaths => "Match paths".to_owned(),
            Sheet::Effect { path, index } => self
                .engine_plugin_views(path)
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
        let mut pitch = None;
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
            if let Some((slot, ChainStage::Pitch)) = sheet.stage() {
                ui.add_space(8.0);
                if let Some(current) = self
                    .paths
                    .get(slot)
                    .and_then(Option::as_ref)
                    .map(|path| path.mix.pitch)
                {
                    pitch = pitch::state_switch(ui, current).map(|next| (slot, current, next));
                }
            } else if let Some((slot, stage)) = sheet.stage() {
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
        if let Some((slot, current, next)) = pitch {
            if self.select_path(slot) {
                self.apply_pitch(current, next, false);
            }
        }
        if let Some(forward) = step {
            self.step_sheet(forward);
        }
        if close {
            self.close_sheet();
        }
    }

    /// The NAM and cab sheets fill `height` instead of scrolling as one
    /// column.  Their lists scroll in [`catalog::catalog_columns`].
    pub(crate) fn sheet_body(&mut self, ui: &mut egui::Ui, height: f32) {
        let Some(sheet) = self.chrome.sheet else {
            return;
        };
        // The sheet edits its own path.
        if let Some(path) = sheet.path() {
            self.select_path(path);
        }
        // A finite region, so the file list can be as tall as the sheet.
        // Inside the scroll area below, the available height is unbounded.
        if matches!(sheet, Sheet::Nam { .. } | Sheet::Cab { .. }) {
            let width = ui.available_width();
            ui.allocate_ui_with_layout(
                Vec2::new(width, height),
                Layout::top_down(Align::Min),
                |ui| {
                    ui.set_width(width);
                    match sheet {
                        Sheet::Nam { path, index } => self.nam_sheet(ui, path, index),
                        Sheet::Cab { path } => self.cab_sheet(ui, path),
                        _ => unreachable!("only the NAM and cab sheets fill this body"),
                    }
                },
            );
            return;
        }
        egui::ScrollArea::vertical()
            .id_salt(("sheet_body", format!("{sheet:?}")))
            .max_height(height)
            .auto_shrink([false, true])
            .show(ui, |ui| match sheet {
                Sheet::Diagnostics => self.diagnostics_page(ui),
                Sheet::Eq { path, post } => self.eq_sheet(ui, path, post),
                Sheet::Gate { path } => self.gate_sheet(ui, path),
                Sheet::Pitch { path } => self.pitch_sheet(ui, path),
                Sheet::Effect { path, index } => self.effect_sheet(ui, path, index),
                Sheet::Path { path } => self.path_sheet(ui, path),
                Sheet::Output { output } => self.output_sheet(ui, output),
                Sheet::MatchPaths => self.match_paths_sheet(ui),
                Sheet::Scope => self.scope_sheet(ui),
                Sheet::Nam { .. } | Sheet::Cab { .. } => {
                    unreachable!("the NAM and cab sheets fill the body above")
                }
            });
    }
}

#[cfg(test)]
mod tests;
