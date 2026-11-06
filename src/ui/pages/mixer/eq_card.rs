//! Built-in EQ inspector: one of the two master-bus EQs (pre or post inserts).

use eframe::egui;
use egui::{
    Align, Align2, Color32, CornerRadius, FontFamily, FontId, Layout, Rect, RichText, Sense,
    Stroke, Vec2,
};

use crate::app::RigWerkApp;
use crate::audio::{
    BandKind, EngineState, EqSettings, EqSlot, EQ_BANDS, EQ_MAX_GAIN_DB, EQ_MAX_Q, EQ_MIN_Q,
};
use crate::ui::theme::ts;
use crate::ui::theme::{
    with_alpha, AMBER, BLUE, BORDER_SOFT, CARD_DEEP, CARD_RAISED, DIM, GREEN, MUTED, RED, TEAL,
    TEXT,
};
use crate::ui::widgets::{hinted_tab, mono, styled_slider};

/// One colour per band, low to high.
const BAND_COLORS: [Color32; 5] = [RED, AMBER, GREEN, TEAL, BLUE];
const CURVE_MIN_HZ: f32 = 20.0;
const CURVE_MAX_HZ: f32 = 20_000.0;
const READOUT_W: f32 = 58.0;
const LABEL_W: f32 = 42.0;

fn hz_label(hz: f32) -> String {
    if hz >= 1_000.0 {
        format!("{:.1} kHz", hz / 1_000.0)
    } else {
        format!("{hz:.0} Hz")
    }
}

/// Hertz after one move of the log-frequency slider.
fn hz_from_log_slider(previous_hz: f32, position: f32, min_hz: f32, max_hz: f32) -> f32 {
    let rounded = position.exp().round();
    let delta = position - previous_hz.ln();
    let next = if (rounded - previous_hz).abs() >= 0.5 {
        rounded
    } else if delta.abs() > 1.0e-6 && delta.abs() <= 0.02 {
        previous_hz + delta.signum()
    } else {
        previous_hz
    };
    next.clamp(min_hz, max_hz)
}

