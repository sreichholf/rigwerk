use eframe::egui;
use egui::accesskit::Toggled;

use crate::app::issues::{DspAlarm, IssueFilter};
use crate::app::{blank_app, Page, RigWerkApp};
use crate::ui::widgets::{accesskit_frame, node_named, run_frame, test_input};

#[test]
fn the_list_names_its_rows_and_hides_when_empty() {
    let mut app = blank_app();
    let tree = accesskit_frame(|ui| app.issues_strip(ui, 460.0));
    assert!(
        !tree
            .nodes
            .iter()
            .any(|(_, node)| node.label() == Some("Show")),
        "hidden while there is nothing to report"
    );
    let added = app.add_path().expect("a free slot");
    app.set_path_mute(added, false);
    app.refresh_issues();
    let tree = accesskit_frame(|ui| app.issues_strip(ui, 460.0));
    let _ = node_named(&tree, "Show output: Path 2 is routed nowhere");
    let _ = node_named(&tree, "Show: Path 2 is routed nowhere");
}

#[test]
fn an_output_row_names_its_action() {
    let mut app = blank_app();
    app.engine
        .note_quantum_failure("PipeWire quantum not set: test".to_owned());
    app.refresh_issues();
    let tree = accesskit_frame(|ui| app.issues_strip(ui, 460.0));
    let _ = node_named(
        &tree,
        "Open diagnostics: PipeWire did not take the buffer change",
    );
    let _ = node_named(&tree, "Show: PipeWire did not take the buffer change");
}

#[test]
fn the_filters_carry_counts() {
    let mut app = blank_app();
    let added = app.add_path().expect("a free slot");
    app.set_path_mute(added, false);
    app.refresh_issues();
    let tree = accesskit_frame(|ui| app.issues_strip(ui, 460.0));
    let all = node_named(&tree, "Issue filter: All");
    assert_eq!(all.toggled(), Some(Toggled::True));
    assert_eq!(all.value(), Some("1"));
    assert_eq!(
        node_named(&tree, "Issue filter: Warnings").value(),
        Some("1")
    );
    let errors = node_named(&tree, "Issue filter: Errors");
    assert_eq!(errors.toggled(), Some(Toggled::False));
    assert_eq!(errors.value(), Some("0"));

    app.set_issue_filter(IssueFilter::Errors);
    let tree = accesskit_frame(|ui| app.issues_strip(ui, 460.0));
    assert_eq!(
        node_named(&tree, "Issue filter: Errors").toggled(),
        Some(Toggled::True)
    );
    assert!(
        tree.nodes
            .iter()
            .all(|(_, node)| node.label() != Some("Dismiss: Path 2 is routed nowhere")),
        "the warning is filtered out"
    );
    assert!(
        tree.nodes.iter().any(|(_, node)| {
            node.label() == Some("No errors right now.")
                || node.value() == Some("No errors right now.")
        }),
        "a muted line instead of an empty body"
    );
}

#[test]
fn rows_offer_dismiss_for_warnings_and_show() {
    let mut app = blank_app();
    app.engine
        .note_quantum_failure("PipeWire quantum not set: test".to_owned());
    app.issue_state.dsp = DspAlarm {
        warning: true,
        error: true,
    };
    app.refresh_issues();
    let tree = accesskit_frame(|ui| app.issues_strip(ui, 460.0));
    let _ = node_named(&tree, "Show: PipeWire did not take the buffer change");
    let _ = node_named(&tree, "Dismiss: PipeWire did not take the buffer change");
    let _ = node_named(&tree, "Show: The chain overloads the output buffer");
    assert!(
        tree.nodes.iter().all(|(_, node)| node
            .label()
            .is_none_or(|label| !label.starts_with("Dismiss: The chain overloads"))),
        "an error has no Dismiss"
    );
}

/// Draws the list on `ctx`, clicking the centre of the node named `name`
/// from the last frame when `at` is given.
fn list_frame(
    ctx: &egui::Context,
    app: &mut RigWerkApp,
    at: Option<egui::Pos2>,
    name: &str,
) -> Option<egui::Pos2> {
    let mut input = test_input(None);
    input.screen_rect = Some(egui::Rect::from_min_size(
        egui::Pos2::ZERO,
        egui::vec2(1300.0, 600.0),
    ));
    if let Some(at) = at {
        input.events.push(egui::Event::PointerMoved(at));
        for pressed in [true, false] {
            input.events.push(egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::default(),
            });
        }
    }
    app.refresh_issues();
    let output = run_frame(ctx, input, |ui| app.issues_strip(ui, 460.0));
    let tree = output.platform_output.accesskit_update?;
    tree.nodes
        .iter()
        .find(|(_, node)| node.label() == Some(name))
        .and_then(|(_, node)| node.bounds())
        .map(|rect| {
            egui::pos2(
                ((rect.x0 + rect.x1) / 2.0) as f32,
                ((rect.y0 + rect.y1) / 2.0) as f32,
            )
        })
}

#[test]
fn clicks_dismiss_a_row_filter_it_and_show_its_anchor() {
    let mut app = blank_app();
    app.engine
        .note_quantum_failure("PipeWire quantum not set: test".to_owned());
    let ctx = egui::Context::default();
    ctx.enable_accesskit();

    let show = "Show: PipeWire did not take the buffer change";
    let at = list_frame(&ctx, &mut app, None, show);
    list_frame(&ctx, &mut app, at, show);
    assert_eq!(app.page, Page::Setup);
    assert_eq!(
        app.chrome.setup_section,
        Some(crate::app::screen::SetupSection::Clock)
    );
    assert!(app.chrome.drawer.is_none());
    app.page = Page::Console;
    app.chrome.setup_section = None;

    let filter = "Issue filter: Errors";
    let at = list_frame(&ctx, &mut app, None, filter);
    list_frame(&ctx, &mut app, at, filter);
    assert_eq!(app.issue_state.filter, IssueFilter::Errors);
    app.set_issue_filter(IssueFilter::All);

    let dismiss = "Dismiss: PipeWire did not take the buffer change";
    let at = list_frame(&ctx, &mut app, None, dismiss);
    list_frame(&ctx, &mut app, at, dismiss);
    let view = app.issue_view();
    assert!(view.visible.is_empty());
    assert_eq!(view.hidden, 1);

    let restore = "Show dismissed";
    let at = list_frame(&ctx, &mut app, None, restore);
    assert!(
        at.is_some(),
        "Show dismissed stays while a row is dismissed"
    );
    list_frame(&ctx, &mut app, at, restore);
    assert_eq!(app.issue_view().visible.len(), 1);
}
