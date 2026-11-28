//! The tuner's needle and strobe.  Disc speed eases toward the cents, and
//! the pattern is the integral of that speed, so a new reading cannot jump
//! the discs.  Each disc is clipped to its own band.

use std::f32::consts::PI;

use eframe::egui;
use egui::{Align2, CornerRadius, FontFamily, FontId, Pos2, Rect, Sense, Shape, Stroke, Vec2};

use crate::audio::TuneState;
use crate::ui::fonts::Face;
use crate::ui::theme::ts;
use crate::ui::theme::{font, with_alpha, Palette, TypeRole};

use super::{signed_cents, state_colour};

const PANEL_H: f32 = 172.0;
/// 9 pt under a disc does not read.
const COLUMN_HEADER: f32 = 58.0;
const ROW_H: f32 = 208.0;
/// A column does not grow past the full needle.
const NEEDLE_RADIUS_CAP: f32 = 92.0;
const NEEDLE_HUB_INSET: f32 = 14.0;
/// Keeps the stroke inside the face.
const NEEDLE_SIDE: f32 = 10.0;
/// The same mark as `CENTER_CENTS`.
const STROBE_HOLD_CENTS: f32 = 1.0;
const STROBE_SPEED_CAP_CENTS: f32 = 24.0;
/// Before a disc's gear.
const STROBE_PIXELS_PER_CENT: f32 = 14.0;
/// The edge shade does not move with the segments.
const STROBE_CLEAR: f32 = 0.56;
/// A tenth of a second reaches most of the way, so one noisy poll cannot
/// slam the pattern.
const STROBE_EASE_PER_SEC: f32 = 8.0;
/// Otherwise a disc heading for a stop requests frames forever.
const STROBE_SPEED_SETTLE: f32 = 0.5;
/// (lit segment, gap, gear) for the wide, medium and fine discs.
const DISCS: [(f32, f32, f32); 3] = [(168.0, 72.0, 1.0), (64.0, 28.0, 2.0), (16.0, 8.0, 5.0)];
/// A multiple of every disc period, so wrapping leaves each pattern where
/// it was.
const STROBE_PHASE_WRAP: f32 = {
    const fn gcd(mut a: u32, mut b: u32) -> u32 {
        while b != 0 {
            let next = a % b;
            a = b;
            b = next;
        }
        a
    }
    const fn lcm(a: u32, b: u32) -> u32 {
        a / gcd(a, b) * b
    }
    lcm(lcm(168 + 72, 64 + 28), 16 + 8) as f32
};
const _: () = {
    let mut index = 0;
    while index < DISCS.len() {
        let period = (DISCS[index].0 + DISCS[index].1) as u32;
        let wrap = STROBE_PHASE_WRAP as u32;
        if period == 0 || !wrap.is_multiple_of(period) {
            panic!("strobe phase wrap must cover each disc period");
        }
        index += 1;
    }
};

/// Negative is flat, and flat moves left.  Zero inside the hold, for a
/// silent face, and for a non-finite cents value.
pub(super) fn strobe_target_speed(cents: f32, valid: bool) -> f32 {
    if !valid || !cents.is_finite() || cents.abs() <= STROBE_HOLD_CENTS {
        return 0.0;
    }
    cents.signum() * cents.abs().min(STROBE_SPEED_CAP_CENTS) * STROBE_PIXELS_PER_CENT
}

/// A fraction of the gap, so a cents jump changes velocity and not position.
pub(super) fn ease_strobe_speed(speed: f32, target: f32, dt: f32) -> f32 {
    if !speed.is_finite() {
        return 0.0;
    }
    if !target.is_finite() || !dt.is_finite() || dt <= 0.0 {
        return speed;
    }
    let alpha = 1.0 - (-STROBE_EASE_PER_SEC * dt.min(0.05)).exp();
    let next = speed + (target - speed) * alpha;
    if target == 0.0 && next.abs() < STROBE_SPEED_SETTLE {
        0.0
    } else {
        next
    }
}

/// Callers scale the result by a disc's gear.
pub(super) fn advance_strobe_phase(phase: f32, speed: f32, dt: f32) -> f32 {
    let phase = if phase.is_finite() { phase } else { 0.0 };
    if !speed.is_finite() || !dt.is_finite() || dt <= 0.0 {
        return phase.rem_euclid(STROBE_PHASE_WRAP);
    }
    (phase + speed * dt.min(0.05)).rem_euclid(STROBE_PHASE_WRAP)
}

