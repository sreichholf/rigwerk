use std::collections::HashMap;

use super::*;
use crate::app::appearance::ThemeChoice;
use crate::app::latency::{LoopbackKind, MeasuredLatency};
use crate::app::Page;
use crate::audio::{
    ChannelMap, EqSettings, GateKey, GateSettings, GateSlot, LegacyNamSettings, NamPosition,
    OutputDeviceSettings, PluginFormat, PluginPref, UnusedChannels, WasapiOutputMode,
    MAX_EXTRA_OUTPUTS, MAX_NAM_BLOCKS, VIRTUAL_OUTPUT_ID, VIRTUAL_OUTPUT_NAME,
};

const BACKUP: &str = "rigwerk-preferences-backup-1";

/// eframe's file storage in memory: the same key/string interface, and
/// `set_value` writes RON into it exactly as the app's autosave does.
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

fn session(name: &str) -> Session {
    Session {
        name: name.to_owned(),
        note: "ASIO → WASAPI".to_owned(),
        input: "Focusrite USB ASIO".to_owned(),
        input_id: "asio:focusrite".to_owned(),
        input_backend: "ASIO".to_owned(),
        output: "Speakers".to_owned(),
        output_id: "wasapi:{speakers}".to_owned(),
        output_backend: "WASAPI".to_owned(),
        rate: 96_000,
        buffer: 64,
        output_rate: 48_000,
        output_buffer: 256,
        eq: Some(EqSettings::default()),
        eq_post: None,
        ..Session::default()
    }
}

fn plugin(path: &str, format: PluginFormat) -> PluginPref {
    PluginPref {
        path: path.to_owned(),
        format,
        class_id: Some("ABCD".to_owned()),
        name: "Amp".to_owned(),
        vendor: "Vendor".to_owned(),
        category: "Fx".to_owned(),
        bypass: true,
        mix: 0.5,
        params: ron::from_str("[(id: 7, value: 0.25)]").expect("params"),
        state: None,
    }
}

fn stored(preferences: &Preferences) -> String {
    let mut storage = MemoryStorage::default();
    store_preferences(&mut storage, preferences, &mut None);
    eframe::Storage::get_string(&storage, PREFERENCES_KEY).expect("stored")
}

#[test]
fn round_trip_keeps_everything() {
    let mut verb = plugin("C:/Plugins/Verb.clap", PluginFormat::Clap);
    verb.state =
        ron::from_str(r#"Some((version: 1, data: "AAEC/w==", params: [(id: 7, value: 0.25)]))"#)
            .expect("state");
    let preferences = Preferences {
        sessions: vec![session("Clean"), session("Lead")],
        plugins: vec![
            plugin("C:/Plugins/Amp.vst3", PluginFormat::Vst3),
            verb.clone(),
        ],
        sample_rate: 96_000,
        session_name: "Gig".to_owned(),
        ..Preferences::default()
    };
    let loaded = load_preferences(Some(&stored(&preferences)), BACKUP);
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    assert!(loaded.backup.is_none());
    let back = loaded.preferences.expect("readable");
    assert_eq!(back.schema_version, PREFERENCES_SCHEMA);
    assert_eq!(back.sample_rate, 96_000);
    assert_eq!(back.session_name, "Gig");
    assert_eq!(back.sessions.len(), 2);
    assert_eq!(back.sessions[1].name, "Lead");
    assert_eq!(back.sessions[0].input_id, "asio:focusrite");
    assert_eq!(back.sessions[0].input_backend, "ASIO");
    assert_eq!(back.sessions[0].output_id, "wasapi:{speakers}");
    assert_eq!(back.sessions[0].output_backend, "WASAPI");
    assert_eq!(back.sessions[0].output_buffer, 256);
    // A session from before paths: path 1, with what it never stored (the
    // post EQ) taken from the first path of the preferences.
    assert_eq!(back.sessions[0].paths.len(), 1);
    assert_eq!(back.sessions[0].paths[0].eq, EqSettings::default());
    assert_eq!(back.sessions[0].paths[0].eq_post, back.paths[0].eq_post);
    assert!(back.sessions[0].eq.is_none());
    let back = &back.paths[0];
    assert_eq!(back.plugins.len(), 2);
    assert_eq!(back.plugins[1].format, PluginFormat::Clap);
    assert_eq!(back.plugins[1].class_id.as_deref(), Some("ABCD"));
    assert!(back.plugins[1].bypass);
    assert_eq!(back.plugins[1].mix, 0.5);
    assert_eq!(back.plugins[1].params[0].id, 7);
    assert_eq!(back.plugins[1].state, verb.state);
    assert!(back.plugins[0].state.is_none());
}

#[test]
fn damaged_plugin_state_keeps_the_effect() {
    let raw = r#"(plugins:[(path:"C:/Plugins/Amp.clap",format:clap,name:"Amp",params:[(id:3,value:0.5)],state:Some((version:"one",data:12))),(path:"C:/Plugins/Old.vst3",name:"Old")])"#;
    let loaded = load_preferences(Some(raw), BACKUP);
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    let preferences = loaded.preferences.expect("readable");
    let plugins = &preferences.paths[0].plugins;
    assert_eq!(plugins.len(), 2);
    assert!(plugins[0].state.is_none());
    assert_eq!(plugins[0].params[0].id, 3);
    assert!(plugins[1].state.is_none());
}

#[test]
fn session_missing_fields_still_loads() {
    // Written before `note`, `buffer` and the output side existed, and
    // before the schema version: used to fail the whole preferences.
    let raw = r#"(auto_start_bridge:true,sessions:[(name:"Old",input:"In",output:"Out",rate:44100)],session_name:"Mine",sample_rate:44100)"#;
    let loaded = load_preferences(Some(raw), BACKUP);
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    let preferences = loaded.preferences.expect("readable");
    assert!(preferences.auto_start_bridge);
    assert_eq!(preferences.schema_version, PREFERENCES_SCHEMA);
    assert_eq!(preferences.sessions.len(), 1);
    let old = &preferences.sessions[0];
    assert_eq!(old.name, "Old");
    assert_eq!(old.rate, 44_100);
    assert_eq!(old.buffer, 128);
    assert_eq!(old.note, "");
    assert_eq!(old.input_id, "");
    assert_eq!(old.input_backend, "");
    assert_eq!(old.output_id, "");
    assert_eq!(old.output_backend, "");
    assert_eq!(old.output_rate, 0);
    // Fields added with the mix: missing means the ordinary defaults,
    // and a 0 output buffer still means "use the input buffer".
    assert_eq!(old.output_buffer, 0);
    assert_eq!(old.input_gain, DEFAULT_INPUT_GAIN);
    assert_eq!(old.output_gain, DEFAULT_OUTPUT_GAIN);
    assert_eq!(old.input_channels, [true, true]);
    assert_eq!(old.paths.len(), 1, "an old session is one path");
    assert_eq!(old.paths[0].output_channels, [true, true]);
    assert!(!old.paths[0].mono);
    assert!(!old.mute);
    assert!(old.drift);
    assert!(old.low_latency_output);
    assert_eq!(old.wasapi_output_mode, Some(WasapiOutputMode::RealStyle));
    assert_eq!(
        old.paths[0].routes,
        [[true, false, false, false], [false, true, false, false]]
    );
    assert_eq!(old.bridge_buffer_mode, 0);
    assert_eq!(old.bridge_buffer_ms, DEFAULT_BRIDGE_BUFFER_MS);
    assert!(old.paths[0].plugins.is_empty());
    assert!(old.eq.is_none());
    assert!(old.eq_post.is_none());
    assert!(old.gate.is_none());
}

#[test]
fn gate_round_trips_in_preferences_and_sessions() {
    let gate = GateSettings {
        enabled: true,
        slot: GateSlot::Pre,
        key: GateKey::Input,
        threshold_db: -42.5,
        range_db: -55.0,
        hysteresis_db: 8.0,
        attack_ms: 2.5,
        release_ms: 180.0,
        hold_ms: 55.0,
    };
    let mut session = session("Gated");
    session.gate = Some(gate);
    session.eq_post = Some(EqSettings::default());
    let preferences = Preferences {
        gate,
        sessions: vec![session],
        ..Preferences::default()
    };
    let loaded = load_preferences(Some(&stored(&preferences)), BACKUP);
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    let back = loaded.preferences.expect("readable");
    assert_eq!(back.paths[0].gate, gate);
    assert_eq!(back.sessions[0].paths[0].gate, gate);
    assert_eq!(back.sessions[0].paths[0].eq_post, EqSettings::default());
}

#[test]
fn nam_missing_from_an_old_path_uses_defaults() {
    let pref: PathPref = ron::from_str(r#"(name:"Main",level_db:-3.0)"#).expect("old path");
    assert_eq!(pref.amp, crate::audio::LegacyNamSettings::default());
    assert!(pref.amp.model_path.is_empty());
    assert!(!pref.amp.enabled);
    assert_eq!(pref.amp.slot, crate::audio::NamPosition::Pre);
    assert!(pref.nam_blocks.is_empty());
    assert_eq!(pref.level_db, -3.0);
}

/// The single `amp` stage of an old save is read as the first NAM block.
const LEGACY_AMP_PATH: &str = r#"(
            name:"Main",
            amp:(
                enabled:true,
                slot:post,
                input_db:-3.0,
                output_db:2.0,
                slim:0.5,
                model_path:"C:/Caps/pedal.nam"
            ),
        )"#;

#[test]
fn a_legacy_amp_stage_becomes_one_nam_block() {
    let mut pref: PathPref = ron::from_str(LEGACY_AMP_PATH).expect("old path");
    assert!(pref.nam_blocks.is_empty(), "the save had no block list");
    pref.sanitize(0);
    assert_eq!(pref.nam_blocks.len(), 1);
    let block = &pref.nam_blocks[0];
    assert_eq!(block.model, "C:/Caps/pedal.nam");
    assert!(block.controls.enabled);
    assert_eq!(block.controls.position, NamPosition::Post);
    assert_eq!(block.controls.input_db, -3.0);
    assert_eq!(block.controls.output_db, 2.0);
    assert_eq!(block.controls.slim, 0.5);
    assert!(block.controls.auto_level);
    assert_eq!(pref.amp, LegacyNamSettings::default(), "the stage is gone");
}

#[test]
fn a_migrated_path_saves_blocks_and_no_amp_key() {
    let mut pref: PathPref = ron::from_str(LEGACY_AMP_PATH).expect("old path");
    pref.sanitize(0);
    let saved = ron::to_string(&pref).expect("save");
    assert!(
        !saved.contains("amp:"),
        "the legacy stage was written again: {saved}"
    );
    assert!(
        saved.contains("nam_blocks:"),
        "the blocks were not written: {saved}"
    );
}

#[test]
fn a_migrated_path_round_trips() {
    let mut pref: PathPref = ron::from_str(LEGACY_AMP_PATH).expect("old path");
    pref.sanitize(0);
    let saved = ron::to_string(&pref).expect("save");
    let mut back: PathPref = ron::from_str(&saved).expect("load");
    back.sanitize(0);
    assert_eq!(back, pref, "load → save → load is stable");
}

#[test]
fn a_path_keeps_at_most_sixteen_nam_blocks() {
    let blocks: Vec<String> = (0..20)
        .map(|index| format!("(model:\"/captures/{index}.nam\")"))
        .collect();
    let raw = format!("(paths:[(nam_blocks:[{}])])", blocks.join(","));
    let preferences = load_preferences(Some(&raw), BACKUP)
        .preferences
        .expect("readable");
    let saved = &preferences.paths[0].nam_blocks;
    assert_eq!(saved.len(), MAX_NAM_BLOCKS);
    assert_eq!(saved[0].model, "/captures/0.nam");
    assert_eq!(saved[MAX_NAM_BLOCKS - 1].model, "/captures/15.nam");
}

#[test]
fn gate_missing_from_old_saves_uses_defaults() {
    let raw = r#"(auto_start_bridge:false,session_name:"Old",sample_rate:48000)"#;
    let loaded = load_preferences(Some(raw), BACKUP);
    let preferences = loaded.preferences.expect("readable");
    let gate = preferences.paths[0].gate;
    assert_eq!(gate, GateSettings::default());
    assert_eq!(gate.slot, GateSlot::Post);
    assert!(!gate.enabled);
}

#[test]
fn a_saved_return_loads_without_a_solo_or_sends() {
    let raw = r#"(paths:[(name:"A",sends:[-60.0,-6.0]),(name:"FX",feed:sends,solo:true,sends:[0.0,0.0])])"#;
    let preferences = load_preferences(Some(raw), BACKUP)
        .preferences
        .expect("readable");
    let paths = &preferences.paths;
    assert_eq!(paths[0].sends, vec![crate::audio::SEND_OFF_DB, -6.0]);
    assert_eq!(paths[1].feed, crate::audio::PathFeed::Sends);
    assert!(!paths[1].solo);
    assert!(paths[1].sends.is_empty());
    assert!(!paths[1].mix().solo);
}

#[test]
fn a_gate_saved_before_the_key_listens_to_its_signal() {
    let raw = r#"(paths:[(gate:(enabled:true,slot:pre,threshold_db:-40.0))])"#;
    let preferences = load_preferences(Some(raw), BACKUP)
        .preferences
        .expect("readable");
    let gate = preferences.paths[0].gate;
    assert!(gate.enabled);
    assert_eq!(gate.slot, GateSlot::Pre);
    assert_eq!(gate.key, GateKey::Signal);
}

#[test]
fn a_save_with_the_dropped_prefer_plugin_interface_still_loads() {
    let raw = r#"(session_name:"Old",prefer_plugin_interface:true,sample_rate:44100)"#;
    let loaded = load_preferences(Some(raw), BACKUP);
    assert!(loaded.backup.is_none(), "nothing to keep aside");
    let preferences = loaded.preferences.expect("readable");
    assert_eq!(preferences.session_name, "Old");
    assert_eq!(preferences.sample_rate, 44100);
}

#[test]
fn gate_knobs_are_sanitized_on_load() {
    let raw = r#"(
            gate:(
                enabled:true,
                slot:pre,
                threshold_db:-200.0,
                range_db:10.0,
                hysteresis_db:NaN,
                attack_ms:0.0,
                release_ms:9999.0,
                hold_ms:-1.0
            ),
            sessions:[(
                name:"BadGate",
                input:"In",
                output:"Out",
                rate:48000,
                gate:(
                    enabled:true,
                    slot:post,
                    threshold_db:inf,
                    range_db:-200.0,
                    hysteresis_db:100.0,
                    attack_ms:NaN,
                    release_ms:0.0,
                    hold_ms:inf
                )
            )]
        )"#;
    let loaded = load_preferences(Some(raw), BACKUP);
    let preferences = loaded.preferences.expect("readable");
    let defaults = GateSettings::default();
    let gate = preferences.paths[0].gate;
    assert!(gate.enabled);
    assert_eq!(gate.slot, GateSlot::Pre);
    assert_eq!(gate.threshold_db, crate::audio::GATE_MIN_THRESHOLD_DB);
    assert_eq!(gate.range_db, crate::audio::GATE_MAX_RANGE_DB);
    assert_eq!(gate.hysteresis_db, defaults.hysteresis_db);
    assert_eq!(gate.attack_ms, crate::audio::GATE_MIN_ATTACK_MS);
    assert_eq!(gate.release_ms, crate::audio::GATE_MAX_RELEASE_MS);
    assert_eq!(gate.hold_ms, crate::audio::GATE_MIN_HOLD_MS);
    let session_gate = preferences.sessions[0].paths[0].gate;
    assert_eq!(session_gate.threshold_db, defaults.threshold_db);
    assert_eq!(session_gate.range_db, crate::audio::GATE_MIN_RANGE_DB);
    assert_eq!(
        session_gate.hysteresis_db,
        crate::audio::GATE_MAX_HYSTERESIS_DB
    );
    assert_eq!(session_gate.attack_ms, defaults.attack_ms);
    assert_eq!(session_gate.release_ms, crate::audio::GATE_MIN_RELEASE_MS);
    assert_eq!(session_gate.hold_ms, defaults.hold_ms);
}

