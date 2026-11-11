//! Small controls shared by several screens: cards, labels, buttons, tabs,
//! meters, sliders and the device picker.

mod path;
mod slider;

pub(crate) use path::{colour_swatch, nav_path_item, path_chip, path_level_text, path_toggle};
pub(crate) use slider::{db_slider, row_info, show_focus_ring, styled_slider};

#[cfg(test)]
pub(crate) use slider::nudge_slider;

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

/// A small BYP pill.  `on` is the hot amber state; off stays a quiet
/// outline.  The caller adds the tooltip.
pub(crate) fn bypass_pill(ui: &mut egui::Ui, on: bool) -> egui::Response {
    let color = if on { AMBER } else { DIM };
    let galley = ui.painter().layout_no_wrap(
        "BYP".to_owned(),
        FontId::new(ts(7.0), FontFamily::Monospace),
        color,
    );
    let size = Vec2::new(galley.size().x + 10.0, 15.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    if on {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(7), with_alpha(AMBER, 24));
    }
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(7),
        Stroke::new(
            1.0,
            if on {
                with_alpha(AMBER, 55)
            } else {
                BORDER_SOFT
            },
        ),
        egui::StrokeKind::Inside,
    );
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, color);
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
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
    nav_row(ui, NavLead::Icon(icon), label, hint, selected).clicked()
}

/// Sub-entries sit this far right of the main entries.
const NAV_SUB_INDENT: f32 = 26.0;

pub(super) enum NavLead {
    Icon(Icon),
    Dot(Color32),
}

pub(super) fn nav_row(
    ui: &mut egui::Ui,
    lead: NavLead,
    label: &str,
    hint: &str,
    selected: bool,
) -> egui::Response {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 42.0), Sense::click());
    let indent = match lead {
        NavLead::Icon(_) => 0.0,
        NavLead::Dot(_) => NAV_SUB_INDENT,
    };
    // The highlight spans the full row (as the mockup's `.nav-path`); only
    // the content moves right.
    let content = Rect::from_min_max(rect.min + Vec2::new(indent, 0.0), rect.max);
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
    let lead_center = content.left_center() + Vec2::new(23.0, 0.0);
    match lead {
        NavLead::Icon(icon) => paint_icon(
            ui.painter(),
            Rect::from_center_size(lead_center, Vec2::splat(18.0)),
            icon,
            if selected { TEAL } else { MUTED },
        ),
        NavLead::Dot(colour) => {
            ui.painter()
                .circle_filled(lead_center, 6.0, with_alpha(colour, 50));
            ui.painter().circle_filled(lead_center, 4.0, colour);
        }
    }
    // Long path names are cut to the row, not run past the sidebar.
    let text_w = (content.width() - 39.0 - 8.0).max(0.0);
    let label_font = FontId::new(ts(12.2), FontFamily::Proportional);
    let hint_font = FontId::new(ts(8.0), FontFamily::Proportional);
    ui.painter().text(
        content.left_center() + Vec2::new(39.0, -5.0),
        Align2::LEFT_CENTER,
        fit_text(ui, label, &label_font, text_w),
        label_font,
        if selected { TEXT } else { MUTED },
    );
    ui.painter().text(
        content.left_center() + Vec2::new(39.0, 8.0),
        Align2::LEFT_CENTER,
        fit_text(ui, hint, &hint_font, text_w),
        hint_font,
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
    response
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

/// A two-state switch: a 38 × 22 track with a 16 px knob.  Returns whether
/// the click changed the value.
pub(crate) fn switch(ui: &mut egui::Ui, value: &mut bool, color: Color32) -> bool {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(38.0, 22.0), Sense::click());
    let t = ui.ctx().animate_bool(response.id, *value);
    ui.painter().rect_filled(
        rect,
        CornerRadius::same(11),
        if *value {
            with_alpha(color, 60)
        } else {
            CARD_DEEP
        },
    );
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(11),
        Stroke::new(
            1.0,
            if *value {
                with_alpha(color, 130)
            } else {
                BORDER_SOFT
            },
        ),
        egui::StrokeKind::Inside,
    );
    let knob_x = rect.left() + 11.0 + (rect.width() - 22.0) * t;
    ui.painter().circle_filled(
        egui::pos2(knob_x, rect.center().y),
        8.0,
        if *value { color } else { MUTED },
    );
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if response.clicked() {
        *value = !*value;
        true
    } else {
        false
    }
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

/// A labelled bar: the name on the left, a formatted value in monospace on the
/// right, and a 4 px bar underneath filled to `fraction`.
pub(crate) fn progress_line(
    ui: &mut egui::Ui,
    label: &str,
    value: &str,
    fraction: f32,
    color: Color32,
) {
    ui.horizontal(|ui| {
        ui.label(micro_label(label, DIM));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(mono(value, color, 9.0));
        });
    });
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 4.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(2), Color32::from_rgb(27, 34, 46));
    let filled = Rect::from_min_size(
        rect.min,
        Vec2::new(rect.width() * fraction.clamp(0.0, 1.0), rect.height()),
    );
    ui.painter()
        .rect_filled(filled, CornerRadius::same(2), with_alpha(color, 220));
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
mod tests;
