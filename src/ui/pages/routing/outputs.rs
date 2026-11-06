//! Layer ② of Signal Routing: which device channels play the finished mix.
//!
//! The device list is the Mixer's (`output_device_rows`), in the same order.
//! The page keeps its own selected device (`routing_device`); "Open in
//! Mixer" selects that device there. Followers and RigWerk Out appear when
//! they are in the session.

use eframe::egui;
use egui::{
    Align, Align2, Color32, CornerRadius, FontFamily, FontId, Layout, Margin, RichText, Sense,
    Stroke, Vec2,
};

use crate::app::{Page, RigWerkApp};
use crate::audio::{
    ChannelMap, EngineState, MapPreset, MixSide, OutputDeviceSettings, UnusedChannels,
    MAX_OUTPUT_CHANNELS,
};
use crate::ui::pages::mixer::chain::ChainSelection;
use crate::ui::pages::mixer::output_devices::{block, device_state, OutputDeviceRow};
use crate::ui::theme::ts;
use crate::ui::theme::{
    with_alpha, AMBER, BLUE, BORDER, BORDER_SOFT, CARD, DIM, GREEN, MUTED, TEAL, TEXT,
};
use crate::ui::widgets::{
    card, clock_tag, fit_text, horizontal_line, micro_label, mono, paint_tag, row_info,
    show_focus_ring, tab_button_sized, tag_width,
};

use super::{column_header, layer_badge, matrix_cell, row_header, CELL, ROW_HEADER_W};

/// Below this card width the device list sits above the selected device's
/// matrix instead of beside it.
pub(crate) const OUTPUTS_STACK_BELOW: f32 = 760.0;

/// Under the device list.
pub(crate) const SAME_DEVICES_NOTE: &str =
    "Same devices, same order as Mixer -> Output. Add, remove and set levels there.";

/// Under the clock device's matrix.
pub(crate) const CLOCK_DEVICE_NOTE: &str =
    "Extra pairs on the clock device cost nothing: same callback, no extra clock or cushion.";

/// Under a 1-channel device's matrix: the mapping rule for mono devices.
pub(crate) const MONO_DEVICE_NOTE: &str =
    "A mono device plays (L + R) / 2 whenever any cell is set. With nothing set it follows Unused channels.";

/// What a device plays, in one line, and whether nothing is routed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MapSummary {
    pub(crate) text: String,
    /// No mix side reaches any of the device's channels.
    pub(crate) nothing_routed: bool,
    /// Nothing routed and unused channels are silent: the device plays
    /// silence. The only case the page warns about.
    pub(crate) silent: bool,
}

/// One line for a device's map: pairs as "Out 1/2" ("L/R" on a stereo
/// device, "swapped" when reversed), single channels as "Out 3←L",
/// "Out 3←R" or "Out 3←mono", and "rest mono copy" when unreached channels
/// carry the average. Follows the mapping rule (`ChannelMap::sides_at`), so
/// a 1-channel device reads "Out 1←mono" for any non-empty map.
pub(crate) fn map_summary(settings: &OutputDeviceSettings, channels: usize) -> MapSummary {
    let channels = channels.min(MAX_OUTPUT_CHANNELS);
    let map = settings.map;
    let copy = settings.unused == UnusedChannels::MonoCopy;
    if !map.reaches_any(channels) {
        return MapSummary {
            text: if copy {
                "nothing routed · mono copy on all".to_owned()
            } else {
                "nothing routed · plays silence".to_owned()
            },
            nothing_routed: true,
            silent: !copy,
        };
    }
    let sides = |channel: usize| map.sides_at(channels, channel);
    let mut parts = Vec::new();
    let mut unreached = false;
    let mut channel = 0;
    while channel < channels {
        if channel % 2 == 0 && channel + 1 < channels {
            let pair = (sides(channel), sides(channel + 1));
            let name = if channels == 2 {
                "L/R".to_owned()
            } else {
                format!("Out {}/{}", channel + 1, channel + 2)
            };
            if pair == ((true, false), (false, true)) {
                parts.push(name);
                channel += 2;
                continue;
            }
            if pair == ((false, true), (true, false)) {
                parts.push(format!("{name} swapped"));
                channel += 2;
                continue;
            }
        }
        let out = channel + 1;
        match sides(channel) {
            (true, true) => parts.push(format!("Out {out}←mono")),
            (true, false) => parts.push(format!("Out {out}←L")),
            (false, true) => parts.push(format!("Out {out}←R")),
            (false, false) => unreached = true,
        }
        channel += 1;
    }
    if unreached && copy {
        parts.push("rest mono copy".to_owned());
    }
    MapSummary {
        text: parts.join(" · "),
        nothing_routed: false,
        silent: false,
    }
}

