use std::collections::HashMap;

use super::*;
use crate::app::latency::{LoopbackKind, MeasuredLatency};
use crate::app::Page;
use crate::audio::{
    ChannelMap, EqSettings, GateSettings, GateSlot, OutputDeviceSettings, PluginFormat, PluginPref,
    UnusedChannels, WasapiOutputMode, MAX_FOLLOWERS, VIRTUAL_OUT_ID, VIRTUAL_OUT_NAME,
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

fn preset(name: &str) -> Preset {
    Preset {
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
        ..Preset::default()
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
        sessions: vec![preset("Clean"), preset("Lead")],
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
    // A patch from before paths: path 1, with what it never stored (the
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
fn damaged_plugin_state_keeps_the_insert() {
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
fn preset_missing_fields_still_loads() {
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
    assert_eq!(old.paths.len(), 1, "an old patch is one path");
    assert_eq!(old.paths[0].output_channels, [true, true]);
    assert!(!old.paths[0].mono);
    assert!(!old.mute);
    assert!(old.monitor);
    assert!(old.drift);
    assert!(old.low_latency_output);
    assert_eq!(old.wasapi_output_mode, Some(WasapiOutputMode::RealStyle));
    assert_eq!(
        old.paths[0].routes,
        [[true, false, false, false], [false, true, false, false]]
    );
    assert_eq!(old.cushion_mode, 0);
    assert_eq!(old.cushion_ms, DEFAULT_CUSHION_MS);
    assert!(old.paths[0].plugins.is_empty());
    assert!(old.eq.is_none());
    assert!(old.eq_post.is_none());
    assert!(old.gate.is_none());
}

#[test]
fn gate_round_trips_in_preferences_and_session_presets() {
    let gate = GateSettings {
        enabled: true,
        slot: GateSlot::Pre,
        threshold_db: -42.5,
        range_db: -55.0,
        hysteresis_db: 8.0,
        attack_ms: 2.5,
        release_ms: 180.0,
        hold_ms: 55.0,
    };
    let mut session = preset("Gated");
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
    assert_eq!(preferences.cushion_ms, DEFAULT_CUSHION_MS);

    let preset = &preferences.sessions[0];
    assert_eq!(preset.rate, DEFAULT_SAMPLE_RATE);
    assert_ne!(preset.rate, 1);
    assert_eq!(preset.buffer, DEFAULT_BUFFER);
    assert_ne!(preset.buffer, 0);
    assert_eq!(preset.output_rate, DEFAULT_SAMPLE_RATE);
    assert_ne!(preset.output_buffer, 9);
    assert_eq!(preset.output_buffer, DEFAULT_BUFFER);
    assert!(preset.output_buffer >= MIN_OUTPUT_BUFFER);
    assert!(preset.input_gain.is_finite());
    assert_eq!(preset.input_gain, DEFAULT_INPUT_GAIN);
    assert_eq!(preset.output_gain, OUTPUT_GAIN_MIN_DB);
    assert!(preset.output_gain.is_finite());
    assert_eq!(preset.cushion_ms, CUSHION_MS_MAX);

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
            cushion_ms: 7.5,
        },
        OutputBufferZero::Invalid,
    );
    assert_eq!(kept.sample_rate, 96_000);
    assert_eq!(kept.buffer_size, 64);
    assert_eq!(kept.output_rate, 0);
    assert_eq!(kept.output_buffer, MIN_OUTPUT_BUFFER);
    assert_eq!(kept.input_gain, INPUT_GAIN_MAX_DB);
    assert_eq!(kept.output_gain, OUTPUT_GAIN_MIN_DB);
    assert_eq!(kept.cushion_ms, 7.5);

    let huge = validate_bridge_settings(
        BridgeSettings {
            sample_rate: 48_000,
            buffer_size: 4_096,
            output_rate: 48_000,
            output_buffer: 4_096,
            input_gain: 0.0,
            output_gain: 0.0,
            cushion_ms: 10.0,
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
fn a_full_preset_round_trips_routes_gains_and_plugins() {
    let mut full = preset("Gig");
    full.input_gain = 6.0;
    full.output_gain = -12.0;
    full.input_channels = [true, false];
    full.output_channels = [false, true];
    full.mono = true;
    full.mute = true;
    full.monitor = false;
    full.drift = false;
    full.low_latency_output = false;
    full.routes = [[false, true, false, true], [true, false, true, false]];
    full.cushion_mode = 2;
    full.cushion_ms = 7.5;
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
    assert!(!back.monitor);
    assert!(!back.drift);
    assert!(!back.low_latency_output);
    assert_eq!(back.cushion_mode, 2);
    assert_eq!(back.cushion_ms, 7.5);
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
fn corrupt_preset_is_skipped_and_the_rest_kept() {
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
        loaded.notices[0].contains("Skipped 2 saved patch"),
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
        loaded.notices[0].contains("2 plugin insert"),
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
fn the_diagnostics_page_round_trips() {
    assert_eq!(page_key(Page::Diagnostics), "diagnostics");
    assert!(page_from_key("diagnostics") == Page::Diagnostics);
    assert!(page_from_key(" Diagnostics ") == Page::Diagnostics);
}

fn output_devices() -> (OutputDeviceSettings, Vec<FollowerPref>) {
    let clock = OutputDeviceSettings {
        level_db: -6.0,
        mute: true,
        path_map: crate::audio::PathMapSettings::first(ChannelMap {
            left: 0b0101,
            right: 0b1010,
        }),
        unused: UnusedChannels::Silent,
        align: false,
    };
    let followers = vec![
        FollowerPref {
            id: "hw:Phones".to_owned(),
            backend: "ALSA".to_owned(),
            name: "Phones".to_owned(),
            settings: OutputDeviceSettings {
                level_db: -3.0,
                ..OutputDeviceSettings::default()
            },
            cushion_mode: 2,
            cushion_ms: 20.0,
            ..FollowerPref::default()
        },
        FollowerPref {
            settings: OutputDeviceSettings {
                mute: true,
                path_map: crate::audio::PathMapSettings::first(ChannelMap { left: 1, right: 1 }),
                ..OutputDeviceSettings::default()
            },
            ..FollowerPref::virtual_out()
        },
    ];
    (clock, followers)
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
    };
    assert_eq!(OutputDeviceSettings::default(), today);
    assert_eq!(preferences.clock_device, today);
    assert!(preferences.followers.is_empty());
    // The master stays where it was.
    assert_eq!(preferences.output_gain, -6.0);
    assert!(preferences.mute);
    assert_eq!(preferences.sessions[0].clock_device, today);
    assert!(preferences.sessions[0].followers.is_empty());
}

#[test]
fn unknown_unused_channel_mode_reads_as_mono_copy() {
    let raw = r#"(session_name:"Mine",sample_rate:96000,clock_device:(level_db:-2.0,unused:loud),followers:[(id:"hw:A",settings:(unused:silent))])"#;
    let loaded = load_preferences(Some(raw), BACKUP);
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    let preferences = loaded.preferences.expect("readable");
    assert_eq!(preferences.clock_device.unused, UnusedChannels::MonoCopy);
    assert_eq!(preferences.clock_device.level_db, -2.0);
    assert_eq!(
        preferences.followers[0].settings.unused,
        UnusedChannels::Silent
    );
    assert_eq!(preferences.session_name, "Mine");
    assert_eq!(preferences.sample_rate, 96_000);
}

#[test]
fn output_devices_round_trip_in_preferences_and_presets() {
    let (clock, followers) = output_devices();
    let mut session = preset("Two outs");
    session.clock_device = clock;
    session.followers = followers.clone();
    let preferences = Preferences {
        clock_device: clock,
        followers: followers.clone(),
        sessions: vec![session],
        ..Preferences::default()
    };
    let loaded = load_preferences(Some(&stored(&preferences)), BACKUP);
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    let back = loaded.preferences.expect("readable");
    assert_eq!(back.clock_device, clock);
    assert_eq!(back.sessions[0].clock_device, clock);
    #[cfg(target_os = "windows")]
    {
        // Virtual-out clears the PipeWire sentinel; rematch fills WASAPI later.
        assert_eq!(back.followers.len(), followers.len());
        assert_eq!(back.followers[0], followers[0]);
        assert!(back.followers[1].virtual_out && back.followers[1].publish);
        assert!(back.followers[1].id.is_empty());
        assert_eq!(back.followers[1].name, VIRTUAL_OUT_NAME);
        assert_eq!(back.sessions[0].followers.len(), followers.len());
        assert!(back.sessions[0].followers[1].id.is_empty());
    }
    #[cfg(not(target_os = "windows"))]
    {
        assert_eq!(back.followers, followers);
        assert_eq!(back.sessions[0].followers, followers);
    }
}

#[test]
fn unreadable_follower_is_skipped_and_the_rest_kept() {
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
        .followers
        .iter()
        .map(|f| f.id.as_str())
        .collect();
    assert_eq!(ids, ["hw:A", "hw:C"]);
    assert_eq!(
        preferences.followers[0].settings.unused,
        UnusedChannels::Silent
    );
    assert_eq!(preferences.followers[1].cushion_ms, CUSHION_MS_MAX);
    assert_eq!(preferences.clock_device.level_db, 0.0);
    assert_eq!(preferences.clock_device.map(0).left, 3);
    assert_eq!(preferences.clock_device.map(0).right, 0b10);
    assert_eq!(preferences.sessions[0].clock_device.level_db, 6.0);
    let session = &preferences.sessions[1].followers;
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
fn nothing_stored_is_quiet() {
    let loaded = load_preferences(None, BACKUP);
    assert!(loaded.preferences.is_none());
    assert!(loaded.backup.is_none());
    assert!(loaded.notices.is_empty());
}

#[test]
fn old_followers_without_publish_default_off() {
    let raw = r#"(followers:[(id:"hw:A",name:"A")])"#;
    let loaded = load_preferences(Some(raw), BACKUP);
    let preferences = loaded.preferences.expect("readable");
    assert_eq!(preferences.followers.len(), 1);
    assert!(!preferences.followers[0].virtual_out);
    assert!(!preferences.followers[0].publish);
}

#[test]
fn virtual_out_and_publish_round_trip() {
    let preferences = Preferences {
        followers: vec![FollowerPref::virtual_out()],
        ..Preferences::default()
    };
    let loaded = load_preferences(Some(&stored(&preferences)), BACKUP);
    let back = loaded.preferences.expect("readable");
    assert_eq!(back.followers.len(), 1);
    assert!(back.followers[0].virtual_out);
    assert!(back.followers[0].publish);
    #[cfg(target_os = "windows")]
    {
        // PipeWire sentinel is cleared; inventory rematch fills the WASAPI id.
        assert!(back.followers[0].id.is_empty());
    }
    #[cfg(not(target_os = "windows"))]
    {
        assert_eq!(back.followers[0].id, VIRTUAL_OUT_ID);
    }
    assert_eq!(back.followers[0].name, VIRTUAL_OUT_NAME);
}

#[test]
fn followers_past_the_cap_are_dropped() {
    let entries: String = (0..=MAX_FOLLOWERS)
        .map(|index| format!(r#"(id:"hw:{index}",name:"D{index}")"#))
        .collect::<Vec<_>>()
        .join(",");
    let raw = format!("(followers:[{entries}])");
    let loaded = load_preferences(Some(&raw), BACKUP);
    let preferences = loaded.preferences.expect("readable");
    assert_eq!(preferences.followers.len(), MAX_FOLLOWERS);
    assert_eq!(preferences.followers[0].id, "hw:0");
    assert_eq!(
        preferences.followers[MAX_FOLLOWERS - 1].id,
        format!("hw:{}", MAX_FOLLOWERS - 1)
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
    assert_eq!(preferences.followers.len(), 2);
    assert!(!preferences.followers[0].virtual_out);
    assert!(preferences.followers[1].virtual_out);
    assert!(preferences.followers[1].publish);
    #[cfg(target_os = "windows")]
    {
        assert!(preferences.followers[1].id.is_empty());
    }
    #[cfg(not(target_os = "windows"))]
    {
        assert_eq!(preferences.followers[1].id, VIRTUAL_OUT_ID);
    }
}

#[test]
fn windows_virtual_out_keeps_wasapi_id_through_sanitize() {
    let pref = FollowerPref {
        id: "wasapi:{cable-input}".to_owned(),
        backend: "WASAPI".to_owned(),
        name: "CABLE Input (VB-Audio Virtual Cable)".to_owned(),
        virtual_out: true,
        publish: true,
        ..FollowerPref::default()
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
        assert_eq!(pref.id, VIRTUAL_OUT_ID);
        assert_eq!(pref.backend, "PipeWire");
    }
    assert!(pref.virtual_out && pref.publish);
    assert_eq!(pref.name, "CABLE Input (VB-Audio Virtual Cable)");
}

#[test]
fn windows_virtual_out_drops_pipewire_sentinel_on_sanitize() {
    let pref = FollowerPref {
        id: VIRTUAL_OUT_ID.to_owned(),
        virtual_out: true,
        publish: true,
        ..FollowerPref::default()
    }
    .sanitized();
    #[cfg(target_os = "windows")]
    {
        assert!(pref.id.is_empty());
        assert_eq!(pref.backend, "WASAPI");
        assert_eq!(pref.name, VIRTUAL_OUT_NAME);
    }
    #[cfg(not(target_os = "windows"))]
    {
        assert_eq!(pref.id, VIRTUAL_OUT_ID);
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
    let mut followers = vec![FollowerPref {
        id: VIRTUAL_OUT_ID.to_owned(),
        virtual_out: true,
        publish: true,
        ..FollowerPref::default()
    }
    .sanitized()];
    rematch_windows_virtual_out(&mut followers, std::slice::from_ref(&cable));
    #[cfg(target_os = "windows")]
    {
        assert_eq!(followers[0].id, cable.id);
        assert_eq!(followers[0].backend, "WASAPI");
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = cable;
        assert_eq!(followers[0].id, VIRTUAL_OUT_ID);
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
            let mut session = preset("Exclusive");
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
        preferences.clock_device.map(0),
        ChannelMap { left: 4, right: 8 }
    );
    assert_eq!(preferences.clock_device.level_db, -3.0);
    assert_eq!(
        preferences.followers[0].settings.map(0),
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
    // What the patch never stored comes from the first path.
    assert_eq!(old.paths[0].gate, path.gate);
    assert_eq!(old.clock_device.map(0), ChannelMap { left: 2, right: 1 });
}

/// Three paths with their own chains, and each device with its own map,
/// level, polarity and trim per path.
fn three_paths() -> (Vec<PathPref>, OutputDeviceSettings, Vec<FollowerPref>) {
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
    let mut stream = FollowerPref::virtual_out();
    stream.settings.path_map[0] = crate::audio::PathMapSettings::unrouted();
    stream.settings.path_map[2].level_db = -2.0;
    // A generic follower with its own cushion and a per-path map.
    let mut phones = FollowerPref {
        id: "hw:Phones".to_owned(),
        backend: "ALSA".to_owned(),
        name: "Phones".to_owned(),
        cushion_mode: 1,
        cushion_ms: 12.5,
        ..FollowerPref::default()
    };
    phones.settings.path_map[1] = crate::audio::PathMapSettings::unrouted();
    phones.settings.path_map[2].invert = true;
    // Expect what a load gives on this platform (Windows drops the PipeWire
    // RigWerk Out id).
    (paths, clock, vec![stream.sanitized(), phones.sanitized()])
}

#[test]
fn three_paths_round_trip_in_preferences() {
    let (paths, clock_device, followers) = three_paths();
    let preferences = Preferences {
        paths: paths.clone(),
        selected_path: 2,
        clock_device,
        followers: followers.clone(),
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
    assert_eq!(back.clock_device, clock_device);
    assert_eq!(back.followers, followers);
    assert_eq!(back.wasapi_output_mode, Some(WasapiOutputMode::Exclusive));
}

#[test]
fn three_paths_round_trip_in_a_preset() {
    let (paths, clock_device, followers) = three_paths();
    let mut session = preset("Three");
    session.eq = None;
    session.paths = paths.clone();
    session.clock_device = clock_device;
    session.followers = followers.clone();
    let preferences = Preferences {
        sessions: vec![session],
        ..Preferences::default()
    };
    let loaded = load_preferences(Some(&stored(&preferences)), BACKUP);
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    let back = &loaded.preferences.expect("readable").sessions[0];
    assert_eq!(back.paths, paths);
    assert_eq!(back.clock_device, clock_device);
    assert_eq!(back.followers, followers);
}

#[test]
fn a_one_path_preset_round_trips() {
    let mut session = preset("One");
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
    assert_eq!(back.clock_device, OutputDeviceSettings::default());
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
    // The drop is reported like a skipped patch, and the original is kept.
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
    for index in 0..(MAX_FOLLOWERS + 1) {
        preferences.followers.push(FollowerPref {
            id: format!("out:{index}"),
            name: format!("Out {index}"),
            ..FollowerPref::default()
        });
    }
    // A duplicate virtual out goes through the same floor rule.
    preferences.followers.push(FollowerPref::virtual_out());
    preferences.followers.push(FollowerPref::virtual_out());

    let loaded = load_preferences(Some(&stored(&preferences)), BACKUP);
    let back = loaded.preferences.expect("readable");
    assert_eq!(back.followers.len(), MAX_FOLLOWERS);
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
    // and the insert's own bypass is still there.
    let raw = r#"(paths:[(name:"Main",plugins:[(path:"A.vst3",name:"Amp",bypass:true)])])"#;
    let loaded = load_preferences(Some(raw), BACKUP);
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    let old = loaded.preferences.expect("readable");
    assert_eq!(old.schema_version, PREFERENCES_SCHEMA);
    assert!(!old.paths[0].chain_bypass);
    assert!(old.paths[0].plugins[0].bypass);
    assert_eq!(old.paths[0].plugins[0].name, "Amp");
}
