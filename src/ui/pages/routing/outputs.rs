//! Layer ② of Signal Routing: which device channels play each path.
//!
//! The device list is the Mixer's (`output_device_rows`), in the same order.
//! The page keeps its own selected device (`routing_device`); "Open in
//! Mixer" selects that device there. Followers and RigWerk Out appear when
//! they are in the session.
//!
//! With one path the matrix is today's Mix L / Mix R onto the device's
//! channels.  With several, the device header carries its align switch,
//! the matrix has one row pair per path in the path's colour (cells in one
//! column add up), and a sum block (`sum`) lists per path its level, polarity and
//! trim on this device and the alignment delay it gets there, then a
//! latency line ("Waits for <path>, +X ms") and latency bars (own chain
//! latency, hatched alignment delay).  The routing warnings
//! (`RigWerkApp::routing_warnings`) sit above the matrix.  Alignment only
//! lines up the paths on one device, never devices with each other.

use crate::app::LogTone;
use eframe::egui;
use egui::{
    Align, Align2, Color32, CornerRadius, FontFamily, FontId, Layout, Margin, RichText, Sense,
    Stroke, Vec2,
};

use crate::app::alignment::{channel_list, RoutingWarning, RoutingWarningKind};
use crate::app::cards::Card;
use crate::app::chain::ChainSelection;
use crate::app::output_devices::OutputDeviceRow;
use crate::app::paths::PathInfo;
use crate::app::{Page, RigWerkApp};
use crate::audio::{
    ChannelMap, EngineState, MapPreset, MixSide, OutputDeviceSettings, PathMapSettings,
    UnusedChannels, MAX_OUTPUT_CHANNELS,
};
use crate::ui::pages::mixer::output_devices::{block, device_state};
use crate::ui::theme::ts;
use crate::ui::theme::{
    path_colour, with_alpha, AMBER, BLUE, BORDER, BORDER_SOFT, CARD, DIM, GREEN, MUTED, RED, TEAL,
    TEXT,
};
use crate::ui::widgets::{
    card, clock_tag, fit_text, horizontal_line, micro_label, mono, paint_tag, path_chip,
    path_level_text, row_info, show_focus_ring, tab_button_sized, tag_width,
};