impl RigWerkApp {
    /// EQ controls for the Mixer inspector (curve on top, compact band rows).
    pub(crate) fn eq_inspector(&mut self, ui: &mut egui::Ui, slot: EqSlot) {
        let current = self.eqs[slot.index()];
        let mut eq = current;
        let rate = if self.engine.state == EngineState::Running && self.snapshot.output_rate > 0 {
            self.snapshot.output_rate
        } else {
            48_000
        };

        ui.horizontal(|ui| {
            ui.label(
                RichText::new(slot_title(slot))
                    .size(ts(16.0))
                    .color(TEXT)
                    .strong(),
            )
            .on_hover_text(slot_hint(slot));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if hinted_tab(
                    ui,
                    "FLAT",
                    false,
                    MUTED,
                    "Reset every band to its default, 0 dB.",
                ) {
                    eq = eq.flat();
                }
                ui.add_space(4.0);
                if hinted_tab(
                    ui,
                    if eq.enabled { "ON" } else { "BYP" },
                    eq.enabled,
                    AMBER,
                    "Switch the whole EQ on or off. Settings are kept while bypassed.",
                ) {
                    eq.enabled = !eq.enabled;
                }
            });
        });
        ui.label(
            RichText::new("5 bands · master mix · no added latency")
                .size(ts(11.0))
                .color(DIM),
        );
        ui.add_space(8.0);

        // One width for curve and band cards so the stack lines up exactly.
        let panel_w = ui.available_width();
        let curve_h = (panel_w * 0.22).clamp(100.0, 140.0);
        let (curve, _) = ui.allocate_exact_size(Vec2::new(panel_w, curve_h), Sense::hover());
        draw_curve(ui, curve, &eq, rate, true);
        ui.add_space(8.0);

        for (index, spec) in EQ_BANDS.iter().enumerate() {
            let color = BAND_COLORS[index];
            let band = &mut eq.bands[index];
            let live = eq.enabled && band.on;
            egui::Frame::new()
                .fill(CARD_RAISED)
                .stroke(Stroke::new(1.0, BORDER_SOFT))
                .corner_radius(CornerRadius::same(8))
                .inner_margin(egui::Margin::symmetric(8, 6))
                .show(ui, |ui| {
                    ui.set_width(panel_w - 16.0);
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(spec.name)
                                        .size(ts(11.0))
                                        .color(if live { color } else { DIM })
                                        .strong(),
                                )
                                .on_hover_text(format!(
                                    "{} {}",
                                    match spec.kind {
                                        BandKind::LowShelf => "Low shelf.",
                                        BandKind::HighShelf => "High shelf.",
                                        BandKind::Bell => "Bell.",
                                    },
                                    spec.role
                                ));
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if hinted_tab(
                                        ui,
                                        if band.on { "ON" } else { "OFF" },
                                        band.on,
                                        color,
                                        "Switch this band on or off.",
                                    ) {
                                        band.on = !band.on;
                                    }
                                });
                            });
                            ui.add_space(2.0);
                            band_slider_row(
                                ui,
                                "GAIN",
                                color,
                                |ui, band| {
                                    let gain = reserved_slider(
                                        ui,
                                        &mut band.gain_db,
                                        -EQ_MAX_GAIN_DB,
                                        EQ_MAX_GAIN_DB,
                                        0.1,
                                        color,
                                        format!("{} gain", spec.name),
                                    )
                                    .on_hover_text(
                                        "Boost or cut, ±12 dB in 0.1 dB steps. Double-click for 0 dB.",
                                    );
                                    if gain.double_clicked() {
                                        band.gain_db = 0.0;
                                    }
                                    format!("{:+.1} dB", band.gain_db)
                                },
                                band,
                                live,
                            );
                    band_slider_row(
                        ui,
                        "FREQ",
                        color,
                        |ui, band| {
                            let mut position = band.hz.ln();
                            let freq = reserved_slider(
                                ui,
                                &mut position,
                                spec.min_hz.ln(),
                                spec.max_hz.ln(),
                                0.001,
                                color,
                                format!("{} frequency", spec.name),
                            )
                            .on_hover_text(format!(
                                "{} to {}. Double-click for {}.",
                                hz_label(spec.min_hz),
                                hz_label(spec.max_hz),
                                hz_label(spec.default_hz)
                            ));
                            if freq.changed() {
                                band.hz = hz_from_log_slider(
                                    band.hz,
                                    position,
                                    spec.min_hz,
                                    spec.max_hz,
                                );
                            }
                            if freq.double_clicked() {
                                band.hz = spec.default_hz;
                            }
                            hz_label(band.hz)
                        },
                        band,
                        live,
                    );
                    band_slider_row(
                        ui,
                        "WIDTH",
                        color,
                        |ui, band| {
                            let width = reserved_slider(
                                ui,
                                &mut band.q,
                                EQ_MIN_Q,
                                EQ_MAX_Q,
                                0.1,
                                color,
                                format!("{} width", spec.name),
                            )
                            .on_hover_text(match spec.kind {
                                BandKind::Bell => format!(
                                    "Width: low Q is broad and musical, high Q is narrow for taming one spot. Double-click for Q {:.1}.",
                                    spec.default_q
                                ),
                                BandKind::LowShelf | BandKind::HighShelf => format!(
                                    "Shelf width: low Q is a gentle slope, high Q is steeper with a resonant bump before the corner and a dip after it. Double-click for Q {:.1}.",
                                    spec.default_q
                                ),
                            });
                            if width.double_clicked() {
                                band.q = spec.default_q;
                            }
                            format!("Q {:.1}", band.q)
                        },
                        band,
                        live,
                    );
                });
            ui.add_space(5.0);
        }

        let eq = eq.sanitized();
        if eq != current {
            let name = slot_title(slot);
            if eq.enabled != current.enabled {
                self.push_log(
                    if eq.enabled {
                        format!("{name} on.")
                    } else {
                        format!("{name} bypassed.")
                    },
                    AMBER,
                );
            } else if eq == current.flat() && current != current.flat() {
                self.push_log(format!("{name} reset to flat."), AMBER);
            }
            self.eqs[slot.index()] = eq;
            self.engine.set_eq(slot, eq);
        }
    }
}

