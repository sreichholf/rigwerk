use eframe::egui;
use egui::accesskit::Toggled;
use egui::{pos2, Event, Modifiers, Pos2};

use super::{
    format_cents, format_semitones, reset_pitch, row_mark, state_index, summary, with_state,
    STATES, USE_HINT,
};
use crate::app::screen::format_ms;
use crate::app::screen::Sheet;
use crate::app::{blank_app, RigWerkApp};
use crate::audio::PitchSettings;
use crate::ui::widgets::{accesskit_frame, node_named, run_frame, test_input};

fn pitch(app: &RigWerkApp) -> PitchSettings {
    app.paths[0].as_ref().expect("path 0").mix.pitch
}

/// One frame of `draw` with `events`; returns the AccessKit tree.
fn frame(
    ctx: &egui::Context,
    app: &mut RigWerkApp,
    events: Vec<Event>,
    draw: fn(&mut RigWerkApp, &mut egui::Ui),
) -> egui::accesskit::TreeUpdate {
    let mut input = test_input(None);
    input.events = events;
    run_frame(ctx, input, |ui| draw(app, ui))
        .platform_output
        .accesskit_update
        .expect("accesskit tree")
}

fn body(app: &mut RigWerkApp, ui: &mut egui::Ui) {
    app.pitch_sheet(ui, 0);
}

fn header(app: &mut RigWerkApp, ui: &mut egui::Ui) {
    app.sheet_header(ui);
}

