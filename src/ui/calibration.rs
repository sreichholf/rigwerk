//! The measured loopback: the panel window that arms, explains, retries and
//! cancels a loopback measurement (`docs/ui-spec.md` §6.6).
//!
//! A measured number and the estimate are never shown as one value.  The
//! panel says which device pair, rates and buffers it measured under, and
//! marks the stored result stale (with the reason) as soon as the rig no
//! longer matches.

use eframe::egui;
use egui::{
    Align, Align2, Color32, CornerRadius, FontFamily, FontId, Layout, Margin, RichText, Stroke,
    Vec2,
};

use crate::app::calibration::CalibrationPhase;
use crate::app::latency::{utc_stamp, LoopbackKind};
use crate::app::RigWerkApp;
use crate::ui::fonts::Face;
use crate::ui::theme::ts;
use crate::ui::theme::{font, palette, with_alpha, Palette, TypeRole};
use crate::ui::widgets::{micro_label, mono, progress_line, segmented_hinted};

impl RigWerkApp {
    /// One row in the Live health card: the stored measurement, what it is
    /// read, and closes without leaving the mix ducked.
    pub(crate) fn calibration_window(&mut self, ctx: &egui::Context) {
        if !self.calibration.open {
            return;
        }
        let colours = palette(ctx);
        // A quiet button's frame and label are the same token; the accent's
        // two differ, because `accent` is a fill and `accent_text` reads.
        let quiet = colours.text2;
        let mut close = false;
        let mut start = false;
        let mut cancel = false;
        let measuring = self.calibration.is_measuring();
        let escape =
            ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        if escape {
            close = true;
        }
        let conditions = self.latency_conditions();
        let blocker = self.calibration_blocker();
        let budget = self.latency_budget();
        let measured = self.measured_latency.clone();
        let stale = measured
            .as_ref()
            .and_then(|measured| measured.stale_reason(&conditions));
        let progress = self.calibration_progress();
        let phase = self.calibration.phase.clone();
        // The loopback the measurement started with, not a tab clicked during
        // it; the tabs are disabled while it runs.
        let loopback = match &phase {
            CalibrationPhase::Measuring { loopback, .. } => *loopback,
            _ => self.calibration.loopback,
        };
        let output_step = self.output_step();

        egui::Window::new("Measure loopback")
            .id(egui::Id::unique("rigwerk_calibration"))
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .movable(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .default_width(520.0)
            .frame(
                egui::Frame::new()
                    .fill(colours.strip)
                    .stroke(Stroke::new(1.0, colours.line2))
                    .corner_radius(CornerRadius::same(14))
                    .inner_margin(Margin::same(18))
                    .shadow(egui::epaint::Shadow {
                        offset: [0, 10],
                        blur: 28,
                        spread: 0,
                        color: Color32::from_black_alpha(120),
                    }),
            )
            .show(ctx, |ui| {
                ui.set_width(520.0);
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(32.0), egui::Sense::hover());
                    ui.painter().rect_filled(
                        rect,
                        CornerRadius::same(9),
                        with_alpha(colours.accent, 14),
                    );
                    ui.painter().rect_stroke(
                        rect,
                        CornerRadius::same(9),
                        Stroke::new(1.0, with_alpha(colours.accent, 38)),
                        egui::StrokeKind::Inside,
                    );
                    ui.painter().text(
                        rect.center(),
                        Align2::CENTER_CENTER,
                        "RT",
                        FontId::new(ts(9.0), FontFamily::Monospace),
                        colours.accent_text,
                    );
                    ui.add_space(2.0);
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("Measure the loopback")
                                .size(ts(14.0))
                                .color(colours.text)
                                .strong(),
                        );
                        ui.label(
                            RichText::new(
                                "Plays a short click, hears it come back, and reports the loopback's delay.",
                            )
                            .size(ts(10.0))
                            .color(colours.text2),
                        );
                    });
                    ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                        if small_button(ui, "Close", quiet, quiet) {
                            close = true;
                        }
                    });
                });
                ui.add_space(12.0);

                // How the loop is closed.
                ui.label(micro_label("LOOPBACK", colours.text3));
                ui.add_space(4.0);
                ui.add_enabled_ui(!measuring, |ui| {
                    let kinds = [LoopbackKind::Cable, LoopbackKind::Microphone];
                    let hints = kinds.map(LoopbackKind::advice);
                    let mut pick =
                        usize::from(self.calibration.loopback == LoopbackKind::Microphone);
                    if segmented_hinted(ui, &mut pick, &["Cable", "Microphone"], &hints).changed() {
                        self.calibration.loopback = kinds[pick];
                    }
                });
                ui.add_space(5.0);
                ui.add(
                    egui::Label::new(
                        RichText::new(loopback.advice()).size(ts(10.5)).color(colours.text2),
                    )
                    .wrap(),
                );
                ui.add_space(12.0);

                // The exact rig this measures, and whether it may run.
                egui::Frame::new()
                    .fill(colours.bg)
                    .stroke(Stroke::new(1.0, colours.line2))
                    .corner_radius(CornerRadius::same(9))
                    .inner_margin(Margin::symmetric(12, 9))
                    .show(ui, |ui| {
                        ui.set_width(496.0);
                        let input = format!(
                            "{} · {:.1} kHz · {} smp",
                            conditions.input_name,
                            conditions.input_rate as f32 / 1000.0,
                            conditions.input_buffer
                        );
                        condition_row(ui, "INPUT", &input, colours);
                        let live_step = output_step
                            .map(|(frames, ms)| format!(" (live step {frames}, {ms:.1} ms)"))
                            .unwrap_or_default();
                        let output = format!(
                            "{} · {:.1} kHz · {} smp{live_step}",
                            conditions.output_name,
                            conditions.output_rate as f32 / 1000.0,
                            conditions.output_step
                        );
                        condition_row(ui, "OUTPUT", &output, colours);
                        condition_row(ui, "MODE", &conditions.output_mode.to_uppercase(), colours);
                        let resolution = (conditions.input_buffer as f32
                            / conditions.input_rate.max(1) as f32)
                            .max(conditions.output_step as f32 / conditions.output_rate.max(1) as f32)
                            * 1000.0;
                        let resolution_note = format!(
                            "±{resolution:.1} ms (one driver block; the stamps are taken at the callback)"
                        );
                        condition_row(ui, "RESOLUTION", &resolution_note, colours);
                    });
                ui.add_space(12.0);

                if measuring {
                    let percent = format!("{}%", (progress * 100.0).round() as i32);
                    progress_line(ui, "CAPTURE", &percent, progress, colours.warn);
                    ui.add_space(6.0);
                    ui.add(
                        egui::Label::new(
                            RichText::new(
                                "The mix on the main output is muted while it measures. The click ignores that output's mute and fader, so a muted output still plays it. Extra outputs keep playing. Keep the loopback connected.",
                            )
                            .size(ts(10.5))
                            .color(colours.warn),
                        )
                        .wrap(),
                    );
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if panel_button(ui, "Cancel", quiet, quiet) {
                            cancel = true;
                        }
                    });
                } else {
                    if let CalibrationPhase::Failed { reason } = &phase {
                        egui::Frame::new()
                            .fill(with_alpha(colours.warn, 10))
                            .stroke(Stroke::new(1.0, with_alpha(colours.warn, 45)))
                            .corner_radius(CornerRadius::same(9))
                            .inner_margin(Margin::symmetric(12, 9))
                            .show(ui, |ui| {
                                ui.set_width(496.0);
                                ui.label(
                                    RichText::new("The measurement did not finish")
                                        .size(ts(11.0))
                                        .color(colours.warn)
                                        .strong(),
                                );
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(reason).size(ts(10.5)).color(colours.text2),
                                    )
                                    .wrap(),
                                );
                            });
                        ui.add_space(10.0);
                    }
                    if let Some(measured) = &measured {
                        measured_summary(
                            ui,
                            measured,
                            stale.as_deref(),
                            budget.estimate_ms,
                            budget.estimate_gap.as_deref(),
                            colours,
                        );
                        ui.add_space(12.0);
                    }
                    ui.horizontal(|ui| {
                        let label = if measured.is_some() || matches!(phase, CalibrationPhase::Failed { .. }) {
                            "Measure again"
                        } else {
                            "Measure"
                        };
                        if panel_button(ui, label, colours.accent, colours.accent_text) {
                            start = true;
                        }
                        ui.add_space(6.0);
                        if panel_button(ui, "Close", quiet, quiet) {
                            close = true;
                        }
                    });
                    if let Some(reason) = blocker.as_deref() {
                        ui.add_space(8.0);
                        ui.add(
                            egui::Label::new(
                                RichText::new(reason).size(ts(10.0)).color(colours.text3),
                            )
                            .wrap(),
                        );
                    } else {
                        ui.add_space(8.0);
                        ui.label(
                            RichText::new(
                                "The result replaces the previous one only when a click is heard \
                                 and correlated. The click ignores the clock output's mute and \
                                 fader, so a muted output still plays it.",
                            )
                            .size(ts(10.0))
                            .color(colours.text3),
                        );
                    }
                }
            });

        // A start and a close in the same frame must not close the panel on a
        // measurement that just began; the start wins.
        if start && self.calibration_blocker().is_none() {
            self.start_calibration();
            self.calibration.open = true;
        } else if close {
            if measuring {
                // Closing the panel must not leave the output ducked.
                self.cancel_calibration();
            }
            self.calibration.open = false;
        } else if cancel {
            self.cancel_calibration();
        }
    }
}

