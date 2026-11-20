use eframe::egui;
use egui::{pos2, Event, Modifiers, Pos2};

use crate::app::screen::Sheet;
use crate::app::{blank_app, RigWerkApp};
use crate::audio::cab::CabCatalogEntry;
use crate::audio::nam::{NamCatalogEntry, NamLoad};
use crate::ui::theme;
use crate::ui::widgets::{accesskit_frame, node_named, run_frame, test_input};

fn styled() -> egui::Context {
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    theme::configure_style(&ctx);
    ctx
}

fn bounds(tree: &egui::accesskit::TreeUpdate, name: &str) -> egui::accesskit::Rect {
    node_named(tree, name).bounds().expect("bounds")
}

fn text_bounds(tree: &egui::accesskit::TreeUpdate, text: &str) -> egui::accesskit::Rect {
    tree.nodes
        .iter()
        .find(|(_, node)| {
            node.label().is_some_and(|label| label.contains(text))
                || node.value().is_some_and(|value| value.contains(text))
        })
        .and_then(|(_, node)| node.bounds())
        .unwrap_or_else(|| panic!("no node containing {text}"))
}

fn width(rect: egui::accesskit::Rect) -> f64 {
    rect.x1 - rect.x0
}

fn seed_models(app: &mut RigWerkApp, count: usize) {
    let folder = "/models".to_owned();
    app.nam_folder.clone_from(&folder);
    app.nam_catalog.folder = folder;
    app.nam_rescan = false;
    app.nam_catalog.entries = (0..count)
        .map(|index| NamCatalogEntry {
            path: format!("/models/{index}.nam"),
            name: format!("Model {index}"),
            arch: "WaveNet".to_owned(),
            file_rate: 48_000,
            slimmable: false,
            load: NamLoad::default(),
        })
        .collect();
}

/// A path needs one NAM block for the sheet to show its settings column.
fn seed_block(app: &mut RigWerkApp) {
    assert!(app.add_nam_block(0).is_some(), "a block fits");
}

fn seed_cabs(app: &mut RigWerkApp, count: usize) {
    let folder = "/impulses".to_owned();
    app.cab_folder.clone_from(&folder);
    app.cab_catalog.folder = folder;
    app.cab_rescan = false;
    app.cab_catalog.entries = (0..count)
        .map(|index| CabCatalogEntry {
            path: format!("/impulses/{index}.wav"),
            name: format!("Cab {index}"),
        })
        .collect();
}

fn centre(rect: egui::accesskit::Rect) -> Pos2 {
    pos2(
        ((rect.x0 + rect.x1) / 2.0) as f32,
        ((rect.y0 + rect.y1) / 2.0) as f32,
    )
}

fn click(
    ctx: &egui::Context,
    app: &mut RigWerkApp,
    at: Pos2,
    draw: impl Fn(&mut RigWerkApp, &mut egui::Ui),
) {
    let mut press = test_input(None);
    press.events = vec![
        Event::PointerMoved(at),
        Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: Modifiers::default(),
        },
    ];
    run_frame(ctx, press, |ui| draw(app, ui));
    let mut release = test_input(None);
    release.events = vec![Event::PointerButton {
        pos: at,
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: Modifiers::default(),
    }];
    run_frame(ctx, release, |ui| draw(app, ui));
}

/// The list stays on the left and the settings stay at its top, and the two
/// sheets use the same column.  Drawn through the sheet body, which is the
/// height the window actually gives these sheets.
#[test]
fn the_list_stays_beside_the_settings() {
    let mut nam_app = blank_app();
    nam_app.open_sheet(Sheet::Nam { path: 0, index: 0 });
    seed_block(&mut nam_app);
    seed_models(&mut nam_app, 30);
    let nam = accesskit_frame(|ui| nam_app.sheet_body(ui, 600.0));
    let model = bounds(&nam, "Model 0");
    let last = bounds(&nam, "Model 29");
    let before = bounds(&nam, "Before effects");
    let input = bounds(&nam, "Input");

    let mut cabs = blank_app();
    cabs.open_sheet(Sheet::Cab { path: 0 });
    seed_cabs(&mut cabs, 30);
    let cab = accesskit_frame(|ui| cabs.sheet_body(ui, 600.0));
    let none = bounds(&cab, "None");
    let last_cab = bounds(&cab, "Cab 29");
    let level = bounds(&cab, "Level");

    assert!(
        model.x1 <= before.x0 && none.x1 <= level.x0,
        "the list is not left of the settings"
    );
    assert!(
        (model.y0 - before.y0).abs() < 24.0 && (none.y0 - level.y0).abs() < 24.0,
        "the settings were pushed below the list"
    );
    assert!(
        last.y0 > input.y1 && last_cab.y0 > level.y1,
        "a long list does not extend past the settings"
    );
    assert!(
        (model.x0 - none.x0).abs() < 1.0 && (width(model) - width(none)).abs() < 1.0,
        "the NAM and cab lists are not the same column"
    );
    assert!(
        (before.x0 - level.x0).abs() < 1.0,
        "the NAM and cab settings are not the same column"
    );
    assert!(
        width(model) > 250.0,
        "the list column is not half the sheet ({})",
        width(model)
    );
}

