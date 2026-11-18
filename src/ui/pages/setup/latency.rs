//! The clock-output estimate, the measurement, and Measure latency.

use eframe::egui;
use egui::{CornerRadius, Rect, RichText, Sense, Vec2};

use crate::app::latency::output_mode_phrase;
use crate::app::screen::{format_khz, format_ms, SetupSection};
use crate::app::RigWerkApp;
use crate::ui::fonts::Face;
use crate::ui::pages::setup::{hint, panel};
use crate::ui::theme::{font, palette, TypeRole};

impl RigWerkApp {
    pub(super) fn setup_latency(&mut self, ui: &mut egui::Ui, section: Option<SetupSection>) {
        let estimate = self.heard_estimate();
        let measured = self.measured_latency.clone();
        let stale = measured
            .as_ref()
            .and_then(|measured| measured.stale_reason(&self.latency_conditions()));
        let measuring = self.calibration.is_measuring();
        panel(ui, "Latency", section, Some(SetupSection::Latency), |ui| {
            let colours = palette(ui.ctx());
            ui.label(
                RichText::new(format!("≈ {}", format_ms(estimate.total_ms)))
                    .font(font(Face::Sans, TypeRole::TunerCents))
                    .color(colours.text),
            );
            if let Some(path) = &estimate.path {
                hint(
                    ui,
                    &format!("Slowest audible path on the clock output: {path}"),
                );
            } else {
                hint(ui, "No audible path on the clock output.");
            }
            ui.add_space(8.0);
            stacked_bar(ui, &estimate);
            ui.add_space(8.0);
            match &measured {
                Some(measured) => {
                    let phrase = crate::app::latency::output_mode_from_key(&measured.output_mode)
                        .map(output_mode_phrase)
                        .unwrap_or("saved mode");
                    let line = format!(
                        "Measured {} · {} / {} · {} / {} samples · {phrase} · {}",
                        format_ms(measured.latency_ms),
                        format_khz(measured.input_rate),
                        format_khz(measured.output_rate),
                        measured.input_buffer,
                        measured.output_step,
                        crate::app::latency::utc_stamp(measured.measured_at),
                    );
                    ui.label(
                        RichText::new(line)
                            .font(font(Face::Sans, TypeRole::Body))
                            .color(if stale.is_some() {
                                colours.warn
                            } else {
                                colours.text
                            }),
                    );
                    if let Some(reason) = &stale {
                        ui.label(
                            RichText::new(format!("Stale: {reason}"))
                                .font(font(Face::Sans, TypeRole::Secondary))
                                .color(colours.warn),
                        );
                    }
                }
                None => hint(ui, "Not measured."),
            }
            ui.add_space(8.0);
            let label = if measuring {
                "Measuring…"
            } else {
                "Measure latency…"
            };
            if ui
                .add_enabled(!measuring, egui::Button::new(label))
                .clicked()
            {
                self.calibration.open = true;
            }
        });
    }
}

fn stacked_bar(ui: &mut egui::Ui, estimate: &crate::app::budget::HeardEstimate) {
    let colours = palette(ui.ctx());
    let swatches = [
        colours.accent,
        colours.path[0],
        colours.path[1],
        colours.path[2],
        colours.path[3],
        colours.text3,
    ];
    let total = estimate
        .parts
        .iter()
        .filter_map(|part| part.ms)
        .map(|ms| ms.max(0.0))
        .sum::<f32>()
        .max(0.001);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 14.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(3), colours.well);
    let mut x = rect.left();
    for (index, part) in estimate.parts.iter().enumerate() {
        let ms = part.ms.unwrap_or(0.0).max(0.0);
        let width = rect.width() * ms / total;
        let slice = Rect::from_min_size(egui::pos2(x, rect.top()), Vec2::new(width, rect.height()));
        ui.painter()
            .rect_filled(slice, CornerRadius::ZERO, swatches[index % swatches.len()]);
        x += width;
    }
    ui.add_space(6.0);
    for (index, part) in estimate.parts.iter().enumerate() {
        let value = match part.ms {
            Some(ms) => format_ms(ms),
            None => "unknown".to_owned(),
        };
        ui.horizontal(|ui| {
            let (dot, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
            ui.painter()
                .circle_filled(dot.center(), 3.0, swatches[index % swatches.len()]);
            ui.label(
                RichText::new(format!("{} {value}", part.label))
                    .font(font(Face::Sans, TypeRole::Secondary))
                    .color(colours.text2),
            );
        });
    }
}
