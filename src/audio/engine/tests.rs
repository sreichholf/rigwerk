use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use super::AudioEngine;
use crate::audio::record::{RecordPhase, RecordSource};
use crate::audio::{AsioPanelSide, EngineState, StartConfig};

fn recording_file(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "rigwerk-arm-{name}-{}-{nanos}.wav",
        std::process::id()
    ))
}

fn wait_until_idle(engine: &AudioEngine) {
    wait_for_phase(engine, |phase| matches!(phase, RecordPhase::Idle));
}

fn wait_until_recording(engine: &AudioEngine) {
    wait_for_phase(engine, |phase| matches!(phase, RecordPhase::Recording));
}

fn wait_for_phase(engine: &AudioEngine, ready: impl Fn(&RecordPhase) -> bool) {
    let start = Instant::now();
    loop {
        let status = engine.recording_status();
        if ready(&status.phase) || matches!(&status.phase, RecordPhase::Failed) {
            assert!(
                !matches!(&status.phase, RecordPhase::Failed),
                "recording failed: {}",
                status.error.unwrap_or_else(|| "no error".to_owned())
            );
            return;
        }
        if start.elapsed() >= Duration::from_secs(2) {
            panic!(
                "recording did not reach the expected phase within 2s ({} frames, error: {})",
                status.frames,
                status.error.unwrap_or_else(|| "none".to_owned())
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn the_panel_stays_unavailable_until_the_bridge_is_running() {
    let mut engine = AudioEngine::new(48_000);
    engine.asio_input_clsid = Some(0xA510);
    engine.asio_output_clsid = Some(0xB0);
    assert_eq!(engine.open_asio_driver(AsioPanelSide::Input), None);
    engine.state = EngineState::Starting;
    assert_eq!(engine.open_asio_driver(AsioPanelSide::Input), None);
    engine.state = EngineState::Running;
    assert_eq!(engine.open_asio_driver(AsioPanelSide::Input), Some(0xA510));
    assert_eq!(engine.open_asio_driver(AsioPanelSide::Output), Some(0xB0));
    engine.stop();
    engine.state = EngineState::Running;
    assert_eq!(
        engine.open_asio_driver(AsioPanelSide::Input),
        None,
        "stopping forgets which driver was open"
    );
}

#[test]
fn clear_error_keeps_a_callback_failure_alive_until_the_run_is_gone() {
    use crate::audio::shared::CallbackFailure;

    let mut engine = AudioEngine::new(48_000);
    engine.shared.note_callback_failure(CallbackFailure::Panic);

    // The app clears the error right after the asynchronous `stop`; the
    // panicked run's streams may still be live, so the flag must survive it.
    engine.clear_error();
    assert_eq!(
        engine.shared.callback_failure(),
        Some(CallbackFailure::Panic)
    );
    assert!(
        engine.snapshot().device_lost,
        "the failed run is still visible"
    );

    // `finish_stop` runs only after every bridge thread has ended, which is
    // when the streams are gone.
    engine.stop();
    assert_eq!(engine.shared.callback_failure(), None);
    assert!(!engine.snapshot().device_lost);
}

#[test]
fn the_bridge_status_is_part_of_the_snapshot() {
    let engine = AudioEngine::new(48_000);
    engine.shared.set_status("Opening independent output…");
    assert_eq!(engine.bridge_status(), "Opening independent output…");
    assert_eq!(engine.snapshot().status, "Opening independent output…");
}

#[test]
fn a_quantum_failure_survives_bridge_stopped() {
    let mut engine = AudioEngine::new(48_000);
    engine.note_quantum_failure(
        "PipeWire quantum was not restored: pw-metadata not available".to_owned(),
    );
    engine.stop();
    assert_eq!(
        engine.quantum_note(),
        Some("PipeWire quantum was not restored: pw-metadata not available")
    );
    assert_eq!(engine.bridge_status(), "Bridge stopped");
}

#[cfg(target_os = "linux")]
#[test]
fn a_missing_metadata_tool_is_kept_when_the_quantum_is_restored() {
    let tool_missing = std::process::Command::new("pw-metadata")
        .arg("-n")
        .arg("settings")
        .output()
        .is_err();
    let mut engine = AudioEngine::new(48_000);
    engine.quantum_forced = true;
    engine.previous_quantum = 1024;
    engine.stop();
    assert_eq!(engine.bridge_status(), "Bridge stopped");
    if tool_missing {
        let note = engine
            .quantum_note()
            .expect("a missing pw-metadata must be reported");
        assert!(
            note.starts_with("PipeWire graph buffer was not restored:"),
            "{note}"
        );
    }
}

#[test]
fn arm_recording_refuses_a_clock_with_no_opened_endpoint() {
    let engine = AudioEngine::new(48_000);
    let path = recording_file("clock-closed");
    assert!(!engine.arm_recording(path.clone(), RecordSource::Clock));
    engine
        .shared
        .set_output(48_000, 64, crate::audio::OutputMode::Standard, "test");
    assert!(
        !engine.arm_recording(path.clone(), RecordSource::Clock),
        "a rate without a channel count is not an endpoint"
    );
    engine.shared.clock_output.set_channels(2);
    engine.shared.output_rate.store(0, Ordering::Relaxed);
    assert!(!engine.arm_recording(path.clone(), RecordSource::Clock));
    engine
        .shared
        .set_output(48_000, 64, crate::audio::OutputMode::Standard, "test");
    assert!(engine.arm_recording(path.clone(), RecordSource::Clock));
    wait_until_recording(&engine);
    assert_eq!(engine.recording_status().rate, 48_000);
    assert_eq!(engine.recording_status().channels, 2);
    engine.disarm_recording();
    wait_until_idle(&engine);
    let _ = std::fs::remove_file(path);
}

#[test]
fn arm_recording_uses_the_extra_outputs_opened_rate() {
    let engine = AudioEngine::new(48_000);
    let path = recording_file("extra-output-rate");
    engine
        .shared
        .set_output(44_100, 64, crate::audio::OutputMode::Standard, "test");
    engine.shared.extra_outputs[0].set_channels(2);
    assert!(
        !engine.arm_recording(path.clone(), RecordSource::ExtraOutput(0)),
        "a generic extra output with no opened rate does not borrow the clock rate"
    );
    assert!(!engine.arm_recording(path.clone(), RecordSource::ExtraOutput(99)));

    engine.shared.extra_output_bus(0).set_playback_rate(48_000);
    assert!(engine.arm_recording(path.clone(), RecordSource::ExtraOutput(0)));
    wait_until_recording(&engine);
    assert_eq!(engine.recording_status().rate, 48_000);
    assert_eq!(engine.recording_status().channels, 2);
    engine.disarm_recording();
    wait_until_idle(&engine);

    // In-graph duplex extra outputs are written at the graph rate, with no
    // resampler.  That is the one fallback to the clock rate.
    engine.shared.extra_output_bus(0).set_playback_rate(0);
    engine.shared.direct.store(true, Ordering::Relaxed);
    assert!(engine.arm_recording(path.clone(), RecordSource::ExtraOutput(0)));
    wait_until_recording(&engine);
    assert_eq!(engine.recording_status().rate, 44_100);
    engine.disarm_recording();
    wait_until_idle(&engine);

    engine.shared.extra_outputs[0].set_channels(0);
    assert!(!engine.arm_recording(path.clone(), RecordSource::ExtraOutput(0)));
    let _ = std::fs::remove_file(path);
}

#[test]
fn arm_recording_stop_lets_the_writer_finish() {
    let mut engine = AudioEngine::new(48_000);
    engine
        .shared
        .set_output(48_000, 64, crate::audio::OutputMode::Standard, "test");
    engine.shared.clock_output.set_channels(2);
    let path = recording_file("stop");
    assert!(engine.arm_recording(path.clone(), RecordSource::Clock));
    wait_until_recording(&engine);
    engine.stop();
    wait_until_idle(&engine);
    let _ = std::fs::remove_file(path);
}

#[test]
fn stopping_the_bridge_clears_the_published_beat() {
    let mut engine = AudioEngine::new(48_000);
    engine
        .shared
        .publish_metronome_beat(4, crate::audio::ClickRhythm::FourFour as u8);
    engine.stop();
    assert_eq!(engine.state, EngineState::Idle);
    // 0 is the "—" the Live strip shows.
    assert_eq!(engine.snapshot().metronome_beat, 0);
}

/// Each engine owns the plugin-slot park its runs share, so a slot one
/// engine's bridge deferred is invisible to another engine.
#[test]
fn two_engines_have_two_plugin_parks() {
    let _main = crate::audio::claim_main_thread();
    let alive = crate::audio::plugins::tracked_slots_alive;
    let first = AudioEngine::new(48_000);
    let second = AudioEngine::new(48_000);
    let first_park = std::sync::Arc::clone(first.shared.plugin_park());
    let second_park = std::sync::Arc::clone(second.shared.plugin_park());
    assert!(
        !std::sync::Arc::ptr_eq(&first_park, &second_park),
        "two engines must not share one park"
    );

    let before = alive();
    first_park.defer(vec![crate::audio::plugins::Slot::tracked_for_test()]);
    assert_eq!(first_park.len(), 1);
    assert_eq!(
        second_park.len(),
        0,
        "the other engine's park never sees it"
    );
    second_park.drain();
    assert_eq!(alive(), before + 1, "another owner's drain frees nothing");

    first_park.drain();
    assert_eq!(alive(), before);
    assert_eq!(first_park.len(), 0);
}

/// Each run the engine swaps in shares its park, so a bridge that defers
/// after `replace_run` still hands its slots to a live owner.
#[test]
fn a_run_inherits_the_engine_plugin_park() {
    let mut engine = AudioEngine::new(48_000);
    let park = std::sync::Arc::clone(engine.shared.plugin_park());
    assert!(std::sync::Arc::ptr_eq(&park, &engine.slot_park));

    engine.replace_run();

    assert!(
        std::sync::Arc::ptr_eq(&park, engine.shared.plugin_park()),
        "a run must inherit the engine's park"
    );
}

/// The start refusal stays the backstop behind the per-engine park: while an
/// old bridge still owns the device, a second start fails with the same
/// message and spawns nothing.
#[test]
fn a_lingering_bridge_refuses_a_second_start_with_the_same_message() {
    const MESSAGE: &str = "The previous bridge is still shutting down and has not released the audio device. Start again once it has finished.";

    let mut engine = AudioEngine::new(48_000);
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let (parked_tx, parked_rx) = std::sync::mpsc::channel::<()>();
    let blocked = std::thread::spawn(move || {
        parked_tx
            .send(())
            .expect("the test is still waiting for the bridge");
        let _ = release_rx.recv();
    });
    parked_rx.recv().expect("the bridge thread starts");
    engine.park_lingering_for_test(blocked);
    assert!(engine.teardown_pending(), "the bridge is still alive");

    engine.start(StartConfig {
        asio_clsid: None,
        input_id: None,
        output_id: "test:output".to_owned(),
        output_asio_clsid: None,
        sample_rate: 48_000,
        buffer_size: 128,
        output_rate: None,
        output_buffer: 128,
        #[cfg(target_os = "windows")]
        asio_wrapper: false,
        low_latency_request: false,
        exclusive_output: false,
        #[cfg(target_os = "linux")]
        pipewire_low_latency: false,
    });

    assert_eq!(engine.state, EngineState::Error);
    assert_eq!(engine.snapshot().error.as_deref(), Some(MESSAGE));
    assert_eq!(engine.bridge_status(), format!("Error: {MESSAGE}"));
    assert!(engine.handle.is_none(), "no bridge was spawned");

    release_tx.send(()).expect("the bridge thread stops");
    let deadline = Instant::now() + Duration::from_secs(2);
    while engine.teardown_pending() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!engine.teardown_pending(), "the helper bridge did not end");
}
