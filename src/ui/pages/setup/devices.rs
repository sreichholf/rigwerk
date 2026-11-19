//! Input and clock output: the device lists, rate, buffer and driver line.

use eframe::egui;
use egui::{Align2, RichText, Sense, Stroke, Vec2};

use crate::app::devices::is_wrapper_driver;
use crate::app::screen::SetupSection;
use crate::app::settings::{rate_label, BUFFERS, RATES};
use crate::app::{LogTone, RigWerkApp};
use crate::audio::{AsioPanelSide, DeviceItem, EngineState, WasapiOutputMode};
use crate::ui::fonts::Face;
use crate::ui::pages::setup::driver_line::driver_line;
use crate::ui::pages::setup::{hint, panel};
use crate::ui::theme::{font, palette, TypeRole};
use crate::ui::widgets::{palette_focus, row_info, segmented};

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
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 52.0), Sense::click());
    let fill = if selected {
        colours.accent_bg
    } else if response.hovered() {
        colours.slot
    } else {
        colours.bg
    };
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::same(8), fill);
    ui.painter().rect_stroke(
        rect,
        egui::CornerRadius::same(8),
        Stroke::new(1.0, if selected { accent } else { colours.line }),
        egui::StrokeKind::Inside,
    );
    let radio = egui::pos2(rect.left() + 15.0, rect.center().y);
    if selected {
        ui.painter().circle_filled(radio, 5.5, colours.accent_bg);
        ui.painter().circle_filled(radio, 2.6, accent);
    } else {
        ui.painter()
            .circle_stroke(radio, 5.0, Stroke::new(1.0, colours.line2));
    }
    let name_font = font(Face::Sans, TypeRole::Body);
    let meta_font = font(Face::Sans, TypeRole::Secondary);
    ui.painter().text(
        rect.left_center() + Vec2::new(28.0, -8.0),
        Align2::LEFT_CENTER,
        crate::ui::widgets::ellipsize(&label, 34),
        name_font,
        if selected {
            colours.text
        } else {
            colours.text2
        },
    );
    let meta = format!(
        "{} · {} ch{}",
        device.backend,
        device.channels,
        if device.demo { " · Demo device" } else { "" }
    );
    ui.painter().text(
        rect.left_center() + Vec2::new(28.0, 10.0),
        Align2::LEFT_CENTER,
        meta.clone(),
        meta_font,
        if device.demo {
            colours.warn
        } else {
            colours.text3
        },
    );
    if selected {
        ui.painter().text(
            rect.right_center() + Vec2::new(-11.0, 0.0),
            Align2::RIGHT_CENTER,
            "In use",
            font(Face::Sans, TypeRole::Secondary),
            accent,
        );
    }
    palette_focus(ui, &response, rect);
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

fn device_group(
    ui: &mut egui::Ui,
    heading: &str,
    devices: &[(usize, DeviceItem)],
    selected_index: usize,
    accent: egui::Color32,
) -> Option<(usize, String)> {
    if devices.is_empty() {
        return None;
    }
    let colours = palette(ui.ctx());
    ui.label(
        RichText::new(heading)
            .font(font(Face::Sans, TypeRole::Secondary))
            .color(colours.text3),
    );
    ui.add_space(4.0);
    let peers: Vec<DeviceItem> = devices.iter().map(|(_, device)| device.clone()).collect();
    let mut picked = None;
    for (index, device) in devices {
        if device_row(ui, device, *index == selected_index, accent, &peers) {
            picked = Some((*index, device.name.clone()));
        }
        ui.add_space(3.0);
    }
    picked
}

