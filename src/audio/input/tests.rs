use std::sync::atomic::Ordering;

use super::{input_block, whole_frames, InputStage};
use crate::audio::shared::{CallbackFailure, Shared};

/// Runs one callback's worth of raw frames the way `build_input` does: one
/// stage per block, the ring push of the returned frame, then the peaks.
fn run_block(shared: &Shared, frames: &[(f32, f32)]) {
    let mut stage = InputStage::new(shared);
    for &(raw_l, raw_r) in frames {
        let (left, right) = stage.frame(shared, raw_l, raw_r);
        shared.push_frame(left, right);
    }
    stage.publish(shared);
}

fn ring(shared: &Shared) -> Vec<(f32, f32)> {
    std::iter::from_fn(|| shared.pop_frame()).collect()
}

fn tap(shared: &Shared, frames: usize) -> Vec<(f32, f32)> {
    let mut samples = vec![0.0; frames * 2];
    assert_eq!(shared.copy_tap(&mut samples, frames), frames);
    samples
        .chunks_exact(2)
        .map(|pair| (pair[0], pair[1]))
        .collect()
}

fn peaks(shared: &Shared) -> (f32, f32) {
    (
        f32::from_bits(shared.in_l.load(Ordering::Relaxed)),
        f32::from_bits(shared.in_r.load(Ordering::Relaxed)),
    )
}

/// A host block that ends on a partial frame contributes only its whole
/// frames; the tail is dropped rather than read as a stereo pair.
#[test]
fn a_short_trailing_chunk_is_not_a_frame() {
    assert_eq!(whole_frames(0, 2), 0);
    assert_eq!(whole_frames(1, 2), 0);
    assert_eq!(whole_frames(3, 2), 1);
    assert_eq!(whole_frames(5, 2), 2);
    assert_eq!(whole_frames(4, 2), 2);
}

/// An odd sample count is not a whole number of frames on any channel count
/// above one.
#[test]
fn an_odd_length_counts_whole_frames_only() {
    assert_eq!(whole_frames(7, 2), 3);
    assert_eq!(whole_frames(7, 4), 1);
    assert_eq!(whole_frames(9, 4), 2);
}

/// A zero channel count cannot form a frame.  `build_input` also clamps it to
/// one, but the arithmetic must not divide by zero if it does not.
#[test]
fn zero_channels_has_no_frames() {
    assert_eq!(whole_frames(0, 0), 0);
    assert_eq!(whole_frames(5, 0), 0);
}

/// The input counterpart of the output guard's test: a body that panics does
/// not count the block as delivered or tap it, and marks the run failed.
/// Zero channels is the body panic a test can reach; the callback clamps it,
/// so this pins the guard rather than the real block shape.
#[test]
fn a_panicking_input_body_leaves_the_counters_and_the_tap_untouched() {
    let shared = Shared::new(48_000);
    shared.note_input_block(256);
    shared.note_input_frames(256);

    input_block(&[0.5f32, -0.25], 0, &shared);

    assert_eq!(shared.callback_failure(), Some(CallbackFailure::Panic));
    assert_eq!(shared.input_block.load(Ordering::Relaxed), 256);
    assert_eq!(shared.input_frames.load(Ordering::Relaxed), 256);
    let mut samples = [0.0f32; 8];
    assert_eq!(shared.copy_tap(&mut samples, 4), 0);
}

/// Every value here is an exact power-of-two fraction, so the products are
/// exact and the comparison is on the numbers, not on a tolerance.
#[test]
fn a_stereo_block_pushes_and_taps_the_same_frames() {
    let shared = Shared::new(48_000);
    let frames = [(0.5, -0.25), (0.25, 0.75), (-0.5, 0.125), (0.125, -0.5)];

    run_block(&shared, &frames);

    assert_eq!(ring(&shared), frames.to_vec());
    assert_eq!(tap(&shared, frames.len()), frames.to_vec());
    assert_eq!(peaks(&shared), (0.5, 0.75));
}

#[test]
fn a_duplicated_frame_reaches_the_ring_the_tap_and_the_peaks() {
    let shared = Shared::new(48_000);
    // `build_input` reads `frame[1]` only on a multi-channel stream and
    // repeats `frame[0]` for a mono one before the stage sees it.
    let mono = [0.5, -0.25, 0.125];
    let frames: Vec<(f32, f32)> = mono.iter().map(|&sample| (sample, sample)).collect();

    run_block(&shared, &frames);

    assert_eq!(ring(&shared), frames);
    assert_eq!(tap(&shared, frames.len()), frames);
    assert_eq!(peaks(&shared), (0.5, 0.5));
}

