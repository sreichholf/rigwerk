use crate::app::RigWerkApp;
use crate::audio::EngineState;

/// Input block + cushion at the input rate, plus the output step at the
/// output rate.  Same terms as `latency_ms`, with no extra fudge.
fn bridge_ms(
    input_block: f32,
    cushion: f32,
    input_rate: f32,
    output_step: f32,
    output_rate: f32,
) -> f32 {
    ((input_block + cushion) / input_rate + output_step / output_rate) * 1000.0
}

/// An app with chosen buffer settings and an empty snapshot.  No device
/// is opened; the engine stays Idle until a test sets its state.
fn latency_app() -> RigWerkApp {
    let mut app = crate::app::blank_app();
    app.use_asio_input = false;
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

#[cfg(target_os = "windows")]
#[test]
fn planned_output_mode_text_names_each_wasapi_choice() {
    let mut app = latency_app();
    app.set_wasapi_output_mode(crate::audio::WasapiOutputMode::Shared);
    assert!(
        app.output_mode_text().contains("Shared"),
        "{}",
        app.output_mode_text()
    );
    app.set_wasapi_output_mode(crate::audio::WasapiOutputMode::RealStyle);
    assert!(
        app.output_mode_text().contains("REAL-style"),
        "{}",
        app.output_mode_text()
    );
    app.set_wasapi_output_mode(crate::audio::WasapiOutputMode::Exclusive);
    let text = app.output_mode_text();
    assert!(text.contains("Exclusive"), "{text}");
    assert!(text.contains("this device only"), "{text}");
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
        "running latency {ms} did not fall back to the requested block and cushion ({planned})"
    );
}

#[test]
fn stopped_latency_ignores_the_previous_cushion() {
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
        "stopped latency {ms} did not use the planned cushion ({planned})"
    );
    assert!(
        (ms - leftover).abs() > 1.0,
        "stopped latency {ms} kept the previous session's cushion"
    );
}

#[test]
fn heard_latency_adds_the_pipewire_path_delay() {
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
    assert!((app.path_delay_ms().unwrap() - 147.5).abs() < 0.01);
    assert!((app.heard_latency_ms().unwrap() - (us + 147.5)).abs() < 0.01);
    assert_eq!(app.latency_caption(), format!("{us:.1} · 147.5 ms"));
}

#[test]
fn path_delay_is_ignored_while_stopped() {
    let mut app = latency_app();
    app.engine.state = EngineState::Idle;
    app.snapshot.path_delay_ms = Some(147.5);
    assert_eq!(app.path_delay_ms(), None);
    assert_eq!(app.heard_latency_ms(), None);
    assert!(app.latency_caption().ends_with(" ms"));
    assert!(!app.latency_caption().contains('·'));
}

#[test]
fn tuner_auto_mute_silences_the_click_and_the_master_mute_does_not() {
    use std::sync::atomic::Ordering;

    let engine_running =
        |app: &RigWerkApp| app.engine.shared.metronome_running.load(Ordering::Relaxed);
    let mut app = latency_app();
    app.engine.state = EngineState::Running;
    app.tuner.auto_mute = true;
    app.page = crate::app::Page::Live;
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
    app.page = crate::app::Page::Live;
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
    app.page = crate::app::Page::Live;
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
    app.page = crate::app::Page::Live;
    app.follow_metronome_audible();
    assert!(shared_running(&app).load(Ordering::Relaxed));
}
