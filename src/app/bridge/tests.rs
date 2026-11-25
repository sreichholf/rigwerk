use crate::app::RigWerkApp;
use crate::audio::EngineState;

/// Input block + bridge buffer at the input rate, plus the output step at the
/// output rate.  Same terms as `latency_ms`, with no extra fudge.
fn bridge_ms(
    input_block: f32,
    bridge_buffer: f32,
    input_rate: f32,
    output_step: f32,
    output_rate: f32,
) -> f32 {
    ((input_block + bridge_buffer) / input_rate + output_step / output_rate) * 1000.0
}

/// An app with chosen buffer settings and an empty snapshot.  No device
/// is opened; the engine stays Idle until a test sets its state.
fn latency_app() -> RigWerkApp {
    let mut app = crate::app::blank_app();
    app.devices.use_asio_input = false;
    app.output_gain = 0.0;
    app.low_latency_output = false;
    app.wasapi_output_mode = crate::audio::WasapiOutputMode::Shared;
    app.output_rate = 48_000;
    app.session_name = String::new();
    app
}

#[test]
fn set_wasapi_output_mode_keeps_the_legacy_bool_aligned() {
    let mut app = latency_app();
    app.set_wasapi_output_mode(crate::audio::WasapiOutputMode::Shared);
    assert!(!app.low_latency_output);
    app.set_wasapi_output_mode(crate::audio::WasapiOutputMode::RealStyle);
    assert!(app.low_latency_output);
    app.set_wasapi_output_mode(crate::audio::WasapiOutputMode::Exclusive);
    assert!(!app.low_latency_output);
    assert_eq!(
        app.wasapi_output_mode,
        crate::audio::WasapiOutputMode::Exclusive
    );
}

#[test]
fn bridge_setters_clamp_like_a_saved_preference() {
    use std::sync::atomic::Ordering;

    let mut app = latency_app();

    app.set_sample_rate(22_050);
    assert_eq!(app.sample_rate, crate::app::settings::DEFAULT_SAMPLE_RATE);
    let before = app.current_bridge_signature();
    app.set_sample_rate(96_000);
    assert_eq!(app.sample_rate, 96_000);
    assert_ne!(
        app.current_bridge_signature(),
        before,
        "the sample rate is missing from the bridge signature"
    );

    app.set_buffer_size(4_096);
    assert_eq!(app.buffer_size, crate::app::settings::DEFAULT_BUFFER);
    let before = app.current_bridge_signature();
    app.set_buffer_size(100);
    assert_eq!(app.buffer_size, 100);
    assert_ne!(
        app.current_bridge_signature(),
        before,
        "the input buffer is missing from the bridge signature"
    );

    app.set_output_rate(12_345);
    assert_eq!(app.output_rate, crate::app::settings::DEFAULT_SAMPLE_RATE);
    let before = app.current_bridge_signature();
    app.set_output_rate(0);
    assert_eq!(app.output_rate, 0);
    assert_ne!(
        app.current_bridge_signature(),
        before,
        "the clock output rate is missing from the bridge signature"
    );

    app.set_output_buffer(1);
    assert_eq!(app.output_buffer, crate::app::settings::DEFAULT_BUFFER);
    app.set_output_buffer(0);
    assert_eq!(app.output_buffer, crate::app::settings::DEFAULT_BUFFER);
    let before = app.current_bridge_signature();
    app.set_output_buffer(256);
    assert_eq!(app.output_buffer, 256);
    assert_ne!(
        app.current_bridge_signature(),
        before,
        "the clock output buffer is missing from the bridge signature"
    );

    app.set_bridge_buffer_ms(f32::NAN);
    assert_eq!(
        app.bridge_buffer_ms,
        crate::app::settings::DEFAULT_BRIDGE_BUFFER_MS
    );
    app.set_bridge_buffer_ms(100.0);
    assert_eq!(
        app.bridge_buffer_ms,
        crate::app::settings::BRIDGE_BUFFER_MS_MAX
    );
    app.set_bridge_buffer_ms(0.0);
    assert_eq!(
        app.bridge_buffer_ms,
        crate::app::settings::BRIDGE_BUFFER_MS_MIN
    );

    // The clock setter pushes as it stores, unlike the rate and buffer pairs
    // that the next `sync_bridge_config` pass re-opens the bridge with.
    app.set_bridge_buffer_mode(crate::audio::BufferMode::Fixed);
    assert_eq!(app.bridge_buffer_mode, crate::audio::BufferMode::Fixed);
    assert_eq!(
        app.engine.shared.bridge_buffer_mode(),
        crate::audio::BufferMode::Fixed
    );
    app.set_bridge_buffer_ms(12.5);
    assert_eq!(app.bridge_buffer_ms, 12.5);
    assert_eq!(
        f32::from_bits(
            app.engine
                .shared
                .fixed_bridge_buffer_ms
                .load(Ordering::Relaxed)
        ),
        12.5
    );
}

