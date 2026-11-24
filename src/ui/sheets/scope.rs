//! The scope sheet (`docs/ui-spec.md` §3.3): the input and output traces
//! overlaid, their peaks, the sample count and the estimate.
//!
//! The curves are the peak-hold bins the frame publishes, not a decorative
//! oscillator.  The sheet reads the palette, so it looks the same in both
//! themes as the console behind it.

use eframe::egui;
use egui::{Align2, CornerRadius, Rect, RichText, Sense, Stroke, Vec2};

use crate::app::screen::format_ms;
use crate::app::RigWerkApp;
use crate::audio::EngineState;
use crate::ui::fonts::Face;
use crate::ui::scope::{amp_to_db, format_compact};
use crate::ui::theme::{font, palette, Palette, TypeRole};

/// The trace area, and the height the console's sheet leaves for it.
const TRACE_H: f32 = 220.0;

impl RigWerkApp {
    /// The scope sheet's body.
    pub(crate) fn scope_sheet(&mut self, ui: &mut egui::Ui) {
        let colours = palette(ui.ctx());
        let width = ui.available_width().max(240.0);
        let active = self.engine.state == EngineState::Running;
        let ctx = ui.ctx().clone();
        let frames = if self.engine.state == EngineState::Running {
            self.snapshot.frames
        } else if self.engine.state == EngineState::Preview {
            (ctx.input(|input| input.time) * self.sample_rate as f64) as u64
        } else {
            0
        };

        ui.horizontal(|ui| {
            legend(ui, colours, colours.meter, "Input");
            ui.add_space(8.0);
            legend(ui, colours, colours.accent, "Output");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new(if active { "Live" } else { "Idle" })
                        .font(font(Face::Mono, TypeRole::GridHeader))
                        .color(if active {
                            colours.accent_text
                        } else {
                            colours.text3
                        }),
                );
            });
        });
        ui.add_space(6.0);

        let (rect, _) = ui.allocate_exact_size(Vec2::new(width, TRACE_H), Sense::hover());
        draw(
            ui,
            rect,
            &self.meters.scope_in,
            &self.meters.scope_out,
            active,
            colours,
        );

        ui.add_space(8.0);
        let peak_in = amp_to_db(self.meters.peak_in_l.max(self.meters.peak_in_r));
        let peak_out = amp_to_db(self.meters.peak_out_l.max(self.meters.peak_out_r));
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 24.0;
            stat(ui, "Peak in", &format!("{peak_in:.1} dBFS"), colours);
            stat(ui, "Peak out", &format!("{peak_out:.1} dBFS"), colours);
            stat(ui, "Estimated", &format_ms(self.latency_ms()), colours);
            stat(
                ui,
                "Samples",
                &if frames == 0 {
                    "—".to_owned()
                } else {
                    format_compact(frames)
                },
                colours,
            );
        });
        ui.label(
            RichText::new("Input and output share one time base. The peaks hold the highest sample since the last reset.")
                .font(font(Face::Sans, TypeRole::GridHeader))
                .color(colours.text3),
        );
    }
}

fn legend(ui: &mut egui::Ui, colours: &Palette, colour: egui::Color32, name: &str) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 5.0;
        let (dot, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
        ui.painter().circle_filled(dot.center(), 3.5, colour);
        ui.label(
            RichText::new(name)
                .font(font(Face::Sans, TypeRole::Secondary))
                .color(colours.text2),
        );
    });
    let _ = colours;
}

fn stat(ui: &mut egui::Ui, name: &str, value: &str, colours: &Palette) {
    ui.vertical(|ui| {
        ui.label(
            RichText::new(name)
                .font(font(Face::Sans, TypeRole::GridHeader))
                .color(colours.text3),
        );
        ui.label(
            RichText::new(value)
                .font(font(Face::Mono, TypeRole::Secondary))
                .color(colours.text),
        );
    });
}

fn draw(ui: &egui::Ui, rect: Rect, input: &[f32], output: &[f32], active: bool, colours: &Palette) {
    let painter = ui.painter().with_clip_rect(rect);
    painter.rect_filled(rect, CornerRadius::same(4), colours.well);
    painter.rect_stroke(
        rect,
        CornerRadius::same(4),
        Stroke::new(1.0, colours.line),
        egui::StrokeKind::Inside,
    );
    let inner = rect.shrink(6.0);
    for step in 1..8 {
        let x = inner.left() + inner.width() * step as f32 / 8.0;
        painter.vline(x, inner.y_range(), Stroke::new(0.6, colours.line));
    }
    for step in 1..4 {
        let y = inner.top() + inner.height() * step as f32 / 4.0;
        painter.hline(inner.x_range(), y, Stroke::new(0.6, colours.line));
    }
    painter.hline(
        inner.x_range(),
        inner.center().y,
        Stroke::new(0.7, colours.line2),
    );
    if active {
        trace(&painter, inner, input, colours.meter);
        trace(&painter, inner, output, colours.accent);
    } else {
        painter.text(
            inner.center(),
            Align2::CENTER_CENTER,
            "Start the bridge to see the scope",
            font(Face::Sans, TypeRole::Secondary),
            colours.text3,
        );
    }
}

fn trace(painter: &egui::Painter, rect: Rect, samples: &[f32], colour: egui::Color32) {
    if samples.is_empty() {
        return;
    }
    let last = (samples.len() - 1).max(1) as f32;
    let half = rect.height() * 0.45;
    let points: Vec<egui::Pos2> = samples
        .iter()
        .enumerate()
        .map(|(index, sample)| {
            let x = rect.left() + rect.width() * index as f32 / last;
            let y = rect.center().y - sample.clamp(-1.0, 1.0) * half;
            egui::pos2(x, y)
        })
        .collect();
    painter.add(egui::Shape::line(points, Stroke::new(1.35, colour)));
}
