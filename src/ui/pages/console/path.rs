//! A path strip: the 3 pt cap in the path colour, the name and its menu
//! (rename, colour, duplicate, layout, remove with a two-click confirm),
//! the *Source* button and popover, the chain (`chain.rs`), the chain's
//! latency and the path's DSP load, and the level with *M* and *S*.

use eframe::egui;
use egui::{CornerRadius, Rect, RichText, Sense, Vec2};

use super::{level_block, menu_entry, strip_button, strip_parts, LevelBlock, StripLayout};
use crate::app::screen::{format_ms, Sheet};
use crate::app::LogTone;
use crate::app::RigWerkApp;
use crate::audio::{PATH_LEVEL_MAX_DB, PATH_LEVEL_MIN_DB};
use crate::ui::fonts::Face;
use crate::ui::theme::{font, palette, TypeRole};
use crate::ui::widgets::{
    chip, fit_text, horizontal_meter, popover, row_info, segmented, FaderRange,
};

/// The path colours by saved index, as the menu names them.
pub(crate) const PATH_COLOUR_NAMES: [&str; crate::audio::MAX_PATHS] =
    ["Blue", "Violet", "Green", "Rose"];

impl RigWerkApp {
    pub(super) fn path_strip(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        slot: usize,
        layout: StripLayout,
    ) {
        let colours = palette(ui.ctx());
        let Some(path) = self.paths[slot].clone() else {
            return;
        };
        let colour = colours.path[path.colour as usize % colours.path.len()];
        let (strip, mut top, mut bottom) = strip_parts(ui, rect, layout, Sense::click());
        let cap = Rect::from_min_size(rect.min, Vec2::new(rect.width(), 3.0));
        ui.painter().rect_filled(
            cap,
            CornerRadius {
                nw: 4,
                ne: 4,
                sw: 0,
                se: 0,
            },
            colour,
        );

        self.path_name(&mut top, slot, &path.name);
        if layout.sheet_open {
            let [left, right] = self.meter_paths[slot];
            horizontal_meter(&mut top, left.max(right), &format!("{} meter", path.name));
        }

        let source = self.source_label(slot);
        let button = strip_button(&mut top, &source, "Source")
            .on_hover_text("Which input channels feed this path, and stereo or mono");
        popover(&mut top, &button, |ui| self.source_popover(ui, slot));

        let chain_height = top.available_height() - 24.0;
        self.chain_column(&mut top, slot, chain_height, layout.step);

        let latency = self
            .path_infos()
            .into_iter()
            .find(|info| info.slot == slot)
            .map_or(0.0, |info| info.latency_ms);
        let load = self
            .engine
            .path_dsp_time(slot)
            .map(|time| time.avg_share() * 100.0);
        let marker = self.dsp_marker_path() == Some(slot);
        top.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.label(
                RichText::new(format_ms(latency))
                    .font(font(Face::Mono, TypeRole::Secondary))
                    .color(colours.text2),
            )
            .on_hover_text("The chain's latency");
            let load_text = match load {
                Some(share) => format!("DSP {share:.0} %"),
                None => "DSP –".to_owned(),
            };
            let response = ui.label(
                RichText::new(if marker {
                    format!("▲ {load_text}")
                } else {
                    load_text
                })
                .font(font(Face::Mono, TypeRole::Secondary))
                .color(if marker { colours.warn } else { colours.text3 }),
            );
            if marker {
                response.on_hover_text(
                    "The paths need more of the buffer than it has; this path takes the most",
                );
            } else {
                response.on_hover_text("This path's share of the buffer time");
            }
        });

        let any_solo = self.paths.iter().flatten().any(|other| other.mix.solo);
        let events = level_block(
            &mut bottom,
            layout,
            &LevelBlock {
                id: egui::Id::unique(("console_path_meter", slot)),
                levels: &self.meter_paths[slot],
                level_db: path.mix.level_db,
                range: FaderRange {
                    min_db: PATH_LEVEL_MIN_DB,
                    max_db: PATH_LEVEL_MAX_DB,
                },
                name: &path.name,
                show_scale: true,
                mute: Some(path.mix.mute),
                solo: Some(path.mix.solo),
                dim: path.mix.mute || (any_solo && !path.mix.solo),
            },
        );
        if let Some(level) = events.level {
            self.set_path_level(slot, level);
        }
        if events.mute {
            self.set_path_mute(slot, !path.mix.mute);
        }
        if events.solo {
            self.set_path_solo(slot, !path.mix.solo);
        }
        row_info(
            &strip,
            egui::Role::Button,
            true,
            format!("{} path", path.name),
            None,
            None,
        );
        if strip.clicked() {
            self.open_sheet(Sheet::Path { path: slot });
        }
    }

    /// The name, the issue marker after it, and the name's menu.
    fn path_name(&mut self, ui: &mut egui::Ui, slot: usize, name: &str) {
        let colours = palette(ui.ctx());
        let issue = self.path_has_issue(slot);
        let response = ui
            .horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                let name_font = font(Face::SansMedium, TypeRole::Control);
                let room = ui.available_width() - if issue { 20.0 } else { 0.0 };
                let shown = fit_text(ui, name, &name_font, room.max(8.0));
                let response = ui
                    .add(
                        egui::Button::new(RichText::new(shown).font(name_font).color(colours.text))
                            .frame(false),
                    )
                    .on_hover_text(format!("{name}: rename, colour, duplicate, remove"));
                row_info(&response, egui::Role::Button, true, name, None, None);
                if issue {
                    ui.label(
                        RichText::new("▲")
                            .font(font(Face::Sans, TypeRole::Secondary))
                            .color(colours.warn),
                    )
                    .on_hover_text("Plays nowhere: no output plays this path");
                }
                response
            })
            .inner;
        popover(ui, &response, |ui| self.path_menu(ui, slot, name));
    }

    /// The name's menu: *Rename*, *Colour ▸*, *Duplicate (muted)*, *Apply
    /// layout ▸*, *Remove* with a two-click confirm.
    fn path_menu(&mut self, ui: &mut egui::Ui, slot: usize, name: &str) {
        let colours = palette(ui.ctx());
        ui.set_min_width(200.0);
        let draft_id = egui::Id::unique(("console_rename", slot));
        let mut draft: String = ui
            .data(|data| data.get_temp(draft_id))
            .unwrap_or_else(|| name.to_owned());
        let field = ui.add(
            egui::TextEdit::singleline(&mut draft)
                .desired_width(180.0)
                .hint_text("Path name"),
        );
        row_info(
            &field,
            egui::Role::TextInput,
            true,
            "Rename",
            None,
            Some(draft.clone()),
        );
        let commit = field.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
        if commit || menu_entry(ui, "Rename") {
            if self.rename_path(slot, &draft) {
                ui.data_mut(|data| data.remove::<String>(draft_id));
                ui.close();
                return;
            }
            self.push_log(
                format!("Another path is already called {}.", draft.trim()),
                LogTone::Amber,
            );
        }
        ui.data_mut(|data| data.insert_temp(draft_id, draft));
        ui.menu_button("Colour ▸", |ui| {
            for (index, colour_name) in PATH_COLOUR_NAMES.iter().enumerate() {
                let swatch = colours.path[index];
                let response = ui
                    .horizontal(|ui| {
                        let (dot, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
                        ui.painter().circle_filled(dot.center(), 5.0, swatch);
                        menu_entry(ui, colour_name)
                    })
                    .inner;
                if response {
                    self.set_path_colour(slot, index as u8);
                    ui.close();
                }
            }
        });
        let can_add = self.add_path_blocker().is_none();
        if ui
            .add_enabled_ui(can_add, |ui| menu_entry(ui, "Duplicate (muted)"))
            .inner
        {
            let _ = self.duplicate_path(slot);
            ui.close();
        }
        self.layout_menu(ui);
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
        } else {
            egui::Button::new(
                RichText::new(label)
                    .font(font(Face::Sans, TypeRole::Body))
                    .color(colours.error_text),
            )
            .frame(false)
        };
        let response = ui.add_enabled(can_remove, button);
        let response = if can_remove {
            response.on_hover_text("Removes the path and its effects")
        } else {
            response.on_disabled_hover_text("The last path cannot be removed")
        };
        row_info(&response, egui::Role::Button, can_remove, label, None, None);
        if response.clicked() && self.request_remove_path(slot) {
            ui.close();
        }
    }

    /// The source popover: the 2 × 2 grid (*In 1*, *In 2* → L, R) and
    /// *Stereo* / *Mono*.
    fn source_popover(&mut self, ui: &mut egui::Ui, slot: usize) {
        let colours = palette(ui.ctx());
        let Some(mix) = self.paths[slot].as_ref().map(|path| path.mix) else {
            return;
        };
        egui::Grid::new(("console_source", slot))
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
                        let mut on = mix.routes[input][side];
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
                            self.set_path_source(slot, input, side, on);
                        }
                    }
                    ui.end_row();
                }
            });
        ui.add_space(6.0);
        let mut mode = usize::from(mix.mono);
        if segmented(ui, &mut mode, &["Stereo", "Mono"]).changed() {
            self.set_path_mono(slot, mode == 1);
        }
    }
}
