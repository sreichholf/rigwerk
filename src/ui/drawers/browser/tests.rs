use std::path::PathBuf;

use eframe::egui::{self, accesskit::Toggled};

use crate::app::blank_app;
use crate::app::plugins::PluginCandidate;
use crate::audio::{PluginFormat, PluginPref};
use crate::ui::widgets::{accesskit_frame, node_named, run_frame, test_input};

fn candidate(name: &str, category: &str) -> PluginCandidate {
    PluginCandidate {
        path: PathBuf::from(format!("{name}.vst3")),
        format: PluginFormat::Vst3,
        class_id: None,
        name: name.to_owned(),
        vendor: "Rig".to_owned(),
        category: category.to_owned(),
        channels: Some(2),
        scan_error: None,
    }
}

fn pref(name: &str) -> PluginPref {
    PluginPref {
        path: format!("{name}.vst3"),
        format: PluginFormat::Vst3,
        class_id: None,
        name: name.to_owned(),
        vendor: String::new(),
        category: String::new(),
        bypass: false,
        mix: 1.0,
        params: Vec::new(),
        state: None,
    }
}

#[test]
fn the_browser_names_the_path_the_row_and_its_controls() {
    let mut app = blank_app();
    app.paths[0].as_mut().unwrap().name = "Room".to_owned();
    app.engine.restore_plugins(0, vec![pref("Limiter Stereo")]);
    app.plugin_browser.library = vec![
        candidate("Amp", "Dynamics"),
        candidate("Rover", "Reverb · Hall"),
    ];
    app.open_effect_browser(0, 1);
    let tree = accesskit_frame(|ui| app.effect_browser(ui));
    let heading = node_named(&tree, "Add to Room");
    assert_eq!(heading.role(), egui::Role::Heading);
    assert_eq!(heading.value(), Some("after Limiter Stereo"));
    assert_eq!(node_named(&tree, "Search").role(), egui::Role::TextInput);
    assert_eq!(node_named(&tree, "Favourites").role(), egui::Role::Button);
    assert_eq!(node_named(&tree, "Recent").role(), egui::Role::Button);
    assert_eq!(node_named(&tree, "Reverb").role(), egui::Role::Button);
    assert_eq!(node_named(&tree, "VST3").role(), egui::Role::Button);
    assert_eq!(node_named(&tree, "CLAP").role(), egui::Role::Button);
    assert_eq!(node_named(&tree, "LV2").role(), egui::Role::Button);
    assert_eq!(node_named(&tree, "Amp").role(), egui::Role::Button);
    assert_eq!(node_named(&tree, "Amp").toggled(), Some(Toggled::True));
    assert_eq!(
        node_named(&tree, "Favourite Amp").toggled(),
        Some(Toggled::False)
    );
    assert_eq!(node_named(&tree, "Rescan").role(), egui::Role::Button);
    assert_eq!(
        node_named(&tree, "Add from path").role(),
        egui::Role::TextInput
    );
    let _ = node_named(&tree, "change");
}

#[test]
fn arrow_down_moves_the_chosen_effect() {
    let mut app = blank_app();
    app.plugin_browser.library = vec![candidate("Amp", "Dynamics"), candidate("Rover", "Reverb")];
    app.open_effect_browser(0, 0);
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let _ = run_frame(&ctx, test_input(None), |ui| app.effect_browser(ui));
    let output = run_frame(&ctx, test_input(Some(egui::Key::ArrowDown)), |ui| {
        app.effect_browser(ui)
    });
    let tree = output.platform_output.accesskit_update.expect("tree");
    assert_eq!(node_named(&tree, "Rover").toggled(), Some(Toggled::True));
    assert_eq!(node_named(&tree, "Amp").toggled(), Some(Toggled::False));
}

#[test]
fn a_row_names_the_paths_that_already_use_it() {
    let mut app = blank_app();
    app.engine.restore_plugins(0, vec![pref("Room")]);
    let mut room = candidate("Room", "Reverb");
    room.path = PathBuf::from("Room.vst3");
    app.plugin_browser.library = vec![room];
    app.open_effect_browser(0, 1);
    let tree = accesskit_frame(|ui| app.effect_browser(ui));
    let value = node_named(&tree, "Room").value().unwrap_or("");
    assert!(value.contains("in Main"), "{value}");
}

#[test]
fn a_row_reports_a_scan_error() {
    let mut row = candidate("Delay", "Echo");
    row.scan_error = Some("Could not read the bundle.".to_owned());
    let mut app = blank_app();
    app.plugin_browser.library = vec![row];
    app.open_effect_browser(0, 0);
    let tree = accesskit_frame(|ui| app.effect_browser(ui));
    let value = node_named(&tree, "Delay").value().unwrap_or("");
    assert!(value.contains("Could not read the bundle."), "{value}");
}

