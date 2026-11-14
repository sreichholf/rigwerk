use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use super::AudioEngine;
use crate::audio::record::{RecordPhase, RecordSource};
use crate::audio::{AsioPanelSide, EngineState};

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
            note.starts_with("PipeWire quantum was not restored:"),
            "{note}"
        );
    }
}

#[test]
fn arm_recording_refuses_a_clock_with_no_opened_endpoint() {
    let engine = AudioEngine::new(48_000);
    let path = recording_file("clock-closed");
    assert!(!engine.arm_recording(path.clone(), RecordSource::Clock));
    engine.shared.set_output(48_000, 64, "test");
    assert!(
        !engine.arm_recording(path.clone(), RecordSource::Clock),
        "a rate without a channel count is not an endpoint"
    );
    engine.shared.clock_device.set_channels(2);
    engine.shared.output_rate.store(0, Ordering::Relaxed);
    assert!(!engine.arm_recording(path.clone(), RecordSource::Clock));
    engine.shared.set_output(48_000, 64, "test");
    assert!(engine.arm_recording(path.clone(), RecordSource::Clock));
    wait_until_recording(&engine);
    assert_eq!(engine.recording_status().rate, 48_000);
    assert_eq!(engine.recording_status().channels, 2);
    engine.disarm_recording();
    wait_until_idle(&engine);
    let _ = std::fs::remove_file(path);
}

#[test]
fn arm_recording_uses_the_followers_opened_rate() {
    let engine = AudioEngine::new(48_000);
    let path = recording_file("follower-rate");
    engine.shared.set_output(44_100, 64, "test");
    engine.shared.followers[0].set_channels(2);
    assert!(
        !engine.arm_recording(path.clone(), RecordSource::Follower(0)),
        "a generic follower with no opened rate does not borrow the clock rate"
    );
    assert!(!engine.arm_recording(path.clone(), RecordSource::Follower(99)));

    engine.shared.follower_bus(0).set_playback_rate(48_000);
    assert!(engine.arm_recording(path.clone(), RecordSource::Follower(0)));
    wait_until_recording(&engine);
    assert_eq!(engine.recording_status().rate, 48_000);
    assert_eq!(engine.recording_status().channels, 2);
    engine.disarm_recording();
    wait_until_idle(&engine);

    // In-graph duplex followers are written at the graph rate, with no
    // resampler.  That is the one fallback to the clock rate.
    engine.shared.follower_bus(0).set_playback_rate(0);
    engine.shared.direct.store(true, Ordering::Relaxed);
    assert!(engine.arm_recording(path.clone(), RecordSource::Follower(0)));
    wait_until_recording(&engine);
    assert_eq!(engine.recording_status().rate, 44_100);
    engine.disarm_recording();
    wait_until_idle(&engine);

    engine.shared.followers[0].set_channels(0);
    assert!(!engine.arm_recording(path.clone(), RecordSource::Follower(0)));
    let _ = std::fs::remove_file(path);
}

#[test]
fn arm_recording_stop_lets_the_writer_finish() {
    let mut engine = AudioEngine::new(48_000);
    engine.shared.set_output(48_000, 64, "test");
    engine.shared.clock_device.set_channels(2);
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
