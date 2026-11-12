use crate::app::LogTone;
use std::collections::HashMap;

use eframe::egui;
use eframe::App;

use crate::ui::widgets::{run_frame, test_input};

use super::settings::FollowerPref;
use super::settings::{load_preferences, Preferences, Preset, PREFERENCES_KEY};
use super::startup::{restore_saved_devices, StartupGate};
use super::{blank_app, repaint_interval, ACTIVE_REPAINT, IDLE_REPAINT};
use crate::audio::{
    self, ChannelMap, DeviceItem, EngineState, Inventory, OutputDeviceSettings, UnusedChannels,
};

#[derive(Default)]
struct MemoryStorage(HashMap<String, String>);

impl eframe::Storage for MemoryStorage {
    fn get_string(&self, key: &str) -> Option<String> {
        self.0.get(key).cloned()
    }
    fn set_string(&mut self, key: &str, value: String) {
        self.0.insert(key.to_owned(), value);
    }
    fn remove_string(&mut self, key: &str) {
        self.0.remove(key);
    }
    fn flush(&mut self) {}
}

fn item(id: &str, name: &str, backend: &str, preferred: bool) -> DeviceItem {
    DeviceItem {
        id: id.to_owned(),
        name: name.to_owned(),
        backend: backend.to_owned(),
        channels: 2,
        demo: false,
        preferred,
        asio_clsid: None,
    }
}

fn install(app: &mut super::RigWerkApp, preferences: &Preferences) {
    let restored = restore_saved_devices(&app.inventory, preferences);
    app.asio_idx = restored.asio_idx;
    app.input_idx = restored.input_idx;
    app.output_idx = restored.output_idx;
    app.asio_held = restored.asio_held;
    app.input_held = restored.input_held;
    app.output_held = restored.output_held;
    app.use_asio_input = preferences.use_asio_input;
}

fn save_text(app: &mut super::RigWerkApp) -> (String, Preferences) {
    let mut storage = MemoryStorage::default();
    app.save(&mut storage);
    let raw = eframe::Storage::get_string(&storage, PREFERENCES_KEY).expect("preferences file");
    let loaded = load_preferences(Some(&raw), "rigwerk-preferences-backup-test");
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    (raw, loaded.preferences.expect("saved preferences"))
}

/// Saved ASIO and output ids are absent. The window shows the fallback devices.
fn absent_studio() -> super::RigWerkApp {
    let mut app = blank_app();
    app.inventory
        .asio
        .push(item("asio:other", "Other interface", "ASIO", false));
    app.inventory
        .inputs
        .push(item("hw:Mic", "Mic", "ALSA", false));
    app.inventory
        .outputs
        .push(item("default-out", "Speakers", "WASAPI", true));
    app.inventory
        .outputs
        .push(item("phones", "Headphones", "WASAPI", false));
    let preferences = Preferences {
        asio_name: Some("Focusrite USB".to_owned()),
        asio_id: "asio:focusrite".to_owned(),
        asio_backend: "ASIO".to_owned(),
        output_name: Some("Studio Monitors".to_owned()),
        output_id: "asio-out:studio".to_owned(),
        output_backend: "ASIO".to_owned(),
        ..Preferences::default()
    };
    install(&mut app, &preferences);
    app
}

