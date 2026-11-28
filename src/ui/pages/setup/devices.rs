//! Device lists for the input and the clock output.  In use stays pinned
//! above the scroll.  A `*` in the filter is an unanchored glob.

use std::borrow::Cow;

use eframe::egui;
use egui::{Align2, CornerRadius, FontId, RichText, Sense, Stroke, Vec2};

use crate::app::devices::is_wrapper_driver;
use crate::app::screen::SetupSection;
use crate::app::settings::{rate_label, BUFFERS, RATES};
use crate::app::{LogTone, RigWerkApp};
use crate::audio::{AsioPanelSide, DeviceItem, EngineState, WasapiOutputMode};
use crate::ui::fonts::Face;
use crate::ui::pages::setup::driver_line::driver_line;
use crate::ui::pages::setup::{hint, panel};
use crate::ui::theme::{font, palette, TypeRole};
use crate::ui::widgets::{
    asio_panel_chip, fit_text, palette_focus, row_info, segmented, TextField,
};

/// Allocated height of one row, including the gap under it.
pub(crate) const DEVICE_ROW_H: f32 = 30.0;
const DEVICE_ROW_GAP: f32 = 2.0;
/// Direct / System heading plus the gap under it, inside the scroll.
const DEVICE_HEADING_H: f32 = 18.0;
pub(crate) const DEVICE_SCROLL_AFTER: usize = 5;

/// Fewer than [`DEVICE_SCROLL_AFTER`] rows are not scrolled.  `direct` and
/// `system` add room for the headings so five device rows stay visible.
pub(crate) fn device_list_scroll_height(rows: usize, direct: bool, system: bool) -> Option<f32> {
    if rows <= DEVICE_SCROLL_AFTER {
        return None;
    }
    let mut height = DEVICE_ROW_H * DEVICE_SCROLL_AFTER as f32;
    if direct {
        height += DEVICE_HEADING_H;
    }
    if system {
        height += DEVICE_HEADING_H;
    }
    if direct && system {
        height += 4.0;
    }
    Some(height)
}

/// A blank filter matches all.  With no `*`, the filter is a substring;
/// with `*`, it is an unanchored glob (`gp*5` matches `Valeton GP-5 USB`).
pub(crate) fn device_matches_filter(label: &str, backend: &str, filter: &str) -> bool {
    let filter = filter.trim();
    if filter.is_empty() {
        return true;
    }
    let filter = filter.to_lowercase();
    let label = label.to_lowercase();
    let backend = backend.to_lowercase();
    text_matches(&label, &filter) || text_matches(&backend, &filter)
}

fn text_matches(haystack: &str, filter: &str) -> bool {
    if !filter.contains('*') {
        return haystack.contains(filter);
    }
    // Unanchored: `gp*5` matches inside a longer name the same way a
    // substring would.  Leading or trailing `*` from the user stay as-is.
    let pattern = if filter.starts_with('*') {
        filter.to_owned()
    } else {
        format!("*{filter}")
    };
    let pattern = if pattern.ends_with('*') {
        pattern
    } else {
        format!("{pattern}*")
    };
    glob_match(haystack, &pattern)
}

fn glob_match(haystack: &str, pattern: &str) -> bool {
    match pattern.split_once('*') {
        None => haystack == pattern,
        Some(("", rest)) => {
            if rest.is_empty() {
                true
            } else {
                (0..=haystack.len())
                    .filter(|&index| haystack.is_char_boundary(index))
                    .any(|index| glob_match(&haystack[index..], rest))
            }
        }
        Some((head, rest)) => {
            haystack.starts_with(head) && glob_match(&haystack[head.len()..], &format!("*{rest}"))
        }
    }
}

fn device_meta(device: &DeviceItem) -> String {
    format!(
        "{} · {} ch{}",
        device.backend,
        device.channels,
        if device.demo { " · Demo device" } else { "" }
    )
}

fn measure(ui: &egui::Ui, text: &str, font_id: &FontId) -> f32 {
    ui.painter()
        .layout_no_wrap(text.to_owned(), font_id.clone(), palette(ui.ctx()).text)
        .size()
        .x
}

