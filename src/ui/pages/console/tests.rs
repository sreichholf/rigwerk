use super::{console_reason, strip_rects, strips_width, trim_line};
use crate::app::blank_app;
use crate::app::screen::Sheet;
use crate::ui::widgets::{accesskit_frame, node_named};

#[test]
fn strips_sit_one_point_apart() {
    assert_eq!(strips_width(0, 172.0), 0.0);
    assert_eq!(strips_width(1, 172.0), 172.0);
    assert_eq!(strips_width(4, 172.0), 4.0 * 172.0 + 3.0);
    let rects = strip_rects(10.0, 20.0, 3, 96.0, 400.0);
    assert_eq!(rects[1].left(), 10.0 + 97.0);
    assert_eq!(rects[2].right(), 10.0 + strips_width(3, 96.0));
    assert!(rects.iter().all(|rect| rect.height() == 400.0));
}

#[test]
fn the_add_columns_say_why_they_are_off() {
    assert_eq!(
        console_reason("Four paths is the most RigWerk runs."),
        "Four paths is the most"
    );
    assert_eq!(
        console_reason("A removed path is still fading out. Try again in a moment."),
        "A removed path is still fading out"
    );
    assert_eq!(
        console_reason("Four extra outputs is the maximum for now"),
        "Four extra outputs is the most"
    );
}

#[test]
fn every_strip_names_its_controls() {
    let mut app = blank_app();
    let tree = accesskit_frame(|ui| app.console_page(ui));
    for name in [
        "In 1",
        "In 2",
        "Scope",
        "Input gain level",
        "Source",
        "Pre EQ",
        "NAM",
        "Cab",
        "Gate",
        "Post EQ",
        "+ Add effect",
        "Main level",
        "Mute Main",
        "Solo Main",
        "Main path",
        "Master level",
        "Mute Master",
        "Main plays",
        "Metronome",
        "Add path",
        "Add output",
    ] {
        let _ = node_named(&tree, name);
    }
}

#[test]
fn the_paths_header_hides_match_paths_with_one_path() {
    use crate::ui::widgets::run_frame;
    let mut app = blank_app();
    let ctx = eframe::egui::Context::default();
    ctx.enable_accesskit();
    let input = eframe::egui::RawInput {
        screen_rect: Some(eframe::egui::Rect::from_min_size(
            eframe::egui::Pos2::ZERO,
            eframe::egui::vec2(1200.0, 700.0),
        )),
        ..Default::default()
    };
    let output = run_frame(&ctx, input, |ui| app.console_page(ui));
    let tree = output
        .platform_output
        .accesskit_update
        .expect("accesskit tree");
    let names: Vec<String> = tree
        .nodes
        .iter()
        .filter_map(|(_, node)| node.label().map(str::to_owned))
        .collect();
    assert!(!names.iter().any(|name| name == "Match paths"));
}

#[test]
fn the_paths_header_offers_match_paths_at_the_normal_width() {
    use crate::ui::widgets::run_frame;
    let mut app = blank_app();
    app.duplicate_path(0).expect("a second path");
    let ctx = eframe::egui::Context::default();
    ctx.enable_accesskit();
    let input = eframe::egui::RawInput {
        screen_rect: Some(eframe::egui::Rect::from_min_size(
            eframe::egui::Pos2::ZERO,
            eframe::egui::vec2(1200.0, 700.0),
        )),
        ..Default::default()
    };
    let output = run_frame(&ctx, input, |ui| app.console_page(ui));
    let tree = output
        .platform_output
        .accesskit_update
        .expect("accesskit tree");
    let _ = node_named(&tree, "Match paths");
}

#[test]
fn the_trim_line_shows_only_when_the_trim_is_not_zero() {
    assert_eq!(trim_line("match", 0.0), None);
    assert_eq!(trim_line("match", 2.5).as_deref(), Some("match +2.5"));
    assert_eq!(trim_line("trim", -1.5).as_deref(), Some("trim −1.5"));
}

#[test]
fn a_path_match_shows_on_its_strip() {
    let mut app = blank_app();
    app.set_path_match(0, 2.5);
    let tree = accesskit_frame(|ui| app.console_page(ui));
    let _ = node_named(&tree, "match +2.5");

    app.set_path_match(0, 0.0);
    let tree = accesskit_frame(|ui| app.console_page(ui));
    let names: Vec<String> = tree
        .nodes
        .iter()
        .filter_map(|(_, node)| node.label().map(str::to_owned))
        .collect();
    assert!(!names.iter().any(|name| name == "match +2.5"));
}

#[test]
fn an_output_trim_shows_on_its_strip() {
    let mut app = blank_app();
    app.set_output_trim(0, 3.0);
    let tree = accesskit_frame(|ui| app.console_page(ui));
    let _ = node_named(&tree, "trim +3.0");

    app.clear_output_learn(0);
    let tree = accesskit_frame(|ui| app.console_page(ui));
    let names: Vec<String> = tree
        .nodes
        .iter()
        .filter_map(|(_, node)| node.label().map(str::to_owned))
        .collect();
    assert!(!names.iter().any(|name| name == "trim +3.0"));
}

#[test]
fn an_open_sheet_puts_a_meter_under_each_name() {
    let mut app = blank_app();
    app.open_sheet(Sheet::Path { path: 0 });
    let tree = accesskit_frame(|ui| app.console_page(ui));
    for name in ["Input meter", "Main meter", "Master meter"] {
        let _ = node_named(&tree, name);
    }
}