use super::paths::trim_text;
use super::sum::{align_switch, sum_block, SumView, SUM_NOTE};
use super::{
    column_header, layer_badge, matrix_cell, path_output_cell, path_row_header, row_header, CELL,
    ROW_HEADER_W,
};

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
pub(crate) fn map_summary(
    settings: &OutputDeviceSettings,
    path: usize,
    channels: usize,
) -> MapSummary {
    let channels = channels.min(MAX_OUTPUT_CHANNELS);
    let map = settings.map(path);
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
/// channels, and whenever one of its channels is left unreached by path
/// `path`.
pub(crate) fn shows_unused_choice(
    settings: &OutputDeviceSettings,
    path: usize,
    channels: usize,
) -> bool {
    channels > 2
        || (0..channels.min(MAX_OUTPUT_CHANNELS))
            .any(|channel| settings.map(path).sides_at(channels, channel) == (false, false))
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
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum RoutingAction {
    /// Show this device's map on this page only.
    Select(usize),
    /// Go to the Mixer with this device selected.
    OpenInMixer(usize),
    /// Set up paths and routing from a card.
    ApplyCard(Card),
    /// Add an empty, muted path.
    AddPath,
    /// Add a muted copy of this path slot.
    DuplicatePath(usize),
    /// Remove this path slot (it fades out).
    RemovePath(usize),
    /// Make the presets on this page set this path slot's map.
    PresetPath(usize),
    /// Go to the Mixer on this path slot.
    OpenPathInMixer(usize),
    PathMute(usize, bool),
    PathSolo(usize, bool),
    PathLevel(usize, f32),
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
    path: usize,
    selected: bool,
    state: EngineState,
    context: &str,
) -> bool {
    let summary = map_summary(&row.settings, path, row.channels);
    let state_color = if summary.silent && state == EngineState::Running {
        AMBER
    } else {
        device_state(row, state).1
    };
    let line = DeviceLine {
        color: if summary.silent { AMBER } else { MUTED },
        text: summary.text,
        dots: Vec::new(),
    };
    device_row(ui, row, &line, state_color, selected, context)
}

/// The detail line of a device row: its text and colour, and the colour
/// dots of the paths that play there (several paths only).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DeviceLine {
    pub(crate) text: String,
    pub(crate) color: Color32,
    pub(crate) dots: Vec<Color32>,
}

/// Paints one device row: state dot, name, tag, then `line`.
fn device_row(
    ui: &mut egui::Ui,
    row: &OutputDeviceRow,
    line: &DeviceLine,
    state_color: Color32,
    selected: bool,
    context: &str,
) -> bool {
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
    let mut detail_x = text_x;
    for color in &line.dots {
        ui.painter()
            .circle_filled(egui::pos2(detail_x + 3.5, rect.top() + 31.0), 3.5, *color);
        detail_x += 10.0;
    }
    if !line.dots.is_empty() {
        detail_x += 2.0;
    }
    let detail = fit_text(
        ui,
        &line.text,
        &detail_font,
        (text_right - detail_x).max(20.0),
    );
    ui.painter().text(
        egui::pos2(detail_x, rect.top() + 31.0),
        Align2::LEFT_CENTER,
        detail,
        detail_font,
        line.color,
    );

    show_focus_ring(ui, &response, rect, 8);
    let response = response.on_hover_text(format!("{} · {}", row.name, line.text));
    row_info(
        &response,
        egui::Role::Button,
        ui.is_enabled(),
        format!("{} {context}", row.name),
        Some(selected),
        Some(line.text.clone()),
    );
    ui.add_space(2.0);
    response.clicked()
}

/// Whether to offer Silent / Mono copy with several paths: on any device
/// with more than two channels, and whenever a channel is reached by none
/// of `slots`.
pub(crate) fn shows_unused_choice_paths(
    settings: &OutputDeviceSettings,
    slots: &[usize],
    channels: usize,
) -> bool {
    channels > 2
        || (0..channels.min(MAX_OUTPUT_CHANNELS)).any(|channel| {
            slots
                .iter()
                .all(|&slot| settings.map(slot).sides_at(channels, channel) == (false, false))
        })
}

/// The red (Sum) or amber (Shift) banner over a device's matrix.  With
/// the device's alignment off it offers to turn it on (pushed to `edits`).
fn warning_banner(
    ui: &mut egui::Ui,
    warning: &RoutingWarning,
    device: &str,
    align: bool,
    edits: &mut Vec<MapEdit>,
) {
    let (color, title, body) = match warning.kind {
        RoutingWarningKind::Sum => (
            RED,
            format!(
                "Paths add up unaligned on {device} ({}).",
                channel_list(&warning.channels)
            ),
            if align {
                "They arrive at different times and comb filter (thin, hollow sound): their trims pull them apart. Set the trims equal."
            } else {
                "They arrive at different times and comb filter (thin, hollow sound). Turn alignment on, or trim by hand."
            },
        ),
        RoutingWarningKind::Shift => (
            AMBER,
            format!("Paths are not aligned on {device}."),
            "Nothing adds up here, so there's no comb filter, but the channels arrive at different times.",
        ),
    };
    egui::Frame::new()
        .fill(with_alpha(color, 14))
        .stroke(Stroke::new(1.0, with_alpha(color, 80)))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.label(RichText::new(title).size(ts(10.5)).color(color).strong());
                ui.label(RichText::new(body).size(ts(10.5)).color(MUTED));
                if !align {
                    ui.add_space(6.0);
                    let button = ui
                        .add(
                            egui::Button::new(
                                RichText::new("Align paths").size(ts(9.5)).color(color),
                            )
                            .fill(with_alpha(color, 18))
                            .stroke(Stroke::new(1.0, with_alpha(color, 90)))
                            .corner_radius(CornerRadius::same(6)),
                        )
                        .on_hover_text(format!("Turn alignment on for {device}"));
                    if button.clicked() {
                        edits.push(MapEdit::Align(true));
                    }
                }
            });
        });
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

/// One edit of the selected device's settings, asked for while drawing and
/// applied after it ([`RigWerkApp::apply_map_edit`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum MapEdit {
    /// Turn one matrix cell of a path on or off.
    Cell {
        slot: usize,
        side: MixSide,
        channel: usize,
        on: bool,
    },
    /// A preset: the path's whole map.
    Map { slot: usize, map: ChannelMap },
    /// The path's level on the device; `commit` on a ± click or at the end
    /// of a drag (only then is it logged).
    Level {
        slot: usize,
        level_db: f32,
        commit: bool,
    },
    /// The path's polarity on the device.
    Invert { slot: usize, invert: bool },
    /// The path's alignment trim on the device, ms; `commit` as for
    /// `Level`.
    Trim {
        slot: usize,
        trim_ms: f32,
        commit: bool,
    },
    /// The device's align switch.
    Align(bool),
    /// What the device's unreached channels play.
    Unused(UnusedChannels),
}

