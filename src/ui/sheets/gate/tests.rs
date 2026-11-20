use eframe::egui;
use egui::accesskit::Toggled;
use egui::{pos2, Event, Modifiers, Pos2};

use super::{reduction_fraction, slot_hint, GateUnit, GATE_CONTROLS};
use crate::app::chain::{chain_stages, ChainStage};
use crate::app::{blank_app, RigWerkApp};
use crate::audio::{EngineState, GateSettings, GateSlot};
use crate::ui::widgets::{accesskit_frame, node_named, run_frame, test_input};

fn gate(app: &RigWerkApp) -> GateSettings {
    app.paths[0].as_ref().expect("path 0").mix.gate
}

/// One frame of the gate sheet of path 0 with `events`; returns the
/// AccessKit tree.
fn frame(
    ctx: &egui::Context,
    app: &mut RigWerkApp,
    events: Vec<Event>,
) -> egui::accesskit::TreeUpdate {
    let mut input = test_input(None);
    input.events = events;
    run_frame(ctx, input, |ui| app.gate_sheet(ui, 0))
        .platform_output
        .accesskit_update
        .expect("accesskit tree")
}

fn bounds(tree: &egui::accesskit::TreeUpdate, name: &str) -> egui::accesskit::Rect {
    node_named(tree, name).bounds().expect("bounds")
}

fn centre(tree: &egui::accesskit::TreeUpdate, name: &str) -> Pos2 {
    let bounds = bounds(tree, name);
    pos2(
        ((bounds.x0 + bounds.x1) / 2.0) as f32,
        ((bounds.y0 + bounds.y1) / 2.0) as f32,
    )
}

fn button(pos: Pos2, pressed: bool) -> Event {
    Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: Modifiers::default(),
    }
}

fn sheet_context() -> egui::Context {
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    ctx
}

fn click(ctx: &egui::Context, app: &mut RigWerkApp, at: Pos2) {
    frame(ctx, app, vec![Event::PointerMoved(at), button(at, true)]);
    frame(ctx, app, vec![button(at, false)]);
}

#[test]
fn every_control_has_a_name() {
    let mut app = blank_app();
    let tree = accesskit_frame(|ui| app.gate_sheet(ui, 0));
    for control in &GATE_CONTROLS {
        let _ = node_named(&tree, control.name);
    }
    for name in ["Reset", "Before effects", "After effects", "Reduction"] {
        let _ = node_named(&tree, name);
    }
    assert_eq!(
        node_named(&tree, "Threshold").value(),
        Some("−48.0 dB"),
        "the value is read with the name"
    );
}

#[test]
fn clicking_a_track_sets_its_value_through_the_path() {
    let mut app = blank_app();
    let ctx = sheet_context();
    let tree = frame(&ctx, &mut app, Vec::new());
    let track = bounds(&tree, "Threshold");
    let end = pos2(track.x1 as f32 - 1.0, centre(&tree, "Threshold").y);
    click(&ctx, &mut app, end);

    let after = gate(&app);
    assert!(after.threshold_db > -48.0, "{after:?}");
    assert_eq!(after.threshold_db, (after.threshold_db * 2.0).round() / 2.0);
    let others = GateSettings {
        threshold_db: after.threshold_db,
        ..GateSettings::default()
    };
    assert_eq!(after, others, "only the threshold moved");
    let tree = frame(&ctx, &mut app, Vec::new());
    assert_eq!(
        node_named(&tree, "Threshold").value(),
        Some(GateUnit::Db.format(after.threshold_db).as_str())
    );
}

#[test]
fn the_position_moves_the_gate_and_its_chain_row() {
    let mut app = blank_app();
    assert_eq!(gate(&app).slot, GateSlot::Post);
    let ctx = sheet_context();
    let tree = frame(&ctx, &mut app, Vec::new());
    assert_eq!(
        node_named(&tree, "After effects").toggled(),
        Some(Toggled::True)
    );
    click(&ctx, &mut app, centre(&tree, "Before effects"));

    assert_eq!(gate(&app).slot, GateSlot::Pre);
    assert_eq!(
        chain_stages(
            1,
            gate(&app).slot,
            &app.paths[0].as_ref().unwrap().nam_stack()
        )[1],
        ChainStage::Gate
    );
    let tree = frame(&ctx, &mut app, Vec::new());
    assert_eq!(
        node_named(&tree, "Before effects").toggled(),
        Some(Toggled::True)
    );
}

