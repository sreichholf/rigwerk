use super::{
    db_at_travel, drag_delta_db, fader, fader_height, format_level, parse_level, scroll_delta_db,
    travel_at_db, FaderRange,
};
use crate::ui::widgets::{node_named, run_frame, test_input};
use eframe::egui;

fn near(actual: f32, expected: f32) {
    assert!((actual - expected).abs() < 1.0e-3, "{actual} != {expected}");
}

fn range() -> FaderRange {
    FaderRange {
        min_db: -60.0,
        max_db: 6.0,
    }
}

#[test]
fn height_is_thirty_percent_clamped() {
    assert_eq!(fader_height(0.0), 140.0);
    assert_eq!(fader_height(400.0), 140.0);
    assert_eq!(fader_height(500.0), 150.0);
    assert_eq!(fader_height(1_000.0), 300.0);
    assert_eq!(fader_height(2_000.0), 300.0);
}

#[test]
fn unity_sits_at_the_same_height_on_every_range() {
    for (min, max) in [(-24.0, 18.0), (-60.0, 6.0), (-36.0, 6.0)] {
        near(travel_at_db(0.0, min, max), 0.75);
        near(travel_at_db(min, min, max), 0.0);
        near(travel_at_db(max, min, max), 1.0);
    }
}

#[test]
fn the_taper_gives_more_travel_near_unity() {
    let near_unity = travel_at_db(0.0, -60.0, 6.0) - travel_at_db(-6.0, -60.0, 6.0);
    let near_floor = travel_at_db(-54.0, -60.0, 6.0) - travel_at_db(-60.0, -60.0, 6.0);
    assert!(
        near_unity > 2.0 * near_floor,
        "{near_unity} vs {near_floor}"
    );
}

#[test]
fn travel_and_level_invert_each_other() {
    for (min, max) in [(-24.0, 18.0), (-60.0, 6.0), (-36.0, 0.0), (0.0, 12.0)] {
        for step in 0..=20 {
            let db = min + (max - min) * step as f32 / 20.0;
            near(db_at_travel(travel_at_db(db, min, max), min, max), db);
        }
    }
}

#[test]
fn the_readout_uses_the_spec_forms() {
    assert_eq!(format_level(0.0), "0.0");
    assert_eq!(format_level(-0.02), "0.0");
    assert_eq!(format_level(-4.0), "−4.0 dB");
    assert_eq!(format_level(6.0), "6.0 dB");
    assert_eq!(format_level(f32::NEG_INFINITY), "−∞");
    assert_eq!(format_level(-120.0), "−∞");
    assert_eq!(parse_level("−4.0 dB"), Some(-4.0));
    assert_eq!(parse_level("-4"), Some(-4.0));
    assert_eq!(parse_level("0.0"), Some(0.0));
    assert_eq!(parse_level("−∞"), Some(f32::NEG_INFINITY));
    assert_eq!(parse_level("nope"), None);
}

#[test]
fn drag_is_fine_near_unity_and_coarser_further_out() {
    near(drag_delta_db(0.0, -10.0, false), 1.0);
    near(drag_delta_db(0.0, -10.0, true), 0.1);
    near(drag_delta_db(-40.0, -10.0, false), 5.0);
    near(scroll_delta_db(1.0, false), 0.5);
    near(scroll_delta_db(1.0, true), 0.1);
    near(scroll_delta_db(-2.0, false), -1.0);
}

#[test]
fn the_fader_exposes_its_name_and_decibel_value() {
    let mut value = -4.0;
    let tree = crate::ui::widgets::accesskit_frame(|ui| {
        fader(ui, &mut value, range(), fader_height(800.0), "Path level");
    });
    let node = node_named(&tree, "Path level");
    assert_eq!(node.role(), egui::Role::Slider);
    assert_eq!(node.value(), Some("−4.0 dB"));
    near(node.numeric_value().unwrap_or(0.0) as f32, -4.0);
}