/// Several frames with time moving, so a programmatic scroll finishes.
fn settle(
    ctx: &egui::Context,
    app: &mut RigWerkApp,
    mut draw: impl FnMut(&mut RigWerkApp, &mut egui::Ui),
) -> egui::accesskit::TreeUpdate {
    let mut tree = None;
    for step in 0..8 {
        let mut input = test_input(None);
        input.time = Some(f64::from(step) * 0.1);
        input.predicted_dt = 0.1;
        tree = Some(
            run_frame(ctx, input, |ui| draw(app, ui))
                .platform_output
                .accesskit_update
                .expect("accesskit tree"),
        );
    }
    tree.expect("frame")
}

fn tree_of(
    ctx: &egui::Context,
    app: &mut RigWerkApp,
    mut draw: impl FnMut(&mut RigWerkApp, &mut egui::Ui),
) -> egui::accesskit::TreeUpdate {
    run_frame(ctx, test_input(None), |ui| draw(app, ui))
        .platform_output
        .accesskit_update
        .expect("accesskit tree")
}

/// A short sheet stays that tall: the file list scrolls inside it instead
/// of growing the body.
#[test]
fn the_list_fills_the_given_sheet_height() {
    let mut app = blank_app();
    app.open_sheet(Sheet::Nam { path: 0, index: 0 });
    seed_block(&mut app);
    seed_models(&mut app, 40);
    let tree = accesskit_frame(|ui| {
        app.sheet_body(ui, 220.0);
        ui.label("Sheet end");
    });
    let end = text_bounds(&tree, "Sheet end");
    assert!(
        (180.0..260.0).contains(&end.y0),
        "a long list changed the sheet body ({})",
        end.y0
    );
    let model = bounds(&tree, "Model 0");
    let before = bounds(&tree, "Before effects");
    assert!(
        (model.y0 - before.y0).abs() < 24.0,
        "the settings were not beside the list ({}, {})",
        model.y0,
        before.y0
    );
    assert!(
        bounds(&tree, "Model 39").y0 > before.y1,
        "the list does not scroll past the settings"
    );
}

/// Opening the cab sheet must still scroll to its own selection after the
/// NAM sheet has revealed the same row index.
#[test]
fn each_sheet_scrolls_to_its_own_selection() {
    let ctx = styled();
    let mut app = blank_app();
    app.open_sheet(Sheet::Nam { path: 0, index: 0 });
    seed_models(&mut app, 30);
    seed_cabs(&mut app, 30);
    // The same identity on purpose: a row index, or a path, must not be
    // enough for the two sheets to share one "already revealed" flag.
    app.nam_catalog.entries[20].path = "/captures/20".to_owned();
    app.cab_catalog.entries[20].path = "/captures/20".to_owned();
    let path = app.paths[0].as_mut().unwrap();
    let mut block = crate::app::paths::NamBlock::new(0);
    block.model = "/captures/20".to_owned();
    path.nam.push(block);
    path.next_nam_id = 1;
    path.cab_file = "/captures/20".to_owned();
    let nam = settle(&ctx, &mut app, |app, ui| app.sheet_body(ui, 280.0));
    let input = bounds(&nam, "Input");
    let model = bounds(&nam, "Model 20");
    assert!(
        model.y0 < input.y0 + 220.0,
        "the selected capture was left below the sheet ({}, settings at {})",
        model.y0,
        input.y0
    );

    app.open_sheet(Sheet::Cab { path: 0 });
    seed_cabs(&mut app, 30);
    app.cab_catalog.entries[20].path = "/captures/20".to_owned();
    let cab = settle(&ctx, &mut app, |app, ui| app.sheet_body(ui, 280.0));
    let level = bounds(&cab, "Level");
    let impulse = bounds(&cab, "Cab 20");
    assert!(
        impulse.y0 < level.y0 + 220.0,
        "the cab list reused the NAM scroll ({}, settings at {})",
        impulse.y0,
        level.y0
    );
}

#[test]
fn choosing_a_row_selects_that_file() {
    let ctx = styled();
    let mut app = blank_app();
    seed_models(&mut app, 4);
    let tree = tree_of(&ctx, &mut app, |app, ui| app.nam_sheet(ui, 0, 0));
    let at = centre(bounds(&tree, "Model 2"));
    click(&ctx, &mut app, at, |app, ui| app.nam_sheet(ui, 0, 0));
    assert_eq!(app.paths[0].as_ref().unwrap().nam[0].model, "/models/2.nam");

    let mut app = blank_app();
    seed_cabs(&mut app, 4);
    let tree = tree_of(&ctx, &mut app, |app, ui| app.cab_sheet(ui, 0));
    let at = centre(bounds(&tree, "Cab 1"));
    click(&ctx, &mut app, at, |app, ui| app.cab_sheet(ui, 0));
    assert_eq!(app.paths[0].as_ref().unwrap().cab_file, "/impulses/1.wav");
}
