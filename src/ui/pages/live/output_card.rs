//! The output card: device, REAL-style, output clock, level and meters.

use eframe::egui;
use egui::{Align, Color32, CornerRadius, Layout, Margin, RichText, Stroke};

use crate::app::devices::is_wrapper_driver;
use crate::app::settings::{rate_label, BUFFERS, OUTPUT_GAIN_MAX_DB, OUTPUT_GAIN_MIN_DB, RATES};
use crate::app::RigWerkApp;
use crate::audio::{DeviceItem, EngineState};
use crate::ui::pages::live::driver_line::driver_line;
use crate::ui::theme::ts;
use crate::ui::theme::{
    with_alpha, AMBER, BLUE, BORDER, BORDER_SOFT, CARD_DEEP, DIM, GREEN, MUTED, RED, TEXT,
};
use crate::ui::widgets::{
    channel_chip, db_slider, device_selector, ellipsize, meter_row, micro_label, mono, source_tab,
};

impl RigWerkApp {
    pub(crate) fn output_card(&mut self, ui: &mut egui::Ui) {
        let low_label = self.inventory.low_latency_label.clone();
        let quantum_problem = self.quantum_problem();
        // Keep the tab on the selected output's side, unless the person has
        // switched to a side that has no outputs at all.
        let selected_low = self.is_low_latency_output(self.selected_output());
        let low_available = !self.output_indices(true).is_empty();
        let system_available = !self.output_indices(false).is_empty();
        if selected_low != self.output_tab_low_latency
            && !self.output_indices(self.output_tab_low_latency).is_empty()
        {
            self.output_tab_low_latency = selected_low;
        }
        if self.output_indices(self.output_tab_low_latency).is_empty()
            && !self.output_indices(!self.output_tab_low_latency).is_empty()
        {
            self.output_tab_low_latency = !self.output_tab_low_latency;
        }
        let mut use_low = self.output_tab_low_latency;
        ui.horizontal(|ui| {
            ui.label(micro_label("OUTPUT", BLUE));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if source_tab(ui, "SYS", !use_low, BLUE) && system_available {
                    use_low = false;
                }
                if source_tab(ui, &low_label, use_low, BLUE) && low_available {
                    use_low = true;
                }
            });
        });
        if use_low != self.output_tab_low_latency {
            self.output_tab_low_latency = use_low;
            if let Some(first) = self.output_indices(use_low).first() {
                self.output_idx = *first;
                self.accept_output_device();
            }
        }
        ui.add_space(7.0);

        let indices = self.output_indices(self.output_tab_low_latency);
        if indices.is_empty() {
            egui::Frame::new()
                .fill(CARD_DEEP)
                .stroke(Stroke::new(1.0, BORDER_SOFT))
                .corner_radius(CornerRadius::same(8))
                .inner_margin(Margin::symmetric(10, 8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(
                        RichText::new(format!(
                            "No {low_label} outputs found. The bridge keeps using {}.",
                            self.selected_output().name
                        ))
                        .size(ts(10.0))
                        .color(MUTED),
                    );
                });
        } else {
            let devices: Vec<DeviceItem> = indices
                .iter()
                .map(|&index| self.inventory.outputs[index].clone())
                .collect();
            let mut position = indices
                .iter()
                .position(|&index| index == self.output_idx)
                .unwrap_or(0);
            if device_selector(ui, "system_output", &devices, &mut position, BLUE) {
                self.accept_output_device();
            }
            self.output_idx = indices[position];
        }

        // A PipeWire quantum that does not match the ALSA period, or one that
        // is too large to count as low latency, is shown right at the output
        // that would suffer from it.
        if let Some(problem) = &quantum_problem {
            ui.label(
                RichText::new(ellipsize(problem, 78))
                    .size(ts(8.6))
                    .color(AMBER),
            )
            .on_hover_text(problem.clone());
        }

        // Measured: ASIO4ALL takes the cards it drives for itself, so the same
        // card cannot also be opened as a system output.  Say so up front.
        if is_wrapper_driver(self.active_input())
            && !self.active_input().demo
            && !self.is_low_latency_output(self.selected_output())
        {
            ui.label(
                RichText::new("ASIO4ALL in use: pick an output on other hardware")
                    .size(ts(8.6))
                    .color(AMBER),
            )
            .on_hover_text(
                "ASIO4ALL takes the sound cards it drives for itself. If it drives this card too, Windows refuses the output as \"in use\". Choose an output on different hardware, or untick this card in the ASIO4ALL panel.",
            );
        }
        ui.add_space(2.0);

        ui.horizontal(|ui| {
            channel_chip(ui, "OUT 1", &mut self.output_channels[0], BLUE);
            channel_chip(ui, "OUT 2", &mut self.output_channels[1], BLUE);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new(if self.mute { "muted" } else { "mute" })
                                .size(ts(9.0))
                                .color(if self.mute { RED } else { GREEN }),
                        )
                        .fill(if self.mute {
                            with_alpha(RED, 24)
                        } else {
                            with_alpha(GREEN, 18)
                        })
                        .stroke(Stroke::new(
                            1.0,
                            if self.mute {
                                with_alpha(RED, 60)
                            } else {
                                with_alpha(GREEN, 40)
                            },
                        ))
                        .corner_radius(CornerRadius::same(7)),
                    )
                    .on_hover_text("Silence the output without stopping the bridge.")
                    .clicked()
                {
                    self.mute = !self.mute;
                }
                ui.add_space(4.0);
                if self.selected_output().asio_clsid.is_some() {
                    ui.label(
                        RichText::new("ASIO direct")
                            .size(ts(8.3))
                            .color(BLUE)
                            .strong(),
                    )
                    .on_hover_text(
                        "ASIO playback uses the driver's own buffer and clock; REAL-style does not apply.",
                    );
                } else {
                    let low_latency = self.low_latency_output;
                    let (label, hint) = if cfg!(target_os = "windows") {
                        (
                            "REAL-style",
                            "REAL-style output: run the WASAPI device at the smallest step its driver allows (often about 3 ms instead of 10 ms), like the REAL tool, and queue only that one step. Other apps stay audible and get the shorter step too. Some drivers only offer their 10 ms default; then RigWerk still uses that minimum and labels it in the MODE line instead of falling back to a larger buffer.",
                        )
                    } else {
                        (
                            "Low latency",
                            "Ask PipeWire for a 64-frame quantum and ALSA for the smallest period this output allows (at least 32 samples). The PIPEWIRE QUANTUM card and the DRIVER line warn when the ALSA period and the quantum do not fit, or when PipeWire falls back to its shared size.",
                        )
                    };
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new(label)
                                    .size(ts(8.3))
                                    .color(if low_latency { AMBER } else { DIM }),
                            )
                            .fill(if low_latency {
                                with_alpha(AMBER, 22)
                            } else {
                                Color32::TRANSPARENT
                            })
                            .stroke(Stroke::new(
                                1.0,
                                if low_latency {
                                    with_alpha(AMBER, 60)
                                } else {
                                    BORDER
                                },
                            ))
                            .corner_radius(CornerRadius::same(7)),
                        )
                        .on_hover_text(hint)
                        .clicked()
                    {
                        self.low_latency_output = !low_latency;
                        self.push_log(
                            if self.low_latency_output {
                                if cfg!(target_os = "windows") {
                                    "REAL-style output on: smallest driver step when allowed."
                                } else {
                                    "Low-latency output on: PipeWire low quantum + smallest ALSA period when allowed."
                                }
                            } else if cfg!(target_os = "windows") {
                                "REAL-style output off: normal shared output."
                            } else {
                                "Low-latency output off: normal output buffer, PipeWire quantum released."
                            },
                            if self.low_latency_output { AMBER } else { MUTED },
                        );
                    }
                }
            });
        });

        ui.add_space(5.0);
        self.output_clock_controls(ui);
        ui.add_space(3.0);
        let output_actual = self.output_actual();
        let stopped = self.engine.state != EngineState::Running;
        let checkable = stopped
            && !self.selected_output().demo
            && self.selected_output().asio_clsid.is_none()
            && (cfg!(target_os = "linux")
                || self
                    .selected_output()
                    .backend
                    .eq_ignore_ascii_case("WASAPI"));
        let checking = checkable.then(|| self.driver_probe_job.is_some());
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

        ui.add_space(5.0);
        ui.horizontal(|ui| {
            ui.label(micro_label("OUTPUT LEVEL", DIM));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(mono(format!("{:+.1} dB", self.output_gain), TEXT, 10.0));
            });
        });
        if db_slider(
            ui,
            &mut self.output_gain,
            OUTPUT_GAIN_MIN_DB,
            OUTPUT_GAIN_MAX_DB,
            BLUE,
        ) {
            self.engine.set_gains(self.input_gain, self.output_gain);
        }
        ui.add_space(4.0);
        meter_row(ui, "L", self.meter_out_l, self.peak_out_l, BLUE);
        meter_row(ui, "R", self.meter_out_r, self.peak_out_r, BLUE);

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let name = self.selected_output().name.clone();
            let backend = self.selected_output().backend.clone();
            let quantum_label = self.quantum_summary();
            let backend_label = match self.output_step() {
                Some((frames, ms)) => format!("STEP {frames} SMP · {ms:.1} MS"),
                None if self.low_latency_output && self.selected_output().asio_clsid.is_none() => {
                    quantum_label.unwrap_or_else(|| {
                        format!(
                            "{backend} · {}",
                            if cfg!(target_os = "windows") {
                                "REAL-STYLE"
                            } else {
                                "LOW LATENCY"
                            }
                        )
                    })
                }
                None => backend,
            };
            ui.label(RichText::new(ellipsize(&name, 30)).size(ts(9.0)).color(DIM));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(micro_label(&backend_label, BLUE))
                    .on_hover_text(self.output_mode_text());
            });
        });
    }

    /// Output side clock: its own rate (or follow the input) and buffer.
    pub(crate) fn output_clock_controls(&mut self, ui: &mut egui::Ui) {
        let real_wasapi = self.low_latency_output && self.selected_output().asio_clsid.is_none();
        ui.horizontal(|ui| {
            ui.label(micro_label("OUT CLOCK", DIM));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let mut buffer = self.output_buffer;
                egui::ComboBox::from_id_salt("output_buffer")
                    .width(72.0)
                    .selected_text(mono(format!("{buffer} smp"), MUTED, 9.0))
                    .show_ui(ui, |ui| {
                        for option in BUFFERS {
                            ui.selectable_value(&mut buffer, option, format!("{option} samples"));
                        }
                    })
                    .response
                    .on_hover_text(if real_wasapi {
                        "Output buffer, used if the driver refuses REAL-style. While REAL-style runs, the driver's smallest step sets the output latency."
                    } else {
                        "Output buffer: the block size asked of the output device."
                    });
                if buffer != self.output_buffer {
                    self.output_buffer = buffer;
                    self.push_log(format!("Output buffer set to {buffer} samples."), BLUE);
                }
                let mut rate = self.output_rate;
                let shown = if rate == 0 {
                    "Match input".to_owned()
                } else {
                    rate_label(rate)
                };
                egui::ComboBox::from_id_salt("output_rate")
                    .width(84.0)
                    .selected_text(mono(shown, MUTED, 9.0))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut rate, 0, "Match input");
                        for option in RATES {
                            ui.selectable_value(&mut rate, option, rate_label(option));
                        }
                    })
                    .response
                    .on_hover_text(if cfg!(target_os = "windows") {
                        "Output rate. When it differs from the input, RigWerk converts with a band-limited resampler. Shared WASAPI outputs mix at the rate set in Windows."
                    } else {
                        "Output rate. When it differs from the input, RigWerk converts with a band-limited resampler. PipeWire keeps its own graph rate; the quantum check compares the graph's frames with ALSA's period."
                    });
                if rate != self.output_rate {
                    self.output_rate = rate;
                    self.push_log(
                        if rate == 0 {
                            "Output rate follows the input.".to_owned()
                        } else {
                            format!("Output clock set to {}.", rate_label(rate))
                        },
                        BLUE,
                    );
                }
            });
        });
    }
}
