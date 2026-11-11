//! The bridge card: ring buffer, drift correction and the cushion.

use eframe::egui;
use egui::{Align, FontFamily, FontId, Layout, RichText, Sense, Vec2};

use crate::app::settings::{CUSHION_MS_MAX, CUSHION_MS_MIN};
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
        let direct = self.engine.state == EngineState::Running && self.snapshot.direct;
        ui.label(
            RichText::new(if direct {
                "One PipeWire node · no bridge buffer"
            } else {
                "Async ring buffer"
            })
            .size(ts(9.5))
            .color(DIM),
        );
        ui.add_space(9.0);

        // The mode row decides this.  A narrow card gives the waveform's spare
        // pixels to the cushion block below, which otherwise runs into the clip.
        let scope_h = bridge_scope_height(cushion_modes_fit(ui));
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), scope_h), Sense::hover());
        // Prefer input when present, else the playback lane.
        let samples = if self.scope_in.iter().any(|sample| sample.abs() > 1.0e-5) {
            self.scope_in.as_slice()
        } else {
            self.scope_out.as_slice()
        };
        draw_mini_scope(ui, rect, samples, self.engine.state == EngineState::Running);

        ui.add_space(9.0);
        let fill = if direct {
            0.0
        } else if self.engine.state == EngineState::Running {
            self.snapshot.ring_fill
        } else if self.engine.state == EngineState::Preview {
            0.38 + (ctx.input(|i| i.time) as f32 * 0.8).sin() * 0.04
        } else {
            0.0
        };
        let fill = fill.clamp(0.0, 1.0);
        let percent = format!("{}%", (fill * 100.0).round() as i32);
        progress_line(ui, "BUFFER", &percent, fill, AMBER);
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
                    _ if running && self.snapshot.direct => "none · duplex node".to_owned(),
                    Some(ms) => format!("{} smp · {ms:.1} ms", self.snapshot.target_frames),
                    None => "—".to_owned(),
                };
                ui.label(mono(text, AMBER, 9.0))
                    .on_hover_text("The cushion the bridge is holding right now.");
            });
        });
        ui.add_space(3.0);
        let mut mode = self.cushion_mode;
        cushion_mode_picker(ui, &mut mode);
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
            let mut ms = self.cushion_ms;
            let response = manual_cushion_size(ui, &mut ms);
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

/// Safe, Low latency, Manual, in the order they are offered.
fn cushion_choices() -> [(CushionMode, &'static str, &'static str); 3] {
    [
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
    ]
}

/// Width of the three mode tabs on one row, including the gaps between them.
fn cushion_tab_row_width(ui: &egui::Ui) -> f32 {
    let font = FontId::new(ts(8.2), FontFamily::Proportional);
    let pad = ui.spacing().button_padding.x * 2.0;
    let gap = ui.spacing().item_spacing.x;
    let mut width = 0.0;
    for (index, (_, label, _)) in cushion_choices().iter().enumerate() {
        if index > 0 {
            width += gap;
        }
        let text = ui
            .painter()
            .layout_no_wrap((*label).to_owned(), font.clone(), TEXT);
        width += text.size().x + pad;
    }
    width
}

/// The three tabs fit the card when this is true.  At the minimum window the
/// bridge card is about 150 px inside and the row is about 190 px, so it is
/// not.
fn cushion_modes_fit(ui: &egui::Ui) -> bool {
    cushion_tab_row_width(ui) <= ui.available_width()
}

/// Full waveform, or the shorter one used when the mode row does not fit.
///
/// The live cards share [`crate::ui::pages::live::LIVE_TOP_CARD_H`] with a
/// 15 px inset.  On a narrow card the title wraps and a second tab row (or
/// the manual slider under an unclipped mode control) would be cut off by
/// `card_fixed`.  Giving back 52 px of the 76 px trace keeps Safe / Low
/// latency / Manual and the manual size slider inside that inset.
fn bridge_scope_height(modes_fit: bool) -> f32 {
    if modes_fit {
        76.0
    } else {
        24.0
    }
}

/// Tabs when they fit.  A combo of the same three modes when they do not, so
/// the last label is not clipped.
fn cushion_mode_picker(ui: &mut egui::Ui, mode: &mut CushionMode) -> egui::Rect {
    let choices = cushion_choices();
    if cushion_modes_fit(ui) {
        ui.horizontal(|ui| {
            for (option, label, hint) in choices {
                if hinted_tab(ui, label, *mode == option, AMBER, hint) {
                    *mode = option;
                }
            }
        })
        .response
        .rect
    } else {
        let (_, label, hint) = choices
            .iter()
            .find(|(option, _, _)| *option == *mode)
            .copied()
            .unwrap_or(choices[0]);
        let available = ui.available_width();
        egui::ComboBox::from_id_salt("cushion_mode")
            .width(available)
            .truncate()
            .selected_text(RichText::new(label).size(ts(8.2)).color(AMBER).strong())
            .show_ui(ui, |ui| {
                for (option, label, hint) in choices {
                    ui.selectable_value(mode, option, label).on_hover_text(hint);
                }
            })
            .response
            .on_hover_text(hint)
            .rect
    }
}

/// The manual size.  The number and the slider; the caller stores and applies it.
fn manual_cushion_size(ui: &mut egui::Ui, ms: &mut f32) -> egui::Response {
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(micro_label("SIZE", DIM));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(mono(format!("{ms:.1} ms"), TEXT, 10.0));
        });
    });
    styled_slider(ui, ms, CUSHION_MS_MIN, CUSHION_MS_MAX, 0.5, AMBER)
}

#[cfg(test)]
mod tests;
