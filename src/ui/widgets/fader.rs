//! The strip fader.  Unity sits at three quarters of the travel whatever
//! the range, so caps at 0.0 line up.

use eframe::egui;
use egui::{Align, CornerRadius, FontId, Layout, Rect, Sense, Stroke, Vec2};

use crate::ui::fonts::Face;
use crate::ui::theme::{font, palette, TypeRole};
use crate::ui::widgets::{format_db, palette_focus, row_info};

use super::slider::{key_steps, slider_access};

/// Inclusive dB range.  Unity (0.0) is the double-click value when it lies
/// inside the range.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct FaderRange {
    pub min_db: f32,
    pub max_db: f32,
}

const READOUT_H: f32 = 16.0;
const RAIL_W: f32 = 3.0;
const CAP_W: f32 = 24.0;
const CAP_H: f32 = 12.0;
/// Wider than the cap, so the tick shows beside a cap parked at unity.
const UNITY_TICK_HALF_W: f32 = CAP_W * 0.5 + 4.0;
/// Drag rate at and near unity, dB per point.
const NEAR_DB_PER_PT: f32 = 0.1;
/// Drag rate once the level is more than 12 dB from unity.
const FAR_DB_PER_PT: f32 = 0.5;
const FINE_DB_PER_PT: f32 = 0.01;
const NEAR_UNITY_DB: f32 = 12.0;

/// Height of a strip fader: `clamp(strip_height × 0.3, 140, 300)`.
pub(crate) const fn fader_height(strip_height: f32) -> f32 {
    (strip_height * 0.3).clamp(140.0, 300.0)
}

/// A typed level.  Accepts the printed form, a hyphen, and `−∞`.
pub(crate) fn parse_level(text: &str) -> Option<f32> {
    let lower = text.trim().to_lowercase();
    let stripped = lower.strip_suffix("db").unwrap_or(&lower).trim();
    let bare = stripped.replace('−', "-").replace(' ', "");
    match bare.as_str() {
        "" => None,
        "-∞" | "-inf" => Some(f32::NEG_INFINITY),
        "∞" | "inf" | "+inf" => Some(f32::INFINITY),
        _ => bare.parse().ok(),
    }
}

/// How many dB a pointer movement adds.  `pointer_dy` is egui's, so up is
/// negative and the level goes up.
pub(super) fn drag_delta_db(value_db: f32, pointer_dy: f32, fine: bool) -> f32 {
    let per_pt = if fine {
        FINE_DB_PER_PT
    } else if value_db.abs() <= NEAR_UNITY_DB {
        NEAR_DB_PER_PT
    } else {
        FAR_DB_PER_PT
    };
    -pointer_dy * per_pt
}

/// How many dB one wheel gesture adds.  `lines` is positive for a scroll up.
pub(super) fn scroll_delta_db(lines: f32, fine: bool) -> f32 {
    lines * if fine { 0.1 } else { 0.5 }
}

pub(crate) fn fader(
    ui: &mut egui::Ui,
    value_db: &mut f32,
    range: FaderRange,
    height: f32,
    label: &str,
) -> egui::Response {
    let min = range.min_db.min(range.max_db);
    let max = range.min_db.max(range.max_db);
    if !value_db.is_finite() {
        *value_db = min;
    }
    *value_db = value_db.clamp(min, max);

    let readout_font = font(Face::Mono, TypeRole::GridHeader);
    let text = format_db(*value_db, 1);
    let text_w = measure(ui, &text, &readout_font);
    let width = text_w.max(CAP_W).max(36.0);
    let height = height.max(READOUT_H + 24.0);
    let (rect, mut response) =
        ui.allocate_exact_size(Vec2::new(width, height), Sense::click_and_drag());

    let readout = Rect::from_min_max(
        egui::pos2(rect.left(), rect.bottom() - READOUT_H),
        rect.right_bottom(),
    );
    let rail = Rect::from_center_size(
        egui::pos2(rect.center().x, (rect.top() + readout.top()) * 0.5),
        Vec2::new(RAIL_W, (readout.top() - rect.top() - 4.0).max(24.0)),
    );

    let edit_id = response.id.with("type");
    let mut editing = ui
        .ctx()
        .data(|data| data.get_temp::<String>(edit_id))
        .is_some();
    let mut changed = false;

    if editing {
        changed |= edit_value(
            ui,
            &mut editing,
            value_db,
            min,
            max,
            readout,
            edit_id,
            label,
        );
    } else {
        changed |= gesture(
            ui,
            &mut response,
            value_db,
            min,
            max,
            rail,
            readout,
            &mut editing,
        );
        if editing {
            let seed = format_db(*value_db, 1);
            ui.ctx().data_mut(|data| data.insert_temp(edit_id, seed));
        }
    }

    if changed {
        *value_db = value_db.clamp(min, max);
        response.mark_changed();
    }

    paint(
        ui,
        &response,
        rect,
        rail,
        readout,
        *value_db,
        min,
        max,
        editing,
        &readout_font,
    );
    slider_access(
        ui,
        &response,
        label,
        f64::from(*value_db),
        f64::from(min),
        f64::from(max),
        f64::from(NEAR_DB_PER_PT),
        &format_db(*value_db, 1),
    );
    response.on_hover_text(
        "Drag to change the level. Shift drags finely, the wheel steps 0.5 dB, and a double-click sets 0.0. Double-click the value to type one.",
    )
}

