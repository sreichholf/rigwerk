//! Devices: every input and output the scan found.

use eframe::egui;
use egui::{
    Align, Align2, Color32, CornerRadius, FontFamily, FontId, Layout, Margin, RichText, Sense,
    Stroke, Vec2,
};

use crate::app::RigWerkApp;
use crate::audio::DeviceItem;
use crate::ui::theme::{
    with_alpha, AMBER, BLUE, BORDER, BORDER_SOFT, CARD, CARD_DEEP, DIM, MUTED, TEAL, TEXT,
};
use crate::ui::widgets::{
    card, ellipsize, ghost_button, horizontal_line, micro_label, pill, status_dot,
};

pub(crate) fn device_row(
    ui: &mut egui::Ui,
    device: &DeviceItem,
    selected: bool,
    accent: Color32,
) -> bool {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 52.0), Sense::click());
    let fill = if selected {
        with_alpha(accent, 15)
    } else if response.hovered() {
        with_alpha(TEXT, 6)
    } else {
        CARD_DEEP
    };
    ui.painter().rect_filled(rect, CornerRadius::same(8), fill);
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(8),
        Stroke::new(
            1.0,
            if selected {
                with_alpha(accent, 65)
            } else {
                BORDER_SOFT
            },
        ),
        egui::StrokeKind::Inside,
    );
    let radio = egui::pos2(rect.left() + 15.0, rect.center().y);
    if selected {
        ui.painter()
            .circle_filled(radio, 5.5, with_alpha(accent, 35));
        ui.painter().circle_filled(radio, 2.6, accent);
    } else {
        ui.painter()
            .circle_stroke(radio, 5.0, Stroke::new(1.0, Color32::from_rgb(52, 64, 81)));
    }
    ui.painter().text(
        rect.left_center() + Vec2::new(28.0, -5.0),
        Align2::LEFT_CENTER,
        ellipsize(&device.name, 34),
        FontId::new(10.8, FontFamily::Proportional),
        if selected { TEXT } else { MUTED },
    );
    ui.painter().text(
        rect.left_center() + Vec2::new(28.0, 9.0),
        Align2::LEFT_CENTER,
        format!(
            "{} · {} CH{}",
            device.backend,
            device.channels,
            if device.demo { " · preview" } else { "" }
        ),
        FontId::new(8.3, FontFamily::Proportional),
        if device.demo { AMBER } else { DIM },
    );
    if selected {
        ui.painter().text(
            rect.right_center() + Vec2::new(-11.0, 0.0),
            Align2::RIGHT_CENTER,
            "IN USE",
            FontId::new(7.5, FontFamily::Monospace),
            accent,
        );
    }
    response.clicked()
}

/// One device group, drawn the same way on both sides of the page: a small
/// heading followed by the rows and a little breathing room before the next
/// group.  Returns the clicked device's list index and name.
fn device_group(
    ui: &mut egui::Ui,
    heading: &str,
    heading_color: Color32,
    devices: &[(usize, DeviceItem)],
    selected_index: usize,
    accent: Color32,
) -> Option<(usize, String)> {
    if devices.is_empty() {
        return None;
    }
    ui.label(micro_label(heading, heading_color));
    ui.add_space(4.0);
    let mut picked = None;
    for (index, device) in devices {
        if device_row(ui, device, *index == selected_index, accent) {
            picked = Some((*index, device.name.clone()));
        }
        ui.add_space(3.0);
    }
    ui.add_space(5.0);
    picked
}

