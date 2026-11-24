use crate::app::blank_app;
use crate::app::screen::Sheet;
use crate::ui::widgets::{accesskit_frame, node_named};

#[test]
fn the_match_paths_sheet_names_its_reference_and_measure() {
    let mut app = blank_app();
    app.open_sheet(Sheet::MatchPaths);
    let tree = accesskit_frame(|ui| app.sheet_body(ui, 600.0));
    let target = node_named(&tree, "Target");
    assert_eq!(
        target.toggled(),
        Some(eframe::egui::accesskit::Toggled::True)
    );
    let _ = node_named(&tree, "A path");
    let _ = node_named(&tree, "Reference loudness");
    let _ = node_named(&tree, "Measure");
}
