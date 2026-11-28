use std::time::Instant;

use eframe::egui;
use eframe::App as _;
use egui::Key;

use crate::app::blank_app;
use crate::app::calibration::CalibrationPhase;
use crate::app::latency::LoopbackKind;
use crate::app::screen::{Drawer, Sheet};
use crate::ui::widgets::{run_frame, test_input};

/// The calibration window consumes its own Escape before `handle_shell_keys`,
/// so Esc does not also close the overlay underneath.
#[test]
fn escape_closes_the_calibration_panel_before_the_overlay_below() {
    let ctx = egui::Context::default();
    let mut app = blank_app();
    let conditions = app.latency_conditions();
    app.calibration.open = true;
    app.calibration.phase = CalibrationPhase::Measuring {
        started: Instant::now(),
        conditions,
        loopback: LoopbackKind::Cable,
    };
    app.chrome.drawer = Some(Drawer::Issues);
    app.show_about = true;
    app.chrome.sheet = Some(Sheet::Scope);

    let mut frame = eframe::Frame::_new_kittest();
    run_frame(&ctx, test_input(Some(Key::Escape)), |ui| {
        app.ui(ui, &mut frame);
    });

    assert!(!app.calibration.open, "the panel closed");
    assert_eq!(
        app.calibration.phase,
        CalibrationPhase::Idle,
        "the measurement was cancelled"
    );
    assert_eq!(
        app.chrome.drawer,
        Some(Drawer::Issues),
        "the drawer is below the panel and stays"
    );
    assert!(app.show_about, "About is below the panel and stays");
    assert_eq!(app.chrome.sheet, Some(Sheet::Scope), "the sheet stays");
}
