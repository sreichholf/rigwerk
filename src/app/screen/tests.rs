use super::{format_ms, Sheet};
use crate::app::settings::rate_label;

#[test]
fn times_and_rates_follow_the_scale() {
    assert_eq!(format_ms(7.6), "7.6 ms");
    assert_eq!(format_ms(1.0), "1.0 ms");
    assert_eq!(format_ms(13.4), "13 ms");
    assert_eq!(rate_label(48_000), "48 kHz");
    assert_eq!(rate_label(44_100), "44.1 kHz");
}

fn with_effects(names: &[&str]) -> crate::app::RigWerkApp {
    let app = crate::app::blank_app();
    app.engine.restore_plugins(
        0,
        names
            .iter()
            .map(|name| crate::audio::PluginPref {
                name: (*name).to_owned(),
                path: format!("{name}.vst3"),
                ..Default::default()
            })
            .collect(),
    );
    app
}

#[test]
fn a_stage_sheet_selects_its_path_and_marks_its_stage() {
    use crate::app::chain::ChainSelection;
    let mut app = with_effects(&["A"]);
    let second = app.add_path().expect("a free slot");
    app.select_path(0);
    app.open_issues();
    app.open_sheet(Sheet::Eq {
        path: second,
        post: true,
    });
    assert_eq!(app.selected_path, second);
    assert_eq!(app.chain_selected, ChainSelection::PostEq);
    assert_eq!(app.chrome.drawer, None, "a sheet closes the drawer");
    app.open_sheet(Sheet::Effect { path: 0, index: 0 });
    assert_eq!(app.selected_path, 0);
    assert_eq!(app.chain_selected, ChainSelection::Plugin(0));
    let before = app.chrome.sheet;
    app.open_sheet(Sheet::Gate { path: 3 });
    assert_eq!(app.chrome.sheet, before, "no path in slot 3");
}

#[test]
fn previous_and_next_walk_the_chain_in_signal_order() {
    use crate::audio::GateSlot;
    let mut app = with_effects(&["A", "B"]);
    app.path_mut().mix.gate.slot = GateSlot::Pre;
    app.open_sheet(Sheet::Pitch { path: 0 });
    assert_eq!(app.sheet_neighbour(false), None);
    let mut seen = vec![app.chrome.sheet.unwrap()];
    while app.step_sheet(true) {
        seen.push(app.chrome.sheet.unwrap());
    }
    assert_eq!(
        seen,
        [
            Sheet::Pitch { path: 0 },
            Sheet::Eq {
                path: 0,
                post: false
            },
            Sheet::Gate { path: 0 },
            Sheet::Nam { path: 0, index: 0 },
            Sheet::Effect { path: 0, index: 0 },
            Sheet::Effect { path: 0, index: 1 },
            Sheet::Cab { path: 0 },
            Sheet::Eq {
                path: 0,
                post: true
            },
        ]
    );
    app.path_mut().mix.gate.slot = GateSlot::Post;
    assert!(app.step_sheet(false));
    assert_eq!(app.chrome.sheet, Some(Sheet::Gate { path: 0 }));
    app.open_sheet(Sheet::Path { path: 0 });
    assert_eq!(app.sheet_neighbour(true), None, "only stages step");
}

#[test]
fn a_sheet_closes_when_what_it_edits_is_gone() {
    let mut app = with_effects(&["A"]);
    let second = app.add_path().expect("a free slot");
    app.open_sheet(Sheet::Path { path: second });
    app.remove_path(second);
    app.tidy_sheet();
    assert_eq!(app.chrome.sheet, None);
    app.open_sheet(Sheet::Effect { path: 0, index: 0 });
    app.tidy_sheet();
    assert!(app.chrome.sheet.is_some());
    app.remove_plugin(0);
    app.tidy_sheet();
    assert_eq!(app.chrome.sheet, None);
    app.chrome.sheet = Some(Sheet::Output { output: 1 });
    app.tidy_sheet();
    assert_eq!(app.chrome.sheet, None, "no extra output");
    app.chrome.sheet = Some(Sheet::Output { output: 0 });
    app.tidy_sheet();
    assert!(app.chrome.sheet.is_some(), "the clock output stays");
}