#[test]
fn finish_startup_scan_restores_saved_devices_and_clears_the_gate() {
    let mut app = blank_app();
    app.inventory = audio::placeholder_inventory();
    app.startup = Some(StartupGate {
        preferences: Preferences {
            asio_name: Some("Focusrite USB".to_owned()),
            asio_id: "asio:focusrite".to_owned(),
            asio_backend: "ASIO".to_owned(),
            output_name: Some("Speakers".to_owned()),
            output_id: "speakers".to_owned(),
            output_backend: "WASAPI".to_owned(),
            auto_start_bridge: false,
            ..Preferences::default()
        },
    });
    let mut fresh = Inventory {
        os: "test".to_owned(),
        low_latency_label: "ASIO".to_owned(),
        ..Inventory::default()
    };
    fresh
        .asio
        .push(item("asio:focusrite", "Focusrite USB", "ASIO", false));
    fresh
        .outputs
        .push(item("speakers", "Speakers", "WASAPI", true));
    fresh.inputs.push(item("mic", "Mic", "WASAPI", false));
    app.finish_startup_scan(fresh);
    assert!(app.startup.is_none());
    assert_eq!(app.selected_asio().id, "asio:focusrite");
    assert_eq!(app.selected_output().id, "speakers");
    assert!(
        app.log
            .iter()
            .any(|entry| entry.text.contains("Ready: Focusrite USB -> Speakers")),
        "{:?}",
        app.log.iter().map(|entry| &entry.text).collect::<Vec<_>>()
    );
}

#[test]
fn save_writes_the_output_devices() {
    let mut app = blank_app();
    app.clock_device = OutputDeviceSettings {
        level_db: -9.0,
        mute: true,
        path_map: crate::audio::PathMapSettings::first(ChannelMap {
            left: 0b0101,
            right: 0b0110,
        }),
        unused: UnusedChannels::Silent,
        align: false,
    };
    app.followers = vec![FollowerPref {
        id: "hw:Phones".to_owned(),
        backend: "ALSA".to_owned(),
        name: "Phones".to_owned(),
        cushion_mode: 1,
        ..FollowerPref::default()
    }];
    let (_, saved) = save_text(&mut app);
    assert_eq!(saved.clock_device, app.clock_device);
    assert_eq!(saved.followers, app.followers);
}

#[test]
fn save_is_skipped_while_startup_scan_is_pending() {
    let mut app = absent_studio();
    app.startup = Some(StartupGate {
        preferences: Preferences::default(),
    });
    let mut storage = MemoryStorage::default();
    app.save(&mut storage);
    assert!(eframe::Storage::get_string(&storage, PREFERENCES_KEY).is_none());
}

#[test]
fn repaint_interval_is_fast_only_for_a_live_bridge_or_open_editor() {
    for state in [
        EngineState::Starting,
        EngineState::Running,
        EngineState::Stopping,
        EngineState::Preview,
    ] {
        assert_eq!(repaint_interval(state, false), ACTIVE_REPAINT);
        assert_eq!(repaint_interval(state, true), ACTIVE_REPAINT);
    }
    for state in [EngineState::Idle, EngineState::Error] {
        assert_eq!(repaint_interval(state, false), IDLE_REPAINT);
        assert_eq!(repaint_interval(state, true), ACTIVE_REPAINT);
    }
    assert_eq!(ACTIVE_REPAINT.as_millis(), 16);
    assert_eq!(IDLE_REPAINT.as_millis(), 200);
}

#[test]
fn save_keeps_an_output_id_the_scan_did_not_find() {
    let mut app = absent_studio();
    // The window is usable on the fallback. That row is what an unguarded
    // save would write.
    assert_eq!(app.selected_output().id, "default-out");
    assert_eq!(app.selected_asio().id, "asio:other");
    assert!(
        app.input_held.is_none(),
        "an empty input is not a stored identity"
    );
    let (raw, saved) = save_text(&mut app);
    assert!(raw.contains("asio-out:studio"), "{raw}");
    assert!(!raw.contains("default-out"), "{raw}");
    assert_eq!(saved.output_id, "asio-out:studio");
    assert_eq!(saved.output_name.as_deref(), Some("Studio Monitors"));
    assert_eq!(saved.output_backend, "ASIO");
    assert_eq!(saved.asio_id, "asio:focusrite");
    assert_eq!(saved.asio_name.as_deref(), Some("Focusrite USB"));
    assert_eq!(saved.asio_backend, "ASIO");
    // Nothing was stored for the system input, so the live device is recorded.
    assert_eq!(saved.input_id, "hw:Mic");
    assert_eq!(saved.input_name.as_deref(), Some("Mic"));
    assert_eq!(saved.input_backend, "ALSA");
}

