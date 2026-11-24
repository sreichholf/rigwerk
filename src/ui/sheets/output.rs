//! The output sheet's body (`docs/ui-spec.md` §6.2): the device's name and
//! badge, its meters, level and mute, the channel map with its presets, the
//! metronome's channels, the paths that play here (level, polarity, trim
//! and how late they arrive) with the align switch, and the extra output's
//! own buffer or the virtual output's switch.
//!
//! Map edits go through [`MapEdit`] and [`RigWerkApp::apply_map_edit`], the
//! same path Signal Routing uses.  The click channels and the align switch
//! are device settings, not path maps.  The clock output's rate and buffer
//! stay in Setup; the sheet changes routing and levels, never the device.

use eframe::egui;
use egui::{Align, CornerRadius, Layout, RichText, Sense, Stroke, Vec2};

use crate::app::output_devices::MapEdit;
use crate::app::output_devices::{OutputBadge, OutputDeviceRow};
use crate::app::screen::format_ms;
use crate::app::settings::{
    bridge_buffer_from_pref, bridge_buffer_to_pref, BRIDGE_BUFFER_MS_MAX, BRIDGE_BUFFER_MS_MIN,
};
use crate::app::{LogTone, RigWerkApp};
use crate::audio::{
    BufferMode, MapPreset, MixSide, OutputDeviceSettings, UnusedChannels, MAX_OUTPUT_CHANNELS,
    PATH_TRIM_MAX_MS,
};
use crate::ui::fonts::Face;
use crate::ui::theme::{font, palette, Palette, TypeRole};
use crate::ui::widgets::{chip, horizontal_meter, row_info, segmented};

use crate::ui::widgets::OutlineButton;

const ROW_H: f32 = 28.0;
const VALUE_W: f32 = 80.0;
const CELL: f32 = 22.0;
const CHANNEL_W: f32 = 34.0;
/// Below this the two columns stack.
const MIN_TWO_COLUMNS_W: f32 = 760.0;
/// Gain reduction the bar spans.
const LIMITER_RANGE_DB: f32 = 12.0;

