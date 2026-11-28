//! The NAM sheet.  A `.nam` file is a frozen capture, so there are no amp
//! knobs.  The page calls `RigWerkApp` and does not touch the engine.
//! From the fifth block on, adding a block asks once more.

use eframe::egui;
use egui::{Align, Layout, RichText, Vec2};

use super::catalog::{catalog_columns, catalog_empty_note, CatalogPick, CatalogRow};
use crate::app::screen::Sheet;
use crate::app::RigWerkApp;
use crate::audio::{NamPosition, MAX_NAM_BLOCKS, NAM_MAX_TRIM_DB, NAM_MIN_TRIM_DB, NAM_SOFT_LIMIT};
use crate::ui::fonts::Face;
use crate::ui::theme::{font, palette, Palette, TypeRole};
use crate::ui::widgets::{
    format_db, row_info, segmented, styled_slider, OutlineButton, SliderStyle, TextField, CONTROL_H,
};

const LABEL_W: f32 = 96.0;
const VALUE_W: f32 = 72.0;
const ROW_H: f32 = 28.0;

fn percent(share: f32) -> String {
    format!("{:.0} %", share * 100.0)
}

impl RigWerkApp {
    /// The add view when the stack has no block at `index`.
    pub(crate) fn nam_sheet(&mut self, ui: &mut egui::Ui, path: usize, index: usize) {
        if self.nam_rescan || self.nam_catalog.folder != self.nam_folder {
            self.rescan_models_folder();
        }
        let Some(signal) = self.paths.get(path).and_then(Option::as_ref) else {
            return;
        };
        let block = signal.nam.get(index).cloned();
        let count = signal.nam.len();
        let (last_running_name, is_last_running) = match block.as_ref() {
            Some(block) => {
                let last = signal.nam.iter().rfind(|other| {
                    other.controls.position == block.controls.position && other.active()
                });
                (
                    last.map(|other| other.name.clone()),
                    last.is_some_and(|other| other.controls.id == block.controls.id),
                )
            }
            None => (None, false),
        };
        let path_load: f32 = signal
            .nam
            .iter()
            .filter(|block| block.active())
            .map(|block| block.load_share(self.nam_rate()))
            .sum();
        let colours = palette(ui.ctx());
        let open_rate = self.nam_rate_for_sheet();

        ui.label(
            RichText::new(
                "Neural Amp Modeler profiles (.nam): WaveNet and LSTM captures, \
                 A1 and A2, including slimmable A2 files.  A profile runs at its \
                 own sample rate: one capture is played as it was made.  A stack \
                 of two or more captures that can be oversampled runs them at \
                 twice the output rate, filtered back down, so the aliases stay \
                 out of the audio band.  Blocks run in order, and each one is a \
                 stage of its own: stack a pedal into an amp.",
            )
            .font(font(Face::Sans, TypeRole::Secondary))
            .color(colours.text3),
        );
        ui.add_space(8.0);

        if let Some(path) = dropped_folder(ui) {
            self.set_models_folder(path);
            self.rescan_models_folder();
        }
        // egui drops Ctrl+V when the clipboard has no text.  A folder
        // copied in Explorer is only a file list, so the key never arrives
        // and the field would stay unchanged.  Read that chord here.
        let edit_id = ui.make_persistent_id("nam-models-folder");
        let was_focused = ui.memory(|memory| memory.has_focus(edit_id));
        let nothing_focused = ui.memory(|memory| memory.focused().is_none());
        let window_focused = ui.input(|input| input.focused);
        let chord_down = folder_paste_down(ui);
        let (paste_now, paste_held) = crate::app::nam::paste_chord_edge(
            window_focused && (was_focused || nothing_focused),
            chord_down,
            self.nam_paste_down,
        );
        self.nam_paste_down = paste_held;
        let text_inserts = was_focused && paste_text_queued(ui);
        if paste_now && !text_inserts {
            self.paste_models_folder();
        }
        let mut folder = self.nam_folder.clone();
        let previous = folder.clone();
        let mut paste_clicked = false;
        let mut browse_clicked = false;
        let mut changed = false;
        // The buttons on the right, the field fills the rest of the row.
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
                let response = TextField::new(&mut folder, "Models folder")
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
            });
        });
        if changed {
            self.apply_typed_models_folder(&previous, folder);
        }
        if paste_clicked {
            self.paste_models_folder();
        }
        if browse_clicked {
            self.browse_models_folder();
        }

        let entries = self.nam_catalog.entries.clone();
        ui.add_space(8.0);
        let model = block
            .as_ref()
            .map(|block| block.model.clone())
            .unwrap_or_default();
        let rows: Vec<CatalogRow<'_>> = entries
            .iter()
            .map(|entry| CatalogRow {
                name: entry.name.as_str(),
                key: entry.path.as_str(),
                selected: entry.path == model,
            })
            .collect();
        let empty = entries
            .is_empty()
            .then(|| catalog_empty_note(&self.nam_folder, "No .nam files in that folder."));

        let mut controls = block.as_ref().map(|block| block.controls);
        let mut chosen = None;
        let mut add = false;
        let mut remove = false;
        let mut move_by: i8 = 0;
        let confirming = self.nam_confirm_pending(path);
        let can_add = count < MAX_NAM_BLOCKS;
        let mut cancel_confirm = false;
        let mut added = false;
        let mut bypass_heaviest = false;
        let overloaded =
            self.issue_state.dsp.severity() == Some(crate::app::issues::IssueSeverity::Error);
        let name = block.as_ref().map(|b| b.name.clone()).unwrap_or_default();
        let arch = block.as_ref().map(|b| b.arch.clone()).unwrap_or_default();
        let error = block.as_ref().and_then(|b| b.error.clone());
        let slimmable = block
            .as_ref()
            .is_some_and(|block| !block.slim_steps.is_empty());
        let mismatch = block.as_ref().is_some_and(|block| block.mismatch);
        let file_rate = block.as_ref().map(|block| block.file_rate).unwrap_or(0);
        let calibrated = block
            .as_ref()
            .is_some_and(|block| block.controls.level_in_dbu.is_some());
        let measured = block
            .as_ref()
            .map(|block| (block.load_share(self.nam_rate()), block.load.is_some()))
            .unwrap_or((0.0, false));
        let salt = block.as_ref().map_or_else(
            || "nam-add".to_owned(),
            |block| format!("nam-{}", block.controls.id),
        );

        if let Some(CatalogPick::Entry(index)) = catalog_columns(
            ui,
            &salt,
            None,
            &rows,
            empty.as_deref(),
            |ui| {
                let Some(controls) = controls.as_mut() else {
                    ui.label(
                        RichText::new(
                            "No NAM block on this path yet.  Pick a capture to add one, \
                             or add an empty block and choose its capture here.",
                        )
                        .font(font(Face::Sans, TypeRole::Secondary))
                        .color(colours.text3),
                    );
                    ui.add_space(8.0);
                    let button = OutlineButton::new("Add NAM block")
                        .enabled(can_add)
                        .show(ui);
                    let button = if can_add {
                        button
                    } else {
                        button.on_hover_text(format!(
                            "{MAX_NAM_BLOCKS} NAM blocks is the most one path runs"
                        ))
                    };
                    if button.clicked() {
                        add = true;
                    }
                    if confirming {
                        confirm_row(ui, colours, &mut added, &mut cancel_confirm);
                    }
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(format!(
                            "{} of {MAX_NAM_BLOCKS} blocks.  Adding one from the {}th on \
                             asks first.",
                            count, NAM_SOFT_LIMIT,
                        ))
                        .font(font(Face::Sans, TypeRole::Secondary))
                        .color(colours.text3),
                    );
                    return;
                };

                let mut position = usize::from(controls.position == NamPosition::Post);
                if segmented(ui, &mut position, &["Before effects", "After effects"]).changed() {
                    controls.position = if position == 0 {
                        NamPosition::Pre
                    } else {
                        NamPosition::Post
                    };
                }

                ui.add_space(8.0);
                trim_row(
                    ui,
                    "Input",
                    "Gain into the model.",
                    &mut controls.input_db,
                    colours,
                );
                trim_row(
                    ui,
                    "Output",
                    "Gain on the model output.",
                    &mut controls.output_db,
                    colours,
                );
                if slimmable {
                    width_row(ui, &mut controls.slim, colours);
                }

                ui.add_space(8.0);
                let mut auto = controls.auto_level;
                if checkbox_row(
                    ui,
                    "Auto level",
                    "Match this block's input to the calibration level of the block before \
                     it, when both files carry one.",
                    &mut auto,
                    colours,
                ) {
                    controls.auto_level = auto;
                }
                let note = if calibrated {
                    "This capture carries calibration levels: the match is the block \
                     before it, output level to input level."
                } else {
                    "No calibration data in this capture: adjust Input by ear."
                };
                ui.label(
                    RichText::new(note)
                        .font(font(Face::Sans, TypeRole::Secondary))
                        .color(colours.text3),
                );

                ui.add_space(8.0);
                let loudness = controls
                    .loudness_db
                    .map(|db| format!("Loudness {} (file)", format_db(db, 1)))
                    .unwrap_or_else(|| "No loudness in file".to_owned());
                ui.label(
                    RichText::new(loudness)
                        .font(font(Face::Mono, TypeRole::Secondary))
                        .color(colours.text3),
                );
                let mut normalize = controls.normalize;
                let enabled = controls.loudness_db.is_some() && is_last_running;
                let hint = if controls.loudness_db.is_none() {
                    "This capture has no loudness figure; nothing to normalize.".to_owned()
                } else if !is_last_running {
                    match &last_running_name {
                        Some(name) => format!(
                            "Normalization applies at the last running capture of this group ({name})."
                        ),
                        None => "Normalization applies at the last running capture of this group.".to_owned(),
                    }
                } else {
                    "Normalize from the file's RMS figure, not LUFS.  It applies at the last running capture of this group.  Normalization happens before Match: match the path again after changing it.".to_owned()
                };
                let response = ui.add_enabled_ui(enabled, |ui| {
                    checkbox_row(ui, "Normalize", &hint, &mut normalize, colours)
                });
                if response.inner {
                    controls.normalize = normalize;
                }
                response.response.on_hover_text(&hint);

                if overloaded {
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(
                            "The DSP alarm is up: these blocks are the expensive ones.  \
                             Bypassing the suggested blocks may be enough.",
                        )
                        .font(font(Face::Sans, TypeRole::Secondary))
                        .color(colours.error_text),
                    );
                    if OutlineButton::new("Bypass suggested").show(ui).clicked() {
                        bypass_heaviest = true;
                    }
                }

                ui.add_space(8.0);
                // Both figures are shares of the audio buffer period.  A
                // bypassed block runs no inference, so its own figure is what
                // it would cost when on, and the path total leaves it out.
                let estimated = if measured.1 { "" } else { " (estimated)" };
                let enabled = block.as_ref().is_some_and(|block| block.controls.enabled);
                let own = if enabled {
                    format!("This capture: {}{estimated}", percent(measured.0))
                } else {
                    format!(
                        "Bypassed, costs nothing now (on: {}{estimated})",
                        percent(measured.0)
                    )
                };
                let load = format!(
                    "{own} · NAM on this path: {} of the audio buffer",
                    percent(path_load),
                );
                let response = ui.label(
                    RichText::new(load)
                        .font(font(Face::Mono, TypeRole::Secondary))
                        .color(if path_load > 0.7 {
                            colours.warn
                        } else {
                            colours.text3
                        }),
                );
                row_info(&response, egui::Role::Label, true, "NAM load", None, None);

                ui.add_space(8.0);
                if mismatch {
                    ui.label(
                        RichText::new(format!(
                            "This capture is {file_rate} Hz. The output is open at {open_rate} Hz."
                        ))
                        .font(font(Face::Sans, TypeRole::Secondary))
                        .color(colours.warn),
                    );
                }
                if let Some(error) = error.as_deref() {
                    ui.label(
                        RichText::new(error)
                            .font(font(Face::Sans, TypeRole::Secondary))
                            .color(colours.error_text),
                    );
                }
                if !name.is_empty() {
                    let caption = if arch.is_empty() {
                        name.clone()
                    } else {
                        format!("{name} · {arch}")
                    };
                    ui.label(
                        RichText::new(caption)
                            .font(font(Face::Sans, TypeRole::Secondary))
                            .color(colours.text3),
                    );
                }

                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    let button = OutlineButton::new("Add block").enabled(can_add).show(ui);
                    let button = if can_add {
                        button
                    } else {
                        button.on_hover_text(format!(
                            "{MAX_NAM_BLOCKS} NAM blocks is the most one path runs"
                        ))
                    };
                    if button.clicked() {
                        add = true;
                    }
                    if OutlineButton::new("Remove")
                        .name("Remove block")
                        .show(ui)
                        .clicked()
                    {
                        remove = true;
                    }
                });
                if confirming {
                    confirm_row(ui, colours, &mut added, &mut cancel_confirm);
                }
                ui.horizontal(|ui| {
                    if OutlineButton::new("↑")
                        .name("Move block up")
                        .show(ui)
                        .on_hover_text("Move this block up its group")
                        .clicked()
                    {
                        move_by = -1;
                    }
                    if OutlineButton::new("↓")
                        .name("Move block down")
                        .show(ui)
                        .on_hover_text("Move this block down its group")
                        .clicked()
                    {
                        move_by = 1;
                    }
                });
            },
        ) {
            chosen = Some(entries[index].path.clone());
        }

        // A block that changed sides moved: the rest of this pass edits it
        // at its new index.
        let index = match controls {
            Some(controls)
                if block
                    .as_ref()
                    .is_some_and(|block| block.controls != controls) =>
            {
                self.set_nam_controls(path, index, controls)
                    .unwrap_or(index)
            }
            _ => index,
        };
        if let Some(path_chosen) = chosen {
            if block.is_some() {
                self.set_nam_model(path, index, path_chosen);
            } else if let Some(new_index) = self.add_nam_block(path) {
                self.set_nam_model(path, new_index, path_chosen);
                self.open_sheet(Sheet::Nam {
                    path,
                    index: new_index,
                });
            }
        }
        if cancel_confirm {
            self.cancel_nam_block();
        }
        // The app keeps this sheet on its block (or the one before a removed
        // block), so neither reopens it.
        if remove {
            self.remove_nam_block(path, index);
        }
        if move_by != 0 {
            self.move_nam_block(path, index, isize::from(move_by));
        }
        if bypass_heaviest {
            // The projection over every path picks the set; when it fits but
            // the live DSP is still up (other effects, other paths), the
            // heaviest block here is the one thing this sheet can offer.
            self.check_nam_load();
            if self.nam_warning.is_some() {
                self.bypass_suggested_nam();
            } else {
                self.bypass_heaviest_nam(path, 1);
            }
        }
        if add {
            if let Some(new_index) = self.request_nam_block(path) {
                self.open_sheet(Sheet::Nam {
                    path,
                    index: new_index,
                });
            }
        }
        if added {
            if let Some(new_index) = self.add_nam_block(path) {
                self.open_sheet(Sheet::Nam {
                    path,
                    index: new_index,
                });
            }
        }
    }

    fn nam_rate_for_sheet(&self) -> u32 {
        match self.snapshot.output_rate {
            0 if self.output_rate > 0 => self.output_rate,
            0 => self.sample_rate.max(1),
            rate => rate,
        }
    }
}

