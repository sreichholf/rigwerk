use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use super::{CalibrationPhase, CALIBRATION_TIMEOUT};
use crate::app::blank_app;
use crate::app::latency::MeasuredLatency;
use crate::app::RigWerkApp;
use crate::audio::{self, CalibrationState, DeviceItem, EngineState};

fn device(id: &str, name: &str) -> DeviceItem {
    DeviceItem {
        id: id.to_owned(),
        name: name.to_owned(),
        backend: "SYSTEM".to_owned(),
        channels: 2,
        demo: false,
        preferred: false,
        asio_clsid: None,
    }
}

/// An app whose bridge is live with both rates reported.  The engine's
/// `Shared` is not actually running; tests that need that say so.
fn live_app() -> RigWerkApp {
    let mut app = blank_app();
    app.use_asio_input = false;
    app.inventory.inputs.push(device("hw:Mic", "Mic"));
    app.inventory
        .outputs
        .push(device("out:Speakers", "Speakers"));
    app.input_idx = 0;
    app.output_idx = 0;
    app.engine.state = EngineState::Running;
    app.snapshot.input_rate = 48_000;
    app.snapshot.output_rate = 48_000;
    app.snapshot.input_block_frames = 128;
    app.snapshot.output_step_frames = 128;
    app
}

fn stored() -> MeasuredLatency {
    MeasuredLatency {
        latency_ms: 21.5,
        samples: 1_032,
        input_id: "hw:Mic".to_owned(),
        input_name: "Mic".to_owned(),
        output_id: "out:Speakers".to_owned(),
        output_name: "Speakers".to_owned(),
        input_rate: 48_000,
        output_rate: 48_000,
        input_buffer: 128,
        output_step: 128,
        output_mode: "low-latency".to_owned(),
        measured_at: 1_700_000_000,
        confidence: 0.9,
        ..MeasuredLatency::default()
    }
}

#[test]
fn a_stopped_bridge_or_a_preview_cannot_measure() {
    let mut app = live_app();
    app.engine.state = EngineState::Idle;
    assert!(app
        .calibration_blocker()
        .expect("stopped")
        .contains("Start the bridge"));
    let mut preview = blank_app();
    preview.engine.state = EngineState::Preview;
    assert!(preview
        .calibration_blocker()
        .expect("preview")
        .contains("Preview"));
}

#[test]
fn a_live_bridge_without_reported_rates_waits() {
    let mut app = live_app();
    app.snapshot.input_rate = 0;
    assert!(app
        .calibration_blocker()
        .expect("no rates")
        .contains("rates"));
    app.snapshot.input_rate = 48_000;
    assert_eq!(app.calibration_blocker(), None);
}

#[test]
fn starting_without_a_running_engine_says_so_and_keeps_the_old_result() {
    let mut app = live_app();
    app.measured_latency = Some(stored());
    // The app's state says live, but the engine's session is not running;
    // `arm_calibration` is the guard that decides.
    app.start_calibration();
    match &app.calibration.phase {
        CalibrationPhase::Failed { reason } => {
            assert!(reason.contains("Start the bridge"), "{reason}")
        }
        other => panic!("expected a failure, got {other:?}"),
    }
    assert_eq!(app.measured_latency, Some(stored()));
}

#[test]
fn cancelling_returns_to_idle_and_keeps_the_last_measurement() {
    let mut app = live_app();
    app.measured_latency = Some(stored());
    app.calibration.phase = CalibrationPhase::Measuring {
        started: Instant::now(),
        conditions: app.latency_conditions(),
        loopback: crate::app::latency::LoopbackKind::Cable,
    };
    app.cancel_calibration();
    assert_eq!(app.calibration.phase, CalibrationPhase::Idle);
    assert_eq!(app.measured_latency, Some(stored()));
}