#[test]
fn dragging_up_near_unity_moves_a_tenth_per_point() {
    let ctx = egui::Context::default();
    let mut value = 0.0f32;
    let mut rect = egui::Rect::NOTHING;
    let _ = run_frame(&ctx, test_input(None), |ui| {
        rect = fader(ui, &mut value, range(), 200.0, "Master level").rect;
    });
    let start = rect.center();
    let mut press = test_input(None);
    press.events.push(egui::Event::PointerMoved(start));
    press.events.push(egui::Event::PointerButton {
        pos: start,
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: egui::Modifiers::default(),
    });
    let _ = run_frame(&ctx, press, |ui| {
        fader(ui, &mut value, range(), 200.0, "Master level");
    });
    let moved = egui::pos2(start.x, start.y - 20.0);
    let mut drag = test_input(None);
    drag.events.push(egui::Event::PointerMoved(moved));
    let _ = run_frame(&ctx, drag, |ui| {
        fader(ui, &mut value, range(), 200.0, "Master level");
    });
    near(value, 2.0);
}

#[test]
fn shift_drag_is_a_hundredth_per_point() {
    let ctx = egui::Context::default();
    let mut value = 0.0f32;
    let mut rect = egui::Rect::NOTHING;
    let _ = run_frame(&ctx, test_input(None), |ui| {
        rect = fader(ui, &mut value, range(), 200.0, "Output level").rect;
    });
    let start = rect.center();
    let modifiers = egui::Modifiers {
        shift: true,
        ..Default::default()
    };
    let mut press = test_input(None);
    press.events.push(egui::Event::PointerMoved(start));
    press.events.push(egui::Event::PointerButton {
        pos: start,
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers,
    });
    let _ = run_frame(&ctx, press, |ui| {
        fader(ui, &mut value, range(), 200.0, "Output level");
    });
    let mut drag = test_input(None);
    drag.events.push(egui::Event::ModifiersChanged(modifiers));
    drag.events.push(egui::Event::PointerMoved(egui::pos2(
        start.x,
        start.y - 20.0,
    )));
    let _ = run_frame(&ctx, drag, |ui| {
        fader(ui, &mut value, range(), 200.0, "Output level");
    });
    near(value, 0.2);
}

#[test]
fn the_wheel_steps_half_a_decibel() {
    let ctx = egui::Context::default();
    let mut value = 0.0f32;
    let mut rect = egui::Rect::NOTHING;
    let _ = run_frame(&ctx, test_input(None), |ui| {
        rect = fader(ui, &mut value, range(), 200.0, "Input gain").rect;
    });
    let mut wheel = test_input(None);
    wheel.events.push(egui::Event::PointerMoved(rect.center()));
    wheel.events.push(egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Line,
        delta: egui::vec2(0.0, 1.0),
        phase: egui::TouchPhase::Move,
        source: egui::MouseWheelSource::Unknown,
        modifiers: egui::Modifiers::default(),
    });
    let _ = run_frame(&ctx, wheel, |ui| {
        fader(ui, &mut value, range(), 200.0, "Input gain");
    });
    near(value, 0.5);
}

#[test]
fn a_double_click_on_the_rail_sets_unity() {
    let ctx = egui::Context::default();
    let mut value = -12.0f32;
    let mut rect = egui::Rect::NOTHING;
    let _ = run_frame(&ctx, test_input(None), |ui| {
        rect = fader(ui, &mut value, range(), 200.0, "Path level").rect;
    });
    let at = egui::pos2(rect.center().x, rect.top() + 30.0);
    for time in [0.0, 0.05] {
        let mut input = test_input(None);
        input.time = Some(time);
        input.events.push(egui::Event::PointerMoved(at));
        for pressed in [true, false] {
            input.events.push(egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::default(),
            });
        }
        let _ = run_frame(&ctx, input, |ui| {
            fader(ui, &mut value, range(), 200.0, "Path level");
        });
    }
    near(value, 0.0);
}

#[test]
fn a_focused_fader_moves_with_the_arrow_keys() {
    let ctx = egui::Context::default();
    let mut value = 0.0f32;
    let show = |ui: &mut egui::Ui, value: &mut f32| fader(ui, value, range(), 200.0, "Path level");
    let _ = run_frame(&ctx, test_input(None), |ui| {
        show(ui, &mut value).request_focus();
    });
    let _ = run_frame(&ctx, test_input(None), |ui| {
        show(ui, &mut value);
    });
    let _ = run_frame(&ctx, test_input(Some(egui::Key::ArrowUp)), |ui| {
        assert!(show(ui, &mut value).changed());
    });
    near(value, 0.1);
}