#[test]
fn corrupt_gains_rates_and_buffers_are_not_applied_raw() {
    let raw = r#"(
            input_gain:NaN,
            output_gain:inf,
            sample_rate:22050,
            buffer_size:7,
            output_rate:12345,
            output_buffer:1,
            cushion_ms:NaN,
            sessions:[(
                name:"Bad",
                input:"In",
                output:"Out",
                rate:1,
                buffer:0,
                output_rate:7,
                output_buffer:9,
                input_gain:NaN,
                output_gain:-inf,
                cushion_ms:inf
            )]
        )"#;
    let loaded = load_preferences(Some(raw), BACKUP);
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    let preferences = loaded.preferences.expect("readable");
    assert!(preferences.input_gain.is_finite());
    assert_eq!(preferences.input_gain, DEFAULT_INPUT_GAIN);
    assert!((INPUT_GAIN_MIN_DB..=INPUT_GAIN_MAX_DB).contains(&preferences.input_gain));
    assert!(preferences.output_gain.is_finite());
    assert_eq!(preferences.output_gain, OUTPUT_GAIN_MAX_DB);
    assert_eq!(preferences.sample_rate, DEFAULT_SAMPLE_RATE);
    assert_ne!(preferences.sample_rate, 22_050);
    assert_eq!(preferences.buffer_size, DEFAULT_BUFFER);
    assert_ne!(preferences.buffer_size, 7);
    assert_eq!(preferences.output_rate, DEFAULT_SAMPLE_RATE);
    assert_ne!(preferences.output_rate, 12_345);
    assert!(RATES.contains(&preferences.output_rate));
    assert_eq!(preferences.output_buffer, DEFAULT_BUFFER);
    assert!(preferences.output_buffer >= MIN_OUTPUT_BUFFER);
    assert_ne!(preferences.output_buffer, 1);
    assert_eq!(preferences.bridge_buffer_ms, DEFAULT_BRIDGE_BUFFER_MS);

    let session = &preferences.sessions[0];
    assert_eq!(session.rate, DEFAULT_SAMPLE_RATE);
    assert_ne!(session.rate, 1);
    assert_eq!(session.buffer, DEFAULT_BUFFER);
    assert_ne!(session.buffer, 0);
    assert_eq!(session.output_rate, DEFAULT_SAMPLE_RATE);
    assert_ne!(session.output_buffer, 9);
    assert_eq!(session.output_buffer, DEFAULT_BUFFER);
    assert!(session.output_buffer >= MIN_OUTPUT_BUFFER);
    assert!(session.input_gain.is_finite());
    assert_eq!(session.input_gain, DEFAULT_INPUT_GAIN);
    assert_eq!(session.output_gain, OUTPUT_GAIN_MIN_DB);
    assert!(session.output_gain.is_finite());
    assert_eq!(session.bridge_buffer_ms, BRIDGE_BUFFER_MS_MAX);

    // A listed size is kept, and so is a step that is not on the combo
    // but still inside the output floor and the largest offered size.
    let kept = validate_bridge_settings(
        BridgeSettings {
            sample_rate: 96_000,
            buffer_size: 64,
            output_rate: 0,
            output_buffer: 16,
            input_gain: 100.0,
            output_gain: -100.0,
            bridge_buffer_ms: 7.5,
        },
        OutputBufferZero::Invalid,
    );
    assert_eq!(kept.sample_rate, 96_000);
    assert_eq!(kept.buffer_size, 64);
    assert_eq!(kept.output_rate, 0);
    assert_eq!(kept.output_buffer, MIN_OUTPUT_BUFFER);
    assert_eq!(kept.input_gain, INPUT_GAIN_MAX_DB);
    assert_eq!(kept.output_gain, OUTPUT_GAIN_MIN_DB);
    assert_eq!(kept.bridge_buffer_ms, 7.5);

    let huge = validate_bridge_settings(
        BridgeSettings {
            sample_rate: 48_000,
            buffer_size: 4_096,
            output_rate: 48_000,
            output_buffer: 4_096,
            input_gain: 0.0,
            output_gain: 0.0,
            bridge_buffer_ms: 10.0,
        },
        OutputBufferZero::FollowInput,
    );
    assert_eq!(huge.buffer_size, DEFAULT_BUFFER);
    assert_ne!(huge.buffer_size, 4_096);
    assert_eq!(huge.output_buffer, DEFAULT_BUFFER);
    assert!(RATES.contains(&DEFAULT_SAMPLE_RATE));
    assert!(BUFFERS.contains(&DEFAULT_BUFFER));
    assert_eq!(MAX_SANE_BUFFER, *BUFFERS.last().unwrap());
}

