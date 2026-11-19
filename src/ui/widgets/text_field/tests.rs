use super::TextField;
use crate::ui::widgets::{accesskit_frame, node_named};
use eframe::egui;

#[test]
fn a_field_is_named_and_reports_its_text_only_when_asked() {
    let mut name = "Main".to_owned();
    let mut search = "amp".to_owned();
    let tree = accesskit_frame(|ui| {
        TextField::new(&mut name, "Rename").report_value().show(ui);
        TextField::new(&mut search, "Search")
            .hint("Search")
            .show(ui);
    });
    let rename = node_named(&tree, "Rename");
    assert_eq!(rename.role(), egui::Role::TextInput);
    assert_eq!(rename.value(), Some("Main"));
    assert_eq!(node_named(&tree, "Search").role(), egui::Role::TextInput);
}
