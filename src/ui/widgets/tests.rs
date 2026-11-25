use super::{
    engine_state_label, node_named, nudge_slider, run_frame, styled_slider, test_input, SliderStyle,
};
use crate::audio::EngineState;
use crate::ui::theme::DARK;
use eframe::egui;

fn near(actual: f32, expected: f32) {
    assert!((actual - expected).abs() < 1.0e-4, "{actual} != {expected}");
}

#[test]
fn every_engine_state_reads_as_its_status_word() {
    for (state, label) in [
        (EngineState::Preview, "Demo"),
        (EngineState::Running, "Running"),
        (EngineState::Starting, "Starting…"),
        (EngineState::Stopping, "Stopping…"),
        (EngineState::Error, "Error"),
        (EngineState::Idle, "Stopped"),
    ] {
        assert_eq!(engine_state_label(state), label, "{state:?}");
    }
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
fn slider_exposes_a_name_and_numeric_value() {
    let mut value = -3.0f32;
    let tree = super::accesskit_frame(|ui| {
        assert!(!styled_slider(ui, &mut value, -12.0, 12.0, 0.1, (DARK.accent, "Gain")).changed());
    });
    let node = node_named(&tree, "Gain");
    assert_eq!(node.role(), egui::Role::Slider);
    assert_eq!(node.numeric_value(), Some(-3.0));
    assert_eq!(node.value(), Some("-3.0"));
    assert_eq!(node.min_numeric_value(), Some(-12.0));
    assert_eq!(node.max_numeric_value(), Some(12.0));
}

#[test]
fn a_focused_slider_changes_with_arrow_keys_and_stays_focused() {
    let ctx = egui::Context::default();
    let mut value = 0.0f32;
    let show = |ui: &mut egui::Ui, value: &mut f32| {
        styled_slider(ui, value, -12.0, 12.0, 0.5, (DARK.accent, "EQ gain"))
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
        let response = styled_slider(ui, &mut value, 0.0, 1.0, 0.01, (DARK.accent, "Mix"));
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
        rect = styled_slider(ui, &mut value, 0.0, 1.0, 0.01, (DARK.accent, "Mix")).rect;
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
        styled_slider(ui, &mut value, 0.0, 1.0, 0.01, (DARK.accent, "Mix"));
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
        styled_slider(ui, &mut value, 0.0, 1.0, 0.01, (DARK.accent, "Mix"));
    });
    assert!(
        value > 0.9,
        "click at the right of the track left the mix at {value}"
    );
}

#[test]
fn a_custom_readout_is_what_the_screen_reader_reads() {
    let mut value = 3.0f32;
    let tree = super::accesskit_frame(|ui| {
        styled_slider(
            ui,
            &mut value,
            -12.0,
            12.0,
            0.1,
            SliderStyle::named(DARK.accent, "EQ gain").value("+3.0 dB"),
        );
    });
    let node = node_named(&tree, "EQ gain");
    assert_eq!(node.value(), Some("+3.0 dB"));
}

#[test]
fn an_explicit_width_keeps_the_row_layout() {
    let ctx = egui::Context::default();
    let mut value = 0.0f32;
    let mut rect = egui::Rect::NOTHING;
    let _ = run_frame(&ctx, test_input(None), |ui| {
        rect = styled_slider(
            ui,
            &mut value,
            0.0,
            1.0,
            0.01,
            SliderStyle::named(DARK.accent, "Mix").width(120.0),
        )
        .rect;
    });
    near(rect.width(), 120.0);
}

#[test]
fn a_logarithmic_track_puts_the_middle_at_the_geometric_mean() {
    let ctx = egui::Context::default();
    let mut value = 100.0f32;
    let mut rect = egui::Rect::NOTHING;
    let _ = run_frame(&ctx, test_input(None), |ui| {
        rect = styled_slider(
            ui,
            &mut value,
            100.0,
            1_600.0,
            1.0,
            SliderStyle::named(DARK.accent, "Release").logarithmic(),
        )
        .rect;
    });
    let pos = rect.center();
    let mut press = test_input(None);
    press.events.push(egui::Event::PointerMoved(pos));
    press.events.push(egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: egui::Modifiers::default(),
    });
    let _ = run_frame(&ctx, press, |ui| {
        styled_slider(
            ui,
            &mut value,
            100.0,
            1_600.0,
            1.0,
            SliderStyle::named(DARK.accent, "Release").logarithmic(),
        );
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
        styled_slider(
            ui,
            &mut value,
            100.0,
            1_600.0,
            1.0,
            SliderStyle::named(DARK.accent, "Release").logarithmic(),
        );
    });
    near(value, 400.0);
}
