use super::{apply_chain_bypass, live_chain_entries, output_summary, path_summary};
use crate::audio::PluginView;
use crate::audio::{EqSettings, GateSettings, GateSlot};

#[test]
fn output_summary_shows_master_level_and_device_count() {
    assert_eq!(output_summary(-1.5, false, 1), "-1.5 dB · 1 device");
    assert_eq!(output_summary(0.0, false, 3), "+0.0 dB · 3 devices");
    assert_eq!(output_summary(-1.5, true, 1), "muted · 1 device");
}

#[test]
fn path_summary_shows_level_mute_and_solo() {
    assert_eq!(path_summary(-3.0, false, false), "-3.0 dB");
    assert_eq!(path_summary(-3.0, true, false), "muted");
    assert_eq!(path_summary(0.0, false, true), "+0.0 dB · solo");
}

#[test]
fn live_strip_puts_post_gate_after_plugins() {
    let gate = GateSettings {
        slot: GateSlot::Post,
        enabled: true,
        ..GateSettings::default()
    };
    let entries = live_chain_entries(
        0.0,
        &EqSettings::default(),
        &[],
        &gate,
        &EqSettings::default(),
        -1.5,
        false,
    );
    let labels: Vec<_> = entries.iter().map(|e| e.label).collect();
    assert_eq!(
        labels,
        ["INPUT", "PRE EQ", "PLUGINS", "GATE", "POST EQ", "OUTPUT"]
    );
}

#[test]
fn live_strip_puts_pre_gate_before_plugins() {
    let gate = GateSettings {
        slot: GateSlot::Pre,
        enabled: true,
        ..GateSettings::default()
    };
    let entries = live_chain_entries(
        0.0,
        &EqSettings::default(),
        &[],
        &gate,
        &EqSettings::default(),
        -1.5,
        false,
    );
    let labels: Vec<_> = entries.iter().map(|e| e.label).collect();
    assert_eq!(
        labels,
        ["INPUT", "PRE EQ", "GATE", "PLUGINS", "POST EQ", "OUTPUT"]
    );
}

#[test]
fn chain_bypass_makes_the_plugins_stage_read_bypassed() {
    let gate = GateSettings {
        slot: GateSlot::Post,
        enabled: true,
        ..GateSettings::default()
    };
    let views = [PluginView {
        name: "Amp".to_owned(),
        ..PluginView::default()
    }];
    let mut entries = live_chain_entries(
        0.0,
        &EqSettings::default(),
        &views,
        &gate,
        &EqSettings::default(),
        0.0,
        false,
    );
    let plugins = |entries: &[super::ChainEntry]| {
        entries
            .iter()
            .find(|entry| entry.label == "PLUGINS")
            .expect("plugins stage")
            .clone()
    };
    let off = plugins(&entries);
    assert_eq!(off.summary, "Amp");
    assert!(off.live);
    apply_chain_bypass(&mut entries, false);
    let still = plugins(&entries);
    assert_eq!(still.summary, "Amp");
    assert!(still.live);
    assert_eq!(still.color, off.color);
    apply_chain_bypass(&mut entries, true);
    let on = plugins(&entries);
    assert_eq!(on.summary, "bypassed");
    assert!(on.live);
    assert_eq!(on.color, off.color);
}
