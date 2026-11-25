use cpal::Sample;

use super::super::output_map::{DeviceControls, DeviceMapper};
use super::{block_frames, render_or_silence, write_frame};
use crate::audio::shared::{CallbackFailure, Shared};

/// Maps one frame with the default device settings (L → 1, R → 2, mono
/// copy on the rest) and writes it like the cpal callback does.
fn written<const N: usize>(left: f32, right: f32) -> [f32; N] {
    let controls = DeviceControls::new();
    let mut mapper = DeviceMapper::new(N);
    mapper.begin_block(&controls, 288.0);
    let mut frame = [0.0f32; N];
    write_frame(&mut frame, mapper.map(left, right));
    frame
}

#[test]
fn write_frame_keeps_the_processed_stereo_pair() {
    assert_eq!(written::<2>(0.5, -0.25), [0.5, -0.25]);
}

#[test]
fn write_frame_fills_extra_channels_from_the_stereo_mix() {
    assert_eq!(written::<4>(0.5, -0.25), [0.5, -0.25, 0.125, 0.125]);
}

#[test]
fn write_frame_sums_mono_devices() {
    assert_eq!(written::<1>(0.5, -0.25), [0.125]);
}

#[test]
fn write_frame_converts_to_the_device_format() {
    let controls = DeviceControls::new();
    let mut mapper = DeviceMapper::new(2);
    mapper.begin_block(&controls, 288.0);
    let mut frame = [0i16; 2];
    // Under the safety ceiling, so the mapper copies and the conversion
    // is the only thing this checks.
    write_frame(&mut frame, mapper.map(0.5, -0.5));
    assert_eq!(frame, [i16::from_sample(0.5f32), i16::from_sample(-0.5f32)]);
}

/// A block is only writable frame by frame when its samples divide into
/// whole frames.
#[test]
fn a_block_with_a_partial_frame_is_refused() {
    assert_eq!(block_frames(480, 2), Some(240));
    assert_eq!(block_frames(481, 2), None);
    assert_eq!(block_frames(3, 2), None);
    assert_eq!(block_frames(7, 4), None);
    assert_eq!(block_frames(0, 2), Some(0));
}

/// A zero channel count cannot form a frame, let alone be written.
#[test]
fn zero_channels_is_refused() {
    assert_eq!(block_frames(0, 0), None);
    assert_eq!(block_frames(480, 0), None);
}

/// The guard both cpal output callbacks use: a panicking body leaves the
/// device block at silence and marks the run failed.  Removing the
/// `catch_unwind` inside `render_or_silence` fails this test.
#[test]
fn a_panicking_output_body_leaves_the_block_silent() {
    let shared = Shared::new(48_000);
    let mut block = [0.5f32; 4];

    render_or_silence(&shared, &mut block, |_| panic!("a plugin failed"));

    assert_eq!(block, [f32::EQUILIBRIUM; 4]);
    assert_eq!(shared.callback_failure(), Some(CallbackFailure::Panic));
}

/// The unsigned formats silence at their midpoint, not at zero.
#[test]
fn a_panicking_output_body_silences_an_unsigned_block_at_its_midpoint() {
    let shared = Shared::new(48_000);
    let mut block = [7u8; 2];

    render_or_silence(&shared, &mut block, |_| panic!("a plugin failed"));

    assert_eq!(block, [u8::EQUILIBRIUM; 2]);
    assert_eq!(shared.callback_failure(), Some(CallbackFailure::Panic));
}

/// A body that returns leaves the block it wrote and no failure behind.
#[test]
fn a_returning_output_body_keeps_the_block() {
    let shared = Shared::new(48_000);
    let mut block = [0.5f32; 4];

    render_or_silence(&shared, &mut block, |block| block[0] = 0.25);

    assert_eq!(block[0], 0.25);
    assert_eq!(shared.callback_failure(), None);
}