#[derive(Clone, Copy, Debug)]
pub(super) struct StrobeMotion {
    pub(super) speed: f32,
    pub(super) phase: f32,
    last_time: f64,
}

impl Default for StrobeMotion {
    fn default() -> Self {
        Self {
            speed: 0.0,
            phase: 0.0,
            last_time: f64::NAN,
        }
    }
}

impl StrobeMotion {
    /// The first call only records the clock, so a late first paint does
    /// not leap.
    pub(super) fn step(&mut self, cents: f32, valid: bool, now: f64) {
        let dt = if self.last_time.is_finite() {
            (now - self.last_time).clamp(0.0, 0.05)
        } else {
            0.0
        };
        self.last_time = now;
        self.speed = ease_strobe_speed(self.speed, strobe_target_speed(cents, valid), dt as f32);
        self.phase = advance_strobe_phase(self.phase, self.speed, dt as f32);
    }

    fn moving(&self, cents: f32, valid: bool) -> bool {
        self.speed != 0.0 || strobe_target_speed(cents, valid) != 0.0
    }
}

/// `column` is `None` for the single face.  A string face's index keys
/// that strobe's motion.
pub(super) struct GaugeFace {
    pub(super) column: Option<usize>,
    pub(super) title: &'static str,
    pub(super) cents: f32,
    pub(super) state: TuneState,
    pub(super) valid: bool,
    pub(super) followed: bool,
    pub(super) pinned: bool,
}

/// Zero when the row has nothing to divide.  The faces plus the gaps are
/// the span, so the row stays the width it was given.
pub(super) fn strobe_column_width(span: f32, count: usize, gap: f32) -> f32 {
    if count == 0 || !span.is_finite() || !gap.is_finite() || span <= 0.0 || gap < 0.0 {
        return 0.0;
    }
    let gaps = gap * count.saturating_sub(1) as f32;
    ((span - gaps) / count as f32).max(0.0)
}

/// The full face is 1.  A column is its width over the row, so it shows the
/// full strobe's bar count.  A width that cannot be divided stays at 1.
pub(super) fn strobe_scale(face_width: f32, row_width: f32) -> f32 {
    if !face_width.is_finite() || !row_width.is_finite() || face_width <= 0.0 || row_width <= 0.0 {
        return 1.0;
    }
    (face_width / row_width).clamp(0.0, 1.0)
}

/// Grows with the column, up to the full needle.  A zero or non-finite
/// size draws nothing.
pub(super) fn column_needle_radius(width: f32, height: f32) -> f32 {
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return 0.0;
    }
    let vertical = (height - COLUMN_HEADER - NEEDLE_HUB_INSET).max(0.0);
    let horizontal = (width * 0.5 - NEEDLE_SIDE).max(0.0);
    vertical.min(horizontal).min(NEEDLE_RADIUS_CAP)
}

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

pub(super) fn strobe(
    ui: &mut egui::Ui,
    cents: f32,
    state: TuneState,
    valid: bool,
    colours: &Palette,
) {
    let span = ui.available_width().max(0.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(span, PANEL_H), Sense::hover());
    paint_strobe(
        ui,
        rect,
        strobe_scale(span, span),
        &GaugeFace {
            column: None,
            title: "STROBE",
            cents,
            state,
            valid,
            followed: false,
            pinned: false,
        },
        colours,
    );
}

fn layout_columns(
    ui: &mut egui::Ui,
    count: usize,
    mut paint: impl FnMut(&mut egui::Ui, Rect, f32, usize),
) {
    if count == 0 {
        return;
    }
    let span = ui.available_width().max(0.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(span, ROW_H), Sense::hover());
    let gap = super::STRING_COLUMN_GAP;
    let column = strobe_column_width(span, count, gap);
    let scale = strobe_scale(column, span);
    for index in 0..count {
        let left = rect.left() + index as f32 * (column + gap);
        let width = column.min((rect.right() - left).max(0.0));
        let face_rect = Rect::from_min_size(
            egui::pos2(left, rect.top()),
            Vec2::new(width, rect.height()),
        );
        paint(ui, face_rect, scale, index);
    }
}

