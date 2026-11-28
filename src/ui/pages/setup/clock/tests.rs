use std::cell::Cell;

use eframe::egui;
use egui::{Align, Layout, Pos2, Rect, Vec2};

use super::{
    bridge_buffer_mode_picker, bridge_buffer_modes_fit, bridge_buffer_tab_row_width,
    fixed_bridge_buffer_size,
};
use crate::audio::BufferMode;
use crate::ui::theme;

/// A width where the three tabs do not fit.
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
fn narrow_remainder_uses_one_bridge_buffer_control() {
    let picker = Cell::new(Rect::NAN);
    let content = run_column(NARROW_W, 200.0, |ui| {
        assert!(
            !bridge_buffer_modes_fit(ui),
            "the fixture stopped being narrower than the three labels"
        );
        let mut mode = BufferMode::Fixed;
        picker.set(bridge_buffer_mode_picker(ui, &mut mode));
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
        predicted.set(bridge_buffer_tab_row_width(ui));
        assert!(bridge_buffer_modes_fit(ui));
        let mut mode = BufferMode::Auto;
        picker.set(bridge_buffer_mode_picker(ui, &mut mode));
    });
    let picker = picker.get();
    let predicted = predicted.get();
    assert!(
        (picker.width() - predicted).abs() < 1.0,
        "tab row {:.1} px, predicted {predicted:.1}",
        picker.width()
    );
    assert!(
        picker.width() > 160.0 && picker.width() < 200.0,
        "tab row {:.1} px",
        picker.width()
    );
    assert!(picker.height() <= 32.0);
}

#[test]
fn fixed_size_sits_under_the_mode_row() {
    let picker = Cell::new(Rect::NAN);
    let content = run_column(720.0, 200.0, |ui| {
        let mut mode = BufferMode::Fixed;
        picker.set(bridge_buffer_mode_picker(ui, &mut mode));
        let mut ms = 12.5;
        let _ = fixed_bridge_buffer_size(ui, &mut ms);
    });
    let picker = picker.get();
    assert!(picker.bottom() < content.bottom() - 8.0);
    assert!(
        content.height() < 120.0,
        "fixed strip is {:.1} px tall",
        content.height()
    );
}