#[test]
fn a_full_session_round_trips_routes_gains_and_plugins() {
    let mut full = session("Gig");
    full.input_gain = 6.0;
    full.output_gain = -12.0;
    full.input_channels = [true, false];
    full.output_channels = [false, true];
    full.mono = true;
    full.mute = true;
    full.drift = false;
    full.low_latency_output = false;
    full.routes = [[false, true, false, true], [true, false, true, false]];
    full.bridge_buffer_mode = 2;
    full.bridge_buffer_ms = 7.5;
    full.plugins = vec![plugin("C:/Plugins/Amp.vst3", PluginFormat::Vst3)];
    let preferences = Preferences {
        sessions: vec![full.clone()],
        ..Preferences::default()
    };
    let loaded = load_preferences(Some(&stored(&preferences)), BACKUP);
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    let back = &loaded.preferences.expect("readable").sessions[0];
    assert_eq!(back.input_id, full.input_id);
    assert_eq!(back.input_backend, full.input_backend);
    assert_eq!(back.output_id, full.output_id);
    assert_eq!(back.output_backend, full.output_backend);
    assert_eq!(back.input_gain, 6.0);
    assert_eq!(back.output_gain, -12.0);
    assert_eq!(back.input_channels, [true, false]);
    assert!(back.mute);
    assert!(!back.drift);
    assert!(!back.low_latency_output);
    assert_eq!(back.bridge_buffer_mode, 2);
    assert_eq!(back.bridge_buffer_ms, 7.5);
    let path = &back.paths[0];
    assert_eq!(path.output_channels, [false, true]);
    assert!(path.mono);
    assert_eq!(path.routes, full.routes);
    assert_eq!(path.plugins.len(), 1);
    assert_eq!(path.plugins[0].path, "C:/Plugins/Amp.vst3");
    assert_eq!(path.plugins[0].mix, 0.5);
    assert_eq!(back.rate, 96_000);
    assert_eq!(back.buffer, 64);
}

#[test]
fn corrupt_session_is_skipped_and_the_rest_kept() {
    let raw = r#"(sessions:[(name:"Good one",rate:48000,buffer:64),(name:12,rate:"fast"),"junk",(name:"Good two")],session_name:"Mine",buffer_size:256,plugins:[(path:"C:/A.vst3",name:"A")])"#;
    let loaded = load_preferences(Some(raw), BACKUP);
    let preferences = loaded.preferences.expect("readable");
    let names: Vec<_> = preferences
        .sessions
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    assert_eq!(names, ["Good one", "Good two"]);
    assert_eq!(preferences.sessions[0].buffer, 64);
    assert_eq!(preferences.session_name, "Mine");
    assert_eq!(preferences.buffer_size, 256);
    assert_eq!(preferences.paths[0].plugins.len(), 1);
    assert_eq!(loaded.notices.len(), 1);
    assert!(
        loaded.notices[0].contains("Skipped 2 saved session"),
        "{}",
        loaded.notices[0]
    );
    // The skipped entries would vanish on the next save: keep the original.
    assert_eq!(loaded.backup, Some((BACKUP.to_owned(), raw.to_owned())));
}

#[test]
fn corrupt_plugin_entry_is_skipped_and_missing_name_filled_in() {
    let raw = r#"(sessions:[(name:"Clean")],plugins:[(path:"C:/Plugins/Amp.vst3",format:clap,name:"Amp",mix:0.75),(name:"No path"),(path:"C:/B.vst3",name:"B",mix:"loud"),(path:"C:/Plugins/Verb.vst3")])"#;
    let loaded = load_preferences(Some(raw), BACKUP);
    let preferences = loaded.preferences.expect("readable");
    assert_eq!(preferences.sessions.len(), 1);
    let plugins = &preferences.paths[0].plugins;
    assert_eq!(plugins.len(), 2);
    assert_eq!(plugins[0].name, "Amp");
    assert_eq!(plugins[0].format, PluginFormat::Clap);
    assert_eq!(plugins[0].mix, 0.75);
    assert_eq!(plugins[1].name, "Verb");
    assert_eq!(plugins[1].mix, 1.0);
    assert!(
        loaded.notices[0].contains("2 plugin effect"),
        "{}",
        loaded.notices[0]
    );
    assert!(loaded.backup.is_some());
}

#[test]
fn unreadable_preferences_are_kept_not_overwritten() {
    let raw = r#"(sessions:[(name:"Clean")],sample_rate:"#;
    let mut storage = MemoryStorage::default();
    eframe::Storage::set_string(&mut storage, PREFERENCES_KEY, raw.to_owned());

    let mut loaded = load_preferences(
        eframe::Storage::get_string(&storage, PREFERENCES_KEY).as_deref(),
        BACKUP,
    );
    assert!(loaded.preferences.is_none());
    assert_eq!(loaded.notices.len(), 1);
    assert!(loaded.notices[0].contains(BACKUP));

    // The app's autosave with the defaults it fell back to.
    store_preferences(&mut storage, &Preferences::default(), &mut loaded.backup);
    assert_eq!(storage.0.get(BACKUP).map(String::as_str), Some(raw));
    let saved = storage.0.get(PREFERENCES_KEY).expect("saved");
    assert!(load_preferences(Some(saved), BACKUP).preferences.is_some());

    // Written once: later saves do not touch the copy again.
    assert!(loaded.backup.is_none());
    storage
        .0
        .insert(BACKUP.to_owned(), "moved by the user".to_owned());
    store_preferences(&mut storage, &Preferences::default(), &mut loaded.backup);
    assert_eq!(storage.0[BACKUP], "moved by the user");
}

#[test]
fn newer_layout_keeps_a_copy() {
    let raw = r#"(schema_version:99,session_name:"Future",some_new_field:[1,2])"#;
    let loaded = load_preferences(Some(raw), BACKUP);
    let preferences = loaded.preferences.expect("readable");
    assert_eq!(preferences.session_name, "Future");
    assert_eq!(preferences.schema_version, PREFERENCES_SCHEMA);
    assert_eq!(loaded.notices.len(), 1);
    assert!(loaded.backup.is_some());
}

#[test]
fn old_pages_open_the_console() {
    for key in [
        "live",
        "mixer",
        "routing",
        "plugins",
        "sessions",
        "diagnostics",
        " Diagnostics ",
        "console",
        "a page from a newer build",
    ] {
        assert_eq!(page_from_key(key), Page::Console, "{key}");
    }
    assert_eq!(page_key(Page::Console), "console");
    assert_eq!(page_key(Page::Tuner), "tuner");
    assert_eq!(page_from_key(&Preferences::default().page), Page::Console);
}

#[test]
fn old_devices_and_guide_pages_open_setup() {
    assert_eq!(page_from_key("devices"), Page::Setup);
    assert_eq!(page_from_key(" Devices "), Page::Setup);
    assert_eq!(page_from_key("guide"), Page::Setup);
    assert_eq!(page_from_key("setup"), Page::Setup);
    assert_eq!(page_key(Page::Setup), "setup");
}

fn output_devices() -> (OutputDeviceSettings, Vec<ExtraOutputPref>) {
    let clock = OutputDeviceSettings {
        level_db: -6.0,
        mute: true,
        path_map: crate::audio::PathMapSettings::first(ChannelMap {
            left: 0b0101,
            right: 0b1010,
        }),
        unused: UnusedChannels::Silent,
        align: false,
        click_mask: 0,
        ..OutputDeviceSettings::default()
    };
    let extra_outputs = vec![
        ExtraOutputPref {
            id: "hw:Phones".to_owned(),
            backend: "ALSA".to_owned(),
            name: "Phones".to_owned(),
            settings: OutputDeviceSettings {
                level_db: -3.0,
                ..OutputDeviceSettings::default()
            },
            bridge_buffer_mode: 2,
            bridge_buffer_ms: 20.0,
            ..ExtraOutputPref::default()
        },
        ExtraOutputPref {
            settings: OutputDeviceSettings {
                mute: true,
                path_map: crate::audio::PathMapSettings::first(ChannelMap { left: 1, right: 1 }),
                ..OutputDeviceSettings::default()
            },
            ..ExtraOutputPref::virtual_output()
        },
    ];
    (clock, extra_outputs)
}

#[test]
fn learned_loudness_saves_and_sanitizes() {
    let mut settings = OutputDeviceSettings::default();
    settings.loudness.target_lufs = -20.0;
    settings.loudness.trim_db = 4.0;
    let text = ron::to_string(&settings).expect("saved");
    assert!(text.contains("trim_db:4.0"), "{text}");
    let back: OutputDeviceSettings = ron::from_str(&text).expect("loaded");
    assert_eq!(back.loudness, settings.loudness);

    let text = ron::to_string(&OutputDeviceSettings::default()).expect("saved");
    assert!(!text.contains("trim_db"), "{text}");

    settings.loudness.trim_db = f32::NAN;
    assert_eq!(settings.sanitized().loudness.trim_db, 0.0);
    settings.loudness.trim_db = 100.0;
    assert_eq!(
        settings.sanitized().loudness.trim_db,
        crate::audio::LOUDNESS_TRIM_MAX_DB
    );
}

#[test]
fn an_old_learned_trim_loads_into_the_output_trim() {
    let old: OutputDeviceSettings =
        ron::from_str("(loudness:(target_lufs:-14.0,learned_db:3.5))").expect("old save");
    assert_eq!(old.loudness.trim_db, 3.5);
    assert_eq!(old.loudness.target_lufs, -14.0);
}