pub(super) fn strobes(ui: &mut egui::Ui, faces: &[GaugeFace], colours: &Palette) {
    let mut moving = false;
    layout_columns(ui, faces.len(), |ui, rect, scale, index| {
        if paint_strobe(ui, rect, scale, &faces[index], colours) {
            moving = true;
        }
    });
    if moving {
        ui.ctx().request_repaint();
    }
}

fn paint_face_plate(
    painter: &egui::Painter,
    rect: Rect,
    face: &GaugeFace,
    color: egui::Color32,
    colours: &Palette,
) {
    let (border, width) = if face.pinned {
        (colours.accent, 1.5)
    } else if face.followed {
        (color, 1.5)
    } else {
        (colours.line, 1.0)
    };
    painter.rect_filled(rect, CornerRadius::same(10), colours.well);
    painter.rect_stroke(
        rect,
        CornerRadius::same(10),
        Stroke::new(width, border),
        egui::StrokeKind::Inside,
    );
}

/// The single face keeps its corner label.
fn paint_column_header(
    painter: &egui::Painter,
    rect: Rect,
    face: &GaugeFace,
    color: egui::Color32,
    colours: &Palette,
) {
    let painter = painter.with_clip_rect(rect);
    painter.text(
        egui::pos2(rect.center().x, rect.top() + 6.0),
        Align2::CENTER_TOP,
        face.title,
        font(Face::Mono, TypeRole::TunerString),
        colours.text,
    );
    painter.text(
        egui::pos2(rect.center().x, rect.top() + 32.0),
        Align2::CENTER_TOP,
        if face.valid {
            format!("{} ¢", signed_cents(face.cents))
        } else {
            "—".to_owned()
        },
        font(Face::Mono, TypeRole::Heading),
        color,
    );
}

/// The row asks for the next frame once, after every face has stepped.
fn paint_strobe(
    ui: &mut egui::Ui,
    rect: Rect,
    scale: f32,
    face: &GaugeFace,
    colours: &Palette,
) -> bool {
    let painter = ui.painter();
    let color = if face.valid {
        state_colour(colours, face.state, face.cents)
    } else {
        colours.text3
    };
    let footer = face.column.is_none();
    paint_face_plate(painter, rect, face, color, colours);
    if footer {
        painter.text(
            egui::pos2(rect.left() + 14.0, rect.top() + 12.0),
            Align2::LEFT_CENTER,
            face.title,
            FontId::new(ts(8.5), FontFamily::Proportional),
            colours.text3,
        );
        painter.text(
            egui::pos2(rect.right() - 14.0, rect.top() + 12.0),
            Align2::RIGHT_CENTER,
            if face.valid {
                format!("{} ¢", signed_cents(face.cents))
            } else {
                "—".to_owned()
            },
            FontId::new(ts(9.0), FontFamily::Monospace),
            color,
        );
    } else {
        paint_column_header(painter, rect, face, color, colours);
    }

    let bands = Rect::from_min_max(
        egui::pos2(
            rect.left() + 1.0,
            rect.top() + if footer { 22.0 } else { COLUMN_HEADER },
        ),
        egui::pos2(
            rect.right() - 1.0,
            rect.bottom() - if footer { 20.0 } else { 6.0 },
        ),
    );
    let gap = 6.0;
    let height = ((bands.height() - gap * (DISCS.len() - 1) as f32) / DISCS.len() as f32).max(0.0);
    let motion_id = match face.column {
        Some(index) => ui.scope_id().with(("strobe", index)),
        None => ui.scope_id().with("strobe_motion"),
    };
    let mut motion = ui
        .data(|data| data.get_temp::<StrobeMotion>(motion_id))
        .unwrap_or_default();
    motion.step(face.cents, face.valid, ui.input(|input| input.time));
    let moving = motion.moving(face.cents, face.valid);
    let phase = motion.phase;
    ui.data_mut(|data| data.insert_temp(motion_id, motion));
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    for (index, (on, space, gear)) in DISCS.iter().copied().enumerate() {
        let top = bands.top() + index as f32 * (height + gap);
        let disc = Rect::from_min_max(
            egui::pos2(bands.left(), top),
            egui::pos2(bands.right(), top + height),
        );
        paint_disc(
            painter,
            disc,
            on * scale,
            space * scale,
            phase * gear * scale,
            color,
            colours.well,
        );
    }

    if footer {
        painter.text(
            egui::pos2(rect.center().x, rect.bottom() - 10.0),
            Align2::CENTER_CENTER,
            "Flat moves left, sharp moves right",
            FontId::new(ts(8.0), FontFamily::Proportional),
            colours.text3,
        );
    }
    if moving && footer {
        ui.ctx().request_repaint();
    }
    moving
}

