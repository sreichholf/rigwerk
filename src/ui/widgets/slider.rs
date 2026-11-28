//! The slider, plus the focus and keyboard helpers it shares with the fader,
//! written once so both get an accessibility fix together.

use eframe::egui;
use egui::{Align, Color32, CornerRadius, Layout, Rect, Sense, Stroke, Vec2};

use crate::ui::fonts::Face;
use crate::ui::theme::{font, palette, with_alpha, TypeRole};

/// A bare [`Color32`] keeps existing call sites.  A `(color, name)` pair
/// is the name AccessKit reads.
pub(crate) struct SliderStyle {
    color: Color32,
    name: String,
    value: Option<String>,
    format: Option<fn(f32) -> String>,
    unit: &'static str,
    editable: bool,
    width: Option<f32>,
    logarithmic: bool,
}

impl SliderStyle {
    pub(crate) fn named(color: Color32, name: impl Into<String>) -> Self {
        Self {
            color,
            name: name.into(),
            value: None,
            format: None,
            unit: "",
            editable: false,
            width: None,
            logarithmic: false,
        }
    }

    /// The value a screen reader reads, when it is not the default number.
    pub(crate) fn value(mut self, text: impl Into<String>) -> Self {
        self.value = Some(text.into());
        self
    }

    /// Format the value from the current number, for a readout that must not
    /// lag the track.
    pub(crate) fn format(mut self, format: fn(f32) -> String) -> Self {
        self.format = Some(format);
        self
    }

    /// The unit the editable value strips before parsing; the formatter still
    /// draws it.
    pub(crate) fn unit(mut self, unit: &'static str) -> Self {
        self.unit = unit;
        self
    }

    /// Draw the readout as the editable number it was before the migration.
    pub(crate) fn editable(mut self) -> Self {
        self.editable = true;
        self
    }

    /// A track narrower than the space the row leaves it.
    pub(crate) fn width(mut self, width: f32) -> Self {
        self.width = Some(width);
        self
    }

    /// Equal travel per ratio instead of per unit; the positive ranges only.
    pub(crate) fn logarithmic(mut self) -> Self {
        self.logarithmic = true;
        self
    }
}

impl From<Color32> for SliderStyle {
    fn from(color: Color32) -> Self {
        Self::named(color, "")
    }
}

impl From<(Color32, &str)> for SliderStyle {
    fn from((color, name): (Color32, &str)) -> Self {
        Self::named(color, name)
    }
}

impl From<(Color32, String)> for SliderStyle {
    fn from((color, name): (Color32, String)) -> Self {
        Self::named(color, name)
    }
}

/// Enter and Space already activate a focused widget; this is the ring
/// that was missing.
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

/// A `step` of zero, or a non-finite one, is a continuous track: the
/// keyboard tick and the drag speed are then `span / 100`.
pub(crate) fn styled_slider(
    ui: &mut egui::Ui,
    value: &mut f32,
    min: f32,
    max: f32,
    step: f32,
    accent: impl Into<SliderStyle>,
) -> egui::Response {
    let style = accent.into();
    let step = if step.is_finite() && step > 0.0 {
        Some(step)
    } else {
        None
    };
    // A continuous track still needs a keyboard tick and a drag speed.
    let tick = step.unwrap_or((max - min) / 100.0);
    *value = value.clamp(min, max);
    let width = style.width.unwrap_or_else(|| ui.available_width());
    let (rect, mut response) =
        ui.allocate_exact_size(Vec2::new(width, 22.0), Sense::click_and_drag());
    // Inset so the grip stays inside the rect.  The empty part is the
    // well, so a short fill is not a short slider.  The caller colour is
    // only the filled part.
    let colours = palette(ui.ctx());
    let enabled = ui.is_enabled();
    let accent = if enabled { style.color } else { colours.text3 };
    let readout_font = font(Face::Mono, TypeRole::Secondary);
    let format = style.format;
    let custom = style.value.clone();
    let readout = |number: f32| {
        format
            .map(|format| format(number))
            .or_else(|| custom.clone())
            .unwrap_or_else(|| format_slider_value(number, tick))
    };
    let readout_w = if style.editable {
        let text = readout(*value);
        let text_w = ui
            .painter()
            .layout_no_wrap(text, readout_font, colours.text)
            .size()
            .x;
        (text_w + 16.0).max(48.0)
    } else {
        0.0
    };
    let track_left = rect.left() + 7.0;
    let track = Rect::from_min_max(
        egui::pos2(track_left, rect.center().y - 2.0),
        egui::pos2(
            (rect.right() - 7.0 - readout_w).max(track_left),
            rect.center().y + 2.0,
        ),
    );
    ui.painter()
        .rect_filled(track, CornerRadius::same(2), colours.well);
    if response.dragged() || response.clicked() {
        if let Some(position) = response.interact_pointer_pos() {
            if (track.left() - 7.0..=track.right() + 7.0).contains(&position.x) {
                let fraction =
                    ((position.x - track.left()) / track.width().max(f32::EPSILON)).clamp(0.0, 1.0);
                let mut new_value = slider_value_at(fraction, min, max, style.logarithmic);
                if let Some(step) = step {
                    new_value = (new_value / step).round() * step;
                }
                *value = new_value.clamp(min, max);
                response.mark_changed();
            }
        }
    }
    if response.enabled() && !response.dragged() {
        let (steps, _fine, set_value) = key_steps(ui, &response, false);
        if apply_slider_steps(value, min, max, step, tick, steps) {
            response.mark_changed();
        }
        if let Some(requested) = set_value {
            let next = match step {
                Some(step) => ((requested as f32 / step).round() * step).clamp(min, max),
                None => (requested as f32).clamp(min, max),
            };
            if next != *value {
                *value = next;
                response.mark_changed();
            }
        }
    }
    let fraction = slider_fraction(*value, min, max, style.logarithmic);
    let x = track.left() + track.width() * fraction;
    if fraction > 0.0 {
        ui.painter().rect_filled(
            Rect::from_min_max(track.min, egui::pos2(x, track.bottom())),
            CornerRadius::same(2),
            with_alpha(accent, 190),
        );
    }
    ui.painter()
        .circle_filled(egui::pos2(x, track.center().y), 7.0, colours.slot);
    ui.painter().circle_stroke(
        egui::pos2(x, track.center().y),
        6.0,
        Stroke::new(
            1.2,
            if !enabled {
                colours.line2
            } else if response.hovered() {
                colours.text
            } else {
                colours.line
            },
        ),
    );
    ui.painter()
        .circle_filled(egui::pos2(x, track.center().y), 3.0, accent);
    if style.editable {
        let readout_rect = Rect::from_min_max(
            egui::pos2(rect.right() - readout_w, rect.top()),
            rect.right_bottom(),
        );
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(readout_rect)
                .layout(Layout::right_to_left(Align::Center)),
        );
        let mut drag = egui::DragValue::new(value)
            .speed(f64::from(tick))
            .range(min..=max);
        if let Some(format) = format {
            drag = drag.custom_formatter(move |number, _| format(number as f32));
        }
        let unit = style.unit;
        drag = drag.custom_parser(move |text| {
            let text = text.trim();
            let text = text.strip_suffix(unit).unwrap_or(text).trim();
            let text = text.replace('−', "-");
            let text = text.trim_start_matches('+');
            text.parse::<f64>().ok()
        });
        if child.add(drag).changed() {
            *value = value.clamp(min, max);
            response.mark_changed();
        }
    }
    show_focus_ring(ui, &response, rect, 6);
    let value_text = readout(*value);
    let name = if style.name.is_empty() {
        value_text.clone()
    } else {
        style.name
    };
    slider_access(
        ui,
        &response,
        &name,
        f64::from(*value),
        f64::from(min),
        f64::from(max),
        f64::from(step.unwrap_or(tick).max(f32::EPSILON)),
        &value_text,
    );
    response.on_hover_and_drag_cursor(egui::CursorIcon::ResizeHorizontal)
}