#[test]
fn a_path_match_survives_a_save_and_stays_out_when_zero() {
    let pref = PathPref {
        match_db: 2.5,
        ..PathPref::default()
    };
    let text = ron::to_string(&pref).expect("saved");
    assert!(text.contains("match_db:2.5"), "{text}");
    let back: PathPref = ron::from_str(&text).expect("loaded");
    assert_eq!(back.match_db, 2.5);

    let text = ron::to_string(&PathPref::default()).expect("saved");
    assert!(!text.contains("match_db"), "{text}");
}

#[test]
fn an_old_per_output_match_trim_loads() {
    let raw = r#"(schema_version:2,clock_device:(path_map:[(level_db:-2.0,match_db:4.5)]))"#;
    let loaded = load_preferences(Some(raw), BACKUP);
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    let preferences = loaded.preferences.expect("readable");
    assert_eq!(preferences.clock_output.path_map[0].level_db, -2.0);
}

#[test]
fn old_saves_without_output_devices_use_defaults() {
    let raw = r#"(output_id:"hw:Speaker",output_gain:-6.0,mute:true,sessions:[(name:"Old",output_id:"hw:Speaker")])"#;
    let loaded = load_preferences(Some(raw), BACKUP);
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    let preferences = loaded.preferences.expect("readable");
    let today = OutputDeviceSettings {
        level_db: 0.0,
        mute: false,
        path_map: crate::audio::PathMapSettings::first(ChannelMap {
            left: 0b01,
            right: 0b10,
        }),
        unused: UnusedChannels::MonoCopy,
        // Files from before the switch existed align.
        align: true,
        click_mask: 0,
        ..OutputDeviceSettings::default()
    };
    assert_eq!(OutputDeviceSettings::default(), today);
    assert_eq!(today.loudness.trim_db, 0.0);
    assert_eq!(preferences.clock_output, today);
    assert_eq!(preferences.clock_output.loudness.trim_db, 0.0);
    assert_eq!(
        preferences.clock_output.loudness.target_lufs,
        crate::audio::LOUDNESS_TARGET_DEFAULT_LUFS
    );
    assert!(preferences.extra_outputs.is_empty());
    // The master stays where it was.
    assert_eq!(preferences.output_gain, -6.0);
    assert!(preferences.mute);
    assert_eq!(preferences.sessions[0].clock_output, today);
    assert!(preferences.sessions[0].extra_outputs.is_empty());
}

#[test]
fn unknown_unused_channel_mode_reads_as_mono_copy() {
    let raw = r#"(session_name:"Mine",sample_rate:96000,clock_device:(level_db:-2.0,unused:loud),followers:[(id:"hw:A",settings:(unused:silent))])"#;
    let loaded = load_preferences(Some(raw), BACKUP);
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    let preferences = loaded.preferences.expect("readable");
    assert_eq!(preferences.clock_output.unused, UnusedChannels::MonoCopy);
    assert_eq!(preferences.clock_output.level_db, -2.0);
    assert_eq!(
        preferences.extra_outputs[0].settings.unused,
        UnusedChannels::Silent
    );
    assert_eq!(preferences.session_name, "Mine");
    assert_eq!(preferences.sample_rate, 96_000);
}

#[test]
fn output_devices_round_trip_in_preferences_and_sessions() {
    let (clock, extra_outputs) = output_devices();
    let mut session = session("Two outs");
    session.clock_output = clock;
    session.extra_outputs = extra_outputs.clone();
    let preferences = Preferences {
        clock_output: clock,
        extra_outputs: extra_outputs.clone(),
        sessions: vec![session],
        ..Preferences::default()
    };
    let loaded = load_preferences(Some(&stored(&preferences)), BACKUP);
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    let back = loaded.preferences.expect("readable");
    assert_eq!(back.clock_output, clock);
    assert_eq!(back.sessions[0].clock_output, clock);
    #[cfg(target_os = "windows")]
    {
        // Virtual-out clears the PipeWire sentinel; rematch fills WASAPI later.
        assert_eq!(back.extra_outputs.len(), extra_outputs.len());
        assert_eq!(back.extra_outputs[0], extra_outputs[0]);
        assert!(back.extra_outputs[1].virtual_output && back.extra_outputs[1].virtual_output_on);
        assert!(back.extra_outputs[1].id.is_empty());
        assert_eq!(back.extra_outputs[1].name, VIRTUAL_OUTPUT_NAME);
        assert_eq!(back.sessions[0].extra_outputs.len(), extra_outputs.len());
        assert!(back.sessions[0].extra_outputs[1].id.is_empty());
    }
    #[cfg(not(target_os = "windows"))]
    {
        assert_eq!(back.extra_outputs, extra_outputs);
        assert_eq!(back.sessions[0].extra_outputs, extra_outputs);
    }
}

#[test]
fn unreadable_extra_output_is_skipped_and_the_rest_kept() {
    let raw = r#"(
            clock_device:(level_db:NaN,map:(left:3)),
            sessions:[
                (name:"Hot",clock_device:(level_db:20.0)),
                (name:"P",followers:[(id:7),(id:"hw:D",settings:(level_db:-100.0))]),
            ],
            followers:[
                (id:"hw:A",name:"A",settings:(unused:silent)),
                (id:"hw:B",settings:(mute:"yes")),
                "junk",
                (id:"hw:C",cushion_ms:inf),
            ],
        )"#;
    let loaded = load_preferences(Some(raw), BACKUP);
    let preferences = loaded.preferences.expect("readable");
    let ids: Vec<_> = preferences
        .extra_outputs
        .iter()
        .map(|f| f.id.as_str())
        .collect();
    assert_eq!(ids, ["hw:A", "hw:C"]);
    assert_eq!(
        preferences.extra_outputs[0].settings.unused,
        UnusedChannels::Silent
    );
    assert_eq!(
        preferences.extra_outputs[1].bridge_buffer_ms,
        BRIDGE_BUFFER_MS_MAX
    );
    assert_eq!(preferences.clock_output.level_db, 0.0);
    assert_eq!(preferences.clock_output.map(0).left, 3);
    assert_eq!(preferences.clock_output.map(0).right, 0b10);
    assert_eq!(preferences.sessions[0].clock_output.level_db, 6.0);
    let session = &preferences.sessions[1].extra_outputs;
    assert_eq!(session.len(), 1);
    assert_eq!(session[0].settings.level_db, -36.0);
    assert!(
        loaded.notices[0].starts_with("Skipped 2 output device(s) that could not be read."),
        "{}",
        loaded.notices[0]
    );
    assert!(loaded.backup.is_some());
}

