//! The path sheet's body (`docs/ui-spec.md` §6.1): the name and colour,
//! the source (a 2 × 2 grid and *Stereo* / *Mono*), the path's L/R output
//! channels, level, mute and solo, *Duplicate (muted)*, *Apply layout ▸*,
//! *Remove*, and the path's latency on every output it plays on.
//!
//! The header above (`sheets.rs`) names the path and its dot.  Every edit
//! goes through a `RigWerkApp` method; the sheet selects its path before it
//! draws (`sheets.rs`), so the selected-path setters name this path.

use eframe::egui;
use egui::{Align, CornerRadius, Layout, RichText, Sense, Stroke, Vec2};

use crate::app::layouts::{custom_label, PathLayout};
use crate::app::screen::format_ms;
use crate::app::{LogTone, RigWerkApp};
use crate::audio::{PATH_LEVEL_MAX_DB, PATH_LEVEL_MIN_DB};
use crate::ui::fonts::Face;
use crate::ui::theme::{font, palette, Palette, TypeRole};
use crate::ui::widgets::{chip, row_info, segmented};

use super::sheet_button;

const ROW_H: f32 = 28.0;
const VALUE_W: f32 = 80.0;
const COLUMN_GAP: f32 = 24.0;

/// The path colours by saved index, as the menu names them.
pub(crate) const PATH_COLOUR_NAMES: [&str; crate::audio::MAX_PATHS] =
    ["Blue", "Violet", "Green", "Rose"];

/// `docs/ui-spec.md` §2.2: one decimal and the real minus for a level.
pub(crate) fn level_label(db: f32) -> String {
    let tenths = (db * 10.0).round();
    if tenths == 0.0 {
        "0.0 dB".to_owned()
    } else if tenths < 0.0 {
        format!("−{:.1} dB", -tenths / 10.0)
    } else {
        format!("+{:.1} dB", tenths / 10.0)
    }
}