pub(crate) fn device_row(
    ui: &mut egui::Ui,
    device: &DeviceItem,
    selected: bool,
    accent: egui::Color32,
    peers: &[DeviceItem],
) -> bool {
    let colours = palette(ui.ctx());
    let label = crate::audio::device_choice_label(device, peers);
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, DEVICE_ROW_H), Sense::click());
    let card = egui::Rect::from_min_size(
        rect.min,
        Vec2::new(rect.width(), rect.height() - DEVICE_ROW_GAP),
    );
    let fill = if selected {
        colours.accent_bg
    } else if response.hovered() {
        colours.slot
    } else {
        colours.bg
    };
    let radius = CornerRadius::same(5);
    ui.painter().rect_filled(card, radius, fill);
    ui.painter().rect_stroke(
        card,
        radius,
        Stroke::new(1.0, if selected { accent } else { colours.line }),
        egui::StrokeKind::Inside,
    );
    let radio = egui::pos2(card.left() + 12.0, card.center().y);
    if selected {
        ui.painter().circle_filled(radio, 5.0, colours.accent_bg);
        ui.painter().circle_filled(radio, 2.4, accent);
    } else {
        ui.painter()
            .circle_stroke(radio, 4.5, Stroke::new(1.0, colours.line2));
    }

    let name_font = font(Face::Sans, TypeRole::Control);
    let meta_font = font(Face::Sans, TypeRole::Secondary);
    let meta = device_meta(device);
    let in_use = "In use";
    let in_use_w = if selected {
        measure(ui, in_use, &meta_font) + 8.0
    } else {
        0.0
    };
    let left = card.left() + 24.0;
    let right = card.right() - 8.0 - in_use_w;
    let meta_w = measure(ui, &meta, &meta_font);
    let name_budget = (right - left - meta_w - 8.0).max(24.0);
    let name_shown = fit_text(ui, &label, &name_font, name_budget);
    let name_w = measure(ui, &name_shown, &name_font);
    ui.painter().text(
        egui::pos2(left, card.center().y),
        Align2::LEFT_CENTER,
        name_shown,
        name_font,
        if selected {
            colours.text
        } else {
            colours.text2
        },
    );
    let meta_left = (left + name_w + 8.0).min(right - 4.0);
    let meta_budget = (right - meta_left).max(0.0);
    let meta_shown = fit_text(ui, &meta, &meta_font, meta_budget);
    ui.painter().text(
        egui::pos2(meta_left, card.center().y),
        Align2::LEFT_CENTER,
        meta_shown,
        meta_font.clone(),
        if device.demo {
            colours.warn
        } else {
            colours.text3
        },
    );
    if selected {
        ui.painter().text(
            egui::pos2(card.right() - 8.0, card.center().y),
            Align2::RIGHT_CENTER,
            in_use,
            meta_font,
            accent,
        );
    }
    palette_focus(ui, &response, card);
    row_info(
        &response,
        egui::Role::RadioButton,
        ui.is_enabled(),
        label,
        Some(selected),
        Some(meta),
    );
    response.clicked()
}

fn device_search(ui: &mut egui::Ui, filter: &mut String, id_salt: &str) {
    let id = ui.make_persistent_id(id_salt);
    TextField::new(filter, "Search")
        .hint("Search")
        .id(id)
        .show(ui);
}

fn device_group_heading(ui: &mut egui::Ui, heading: &str) {
    let colours = palette(ui.ctx());
    ui.label(
        RichText::new(heading)
            .font(font(Face::Sans, TypeRole::Secondary))
            .color(colours.text3),
    );
    ui.add_space(2.0);
}

/// The peer list `device_choice_label` compares against.  An inventory list
/// is its own peer list, so only a tab's subset is copied.
struct DeviceGroup<'a> {
    rows: Vec<(usize, &'a DeviceItem)>,
    peers: Cow<'a, [DeviceItem]>,
}

