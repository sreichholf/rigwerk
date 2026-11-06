//! Pure row/widget helpers for mixer output-device lists and inspectors.

use eframe::egui;
use egui::{
    Align, Align2, Color32, CornerRadius, FontFamily, FontId, Layout, Margin, RichText, Sense,
    Stroke, Vec2,
};

use super::{device_level_text, device_row_value, device_state, OutputDeviceRow};
use crate::audio::{EngineState, OutputDeviceSettings, DEVICE_LEVEL_MAX_DB, DEVICE_LEVEL_MIN_DB};
use crate::ui::theme::ts;
use crate::ui::theme::{
    with_alpha, BLUE, BORDER_SOFT, CARD_DEEP, DIM, GREEN, MUTED, RED, TEAL, TEXT,
};
use crate::ui::widgets::{
    db_slider, fit_text, meter_row, micro_label, mono, paint_tag, row_info, show_focus_ring,
    tag_width,
};

/// A framed block in the output inspectors.
pub(crate) fn block(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(CARD_DEEP)
        .stroke(Stroke::new(1.0, BORDER_SOFT))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
}

pub(super) fn device_level_block(
    ui: &mut egui::Ui,
    settings: &mut OutputDeviceSettings,
    channels: usize,
    meters: &[f32],
    peaks: &[f32],
) {
    block(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(micro_label("DEVICE LEVEL", BLUE));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(mono(
                    device_level_text(settings),
                    if settings.mute { RED } else { TEXT },
                    12.0,
                ));
            });
        });
        ui.add_space(4.0);
        db_slider(
            ui,
            &mut settings.level_db,
            DEVICE_LEVEL_MIN_DB,
            DEVICE_LEVEL_MAX_DB,
            (BLUE, "Device level"),
        );
        ui.add_space(6.0);
        for channel in 0..channels {
            let meter = meters.get(channel).copied().unwrap_or(0.0);
            let peak = peaks.get(channel).copied().unwrap_or(0.0);
            meter_row(ui, &(channel + 1).to_string(), meter, peak, BLUE);
        }
        ui.add_space(4.0);
        ui.label(
            RichText::new("Trims this device only, after the master level.")
                .size(ts(10.5))
                .color(MUTED),
        );
    });
}

/// Width left for the inspector title's device name after the CLOCK tag and
/// the MUTE button (and the gaps before them), never below 40 px.
pub(super) fn title_name_width(available: f32, tag_w: f32, mute_w: f32, spacing: f32) -> f32 {
    (available - tag_w - mute_w - spacing * 2.0).max(40.0)
}

/// The device's mute toggle, labelled like the master's.
pub(super) fn mute_button(ui: &mut egui::Ui, mute: bool, device: &str) -> egui::Response {
    let response = ui
        .add(
            egui::Button::new(
                RichText::new(if mute { "MUTED" } else { "MUTE" })
                    .size(ts(9.0))
                    .color(if mute { RED } else { DIM }),
            )
            .fill(if mute {
                with_alpha(RED, 22)
            } else {
                Color32::TRANSPARENT
            })
            .stroke(Stroke::new(
                1.0,
                if mute {
                    with_alpha(RED, 55)
                } else {
                    BORDER_SOFT
                },
            ))
            .corner_radius(CornerRadius::same(6)),
        )
        .on_hover_text("Silence this device only. The master and the bridge keep running.");
    row_info(
        &response,
        egui::Role::Button,
        ui.is_enabled(),
        format!("Mute {device}"),
        Some(mute),
        None,
    );
    response
}