#[test]
fn a_successful_restore_saves_the_current_device() {
    let mut app = blank_app();
    app.inventory
        .outputs
        .push(item("default-out", "Speakers", "WASAPI", true));
    app.inventory
        .outputs
        .push(item("hw:Speaker", "Studio", "ALSA", false));
    install(
        &mut app,
        &Preferences {
            output_name: Some("Old Studio Name".to_owned()),
            output_id: "hw:Speaker".to_owned(),
            output_backend: "ALSA".to_owned(),
            ..Preferences::default()
        },
    );
    assert!(app.output_held.is_none());
    assert_eq!(app.selected_output().id, "hw:Speaker");
    assert_ne!(app.selected_output().id, "default-out");
    let (raw, saved) = save_text(&mut app);
    assert!(raw.contains("hw:Speaker"), "{raw}");
    assert!(!raw.contains("default-out"), "{raw}");
    assert!(!raw.contains("Old Studio Name"), "{raw}");
    assert_eq!(saved.output_id, "hw:Speaker");
    assert_eq!(saved.output_name.as_deref(), Some("Studio"));
    assert_eq!(saved.output_backend, "ALSA");
}

#[test]
fn a_legacy_name_match_gains_an_id_on_save() {
    let mut app = blank_app();
    app.inventory
        .outputs
        .push(item("default-out", "System Default", "WASAPI", true));
    app.inventory
        .outputs
        .push(item("wasapi:{spk}", "Speakers", "WASAPI", false));
    install(
        &mut app,
        &Preferences {
            output_name: Some("Speakers".to_owned()),
            output_id: String::new(),
            output_backend: String::new(),
            ..Preferences::default()
        },
    );
    assert!(app.output_held.is_none());
    assert_eq!(app.selected_output().id, "wasapi:{spk}");
    let (raw, saved) = save_text(&mut app);
    assert!(raw.contains("wasapi:{spk}"), "{raw}");
    assert!(!raw.contains("default-out"), "{raw}");
    assert_eq!(saved.output_id, "wasapi:{spk}");
    assert_eq!(saved.output_name.as_deref(), Some("Speakers"));
    assert_eq!(saved.output_backend, "WASAPI");
}

#[test]
fn a_same_name_device_does_not_replace_a_stored_id() {
    let mut app = blank_app();
    app.inventory
        .outputs
        .push(item("wasapi:{spk}", "Speakers", "WASAPI", true));
    install(
        &mut app,
        &Preferences {
            output_name: Some("Speakers".to_owned()),
            output_id: "asio-out:studio".to_owned(),
            output_backend: "ASIO".to_owned(),
            ..Preferences::default()
        },
    );
    assert_eq!(app.selected_output().id, "wasapi:{spk}");
    assert_eq!(
        app.output_held.as_ref().map(|held| held.id.as_str()),
        Some("asio-out:studio")
    );
    let (raw, saved) = save_text(&mut app);
    assert!(raw.contains("asio-out:studio"), "{raw}");
    assert!(!raw.contains("wasapi:{spk}"), "{raw}");
    assert_eq!(saved.output_id, "asio-out:studio");
    assert_eq!(saved.output_name.as_deref(), Some("Speakers"));
    assert_eq!(saved.output_backend, "ASIO");
}

#[test]
fn an_explicit_output_pick_replaces_the_held_identity() {
    let mut app = absent_studio();
    let (_, kept) = save_text(&mut app);
    assert_eq!(kept.output_id, "asio-out:studio");
    app.output_idx = 1;
    app.accept_output_device();
    assert!(app.output_held.is_none());
    let (_, saved) = save_text(&mut app);
    assert_eq!(saved.output_id, "phones");
    assert_eq!(saved.output_name.as_deref(), Some("Headphones"));
    assert_eq!(saved.output_backend, "WASAPI");
    // The other side was not picked, so its stored identity stays.
    assert_eq!(saved.asio_id, "asio:focusrite");
}

