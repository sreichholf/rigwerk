use cpal::Sample;

use super::super::output_map::{DeviceControls, DeviceMapper};
use super::write_frame;

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
