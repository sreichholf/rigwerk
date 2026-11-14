use super::{
    chain_device_row, device_level_text, device_row_value, device_state, gain_reduction_text,
    list_device_row, title_name_width,
};
use crate::app::output_devices::OutputDeviceRow;
use crate::audio::{EngineState, OutputDeviceSettings};
use crate::ui::widgets::fit_text;
use crate::ui::widgets::{accesskit_frame, node_named};
use eframe::egui::{self, accesskit::Toggled};

fn clock_row(level_db: f32, mute: bool) -> OutputDeviceRow {
    OutputDeviceRow {
        name: "AKG N9 Hybrid".to_owned(),
        backend: "PipeWire".to_owned(),
        channels: 2,
        click_channels: 2,
        clock: true,
        virtual_out: false,
        follower_index: None,
        missing: false,
        publish: false,
        reduction_db: 0.0,
        settings: OutputDeviceSettings {
            level_db,
            mute,
            ..OutputDeviceSettings::default()
        },
    }
}

#[test]
fn device_level_reads_muted_or_decibels() {
    assert_eq!(
        device_level_text(&clock_row(-1.5, false).settings),
        "-1.5 dB"
    );
    assert_eq!(
        device_level_text(&clock_row(0.0, false).settings),
        "+0.0 dB"
    );
    assert_eq!(device_level_text(&clock_row(-1.5, true).settings), "muted");
}

#[test]
fn device_row_names_the_reduction_when_the_ceiling_holds() {
    let mut row = clock_row(0.0, false);
    assert_eq!(gain_reduction_text(0.0), None);
    assert_eq!(gain_reduction_text(0.4), None);
    row.reduction_db = 3.2;
    assert_eq!(
        gain_reduction_text(row.reduction_db).as_deref(),
        Some("3.2 dB")
    );
    assert_eq!(
        device_row_value(&row, EngineState::Running),
        "clock device · +0.0 dB · 3.2 dB · running"
    );
}

#[test]
fn device_state_follows_the_bridge() {
    let row = clock_row(0.0, false);
    assert_eq!(device_state(&row, EngineState::Running).0, "running");
    assert_eq!(device_state(&row, EngineState::Idle).0, "stopped");
    assert_eq!(device_state(&row, EngineState::Error).0, "error");
    assert_eq!(
        device_row_value(&clock_row(-6.0, false), EngineState::Running),
        "clock device · -6.0 dB · running"
    );
}

#[test]
fn chain_device_row_exposes_name_selection_and_mute() {
    let row = clock_row(-1.5, true);
    let tree = accesskit_frame(|ui| {
        let (clicked, mute, remove) = chain_device_row(ui, &row, true, EngineState::Idle);
        assert!(!clicked && !mute && !remove);
    });
    let node = node_named(&tree, "AKG N9 Hybrid");
    assert_eq!(node.role(), egui::Role::Button);
    assert_eq!(node.toggled(), Some(Toggled::True));
    assert_eq!(node.value(), Some("clock device · muted · stopped"));
    let mute = node_named(&tree, "Mute AKG N9 Hybrid");
    assert_eq!(mute.role(), egui::Role::Button);
    assert_eq!(mute.toggled(), Some(Toggled::True));
}

#[test]
fn inspector_list_row_exposes_name_and_level() {
    let row = clock_row(-1.5, false);
    let tree = accesskit_frame(|ui| {
        assert!(!list_device_row(ui, &row, false, EngineState::Running));
    });
    let node = node_named(&tree, "AKG N9 Hybrid in output devices");
    assert_eq!(node.role(), egui::Role::Button);
    assert_eq!(node.toggled(), Some(Toggled::False));
    assert_eq!(node.value(), Some("clock device · -1.5 dB · running"));
}

#[test]
fn the_title_name_leaves_room_for_tag_and_mute() {
    assert_eq!(title_name_width(320.0, 50.0, 60.0, 8.0), 194.0);
    assert_eq!(title_name_width(100.0, 50.0, 60.0, 8.0), 40.0);
}

#[test]
fn long_names_are_shortened_to_the_width() {
    let font = egui::FontId::proportional(16.0);
    let _ = accesskit_frame(|ui| {
        assert_eq!(fit_text(ui, "Out", &font, 500.0), "Out");
        let long = "Focusrite Scarlett 18i20 3rd Gen USB Analog Surround 7.1";
        let short = fit_text(ui, long, &font, 120.0);
        assert!(short.ends_with('…'), "{short}");
        let width = ui
            .painter()
            .layout_no_wrap(short.clone(), font.clone(), egui::Color32::WHITE)
            .size()
            .x;
        assert!(width <= 120.0, "{short} is {width} px");
    });
}
