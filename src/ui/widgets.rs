//! Small controls shared by several screens: cards, labels, buttons, tabs,
//! meters, sliders and the device picker.

use eframe::egui;
use egui::{
    Align, Align2, Color32, CornerRadius, FontFamily, FontId, Layout, Margin, Rect, RichText,
    Sense, Stroke, Vec2,
};

use crate::audio::DeviceItem;
use crate::ui::icons::{paint_icon, Icon};
use crate::ui::theme::ts;
use crate::ui::theme::{
    with_alpha, AMBER, BLUE, BORDER, BORDER_SOFT, CARD_DEEP, CARD_RAISED, DIM, GREEN, MUTED, TEAL,
    TEXT,
};

pub(crate) fn card<R>(
    ui: &mut egui::Ui,
    fill: Color32,
    border: Color32,
    inner: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    egui::Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0, border))
        .corner_radius(CornerRadius::same(13))
        .inner_margin(Margin::same(15))
        .show(ui, inner)
        .inner
}

/// A [`card`] with an exact outer height, so cards sharing a row always line
/// up.  The inside keeps the same 15 px margin as `card`; content that does
/// not fit is clipped instead of stretching the frame.
pub(crate) fn card_fixed<R>(
    ui: &mut egui::Ui,
    fill: Color32,
    border: Color32,
    height: f32,
    inner: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(13), fill);
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(13),
        Stroke::new(1.0, border),
        egui::StrokeKind::Inside,
    );
    let content = rect.shrink(15.0);
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(content)
            .layout(Layout::top_down(Align::LEFT)),
    );
    child.set_clip_rect(content);
    inner(&mut child)
}

pub(crate) fn micro_label(text: &str, color: Color32) -> RichText {
    RichText::new(text)
        .size(ts(9.5))
        .color(color)
        .family(FontFamily::Proportional)
        .strong()
}

pub(crate) fn mono(text: impl Into<String>, color: Color32, size: f32) -> RichText {
    RichText::new(text)
        .family(FontFamily::Monospace)
        .size(ts(size))
        .color(color)
}

pub(crate) fn pill(ui: &mut egui::Ui, text: &str, color: Color32) {
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        FontId::new(ts(9.5), FontFamily::Proportional),
        color,
    );
    let size = Vec2::new(galley.size().x + 14.0, 19.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(9), with_alpha(color, 24));
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(9),
        Stroke::new(1.0, with_alpha(color, 55)),
        egui::StrokeKind::Inside,
    );
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, color);
}

pub(crate) fn status_dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
    ui.painter()
        .circle_filled(rect.center(), 4.0, with_alpha(color, 60));
    ui.painter().circle_filled(rect.center(), 2.2, color);
}

pub(crate) fn status_pill(ui: &mut egui::Ui, text: &str, color: Color32) {
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        FontId::new(ts(10.0), FontFamily::Proportional),
        color,
    );
    let (rect, _) = ui.allocate_exact_size(Vec2::new(galley.size().x + 30.0, 25.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(12), with_alpha(color, 22));
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(12),
        Stroke::new(1.0, with_alpha(color, 55)),
        egui::StrokeKind::Inside,
    );
    ui.painter()
        .circle_filled(egui::pos2(rect.left() + 12.0, rect.center().y), 3.2, color);
    ui.painter().galley(
        egui::pos2(rect.left() + 20.0, rect.center().y - galley.size().y / 2.0),
        galley,
        color,
    );
}

pub(crate) fn horizontal_line(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(rect, 0, color);
}

pub(crate) fn ellipsize(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        let mut output: String = text.chars().take(max.saturating_sub(1)).collect();
        output.push('…');
        output
    }
}

