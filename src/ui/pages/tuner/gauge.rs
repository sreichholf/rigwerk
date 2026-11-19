//! The tuner's two gauge faces.  Both are painted inside a 146 px panel and
//! share the tolerance band and the permanent ±1 centered mark.

use std::f32::consts::PI;

use eframe::egui;
use egui::{Align2, CornerRadius, FontFamily, FontId, Pos2, Rect, Sense, Shape, Stroke, Vec2};

use crate::audio::TuneState;
use crate::ui::theme::ts;
use crate::ui::theme::{palette, with_alpha, Palette};

use super::{signed_cents, state_colour};

const PANEL_H: f32 = 146.0;
const STROBE_BARS: usize = 48;

/// Phase added to the strobe pattern.  Negative cents (flat) move the bars
/// left, positive cents (sharp) move them right.  The bars hold inside the
/// band the cents readout draws as 0.0.
pub(super) fn strobe_phase(cents: f32, phase: f32) -> f32 {
    if cents < -super::DISPLAYED_ZERO_CENTS {
        phase
    } else if cents > super::DISPLAYED_ZERO_CENTS {
        -phase
    } else {
        0.0
    }
}

/// The empty panel both faces draw into.
fn panel(ui: &mut egui::Ui) -> Rect {
    let colours = palette(ui.ctx());
    let (rect, _) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), PANEL_H), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(10), colours.well);
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(10),
        Stroke::new(1.0, colours.line),
        egui::StrokeKind::Inside,
    );
    rect
}

/// Strobe: bars that drift left when flat and right when sharp.
pub(super) fn strobe(
    ui: &mut egui::Ui,
    cents: f32,
    state: TuneState,
    valid: bool,
    tolerance: f32,
    colours: &Palette,
) {
    let rect = panel(ui);
    let painter = ui.painter();
    let color = if valid {
        state_colour(colours, state)
    } else {
        colours.text3
    };
    painter.text(
        egui::pos2(rect.left() + 14.0, rect.top() + 12.0),
        Align2::LEFT_CENTER,
        "STROBE · FINE",
        FontId::new(ts(8.5), FontFamily::Proportional),
        colours.text3,
    );
    painter.text(
        egui::pos2(rect.right() - 14.0, rect.top() + 12.0),
        Align2::RIGHT_CENTER,
        if valid {
            format!("{} ¢", signed_cents(cents))
        } else {
            "—".to_owned()
        },
        FontId::new(ts(9.0), FontFamily::Monospace),
        color,
    );

    let bars = Rect::from_min_max(
        egui::pos2(rect.left() + 16.0, rect.top() + 26.0),
        egui::pos2(rect.right() - 16.0, rect.bottom() - 40.0),
    );
    painter.rect_filled(bars, CornerRadius::same(6), with_alpha(color, 5));

    let mid = bars.center().x;
    let half = bars.width() / 2.0;
    let step = bars.width() / STROBE_BARS as f32;
    let bar_width = (step * 0.58).max(2.0);
    let time = ui.input(|input| input.time) as f32;
    // The pattern has to crawl without aliasing into a stationary image: the
    // speed follows the offset, and an in-tune reading crawls slowly.
    let speed = 0.7 + cents.abs().min(50.0) * 0.05;
    let phase = if valid { time * speed * 2.0 } else { 0.0 };
    let offset = strobe_phase(cents, phase);
    let tolerance_x = half * (tolerance / 50.0).clamp(0.0, 1.0);

    for index in 0..STROBE_BARS {
        let x = bars.left() + (index as f32 + 0.5) * step;
        let edge = ((x - mid) / half).clamp(-1.0, 1.0);
        let dome = (1.0 - edge * edge).max(0.0);
        let wave = 0.5 + 0.5 * (index as f32 * 0.62 + offset).sin();
        let height = bars.height() * (0.50 + 0.42 * dome) * (0.72 + 0.28 * wave);
        let bar_color = if !valid {
            with_alpha(colours.text3, 55)
        } else if (x - mid).abs() <= tolerance_x {
            with_alpha(color, 225)
        } else {
            with_alpha(color, 105)
        };
        painter.rect_filled(
            Rect::from_min_max(
                egui::pos2(x - bar_width / 2.0, bars.bottom() - height),
                egui::pos2(x + bar_width / 2.0, bars.bottom()),
            ),
            CornerRadius::same(1),
            bar_color,
        );
    }

    painter.rect_filled(
        Rect::from_min_max(
            egui::pos2(mid - tolerance_x, bars.bottom() + 1.0),
            egui::pos2(mid + tolerance_x, bars.bottom() + 3.0),
        ),
        CornerRadius::same(1),
        with_alpha(colours.accent, 55),
    );
    let center_x = half * (1.0 / 50.0);
    painter.rect_filled(
        Rect::from_min_max(
            egui::pos2(mid - center_x, bars.top()),
            egui::pos2(mid + center_x, bars.bottom()),
        ),
        0,
        with_alpha(colours.accent, 24),
    );
    painter.line_segment(
        [
            egui::pos2(mid, bars.top() - 3.0),
            egui::pos2(mid, bars.bottom() + 3.0),
        ],
        Stroke::new(1.4, with_alpha(colours.accent, 220)),
    );

    for (fraction, label) in [
        (-1.0, "-50"),
        (-0.5, "-25"),
        (0.0, "0"),
        (0.5, "+25"),
        (1.0, "+50"),
    ] {
        painter.text(
            egui::pos2(mid + half * fraction, rect.bottom() - 26.0),
            Align2::CENTER_CENTER,
            label,
            FontId::new(ts(8.0), FontFamily::Monospace),
            colours.text3,
        );
    }
    // Arrow glyphs are not in the interface font, so the drift directions are
    // painted as two-line chevrons.
    chevron(
        painter,
        egui::pos2(bars.left() + 3.0, rect.bottom() - 9.0),
        true,
        colours,
    );
    painter.text(
        egui::pos2(bars.left() + 11.0, rect.bottom() - 9.0),
        Align2::LEFT_CENTER,
        "drifting flat",
        FontId::new(ts(8.0), FontFamily::Proportional),
        colours.text3,
    );
    painter.text(
        egui::pos2(bars.right() - 11.0, rect.bottom() - 9.0),
        Align2::RIGHT_CENTER,
        "drifting sharp",
        FontId::new(ts(8.0), FontFamily::Proportional),
        colours.text3,
    );
    chevron(
        painter,
        egui::pos2(bars.right() - 3.0, rect.bottom() - 9.0),
        false,
        colours,
    );
}