impl RigWerkApp {
    pub(crate) fn devices_page(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!(
                    "{} audio endpoint{} found",
                    self.inventory.asio.len()
                        + self.inventory.inputs.len()
                        + self.inventory.outputs.len(),
                    if self.inventory.asio.len()
                        + self.inventory.inputs.len()
                        + self.inventory.outputs.len()
                        == 1
                    {
                        ""
                    } else {
                        "s"
                    }
                ))
                .size(10.5)
                .color(DIM),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ghost_button(ui, "Rescan devices", 128.0) {
                    self.rescan();
                }
            });
        });
        ui.add_space(8.0);

        let total = ui.available_width();
        let low_label = self.inventory.low_latency_label.clone();
        let input_backend = self.active_input().backend.clone();
        let output_backend = self.selected_output().backend.clone();
        let column_w = (total - 8.0) / 2.0;

        let low_inputs: Vec<(usize, DeviceItem)> =
            self.inventory.asio.iter().cloned().enumerate().collect();
        let system_inputs: Vec<(usize, DeviceItem)> =
            self.inventory.inputs.iter().cloned().enumerate().collect();
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
        let selected_system_input = if self.use_asio_input {
            usize::MAX
        } else {
            self.input_idx
        };

        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                Vec2::new(column_w, 430.0),
                Layout::top_down(Align::LEFT),
                |ui| {
                    ui.set_min_size(Vec2::new(column_w, 430.0));
                    card(ui, CARD, BORDER, |ui| {
                        ui.set_width(column_w - 32.0);
                        ui.horizontal(|ui| {
                            ui.label(micro_label("AUDIO INPUT", TEAL));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                pill(ui, &input_backend, TEAL);
                            });
                        });
                        ui.add_space(7.0);
                        ui.label(
                            RichText::new("Capture on its own clock, independent of playback.")
                                .size(10.0)
                                .color(MUTED),
                        );
                        ui.add_space(8.0);
                        if let Some((index, name)) = device_group(
                            ui,
                            &format!("{} INPUTS", low_label.to_uppercase()),
                            TEAL,
                            &low_inputs,
                            self.asio_idx,
                            TEAL,
                        ) {
                            self.asio_idx = index;
                            self.use_asio_input = true;
                            self.push_log(format!("Input set to {name}."), TEAL);
                        }
                        if let Some((index, name)) = device_group(
                            ui,
                            "SYSTEM INPUTS",
                            DIM,
                            &system_inputs,
                            selected_system_input,
                            BLUE,
                        ) {
                            self.input_idx = index;
                            self.use_asio_input = false;
                            self.push_log(format!("Input set to {name} (system input)."), BLUE);
                        }
                        ui.add_space(5.0);
                        horizontal_line(ui, BORDER_SOFT);
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            status_dot(ui, TEAL);
                            ui.label(
                                RichText::new(
                                    "Input opens independently; the driver reports if it cannot.",
                                )
                                .size(9.5)
                                .color(MUTED),
                            );
                        });
                    });
                },
            );
            ui.allocate_ui_with_layout(
                Vec2::new(column_w, 430.0),
                Layout::top_down(Align::LEFT),
                |ui| {
                    ui.set_min_size(Vec2::new(column_w, 430.0));
                    card(ui, CARD, BORDER, |ui| {
                        ui.set_width(column_w - 32.0);
                        ui.horizontal(|ui| {
                            ui.label(micro_label("AUDIO OUTPUT", BLUE));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                pill(ui, &output_backend, BLUE);
                            });
                        });
                        ui.add_space(7.0);
                        ui.label(
                            RichText::new("Playback on its own clock, independent of capture.")
                                .size(10.0)
                                .color(MUTED),
                        );
                        ui.add_space(8.0);
                        if let Some((index, name)) = device_group(
                            ui,
                            &format!("{} OUTPUTS", low_label.to_uppercase()),
                            BLUE,
                            &low_outputs,
                            self.output_idx,
                            BLUE,
                        ) {
                            self.output_idx = index;
                            self.push_log(format!("Output set to {name}."), BLUE);
                        }
                        if let Some((index, name)) = device_group(
                            ui,
                            "SYSTEM OUTPUTS",
                            DIM,
                            &system_outputs,
                            self.output_idx,
                            BLUE,
                        ) {
                            self.output_idx = index;
                            self.push_log(format!("Output set to {name}."), BLUE);
                        }
                        ui.add_space(5.0);
                        horizontal_line(ui, BORDER_SOFT);
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            status_dot(ui, BLUE);
                            ui.label(
                                RichText::new(
                                    "Output opens independently; the driver reports if it cannot.",
                                )
                                .size(9.5)
                                .color(MUTED),
                            );
                        });
                    });
                },
            );
        });

        ui.add_space(12.0);
        egui::Frame::new()
            .fill(with_alpha(AMBER, 12))
            .stroke(Stroke::new(1.0, with_alpha(AMBER, 45)))
            .corner_radius(CornerRadius::same(11))
            .inner_margin(Margin::same(12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(micro_label("DRIVER NOTE", AMBER));
                    ui.label(
                        RichText::new(if cfg!(target_os = "windows") {
                            "Inputs and outputs are grouped by driver type: ASIO on the low-latency side, WASAPI, DirectSound and MME on the system side, and every device stays selectable. Same-interface playback is up to the driver: most vendor ASIO drivers allow it, ASIO4ALL-style wrappers usually do not. RigWerk tries the pairing and shows the driver's error instead of blocking it."
                        } else {
                            "Inputs and outputs are grouped by driver type: direct ALSA hardware PCMs (hw: or plughw:) sit on the low-latency side, while the default, PipeWire and PulseAudio devices stay on the system side. Direct hardware can take the card exclusively; the virtual device shares it with every other app. RigWerk tries the pairing and shows the driver's error instead of blocking it."
                        })
                        .size(10.0)
                        .color(MUTED),
                    );
                });
            });
    }
}
