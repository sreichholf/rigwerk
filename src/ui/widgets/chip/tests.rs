use super::chip;
use crate::ui::widgets::{accesskit_frame, node_named, run_frame, test_input};
use eframe::egui::{self, accesskit::Toggled};

#[test]
fn a_chip_toggles_and_exposes_its_label() {
    let mut on = false;
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let output = run_frame(&ctx, test_input(None), |ui| {
        chip(ui, &mut on, "Monitor");
    });
    let tree = output.platform_output.accesskit_update.expect("tree");
    let node = node_named(&tree, "Monitor");
    assert_eq!(node.role(), egui::Role::Button);
    assert_eq!(node.toggled(), Some(Toggled::False));
    let bounds = node.bounds().expect("bounds");
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
        assert!(chip(ui, &mut on, "Monitor").changed());
    });
    assert!(on);

    let tree = accesskit_frame(|ui| {
        chip(ui, &mut on, "Monitor");
    });
    assert_eq!(node_named(&tree, "Monitor").toggled(), Some(Toggled::True));
}
