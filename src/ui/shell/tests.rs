use eframe::egui;
use egui::{pos2, vec2, Event, Modifiers, Pos2};

use crate::app::blank_app;
use crate::app::screen::Sheet;
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
