//! The amp sheet's body: the models folder, the captures in it, where the
//! stage sits, and the trims.
//!
//! A `.nam` file is a frozen capture, so there are no amp knobs.  The
//! header above (`sheets.rs`) already holds *On* / *Bypass*.  The width
//! dial is only there when the loaded file is a SlimmableContainer.  The
//! page calls [`RigWerkApp`] methods; it does not touch the engine.

use eframe::egui;
use egui::{Align, CornerRadius, Layout, RichText, Stroke, Vec2};

use crate::app::RigWerkApp;
use crate::audio::{AmpSlot, AMP_MAX_TRIM_DB, AMP_MIN_TRIM_DB};
use crate::ui::fonts::Face;
use crate::ui::theme::{font, palette, Palette, TypeRole};
use crate::ui::widgets::{row_info, segmented};

const LABEL_W: f32 = 96.0;
const VALUE_W: f32 = 72.0;
const ROW_H: f32 = 28.0;

fn db_label(db: f32) -> String {
    let tenths = (db * 10.0).round();
    if tenths < 0.0 {
        format!("−{:.1} dB", -tenths / 10.0)
    } else {
        format!("{:.1} dB", tenths.abs() / 10.0)
    }
}

impl RigWerkApp {
    /// The amp sheet body for path `path`.
    pub(crate) fn amp_sheet(&mut self, ui: &mut egui::Ui, path: usize) {
        if self.amp_rescan || self.amp_catalog.folder != self.amp_folder {
            self.rescan_amp_folder();
        }
        let Some(signal) = self.paths.get(path).and_then(Option::as_ref) else {
            return;
        };
        let current = signal.mix.amp;
        let model = signal.amp_model.clone();
        let name = signal.amp_name.clone();
        let error = signal.amp_error.clone();
        let slimmable = !signal.amp_slim_steps.is_empty();
        let mismatch = signal.amp_mismatch;
        let file_rate = signal.amp_file_rate;
        let colours = palette(ui.ctx());
        let mut amp = current;
        let mut chosen = None;

        ui.label(
            RichText::new(
                "Neural Amp Modeler profiles (.nam): WaveNet and LSTM captures, \
                 A1 and A2, including slimmable A2 files.  A profile runs at its \
                 own sample rate and is not resampled.  It has no cabinet unless \
                 one was captured with it: add an IR after it.",
            )
            .font(font(Face::Sans, TypeRole::Secondary))
            .color(colours.text3),
        );
        ui.add_space(8.0);

        if let Some(path) = dropped_folder(ui) {
            self.set_amp_folder(path);
            self.rescan_amp_folder();
        }
        // egui drops Ctrl+V when the clipboard has no text.  A folder
        // copied in Explorer is only a file list, so the key never arrives
        // and the field would stay unchanged.  Read that chord here.
        let edit_id = ui.make_persistent_id("amp-models-folder");
        let was_focused = ui.memory(|memory| memory.has_focus(edit_id));
        let nothing_focused = ui.memory(|memory| memory.focused().is_none());
        let window_focused = ui.input(|input| input.focused);
        let chord_down = folder_paste_down(ui);
        let (paste_now, paste_held) = crate::app::amp::paste_chord_edge(
            window_focused && (was_focused || nothing_focused),
            chord_down,
            self.amp_paste_down,
        );
        self.amp_paste_down = paste_held;
        let text_inserts = was_focused && paste_text_queued(ui);
        if paste_now && !text_inserts {
            self.paste_amp_folder();
        }
        let mut folder = self.amp_folder.clone();
        let previous = folder.clone();
        let mut paste_clicked = false;
        let mut browse_clicked = false;
        let mut changed = false;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            // The theme's button padding is taller than this row, so the
            // buttons overflow and sit off the field.  One shared height.
            ui.spacing_mut().button_padding.y = 4.0;
            let control =
                ui.fonts_mut(|fonts| fonts.row_height(&font(Face::Sans, TypeRole::Control)));
            let body = ui.fonts_mut(|fonts| fonts.row_height(&font(Face::Sans, TypeRole::Body)));
            let row_h = control.max(body) + 12.0;
            let paste_w = 80.0;
            let browse_w = 96.0;
            let edit_w = (ui.available_width() - paste_w - browse_w - 16.0).max(80.0);
            let response = ui
                .add(
                    egui::TextEdit::singleline(&mut folder)
                        .id(edit_id)
                        .hint_text("Path to a folder")
                        .desired_width(edit_w)
                        .min_size(Vec2::new(edit_w, row_h))
                        .margin(egui::Margin::symmetric(8, 4))
                        .font(font(Face::Sans, TypeRole::Body)),
                )
                .on_hover_text("Type a path, or press Ctrl+V. A copied folder works too.");
            response.context_menu(|ui| {
                if ui.button("Paste").clicked() {
                    paste_clicked = true;
                    ui.close();
                }
            });
            if folder_button(
                ui,
                "Paste",
                paste_w,
                row_h,
                "Paste a path, or a folder copied in Explorer.",
            )
            .clicked()
            {
                paste_clicked = true;
            }
            if folder_button(ui, "Browse", browse_w, row_h, "Open a folder window.").clicked() {
                browse_clicked = true;
            }
            changed = response.changed();
        });
        if changed {
            self.apply_typed_amp_folder(&previous, folder);
        }
        if paste_clicked {
            self.paste_amp_folder();
        }
        if browse_clicked {
            self.browse_amp_folder();
        }

        let entries = self.amp_catalog.entries.clone();
        ui.add_space(8.0);
        if entries.is_empty() {
            let note = if self.amp_folder.is_empty() {
                "Browse for a folder, or paste its path."
            } else if !std::path::Path::new(&self.amp_folder).is_dir() {
                "That folder can't be read."
            } else {
                "No .nam files in that folder."
            };
            ui.label(
                RichText::new(note)
                    .font(font(Face::Sans, TypeRole::Body))
                    .color(colours.text2),
            );
        } else {
            for entry in &entries {
                let selected = entry.path == model;
                let response = ui.selectable_label(selected, &entry.name);
                if response.clicked() {
                    chosen = Some(entry.path.clone());
                }
            }
        }

        ui.add_space(12.0);
        let mut position = usize::from(amp.slot == AmpSlot::Post);
        if segmented(ui, &mut position, &["Before effects", "After effects"]).changed() {
            amp.slot = if position == 0 {
                AmpSlot::Pre
            } else {
                AmpSlot::Post
            };
        }

        ui.add_space(8.0);
        trim_row(
            ui,
            "Input",
            "Gain into the model.",
            &mut amp.input_db,
            colours,
        );
        trim_row(
            ui,
            "Output",
            "Gain on the model output.",
            &mut amp.output_db,
            colours,
        );
        if slimmable {
            width_row(ui, &mut amp.slim, colours);
        }

        ui.add_space(8.0);
        if mismatch {
            let open = self.amp_rate_for_sheet();
            ui.label(
                RichText::new(format!(
                    "This capture is {file_rate} Hz. The output is open at {open} Hz."
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
        if !name.is_empty() && error.is_none() {
            ui.label(
                RichText::new(&name)
                    .font(font(Face::Sans, TypeRole::Secondary))
                    .color(colours.text3),
            );
        }

        if let Some(path_chosen) = chosen {
            self.set_path_amp_model(path, path_chosen);
        }
        if amp != current {
            self.select_path(path);
            self.set_path_amp(amp);
        }
    }

    fn amp_rate_for_sheet(&self) -> u32 {
        match self.snapshot.output_rate {
            0 if self.output_rate > 0 => self.output_rate,
            0 => self.sample_rate.max(1),
            rate => rate,
        }
    }
}

fn trim_row(ui: &mut egui::Ui, name: &str, hint: &str, value: &mut f32, colours: &Palette) {
    slider_row(
        ui,
        name,
        hint,
        value,
        AMP_MIN_TRIM_DB..=AMP_MAX_TRIM_DB,
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
    ui.visuals_mut().selection.bg_fill = colours.accent;
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
            ui.spacing_mut().slider_width = slider_w;
            let response = ui
                .add(
                    egui::Slider::new(value, range)
                        .show_value(false)
                        .step_by(f64::from(step))
                        .handle_shape(egui::style::HandleShape::Circle)
                        .trailing_fill(true),
                )
                .on_hover_text(hint);
            let text = if decibels {
                db_label(*value)
            } else {
                format!("{value:.2}")
            };
            row_info(
                &response,
                egui::Role::Slider,
                true,
                name,
                None,
                Some(text.clone()),
            );
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

fn folder_button(
    ui: &mut egui::Ui,
    label: &str,
    width: f32,
    height: f32,
    hint: &str,
) -> egui::Response {
    let colours = palette(ui.ctx());
    let response = ui.add(
        egui::Button::new(
            RichText::new(label)
                .font(font(Face::Sans, TypeRole::Control))
                .color(colours.text),
        )
        .min_size(Vec2::new(width, height))
        .fill(colours.slot)
        .stroke(Stroke::new(1.0, colours.line2))
        .corner_radius(CornerRadius::same(4)),
    );
    row_info(&response, egui::Role::Button, true, label, None, None);
    response.on_hover_text(hint)
}

/// Ctrl+V, or a paste egui already turned into text.
fn folder_paste_down(ui: &egui::Ui) -> bool {
    paste_text_queued(ui) || ctrl_v_key(ui) || crate::app::amp::ctrl_v_is_down()
}

fn paste_text_queued(ui: &egui::Ui) -> bool {
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

fn dropped_folder(ui: &egui::Ui) -> Option<String> {
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
