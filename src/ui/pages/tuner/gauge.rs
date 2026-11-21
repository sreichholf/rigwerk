//! The tuner's two gauge faces.  The needle is the ±50 cent arc.  The strobe
//! is three discs seen from the front, wide to fine, and a disc holds inside
//! ±1 cent.  Disc speed eases toward the cents reading, and the pattern is
//! the integral of that speed, so a new reading cannot jump the discs.  Each
//! disc is clipped to its own band so a slide cannot spill.

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
/// How fast the disc speed closes on a new cents reading, per second.
/// A tenth of a second reaches most of the way there, so one noisy poll
/// cannot slam the pattern.
const STROBE_EASE_PER_SEC: f32 = 8.0;
/// Below this many pixels per second a disc that is heading for a stop is
/// already still.  It keeps the tuner from requesting frames forever.
const STROBE_SPEED_SETTLE: f32 = 0.5;
/// (lit segment, gap, gear) for the wide, medium and fine discs.
const DISCS: [(f32, f32, f32); 3] = [(168.0, 72.0, 1.0), (64.0, 28.0, 2.0), (16.0, 8.0, 5.0)];
/// One revolution of the shared phase.  A multiple of every disc period, so
/// wrapping it leaves each geared pattern where it was.
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

/// Gear-1 speed in pixels per second for this reading.  Zero inside ±1 cent,
/// for a silent face, and for a non-finite cents value.  The sign is the
/// direction: negative is flat, and flat moves left.
pub(super) fn strobe_target_speed(cents: f32, valid: bool) -> f32 {
    if !valid || !cents.is_finite() || cents.abs() <= STROBE_HOLD_CENTS {
        return 0.0;
    }
    cents.signum() * cents.abs().min(STROBE_SPEED_CAP_CENTS) * STROBE_PIXELS_PER_CENT
}

/// Moves `speed` part of the way to `target` over `dt` seconds.  The step is
/// a fraction of the gap, so a cents jump changes velocity and not position.
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

/// Advances the shared phase by `speed * dt` pixels and wraps it.  Callers
/// scale the result by a disc's gear.
pub(super) fn advance_strobe_phase(phase: f32, speed: f32, dt: f32) -> f32 {
    let phase = if phase.is_finite() { phase } else { 0.0 };
    if !speed.is_finite() || !dt.is_finite() || dt <= 0.0 {
        return phase.rem_euclid(STROBE_PHASE_WRAP);
    }
    (phase + speed * dt.min(0.05)).rem_euclid(STROBE_PHASE_WRAP)
}

/// The speed and the pattern position carried from one frame to the next.
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
    /// One frame.  `now` is the egui clock, in seconds.  The first call only
    /// records the clock, so a late first paint does not leap.
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

    /// True while the discs are moving or still catching a non-zero reading.
    fn moving(&self, cents: f32, valid: bool) -> bool {
        self.speed != 0.0 || strobe_target_speed(cents, valid) != 0.0
    }
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
    let motion_id = ui.scope_id().with("strobe_motion");
    let mut motion = ui
        .data(|data| data.get_temp::<StrobeMotion>(motion_id))
        .unwrap_or_default();
    motion.step(cents, valid, ui.input(|input| input.time));
    if motion.moving(cents, valid) {
        ui.ctx().request_repaint();
    }
    ui.data_mut(|data| data.insert_temp(motion_id, motion));
    for (index, (on, space, gear)) in DISCS.iter().copied().enumerate() {
        let top = bands.top() + index as f32 * (height + gap);
        let disc = Rect::from_min_max(
            egui::pos2(bands.left(), top),
            egui::pos2(bands.right(), top + height),
        );
        paint_disc(
            painter,
            disc,
            on,
            space,
            motion.phase * gear,
            color,
            colours.well,
        );
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