/// `offset` already carries the sign and the face's scale, so a column
/// crosses a bar as fast as the full face.
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

/// Painted over the segments, so they stay a flat fill.
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
        .clamp(54.0, NEEDLE_RADIUS_CAP)
        .min(rect.height() - 54.0);
    paint_needle_dial(
        painter, rect, hub, radius, cents, valid, tolerance, color, colours, true, 1.0,
    );
}

pub(super) fn needles(ui: &mut egui::Ui, faces: &[GaugeFace], tolerance: f32, colours: &Palette) {
    layout_columns(ui, faces.len(), |ui, rect, _, index| {
        let face = &faces[index];
        let painter = ui.painter().with_clip_rect(rect);
        let color = if face.valid {
            state_colour(colours, face.state, face.cents)
        } else {
            colours.text3
        };
        paint_face_plate(&painter, rect, face, color, colours);
        paint_column_header(&painter, rect, face, color, colours);
        let radius = column_needle_radius(rect.width(), rect.height());
        let hub = egui::pos2(rect.center().x, rect.bottom() - NEEDLE_HUB_INSET);
        let weight = (radius / NEEDLE_RADIUS_CAP).clamp(0.55, 1.0);
        paint_needle_dial(
            &painter, rect, hub, radius, face.cents, face.valid, tolerance, color, colours, false,
            weight,
        );
    });
}

/// `weight` is 1 on the full face; a column scales the strokes with its
/// radius.  `labels` is off on a column: its header already says the cents.
#[allow(clippy::too_many_arguments)]
fn paint_needle_dial(
    painter: &egui::Painter,
    rect: Rect,
    hub: Pos2,
    radius: f32,
    cents: f32,
    valid: bool,
    tolerance: f32,
    color: egui::Color32,
    colours: &Palette,
    labels: bool,
    weight: f32,
) {
    if radius <= 0.0 || !radius.is_finite() {
        return;
    }
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
        Stroke::new(9.0 * weight, colours.line2),
    ));
    let tolerance_fraction = (tolerance / 50.0).clamp(0.0, 1.0);
    painter.add(Shape::line(
        arc(-tolerance_fraction, tolerance_fraction, 24),
        Stroke::new(9.0 * weight, with_alpha(colours.path[2], 140)),
    ));
    painter.add(Shape::line(
        arc(-1.0 / 50.0, 1.0 / 50.0, 4),
        Stroke::new(10.0 * weight, colours.path[2]),
    ));

    for tick in -10..=10 {
        let fraction = tick as f32 / 10.0;
        let inner = point(fraction);
        let direction = (inner - hub).normalized();
        let major = tick % 2 == 0;
        painter.line_segment(
            [
                inner - direction * ((if major { 15.0 } else { 9.0 }) * weight),
                inner - direction * 5.0 * weight,
            ],
            Stroke::new(
                weight.max(0.75),
                if major {
                    with_alpha(colours.text2, 190)
                } else {
                    with_alpha(colours.text3, 150)
                },
            ),
        );
    }
    if labels {
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
    }

    if valid {
        let fraction = (cents / 50.0).clamp(-1.0, 1.0);
        let tip = point(fraction);
        let start = egui::pos2(
            hub.x + (tip.x - hub.x) * 0.16,
            hub.y + (tip.y - hub.y) * 0.16,
        );
        painter.line_segment([start, tip], Stroke::new(3.0 * weight, color));
        painter.circle_filled(tip, 2.6 * weight, color);
    }
    painter.circle_filled(hub, 7.0 * weight, colours.bg);
    painter.circle_stroke(hub, 7.0 * weight, Stroke::new(1.2 * weight, colours.line));
    painter.circle_filled(hub, 3.2 * weight, color);
}
