//! The output card: device, REAL-style, output clock, level and meters.
//! When the output is ASIO, a Panel chip sits with the channel chips and
//! asks that output's bridge to open its driver's control panel.

use eframe::egui;
use egui::{Align, CornerRadius, Layout, Margin, RichText, Stroke, Vec2};

use crate::app::devices::is_wrapper_driver;
use crate::app::settings::{rate_label, OUTPUT_GAIN_MAX_DB, OUTPUT_GAIN_MIN_DB};
use crate::app::RigWerkApp;
use crate::audio::{AsioPanelSide, DeviceItem, EngineState};
use crate::ui::pages::live::driver_line::driver_line;
use crate::ui::pages::mixer::output_devices::gain_reduction_text;
use crate::ui::theme::ts;
use crate::ui::theme::{
    with_alpha, AMBER, BLUE, BORDER_SOFT, CARD_DEEP, DIM, GREEN, MUTED, RED, TEXT,
};
use crate::ui::widgets::{
    channel_chip, db_slider, device_selector, ellipsize, meter_row, micro_label, mono, source_tab,
};

impl RigWerkApp {
    pub(crate) fn output_card(&mut self, ui: &mut egui::Ui) {
        // Top-down, so the card is as tall as its content and the Input card
        // can match that height.  A fixed frame used to pin this row to the
        // bottom; the extra outputs now follow the body, and nothing is clipped.
        ui.set_min_width(ui.available_width());
        self.output_card_body(ui);
        ui.add_space(8.0);
        self.also_playing_on(ui);
    }

    fn output_card_body(&mut self, ui: &mut egui::Ui) {
        let low_label = self.inventory.low_latency_label.clone();
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

        let mut enabled = self.path().mix.output_enabled;
        ui.horizontal(|ui| {
            channel_chip(ui, "OUT 1", &mut enabled[0], BLUE);
            channel_chip(ui, "OUT 2", &mut enabled[1], BLUE);
            self.asio_panel_chip(ui, AsioPanelSide::Output, BLUE);
            ui.label(
                RichText::new(crate::app::latency::output_mode_phrase(self.output_mode()))
                    .size(ts(8.3))
                    .color(BLUE)
                    .strong(),
            )
            .on_hover_text(self.output_mode_text());
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
                .on_hover_text(
                    "Silence the output without stopping the bridge. Mute (Ctrl+Shift+M).",
                )
                .clicked()
            {
                self.mute = !self.mute;
            }
        });
        if enabled != self.path().mix.output_enabled {
            self.set_path_output_enabled(enabled);
        }

        ui.add_space(5.0);
        self.output_clock_reading(ui);
        ui.add_space(3.0);
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
            self.apply_gains();
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
                None if self.wants_low_latency_output() => quantum_label.unwrap_or_else(|| {
                    format!(
                        "{backend} · {}",
                        crate::app::latency::output_mode_phrase(self.output_mode())
                    )
                }),
                None => backend,
            };
            ui.label(RichText::new(ellipsize(&name, 30)).size(ts(9.0)).color(DIM));
            if self.engine.state == EngineState::Running {
                if let Some(text) = gain_reduction_text(self.snapshot.out_reduction_db) {
                    ui.label(mono(text, AMBER, 9.0));
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(micro_label(&backend_label, BLUE))
                    .on_hover_text(self.output_mode_text());
            });
        });
    }

    /// One row under the Output body: a hairline, then the label, extra output
    /// chips and + add.
    pub(crate) fn also_playing_on(&mut self, ui: &mut egui::Ui) {
        use crate::app::chain::ChainSelection;
        use crate::app::Page;
        use crate::ui::widgets::horizontal_line;

        let rows = self.output_device_rows();
        let extra_outputs: Vec<_> = rows.into_iter().filter(|row| !row.clock).collect();
        horizontal_line(ui, BORDER_SOFT);
        ui.add_space(6.0);
        // Exact row height (chip text + button padding).
        let font_h = ui.fonts_mut(|fonts| fonts.row_height(&egui::FontId::proportional(ts(9.5))));
        let row_h = (font_h + ui.spacing().button_padding.y * 2.0).max(24.0);
        ui.allocate_ui_with_layout(
            Vec2::new(ui.available_width(), row_h),
            Layout::left_to_right(Align::Center),
            |ui| {
                ui.set_height(row_h);
                ui.spacing_mut().item_spacing = Vec2::new(6.0, 0.0);
                ui.label(micro_label("ALSO PLAYING ON", MUTED));
                // Chips sit in the middle; + add stays on the right.
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let add = ui.add_enabled(
                        self.add_output_disabled_reason().is_none(),
                        egui::Button::new(RichText::new("+ add").size(ts(9.5)).color(BLUE))
                            .fill(with_alpha(BLUE, 14))
                            .stroke(Stroke::new(1.0, with_alpha(BLUE, 45)))
                            .corner_radius(CornerRadius::same(6))
                            .min_size(Vec2::new(0.0, 24.0)),
                    );
                    if let Some(reason) = self.add_output_disabled_reason() {
                        add.on_disabled_hover_text(reason);
                    } else if add.clicked() {
                        self.chain_selected = ChainSelection::AddOutput;
                        self.page = Page::Mixer;
                    }
                    ui.add_space(2.0);
                    // Remaining width, left-to-right so names read naturally.
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing = Vec2::new(4.0, 0.0);
                        if extra_outputs.is_empty() {
                            ui.label(RichText::new("clock only").size(ts(9.5)).color(MUTED));
                            return;
                        }
                        // Short chips so about six still fit beside + add.
                        let name_chars = match extra_outputs.len() {
                            1 => 18,
                            2 => 14,
                            3 => 11,
                            _ => 9,
                        };
                        for (offset, row) in extra_outputs.iter().enumerate() {
                            let index = offset + 1; // clock is 0
                            let tip = if row.virtual_output {
                                format!("{} · virtual · open in Mixer", row.name)
                            } else if row.missing {
                                format!("{} · missing · open in Mixer", row.name)
                            } else {
                                format!("{} · open in Mixer", row.name)
                            };
                            let response = ui
                                .add(
                                    egui::Button::new(
                                        RichText::new(ellipsize(&row.name, name_chars))
                                            .size(ts(9.5))
                                            .color(TEXT),
                                    )
                                    .fill(with_alpha(TEXT, 4))
                                    .stroke(Stroke::new(1.0, BORDER_SOFT))
                                    .corner_radius(CornerRadius::same(6))
                                    .min_size(Vec2::new(0.0, 24.0)),
                                )
                                .on_hover_text(tip);
                            if response.clicked() {
                                self.chain_selected = ChainSelection::OutputDevice(index);
                                self.page = Page::Mixer;
                            }
                            if let Some(text) = gain_reduction_text(row.reduction_db) {
                                ui.label(mono(text, AMBER, 9.0));
                            }
                        }
                    });
                });
            },
        );
    }

    /// Clock output rate and buffer, as readings.  Setup changes them.
    fn output_clock_reading(&mut self, ui: &mut egui::Ui) {
        let shown = if self.output_rate == 0 {
            "Match input".to_owned()
        } else {
            rate_label(self.output_rate)
        };
        ui.horizontal(|ui| {
            ui.label(micro_label("RATE", DIM));
            ui.label(mono(shown, MUTED, 9.0));
            ui.label(micro_label("BUFFER", DIM));
            ui.label(mono(format!("{} samples", self.output_buffer), MUTED, 9.0));
        });
    }
}
