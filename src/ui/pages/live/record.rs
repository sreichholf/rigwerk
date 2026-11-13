//! The record row under the bridge card.
//!
//! One line: the label, Record or Stop, elapsed time, an overrun count when
//! the queue could not take a frame, and the take's file name.  The button calls
//! [`RigWerkApp::start_recording`] and [`RigWerkApp::stop_recording`].  This
//! file does not open the wav.

use std::path::Path;

use eframe::egui;
use egui::{Align, CornerRadius, Layout, Margin, RichText, Stroke, Vec2};

use crate::app::record::{format_elapsed, overrun_label};
use crate::app::RigWerkApp;
use crate::audio::{EngineState, RecordPhase};
use crate::ui::theme::ts;
use crate::ui::theme::{with_alpha, AMBER, BORDER, CARD, GREEN, MUTED, RED, TEXT};
use crate::ui::widgets::{ellipsize, micro_label, mono};

impl RigWerkApp {
    /// The full-width record strip under the bridge card.  One line.
    pub(crate) fn record_row(&mut self, ui: &mut egui::Ui) {
        let width = ui.available_width();
        let running = self.engine.state == EngineState::Running;
        let status = self.engine.recording_status();
        let (stop, failed, elapsed_color) = match status.phase {
            RecordPhase::Recording => (true, false, GREEN),
            RecordPhase::Arming | RecordPhase::Finishing => (true, false, AMBER),
            RecordPhase::Failed => (false, true, TEXT),
            RecordPhase::Idle => (false, false, TEXT),
        };
        let elapsed = format_elapsed(status.frames, status.rate);
        let overruns = status.overruns;
        let error = status.error;
        let path = status.path;
        egui::Frame::new()
            .fill(CARD)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(CornerRadius::same(12))
            .inner_margin(Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.set_width((width - 24.0).max(0.0));
                ui.horizontal(|ui| {
                    ui.label(micro_label("RECORD", AMBER));
                    let label = if stop { "Stop" } else { "Record" };
                    let response = ui
                        .add_enabled_ui(running, |ui| {
                            ui.add_sized([72.0, 24.0], transport_button(label, stop))
                        })
                        .inner;
                    if !running {
                        let _ = response.on_disabled_hover_text("Start the bridge to record.");
                    } else if response
                        .on_hover_text(if stop {
                            "Stop and close the take."
                        } else {
                            "Record what this output is playing."
                        })
                        .clicked()
                    {
                        if stop {
                            self.stop_recording();
                        } else {
                            self.start_recording();
                        }
                    }
                    let rate_line = if stop && status.rate > 0 && status.channels > 0 {
                        format!("{elapsed} · {} Hz · {} ch", status.rate, status.channels)
                    } else {
                        elapsed
                    };
                    ui.label(mono(rate_line, elapsed_color, 11.0));
                    if overruns > 0 {
                        ui.label(mono(overrun_label(overruns), RED, 11.0));
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if let Some(path) = &path {
                            let name = ellipsize(&file_name(path), 48);
                            let _ = ui.label(mono(name, MUTED, 11.0)).on_hover_text(path);
                        }
                        if failed {
                            if let Some(error) = error.as_deref() {
                                let _ = ui
                                    .label(mono(ellipsize(error, 64), RED, 11.0))
                                    .on_hover_text(error);
                            }
                        }
                    });
                });
            });
    }
}

fn transport_button(label: &str, stop: bool) -> egui::Button<'_> {
    let (fill, text, stroke) = if stop {
        (with_alpha(RED, 18), RED, with_alpha(RED, 70))
    } else {
        (with_alpha(GREEN, 18), GREEN, with_alpha(GREEN, 70))
    };
    egui::Button::new(RichText::new(label).size(ts(11.0)).color(text).strong())
        .fill(fill)
        .stroke(Stroke::new(1.0, stroke))
        .corner_radius(CornerRadius::same(6))
        .min_size(Vec2::new(72.0, 22.0))
}

fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path.to_owned())
}
