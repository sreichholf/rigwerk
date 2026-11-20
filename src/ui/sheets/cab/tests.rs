use eframe::egui;
use egui::{pos2, Event, Modifiers, Pos2};

use crate::app::{blank_app, RigWerkApp};
use crate::ui::theme;
use crate::ui::widgets::{node_named, run_frame, test_input};

fn styled() -> egui::Context {
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    theme::configure_style(&ctx);
    ctx
}

fn frame(ctx: &egui::Context, app: &mut RigWerkApp, events: Vec<Event>) {
    let mut input = test_input(None);
    input.events = events;
    run_frame(ctx, input, |ui| app.cab_sheet(ui, 0));
}

fn centre(tree: &egui::accesskit::TreeUpdate, name: &str) -> Pos2 {
    let bounds = node_named(tree, name).bounds().expect("bounds");
    pos2(
        ((bounds.x0 + bounds.x1) / 2.0) as f32,
        ((bounds.y0 + bounds.y1) / 2.0) as f32,
    )
}

fn tree(ctx: &egui::Context, app: &mut RigWerkApp) -> egui::accesskit::TreeUpdate {
    let mut input = test_input(None);
    input.events.clear();
    run_frame(ctx, input, |ui| app.cab_sheet(ui, 0))
        .platform_output
        .accesskit_update
        .expect("accesskit tree")
}

#[test]
fn invert_sets_the_cabinet_polarity() {
    let ctx = styled();
    let mut app = blank_app();
    app.open_sheet(crate::app::screen::Sheet::Cab { path: 0 });
    let tree = tree(&ctx, &mut app);
    let at = centre(&tree, "Invert");
    frame(
        &ctx,
        &mut app,
        vec![
            Event::PointerMoved(at),
            Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::default(),
            },
        ],
    );
    frame(
        &ctx,
        &mut app,
        vec![Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: Modifiers::default(),
        }],
    );
    assert!(app.paths[0].as_ref().unwrap().mix.cab.polarity);
}

#[test]
fn none_clears_the_cabinet_file() {
    let ctx = styled();
    let mut app = blank_app();
    app.paths[0].as_mut().unwrap().cab_file = "missing.wav".to_owned();
    app.open_sheet(crate::app::screen::Sheet::Cab { path: 0 });
    let tree = tree(&ctx, &mut app);
    let at = centre(&tree, "None");
    frame(
        &ctx,
        &mut app,
        vec![
            Event::PointerMoved(at),
            Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::default(),
            },
        ],
    );
    frame(
        &ctx,
        &mut app,
        vec![Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: Modifiers::default(),
        }],
    );
    assert!(app.paths[0].as_ref().unwrap().cab_file.is_empty());
}

fn click(ctx: &egui::Context, app: &mut RigWerkApp, name: &str) {
    let at = centre(&tree(ctx, app), name);
    frame(
        ctx,
        app,
        vec![
            Event::PointerMoved(at),
            Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::default(),
            },
        ],
    );
    frame(
        ctx,
        app,
        vec![Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: Modifiers::default(),
        }],
    );
}

fn type_chars(ctx: &egui::Context, app: &mut RigWerkApp, text: &str) {
    for ch in text.chars() {
        frame(ctx, app, vec![Event::Text(ch.to_string())]);
    }
}

fn any_text(tree: &egui::accesskit::TreeUpdate, text: &str) -> bool {
    tree.nodes.iter().any(|(_, node)| {
        node.label().is_some_and(|label| label.contains(text))
            || node.value().is_some_and(|value| value.contains(text))
    })
}

#[test]
fn a_space_in_the_impulse_folder_survives_the_keystroke() {
    let ctx = styled();
    let mut app = blank_app();
    app.open_sheet(crate::app::screen::Sheet::Cab { path: 0 });
    click(&ctx, &mut app, "Impulses folder");
    type_chars(&ctx, &mut app, "A B");
    assert_eq!(app.cab_folder, "A B");
    click(&ctx, &mut app, "None");
    assert_eq!(app.cab_folder, "A B");
}

#[test]
fn a_bypassed_nam_block_does_not_warn_about_its_cabinet() {
    let ctx = styled();
    let mut app = blank_app();
    let path = app.paths[0].as_mut().unwrap();
    let mut block = crate::app::paths::NamBlock::new(0);
    block.model = "/captures/amp.nam".to_owned();
    block.gear = Some("amp".to_owned());
    block.controls.enabled = false;
    path.nam.push(block);
    path.next_nam_id = 1;
    app.open_sheet(crate::app::screen::Sheet::Cab { path: 0 });
    let quiet = tree(&ctx, &mut app);
    assert!(!any_text(&quiet, "This capture has no cabinet"));
    app.paths[0].as_mut().unwrap().nam[0].controls.enabled = true;
    let warned = tree(&ctx, &mut app);
    assert!(any_text(&warned, "This capture has no cabinet"));
}
