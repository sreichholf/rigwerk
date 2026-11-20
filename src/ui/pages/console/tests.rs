use super::{console_reason, strip_rects, strips_width};
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
        "Amp",
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
fn an_open_sheet_puts_a_meter_under_each_name() {
    let mut app = blank_app();
    app.open_sheet(Sheet::Path { path: 0 });
    let tree = accesskit_frame(|ui| app.console_page(ui));
    for name in ["Input meter", "Main meter", "Master meter"] {
        let _ = node_named(&tree, name);
    }
}
