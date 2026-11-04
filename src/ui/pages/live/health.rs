//! The live signal scope and the bridge health tiles.

use eframe::egui;
use egui::{Align, CornerRadius, Layout, Margin, RichText, Sense, Vec2};

use crate::app::RigWerkApp;
use crate::audio::EngineState;
use crate::ui::scope::{draw_scope, scope_stats};
use crate::ui::theme::{AMBER, BLUE, CARD_DEEP, DIM, GREEN, MUTED, TEAL};
use crate::ui::widgets::{ellipsize, legend_dot, metric_tile, micro_label, mono, status_dot};

impl RigWerkApp {
    pub(crate) fn scope_card(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, width: f32) {
        ui.horizontal(|ui| {
            ui.label(micro_label("LIVE SIGNAL", DIM));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                legend_dot(ui, "OUT", BLUE);
                ui.add_space(6.0);
                legend_dot(ui, "IN", TEAL);
            });
        });
        ui.add_space(7.0);

        let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 188.0), Sense::hover());
        draw_scope(
            ui,
            rect,
            ctx,
            self.meter_in_l,
            self.meter_in_r,
            self.meter_out_l,
            self.meter_out_r,
            self.is_active(),
        );

        ui.add_space(8.0);
        scope_stats(
            ui,
            width,
            self.meter_in_l,
            self.meter_out_l,
            self.latency_ms(),
            if self.engine.state == EngineState::Running {
                self.snapshot.frames
            } else if self.engine.state == EngineState::Preview {
                (ctx.input(|input| input.time) * self.sample_rate as f64) as u64
            } else {
                0
            },
        );
    }

    pub(crate) fn health_card(&mut self, ui: &mut egui::Ui, width: f32) {
        ui.horizontal(|ui| {
            ui.label(micro_label("BRIDGE HEALTH", DIM));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                status_dot(ui, if self.is_active() { GREEN } else { DIM });
            });
        });
        ui.add_space(8.0);

        let gap = 6.0;
        let tile_w = (width - gap) / 2.0;
        let (drift_value, drift_unit) = if self.engine.state == EngineState::Running {
            (format!("{:+.1}", self.snapshot.drift_ppm), "ppm")
        } else if self.engine.state == EngineState::Preview {
            ("AUTO".to_owned(), "")
        } else {
            ("—".to_owned(), "")
        };
        ui.horizontal(|ui| {
            metric_tile(
                ui,
                "LATENCY",
                &format!("{:.1}", self.latency_ms()),
                "ms",
                TEAL,
                tile_w,
            );
            metric_tile(ui, "DRIFT", &drift_value, drift_unit, GREEN, tile_w);
        });
        let buffer_fill = if self.engine.state == EngineState::Preview {
            0.38 + (ui.input(|input| input.time) as f32 * 0.8).sin() * 0.04
        } else {
            self.snapshot.ring_fill
        };
        ui.horizontal(|ui| {
            metric_tile(
                ui,
                "BUFFER",
                &format!("{}", (buffer_fill * 100.0).round() as i32),
                "%",
                AMBER,
                tile_w,
            );
            metric_tile(
                ui,
                "UNDERRUNS",
                &format!("{}", self.snapshot.underruns),
                "",
                BLUE,
                tile_w,
            );
        });

        ui.add_space(9.0);
        let current = self.status_message();
        egui::Frame::new()
            .fill(CARD_DEEP)
            .corner_radius(CornerRadius::same(8))
            .inner_margin(Margin::same(9))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    status_dot(ui, current.1);
                    ui.label(
                        RichText::new(ellipsize(&current.0, 42))
                            .size(9.5)
                            .color(MUTED),
                    );
                });
            });

        ui.add_space(6.0);
        let running = self.engine.state == EngineState::Running;
        let mode_text = &self.snapshot.output_mode;
        let (mode, color) = if self.selected_output().asio_clsid.is_some() {
            ("ASIO output · direct".to_owned(), BLUE)
        } else if running && mode_text.starts_with("REAL-style") {
            let driver_minimum = mode_text.contains("no smaller step");
            let step = self
                .output_step()
                .map(|(frames, ms)| {
                    if driver_minimum {
                        format!("REAL · {frames} smp · {ms:.1} ms · driver minimum")
                    } else {
                        format!("REAL · {frames} smp · {ms:.1} ms")
                    }
                })
                .unwrap_or_else(|| "REAL-style".to_owned());
            (step, if driver_minimum { AMBER } else { GREEN })
        } else if running && mode_text.starts_with("Low latency") {
            let step = self
                .output_step()
                .map(|(frames, ms)| format!("Low latency · {frames} smp · {ms:.1} ms"))
                .unwrap_or_else(|| "Low latency".to_owned());
            (step, GREEN)
        } else if running && mode_text.contains("refused") {
            let step = self
                .output_step()
                .map(|(frames, ms)| format!("Fallback · {frames} smp · {ms:.1} ms"))
                .unwrap_or_else(|| "Fallback · driver step".to_owned());
            (step, AMBER)
        } else if self.low_latency_output {
            (
                if cfg!(target_os = "windows") {
                    "REAL-style · driver's smallest step".to_owned()
                } else {
                    "Low latency · smallest ALSA period".to_owned()
                },
                AMBER,
            )
        } else if running {
            let step = self
                .output_step()
                .map(|(frames, ms)| format!("Standard · {frames} smp · {ms:.1} ms"))
                .unwrap_or_else(|| "Standard · output buffer".to_owned());
            (step, MUTED)
        } else {
            ("Shared · output buffer".to_owned(), MUTED)
        };
        ui.horizontal(|ui| {
            ui.label(mono("MODE", DIM, 8.5));
            ui.label(mono(mode, color, 9.5))
                .on_hover_text(self.output_mode_text());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(mono(
                    &format!(
                        "{} · {}",
                        self.inventory.low_latency_label,
                        self.inventory.os.to_uppercase()
                    ),
                    DIM,
                    8.5,
                ));
            });
        });
    }
}