#[test]
fn the_sheet_grows_into_the_console_and_stops_at_its_edges() {
    let mut app = crate::app::blank_app();
    assert_eq!(app.sheet_draw_height(1000.0), 400.0, "40 % until a drag");
    app.chrome.sheet_height = 720.0;
    assert_eq!(app.sheet_draw_height(1000.0), 720.0);
    app.chrome.sheet_height = 2000.0;
    assert_eq!(
        app.sheet_draw_height(1000.0),
        1000.0,
        "the console area is the tallest"
    );
    app.chrome.sheet_height = 10.0;
    assert_eq!(app.sheet_draw_height(1000.0), 300.0);
    app.chrome.sheet_height = 10.0;
    assert_eq!(app.sheet_draw_height(200.0), 200.0, "a short window wins");
}

#[test]
fn sheet_keys_name_every_sheet() {
    assert_eq!(
        Sheet::from_key("eq", 2),
        Some(Sheet::Eq {
            path: 2,
            post: false
        })
    );
    assert_eq!(Sheet::from_key("Gate", 1), Some(Sheet::Gate { path: 1 }));
    assert_eq!(
        Sheet::from_key("amp", 0),
        Some(Sheet::Nam { path: 0, index: 0 })
    );
    assert_eq!(
        Sheet::from_key("nam", 0),
        Some(Sheet::Nam { path: 0, index: 0 })
    );
    assert_eq!(Sheet::from_key("pitch", 0), Some(Sheet::Pitch { path: 0 }));
    assert_eq!(Sheet::from_key("cab", 0), Some(Sheet::Cab { path: 0 }));
    assert_eq!(
        Sheet::from_key("effect", 0),
        Some(Sheet::Effect { path: 0, index: 0 })
    );
    assert_eq!(Sheet::from_key("path", 0), Some(Sheet::Path { path: 0 }));
    assert_eq!(
        Sheet::from_key("output", 0),
        Some(Sheet::Output { output: 0 })
    );
    assert_eq!(Sheet::from_key(" scope ", 0), Some(Sheet::Scope));
    assert_eq!(Sheet::from_key("diagnostics", 0), Some(Sheet::Diagnostics));
    assert_eq!(Sheet::from_key("mixer", 0), None);
}

/// The status bar's *Log* button and the issues' action both call
/// `open_diagnostics`, so About cannot stay up behind the sheet.  Escape then
/// has one `Middle` window to close, and it is the sheet the user can see.
#[test]
fn the_log_action_leaves_one_middle_window_that_escape_closes() {
    let mut app = crate::app::blank_app();
    app.show_about = true;
    app.open_diagnostics();
    assert!(!app.show_about, "Log closes About");
    assert_eq!(app.chrome.sheet, Some(Sheet::Diagnostics));
    assert!(app.close_innermost());
    assert_eq!(
        app.chrome.sheet, None,
        "Escape closes the sheet that was up, not About"
    );
    assert!(!app.show_about);
    assert!(!app.close_innermost());
}

/// A drawer is a `Foreground` area, so it closes before the `Middle` window
/// beneath it.
#[test]
fn escape_closes_the_drawer_over_about_first() {
    let mut app = crate::app::blank_app();
    app.show_about = true;
    app.open_issues();
    assert!(app.chrome.drawer.is_some());
    assert!(app.close_innermost());
    assert!(app.chrome.drawer.is_none(), "the drawer is over About");
    assert!(app.show_about, "About is still up");
    assert!(app.close_innermost());
    assert!(!app.show_about);
}

#[test]
fn escape_closes_a_lone_about() {
    let mut app = crate::app::blank_app();
    app.show_about = true;
    assert!(app.close_innermost());
    assert!(!app.show_about);
    assert!(!app.close_innermost());
}

#[test]
fn opening_a_sheet_and_opening_about_close_each_other() {
    let mut app = crate::app::blank_app();
    app.show_about = true;
    app.open_diagnostics();
    assert!(!app.show_about, "only one Middle window is up");
    assert_eq!(app.chrome.sheet, Some(Sheet::Diagnostics));

    app.open_about();
    assert_eq!(app.chrome.sheet, None, "About closes the sheet");
    assert!(app.show_about);
}