#[test]
fn a_preset_that_finds_the_output_may_be_saved() {
    let mut app = absent_studio();
    assert!(app.output_held.is_some());
    app.sessions.push(Preset {
        name: "Phones".to_owned(),
        input: "Other interface".to_owned(),
        input_id: "asio:other".to_owned(),
        input_backend: "ASIO".to_owned(),
        output: "Headphones".to_owned(),
        output_id: "phones".to_owned(),
        output_backend: "WASAPI".to_owned(),
        ..Preset::default()
    });
    app.apply_preset(0);
    assert!(app.output_held.is_none());
    assert!(app.asio_held.is_none());
    assert_eq!(app.selected_output().id, "phones");
    assert_eq!(app.selected_asio().id, "asio:other");
    let (_, saved) = save_text(&mut app);
    assert_eq!(saved.output_id, "phones");
    assert_eq!(saved.output_backend, "WASAPI");
    assert_eq!(saved.asio_id, "asio:other");
}

#[test]
fn a_preset_that_misses_the_device_keeps_the_stored_id() {
    let mut app = absent_studio();
    app.sessions.push(Preset {
        name: "Gone".to_owned(),
        input: "Missing interface".to_owned(),
        input_id: "asio:missing".to_owned(),
        input_backend: "ASIO".to_owned(),
        output: "Missing monitors".to_owned(),
        output_id: "asio-out:nope".to_owned(),
        output_backend: "ASIO".to_owned(),
        ..Preset::default()
    });
    app.apply_preset(0);
    assert_eq!(
        app.output_held.as_ref().map(|held| held.id.as_str()),
        Some("asio-out:studio")
    );
    assert_eq!(
        app.asio_held.as_ref().map(|held| held.id.as_str()),
        Some("asio:focusrite")
    );
    assert_eq!(app.selected_output().id, "default-out");
    let (raw, saved) = save_text(&mut app);
    assert!(raw.contains("asio-out:studio"), "{raw}");
    assert!(!raw.contains("default-out"), "{raw}");
    assert_eq!(saved.output_id, "asio-out:studio");
    assert_eq!(saved.output_name.as_deref(), Some("Studio Monitors"));
    assert_eq!(saved.asio_id, "asio:focusrite");
    assert_eq!(saved.sessions[0].output_id, "asio-out:nope");
}

#[test]
fn nothing_stored_saves_the_selected_device() {
    let mut app = blank_app();
    app.inventory
        .asio
        .push(item("asio:other", "Other interface", "ASIO", false));
    app.inventory
        .inputs
        .push(item("hw:Mic", "Mic", "ALSA", false));
    app.inventory
        .outputs
        .push(item("default-out", "Speakers", "WASAPI", true));
    install(&mut app, &Preferences::default());
    assert!(app.asio_held.is_none());
    assert!(app.input_held.is_none());
    assert!(app.output_held.is_none());
    let (_, saved) = save_text(&mut app);
    assert_eq!(saved.asio_id, "asio:other");
    assert_eq!(saved.input_id, "hw:Mic");
    assert_eq!(saved.output_id, "default-out");
    assert_eq!(saved.output_name.as_deref(), Some("Speakers"));
}

#[test]
fn the_activity_log_keeps_hundreds_of_entries() {
    use super::ACTIVITY_LOG_CAP;

    assert!(
        (200..=500).contains(&ACTIVITY_LOG_CAP),
        "cap {ACTIVITY_LOG_CAP} is outside 200..=500"
    );
    let mut app = blank_app();
    let extra = 25;
    for index in 0..(ACTIVITY_LOG_CAP + extra) {
        app.push_log(format!("entry {index}"), LogTone::Muted);
    }
    assert_eq!(app.log.len(), ACTIVITY_LOG_CAP);
    assert_eq!(app.log[0].text, format!("entry {extra}"));
    let last = format!("entry {}", ACTIVITY_LOG_CAP + extra - 1);
    assert_eq!(
        app.log.last().map(|entry| entry.text.as_str()),
        Some(last.as_str())
    );
    assert_ne!(app.log.len(), 6);
}