/// Whether to offer Silent / Mono copy: on any device with more than two
/// channels, and whenever one of its channels is left unreached.
pub(crate) fn shows_unused_choice(settings: &OutputDeviceSettings, channels: usize) -> bool {
    channels > 2
        || (0..channels.min(MAX_OUTPUT_CHANNELS))
            .any(|channel| settings.map.sides_at(channels, channel) == (false, false))
}

/// The device the page shows: `index` while it exists, else the clock
/// device.
pub(crate) fn selected_device(index: usize, count: usize) -> usize {
    if index < count {
        index
    } else {
        0
    }
}

/// Whether a preset button shows as active: the map already is what the
/// preset would make it. Swap never is. On a 1-channel device any routed
/// map plays the average, so Mono to all is active whenever anything is
/// routed.
pub(crate) fn preset_active(map: ChannelMap, preset: MapPreset, channels: usize) -> bool {
    match preset {
        MapPreset::Swap => false,
        MapPreset::MonoToAll if channels == 1 => map.reaches_any(1),
        _ => map.with_preset(preset, channels) == map,
    }
}

/// What the page asks the app to do after drawing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RoutingAction {
    /// Show this device's map on this page only.
    Select(usize),
    /// Go to the Mixer with this device selected.
    OpenInMixer(usize),
}

/// A preset button, named for screen readers as "<label> preset".
fn preset_button(ui: &mut egui::Ui, label: &str, active: bool) -> bool {
    let response = ui.add(
        egui::Button::new(RichText::new(label).size(ts(9.5)).color(if active {
            BLUE
        } else {
            MUTED
        }))
        .fill(if active {
            with_alpha(BLUE, 20)
        } else {
            Color32::TRANSPARENT
        })
        .stroke(Stroke::new(
            1.0,
            if active {
                with_alpha(BLUE, 60)
            } else {
                BORDER_SOFT
            },
        ))
        .corner_radius(CornerRadius::same(6)),
    );
    row_info(
        &response,
        egui::Role::Button,
        ui.is_enabled(),
        format!("{label} preset"),
        None,
        None,
    );
    response.clicked()
}

/// A layer ② cell: mix side `side` onto device channel `channel` (0-based).
pub(crate) fn output_cell(ui: &mut egui::Ui, on: bool, side: MixSide, channel: usize) -> bool {
    let letter = match side {
        MixSide::L => "L",
        MixSide::R => "R",
    };
    matrix_cell(
        ui,
        on,
        &format!("{letter}{}", channel + 1),
        format!("Mix {letter} to Out {}", channel + 1),
        ("routed", "off"),
    )
}