impl RigWerkApp {
    /// The output sheet body for output `device` (0 is the clock output).
    pub(crate) fn output_sheet(&mut self, ui: &mut egui::Ui, device: usize) {
        let rows = self.output_device_rows();
        let Some(row) = rows.get(device).cloned() else {
            return;
        };
        let Some(settings) = self.device_settings().get(device).copied().copied() else {
            return;
        };
        let colours = palette(ui.ctx());
        let badge = self.output_badge(device);
        let buffer = self.output_buffer_line(device);
        let levels: Vec<f32> = match row.extra_output_index {
            None => self
                .meter_out_channels
                .iter()
                .take(row.channels)
                .copied()
                .collect(),
            Some(index) => self
                .meter_extra_output_channels
                .get(index)
                .map(|levels| levels.iter().take(row.channels).copied().collect())
                .unwrap_or_default(),
        };
        let mut level = settings.level_db;
        let mut mute = settings.mute;
        let mut remove = false;
        let mut edits: Vec<MapEdit> = Vec::new();
        let mut enable_virtual = None;

        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            badge_word(ui, badge, colours);
            ui.label(
                RichText::new(format!("{} ch · {}", row.channels, row.backend))
                    .font(font(Face::Sans, TypeRole::Secondary))
                    .color(colours.text3),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if row.extra_output_index.is_some()
                    && OutlineButton::new("Remove")
                        .show(ui)
                        .on_hover_text(
                            "Removes the output from the session. The others keep playing.",
                        )
                        .clicked()
                {
                    remove = true;
                }
                if row.clock {
                    self.asio_panel_chip(ui, crate::audio::AsioPanelSide::Output);
                }
            });
        });
        ui.label(
            RichText::new(buffer)
                .font(font(Face::Mono, TypeRole::Secondary))
                .color(colours.text2),
        );
        if row.missing {
            ui.label(
                RichText::new(
                    "Device not found. The bridge runs without it; it stays in the session and is retried.",
                )
                .font(font(Face::SansMedium, TypeRole::Body))
                .color(colours.warn),
            );
        }
        ui.add_space(8.0);

        let width = ui.available_width();
        if width >= MIN_TWO_COLUMNS_W {
            let column = (width - 24.0) / 2.0;
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 24.0;
                ui.allocate_ui_with_layout(
                    Vec2::new(column, 0.0),
                    Layout::top_down(Align::LEFT),
                    |ui| {
                        ui.set_width(column);
                        meters(ui, &levels, row.channels, &row.name, colours);
                        ui.add_space(8.0);
                        level_row(ui, &mut level, colours);
                        ui.add_space(4.0);
                        if chip(ui, &mut mute, "Mute")
                            .on_hover_text(
                                "Silence this output only; the master and the bridge keep running",
                            )
                            .changed()
                        {
                            self.set_output_mute(device, mute);
                        }
                        ui.add_space(8.0);
                        limiter(ui, row.reduction_db, colours);
                        ui.add_space(12.0);
                        self.loudness_section(ui, device, &row);
                        ui.add_space(12.0);
                        self.clock_and_paths(ui, device, &row, &settings, &mut edits);
                    },
                );
                ui.allocate_ui_with_layout(
                    Vec2::new(column, 0.0),
                    Layout::top_down(Align::LEFT),
                    |ui| {
                        ui.set_width(column);
                        self.channel_map(ui, device, &row, &settings, &mut edits);
                        ui.add_space(12.0);
                        self.output_kind(ui, &row, &mut enable_virtual);
                    },
                );
            });
        } else {
            meters(ui, &levels, row.channels, &row.name, colours);
            ui.add_space(8.0);
            level_row(ui, &mut level, colours);
            ui.add_space(4.0);
            if chip(ui, &mut mute, "Mute")
                .on_hover_text("Silence this output only; the master and the bridge keep running")
                .changed()
            {
                self.set_output_mute(device, mute);
            }
            ui.add_space(8.0);
            limiter(ui, row.reduction_db, colours);
            ui.add_space(12.0);
            self.loudness_section(ui, device, &row);
            ui.add_space(12.0);
            self.channel_map(ui, device, &row, &settings, &mut edits);
            ui.add_space(12.0);
            self.clock_and_paths(ui, device, &row, &settings, &mut edits);
            ui.add_space(12.0);
            self.output_kind(ui, &row, &mut enable_virtual);
        }

        for edit in edits {
            self.apply_map_edit(device, edit);
        }
        if (level - settings.level_db).abs() > 0.05 {
            self.set_output_level(device, level);
        }
        if let Some(on) = enable_virtual {
            if let Some(index) = row.extra_output_index {
                if let Some(pref) = self.extra_outputs.get_mut(index) {
                    pref.virtual_output_on = on;
                }
            }
        }
        if remove {
            if let Some(index) = row.extra_output_index {
                self.remove_extra_output(index);
                self.push_log(format!("{} removed.", row.name), LogTone::Amber);
                self.close_sheet();
            }
        }
    }

    /// The measured loudness and the output trim.
    fn loudness_section(&mut self, ui: &mut egui::Ui, device: usize, row: &OutputDeviceRow) {
        let colours = palette(ui.ctx());
        heading(ui, "Loudness", colours);
        let reading = row.loudness;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            for (name, value) in [
                ("Short-term", reading.short_term_lufs),
                ("Momentary", reading.momentary_lufs),
                ("Integrated", reading.integrated_lufs),
            ] {
                ui.label(
                    RichText::new(name)
                        .font(font(Face::Mono, TypeRole::GridHeader))
                        .color(colours.text3),
                );
                ui.label(
                    RichText::new(if value > crate::audio::SILENCE_LUFS {
                        format!("{value:.1}")
                    } else {
                        "—".to_owned()
                    })
                    .font(font(Face::Mono, TypeRole::Secondary))
                    .color(colours.text),
                );
            }
            ui.label(
                RichText::new("LUFS")
                    .font(font(Face::Mono, TypeRole::GridHeader))
                    .color(colours.text3),
            );
        });
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.spacing_mut().slider_width = (ui.available_width() - VALUE_W - 8.0).max(80.0);
            ui.visuals_mut().selection.bg_fill = colours.accent;
            let mut target = row.settings.loudness.target_lufs;
            let response = ui
                .add(
                    egui::Slider::new(
                        &mut target,
                        crate::audio::LOUDNESS_TARGET_MIN_LUFS
                            ..=crate::audio::LOUDNESS_TARGET_MAX_LUFS,
                    )
                    .show_value(false)
                    .step_by(0.5)
                    .handle_shape(egui::style::HandleShape::Circle)
                    .trailing_fill(true),
                )
                .on_hover_text(
                    "The output Learn target. Learn sets the trim so this output reaches it with the level fader at 0 dB.",
                );
            row_info(
                &response,
                egui::Role::Slider,
                true,
                "Loudness target",
                None,
                Some(format!("{target:.1} LUFS")),
            );
            if response.changed() {
                self.set_output_loudness_target(device, target);
            }
            ui.allocate_ui_with_layout(
                Vec2::new(VALUE_W, ROW_H),
                Layout::right_to_left(Align::Center),
                |ui| {
                    ui.set_width(VALUE_W);
                    ui.label(
                        RichText::new(format!("{target:.1} LUFS"))
                            .font(font(Face::Mono, TypeRole::Secondary))
                            .color(colours.text),
                    );
                },
            );
        });
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.label(
                RichText::new("Output trim")
                    .font(font(Face::Sans, TypeRole::Secondary))
                    .color(colours.text2),
            );
            let mut trim = row.settings.loudness.trim_db;
            let response = ui.add(
                egui::DragValue::new(&mut trim)
                    .speed(0.1)
                    .range(-crate::audio::LOUDNESS_TRIM_MAX_DB..=crate::audio::LOUDNESS_TRIM_MAX_DB)
                    .suffix(" dB"),
            );
            row_info(
                &response,
                egui::Role::Slider,
                true,
                "Output trim",
                None,
                Some(format!("{trim:+.1} dB")),
            );
            let mut commit = (trim - row.settings.loudness.trim_db).abs() > 0.001;
            if response.double_clicked() {
                trim = 0.0;
                commit = true;
            }
            if commit {
                self.set_output_trim(device, trim);
            }
            if self.is_learning_loudness(device) {
                ui.label(
                    RichText::new("Learning…")
                        .font(font(Face::Sans, TypeRole::Secondary))
                        .color(colours.warn),
                );
            } else if OutlineButton::new("Learn")
                .show(ui)
                .on_hover_text(
                    "Measure this output for eight seconds and save a trim so it reaches the target with the level fader at 0 dB. Nothing moves on its own.",
                )
                .clicked()
            {
                self.start_output_learn(device);
            }
            if row.settings.loudness.trim_db.abs() >= 0.05
                && OutlineButton::new("Clear")
                    .show(ui)
                    .on_hover_text("Ramp this output's trim back to unity")
                    .clicked()
            {
                self.clear_output_learn(device);
            }
            if OutlineButton::new("Reset")
                .show(ui)
                .on_hover_text("Clear the loudness readings only; the trim stays")
                .clicked()
            {
                self.reset_output_loudness(device);
            }
        });
        let trim = row.settings.loudness.trim_db;
        let note = if trim.abs() < 0.05 {
            "No trim".to_owned()
        } else {
            match self.output_trim_origin(device) {
                crate::app::output_devices::OutputTrimOrigin::Learned { lufs } => {
                    format!("Learned at {lufs:.1} LUFS")
                }
                crate::app::output_devices::OutputTrimOrigin::None
                | crate::app::output_devices::OutputTrimOrigin::Hand => "Set by hand".to_owned(),
            }
        };
        ui.label(
            RichText::new(note)
                .font(font(Face::Sans, TypeRole::GridHeader))
                .color(colours.text3),
        );
    }

    /// The paths that play here, each with its level, polarity and trim on
    /// this output and when it arrives, then the align switch.
    fn clock_and_paths(
        &mut self,
        ui: &mut egui::Ui,
        device: usize,
        row: &OutputDeviceRow,
        settings: &OutputDeviceSettings,
        edits: &mut Vec<MapEdit>,
    ) {
        let colours = palette(ui.ctx());
        let infos = self.path_infos();
        let reaching: Vec<usize> = infos
            .iter()
            .filter(|info| self.path_reaches_device(info.slot, device))
            .map(|info| info.slot)
            .collect();
        let alignment = self.device_alignment(device);
        heading(ui, "Paths here", colours);
        if reaching.is_empty() {
            ui.label(
                RichText::new("Nothing plays here. Pick paths in the map above.")
                    .font(font(Face::SansMedium, TypeRole::Body))
                    .color(colours.warn),
            );
            return;
        }
        for slot in &reaching {
            let Some(info) = infos.iter().find(|info| info.slot == *slot) else {
                continue;
            };
            let entry = settings.path_map[*slot];
            let arriving = alignment
                .as_ref()
                .and_then(|alignment| alignment.arrival_ms[*slot]);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let (dot, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                ui.painter().circle_filled(
                    dot.center(),
                    4.0,
                    colours.path[info.colour_index as usize % colours.path.len()],
                );
                ui.add(
                    egui::Label::new(
                        RichText::new(&info.name)
                            .font(font(Face::Sans, TypeRole::Body))
                            .color(colours.text2),
                    )
                    .truncate(),
                )
                .on_hover_text(&info.name);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(match arriving {
                            Some(ms) => format!("arrives {}", format_ms(ms)),
                            None => "not aligned".to_owned(),
                        })
                        .font(font(Face::Mono, TypeRole::GridHeader))
                        .color(match arriving {
                            Some(_) => colours.text3,
                            None => colours.warn,
                        }),
                    )
                    .on_hover_text("The path's chain latency plus its wait on this output");
                });
            });
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let mut level_db = entry.level_db;
                let response = ui.add(
                    egui::DragValue::new(&mut level_db)
                        .speed(0.1)
                        .range(crate::audio::PATH_LEVEL_MIN_DB..=crate::audio::PATH_LEVEL_MAX_DB)
                        .suffix(" dB"),
                );
                row_info(
                    &response,
                    egui::Role::Slider,
                    true,
                    format!("{} level on {}", info.name, row.name),
                    None,
                    Some(format!("{level_db:+.1} dB")),
                );
                if (level_db - entry.level_db).abs() > 0.001 {
                    edits.push(MapEdit::Level {
                        slot: *slot,
                        level_db,
                        commit: true,
                    });
                }
                let mut invert = entry.invert;
                if chip(ui, &mut invert, "Polarity")
                    .on_hover_text("Flip this path's polarity on this output only")
                    .changed()
                {
                    edits.push(MapEdit::Invert {
                        slot: *slot,
                        invert,
                    });
                }
                let mut trim_ms = entry.trim_ms;
                let response = ui.add(
                    egui::DragValue::new(&mut trim_ms)
                        .speed(0.05)
                        .range(-PATH_TRIM_MAX_MS..=PATH_TRIM_MAX_MS)
                        .suffix(" ms"),
                );
                row_info(
                    &response,
                    egui::Role::Slider,
                    true,
                    format!("{} trim on {}", info.name, row.name),
                    None,
                    Some(format!("{trim_ms:+.1} ms")),
                );
                if (trim_ms - entry.trim_ms).abs() > 0.0005 {
                    edits.push(MapEdit::Trim {
                        slot: *slot,
                        trim_ms,
                        commit: true,
                    });
                }
            });
        }
        ui.add_space(8.0);
        let align = settings.align;
        let mut choice = usize::from(!align);
        let response = segmented(ui, &mut choice, &["Align on", "Align off"]);
        if response.changed() {
            edits.push(MapEdit::Align(choice == 0));
        }
        let summary = match (alignment.as_ref(), align) {
            (Some(alignment), true) => match alignment.waits_for {
                Some(slot) => infos
                    .iter()
                    .find(|info| info.slot == slot)
                    .map(|info| format!("Every path here waits for {}", info.name)),
                None => Some("Every path here already arrives together".to_owned()),
            },
            (Some(alignment), false) if !reaching.is_empty() => Some(format!(
                "Off: each path plays when its chain finishes, {:.1} ms apart",
                alignment.added_ms
            )),
            _ => None,
        };
        if let Some(summary) = summary {
            ui.label(
                RichText::new(summary)
                    .font(font(Face::Sans, TypeRole::Secondary))
                    .color(colours.text3),
            );
        }
    }

    /// The channel matrix, its presets, the click row and what unreached
    /// channels play.
    fn channel_map(
        &mut self,
        ui: &mut egui::Ui,
        device: usize,
        row: &OutputDeviceRow,
        settings: &OutputDeviceSettings,
        edits: &mut Vec<MapEdit>,
    ) {
        let colours = palette(ui.ctx());
        let channels = row.channels.clamp(1, MAX_OUTPUT_CHANNELS);
        let slots: Vec<(usize, String, u8)> = self
            .path_slots()
            .into_iter()
            .filter_map(|slot| {
                let path = self.paths.get(slot).and_then(Option::as_ref)?;
                Some((slot, path.name.clone(), path.colour))
            })
            .collect();
        heading(ui, "Channel map", colours);
        let mut click_edit: Option<(usize, bool)> = None;
        egui::Grid::new(("output_sheet_map", device))
            .spacing(Vec2::new(4.0, 4.0))
            .show(ui, |ui| {
                ui.label("");
                for channel in 0..channels {
                    ui.allocate_ui_with_layout(
                        Vec2::new(CHANNEL_W, CELL),
                        Layout::left_to_right(Align::Center),
                        |ui| {
                            ui.label(
                                RichText::new(format!("{}", channel + 1))
                                    .font(font(Face::Mono, TypeRole::GridHeader))
                                    .color(colours.text3),
                            );
                        },
                    );
                }
                ui.end_row();
                for (slot, name, _) in &slots {
                    let entry = settings.path_map[*slot];
                    for (side, letter) in [(MixSide::L, "L"), (MixSide::R, "R")] {
                        ui.label(
                            RichText::new(format!("{name} {letter}"))
                                .font(font(Face::Sans, TypeRole::GridHeader))
                                .color(colours.text2),
                        );
                        for channel in 0..channels {
                            let on = entry.map.routes(side, channel);
                            let response = map_cell(ui, on, colours);
                            let cell_name = format!("{name} {letter} to out {}", channel + 1);
                            row_info(
                                &response,
                                egui::Role::CheckBox,
                                true,
                                cell_name.clone(),
                                Some(on),
                                None,
                            );
                            if response.on_hover_text(cell_name).clicked() {
                                edits.push(MapEdit::Cell {
                                    slot: *slot,
                                    side,
                                    channel,
                                    on: !on,
                                });
                            }
                        }
                        ui.end_row();
                    }
                }
                ui.label(
                    RichText::new("Click")
                        .font(font(Face::Sans, TypeRole::GridHeader))
                        .color(colours.warn),
                );
                for channel in 0..channels {
                    let on = settings.click_mask & (1 << channel) != 0;
                    let response = map_cell(ui, on, colours);
                    let cell_name = format!("Metronome on out {}", channel + 1);
                    row_info(
                        &response,
                        egui::Role::CheckBox,
                        true,
                        cell_name.clone(),
                        Some(on),
                        None,
                    );
                    if response.on_hover_text(cell_name).clicked() {
                        click_edit = Some((channel, !on));
                    }
                }
                ui.end_row();
            });
        ui.label(
            RichText::new("One column per output channel; L and R are each path's own sides.")
                .font(font(Face::Sans, TypeRole::GridHeader))
                .color(colours.text3),
        );

        ui.add_space(8.0);
        heading(ui, "Map presets", colours);
        let mut preset_path = self
            .routing_preset_path
            .filter(|slot| slots.iter().any(|(have, _, _)| have == slot))
            .or_else(|| slots.first().map(|(slot, _, _)| *slot))
            .unwrap_or(0);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            for (slot, name, _) in &slots {
                let mut on = *slot == preset_path;
                if chip(ui, &mut on, name)
                    .on_hover_text("The path these presets set")
                    .changed()
                {
                    preset_path = *slot;
                }
            }
        });
        self.routing_preset_path = Some(preset_path);
        let mut preset: Option<MapPreset> = None;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            for option in MapPreset::available(channels) {
                if chip(ui, &mut false, option.label(channels)).clicked() {
                    preset = Some(*option);
                }
            }
        });
        if let Some(preset) = preset {
            let entry = settings.path_map[preset_path];
            edits.push(MapEdit::Map {
                slot: preset_path,
                map: entry.map.with_preset(preset, channels),
            });
        }
        if channels >= 4
            && OutlineButton::new("Metronome on 3/4")
                .show(ui)
                .on_hover_text("Music on 1 and 2, the click alone on 3 and 4")
                .clicked()
        {
            self.apply_click_pair(device);
        }

        ui.add_space(8.0);
        heading(ui, "Channels nothing plays", colours);
        let mut unused = settings.unused == UnusedChannels::MonoCopy;
        if chip(ui, &mut unused, "Mono copy")
            .on_hover_text("A mono device plays (L + R) / 2 of whatever is mapped")
            .changed()
        {
            edits.push(MapEdit::Unused(if unused {
                UnusedChannels::MonoCopy
            } else {
                UnusedChannels::Silent
            }));
        }
        if let Some((channel, on)) = click_edit {
            self.set_click_channel(device, channel, on);
        }
    }

    /// The clock's Setup line, the extra output's own buffer, or the
    /// virtual output's switch.
    fn output_kind(
        &mut self,
        ui: &mut egui::Ui,
        row: &OutputDeviceRow,
        enable_virtual: &mut Option<bool>,
    ) {
        let colours = palette(ui.ctx());
        heading(ui, "Output", colours);
        if row.clock {
            ui.label(
                RichText::new(
                    "The clock output's rate, buffer and mode are in Setup › Clock output.",
                )
                .font(font(Face::Sans, TypeRole::Secondary))
                .color(colours.text3),
            );
            return;
        }
        let Some(index) = row.extra_output_index else {
            return;
        };
        if row.virtual_output {
            let on = self
                .extra_outputs
                .get(index)
                .is_some_and(|pref| pref.virtual_output_on);
            let mut choice = usize::from(!on);
            if segmented(ui, &mut choice, &["On", "Off"]).changed() {
                *enable_virtual = Some(choice == 0);
            }
            ui.label(
                RichText::new(if self.pipewire_extra_outputs_enabled() {
                    "RigWerk Out: other apps can record the master while the bridge runs."
                } else {
                    "Plays into VB-Cable; other apps record CABLE Output."
                })
                .font(font(Face::Sans, TypeRole::Secondary))
                .color(colours.text3),
            );
            return;
        }
        if self.pipewire_extra_outputs_enabled() {
            ui.label(
                RichText::new("In the PipeWire graph: no bridge buffer needed.")
                    .font(font(Face::Sans, TypeRole::Secondary))
                    .color(colours.text3),
            );
            return;
        }
        let Some(pref) = self.extra_outputs.get_mut(index) else {
            return;
        };
        let mut mode = bridge_buffer_from_pref(pref.bridge_buffer_mode);
        let mut choice = match mode {
            BufferMode::Auto => 0,
            BufferMode::Minimum => 1,
            BufferMode::Fixed => 2,
        };
        if segmented(ui, &mut choice, &["Auto", "Minimum", "Fixed"]).changed() {
            mode = match choice {
                0 => BufferMode::Auto,
                1 => BufferMode::Minimum,
                _ => BufferMode::Fixed,
            };
            pref.bridge_buffer_mode = bridge_buffer_to_pref(mode);
        }
        if mode == BufferMode::Fixed {
            let mut ms = pref.bridge_buffer_ms;
            let response = ui.add(
                egui::Slider::new(&mut ms, BRIDGE_BUFFER_MS_MIN..=BRIDGE_BUFFER_MS_MAX)
                    .suffix(" ms")
                    .show_value(true),
            );
            row_info(
                &response,
                egui::Role::Slider,
                true,
                "Bridge buffer",
                None,
                Some(format!("{ms:.1} ms")),
            );
            if response.changed() {
                pref.bridge_buffer_ms = ms;
            }
        }
        ui.label(
            RichText::new(
                "Own ring between the mix and this output; clock drift is absorbed here.",
            )
            .font(font(Face::Sans, TypeRole::Secondary))
            .color(colours.text3),
        );
    }
}

