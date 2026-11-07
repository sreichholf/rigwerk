use super::issue_counts;
use crate::app::blank_app;
use crate::ui::widgets::{accesskit_frame, node_named};

#[test]
fn the_strip_counts_and_names_its_rows() {
    let mut app = blank_app();
    let tree = accesskit_frame(|ui| app.issues_strip(ui, 1200.0));
    assert!(
        !tree
            .nodes
            .iter()
            .any(|(_, node)| node.label() == Some("Live issues")),
        "hidden while there is nothing to report"
    );
    let added = app.add_path().expect("a free slot");
    app.set_path_mute(added, false);
    let issues = app.live_issues();
    assert_eq!(issue_counts(&issues), "1 warning");
    let tree = accesskit_frame(|ui| app.issues_strip(ui, 1200.0));
    let strip = node_named(&tree, "Live issues");
    assert_eq!(strip.value(), Some("1 warning"));
    let _ = node_named(&tree, "Show in Routing: Path 2 is routed nowhere");
}