#[test]
fn toggle_mute_flips_and_the_frame_push_keeps_it() {
    use std::sync::atomic::Ordering;

    let mut app = latency_app();
    app.toggle_mute();
    assert!(app.mute);
    app.publish_engine();
    assert!(app.engine.shared.mute.load(Ordering::Relaxed));

    app.toggle_mute();
    assert!(!app.mute);
    app.publish_engine();
    assert!(!app.engine.shared.mute.load(Ordering::Relaxed));
}

#[test]
fn set_drift_is_pushed_by_the_frame() {
    use std::sync::atomic::Ordering;

    let mut app = latency_app();
    app.set_drift(false);
    assert!(!app.drift);
    app.publish_engine();
    assert!(!app.engine.shared.drift_enabled.load(Ordering::Relaxed));
}

#[test]
fn running_latency_uses_the_delivered_input_block() {
    let mut app = latency_app();
    app.buffer_size = 128;
    app.sample_rate = 96_000;
    app.output_rate = 44_100;
    app.output_buffer = 64;
    app.engine.state = EngineState::Running;
    app.snapshot.input_block_frames = 480;
    app.snapshot.input_rate = 48_000;
    app.snapshot.output_rate = 48_000;
    app.snapshot.output_step_frames = 128;
    app.snapshot.target_frames = 960;

    let ms = app.latency_ms();
    let delivered = bridge_ms(480.0, 960.0, 48_000.0, 128.0, 48_000.0);
    let requested = bridge_ms(128.0, 960.0, 48_000.0, 128.0, 48_000.0);
    assert!(
        (ms - delivered).abs() < 0.01,
        "running latency {ms} did not use the 480-frame block ({delivered})"
    );
    assert!(
        (ms - requested).abs() > 1.0,
        "running latency {ms} still used the requested 128-frame block"
    );
    assert!(
        (ms - (delivered + 1.5)).abs() > 1.0,
        "running latency {ms} still adds the 1.5 ms fudge"
    );
}

#[test]
fn running_latency_falls_back_until_the_device_reports() {
    let mut app = latency_app();
    app.buffer_size = 128;
    app.output_buffer = 64;
    app.engine.state = EngineState::Running;
    app.snapshot.input_block_frames = 0;
    app.snapshot.target_frames = 0;
    app.snapshot.input_rate = 48_000;
    app.snapshot.output_rate = 48_000;
    app.snapshot.output_step_frames = 64;

    let ms = app.latency_ms();
    let planned = bridge_ms(128.0, 256.0, 48_000.0, 64.0, 48_000.0);
    assert!(
        (ms - planned).abs() < 0.01,
        "running latency {ms} did not fall back to the requested block and bridge buffer ({planned})"
    );
}