/// `focused_only` is the fader's rule: a screen reader request does nothing
/// while the control is unfocused.
pub(crate) fn key_steps(
    ui: &egui::Ui,
    response: &egui::Response,
    focused_only: bool,
) -> (i32, bool, Option<f64>) {
    let focused = response.has_focus();
    if focused_only && !focused {
        return (0, false, None);
    }
    if focused {
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
    let (steps, fine) = if focused {
        ui.input(|input| {
            let down =
                input.num_presses(egui::Key::ArrowLeft) + input.num_presses(egui::Key::ArrowDown);
            let up =
                input.num_presses(egui::Key::ArrowRight) + input.num_presses(egui::Key::ArrowUp);
            (up as i32 - down as i32, input.modifiers.shift)
        })
    } else {
        (0, false)
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
    (steps + access_steps, fine, set_value)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn slider_access(
    ui: &egui::Ui,
    response: &egui::Response,
    name: &str,
    value: f64,
    min: f64,
    max: f64,
    step: f64,
    text: &str,
) {
    let enabled = ui.is_enabled();
    response.widget_info({
        let name = name.to_owned();
        let text = text.to_owned();
        move || {
            let mut info = egui::WidgetInfo::slider(enabled, value, name.clone());
            info.current_text_value = Some(text.clone());
            info
        }
    });
    ui.ctx().accesskit_node_builder(response.id, |builder| {
        use egui::accesskit::Action;
        builder.set_min_numeric_value(min);
        builder.set_max_numeric_value(max);
        builder.set_numeric_value_step(step);
        builder.add_action(Action::SetValue);
        if value < max {
            builder.add_action(Action::Increment);
        }
        if value > min {
            builder.add_action(Action::Decrement);
        }
    });
}

fn slider_fraction(value: f32, min: f32, max: f32, logarithmic: bool) -> f32 {
    let fraction = if logarithmic && min > 0.0 && max > min {
        (value / min).ln() / (max / min).ln()
    } else if max > min {
        (value - min) / (max - min)
    } else {
        0.0
    };
    fraction.clamp(0.0, 1.0)
}

fn slider_value_at(fraction: f32, min: f32, max: f32, logarithmic: bool) -> f32 {
    if logarithmic && min > 0.0 && max > min {
        min * (max / min).powf(fraction)
    } else {
        min + fraction * (max - min)
    }
}

fn apply_slider_steps(
    value: &mut f32,
    min: f32,
    max: f32,
    step: Option<f32>,
    tick: f32,
    steps: i32,
) -> bool {
    if steps == 0 {
        return false;
    }
    let next = match step {
        Some(step) => nudge_slider(*value, min, max, step, steps),
        None => (*value + steps as f32 * tick).clamp(min, max),
    };
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
