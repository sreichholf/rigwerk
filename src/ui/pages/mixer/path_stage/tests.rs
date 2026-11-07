use super::{path_toolbar, PathAction};
use crate::app::blank_app;
use crate::app::paths::PathInfo;
use crate::ui::widgets::{run_frame, test_input};
use eframe::egui;

#[test]
fn path_actions_add_edit_select_and_remove_paths() {
    let mut app = blank_app();
    let first = app.path_slots()[0];
    app.apply_path_action(PathAction::Duplicate(first));
    let copy = app.path_slots()[1];
    assert_eq!(app.selected_path, copy);
    assert!(app.path().mix.mute, "a duplicate is added muted");

    app.apply_path_action(PathAction::Rename(copy, "Wet ".to_owned()));
    app.apply_path_action(PathAction::Colour(copy, 3));
    app.apply_path_action(PathAction::Level(copy, -6.0));
    app.apply_path_action(PathAction::Mute(copy, false));
    app.apply_path_action(PathAction::Solo(copy, true));
    let info = app.path_infos()[1].clone();
    assert_eq!(info.name, "Wet");
    assert_eq!(info.colour_index, 3);
    assert_eq!(info.level_db, -6.0);
    assert!(!info.mute && info.solo && info.audible);
    assert!(!app.path_infos()[0].audible, "the other path is not soloed");
    // A name another path uses is refused.
    app.apply_path_action(PathAction::Rename(copy, "main".to_owned()));
    assert_eq!(app.path_infos()[1].name, "Wet");

    app.apply_path_action(PathAction::Select(first));
    assert_eq!(app.selected_path, first);
    app.apply_path_action(PathAction::Remove(copy));
    assert_eq!(app.path_slots(), vec![first]);
    // The last path stays.
    app.apply_path_action(PathAction::Remove(first));
    assert_eq!(app.path_slots(), vec![first]);
}

fn click(input: &mut egui::RawInput, at: egui::Pos2, button: egui::PointerButton) {
    input.events.push(egui::Event::PointerMoved(at));
    for pressed in [true, false] {
        input.events.push(egui::Event::PointerButton {
            pos: at,
            button,
            pressed,
            modifiers: egui::Modifiers::default(),
        });
    }
}

/// Runs one frame of the toolbar and returns its actions and the
/// bounds of the accessibility node with `role` and (optional) `name`.
fn frame(
    ctx: &egui::Context,
    infos: &[PathInfo],
    input: egui::RawInput,
    find: (egui::accesskit::Role, Option<&str>),
) -> (Vec<PathAction>, Option<egui::Pos2>) {
    let mut actions = Vec::new();
    let output = run_frame(ctx, input, |ui| {
        ui.horizontal(|ui| path_toolbar(ui, infos, None, 600.0, &mut actions));
    });
    let tree = output.platform_output.accesskit_update.expect("tree");
    let at = tree
        .nodes
        .iter()
        .find(|(_, node)| {
            node.role() == find.0 && find.1.is_none_or(|name| node.label() == Some(name))
        })
        .and_then(|(_, node)| node.bounds())
        .map(|rect| {
            egui::pos2(
                ((rect.x0 + rect.x1) / 2.0) as f32,
                ((rect.y0 + rect.y1) / 2.0) as f32,
            )
        });
    (actions, at)
}

#[test]
fn the_chip_menu_stays_open_to_rename_a_path() {
    let mut app = blank_app();
    app.duplicate_path(app.path_slots()[0]);
    app.rename_path(app.path_slots()[1], "Wet");
    let infos = app.path_infos();
    let wet = infos[1].slot;
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let chip = (egui::accesskit::Role::Button, Some("Wet"));
    let field = (egui::accesskit::Role::TextInput, None);

    let (_, at) = frame(&ctx, &infos, test_input(None), chip);
    let mut input = test_input(None);
    click(
        &mut input,
        at.expect("the Wet chip"),
        egui::PointerButton::Secondary,
    );
    frame(&ctx, &infos, input, field);
    let (_, at) = frame(&ctx, &infos, test_input(None), field);
    let mut input = test_input(None);
    click(
        &mut input,
        at.expect("the menu's name field"),
        egui::PointerButton::Primary,
    );
    frame(&ctx, &infos, input, field);

    // The click left the menu open and focused the field.
    let mut input = test_input(None);
    input.events.push(egui::Event::Text(" Mix".to_owned()));
    let (actions, at) = frame(&ctx, &infos, input, field);
    assert!(at.is_some(), "the menu stays open while typing");
    assert!(actions.is_empty(), "no rename on every key");
    let (actions, _) = frame(&ctx, &infos, test_input(Some(egui::Key::Enter)), field);
    assert!(
        matches!(&actions[..], [PathAction::Rename(slot, name)]
            if *slot == wet && name.contains("Mix") && name.contains("Wet")),
        "{actions:?}"
    );
}