impl RigWerkApp {
    /// The path sheet body for path `slot`.
    pub(crate) fn path_sheet(&mut self, ui: &mut egui::Ui, slot: usize) {
        let Some(path) = self.paths.get(slot).and_then(Option::as_ref).cloned() else {
            return;
        };
        let colours = palette(ui.ctx());
        let colour = colours.path[path.colour as usize % colours.path.len()];
        let mut level = path.mix.level_db;
        let mut enabled = path.mix.output_enabled;
        let mut routes = path.mix.routes;
        let mut mono = path.mix.mono;
        let mut mute = path.mix.mute;
        let mut solo = path.mix.solo;
        let mut rename: Option<String> = None;
        let mut colour_pick: Option<u8> = None;
        let mut duplicate = false;
        let mut remove = false;
        let draft_id = egui::Id::unique(("path_sheet_name", slot));
        let mut draft: String = ui
            .data(|data| data.get_temp(draft_id))
            .unwrap_or_else(|| path.name.clone());

        let width = ui.available_width();
        let two_columns = width >= 2.0 * 340.0 + COLUMN_GAP;
        let column_w = if two_columns {
            (width - COLUMN_GAP) / 2.0
        } else {
            width
        };

        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = COLUMN_GAP;
            ui.allocate_ui_with_layout(
                Vec2::new(column_w, 0.0),
                Layout::top_down(Align::LEFT),
                |ui| {
                    ui.set_width(column_w);
                    heading(ui, "Name", colours);
                    let field = ui.add(
                        egui::TextEdit::singleline(&mut draft)
                            .desired_width(column_w.min(320.0))
                            .hint_text("Path name"),
                    );
                    row_info(
                        &field,
                        egui::Role::TextInput,
                        true,
                        "Name",
                        None,
                        Some(draft.clone()),
                    );
                    if (field.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)))
                        || changed_and_enter(ui, &field)
                    {
                        rename = Some(draft.clone());
                    }
                    ui.add_space(10.0);
                    heading(ui, "Colour", colours);
                    colour_row(ui, path.colour, colour, &mut colour_pick, colours);
                    ui.add_space(10.0);
                    heading(ui, "Source", colours);
                    source_grid_widget(ui, &mut routes, colours);
                    ui.add_space(8.0);
                    let mut choice = usize::from(mono);
                    if segmented(ui, &mut choice, &["Stereo", "Mono"]).changed() {
                        mono = choice == 1;
                    }
                    ui.add_space(12.0);
                    heading(ui, "Path outputs", colours);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        for (side, label) in ["L", "R"].into_iter().enumerate() {
                            let name = format!("Path output {label}");
                            let response = chip(ui, &mut enabled[side], label);
                            let on = enabled[side];
                            row_info(
                                &response,
                                egui::Role::Button,
                                true,
                                name.clone(),
                                Some(on),
                                None,
                            );
                            if response.on_hover_text(&name).changed() {
                                self.set_path_output_enabled(enabled);
                            }
                        }
                    });
                    ui.label(
                        RichText::new(match enabled {
                            [true, true] => "Both channels are sent to the outputs.",
                            [true, false] => "Only the left channel is sent to the outputs.",
                            [false, true] => "Only the right channel is sent to the outputs.",
                            [false, false] => {
                                "Neither channel is sent; the path is silent everywhere."
                            }
                        })
                        .font(font(Face::Sans, TypeRole::GridHeader))
                        .color(colours.text3),
                    );
                },
            );
            ui.allocate_ui_with_layout(
                Vec2::new(column_w, 0.0),
                Layout::top_down(Align::LEFT),
                |ui| {
                    ui.set_width(column_w);
                    heading(ui, "Level", colours);
                    level_row(ui, &mut level, colour, colours);
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        if chip(ui, &mut mute, "Mute")
                            .on_hover_text("Silence this path on every output")
                            .changed()
                        {
                            self.set_path_mute(slot, mute);
                        }
                        if chip(ui, &mut solo, "Solo")
                            .on_hover_text("While any path is soloed, only soloed paths sound")
                            .changed()
                        {
                            self.set_path_solo(slot, solo);
                        }
                    });
                    ui.add_space(12.0);
                    heading(ui, "Latency on each output", colours);
                    let rows = self.output_device_rows();
                    let mut played = 0;
                    for (device, row) in rows.iter().enumerate() {
                        if !self.path_reaches_device(slot, device) {
                            continue;
                        }
                        played += 1;
                        let arrival = self
                            .device_alignment(device)
                            .and_then(|alignment| alignment.arrival_ms[slot]);
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(&row.name)
                                    .font(font(Face::Mono, TypeRole::Secondary))
                                    .color(colours.text2),
                            );
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(
                                    RichText::new(match arrival {
                                        Some(ms) => format_ms(ms),
                                        None => format_ms(self.path_latency_ms(slot)),
                                    })
                                    .font(font(Face::Mono, TypeRole::Secondary))
                                    .color(colours.text),
                                );
                            });
                        });
                    }
                    if played == 0 {
                        ui.label(
                            RichText::new("Plays nowhere: no output has a channel for this path.")
                                .font(font(Face::SansMedium, TypeRole::Body))
                                .color(colours.warn),
                        );
                    }
                    ui.add_space(12.0);
                    let can_add = self.add_path_blocker().is_none();
                    if sheet_button(ui, "Duplicate (muted)", "Duplicate (muted)", can_add)
                        .on_hover_text("A full copy of this chain and its routing, added muted")
                        .clicked()
                    {
                        duplicate = true;
                    }
                    ui.add_space(6.0);
                    let current = self.current_layout();
                    let caption = match current {
                        Some(layout) => format!("Apply layout ▸ · {}", layout.label()),
                        None => format!(
                            "Apply layout ▸ · {}",
                            custom_label(self.routing_last_layout)
                        ),
                    };
                    let apply = sheet_button(ui, &caption, "Apply layout", true)
                        .on_hover_text("Set up paths and routing from a starting point");
                    let mut layout = None;
                    egui::Popup::menu(&apply)
                        .close_behavior(egui::PopupCloseBehavior::CloseOnClick)
                        .show(|ui| {
                            ui.set_min_width(220.0);
                            for choice in PathLayout::ALL {
                                let response = ui
                                    .add(egui::Button::new(
                                        RichText::new(choice.label())
                                            .font(font(Face::Sans, TypeRole::Body))
                                            .color(colours.text),
                                    ))
                                    .on_hover_text(choice.caption());
                                if response.clicked() {
                                    layout = Some(choice);
                                }
                            }
                        });
                    if let Some(layout) = layout {
                        if !self.apply_layout(layout) {
                            self.push_log(
                                format!("{}: no free path slot yet, try again.", layout.label()),
                                LogTone::Amber,
                            );
                        }
                    }
                    ui.add_space(6.0);
                    let can_remove = self.path_count() > 1;
                    let armed = self.chrome.remove_confirm == Some(slot);
                    let label = if armed {
                        "Remove: click again"
                    } else {
                        "Remove"
                    };
                    let button = if armed {
                        egui::Button::new(
                            RichText::new(label)
                                .font(font(Face::SansMedium, TypeRole::Body))
                                .color(colours.bar),
                        )
                        .fill(colours.error)
                        .corner_radius(CornerRadius::same(4))
                        .min_size(Vec2::new(column_w.min(320.0), ROW_H))
                    } else {
                        egui::Button::new(
                            RichText::new(label)
                                .font(font(Face::Sans, TypeRole::Body))
                                .color(colours.error_text),
                        )
                        .fill(colours.slot)
                        .stroke(Stroke::new(1.0, colours.line2))
                        .corner_radius(CornerRadius::same(4))
                        .min_size(Vec2::new(column_w.min(320.0), ROW_H))
                    };
                    let response = ui.add_enabled(can_remove, button);
                    let response = if can_remove {
                        response.on_hover_text("Removes the path and its effects")
                    } else {
                        response.on_disabled_hover_text("The last path cannot be removed")
                    };
                    row_info(&response, egui::Role::Button, can_remove, label, None, None);
                    if response.clicked() && self.request_remove_path(slot) {
                        remove = true;
                    }
                },
            );
        });

        ui.data_mut(|data| data.insert_temp(draft_id, draft.clone()));
        if let Some(name) = rename.filter(|name| name.trim() != path.name) {
            if self.rename_path(slot, &name) {
                ui.data_mut(|data| data.remove::<String>(draft_id));
            } else {
                self.push_log(
                    format!("Another path is already called {}.", name.trim()),
                    LogTone::Amber,
                );
            }
        }
        if let Some(index) = colour_pick {
            self.set_path_colour(slot, index);
        }
        if routes != path.mix.routes {
            self.set_path_routes(slot, routes);
        }
        if mono != path.mix.mono {
            self.set_path_mono(slot, mono);
        }
        if (level - path.mix.level_db).abs() > 0.05 {
            self.set_path_level(slot, level);
        }
        if duplicate {
            let _ = self.duplicate_path(slot);
        }
        if remove {
            self.close_sheet();
        }
    }

    /// The chain latency of path `slot` alone, ms.
    fn path_latency_ms(&self, slot: usize) -> f32 {
        self.path_infos()
            .into_iter()
            .find(|info| info.slot == slot)
            .map_or(0.0, |info| info.latency_ms)
    }
}

