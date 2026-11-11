//! The input card: device, channels, input clock, gain and meters.  When
//! the input is ASIO, a Panel chip sits with the channel chips and asks
//! the bridge to open that driver's control panel.

use eframe::egui;
use egui::{Align, Layout, RichText};

use crate::app::settings::{rate_label, BUFFERS, INPUT_GAIN_MAX_DB, INPUT_GAIN_MIN_DB, RATES};
use crate::app::RigWerkApp;
use crate::audio::{AsioPanelSide, EngineState};
use crate::ui::pages::live::driver_line::driver_line;
use crate::ui::theme::ts;
use crate::ui::theme::{AMBER, BLUE, DIM, GREEN, MUTED, TEAL, TEXT};
use crate::ui::widgets::{
    channel_chip, db_slider, device_selector, ellipsize, meter_row, micro_label, mono, source_tab,
};

impl RigWerkApp {
    pub(crate) fn input_card(&mut self, ui: &mut egui::Ui) {
        let low_label = self.inventory.low_latency_label.clone();
        let mut use_asio = self.use_asio_input;
        ui.horizontal(|ui| {
            ui.label(micro_label("INPUT", TEAL));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if source_tab(ui, "SYS", !use_asio, BLUE) {
                    use_asio = false;
                }
                if source_tab(ui, &low_label, use_asio, TEAL) {
                    use_asio = true;
                }
            });
        });
        self.use_asio_input = use_asio;
        ui.add_space(7.0);

        if self.using_other_input() {
            let mut idx = self.input_idx;
            if device_selector(
                ui,
                "system_input_choice",
                &self.inventory.inputs,
                &mut idx,
                TEAL,
            ) {
                self.accept_input_device();
            }
            self.input_idx = idx;
        } else {
            let previous_asio = self.asio_idx;
            let mut idx = self.asio_idx;
            if device_selector(ui, "asio_input", &self.inventory.asio, &mut idx, TEAL) {
                self.accept_asio_device();
            }
            self.asio_idx = idx;
            if idx != previous_asio {
                self.use_asio_input = true;
            }
        }
        ui.add_space(2.0);

        ui.horizontal(|ui| {
            channel_chip(ui, "IN 1", &mut self.input_channels[0], TEAL);
            channel_chip(ui, "IN 2", &mut self.input_channels[1], TEAL);
            self.asio_panel_chip(ui, AsioPanelSide::Input, TEAL);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(mono(self.latency_caption(), MUTED, 9.5));
            });
        });

        ui.add_space(5.0);
        self.clock_controls(ui);
        ui.add_space(3.0);
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

        ui.add_space(5.0);
        ui.horizontal(|ui| {
            ui.label(micro_label("GAIN", DIM));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(mono(format!("{:+.1} dB", self.input_gain), TEXT, 10.0));
            });
        });
        if db_slider(
            ui,
            &mut self.input_gain,
            INPUT_GAIN_MIN_DB,
            INPUT_GAIN_MAX_DB,
            TEAL,
        ) {
            self.engine.set_gains(self.input_gain, self.output_gain);
        }

        ui.add_space(4.0);
        meter_row(ui, "L", self.meter_in_l, self.peak_in_l, TEAL);
        meter_row(ui, "R", self.meter_in_r, self.peak_in_r, TEAL);

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let name = self.active_input().name.clone();
            let demo = self.active_input().demo;
            let hint = if demo {
                "preview source"
            } else if self.using_other_input() {
                "system input"
            } else {
                "dedicated clock"
            };
            ui.label(RichText::new(ellipsize(&name, 24)).size(ts(9.0)).color(DIM));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new(hint)
                        .size(ts(9.0))
                        .color(if demo { AMBER } else { GREEN }),
                );
            });
        });
    }

    /// Input side clock: the rate and buffer asked of the capture device,
    /// laid out like the output card's own pair.
    pub(crate) fn clock_controls(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(micro_label("IN CLOCK", DIM));
            let mut rate = self.sample_rate;
            egui::ComboBox::from_id_salt("sample_rate")
                .width(84.0)
                .selected_text(mono(rate_label(rate), MUTED, 9.0))
                .show_ui(ui, |ui| {
                    for option in RATES {
                        ui.selectable_value(&mut rate, option, rate_label(option));
                    }
                })
                .response
                .on_hover_text(
                    "Input rate. A device that cannot run at it opens at its own rate and the output converts.",
                );
            if rate != self.sample_rate {
                self.sample_rate = rate;
                self.push_log(format!("Input clock set to {}.", rate_label(rate)), BLUE);
            }
            let mut buffer = self.buffer_size;
            egui::ComboBox::from_id_salt("buffer_size")
                .width(72.0)
                .selected_text(mono(format!("{buffer} smp"), MUTED, 9.0))
                .show_ui(ui, |ui| {
                    for option in BUFFERS {
                        ui.selectable_value(&mut buffer, option, format!("{option} samples"));
                    }
                })
                .response
                .on_hover_text("Input buffer: the block size asked of the input device.");
            if buffer != self.buffer_size {
                self.buffer_size = buffer;
                self.push_log(format!("Input buffer set to {buffer} samples."), BLUE);
            }
        });
    }
}
