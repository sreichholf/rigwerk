use super::super::super::mixer::drift::{DRIFT_LIMIT_RATIO, DRIFT_STEP_SECONDS};
use super::super::super::shared::Shared;
use super::ExtraOutputMixer;

/// A block's worth of slack, as `render` passes it: 96 frames at 48 kHz.
const MARGIN: f32 = 96.0 / 48_000.0;

/// A mixer outside priming with its integral at the limit, so `drift_pinned`
/// answers only the step question.  `previous` is the target it steered to
/// last block, `fill` the ring's fill, `error` the filtered error.
fn pinned_mixer(previous: usize, fill: usize, error: f32) -> (ExtraOutputMixer, Shared) {
    let shared = Shared::new(48_000);
    let bus = shared.extra_output_bus(0);
    bus.set_source_rate(48_000);
    for _ in 0..fill {
        bus.push_lanes(&[0.0, 0.0]);
    }
    assert_eq!(bus.ring_frames(), fill);
    let mut mixer = ExtraOutputMixer::new(0, 48_000, 2);
    mixer.priming = false;
    mixer.steer_target = previous;
    mixer.drift_integral = DRIFT_LIMIT_RATIO;
    mixer.drift_error = Some(error);
    (mixer, shared)
}

/// A target step of at least `DRIFT_STEP_SECONDS` opens a step error, and a
/// filtered error that keeps its sign and stays inside the margin leaves it
/// open: the pin belongs to the step, not the clock.
#[test]
fn a_target_step_explains_the_drift_pin() {
    let previous = 1_000;
    let target = 1_512; // 512 frames is past the 400-frame threshold.
    assert!(
        (target - previous) as f32 / 48_000.0 >= DRIFT_STEP_SECONDS,
        "the fixture must step past the threshold"
    );
    let fill = 1_000;
    let error = (fill as f32 - target as f32) / 48_000.0;
    let (mut mixer, shared) = pinned_mixer(previous, fill, error);
    assert!(!mixer.drift_pinned(&shared, target, MARGIN));
    assert!(mixer.step_error.is_some(), "the step stays open");
}

/// The step error is signed from the fill: a target that jumps above the
/// fill leaves a negative error.  An error that turns positive has crossed
/// over, so the step no longer explains the pin.
#[test]
fn an_error_that_turns_against_the_step_is_blamed_on_drift() {
    let (mut mixer, shared) = pinned_mixer(1_000, 1_000, 0.001);
    assert!(mixer.drift_pinned(&shared, 1_512, MARGIN));
    assert!(
        mixer.step_error.is_none(),
        "the step no longer explains the error"
    );
}

/// How far past the step counts as no longer the step's doing.  The same
/// step is explained while the error stays inside the margin and blames the
/// clock once it grows past it.
#[test]
fn the_drift_margin_is_the_slack_a_step_gets() {
    let step = (1_000.0 - 1_512.0) / 48_000.0;
    let inside = step - MARGIN * 0.5;
    let (mut mixer, shared) = pinned_mixer(1_000, 1_000, inside);
    assert!(
        !mixer.drift_pinned(&shared, 1_512, MARGIN),
        "inside the margin the step explains the error"
    );

    let outside = step - MARGIN * 1.5;
    let (mut mixer, shared) = pinned_mixer(1_000, 1_000, outside);
    assert!(
        mixer.drift_pinned(&shared, 1_512, MARGIN),
        "past the margin the step no longer explains it"
    );
}

/// A target move under `DRIFT_STEP_SECONDS` is not a step the loop tracks at
/// all: no step error is opened, so the pin stays with the clock.
#[test]
fn a_step_below_the_threshold_never_explains_the_drift_pin() {
    let previous = 1_000;
    let target = 1_399; // 399 frames at 48 kHz, under the threshold.
    assert!(
        (target - previous) as f32 / 48_000.0 < DRIFT_STEP_SECONDS,
        "the fixture must stay under the threshold"
    );
    let error = (previous as f32 - target as f32) / 48_000.0;
    let (mut mixer, shared) = pinned_mixer(previous, previous, error);
    assert!(mixer.drift_pinned(&shared, target, MARGIN));
    assert!(mixer.step_error.is_none(), "no step was opened");
}
