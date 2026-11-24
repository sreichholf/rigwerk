use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use azo::sys::SampleType;

use super::{AsioCallbackContext, AsioSignal};
use crate::audio::shared::Shared;

/// One input-side buffer switch over f32 channel buffers.  It calls
/// `process_input` directly rather than through `dispatch`: that keeps the
/// test off the process-wide `INPUT_CONTEXT` slot, and a panic fails the test
/// instead of being silenced by the callback's `catch_unwind`.  What
/// `dispatch` adds - the buffer-index clamp and the denormal guard - is not
/// what these tests measure.
fn run_input(shared: &Arc<Shared>, left: &[f32], right: Option<&[f32]>) {
    let mut buffers = vec![[left.as_ptr().cast_mut().cast::<c_void>(), ptr::null_mut()]];
    let mut types = vec![SampleType::PCM_F32_LSB];
    if let Some(right) = right {
        buffers.push([right.as_ptr().cast_mut().cast::<c_void>(), ptr::null_mut()]);
        types.push(SampleType::PCM_F32_LSB);
    }
    let mut context = AsioCallbackContext::new(Arc::clone(shared), AsioSignal::new(), left.len())
        .with_input(buffers, types);
    context.process_input(0);
}

fn ring(shared: &Shared) -> Vec<(f32, f32)> {
    std::iter::from_fn(|| shared.pop_frame()).collect()
}

/// The tuner's tap, which is taken before the channel enable.
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

/// Every value is an exact power-of-two fraction, so the gain product is
/// exact and the comparison is on the numbers, not on a tolerance.
///
/// Gain 2 and a muted left side keep the raw, tapped and pushed values
/// distinct: the tap is pre-mute, the ring is post-mute, and the peaks follow
/// the ring.
#[test]
fn a_stereo_input_pushes_the_driver_frames_and_publishes_the_peaks() {
    let shared = Arc::new(Shared::new(48_000));
    shared.input_gain.store(2.0f32.to_bits(), Ordering::Relaxed);
    shared.input_enabled[0].store(false, Ordering::Relaxed);
    let left = [0.5f32, -0.25, 0.125];
    let right = [-0.125f32, 0.75, -0.5];

    run_input(&shared, &left, Some(&right));

    assert_eq!(ring(&shared), vec![(0.0, -0.25), (0.0, 1.5), (0.0, -1.0)]);
    assert_eq!(
        tap(&shared, 3),
        vec![(1.0, -0.25), (-0.5, 1.5), (0.25, -1.0)]
    );
    assert_eq!(peaks(&shared), (0.0, 1.5));
    // `process_input` counts the block once, before the frame loop.
    assert_eq!(shared.snapshot().input_frames, 3);
}

/// A driver with one input channel only gives a left buffer; the right side
/// repeats that sample rather than reading past the channel list.
#[test]
fn a_mono_input_copies_its_only_channel_to_both_sides() {
    let shared = Arc::new(Shared::new(48_000));
    let left = [0.5f32, -0.25];

    run_input(&shared, &left, None);

    assert_eq!(ring(&shared), vec![(0.5, 0.5), (-0.25, -0.25)]);
    // Peaks are block maxima and a mono source repeats its sample, so both
    // sides hold the louder sample's level.
    assert_eq!(peaks(&shared), (0.5, 0.5));
    assert_eq!(shared.snapshot().input_frames, 2);
}