fn band_slider_row(
    ui: &mut egui::Ui,
    label: &str,
    _color: Color32,
    edit: impl FnOnce(&mut egui::Ui, &mut crate::audio::EqBand) -> String,
    band: &mut crate::audio::EqBand,
    live: bool,
) {
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(
            Vec2::new(LABEL_W, 22.0),
            Layout::left_to_right(Align::Center),
            |ui| {
                ui.label(mono(label, DIM, 7.5));
            },
        );
        let slider_w = (ui.available_width() - READOUT_W - 6.0).max(80.0);
        let readout = ui
            .allocate_ui_with_layout(
                Vec2::new(slider_w, 22.0),
                Layout::left_to_right(Align::Center),
                |ui| {
                    ui.set_width(slider_w);
                    edit(ui, band)
                },
            )
            .inner;
        ui.allocate_ui_with_layout(
            Vec2::new(READOUT_W, 22.0),
            Layout::right_to_left(Align::Center),
            |ui| {
                ui.label(mono(readout, if live { TEXT } else { DIM }, 8.5));
            },
        );
    });
}

/// Like [`styled_slider`], but only as wide as the current ui — callers must
/// reserve space for labels/readouts first so nothing paints past the pane.
fn reserved_slider(
    ui: &mut egui::Ui,
    value: &mut f32,
    min: f32,
    max: f32,
    step: f32,
    color: Color32,
    name: String,
) -> egui::Response {
    styled_slider(ui, value, min, max, step, (color, name))
}

pub(crate) fn slot_title(slot: EqSlot) -> &'static str {
    match slot {
        EqSlot::Pre => "Pre EQ",
        EqSlot::Post => "Post EQ",
    }
}

pub(crate) fn slot_hint(slot: EqSlot) -> &'static str {
    match slot {
        EqSlot::Pre => {
            "Before the plugin inserts: shapes what the plugins receive, e.g. tighten the low end before an amp sim."
        }
        EqSlot::Post => {
            "After the plugin inserts: shapes what the plugins produce, e.g. tame fizz after an amp sim."
        }
    }
}

