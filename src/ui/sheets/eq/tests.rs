use eframe::egui;
use egui::accesskit::Toggled;
use egui::{pos2, vec2, Event, Modifiers, Pos2, Rect};

use super::{dragged_band, gain_label, hz_label, parse_value, q_hint, scrolled_q, slot_hint, Plot};
use crate::app::{blank_app, RigWerkApp};
use crate::audio::{BandKind, EqSettings, EqSlot, EQ_BANDS};
use crate::ui::widgets::{accesskit_frame, node_named, run_frame, test_input};

fn pre_eq(app: &RigWerkApp) -> EqSettings {
    app.paths[0].as_ref().expect("path 0").mix.eqs[0]
}

/// One frame of the pre EQ sheet of path 0 with `events`; returns the
/// AccessKit tree.
fn frame(
    ctx: &egui::Context,
    app: &mut RigWerkApp,
    events: Vec<Event>,
) -> egui::accesskit::TreeUpdate {
    let mut input = test_input(None);
    input.events = events;
    run_frame(ctx, input, |ui| app.eq_sheet(ui, 0, false))
        .platform_output
        .accesskit_update
        .expect("accesskit tree")
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

fn click(ctx: &egui::Context, app: &mut RigWerkApp, at: Pos2) {
    frame(ctx, app, vec![Event::PointerMoved(at), button(at, true)]);
    frame(ctx, app, vec![button(at, false)]);
}

#[test]
fn every_control_has_a_name() {
    let mut app = blank_app();
    let tree = accesskit_frame(|ui| app.eq_sheet(ui, 0, false));
    let _ = node_named(&tree, "Flat");
    let _ = node_named(&tree, "EQ curve");
    for (index, spec) in EQ_BANDS.iter().enumerate() {
        let _ = node_named(&tree, &format!("Band {}", index + 1));
        let _ = node_named(&tree, &format!("Select band {}", index + 1));
        for control in ["frequency", "gain", "Q", "on"] {
            let _ = node_named(&tree, &format!("{} {control}", spec.name));
        }
    }
}

#[test]
fn dragging_a_handle_moves_frequency_and_gain() {
    let mut app = blank_app();
    let ctx = sheet_context();
    let tree = frame(&ctx, &mut app, Vec::new());
    let start = centre(&tree, "Band 3");
    frame(
        &ctx,
        &mut app,
        vec![Event::PointerMoved(start), button(start, true)],
    );
    frame(
        &ctx,
        &mut app,
        vec![Event::PointerMoved(start + vec2(20.0, -20.0))],
    );
    let end = start + vec2(40.0, -40.0);
    frame(&ctx, &mut app, vec![Event::PointerMoved(end)]);
    let tree = frame(&ctx, &mut app, vec![button(end, false)]);

    let band = pre_eq(&app).bands[2];
    assert!(band.hz > EQ_BANDS[2].default_hz, "{band:?}");
    assert!(band.gain_db > 0.0, "{band:?}");
    assert_eq!(band.hz, band.hz.round());
    assert_eq!(band.gain_db, (band.gain_db * 10.0).round() / 10.0);
    assert_eq!(band.q, EQ_BANDS[2].default_q);
    // The handle follows the pointer and its band is the selected one.
    assert!((centre(&tree, "Band 3") - end).length() < 2.0);
    assert_eq!(node_named(&tree, "Band 3").toggled(), Some(Toggled::True));
}

#[test]
fn double_clicking_a_handle_resets_its_band() {
    let mut app = blank_app();
    let mut eq = pre_eq(&app);
    eq.bands[1].hz = 450.0;
    eq.bands[1].gain_db = 6.0;
    eq.bands[1].q = 3.0;
    eq.bands[3].gain_db = -4.0;
    app.set_path_eq(0, eq);
    let ctx = sheet_context();
    let tree = frame(&ctx, &mut app, Vec::new());
    let at = centre(&tree, "Band 2");
    click(&ctx, &mut app, at);
    click(&ctx, &mut app, at);

    let after = pre_eq(&app);
    assert_eq!(after.bands[1], EqSettings::default().bands[1]);
    assert_eq!(after.bands[3].gain_db, -4.0, "other bands stay");
}

#[test]
fn the_wheel_over_a_handle_changes_its_q() {
    let mut app = blank_app();
    let ctx = sheet_context();
    let tree = frame(&ctx, &mut app, Vec::new());
    let at = centre(&tree, "Band 4");
    frame(&ctx, &mut app, vec![Event::PointerMoved(at)]);
    frame(
        &ctx,
        &mut app,
        vec![Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: vec2(0.0, 1.0),
            phase: egui::TouchPhase::Move,
            source: egui::MouseWheelSource::Wheel,
            modifiers: Modifiers::default(),
        }],
    );
    let band = pre_eq(&app).bands[3];
    assert!(band.q > EQ_BANDS[3].default_q, "{band:?}");
    assert_eq!(band.hz, EQ_BANDS[3].default_hz);
    assert_eq!(band.gain_db, 0.0);
}