pub(crate) fn primary_button(ui: &mut egui::Ui, label: &str, fill: Color32, width: f32) -> bool {
    ui.add_sized(
        [width, 34.0],
        egui::Button::new(
            RichText::new(label)
                .size(ts(12.5))
                .color(Color32::from_rgb(8, 16, 14))
                .strong(),
        )
        .fill(fill)
        .stroke(Stroke::new(1.0, with_alpha(Color32::WHITE, 28)))
        .corner_radius(CornerRadius::same(9)),
    )
    .clicked()
}

pub(crate) fn ghost_button(ui: &mut egui::Ui, label: &str, width: f32) -> bool {
    ui.add_sized(
        [width, 32.0],
        egui::Button::new(RichText::new(label).size(ts(12.0)).color(MUTED))
            .fill(CARD_RAISED)
            .stroke(Stroke::new(1.0, BORDER_SOFT))
            .corner_radius(CornerRadius::same(8)),
    )
    .clicked()
}

/// A small, borderless button whose arrow is painted as two lines.  Drawing
/// the glyph ourselves keeps it visible even when the font has no arrow
/// characters, which otherwise leaves an empty rectangle behind.
pub(crate) fn arrow_button(ui: &mut egui::Ui, up: bool, color: Color32) -> egui::Response {
    let response = ui.add(
        egui::Button::new("")
            .fill(Color32::TRANSPARENT)
            .stroke(Stroke::NONE)
            .min_size(Vec2::new(16.0, 16.0)),
    );
    let tint = if response.hovered() { TEXT } else { color };
    let center = response.rect.center();
    let tip_y = center.y + if up { -3.0 } else { 3.0 };
    let base_y = center.y + if up { 2.5 } else { -2.5 };
    let tip = egui::pos2(center.x, tip_y);
    ui.painter().line_segment(
        [egui::pos2(center.x - 3.5, base_y), tip],
        Stroke::new(1.3, tint),
    );
    ui.painter().line_segment(
        [tip, egui::pos2(center.x + 3.5, base_y)],
        Stroke::new(1.3, tint),
    );
    response
}

pub(crate) fn nav_item(
    ui: &mut egui::Ui,
    icon: Icon,
    label: &str,
    hint: &str,
    selected: bool,
) -> bool {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 42.0), Sense::click());
    let hover = response.hovered();
    let fill = if selected {
        with_alpha(TEAL, 16)
    } else if hover {
        with_alpha(TEXT, 7)
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, CornerRadius::same(9), fill);
    if selected {
        ui.painter().rect_filled(
            Rect::from_min_size(rect.min + Vec2::new(0.0, 11.0), Vec2::new(3.0, 20.0)),
            CornerRadius::same(2),
            TEAL,
        );
    }
    let icon_rect =
        Rect::from_center_size(rect.left_center() + Vec2::new(23.0, 0.0), Vec2::splat(18.0));
    paint_icon(
        ui.painter(),
        icon_rect,
        icon,
        if selected { TEAL } else { MUTED },
    );
    ui.painter().text(
        rect.left_center() + Vec2::new(39.0, -5.0),
        Align2::LEFT_CENTER,
        label,
        FontId::new(ts(12.2), FontFamily::Proportional),
        if selected { TEXT } else { MUTED },
    );
    ui.painter().text(
        rect.left_center() + Vec2::new(39.0, 8.0),
        Align2::LEFT_CENTER,
        hint,
        FontId::new(ts(8.0), FontFamily::Proportional),
        if selected { with_alpha(TEAL, 190) } else { DIM },
    );
    show_focus_ring(ui, &response, rect, 9);
    row_info(
        &response,
        egui::Role::Button,
        ui.is_enabled(),
        label,
        Some(selected),
        Some(hint.to_owned()),
    );
    response.clicked()
}

pub(crate) fn source_tab(ui: &mut egui::Ui, label: &str, selected: bool, color: Color32) -> bool {
    tab_button(ui, label, selected, color).clicked()
}

/// A [`source_tab`] that explains itself on hover.
pub(crate) fn hinted_tab(
    ui: &mut egui::Ui,
    label: &str,
    selected: bool,
    color: Color32,
    hint: &str,
) -> bool {
    tab_button(ui, label, selected, color)
        .on_hover_text(hint)
        .clicked()
}