/// The one-more-click row from the fifth block on.
fn confirm_row(ui: &mut egui::Ui, colours: &Palette, add: &mut bool, cancel: &mut bool) {
    ui.label(
        RichText::new("This is a lot of NAM blocks; check the DSP load.  Add one more?")
            .font(font(Face::Sans, TypeRole::Secondary))
            .color(colours.warn),
    );
    ui.horizontal(|ui| {
        if OutlineButton::new("Add anyway").show(ui).clicked() {
            *add = true;
        }
        if OutlineButton::new("Cancel").show(ui).clicked() {
            *cancel = true;
        }
    });
}

fn checkbox_row(
    ui: &mut egui::Ui,
    name: &str,
    hint: &str,
    value: &mut bool,
    colours: &Palette,
) -> bool {
    let width = ui.available_width();
    let mut changed = false;
    ui.allocate_ui_with_layout(
        Vec2::new(width, ROW_H),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.set_height(ROW_H);
            ui.allocate_ui_with_layout(
                Vec2::new(LABEL_W, ROW_H),
                Layout::left_to_right(Align::Center),
                |ui| {
                    ui.set_width(LABEL_W);
                    ui.label(
                        RichText::new(name)
                            .font(font(Face::Sans, TypeRole::Body))
                            .color(colours.text2),
                    )
                    .on_hover_text(hint);
                },
            );
            let response = ui.checkbox(value, "");
            changed = response.changed();
            row_info(
                &response,
                egui::Role::CheckBox,
                true,
                name,
                Some(*value),
                None,
            );
        },
    );
    changed
}