#[test]
fn the_on_switch_turns_one_band_off() {
    let mut app = blank_app();
    let ctx = sheet_context();
    let tree = frame(&ctx, &mut app, Vec::new());
    assert_eq!(
        node_named(&tree, "Low-mid on").toggled(),
        Some(Toggled::True)
    );
    click(&ctx, &mut app, centre(&tree, "Low-mid on"));

    let eq = pre_eq(&app);
    assert!(!eq.bands[1].on);
    assert!(eq.bands[0].on && eq.bands[2].on);
    assert!(eq.enabled, "the band switch is not the EQ's switch");
}

#[test]
fn flat_resets_every_band_and_keeps_the_switch() {
    let mut app = blank_app();
    let mut eq = pre_eq(&app);
    eq.enabled = false;
    eq.bands[0].gain_db = 5.0;
    eq.bands[4].hz = 9_000.0;
    app.set_path_eq(0, eq);
    let ctx = sheet_context();
    let tree = frame(&ctx, &mut app, Vec::new());
    click(&ctx, &mut app, centre(&tree, "Flat"));

    assert_eq!(pre_eq(&app), eq.flat());
    assert!(!pre_eq(&app).enabled);
}

#[test]
fn a_missing_path_draws_nothing() {
    let mut app = blank_app();
    let before = app.paths.clone();
    let _ = accesskit_frame(|ui| app.eq_sheet(ui, 3, true));
    assert_eq!(app.paths, before);
}

#[test]
fn the_plot_maps_frequency_and_gain_both_ways() {
    let plot = Plot {
        rect: Rect::from_min_size(pos2(10.0, 20.0), vec2(600.0, 200.0)),
    };
    for hz in [20.0, 100.0, 1_000.0, 20_000.0] {
        assert!((plot.hz_at(plot.x_of(hz)) - hz).abs() / hz < 1.0e-3);
    }
    for db in [-12.0, 0.0, 6.5] {
        assert!((plot.db_at(plot.y_of(db)) - db).abs() < 1.0e-3);
    }
    assert_eq!(plot.y_of(0.0), plot.rect.center().y);
}

#[test]
fn a_drag_stays_inside_the_band_and_its_steps() {
    let plot = Plot {
        rect: Rect::from_min_size(Pos2::ZERO, vec2(600.0, 200.0)),
    };
    let band = EqSettings::default().bands[0];
    let far = dragged_band(0, band, plot, pos2(10_000.0, -1_000.0));
    assert_eq!(far.hz, EQ_BANDS[0].max_hz);
    assert_eq!(far.gain_db, 12.0);
    let low = dragged_band(0, band, plot, pos2(-50.0, 1_000.0));
    assert_eq!(low.hz, EQ_BANDS[0].min_hz);
    assert_eq!(low.gain_db, -12.0);
    let near = dragged_band(0, band, plot, pos2(plot.x_of(123.4), plot.y_of(3.04)));
    assert_eq!(near.hz, 123.0);
    assert_eq!(near.gain_db, 3.0);
    assert_eq!((near.q, near.on), (band.q, band.on));
}

#[test]
fn the_wheel_moves_q_in_steps_inside_its_range() {
    let up = scrolled_q(1.0, 40.0);
    assert!(up > 1.0 && up < 1.5);
    assert_eq!(up, (up * 100.0).round() / 100.0);
    assert!(scrolled_q(1.0, -40.0) < 1.0);
    assert_eq!(scrolled_q(5.9, 4_000.0), 6.0);
    assert_eq!(scrolled_q(0.35, -4_000.0), 0.3);
}

#[test]
fn values_read_and_parse_like_the_rest_of_the_ui() {
    assert_eq!(gain_label(-4.0), "−4.0 dB");
    assert_eq!(gain_label(3.04), "+3.0 dB");
    assert_eq!(gain_label(-0.01), "0.0 dB");
    assert_eq!(hz_label(800.0), "800 Hz");
    assert_eq!(hz_label(2_500.0), "2.5 kHz");
    assert_eq!(parse_value("−4"), Some(-4.0));
    assert_eq!(parse_value("+3.5 dB"), Some(3.5));
    assert_eq!(parse_value("2.5 kHz"), Some(2_500.0));
    assert_eq!(parse_value("1.2k"), Some(1_200.0));
    assert_eq!(parse_value("800 Hz"), Some(800.0));
    assert_eq!(parse_value("loud"), None);
}

#[test]
fn hover_texts_keep_the_cards_explanations() {
    assert!(slot_hint(EqSlot::Pre).starts_with("Before the plugin effects"));
    assert!(slot_hint(EqSlot::Post).starts_with("After the plugin effects"));
    assert!(q_hint(BandKind::Bell).contains("narrow for taming one spot"));
    assert!(q_hint(BandKind::LowShelf).contains("resonant bump"));
}