/// The stored result: value, conditions, date, confidence, and the estimate
/// beside it.  The two cover different legs, so they are never subtracted.
fn measured_summary(
    ui: &mut egui::Ui,
    measured: &crate::app::latency::MeasuredLatency,
    stale: Option<&str>,
    estimate_ms: f32,
    estimate_gap: Option<&str>,
    colours: &Palette,
) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("{:.1}", measured.latency_ms))
                .family(FontFamily::Monospace)
                .size(ts(28.0))
                .color(if stale.is_some() {
                    colours.warn
                } else {
                    colours.ok_text
                }),
        );
        ui.vertical(|ui| {
            ui.add_space(6.0);
            ui.label(
                RichText::new(format!(
                    "ms · measured loopback (OUT → {} → IN)",
                    measured.loopback.label()
                ))
                .size(ts(11.0))
                .color(colours.text),
            );
            ui.label(
                RichText::new(format!(
                    "{} samples at {:.1} kHz · {:.0}% confidence · {}",
                    measured.samples,
                    measured.input_rate as f32 / 1000.0,
                    measured.confidence * 100.0,
                    utc_stamp(measured.measured_at)
                ))
                .size(ts(10.0))
                .color(colours.text2),
            );
        });
    });
    ui.add_space(6.0);
    ui.add(
        egui::Label::new(RichText::new(measured.summary(stale)).size(ts(10.0)).color(
            if stale.is_some() {
                colours.warn
            } else {
                colours.text2
            },
        ))
        .wrap(),
    );
    ui.add_space(4.0);
    ui.add(
        egui::Label::new(
            RichText::new(match estimate_gap {
                Some(gap) => format!(
                    "Estimated path now {estimate_ms:.1} ms (derived; {gap}). It is a different leg, not this measurement's error."
                ),
                None => format!(
                    "Estimated path now {estimate_ms:.1} ms (derived; input block, bridge buffer, chain, output step and after-node). It is a different leg, not this measurement's error."
                ),
            })
            .size(ts(10.0))
            .color(colours.text3),
        )
        .wrap(),
    );
    ui.add_space(4.0);
    ui.add(
        egui::Label::new(
            RichText::new(crate::app::budget::LEGS_NOTE)
                .size(ts(9.5))
                .color(colours.text3),
        )
        .wrap(),
    );
}

