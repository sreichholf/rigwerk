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