impl<'a> DeviceGroup<'a> {
    /// A whole inventory list: nothing is copied.
    fn all(devices: &'a [DeviceItem]) -> Self {
        Self {
            rows: devices.iter().enumerate().collect(),
            peers: Cow::Borrowed(devices),
        }
    }

    /// The rows stay borrowed.  The peer list is the subset, so it
    /// is copied once.
    fn subset(devices: &'a [DeviceItem], indices: &[usize]) -> Self {
        Self {
            rows: indices
                .iter()
                .map(|&index| (index, &devices[index]))
                .collect(),
            peers: Cow::Owned(
                indices
                    .iter()
                    .map(|&index| devices[index].clone())
                    .collect(),
            ),
        }
    }
}

/// A blank filter passes every row through before any label is built, so
/// the common case allocates nothing per device.
pub(crate) fn filter_group<'a>(
    devices: &'a [(usize, &'a DeviceItem)],
    peers: &[DeviceItem],
    skip_index: usize,
    filter: &str,
) -> Vec<(usize, &'a DeviceItem)> {
    let filter = filter.trim();
    let mut shown = Vec::with_capacity(devices.len());
    for (index, device) in devices {
        if *index == skip_index {
            continue;
        }
        if !filter.is_empty()
            && !device_matches_filter(
                &crate::audio::device_choice_label(device, peers),
                &device.backend,
                filter,
            )
        {
            continue;
        }
        shown.push((*index, *device));
    }
    shown
}

fn device_lists(
    ui: &mut egui::Ui,
    filter: &mut String,
    id_prefix: &str,
    low: &DeviceGroup<'_>,
    system: &DeviceGroup<'_>,
    selected: (usize, usize),
    accent: egui::Color32,
) -> Option<(usize, String, bool)> {
    let (selected_direct, selected_system) = selected;
    device_search(ui, filter, &format!("{id_prefix}_search"));
    ui.add_space(4.0);

    let pinned = low
        .rows
        .iter()
        .find(|(index, _)| *index == selected_direct)
        .map(|(index, device)| (*index, device, true, low.peers.as_ref()))
        .or_else(|| {
            system
                .rows
                .iter()
                .find(|(index, _)| *index == selected_system)
                .map(|(index, device)| (*index, device, false, system.peers.as_ref()))
        });

    let mut picked = None;
    if let Some((index, device, from_direct, peers)) = pinned {
        if device_row(ui, device, true, accent, peers) {
            picked = Some((index, device.name.clone(), from_direct));
        }
        ui.add_space(2.0);
    }

    let low_shown = filter_group(&low.rows, &low.peers, selected_direct, filter);
    let system_shown = filter_group(&system.rows, &system.peers, selected_system, filter);
    let scroll_rows = low_shown.len() + system_shown.len();
    let has_direct = !low_shown.is_empty();
    let has_system = !system_shown.is_empty();

    let mut paint_groups = |ui: &mut egui::Ui| {
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            if has_direct {
                device_group_heading(ui, "Direct");
                for (index, device) in &low_shown {
                    if device_row(ui, device, false, accent, &low.peers) {
                        picked = Some((*index, device.name.clone(), true));
                    }
                }
                if has_system {
                    ui.add_space(4.0);
                }
            }
            if has_system {
                device_group_heading(ui, "System");
                for (index, device) in &system_shown {
                    if device_row(ui, device, false, accent, &system.peers) {
                        picked = Some((*index, device.name.clone(), false));
                    }
                }
            }
        });
    };

    if let Some(height) = device_list_scroll_height(scroll_rows, has_direct, has_system) {
        // Force the viewport: Setup sits in the page ScrollArea, and a nested
        // max_height alone collapses to the leftover strip (~1.5 rows).
        let width = ui.available_width();
        ui.allocate_ui_with_layout(
            Vec2::new(width, height),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.set_width(width);
                ui.set_min_height(height);
                egui::ScrollArea::vertical()
                    .id_salt(format!("{id_prefix}_devices"))
                    .max_height(height)
                    .auto_shrink([false, false])
                    .show(ui, |ui| paint_groups(ui));
            },
        );
    } else {
        paint_groups(ui);
    }
    picked
}

