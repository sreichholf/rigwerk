use super::library_row;
use crate::app::plugins::PluginCandidate;
use crate::audio::PluginFormat;
use crate::ui::widgets::{accesskit_frame, node_named};
use std::path::PathBuf;

#[test]
fn a_plugin_row_exposes_its_name_and_whether_it_can_be_added() {
    let candidate = PluginCandidate {
        path: PathBuf::from("reverb.vst3"),
        format: PluginFormat::Vst3,
        class_id: None,
        name: "Room".to_owned(),
        vendor: "Rig".to_owned(),
        category: "Reverb".to_owned(),
    };
    let tree = accesskit_frame(|ui| {
        assert!(!library_row(ui, &candidate, false, &[]));
    });
    let node = node_named(&tree, "Room");
    assert_eq!(node.role(), eframe::egui::Role::Button);
    assert_eq!(node.value(), Some("Add"));
}

#[test]
fn a_plugin_row_names_the_paths_that_hold_it() {
    let candidate = PluginCandidate {
        path: PathBuf::from("amp.clap"),
        format: PluginFormat::Clap,
        class_id: None,
        name: "Amp".to_owned(),
        vendor: String::new(),
        category: "Amp".to_owned(),
    };
    let holders = [(eframe::egui::Color32::RED, "Wet".to_owned())];
    let tree = accesskit_frame(|ui| {
        assert!(!library_row(ui, &candidate, true, &holders));
    });
    let node = node_named(&tree, "Amp");
    assert_eq!(node.value(), Some("In chain, on Wet"));
}