fn badge_word(ui: &mut egui::Ui, badge: OutputBadge, colours: &Palette) {
    let ink = if badge.warns() {
        colours.warn
    } else {
        colours.text2
    };
    let (rect, response) = ui.allocate_exact_size(Vec2::new(64.0, 20.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(3), colours.slot);
    if badge.warns() {
        ui.painter().rect_stroke(
            rect,
            CornerRadius::same(3),
            Stroke::new(1.0, colours.warn),
            egui::StrokeKind::Inside,
        );
    }
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        badge.label(),
        font(Face::SansMedium, TypeRole::GridHeader),
        ink,
    );
    row_info(
        &response,
        egui::Role::Label,
        true,
        badge.label(),
        None,
        None,
    );
}

fn heading(ui: &mut egui::Ui, text: &str, colours: &Palette) {
    ui.label(
        RichText::new(text)
            .font(font(Face::SansMedium, TypeRole::Secondary))
            .color(colours.text3),
    );
    ui.add_space(3.0);
}

/// One meter row per device channel.
fn meters(ui: &mut egui::Ui, levels: &[f32], channels: usize, device: &str, colours: &Palette) {
    heading(ui, "Meters", colours);
    for channel in 0..channels.min(MAX_OUTPUT_CHANNELS) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.label(
                RichText::new(format!("Out {}", channel + 1))
                    .font(font(Face::Mono, TypeRole::GridHeader))
                    .color(colours.text3),
            );
            let level = levels.get(channel).copied().unwrap_or(0.0);
            horizontal_meter(ui, level, &format!("{device} channel {}", channel + 1));
        });
    }
}