/// A device row under the Output stage, styled like an insert row: state
/// dot, name with its tag, backend and channels, level, mute and optional
/// remove. Returns (row clicked, mute clicked, remove clicked).
pub(super) fn chain_device_row(
    ui: &mut egui::Ui,
    row: &OutputDeviceRow,
    selected: bool,
    state: EngineState,
) -> (bool, bool, bool) {
    let title_font = FontId::new(ts(11.0), FontFamily::Proportional);
    let detail_font = FontId::new(ts(8.5), FontFamily::Monospace);
    let level_font = FontId::new(ts(9.0), FontFamily::Monospace);
    let pad = Vec2::new(10.0, 6.0);
    let text_h =
        ui.fonts_mut(|fonts| fonts.row_height(&title_font) + fonts.row_height(&detail_font)) + 4.0;
    let row_h = text_h + pad.y * 2.0;
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), row_h), Sense::click());
    let border = if selected {
        with_alpha(TEAL, 90)
    } else {
        with_alpha(BLUE, 40)
    };
    ui.painter().rect_filled(
        rect,
        CornerRadius::same(8),
        if selected {
            with_alpha(TEAL, 18)
        } else if response.hovered() {
            with_alpha(TEXT, 8)
        } else {
            Color32::TRANSPARENT
        },
    );
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(8),
        Stroke::new(1.0, border),
        egui::StrokeKind::Inside,
    );
    let inner = rect.shrink2(pad);

    // Right side: mute, optional remove, then the level to their left.
    let mute = row.settings.mute;
    let mute_rect = egui::Rect::from_center_size(
        egui::pos2(inner.right() - 11.0, inner.center().y),
        Vec2::new(22.0, 22.0),
    );
    // Painted at a fixed size: a Button would grow by its padding and run
    // into the level readout.
    let mute_response = ui
        .interact(mute_rect, response.id.with("mute"), Sense::click())
        .on_hover_text(if mute {
            "Unmute this device"
        } else {
            "Mute this device only"
        });
    let mute_color = if mute { RED } else { DIM };
    ui.painter().rect_filled(
        mute_rect,
        CornerRadius::same(5),
        if mute {
            with_alpha(RED, 22)
        } else if mute_response.hovered() {
            with_alpha(TEXT, 10)
        } else {
            Color32::TRANSPARENT
        },
    );
    ui.painter().rect_stroke(
        mute_rect,
        CornerRadius::same(5),
        Stroke::new(
            1.0,
            if mute {
                with_alpha(RED, 55)
            } else {
                BORDER_SOFT
            },
        ),
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        mute_rect.center(),
        Align2::CENTER_CENTER,
        "M",
        FontId::new(ts(8.5), FontFamily::Monospace),
        mute_color,
    );
    show_focus_ring(ui, &mute_response, mute_rect, 5);
    row_info(
        &mute_response,
        egui::Role::Button,
        ui.is_enabled(),
        format!("Mute {}", row.name),
        Some(mute),
        None,
    );
    let mut remove_clicked = false;
    let mut controls_left = mute_rect.left();
    if !row.clock {
        let remove_rect = egui::Rect::from_center_size(
            egui::pos2(mute_rect.left() - 14.0, inner.center().y),
            Vec2::new(16.0, 16.0),
        );
        let remove_response = ui
            .interact(remove_rect, response.id.with("remove"), Sense::click())
            .on_hover_text("Remove this output device");
        ui.painter().text(
            remove_rect.center(),
            Align2::CENTER_CENTER,
            "×",
            FontId::new(ts(12.0), FontFamily::Proportional),
            if remove_response.hovered() { RED } else { DIM },
        );
        remove_clicked = remove_response.clicked();
        controls_left = remove_rect.left();
    }
    let level = device_level_text(&row.settings);
    let level_galley =
        ui.painter()
            .layout_no_wrap(level, level_font, if mute { RED } else { MUTED });
    let level_right = controls_left - 8.0;
    let level_w = level_galley.size().x;
    ui.painter().galley(
        egui::pos2(
            level_right - level_w,
            inner.center().y - level_galley.size().y * 0.5,
        ),
        level_galley,
        MUTED,
    );

    // Left side: state dot, then the two text lines.
    let (_, state_color) = device_state(row, state);
    let dot = egui::pos2(inner.left() + 4.0, inner.center().y);
    ui.painter()
        .circle_filled(dot, 4.0, with_alpha(state_color, 60));
    ui.painter().circle_filled(dot, 2.4, state_color);
    let text_x = inner.left() + 16.0;
    let text_right = level_right - level_w - 10.0;
    let (tag, tag_color) = if row.clock {
        (Some("CLOCK"), BLUE)
    } else if row.virtual_out {
        (Some("VIRTUAL"), GREEN)
    } else {
        (None, BLUE)
    };
    let tag_w = tag.map_or(0.0, |text| tag_width(ui, text) + 6.0);
    let name = fit_text(
        ui,
        &row.name,
        &title_font,
        (text_right - text_x - tag_w).max(20.0),
    );
    let name_galley = ui.painter().layout_no_wrap(name, title_font, TEXT);
    let title_h = name_galley.size().y;
    let top = inner.center().y - text_h * 0.5;
    let name_w = name_galley.size().x;
    ui.painter()
        .galley(egui::pos2(text_x, top), name_galley, TEXT);
    if let Some(text) = tag {
        paint_tag(
            ui,
            egui::pos2(text_x + name_w + 6.0, top + title_h * 0.5),
            text,
            tag_color,
        );
    }
    let detail = fit_text(
        ui,
        &format!("{} · {} CH", row.backend.to_uppercase(), row.channels),
        &detail_font,
        (text_right - text_x).max(20.0),
    );
    ui.painter().text(
        egui::pos2(text_x, top + title_h + 4.0),
        Align2::LEFT_TOP,
        detail,
        detail_font,
        DIM,
    );

    show_focus_ring(ui, &response, rect, 8);
    row_info(
        &response,
        egui::Role::Button,
        ui.is_enabled(),
        row.name.clone(),
        Some(selected),
        Some(device_row_value(row, state)),
    );
    let mute_clicked = mute_response.clicked();
    (
        response.clicked() && !mute_clicked && !remove_clicked,
        mute_clicked,
        remove_clicked,
    )
}

