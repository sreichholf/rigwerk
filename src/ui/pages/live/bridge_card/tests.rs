use std::cell::Cell;

use eframe::egui;
use egui::{Align, Layout, Pos2, Rect, RichText, Vec2};

use super::{
    bridge_scope_height, cushion_mode_picker, cushion_modes_fit, cushion_tab_row_width,
    manual_cushion_size,
};
use crate::audio::CushionMode;
use crate::ui::theme::{self, ts, AMBER, DIM, MUTED, TEXT};
use crate::ui::widgets::{metric_inline, micro_label, mono, progress_line, status_dot};

/// Inner height of the live row cards: shared outer height, 15 px inset each side.
const CARD_INNER_H: f32 = crate::ui::pages::live::LIVE_TOP_CARD_H - 30.0;
/// Inner width of the bridge card at the 1040 px minimum window.
const NARROW_W: f32 = 150.0;

fn run_column(width: f32, height: f32, draw: impl FnOnce(&mut egui::Ui, Rect)) -> Rect {
    let ctx = egui::Context::default();
    theme::configure_style(&ctx);
    let input = egui::RawInput {
        screen_rect: Some(Rect::from_min_size(
            Pos2::ZERO,
            Vec2::new(width + 80.0, height + 80.0),
        )),
        ..Default::default()
    };
    let mut draw = Some(draw);
    let mut content = Rect::NAN;
    let mut output = ctx.run_ui(input, |ui| {
        let Some(draw) = draw.take() else {
            return;
        };
        let clip = Rect::from_min_size(Pos2::new(8.0, 8.0), Vec2::new(width, height));
        let mut column = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(clip)
                .layout(Layout::top_down(Align::LEFT)),
        );
        column.set_clip_rect(clip);
        draw(&mut column, clip);
        content = column.min_rect();
    });
    output.textures_delta.clear();
    content
}

/// The bridge card's vertical rhythm, with the real scope height and picker.
fn bridge_column(ui: &mut egui::Ui, manual: bool) -> Rect {
    ui.horizontal(|ui| {
        ui.label(micro_label("BRIDGE", AMBER));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            status_dot(ui, DIM);
        });
    });
    ui.add_space(4.0);
    ui.label(
        RichText::new("Two clocks, one strip of audio")
            .size(ts(11.5))
            .color(TEXT)
            .strong(),
    );
    ui.label(RichText::new("Async ring buffer").size(ts(9.5)).color(DIM));
    ui.add_space(9.0);
    let scope_h = bridge_scope_height(cushion_modes_fit(ui));
    let _ = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), scope_h),
        egui::Sense::hover(),
    );
    ui.add_space(9.0);
    progress_line(ui, "BUFFER", "0%", 0.0, AMBER);
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        metric_inline(ui, "DRIFT", "OFF", MUTED);
        metric_inline(ui, "UNDERRUN", "0", MUTED);
    });
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        let mut drift = true;
        let _ = ui.checkbox(
            &mut drift,
            RichText::new("Drift correction")
                .size(ts(10.0))
                .color(MUTED),
        );
    });
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label(micro_label("CUSHION", DIM));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(mono("—", AMBER, 9.0));
        });
    });
    ui.add_space(3.0);
    let mut mode = if manual {
        CushionMode::Manual
    } else {
        CushionMode::Safe
    };
    let picker = cushion_mode_picker(ui, &mut mode);
    if manual {
        let mut ms = 10.0;
        let _ = manual_cushion_size(ui, &mut ms);
    }
    picker
}

#[test]
fn narrow_bridge_card_keeps_the_cushion_control_inside() {
    let picker = Cell::new(Rect::NAN);
    let content = run_column(NARROW_W, 800.0, |ui, _clip| {
        picker.set(bridge_column(ui, true));
    });
    let picker = picker.get();
    assert!(
        cushion_tab_row_width_at(NARROW_W) > NARROW_W,
        "the fixture stopped being narrower than the three labels"
    );
    assert!(
        picker.width() <= NARROW_W + 0.5,
        "mode control is {:.1} px in a {NARROW_W} px card",
        picker.width()
    );
    assert!(
        picker.height() <= 36.0,
        "mode control is {:.1} px tall; a wrapped row would be clipped",
        picker.height()
    );
    assert!(
        content.width() <= NARROW_W + 0.5,
        "card content is {:.1} px wide",
        content.width()
    );
    assert!(
        content.height() <= CARD_INNER_H + 0.5,
        "manual cushion block is {:.1} px tall and the card only has {CARD_INNER_H}",
        content.height()
    );
}

#[test]
fn wide_bridge_card_keeps_the_three_mode_tabs() {
    let picker = Cell::new(Rect::NAN);
    let predicted = Cell::new(0.0);
    let _ = run_column(400.0, 800.0, |ui, _clip| {
        predicted.set(cushion_tab_row_width(ui));
        assert!(cushion_modes_fit(ui));
        let mut mode = CushionMode::Safe;
        picker.set(cushion_mode_picker(ui, &mut mode));
    });
    let picker = picker.get();
    let predicted = predicted.get();
    assert!(
        (picker.width() - predicted).abs() < 1.0,
        "tab row {:.1} px, predicted {predicted:.1}",
        picker.width()
    );
    assert!(picker.width() > 180.0 && picker.width() < 220.0);
    assert!(picker.height() <= 32.0);
    assert_eq!(bridge_scope_height(true), 76.0);
    assert_eq!(bridge_scope_height(false), 24.0);
}

fn cushion_tab_row_width_at(width: f32) -> f32 {
    let width_cell = Cell::new(0.0);
    let _ = run_column(width, 80.0, |ui, _clip| {
        width_cell.set(cushion_tab_row_width(ui));
    });
    width_cell.get()
}