/// Needle: a 180° arc over ±50 cents with the tolerance band and the ±1 mark
/// painted on the arc itself.
pub(super) fn needle(
    ui: &mut egui::Ui,
    cents: f32,
    state: TuneState,
    valid: bool,
    tolerance: f32,
    colours: &Palette,
) {
    let rect = panel(ui);
    let painter = ui.painter();
    let color = if valid {
        state_colour(colours, state)
    } else {
        colours.text3
    };
    painter.text(
        egui::pos2(rect.left() + 14.0, rect.top() + 12.0),
        Align2::LEFT_CENTER,
        "NEEDLE · ±50 CENTS",
        FontId::new(ts(8.5), FontFamily::Proportional),
        colours.text3,
    );
    painter.text(
        egui::pos2(rect.right() - 14.0, rect.top() + 12.0),
        Align2::RIGHT_CENTER,
        if valid {
            format!("{} ¢", signed_cents(cents))
        } else {
            "—".to_owned()
        },
        FontId::new(ts(9.0), FontFamily::Monospace),
        color,
    );

    let hub = egui::pos2(rect.center().x, rect.bottom() - 30.0);
    let radius = (rect.width() * 0.30)
        .clamp(54.0, 92.0)
        .min(rect.height() - 54.0);
    let point = |fraction: f32| {
        let angle = PI + (fraction + 1.0) * 0.5 * PI;
        egui::pos2(hub.x + angle.cos() * radius, hub.y + angle.sin() * radius)
    };
    let arc = |from: f32, to: f32, steps: usize| {
        (0..=steps)
            .map(|step| {
                let fraction = from + (to - from) * step as f32 / steps as f32;
                point(fraction.clamp(-1.0, 1.0))
            })
            .collect::<Vec<Pos2>>()
    };

    painter.add(Shape::line(
        arc(-1.0, 1.0, 64),
        Stroke::new(9.0, colours.line2),
    ));
    let tolerance_fraction = (tolerance / 50.0).clamp(0.0, 1.0);
    painter.add(Shape::line(
        arc(-tolerance_fraction, tolerance_fraction, 24),
        Stroke::new(9.0, with_alpha(colours.accent, 120)),
    ));
    painter.add(Shape::line(
        arc(-1.0 / 50.0, 1.0 / 50.0, 4),
        Stroke::new(10.0, with_alpha(colours.accent, 200)),
    ));

    for tick in -10..=10 {
        let fraction = tick as f32 / 10.0;
        let inner = point(fraction);
        let direction = (inner - hub).normalized();
        let major = tick % 2 == 0;
        painter.line_segment(
            [
                inner - direction * if major { 15.0 } else { 9.0 },
                inner - direction * 5.0,
            ],
            Stroke::new(
                1.0,
                if major {
                    with_alpha(colours.text2, 190)
                } else {
                    with_alpha(colours.text3, 150)
                },
            ),
        );
    }
    for (fraction, label) in [(-0.5, "−25"), (0.5, "+25")] {
        let position = point(fraction);
        let direction = (position - hub).normalized();
        painter.text(
            position - direction * 27.0,
            Align2::CENTER_CENTER,
            label,
            FontId::new(ts(8.5), FontFamily::Monospace),
            colours.text2,
        );
    }
    let top = point(0.0);
    painter.text(
        top + Vec2::new(0.0, 14.0),
        Align2::CENTER_CENTER,
        "0",
        FontId::new(ts(7.5), FontFamily::Monospace),
        colours.accent,
    );
    painter.text(
        egui::pos2(hub.x - radius - 4.0, hub.y + 4.0),
        Align2::LEFT_TOP,
        "−50",
        FontId::new(ts(8.0), FontFamily::Monospace),
        colours.text3,
    );
    painter.text(
        egui::pos2(hub.x + radius + 4.0, hub.y + 4.0),
        Align2::RIGHT_TOP,
        "+50",
        FontId::new(ts(8.0), FontFamily::Monospace),
        colours.text3,
    );
    painter.text(
        egui::pos2(rect.left() + 16.0, rect.bottom() - 11.0),
        Align2::LEFT_CENTER,
        "FLAT",
        FontId::new(ts(8.0), FontFamily::Proportional),
        colours.text3,
    );
    painter.text(
        egui::pos2(rect.right() - 16.0, rect.bottom() - 11.0),
        Align2::RIGHT_CENTER,
        "SHARP",
        FontId::new(ts(8.0), FontFamily::Proportional),
        colours.text3,
    );

    if valid {
        let fraction = (cents / 50.0).clamp(-1.0, 1.0);
        let tip = point(fraction);
        let start = egui::pos2(
            hub.x + (tip.x - hub.x) * 0.16,
            hub.y + (tip.y - hub.y) * 0.16,
        );
        painter.line_segment([start, tip], Stroke::new(3.0, color));
        painter.circle_filled(tip, 2.6, color);
    }
    painter.circle_filled(hub, 7.0, colours.bg);
    painter.circle_stroke(hub, 7.0, Stroke::new(1.2, colours.line));
    painter.circle_filled(hub, 3.2, color);
}

fn chevron(painter: &egui::Painter, center: egui::Pos2, left: bool, colours: &Palette) {
    let tip = egui::pos2(center.x + if left { -3.0 } else { 3.0 }, center.y);
    let base = egui::pos2(center.x + if left { 2.0 } else { -2.0 }, center.y - 4.0);
    let base_low = egui::pos2(base.x, center.y + 4.0);
    painter.line_segment([base, tip], Stroke::new(1.1, colours.text3));
    painter.line_segment([base_low, tip], Stroke::new(1.1, colours.text3));
}
