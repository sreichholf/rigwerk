use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use azo::sys::SampleType;

use super::{
    wait_for_callbacks, AsioCallbackContext, AsioCallbackSlot, AsioHost, AsioSignal,
    CallbackRegistration, CallbackWait,
};
use crate::audio::shared::Shared;

/// Calls `process_input` directly, not `dispatch`: that stays off the
/// process-global callback slot, and a panic fails the test instead of
/// being caught.  The clamp and the denormal guard are not measured here.
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

/// Taken before the channel enable.
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

/// Powers of two, so the gain product is exact.  Gain 2 and a muted left
/// keep the raw, tapped and pushed values distinct: the tap is pre-mute,
/// the ring is post-mute, and the peaks follow the ring.
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
    // One add for the block and one per frame give the same total.
    assert_eq!(shared.snapshot().input_frames, 3);
}

/// One input channel gives only a left buffer; the right side repeats
/// that sample rather than reading past the channel list.
#[test]
fn a_mono_input_copies_its_only_channel_to_both_sides() {
    let shared = Arc::new(Shared::new(48_000));
    let left = [0.5f32, -0.25];

    run_input(&shared, &left, None);

    assert_eq!(ring(&shared), vec![(0.5, 0.5), (-0.25, -0.25)]);
    // Peaks are block maxima, and a mono source repeats its sample.
    assert_eq!(peaks(&shared), (0.5, 0.5));
    assert_eq!(shared.snapshot().input_frames, 2);
}

/// Every test leaks its own host, so a registration never lands in the
/// production `ASIO_HOST`.
fn install(
    host: &'static AsioHost,
    slot: AsioCallbackSlot,
    shared: &Arc<Shared>,
    signal: &Arc<AsioSignal>,
) -> Result<CallbackRegistration, String> {
    let context = AsioCallbackContext::new(Arc::clone(shared), Arc::clone(signal), 1);
    CallbackRegistration::install(host, slot, Box::new(context), Arc::clone(signal))
}

/// A reset belongs to the registration installed when it arrived.  It
/// reaches every installed role once, and a later session baselines the
/// bump away.
#[test]
fn a_reset_request_reaches_the_installed_session_only() {
    let host: &'static AsioHost = Box::leak(Box::new(AsioHost::new()));
    let shared = Arc::new(Shared::new(48_000));

    let input_signal = AsioSignal::new();
    let input = install(host, AsioCallbackSlot::Input, &shared, &input_signal)
        .expect("the empty input slot takes a session");

    host.request_reset();
    assert!(input.take_reset());
    assert!(!input.take_reset(), "one bump ends one wait");

    input.clear();
    host.request_reset();
    let input_signal = AsioSignal::new();
    let input = install(host, AsioCallbackSlot::Input, &shared, &input_signal)
        .expect("the input slot is free again");
    assert!(
        !input.take_reset(),
        "a fresh install captures a fresh baseline"
    );

    // One bump reaches both roles installed when it arrives.
    let output_signal = AsioSignal::new();
    let output = install(host, AsioCallbackSlot::Output, &shared, &output_signal)
        .expect("the empty output slot takes a session");

    host.request_reset();
    assert!(input.take_reset());
    assert!(output.take_reset());
    assert!(!input.take_reset());
    assert!(!output.take_reset());

    input.clear();
    output.clear();
}

/// The host refuses a second session per role.  The first context stays
/// installed and keeps receiving resets; the second gets no slot.
#[test]
fn a_second_install_on_the_same_slot_is_refused_by_the_owner() {
    let host: &'static AsioHost = Box::leak(Box::new(AsioHost::new()));
    let shared = Arc::new(Shared::new(48_000));

    let first_signal = AsioSignal::new();
    let first = install(host, AsioCallbackSlot::Input, &shared, &first_signal)
        .expect("the empty input slot takes the first session");

    let second_signal = AsioSignal::new();
    let error = match install(host, AsioCallbackSlot::Input, &shared, &second_signal) {
        Ok(_) => panic!("the owner must refuse a second session on one slot"),
        Err(error) => error,
    };
    assert_eq!(
        error,
        "The previous ASIO session is still shutting down and has not released its driver callbacks."
    );

    // The refusal leaves the installed context in place: it still owns
    // the slot and still sees the reset.
    host.request_reset();
    assert!(first.take_reset());

    first.clear();

    let second = install(host, AsioCallbackSlot::Input, &shared, &second_signal)
        .expect("the released slot takes the next session");
    assert!(
        !second.take_reset(),
        "the second session does not inherit the earlier bump"
    );
    host.request_reset();
    assert!(second.take_reset());

    second.clear();
}

/// A reset between `install` and the first wait stays pending.  After
/// one wait consumes it, a later wait does not fire again.
#[test]
fn a_reset_before_the_wait_is_still_reported() {
    let host: &'static AsioHost = Box::leak(Box::new(AsioHost::new()));
    let shared = Arc::new(Shared::new(48_000));
    let signal = AsioSignal::new();
    let registration = install(host, AsioCallbackSlot::Input, &shared, &signal)
        .expect("the empty input slot takes a session");

    host.request_reset();
    let wait = wait_for_callbacks(&registration, || false, || false);
    assert!(
        matches!(wait, CallbackWait::Reset),
        "the gap reset is pending"
    );

    // The wait must pass through `take_reset()` before `stopped`, or this
    // would only prove that `stopped` is read first.
    let ticks = std::cell::Cell::new(0);
    let wait = wait_for_callbacks(
        &registration,
        || ticks.get() > 1,
        || {
            ticks.set(ticks.get() + 1);
            false
        },
    );
    assert!(
        matches!(wait, CallbackWait::Stopped),
        "a second wait is Stopped, not Reset"
    );

    registration.clear();
}

/// `Drop` detaches the slot without freeing the context.  The role can
/// be installed again, from its own baseline.
#[test]
fn dropping_a_registration_lets_the_slot_be_installed_again() {
    let host: &'static AsioHost = Box::leak(Box::new(AsioHost::new()));
    let shared = Arc::new(Shared::new(48_000));

    let first_signal = AsioSignal::new();
    let first = install(host, AsioCallbackSlot::Input, &shared, &first_signal)
        .expect("the empty input slot takes a session");
    host.request_reset();
    drop(first);

    let second_signal = AsioSignal::new();
    let second = install(host, AsioCallbackSlot::Input, &shared, &second_signal)
        .expect("Drop released the slot");
    assert!(
        !second.take_reset(),
        "no reset is attributed to the session after a detach"
    );

    host.request_reset();
    assert!(second.take_reset());

    second.clear();
}
