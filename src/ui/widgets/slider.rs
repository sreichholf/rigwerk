//! Gain and styled sliders, plus focus/row helpers they share with other widgets.

use eframe::egui;
use egui::{Color32, CornerRadius, Rect, Sense, Stroke, Vec2};

use crate::ui::theme::{with_alpha, BORDER, CARD_RAISED, TEXT};

/// Colour and accessible name for [`styled_slider`].
///
/// A bare [`Color32`] keeps existing call sites. A `(color, name)` pair is the
/// name AccessKit reads.
pub(crate) struct SliderStyle {
    color: Color32,
    name: String,
    decibels: bool,
}

impl From<Color32> for SliderStyle {
    fn from(color: Color32) -> Self {
        Self {
            color,
            name: String::new(),
            decibels: false,
        }
    }
}

impl From<(Color32, &str)> for SliderStyle {
    fn from((color, name): (Color32, &str)) -> Self {
        Self {
            color,
            name: name.to_owned(),
            decibels: false,
        }
    }
}

impl From<(Color32, String)> for SliderStyle {
    fn from((color, name): (Color32, String)) -> Self {
        Self {
            color,
            name,
            decibels: false,
        }
    }
}

/// Keyboard focus on a custom-painted control. Enter and Space already activate
/// a focusable widget; this is the ring that was missing.
pub(crate) fn show_focus_ring(ui: &egui::Ui, response: &egui::Response, rect: Rect, radius: u8) {
    if !response.has_focus() {
        return;
    }
    let mut stroke = ui.visuals().selection.stroke;
    stroke.width = 2.0;
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(radius),
        stroke,
        egui::StrokeKind::Inside,
    );
}

/// Name and value for a custom row. `selected` is the toggled value.
pub(crate) fn row_info(
    response: &egui::Response,
    role: egui::Role,
    enabled: bool,
    name: impl Into<String>,
    selected: Option<bool>,
    value: Option<String>,
) {
    let name = name.into();
    response.widget_info(move || {
        let mut info = match selected {
            Some(on) => egui::WidgetInfo::selected(role, enabled, on, name.clone()),
            None => egui::WidgetInfo::labeled(role, enabled, name.clone()),
        };
        if let Some(value) = value.clone() {
            info.current_text_value = Some(value);
        }
        info
    });
}

pub(crate) fn db_slider(
    ui: &mut egui::Ui,
    value: &mut f32,
    min: f32,
    max: f32,
    accent: impl Into<SliderStyle>,
) -> bool {
    let mut style = accent.into();
    if style.name.is_empty() {
        style.name = "Gain".to_owned();
    }
    style.decibels = true;
    styled_slider(ui, value, min, max, 0.1, style).changed()
}

