use eframe::egui;
use egui::{pos2, vec2, Event, Modifiers, Pos2};

use crate::app::blank_app;
use crate::app::issues::StopReason;
use crate::app::screen::Sheet;
use crate::audio::CallbackFailure;
use crate::ui::widgets::{accesskit_frame, node_named, run_frame, test_input};

#[test]
fn the_title_bar_names_mute_setup_and_the_tuner_but_no_classic_menu() {
    let mut app = blank_app();
    let tree = accesskit_frame(|ui| app.title_bar(ui));
    let _ = node_named(&tree, "Mute");
    let _ = node_named(&tree, "Setup");
    let _ = node_named(&tree, "Tuner");
    let _ = node_named(&tree, "RigWerk");
    assert!(
        tree.nodes
            .iter()
            .all(|(_, node)| node.label() != Some("Classic ▸")),
        "the classic menu is gone"
    );
}

#[test]
fn the_status_bar_names_the_issue_count_and_the_log() {
    let mut app = blank_app();
    let tree = accesskit_frame(|ui| app.status_bar(ui));
    let _ = node_named(&tree, "No issues");
    let _ = node_named(&tree, "Log ▴");
}

#[test]
fn a_lost_device_alert_offers_a_rescan() {
    let mut app = blank_app();
    app.alert_stopped_bridge(
        "Scarlett 2i2 was unplugged, so the bridge was stopped.".to_owned(),
        StopReason::DeviceLost,
    );
    let tree = accesskit_frame(|ui| app.error_banner(ui));
    let _ = node_named(&tree, "Rescan");
    let _ = node_named(&tree, "Open Setup");
    let _ = node_named(&tree, "Dismiss");
}

#[test]
fn a_callback_alert_offers_diagnostics_not_a_rescan() {
    let mut app = blank_app();
    app.alert_stopped_bridge(
        "A real-time audio callback panicked. The bridge was stopped.".to_owned(),
        StopReason::Callback(CallbackFailure::Panic),
    );
    let tree = accesskit_frame(|ui| app.error_banner(ui));
    let _ = node_named(&tree, "Open diagnostics");
    let _ = node_named(&tree, "Dismiss");
    for label in ["Rescan", "Open Setup"] {
        assert!(
            tree.nodes
                .iter()
                .all(|(_, node)| node.label() != Some(label)),
            "a failed callback is not a device problem, so no {label:?}"
        );
    }
}

#[test]
fn the_buffer_line_shows_the_published_quantum_before_it_is_a_reading() {
    let mut app = blank_app();
    app.buffer_size = 64;
    app.output_buffer = 128;
    app.engine.state = crate::audio::EngineState::Running;
    app.snapshot.input_block_frames = 64;
    // The native node published the 64 it asked for while the setting still
    // says 128: the line shows the published number and marks it, then keeps
    // the same number once the graph reports it.
    app.snapshot.output_step_frames = 64;
    assert_eq!(app.buffers_line(), "64 smp (asked)");
    app.snapshot.output_step_reported = true;
    assert_eq!(app.buffers_line(), "64 smp");
}

#[test]
fn the_driver_line_marks_a_rate_the_graph_has_not_reported() {
    let mut app = blank_app();
    app.engine.state = crate::audio::EngineState::Running;
    // The native path asked for the rate it will run at; until the graph
    // reports its own, the line marks the number, and one reported half is
    // not enough.
    assert!(app.drivers_line().ends_with(" (asked)"));
    app.snapshot.input_rate_reported = true;
    assert!(app.drivers_line().ends_with(" (asked)"));
    app.snapshot.output_rate_reported = true;
    assert!(!app.drivers_line().ends_with(" (asked)"));
}

fn sheet_frame(
    ctx: &egui::Context,
    app: &mut crate::app::RigWerkApp,
    events: Vec<Event>,
) -> egui::FullOutput {
    let mut input = test_input(None);
    input.events = events;
    run_frame(ctx, input, |ui| app.sheet_host(ui.ctx()))
}

fn button(pos: Pos2, pressed: bool) -> Event {
    Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: Modifiers::default(),
    }
}

fn centre(tree: &egui::accesskit::TreeUpdate, name: &str) -> Pos2 {
    let bounds = node_named(tree, name).bounds().expect("bounds");
    pos2(
        ((bounds.x0 + bounds.x1) / 2.0) as f32,
        ((bounds.y0 + bounds.y1) / 2.0) as f32,
    )
}

#[test]
fn hovering_a_button_uses_a_hand() {
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    crate::ui::theme::configure_style(&ctx);
    let mut input = test_input(None);
    let tree = run_frame(&ctx, input.clone(), |ui| {
        let _ = ui.add(egui::Button::new("Mute"));
    })
    .platform_output
    .accesskit_update
    .expect("accesskit tree");
    input.events = vec![Event::PointerMoved(centre(&tree, "Mute"))];
    let output = run_frame(&ctx, input, |ui| {
        let _ = ui.add(egui::Button::new("Mute"));
    });
    assert_eq!(
        output.platform_output.cursor_icon,
        egui::CursorIcon::PointingHand
    );
}

#[test]
fn a_painted_click_target_uses_the_same_hand() {
    let ctx = egui::Context::default();
    crate::ui::theme::configure_style(&ctx);
    let mut input = test_input(None);
    let paint = |ui: &mut egui::Ui| {
        let rect = egui::Rect::from_min_size(pos2(40.0, 40.0), egui::vec2(80.0, 32.0));
        let _ = ui.interact(rect, ui.scope_id().with("chip"), egui::Sense::click());
    };
    run_frame(&ctx, input.clone(), &paint);
    input.events = vec![Event::PointerMoved(pos2(50.0, 50.0))];
    let output = run_frame(&ctx, input, &paint);
    assert_eq!(
        output.platform_output.cursor_icon,
        egui::CursorIcon::PointingHand
    );
}

#[test]
fn the_sheet_edge_uses_a_resize_cursor_and_a_drag_grows_it() {
    let mut app = blank_app();
    app.open_sheet(Sheet::Diagnostics);
    let ctx = egui::Context::default();
    crate::ui::theme::configure_style(&ctx);
    // The first frame sizes the area and does not take the pointer.
    sheet_frame(&ctx, &mut app, Vec::new());
    sheet_frame(&ctx, &mut app, Vec::new());
    // 40 % of the 600 pt screen is under the 300 pt minimum, so the edge
    // is 300 pt up from the bottom.
    let edge = pos2(400.0, 300.0);
    let output = sheet_frame(&ctx, &mut app, vec![Event::PointerMoved(edge)]);
    assert_eq!(
        output.platform_output.cursor_icon,
        egui::CursorIcon::ResizeVertical,
        "hovering the top edge offers the drag"
    );

    sheet_frame(
        &ctx,
        &mut app,
        vec![Event::PointerMoved(edge), button(edge, true)],
    );
    let grown = edge + vec2(0.0, -120.0);
    let output = sheet_frame(&ctx, &mut app, vec![Event::PointerMoved(grown)]);
    assert_eq!(
        output.platform_output.cursor_icon,
        egui::CursorIcon::ResizeVertical,
        "the cursor stays a resize while the edge is dragged"
    );
    sheet_frame(&ctx, &mut app, vec![button(grown, false)]);
    assert!(
        (app.chrome.sheet_height - 420.0).abs() < 1.0,
        "dragging up uses the space above: {}",
        app.chrome.sheet_height
    );
}
