//! The bridge card: ring buffer, drift correction and the cushion.

use eframe::egui;
use egui::{Align, Layout, RichText, Sense, Vec2};

use crate::app::RigWerkApp;
use crate::audio::{CushionMode, EngineState};
use crate::ui::scope::draw_mini_scope;
use crate::ui::theme::ts;
use crate::ui::theme::{AMBER, DIM, GREEN, MUTED, TEXT};
use crate::ui::widgets::{
    hinted_tab, metric_inline, micro_label, mono, progress_line, status_dot, styled_slider,
};

impl RigWerkApp {
    pub(crate) fn bridge_card(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            ui.label(micro_label("BRIDGE", AMBER));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                status_dot(ui, if self.is_active() { AMBER } else { DIM });
            });
        });
        ui.add_space(4.0);
        ui.label(
            RichText::new("Two clocks, one strip of audio")
                .size(ts(11.5))
                .color(TEXT)
                .strong(),
        );
        ui.label(RichText::new("Async ring buffer").size(ts(9.5)).color(DIM));
        ui.add_space(9.0);

        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 76.0), Sense::hover());
        draw_mini_scope(ui, rect, ctx, self.is_active());

        ui.add_space(9.0);
        let fill = if self.engine.state == EngineState::Running {
            self.snapshot.ring_fill
        } else if self.engine.state == EngineState::Preview {
            0.38 + (ctx.input(|i| i.time) as f32 * 0.8).sin() * 0.04
        } else {
            0.0
        };
        progress_line(ui, "BUFFER", fill.clamp(0.0, 1.0), AMBER);
        ui.add_space(4.0);

        ui.horizontal(|ui| {
            metric_inline(
                ui,
                "DRIFT",
                if self.drift { "AUTO" } else { "OFF" },
                if self.drift { GREEN } else { MUTED },
            );
            metric_inline(
                ui,
                "UNDERRUN",
                &format!(
                    "{}",
                    if self.engine.state == EngineState::Running {
                        self.snapshot.underruns
                    } else {
                        0
                    }
                ),
                MUTED,
            );
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let mut drift = self.drift;
            if ui
                .checkbox(
                    &mut drift,
                    RichText::new("Drift correction")
                        .size(ts(10.0))
                        .color(MUTED),
                )
                .changed()
            {
                self.drift = drift;
                self.push_log(
                    if self.drift {
                        "Automatic drift correction enabled."
                    } else {
                        "Drift correction disabled — clocks may slip."
                    },
                    if self.drift { GREEN } else { AMBER },
                );
            }
        });
        ui.add_space(6.0);
        self.cushion_controls(ui);
    }

    /// The cushion between input and output: Safe, Low latency or a fixed
    /// manual size.  Applies immediately, also while the bridge runs.
    pub(crate) fn cushion_controls(&mut self, ui: &mut egui::Ui) {
        let running = self.engine.state == EngineState::Running;
        ui.horizontal(|ui| {
            ui.label(micro_label("CUSHION", DIM));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let live = (running && self.snapshot.input_rate > 0).then(|| {
                    self.snapshot.target_frames as f32 / self.snapshot.input_rate as f32 * 1000.0
                });
                let text = match live {
                    Some(ms) => format!("{} smp · {ms:.1} ms", self.snapshot.target_frames),
                    None => "—".to_owned(),
                };
                ui.label(mono(text, AMBER, 9.0))
                    .on_hover_text("The cushion the bridge is holding right now.");
            });
        });
        ui.add_space(3.0);
        let mut mode = self.cushion_mode;
        ui.horizontal(|ui| {
            for (option, label, hint) in [
                (
                    CushionMode::Safe,
                    "Safe",
                    "Twice the input buffer, at least two output blocks. Grows 256 samples per dropout, shrinks 64 after 4 calm seconds.",
                ),
                (
                    CushionMode::LowLatency,
                    "Low latency",
                    "One input block plus one output block, the least that can work. Recovers from dropouts in smaller, quicker steps.",
                ),
                (
                    CushionMode::Manual,
                    "Manual",
                    "A fixed size that never grows on its own. Lower it until dropouts start, then go one notch back up.",
                ),
            ] {
                if hinted_tab(ui, label, mode == option, AMBER, hint) {
                    mode = option;
                }
            }
        });
        if mode != self.cushion_mode {
            self.cushion_mode = mode;
            self.push_log(
                match mode {
                    CushionMode::Safe => "Cushion: Safe.".to_owned(),
                    CushionMode::LowLatency => "Cushion: Low latency.".to_owned(),
                    CushionMode::Manual => {
                        format!("Cushion: Manual, {:.1} ms.", self.cushion_ms)
                    }
                },
                AMBER,
            );
            self.engine.set_cushion(self.cushion_mode, self.cushion_ms);
        }
        if self.cushion_mode == CushionMode::Manual {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(micro_label("SIZE", DIM));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(mono(format!("{:.1} ms", self.cushion_ms), TEXT, 10.0));
                });
            });
            let mut ms = self.cushion_ms;
            let response = styled_slider(ui, &mut ms, 1.0, 40.0, 0.5, AMBER);
            if response.changed() {
                self.cushion_ms = ms;
                self.engine.set_cushion(self.cushion_mode, self.cushion_ms);
            }
            if response.drag_stopped() || (response.changed() && !response.dragged()) {
                self.push_log(
                    format!("Manual cushion set to {:.1} ms.", self.cushion_ms),
                    AMBER,
                );
            }
        }
    }
}
