use crate::app::blank_app;
use crate::app::screen::Sheet;
use crate::app::settings::ExtraOutputPref;
use crate::audio::DeviceItem;
use crate::ui::widgets::{accesskit_frame, node_named};
use eframe::egui::accesskit::Toggled;

fn four_channel_extra() -> crate::app::RigWerkApp {
    let mut app = blank_app();
    app.inventory.outputs.push(DeviceItem {
        id: "hw:Card".to_owned(),
        name: "Card".to_owned(),
        backend: "ALSA".to_owned(),
        channels: 4,
        demo: false,
        preferred: false,
        asio_clsid: None,
    });
    app.extra_outputs.push(ExtraOutputPref {
        id: "hw:Card".to_owned(),
        backend: "ALSA".to_owned(),
        name: "Card".to_owned(),
        ..ExtraOutputPref::default()
    });
    app.open_sheet(Sheet::Output { output: 1 });
    app
}

#[test]
fn a_map_cell_names_its_path_side_and_channel() {
    let mut app = four_channel_extra();
    let tree = accesskit_frame(|ui| app.sheet_body(ui, 600.0));
    let left = node_named(&tree, "Main L to out 1");
    assert_eq!(left.role(), eframe::egui::Role::CheckBox);
    assert_eq!(
        left.toggled(),
        Some(Toggled::True),
        "the default map plays L on Out 1"
    );
    let right = node_named(&tree, "Main R to out 2");
    assert_eq!(right.toggled(), Some(Toggled::True));
    let unused = node_named(&tree, "Main L to out 4");
    assert_eq!(unused.toggled(), Some(Toggled::False));
}

#[test]
fn the_click_row_and_presets_are_named() {
    let mut app = four_channel_extra();
    let tree = accesskit_frame(|ui| app.sheet_body(ui, 600.0));
    let click = node_named(&tree, "Metronome on out 3");
    assert_eq!(click.role(), eframe::egui::Role::CheckBox);
    assert_eq!(click.toggled(), Some(Toggled::False));
    let _ = node_named(&tree, "Mono to all");
    let _ = node_named(&tree, "Metronome on 3/4");
}

#[test]
fn the_loudness_controls_are_named() {
    let mut app = four_channel_extra();
    let tree = accesskit_frame(|ui| app.sheet_body(ui, 600.0));
    let target = node_named(&tree, "Loudness target");
    assert_eq!(target.role(), eframe::egui::Role::Slider);
    let trim = tree
        .nodes
        .iter()
        .find(|(_, node)| {
            node.label() == Some("Output trim") && node.role() == eframe::egui::Role::Slider
        })
        .map(|(_, node)| node)
        .expect("the Output trim slider");
    assert_eq!(trim.role(), eframe::egui::Role::Slider);
    let _ = node_named(&tree, "Learn");
    let _ = node_named(&tree, "Reset");
}

#[test]
fn a_map_edit_writes_the_path_slot_and_keeps_the_other_paths() {
    let mut app = four_channel_extra();
    let second = app.duplicate_path(0).unwrap();
    let other = app.device_path_map(1, second).unwrap();
    app.apply_map_edit(
        1,
        crate::app::output_devices::MapEdit::Cell {
            slot: 0,
            side: crate::audio::MixSide::L,
            channel: 3,
            on: true,
        },
    );
    let first = app.device_path_map(1, 0).unwrap();
    assert!(first.map.routes(crate::audio::MixSide::L, 3));
    assert_eq!(app.device_path_map(1, second), Some(other));
}