/// One device channel's meter under its column: a vertical bar while the
/// channel plays, with a "copy" caption when it carries the unused-channel
/// mono copy; "silent" when it plays nothing.
fn channel_meter(ui: &mut egui::Ui, reached: bool, unused: UnusedChannels, level: f32, peak: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(CELL.x, 38.0), Sense::hover());
    let caption = |text: &str| {
        ui.painter().text(
            egui::pos2(rect.center().x, rect.bottom() - 5.0),
            Align2::CENTER_CENTER,
            text,
            FontId::new(ts(7.5), FontFamily::Monospace),
            DIM,
        );
    };
    if !reached {
        match unused {
            UnusedChannels::Silent => {
                caption("silent");
                return;
            }
            UnusedChannels::MonoCopy => caption("copy"),
        }
    }
    let track = egui::Rect::from_min_size(
        egui::pos2(rect.center().x - 3.0, rect.top() + 2.0),
        Vec2::new(6.0, 24.0),
    );
    ui.painter()
        .rect_filled(track, CornerRadius::same(3), Color32::from_rgb(25, 32, 44));
    let height = track.height() * level.clamp(0.0, 1.0);
    if height > 1.0 {
        let filled =
            egui::Rect::from_min_max(egui::pos2(track.left(), track.bottom() - height), track.max);
        ui.painter()
            .rect_filled(filled, CornerRadius::same(3), with_alpha(BLUE, 215));
    }
    let peak_y = (track.bottom() - track.height() * peak.clamp(0.0, 1.0))
        .clamp(track.top(), track.bottom() - 1.0);
    if peak > 0.0 {
        ui.painter().line_segment(
            [
                egui::pos2(track.left() - 1.0, peak_y),
                egui::pos2(track.right() + 1.0, peak_y),
            ],
            Stroke::new(1.0, with_alpha(TEXT, 195)),
        );
    }
}

/// A device in the list or the patch strip: state dot, name, CLOCK tag and
/// the map summary under it. Named "<device> <context>" for screen readers,
/// with the summary as its value. Returns whether it was clicked.
pub(crate) fn device_summary_row(
    ui: &mut egui::Ui,
    row: &OutputDeviceRow,
    selected: bool,
    state: EngineState,
    context: &str,
) -> bool {
    let summary = map_summary(&row.settings, row.channels);
    let name_font = FontId::new(ts(11.0), FontFamily::Proportional);
    let detail_font = FontId::new(ts(8.5), FontFamily::Monospace);
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 44.0), Sense::click());
    ui.painter().rect_filled(
        rect,
        CornerRadius::same(8),
        if selected {
            with_alpha(TEAL, 18)
        } else if response.hovered() {
            with_alpha(TEXT, 8)
        } else {
            with_alpha(TEXT, 4)
        },
    );
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(8),
        Stroke::new(
            1.0,
            if selected {
                with_alpha(TEAL, 90)
            } else {
                BORDER_SOFT
            },
        ),
        egui::StrokeKind::Inside,
    );
    let state_color = if summary.silent && state == EngineState::Running {
        AMBER
    } else {
        device_state(row, state).1
    };
    let dot = egui::pos2(rect.left() + 14.0, rect.top() + 15.0);
    ui.painter()
        .circle_filled(dot, 4.0, with_alpha(state_color, 60));
    ui.painter().circle_filled(dot, 2.4, state_color);

    let text_x = rect.left() + 28.0;
    let text_right = rect.right() - 10.0;
    let (tag, tag_color) = if row.clock {
        (Some("CLOCK"), BLUE)
    } else if row.virtual_out {
        (Some("VIRTUAL"), GREEN)
    } else {
        (None, BLUE)
    };
    let tag_w = tag.map_or(0.0, |text| tag_width(ui, text) + 6.0);
    let name = fit_text(
        ui,
        &row.name,
        &name_font,
        (text_right - text_x - tag_w).max(20.0),
    );
    let name_galley = ui.painter().layout_no_wrap(name, name_font, TEXT);
    let name_size = name_galley.size();
    let name_top = rect.top() + 15.0 - name_size.y * 0.5;
    ui.painter()
        .galley(egui::pos2(text_x, name_top), name_galley, TEXT);
    if let Some(text) = tag {
        paint_tag(
            ui,
            egui::pos2(text_x + name_size.x + 6.0, rect.top() + 15.0),
            text,
            tag_color,
        );
    }
    let detail = fit_text(
        ui,
        &summary.text,
        &detail_font,
        (text_right - text_x).max(20.0),
    );
    ui.painter().text(
        egui::pos2(text_x, rect.top() + 31.0),
        Align2::LEFT_CENTER,
        detail,
        detail_font,
        if summary.silent { AMBER } else { MUTED },
    );

    show_focus_ring(ui, &response, rect, 8);
    let response = response.on_hover_text(format!("{} · {}", row.name, summary.text));
    row_info(
        &response,
        egui::Role::Button,
        ui.is_enabled(),
        format!("{} {context}", row.name),
        Some(selected),
        Some(summary.text),
    );
    ui.add_space(2.0);
    response.clicked()
}