impl RigWerkApp {
    pub(super) fn setup_devices(&mut self, ui: &mut egui::Ui, section: Option<SetupSection>) {
        let colours = palette(ui.ctx());
        let accent = colours.accent;
        panel(ui, "Input", section, Some(SetupSection::Devices), |ui| {
            let low_inputs = DeviceGroup::all(&self.devices.inventory.asio);
            let system_inputs = DeviceGroup::all(&self.devices.inventory.inputs);
            let selected_system = if self.devices.use_asio_input {
                usize::MAX
            } else {
                self.devices.input_idx
            };
            let selected_direct = if self.devices.use_asio_input {
                self.devices.asio_idx
            } else {
                usize::MAX
            };
            let mut filter = std::mem::take(&mut self.chrome.input_device_filter);
            let picked = device_lists(
                ui,
                &mut filter,
                "setup_input",
                &low_inputs,
                &system_inputs,
                (selected_direct, selected_system),
                accent,
            );
            self.chrome.input_device_filter = filter;
            if let Some((index, name, from_direct)) = picked {
                if from_direct {
                    self.devices.asio_idx = index;
                    self.devices.use_asio_input = true;
                    self.accept_asio_device();
                    self.push_log(format!("Input set to {name}."), LogTone::Teal);
                } else {
                    self.devices.input_idx = index;
                    self.devices.use_asio_input = false;
                    self.accept_input_device();
                    self.push_log(format!("Input set to {name}."), LogTone::Blue);
                }
            }
            ui.add_space(8.0);
            self.input_rate_and_buffer(ui);
            ui.add_space(6.0);
            let input_actual = self.input_actual();
            let stopped = self.engine_state() != EngineState::Running;
            let _ = driver_line(
                ui,
                input_actual.as_ref(),
                if self.engine_state() == EngineState::Running {
                    "measuring…"
                } else {
                    "not running"
                },
                stopped,
                None,
            );
            ui.add_space(6.0);
            let side = AsioPanelSide::Input;
            if let Some(ready) = self.driver_panel(side) {
                if asio_panel_chip(
                    ui,
                    ready,
                    "Driver panel",
                    false,
                    "The driver panel is available once the bridge is running on that ASIO driver.",
                    "Open this ASIO driver's control panel.",
                ) {
                    self.request_asio_panel(side);
                }
            }
        });

        panel(ui, "Clock output", section, None, |ui| {
            let low_outputs =
                DeviceGroup::subset(&self.devices.inventory.outputs, &self.output_indices(true));
            let system_outputs =
                DeviceGroup::subset(&self.devices.inventory.outputs, &self.output_indices(false));
            let selected = self.devices.output_idx;
            let mut filter = std::mem::take(&mut self.chrome.output_device_filter);
            let picked = device_lists(
                ui,
                &mut filter,
                "setup_output",
                &low_outputs,
                &system_outputs,
                (selected, selected),
                accent,
            );
            self.chrome.output_device_filter = filter;
            if let Some((index, name, from_direct)) = picked {
                self.devices.output_tab_low_latency = from_direct;
                self.devices.output_idx = index;
                self.accept_output_device();
                self.push_log(format!("Clock output set to {name}."), LogTone::Blue);
            }
            if is_wrapper_driver(self.active_input())
                && !self.active_input().demo
                && !self.is_low_latency_output(self.selected_output())
            {
                ui.add_space(6.0);
                ui.label(
                        RichText::new("ASIO4ALL in use: pick a clock output on other hardware")
                            .font(font(Face::Sans, TypeRole::Secondary))
                            .color(colours.warn),
                    )
                    .on_hover_text(
                        "ASIO4ALL takes the sound cards it drives for itself. If it drives this card too, Windows refuses the output as in use. Choose a clock output on different hardware, or untick this card in the ASIO4ALL driver panel.",
                    );
            }
            ui.add_space(8.0);
            self.output_rate_and_buffer(ui);
            ui.add_space(6.0);
            self.output_mode_control(ui);
            ui.add_space(6.0);
            let output_actual = self.output_actual();
            let stopped = self.engine_state() != EngineState::Running;
            let checking = self
                .output_driver_checkable()
                .then(|| self.driver_probe_job.is_some());
            if driver_line(
                ui,
                output_actual.as_ref(),
                if stopped {
                    "no reading yet"
                } else {
                    "measuring…"
                },
                stopped,
                checking,
            ) {
                self.start_output_driver_check(true);
            }
            if self.output_driver_checkable() && self.output_actual().is_none() {
                ui.add_space(6.0);
                let busy = self.driver_probe_job.is_some();
                let label = if busy { "Checking…" } else { "Check driver" };
                if ui
                        .add_enabled(
                            !busy,
                            egui::Button::new(
                                RichText::new(label)
                                    .font(font(Face::Sans, TypeRole::Control))
                                    .color(colours.text),
                            ),
                        )
                        .on_hover_text(
                            "Ask the sound system which rate and buffer this clock output allows. It opens no audio.",
                        )
                        .clicked()
                    {
                        self.start_output_driver_check(true);
                    }
            }
            ui.add_space(6.0);
            let side = AsioPanelSide::Output;
            if let Some(ready) = self.driver_panel(side) {
                if asio_panel_chip(
                    ui,
                    ready,
                    "Driver panel",
                    false,
                    "The driver panel is available once the bridge is running on that ASIO driver.",
                    "Open this ASIO driver's control panel.",
                ) {
                    self.request_asio_panel(side);
                }
            }
        });

        if ui
            .button(
                RichText::new("Rescan devices")
                    .font(font(Face::Sans, TypeRole::Control))
                    .color(colours.text),
            )
            .on_hover_text("Look for inputs and outputs again")
            .clicked()
        {
            self.rescan();
        }
        if let Some(line) = self.graph_buffer_line() {
            ui.add_space(8.0);
            hint(ui, &line);
        }
        ui.add_space(8.0);
        hint(
            ui,
            if cfg!(target_os = "windows") {
                "Direct is ASIO. System is WASAPI, DirectSound and MME, and every device stays selectable. Whether one interface can play while its ASIO driver holds it depends on that driver. RigWerk tries the pairing and shows the driver's error."
            } else {
                "Direct is PipeWire and ALSA hardware (hw:). System is PulseAudio and the desktop default. Low latency asks a PipeWire clock output for a 64-sample buffer. A failed settings read does not by itself stop the bridge."
            },
        );
    }

