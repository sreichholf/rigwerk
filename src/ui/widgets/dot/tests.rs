use super::path_dot;
use crate::ui::widgets::{accesskit_frame, node_named};
use eframe::egui::{self, accesskit::Toggled};

#[test]
fn a_path_dot_is_named_and_reports_whether_it_is_filled() {
    let tree = accesskit_frame(|ui| {
        path_dot(ui, 0, true, "Room");
        path_dot(ui, 1, false, "Drive plays");
    });
    let room = node_named(&tree, "Room");
    assert_eq!(room.role(), egui::Role::Button);
    assert_eq!(room.toggled(), Some(Toggled::True));
    assert_eq!(
        node_named(&tree, "Drive plays").toggled(),
        Some(Toggled::False)
    );
}