#[test]
fn a_requested_native_start_is_not_reported_until_the_graph_measures_it() {
    let mut app = latency_app();
    app.buffer_size = 128;
    app.output_buffer = 64;
    app.sample_rate = 48_000;
    app.engine.state = EngineState::Running;
    // What a native PipeWire start publishes before it negotiates: the rate
    // and the quantum asked for, with no reading behind either.  The input
    // block is real as soon as the callback runs, the rate is not.
    app.snapshot.input_block_frames = 64;
    app.snapshot.input_rate = 48_000;
    app.snapshot.output_rate = 48_000;
    app.snapshot.output_step_frames = 64;
    app.snapshot.target_frames = 128;
    let parts = app.latency_parts();
    assert!(
        parts.input_reported,
        "the input block itself is the callback's"
    );
    assert!(
        !parts.input_read(),
        "a measured block over an unread rate is not a reading"
    );
    assert!(
        !parts.output_reported,
        "the published output step is the quantum asked for"
    );
    assert!(!parts.output_read());
    let step = |app: &RigWerkApp| {
        app.latency_budget()
            .parts
            .into_iter()
            .find(|part| part.label == "OUTPUT STEP")
            .expect("an output step part")
    };
    assert_eq!(
        step(&app).source,
        crate::app::budget::LatencySource::Planned
    );
    assert!(step(&app).detail.ends_with("(planned)"));

    // The capture's format handler, the playback node's first block and the
    // playback format handler make them readings.
    app.snapshot.input_rate_reported = true;
    app.snapshot.output_rate_reported = true;
    app.snapshot.output_step_reported = true;
    let parts = app.latency_parts();
    assert!(parts.input_read());
    assert!(parts.output_read());
    assert_eq!(
        step(&app).source,
        crate::app::budget::LatencySource::Reported
    );
    assert!(!step(&app).detail.contains("(planned)"));
}

#[test]
fn a_measured_output_step_over_an_unread_rate_stays_planned() {
    let mut app = latency_app();
    app.buffer_size = 128;
    app.output_buffer = 64;
    app.sample_rate = 48_000;
    app.engine.state = EngineState::Running;
    app.snapshot.output_step_frames = 64;
    app.snapshot.output_rate = 48_000;
    // The step arrived, but the stream's format handler has not read the
    // rate yet: the label names both, so it is still planned.
    app.snapshot.output_step_reported = true;
    let parts = app.latency_parts();
    assert!(parts.output_reported);
    assert!(!parts.output_read());
    let step = app
        .latency_budget()
        .parts
        .into_iter()
        .find(|part| part.label == "OUTPUT STEP")
        .expect("an output step part");
    assert_eq!(step.source, crate::app::budget::LatencySource::Planned);
    assert!(step.detail.ends_with("(planned)"));

    app.snapshot.output_rate_reported = true;
    assert!(app.latency_parts().output_read());
}

#[test]
fn stopped_latency_ignores_the_previous_bridge_buffer() {
    let mut app = latency_app();
    app.buffer_size = 128;
    app.output_buffer = 256;
    app.sample_rate = 48_000;
    app.output_rate = 0;
    app.engine.state = EngineState::Idle;
    app.snapshot.target_frames = 4_096;
    app.snapshot.input_block_frames = 480;
    app.snapshot.input_rate = 96_000;
    app.snapshot.output_rate = 44_100;
    app.snapshot.output_step_frames = 512;

    let ms = app.latency_ms();
    let planned = bridge_ms(128.0, 256.0, 48_000.0, 256.0, 48_000.0);
    let leftover = bridge_ms(128.0, 4_096.0, 48_000.0, 256.0, 48_000.0);
    assert!(
        (ms - planned).abs() < 0.01,
        "stopped latency {ms} did not use the planned bridge buffer ({planned})"
    );
    assert!(
        (ms - leftover).abs() > 1.0,
        "stopped latency {ms} kept the previous session's bridge buffer"
    );
}

#[test]
fn heard_estimate_adds_the_pipewire_path_delay() {
    let mut app = latency_app();
    app.buffer_size = 128;
    app.sample_rate = 48_000;
    app.output_buffer = 64;
    app.engine.state = EngineState::Running;
    app.snapshot.input_block_frames = 64;
    app.snapshot.input_rate = 48_000;
    app.snapshot.output_rate = 48_000;
    app.snapshot.output_step_frames = 64;
    app.snapshot.target_frames = 128;
    app.snapshot.path_delay_ms = Some(147.5);

    let us = app.latency_ms();
    assert!((us - bridge_ms(64.0, 128.0, 48_000.0, 64.0, 48_000.0)).abs() < 0.01);
    let heard = app.heard_estimate();
    let after = heard
        .parts
        .iter()
        .find(|part| part.label == "After RigWerk")
        .expect("an after-node part");
    assert_eq!(after.ms, Some(147.5));
    app.snapshot.path_delay_ms = None;
    let without = app.heard_estimate();
    assert!((heard.total_ms - without.total_ms - 147.5).abs() < 0.01);
}