/// The tap is taken before the channel enable.  Swapping the two would leave
/// the tap silent on the muted side while the ring still looked correct.
#[test]
fn a_muted_left_side_still_reaches_the_tap() {
    let shared = Shared::new(48_000);
    shared.set_master(false, [false, true], true);
    let frames = [(0.5, 0.25), (-0.75, 0.125)];

    run_block(&shared, &frames);

    assert_eq!(ring(&shared), vec![(0.0, 0.25), (0.0, 0.125)]);
    assert_eq!(tap(&shared, frames.len()), frames.to_vec());
    // The peaks follow the muted frame, as the meters always have.
    assert_eq!(peaks(&shared), (0.0, 0.25));
}

#[test]
fn a_muted_right_side_still_reaches_the_tap() {
    let shared = Shared::new(48_000);
    shared.set_master(false, [true, false], true);
    let frames = [(0.5, 0.25), (-0.75, -0.125)];

    run_block(&shared, &frames);

    assert_eq!(ring(&shared), vec![(0.5, 0.0), (-0.75, 0.0)]);
    assert_eq!(tap(&shared, frames.len()), frames.to_vec());
    assert_eq!(peaks(&shared), (0.75, 0.0));
}

/// Both sides muted is reachable from the UI.  The ring and the peaks go
/// flat while the tap still carries the pair.
#[test]
fn both_sides_muted_leave_an_all_zero_ring_and_an_intact_tap() {
    let shared = Shared::new(48_000);
    shared.set_master(false, [false, false], true);
    let frames = [(0.5, 0.25), (-0.75, -0.125)];

    run_block(&shared, &frames);

    assert_eq!(ring(&shared), vec![(0.0, 0.0), (0.0, 0.0)]);
    assert_eq!(tap(&shared, frames.len()), frames.to_vec());
    assert_eq!(peaks(&shared), (0.0, 0.0));
}

/// The gain is applied before the tap and the peaks, so the tuner sees the
/// level the ring carries.
#[test]
fn input_gain_scales_the_ring_the_tap_and_the_peaks() {
    let shared = Shared::new(48_000);
    shared.input_gain.store(2.0f32.to_bits(), Ordering::Relaxed);
    let frames = [(0.25, -0.125), (-0.5, 0.0625)];
    let scaled = [(0.5, -0.25), (-1.0, 0.125)];

    run_block(&shared, &frames);

    assert_eq!(ring(&shared), scaled.to_vec());
    assert_eq!(tap(&shared, scaled.len()), scaled.to_vec());
    assert_eq!(peaks(&shared), (1.0, 0.25));
}

/// A calibration capture is taken from the raw frame, so it cannot mistake
/// the input gain for the device's own level.  Moving it below the gain
/// would record 0.5 here instead of 0.25.
#[test]
fn a_calibration_capture_records_the_raw_frame_before_the_gain() {
    let shared = Shared::new(48_000);
    shared.input_gain.store(2.0f32.to_bits(), Ordering::Relaxed);
    shared
        .calibration
        .arm(&[0.5, -0.5], 48_000, 1, 1)
        .expect("the measurement armed");
    shared.calibration.note_ducked();
    // `arm` holds two input blocks for the duck's tail, so the first two
    // frames are dropped and the rest are captured.
    let frames = [(0.25, -0.125); 4];

    run_block(&shared, &frames);

    let mut left = Vec::new();
    let mut right = Vec::new();
    assert_eq!(shared.calibration.copy_capture(&mut left, &mut right), 2);
    assert_eq!(left, vec![0.25, 0.25]);
    assert_eq!(right, vec![-0.125, -0.125]);
    // The same frames reach the ring and the tap after the gain.
    assert_eq!(ring(&shared), vec![(0.5, -0.25); 4]);
    assert_eq!(tap(&shared, 4), vec![(0.5, -0.25); 4]);
}

/// A full ring drops frames, and only the ring does: the tap still sees them
/// and the peaks still hold them.
#[test]
fn a_full_ring_drops_frames_while_the_tap_and_the_peaks_keep_them() {
    let shared = Shared::new(48_000);
    let capacity = shared.capacity_frames;
    assert_eq!(capacity, 96_000);
    let count = capacity + 2;
    let mut stage = InputStage::new(&shared);
    for frame in 0..count {
        let sample = if frame < capacity { 0.25 } else { 0.5 };
        let (left, right) = stage.frame(&shared, sample, -sample);
        shared.push_frame(left, right);
    }
    stage.publish(&shared);

    let pushed = ring(&shared);
    assert_eq!(pushed.len(), capacity);
    assert!(pushed
        .iter()
        .all(|&(left, right)| left == 0.25 && right == -0.25));
    assert_eq!(shared.overruns.load(Ordering::Relaxed), 2);

    let tapped = tap(&shared, 4);
    assert_eq!(
        tapped,
        vec![(0.25, -0.25), (0.25, -0.25), (0.5, -0.5), (0.5, -0.5)]
    );
    assert_eq!(peaks(&shared), (0.5, 0.5));
}