/// The combined response from 20 Hz to 20 kHz, ±12 dB, with a dot per band.
pub(crate) fn draw_curve(ui: &egui::Ui, rect: Rect, eq: &EqSettings, rate: u32, labels: bool) {
    use egui::epaint::{Mesh, Vertex, WHITE_UV};

    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, CornerRadius::same(9), CARD_DEEP);
    painter.rect_stroke(
        rect,
        CornerRadius::same(9),
        Stroke::new(1.0, BORDER_SOFT),
        egui::StrokeKind::Inside,
    );
    let plot = rect.shrink2(Vec2::new(8.0, 10.0));
    let span = (CURVE_MAX_HZ / CURVE_MIN_HZ).ln();
    let x_of = |hz: f32| plot.left() + plot.width() * (hz / CURVE_MIN_HZ).ln() / span;
    let range = EQ_MAX_GAIN_DB + 3.0;
    let y_of = |db: f32| plot.center().y - db.clamp(-range, range) / range * plot.height() / 2.0;
    let label = FontId::new(ts(7.5), FontFamily::Monospace);
    let dot = if labels { 4.0 } else { 2.5 };
    let zero_y = y_of(0.0);

    for db in [-12.0, -6.0, 6.0, 12.0] {
        painter.hline(
            plot.x_range(),
            y_of(db),
            Stroke::new(1.0, with_alpha(BORDER_SOFT, 140)),
        );
    }
    painter.hline(plot.x_range(), zero_y, Stroke::new(1.0, BORDER_SOFT));
    for (hz, text) in [(100.0, "100"), (1_000.0, "1k"), (10_000.0, "10k")] {
        let x = x_of(hz);
        painter.vline(
            x,
            plot.y_range(),
            Stroke::new(1.0, with_alpha(BORDER_SOFT, 140)),
        );
        if labels {
            painter.text(
                egui::pos2(x + 3.0, plot.bottom()),
                Align2::LEFT_BOTTOM,
                text,
                label.clone(),
                DIM,
            );
        }
    }
    if labels {
        painter.text(plot.left_top(), Align2::LEFT_TOP, "+12", label.clone(), DIM);
        painter.text(plot.left_bottom(), Align2::LEFT_BOTTOM, "−12", label, DIM);
    }

    let live = eq.enabled;
    let color = if live { AMBER } else { DIM };
    let steps = if labels { 200 } else { 120 };
    let sample = |db: &dyn Fn(f32) -> f32| -> Vec<egui::Pos2> {
        (0..=steps)
            .map(|step| {
                let hz = CURVE_MIN_HZ * (span * step as f32 / steps as f32).exp();
                egui::pos2(x_of(hz), y_of(db(hz)))
            })
            .collect()
    };
    let points = sample(&|hz| eq.response_db(hz, rate));
    let clipped = painter.with_clip_rect(plot);

    if live {
        let top = with_alpha(color, if labels { 52 } else { 64 });
        let bottom = with_alpha(color, 0);
        let mut mesh = Mesh::default();
        for pair in points.windows(2) {
            let (left, right) = (pair[0], pair[1]);
            let base = mesh.vertices.len() as u32;
            mesh.vertices.extend([
                Vertex {
                    pos: left,
                    uv: WHITE_UV,
                    color: top,
                },
                Vertex {
                    pos: egui::pos2(left.x, zero_y),
                    uv: WHITE_UV,
                    color: bottom,
                },
                Vertex {
                    pos: right,
                    uv: WHITE_UV,
                    color: top,
                },
                Vertex {
                    pos: egui::pos2(right.x, zero_y),
                    uv: WHITE_UV,
                    color: bottom,
                },
            ]);
            mesh.indices
                .extend([base, base + 1, base + 2, base + 1, base + 3, base + 2]);
        }
        clipped.add(egui::Shape::mesh(mesh));

        if labels {
            for (index, band) in eq.bands.iter().enumerate() {
                if !band.on || band.gain_db.abs() < 0.01 {
                    continue;
                }
                let trace = sample(&|hz| eq.band_response_db(index, hz, rate));
                clipped.add(egui::Shape::line(
                    trace,
                    Stroke::new(1.0, with_alpha(BAND_COLORS[index], 135)),
                ));
            }
        }

        clipped.add(egui::Shape::line(
            points.clone(),
            Stroke::new(if labels { 7.0 } else { 5.0 }, with_alpha(color, 24)),
        ));
    }
    clipped.add(egui::Shape::line(
        points,
        Stroke::new(if labels { 2.4 } else { 2.0 }, color),
    ));

    for (index, band) in eq.bands.iter().enumerate() {
        let on = live && band.on;
        let center = egui::pos2(
            x_of(band.hz),
            y_of(if live {
                eq.response_db(band.hz, rate)
            } else {
                0.0
            }),
        );
        let band_color = BAND_COLORS[index];
        if labels && on {
            painter.line_segment(
                [egui::pos2(center.x, zero_y), center],
                Stroke::new(1.0, with_alpha(band_color, 50)),
            );
        }
        painter.circle_filled(
            center,
            dot + if labels { 2.5 } else { 1.5 },
            with_alpha(band_color, if on { 42 } else { 16 }),
        );
        if on {
            painter.circle_filled(center, dot, band_color);
            painter.circle_stroke(
                center,
                dot,
                Stroke::new(1.0, with_alpha(Color32::WHITE, 80)),
            );
        } else {
            painter.circle_filled(center, (dot - 0.7).max(1.0), CARD_DEEP);
            painter.circle_stroke(center, dot, Stroke::new(1.2, with_alpha(DIM, 190)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::hz_from_log_slider;

    #[test]
    fn one_arrow_step_moves_a_low_frequency_by_one_hertz() {
        let hz = 40.0;
        let up = hz_from_log_slider(hz, hz.ln() + 0.001, 20.0, 20_000.0);
        let down = hz_from_log_slider(hz, hz.ln() - 0.001, 20.0, 20_000.0);
        assert_eq!(up, 41.0);
        assert_eq!(down, 39.0);
    }

    #[test]
    fn a_slider_move_that_crosses_a_hertz_keeps_the_rounded_value() {
        let hz = 1_000.0;
        let next = hz_from_log_slider(hz, 1_008.4_f32.ln(), 20.0, 20_000.0);
        assert_eq!(next, 1_008.0);
    }

    #[test]
    fn the_frequency_slider_stays_inside_the_band() {
        assert_eq!(
            hz_from_log_slider(20.0, 20.0_f32.ln() - 0.001, 20.0, 400.0),
            20.0
        );
        assert_eq!(
            hz_from_log_slider(400.0, 400.0_f32.ln() + 0.001, 20.0, 400.0),
            400.0
        );
    }
}
