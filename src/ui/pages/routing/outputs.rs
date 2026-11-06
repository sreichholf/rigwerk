//! Layer ② of Signal Routing: which device channels play each path.
//!
//! The device list is the Mixer's (`output_device_rows`), in the same order.
//! The page keeps its own selected device (`routing_device`); "Open in
//! Mixer" selects that device there. Followers and RigWerk Out appear when
//! they are in the session.
//!
//! With one path the matrix is today's Mix L / Mix R onto the device's
//! channels.  With several, it has one row pair per path in the path's
//! colour (cells in one column add up), a sum block per path (level and
//! polarity on this device; trim and alignment wait for P3), and the
//! routing warnings (`RigWerkApp::routing_warnings`).

use eframe::egui;
use egui::{
    Align, Align2, Color32, CornerRadius, FontFamily, FontId, Layout, Margin, RichText, Sense,
    Stroke, Vec2,
};

use crate::app::cards::{Card, RoutingWarning, RoutingWarningKind};
use crate::app::paths::PathInfo;
use crate::app::{Page, RigWerkApp};
use crate::audio::{
    ChannelMap, EngineState, MapPreset, MixSide, OutputDeviceSettings, PathMapSettings,
    UnusedChannels, MAX_OUTPUT_CHANNELS,
};
use crate::ui::pages::mixer::chain::ChainSelection;
use crate::ui::pages::mixer::output_devices::{block, device_state, OutputDeviceRow};
use crate::ui::theme::ts;
use crate::ui::theme::{
    with_alpha, AMBER, BLUE, BORDER, BORDER_SOFT, CARD, DIM, GREEN, MUTED, RED, TEAL, TEXT,
};
use crate::ui::widgets::{
    card, clock_tag, fit_text, horizontal_line, micro_label, mono, paint_tag, path_chip,
    path_level_text, path_toggle, row_info, show_focus_ring, tab_button_sized, tag_width,
};

use super::paths::level_stepper;
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

/// "Out 1, Out 2" for 0-based channels.
pub(crate) fn channel_list(channels: &[usize]) -> String {
    channels
        .iter()
        .map(|channel| format!("Out {}", channel + 1))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The red (Sum) or amber (Shift) banner over a device's matrix.
fn warning_banner(ui: &mut egui::Ui, warning: &RoutingWarning, device: &str) {
    let (color, title, body) = match warning.kind {
        RoutingWarningKind::Sum => (
            RED,
            format!(
                "Paths add up unaligned on {device} ({}).",
                channel_list(&warning.channels)
            ),
            "They arrive at different times and can comb filter (thin, hollow sound). Until alignment lands, keep summed paths on separate channels or devices.",
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
                        AMBER,
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
                    TEAL,
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
                    if invert { AMBER } else { TEAL },
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
                    TEAL,
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
            if summary.silent { AMBER } else { TEAL },
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
        let dots = reaching.iter().map(|info| info.colour).collect();
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
                let slowest = several
                    .iter()
                    .fold(0.0_f32, |slowest, info| slowest.max(info.latency_ms));
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
        if multi {
            for warning in self.routing_warnings(index) {
                warning_banner(ui, &warning, &row.name);
                ui.add_space(8.0);
            }
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
                        info.colour,
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
                                    info.colour,
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
                                        info.colour,
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
            sum_block(ui, row, &settings, &reaching, edits);
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

/// Under the sum block when several paths play on one device.
pub(crate) const SUM_NOTE: &str =
    "Cells in one column add up. Master level and mute (Mixer -> Output) act after the sum.";

/// The sum block: per path on this device, its level and polarity; trim
/// and the alignment delay come with alignment.  Then the latency line.
fn sum_block(
    ui: &mut egui::Ui,
    row: &OutputDeviceRow,
    settings: &OutputDeviceSettings,
    reaching: &[&PathInfo],
    edits: &mut Vec<MapEdit>,
) {
    block(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(micro_label(
                &format!("SUM ON {}", row.name.to_uppercase()),
                BLUE,
            ));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(mono("ALIGN · AFTER ALIGNMENT", DIM, 8.5))
                    .on_hover_text("Per-device alignment comes in a later version.");
            });
        });
        ui.add_space(6.0);
        egui::ScrollArea::horizontal()
            .id_salt(("routing_sum_scroll", row.name.as_str()))
            .auto_shrink([false, true])
            .show(ui, |ui| {
                egui::Grid::new(("routing_sum", row.name.as_str()))
                    .num_columns(5)
                    .spacing([16.0, 8.0])
                    .show(ui, |ui| {
                        for header in ["PATH", "LEVEL HERE", "POLARITY", "TRIM", "ALIGNMENT"] {
                            ui.label(micro_label(header, DIM));
                        }
                        ui.end_row();
                        for info in reaching {
                            sum_row(ui, row, settings.path_map[info.slot], info, edits);
                            ui.end_row();
                        }
                    });
            });
        ui.add_space(6.0);
        horizontal_line(ui, BORDER_SOFT);
        ui.add_space(6.0);
        ui.add(
            egui::Label::new(
                RichText::new(latency_line(&row.name, reaching))
                    .size(ts(10.0))
                    .color(MUTED),
            )
            .wrap(),
        );
    });
}

