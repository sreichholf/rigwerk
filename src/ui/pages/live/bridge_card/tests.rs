use std::cell::Cell;

use eframe::egui;
use egui::{Align, Layout, Pos2, Rect, Vec2};

use super::{cushion_mode_picker, cushion_modes_fit, cushion_tab_row_width, manual_cushion_size};
use crate::audio::CushionMode;
use crate::ui::theme;

/// Inner width of the old bridge column at the 1040 px minimum window.
/// The strip is the full page now; this width is only the "tabs do not fit" case.
const NARROW_W: f32 = 150.0;

fn run_column(width: f32, height: f32, draw: impl FnOnce(&mut egui::Ui)) -> Rect {
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
        draw(&mut column);
        content = column.min_rect();
    });
    output.textures_delta.clear();
    content
}

#[test]
fn narrow_remainder_uses_one_cushion_control() {
    let picker = Cell::new(Rect::NAN);
    let content = run_column(NARROW_W, 200.0, |ui| {
        assert!(
            !cushion_modes_fit(ui),
            "the fixture stopped being narrower than the three labels"
        );
        let mut mode = CushionMode::Manual;
        picker.set(cushion_mode_picker(ui, &mut mode));
    });
    let picker = picker.get();
    assert!(
        picker.width() <= NARROW_W + 0.5,
        "mode control is {:.1} px in a {NARROW_W} px row",
        picker.width()
    );
    assert!(
        picker.height() <= 36.0,
        "mode control is {:.1} px tall; a wrapped row would be clipped",
        picker.height()
    );
    assert!(
        content.width() <= NARROW_W + 0.5,
        "row content is {:.1} px wide",
        content.width()
    );
}

#[test]
fn wide_strip_keeps_the_three_mode_tabs() {
    let picker = Cell::new(Rect::NAN);
    let predicted = Cell::new(0.0);
    let _ = run_column(400.0, 80.0, |ui| {
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
}

#[test]
fn manual_size_sits_under_the_mode_row() {
    let picker = Cell::new(Rect::NAN);
    let content = run_column(720.0, 200.0, |ui| {
        let mut mode = CushionMode::Manual;
        picker.set(cushion_mode_picker(ui, &mut mode));
        let mut ms = 12.5;
        let _ = manual_cushion_size(ui, &mut ms);
    });
    let picker = picker.get();
    assert!(picker.bottom() < content.bottom() - 8.0);
    assert!(
        content.height() < 120.0,
        "manual strip is {:.1} px tall",
        content.height()
    );
}