#[test]
fn a_stopped_bridge_during_a_measurement_fails_it() {
    let mut app = live_app();
    app.calibration.phase = CalibrationPhase::Measuring {
        started: Instant::now(),
        conditions: app.latency_conditions(),
        loopback: crate::app::latency::LoopbackKind::Cable,
    };
    app.engine.state = EngineState::Idle;
    app.poll_calibration();
    match &app.calibration.phase {
        CalibrationPhase::Failed { reason } => {
            assert!(reason.contains("bridge stopped"), "{reason}")
        }
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[test]
fn a_measurement_that_never_hears_the_click_times_out() {
    let mut app = live_app();
    // Let the engine arm, so the state machine really is waiting for audio.
    app.engine.shared.running.store(true, Ordering::Relaxed);
    let click = audio::click_waveform(48_000);
    app.engine
        .arm_calibration(&click, 48_000, 128, 128)
        .expect("armed on a running engine");
    app.engine.shared.running.store(false, Ordering::Relaxed);
    // The app still believes the bridge is live long enough to hit the
    // timeout; the capture never starts because no callback runs.
    app.calibration.phase = CalibrationPhase::Measuring {
        started: Instant::now() - CALIBRATION_TIMEOUT - Duration::from_secs(1),
        conditions: app.latency_conditions(),
        loopback: crate::app::latency::LoopbackKind::Cable,
    };
    app.poll_calibration();
    match &app.calibration.phase {
        CalibrationPhase::Failed { reason } => {
            assert!(reason.contains("four seconds"), "{reason}")
        }
        other => panic!("expected a timeout, got {other:?}"),
    }
    assert_eq!(app.engine.calibration_state(), CalibrationState::Idle);
}

#[test]
fn the_live_progress_line_is_empty_until_a_measurement_is_armed() {
    let app = live_app();
    assert_eq!(app.calibration_progress(), 0.0);
}

/// Runs a live measurement up to `Ready`, with a synthetic click in the
/// capture at `peak_offset` frames.  The capture starts only after the duck
/// and its hold, as the real callbacks do.
fn armed_measurement(app: &mut RigWerkApp, peak_offset: usize) -> (usize, u64) {
    app.engine.shared.running.store(true, Ordering::Relaxed);
    app.start_calibration();
    assert!(app.calibration.is_measuring(), "not armed");
    let (_, limit) = app.engine.calibration_progress();
    let input_block = app.snapshot.input_block_frames as usize;
    let rate = app.snapshot.input_rate;
    let template = audio::click_waveform(rate);
    {
        let calibration = &app.engine.shared.calibration;
        calibration.note_ducked();
        // Two input blocks of hold (or one output step if longer), then the
        // first captured frame.
        for _ in 0..(2 * input_block).max(app.snapshot.output_step_frames as usize) {
            calibration.capture_frame(0.0, 0.0);
        }
        calibration.capture_frame(0.0, 0.0);
        assert_eq!(calibration.state(), CalibrationState::Capturing);
        assert!(calibration.note_emitting(Instant::now()));
        for index in 1..limit {
            let sample = if index >= peak_offset && index < peak_offset + template.len() {
                template[index - peak_offset]
            } else {
                0.0
            };
            calibration.capture_frame(sample, sample);
        }
        assert_eq!(calibration.state(), CalibrationState::Ready);
    }
    let start = app.engine.calibration_capture_start_ns();
    let end = start + (limit as f64 / rate as f64 * 1e9) as u64;
    app.engine
        .shared
        .calibration
        .set_capture_end_ns_for_test(end);
    (limit, start)
}

#[test]
fn a_complete_measurement_is_correlated_and_stored() {
    let mut app = live_app();
    let (_, start) = armed_measurement(&mut app, 4_800);
    app.poll_calibration();
    let measured = app.measured_latency.clone().expect("a stored measurement");
    assert!(
        (measured.latency_ms - 100.0).abs() < 0.5,
        "{} ms",
        measured.latency_ms
    );
    assert!(measured.samples.abs_diff(4_800) <= 2);
    assert_eq!(measured.input_id, app.latency_conditions().input_id);
    assert_eq!(measured.loopback, crate::app::latency::LoopbackKind::Cable);
    assert!(measured.confidence > 0.9);
    assert!(measured.measured_at > 0);
    assert!(start > 0);
    assert_eq!(app.calibration.phase, CalibrationPhase::Done);
    assert_eq!(app.engine.calibration_state(), CalibrationState::Idle);
}

#[test]
fn a_measurement_whose_rig_changed_is_discarded() {
    let mut app = live_app();
    armed_measurement(&mut app, 4_800);
    // The input block changed while the window was filling.
    app.snapshot.input_block_frames = 64;
    app.poll_calibration();
    assert_eq!(app.measured_latency, None);
    match &app.calibration.phase {
        CalibrationPhase::Failed { reason } => {
            assert!(reason.contains("settings changed"), "{reason}")
        }
        other => panic!("expected a discard, got {other:?}"),
    }
}

#[test]
fn a_rename_during_the_measurement_keeps_the_result() {
    let mut app = live_app();
    armed_measurement(&mut app, 4_800);
    app.inventory.inputs[0].name = "Mic (renamed)".to_owned();
    app.poll_calibration();
    assert!(app.measured_latency.is_some());
}

#[test]
fn a_window_that_ends_early_is_aborted() {
    let mut app = live_app();
    app.engine.shared.running.store(true, Ordering::Relaxed);
    app.start_calibration();
    assert!(app.calibration.is_measuring());
    // The audio side lost the measurement (a cancel or a session change).
    app.engine.shared.calibration.finish();
    app.poll_calibration();
    match &app.calibration.phase {
        CalibrationPhase::Failed { reason } => {
            assert!(
                reason.contains("before the capture window filled"),
                "{reason}"
            )
        }
        other => panic!("expected an abort, got {other:?}"),
    }
    assert_eq!(app.engine.calibration_state(), CalibrationState::Idle);
}

#[test]
fn a_capture_that_lost_time_is_rejected() {
    let mut app = live_app();
    armed_measurement(&mut app, 4_800);
    // A capture that took only a fraction of its frames' time means an input
    // block went missing.
    app.engine
        .shared
        .calibration
        .set_capture_end_ns_for_test(app.engine.calibration_capture_start_ns() + 1_000_000);
    app.poll_calibration();
    assert_eq!(app.measured_latency, None);
    match &app.calibration.phase {
        CalibrationPhase::Failed { reason } => {
            assert!(reason.contains("lost input blocks"), "{reason}")
        }
        other => panic!("expected a timing failure, got {other:?}"),
    }
}

#[test]
fn a_stray_audio_state_is_cleared_when_nothing_is_measuring() {
    let mut app = live_app();
    app.engine.shared.running.store(true, Ordering::Relaxed);
    let click = audio::click_waveform(48_000);
    app.engine
        .arm_calibration(&click, 48_000, 128, 128)
        .expect("armed");
    assert_eq!(app.engine.calibration_state(), CalibrationState::Armed);
    app.poll_calibration();
    assert_eq!(app.engine.calibration_state(), CalibrationState::Idle);
    assert_eq!(app.calibration.phase, CalibrationPhase::Idle);
}
