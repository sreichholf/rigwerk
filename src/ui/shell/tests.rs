use crate::app::blank_app;
use crate::ui::widgets::{accesskit_frame, node_named};

#[test]
fn the_title_bar_names_mute_setup_and_classic() {
    let mut app = blank_app();
    let tree = accesskit_frame(|ui| app.title_bar(ui));
    let _ = node_named(&tree, "Mute");
    let _ = node_named(&tree, "Setup");
    let _ = node_named(&tree, "Classic ▸");
    let _ = node_named(&tree, "RigWerk");
}

#[test]
fn the_status_bar_names_the_issue_count_and_the_log() {
    let mut app = blank_app();
    let tree = accesskit_frame(|ui| app.status_bar(ui));
    let _ = node_named(&tree, "No issues");
    let _ = node_named(&tree, "Log ▴");
}