#[test]
fn path_delay_is_ignored_while_stopped() {
    let mut app = latency_app();
    app.engine.state = EngineState::Idle;
    app.snapshot.path_delay_ms = Some(147.5);
    let budget = app.latency_budget();
    let after = budget
        .parts
        .iter()
        .find(|part| part.label == "AFTER PLAYBACK NODE")
        .expect("an after-node part");
    assert_eq!(after.ms, None);
    let heard = app.heard_estimate();
    let after = heard
        .parts
        .iter()
        .find(|part| part.label == "After RigWerk")
        .expect("an after-node part");
    assert_eq!(after.ms, None);
}

#[test]
fn tuner_auto_mute_silences_the_click_and_the_master_mute_does_not() {
    use std::sync::atomic::Ordering;

    let engine_running =
        |app: &RigWerkApp| app.engine.shared.metronome_running.load(Ordering::Relaxed);
    let mut app = latency_app();
    app.engine.state = EngineState::Running;
    app.tuner.auto_mute = true;
    app.page = crate::app::Page::Console;
    app.set_metronome_running(true);
    assert!(engine_running(&app));

    // The person's own mute leaves the click ticking.
    app.mute = true;
    app.apply_metronome();
    assert!(engine_running(&app));

    // The tuner's layer silences it, without forgetting the transport.
    app.mute = false;
    app.page = crate::app::Page::Tuner;
    app.apply_metronome();
    assert!(!engine_running(&app));
    assert!(app.metronome_running);

    // Auto-mute off, or the tuner closed: the click plays again.
    app.tuner.auto_mute = false;
    app.apply_metronome();
    assert!(engine_running(&app));
    app.tuner.auto_mute = true;
    app.apply_metronome();
    assert!(!engine_running(&app));
    app.page = crate::app::Page::Console;
    app.apply_metronome();
    assert!(engine_running(&app));

    // A stopped click stays stopped everywhere.
    app.set_metronome_running(false);
    assert!(!engine_running(&app));
}

#[test]
fn a_frame_pushes_the_click_only_when_the_tuner_layer_changes() {
    use std::sync::atomic::Ordering;

    fn shared_running(app: &RigWerkApp) -> &std::sync::atomic::AtomicBool {
        &app.engine.shared.metronome_running
    }
    let mut app = latency_app();
    app.engine.state = EngineState::Running;
    app.tuner.auto_mute = true;
    app.page = crate::app::Page::Console;
    app.set_metronome_running(true);
    assert!(shared_running(&app).load(Ordering::Relaxed));

    // Nothing changed: the frame leaves the engine's flag alone.
    shared_running(&app).store(false, Ordering::Relaxed);
    app.follow_metronome_audible();
    assert!(!shared_running(&app).load(Ordering::Relaxed));

    // Opening the tuner, then leaving it, pushes each change.
    shared_running(&app).store(true, Ordering::Relaxed);
    app.page = crate::app::Page::Tuner;
    app.follow_metronome_audible();
    assert!(!shared_running(&app).load(Ordering::Relaxed));
    app.page = crate::app::Page::Console;
    app.follow_metronome_audible();
    assert!(shared_running(&app).load(Ordering::Relaxed));
}

#[test]
fn console_levels_are_clamped_and_switches_stored() {
    let mut app = crate::app::blank_app();
    app.set_input_gain(100.0);
    assert_eq!(app.input_gain, crate::app::settings::INPUT_GAIN_MAX_DB);
    app.set_master_level(f32::NAN);
    assert_eq!(app.output_gain, 0.0);
    app.set_master_level(-100.0);
    assert_eq!(app.output_gain, crate::app::settings::OUTPUT_GAIN_MIN_DB);
    app.set_input_channel(1, false);
    assert_eq!(app.input_channels, [true, false]);
    app.set_input_channel(5, false);
}

#[test]
fn a_push_over_the_placeholder_inventory_keeps_a_saved_extra_output_id() {
    let mut app = crate::app::blank_app();
    app.devices.inventory = crate::audio::placeholder_inventory();
    app.extra_outputs
        .push(crate::app::settings::ExtraOutputPref {
            id: "alsa:hw:Quad".to_owned(),
            backend: "ALSA".to_owned(),
            name: "System speakers".to_owned(),
            ..Default::default()
        });
    app.push_extra_outputs_to_engine();
    assert_eq!(app.extra_outputs[0].id, "alsa:hw:Quad");
    assert_eq!(app.extra_outputs[0].backend, "ALSA");
}

