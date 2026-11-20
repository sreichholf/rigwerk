//! The cabinet sheet's body: the impulse folder, the responses in it, the
//! level, the cuts and the polarity.
//!
//! The header above (`sheets.rs`) already holds *On* / *Bypass*.  The
//! responses and the controls are
//! [`catalog_columns`](super::catalog::catalog_columns), the same split the
//! amp sheet uses.  The page writes [`CabControls`](crate::audio::CabControls)
//! onto the path; the frame's mix push carries them, and a cut or a polarity
//! change is rebaked by `sync_cab_files`.  This file does not call the engine.

use eframe::egui;
use egui::{Align, Layout, RichText};

use super::catalog::{catalog_columns, catalog_empty_note, CatalogPick, CatalogRow};
use crate::app::RigWerkApp;
use crate::audio::cab::cab_note;
use crate::audio::{
    CabControls, CabNote, CAB_MAX_HIGH_CUT_HZ, CAB_MAX_LEVEL_DB, CAB_MAX_LOW_CUT_HZ,
    CAB_MIN_LEVEL_DB,
};
use crate::ui::fonts::Face;
use crate::ui::theme::{font, palette, TypeRole};
use crate::ui::widgets::{row_info, segmented, styled_slider, OutlineButton, TextField, CONTROL_H};

fn db_label(db: f32) -> String {
    let tenths = (db * 10.0).round();
    if tenths < 0.0 {
        format!("−{:.1} dB", -tenths / 10.0)
    } else {
        format!("{:.1} dB", tenths.abs() / 10.0)
    }
}

fn hz_label(hz: f32) -> String {
    if hz <= 0.0 {
        "Off".to_owned()
    } else {
        format!("{hz:.0} Hz")
    }
}

