use super::popover;
use crate::ui::widgets::{node_named, run_frame, test_input};
use eframe::egui;

#[test]
fn clicking_the_anchor_shows_the_popover_body() {
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let output = run_frame(&ctx, test_input(None), |ui| {
        let anchor = ui.button("Source");
        popover(ui, &anchor, |ui| {
            let _ = ui.button("In 1");
        });
    });
    let tree = output.platform_output.accesskit_update.expect("tree");
    let bounds = node_named(&tree, "Source").bounds().expect("bounds");
    let at = egui::pos2(
        ((bounds.x0 + bounds.x1) / 2.0) as f32,
        ((bounds.y0 + bounds.y1) / 2.0) as f32,
    );
    let mut click = test_input(None);
    click.events.push(egui::Event::PointerMoved(at));
    for pressed in [true, false] {
        click.events.push(egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        });
    }
    let _ = run_frame(&ctx, click, |ui| {
        let anchor = ui.button("Source");
        popover(ui, &anchor, |ui| {
            let _ = ui.button("In 1");
        });
    });
    let output = run_frame(&ctx, test_input(None), |ui| {
        let anchor = ui.button("Source");
        popover(ui, &anchor, |ui| {
            let _ = ui.button("In 1");
        });
    });
    let tree = output.platform_output.accesskit_update.expect("tree");
    assert_eq!(node_named(&tree, "In 1").role(), egui::Role::Button);
}