/// The paths whose own map reaches some channel of a `channels`-channel
/// device with these settings.
fn paths_reaching<'a>(
    settings: &OutputDeviceSettings,
    channels: usize,
    infos: &'a [PathInfo],
) -> Vec<&'a PathInfo> {
    infos
        .iter()
        .filter(|info| settings.map(info.slot).reaches_any(channels))
        .collect()
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
            RoutingAction::ApplyCard(card) => {
                if !self.apply_card(card) {
                    self.push_log(
                        format!("{}: no free path slot yet, try again.", card.label()),
                        LogTone::Amber,
                    );
                }
            }
            RoutingAction::AddPath => {
                let _ = self.add_path();
            }
            RoutingAction::DuplicatePath(slot) => {
                let _ = self.duplicate_path(slot);
            }
            RoutingAction::RemovePath(slot) => {
                let _ = self.remove_path(slot);
            }
            RoutingAction::PresetPath(slot) => self.routing_preset_path = Some(slot),
            RoutingAction::OpenPathInMixer(slot) => {
                if self.select_path(slot) {
                    if let ChainSelection::OutputDevice(_) = self.chain_selected {
                        self.chain_selected = ChainSelection::PreEq;
                    }
                    self.page = Page::Mixer;
                }
            }
            RoutingAction::PathMute(slot, mute) => self.set_path_mute(slot, mute),
            RoutingAction::PathSolo(slot, solo) => self.set_path_solo(slot, solo),
            RoutingAction::PathLevel(slot, level_db) => self.set_path_level(slot, level_db),
        }
    }

    /// Applies one edit of output device `device` and logs it: map edits
    /// with the device's new map, level only when committed, polarity on
    /// its own line.
    pub(crate) fn apply_map_edit(&mut self, device: usize, edit: MapEdit) {
        let Some(row) = self.output_device_rows().into_iter().nth(device) else {
            return;
        };
        let multi = self.path_count() > 1;
        let path_name = |app: &Self, slot: usize| {
            app.paths
                .get(slot)
                .and_then(Option::as_ref)
                .map_or_else(String::new, |path| path.name.clone())
        };
        let edit_entry = |app: &mut Self, slot: usize, change: &dyn Fn(&mut PathMapSettings)| {
            let Some(mut entry) = app.device_path_map(device, slot) else {
                return false;
            };
            change(&mut entry);
            app.set_device_path_map(device, slot, entry);
            true
        };
        match edit {
            MapEdit::Cell {
                slot,
                side,
                channel,
                on,
            } => {
                if !edit_entry(self, slot, &|entry| entry.map.set(side, channel, on)) {
                    return;
                }
                self.log_map(device, &row.name, row.channels, slot, multi);
            }
            MapEdit::Map { slot, map } => {
                if !edit_entry(self, slot, &|entry| entry.map = map) {
                    return;
                }
                self.log_map(device, &row.name, row.channels, slot, multi);
            }
            MapEdit::Level {
                slot,
                level_db,
                commit,
            } => {
                if !edit_entry(self, slot, &|entry| entry.level_db = level_db) || !commit {
                    return;
                }
                let level = self
                    .device_path_map(device, slot)
                    .map_or(level_db, |entry| entry.level_db);
                self.push_log(
                    format!(
                        "{} level on {}: {}.",
                        path_name(self, slot),
                        row.name,
                        path_level_text(level)
                    ),
                    LogTone::Teal,
                );
            }
            MapEdit::Invert { slot, invert } => {
                if !edit_entry(self, slot, &|entry| entry.invert = invert) {
                    return;
                }
                self.push_log(
                    format!(
                        "{} polarity on {}: {}.",
                        path_name(self, slot),
                        row.name,
                        if invert { "flipped" } else { "normal" }
                    ),
                    if invert {
                        LogTone::Amber
                    } else {
                        LogTone::Teal
                    },
                );
            }
            MapEdit::Trim {
                slot,
                trim_ms,
                commit,
            } => {
                if !edit_entry(self, slot, &|entry| entry.trim_ms = trim_ms) || !commit {
                    return;
                }
                let trim = self
                    .device_path_map(device, slot)
                    .map_or(trim_ms, |entry| entry.trim_ms);
                self.push_log(
                    format!(
                        "{} trim on {}: {}.",
                        path_name(self, slot),
                        row.name,
                        trim_text(trim)
                    ),
                    LogTone::Teal,
                );
            }
            MapEdit::Align(align) => {
                self.set_device_align(device, align);
                self.push_log(
                    format!(
                        "{}: alignment {}.",
                        row.name,
                        if align {
                            "on, every path waits for the slowest"
                        } else {
                            "off, each path plays as its chain finishes"
                        }
                    ),
                    if align { LogTone::Teal } else { LogTone::Amber },
                );
            }
            MapEdit::Unused(mode) => {
                if let Some(settings) = self.output_settings_mut(device) {
                    settings.unused = mode;
                }
                self.push_log(
                    format!(
                        "{}: unused channels {}.",
                        row.name,
                        match mode {
                            UnusedChannels::Silent => "silent",
                            UnusedChannels::MonoCopy => "play a mono copy",
                        }
                    ),
                    LogTone::Teal,
                );
            }
        }
    }

    /// Logs path `slot`'s map on a device after an edit.
    fn log_map(&mut self, device: usize, name: &str, channels: usize, slot: usize, multi: bool) {
        let Some(settings) = self.device_settings().get(device).copied().copied() else {
            return;
        };
        let summary = map_summary(&settings, slot, channels);
        let prefix = match self.paths.get(slot).and_then(Option::as_ref) {
            Some(path) if multi => format!("{name}: {}", path.name),
            _ => name.to_owned(),
        };
        self.push_log(
            format!("{prefix}: {}.", summary.text),
            if summary.silent {
                LogTone::Amber
            } else {
                LogTone::Teal
            },
        );
    }

    /// What a device's list row says with several paths: an unaligned sum
    /// first, then silence, the one path that plays there, or "N paths"
    /// (amber while they are not aligned) with the slowest path's latency.
    pub(crate) fn device_paths_line(
        &self,
        device: usize,
        row: &OutputDeviceRow,
        infos: &[PathInfo],
    ) -> DeviceLine {
        let reaching = paths_reaching(&row.settings, row.channels, infos);
        let dots = reaching
            .iter()
            .map(|info| path_colour(info.colour_index))
            .collect();
        let warning = self
            .routing_warnings(device)
            .first()
            .map(|warning| warning.kind);
        if warning == Some(RoutingWarningKind::Sum) {
            return DeviceLine {
                text: "unaligned sum · comb filter".to_owned(),
                color: RED,
                dots,
            };
        }
        match reaching.as_slice() {
            [] => {
                let summary = map_summary(&row.settings, infos[0].slot, row.channels);
                DeviceLine {
                    color: if summary.silent { AMBER } else { MUTED },
                    text: summary.text,
                    dots,
                }
            }
            [info] => DeviceLine {
                text: format!(
                    "{} · {}",
                    info.name,
                    map_summary(&row.settings, info.slot, row.channels).text
                ),
                color: MUTED,
                dots,
            },
            several => {
                let channels = row.channels.min(MAX_OUTPUT_CHANNELS);
                let summed = (0..channels).any(|channel| {
                    several
                        .iter()
                        .filter(|info| {
                            row.settings.map(info.slot).sides_at(channels, channel)
                                != (false, false)
                        })
                        .count()
                        > 1
                });
                // What the device plays at: the slowest arrival there.
                let slowest = self.device_alignment(device).map_or_else(
                    || {
                        several
                            .iter()
                            .fold(0.0_f32, |slowest, info| slowest.max(info.latency_ms))
                    },
                    |alignment| alignment.total_ms,
                );
                DeviceLine {
                    text: format!(
                        "{} paths{} · {slowest:.1} ms",
                        several.len(),
                        if summed { " · summed" } else { "" }
                    ),
                    color: if warning == Some(RoutingWarningKind::Shift) {
                        AMBER
                    } else {
                        MUTED
                    },
                    dots,
                }
            }
        }
    }

    /// Layer ②: the device list and the selected device's map.
    pub(super) fn routing_outputs_card(
        &mut self,
        ui: &mut egui::Ui,
        width: f32,
        infos: &[PathInfo],
        actions: &mut Vec<RoutingAction>,
    ) {
        let rows = self.output_device_rows();
        let selected = selected_device(self.routing_device, rows.len());
        let mut edits = Vec::new();
        card(ui, CARD, BORDER, |ui| {
            let inner = width - 32.0;
            ui.set_width(inner);
            if inner < OUTPUTS_STACK_BELOW {
                self.routing_device_list(ui, &rows, selected, infos, actions);
                ui.add_space(10.0);
                horizontal_line(ui, BORDER_SOFT);
                ui.add_space(10.0);
                if let Some(row) = rows.get(selected) {
                    self.routing_device_map(ui, row, selected, infos, actions, &mut edits);
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
                        self.routing_device_list(ui, &rows, selected, infos, actions);
                    },
                );
                ui.add_space(gap - ui.spacing().item_spacing.x);
                ui.allocate_ui_with_layout(
                    Vec2::new(map_w, 0.0),
                    Layout::top_down(Align::LEFT),
                    |ui| {
                        ui.set_width(map_w);
                        if let Some(row) = rows.get(selected) {
                            self.routing_device_map(ui, row, selected, infos, actions, &mut edits);
                        }
                    },
                );
            });
        });
        for edit in edits {
            self.apply_map_edit(selected, edit);
        }
    }

    fn routing_device_list(
        &self,
        ui: &mut egui::Ui,
        rows: &[OutputDeviceRow],
        selected: usize,
        infos: &[PathInfo],
        actions: &mut Vec<RoutingAction>,
    ) {
        let state = self.engine.state;
        let multi = infos.len() > 1;
        let path = infos.first().map_or(0, |info| info.slot);
        let live = self.live_path_mask();
        let playing: usize = rows
            .iter()
            .map(|row| row.settings.playing_channels(row.channels, live))
            .sum();
        // Title and count on separate lines: the list column is only
        // ~240–300 px, so a single row overlaps "MIX -> OUTPUTS".
        ui.horizontal(|ui| {
            layer_badge(ui, 2, BLUE);
            ui.label(micro_label(
                if multi {
                    "PATHS -> OUTPUTS"
                } else {
                    "MIX -> OUTPUTS"
                },
                BLUE,
            ));
        });
        ui.label(mono(
            super::patch_count_label(rows.len(), playing),
            DIM,
            9.5,
        ));
        ui.add_space(6.0);
        for (index, row) in rows.iter().enumerate() {
            let clicked = if multi {
                let line = self.device_paths_line(index, row, infos);
                // While running, a warning colours the state dot too.
                let state_color = if state == EngineState::Running && line.color != MUTED {
                    line.color
                } else {
                    device_state(row, state).1
                };
                device_row(
                    ui,
                    row,
                    &line,
                    state_color,
                    index == selected,
                    "in routing outputs",
                )
            } else {
                device_summary_row(
                    ui,
                    row,
                    path,
                    index == selected,
                    state,
                    "in routing outputs",
                )
            };
            if clicked {
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

    /// The path the presets set: the page's own choice while that path
    /// lives, else the selected path.
    fn routing_preset_slot(&self, infos: &[PathInfo]) -> usize {
        self.routing_preset_path
            .filter(|&slot| infos.iter().any(|info| info.slot == slot))
            .or_else(|| {
                infos
                    .iter()
                    .find(|info| info.selected)
                    .map(|info| info.slot)
            })
            .or_else(|| infos.first().map(|info| info.slot))
            .unwrap_or(0)
    }

    /// The selected device: title, warnings, presets, its matrix (one row
    /// pair per path) with meters, the sum block, the unused-channel choice
    /// and notes.  Edits go to `edits`.
    fn routing_device_map(
        &self,
        ui: &mut egui::Ui,
        row: &OutputDeviceRow,
        index: usize,
        infos: &[PathInfo],
        actions: &mut Vec<RoutingAction>,
        edits: &mut Vec<MapEdit>,
    ) {
        let channels = row.channels;
        let multi = infos.len() > 1;
        let alignment = self.device_alignment(index).unwrap_or_default();
        // Presets (and the single-path matrix) edit this path.
        let path = self.routing_preset_slot(infos);
        let slots: Vec<usize> = infos.iter().map(|info| info.slot).collect();
        let settings = row.settings;

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
            if multi {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if let Some(align) = align_switch(ui, settings.align, &row.name) {
                        edits.push(MapEdit::Align(align));
                    }
                });
            }
        });
        ui.label(mono(
            format!("{} · {} CH", row.backend.to_uppercase(), channels),
            DIM,
            9.5,
        ));
        ui.add_space(8.0);

        let reaching = paths_reaching(&settings, channels, infos);
        let (silent, nothing_routed) = if multi {
            let none = reaching.is_empty();
            (none && settings.unused == UnusedChannels::Silent, none)
        } else {
            let summary = map_summary(&settings, path, channels);
            (summary.silent, summary.nothing_routed)
        };
        let warnings = if multi {
            self.routing_warnings(index)
        } else {
            Vec::new()
        };
        for warning in &warnings {
            warning_banner(ui, warning, &row.name, settings.align, edits);
            ui.add_space(8.0);
        }
        if silent {
            nothing_routed_banner(ui, &row.name);
            ui.add_space(8.0);
        } else if nothing_routed {
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
            if multi {
                // Which path the presets set (this page only).
                for info in infos {
                    let chip = path_chip(
                        ui,
                        path_colour(info.colour_index),
                        &info.name,
                        info.mute,
                        info.slot == path,
                        160.0,
                    )
                    .on_hover_text(format!("Presets set {}'s map here", info.name));
                    if chip.clicked() {
                        actions.push(RoutingAction::PresetPath(info.slot));
                    }
                }
                ui.add_space(4.0);
            }
            for &preset in MapPreset::available(channels) {
                let result = settings.map(path).with_preset(preset, channels);
                let active = preset_active(settings.map(path), preset, channels);
                if preset_button(ui, preset.label(channels), active) {
                    edits.push(MapEdit::Map {
                        slot: path,
                        map: result,
                    });
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
                let mut cell = |slot: usize, side: MixSide, channel: usize, on: bool| {
                    edits.push(MapEdit::Cell {
                        slot,
                        side,
                        channel,
                        on,
                    });
                };
                if multi {
                    for (position, info) in infos.iter().enumerate() {
                        for (side, letter) in [(MixSide::L, "L"), (MixSide::R, "R")] {
                            ui.horizontal(|ui| {
                                path_row_header(
                                    ui,
                                    &format!("{} {letter}", info.name),
                                    &format!("path {} · after path level", position + 1),
                                    path_colour(info.colour_index),
                                    info.audible,
                                );
                                for channel in 0..channels {
                                    let on = settings.map(info.slot).routes(side, channel);
                                    if path_output_cell(
                                        ui,
                                        on,
                                        &info.name,
                                        letter,
                                        channel,
                                        path_colour(info.colour_index),
                                    ) {
                                        cell(info.slot, side, channel, !on);
                                    }
                                }
                            });
                            ui.add_space(5.0);
                        }
                    }
                } else {
                    for (side, name) in [(MixSide::L, "Mix L"), (MixSide::R, "Mix R")] {
                        ui.horizontal(|ui| {
                            row_header(ui, name, "after master", AMBER);
                            for channel in 0..channels {
                                let on = settings.map(path).routes(side, channel);
                                if output_cell(ui, on, side, channel) {
                                    cell(path, side, channel, !on);
                                }
                            }
                        });
                        ui.add_space(5.0);
                    }
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
                        let reached = slots.iter().any(|&slot| {
                            settings.map(slot).sides_at(channels, channel) != (false, false)
                        });
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
                        channel_meter(ui, reached, settings.unused, level, peak);
                    }
                });
            });
        ui.add_space(8.0);

        if multi && !reaching.is_empty() {
            let unaligned: Vec<usize> = warnings
                .iter()
                .filter(|warning| warning.kind == RoutingWarningKind::Sum)
                .flat_map(|warning| warning.paths.iter().copied())
                .collect();
            sum_block(
                ui,
                row,
                &settings,
                &SumView {
                    reaching: &reaching,
                    alignment: &alignment,
                    unaligned: &unaligned,
                },
                edits,
            );
            ui.add_space(8.0);
        }

        let unused_choice = if multi {
            shows_unused_choice_paths(&settings, &slots, channels)
        } else {
            shows_unused_choice(&settings, path, channels)
        };
        if unused_choice {
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
                            && settings.unused != mode
                        {
                            edits.push(MapEdit::Unused(mode));
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

        if multi && reaching.len() > 1 {
            ui.add(egui::Label::new(RichText::new(SUM_NOTE).size(ts(10.0)).color(MUTED)).wrap());
            ui.add_space(4.0);
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
    }
}

#[cfg(test)]
mod tests;