pub(crate) fn tab_button(
    ui: &mut egui::Ui,
    label: &str,
    selected: bool,
    color: Color32,
) -> egui::Response {
    tab_button_sized(ui, label, selected, color, 8.2)
}

/// A [`tab_button`] at a larger font size, for choices such as the channel
/// mode.  The unselected state keeps a quiet outline, so an "off" choice still
/// looks like a button instead of plain text.
pub(crate) fn tab_button_sized(
    ui: &mut egui::Ui,
    label: &str,
    selected: bool,
    color: Color32,
    size: f32,
) -> egui::Response {
    ui.add(
        egui::Button::new(
            RichText::new(label)
                .size(ts(size))
                .color(if selected { color } else { DIM })
                .strong(),
        )
        .fill(if selected {
            with_alpha(color, 20)
        } else {
            Color32::TRANSPARENT
        })
        .stroke(Stroke::new(
            1.0,
            if selected {
                with_alpha(color, 60)
            } else {
                BORDER_SOFT
            },
        ))
        .corner_radius(CornerRadius::same(5))
        .min_size(Vec2::new(0.0, 18.0)),
    )
}

pub(crate) fn channel_chip(ui: &mut egui::Ui, label: &str, checked: &mut bool, color: Color32) {
    let fill = if *checked {
        with_alpha(color, 22)
    } else {
        CARD_DEEP
    };
    let text_color = if *checked { color } else { DIM };
    let response = ui.add(
        egui::Button::new(
            RichText::new(label)
                .size(ts(9.0))
                .color(text_color)
                .strong(),
        )
        .fill(fill)
        .stroke(Stroke::new(
            1.0,
            if *checked {
                with_alpha(color, 65)
            } else {
                BORDER_SOFT
            },
        ))
        .corner_radius(CornerRadius::same(6)),
    );
    if response.clicked() {
        *checked = !*checked;
    }
}

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

fn nudge_slider(value: f32, min: f32, max: f32, step: f32, steps: i32) -> f32 {
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

pub(crate) fn meter_row(ui: &mut egui::Ui, label: &str, value: f32, peak: f32, color: Color32) {
    ui.horizontal(|ui| {
        ui.add_sized(
            [14.0, 12.0],
            egui::Label::new(mono(label, DIM, 9.0)).selectable(false),
        );
        let desired = Vec2::new(ui.available_width(), 6.0);
        let (rect, _) = ui.allocate_exact_size(desired, Sense::hover());
        ui.painter()
            .rect_filled(rect, CornerRadius::same(3), Color32::from_rgb(25, 32, 44));
        let width = (rect.width() * value.clamp(0.0, 1.0)).max(0.0);
        if width > 1.0 {
            let filled = Rect::from_min_size(rect.min, Vec2::new(width, rect.height()));
            ui.painter()
                .rect_filled(filled, CornerRadius::same(3), with_alpha(color, 215));
            let glow = Rect::from_min_size(
                egui::pos2(rect.min.x, rect.min.y - 1.0),
                Vec2::new(width, rect.height() + 2.0),
            );
            ui.painter()
                .rect_filled(glow, CornerRadius::same(3), with_alpha(color, 22));
        }
        let peak_x = (rect.min.x + rect.width() * peak.clamp(0.0, 1.0))
            .min(rect.right() - 1.0)
            .max(rect.left());
        ui.painter().line_segment(
            [
                egui::pos2(peak_x, rect.top() - 1.0),
                egui::pos2(peak_x, rect.bottom() + 1.0),
            ],
            Stroke::new(1.0, with_alpha(TEXT, 195)),
        );
    });
}

pub(crate) fn metric_inline(ui: &mut egui::Ui, label: &str, value: &str, color: Color32) {
    ui.horizontal(|ui| {
        ui.label(micro_label(label, DIM));
        ui.label(mono(value, color, 9.0));
    });
}

pub(crate) fn progress_line(ui: &mut egui::Ui, label: &str, value: f32, color: Color32) {
    ui.horizontal(|ui| {
        ui.label(micro_label(label, DIM));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(mono(
                format!("{}%", (value * 100.0).round() as i32),
                color,
                8.5,
            ));
        });
    });
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 4.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(2), Color32::from_rgb(27, 34, 46));
    let filled = Rect::from_min_size(
        rect.min,
        Vec2::new(rect.width() * value.clamp(0.0, 1.0), rect.height()),
    );
    ui.painter()
        .rect_filled(filled, CornerRadius::same(2), with_alpha(color, 210));
}

