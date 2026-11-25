use super::{hint, segmented, segmented_hinted};
use crate::ui::widgets::{accesskit_frame, node_named, run_frame, test_input};
use eframe::egui::{self, accesskit::Toggled};

/// Clicks the segment AccessKit names `name` and runs `show` with that click.
fn click_segment(
    ctx: &egui::Context,
    tree: &egui::accesskit::TreeUpdate,
    name: &str,
    mut show: impl FnMut(&mut egui::Ui),
) {
    let bounds = node_named(tree, name).bounds().expect("bounds");
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
    let _ = run_frame(ctx, click, |ui| show(ui));
}

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

    let mut changed = false;
    click_segment(&ctx, &tree, "Controls", |ui| {
        changed = segmented(ui, &mut selected, &["Editor", "Controls"]).changed();
    });
    assert!(changed);
    assert_eq!(selected, 1);
}

#[test]
fn five_options_keep_the_last_segment_clickable() {
    const LABELS: [&str; 5] = ["Log", "Performance", "Latency", "Scan", "Status"];
    let mut selected = 0usize;
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let output = run_frame(&ctx, test_input(None), |ui| {
        segmented(ui, &mut selected, &LABELS);
    });
    let tree = output.platform_output.accesskit_update.expect("tree");
    let status = node_named(&tree, "Status");
    assert_eq!(status.role(), egui::Role::RadioButton);
    assert_eq!(status.toggled(), Some(Toggled::False));

    let mut changed = false;
    click_segment(&ctx, &tree, "Status", |ui| {
        changed = segmented(ui, &mut selected, &LABELS).changed();
    });
    assert!(changed);
    assert_eq!(selected, 4);
    let after = accesskit_frame(|ui| {
        segmented(ui, &mut selected, &LABELS);
    });
    assert_eq!(node_named(&after, "Status").toggled(), Some(Toggled::True));
}

#[test]
fn a_clicked_option_without_a_hint_selects_its_own_index() {
    const LABELS: [&str; 3] = ["Source", "Loopback", "Silent"];
    // Fewer hints than options: the last two are explained by their labels.
    const HINTS: [&str; 1] = ["What the input device hears"];
    let mut selected = 0usize;
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let output = run_frame(&ctx, test_input(None), |ui| {
        segmented_hinted(ui, &mut selected, &LABELS, &HINTS);
    });
    let tree = output.platform_output.accesskit_update.expect("tree");

    let mut changed = false;
    click_segment(&ctx, &tree, "Silent", |ui| {
        changed = segmented_hinted(ui, &mut selected, &LABELS, &HINTS).changed();
    });
    assert!(changed);
    assert_eq!(selected, 2);
}

#[test]
fn an_option_without_a_hint_is_explained_by_its_label() {
    assert_eq!(
        hint(&["Play what the device hears"], 0, "Source"),
        "Play what the device hears"
    );
    assert_eq!(
        hint(&["Play what the device hears"], 1, "Loopback"),
        "Loopback"
    );
    assert_eq!(hint(&[], 0, "Source"), "Source");
    assert_eq!(hint(&["one", "two", "three"], 2, "Silent"), "three");
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