fn centre(tree: &egui::accesskit::TreeUpdate, name: &str) -> Pos2 {
    let bounds = node_named(tree, name).bounds().expect("bounds");
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

/// Clicks the centre of the node `name` in `draw`'s frame.
fn click(
    ctx: &egui::Context,
    app: &mut RigWerkApp,
    name: &str,
    draw: fn(&mut RigWerkApp, &mut egui::Ui),
) {
    let tree = frame(ctx, app, Vec::new(), draw);
    let at = centre(&tree, name);
    frame(
        ctx,
        app,
        vec![Event::PointerMoved(at), button(at, true)],
        draw,
    );
    frame(ctx, app, vec![button(at, false)], draw);
}

#[test]
fn shifts_read_with_the_real_minus_and_their_unit() {
    assert_eq!(format_semitones(-2), "−2 st");
    assert_eq!(format_semitones(0), "0 st");
    assert_eq!(format_semitones(12), "+12 st");
    assert_eq!(format_semitones(-12), "−12 st");
    assert_eq!(format_cents(-10.4), "−10 ct");
    assert_eq!(format_cents(0.0), "0 ct");
    assert_eq!(format_cents(25.0), "+25 ct");
    let on = PitchSettings {
        enabled: true,
        semitones: -2,
        ..PitchSettings::default()
    };
    assert_eq!(row_mark(on), "−2 st");
    assert_eq!(row_mark(PitchSettings { cents: 5.0, ..on }), "−1.95 st");
    assert_eq!(
        row_mark(PitchSettings {
            semitones: 0,
            cents: 10.0,
            ..on
        }),
        "+0.10 st"
    );
    assert_eq!(row_mark(PitchSettings { bypass: true, ..on }), "Bypass");
    assert_eq!(
        row_mark(PitchSettings {
            enabled: false,
            ..on
        }),
        "Off"
    );
}

#[test]
fn the_three_states_round_trip_and_off_keeps_the_bypass() {
    let base = PitchSettings {
        semitones: 5,
        ..PitchSettings::default()
    };
    for index in 0..STATES.len() {
        assert_eq!(state_index(with_state(base, index)), index);
        assert_eq!(with_state(base, index).semitones, 5, "only the state moves");
    }
    let bypassed = with_state(base, 1);
    let off = with_state(bypassed, 2);
    assert!(!off.enabled && off.bypass, "off keeps the bypass");
    assert_eq!(state_index(off), 2);
    assert!(!with_state(off, 0).bypass, "on clears it");
}

#[test]
fn reset_restores_the_settings_and_keeps_the_state() {
    let edited = PitchSettings {
        enabled: true,
        bypass: true,
        semitones: -5,
        cents: 20.0,
        wet: 0.5,
        delay_ms: 20.0,
        engine: crate::audio::PitchEngine::Spectral,
        mode: crate::audio::PitchMode::Quality,
    };
    assert_eq!(
        reset_pitch(edited),
        PitchSettings {
            enabled: true,
            bypass: true,
            ..PitchSettings::default()
        }
    );
}

#[test]
fn every_control_has_a_name_and_the_sheet_shows_the_latency() {
    let mut app = blank_app();
    let tree = accesskit_frame(|ui| app.pitch_sheet(ui, 0));
    for name in [
        "Reset",
        "Semitones",
        "Semitone down",
        "Semitone up",
        "Cents",
        "Wet",
        "Delay",
    ] {
        let _ = node_named(&tree, name);
    }
    assert_eq!(node_named(&tree, "Semitones").value(), Some("0 st"));
    assert_eq!(node_named(&tree, "Wet").value(), Some("100 %"));
    assert_eq!(node_named(&tree, "Delay").value(), Some("4 ms"));
    let latency = format_ms(app.pitch_latency_ms(pitch(&app)));
    assert!(
        drawn(&tree, &summary(&latency)),
        "the summary with {latency}"
    );
    assert!(drawn(&tree, USE_HINT), "what the shifter is for");
}

#[test]
fn a_delay_drag_applies_only_when_released() {
    let mut app = blank_app();
    let ctx = sheet_context();
    let tree = frame(&ctx, &mut app, Vec::new(), body);
    let bounds = node_named(&tree, "Delay").bounds().expect("bounds");
    let y = ((bounds.y0 + bounds.y1) / 2.0) as f32;
    let (left, right) = (
        pos2(bounds.x0 as f32 + 1.0, y),
        pos2(bounds.x1 as f32 - 1.0, y),
    );
    frame(
        &ctx,
        &mut app,
        vec![Event::PointerMoved(left), button(left, true)],
        body,
    );
    let mid = pos2((left.x + right.x) / 2.0, y);
    frame(&ctx, &mut app, vec![Event::PointerMoved(mid)], body);
    let tree = frame(&ctx, &mut app, vec![Event::PointerMoved(right)], body);
    assert_eq!(pitch(&app).delay_ms, 4.0, "nothing applied mid-drag");
    assert_ne!(
        node_named(&tree, "Delay").value(),
        Some("4 ms"),
        "shows the drag"
    );
    frame(&ctx, &mut app, vec![button(right, false)], body);
    frame(&ctx, &mut app, Vec::new(), body);
    assert!(pitch(&app).delay_ms > 30.0, "{}", pitch(&app).delay_ms);
}

/// A sheet closed mid-drag must not commit the drag when it opens again.
#[test]
fn a_delay_drag_cut_off_by_closing_the_sheet_is_dropped() {
    fn closed(_: &mut RigWerkApp, _: &mut egui::Ui) {}
    let mut app = blank_app();
    let ctx = sheet_context();
    let tree = frame(&ctx, &mut app, Vec::new(), body);
    let bounds = node_named(&tree, "Delay").bounds().expect("bounds");
    let y = ((bounds.y0 + bounds.y1) / 2.0) as f32;
    let (left, right) = (
        pos2(bounds.x0 as f32 + 1.0, y),
        pos2(bounds.x1 as f32 - 1.0, y),
    );
    frame(
        &ctx,
        &mut app,
        vec![Event::PointerMoved(left), button(left, true)],
        body,
    );
    frame(&ctx, &mut app, vec![Event::PointerMoved(right)], body);
    // The sheet closes; the button comes up with nothing drawn.
    frame(&ctx, &mut app, vec![button(right, false)], closed);
    frame(&ctx, &mut app, Vec::new(), closed);
    let tree = frame(&ctx, &mut app, Vec::new(), body);
    frame(&ctx, &mut app, Vec::new(), body);
    assert_eq!(pitch(&app).delay_ms, 4.0, "the cut-off drag is dropped");
    assert_eq!(node_named(&tree, "Delay").value(), Some("4 ms"));
}

#[test]
fn the_steppers_go_through_the_path() {
    let mut app = blank_app();
    let ctx = sheet_context();
    click(&ctx, &mut app, "Semitone down", body);
    click(&ctx, &mut app, "Semitone down", body);
    assert_eq!(pitch(&app).semitones, -2);
    assert!(!pitch(&app).enabled, "an edit does not switch the stage on");
    app.push_paths_to_engine();
    assert_eq!(app.engine.shared.path(0).mix().pitch, pitch(&app));

    click(&ctx, &mut app, "Reset", body);
    assert_eq!(pitch(&app), PitchSettings::default());
}

#[test]
fn the_header_switches_the_stage_on_bypassed_and_off() {
    let mut app = blank_app();
    app.open_sheet(Sheet::Pitch { path: 0 });
    let ctx = sheet_context();
    let tree = frame(&ctx, &mut app, Vec::new(), header);
    let _ = node_named(&tree, "Pitch");
    assert_eq!(node_named(&tree, "Off").toggled(), Some(Toggled::True));

    click(&ctx, &mut app, "Bypass", header);
    assert!(pitch(&app).enabled && pitch(&app).bypass);
    click(&ctx, &mut app, "On", header);
    assert!(pitch(&app).enabled && !pitch(&app).bypass);
    assert!(app.stage_on(0, crate::app::chain::ChainStage::Pitch));
    click(&ctx, &mut app, "Off", header);
    assert!(!pitch(&app).enabled);
}

/// Whether a node in `tree` is named or reads `text`.
fn drawn(tree: &egui::accesskit::TreeUpdate, text: &str) -> bool {
    tree.nodes
        .iter()
        .any(|(_, node)| node.label() == Some(text) || node.value() == Some(text))
}

#[test]
fn spectral_swaps_the_delay_for_the_mode_and_back() {
    use crate::audio::{PitchEngine, PitchMode};
    let mut app = blank_app();
    app.open_sheet(Sheet::Pitch { path: 0 });
    let ctx = sheet_context();
    let tree = frame(&ctx, &mut app, Vec::new(), body);
    assert_eq!(node_named(&tree, "Tight").toggled(), Some(Toggled::True));
    assert!(!drawn(&tree, "Balanced"), "no mode for Tight");

    click(&ctx, &mut app, "Spectral", body);
    assert_eq!(super::engine_of(pitch(&app)), PitchEngine::Spectral);
    let tree = frame(&ctx, &mut app, Vec::new(), body);
    assert!(!drawn(&tree, "Delay"), "no delay for Spectral");
    assert_eq!(node_named(&tree, "Balanced").toggled(), Some(Toggled::True));
    let latency = format_ms(app.pitch_latency_ms(pitch(&app)));
    assert!(drawn(&tree, &latency), "the mode's latency, {latency}");

    click(&ctx, &mut app, "Quality", body);
    assert_eq!(pitch(&app).mode, PitchMode::Quality);
    click(&ctx, &mut app, "Tight", body);
    assert_eq!(super::engine_of(pitch(&app)), PitchEngine::Tight);
    assert_eq!(pitch(&app).mode, PitchMode::Quality, "the mode is kept");
}