impl RigWerkApp {
    pub(super) fn setup_devices(&mut self, ui: &mut egui::Ui, section: Option<SetupSection>) {
        let colours = palette(ui.ctx());
        let accent = colours.accent;
        panel(ui, "Input", section, Some(SetupSection::Devices), |ui| {
            let low_inputs: Vec<(usize, DeviceItem)> =
                self.inventory.asio.iter().cloned().enumerate().collect();
            let system_inputs: Vec<(usize, DeviceItem)> =
                self.inventory.inputs.iter().cloned().enumerate().collect();
            let selected_system = if self.use_asio_input {
                usize::MAX
            } else {
                self.input_idx
            };
            let selected_direct = if self.use_asio_input {
                self.asio_idx
            } else {
                usize::MAX
            };
            if let Some((index, name)) =
                device_group(ui, "Direct", &low_inputs, selected_direct, accent)
            {
                self.asio_idx = index;
                self.use_asio_input = true;
                self.accept_asio_device();
                self.push_log(format!("Input set to {name}."), LogTone::Teal);
            }
            if let Some((index, name)) =
                device_group(ui, "System", &system_inputs, selected_system, accent)
            {
                self.input_idx = index;
                self.use_asio_input = false;
                self.accept_input_device();
                self.push_log(format!("Input set to {name}."), LogTone::Blue);
            }
            ui.add_space(8.0);
            self.input_rate_and_buffer(ui);
            ui.add_space(6.0);
            let input_actual = self.input_actual();
            let stopped = self.engine.state != EngineState::Running;
            let _ = driver_line(
                ui,
                input_actual.as_ref(),
                if self.engine.state == EngineState::Running {
                    "measuring…"
                } else {
                    "not running"
                },
                stopped,
                None,
            );
            ui.add_space(6.0);
            self.asio_panel_chip(ui, AsioPanelSide::Input);
        });

        panel(ui, "Clock output", section, None, |ui| {
            let low_outputs: Vec<(usize, DeviceItem)> = self
                .output_indices(true)
                .into_iter()
                .map(|index| (index, self.inventory.outputs[index].clone()))
                .collect();
            let system_outputs: Vec<(usize, DeviceItem)> = self
                .output_indices(false)
                .into_iter()
                .map(|index| (index, self.inventory.outputs[index].clone()))
                .collect();
            let selected = self.output_idx;
            if let Some((index, name)) = device_group(ui, "Direct", &low_outputs, selected, accent)
            {
                self.output_tab_low_latency = true;
                self.output_idx = index;
                self.accept_output_device();
                self.push_log(format!("Clock output set to {name}."), LogTone::Blue);
            }
            if let Some((index, name)) =
                device_group(ui, "System", &system_outputs, selected, accent)
            {
                self.output_tab_low_latency = false;
                self.output_idx = index;
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
            let stopped = self.engine.state != EngineState::Running;
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
                        .add_enabled(!busy, egui::Button::new(label))
                        .on_hover_text(
                            "Ask the sound system which rate and buffer this clock output allows. It opens no audio.",
                        )
                        .clicked()
                    {
                        self.start_output_driver_check(true);
                    }
            }
            ui.add_space(6.0);
            self.asio_panel_chip(ui, AsioPanelSide::Output);
        });

        if ui
            .button("Rescan devices")
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
        ui.horizontal(|ui| {
            hint(ui, "Rate");
            let mut rate = self.sample_rate;
            egui::ComboBox::from_id_salt("setup_input_rate")
                .selected_text(rate_label(rate))
                .show_ui(ui, |ui| {
                    for option in RATES {
                        ui.selectable_value(&mut rate, option, rate_label(option));
                    }
                });
            if rate != self.sample_rate {
                self.sample_rate = rate;
                self.push_log(
                    format!("Input rate set to {}.", rate_label(rate)),
                    LogTone::Blue,
                );
            }
            ui.add_space(8.0);
            hint(ui, "Buffer size");
            let mut buffer = self.buffer_size;
            egui::ComboBox::from_id_salt("setup_input_buffer")
                .selected_text(format!("{buffer} samples"))
                .show_ui(ui, |ui| {
                    for option in BUFFERS {
                        ui.selectable_value(&mut buffer, option, format!("{option} samples"));
                    }
                });
            if buffer != self.buffer_size {
                self.buffer_size = buffer;
                self.push_log(
                    format!("Input buffer set to {buffer} samples."),
                    LogTone::Blue,
                );
            }
        });
    }

    fn output_rate_and_buffer(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            hint(ui, "Rate");
            let mut rate = self.output_rate;
            let shown = if rate == 0 {
                "Match input".to_owned()
            } else {
                rate_label(rate)
            };
            egui::ComboBox::from_id_salt("setup_output_rate")
                .selected_text(shown)
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
                self.output_rate = rate;
                self.push_log(
                    if rate == 0 {
                        "Clock output rate matches the input.".to_owned()
                    } else {
                        format!("Clock output rate set to {}.", rate_label(rate))
                    },
                    LogTone::Blue,
                );
            }
            ui.add_space(8.0);
            hint(ui, "Buffer size");
            let mut buffer = self.output_buffer;
            egui::ComboBox::from_id_salt("setup_output_buffer")
                .selected_text(format!("{buffer} samples"))
                .show_ui(ui, |ui| {
                    for option in BUFFERS {
                        ui.selectable_value(&mut buffer, option, format!("{option} samples"));
                    }
                });
            if buffer != self.output_buffer {
                self.output_buffer = buffer;
                self.push_log(
                    format!("Clock output buffer set to {buffer} samples."),
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

/// Why output mode is off for this clock output, or none when it applies.
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
