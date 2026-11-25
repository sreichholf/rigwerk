use crate::app::blank_app;
use crate::ui::widgets::accesskit_frame;
use eframe::egui;

fn slider_named<'a>(
    tree: &'a egui::accesskit::TreeUpdate,
    name: &str,
) -> &'a egui::accesskit::Node {
    tree.nodes
        .iter()
        .find(|(_, node)| node.label() == Some(name) && node.role() == egui::Role::Slider)
        .map(|(_, node)| node)
        .unwrap_or_else(|| panic!("no slider named {name}"))
}

#[test]
fn the_size_slider_pins_the_ui_scale_range() {
    let mut app = blank_app();
    let tree = accesskit_frame(|ui| app.setup_appearance(ui));
    let size = slider_named(&tree, "Size");
    assert_eq!(size.min_numeric_value(), Some(80.0));
    assert_eq!(size.max_numeric_value(), Some(150.0));
}
