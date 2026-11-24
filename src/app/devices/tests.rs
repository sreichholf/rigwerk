use super::{
    extra_output_index, restore_device, restore_extra_output, restore_input, DeviceRestore,
    InputGroup,
};
use crate::app::settings::{load_preferences, ExtraOutputPref};
use crate::audio::{DeviceItem, VIRTUAL_OUTPUT_NAME};

fn item(id: &str, name: &str, backend: &str) -> DeviceItem {
    DeviceItem {
        id: id.to_owned(),
        name: name.to_owned(),
        backend: backend.to_owned(),
        channels: 2,
        demo: false,
        preferred: false,
        asio_clsid: None,
    }
}

#[test]
fn restore_prefers_id_over_a_renamed_display() {
    let outputs = vec![
        item("wasapi:{spk}", "Speakers (Realtek)", "WASAPI"),
        item("asio-out:abc", "Speakers", "ASIO"),
    ];
    let hit = restore_device(&outputs, "wasapi:{spk}", "WASAPI", "Speakers");
    assert_eq!(hit, DeviceRestore::ById(0));
    assert!(!hit.matched_by_name());
    assert!(!hit.saved_identity_unconfirmed());

    // The other backend that still has the old name stays selectable by its own id.
    let asio = restore_device(&outputs, "asio-out:abc", "ASIO", "Speakers");
    assert_eq!(asio, DeviceRestore::ById(1));

    // Session input: the saved id is the system device, even though the
    // low-latency list is searched first and still shows the old name.
    let low_latency = vec![item("asio:abc", "Focusrite", "ASIO")];
    let system = vec![item("wasapi:{mic}", "Focusrite USB", "WASAPI")];
    let input = restore_input(&low_latency, &system, "wasapi:{mic}", "WASAPI", "Focusrite")
        .expect("id match");
    assert_eq!(input.group, InputGroup::System);
    assert_eq!(input.restore, DeviceRestore::ById(0));
    assert!(!input.restore.matched_by_name());

    // The id is gone: the name fallback is visible, and it must not be
    // treated as a confirmed id (a save keeps the stored one).
    let fallback = restore_device(&outputs, "wasapi:{gone}", "WASAPI", "Speakers");
    assert_eq!(
        fallback,
        DeviceRestore::ByName {
            index: 1,
            saved_id_missing: true,
        }
    );
    assert!(fallback.matched_by_name());
    assert!(fallback.saved_identity_unconfirmed());
    let note = super::device_restore_note("Output", "Speakers", fallback).expect("logged");
    assert!(note.contains("matched by name"), "{note}");
    assert!(note.contains("saved device id was not found"), "{note}");
}

#[test]
fn preferences_without_device_ids_load_and_match_by_name() {
    let raw = r#"(
        asio_name:Some("Focusrite USB"),
        input_name:Some("Analog In"),
        output_name:Some("Speakers"),
        sessions:[(name:"Old",input:"Analog In",output:"Speakers",rate:48000)]
    )"#;
    let loaded = load_preferences(Some(raw), "rigwerk-preferences-backup-1");
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    let preferences = loaded.preferences.expect("old file loads");
    assert_eq!(preferences.asio_id, "");
    assert_eq!(preferences.asio_backend, "");
    assert_eq!(preferences.input_id, "");
    assert_eq!(preferences.input_backend, "");
    assert_eq!(preferences.output_id, "");
    assert_eq!(preferences.output_backend, "");
    assert_eq!(preferences.sessions[0].input_id, "");
    assert_eq!(preferences.sessions[0].input_backend, "");
    assert_eq!(preferences.sessions[0].output_id, "");
    assert_eq!(preferences.sessions[0].output_backend, "");

    let asio = vec![
        item("asio:other", "Other", "ASIO"),
        item("asio:focusrite", "Focusrite USB", "ASIO"),
    ];
    let inputs = vec![item("wasapi:{mic}", "Analog In", "WASAPI")];
    let outputs = vec![
        item("wasapi:{spk}", "Speakers", "WASAPI"),
        item("asio-out:abc", "Speakers", "ASIO"),
    ];
    let asio_hit = restore_device(
        &asio,
        &preferences.asio_id,
        &preferences.asio_backend,
        preferences.asio_name.as_deref().unwrap_or(""),
    );
    assert_eq!(
        asio_hit,
        DeviceRestore::ByName {
            index: 1,
            saved_id_missing: false,
        }
    );
    assert!(asio_hit.matched_by_name());
    assert!(!asio_hit.saved_identity_unconfirmed());

    let input_hit = restore_device(
        &inputs,
        &preferences.input_id,
        &preferences.input_backend,
        preferences.input_name.as_deref().unwrap_or(""),
    );
    assert_eq!(input_hit.index(), Some(0));
    assert!(input_hit.matched_by_name());

    // No saved backend: the first display-name row wins, and it is reported as a name match.
    let output_hit = restore_device(
        &outputs,
        &preferences.output_id,
        &preferences.output_backend,
        preferences.output_name.as_deref().unwrap_or(""),
    );
    assert_eq!(output_hit.index(), Some(0));
    assert!(output_hit.matched_by_name());
    assert!(!output_hit.saved_identity_unconfirmed());

    let preset = &preferences.sessions[0];
    let preset_input = restore_input(
        &asio,
        &inputs,
        &preset.input_id,
        &preset.input_backend,
        &preset.input,
    )
    .expect("name match");
    assert_eq!(preset_input.group, InputGroup::System);
    assert!(preset_input.restore.matched_by_name());
    let preset_output = restore_device(
        &outputs,
        &preset.output_id,
        &preset.output_backend,
        &preset.output,
    );
    assert_eq!(preset_output.index(), Some(0));
    assert!(preset_output.matched_by_name());
}