fn measure(ui: &egui::Ui, text: &str, font_id: &FontId) -> f32 {
    ui.painter()
        .layout_no_wrap(text.to_owned(), font_id.clone(), palette(ui.ctx()).text)
        .size()
        .x
}

#[allow(clippy::too_many_arguments)]
fn gesture(
    ui: &egui::Ui,
    response: &mut egui::Response,
    value_db: &mut f32,
    min: f32,
    max: f32,
    rail: Rect,
    readout: Rect,
    editing: &mut bool,
) -> bool {
    let fine = ui.input(|input| input.modifiers.shift);
    let mut changed = false;
    if let Some(pos) = response.interact_pointer_pos() {
        if response.double_clicked() {
            if readout.contains(pos) {
                *editing = true;
            } else if min <= 0.0 && max >= 0.0 {
                *value_db = 0.0;
                changed = true;
            }
        } else if response.dragged() {
            let step = if fine { FINE_DB_PER_PT } else { NEAR_DB_PER_PT };
            let next = *value_db + drag_delta_db(*value_db, response.drag_delta().y, fine);
            *value_db = snap(next, step).clamp(min, max);
            changed = true;
        } else if response.clicked() && !readout.contains(pos) && rail.expand(8.0).contains(pos) {
            *value_db = snap(db_at_y(pos.y, rail, min, max), NEAR_DB_PER_PT).clamp(min, max);
            changed = true;
        }
    }
    if response.hovered() {
        let (lines, wheel_fine) = wheel(ui);
        if lines != 0.0 {
            let step = if wheel_fine { 0.1 } else { 0.5 };
            *value_db = snap(*value_db + scroll_delta_db(lines, wheel_fine), step).clamp(min, max);
            changed = true;
        }
    }
    changed |= keys(ui, response, value_db, min, max);
    if response.hovered() || response.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
    }
    changed
}

#[allow(clippy::too_many_arguments)]
fn edit_value(
    ui: &mut egui::Ui,
    editing: &mut bool,
    value_db: &mut f32,
    min: f32,
    max: f32,
    readout: Rect,
    edit_id: egui::Id,
    label: &str,
) -> bool {
    let mut text = ui
        .ctx()
        .data(|data| data.get_temp::<String>(edit_id))
        .unwrap_or_else(|| format_db(*value_db, 1));
    let opened = ui
        .ctx()
        .data(|data| data.get_temp::<bool>(edit_id.with("open")))
        .unwrap_or(true);
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(readout)
            .layout(Layout::left_to_right(Align::Center)),
    );
    let edit = child.add(
        egui::TextEdit::singleline(&mut text)
            .font(font(Face::Mono, TypeRole::GridHeader))
            .desired_width(readout.width())
            .hint_text(label),
    );
    if opened {
        edit.request_focus();
        ui.ctx()
            .data_mut(|data| data.insert_temp(edit_id.with("open"), false));
    }
    row_info(
        &edit,
        egui::Role::TextInput,
        true,
        label,
        None,
        Some(text.clone()),
    );
    let enter = child.input(|input| input.key_pressed(egui::Key::Enter));
    let escape = child.input(|input| input.key_pressed(egui::Key::Escape));
    let mut changed = false;
    if escape {
        *editing = false;
    } else if !opened && (enter || edit.lost_focus()) {
        if let Some(parsed) = parse_level(&text) {
            *value_db = parsed.clamp(min, max);
            changed = true;
        }
        *editing = false;
    }
    if *editing {
        ui.ctx().data_mut(|data| data.insert_temp(edit_id, text));
    } else {
        ui.ctx().data_mut(|data| {
            data.remove_temp::<String>(edit_id);
            data.remove_temp::<bool>(edit_id.with("open"));
        });
    }
    changed
}

fn keys(ui: &egui::Ui, response: &egui::Response, value_db: &mut f32, min: f32, max: f32) -> bool {
    let (steps, fine, set_value) = key_steps(ui, response, true);
    let mut changed = false;
    if steps != 0 {
        let step = if fine { FINE_DB_PER_PT } else { NEAR_DB_PER_PT };
        *value_db = snap(*value_db + steps as f32 * step, step).clamp(min, max);
        changed = true;
    }
    if let Some(requested) = set_value {
        let next = snap(requested as f32, NEAR_DB_PER_PT).clamp(min, max);
        if next != *value_db {
            *value_db = next;
            changed = true;
        }
    }
    changed
}

fn wheel(ui: &egui::Ui) -> (f32, bool) {
    ui.input(|input| {
        let mut lines = 0.0;
        for event in &input.events {
            if let egui::Event::MouseWheel { unit, delta, .. } = event {
                lines += match unit {
                    egui::MouseWheelUnit::Point => delta.y / 50.0,
                    egui::MouseWheelUnit::Line | egui::MouseWheelUnit::Page => delta.y,
                };
            }
        }
        (lines, input.modifiers.shift)
    })
}