#[test]
fn a_scan_failure_is_kept_when_the_device_list_does_not_change() {
    let mut app = blank_app();
    app.inventory_fingerprint = super::devices::inventory_fingerprint(&app.inventory);
    let mut failed = app.inventory.clone();
    failed
        .notes
        .push("ASIO registry scan: registry unavailable".to_owned());
    assert!(!app.apply_inventory(failed));
    assert_eq!(
        app.inventory.notes.as_slice(),
        ["ASIO registry scan: registry unavailable"]
    );
    assert_eq!(
        app.log
            .iter()
            .filter(|entry| entry.text.contains("registry unavailable"))
            .count(),
        1
    );

    let again = app.inventory.clone();
    assert!(!app.apply_inventory(again.clone()));
    assert_eq!(
        app.log
            .iter()
            .filter(|entry| entry.text.contains("registry unavailable"))
            .count(),
        1
    );

    let mut cleared = again;
    cleared.notes.clear();
    assert!(!app.apply_inventory(cleared));
    assert!(app.inventory.notes.is_empty());
    assert!(app
        .log
        .iter()
        .any(|entry| entry.text.contains("registry unavailable")));
}

#[test]
fn a_device_change_still_records_a_scan_note() {
    let mut app = blank_app();
    app.inventory_fingerprint = super::devices::inventory_fingerprint(&app.inventory);
    let mut fresh = app.inventory.clone();
    fresh
        .outputs
        .push(item("default-out", "Speakers", "WASAPI", true));
    fresh.notes.push("ASIO registry scan: boom".to_owned());
    assert!(app.apply_inventory(fresh));
    assert_eq!(app.inventory.notes.as_slice(), ["ASIO registry scan: boom"]);
    assert!(app
        .log
        .iter()
        .any(|entry| entry.text.contains("Device change applied")));
    assert_eq!(
        app.log.last().map(|entry| entry.text.as_str()),
        Some("ASIO registry scan: boom")
    );
}

#[test]
fn one_missed_scan_does_not_stop_the_bridge() {
    let mut app = blank_app();
    app.inventory
        .outputs
        .push(item("hw:Speaker", "Studio", "ALSA", false));
    app.output_idx = 0;
    app.bridge_signature = 7;
    app.inventory_fingerprint = super::devices::inventory_fingerprint(&app.inventory);
    let mut gone = app.inventory.clone();
    gone.outputs.clear();
    assert!(!app.apply_inventory(gone.clone()));
    assert_eq!(app.bridge_signature, 7);
    assert_eq!(app.selected_output().id, "hw:Speaker");
    assert!(app
        .log
        .iter()
        .any(|entry| entry.text.contains("missed one device scan")));
    assert!(app.apply_inventory(gone));
    assert_eq!(app.bridge_signature, 0);
    assert!(app
        .log
        .iter()
        .any(|entry| entry.text.contains("unplugged — bridge stopped")));
    assert_eq!(app.issue_state.device_lost, app.device_alert);
}

/// One M key event.  `down` is false for the release between two presses.
/// egui treats another key-down, while M is still down, as a repeat.
fn key_m(modifiers: egui::Modifiers, down: bool) -> egui::RawInput {
    let mut input = test_input(None);
    input.events.push(egui::Event::Key {
        key: egui::Key::M,
        physical_key: None,
        pressed: down,
        repeat: false,
        modifiers,
    });
    input
}

