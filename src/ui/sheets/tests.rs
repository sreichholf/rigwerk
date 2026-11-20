use crate::app::blank_app;
use crate::app::screen::Sheet;
use crate::ui::widgets::{accesskit_frame, node_named};

#[test]
fn a_stage_sheet_header_names_its_stage_and_its_controls() {
    let mut app = blank_app();
    app.open_sheet(Sheet::Eq {
        path: 0,
        post: false,
    });
    let tree = accesskit_frame(|ui| app.sheet_header(ui));
    for name in [
        "Pre EQ",
        "On",
        "Bypass",
        "Previous stage",
        "Next stage",
        "Close",
    ] {
        let _ = node_named(&tree, name);
    }
}

#[test]
fn sheet_titles_name_the_item() {
    let mut app = blank_app();
    app.open_sheet(Sheet::Eq {
        path: 0,
        post: false,
    });
    assert_eq!(app.sheet_title(app.chrome.sheet.unwrap()), "Pre EQ");
    assert!(app.step_sheet(true));
    assert_eq!(app.sheet_title(app.chrome.sheet.unwrap()), "NAM");
    assert!(app.step_sheet(true));
    assert_eq!(app.sheet_title(app.chrome.sheet.unwrap()), "Cab");
    assert!(app.step_sheet(true));
    assert_eq!(app.sheet_title(app.chrome.sheet.unwrap()), "Gate");
    assert_eq!(app.sheet_title(Sheet::Cab { path: 0 }), "Cab");
    assert_eq!(app.sheet_title(Sheet::Scope), "Scope");
    assert_eq!(app.sheet_title(Sheet::Path { path: 0 }), "Path");
    assert_eq!(
        app.sheet_title(Sheet::Effect { path: 0, index: 0 }),
        "Effect"
    );
}