/// One path's row in the sum block.
fn sum_row(
    ui: &mut egui::Ui,
    row: &OutputDeviceRow,
    entry: PathMapSettings,
    info: &PathInfo,
    edits: &mut Vec<MapEdit>,
) {
    ui.horizontal(|ui| {
        let (dot, _) = ui.allocate_exact_size(Vec2::new(8.0, 18.0), Sense::hover());
        ui.painter().circle_filled(dot.center(), 4.0, info.colour);
        ui.label(RichText::new(&info.name).size(ts(10.5)).color(TEXT));
        ui.label(mono(format!("{:.1} ms", info.latency_ms), DIM, 8.5));
    });
    if let Some(step) = level_stepper(
        ui,
        entry.level_db,
        &format!("{} level on {}", info.name, row.name),
    ) {
        edits.push(MapEdit::Level {
            slot: info.slot,
            level_db: step.level_db,
            commit: step.commit,
        });
    }
    if path_toggle(
        ui,
        "ø",
        entry.invert,
        AMBER,
        &format!("Flip polarity of {} on {}", info.name, row.name),
        "Flip this path's polarity on this device",
    ) {
        edits.push(MapEdit::Invert {
            slot: info.slot,
            invert: !entry.invert,
        });
    }
    let _ = ui
        .add_enabled(
            false,
            egui::Button::new(mono(format!("{:+.1} ms", entry.trim_ms), DIM, 9.0))
                .fill(Color32::TRANSPARENT)
                .stroke(Stroke::new(1.0, BORDER_SOFT))
                .corner_radius(CornerRadius::same(6))
                .min_size(Vec2::new(70.0, 22.0)),
        )
        .on_disabled_hover_text("Trim (±5 ms) applies after alignment");
    ui.label(mono("after alignment", DIM, 9.0));
}