/// Shown under the title when nothing is routed but Mono copy fills every
/// channel: not a warning, the device still plays.
pub(crate) const MONO_COPY_ONLY_NOTE: &str = "Nothing routed: every channel plays the mono copy.";

/// The amber "Nothing routed" banner, for a device that plays silence.
fn nothing_routed_banner(ui: &mut egui::Ui, device: &str) {
    egui::Frame::new()
        .fill(with_alpha(AMBER, 14))
        .stroke(Stroke::new(1.0, with_alpha(AMBER, 70)))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.label(
                    RichText::new("Nothing routed.")
                        .size(ts(10.5))
                        .color(AMBER)
                        .strong(),
                );
                ui.label(
                    RichText::new(format!(
                        "{device} is in the session but plays silence. Pick a preset or a cell."
                    ))
                    .size(ts(10.5))
                    .color(MUTED),
                );
            });
        });
}

impl RigWerkApp {
    /// The stored settings of output device `index` (0 = clock, then followers).
    fn output_settings_mut(&mut self, index: usize) -> Option<&mut OutputDeviceSettings> {
        if index == 0 {
            Some(&mut self.clock_device)
        } else {
            self.followers
                .get_mut(index - 1)
                .map(|follower| &mut follower.settings)
        }
    }

    pub(crate) fn apply_routing_action(&mut self, action: RoutingAction) {
        match action {
            RoutingAction::Select(index) => self.routing_device = index,
            RoutingAction::OpenInMixer(index) => {
                self.chain_selected = ChainSelection::OutputDevice(index);
                self.page = Page::Mixer;
            }
        }
    }

    /// Layer ②: the device list and the selected device's map.
    pub(super) fn routing_outputs_card(
        &mut self,
        ui: &mut egui::Ui,
        width: f32,
        actions: &mut Vec<RoutingAction>,
    ) {
        let rows = self.output_device_rows();
        let selected = selected_device(self.routing_device, rows.len());
        card(ui, CARD, BORDER, |ui| {
            let inner = width - 32.0;
            ui.set_width(inner);
            if inner < OUTPUTS_STACK_BELOW {
                self.routing_device_list(ui, &rows, selected, actions);
                ui.add_space(10.0);
                horizontal_line(ui, BORDER_SOFT);
                ui.add_space(10.0);
                if let Some(row) = rows.get(selected) {
                    self.routing_device_map(ui, row, selected);
                }
                return;
            }
            let list_w = (inner * 0.3).clamp(240.0, 300.0);
            let gap = 24.0;
            let map_w = inner - list_w - gap;
            ui.horizontal_top(|ui| {
                ui.allocate_ui_with_layout(
                    Vec2::new(list_w, 0.0),
                    Layout::top_down(Align::LEFT),
                    |ui| {
                        ui.set_width(list_w);
                        self.routing_device_list(ui, &rows, selected, actions);
                    },
                );
                ui.add_space(gap - ui.spacing().item_spacing.x);
                ui.allocate_ui_with_layout(
                    Vec2::new(map_w, 0.0),
                    Layout::top_down(Align::LEFT),
                    |ui| {
                        ui.set_width(map_w);
                        if let Some(row) = rows.get(selected) {
                            self.routing_device_map(ui, row, selected);
                        }
                    },
                );
            });
        });
    }

