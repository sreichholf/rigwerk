//! The measured loopback on Live: the compact readout in the health card and
//! the panel window that arms, explains, retries and cancels a loopback
//! measurement.
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
use crate::ui::theme::ts;
use crate::ui::theme::{with_alpha, AMBER, BORDER, CARD, DIM, GREEN, MUTED, TEAL, TEXT};
use crate::ui::widgets::{micro_label, mono, progress_line};

impl RigWerkApp {
    /// One row in the Live health card: the stored measurement, what it is
    /// doing now, or that it was never measured.  The panel opens from here.
    pub(crate) fn calibration_readout(&mut self, ui: &mut egui::Ui) {
        let measuring = self.calibration.is_measuring();
        let measured = self.measured_latency.clone();
        let stale = measured
            .as_ref()
            .and_then(|measured| measured.stale_reason(&self.latency_conditions()));
        let progress = self.calibration_progress();
        let mut open = false;
        let mut cancel = false;
        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (label, color) = if measuring {
                    ("Cancel", AMBER)
                } else {
                    ("Measure…", TEAL)
                };
                if small_button(ui, label, color) {
                    if measuring {
                        cancel = true;
                    } else {
                        open = true;
                    }
                }
                ui.add_space(8.0);
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.label(micro_label(MEASURED_LABEL, DIM));
                    if measuring {
                        ui.label(mono(
                            format!("measuring · {:.0}%", progress * 100.0),
                            AMBER,
                            9.5,
                        ))
                        .on_hover_text(
                            "Playing a short click through the loopback with the mix muted. \
                             This is a measurement, not an estimate.",
                        );
                    } else if let Some(measured) = &measured {
                        let color = if stale.is_some() { AMBER } else { TEAL };
                        ui.label(mono(format!("{:.1} ms", measured.latency_ms), color, 9.5))
                        .on_hover_text(format!(
                            "Measured loopback (OUT → {} → IN), {} samples at {:.1} kHz; not an estimate.\n{}\nMeasured {:.0}%.{}",
                            measured.loopback.label(),
                            measured.samples,
                            measured.input_rate as f32 / 1000.0,
                            measured.summary(stale.as_deref()),
                            measured.confidence * 100.0,
                            if stale.is_some() {
                                "\nMeasured under different settings, so it is stale."
                            } else {
                                ""
                            }
                        ));
                    } else {
                        ui.label(mono("not measured", MUTED, 9.5)).on_hover_text(
                            "No loopback measurement yet. Measure… plays a click through a \
                             cable or microphone loop; the estimate above is not a measurement.",
                        );
                    }
                });
            });
        });
        if open {
            self.calibration.open = true;
        }
        if cancel {
            self.cancel_calibration();
        }
    }

    /// The measurement panel window, in the same visual language as the
    /// plugin control panel.  It stays open on a result so the number can be
    /// read, and closes without leaving the mix ducked.
    pub(crate) fn calibration_window(&mut self, ctx: &egui::Context) {
        if !self.calibration.open {
            return;
        }
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
                    .fill(CARD)
                    .stroke(Stroke::new(1.0, BORDER))
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
                    ui.painter()
                        .rect_filled(rect, CornerRadius::same(9), with_alpha(TEAL, 14));
                    ui.painter().rect_stroke(
                        rect,
                        CornerRadius::same(9),
                        Stroke::new(1.0, with_alpha(TEAL, 38)),
                        egui::StrokeKind::Inside,
                    );
                    ui.painter().text(
                        rect.center(),
                        Align2::CENTER_CENTER,
                        "RT",
                        FontId::new(ts(9.0), FontFamily::Monospace),
                        TEAL,
                    );
                    ui.add_space(2.0);
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("Measure the loopback")
                                .size(ts(14.0))
                                .color(TEXT)
                                .strong(),
                        );
                        ui.label(
                            RichText::new(
                                "Plays a short click, hears it come back, and reports the loopback's delay.",
                            )
                            .size(ts(10.0))
                            .color(MUTED),
                        );
                    });
                    ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                        if small_button(ui, "Close", MUTED) {
                            close = true;
                        }
                    });
                });
                ui.add_space(12.0);

                // How the loop is closed.
                ui.label(micro_label("LOOPBACK", DIM));
                ui.add_space(4.0);
                ui.add_enabled_ui(!measuring, |ui| {
                    ui.horizontal(|ui| {
                        let mut pick = self.calibration.loopback;
                        if hinted_loopback_tab(ui, "Cable", pick == LoopbackKind::Cable) {
                            pick = LoopbackKind::Cable;
                        }
                        if hinted_loopback_tab(
                            ui,
                            "Microphone",
                            pick == LoopbackKind::Microphone,
                        ) {
                            pick = LoopbackKind::Microphone;
                        }
                        if pick != self.calibration.loopback {
                            self.calibration.loopback = pick;
                        }
                    });
                });
                ui.add_space(5.0);
                ui.add(
                    egui::Label::new(
                        RichText::new(loopback.advice()).size(ts(10.5)).color(MUTED),
                    )
                    .wrap(),
                );
                ui.add_space(12.0);

                // The exact rig this measures, and whether it may run.
                egui::Frame::new()
                    .fill(Color32::from_rgb(12, 16, 25))
                    .stroke(Stroke::new(1.0, BORDER))
                    .corner_radius(CornerRadius::same(9))
                    .inner_margin(Margin::symmetric(12, 9))
                    .show(ui, |ui| {
                        ui.set_width(496.0);
                        condition_row(ui, "INPUT", &format!(
                            "{} · {:.1} kHz · {} smp",
                            conditions.input_name,
                            conditions.input_rate as f32 / 1000.0,
                            conditions.input_buffer
                        ));
                        condition_row(ui, "OUTPUT", &format!(
                            "{} · {:.1} kHz · {} smp{}",
                            conditions.output_name,
                            conditions.output_rate as f32 / 1000.0,
                            conditions.output_step,
                            output_step
                                .map(|(frames, ms)| format!(" (live step {frames}, {ms:.1} ms)"))
                                .unwrap_or_default()
                        ));
                        condition_row(ui, "MODE", &conditions.output_mode.to_uppercase());
                        let resolution = (conditions.input_buffer as f32
                            / conditions.input_rate.max(1) as f32)
                            .max(conditions.output_step as f32 / conditions.output_rate.max(1) as f32)
                            * 1000.0;
                        condition_row(
                            ui,
                            "RESOLUTION",
                            &format!(
                                "±{resolution:.1} ms (one driver block; the stamps are taken at the callback)"
                            ),
                        );
                    });
                ui.add_space(12.0);

                if measuring {
                    let percent = format!("{}%", (progress * 100.0).round() as i32);
                    progress_line(ui, "CAPTURE", &percent, progress, AMBER);
                    ui.add_space(6.0);
                    ui.add(
                        egui::Label::new(
                            RichText::new(
                                "The mix on the main output is muted while it measures. The click ignores that output's mute and fader, so a muted output still plays it. Extra outputs keep playing. Keep the loopback connected.",
                            )
                            .size(ts(10.5))
                            .color(AMBER),
                        )
                        .wrap(),
                    );
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if panel_button(ui, "Cancel", MUTED) {
                            cancel = true;
                        }
                    });
                } else {
                    if let CalibrationPhase::Failed { reason } = &phase {
                        egui::Frame::new()
                            .fill(with_alpha(AMBER, 10))
                            .stroke(Stroke::new(1.0, with_alpha(AMBER, 45)))
                            .corner_radius(CornerRadius::same(9))
                            .inner_margin(Margin::symmetric(12, 9))
                            .show(ui, |ui| {
                                ui.set_width(496.0);
                                ui.label(
                                    RichText::new("The measurement did not finish")
                                        .size(ts(11.0))
                                        .color(AMBER)
                                        .strong(),
                                );
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(reason).size(ts(10.5)).color(MUTED),
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
                        );
                        ui.add_space(12.0);
                    }
                    ui.horizontal(|ui| {
                        let label = if measured.is_some() || matches!(phase, CalibrationPhase::Failed { .. }) {
                            "Measure again"
                        } else {
                            "Measure"
                        };
                        if panel_button(ui, label, TEAL) {
                            start = true;
                        }
                        ui.add_space(6.0);
                        if panel_button(ui, "Close", MUTED) {
                            close = true;
                        }
                    });
                    if let Some(reason) = blocker.as_deref() {
                        ui.add_space(8.0);
                        ui.add(
                            egui::Label::new(RichText::new(reason).size(ts(10.0)).color(DIM))
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
                            .color(DIM),
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
) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("{:.1}", measured.latency_ms))
                .family(FontFamily::Monospace)
                .size(ts(28.0))
                .color(if stale.is_some() { AMBER } else { GREEN }),
        );
        ui.vertical(|ui| {
            ui.add_space(6.0);
            ui.label(
                RichText::new(format!(
                    "ms · measured loopback (OUT → {} → IN)",
                    measured.loopback.label()
                ))
                .size(ts(11.0))
                .color(TEXT),
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
                .color(MUTED),
            );
        });
    });
    ui.add_space(6.0);
    ui.add(
        egui::Label::new(
            RichText::new(measured.summary(stale))
                .size(ts(10.0))
                .color(if stale.is_some() { AMBER } else { MUTED }),
        )
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
                    "Estimated path now {estimate_ms:.1} ms (derived; input block, cushion, chain, output step and after-node). It is a different leg, not this measurement's error."
                ),
            })
            .size(ts(10.0))
            .color(DIM),
        )
        .wrap(),
    );
    ui.add_space(4.0);
    ui.add(
        egui::Label::new(
            RichText::new(crate::app::budget::LEGS_NOTE)
                .size(ts(9.5))
                .color(DIM),
        )
        .wrap(),
    );
}

