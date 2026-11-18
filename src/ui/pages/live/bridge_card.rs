//! The bridge strip under the Input and Output cards.  One shared clock is
//! a single line.  Two clocks show the fill, the drift and the bridge
//! buffer mode.  The controls live on Setup.

use eframe::egui;
use egui::{Align, Color32, CornerRadius, Layout, Margin, Rect, RichText, Sense, Stroke, Vec2};

use crate::app::issues::IssueCard;
use crate::app::RigWerkApp;
use crate::audio::{BufferMode, EngineState};
use crate::ui::theme::ts;
use crate::ui::theme::{AMBER, BORDER, CARD, GREEN, MUTED, TEXT};
use crate::ui::widgets::{micro_label, mono};

impl RigWerkApp {
    /// The full-width bridge strip.  One clock is one line.  Two clocks
    /// keep the fill, the drift and the bridge buffer mode on the same line.
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
                RichText::new("One clock: no bridge buffer needed.")
                    .size(ts(12.0))
                    .color(TEXT),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(mono(dropout_text(self.bridge_dropouts()), MUTED, 11.0));
            });
        });
    }

    fn bridge_ring_line(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let fill = self.bridge_fill(ctx);
        let percent = format!("{}%", (fill * 100.0).round() as i32);
        ui.horizontal(|ui| {
            ui.label(micro_label("FILL", AMBER));
            fill_bar(ui, fill);
            ui.label(mono(percent, TEXT, 11.0));
            self.drift_reading(ui);
            ui.label(mono(dropout_text(self.bridge_dropouts()), MUTED, 11.0));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(mono(
                    bridge_mode_label(self.bridge_buffer_mode),
                    MUTED,
                    11.0,
                ));
            });
        });
    }

    pub(crate) fn bridge_fill(&self, ctx: &egui::Context) -> f32 {
        let fill = if self.engine.state == EngineState::Running {
            self.snapshot.ring_fill
        } else if self.engine.state == EngineState::Preview {
            0.38 + (ctx.input(|input| input.time) as f32 * 0.8).sin() * 0.04
        } else {
            0.0
        };
        fill.clamp(0.0, 1.0)
    }

    pub(crate) fn bridge_dropouts(&self) -> u64 {
        if self.engine.state == EngineState::Running {
            self.snapshot.dropouts
        } else {
            0
        }
    }

    /// The drift reading.  Drift correction is set on Setup.
    fn drift_reading(&mut self, ui: &mut egui::Ui) {
        let text = if !self.drift {
            "drift off".to_owned()
        } else if self.engine.state == EngineState::Running {
            format!("drift {:+.1} ppm", self.snapshot.drift_ppm)
        } else {
            "drift auto".to_owned()
        };
        let color = if self.drift { GREEN } else { MUTED };
        ui.label(mono(text, color, 11.0));
    }
}

fn bridge_mode_label(mode: BufferMode) -> &'static str {
    match mode {
        BufferMode::Auto => "Auto",
        BufferMode::Minimum => "Minimum",
        BufferMode::Fixed => "Fixed",
    }
}

fn dropout_text(count: u64) -> String {
    match count {
        1 => "1 dropout".to_owned(),
        n => format!("{n} dropouts"),
    }
}

/// A short fill bar.
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