fn condition_row(ui: &mut egui::Ui, label: &str, value: &str, colours: &Palette) {
    ui.horizontal(|ui| {
        ui.add_sized(
            [64.0, 14.0],
            egui::Label::new(mono(label, colours.text3, 8.5)),
        );
        ui.add(egui::Label::new(RichText::new(value).size(ts(10.5)).color(colours.text)).wrap());
    });
}

/// A small panel button.  `frame` tints its fill and border, `text` labels it:
/// the accent's two tokens differ, because `accent` is a fill and
/// `accent_text` reads on the light theme.
fn small_button(ui: &mut egui::Ui, label: &str, frame: Color32, text: Color32) -> bool {
    ui.add(
        egui::Button::new(
            RichText::new(label)
                .font(font(Face::Sans, TypeRole::Control))
                .color(text),
        )
        .fill(with_alpha(frame, 14))
        .stroke(Stroke::new(1.0, with_alpha(frame, 45)))
        .corner_radius(CornerRadius::same(7)),
    )
    .clicked()
}

/// The panel's larger button.  `frame` and `text` as [`small_button`].
fn panel_button(ui: &mut egui::Ui, label: &str, frame: Color32, text: Color32) -> bool {
    ui.add(
        egui::Button::new(
            RichText::new(label)
                .font(font(Face::Sans, TypeRole::Control))
                .color(text),
        )
        .fill(with_alpha(frame, 14))
        .stroke(Stroke::new(1.0, with_alpha(frame, 45)))
        .corner_radius(CornerRadius::same(8))
        .min_size(Vec2::new(110.0, 30.0)),
    )
    .clicked()
}