    fn input_rate_and_buffer(&mut self, ui: &mut egui::Ui) {
        let colours = palette(ui.ctx());
        ui.horizontal(|ui| {
            hint(ui, "Rate");
            let mut rate = self.sample_rate;
            egui::ComboBox::from_id_salt("setup_input_rate")
                .selected_text(
                    RichText::new(rate_label(rate))
                        .font(font(Face::Sans, TypeRole::Control))
                        .color(colours.text),
                )
                .show_ui(ui, |ui| {
                    for option in RATES {
                        ui.selectable_value(&mut rate, option, rate_label(option));
                    }
                });
            if rate != self.sample_rate {
                self.set_sample_rate(rate);
                self.push_log(
                    format!("Input rate set to {}.", rate_label(self.sample_rate)),
                    LogTone::Blue,
                );
            }
            ui.add_space(8.0);
            hint(ui, "Buffer size");
            let mut buffer = self.buffer_size;
            egui::ComboBox::from_id_salt("setup_input_buffer")
                .selected_text(
                    RichText::new(format!("{buffer} samples"))
                        .font(font(Face::Sans, TypeRole::Control))
                        .color(colours.text),
                )
                .show_ui(ui, |ui| {
                    for option in BUFFERS {
                        ui.selectable_value(&mut buffer, option, format!("{option} samples"));
                    }
                });
            if buffer != self.buffer_size {
                self.set_buffer_size(buffer);
                self.push_log(
                    format!("Input buffer set to {} samples.", self.buffer_size),
                    LogTone::Blue,
                );
            }
        });
    }