/// The latency line under the sum: what each path adds, and how far apart
/// they arrive until alignment exists.
pub(crate) fn latency_line(device: &str, reaching: &[&PathInfo]) -> String {
    let parts: Vec<String> = reaching
        .iter()
        .map(|info| format!("{} {:.1} ms", info.name, info.latency_ms))
        .collect();
    let mut text = format!(
        "Latency: {device} plays each path as its chain finishes ({}).",
        parts.join(" · ")
    );
    if reaching.len() > 1 {
        let (min, max) = reaching
            .iter()
            .fold((f32::INFINITY, 0.0_f32), |(min, max), info| {
                (min.min(info.latency_ms), max.max(info.latency_ms))
            });
        let spread = (max - min).max(0.0);
        if spread >= 0.05 {
            text.push_str(&format!(" Up to {spread:.1} ms apart: no alignment yet."));
        } else {
            text.push_str(" No alignment yet: a plugin that adds latency shifts its path.");
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::{
        channel_list, device_summary_row, latency_line, map_summary, output_cell, preset_active,
        selected_device, shows_unused_choice, shows_unused_choice_paths, MapEdit, RoutingAction,
    };
    use crate::app::cards::Card;
    use crate::app::{blank_app, Page};
    use crate::audio::{
        ChannelMap, EngineState, MapPreset, MixSide, OutputDeviceSettings, UnusedChannels,
    };
    use crate::ui::pages::mixer::chain::ChainSelection;
    use crate::ui::pages::mixer::output_devices::OutputDeviceRow;
    use crate::ui::theme::{AMBER, MUTED, RED};
    use crate::ui::widgets::{accesskit_frame, node_named};
    use eframe::egui::{self, accesskit::Toggled};

    fn with_map(left: u8, right: u8, unused: UnusedChannels) -> OutputDeviceSettings {
        OutputDeviceSettings {
            path_map: crate::audio::PathMapSettings::first(ChannelMap { left, right }),
            unused,
            ..OutputDeviceSettings::default()
        }
    }

    fn text(settings: &OutputDeviceSettings, channels: usize) -> String {
        map_summary(settings, 0, channels).text
    }

    const SILENT: UnusedChannels = UnusedChannels::Silent;
    const COPY: UnusedChannels = UnusedChannels::MonoCopy;

    #[test]
    fn the_default_map_reads_as_a_pair() {
        let default = OutputDeviceSettings::default();
        assert_eq!(text(&default, 2), "L/R");
        assert_eq!(text(&default, 4), "Out 1/2 · rest mono copy");
        assert_eq!(text(&with_map(0b01, 0b10, SILENT), 4), "Out 1/2");
        assert!(!map_summary(&default, 0, 2).nothing_routed);
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
        let empty = map_summary(&with_map(0, 0, SILENT), 0, 2);
        assert_eq!(empty.text, "nothing routed · plays silence");
        assert!(empty.nothing_routed);
        assert!(empty.silent, "the only case that warns");
        let copy = map_summary(&with_map(0, 0, COPY), 0, 4);
        assert_eq!(copy.text, "nothing routed · mono copy on all");
        assert!(copy.nothing_routed);
        assert!(!copy.silent, "mono copy still plays: no warning");
        assert!(!map_summary(&OutputDeviceSettings::default(), 0, 2).silent);
        // Bits past the device don't count.
        assert!(map_summary(&with_map(0b0100, 0b1000, SILENT), 0, 2).nothing_routed);
    }

    #[test]
    fn a_mono_device_reads_mono_whenever_anything_is_routed() {
        assert_eq!(text(&OutputDeviceSettings::default(), 1), "Out 1←mono");
        assert_eq!(text(&with_map(0, 0b1000, SILENT), 1), "Out 1←mono");
        assert!(map_summary(&with_map(0, 0, SILENT), 0, 1).nothing_routed);
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
        assert!(!shows_unused_choice(&default, 0, 2));
        assert!(shows_unused_choice(&default, 0, 4));
        assert!(shows_unused_choice(&with_map(0b01, 0, SILENT), 0, 2));
        assert!(!shows_unused_choice(&default, 0, 1));
        assert!(shows_unused_choice(&with_map(0, 0, SILENT), 0, 1));
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
                0,
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

    fn has_node(tree: &egui::accesskit::TreeUpdate, name: &str) -> bool {
        tree.nodes
            .iter()
            .any(|(_, node)| node.label() == Some(name))
    }

    #[test]
    fn one_path_keeps_the_page_plain() {
        let mut app = blank_app();
        let tree = accesskit_frame(|ui| app.routing_page(ui));
        let single = node_named(&tree, "Single path card");
        assert_eq!(single.toggled(), Some(Toggled::True));
        assert!(node_named(&tree, "Blend clean card").is_disabled());
        assert!(node_named(&tree, "Dual-amp card").is_disabled());
        assert!(!node_named(&tree, "Dry L / Wet R card").is_disabled());
        for name in [
            "Mute Main",
            "Open Main in Mixer",
            "Duplicate Main",
            "Add path",
        ] {
            let _ = node_named(&tree, name);
        }
        for name in ["Remove Main", "Main L to Out 1", "Presets for Main"] {
            assert!(!has_node(&tree, name), "{name} only with several paths");
        }
    }

    #[test]
    fn several_paths_get_a_row_pair_each_and_their_own_controls() {
        let mut app = blank_app();
        assert!(app.apply_card(Card::DryWet));
        let tree = accesskit_frame(|ui| app.routing_page(ui));
        assert_eq!(
            node_named(&tree, "Dry L / Wet R card").toggled(),
            Some(Toggled::True)
        );
        for name in [
            "Route input 1 to Main L",
            "Route input 1 to Path 2 R",
            "Path 2 R to Out 2",
            "Remove Path 2",
            "Solo Path 2",
        ] {
            let _ = node_named(&tree, name);
        }
        let on = node_named(&tree, "Main L to Out 1");
        assert_eq!(on.toggled(), Some(Toggled::True));
        let off = node_named(&tree, "Main R to Out 2");
        assert_eq!(off.toggled(), Some(Toggled::False));
        assert!(!has_node(&tree, "Mix L to Out 1"));
    }

    #[test]
    fn path_actions_apply_after_drawing() {
        let mut app = blank_app();
        app.apply_routing_action(RoutingAction::AddPath);
        assert_eq!(app.path_count(), 2);
        let second = app.path_slots()[1];
        app.apply_routing_action(RoutingAction::PathMute(second, false));
        app.apply_routing_action(RoutingAction::PathSolo(second, true));
        app.apply_routing_action(RoutingAction::PathLevel(second, -3.0));
        let info = app.path_infos()[1].clone();
        assert!(!info.mute && info.solo);
        assert_eq!(info.level_db, -3.0);
        app.apply_routing_action(RoutingAction::DuplicatePath(second));
        assert_eq!(app.path_count(), 3);

        app.page = Page::Routing;
        app.chain_selected = ChainSelection::OutputDevice(0);
        app.apply_routing_action(RoutingAction::OpenPathInMixer(0));
        assert!(app.page == Page::Mixer);
        assert_eq!(app.selected_path, 0);
        assert_eq!(app.chain_selected, ChainSelection::PreEq);

        app.apply_routing_action(RoutingAction::PresetPath(second));
        assert_eq!(app.routing_preset_path, Some(second));
        assert_eq!(
            app.selected_path, 0,
            "presets leave the shared selection alone"
        );
        app.apply_routing_action(RoutingAction::RemovePath(second));
        assert_eq!(app.path_count(), 2);
        app.apply_routing_action(RoutingAction::ApplyCard(Card::Single));
        assert_eq!(app.current_card(), Some(Card::Single));
    }

    #[test]
    fn device_rows_name_their_paths_and_warn() {
        let mut app = blank_app();
        assert!(app.apply_card(Card::DryWet));
        let infos = app.path_infos();
        let line = |app: &crate::app::RigWerkApp| {
            let rows = app.output_device_rows();
            app.device_paths_line(0, &rows[0], &infos)
        };
        // Aligned (the default): no warning.
        let aligned = line(&app);
        assert_eq!(aligned.text, "2 paths · 0.0 ms");
        assert_eq!(aligned.color, MUTED);
        assert_eq!(aligned.dots, vec![infos[0].colour, infos[1].colour]);

        // Unaligned, and the second path trimmed: they arrive apart.
        app.set_device_align(0, false);
        app.clock_device.path_map[infos[1].slot].trim_ms = 1.0;
        let shift = line(&app);
        assert_eq!(shift.text, "2 paths · 0.0 ms");
        assert_eq!(shift.color, AMBER);

        // Both on L/R: they add up.
        app.clock_device.path_map[infos[0].slot].map = ChannelMap::default();
        app.clock_device.path_map[infos[1].slot].map = ChannelMap::default();
        let sum = line(&app);
        assert_eq!(sum.text, "unaligned sum · comb filter");
        assert_eq!(sum.color, RED);
        // Aligned again: the trim still pulls the sum apart ...
        app.set_device_align(0, true);
        assert_eq!(line(&app).color, RED);
        // ... and without it the same sum lines up.
        app.clock_device.path_map[infos[1].slot].trim_ms = 0.0;
        assert_ne!(line(&app).color, RED);

        // Only the second path plays here.
        app.clock_device.path_map[infos[0].slot].map = ChannelMap { left: 0, right: 0 };
        let one = line(&app);
        assert_eq!(one.text, "Path 2 · L/R");
        assert_eq!(one.dots, vec![infos[1].colour]);
    }

    #[test]
    fn the_unused_choice_counts_every_path() {
        let mut settings = with_map(0b01, 0, SILENT);
        settings.path_map[1] = crate::audio::PathMapSettings::unrouted();
        assert!(shows_unused_choice_paths(&settings, &[0, 1], 2));
        settings.path_map[1].map = ChannelMap {
            left: 0,
            right: 0b10,
        };
        assert!(!shows_unused_choice_paths(&settings, &[0, 1], 2));
        assert!(shows_unused_choice_paths(&settings, &[0, 1], 4));
    }

    #[test]
    fn the_latency_line_says_how_far_apart_paths_arrive() {
        let mut app = blank_app();
        assert!(app.apply_card(Card::DryWet));
        let mut infos = app.path_infos();
        let one = latency_line("Scarlett", &[&infos[0]]);
        assert_eq!(
            one,
            "Latency: Scarlett plays each path as its chain finishes (Main 0.0 ms)."
        );
        infos[1].latency_ms = 1.3;
        let two = latency_line("Scarlett", &[&infos[0], &infos[1]]);
        assert!(
            two.ends_with("Up to 1.3 ms apart: no alignment yet."),
            "{two}"
        );
        assert_eq!(channel_list(&[0, 2]), "Out 1, Out 3");
    }

    #[test]
    fn sum_edits_write_their_own_slot_sanitized() {
        let mut app = blank_app();
        assert!(app.apply_card(Card::DryWet));
        let second = app.path_slots()[1];
        let before = app.clock_device.path_map[0];
        app.apply_map_edit(
            0,
            MapEdit::Level {
                slot: second,
                level_db: 40.0,
                commit: true,
            },
        );
        assert_eq!(app.clock_device.path_map[second].level_db, 6.0, "clamped");
        app.apply_map_edit(
            0,
            MapEdit::Invert {
                slot: second,
                invert: true,
            },
        );
        assert!(app.clock_device.path_map[second].invert);
        assert_eq!(app.clock_device.path_map[0], before, "other slot untouched");
        assert!(app
            .log
            .iter()
            .any(|entry| entry.text.contains("Path 2 level on")));
        assert!(app
            .log
            .iter()
            .any(|entry| entry.text.contains("Path 2 polarity on")));
    }

    #[test]
    fn a_drag_logs_only_when_it_ends() {
        let mut app = blank_app();
        assert!(app.apply_card(Card::DryWet));
        let logged = app.log.len();
        for level_db in [-1.0, -2.0, -3.0] {
            app.apply_map_edit(
                0,
                MapEdit::Level {
                    slot: 0,
                    level_db,
                    commit: false,
                },
            );
        }
        assert_eq!(app.log.len(), logged);
        assert_eq!(app.clock_device.path_map[0].level_db, -3.0);
    }

    #[test]
    fn a_matrix_edit_makes_the_routing_custom() {
        let mut app = blank_app();
        assert!(app.apply_card(Card::DryWet));
        assert_eq!(app.current_card(), Some(Card::DryWet));
        app.apply_map_edit(
            0,
            MapEdit::Cell {
                slot: 0,
                side: MixSide::R,
                channel: 1,
                on: true,
            },
        );
        assert_eq!(app.current_card(), None);
        assert_eq!(app.routing_last_card, Some(Card::DryWet));
        assert_eq!(
            super::super::paths::custom_label(app.routing_last_card),
            "Custom · edited from Dry L / Wet R"
        );
    }
}