fn extra(id: &str, backend: &str, name: &str) -> ExtraOutputPref {
    ExtraOutputPref {
        id: id.to_owned(),
        backend: backend.to_owned(),
        name: name.to_owned(),
        ..ExtraOutputPref::default()
    }
}

#[test]
fn an_extra_output_id_match_wins_over_a_same_name_row() {
    let outputs = vec![
        item("asio-out:abc", "Speakers", "ASIO"),
        item("wasapi:{spk}", "Studio", "WASAPI"),
    ];
    let mut saved = extra("wasapi:{spk}", "WASAPI", "Speakers");
    let restore = restore_extra_output(&outputs, &mut saved);
    assert_eq!(restore, DeviceRestore::ById(1));
    assert_eq!(extra_output_index(&outputs, &saved), Some(1));
    assert_eq!(saved.id, "wasapi:{spk}");
    assert_eq!(saved.backend, "WASAPI");
    assert_eq!(saved.name, "Studio", "an id match refreshes the label");
}

#[test]
fn a_reenumerated_extra_output_id_is_taken_from_the_name_match() {
    let outputs = vec![item("wasapi:{new}", "Speakers", "WASAPI")];
    let mut saved = extra("wasapi:{old}", "WASAPI", "Speakers");
    let restore = restore_extra_output(&outputs, &mut saved);
    assert!(restore.matched_by_name());
    assert_eq!(restore.index(), Some(0));
    assert_eq!(saved.id, "wasapi:{new}");
    assert_eq!(saved.backend, "WASAPI");
}

#[test]
fn a_same_name_extra_output_on_another_backend_follows_restore_device() {
    let outputs = vec![
        item("wasapi:{spk}", "Speakers", "WASAPI"),
        item("alsa:hw:Other", "Speakers (extra)", "ALSA"),
    ];
    let mut saved = extra("alsa:hw:Gone", "ALSA", "Speakers");
    let decided = restore_device(&outputs, &saved.id, &saved.backend, &saved.name);
    let restore = restore_extra_output(&outputs, &mut saved);
    assert_eq!(restore, decided);
    assert_eq!(
        decided,
        DeviceRestore::ByName {
            index: 0,
            saved_id_missing: true,
        }
    );
    assert_eq!(saved.id, "wasapi:{spk}");
    assert_eq!(saved.backend, "WASAPI");
}

#[test]
fn a_missing_extra_output_keeps_its_saved_identity() {
    let outputs = vec![item("wasapi:{spk}", "Speakers", "WASAPI")];
    let mut saved = extra("asio:{gone}", "ASIO", "Guitar out");
    let restore = restore_extra_output(&outputs, &mut saved);
    assert_eq!(restore, DeviceRestore::Missing);
    assert_eq!(extra_output_index(&outputs, &saved), None);
    assert_eq!(saved.id, "asio:{gone}");
    assert_eq!(saved.backend, "ASIO");
    assert_eq!(saved.name, "Guitar out");
}

#[test]
fn restore_leaves_a_virtual_extra_output_untouched() {
    let outputs = vec![item("wasapi:{spk}", VIRTUAL_OUTPUT_NAME, "WASAPI")];
    let mut saved = extra("wasapi:{gone}", "WASAPI", VIRTUAL_OUTPUT_NAME);
    saved.virtual_output = true;
    let restore = restore_extra_output(&outputs, &mut saved);
    assert!(restore.matched_by_name());
    assert_eq!(saved.id, "wasapi:{gone}");
    assert_eq!(saved.backend, "WASAPI");
    assert_eq!(saved.name, VIRTUAL_OUTPUT_NAME);
}

/// The placeholder inventory is painted before the first scan, and its demo
/// row is named "System speakers".  A saved extra output with that name must
/// not adopt `demo:output`, and the preview row is not its device.
#[test]
fn a_placeholder_output_never_adopts_a_saved_extra_output() {
    let outputs = crate::audio::placeholder_inventory().outputs;
    let mut saved = extra("alsa:hw:Quad", "ALSA", "System speakers");
    let restore = restore_extra_output(&outputs, &mut saved);
    assert!(restore.matched_by_name(), "{restore:?}");
    assert_eq!(saved.id, "alsa:hw:Quad");
    assert_eq!(saved.backend, "ALSA");
    assert_eq!(saved.name, "System speakers");
    assert_eq!(extra_output_index(&outputs, &saved), None);
}