fn changed_and_enter(ui: &egui::Ui, field: &egui::Response) -> bool {
    field.has_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter))
}

fn heading(ui: &mut egui::Ui, text: &str, colours: &Palette) {
    ui.label(
        RichText::new(text)
            .font(font(Face::SansMedium, TypeRole::Secondary))
            .color(colours.text3),
    );
    ui.add_space(3.0);
}

fn colour_row(
    ui: &mut egui::Ui,
    current: u8,
    current_colour: egui::Color32,
    pick: &mut Option<u8>,
    colours: &Palette,
) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        for (index, name) in PATH_COLOUR_NAMES.iter().enumerate() {
            let swatch = colours.path[index];
            let selected = current as usize == index;
            let (rect, response) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::click());
            ui.painter()
                .rect_filled(rect, CornerRadius::same(3), colours.slot);
            if selected {
                ui.painter().rect_stroke(
                    rect,
                    CornerRadius::same(3),
                    Stroke::new(1.0, current_colour),
                    egui::StrokeKind::Inside,
                );
            }
            ui.painter().circle_filled(rect.center(), 6.0, swatch);
            row_info(
                &response,
                egui::Role::Button,
                true,
                *name,
                Some(selected),
                None,
            );
            if response.on_hover_text(*name).clicked() {
                *pick = Some(index as u8);
            }
        }
    });
}

/// The 2 × 2 grid: *In 1* / *In 2* down, *L* / *R* across.
fn source_grid_widget(ui: &mut egui::Ui, routes: &mut [[bool; 4]; 2], colours: &Palette) {
    egui::Grid::new("path_sheet_source")
        .spacing(Vec2::new(6.0, 6.0))
        .show(ui, |ui| {
            ui.label("");
            for side in ["L", "R"] {
                ui.label(
                    RichText::new(side)
                        .font(font(Face::Mono, TypeRole::GridHeader))
                        .color(colours.text3),
                );
            }
            ui.end_row();
            for (input, label) in ["In 1", "In 2"].into_iter().enumerate() {
                ui.label(
                    RichText::new(label)
                        .font(font(Face::Sans, TypeRole::Body))
                        .color(colours.text2),
                );
                for (side, side_name) in ["L", "R"].into_iter().enumerate() {
                    let mut on = routes[input][side];
                    let name = format!("{label} to {side_name}");
                    let response = ui.scope(|ui| chip(ui, &mut on, "")).inner;
                    row_info(
                        &response,
                        egui::Role::CheckBox,
                        true,
                        name.clone(),
                        Some(on),
                        None,
                    );
                    if response.on_hover_text(name).changed() {
                        routes[input][side] = on;
                    }
                }
                ui.end_row();
            }
        });
}

fn level_row(ui: &mut egui::Ui, level: &mut f32, colour: egui::Color32, colours: &Palette) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let value_w = VALUE_W;
        ui.spacing_mut().slider_width = (ui.available_width() - value_w - 8.0).max(80.0);
        ui.visuals_mut().selection.bg_fill = colour;
        let response = ui
            .add(
                egui::Slider::new(level, PATH_LEVEL_MIN_DB..=PATH_LEVEL_MAX_DB)
                    .show_value(false)
                    .step_by(0.1)
                    .handle_shape(egui::style::HandleShape::Circle)
                    .trailing_fill(true),
            )
            .on_hover_text("This path's level, before the master");
        row_info(
            &response,
            egui::Role::Slider,
            true,
            "Level",
            None,
            Some(level_label(*level)),
        );
        ui.allocate_ui_with_layout(
            Vec2::new(value_w, ROW_H),
            Layout::right_to_left(Align::Center),
            |ui| {
                ui.set_width(value_w);
                ui.label(
                    RichText::new(level_label(*level))
                        .font(font(Face::Mono, TypeRole::Secondary))
                        .color(colours.text),
                );
            },
        );
    });
}

#[cfg(test)]
mod tests;