#[test]
fn record_the_click_defaults_off_and_round_trips() {
    assert!(!Preferences::default().record_click);
    let old = load_preferences(Some(r#"(session_name:"Old")"#), BACKUP)
        .preferences
        .expect("readable");
    assert!(!old.record_click, "an old save without the field loads off");
    let preferences = Preferences {
        record_click: true,
        ..Preferences::default()
    };
    let loaded = load_preferences(Some(&stored(&preferences)), BACKUP);
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    assert!(loaded.preferences.expect("readable").record_click);
}

#[test]
fn nothing_stored_is_quiet() {
    let loaded = load_preferences(None, BACKUP);
    assert!(loaded.preferences.is_none());
    assert!(loaded.backup.is_none());
    assert!(loaded.notices.is_empty());
}

#[test]
fn old_extra_outputs_without_publish_default_off() {
    let raw = r#"(followers:[(id:"hw:A",name:"A")])"#;
    let loaded = load_preferences(Some(raw), BACKUP);
    let preferences = loaded.preferences.expect("readable");
    assert_eq!(preferences.extra_outputs.len(), 1);
    assert!(!preferences.extra_outputs[0].virtual_output);
    assert!(!preferences.extra_outputs[0].virtual_output_on);
}

#[test]
fn virtual_out_and_publish_round_trip() {
    let preferences = Preferences {
        extra_outputs: vec![ExtraOutputPref::virtual_output()],
        ..Preferences::default()
    };
    let loaded = load_preferences(Some(&stored(&preferences)), BACKUP);
    let back = loaded.preferences.expect("readable");
    assert_eq!(back.extra_outputs.len(), 1);
    assert!(back.extra_outputs[0].virtual_output);
    assert!(back.extra_outputs[0].virtual_output_on);
    #[cfg(target_os = "windows")]
    {
        // PipeWire sentinel is cleared; inventory rematch fills the WASAPI id.
        assert!(back.extra_outputs[0].id.is_empty());
    }
    #[cfg(not(target_os = "windows"))]
    {
        assert_eq!(back.extra_outputs[0].id, VIRTUAL_OUTPUT_ID);
    }
    assert_eq!(back.extra_outputs[0].name, VIRTUAL_OUTPUT_NAME);
}

#[test]
fn extra_outputs_past_the_cap_are_dropped() {
    let entries: String = (0..=MAX_EXTRA_OUTPUTS)
        .map(|index| format!(r#"(id:"hw:{index}",name:"D{index}")"#))
        .collect::<Vec<_>>()
        .join(",");
    let raw = format!("(followers:[{entries}])");
    let loaded = load_preferences(Some(&raw), BACKUP);
    let preferences = loaded.preferences.expect("readable");
    assert_eq!(preferences.extra_outputs.len(), MAX_EXTRA_OUTPUTS);
    assert_eq!(preferences.extra_outputs[0].id, "hw:0");
    assert_eq!(
        preferences.extra_outputs[MAX_EXTRA_OUTPUTS - 1].id,
        format!("hw:{}", MAX_EXTRA_OUTPUTS - 1)
    );
}

#[test]
fn a_second_virtual_out_is_dropped() {
    let raw = r#"(followers:[
            (id:"hw:A",name:"A"),
            (virtual_out:true,publish:true),
            (virtual_out:true,publish:false,name:"Other"),
        ])"#;
    let loaded = load_preferences(Some(raw), BACKUP);
    let preferences = loaded.preferences.expect("readable");
    assert_eq!(preferences.extra_outputs.len(), 2);
    assert!(!preferences.extra_outputs[0].virtual_output);
    assert!(preferences.extra_outputs[1].virtual_output);
    assert!(preferences.extra_outputs[1].virtual_output_on);
    #[cfg(target_os = "windows")]
    {
        assert!(preferences.extra_outputs[1].id.is_empty());
    }
    #[cfg(not(target_os = "windows"))]
    {
        assert_eq!(preferences.extra_outputs[1].id, VIRTUAL_OUTPUT_ID);
    }
}

#[test]
fn windows_virtual_out_keeps_wasapi_id_through_sanitize() {
    let pref = ExtraOutputPref {
        id: "wasapi:{cable-input}".to_owned(),
        backend: "WASAPI".to_owned(),
        name: "CABLE Input (VB-Audio Virtual Cable)".to_owned(),
        virtual_output: true,
        virtual_output_on: true,
        ..ExtraOutputPref::default()
    }
    .sanitized();
    #[cfg(target_os = "windows")]
    {
        assert_eq!(pref.id, "wasapi:{cable-input}");
        assert_eq!(pref.backend, "WASAPI");
    }
    #[cfg(not(target_os = "windows"))]
    {
        // PipeWire RigWerk Out always uses the sentinel id.
        assert_eq!(pref.id, VIRTUAL_OUTPUT_ID);
        assert_eq!(pref.backend, "PipeWire");
    }
    assert!(pref.virtual_output && pref.virtual_output_on);
    assert_eq!(pref.name, "CABLE Input (VB-Audio Virtual Cable)");
}

#[test]
fn windows_virtual_out_drops_pipewire_sentinel_on_sanitize() {
    let pref = ExtraOutputPref {
        id: VIRTUAL_OUTPUT_ID.to_owned(),
        virtual_output: true,
        virtual_output_on: true,
        ..ExtraOutputPref::default()
    }
    .sanitized();
    #[cfg(target_os = "windows")]
    {
        assert!(pref.id.is_empty());
        assert_eq!(pref.backend, "WASAPI");
        assert_eq!(pref.name, VIRTUAL_OUTPUT_NAME);
    }
    #[cfg(not(target_os = "windows"))]
    {
        assert_eq!(pref.id, VIRTUAL_OUTPUT_ID);
        assert_eq!(pref.backend, "PipeWire");
    }
}

#[test]
fn windows_virtual_out_rematch_binds_vb_cable_from_inventory() {
    use crate::audio::DeviceItem;

    let cable = DeviceItem {
        id: "wasapi:{live-cable}".to_owned(),
        name: "CABLE Input (VB-Audio Virtual Cable)".to_owned(),
        backend: "WASAPI".to_owned(),
        channels: 2,
        demo: false,
        preferred: false,
        asio_clsid: None,
    };
    let mut extra_outputs = vec![ExtraOutputPref {
        id: VIRTUAL_OUTPUT_ID.to_owned(),
        virtual_output: true,
        virtual_output_on: true,
        ..ExtraOutputPref::default()
    }
    .sanitized()];
    rematch_windows_virtual_output(&mut extra_outputs, std::slice::from_ref(&cable));
    #[cfg(target_os = "windows")]
    {
        assert_eq!(extra_outputs[0].id, cable.id);
        assert_eq!(extra_outputs[0].backend, "WASAPI");
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = cable;
        assert_eq!(extra_outputs[0].id, VIRTUAL_OUTPUT_ID);
    }
}

#[test]
fn wasapi_mode_migrates_from_legacy_bool() {
    let raw_on = r#"(low_latency_output:true,sample_rate:48000)"#;
    let on = load_preferences(Some(raw_on), BACKUP)
        .preferences
        .expect("readable");
    assert_eq!(on.wasapi_output_mode, Some(WasapiOutputMode::RealStyle));
    assert!(on.low_latency_output);

    let raw_off = r#"(low_latency_output:false,sample_rate:48000)"#;
    let off = load_preferences(Some(raw_off), BACKUP)
        .preferences
        .expect("readable");
    assert_eq!(off.wasapi_output_mode, Some(WasapiOutputMode::Shared));
    assert!(!off.low_latency_output);
}

#[test]
fn wasapi_exclusive_mode_round_trips() {
    let preferences = Preferences {
        low_latency_output: false,
        wasapi_output_mode: Some(WasapiOutputMode::Exclusive),
        sessions: {
            let mut session = session("Exclusive");
            session.low_latency_output = false;
            session.wasapi_output_mode = Some(WasapiOutputMode::Exclusive);
            vec![session]
        },
        ..Preferences::default()
    };
    let loaded = load_preferences(Some(&stored(&preferences)), BACKUP);
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    let back = loaded.preferences.expect("readable");
    assert_eq!(back.wasapi_output_mode, Some(WasapiOutputMode::Exclusive));
    assert!(!back.low_latency_output);
    assert_eq!(
        back.sessions[0].wasapi_output_mode,
        Some(WasapiOutputMode::Exclusive)
    );
    assert_eq!(
        resolved_wasapi_output_mode(&back),
        WasapiOutputMode::Exclusive
    );
}

#[test]
fn wasapi_mode_from_low_latency_bool() {
    assert_eq!(
        WasapiOutputMode::from_low_latency(true),
        WasapiOutputMode::RealStyle
    );
    assert_eq!(
        WasapiOutputMode::from_low_latency(false),
        WasapiOutputMode::Shared
    );
    assert!(WasapiOutputMode::RealStyle.wants_low_latency());
    assert!(WasapiOutputMode::Exclusive.wants_low_latency());
    assert!(!WasapiOutputMode::Shared.wants_low_latency());
    assert_eq!(WasapiOutputMode::Exclusive.label(), "Exclusive");
}

#[test]
fn an_old_file_loads_as_one_path() {
    let raw = r#"(
            mono:true,
            mute:true,
            routes:((false,true,false,false),(true,false,false,false)),
            output_channels:(true,false),
            eq:(enabled:false),
            gate:(enabled:true),
            plugins:[(path:"C:/Plugins/Amp.vst3",name:"Amp")],
            clock_device:(level_db:-3.0,map:(left:4,right:8)),
            followers:[(id:"hw:A",settings:(map:(left:3,right:0)))],
            sessions:[(name:"Old",mono:true,routes:((true,true,false,false),(false,false,false,false)),plugins:[(path:"C:/B.clap",format:clap,name:"B")],clock_device:(map:(left:2,right:1)))],
        )"#;
    let loaded = load_preferences(Some(raw), BACKUP);
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    let preferences = loaded.preferences.expect("readable");
    assert_eq!(preferences.paths.len(), 1);
    let path = &preferences.paths[0];
    assert_eq!(path.name, "Main");
    assert!(path.mono);
    assert_eq!(
        path.routes,
        [[false, true, false, false], [true, false, false, false]]
    );
    assert_eq!(path.output_channels, [true, false]);
    assert!(!path.eq.enabled);
    assert!(path.gate.enabled);
    assert_eq!(path.plugins[0].name, "Amp");
    assert_eq!((path.level_db, path.mute, path.solo), (0.0, false, false));
    // The master mute stays the master's.
    assert!(preferences.mute);
    assert_eq!(
        preferences.clock_output.map(0),
        ChannelMap { left: 4, right: 8 }
    );
    assert_eq!(preferences.clock_output.level_db, -3.0);
    assert_eq!(
        preferences.extra_outputs[0].settings.map(0),
        ChannelMap { left: 3, right: 0 }
    );
    // The legacy fields are cleared once migrated.
    assert!(preferences.plugins.is_empty());
    assert!(!preferences.mono);

    let old = &preferences.sessions[0];
    assert_eq!(old.paths.len(), 1);
    assert!(old.paths[0].mono);
    assert_eq!(old.paths[0].routes[0], [true, true, false, false]);
    assert_eq!(old.paths[0].plugins[0].name, "B");
    // What the session never stored comes from the first path.
    assert_eq!(old.paths[0].gate, path.gate);
    assert_eq!(old.clock_output.map(0), ChannelMap { left: 2, right: 1 });
}

/// Three paths with their own chains, and each device with its own map,
/// level, polarity and trim per path.
fn three_paths() -> (Vec<PathPref>, OutputDeviceSettings, Vec<ExtraOutputPref>) {
    let mut eq = EqSettings::default();
    eq.bands[2].gain_db = 4.5;
    let paths = vec![
        PathPref {
            name: "Clean".to_owned(),
            plugins: vec![plugin("C:/Plugins/Comp.vst3", PluginFormat::Vst3)],
            ..PathPref::default()
        },
        PathPref {
            name: "Drive".to_owned(),
            colour: 1,
            routes: [[true, true, false, false], [false, false, false, false]],
            mono: true,
            output_channels: [true, false],
            eq,
            gate: GateSettings {
                enabled: true,
                ..GateSettings::default()
            },
            plugins: vec![
                plugin("C:/Plugins/Amp.clap", PluginFormat::Clap),
                plugin("C:/Plugins/Cab.vst3", PluginFormat::Vst3),
            ],
            level_db: -4.0,
            mute: true,
            ..PathPref::default()
        },
        PathPref {
            name: "Stream".to_owned(),
            colour: 2,
            eq_post: eq,
            solo: true,
            ..PathPref::default()
        },
    ];
    let mut clock = OutputDeviceSettings::default();
    clock.path_map[1] = crate::audio::PathMapSettings {
        map: ChannelMap {
            left: 0b0100,
            right: 0b1000,
        },
        level_db: -6.0,
        invert: true,
        trim_ms: 1.5,
    };
    clock.path_map[2] = crate::audio::PathMapSettings::unrouted();
    let mut stream = ExtraOutputPref::virtual_output();
    stream.settings.path_map[0] = crate::audio::PathMapSettings::unrouted();
    stream.settings.path_map[2].level_db = -2.0;
    // A generic extra output with its own bridge buffer and a per-path map.
    let mut phones = ExtraOutputPref {
        id: "hw:Phones".to_owned(),
        backend: "ALSA".to_owned(),
        name: "Phones".to_owned(),
        bridge_buffer_mode: 1,
        bridge_buffer_ms: 12.5,
        ..ExtraOutputPref::default()
    };
    phones.settings.path_map[1] = crate::audio::PathMapSettings::unrouted();
    phones.settings.path_map[2].invert = true;
    // Expect what a load gives on this platform (Windows drops the PipeWire
    // RigWerk Out id).
    (paths, clock, vec![stream.sanitized(), phones.sanitized()])
}

#[test]
fn three_paths_round_trip_in_preferences() {
    let (paths, clock_output, extra_outputs) = three_paths();
    let preferences = Preferences {
        paths: paths.clone(),
        selected_path: 2,
        clock_output,
        extra_outputs: extra_outputs.clone(),
        wasapi_output_mode: Some(WasapiOutputMode::Exclusive),
        ..Preferences::default()
    };
    let raw = stored(&preferences);
    // New files write paths only, no legacy mix fields.
    let value: ron::Value = ron::from_str(&raw).expect("ron");
    let ron::Value::Map(top) = value else {
        panic!("a struct: {raw}");
    };
    for legacy in [
        "routes",
        "mono",
        "output_channels",
        "eq",
        "eq_post",
        "gate",
        "plugins",
    ] {
        assert!(
            top.get(&ron::Value::String(legacy.to_owned())).is_none(),
            "{legacy} written"
        );
    }
    let loaded = load_preferences(Some(&raw), BACKUP);
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    let back = loaded.preferences.expect("readable");
    assert_eq!(back.paths, paths);
    assert_eq!(back.selected_path, 2);
    assert_eq!(back.clock_output, clock_output);
    assert_eq!(back.extra_outputs, extra_outputs);
    assert_eq!(back.wasapi_output_mode, Some(WasapiOutputMode::Exclusive));
}

#[test]
fn three_paths_round_trip_in_a_session() {
    let (paths, clock_output, extra_outputs) = three_paths();
    let mut session = session("Three");
    session.eq = None;
    session.paths = paths.clone();
    session.clock_output = clock_output;
    session.extra_outputs = extra_outputs.clone();
    let preferences = Preferences {
        sessions: vec![session],
        ..Preferences::default()
    };
    let loaded = load_preferences(Some(&stored(&preferences)), BACKUP);
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    let back = &loaded.preferences.expect("readable").sessions[0];
    assert_eq!(back.paths, paths);
    assert_eq!(back.clock_output, clock_output);
    assert_eq!(back.extra_outputs, extra_outputs);
}

#[test]
fn a_one_path_session_round_trips() {
    let mut session = session("One");
    session.paths = vec![PathPref {
        routes: [[false, true, false, false], [true, false, false, false]],
        ..PathPref::default()
    }];
    let preferences = Preferences {
        sessions: vec![session.clone()],
        ..Preferences::default()
    };
    let loaded = load_preferences(Some(&stored(&preferences)), BACKUP);
    let back = &loaded.preferences.expect("readable").sessions[0];
    assert_eq!(back.paths, session.paths);
    assert_eq!(back.clock_output, OutputDeviceSettings::default());
}

#[test]
fn paths_are_capped_and_sanitized_on_load() {
    let raw = r#"(paths:[(name:"",level_db:NaN,colour:9),(level_db:99.0),(),(),(name:"Fifth")],selected_path:7)"#;
    let preferences = load_preferences(Some(raw), BACKUP)
        .preferences
        .expect("readable");
    assert_eq!(preferences.paths.len(), crate::audio::MAX_PATHS);
    assert_eq!(preferences.paths[0].name, "Main");
    assert_eq!(preferences.paths[0].level_db, 0.0);
    assert_eq!(preferences.paths[0].colour, 1);
    assert_eq!(
        preferences.paths[1].level_db,
        crate::audio::PATH_LEVEL_MAX_DB
    );
    assert_eq!(preferences.selected_path, 3);
}

fn measured_latency() -> MeasuredLatency {
    MeasuredLatency {
        latency_ms: 21.5,
        samples: 1_032,
        input_id: "asio:focusrite".to_owned(),
        input_name: "Focusrite USB".to_owned(),
        output_id: "wasapi:{speakers}".to_owned(),
        output_name: "Speakers".to_owned(),
        input_rate: 48_000,
        output_rate: 44_100,
        input_buffer: 128,
        output_step: 256,
        output_mode: "real-style".to_owned(),
        measured_at: 1_700_000_000,
        confidence: 0.93,
        loopback: LoopbackKind::Microphone,
    }
}

#[test]
fn a_measured_latency_round_trips_with_its_conditions() {
    let preferences = Preferences {
        measured_latency: Some(measured_latency()),
        ..Preferences::default()
    };
    let loaded = load_preferences(Some(&stored(&preferences)), BACKUP);
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    let back = loaded.preferences.expect("readable");
    assert_eq!(back.measured_latency, Some(measured_latency()));
}

#[test]
fn an_older_save_loads_without_a_measurement() {
    let loaded = load_preferences(Some("(auto_start_bridge:true)"), BACKUP);
    let preferences = loaded.preferences.expect("readable");
    assert!(preferences.auto_start_bridge);
    assert_eq!(preferences.measured_latency, None);
}

#[test]
fn a_newer_save_is_read_through_field_defaults() {
    let raw = r#"(measured_latency: Some((latency_ms: 12.5, samples: 600, input_id: "in", output_id: "out", input_rate: 48000, output_rate: 48000)))"#;
    let loaded = load_preferences(Some(raw), BACKUP);
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    let measured = loaded
        .preferences
        .expect("readable")
        .measured_latency
        .expect("measurement");
    assert_eq!(measured.latency_ms, 12.5);
    assert_eq!(measured.samples, 600);
    // Missing fields keep their defaults.
    assert_eq!(measured.input_buffer, 0);
    assert_eq!(measured.loopback, LoopbackKind::Cable);
    assert_eq!(measured.confidence, 0.0);
}

#[test]
fn a_damaged_measurement_is_skipped_not_fatal() {
    let raw =
        r#"(auto_start_bridge: true, measured_latency: Some((latency_ms: "hot", samples: 12)))"#;
    let loaded = load_preferences(Some(raw), BACKUP);
    let preferences = loaded.preferences.expect("the rest still loads");
    assert!(preferences.auto_start_bridge);
    assert_eq!(preferences.measured_latency, None);
    // The drop is reported like a skipped session, and the original is kept.
    assert!(
        loaded
            .notices
            .iter()
            .any(|notice| notice.contains("1 measurement(s)")),
        "{:?}",
        loaded.notices
    );
    assert!(loaded.backup.is_some());
}

#[test]
fn an_impossible_measurement_is_dropped_and_reported() {
    let raw = r#"(measured_latency: Some((latency_ms: 12.5, samples: 0)))"#;
    let loaded = load_preferences(Some(raw), BACKUP);
    assert_eq!(loaded.preferences.expect("readable").measured_latency, None);
    assert!(
        loaded
            .notices
            .iter()
            .any(|notice| notice.contains("1 measurement(s)")),
        "{:?}",
        loaded.notices
    );
}

#[test]
fn a_cap_dropped_output_is_not_reported_as_unreadable() {
    let mut preferences = Preferences::default();
    for index in 0..(MAX_EXTRA_OUTPUTS + 1) {
        preferences.extra_outputs.push(ExtraOutputPref {
            id: format!("out:{index}"),
            name: format!("Out {index}"),
            ..ExtraOutputPref::default()
        });
    }
    // A duplicate virtual out goes through the same floor rule.
    preferences
        .extra_outputs
        .push(ExtraOutputPref::virtual_output());
    preferences
        .extra_outputs
        .push(ExtraOutputPref::virtual_output());

    let loaded = load_preferences(Some(&stored(&preferences)), BACKUP);
    let back = loaded.preferences.expect("readable");
    assert_eq!(back.extra_outputs.len(), MAX_EXTRA_OUTPUTS);
    assert!(
        loaded
            .notices
            .iter()
            .all(|notice| !notice.contains("output device")),
        "{:?}",
        loaded.notices
    );
}

#[test]
fn tuner_settings_round_trip() {
    let preferences = Preferences {
        tuner_tuning: "drop-c".to_owned(),
        tuner_reference_hz: 432.0,
        tuner_channel: 2,
        tuner_display: 1,
        tuner_listen: 1,
        tuner_auto_mute: false,
        tuner_tolerance: 2,
        ..Preferences::default()
    };
    let loaded = load_preferences(Some(&stored(&preferences)), BACKUP);
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    let back = loaded.preferences.expect("readable");
    assert_eq!(back.tuner_tuning, "drop-c");
    assert_eq!(back.tuner_reference_hz, 432.0);
    assert_eq!(back.tuner_channel, 2);
    assert_eq!(back.tuner_display, 1);
    assert_eq!(back.tuner_listen, 1);
    assert!(!back.tuner_auto_mute);
    assert_eq!(back.tuner_tolerance, 2);
    assert_eq!(back.schema_version, PREFERENCES_SCHEMA);
}

#[test]
fn missing_tuner_fields_use_the_page_defaults() {
    let raw = r#"(session_name:"Old rig")"#;
    let loaded = load_preferences(Some(raw), BACKUP);
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    let preferences = loaded.preferences.expect("readable");
    assert_eq!(preferences.tuner_tuning, crate::audio::DEFAULT_PRESET_ID);
    assert_eq!(preferences.tuner_reference_hz, 440.0);
    assert_eq!(preferences.tuner_channel, 0);
    assert_eq!(preferences.tuner_display, 0);
    assert_eq!(preferences.tuner_listen, 0);
    assert!(preferences.tuner_auto_mute);
    assert_eq!(preferences.tuner_tolerance, 1);
}

#[test]
fn tuner_reference_is_sanitized_on_load() {
    let high = load_preferences(Some(r#"(tuner_reference_hz:500.0)"#), BACKUP)
        .preferences
        .expect("readable");
    assert_eq!(high.tuner_reference_hz, crate::app::tuner::REFERENCE_MAX_HZ);
    let low = load_preferences(Some(r#"(tuner_reference_hz:400.0)"#), BACKUP)
        .preferences
        .expect("readable");
    assert_eq!(low.tuner_reference_hz, crate::app::tuner::REFERENCE_MIN_HZ);
    let nan = load_preferences(Some(r#"(tuner_reference_hz:NaN)"#), BACKUP)
        .preferences
        .expect("readable");
    assert_eq!(
        nan.tuner_reference_hz,
        crate::app::tuner::DEFAULT_REFERENCE_HZ
    );
}

#[test]
fn tuner_page_key_round_trips() {
    assert_eq!(page_key(Page::Tuner), "tuner");
    assert!(page_from_key("tuner") == Page::Tuner);
    assert!(page_from_key(" Tuner ") == Page::Tuner);
}

#[test]
fn chain_bypass_round_trips_and_an_old_path_stays_off() {
    let mut running = plugin("C:/Plugins/Amp.vst3", PluginFormat::Vst3);
    running.bypass = false;
    let bypassed = plugin("C:/Plugins/Verb.vst3", PluginFormat::Vst3);
    let preferences = Preferences {
        paths: vec![PathPref {
            name: "Main".to_owned(),
            chain_bypass: true,
            plugins: vec![running, bypassed],
            ..PathPref::default()
        }],
        ..Preferences::default()
    };
    let loaded = load_preferences(Some(&stored(&preferences)), BACKUP);
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    let back = loaded.preferences.expect("readable");
    assert_eq!(back.schema_version, PREFERENCES_SCHEMA);
    let path = &back.paths[0];
    assert!(path.chain_bypass);
    assert!(!path.plugins[0].bypass);
    assert!(path.plugins[1].bypass);

    // A save from before the field: the path loads, chain bypass stays off,
    // and the effect's own bypass is still there.
    let raw = r#"(paths:[(name:"Main",plugins:[(path:"A.vst3",name:"Amp",bypass:true)])])"#;
    let loaded = load_preferences(Some(raw), BACKUP);
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    let old = loaded.preferences.expect("readable");
    assert_eq!(old.schema_version, PREFERENCES_SCHEMA);
    assert!(!old.paths[0].chain_bypass);
    assert!(old.paths[0].plugins[0].bypass);
    assert_eq!(old.paths[0].plugins[0].name, "Amp");
}

#[test]
fn record_folder_missing_or_empty_sanitises_without_a_schema_bump() {
    assert_eq!(PREFERENCES_SCHEMA, 2);
    let expected = crate::app::record::default_record_folder();

    // A save from before the field existed still loads, and the layout
    // number stays 2: the field defaults, then an empty folder is filled in.
    let missing = load_preferences(Some(r#"(session_name:"Old")"#), BACKUP);
    assert!(missing.notices.is_empty(), "{:?}", missing.notices);
    let missing = missing.preferences.expect("readable");
    assert_eq!(missing.schema_version, PREFERENCES_SCHEMA);
    assert_eq!(missing.session_name, "Old");
    assert_eq!(missing.record_folder, expected);

    let empty = load_preferences(Some(r#"(record_folder:"")"#), BACKUP);
    assert!(empty.notices.is_empty(), "{:?}", empty.notices);
    assert_eq!(empty.preferences.expect("readable").record_folder, expected);

    let blank = load_preferences(Some(r#"(record_folder:"   ")"#), BACKUP);
    assert!(blank.notices.is_empty(), "{:?}", blank.notices);
    assert_eq!(blank.preferences.expect("readable").record_folder, expected);

    let custom = Preferences {
        record_folder: "  D:/Takes  ".to_owned(),
        ..Preferences::default()
    };
    let loaded = load_preferences(Some(&stored(&custom)), BACKUP);
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    let back = loaded.preferences.expect("readable");
    assert_eq!(back.schema_version, 2);
    assert_eq!(back.record_folder, "D:/Takes");
}

#[test]
fn preferences_round_trip_the_metronome_and_stay_on_schema_2() {
    assert_eq!(PREFERENCES_SCHEMA, 2);
    let preferences = Preferences {
        metronome_running: true,
        metronome_bpm: 100.0,
        metronome_rhythm: crate::audio::ClickRhythm::SixEight,
        metronome_level_db: -6.0,
        clock_output: OutputDeviceSettings {
            click_mask: 0b1100,
            ..OutputDeviceSettings::default()
        },
        extra_outputs: vec![ExtraOutputPref {
            settings: OutputDeviceSettings {
                click_mask: 0b0001,
                ..OutputDeviceSettings::default()
            },
            ..ExtraOutputPref::default()
        }],
        ..Preferences::default()
    };
    let text = stored(&preferences);
    // The transport is not written, so a launch cannot start the click.
    assert!(!text.contains("metronome_running"), "{text}");
    assert!(text.contains("metronome_bpm"), "{text}");
    let loaded = load_preferences(Some(&text), BACKUP);
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    let back = loaded.preferences.expect("readable");
    assert_eq!(back.schema_version, 2);
    assert!(!back.metronome_running);
    assert_eq!(back.metronome_bpm, 100.0);
    assert_eq!(back.metronome_rhythm, crate::audio::ClickRhythm::SixEight);
    assert_eq!(back.metronome_level_db, -6.0);
    assert_eq!(back.clock_output.click_mask, 0b1100);
    assert_eq!(back.extra_outputs[0].settings.click_mask, 0b0001);
}

#[test]
fn old_preferences_load_the_metronome_off() {
    let loaded = load_preferences(Some(r#"(session_name:"Old")"#), BACKUP);
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    let preferences = loaded.preferences.expect("readable");
    assert_eq!(preferences.schema_version, 2);
    assert!(!preferences.metronome_running);
    assert_eq!(preferences.metronome_bpm, 120.0);
    assert_eq!(
        preferences.metronome_rhythm,
        crate::audio::ClickRhythm::FourFour
    );
    assert_eq!(preferences.metronome_level_db, -12.0);
    assert_eq!(preferences.clock_output.click_mask, 0);
    assert!(preferences.extra_outputs.is_empty());
}

#[test]
fn a_save_that_wrote_the_metronome_running_loads_it_stopped() {
    let loaded = load_preferences(
        Some(r#"(schema_version:2,metronome_running:true,metronome_bpm:90.0)"#),
        BACKUP,
    );
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    assert!(loaded.backup.is_none());
    let preferences = loaded.preferences.expect("readable");
    assert!(!preferences.metronome_running);
    assert_eq!(preferences.metronome_bpm, 90.0);
}

#[test]
fn preferences_clamp_a_nan_tempo_and_an_unknown_rhythm() {
    let nan = load_preferences(
        Some(r#"(metronome_bpm:NaN,metronome_rhythm:waltz,metronome_level_db:NaN)"#),
        BACKUP,
    );
    assert!(nan.notices.is_empty(), "{:?}", nan.notices);
    let nan = nan.preferences.expect("readable");
    assert_eq!(nan.metronome_bpm, 120.0);
    assert_eq!(nan.metronome_rhythm, crate::audio::ClickRhythm::FourFour);
    assert_eq!(nan.metronome_level_db, -12.0);

    let hot = load_preferences(
        Some(r#"(metronome_bpm:999.0,metronome_level_db:12.0)"#),
        BACKUP,
    );
    let hot = hot.preferences.expect("readable");
    assert_eq!(hot.metronome_bpm, 240.0);
    assert_eq!(hot.metronome_rhythm, crate::audio::ClickRhythm::FourFour);
    assert_eq!(hot.metronome_level_db, 0.0);
}

#[test]
fn preferences_session_keeps_the_click_mask_and_not_the_transport() {
    let session = Session {
        clock_output: OutputDeviceSettings {
            click_mask: 0b1100,
            ..OutputDeviceSettings::default()
        },
        extra_outputs: vec![ExtraOutputPref::default()],
        ..Session::default()
    };
    let text = ron::to_string(&session).expect("session");
    assert!(text.contains("clock_device"), "{text}");
    assert!(text.contains("followers"), "{text}");
    assert!(!text.contains("metronome_bpm"), "{text}");
    assert!(!text.contains("metronome_running"), "{text}");
    assert!(!text.contains("metronome_rhythm"), "{text}");
    assert!(!text.contains("metronome_level_db"), "{text}");
    let back: Session = ron::from_str(&text).expect("session");
    assert_eq!(back.clock_output.click_mask, 0b1100);
}

#[test]
fn an_old_file_without_theme_or_size_loads_the_defaults() {
    let loaded = load_preferences(Some(r#"(schema_version:2,session_name:"Old")"#), BACKUP);
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    assert!(loaded.backup.is_none());
    let old = loaded.preferences.expect("readable");
    assert_eq!(old.theme, ThemeChoice::System);
    assert_eq!(old.ui_size, 1.0);
    assert_eq!(old.schema_version, PREFERENCES_SCHEMA);
}

#[test]
fn theme_and_size_round_trip() {
    let preferences = Preferences {
        theme: ThemeChoice::Light,
        ui_size: 1.3,
        ..Preferences::default()
    };
    let loaded = load_preferences(Some(&stored(&preferences)), BACKUP);
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    let loaded = loaded.preferences.expect("readable");
    assert_eq!(loaded.theme, ThemeChoice::Light);
    assert_eq!(loaded.ui_size, 1.3);
}

#[test]
fn ui_size_is_sanitized_on_load_and_an_unknown_theme_is_system() {
    for (raw, size) in [
        ("NaN", 1.0),
        ("inf", 1.5),
        ("-inf", 0.8),
        ("0.2", 0.8),
        ("4.0", 1.5),
        ("1.25", 1.25),
    ] {
        let loaded = load_preferences(Some(&format!("(ui_size:{raw})")), BACKUP);
        assert!(loaded.notices.is_empty(), "{raw}: {:?}", loaded.notices);
        assert_eq!(loaded.preferences.expect("readable").ui_size, size, "{raw}");
    }
    let loaded = load_preferences(Some("(theme:Sepia,ui_size:1.1)"), BACKUP);
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    let loaded = loaded.preferences.expect("readable");
    assert_eq!(loaded.theme, ThemeChoice::System);
    assert_eq!(loaded.ui_size, 1.1);
}

/// One output's settings with every stored field set by hand.
fn golden_device(level_db: f32, click_mask: u8) -> OutputDeviceSettings {
    let mut path_map = [crate::audio::PathMapSettings::default(); crate::audio::MAX_PATHS];
    path_map[0] = crate::audio::PathMapSettings {
        map: ChannelMap {
            left: 0b0101,
            right: 0b1010,
        },
        level_db: -2.5,
        invert: true,
        trim_ms: 1.25,
    };
    path_map[1] = crate::audio::PathMapSettings::unrouted();
    OutputDeviceSettings {
        level_db,
        mute: true,
        unused: UnusedChannels::Silent,
        path_map,
        align: false,
        click_mask,
        ..OutputDeviceSettings::default()
    }
}

fn golden_extra_output(virtual_output: bool) -> ExtraOutputPref {
    let (id, backend, name) = if virtual_output {
        (VIRTUAL_OUTPUT_ID, "PipeWire", VIRTUAL_OUTPUT_NAME)
    } else {
        ("hw:Phones", "ALSA", "Phones")
    };
    ExtraOutputPref {
        id: id.to_owned(),
        backend: backend.to_owned(),
        name: name.to_owned(),
        settings: golden_device(-3.0, 0b0011),
        bridge_buffer_mode: 2,
        bridge_buffer_ms: 12.5,
        virtual_output,
        virtual_output_on: virtual_output,
    }
}

fn golden_path(name: &str) -> PathPref {
    let is_return = name == "Drive";
    let mut eq = EqSettings {
        enabled: false,
        ..EqSettings::default()
    };
    eq.bands[1].gain_db = -3.5;
    eq.bands[2].on = false;
    let mut eq_post = EqSettings::default();
    eq_post.bands[4].q = 1.5;
    let mut verb = plugin("C:/Plugins/Verb.clap", PluginFormat::Clap);
    verb.state =
        ron::from_str(r#"Some((version: 1, data: "AAEC/w==", params: [(id: 7, value: 0.25)]))"#)
            .expect("state");
    PathPref {
        name: name.to_owned(),
        colour: 2,
        routes: [[true, false, true, false], [false, true, false, true]],
        mono: true,
        output_channels: [true, false],
        eq,
        eq_post,
        gate: GateSettings {
            enabled: true,
            slot: GateSlot::Pre,
            key: GateKey::Input,
            threshold_db: -45.0,
            range_db: -60.0,
            hysteresis_db: 6.0,
            attack_ms: 2.0,
            release_ms: 150.0,
            hold_ms: 20.0,
        },
        plugins: vec![
            PluginPref {
                path: "/usr/lib/vst3/Amp.vst3".to_owned(),
                format: PluginFormat::Vst3,
                class_id: None,
                name: "Amp".to_owned(),
                vendor: "Vendor".to_owned(),
                category: "Fx|Distortion".to_owned(),
                bypass: false,
                mix: 0.75,
                params: Vec::new(),
                state: None,
            },
            verb,
        ],
        chain_bypass: true,
        // Drive is a return (no solo, no sends); the others send to it.
        feed: if is_return {
            crate::audio::PathFeed::Sends
        } else {
            crate::audio::PathFeed::Input
        },
        sends: if is_return {
            Vec::new()
        } else {
            vec![crate::audio::SEND_OFF_DB, -12.0]
        },
        nam_blocks: Vec::new(),
        amp: crate::audio::LegacyNamSettings::default(),
        cab: crate::audio::CabSettings::default(),
        level_db: -4.0,
        match_db: 0.0,
        mute: true,
        solo: !is_return,
    }
}

fn golden_session() -> Session {
    Session {
        name: "Clean".to_owned(),
        note: "ASIO → WASAPI".to_owned(),
        input: "Focusrite USB ASIO".to_owned(),
        input_id: "asio:focusrite".to_owned(),
        input_backend: "ASIO".to_owned(),
        output: "Speakers".to_owned(),
        output_id: "wasapi:{speakers}".to_owned(),
        output_backend: "WASAPI".to_owned(),
        rate: 96_000,
        buffer: 64,
        output_rate: 48_000,
        output_buffer: 256,
        eq: None,
        eq_post: None,
        gate: None,
        input_gain: 3.0,
        output_gain: -6.0,
        input_channels: [true, false],
        output_channels: [true, true],
        mono: false,
        mute: true,
        drift: false,
        low_latency_output: false,
        wasapi_output_mode: Some(WasapiOutputMode::Exclusive),
        routes: [[true, false, false, false], [false, true, false, false]],
        bridge_buffer_mode: 2,
        bridge_buffer_ms: 6.0,
        plugins: Vec::new(),
        match_paths: crate::app::match_paths::MatchPathsSettings::default(),
        paths: vec![golden_path("Lead")],
        clock_output: golden_device(-1.0, 0b0100),
        extra_outputs: vec![golden_extra_output(false)],
    }
}

/// The preferences behind `testdata/preferences.ron`.  Every field is set
/// by hand, none from `Default` (which differs by platform), and every
/// optional part is present, so the saved text is the same on every build.
fn golden_preferences() -> Preferences {
    Preferences {
        schema_version: PREFERENCES_SCHEMA,
        auto_start_bridge: true,
        sessions: vec![golden_session()],
        session_name: "Gig".to_owned(),
        page: "mixer".to_owned(),
        eq_edit: 1,
        output_tab_low_latency: true,
        selected_session: Some(0),
        asio_name: Some("Focusrite USB ASIO".to_owned()),
        asio_id: "asio:focusrite".to_owned(),
        asio_backend: "ASIO".to_owned(),
        input_name: Some("Scarlett 2i2".to_owned()),
        input_id: "alsa:hw:2".to_owned(),
        input_backend: "ALSA".to_owned(),
        output_name: Some("Speakers".to_owned()),
        output_id: "pipewire:speakers".to_owned(),
        output_backend: "PipeWire".to_owned(),
        use_asio_input: false,
        sample_rate: 96_000,
        buffer_size: 64,
        input_gain: 2.5,
        output_gain: -3.0,
        input_channels: [true, false],
        output_channels: [true, true],
        mono: false,
        mute: true,
        drift: false,
        low_latency_output: false,
        wasapi_output_mode: Some(WasapiOutputMode::RealStyle),
        routes: [[true, false, false, false], [false, true, false, false]],
        output_rate: 48_000,
        output_buffer: 256,
        bridge_buffer_mode: 1,
        bridge_buffer_ms: 8.5,
        eq: EqSettings::default(),
        eq_post: EqSettings::default(),
        gate: GateSettings::default(),
        plugins: Vec::new(),
        paths: vec![golden_path("Clean"), golden_path("Drive")],
        selected_path: 1,
        match_paths: crate::app::match_paths::MatchPathsSettings::default(),
        clock_output: golden_device(-6.0, 0b0001),
        extra_outputs: vec![golden_extra_output(false), golden_extra_output(true)],
        measured_latency: Some(measured_latency()),
        tuner_tuning: "drop-d".to_owned(),
        tuner_reference_hz: 442.0,
        tuner_channel: 2,
        tuner_display: 1,
        tuner_listen: 1,
        tuner_auto_mute: false,
        tuner_tolerance: 2,
        record_folder: "/home/me/Takes".to_owned(),
        amp_folder: String::new(),
        cab_folder: String::new(),
        record_click: true,
        metronome_running: false,
        metronome_bpm: 96.0,
        metronome_rhythm: crate::audio::ClickRhythm::SixEight,
        metronome_level_db: -18.0,
        theme: ThemeChoice::Light,
        ui_size: 1.2,
        effect_favourites: Vec::new(),
        effect_recent: Vec::new(),
        sheet_height: 0.0,
        effect_double_click: crate::app::settings::EffectOpen::Editor,
        last_click_mask: 0,
    }
}

/// What this build saves for [`golden_preferences`], plus a final newline.
const GOLDEN: &str = include_str!("testdata/preferences.ron");

/// Pins the saved format: field names, variant names and how values are
/// spelled.  A rename in the code keeps these bytes with `#[serde(rename)]`,
/// so old and new builds keep reading each other's files.  Do not edit the
/// golden file to make this pass.
#[test]
fn the_stored_format_matches_the_golden_file() {
    let raw = stored(&golden_preferences());
    assert!(
        GOLDEN == format!("{raw}\n"),
        "the saved preferences changed:\n{raw}"
    );
}

#[test]
fn the_golden_file_loads_and_round_trips() {
    let raw = GOLDEN.strip_suffix('\n').expect("one line");
    let read: Preferences = ron::from_str(raw).expect("golden");
    assert_eq!(stored(&read), raw);
    let loaded = load_preferences(Some(raw), BACKUP);
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    assert!(loaded.backup.is_none());
    let back = loaded.preferences.expect("readable");
    assert_eq!(back.sessions.len(), 1);
    assert_eq!(back.sessions[0].paths, vec![golden_path("Lead")]);
    assert_eq!(
        back.sessions[0].extra_outputs,
        vec![golden_extra_output(false)]
    );
    assert_eq!(back.sessions[0].bridge_buffer_mode, 2);
    assert_eq!(back.paths, vec![golden_path("Clean"), golden_path("Drive")]);
    assert_eq!(back.clock_output, golden_device(-6.0, 0b0001));
    assert_eq!(back.extra_outputs.len(), 2);
    assert_eq!(back.extra_outputs[0], golden_extra_output(false));
    assert!(back.extra_outputs[1].virtual_output && back.extra_outputs[1].virtual_output_on);
    assert_eq!(back.measured_latency, Some(measured_latency()));
    assert_eq!(back.bridge_buffer_mode, 1);
    assert_eq!(back.bridge_buffer_ms, 8.5);
    assert_eq!(back.tuner_tuning, "drop-d");
}

#[test]
fn the_last_click_mask_is_saved_only_once_used() {
    let unused = ron::to_string(&Preferences::default()).expect("preferences");
    assert!(!unused.contains("last_click_mask"));
    let used = Preferences {
        last_click_mask: 0b1100,
        ..Preferences::default()
    };
    let text = ron::to_string(&used).expect("preferences");
    let back: Preferences = ron::from_str(&text).expect("preferences");
    assert_eq!(back.last_click_mask, 0b1100);
}