#[test]
fn reset_restores_the_defaults_and_keeps_on_off() {
    for enabled in [true, false] {
        let mut app = blank_app();
        app.select_path(0);
        let edited = GateSettings {
            enabled,
            slot: GateSlot::Pre,
            threshold_db: -30.0,
            range_db: -20.0,
            hysteresis_db: 12.0,
            attack_ms: 8.0,
            release_ms: 600.0,
            hold_ms: 300.0,
        };
        app.set_path_gate(edited);
        let ctx = sheet_context();
        let tree = frame(&ctx, &mut app, Vec::new());
        click(&ctx, &mut app, centre(&tree, "Reset"));

        assert_eq!(
            gate(&app),
            GateSettings {
                enabled,
                ..GateSettings::default()
            }
        );
    }
}

#[test]
fn the_meter_reads_the_gate_reduction() {
    let mut app = blank_app();
    app.select_path(0);
    app.set_path_gate(GateSettings {
        enabled: true,
        ..GateSettings::default()
    });
    app.snapshot.gate_reduction_db[0] = 24.0;
    let tree = accesskit_frame(|ui| app.gate_sheet(ui, 0));
    assert_eq!(
        node_named(&tree, "Reduction").value(),
        Some("0.0 dB"),
        "stopped, the meter is empty"
    );
    app.engine.state = EngineState::Running;
    let tree = accesskit_frame(|ui| app.gate_sheet(ui, 0));
    assert_eq!(node_named(&tree, "Reduction").value(), Some("−24.0 dB"));
}

#[test]
fn a_bypassed_gate_reads_no_reduction() {
    let mut app = blank_app();
    app.engine.state = EngineState::Running;
    app.snapshot.gate_reduction_db[0] = 24.0;
    assert!(!gate(&app).enabled);
    let tree = accesskit_frame(|ui| app.gate_sheet(ui, 0));
    assert_eq!(node_named(&tree, "Reduction").value(), Some("0.0 dB"));
}

#[test]
fn a_missing_path_draws_nothing() {
    let mut app = blank_app();
    let before = app.paths.clone();
    let _ = accesskit_frame(|ui| app.gate_sheet(ui, 3));
    assert_eq!(app.paths, before);
}

#[test]
fn the_meter_fills_against_the_range() {
    assert_eq!(reduction_fraction(0.0, -60.0), 0.0);
    assert_eq!(reduction_fraction(30.0, -60.0), 0.5);
    assert_eq!(reduction_fraction(90.0, -60.0), 1.0);
    assert_eq!(reduction_fraction(f32::NAN, -60.0), 0.0);
    assert_eq!(
        reduction_fraction(0.5, 0.0),
        0.5,
        "a 0 dB range cannot divide by zero"
    );
}

#[test]
fn values_read_like_the_rest_of_the_ui() {
    assert_eq!(GateUnit::Db.format(-48.0), "−48.0 dB");
    assert_eq!(GateUnit::Db.format(6.0), "6.0 dB");
    assert_eq!(GateUnit::Db.format(-0.01), "0.0 dB");
    assert_eq!(GateUnit::Ms.format(1.2), "1.2 ms");
    assert_eq!(GateUnit::Ms.format(120.0), "120 ms");
}

#[test]
fn hover_texts_keep_the_cards_explanations() {
    assert!(slot_hint(GateSlot::Pre).starts_with("Before effects"));
    assert!(slot_hint(GateSlot::Post).starts_with("After effects"));
    let hints: Vec<&str> = GATE_CONTROLS.iter().map(|control| control.hint).collect();
    assert!(hints[0].starts_with("Open threshold"));
    assert!(hints[2].contains("Stops chatter"));
    assert!(hints[5].contains("close threshold"));
}
