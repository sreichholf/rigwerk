//! The tuner's two gauge faces.  The needle is the ±50 cent arc.  The strobe
//! is three discs seen from the front, wide to fine, and a disc holds inside
//! ±1 cent.  Each disc is clipped to its own band so a slide cannot spill.

use std::f32::consts::PI;

use eframe::egui;
use egui::{Align2, CornerRadius, FontFamily, FontId, Pos2, Rect, Sense, Shape, Stroke, Vec2};

use crate::audio::TuneState;
use crate::ui::theme::ts;
use crate::ui::theme::{with_alpha, Palette};

use super::{signed_cents, state_colour};

const PANEL_H: f32 = 172.0;
/// Discs hold inside this many cents.  The same mark as `CENTER_CENTS`.
const STROBE_HOLD_CENTS: f32 = 1.0;
/// Past this many cents the discs do not spin any faster.
const STROBE_SPEED_CAP_CENTS: f32 = 24.0;
/// Pixels per second for one cent of error, before the gear.
const STROBE_PIXELS_PER_CENT: f32 = 14.0;
/// Share of each disc that stays clear.  The well colour shades the two
/// edges, and the shade does not move with the segments.
const STROBE_CLEAR: f32 = 0.56;
/// (lit segment, gap, gear) for the wide, medium and fine discs.
const DISCS: [(f32, f32, f32); 3] = [(168.0, 72.0, 1.0), (64.0, 28.0, 2.0), (16.0, 8.0, 5.0)];

/// Horizontal shift of one disc, in pixels.  Inside ±1 cent the disc holds.
/// Outside, the sign is the direction (negative is flat, and flat moves
/// left) and the magnitude is `min(|cents|, 24) * 14 * gear * time_secs`.
pub(super) fn strobe_offset(cents: f32, time_secs: f32, gear: f32) -> f32 {
    if !cents.is_finite() || cents.abs() <= STROBE_HOLD_CENTS {
        return 0.0;
    }
    let speed = cents.abs().min(STROBE_SPEED_CAP_CENTS) * STROBE_PIXELS_PER_CENT * gear;
    cents.signum() * speed * time_secs
}

/// The empty panel both faces draw into.
fn panel(ui: &mut egui::Ui) -> Rect {
    let colours = crate::ui::theme::palette(ui.ctx());
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

/// Strobe: three full-width discs.  Flat slides left, sharp slides right.
pub(super) fn strobe(
    ui: &mut egui::Ui,
    cents: f32,
    state: TuneState,
    valid: bool,
    colours: &Palette,
) {
    let rect = panel(ui);
    let painter = ui.painter();
    let color = if valid {
        state_colour(colours, state, cents)
    } else {
        colours.text3
    };
    painter.text(
        egui::pos2(rect.left() + 14.0, rect.top() + 12.0),
        Align2::LEFT_CENTER,
        "STROBE",
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

    let bands = Rect::from_min_max(
        egui::pos2(rect.left() + 1.0, rect.top() + 22.0),
        egui::pos2(rect.right() - 1.0, rect.bottom() - 20.0),
    );
    let gap = 6.0;
    let height = ((bands.height() - gap * (DISCS.len() - 1) as f32) / DISCS.len() as f32).max(0.0);
    let time = ui.input(|input| input.time) as f32;
    for (index, (on, space, gear)) in DISCS.iter().copied().enumerate() {
        let top = bands.top() + index as f32 * (height + gap);
        let disc = Rect::from_min_max(
            egui::pos2(bands.left(), top),
            egui::pos2(bands.right(), top + height),
        );
        let offset = if valid {
            strobe_offset(cents, time, gear)
        } else {
            0.0
        };
        paint_disc(painter, disc, on, space, offset, color, colours.well);
    }

    painter.text(
        egui::pos2(rect.center().x, rect.bottom() - 10.0),
        Align2::CENTER_CENTER,
        "Flat moves left, sharp moves right",
        FontId::new(ts(8.0), FontFamily::Proportional),
        colours.text3,
    );
}

/// One disc: vertical segments of `on` pixels, a gap of `space`, then the
/// still edge shade.  `offset` is in pixels and already carries the sign.
fn paint_disc(
    painter: &egui::Painter,
    disc: Rect,
    on: f32,
    space: f32,
    offset: f32,
    colour: egui::Color32,
    well: egui::Color32,
) {
    if disc.height() <= 0.0 || disc.width() <= 0.0 {
        return;
    }
    let painter = painter.with_clip_rect(disc);
    let period = on + space;
    if period > 0.0 && on > 0.0 {
        let shift = if offset.is_finite() {
            offset.rem_euclid(period)
        } else {
            0.0
        };
        let mut x = disc.left() + shift - period;
        while x < disc.right() {
            painter.rect_filled(
                Rect::from_min_max(egui::pos2(x, disc.top()), egui::pos2(x + on, disc.bottom())),
                0,
                colour,
            );
            x += period;
        }
    }
    shade_disc(&painter, disc, well);
}

/// Well colour at the two edges, clear through the middle.  Painted over the
/// segments, so the segments themselves stay a flat fill.
fn shade_disc(painter: &egui::Painter, disc: Rect, well: egui::Color32) {
    let clear = well.gamma_multiply(0.0);
    let edge = (1.0 - STROBE_CLEAR) * 0.5;
    let stops = [0.0, edge, 1.0 - edge, 1.0];
    let colors = [well, clear, clear, well];
    let mut mesh = egui::Mesh::default();
    for (stop, color) in stops.iter().zip(colors) {
        let x = disc.left() + disc.width() * stop;
        mesh.colored_vertex(egui::pos2(x, disc.top()), color);
        mesh.colored_vertex(egui::pos2(x, disc.bottom()), color);
    }
    for column in 0..3u32 {
        let left_top = column * 2;
        let left_bottom = left_top + 1;
        let right_top = left_top + 2;
        let right_bottom = left_top + 3;
        mesh.add_triangle(left_top, right_top, left_bottom);
        mesh.add_triangle(right_top, right_bottom, left_bottom);
    }
    painter.add(Shape::mesh(mesh));
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
        state_colour(colours, state, cents)
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
        Stroke::new(9.0, with_alpha(colours.path[2], 140)),
    ));
    painter.add(Shape::line(
        arc(-1.0 / 50.0, 1.0 / 50.0, 4),
        Stroke::new(10.0, colours.path[2]),
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
        colours.path[2],
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
