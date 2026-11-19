//! The strip fader (`docs/ui-spec.md` §4.1).
//!
//! The rail is 3 pt and the cap is 24 × 12 pt, with a tick at unity.
//! Dragging moves 0.1 dB per point while the level is within 12 dB of
//! unity, and 0.5 dB per point further away.  Shift drags at 0.01 dB per
//! point.  A double-click on the rail sets 0.0, a double-click on the
//! value types one, and the wheel steps 0.5 dB (0.1 dB with Shift).
//! [`fader_height`] is the whole control, readout included.

use eframe::egui;
use egui::{Align, CornerRadius, FontId, Layout, Rect, Sense, Stroke, Vec2};

use crate::ui::fonts::Face;
use crate::ui::theme::{font, palette, TypeRole};
use crate::ui::widgets::{palette_focus, row_info};

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

/// The level as the fader prints it.  Unity is `0.0`, off is `−∞`, and
/// every other finite value carries one decimal and `dB`.
pub(crate) fn format_level(db: f32) -> String {
    if db.is_nan() || db == f32::NEG_INFINITY || db <= -120.0 {
        return "−∞".to_owned();
    }
    if !db.is_finite() {
        return format_level(120.0);
    }
    if db.abs() < 0.05 {
        return "0.0".to_owned();
    }
    if db < 0.0 {
        format!("−{:.1} dB", db.abs())
    } else {
        format!("{db:.1} dB")
    }
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

/// Rail, cap, unity tick and the typed readout.  `height` is the whole
/// control; pass [`fader_height`].
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
    let text = format_level(*value_db);
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
            let seed = format_level(*value_db);
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
        *value_db,
        min,
        max,
        label,
        &format_level(*value_db),
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
        .unwrap_or_else(|| format_level(*value_db));
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
    if !response.has_focus() {
        return false;
    }
    ui.memory_mut(|memory| {
        memory.set_focus_lock_filter(
            response.id,
            egui::EventFilter {
                horizontal_arrows: true,
                vertical_arrows: true,
                ..Default::default()
            },
        );
    });
    let (steps, fine, set_value) = ui.input(|input| {
        use egui::accesskit::{Action, ActionData};
        let down =
            input.num_presses(egui::Key::ArrowDown) + input.num_presses(egui::Key::ArrowLeft);
        let up = input.num_presses(egui::Key::ArrowUp) + input.num_presses(egui::Key::ArrowRight);
        let access = input.num_accesskit_action_requests(response.id, Action::Increment) as i32
            - input.num_accesskit_action_requests(response.id, Action::Decrement) as i32;
        let set_value = input
            .accesskit_action_requests(response.id, Action::SetValue)
            .filter_map(|request| match request.data {
                Some(ActionData::NumericValue(value)) => Some(value),
                _ => None,
            })
            .last();
        (
            up as i32 - down as i32 + access,
            input.modifiers.shift,
            set_value,
        )
    });
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

fn db_at_y(y: f32, rail: Rect, min: f32, max: f32) -> f32 {
    let span = (max - min).max(f32::EPSILON);
    let t = ((rail.bottom() - y) / rail.height().max(1.0)).clamp(0.0, 1.0);
    min + t * span
}

fn y_at_db(db: f32, rail: Rect, min: f32, max: f32) -> f32 {
    let span = (max - min).max(f32::EPSILON);
    let t = ((db - min) / span).clamp(0.0, 1.0);
    rail.bottom() - t * rail.height()
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
            (rail.center().x - 7.0)..=(rail.center().x + 7.0),
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
            format_level(value_db),
            readout_font.clone(),
            colours.text,
        );
    }
    palette_focus(ui, response, rect);
}

fn slider_access(
    ui: &egui::Ui,
    response: &egui::Response,
    value_db: f32,
    min: f32,
    max: f32,
    label: &str,
    text: &str,
) {
    let enabled = ui.is_enabled();
    let numeric = f64::from(value_db);
    response.widget_info({
        let label = label.to_owned();
        let text = text.to_owned();
        move || {
            let mut info = egui::WidgetInfo::slider(enabled, numeric, label.clone());
            info.current_text_value = Some(text.clone());
            info
        }
    });
    ui.ctx().accesskit_node_builder(response.id, |builder| {
        use egui::accesskit::Action;
        builder.set_min_numeric_value(f64::from(min));
        builder.set_max_numeric_value(f64::from(max));
        builder.set_numeric_value_step(f64::from(NEAR_DB_PER_PT));
        builder.add_action(Action::SetValue);
        if value_db < max {
            builder.add_action(Action::Increment);
        }
        if value_db > min {
            builder.add_action(Action::Decrement);
        }
    });
}

#[cfg(test)]
mod tests;
