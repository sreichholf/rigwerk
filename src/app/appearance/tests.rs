use eframe::egui;

use super::{drawn_theme, sanitize_ui_size, stepped_ui_size, ThemeChoice, UI_SIZE_DEFAULT};
use crate::app::blank_app;
use crate::ui::widgets::{run_frame, test_input};

/// Runs one pass, so a zoom factor set on the context takes effect.
fn pass(ctx: &egui::Context) {
    let _ = run_frame(ctx, test_input(None), |_| {});
}

#[test]
fn ui_size_is_clamped_and_a_nan_is_the_default() {
    assert_eq!(sanitize_ui_size(f32::NAN), UI_SIZE_DEFAULT);
    assert_eq!(sanitize_ui_size(1.2), 1.2);
    assert_eq!(sanitize_ui_size(0.1), 0.8);
    assert_eq!(sanitize_ui_size(3.0), 1.5);
    assert_eq!(sanitize_ui_size(f32::INFINITY), 1.5);
    assert_eq!(sanitize_ui_size(f32::NEG_INFINITY), 0.8);
}

#[test]
fn a_step_is_ten_percent_on_the_grid_and_stops_at_the_ends() {
    assert_eq!(stepped_ui_size(1.0, 1), 1.1);
    assert_eq!(stepped_ui_size(1.0, -1), 0.9);
    assert_eq!(stepped_ui_size(1.04, 1), 1.1, "snaps to the 10 % grid");
    assert_eq!(stepped_ui_size(1.5, 1), 1.5);
    assert_eq!(stepped_ui_size(0.8, -1), 0.8);
    let mut size = 0.8;
    for _ in 0..7 {
        size = stepped_ui_size(size, 1);
    }
    assert_eq!(size, 1.5, "seven steps from 80 % reach 150 % exactly");
}

#[test]
fn the_saved_theme_and_size_win_over_egui_memory() {
    let ctx = egui::Context::default();
    // What eframe restores from egui's own saved memory.
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.set_zoom_factor(1.3);
    pass(&ctx);

    let mut app = blank_app();
    app.theme = ThemeChoice::Light;
    app.ui_size = 1.2;
    app.apply_appearance(&ctx, Some(ThemeChoice::Light));
    pass(&ctx);
    assert_eq!(ctx.theme(), egui::Theme::Light);
    assert_eq!(ctx.zoom_factor(), 1.2);
    assert!(!ctx.options(|options| options.zoom_with_keyboard));
}

/// Until the console replaces the old pages, which paint with the dark
/// constants, every saved choice draws dark and stays saved as chosen.
#[test]
fn the_window_stays_dark_until_the_old_pages_are_gone() {
    for saved in [ThemeChoice::System, ThemeChoice::Light, ThemeChoice::Dark] {
        assert_eq!(drawn_theme(saved, None), ThemeChoice::Dark);
        assert_eq!(
            drawn_theme(saved, Some(ThemeChoice::Light)),
            ThemeChoice::Light
        );

        let ctx = egui::Context::default();
        let mut app = blank_app();
        app.theme = saved;
        app.apply_appearance(&ctx, None);
        assert_eq!(ctx.theme(), egui::Theme::Dark);
        assert_eq!(app.theme, saved);
    }
}

#[test]
fn system_is_the_default_choice_and_the_fallback_is_dark() {
    let ctx = egui::Context::default();
    let app = blank_app();
    assert_eq!(app.theme, ThemeChoice::System);
    app.apply_appearance(&ctx, None);
    assert_eq!(
        ctx.options(|options| options.fallback_theme),
        egui::Theme::Dark
    );
    assert_eq!(ctx.theme(), egui::Theme::Dark);
}

#[test]
fn the_override_wins_for_the_run_and_is_not_kept() {
    let ctx = egui::Context::default();
    let mut app = blank_app();
    app.theme = ThemeChoice::Dark;
    app.apply_appearance(&ctx, Some(ThemeChoice::Light));
    assert_eq!(ctx.theme(), egui::Theme::Light);
    assert_eq!(app.theme, ThemeChoice::Dark);
}

#[test]
fn setters_store_the_value_and_change_the_context() {
    let ctx = egui::Context::default();
    let mut app = blank_app();
    app.set_theme(&ctx, ThemeChoice::Light);
    assert_eq!(app.theme, ThemeChoice::Light);
    assert_eq!(ctx.theme(), egui::Theme::Dark);

    app.set_ui_size(&ctx, 9.0);
    assert_eq!(app.ui_size, 1.5);
    app.step_ui_size(&ctx, -2);
    assert_eq!(app.ui_size, 1.3);
    pass(&ctx);
    assert_eq!(ctx.zoom_factor(), 1.3);
}
