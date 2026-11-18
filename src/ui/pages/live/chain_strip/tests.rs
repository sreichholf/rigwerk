use super::{device_level_line, device_levels_value, output_summary, path_count_label};
use crate::app::blank_app;
use crate::app::layouts::PathLayout;
use crate::app::output_devices::DeviceLevel;
use crate::app::settings::ExtraOutputPref;
use crate::ui::widgets::{accesskit_frame, node_named};
use eframe::egui;

fn level(name: &str, peak: f32, mute: bool) -> DeviceLevel {
    DeviceLevel {
        name: name.to_owned(),
        meter: peak,
        peak,
        mute,
    }
}

#[test]
fn the_strip_counts_paths_and_shows_the_alignment() {
    assert_eq!(path_count_label("Main", 3), "Main · 3 paths");
    assert_eq!(output_summary("+0.0 dB", 2.0), "+0.0 dB · +2.0 ms");
    let mut app = blank_app();
    assert_eq!(app.added_alignment_ms(), 0.0);
    assert!(app.apply_layout(PathLayout::Blend));
    let second = app.path_slots()[1];
    app.engine.screenshot_path_latency(second, 96);
    assert!((app.added_alignment_ms() - 2.0).abs() < 1e-3);
    app.set_device_align(0, false);
    assert_eq!(app.added_alignment_ms(), 0.0, "nothing waits");
    app.engine.screenshot_path_latency(second, 0);
}

#[test]
fn each_device_reads_its_peak_or_muted() {
    assert_eq!(
        device_level_line(&level("AKG", 0.5, false)),
        "AKG -6.0 dBFS"
    );
    assert_eq!(device_level_line(&level("AKG", 0.0, false)), "AKG silent");
    assert_eq!(device_level_line(&level("AKG", 0.5, true)), "AKG muted");
    assert_eq!(
        device_levels_value(&[level("AKG", 1.0, false), level("Out", 0.2, true)]),
        "AKG 0.0 dBFS; Out muted"
    );
}

#[test]
fn the_output_chip_names_each_device_level() {
    let mut app = blank_app();
    let mut out = ExtraOutputPref::virtual_output();
    out.settings.mute = true;
    let out_name = out.name.clone();
    app.extra_outputs.push(out);
    app.peak_out_channels[0] = 0.5;
    let clock = app.output_device_rows()[0].name.clone();
    let tree = accesskit_frame(|ui| app.chain_strip(ui, 1200.0));
    let node = node_named(&tree, "OUTPUT");
    assert_eq!(node.role(), egui::Role::Button);
    let value = node.value().expect("value");
    assert!(
        value.ends_with(&format!("; {clock} -6.0 dBFS; {out_name} muted")),
        "{value}"
    );
}