#[test]
fn the_list_only_builds_the_rows_on_screen() {
    let rows = (0..40)
        .map(|index| candidate(&format!("Effect {index:02}"), "Delay"))
        .collect();
    let mut app = blank_app();
    app.plugin_browser.library = rows;
    app.open_effect_browser(0, 0);
    let tree = accesskit_frame(|ui| app.effect_browser(ui));
    let shown = tree
        .nodes
        .iter()
        .filter(|(_, node)| {
            node.label()
                .is_some_and(|label| label.starts_with("Effect "))
        })
        .count();
    assert!(shown > 0 && shown < 20, "{shown} rows were built");
}

#[test]
fn enter_adds_the_chosen_effect_and_leaves_the_browser_open() {
    let _main = crate::audio::claim_main_thread();
    let _loader = crate::audio::set_test_loader(|_pref, _rate| {
        Ok((
            crate::audio::clap::tests::fake_plugin_with_latency_for_test(0),
            false,
        ))
    });
    let mut hall = candidate("Hall", "Reverb");
    hall.path = PathBuf::from(format!("{}hall", crate::audio::TEST_PATH));
    let mut room = candidate("Room", "Reverb");
    room.path = PathBuf::from(format!("{}room", crate::audio::TEST_PATH));
    let mut app = blank_app();
    app.plugin_browser.library = vec![hall, room];
    app.open_effect_browser(0, 0);
    let ctx = egui::Context::default();
    let _ = run_frame(&ctx, test_input(None), |ui| app.effect_browser(ui));
    let _ = run_frame(&ctx, test_input(Some(egui::Key::ArrowDown)), |ui| {
        app.effect_browser(ui);
    });
    let _ = run_frame(&ctx, test_input(Some(egui::Key::Enter)), |ui| {
        app.effect_browser(ui);
    });
    let names: Vec<_> = app
        .engine
        .plugin_views(0)
        .iter()
        .map(|view| view.name.clone())
        .collect();
    assert_eq!(names, ["Room"]);
    assert!(app.plugin_browser.open);
    assert_eq!(app.plugin_browser.position, 1);
    assert!(app.plugin_editor.is_none());
}

#[test]
fn shift_enter_adds_and_opens_the_controls() {
    let _main = crate::audio::claim_main_thread();
    let _loader = crate::audio::set_test_loader(|_pref, _rate| {
        Ok((
            crate::audio::clap::tests::fake_plugin_with_latency_for_test(0),
            false,
        ))
    });
    let mut room = candidate("Room", "Reverb");
    room.path = PathBuf::from(format!("{}room", crate::audio::TEST_PATH));
    let mut app = blank_app();
    app.plugin_browser.library = vec![room];
    app.open_effect_browser(0, 0);
    let ctx = egui::Context::default();
    let _ = run_frame(&ctx, test_input(None), |ui| app.effect_browser(ui));
    let mut input = test_input(None);
    input.events.push(egui::Event::Key {
        key: egui::Key::Enter,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::SHIFT,
    });
    let _ = run_frame(&ctx, input, |ui| app.effect_browser(ui));
    assert_eq!(app.engine.plugin_views(0).len(), 1);
    // Opening the effect's controls is a sheet, so the browser drawer closes.
    assert!(!app.plugin_browser.open);
    assert!(app.plugin_editor.is_some());
}

#[test]
fn the_star_toggles_a_favourite() {
    let mut app = blank_app();
    app.plugin_browser.library = vec![candidate("Room", "Reverb")];
    app.open_effect_browser(0, 0);
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let output = run_frame(&ctx, test_input(None), |ui| app.effect_browser(ui));
    let tree = output.platform_output.accesskit_update.expect("tree");
    let node = node_named(&tree, "Favourite Room");
    let bounds = node.bounds().expect("bounds");
    let at = egui::pos2(
        ((bounds.x0 + bounds.x1) / 2.0) as f32,
        ((bounds.y0 + bounds.y1) / 2.0) as f32,
    );
    let mut click = test_input(None);
    click.events.push(egui::Event::PointerMoved(at));
    for pressed in [true, false] {
        click.events.push(egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        });
    }
    let _ = run_frame(&ctx, click, |ui| app.effect_browser(ui));
    assert_eq!(app.plugin_browser.favourites.len(), 1);
    assert!(app.engine.plugin_views(0).is_empty());
}

#[test]
fn change_moves_where_the_next_effect_is_added() {
    let mut app = blank_app();
    app.engine.restore_plugins(0, vec![pref("Limiter Stereo")]);
    app.open_effect_browser(0, 1);
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let output = run_frame(&ctx, test_input(None), |ui| app.effect_browser(ui));
    let tree = output.platform_output.accesskit_update.expect("tree");
    let node = node_named(&tree, "change");
    let bounds = node.bounds().expect("bounds");
    let at = egui::pos2(
        ((bounds.x0 + bounds.x1) / 2.0) as f32,
        ((bounds.y0 + bounds.y1) / 2.0) as f32,
    );
    let mut click = test_input(None);
    click.events.push(egui::Event::PointerMoved(at));
    for pressed in [true, false] {
        click.events.push(egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        });
    }
    let _ = run_frame(&ctx, click, |ui| app.effect_browser(ui));
    assert_eq!(app.plugin_browser.position, 0);
}
