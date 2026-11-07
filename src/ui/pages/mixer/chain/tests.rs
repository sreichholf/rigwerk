use super::{live_chain_entries, output_summary, path_summary, ChainSelection};
use crate::audio::{EqSettings, EqSlot, GateSettings, GateSlot};

#[test]
fn eq_edit_round_trips_through_selection() {
    assert_eq!(
        ChainSelection::from_eq_edit(EqSlot::Pre).eq_slot(),
        Some(EqSlot::Pre)
    );
    assert_eq!(
        ChainSelection::from_eq_edit(EqSlot::Post).eq_slot(),
        Some(EqSlot::Post)
    );
}

#[test]
fn plugin_selection_clamps_after_remove() {
    assert_eq!(
        ChainSelection::Plugin(3).clamp(2),
        ChainSelection::Plugin(1)
    );
    assert_eq!(ChainSelection::Plugin(0).clamp(0), ChainSelection::Plugins);
}

#[test]
fn selection_follows_its_insert_after_remove() {
    // Removing an earlier insert keeps the same insert selected.
    assert_eq!(
        ChainSelection::Plugin(2).after_removal(0, 2),
        ChainSelection::Plugin(1)
    );
    // Removing a later one leaves the selection alone.
    assert_eq!(
        ChainSelection::Plugin(0).after_removal(1, 2),
        ChainSelection::Plugin(0)
    );
    // Removing the selected last insert selects the new last one.
    assert_eq!(
        ChainSelection::Plugin(2).after_removal(2, 2),
        ChainSelection::Plugin(1)
    );
    assert_eq!(
        ChainSelection::Plugin(0).after_removal(0, 0),
        ChainSelection::Plugins
    );
    assert_eq!(
        ChainSelection::Gate.after_removal(0, 0),
        ChainSelection::Gate
    );
    // Removing the selected insert selects the one that slid into place.
    assert_eq!(
        ChainSelection::Plugin(1).after_removal(1, 2),
        ChainSelection::Plugin(1)
    );
    assert_eq!(
        ChainSelection::Plugins.after_removal(0, 1),
        ChainSelection::Plugins
    );
}

#[test]
fn output_device_selection_falls_back_to_the_master() {
    assert_eq!(
        ChainSelection::OutputDevice(0).clamp_output(1),
        ChainSelection::OutputDevice(0)
    );
    assert_eq!(
        ChainSelection::OutputDevice(1).clamp_output(1),
        ChainSelection::Output
    );
    assert_eq!(ChainSelection::Gate.clamp_output(0), ChainSelection::Gate);
    // The insert clamp leaves output devices alone.
    assert_eq!(
        ChainSelection::OutputDevice(0).clamp(0),
        ChainSelection::OutputDevice(0)
    );
    assert_eq!(ChainSelection::OutputDevice(0).plugin_insert_at(2), None);
    assert_eq!(ChainSelection::OutputDevice(0).eq_slot(), None);
}

#[test]
fn stage_keys_name_the_mixer_stages() {
    assert_eq!(
        ChainSelection::from_key("output"),
        Some(ChainSelection::Output)
    );
    assert_eq!(
        ChainSelection::from_key("device"),
        Some(ChainSelection::OutputDevice(0))
    );
    assert_eq!(
        ChainSelection::from_key("post_eq"),
        Some(ChainSelection::PostEq)
    );
    assert_eq!(ChainSelection::from_key("path"), Some(ChainSelection::Path));
    assert_eq!(ChainSelection::from_key("clock"), None);
    assert_eq!(ChainSelection::from_key("nope"), None);
}

#[test]
fn path_stage_needs_several_paths() {
    assert_eq!(ChainSelection::Path.clamp_path(1), ChainSelection::PostEq);
    assert_eq!(ChainSelection::Path.clamp_path(2), ChainSelection::Path);
    assert_eq!(ChainSelection::Gate.clamp_path(1), ChainSelection::Gate);
}

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
fn insert_point_follows_selected_plugin() {
    assert_eq!(ChainSelection::Plugin(1).plugin_insert_at(3), Some(2));
    assert_eq!(ChainSelection::Plugins.plugin_insert_at(3), None);
    assert_eq!(ChainSelection::PreEq.plugin_insert_at(3), None);
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