pub(crate) fn metric_tile(
    ui: &mut egui::Ui,
    label: &str,
    value: &str,
    unit: &str,
    color: Color32,
    width: f32,
) -> egui::Response {
    metric_tile_with_badge(ui, label, value, unit, color, width, None)
}

/// Like [`metric_tile`], with an optional top-right micro badge (e.g. underruns).
pub(crate) fn metric_tile_with_badge(
    ui: &mut egui::Ui,
    label: &str,
    value: &str,
    unit: &str,
    color: Color32,
    width: f32,
    badge: Option<(&str, Color32)>,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 58.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(8), CARD_DEEP);
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(8),
        Stroke::new(1.0, BORDER_SOFT),
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        rect.left_top() + Vec2::new(9.0, 8.0),
        Align2::LEFT_TOP,
        label,
        FontId::new(ts(8.0), FontFamily::Proportional),
        DIM,
    );
    if let Some((badge_text, badge_color)) = badge {
        ui.painter().text(
            rect.right_top() + Vec2::new(-9.0, 8.0),
            Align2::RIGHT_TOP,
            badge_text,
            FontId::new(ts(8.0), FontFamily::Monospace),
            badge_color,
        );
    }
    ui.painter().text(
        rect.left_bottom() + Vec2::new(9.0, -8.0),
        Align2::LEFT_BOTTOM,
        value,
        FontId::new(ts(18.0), FontFamily::Monospace),
        color,
    );
    if !unit.is_empty() {
        let value_width = ui
            .painter()
            .layout_no_wrap(
                value.to_owned(),
                FontId::new(ts(18.0), FontFamily::Monospace),
                color,
            )
            .size()
            .x;
        ui.painter().text(
            rect.left_bottom() + Vec2::new(11.0 + value_width, -10.0),
            Align2::LEFT_BOTTOM,
            unit,
            FontId::new(ts(8.0), FontFamily::Proportional),
            DIM,
        );
    }
    response
}

pub(crate) fn small_fact(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.vertical(|ui| {
        ui.label(micro_label(label, DIM));
        ui.label(mono(value, MUTED, 9.5));
    });
}

pub(crate) fn route_summary(ui: &mut egui::Ui, label: &str, value: &str, color: Color32) {
    ui.horizontal(|ui| {
        ui.label(micro_label(label, DIM));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                RichText::new(ellipsize(value, 28))
                    .size(ts(9.5))
                    .color(color),
            );
        });
    });
}

pub(crate) fn connector(ui: &mut egui::Ui, height: f32, active: bool, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(20.0, height), Sense::hover());
    let y = rect.center().y;
    let line = if active { color } else { BORDER };
    ui.painter().line_segment(
        [
            egui::pos2(rect.left() + 2.0, y),
            egui::pos2(rect.right() - 3.0, y),
        ],
        Stroke::new(1.0, with_alpha(line, 120)),
    );
    ui.painter().circle_filled(
        egui::pos2(rect.right() - 2.0, y),
        2.0,
        with_alpha(line, 200),
    );
    if active {
        let t = ui.input(|input| input.time) as f32;
        let pulse = (t * 1.4).fract();
        let x = rect.left() + 2.0 + (rect.width() - 7.0) * pulse;
        ui.painter()
            .circle_filled(egui::pos2(x, y), 2.4, with_alpha(color, 210));
        ui.painter()
            .circle_filled(egui::pos2(x, y), 5.0, with_alpha(color, 28));
    }
}

