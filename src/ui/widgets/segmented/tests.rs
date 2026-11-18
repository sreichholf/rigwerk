use super::segmented;
use crate::ui::widgets::{accesskit_frame, node_named, run_frame, test_input};
use eframe::egui::{self, accesskit::Toggled};

#[test]
fn each_choice_is_a_named_radio_and_a_click_selects_it() {
    let mut selected = 0usize;
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let output = run_frame(&ctx, test_input(None), |ui| {
        segmented(ui, &mut selected, &["Editor", "Controls"]);
    });
    let tree = output.platform_output.accesskit_update.expect("tree");
    let editor = node_named(&tree, "Editor");
    assert_eq!(editor.role(), egui::Role::RadioButton);
    assert_eq!(editor.toggled(), Some(Toggled::True));
    assert_eq!(
        node_named(&tree, "Controls").toggled(),
        Some(Toggled::False)
    );

    let bounds = node_named(&tree, "Controls").bounds().expect("bounds");
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
    let mut changed = false;
    let _ = run_frame(&ctx, click, |ui| {
        changed = segmented(ui, &mut selected, &["Editor", "Controls"]).changed();
    });
    assert!(changed);
    assert_eq!(selected, 1);
}

#[test]
fn arrow_keys_move_the_focused_choice() {
    let ctx = egui::Context::default();
    let mut selected = 0usize;
    let show = |ui: &mut egui::Ui, selected: &mut usize| {
        segmented(ui, selected, &["Before effects", "After effects"])
    };
    let _ = run_frame(&ctx, test_input(None), |ui| {
        show(ui, &mut selected).request_focus();
    });
    let _ = run_frame(&ctx, test_input(None), |ui| {
        show(ui, &mut selected);
    });
    let _ = run_frame(&ctx, test_input(Some(egui::Key::ArrowRight)), |ui| {
        assert!(show(ui, &mut selected).changed());
    });
    assert_eq!(selected, 1);
}

#[test]
fn labels_survive_an_accesskit_pass() {
    let mut selected = 2usize;
    let tree = accesskit_frame(|ui| {
        segmented(ui, &mut selected, &["Auto", "Minimum", "Fixed"]);
    });
    assert_eq!(node_named(&tree, "Fixed").toggled(), Some(Toggled::True));
    assert_eq!(node_named(&tree, "Auto").toggled(), Some(Toggled::False));
}