    fn output_rate_and_buffer(&mut self, ui: &mut egui::Ui) {
        let colours = palette(ui.ctx());
        ui.horizontal(|ui| {
            hint(ui, "Rate");
            let mut rate = self.output_rate;
            let shown = if rate == 0 {
                "Match input".to_owned()
            } else {
                rate_label(rate)
            };
            egui::ComboBox::from_id_salt("setup_output_rate")
                .selected_text(
                    RichText::new(shown)
                        .font(font(Face::Sans, TypeRole::Control))
                        .color(colours.text),
                )
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut rate, 0, "Match input");
                    for option in RATES {
                        ui.selectable_value(&mut rate, option, rate_label(option));
                    }
                })
                .response
                .on_hover_text(
                    "Clock output rate. Match input follows the input. A different rate is converted.",
                );
            if rate != self.output_rate {
                self.set_output_rate(rate);
                self.push_log(
                    if self.output_rate == 0 {
                        "Clock output rate matches the input.".to_owned()
                    } else {
                        format!(
                            "Clock output rate set to {}.",
                            rate_label(self.output_rate)
                        )
                    },
                    LogTone::Blue,
                );
            }
            ui.add_space(8.0);
            hint(ui, "Buffer size");
            let mut buffer = self.output_buffer;
            egui::ComboBox::from_id_salt("setup_output_buffer")
                .selected_text(
                    RichText::new(format!("{buffer} samples"))
                        .font(font(Face::Sans, TypeRole::Control))
                        .color(colours.text),
                )
                .show_ui(ui, |ui| {
                    for option in BUFFERS {
                        ui.selectable_value(&mut buffer, option, format!("{option} samples"));
                    }
                });
            if buffer != self.output_buffer {
                self.set_output_buffer(buffer);
                self.push_log(
                    format!("Clock output buffer set to {} samples.", self.output_buffer),
                    LogTone::Blue,
                );
            }
        });
    }

    fn output_mode_control(&mut self, ui: &mut egui::Ui) {
        let reason = output_mode_reason(self.selected_output());
        hint(ui, "Output mode");
        ui.add_space(4.0);
        if cfg!(target_os = "windows") {
            let options = ["Shared", "Shared, smallest buffer", "Exclusive"];
            let mut index = match self.wasapi_output_mode {
                WasapiOutputMode::Shared => 0,
                WasapiOutputMode::RealStyle => 1,
                WasapiOutputMode::Exclusive => 2,
            };
            let response = ui
                .add_enabled_ui(reason.is_none(), |ui| segmented(ui, &mut index, &options))
                .inner;
            if reason.is_none() && response.changed() {
                let mode = match index {
                    0 => WasapiOutputMode::Shared,
                    1 => WasapiOutputMode::RealStyle,
                    _ => WasapiOutputMode::Exclusive,
                };
                self.set_wasapi_output_mode(mode);
                self.push_log(format!("Output mode: {}.", options[index]), LogTone::Amber);
            }
        } else {
            let options = ["Standard", "Low latency"];
            let mut index = usize::from(self.low_latency_output);
            let response = ui
                .add_enabled_ui(reason.is_none(), |ui| segmented(ui, &mut index, &options))
                .inner;
            if reason.is_none() && response.changed() {
                self.set_low_latency_output(index == 1);
            }
        }
        if let Some(reason) = reason {
            hint(ui, reason);
        }
    }
}

pub(super) fn output_mode_reason(device: &DeviceItem) -> Option<&'static str> {
    if device.asio_clsid.is_some() {
        return Some("ASIO uses the driver's own buffer. Output mode does not apply.");
    }
    if device.backend.eq_ignore_ascii_case("DirectSound") {
        return Some("DirectSound has no output mode.");
    }
    if device.backend.eq_ignore_ascii_case("MME") {
        return Some("MME has no output mode.");
    }
    None
}

#[cfg(test)]
mod tests;
