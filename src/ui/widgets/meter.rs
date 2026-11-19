//! Level meters (`docs/ui-spec.md` §4.1 and the 4 pt strip meter in §4.3).
//!
//! Bars are 5 pt for one or two channels and 2–3 pt for more, up to eight.
//! The scale is 0, 6, 12, 24 and 48 dB below full scale.  The peak is a 2 pt
//! line held for 1.5 s.  A sample at or above 0 dBFS latches `error` on that
//! bar until the bar is clicked.  `meter` is the fill up to −6 dBFS,
//! `meter_hot` from there to −1, and `error` above −1.

use eframe::egui;
use egui::{CornerRadius, Rect, Sense, Stroke, Vec2};

use crate::ui::fonts::Face;
use crate::ui::theme::{font, palette, Palette, TypeRole};
use crate::ui::widgets::row_info;

const FLOOR_DB: f32 = -48.0;
const PEAK_HOLD_S: f64 = 1.5;
const MAX_CHANNELS: usize = 8;
const SCALE_W: f32 = 22.0;

/// One channel's held peak and clip latch.  The widget advances it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct MeterChannel {
    pub(crate) peak_db: f32,
    pub(crate) hold_until: f64,
    pub(crate) clipped: bool,
}

impl Default for MeterChannel {
    fn default() -> Self {
        Self {
            peak_db: FLOOR_DB - 24.0,
            hold_until: 0.0,
            clipped: false,
        }
    }
}

/// Linear amplitude to dBFS.  Silence is below the scale.
pub(crate) fn linear_to_db(amplitude: f32) -> f32 {
    if !amplitude.is_finite() || amplitude <= 1.0e-8 {
        FLOOR_DB - 24.0
    } else {
        20.0 * amplitude.log10()
    }
}

/// 0 at the floor (−48 dBFS) and 1 at 0 dBFS.  Hotter than full scale stays 1.
pub(super) fn meter_fraction(db: f32) -> f32 {
    ((db - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0)
}

/// Bar thickness: 5 pt up to two channels, 3 pt up to four, 2 pt after that.
pub(super) fn bar_width(channels: usize) -> f32 {
    match channels {
        0..=2 => 5.0,
        3..=4 => 3.0,
        _ => 2.0,
    }
}

fn advance(channel: &mut MeterChannel, db: f32, now: f64) {
    let db = if db.is_finite() { db } else { FLOOR_DB };
    if db >= 0.0 {
        channel.clipped = true;
    }
    // A level that only equals the held peak does not refresh the hold.
    if db > channel.peak_db || now >= channel.hold_until {
        channel.peak_db = db;
        channel.hold_until = now + PEAK_HOLD_S;
    }
}

/// Which colour a level uses.  The boundaries belong to the lower zone:
/// −6 is still `meter`, −1 is still `meter_hot`.
pub(super) fn zone_colour(colours: &Palette, db: f32) -> egui::Color32 {
    if db > -1.0 {
        colours.error
    } else if db > -6.0 {
        colours.meter_hot
    } else {
        colours.meter
    }
}

/// Vertical bars for `levels` (linear amplitudes, one per channel).
/// `state` is resized to the channels drawn.  `show_scale` is the 0–48
/// scale; narrow strips turn it off.
pub(crate) fn vertical_meter(
    ui: &mut egui::Ui,
    levels: &[f32],
    state: &mut Vec<MeterChannel>,
    height: f32,
    show_scale: bool,
    label: &str,
) -> egui::Response {
    let count = levels.len().clamp(0, MAX_CHANNELS);
    if state.len() != count {
        state.resize(count, MeterChannel::default());
    }
    let now = ui.input(|input| input.time);
    let mut loudest = FLOOR_DB - 24.0;
    for (channel, level) in state.iter_mut().zip(levels.iter().take(count)) {
        let db = linear_to_db(*level);
        loudest = loudest.max(db);
        advance(channel, db, now);
    }

    let width = meter_width(count, show_scale);
    let (rect, mut response) =
        ui.allocate_exact_size(Vec2::new(width, height.max(48.0)), Sense::click());
    let bars = bar_rects(rect, count, show_scale);
    if response.clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
            if let Some(index) = bars.iter().position(|bar| bar.contains(pos)) {
                if state.get(index).is_some_and(|channel| channel.clipped) {
                    state[index].clipped = false;
                    response.mark_changed();
                }
            }
        }
    }
    let live: Vec<f32> = levels
        .iter()
        .take(count)
        .copied()
        .map(linear_to_db)
        .collect();
    paint_vertical(ui, rect, &bars, &live, state, show_scale);
    let value = super::fader::format_level(loudest);
    row_info(&response, egui::Role::Meter, true, label, None, Some(value));
    response
}

