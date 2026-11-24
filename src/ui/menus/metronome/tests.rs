use crate::app::blank_app;
use crate::audio::{CLICK_LEVEL_MAX_DB, CLICK_LEVEL_MIN_DB, METRONOME_BPM_MAX, METRONOME_BPM_MIN};
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

fn slider_width(tree: &egui::accesskit::TreeUpdate, name: &str) -> f32 {
    let bounds = slider_named(tree, name).bounds().expect("slider bounds");
    (bounds.x1 - bounds.x0) as f32
}

#[test]
fn the_menu_pins_the_tempo_and_click_level_ranges() {
    let mut app = blank_app();
    let tree = accesskit_frame(|ui| app.metronome_menu(ui));
    let tempo = slider_named(&tree, "Tempo");
    assert_eq!(
        tempo.min_numeric_value(),
        Some(f64::from(METRONOME_BPM_MIN))
    );
    assert_eq!(
        tempo.max_numeric_value(),
        Some(f64::from(METRONOME_BPM_MAX))
    );
    let level = slider_named(&tree, "Click level");
    assert_eq!(
        level.min_numeric_value(),
        Some(f64::from(CLICK_LEVEL_MIN_DB))
    );
    assert_eq!(
        level.max_numeric_value(),
        Some(f64::from(CLICK_LEVEL_MAX_DB))
    );
}

#[test]
fn the_menu_sliders_keep_their_width_in_a_wide_ui() {
    // The popup sits on egui's 600 pt default area, so a slider that took
    // `available_width` would stretch the 240 pt menu.  A wide root Ui is the
    // same trap without opening the popup.
    let mut app = blank_app();
    let tree = accesskit_frame(|ui| {
        assert!(
            ui.available_width() > 240.0,
            "the trap needs a Ui wider than the menu"
        );
        app.metronome_menu(ui);
    });
    for name in ["Tempo", "Click level"] {
        let width = slider_width(&tree, name);
        assert!(
            (width - 200.0).abs() < 0.5,
            "{name} is {width} pt wide, not 200"
        );
    }
}
