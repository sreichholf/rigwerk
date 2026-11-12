//! The bridge strip under the Input and Output cards.  A PipeWire duplex
//! node has no ring, so the strip is one line.  A ring shows its fill, the
//! drift reading and the cushion.  Manual cushion adds a size row.

use crate::app::LogTone;
use eframe::egui;
use egui::{
    Align, Color32, CornerRadius, FontFamily, FontId, Layout, Margin, Rect, RichText, Sense,
    Stroke, Vec2,
};

use crate::app::issues::IssueCard;
use crate::app::settings::{CUSHION_MS_MAX, CUSHION_MS_MIN};
use crate::app::RigWerkApp;
use crate::audio::{CushionMode, EngineState};
use crate::ui::theme::ts;
use crate::ui::theme::{AMBER, BORDER, CARD, DIM, GREEN, MUTED, TEXT};
use crate::ui::widgets::{hinted_tab, micro_label, mono, styled_slider};

impl RigWerkApp {
    /// The full-width bridge strip.  Duplex is one line.  A ring keeps the
    /// cushion on the same line, and Manual opens a size row under it.
    pub(crate) fn bridge_card(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let direct = self.engine.state == EngineState::Running && self.snapshot.direct;
        let width = ui.available_width();
        let response = egui::Frame::new()
            .fill(CARD)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(CornerRadius::same(12))
            .inner_margin(Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.set_width((width - 24.0).max(0.0));
                if direct {
                    self.bridge_direct_line(ui);
                } else {
                    self.bridge_ring_line(ui, ctx);
                }
            });
        self.card_highlight_ring(ui, response.response.rect, IssueCard::Bridge);
    }

    fn bridge_direct_line(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(micro_label("BRIDGE", AMBER));
            ui.label(
                RichText::new("PipeWire node · no ring")
                    .size(ts(12.0))
                    .color(TEXT),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(mono(underrun_text(self.bridge_underruns()), MUTED, 11.0));
            });
        });
    }

    fn bridge_ring_line(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let fill = self.bridge_fill(ctx);
        let percent = format!("{}%", (fill * 100.0).round() as i32);
        ui.horizontal(|ui| {
            ui.label(micro_label("RING", AMBER));
            fill_bar(ui, fill);
            ui.label(mono(percent, TEXT, 11.0));
            self.drift_reading(ui);
            ui.label(mono(underrun_text(self.bridge_underruns()), MUTED, 11.0));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                self.cushion_mode_row(ui);
            });
        });
        if self.cushion_mode == CushionMode::Manual {
            let mut ms = self.cushion_ms;
            let response = manual_cushion_size(ui, &mut ms);
            if response.changed() {
                self.cushion_ms = ms;
                self.apply_cushion();
            }
            if response.drag_stopped() || (response.changed() && !response.dragged()) {
                self.push_log(
                    format!("Manual cushion set to {:.1} ms.", self.cushion_ms),
                    LogTone::Amber,
                );
            }
        }
    }

    fn bridge_fill(&self, ctx: &egui::Context) -> f32 {
        let fill = if self.engine.state == EngineState::Running {
            self.snapshot.ring_fill
        } else if self.engine.state == EngineState::Preview {
            0.38 + (ctx.input(|input| input.time) as f32 * 0.8).sin() * 0.04
        } else {
            0.0
        };
        fill.clamp(0.0, 1.0)
    }

    fn bridge_underruns(&self) -> u64 {
        if self.engine.state == EngineState::Running {
            self.snapshot.underruns
        } else {
            0
        }
    }

    /// The drift reading.  Click turns correction on or off.  The Signal
    /// Routing page has the same switch.
    fn drift_reading(&mut self, ui: &mut egui::Ui) {
        let text = if !self.drift {
            "drift off".to_owned()
        } else if self.engine.state == EngineState::Running {
            format!("drift {:+.1} ppm", self.snapshot.drift_ppm)
        } else {
            "drift auto".to_owned()
        };
        let color = if self.drift { GREEN } else { MUTED };
        if ui
            .add(
                egui::Button::new(mono(text, color, 11.0))
                    .frame(false)
                    .min_size(Vec2::ZERO),
            )
            .on_hover_text(
                "Drift correction keeps the two clocks from walking apart. Click to turn it on or off.",
            )
            .clicked()
        {
            self.drift = !self.drift;
            self.push_log(
                if self.drift {
                    "Automatic drift correction enabled."
                } else {
                    "Drift correction disabled — clocks may slip."
                },
                if self.drift { LogTone::Green } else { LogTone::Amber });
        }
    }

    fn cushion_mode_row(&mut self, ui: &mut egui::Ui) {
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
                LogTone::Amber,
            );
            self.apply_cushion();
        }
    }
}

fn underrun_text(count: u64) -> String {
    match count {
        1 => "1 underrun".to_owned(),
        n => format!("{n} underruns"),
    }
}

/// A short fill bar for the ring line.
fn fill_bar(ui: &mut egui::Ui, fraction: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(120.0, 4.0), Sense::hover());
    let track = Color32::from_rgb(27, 34, 46);
    ui.painter().rect_filled(rect, CornerRadius::same(2), track);
    let filled = Rect::from_min_size(
        rect.min,
        Vec2::new(rect.width() * fraction.clamp(0.0, 1.0), rect.height()),
    );
    ui.painter()
        .rect_filled(filled, CornerRadius::same(2), AMBER);
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

/// The three tabs fit the strip when this is true.  What is left of the line
/// after the readings is the width that matters.
fn cushion_modes_fit(ui: &egui::Ui) -> bool {
    cushion_tab_row_width(ui) <= ui.available_width()
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
