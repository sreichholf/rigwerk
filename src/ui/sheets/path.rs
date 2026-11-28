//! The path sheet.  A path and a return edit the same send.  A return has
//! no solo: it is solo-safe.  Every edit goes through a `RigWerkApp` method.

use eframe::egui;
use egui::{Align, CornerRadius, Layout, RichText, Sense, Stroke, Vec2};

use crate::app::layouts::{custom_label, PathLayout};
use crate::app::screen::format_ms;
use crate::app::{LogTone, RigWerkApp};
use crate::audio::{PathFeed, PATH_LEVEL_MAX_DB, PATH_LEVEL_MIN_DB, SEND_OFF_DB};
use crate::ui::fonts::Face;
use crate::ui::theme::{font, palette, Palette, TypeRole, PATH_COLOUR_NAMES};
use crate::ui::widgets::{
    chip, format_db, heading, level_row, row_info, segmented, styled_slider, SliderStyle,
    TextField, ROW_H, VALUE_W,
};

use crate::ui::widgets::OutlineButton;

const COLUMN_GAP: f32 = 24.0;

impl RigWerkApp {
    pub(crate) fn path_sheet(&mut self, ui: &mut egui::Ui, slot: usize) {
        let Some(path) = self.paths.get(slot).and_then(Option::as_ref).cloned() else {
            return;
        };
        let colours = palette(ui.ctx());
        let colour = colours.path[path.colour as usize % colours.path.len()];
        let mut level = path.mix.level_db;
        let mut match_db = path.mix.match_db;
        let mut enabled = path.mix.output_enabled;
        let mut routes = path.mix.routes;
        let mut mono = path.mix.mono;
        let mut mute = path.mix.mute;
        let mut solo = path.mix.solo;
        let mut feed = path.mix.feed;
        let mut send_edits: Vec<(usize, usize, f32)> = Vec::new();
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
                    let field = TextField::new(&mut draft, "Name")
                        .hint("Path name")
                        .width(column_w.min(320.0))
                        .report_value()
                        .show(ui);
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
                    let mut choice = usize::from(feed == PathFeed::Sends);
                    let response = segmented(ui, &mut choice, &["Input", "Sends"]);
                    if response.changed() {
                        feed = if choice == 1 {
                            PathFeed::Sends
                        } else {
                            PathFeed::Input
                        };
                    }
                    response.on_hover_text(
                        "Input: the interface, through the grid below.\n\
                         Sends: the other paths' sends, after their level. One reverb \
                         or delay here serves every path.",
                    );
                    ui.add_space(8.0);
                    if path.mix.is_return() {
                        heading(ui, "Sends from", colours);
                        let senders = self.sender_slots();
                        if senders.is_empty() {
                            note(ui, "No path plays the input yet.", colours);
                        }
                        for sender in senders {
                            let name = self.paths[sender]
                                .as_ref()
                                .map_or_else(String::new, |p| p.name.clone());
                            let mut level = self.path_send(sender, slot);
                            if send_row(
                                ui,
                                &format!("Send from {name}"),
                                &name,
                                &mut level,
                                colour,
                                colours,
                            ) {
                                send_edits.push((sender, slot, level));
                            }
                        }
                        note(
                            ui,
                            "Each send is taken after that path's level, mute and solo.",
                            colours,
                        );
                    } else {
                        source_grid_widget(ui, &mut routes, colours);
                        ui.add_space(8.0);
                        let mut choice = usize::from(mono);
                        if segmented(ui, &mut choice, &["Stereo", "Mono"]).changed() {
                            mono = choice == 1;
                        }
                        let returns = self.return_slots();
                        if !returns.is_empty() {
                            ui.add_space(12.0);
                            heading(ui, "Sends", colours);
                            for ret in returns {
                                let name = self.paths[ret]
                                    .as_ref()
                                    .map_or_else(String::new, |p| p.name.clone());
                                let mut level = self.path_send(slot, ret);
                                if send_row(
                                    ui,
                                    &format!("Send to {name}"),
                                    &name,
                                    &mut level,
                                    colour,
                                    colours,
                                ) {
                                    send_edits.push((slot, ret, level));
                                }
                            }
                        }
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
                    level_row(
                        ui,
                        &mut level,
                        PATH_LEVEL_MIN_DB,
                        PATH_LEVEL_MAX_DB,
                        colour,
                        "This path's level, before the master",
                        colours,
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        if chip(ui, &mut mute, "Mute")
                            .on_hover_text("Silence this path on every output")
                            .changed()
                        {
                            self.set_path_mute(slot, mute);
                        }
                        if !path.mix.is_return()
                            && chip(ui, &mut solo, "Solo")
                                .on_hover_text("While any path is soloed, only soloed paths sound")
                                .changed()
                        {
                            self.set_path_solo(slot, solo);
                        }
                    });
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        ui.label(
                            RichText::new("Match")
                                .font(font(Face::Sans, TypeRole::Secondary))
                                .color(colours.text2),
                        );
                        let response = ui.add(
                            egui::DragValue::new(&mut match_db)
                                .speed(0.1)
                                .range(
                                    -crate::audio::PATH_MATCH_MAX_DB
                                        ..=crate::audio::PATH_MATCH_MAX_DB,
                                )
                                .custom_formatter(|value, _| format_db(value as f32, 1)),
                        );
                        row_info(
                            &response,
                            egui::Role::Slider,
                            true,
                            "Path match",
                            None,
                            Some(format_db(match_db, 1)),
                        );
                        if response.double_clicked() {
                            match_db = 0.0;
                        }
                        if response.changed() || response.double_clicked() {
                            self.set_path_match(slot, match_db);
                        }
                        if match_db.abs() >= 0.05
                            && OutlineButton::new("Clear")
                                .show(ui)
                                .on_hover_text("Set this path's match trim back to 0 dB")
                                .clicked()
                        {
                            match_db = 0.0;
                            self.set_path_match(slot, 0.0);
                        }
                        if self.path_slots().len() >= 2
                            && OutlineButton::new("Match paths…")
                                .show(ui)
                            .on_hover_text(
                                "Measure the paths and set one loudness trim per path",
                            )
                            .clicked()
                        {
                            self.open_match_paths();
                        }
                    });
                    note(
                        ui,
                        "Set by Match paths, or drag it. Applied before the fader, so the fader stays your own balance.",
                        colours,
                    );
                    ui.add_space(12.0);
                    heading(ui, "On each output", colours);
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
                    if OutlineButton::new("Duplicate (muted)")
                        .enabled(can_add)
                        .show(ui)
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
                    let apply = OutlineButton::new(&caption)
                        .name("Apply layout")
                        .show(ui)
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
        for (sender, ret, level) in send_edits {
            self.set_path_send(sender, ret, level);
        }
        if feed != path.mix.feed {
            self.set_path_feed(slot, feed);
        }
        if routes != path.mix.routes && feed == PathFeed::Input {
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

fn note(ui: &mut egui::Ui, text: &str, colours: &Palette) {
    ui.label(
        RichText::new(text)
            .font(font(Face::Sans, TypeRole::GridHeader))
            .color(colours.text3),
    );
}

/// A send level as it reads: *Off* at the bottom of the range.
pub(crate) fn send_label(db: f32) -> String {
    if db <= SEND_OFF_DB {
        "Off".to_owned()
    } else {
        format_db(db, 1)
    }
}

/// One send: the other path's name, a slider and the level.  `name` is
/// what the slider is called for a screen reader.  Returns whether it
/// moved.
fn send_row(
    ui: &mut egui::Ui,
    name: &str,
    label: &str,
    level: &mut f32,
    colour: egui::Color32,
    colours: &Palette,
) -> bool {
    const LABEL_W: f32 = 96.0;
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.allocate_ui_with_layout(
            Vec2::new(LABEL_W, ROW_H),
            Layout::left_to_right(Align::Center),
            |ui| {
                ui.set_width(LABEL_W);
                ui.add(
                    egui::Label::new(
                        RichText::new(label)
                            .font(font(Face::Sans, TypeRole::Body))
                            .color(colours.text2),
                    )
                    .truncate(),
                );
            },
        );
        let slider_w = (ui.available_width() - VALUE_W - 8.0).max(80.0);
        let response = styled_slider(
            ui,
            level,
            SEND_OFF_DB,
            PATH_LEVEL_MAX_DB,
            0.5,
            SliderStyle::named(colour, name)
                .value(send_label(*level))
                .width(slider_w),
        )
        .on_hover_text("Send level, after the sending path's level. All the way down is off.");
        changed = response.changed();
        ui.allocate_ui_with_layout(
            Vec2::new(VALUE_W, ROW_H),
            Layout::right_to_left(Align::Center),
            |ui| {
                ui.set_width(VALUE_W);
                ui.label(
                    RichText::new(send_label(*level))
                        .font(font(Face::Mono, TypeRole::Secondary))
                        .color(colours.text),
                );
            },
        );
    });
    changed
}

#[cfg(test)]
mod tests;