fn meter_width(count: usize, show_scale: bool) -> f32 {
    if count == 0 {
        return if show_scale { SCALE_W } else { 8.0 };
    }
    let bars = count as f32 * bar_width(count) + (count - 1) as f32 * 2.0;
    if show_scale {
        SCALE_W + bars
    } else {
        bars
    }
}

fn bar_rects(rect: Rect, count: usize, show_scale: bool) -> Vec<Rect> {
    let left = if show_scale {
        rect.left() + SCALE_W
    } else {
        rect.left()
    };
    let width = bar_width(count);
    (0..count)
        .map(|index| {
            let x = left + index as f32 * (width + 2.0);
            Rect::from_min_size(egui::pos2(x, rect.top()), Vec2::new(width, rect.height()))
        })
        .collect()
}

fn paint_vertical(
    ui: &egui::Ui,
    rect: Rect,
    bars: &[Rect],
    live_db: &[f32],
    state: &[MeterChannel],
    show_scale: bool,
) {
    let colours = palette(ui.ctx());
    let painter = ui.painter();
    if show_scale {
        let scale_font = font(Face::Mono, TypeRole::GridHeader);
        for mark in [0.0, 6.0, 12.0, 24.0, 48.0] {
            let y = rect.bottom() - meter_fraction(-mark) * rect.height();
            painter.text(
                egui::pos2(rect.left() + SCALE_W - 4.0, y),
                egui::Align2::RIGHT_CENTER,
                format!("{mark:.0}"),
                scale_font.clone(),
                colours.text3,
            );
        }
    }
    for ((bar, db), channel) in bars.iter().zip(live_db.iter()).zip(state.iter()) {
        painter.rect_filled(*bar, CornerRadius::same(1), colours.well);
        paint_fill(painter, *bar, *db, colours);
        let y = bar.bottom() - meter_fraction(channel.peak_db) * bar.height();
        painter.hline(
            (bar.left() - 1.0)..=(bar.right() + 1.0),
            y,
            Stroke::new(2.0, colours.text),
        );
        if channel.clipped {
            let cap = Rect::from_min_max(bar.min, egui::pos2(bar.right(), bar.top() + 4.0));
            painter.rect_filled(cap, CornerRadius::same(1), colours.error);
        }
    }
}

fn paint_fill(painter: &egui::Painter, bar: Rect, db: f32, colours: &Palette) {
    let top = bar.bottom() - meter_fraction(db) * bar.height();
    let y6 = bar.bottom() - meter_fraction(-6.0) * bar.height();
    let y1 = bar.bottom() - meter_fraction(-1.0) * bar.height();
    fill_zone(painter, bar, top, y6, bar.bottom(), colours.meter);
    fill_zone(painter, bar, top, y1, y6, colours.meter_hot);
    fill_zone(painter, bar, top, bar.top(), y1, colours.error);
}

fn fill_zone(
    painter: &egui::Painter,
    bar: Rect,
    level_y: f32,
    zone_top: f32,
    zone_bottom: f32,
    colour: egui::Color32,
) {
    let y_hi = level_y.max(zone_top);
    let y_lo = zone_bottom;
    if y_lo - y_hi > 0.4 {
        painter.rect_filled(
            Rect::from_min_max(egui::pos2(bar.left(), y_hi), egui::pos2(bar.right(), y_lo)),
            CornerRadius::ZERO,
            colour,
        );
    }
}

/// The 4 pt meter a strip keeps under its name while a sheet is open.
/// `level` is a linear amplitude.  There is no peak hold and no latch.
pub(crate) fn horizontal_meter(ui: &mut egui::Ui, level: f32, label: &str) -> egui::Response {
    let width = ui.available_width().clamp(8.0, 10_000.0);
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 4.0), Sense::hover());
    let colours = palette(ui.ctx());
    let db = linear_to_db(level);
    ui.painter()
        .rect_filled(rect, CornerRadius::same(1), colours.well);
    let end = rect.left() + meter_fraction(db) * rect.width();
    fill_h(ui, rect, rect.left(), end, zone_colour(colours, db));
    row_info(
        &response,
        egui::Role::Meter,
        true,
        label,
        None,
        Some(super::fader::format_level(db)),
    );
    response
}

fn fill_h(ui: &egui::Ui, rect: Rect, from: f32, to: f32, colour: egui::Color32) {
    if to - from > 0.4 && from < rect.right() {
        let to = to.min(rect.right());
        ui.painter().rect_filled(
            Rect::from_min_max(egui::pos2(from, rect.top()), egui::pos2(to, rect.bottom())),
            CornerRadius::ZERO,
            colour,
        );
    }
}

#[cfg(test)]
mod tests;