fn level_row(ui: &mut egui::Ui, level: &mut f32, colours: &Palette) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.spacing_mut().slider_width = (ui.available_width() - VALUE_W - 8.0).max(80.0);
        ui.visuals_mut().selection.bg_fill = colours.accent;
        let response = ui
            .add(
                egui::Slider::new(
                    level,
                    crate::audio::DEVICE_LEVEL_MIN_DB..=crate::audio::DEVICE_LEVEL_MAX_DB,
                )
                .show_value(false)
                .step_by(0.1)
                .handle_shape(egui::style::HandleShape::Circle)
                .trailing_fill(true),
            )
            .on_hover_text("This output's level, after the master");
        row_info(
            &response,
            egui::Role::Slider,
            true,
            "Level",
            None,
            Some(format!("{level:+.1} dB")),
        );
        ui.allocate_ui_with_layout(
            Vec2::new(VALUE_W, ROW_H),
            Layout::right_to_left(Align::Center),
            |ui| {
                ui.set_width(VALUE_W);
                ui.label(
                    RichText::new(format!("{level:+.1} dB"))
                        .font(font(Face::Mono, TypeRole::Secondary))
                        .color(colours.text),
                );
            },
        );
    });
}

fn limiter(ui: &mut egui::Ui, reduction_db: f32, colours: &Palette) {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 4.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(1), colours.well);
    let reduction = reduction_db.abs().min(LIMITER_RANGE_DB);
    if reduction > 0.05 {
        let filled = egui::Rect::from_min_size(
            rect.min,
            Vec2::new(rect.width() * reduction / LIMITER_RANGE_DB, rect.height()),
        );
        ui.painter()
            .rect_filled(filled, CornerRadius::same(1), colours.meter_hot);
    }
    row_info(
        &response,
        egui::Role::Meter,
        true,
        "Limiter reduction",
        None,
        Some(format!("{reduction:.1} dB")),
    );
    let _ = response.on_hover_text(format!("Limiter reduction: {reduction:.1} dB"));
}

fn map_cell(ui: &mut egui::Ui, on: bool, colours: &Palette) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(CELL), Sense::click());
    ui.painter().rect_filled(
        rect,
        CornerRadius::same(3),
        if on { colours.accent_bg } else { colours.slot },
    );
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(3),
        Stroke::new(1.0, if on { colours.accent } else { colours.line2 }),
        egui::StrokeKind::Inside,
    );
    if on {
        ui.painter()
            .circle_filled(rect.center(), 3.5, colours.accent);
    }
    response
}

#[cfg(test)]
mod tests;
