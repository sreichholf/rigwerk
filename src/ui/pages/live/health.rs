//! The Live scope and the bridge health tiles.

use eframe::egui;
use egui::{Align, Layout, Sense, Vec2};

use crate::app::RigWerkApp;
use crate::audio::EngineState;
use crate::ui::scope::{draw_scope, scope_stats, LEVEL_PREVIEW_LABEL};
use crate::ui::theme::{AMBER, BLUE, DIM, GREEN, MUTED, TEAL};
use crate::ui::widgets::{
    ellipsize, legend_dot, metric_tile, metric_tile_with_badge, micro_label, mono, status_dot,
};

impl RigWerkApp {
    pub(crate) fn scope_card(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, width: f32) {
        ui.horizontal(|ui| {
            ui.label(micro_label(LEVEL_PREVIEW_LABEL, DIM));
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
            &self.scope_in,
            &self.scope_out,
            self.engine.state == EngineState::Running,
        );

        ui.add_space(8.0);
        scope_stats(
            ui,
            width,
            self.peak_in_l.max(self.peak_in_r),
            self.peak_out_l.max(self.peak_out_r),
            self.latency_ms(),
            self.path_delay_ms(),
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
        let path_ms = self.latency_ms();
        let path_hint =
            "RigWerk path only (block + cushion + output step). What you can tune here.";
        let buffer_fill = if self.engine.state == EngineState::Preview {
            0.38 + (ui.input(|input| input.time) as f32 * 0.8).sin() * 0.04
        } else {
            self.snapshot.ring_fill
        };
        let buffer_pct = format!("{}", (buffer_fill * 100.0).round() as i32);
        let underruns = self.snapshot.underruns;

        // When the graph/device delay is known: PATH + HEARD on top, BUFFER +
        // DRIFT below, with underruns as a badge on BUFFER. Otherwise keep a
        // full UNDERRUNS tile — path delay is often missing on non-PipeWire.
        if let Some(heard) = self.heard_latency_ms() {
            let after = self.path_delay_ms().unwrap_or(0.0);
            ui.horizontal(|ui| {
                metric_tile(ui, "PATH", &format!("{path_ms:.1}"), "ms", TEAL, tile_w)
                    .on_hover_text(path_hint);
                metric_tile(ui, "HEARD", &format!("{heard:.1}"), "ms", BLUE, tile_w).on_hover_text(
                    format!(
                        "{path_ms:.1} ms in RigWerk + {after:.1} ms after (graph/device). \
                         Closer to what you hear; not a measured round trip."
                    ),
                );
            });
            let underrun_badge = (
                if underruns == 0 {
                    "0 drop".to_owned()
                } else if underruns == 1 {
                    "1 drop".to_owned()
                } else {
                    format!("{underruns} drops")
                },
                if underruns == 0 { DIM } else { AMBER },
            );
            ui.horizontal(|ui| {
                metric_tile_with_badge(
                    ui,
                    "BUFFER",
                    &buffer_pct,
                    "%",
                    AMBER,
                    tile_w,
                    Some((underrun_badge.0.as_str(), underrun_badge.1)),
                )
                .on_hover_text(if underruns == 0 {
                    "Ring-buffer fill. No underruns this session.".to_owned()
                } else {
                    format!(
                        "Ring-buffer fill. {underruns} underrun(s) — the ring ran empty and audio glitched."
                    )
                });
                metric_tile(ui, "DRIFT", &drift_value, drift_unit, GREEN, tile_w);
            });
        } else {
            ui.horizontal(|ui| {
                metric_tile(ui, "PATH", &format!("{path_ms:.1}"), "ms", TEAL, tile_w)
                    .on_hover_text(format!(
                        "{path_hint} Graph/device delay is unknown on this output."
                    ));
                metric_tile(ui, "DRIFT", &drift_value, drift_unit, GREEN, tile_w);
            });
            ui.horizontal(|ui| {
                metric_tile(ui, "BUFFER", &buffer_pct, "%", AMBER, tile_w);
                metric_tile(ui, "UNDERRUNS", &format!("{underruns}"), "", BLUE, tile_w);
            });
        }

        ui.add_space(10.0);
        let running = self.engine.state == EngineState::Running;
        let mode_text = &self.snapshot.output_mode;
        // While the bridge runs, MODE shows the live output block — not the
        // desktop PipeWire graph size (that lives on the OUTPUT card warning).
        let (mode, color, hint) = if self.selected_output().asio_clsid.is_some() {
            (
                "ASIO output · direct".to_owned(),
                BLUE,
                self.output_mode_text(),
            )
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
            (
                step,
                if driver_minimum { AMBER } else { GREEN },
                self.output_mode_text(),
            )
        } else if running && mode_text.starts_with("Low latency") {
            let step = self
                .output_step()
                .map(|(frames, ms)| format!("Low latency · {frames} smp · {ms:.1} ms"))
                .unwrap_or_else(|| "Low latency".to_owned());
            (step, GREEN, self.output_mode_text())
        } else if running && mode_text.contains("refused") {
            let step = self
                .output_step()
                .map(|(frames, ms)| format!("Fallback · {frames} smp · {ms:.1} ms"))
                .unwrap_or_else(|| "Fallback · driver step".to_owned());
            (step, AMBER, self.output_mode_text())
        } else if running {
            if let Some((frames, ms)) = self.output_step() {
                let (label, tone) = if self.low_latency_output {
                    (format!("Low latency · {frames} smp · {ms:.1} ms"), GREEN)
                } else {
                    (format!("Standard · {frames} smp · {ms:.1} ms"), MUTED)
                };
                (label, tone, self.output_mode_text())
            } else if let Some(summary) = self.quantum_summary() {
                let tone = if self.quantum_problem().is_some() {
                    AMBER
                } else {
                    GREEN
                };
                (summary, tone, self.graph_block_hint())
            } else {
                (
                    "Standard · output buffer".to_owned(),
                    MUTED,
                    self.output_mode_text(),
                )
            }
        } else if let Some(summary) = self.quantum_summary() {
            let tone = if self.quantum_problem().is_some() {
                AMBER
            } else {
                GREEN
            };
            (summary, tone, self.graph_block_hint())
        } else if self.low_latency_output {
            (
                if cfg!(target_os = "windows") {
                    "REAL-style · driver's smallest step".to_owned()
                } else {
                    "Low latency · smallest ALSA period".to_owned()
                },
                AMBER,
                self.output_mode_text(),
            )
        } else {
            (
                "Shared · output buffer".to_owned(),
                MUTED,
                self.output_mode_text(),
            )
        };
        // Host (PipeWire / ASIO / …) is already on INPUT, OUTPUT and AUDIO PATH.
        // Keep only the OS here so MODE's timing line has room.
        let host = self.inventory.os.to_uppercase();
        ui.horizontal(|ui| {
            ui.label(mono("MODE", DIM, 8.5)).on_hover_text(&hint);
            ui.add_space(6.0);
            let host_w = 52.0;
            let mode_w = (ui.available_width() - host_w - 8.0).max(80.0);
            let shown = ellipsize(&mode, (mode_w / 7.0).floor().max(8.0) as usize);
            ui.allocate_ui_with_layout(
                Vec2::new(mode_w, 18.0),
                Layout::left_to_right(Align::Center),
                |ui| {
                    ui.set_width(mode_w);
                    ui.label(mono(shown, color, 9.5)).on_hover_text(&hint);
                },
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(mono(&host, DIM, 8.5));
            });
        });
    }
}
