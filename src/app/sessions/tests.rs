use crate::app::settings::{
    DEFAULT_BUFFER, DEFAULT_INPUT_GAIN, DEFAULT_SAMPLE_RATE, INPUT_GAIN_MIN_DB, MIN_OUTPUT_BUFFER,
    OUTPUT_GAIN_MAX_DB, RATES,
};
use crate::app::LogTone;
use crate::app::Page;
use crate::app::RigWerkApp;
use crate::audio::{BufferMode, DeviceItem, PluginFormat, PluginPref};

fn device(id: &str, name: &str, backend: &str) -> DeviceItem {
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

fn test_app() -> RigWerkApp {
    let mut app = crate::app::blank_app();
    app.page = Page::Live;
    app.inventory
        .asio
        .push(device("asio:focusrite", "Focusrite", "ASIO"));
    app.inventory.inputs.push(device("hw:Mic", "Mic", "ALSA"));
    app.inventory
        .outputs
        .push(device("hw:Speaker", "Speaker", "ALSA"));
    app
}

fn log_lines(app: &RigWerkApp) -> Vec<&str> {
    app.log.iter().map(|entry| entry.text.as_str()).collect()
}

#[test]
fn a_missing_device_is_an_error_and_the_mix_still_applies() {
    let mut app = test_app();
    let routes = [[false, true, false, false], [true, false, false, true]];
    app.sessions.push(crate::app::settings::Session {
        name: "Gone".to_owned(),
        input: "Other interface".to_owned(),
        input_id: "asio:missing".to_owned(),
        input_backend: "ASIO".to_owned(),
        output: "Other speakers".to_owned(),
        output_id: "hw:missing".to_owned(),
        output_backend: "ALSA".to_owned(),
        input_gain: 4.5,
        output_gain: -8.0,
        routes,
        input_channels: [true, false],
        output_channels: [false, true],
        mono: true,
        ..crate::app::settings::Session::default()
    });
    app.input_gain = -12.0;
    app.asio_idx = 0;
    app.output_idx = 0;

    app.apply_session(0);

    assert_eq!(app.input_gain, 4.5);
    assert_eq!(app.output_gain, -8.0);
    assert_eq!(app.path().mix.routes, routes);
    assert_eq!(app.input_channels, [true, false]);
    assert_eq!(app.path().mix.output_enabled, [false, true]);
    assert!(app.path().mix.mono);
    assert_eq!(app.asio_idx, 0);
    assert_eq!(app.output_idx, 0);
    assert_eq!(app.active_input().id, "asio:focusrite");
    assert_eq!(app.selected_output().id, "hw:Speaker");
    let lines = log_lines(&app);
    assert!(
        lines.iter().any(|line| line.contains("was not found")),
        "{lines:?}"
    );
    assert!(
        lines.iter().all(|line| !line.contains("Loaded session")),
        "{lines:?}"
    );
    assert!(app
        .log
        .iter()
        .any(|entry| entry.text.contains("was not found") && entry.tone == LogTone::Red));
    let linear = app.engine.shared.input_gain();
    assert!(linear.is_finite());
    assert!((linear - 10.0f32.powf(4.5 / 20.0)).abs() < 1e-4);
}

#[test]
fn applying_a_session_rejects_nan_gain_and_unknown_rate() {
    let mut app = test_app();
    app.sessions.push(crate::app::settings::Session {
        name: "Hot".to_owned(),
        input: "Focusrite".to_owned(),
        input_id: "asio:focusrite".to_owned(),
        input_backend: "ASIO".to_owned(),
        output: "Speaker".to_owned(),
        output_id: "hw:Speaker".to_owned(),
        output_backend: "ALSA".to_owned(),
        input_gain: f32::NAN,
        output_gain: f32::INFINITY,
        rate: 22_050,
        buffer: 7,
        output_rate: 12_345,
        output_buffer: 4,
        ..crate::app::settings::Session::default()
    });

    app.apply_session(0);

    assert!(app.input_gain.is_finite());
    assert_eq!(app.input_gain, DEFAULT_INPUT_GAIN);
    assert!(app.input_gain >= INPUT_GAIN_MIN_DB);
    assert_eq!(app.output_gain, OUTPUT_GAIN_MAX_DB);
    assert!(app.output_gain.is_finite());
    assert_eq!(app.sample_rate, DEFAULT_SAMPLE_RATE);
    assert_ne!(app.sample_rate, 22_050);
    assert!(RATES.contains(&app.sample_rate));
    assert_eq!(app.buffer_size, DEFAULT_BUFFER);
    assert_ne!(app.buffer_size, 7);
    assert_eq!(app.output_rate, DEFAULT_SAMPLE_RATE);
    assert_ne!(app.output_rate, 12_345);
    assert_eq!(app.output_buffer, DEFAULT_BUFFER);
    assert!(app.output_buffer >= MIN_OUTPUT_BUFFER);
    assert_ne!(app.output_buffer, 4);
    assert_eq!(app.sessions[0].input_gain, DEFAULT_INPUT_GAIN);
    assert_eq!(app.sessions[0].rate, DEFAULT_SAMPLE_RATE);
    let input_linear = app.engine.shared.input_gain();
    let output_linear = app.engine.shared.output_gain();
    assert!(input_linear.is_finite() && input_linear > 0.0);
    assert!(output_linear.is_finite() && output_linear > 0.0);
    assert!((input_linear - 1.0).abs() < 1e-5);
    assert!(log_lines(&app)
        .iter()
        .any(|line| line.contains("Loaded session")));
    assert_eq!(
        app.log
            .iter()
            .find(|entry| entry.text.contains("Loaded session"))
            .map(|entry| entry.tone),
        Some(LogTone::Blue)
    );
}

#[test]
fn saving_and_applying_a_session_round_trips_the_mix() {
    let mut app = test_app();
    app.session_name = "Gig".to_owned();
    app.input_gain = 6.0;
    app.output_gain = -12.0;
    app.input_channels = [true, false];
    app.path_mut().mix.output_enabled = [false, true];
    app.path_mut().mix.mono = true;
    app.mute = true;
    app.monitor = false;
    app.drift = false;
    app.low_latency_output = false;
    app.wasapi_output_mode = crate::audio::WasapiOutputMode::Shared;
    app.path_mut().mix.routes = [[false, true, false, true], [true, false, true, false]];
    app.bridge_buffer_mode = BufferMode::Fixed;
    app.bridge_buffer_ms = 7.5;
    app.sample_rate = 96_000;
    app.buffer_size = 64;
    app.output_rate = 48_000;
    app.output_buffer = 256;
    app.engine.restore_plugins(
        0,
        vec![PluginPref {
            path: "C:/Plugins/Amp.vst3".to_owned(),
            format: PluginFormat::Vst3,
            name: "Amp".to_owned(),
            mix: 0.4,
            ..PluginPref::default()
        }],
    );

    app.save_session();
    let saved = app.sessions[0].clone();
    assert_eq!(saved.input_id, "asio:focusrite");
    assert_eq!(saved.input_backend, "ASIO");
    assert_eq!(saved.output_id, "hw:Speaker");
    assert_eq!(saved.output_backend, "ALSA");
    assert_eq!(saved.input_gain, 6.0);
    assert_eq!(
        saved.wasapi_output_mode,
        Some(crate::audio::WasapiOutputMode::Shared)
    );
    assert_eq!(saved.paths[0].routes, app.path().mix.routes);
    assert_eq!(saved.paths[0].plugins[0].path, "C:/Plugins/Amp.vst3");
    assert_eq!(saved.paths[0].plugins[0].mix, 0.4);

    app.input_gain = 0.0;
    app.output_gain = 0.0;
    app.path_mut().mix.routes = [[true, false, false, false], [false, true, false, false]];
    app.path_mut().mix.mono = false;
    app.mute = false;
    app.monitor = true;
    app.drift = true;
    app.low_latency_output = true;
    app.wasapi_output_mode = crate::audio::WasapiOutputMode::RealStyle;
    app.input_channels = [true, true];
    app.path_mut().mix.output_enabled = [true, true];
    app.bridge_buffer_mode = BufferMode::Auto;
    app.bridge_buffer_ms = 10.0;
    app.sample_rate = 48_000;
    app.buffer_size = 128;
    app.output_rate = 0;
    app.output_buffer = 128;
    app.engine.restore_plugins(0, Vec::new());
    app.inventory
        .asio
        .insert(0, device("asio:other", "Other", "ASIO"));
    app.asio_idx = 0;
    app.inventory
        .outputs
        .insert(0, device("hw:Other", "Other out", "ALSA"));
    app.output_idx = 0;

    app.apply_session(0);

    assert_eq!(app.input_gain, 6.0);
    assert_eq!(app.output_gain, -12.0);
    assert_eq!(app.input_channels, [true, false]);
    assert_eq!(app.path().mix.output_enabled, [false, true]);
    assert!(app.path().mix.mono);
    assert!(app.mute);
    assert!(!app.monitor);
    assert!(!app.drift);
    assert!(!app.low_latency_output);
    assert_eq!(
        app.wasapi_output_mode,
        crate::audio::WasapiOutputMode::Shared
    );
    assert_eq!(
        app.path().mix.routes,
        [[false, true, false, true], [true, false, true, false]]
    );
    assert_eq!(app.bridge_buffer_mode, BufferMode::Fixed);
    assert_eq!(app.bridge_buffer_ms, 7.5);
    assert_eq!(app.sample_rate, 96_000);
    assert_eq!(app.buffer_size, 64);
    assert_eq!(app.output_rate, 48_000);
    assert_eq!(app.output_buffer, 256);
    assert_eq!(app.active_input().id, "asio:focusrite");
    assert_eq!(app.selected_output().id, "hw:Speaker");
    let plugins = app.engine.plugin_prefs(0);
    assert_eq!(plugins.len(), 1);
    assert_eq!(plugins[0].path, "C:/Plugins/Amp.vst3");
    assert_eq!(plugins[0].mix, 0.4);
    assert!((app.engine.shared.input_gain() - 10.0f32.powf(6.0 / 20.0)).abs() < 1e-4);
    assert!(log_lines(&app)
        .iter()
        .any(|line| line.contains("Loaded session")));
}

#[test]
fn an_old_session_output_buffer_follows_the_input_buffer() {
    let mut app = test_app();
    app.sessions.push(crate::app::settings::Session {
        name: "Old".to_owned(),
        input: "Focusrite".to_owned(),
        input_id: "asio:focusrite".to_owned(),
        input_backend: "ASIO".to_owned(),
        output: "Speaker".to_owned(),
        output_id: "hw:Speaker".to_owned(),
        output_backend: "ALSA".to_owned(),
        rate: 44_100,
        buffer: 64,
        output_rate: 0,
        output_buffer: 0,
        ..crate::app::settings::Session::default()
    });
    app.apply_session(0);
    assert_eq!(app.buffer_size, 64);
    assert_eq!(app.output_buffer, 64);
    assert_eq!(app.sessions[0].output_buffer, 0);
    assert_eq!(app.input_gain, DEFAULT_INPUT_GAIN);
}

#[test]
fn applying_a_session_restores_gate_settings() {
    use crate::audio::{GateSettings, GateSlot};

    let mut app = test_app();
    let gate = GateSettings {
        enabled: true,
        slot: GateSlot::Pre,
        threshold_db: -36.0,
        range_db: -72.0,
        hysteresis_db: 4.0,
        attack_ms: 3.0,
        release_ms: 200.0,
        hold_ms: 30.0,
    };
    app.sessions.push(crate::app::settings::Session {
        name: "Gated".to_owned(),
        input: "Focusrite".to_owned(),
        input_id: "asio:focusrite".to_owned(),
        input_backend: "ASIO".to_owned(),
        output: "Speaker".to_owned(),
        output_id: "hw:Speaker".to_owned(),
        output_backend: "ALSA".to_owned(),
        gate: Some(gate),
        ..crate::app::settings::Session::default()
    });
    app.path_mut().mix.gate = GateSettings::default();
    app.apply_session(0);
    assert_eq!(app.path().mix.gate, gate);
    assert_eq!(app.engine.shared.paths[0].try_gate(), Some(gate));
}

#[test]
fn sessions_carry_the_output_devices() {
    use crate::app::settings::ExtraOutputPref;
    use crate::audio::{ChannelMap, OutputDeviceSettings, UnusedChannels};

    let mut app = test_app();
    let clock = OutputDeviceSettings {
        level_db: -4.5,
        mute: true,
        path_map: crate::audio::PathMapSettings::first(ChannelMap {
            left: 0b0011,
            right: 0b1100,
        }),
        unused: UnusedChannels::Silent,
        align: false,
        click_mask: 0,
    };
    let extra_outputs = vec![
        ExtraOutputPref {
            id: "hw:Phones".to_owned(),
            backend: "ALSA".to_owned(),
            name: "Phones".to_owned(),
            bridge_buffer_mode: 2,
            bridge_buffer_ms: 15.0,
            ..ExtraOutputPref::default()
        },
        ExtraOutputPref {
            id: "hw:Monitor".to_owned(),
            name: "Monitor".to_owned(),
            ..ExtraOutputPref::default()
        },
    ];
    app.session_name = "Two outs".to_owned();
    app.clock_output = clock;
    app.extra_outputs = extra_outputs.clone();

    app.save_session();
    assert_eq!(app.sessions[0].clock_output, clock);
    assert_eq!(app.sessions[0].extra_outputs, extra_outputs);

    app.clock_output = OutputDeviceSettings::default();
    app.extra_outputs.clear();
    app.apply_session(0);
    assert_eq!(app.clock_output, clock);
    assert_eq!(app.extra_outputs, extra_outputs);

    // An older session without them brings back today's output.
    app.sessions.push(crate::app::settings::Session {
        name: "Old".to_owned(),
        ..crate::app::settings::Session::default()
    });
    app.apply_session(1);
    assert_eq!(app.clock_output, OutputDeviceSettings::default());
    assert!(app.extra_outputs.is_empty());
}

#[test]
fn applying_a_session_keeps_the_click_mask() {
    use crate::app::settings::ExtraOutputPref;

    let mut app = test_app();
    let phones = ExtraOutputPref {
        id: "hw:Phones".to_owned(),
        backend: "ALSA".to_owned(),
        name: "Phones".to_owned(),
        ..ExtraOutputPref::default()
    };
    let monitor = ExtraOutputPref {
        id: "hw:Monitor".to_owned(),
        backend: "ALSA".to_owned(),
        name: "Monitor".to_owned(),
        ..ExtraOutputPref::default()
    };
    app.session_name = "Click".to_owned();
    app.clock_output.click_mask = 0b0011;
    app.extra_outputs = vec![phones.clone(), monitor.clone()];
    app.extra_outputs[0].settings.click_mask = 0b0100;
    app.extra_outputs[1].settings.click_mask = 0b1000;
    app.save_session();
    // Saving still records the mask.
    assert_eq!(app.sessions[0].clock_output.click_mask, 0b0011);
    assert_eq!(app.sessions[0].extra_outputs[0].settings.click_mask, 0b0100);

    // The click moves after the save; the session does not move it back.
    // Extra outputs are matched by id and backend, not by position.
    app.clock_output.click_mask = 0b1100;
    app.extra_outputs = vec![monitor.clone()];
    app.extra_outputs[0].settings.click_mask = 0b0001;
    app.extra_outputs[0].settings.level_db = -6.0;
    app.apply_session(0);
    assert_eq!(app.clock_output.click_mask, 0b1100);
    assert_eq!(app.extra_outputs.len(), 2);
    // Phones is not live now: no click there.
    assert_eq!(app.extra_outputs[0].id, "hw:Phones");
    assert_eq!(app.extra_outputs[0].settings.click_mask, 0);
    // Monitor keeps its live mask; the rest comes from the session.
    assert_eq!(app.extra_outputs[1].id, "hw:Monitor");
    assert_eq!(app.extra_outputs[1].settings.click_mask, 0b0001);
    assert_eq!(app.extra_outputs[1].settings.level_db, 0.0);

    // An older session without a mask does not silence the click.
    app.sessions.push(crate::app::settings::Session {
        name: "Old".to_owned(),
        ..crate::app::settings::Session::default()
    });
    app.apply_session(1);
    assert_eq!(app.clock_output.click_mask, 0b1100);
}

#[test]
fn the_click_mask_follows_the_device_when_a_session_swaps_clock_and_extra_output() {
    use crate::app::settings::ExtraOutputPref;

    let mut app = test_app();
    app.inventory
        .outputs
        .push(device("hw:Monitor", "Monitor", "ALSA"));
    let as_extra_output = |id: &str, name: &str| ExtraOutputPref {
        id: id.to_owned(),
        backend: "ALSA".to_owned(),
        name: name.to_owned(),
        ..ExtraOutputPref::default()
    };
    // Saved with Speaker as the clock and Monitor as an extra output.
    app.session_name = "Speaker clock".to_owned();
    app.output_idx = 0;
    app.extra_outputs = vec![as_extra_output("hw:Monitor", "Monitor")];
    app.save_session();

    // Now Monitor is the clock and Speaker an extra output, each with its click.
    app.output_idx = 1;
    app.clock_output.click_mask = 0b0011;
    app.extra_outputs = vec![as_extra_output("hw:Speaker", "Speaker")];
    app.extra_outputs[0].settings.click_mask = 0b1100;
    app.apply_session(0);

    // The masks move with the devices, not with the roles.
    assert_eq!(app.selected_output().id, "hw:Speaker");
    assert_eq!(app.clock_output.click_mask, 0b1100);
    assert_eq!(app.extra_outputs.len(), 1);
    assert_eq!(app.extra_outputs[0].id, "hw:Monitor");
    assert_eq!(app.extra_outputs[0].settings.click_mask, 0b0011);
}

#[test]
fn saving_a_session_writes_current_gate() {
    use crate::audio::{GateSettings, GateSlot};

    let mut app = test_app();
    app.session_name = "Mine".to_owned();
    app.asio_idx = 0;
    app.output_idx = 0;
    app.use_asio_input = true;
    app.path_mut().mix.gate = GateSettings {
        enabled: true,
        slot: GateSlot::Pre,
        threshold_db: -40.0,
        range_db: -50.0,
        hysteresis_db: 5.0,
        attack_ms: 1.5,
        release_ms: 90.0,
        hold_ms: 25.0,
    };
    app.save_session();
    assert_eq!(app.sessions.len(), 1);
    assert_eq!(app.sessions[0].paths[0].gate, app.path().mix.gate);
}

#[test]
fn a_saved_session_is_edited_when_the_gain_changes() {
    let mut app = test_app();
    app.session_name = "Clean".to_owned();
    app.asio_idx = 0;
    app.output_idx = 0;
    app.save_session();
    assert!(!app.session_edited(), "a fresh save matches");
    app.input_gain += 1.0;
    assert!(app.session_edited());
    app.start_new_session();
    assert_eq!(app.session_name, "Untitled session");
    assert!(app.session_edited());
}