#[test]
fn the_engine_reads_follow_the_engine() {
    use std::sync::atomic::Ordering;

    use crate::audio::{AsioPanelSide, DspTime};

    let mut app = crate::app::blank_app();
    assert_eq!(app.engine_state(), EngineState::Idle);
    app.engine.state = EngineState::Running;
    assert_eq!(app.engine_state(), EngineState::Running);

    assert_eq!(app.engine_quantum_note(), None);
    app.engine
        .note_quantum_failure("PipeWire quantum was not restored".to_owned());
    assert_eq!(
        app.engine_quantum_note(),
        Some("PipeWire quantum was not restored")
    );
    assert_eq!(
        app.engine_bridge_status(),
        "PipeWire quantum was not restored"
    );

    // No mixer runs headlessly, so the DSP cells start empty.  This publishes
    // the same cells a mixer would; the wrapper must read them, not a default.
    assert_eq!(app.engine_total_dsp_time(), None);
    assert_eq!(app.engine_path_dsp_time(0), None);
    let time = DspTime {
        avg_ns: 210_000,
        peak_ns: 428_000,
        block_ns: 10_700_000,
    };
    app.engine.shared.running.store(true, Ordering::Relaxed);
    app.engine.shared.path(0).store_dsp_time(time);
    app.engine.shared.total_dsp.store(time);
    assert_eq!(app.engine_path_dsp_time(0), Some(time));
    assert_eq!(app.engine_total_dsp_time(), Some(time));

    assert!(app.engine_plugin_views(0).is_empty());
    assert_eq!(app.engine_plugin_lock_report(0).chain.attempts, 0);
    assert_eq!(app.driver_panel(AsioPanelSide::Input), None);
    assert_eq!(app.driver_panel(AsioPanelSide::Output), None);
}

#[test]
fn a_selected_asio_driver_makes_the_panel_wait_for_the_bridge() {
    use crate::audio::{AsioPanelSide, DeviceItem};

    let mut app = crate::app::blank_app();
    app.devices.inventory.asio.push(DeviceItem {
        id: "asio:test".to_owned(),
        name: "Test ASIO".to_owned(),
        backend: "ASIO".to_owned(),
        channels: 2,
        demo: false,
        preferred: false,
        asio_clsid: Some(7),
    });
    app.devices.use_asio_input = true;
    // Selected, but the bridge is not running on it: the panel exists and is
    // disabled.  `Some(true)` needs a live ASIO driver, so it is not reachable
    // headlessly.
    assert_eq!(app.driver_panel(AsioPanelSide::Input), Some(false));
    assert_eq!(app.driver_panel(AsioPanelSide::Output), None);
}

#[test]
fn request_asio_panel_raises_the_engine_flag() {
    use crate::audio::AsioPanelSide;

    let mut app = crate::app::blank_app();
    assert!(!app.engine.shared.asio_panel.take(AsioPanelSide::Input));
    app.request_asio_panel(AsioPanelSide::Input);
    assert!(app.engine.shared.asio_panel.take(AsioPanelSide::Input));
    assert!(!app.engine.shared.asio_panel.take(AsioPanelSide::Output));

    app.request_asio_panel(AsioPanelSide::Output);
    assert!(app.engine.shared.asio_panel.take(AsioPanelSide::Output));
}

#[test]
fn reset_plugin_lock_counters_clears_the_engine_report() {
    let _main = crate::audio::claim_main_thread();
    let mut app = crate::app::blank_app();
    {
        let path = app.engine.shared.path(0);
        path.insert_delay_for_test(16);
        let mut left = vec![0.0f32; 64];
        let mut right = vec![0.0f32; 64];
        assert!(path.process_plugins(&mut left, &mut right, 64));
    }
    assert!(app.engine_plugin_lock_report(0).effects[0].attempts > 0);
    assert_eq!(
        app.engine_plugin_views(0).len(),
        1,
        "the inserted delay is published"
    );

    app.reset_plugin_lock_counters();
    let report = app.engine_plugin_lock_report(0);
    assert_eq!(report.chain.attempts, 0);
    assert!(report.effects.iter().all(|effect| effect.attempts == 0));

    app.engine.shared.path(0).restore_plugins(Vec::new());
}
