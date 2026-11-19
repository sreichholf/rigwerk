use super::{level_label, PATH_COLOUR_NAMES};
use crate::ui::theme::DARK;

#[test]
fn a_level_reads_with_the_real_minus_and_one_decimal() {
    assert_eq!(level_label(0.0), "0.0 dB");
    assert_eq!(level_label(-6.0), "−6.0 dB");
    assert_eq!(level_label(1.5), "+1.5 dB");
}

#[test]
fn every_path_colour_has_a_name() {
    assert_eq!(PATH_COLOUR_NAMES.len(), DARK.path.len());
}

#[test]
fn the_source_grid_and_the_output_chips_are_named() {
    use crate::app::blank_app;
    use crate::app::screen::Sheet;
    use crate::ui::widgets::{accesskit_frame, node_named};

    let mut app = blank_app();
    app.open_sheet(Sheet::Path { path: 0 });
    let tree = accesskit_frame(|ui| app.sheet_body(ui, 600.0));
    let in_one = node_named(&tree, "In 1 to L");
    assert_eq!(in_one.role(), eframe::egui::Role::CheckBox);
    let _ = node_named(&tree, "In 1 to R");
    let _ = node_named(&tree, "In 2 to L");
    let _ = node_named(&tree, "In 2 to R");
    let left = node_named(&tree, "Path output L");
    assert_eq!(left.toggled(), Some(eframe::egui::accesskit::Toggled::True));
    let _ = node_named(&tree, "Path output R");
}
