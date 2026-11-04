//! Small controls shared by several screens: cards, labels, buttons, tabs,
//! meters, sliders and the device picker.

use eframe::egui;
use egui::{
    Align, Align2, Color32, CornerRadius, FontFamily, FontId, Layout, Margin, Rect, RichText,
    Sense, Stroke, Vec2,
};

use crate::audio::DeviceItem;
use crate::ui::icons::{paint_icon, Icon};
use crate::ui::theme::{
    with_alpha, AMBER, BORDER, BORDER_SOFT, CARD_DEEP, CARD_RAISED, DIM, MUTED, TEAL, TEXT,
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
        .size(9.5)
        .color(color)
        .family(FontFamily::Proportional)
        .strong()
}

pub(crate) fn mono(text: impl Into<String>, color: Color32, size: f32) -> RichText {
    RichText::new(text)
        .family(FontFamily::Monospace)
        .size(size)
        .color(color)
}

pub(crate) fn pill(ui: &mut egui::Ui, text: &str, color: Color32) {
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        FontId::new(9.5, FontFamily::Proportional),
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
        FontId::new(10.0, FontFamily::Proportional),
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
                .size(12.5)
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
        egui::Button::new(RichText::new(label).size(12.0).color(MUTED))
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
        FontId::new(12.2, FontFamily::Proportional),
        if selected { TEXT } else { MUTED },
    );
    ui.painter().text(
        rect.left_center() + Vec2::new(39.0, 8.0),
        Align2::LEFT_CENTER,
        hint,
        FontId::new(8.0, FontFamily::Proportional),
        if selected { with_alpha(TEAL, 190) } else { DIM },
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
    ui.add(
        egui::Button::new(
            RichText::new(label)
                .size(8.2)
                .color(if selected { color } else { DIM })
                .strong(),
        )
        .fill(if selected {
            with_alpha(color, 20)
        } else {
            Color32::TRANSPARENT
        })
        .stroke(if selected {
            Stroke::new(1.0, with_alpha(color, 60))
        } else {
            Stroke::NONE
        })
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
        egui::Button::new(RichText::new(label).size(9.0).color(text_color).strong())
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

pub(crate) fn db_slider(
    ui: &mut egui::Ui,
    value: &mut f32,
    min: f32,
    max: f32,
    color: Color32,
) -> bool {
    styled_slider(ui, value, min, max, 0.1, color).changed()
}

/// The gain slider's look, with an explicit step, for other values in the app.
pub(crate) fn styled_slider(
    ui: &mut egui::Ui,
    value: &mut f32,
    min: f32,
    max: f32,
    step: f32,
    color: Color32,
) -> egui::Response {
    let (rect, mut response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), 22.0),
        Sense::click_and_drag(),
    );
    let track = Rect::from_center_size(rect.center(), Vec2::new(rect.width(), 4.0));
    ui.painter()
        .rect_filled(track, CornerRadius::same(2), Color32::from_rgb(26, 33, 45));
    if response.dragged() || response.clicked() {
        if let Some(position) = response.interact_pointer_pos() {
            let fraction = ((position.x - track.left()) / track.width()).clamp(0.0, 1.0);
            let new_value = min + fraction * (max - min);
            let step = step.max(f32::EPSILON);
            *value = ((new_value / step).round() * step).clamp(min, max);
            response.mark_changed();
        }
    }
    let fraction = ((*value - min) / (max - min)).clamp(0.0, 1.0);
    let x = track.left() + track.width() * fraction;
    if fraction > 0.0 {
        ui.painter().rect_filled(
            Rect::from_min_max(track.min, egui::pos2(x, track.bottom())),
            CornerRadius::same(2),
            with_alpha(color, 190),
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
        .circle_filled(egui::pos2(x, track.center().y), 3.0, color);
    response
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
) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 58.0), Sense::hover());
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
        FontId::new(8.0, FontFamily::Proportional),
        DIM,
    );
    ui.painter().text(
        rect.left_bottom() + Vec2::new(9.0, -8.0),
        Align2::LEFT_BOTTOM,
        value,
        FontId::new(18.0, FontFamily::Monospace),
        color,
    );
    if !unit.is_empty() {
        let value_width = ui
            .painter()
            .layout_no_wrap(
                value.to_owned(),
                FontId::new(18.0, FontFamily::Monospace),
                color,
            )
            .size()
            .x;
        ui.painter().text(
            rect.left_bottom() + Vec2::new(11.0 + value_width, -10.0),
            Align2::LEFT_BOTTOM,
            unit,
            FontId::new(8.0, FontFamily::Proportional),
            DIM,
        );
    }
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
            ui.label(RichText::new(ellipsize(value, 28)).size(9.5).color(color));
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

pub(crate) fn device_selector(
    ui: &mut egui::Ui,
    id_source: &str,
    devices: &[DeviceItem],
    selected: &mut usize,
    accent: Color32,
) {
    let current = devices
        .get(*selected)
        .map(|device| device.name.clone())
        .unwrap_or_else(|| "No device".to_owned());
    let mut changed = false;
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
                        .size(12.0)
                        .color(TEXT),
                )
                .show_ui(ui, |ui| {
                    ui.set_min_width(250.0);
                    for (index, device) in devices.iter().enumerate() {
                        let label = format!(
                            "{}  ·  {}",
                            device.name,
                            if device.demo {
                                "preview".to_owned()
                            } else {
                                device.backend.clone()
                            }
                        );
                        if ui.selectable_value(selected, index, label).changed() {
                            changed = true;
                        }
                    }
                });
        });
    if changed {
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
}
