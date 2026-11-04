//! The EQ card: edits one of the two built-in 5-band EQs (before or after
//! the plugin inserts), with its response curve.  Changes apply at once,
//! also while the bridge runs.

use eframe::egui;
use egui::{
    Align, Align2, Color32, CornerRadius, FontFamily, FontId, Layout, Rect, RichText, Sense,
    Stroke, Vec2,
};

use crate::app::RigWerkApp;
use crate::audio::{
    BandKind, EngineState, EqSettings, EqSlot, EQ_BANDS, EQ_MAX_GAIN_DB, EQ_MAX_Q, EQ_MIN_Q,
};
use crate::ui::theme::{
    with_alpha, AMBER, BLUE, BORDER, BORDER_SOFT, CARD, CARD_DEEP, DIM, GREEN, MUTED, RED, TEAL,
    TEXT,
};
use crate::ui::widgets::{card_fixed, hinted_tab, micro_label, mono, styled_slider};

/// One colour per band, low to high.
const BAND_COLORS: [Color32; 5] = [RED, AMBER, GREEN, TEAL, BLUE];
const CURVE_MIN_HZ: f32 = 20.0;
const CURVE_MAX_HZ: f32 = 20_000.0;

fn hz_label(hz: f32) -> String {
    if hz >= 1_000.0 {
        format!("{:.1} kHz", hz / 1_000.0)
    } else {
        format!("{hz:.0} Hz")
    }
}

impl RigWerkApp {
    pub(crate) fn eq_card(&mut self, ui: &mut egui::Ui, width: f32) {
        let slot = self.eq_edit;
        let current = self.eqs[slot.index()];
        let mut eq = current;
        let mut switch_to: Option<EqSlot> = None;
        let rate = if self.engine.state == EngineState::Running && self.snapshot.output_rate > 0 {
            self.snapshot.output_rate
        } else {
            48_000
        };

        card_fixed(ui, CARD, BORDER, 290.0, |ui| {
            ui.set_width(width - 32.0);
            ui.horizontal(|ui| {
                ui.label(micro_label("EQ", AMBER));
                ui.add_space(6.0);
                for option in EqSlot::ALL {
                    if hinted_tab(
                        ui,
                        slot_title(option),
                        option == slot,
                        AMBER,
                        slot_hint(option),
                    ) {
                        switch_to = Some(option);
                    }
                }
                ui.add_space(6.0);
                ui.label(
                    RichText::new("5 bands · master mix · no added latency")
                        .size(10.0)
                        .color(DIM),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if hinted_tab(
                        ui,
                        if eq.enabled { "ON" } else { "BYPASSED" },
                        eq.enabled,
                        AMBER,
                        "Switch the whole EQ on or off. Settings are kept while bypassed.",
                    ) {
                        eq.enabled = !eq.enabled;
                    }
                    ui.add_space(4.0);
                    if hinted_tab(
                        ui,
                        "FLAT",
                        false,
                        MUTED,
                        "Reset every band to its default, 0 dB.",
                    ) {
                        eq = eq.flat();
                    }
                });
            });
            ui.add_space(8.0);

            ui.horizontal_top(|ui| {
                // Columns are placed edge to edge; the gaps below are the only
                // spacing, so five bands always fit the width that is left.
                ui.spacing_mut().item_spacing.x = 0.0;
                let curve_w = (ui.available_width() * 0.3).clamp(200.0, 380.0);
                let (rect, _) = ui.allocate_exact_size(Vec2::new(curve_w, 224.0), Sense::hover());
                draw_curve(ui, rect, &eq, rate, true);
                ui.add_space(12.0);

                let gap = 8.0;
                let band_w = ((ui.available_width() - gap * 4.0) / 5.0).max(90.0);
                for (index, spec) in EQ_BANDS.iter().enumerate() {
                    let color = BAND_COLORS[index];
                    ui.allocate_ui_with_layout(
                        Vec2::new(band_w, 224.0),
                        Layout::top_down(Align::LEFT),
                        |ui| {
                            ui.set_width(band_w);
                            ui.spacing_mut().item_spacing.x = 6.0;
                            let band = &mut eq.bands[index];
                            let live = eq.enabled && band.on;
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(spec.name)
                                        .size(10.5)
                                        .color(if live { TEXT } else { DIM })
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
                            ui.horizontal(|ui| {
                                ui.label(mono("GAIN", DIM, 7.5));
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    ui.label(mono(
                                        format!("{:+.1} dB", band.gain_db),
                                        if live { TEXT } else { DIM },
                                        8.5,
                                    ));
                                });
                            });
                            let gain = styled_slider(
                                ui,
                                &mut band.gain_db,
                                -EQ_MAX_GAIN_DB,
                                EQ_MAX_GAIN_DB,
                                0.5,
                                color,
                            )
                            .on_hover_text("Boost or cut, ±12 dB. Double-click for 0 dB.");
                            if gain.double_clicked() {
                                band.gain_db = 0.0;
                            }

                            ui.horizontal(|ui| {
                                ui.label(mono("FREQ", DIM, 7.5));
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    ui.label(mono(hz_label(band.hz), MUTED, 8.5));
                                });
                            });
                            // The frequency slider moves in octaves, not hertz,
                            // so every part of its range is equally easy to set.
                            let mut position = band.hz.ln();
                            let freq = styled_slider(
                                ui,
                                &mut position,
                                spec.min_hz.ln(),
                                spec.max_hz.ln(),
                                0.001,
                                color,
                            )
                            .on_hover_text(format!(
                                "{} to {}. Double-click for {}.",
                                hz_label(spec.min_hz),
                                hz_label(spec.max_hz),
                                hz_label(spec.default_hz)
                            ));
                            if freq.changed() {
                                band.hz = position.exp().round();
                            }
                            if freq.double_clicked() {
                                band.hz = spec.default_hz;
                            }

                            ui.horizontal(|ui| {
                                ui.label(mono("WIDTH", DIM, 7.5));
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    ui.label(mono(format!("Q {:.1}", band.q), MUTED, 8.5));
                                });
                            });
                            // Left is wide, right is narrow, on every band.
                            let width = styled_slider(ui, &mut band.q, EQ_MIN_Q, EQ_MAX_Q, 0.1, color)
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
                        },
                    );
                    if index < 4 {
                        ui.add_space(gap);
                    }
                }
            });
        });

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
        if let Some(option) = switch_to {
            self.eq_edit = option;
        }
    }
}