impl RigWerkApp {
    /// The cabinet sheet body for path `path`.
    pub(crate) fn cab_sheet(&mut self, ui: &mut egui::Ui, path: usize) {
        let edit_id = ui.make_persistent_id("cab-impulses-folder");
        let editing = ui.memory(|memory| memory.has_focus(edit_id));
        // A half-typed path is not a folder yet.  Scanning it would trim
        // the space the field is holding.
        if self.cab_rescan || (!editing && self.cab_catalog.folder != self.cab_folder) {
            self.rescan_cab_folder();
        }
        let Some(signal) = self.paths.get(path).and_then(Option::as_ref) else {
            return;
        };
        let current = signal.mix.cab;
        let file = signal.cab_file.clone();
        let error = signal.cab_error.clone();
        let gear = signal
            .mix
            .amp
            .enabled
            .then(|| signal.amp_gear.clone())
            .flatten();
        let ir_active = current.enabled
            && error.is_none()
            && !file.trim().is_empty()
            && !signal.cab_name.is_empty();
        let colours = palette(ui.ctx());
        let mut cab: CabControls = current;
        let mut chosen = None;

        ui.label(
            RichText::new(
                "The cabinet is after the effects, so the effect list sits between \
                 the amp and the speaker.  An impulse response is resampled to the \
                 clock rate.",
            )
            .font(font(Face::Sans, TypeRole::Secondary))
            .color(colours.text3),
        );
        ui.add_space(8.0);

        if let Some(folder) = super::amp::dropped_folder(ui) {
            self.set_cab_folder(folder);
            self.rescan_cab_folder();
        }
        let was_focused = ui.memory(|memory| memory.has_focus(edit_id));
        let nothing_focused = ui.memory(|memory| memory.focused().is_none());
        let window_focused = ui.input(|input| input.focused);
        let chord_down = super::amp::folder_paste_down(ui);
        let (paste_now, paste_held) = crate::app::amp::paste_chord_edge(
            window_focused && (was_focused || nothing_focused),
            chord_down,
            self.cab_paste_down,
        );
        self.cab_paste_down = paste_held;
        let text_inserts = was_focused && super::amp::paste_text_queued(ui);
        if paste_now && !text_inserts {
            self.paste_cab_folder();
        }
        let mut folder = self.cab_folder.clone();
        let previous = folder.clone();
        let mut paste_clicked = false;
        let mut browse_clicked = false;
        let mut changed = false;
        let mut commit = false;
        ui.horizontal(|ui| {
            ui.set_min_height(CONTROL_H);
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if OutlineButton::new("Browse")
                    .show(ui)
                    .on_hover_text("Open a folder window.")
                    .clicked()
                {
                    browse_clicked = true;
                }
                if OutlineButton::new("Paste")
                    .show(ui)
                    .on_hover_text("Paste a path, or a folder copied in Explorer.")
                    .clicked()
                {
                    paste_clicked = true;
                }
                let response = TextField::new(&mut folder, "Impulses folder")
                    .hint("Path to a folder")
                    .id(edit_id)
                    .show(ui)
                    .on_hover_text("Type a path, or press Ctrl+V. A copied folder works too.");
                response.context_menu(|ui| {
                    if ui.button("Paste").clicked() {
                        paste_clicked = true;
                        ui.close();
                    }
                });
                changed = response.changed();
                commit = response.lost_focus();
            });
        });
        if changed || commit {
            self.apply_typed_cab_folder(&previous, folder, commit);
        }
        if paste_clicked {
            self.paste_cab_folder();
        }
        if browse_clicked {
            self.browse_cab_folder();
        }

        let entries = self.cab_catalog.entries.clone();
        ui.add_space(8.0);
        let rows: Vec<CatalogRow<'_>> = entries
            .iter()
            .map(|entry| CatalogRow {
                name: entry.name.as_str(),
                key: entry.path.as_str(),
                selected: entry.path == file,
            })
            .collect();
        let empty = entries
            .is_empty()
            .then(|| catalog_empty_note(&self.cab_folder, "No .wav files in that folder."));
        let note = cab_note(gear.as_deref(), ir_active);
        match catalog_columns(
            ui,
            "cab",
            Some(CatalogRow {
                name: "None",
                key: "none",
                selected: file.trim().is_empty(),
            }),
            &rows,
            empty.as_deref(),
            |ui| {
                let mut noted = false;
                if let Some(error) = error.as_deref() {
                    ui.label(
                        RichText::new(error)
                            .font(font(Face::Sans, TypeRole::Secondary))
                            .color(colours.error_text),
                    );
                    noted = true;
                }
                if let Some(note) = note {
                    let text = match note {
                        CabNote::NoCabinet => {
                            "This capture has no cabinet. Choose an impulse response so it does not sound like a DI."
                        }
                        CabNote::TwoCabinets => {
                            "This capture already includes a cabinet. An impulse response here puts two cabinets in series."
                        }
                    };
                    ui.label(
                        RichText::new(text)
                            .font(font(Face::Sans, TypeRole::Secondary))
                            .color(colours.warn),
                    );
                    noted = true;
                }
                if noted {
                    ui.add_space(12.0);
                }
                value_slider(
                    ui,
                    "Level",
                    "Output trim of the cabinet.",
                    &mut cab.level_db,
                    CAB_MIN_LEVEL_DB..=CAB_MAX_LEVEL_DB,
                    0.1,
                    false,
                );
                value_slider(
                    ui,
                    "Low cut",
                    "High-pass of the impulse. Off leaves the low end alone.",
                    &mut cab.low_cut_hz,
                    0.0..=CAB_MAX_LOW_CUT_HZ,
                    1.0,
                    true,
                );
                value_slider(
                    ui,
                    "High cut",
                    "Low-pass of the impulse. Off leaves the high end alone.",
                    &mut cab.high_cut_hz,
                    0.0..=CAB_MAX_HIGH_CUT_HZ,
                    100.0,
                    true,
                );

                ui.add_space(8.0);
                let mut polarity = usize::from(cab.polarity);
                if segmented(ui, &mut polarity, &["Normal", "Invert"]).changed() {
                    cab.polarity = polarity == 1;
                }
            },
        ) {
            Some(CatalogPick::Leading) => chosen = Some(String::new()),
            Some(CatalogPick::Entry(index)) => chosen = Some(entries[index].path.clone()),
            None => {}
        }

        if cab != current {
            self.select_path(path);
            self.set_path_cab(cab);
        }
        if let Some(chosen) = chosen {
            self.set_path_cab_file(path, chosen);
        }
    }
}

fn value_slider(
    ui: &mut egui::Ui,
    name: &str,
    hint: &str,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    step: f32,
    hertz: bool,
) {
    let colours = palette(ui.ctx());
    let shown = if hertz {
        hz_label(*value)
    } else {
        db_label(*value)
    };
    ui.horizontal(|ui| {
        let label = ui.label(
            RichText::new(name)
                .font(font(Face::Sans, TypeRole::Body))
                .color(colours.text2),
        );
        row_info(
            &label,
            egui::Role::Label,
            true,
            name,
            None,
            Some(shown.clone()),
        );
        label.on_hover_text(hint);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                RichText::new(&shown)
                    .font(font(Face::Mono, TypeRole::Secondary))
                    .color(colours.text),
            );
        });
    });
    styled_slider(
        ui,
        value,
        *range.start(),
        *range.end(),
        step,
        (colours.accent, name),
    )
    .on_hover_text(hint);
}

#[cfg(test)]
mod tests;