    fn routing_device_list(
        &mut self,
        ui: &mut egui::Ui,
        rows: &[OutputDeviceRow],
        selected: usize,
        actions: &mut Vec<RoutingAction>,
    ) {
        let state = self.engine.state;
        let playing: usize = rows
            .iter()
            .map(|row| row.settings.playing_channels(row.channels))
            .sum();
        // Title and count on separate lines: the list column is only
        // ~240–300 px, so a single row overlaps "MIX -> OUTPUTS".
        ui.horizontal(|ui| {
            layer_badge(ui, 2, BLUE);
            ui.label(micro_label("MIX -> OUTPUTS", BLUE));
        });
        ui.label(mono(
            super::patch_count_label(rows.len(), playing),
            DIM,
            9.5,
        ));
        ui.add_space(6.0);
        // Followers join this list in steps 2 and 3 (see `output_device_rows`).
        for (index, row) in rows.iter().enumerate() {
            if device_summary_row(ui, row, index == selected, state, "in routing outputs") {
                actions.push(RoutingAction::Select(index));
            }
        }
        ui.add_space(8.0);
        ui.add(
            egui::Label::new(RichText::new(SAME_DEVICES_NOTE).size(ts(10.0)).color(MUTED)).wrap(),
        );
        ui.add_space(6.0);
        let open = ui
            .add(
                egui::Button::new(RichText::new("Open in Mixer").size(ts(9.5)).color(BLUE))
                    .fill(with_alpha(BLUE, 14))
                    .stroke(Stroke::new(1.0, with_alpha(BLUE, 45)))
                    .corner_radius(CornerRadius::same(7)),
            )
            .on_hover_text("Open the Mixer with this output device selected");
        if open.clicked() {
            actions.push(RoutingAction::OpenInMixer(selected));
        }
    }