/// Runs one frame and handles the panic chord the way `App::ui` does.
fn press_panic_chord(app: &mut super::RigWerkApp, ctx: &egui::Context, input: egui::RawInput) {
    let handle = ctx.clone();
    let _ = run_frame(ctx, input, |_| app.handle_panic_mute(&handle));
}

/// Ctrl on Windows and Linux sets both `ctrl` and egui's `command`.
fn ctrl_shift() -> egui::Modifiers {
    egui::Modifiers::COMMAND
        .plus(egui::Modifiers::CTRL)
        .plus(egui::Modifiers::SHIFT)
}

/// Command on macOS sets `command` and `mac_cmd`, not `ctrl`.
fn command_shift() -> egui::Modifiers {
    egui::Modifiers::COMMAND
        .plus(egui::Modifiers::MAC_CMD)
        .plus(egui::Modifiers::SHIFT)
}

#[test]
fn panic_chord_toggles_master_mute() {
    let mut app = blank_app();
    let ctx = egui::Context::default();
    assert!(!app.mute);

    press_panic_chord(&mut app, &ctx, key_m(ctrl_shift(), true));
    assert!(app.mute, "Ctrl+Shift+M mutes");
    // Still held: egui reports the next key-down as a repeat, which must not unmute.
    press_panic_chord(&mut app, &ctx, key_m(ctrl_shift(), true));
    assert!(app.mute, "holding the chord does not toggle again");
    press_panic_chord(&mut app, &ctx, key_m(ctrl_shift(), false));
    press_panic_chord(&mut app, &ctx, key_m(ctrl_shift(), true));
    assert!(!app.mute, "a second press unmutes");
    press_panic_chord(&mut app, &ctx, key_m(ctrl_shift(), false));

    press_panic_chord(&mut app, &ctx, key_m(command_shift(), true));
    assert!(app.mute, "Command+Shift+M mutes on macOS");
    press_panic_chord(&mut app, &ctx, key_m(command_shift(), false));
    press_panic_chord(&mut app, &ctx, key_m(command_shift(), true));
    assert!(!app.mute, "a second Command+Shift+M unmutes");
}

#[test]
fn panic_chord_ignores_ctrl_m() {
    let mut app = blank_app();
    let ctx = egui::Context::default();
    press_panic_chord(
        &mut app,
        &ctx,
        key_m(egui::Modifiers::COMMAND.plus(egui::Modifiers::CTRL), true),
    );
    assert!(!app.mute);
}

#[test]
fn panic_chord_ignores_alt() {
    let mut app = blank_app();
    let ctx = egui::Context::default();
    press_panic_chord(
        &mut app,
        &ctx,
        key_m(ctrl_shift().plus(egui::Modifiers::ALT), true),
    );
    assert!(!app.mute);
}

#[test]
fn panic_chord_does_not_toggle_while_a_text_edit_has_focus() {
    let mut app = blank_app();
    let ctx = egui::Context::default();
    let handle = ctx.clone();
    let mut text = String::new();

    let _ = run_frame(&ctx, test_input(None), |ui| {
        ui.text_edit_singleline(&mut text).request_focus();
    });
    // Focus lands on the next pass.  The field has to stay on screen or egui drops it.
    let _ = run_frame(&ctx, test_input(None), |ui| {
        ui.text_edit_singleline(&mut text);
    });

    let mut kept_the_key = false;
    let _ = run_frame(&ctx, key_m(ctrl_shift(), true), |ui| {
        assert!(handle.text_edit_focused(), "the text edit still has focus");
        app.handle_panic_mute(&handle);
        kept_the_key = handle.input(|input| {
            input.events.iter().any(|event| {
                matches!(
                    event,
                    egui::Event::Key {
                        key: egui::Key::M,
                        pressed: true,
                        ..
                    }
                )
            })
        });
        ui.text_edit_singleline(&mut text);
    });
    assert!(!app.mute, "typing is not a mute");
    assert!(kept_the_key, "the text edit keeps the key");
}
