use super::super::node_named;
use super::{colour_swatch, nav_path_item, path_chip};
use crate::ui::theme::TEAL;
use eframe::egui::{self, accesskit::Toggled};

#[test]
fn path_rows_expose_their_name_and_state() {
    let tree = super::super::accesskit_frame(|ui| {
        let entry = nav_path_item(ui, TEAL, "Wet", "2 stages · 1.3 ms", false);
        assert!(!entry.clicked());
        assert!(!path_chip(ui, TEAL, "Stream", true, true, 200.0).clicked());
        assert!(!colour_swatch(ui, TEAL, "Teal", true).clicked());
    });
    let entry = node_named(&tree, "Wet");
    assert_eq!(entry.role(), egui::Role::Button);
    assert_eq!(entry.value(), Some("2 stages · 1.3 ms"));
    assert_eq!(entry.toggled(), Some(Toggled::False));
    let chip = node_named(&tree, "Stream");
    assert_eq!(chip.value(), Some("muted"));
    assert_eq!(chip.toggled(), Some(Toggled::True));
    let swatch = node_named(&tree, "Teal");
    assert_eq!(swatch.role(), egui::Role::Button);
    assert_eq!(swatch.toggled(), Some(Toggled::True));
}