fn trim_row(ui: &mut egui::Ui, name: &str, hint: &str, value: &mut f32, colours: &Palette) {
    slider_row(
        ui,
        name,
        hint,
        value,
        NAM_MIN_TRIM_DB..=NAM_MAX_TRIM_DB,
        0.1,
        colours,
        true,
    );
}

fn width_row(ui: &mut egui::Ui, value: &mut f32, colours: &Palette) {
    slider_row(
        ui,
        "Width",
        "Which submodel a slimmable capture runs.",
        value,
        0.0..=1.0,
        0.01,
        colours,
        false,
    );
}

#[allow(clippy::too_many_arguments)]
fn slider_row(
    ui: &mut egui::Ui,
    name: &str,
    hint: &str,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    step: f32,
    colours: &Palette,
    decibels: bool,
) {
    let width = ui.available_width();
    let slider_w = (width - LABEL_W - VALUE_W - 16.0).max(80.0);
    let show = |value: f32| {
        if decibels {
            format_db(value, 1)
        } else {
            format!("{value:.2}")
        }
    };
    ui.allocate_ui_with_layout(
        Vec2::new(width, ROW_H),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.set_height(ROW_H);
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.allocate_ui_with_layout(
                Vec2::new(LABEL_W, ROW_H),
                Layout::left_to_right(Align::Center),
                |ui| {
                    ui.set_width(LABEL_W);
                    ui.label(
                        RichText::new(name)
                            .font(font(Face::Sans, TypeRole::Body))
                            .color(colours.text2),
                    )
                    .on_hover_text(hint);
                },
            );
            styled_slider(
                ui,
                value,
                *range.start(),
                *range.end(),
                step,
                SliderStyle::named(colours.accent, name)
                    .value(show(*value))
                    .width(slider_w),
            )
            .on_hover_text(hint);
            let text = show(*value);
            ui.allocate_ui_with_layout(
                Vec2::new(VALUE_W, ROW_H),
                Layout::right_to_left(Align::Center),
                |ui| {
                    ui.set_width(VALUE_W);
                    ui.label(
                        RichText::new(text)
                            .font(font(Face::Mono, TypeRole::Body))
                            .color(colours.text),
                    );
                },
            );
        },
    );
}