/// The gain slider's look, with an explicit step, for other values in the app.
pub(crate) fn styled_slider(
    ui: &mut egui::Ui,
    value: &mut f32,
    min: f32,
    max: f32,
    step: f32,
    accent: impl Into<SliderStyle>,
) -> egui::Response {
    let mut style = accent.into();
    let (rect, mut response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), 22.0),
        Sense::click_and_drag(),
    );
    // Inset the track so the 7px grip stays inside the allocated rect.
    // Empty portion uses a lift above CARD so equal-length tracks read as
    // equal (a short fill must not look like a short slider).
    let track = Rect::from_center_size(
        rect.center(),
        Vec2::new((rect.width() - 14.0).max(0.0), 4.0),
    );
    ui.painter()
        .rect_filled(track, CornerRadius::same(2), Color32::from_rgb(42, 52, 70));
    if response.dragged() || response.clicked() {
        if let Some(position) = response.interact_pointer_pos() {
            let fraction = ((position.x - track.left()) / track.width()).clamp(0.0, 1.0);
            let new_value = min + fraction * (max - min);
            let step = step.max(f32::EPSILON);
            *value = ((new_value / step).round() * step).clamp(min, max);
            response.mark_changed();
        }
    }
    if response.enabled() && !response.dragged() {
        if response.has_focus() {
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
        }
        let (key_steps, set_value) = slider_keyboard_and_accesskit(ui, &response);
        if apply_slider_steps(value, min, max, step, key_steps) {
            response.mark_changed();
        }
        if let Some(requested) = set_value {
            let step = step.max(f32::EPSILON);
            let next = ((requested as f32 / step).round() * step).clamp(min, max);
            if next != *value {
                *value = next;
                response.mark_changed();
            }
        }
    }
    let fraction = ((*value - min) / (max - min)).clamp(0.0, 1.0);
    let x = track.left() + track.width() * fraction;
    if fraction > 0.0 {
        ui.painter().rect_filled(
            Rect::from_min_max(track.min, egui::pos2(x, track.bottom())),
            CornerRadius::same(2),
            with_alpha(style.color, 190),
        );
    }
    ui.painter()
        .circle_filled(egui::pos2(x, track.center().y), 7.0, CARD_RAISED);
    ui.painter().circle_stroke(
        egui::pos2(x, track.center().y),
        6.0,
        Stroke::new(1.2, if response.hovered() { TEXT } else { BORDER }),
    );
    ui.painter()
        .circle_filled(egui::pos2(x, track.center().y), 3.0, style.color);
    show_focus_ring(ui, &response, rect, 6);
    let value_text = if style.decibels {
        format!("{:+.1} dB", *value)
    } else {
        format_slider_value(*value, step)
    };
    if style.name.is_empty() {
        style.name.clone_from(&value_text);
    }
    let enabled = ui.is_enabled();
    let numeric = f64::from(*value);
    let name = style.name;
    response.widget_info({
        let value_text = value_text.clone();
        let name = name.clone();
        move || {
            let mut info = egui::WidgetInfo::slider(enabled, numeric, name.clone());
            info.current_text_value = Some(value_text.clone());
            info
        }
    });
    let step_f = f64::from(step.max(f32::EPSILON));
    ui.ctx().accesskit_node_builder(response.id, |builder| {
        use egui::accesskit::Action;
        builder.set_min_numeric_value(f64::from(min));
        builder.set_max_numeric_value(f64::from(max));
        builder.set_numeric_value_step(step_f);
        builder.add_action(Action::SetValue);
        if numeric < f64::from(max) {
            builder.add_action(Action::Increment);
        }
        if numeric > f64::from(min) {
            builder.add_action(Action::Decrement);
        }
    });
    response
}

/// Arrow keys and a screen reader's increment or set-value, while a drag owns
/// the pointer.
fn slider_keyboard_and_accesskit(ui: &egui::Ui, response: &egui::Response) -> (i32, Option<f64>) {
    let key_steps = if response.has_focus() {
        ui.input(|input| {
            let down =
                input.num_presses(egui::Key::ArrowLeft) + input.num_presses(egui::Key::ArrowDown);
            let up =
                input.num_presses(egui::Key::ArrowRight) + input.num_presses(egui::Key::ArrowUp);
            up as i32 - down as i32
        })
    } else {
        0
    };
    let (access_steps, set_value) = ui.input(|input| {
        use egui::accesskit::{Action, ActionData};
        let steps = input.num_accesskit_action_requests(response.id, Action::Increment) as i32
            - input.num_accesskit_action_requests(response.id, Action::Decrement) as i32;
        let set_value = input
            .accesskit_action_requests(response.id, Action::SetValue)
            .filter_map(|request| match request.data {
                Some(ActionData::NumericValue(value)) => Some(value),
                _ => None,
            })
            .last();
        (steps, set_value)
    });
    (key_steps + access_steps, set_value)
}

fn apply_slider_steps(value: &mut f32, min: f32, max: f32, step: f32, steps: i32) -> bool {
    if steps == 0 {
        return false;
    }
    let next = nudge_slider(*value, min, max, step, steps);
    if next == *value {
        return false;
    }
    *value = next;
    true
}

pub(crate) fn nudge_slider(value: f32, min: f32, max: f32, step: f32, steps: i32) -> f32 {
    let step = if step.is_finite() && step > 0.0 {
        step
    } else {
        f32::EPSILON
    };
    let snapped = ((value + steps as f32 * step) / step).round() * step;
    snapped.clamp(min, max)
}

fn format_slider_value(value: f32, step: f32) -> String {
    let decimals = if step + 1.0e-4 >= 1.0 {
        0
    } else if step + 1.0e-4 >= 0.1 {
        1
    } else if step + 1.0e-4 >= 0.01 {
        2
    } else {
        3
    };
    format!("{value:.decimals$}")
}