fn condition_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.add_sized([64.0, 14.0], egui::Label::new(mono(label, DIM, 8.5)));
        ui.add(egui::Label::new(RichText::new(value).size(ts(10.5)).color(TEXT)).wrap());
    });
}

fn small_button(ui: &mut egui::Ui, label: &str, color: Color32) -> bool {
    ui.add(
        egui::Button::new(RichText::new(label).size(ts(9.5)).color(color))
            .fill(with_alpha(color, 14))
            .stroke(Stroke::new(1.0, with_alpha(color, 45)))
            .corner_radius(CornerRadius::same(7)),
    )
    .clicked()
}

fn panel_button(ui: &mut egui::Ui, label: &str, color: Color32) -> bool {
    ui.add(
        egui::Button::new(RichText::new(label).size(ts(11.0)).color(color))
            .fill(with_alpha(color, 14))
            .stroke(Stroke::new(1.0, with_alpha(color, 45)))
            .corner_radius(CornerRadius::same(8))
            .min_size(Vec2::new(110.0, 30.0)),
    )
    .clicked()
}

/// One of the two loopback choices, with its explanation on hover.
pub(super) fn hinted_loopback_tab(ui: &mut egui::Ui, label: &str, selected: bool) -> bool {
    let hint = match label {
        "Cable" => LoopbackKind::Cable.advice(),
        _ => LoopbackKind::Microphone.advice(),
    };
    crate::ui::widgets::hinted_tab(ui, label, selected, TEAL, hint)
}

/// Kept next to the readout so the Live card's wording cannot drift from the
/// panel's.
pub(crate) const MEASURED_LABEL: &str = "MEASURED";
