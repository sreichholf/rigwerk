use super::{restore_device, restore_input, DeviceRestore, InputGroup};
use crate::app::settings::load_preferences;
use crate::audio::DeviceItem;

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

    // Preset input: the saved id is the system device, even though the
    // low-latency list is searched first and still shows the old name.
    let low_latency = vec![item("asio:abc", "Focusrite", "ASIO")];
    let system = vec![item("wasapi:{mic}", "Focusrite USB", "WASAPI")];
    let input = restore_input(&low_latency, &system, "wasapi:{mic}", "WASAPI", "Focusrite")
        .expect("id match");
    assert_eq!(input.group, InputGroup::System);
    assert_eq!(input.restore, DeviceRestore::ById(0));
    assert!(!input.restore.matched_by_name());

    // The id is gone: the name fallback is visible, and it must not be
    // treated as a confirmed id (H15 uses that to avoid overwriting).
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
