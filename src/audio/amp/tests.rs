//! The amp stage: sanitising, loading a capture, bypass, warm-up and the
//! swap fades.

use super::{
    prepare_amp_json, slim_submodel, AmpControls, AmpFile, AmpSettings, AmpSlot, AmpStage,
    LoadedAmp,
};
use crate::audio::path::PathControls;

/// The tiny WaveNet from the nam-rs crate docs.
const TINY: &str = r#"{
    "version": "0.5.4", "architecture": "WaveNet",
    "config": {
        "layers": [{
            "input_size": 1, "condition_size": 1, "channels": 1, "head_size": 1,
            "kernel_size": 1, "dilations": [1], "activation": "ReLU",
            "gated": false, "head_bias": false
        }],
        "head": null, "head_scale": 1.0
    },
    "weights": [1.0, 2.0, 0.0, 0.0, 1.0, 0.0, 1.0, 1.0]
}"#;

#[test]
fn sanitized_clamps_a_nan_trim_and_an_out_of_range_slim() {
    let settings = AmpSettings {
        enabled: true,
        slot: AmpSlot::Post,
        input_db: f32::NAN,
        output_db: 80.0,
        slim: 4.0,
        model_path: "  cab.nam  ".to_owned(),
    }
    .sanitized();
    assert!(settings.enabled);
    assert_eq!(settings.slot, AmpSlot::Post);
    assert_eq!(settings.input_db, 0.0);
    assert_eq!(settings.output_db, 24.0);
    assert_eq!(settings.slim, 1.0);
    assert_eq!(settings.model_path, "cab.nam");

    let slim = AmpSettings {
        slim: f32::NAN,
        ..AmpSettings::default()
    }
    .sanitized();
    assert_eq!(slim.slim, 1.0);
    assert!(slim.model_path.is_empty());
}

fn tiny() -> LoadedAmp {
    match prepare_amp_json(TINY, 48_000, 1.0) {
        AmpFile::Ready(loaded) => loaded,
        _ => panic!("the docs model should load at 48 kHz"),
    }
}

fn controls(enabled: bool) -> PathControls {
    let controls = PathControls::new(48_000);
    controls.set_amp(AmpControls {
        enabled,
        ..AmpControls::default()
    });
    controls
}

/// Runs `blocks` blocks of `frames` frames of a constant `value` and
/// returns the left channel of the last one.
fn run(
    stage: &mut AmpStage,
    controls: &PathControls,
    blocks: usize,
    frames: usize,
    value: f32,
) -> Vec<f32> {
    stage.reserve(frames);
    let mut last = Vec::new();
    for _ in 0..blocks {
        stage.begin(controls, 48_000, 64.0);
        let mut block = vec![[value, value, 0.0, 0.0]; frames];
        stage.process_block(&mut block, frames);
        last = block.iter().map(|frame| frame[0]).collect();
    }
    last
}

/// What the docs model settles on for a constant `value`.
fn settled(value: f32) -> f32 {
    let controls = controls(true);
    let mut stage = AmpStage::new(48_000);
    stage.install_prepared(tiny().prepared);
    *run(&mut stage, &controls, 1, 64, value).last().unwrap()
}

#[test]
fn a_tiny_wavenet_prewarms_and_stays_finite() {
    let controls = controls(true);
    let mut stage = AmpStage::new(48_000);
    stage.install_prepared(tiny().prepared);
    let out = run(&mut stage, &controls, 1, 32, 0.2);
    assert!(out.iter().all(|sample| sample.is_finite()));
    assert!(
        (settled(0.2) - 0.2).abs() > 1.0e-3,
        "the model shapes the signal"
    );
}

#[test]
fn bypass_and_no_model_are_the_dry_signal_undelayed() {
    let mut stage = AmpStage::new(48_000);
    stage.reserve(4);
    stage.install_prepared(tiny().prepared);
    let mut block = [[0.0; 4]; 4];
    block[0][0] = 1.0;
    block[0][1] = -1.0;
    stage.process_block(&mut block, 4);
    assert_eq!(block[0][0], 1.0, "bypass adds no delay");
    assert_eq!(block[0][1], -1.0);

    let mut dry = AmpStage::new(48_000);
    let out = run(&mut dry, &controls(true), 2, 4, 0.3);
    assert_eq!(out, [0.3; 4]);
}

#[test]
fn leaving_bypass_warms_the_model_before_it_is_heard() {
    let controls = controls(false);
    let mut stage = AmpStage::new(48_000);
    stage.install_prepared(tiny().prepared);
    assert_eq!(run(&mut stage, &controls, 2, 64, 0.2), [0.2; 64]);
    controls.set_amp(AmpControls {
        enabled: true,
        ..AmpControls::default()
    });
    let first = run(&mut stage, &controls, 1, 1, 0.2);
    assert_eq!(first, [0.2], "the first samples are still dry");
    // After the warm-up and the 5 ms fade the model is all that plays.
    let late = run(&mut stage, &controls, 20, 64, 0.2);
    assert!((late[63] - settled(0.2)).abs() < 1.0e-5);
}

#[test]
fn a_queued_model_fades_in_and_a_clear_fades_back_to_dry() {
    let controls = controls(true);
    let mut stage = AmpStage::new(48_000);
    assert_eq!(run(&mut stage, &controls, 1, 64, 0.2), [0.2; 64]);
    controls.queue_amp(tiny().prepared);
    let first = run(&mut stage, &controls, 1, 64, 0.2);
    assert_eq!(first[0], 0.2, "no jump when the model arrives");
    let target = settled(0.2);
    for pair in first.windows(2) {
        let step = (pair[1] - pair[0]).abs();
        assert!(step <= (target - 0.2).abs() / 100.0, "fades, step {step}");
    }
    let late = run(&mut stage, &controls, 20, 64, 0.2);
    assert!((late[63] - target).abs() < 1.0e-5);

    controls.clear_amp();
    let cleared = run(&mut stage, &controls, 1, 64, 0.2);
    assert!((cleared[0] - target).abs() < 1.0e-3, "no jump on clear");
    let late = run(&mut stage, &controls, 20, 64, 0.2);
    assert_eq!(late[63], 0.2);
}

#[test]
fn the_width_picks_the_first_submodel_above_it() {
    let steps = [0.33, 0.66, 1.0];
    assert_eq!(slim_submodel(&steps, 0.0), 0);
    assert_eq!(slim_submodel(&steps, 0.5), 1);
    assert_eq!(slim_submodel(&steps, 0.99), 2);
    assert_eq!(slim_submodel(&steps, 1.0), 2);
    assert_eq!(slim_submodel(&[], 0.5), 0);
}

#[test]
fn a_mismatched_sample_rate_does_not_install() {
    let json = format!(
        "{}, \"sample_rate\": 44100.0}}",
        TINY.trim().trim_end_matches('}')
    );
    match prepare_amp_json(&json, 48_000, 1.0) {
        AmpFile::RateMismatch { file_rate, .. } => assert_eq!(file_rate, 44_100),
        AmpFile::Ready(_) => panic!("a 44.1 kHz capture must not install at 48 kHz"),
        AmpFile::Failed(error) => panic!("parse failed: {error}"),
    }
}