    /// The selected device: title, presets, its matrix with meters, the
    /// unused-channel choice and notes.
    fn routing_device_map(&mut self, ui: &mut egui::Ui, row: &OutputDeviceRow, index: usize) {
        let channels = row.channels;
        let Some(settings) = self.output_settings_mut(index).copied() else {
            return;
        };
        let mut changed = settings;

        ui.horizontal(|ui| {
            let title_font = FontId::new(ts(16.0), FontFamily::Proportional);
            let tag_w = if row.clock {
                tag_width(ui, "CLOCK") + ui.spacing().item_spacing.x
            } else if row.virtual_out {
                tag_width(ui, "VIRTUAL") + ui.spacing().item_spacing.x
            } else {
                0.0
            };
            let name = fit_text(
                ui,
                &row.name,
                &title_font,
                (ui.available_width() - tag_w).max(40.0),
            );
            ui.label(RichText::new(name).font(title_font).color(TEXT).strong())
                .on_hover_text(&row.name);
            if row.clock {
                clock_tag(ui);
            } else if row.virtual_out {
                crate::ui::widgets::virtual_tag(ui);
            }
        });
        ui.label(mono(
            format!("{} · {} CH", row.backend.to_uppercase(), channels),
            DIM,
            9.5,
        ));
        ui.add_space(8.0);

        let summary = map_summary(&settings, channels);
        if summary.silent {
            nothing_routed_banner(ui, &row.name);
            ui.add_space(8.0);
        } else if summary.nothing_routed {
            ui.label(
                RichText::new(MONO_COPY_ONLY_NOTE)
                    .size(ts(10.5))
                    .color(MUTED),
            );
            ui.add_space(8.0);
        }

        ui.horizontal_wrapped(|ui| {
            ui.label(micro_label("PRESETS", DIM));
            ui.add_space(4.0);
            for &preset in MapPreset::available(channels) {
                let result = settings.map.with_preset(preset, channels);
                let active = preset_active(settings.map, preset, channels);
                if preset_button(ui, preset.label(channels), active) {
                    changed.map = result;
                }
            }
        });
        ui.add_space(8.0);

        egui::ScrollArea::horizontal()
            .id_salt(("routing_device_matrix", index))
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(ROW_HEADER_W);
                    for channel in 0..channels {
                        column_header(ui, &format!("OUT {}", channel + 1), None);
                    }
                });
                ui.add_space(2.0);
                for (side, name) in [(MixSide::L, "Mix L"), (MixSide::R, "Mix R")] {
                    ui.horizontal(|ui| {
                        row_header(ui, name, "after master", AMBER);
                        for channel in 0..channels {
                            let on = settings.map.routes(side, channel);
                            if output_cell(ui, on, side, channel) {
                                changed.map.set(side, channel, !on);
                            }
                        }
                    });
                    ui.add_space(5.0);
                }
                ui.horizontal(|ui| {
                    let (rect, _) =
                        ui.allocate_exact_size(Vec2::new(ROW_HEADER_W, 34.0), Sense::hover());
                    ui.painter().text(
                        rect.right_center() - Vec2::new(12.0, 0.0),
                        Align2::RIGHT_CENTER,
                        "LEVEL",
                        FontId::new(ts(8.0), FontFamily::Proportional),
                        DIM,
                    );
                    for channel in 0..channels {
                        let (left, right) = settings.map.sides_at(channels, channel);
                        let (level, peak) = if index == 0 {
                            (
                                self.meter_out_channels[channel],
                                self.peak_out_channels[channel],
                            )
                        } else if let Some(follower) = index
                            .checked_sub(1)
                            .filter(|&follower| follower < self.meter_follower_channels.len())
                        {
                            (
                                self.meter_follower_channels[follower][channel],
                                self.peak_follower_channels[follower][channel],
                            )
                        } else {
                            (0.0, 0.0)
                        };
                        channel_meter(ui, left || right, settings.unused, level, peak);
                    }
                });
            });
        ui.add_space(8.0);

        if shows_unused_choice(&settings, channels) {
            block(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(micro_label("UNUSED CHANNELS", DIM));
                    ui.add_space(4.0);
                    for (mode, label) in [
                        (UnusedChannels::Silent, "Silent"),
                        (UnusedChannels::MonoCopy, "Mono copy"),
                    ] {
                        if tab_button_sized(ui, label, settings.unused == mode, BLUE, 9.5)
                            .clicked()
                        {
                            changed.unused = mode;
                        }
                    }
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(
                            "Channels with no cell set. Mono copy is how RigWerk fills extra channels today.",
                        )
                        .size(ts(10.0))
                        .color(MUTED),
                    );
                });
            });
            ui.add_space(8.0);
        }

        if channels == 1 {
            ui.add(
                egui::Label::new(RichText::new(MONO_DEVICE_NOTE).size(ts(10.0)).color(MUTED))
                    .wrap(),
            );
            ui.add_space(4.0);
        }
        if row.clock {
            ui.add(
                egui::Label::new(RichText::new(CLOCK_DEVICE_NOTE).size(ts(10.0)).color(MUTED))
                    .wrap(),
            );
        }

        if changed != settings {
            if let Some(stored) = self.output_settings_mut(index) {
                *stored = changed;
            }
            let summary = map_summary(&changed, channels);
            self.push_log(
                format!("{}: {}.", row.name, summary.text),
                if summary.silent { AMBER } else { TEAL },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        device_summary_row, map_summary, output_cell, preset_active, selected_device,
        shows_unused_choice, RoutingAction,
    };
    use crate::app::{blank_app, Page};
    use crate::audio::{
        ChannelMap, EngineState, MapPreset, MixSide, OutputDeviceSettings, UnusedChannels,
    };
    use crate::ui::pages::mixer::chain::ChainSelection;
    use crate::ui::pages::mixer::output_devices::OutputDeviceRow;
    use crate::ui::widgets::{accesskit_frame, node_named};
    use eframe::egui::{self, accesskit::Toggled};

    fn with_map(left: u8, right: u8, unused: UnusedChannels) -> OutputDeviceSettings {
        OutputDeviceSettings {
            map: ChannelMap { left, right },
            unused,
            ..OutputDeviceSettings::default()
        }
    }

    fn text(settings: &OutputDeviceSettings, channels: usize) -> String {
        map_summary(settings, channels).text
    }

    const SILENT: UnusedChannels = UnusedChannels::Silent;
    const COPY: UnusedChannels = UnusedChannels::MonoCopy;

    #[test]
    fn the_default_map_reads_as_a_pair() {
        let default = OutputDeviceSettings::default();
        assert_eq!(text(&default, 2), "L/R");
        assert_eq!(text(&default, 4), "Out 1/2 · rest mono copy");
        assert_eq!(text(&with_map(0b01, 0b10, SILENT), 4), "Out 1/2");
        assert!(!map_summary(&default, 2).nothing_routed);
    }

    #[test]
    fn pair_presets_read_as_pairs() {
        let on4 = |preset| {
            let map = ChannelMap::default().with_preset(preset, 4);
            text(&with_map(map.left, map.right, SILENT), 4)
        };
        assert_eq!(on4(MapPreset::Stereo34), "Out 3/4");
        assert_eq!(on4(MapPreset::BothPairs), "Out 1/2 · Out 3/4");
        assert_eq!(on4(MapPreset::Swap), "Out 1/2 swapped");
        assert_eq!(
            on4(MapPreset::MonoToAll),
            "Out 1←mono · Out 2←mono · Out 3←mono · Out 4←mono"
        );
        assert_eq!(text(&with_map(0b10, 0b01, COPY), 2), "L/R swapped");
    }

    #[test]
    fn single_channels_name_their_side() {
        assert_eq!(
            text(&with_map(0b001, 0b100, SILENT), 4),
            "Out 1←L · Out 3←R"
        );
        assert_eq!(
            text(&with_map(0b11, 0b10, SILENT), 2),
            "Out 1←L · Out 2←mono"
        );
        // Out 2/3 is not a pair: pairs start on odd outputs.
        assert_eq!(
            text(&with_map(0b010, 0b100, SILENT), 4),
            "Out 2←L · Out 3←R"
        );
    }

    #[test]
    fn an_empty_map_says_what_plays_instead() {
        let empty = map_summary(&with_map(0, 0, SILENT), 2);
        assert_eq!(empty.text, "nothing routed · plays silence");
        assert!(empty.nothing_routed);
        assert!(empty.silent, "the only case that warns");
        let copy = map_summary(&with_map(0, 0, COPY), 4);
        assert_eq!(copy.text, "nothing routed · mono copy on all");
        assert!(copy.nothing_routed);
        assert!(!copy.silent, "mono copy still plays: no warning");
        assert!(!map_summary(&OutputDeviceSettings::default(), 2).silent);
        // Bits past the device don't count.
        assert!(map_summary(&with_map(0b0100, 0b1000, SILENT), 2).nothing_routed);
    }

    #[test]
    fn a_mono_device_reads_mono_whenever_anything_is_routed() {
        assert_eq!(text(&OutputDeviceSettings::default(), 1), "Out 1←mono");
        assert_eq!(text(&with_map(0, 0b1000, SILENT), 1), "Out 1←mono");
        assert!(map_summary(&with_map(0, 0, SILENT), 1).nothing_routed);
    }

    #[test]
    fn preset_buttons_show_the_current_map() {
        let default = ChannelMap::default();
        assert!(preset_active(default, MapPreset::Stereo12, 2));
        assert!(!preset_active(default, MapPreset::Swap, 2));
        assert!(!preset_active(default, MapPreset::MonoToAll, 2));
        // One channel: any routed map plays (L + R) / 2.
        assert!(preset_active(default, MapPreset::MonoToAll, 1));
        assert!(preset_active(
            ChannelMap {
                left: 0,
                right: 0b100
            },
            MapPreset::MonoToAll,
            1
        ));
        let empty = ChannelMap { left: 0, right: 0 };
        assert!(!preset_active(empty, MapPreset::MonoToAll, 1));
        assert!(preset_active(empty, MapPreset::Clear, 1));
    }

    #[test]
    fn the_unused_choice_shows_when_it_matters() {
        let default = OutputDeviceSettings::default();
        assert!(!shows_unused_choice(&default, 2));
        assert!(shows_unused_choice(&default, 4));
        assert!(shows_unused_choice(&with_map(0b01, 0, SILENT), 2));
        assert!(!shows_unused_choice(&default, 1));
        assert!(shows_unused_choice(&with_map(0, 0, SILENT), 1));
    }

    #[test]
    fn the_selection_falls_back_to_the_clock_device() {
        assert_eq!(selected_device(0, 1), 0);
        assert_eq!(
            selected_device(3, 1),
            0,
            "out of range falls back to the clock device"
        );
        assert_eq!(selected_device(1, 2), 1);
    }

    #[test]
    fn open_in_mixer_selects_the_device_there() {
        let mut app = blank_app();
        app.page = Page::Routing;
        app.chain_selected = ChainSelection::PreEq;
        app.apply_routing_action(RoutingAction::Select(1));
        assert!(app.page == Page::Routing);
        assert_eq!(app.routing_device, 1);
        assert_eq!(
            app.chain_selected,
            ChainSelection::PreEq,
            "selecting here leaves the Mixer alone"
        );

        app.apply_routing_action(RoutingAction::OpenInMixer(0));
        assert!(app.page == Page::Mixer);
        assert_eq!(app.chain_selected, ChainSelection::OutputDevice(0));
    }

    #[test]
    fn an_output_cell_exposes_its_name_and_value() {
        let tree = accesskit_frame(|ui| {
            assert!(!output_cell(ui, true, MixSide::L, 2));
            assert!(!output_cell(ui, false, MixSide::R, 0));
        });
        let on = node_named(&tree, "Mix L to Out 3");
        assert_eq!(on.role(), egui::Role::CheckBox);
        assert_eq!(on.toggled(), Some(Toggled::True));
        assert_eq!(on.value(), Some("routed"));
        let off = node_named(&tree, "Mix R to Out 1");
        assert_eq!(off.toggled(), Some(Toggled::False));
        assert_eq!(off.value(), Some("off"));
    }

    #[test]
    fn a_device_row_reads_its_map() {
        let row = OutputDeviceRow {
            virtual_out: false,
            follower_index: None,
            missing: false,
            publish: false,
            name: "Scarlett 4i4".to_owned(),
            backend: "PipeWire".to_owned(),
            channels: 4,
            clock: true,
            settings: with_map(0b0101, 0b1010, SILENT),
        };
        let tree = accesskit_frame(|ui| {
            assert!(!device_summary_row(
                ui,
                &row,
                true,
                EngineState::Running,
                "in routing outputs"
            ));
        });
        let node = node_named(&tree, "Scarlett 4i4 in routing outputs");
        assert_eq!(node.role(), egui::Role::Button);
        assert_eq!(node.toggled(), Some(Toggled::True));
        assert_eq!(node.value(), Some("Out 1/2 · Out 3/4"));
    }

    #[test]
    fn the_page_labels_presets_and_cells() {
        let mut app = blank_app();
        let tree = accesskit_frame(|ui| app.routing_page(ui));
        for name in [
            "Stereo preset",
            "Mono to all preset",
            "Swap L/R preset",
            "Clear preset",
            "Mix L to Out 1",
            "Mix R to Out 2",
            "Route input 1 to Mix L",
        ] {
            let _ = node_named(&tree, name);
        }
        let open = node_named(&tree, "Open in Mixer");
        assert_eq!(open.role(), egui::Role::Button);
    }
}