pub(crate) fn legend_dot(ui: &mut egui::Ui, label: &str, color: Color32) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(7.0), Sense::hover());
        ui.painter().circle_filled(rect.center(), 2.8, color);
        ui.label(mono(label, MUTED, 8.5));
    });
}

/// Draws the device combo. `true` when the person picks a row, including the
/// row that was already selected. Opening the combo does not count.
pub(crate) fn device_selector(
    ui: &mut egui::Ui,
    id_source: &str,
    devices: &[DeviceItem],
    selected: &mut usize,
    accent: Color32,
) -> bool {
    let current = devices
        .get(*selected)
        .map(|device| crate::audio::device_choice_label(device, devices))
        .unwrap_or_else(|| "No device".to_owned());
    let mut picked = false;
    egui::Frame::new()
        .fill(CARD_DEEP)
        .stroke(Stroke::new(1.0, BORDER_SOFT))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(9, 3))
        .show(ui, |ui| {
            egui::ComboBox::from_id_salt(id_source)
                .width(ui.available_width())
                .selected_text(
                    RichText::new(ellipsize(&current, 30))
                        .size(ts(12.0))
                        .color(TEXT),
                )
                .show_ui(ui, |ui| {
                    ui.set_min_width(250.0);
                    for (index, device) in devices.iter().enumerate() {
                        let label = format!(
                            "{}  ·  {}",
                            crate::audio::device_choice_label(device, devices),
                            if device.demo {
                                "preview".to_owned()
                            } else {
                                device.backend.clone()
                            }
                        );
                        let response = ui.selectable_value(selected, index, label);
                        if response.clicked() || response.changed() {
                            picked = true;
                        }
                    }
                });
        });
    if picked {
        ui.ctx().request_repaint();
    }
    if let Some(device) = devices.get(*selected) {
        ui.horizontal(|ui| {
            ui.label(micro_label(&device.backend.to_uppercase(), accent));
            if device.demo {
                ui.label(micro_label("PREVIEW", AMBER));
            } else {
                ui.label(micro_label(&format!("{} CH", device.channels), DIM));
            }
        });
    }
    picked
}

const TAG_FONT: f32 = 7.5;
const TAG_PAD: Vec2 = Vec2::new(5.0, 2.0);

/// Paints a small outlined tag ("CLOCK") with its left edge centred on
/// `left_center` and returns its width.
pub(crate) fn paint_tag(ui: &egui::Ui, left_center: egui::Pos2, text: &str, color: Color32) -> f32 {
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        FontId::new(ts(TAG_FONT), FontFamily::Monospace),
        color,
    );
    let size = galley.size() + TAG_PAD * 2.0;
    let rect = egui::Rect::from_min_size(
        egui::pos2(left_center.x, left_center.y - size.y * 0.5),
        size,
    );
    ui.painter()
        .rect_filled(rect, CornerRadius::same(4), with_alpha(color, 18));
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(4),
        Stroke::new(1.0, with_alpha(color, 90)),
        egui::StrokeKind::Inside,
    );
    ui.painter().galley(rect.min + TAG_PAD, galley, color);
    size.x
}

pub(crate) fn tag_width(ui: &egui::Ui, text: &str) -> f32 {
    ui.painter()
        .layout_no_wrap(
            text.to_owned(),
            FontId::new(ts(TAG_FONT), FontFamily::Monospace),
            DIM,
        )
        .size()
        .x
        + TAG_PAD.x * 2.0
}

/// The CLOCK tag as a laid-out widget, for the inspector title.
pub(crate) fn clock_tag(ui: &mut egui::Ui) {
    let width = tag_width(ui, "CLOCK");
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 18.0), Sense::hover());
    paint_tag(ui, rect.left_center(), "CLOCK", BLUE);
    let _ = response.on_hover_text("The chain runs on this device's callback.");
}