/// Ctrl+V, or a paste egui already turned into text.
pub(crate) fn folder_paste_down(ui: &egui::Ui) -> bool {
    paste_text_queued(ui) || ctrl_v_key(ui) || crate::app::nam::ctrl_v_is_down()
}

pub(crate) fn paste_text_queued(ui: &egui::Ui) -> bool {
    ui.input(|input| {
        input
            .events
            .iter()
            .any(|event| matches!(event, egui::Event::Paste(text) if !text.is_empty()))
    })
}

fn ctrl_v_key(ui: &egui::Ui) -> bool {
    ui.input(|input| {
        input.events.iter().any(|event| match event {
            egui::Event::Key {
                key,
                pressed,
                repeat,
                modifiers,
                ..
            } => {
                *pressed
                    && !*repeat
                    && *key == egui::Key::V
                    && modifiers.command
                    && !modifiers.shift
                    && !modifiers.alt
            }
            _ => false,
        })
    })
}

pub(crate) fn dropped_folder(ui: &egui::Ui) -> Option<String> {
    let pos = ui.input(|input| input.pointer.interact_pos())?;
    if !ui.clip_rect().contains(pos) {
        return None;
    }
    ui.input(|input| {
        input
            .raw
            .dropped_files
            .first()
            .map(|file| file.path().to_string_lossy().into_owned())
    })
}

#[cfg(test)]
mod tests;
