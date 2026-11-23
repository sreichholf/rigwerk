use eframe::egui;
use egui::{pos2, Event, Modifiers, Pos2};

use crate::app::nam::set_ctrl_v_for_test;
use crate::app::{blank_app, RigWerkApp};
use crate::ui::theme;
use crate::ui::widgets::{node_named, run_frame, test_input};

fn styled() -> egui::Context {
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    theme::configure_style(&ctx);
    ctx
}

fn frame(
    ctx: &egui::Context,
    app: &mut RigWerkApp,
    events: Vec<Event>,
) -> egui::accesskit::TreeUpdate {
    let mut input = test_input(None);
    input.events = events;
    run_frame(ctx, input, |ui| app.nam_sheet(ui, 0, 0))
        .platform_output
        .accesskit_update
        .expect("accesskit tree")
}

fn bounds(tree: &egui::accesskit::TreeUpdate, name: &str) -> egui::accesskit::Rect {
    node_named(tree, name).bounds().expect("bounds")
}

#[test]
fn the_nam_block_names_its_loudness_and_normalize_switch() {
    let ctx = styled();
    let mut app = blank_app();
    let _ = app.add_nam_block(0).expect("a block");
    let tree = frame(&ctx, &mut app, Vec::new());
    let normalize = node_named(&tree, "Normalize");
    assert_eq!(normalize.role(), egui::accesskit::Role::CheckBox);
}

fn text_field(tree: &egui::accesskit::TreeUpdate) -> &egui::accesskit::Node {
    tree.nodes
        .iter()
        .find(|(_, node)| node.role() == egui::accesskit::Role::TextInput)
        .map(|(_, node)| node)
        .expect("models folder field")
}

fn centre(rect: egui::accesskit::Rect) -> Pos2 {
    pos2(
        ((rect.x0 + rect.x1) / 2.0) as f32,
        ((rect.y0 + rect.y1) / 2.0) as f32,
    )
}

fn click(ctx: &egui::Context, app: &mut RigWerkApp, at: Pos2) {
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

struct ReleaseCtrlV;

impl Drop for ReleaseCtrlV {
    fn drop(&mut self) {
        set_ctrl_v_for_test(false);
    }
}

#[test]
fn folder_row_lines_up() {
    let ctx = styled();
    let mut app = blank_app();
    let tree = frame(&ctx, &mut app, vec![]);
    let paste = bounds(&tree, "Paste");
    let browse = bounds(&tree, "Browse");
    let field = text_field(&tree).bounds().expect("field bounds");
    assert!(
        (paste.y0 - browse.y0).abs() < 1.0 && (paste.y1 - browse.y1).abs() < 1.0,
        "Paste {paste:?} and Browse {browse:?} are not the same height"
    );
    assert!(
        (paste.y0 - field.y0).abs() < 1.0 && (paste.y1 - field.y1).abs() < 1.0,
        "Paste {paste:?} and the path field {field:?} are not on one row"
    );
}

#[test]
fn pasting_a_quoted_path_uses_the_folder() {
    let dir = std::env::temp_dir().join(format!("rigwerk-paste-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp folder");
    let path = dir.to_string_lossy().into_owned();
    let ctx = styled();
    let mut app = blank_app();
    let tree = frame(&ctx, &mut app, vec![]);
    let at = centre(text_field(&tree).bounds().expect("field bounds"));
    click(&ctx, &mut app, at);
    frame(&ctx, &mut app, vec![Event::Paste(format!("\"{path}\""))]);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(app.nam_folder, path);
}

#[test]
fn ctrl_v_does_not_steal_from_another_field() {
    let _release = ReleaseCtrlV;
    set_ctrl_v_for_test(true);
    let ctx = styled();
    ctx.memory_mut(|memory| memory.request_focus(egui::Id::unique("elsewhere")));
    let mut app = blank_app();
    frame(&ctx, &mut app, vec![]);
    assert!(
        app.nam_folder.is_empty(),
        "stole a paste: {}",
        app.nam_folder
    );
    assert!(
        app.log.is_empty(),
        "read the clipboard while another field was focused"
    );
}

#[test]
fn ctrl_v_without_an_egui_event_pastes_once() {
    let _release = ReleaseCtrlV;
    set_ctrl_v_for_test(true);
    let ctx = styled();
    let mut app = blank_app();
    frame(&ctx, &mut app, vec![]);
    frame(&ctx, &mut app, vec![]);
    let logs = app.log.len();
    assert!(
        logs <= 1,
        "a held Ctrl+V pasted on more than one frame ({logs} logs)"
    );
    assert!(
        !app.nam_folder.is_empty() || logs == 1,
        "Ctrl+V did not read the clipboard"
    );
    set_ctrl_v_for_test(false);
    frame(&ctx, &mut app, vec![]);
    set_ctrl_v_for_test(true);
    frame(&ctx, &mut app, vec![]);
    if logs == 1 {
        assert_eq!(
            app.log.len(),
            2,
            "releasing Ctrl+V should arm the next press"
        );
        assert!(app.log.iter().all(|entry| entry.text.contains("clipboard")));
    } else {
        assert!(app.log.is_empty());
        assert!(!app.nam_folder.is_empty());
    }
}