/// The VIRTUAL tag for RigWerk Out.
pub(crate) fn virtual_tag(ui: &mut egui::Ui) {
    let width = tag_width(ui, "VIRTUAL");
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 18.0), Sense::hover());
    paint_tag(ui, rect.left_center(), "VIRTUAL", GREEN);
    let _ = response.on_hover_text("A virtual source other apps can capture.");
}

/// `text` shortened with "…" until it fits `max_width` in `font`.
pub(crate) fn fit_text(ui: &egui::Ui, text: &str, font: &FontId, max_width: f32) -> String {
    let width = |candidate: &str| {
        ui.painter()
            .layout_no_wrap(candidate.to_owned(), font.clone(), TEXT)
            .size()
            .x
    };
    if width(text) <= max_width {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    for keep in (0..chars.len()).rev() {
        let mut candidate: String = chars[..keep].iter().collect();
        candidate.push('…');
        if width(&candidate) <= max_width {
            return candidate;
        }
    }
    "…".to_owned()
}

#[cfg(test)]
pub(crate) fn run_frame(
    ctx: &egui::Context,
    input: egui::RawInput,
    add: impl FnMut(&mut egui::Ui),
) -> egui::FullOutput {
    let mut output = ctx.run_ui(input, add);
    output.textures_delta.clear();
    output
}

#[cfg(test)]
pub(crate) fn accesskit_frame(add: impl FnMut(&mut egui::Ui)) -> egui::accesskit::TreeUpdate {
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let output = run_frame(&ctx, test_input(None), add);
    output
        .platform_output
        .accesskit_update
        .expect("accesskit tree")
}

#[cfg(test)]
pub(crate) fn node_named<'a>(
    tree: &'a egui::accesskit::TreeUpdate,
    name: &str,
) -> &'a egui::accesskit::Node {
    tree.nodes
        .iter()
        .find(|(_, node)| node.label() == Some(name))
        .map(|(_, node)| node)
        .unwrap_or_else(|| panic!("no AccessKit node named {name}"))
}

#[cfg(test)]
pub(crate) fn test_input(key: Option<egui::Key>) -> egui::RawInput {
    let mut input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(800.0, 600.0),
        )),
        ..Default::default()
    };
    if let Some(key) = key {
        input.events.push(egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        });
    }
    input
}

#[cfg(test)]
mod tests {
    use super::{
        db_slider, nav_item, node_named, nudge_slider, run_frame, styled_slider, test_input,
    };
    use crate::ui::icons::Icon;
    use crate::ui::theme::TEAL;
    use eframe::egui::{self, accesskit::Toggled};

    fn near(actual: f32, expected: f32) {
        assert!((actual - expected).abs() < 1.0e-4, "{actual} != {expected}");
    }

    #[test]
    fn arrow_steps_follow_the_slider_step_and_stay_inside_the_range() {
        near(nudge_slider(0.0, -12.0, 12.0, 0.5, 0), 0.0);
        near(nudge_slider(0.0, -12.0, 12.0, 0.5, 1), 0.5);
        near(nudge_slider(0.0, -12.0, 12.0, 0.5, -1), -0.5);
        near(nudge_slider(0.2, 0.0, 1.0, 0.1, 1), 0.3);
        near(nudge_slider(12.0, -12.0, 12.0, 0.5, 4), 12.0);
        near(nudge_slider(-12.0, -12.0, 12.0, 0.5, -3), -12.0);
    }

    #[test]
    fn nav_row_exposes_its_name_and_selected_value() {
        let tree = super::accesskit_frame(|ui| {
            assert!(!nav_item(ui, Icon::Wave, "Live", "Bridge", true));
        });
        let node = node_named(&tree, "Live");
        assert_eq!(node.role(), egui::Role::Button);
        assert_eq!(node.value(), Some("Bridge"));
        assert_eq!(node.toggled(), Some(Toggled::True));
    }