fn snap(db: f32, step: f32) -> f32 {
    let step = if step.is_finite() && step > 0.0 {
        step
    } else {
        NEAR_DB_PER_PT
    };
    (db / step).round() * step
}

/// Where unity sits on every fader whose range spans it, as a fraction of
/// the travel from the bottom.
const UNITY_TRAVEL: f32 = 0.75;
/// Bend of the taper below unity.  Higher gives more travel near unity
/// and less near the floor.
const TAPER_BEND: f32 = 3.0;

/// The fraction of the travel, 0 at the bottom, that `db` sits at.  A range
/// that spans unity puts it at [`UNITY_TRAVEL`] whatever the range: above
/// it the travel is linear to `max`, below it bends toward `min`.  A range
/// that does not span unity is linear.
pub(super) fn travel_at_db(db: f32, min: f32, max: f32) -> f32 {
    let db = db.clamp(min, max);
    if max > 0.0 && min < 0.0 {
        if db >= 0.0 {
            UNITY_TRAVEL + (1.0 - UNITY_TRAVEL) * db / max
        } else {
            UNITY_TRAVEL * (1.0 - bend(db / min))
        }
    } else if max <= 0.0 && min < 0.0 {
        1.0 - bend((db - max) / (min - max))
    } else {
        let span = (max - min).max(f32::EPSILON);
        (db - min) / span
    }
}

pub(super) fn db_at_travel(travel: f32, min: f32, max: f32) -> f32 {
    let travel = travel.clamp(0.0, 1.0);
    let db = if max > 0.0 && min < 0.0 {
        if travel >= UNITY_TRAVEL {
            (travel - UNITY_TRAVEL) / (1.0 - UNITY_TRAVEL) * max
        } else {
            unbend(1.0 - travel / UNITY_TRAVEL) * min
        }
    } else if max <= 0.0 && min < 0.0 {
        max + unbend(1.0 - travel) * (min - max)
    } else {
        min + travel * (max - min)
    };
    db.clamp(min, max)
}

/// 0 → 0 and 1 → 1, steep at 0.  `x` is how far below the top of the
/// tapered part the level is, as a fraction of its depth.
fn bend(x: f32) -> f32 {
    (1.0 + TAPER_BEND * x.clamp(0.0, 1.0)).ln() / (1.0 + TAPER_BEND).ln()
}

fn unbend(y: f32) -> f32 {
    ((1.0 + TAPER_BEND).powf(y.clamp(0.0, 1.0)) - 1.0) / TAPER_BEND
}

fn db_at_y(y: f32, rail: Rect, min: f32, max: f32) -> f32 {
    let t = (rail.bottom() - y) / rail.height().max(1.0);
    db_at_travel(t, min, max)
}

fn y_at_db(db: f32, rail: Rect, min: f32, max: f32) -> f32 {
    rail.bottom() - travel_at_db(db, min, max) * rail.height()
}

#[allow(clippy::too_many_arguments)]
fn paint(
    ui: &egui::Ui,
    response: &egui::Response,
    rect: Rect,
    rail: Rect,
    readout: Rect,
    value_db: f32,
    min: f32,
    max: f32,
    editing: bool,
    readout_font: &FontId,
) {
    let colours = palette(ui.ctx());
    let painter = ui.painter();
    painter.rect_filled(rail, CornerRadius::same(1), colours.well);
    let y = y_at_db(value_db, rail, min, max);
    if y < rail.bottom() - 0.5 {
        painter.rect_filled(
            Rect::from_min_max(egui::pos2(rail.left(), y), rail.right_bottom()),
            CornerRadius::same(1),
            colours.text3,
        );
    }
    if min <= 0.0 && max >= 0.0 {
        let unity = y_at_db(0.0, rail, min, max);
        painter.hline(
            (rail.center().x - UNITY_TICK_HALF_W)..=(rail.center().x + UNITY_TICK_HALF_W),
            unity,
            Stroke::new(1.5, colours.text),
        );
    }
    let cap_y = y.clamp(rail.top() + CAP_H * 0.5, rail.bottom() - CAP_H * 0.5);
    let cap = Rect::from_center_size(egui::pos2(rail.center().x, cap_y), Vec2::new(CAP_W, CAP_H));
    let cap_stroke = if response.dragged() || response.hovered() {
        colours.accent
    } else {
        colours.line2
    };
    painter.rect_filled(cap, CornerRadius::same(2), colours.slot);
    painter.rect_stroke(
        cap,
        CornerRadius::same(2),
        Stroke::new(1.0, cap_stroke),
        egui::StrokeKind::Inside,
    );
    painter.hline(
        (cap.center().x - 4.0)..=(cap.center().x + 4.0),
        cap.center().y,
        Stroke::new(1.5, colours.text3),
    );
    if !editing {
        painter.text(
            readout.center(),
            egui::Align2::CENTER_CENTER,
            format_db(value_db, 1),
            readout_font.clone(),
            colours.text,
        );
    }
    palette_focus(ui, response, rect);
}

#[cfg(test)]
mod tests;