pub(crate) fn slot_title(slot: EqSlot) -> &'static str {
    match slot {
        EqSlot::Pre => "PRE EQ",
        EqSlot::Post => "POST EQ",
    }
}

pub(crate) fn slot_hint(slot: EqSlot) -> &'static str {
    match slot {
        EqSlot::Pre => "Before the plugin inserts: shapes what the plugins receive, e.g. tighten the low end before an amp sim.",
        EqSlot::Post => "After the plugin inserts: shapes what the plugins produce, e.g. tame fizz after an amp sim.",
    }
}

/// The combined response from 20 Hz to 20 kHz, ±12 dB, with a dot per band.
///
/// The line is the real biquad response of every enabled band, so moving
/// gain, frequency or Q redraws it.  The big card also shows each band's own
/// faint trace, which makes it obvious which control changed the shape.
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
    let label = FontId::new(7.5, FontFamily::Monospace);
    let dot = if labels { 4.0 } else { 2.5 };
    let zero_y = y_of(0.0);

    for db in [-12.0, -6.0, 6.0, 12.0] {
        painter.hline(
            plot.x_range(),
            y_of(db),
            Stroke::new(1.0, with_alpha(BORDER_SOFT, 140)),
        );
    }
    painter.hline(plot.x_range(), zero_y, Stroke::new(1.0, BORDER));
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
        // Soft gradient between the curve and 0 dB turns the line into a shape.
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

        // Faint trace per band: gain, frequency and Q all move their own line.
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

        // A wide, nearly transparent line underneath gives the curve a glow.
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
        // Dots sit on the resulting curve, not on the raw slider value, so
        // the line and its handles tell the same story.
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