    #[test]
    fn gain_slider_exposes_a_name_and_decibel_value() {
        let mut value = -3.0f32;
        let tree = super::accesskit_frame(|ui| {
            assert!(!db_slider(ui, &mut value, -12.0, 12.0, TEAL));
        });
        let node = node_named(&tree, "Gain");
        assert_eq!(node.role(), egui::Role::Slider);
        assert_eq!(node.numeric_value(), Some(-3.0));
        assert_eq!(node.value(), Some("-3.0 dB"));
        assert_eq!(node.min_numeric_value(), Some(-12.0));
        assert_eq!(node.max_numeric_value(), Some(12.0));
    }

    #[test]
    fn a_focused_slider_changes_with_arrow_keys_and_stays_focused() {
        let ctx = egui::Context::default();
        let mut value = 0.0f32;
        let show = |ui: &mut egui::Ui, value: &mut f32| {
            styled_slider(ui, value, -12.0, 12.0, 0.5, (TEAL, "EQ gain"))
        };
        let _ = run_frame(&ctx, test_input(None), |ui| {
            show(ui, &mut value).request_focus();
        });
        // One idle frame installs the arrow-key filter egui applies on the next pass.
        let _ = run_frame(&ctx, test_input(None), |ui| {
            show(ui, &mut value);
        });
        let mut focused = false;
        let _ = run_frame(&ctx, test_input(Some(egui::Key::ArrowRight)), |ui| {
            let response = show(ui, &mut value);
            focused = response.has_focus();
            assert!(response.changed());
        });
        near(value, 0.5);
        assert!(focused);

        let _ = run_frame(&ctx, test_input(Some(egui::Key::ArrowLeft)), |ui| {
            show(ui, &mut value);
        });
        near(value, 0.0);
        let _ = run_frame(&ctx, test_input(Some(egui::Key::ArrowUp)), |ui| {
            show(ui, &mut value);
        });
        near(value, 0.5);
        let _ = run_frame(&ctx, test_input(Some(egui::Key::ArrowDown)), |ui| {
            show(ui, &mut value);
        });
        near(value, 0.0);
    }

    #[test]
    fn arrow_keys_do_nothing_until_the_slider_is_focused() {
        let ctx = egui::Context::default();
        let mut value = 1.0f32;
        let _ = run_frame(&ctx, test_input(Some(egui::Key::ArrowRight)), |ui| {
            let response = styled_slider(ui, &mut value, 0.0, 1.0, 0.01, (TEAL, "Mix"));
            assert!(!response.has_focus());
        });
        near(value, 1.0);
    }

    #[test]
    fn pointer_click_still_sets_the_slider() {
        let ctx = egui::Context::default();
        let mut value = 0.0f32;
        let mut rect = egui::Rect::NOTHING;
        let _ = run_frame(&ctx, test_input(None), |ui| {
            rect = styled_slider(ui, &mut value, 0.0, 1.0, 0.01, (TEAL, "Mix")).rect;
        });
        let pos = egui::pos2(rect.right() - 2.0, rect.center().y);
        let mut press = test_input(None);
        press.events.push(egui::Event::PointerMoved(pos));
        press.events.push(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::default(),
        });
        let _ = run_frame(&ctx, press, |ui| {
            styled_slider(ui, &mut value, 0.0, 1.0, 0.01, (TEAL, "Mix"));
        });
        let mut release = test_input(None);
        release.events.push(egui::Event::PointerMoved(pos));
        release.events.push(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::default(),
        });
        let _ = run_frame(&ctx, release, |ui| {
            styled_slider(ui, &mut value, 0.0, 1.0, 0.01, (TEAL, "Mix"));
        });
        assert!(
            value > 0.9,
            "click at the right of the track left the mix at {value}"
        );
    }
}
