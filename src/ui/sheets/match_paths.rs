//! The Match paths sheet: the reference, the measurement, and one proposed
//! trim per path.  Nothing is written until Apply.

use eframe::egui;
use egui::{Align, Layout, RichText, Vec2};

use crate::app::match_paths::{MatchPathReason, MATCH_PATHS_SECONDS};
use crate::app::RigWerkApp;
use crate::audio::{LOUDNESS_TARGET_MAX_LUFS, LOUDNESS_TARGET_MIN_LUFS, SILENCE_LUFS};
use crate::ui::fonts::Face;
use crate::ui::theme::{font, palette, Palette, TypeRole};
use crate::ui::widgets::{
    format_db, row_info, segmented, styled_slider, OutlineButton, SliderStyle,
};

const COLUMN_GAP: f32 = 24.0;
const MIN_TWO_COLUMNS_W: f32 = 760.0;

impl RigWerkApp {
    pub(crate) fn match_paths_sheet(&mut self, ui: &mut egui::Ui) {
        let colours = palette(ui.ctx());
        ui.label(
            RichText::new(
                "Brings each path to the same loudness, measured after its chain and before \
                 its fader. The faders stay where they are.",
            )
            .font(font(Face::Sans, TypeRole::Secondary))
            .color(colours.text3),
        );
        ui.add_space(12.0);
        let width = ui.available_width();
        if width >= MIN_TWO_COLUMNS_W {
            let column = (width - COLUMN_GAP) / 2.0;
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = COLUMN_GAP;
                ui.allocate_ui_with_layout(
                    Vec2::new(column, 0.0),
                    Layout::top_down(Align::LEFT),
                    |ui| {
                        ui.set_width(column);
                        self.match_reference(ui, colours);
                    },
                );
                ui.allocate_ui_with_layout(
                    Vec2::new(column, 0.0),
                    Layout::top_down(Align::LEFT),
                    |ui| {
                        ui.set_width(column);
                        self.match_results(ui, colours);
                    },
                );
            });
        } else {
            self.match_reference(ui, colours);
            ui.add_space(12.0);
            self.match_results(ui, colours);
        }
    }

    fn match_reference(&mut self, ui: &mut egui::Ui, colours: &Palette) {
        heading(ui, "Reference", colours);
        let mut choice = usize::from(self.match_paths_reference.is_some());
        let response = segmented(ui, &mut choice, &["Target", "A path"]);
        if response.changed() {
            let reference = if choice == 0 {
                None
            } else {
                self.match_paths_reference
                    .or_else(|| self.path_slots().first().copied())
            };
            self.set_match_reference(reference);
        }
        ui.add_space(8.0);
        if let Some(reference) = self.match_paths_reference {
            let slots = self.path_slots();
            let mut selected: Option<usize> = None;
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                for slot in slots {
                    let name = self.paths[slot]
                        .as_ref()
                        .map_or_else(String::new, |path| path.name.clone());
                    let mut on = slot == reference;
                    if crate::ui::widgets::chip(ui, &mut on, &name)
                        .on_hover_text("Match the other paths to this one; it keeps its trim")
                        .changed()
                        && on
                    {
                        selected = Some(slot);
                    }
                }
            });
            if let Some(slot) = selected {
                if self.match_paths_reference != Some(slot) {
                    self.set_match_reference(Some(slot));
                }
            }
            ui.label(
                RichText::new("That path keeps its trim; the others are matched to it.")
                    .font(font(Face::Sans, TypeRole::GridHeader))
                    .color(colours.text3),
            );
        } else {
            let mut target = self.match_paths.target_lufs;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let slider_w = (ui.available_width() - 80.0 - 8.0).max(80.0);
                let reading = format!("{target:.1} LUFS");
                styled_slider(
                    ui,
                    &mut target,
                    LOUDNESS_TARGET_MIN_LUFS,
                    LOUDNESS_TARGET_MAX_LUFS,
                    0.5,
                    SliderStyle::named(colours.accent, "Reference loudness")
                        .value(reading)
                        .width(slider_w),
                )
                .on_hover_text(
                    "Every ticked path is brought to this loudness with the level fader at 0 dB.",
                );
                ui.label(
                    RichText::new(format!("{target:.1} LUFS"))
                        .font(font(Face::Mono, TypeRole::Secondary))
                        .color(colours.text),
                );
            });
            if (target - self.match_paths.target_lufs).abs() > 0.001 {
                self.match_paths.target_lufs = target;
                self.match_paths_result = None;
                self.match_paths_ticks.clear();
                self.match_paths_status = None;
            }
            ui.label(
                RichText::new(
                    "Every ticked path is brought to this loudness. The default −18 LUFS \
                     leaves headroom before the outputs.",
                )
                .font(font(Face::Sans, TypeRole::GridHeader))
                .color(colours.text3),
            );
        }

        ui.add_space(12.0);
        heading(ui, "Measure", colours);
        if self.is_matching_paths() {
            ui.label(
                RichText::new(format!("Play the rig… {:.0} s", MATCH_PATHS_SECONDS))
                    .font(font(Face::SansMedium, TypeRole::Body))
                    .color(colours.warn),
            );
            let progress = self.match_paths_progress();
            ui.add(
                egui::ProgressBar::new(progress)
                    .desired_width(ui.available_width().min(260.0))
                    .fill(colours.warn),
            );
        } else if OutlineButton::new(if self.match_paths_result.is_some() {
            "Measure again"
        } else {
            "Measure"
        })
        .show(ui)
        .on_hover_text("Measure every path for four seconds; nothing changes until Apply")
        .clicked()
        {
            self.start_match_paths();
        }
        ui.label(
            RichText::new(
                "Play as you normally would for about 4 s. Every path, muted or not, is \
                 measured on its own, all at once, because they all hear the same input.",
            )
            .font(font(Face::Sans, TypeRole::GridHeader))
            .color(colours.text3),
        );
    }

    fn match_results(&mut self, ui: &mut egui::Ui, colours: &Palette) {
        heading(ui, "Paths", colours);
        let status = if self.is_matching_paths() {
            "Measuring… play the rig."
        } else if self.match_paths_result.is_none() {
            "Measure first."
        } else {
            self.match_paths_status
                .as_deref()
                .unwrap_or("Nothing changes until Apply.")
        };
        ui.label(
            RichText::new(status)
                .font(font(Face::SansMedium, TypeRole::Body))
                .color(if self.is_matching_paths() {
                    colours.warn
                } else if self.match_paths_status.is_some() {
                    colours.accent_text
                } else {
                    colours.text2
                }),
        );
        let Some(result) = self.match_paths_result.clone() else {
            ui.label(
                RichText::new(
                    "Measure every path to see its loudness and the trim that would match it.",
                )
                .font(font(Face::Sans, TypeRole::Secondary))
                .color(colours.text3),
            );
            return;
        };
        if self.match_paths_ticks.len() != result.rows.len() {
            self.match_paths_ticks = result.rows.iter().map(|row| row.ticked).collect();
        }
        let mut can_apply = false;
        egui::Grid::new("match_paths_results")
            .spacing(Vec2::new(10.0, 4.0))
            .show(ui, |ui| {
                for label in ["", "", "Path", "Measured", "Now", "New", "Change"] {
                    ui.label(
                        RichText::new(label)
                            .font(font(Face::Mono, TypeRole::GridHeader))
                            .color(colours.text3),
                    );
                }
                ui.end_row();
                for (index, row) in result.rows.iter().enumerate() {
                    let tickable = row.reason.is_none_or(MatchPathReason::blocks_apply);
                    let mut ticked = self
                        .match_paths_ticks
                        .get(index)
                        .copied()
                        .unwrap_or(row.ticked);
                    let response =
                        ui.add_enabled(tickable, egui::Checkbox::without_text(&mut ticked));
                    let name = self.paths[row.slot].as_ref().map_or_else(
                        || format!("Path {}", row.slot + 1),
                        |path| path.name.clone(),
                    );
                    row_info(
                        &response,
                        egui::Role::CheckBox,
                        tickable,
                        format!("Match {name}"),
                        Some(ticked),
                        None,
                    );
                    if let Some(entry) = self.match_paths_ticks.get_mut(index) {
                        *entry = ticked;
                    }
                    let is_reference = result.reference_path == Some(row.slot);
                    let changed = row.change_db.abs() >= 0.05;
                    if ticked && tickable && changed {
                        can_apply = true;
                    }
                    let (dot, _) = ui.allocate_exact_size(Vec2::splat(10.0), egui::Sense::hover());
                    ui.painter().circle_filled(
                        dot.center(),
                        4.0,
                        colours.path[self.paths[row.slot].as_ref().map_or(0, |path| path.colour)
                            as usize
                            % colours.path.len()],
                    );
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(&name)
                                .font(font(Face::Sans, TypeRole::Body))
                                .color(colours.text2),
                        );
                        if is_reference {
                            ui.label(
                                RichText::new("reference")
                                    .font(font(Face::Sans, TypeRole::GridHeader))
                                    .color(colours.text3),
                            );
                        } else if let Some(reason) = row.reason {
                            ui.label(
                                RichText::new(reason.label())
                                    .font(font(Face::Sans, TypeRole::GridHeader))
                                    .color(if tickable {
                                        colours.text3
                                    } else {
                                        colours.warn
                                    }),
                            );
                        }
                    });
                    ui.label(
                        RichText::new(measured_label(row.measured_lufs))
                            .font(font(Face::Mono, TypeRole::Secondary))
                            .color(colours.text),
                    );
                    ui.label(
                        RichText::new(format_db(row.current_db, 1))
                            .font(font(Face::Mono, TypeRole::Secondary))
                            .color(colours.text2),
                    );
                    ui.label(
                        RichText::new(format_db(row.new_db, 1))
                            .font(font(Face::Mono, TypeRole::Secondary))
                            .color(if ticked && changed {
                                colours.accent_text
                            } else {
                                colours.text2
                            }),
                    );
                    let change = if is_reference {
                        "—".to_owned()
                    } else {
                        format_db(row.change_db, 1)
                    };
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(change)
                                .font(font(Face::Mono, TypeRole::Secondary))
                                .color(if is_reference {
                                    colours.text3
                                } else if ticked && changed {
                                    colours.accent_text
                                } else {
                                    colours.text
                                }),
                        );
                        if row.clamped {
                            ui.label(
                                RichText::new("limit")
                                    .font(font(Face::Mono, TypeRole::GridHeader))
                                    .color(colours.warn),
                            );
                        }
                    });
                    ui.end_row();
                }
            });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            if accent_button(ui, "Apply", can_apply)
                .on_hover_text("Write the ticked trims; they ramp in")
                .clicked()
            {
                let ticks = self.match_paths_ticks.clone();
                self.apply_match_paths(&ticks);
            }
            if OutlineButton::new("Cancel")
                .show(ui)
                .on_hover_text("Change nothing")
                .clicked()
            {
                self.cancel_match_paths();
                self.close_sheet();
            }
        });
        ui.label(
            RichText::new(
                "For paths you switch between. For paths that play together, untick all but one, \
                 or Learn the output afterwards.",
            )
            .font(font(Face::Sans, TypeRole::GridHeader))
            .color(colours.text3),
        );
    }
}

fn accent_button(ui: &mut egui::Ui, label: &str, enabled: bool) -> egui::Response {
    let colours = palette(ui.ctx());
    let text_font = font(Face::SansMedium, TypeRole::Body);
    let button = egui::Button::new(RichText::new(label).font(text_font).color(if enabled {
        colours.accent_text
    } else {
        colours.text3
    }))
    .fill(if enabled {
        colours.accent_bg
    } else {
        colours.slot
    })
    .stroke(egui::Stroke::new(
        1.0,
        if enabled {
            colours.accent
        } else {
            colours.line2
        },
    ))
    .min_size(Vec2::new(80.0, 28.0));
    let response = ui.add_enabled(enabled, button);
    row_info(&response, egui::Role::Button, enabled, label, None, None);
    response
}

fn measured_label(lufs: f32) -> String {
    if lufs > SILENCE_LUFS {
        format!("{lufs:.1} LUFS")
    } else {
        "—".to_owned()
    }
}

fn heading(ui: &mut egui::Ui, text: &str, colours: &Palette) {
    ui.label(
        RichText::new(text)
            .font(font(Face::SansMedium, TypeRole::Secondary))
            .color(colours.text3),
    );
    ui.add_space(3.0);
}

#[cfg(test)]
mod tests;