/// A device row in the master inspector's list: state dot, name, tag, level
/// and state. Clicking it selects the device.
pub(super) fn list_device_row(
    ui: &mut egui::Ui,
    row: &OutputDeviceRow,
    selected: bool,
    state: EngineState,
) -> bool {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 32.0), Sense::click());
    ui.painter().rect_filled(
        rect,
        CornerRadius::same(7),
        if selected {
            with_alpha(TEAL, 18)
        } else if response.hovered() {
            with_alpha(TEXT, 10)
        } else {
            with_alpha(TEXT, 4)
        },
    );
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(7),
        Stroke::new(
            1.0,
            if selected {
                with_alpha(TEAL, 90)
            } else {
                BORDER_SOFT
            },
        ),
        egui::StrokeKind::Inside,
    );
    let (state_text, state_color) = device_state(row, state);
    let center_y = rect.center().y;
    let dot = egui::pos2(rect.left() + 14.0, center_y);
    ui.painter()
        .circle_filled(dot, 4.0, with_alpha(state_color, 60));
    ui.painter().circle_filled(dot, 2.4, state_color);

    // Fixed columns from the right: state, level, tag.
    let state_x = rect.right() - 12.0;
    let level_x = state_x - 80.0;
    let tag_x = level_x - 70.0 - tag_width(ui, "VIRTUAL");
    ui.painter().text(
        egui::pos2(state_x, center_y),
        Align2::RIGHT_CENTER,
        state_text,
        FontId::new(ts(9.0), FontFamily::Monospace),
        state_color,
    );
    ui.painter().text(
        egui::pos2(level_x, center_y),
        Align2::RIGHT_CENTER,
        device_level_text(&row.settings),
        FontId::new(ts(9.0), FontFamily::Monospace),
        if row.settings.mute { RED } else { MUTED },
    );
    if row.clock {
        paint_tag(ui, egui::pos2(tag_x, center_y), "CLOCK", BLUE);
    } else if row.virtual_out {
        paint_tag(ui, egui::pos2(tag_x, center_y), "VIRTUAL", GREEN);
    }
    let name_font = FontId::new(ts(10.5), FontFamily::Proportional);
    let name_x = rect.left() + 28.0;
    let name = fit_text(ui, &row.name, &name_font, (tag_x - name_x - 10.0).max(20.0));
    ui.painter().text(
        egui::pos2(name_x, center_y),
        Align2::LEFT_CENTER,
        name,
        name_font,
        TEXT,
    );

    show_focus_ring(ui, &response, rect, 7);
    // Named apart from the same device's row in the chain list.
    row_info(
        &response,
        egui::Role::Button,
        ui.is_enabled(),
        format!("{} in output devices", row.name),
        Some(selected),
        Some(device_row_value(row, state)),
    );
    ui.add_space(2.0);
    response.clicked()
}
