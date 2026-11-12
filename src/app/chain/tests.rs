use super::ChainSelection;
use crate::audio::EqSlot;

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
fn insert_point_follows_selected_plugin() {
    assert_eq!(ChainSelection::Plugin(1).plugin_insert_at(3), Some(2));
    assert_eq!(ChainSelection::Plugins.plugin_insert_at(3), None);
    assert_eq!(ChainSelection::PreEq.plugin_insert_at(3), None);
}
