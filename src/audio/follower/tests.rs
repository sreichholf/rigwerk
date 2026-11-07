//! Unit tests for generic follower playback (ring → device).

use std::sync::atomic::Ordering;

use super::super::output::write_frame;
use super::super::output_map::OutputDeviceSettings;
use super::super::shared::{FollowerLink, FollowerPlan, Shared};
use super::super::types::CushionMode;
use super::mixer::FollowerMixer;

/// A shared state with one generic follower slot, its ring laid out for a
/// `channels`-channel device and a manual cushion of `cushion_ms`.
fn one_follower(channels: usize, cushion_ms: f32) -> Shared {
    one_follower_at(48_000, channels, cushion_ms)
}

/// [`one_follower`] with everything at `rate`.
fn one_follower_at(rate: u32, channels: usize, cushion_ms: f32) -> Shared {
    let shared = Shared::new(rate);
    shared.set_output(rate, 64, "test");
    shared.drift_enabled.store(false, Ordering::Relaxed);
    let mut plan = FollowerPlan {
        count: 1,
        ..FollowerPlan::default()
    };
    plan.slots[0] = FollowerLink {
        device_id: "test:follower".to_owned(),
        ..FollowerLink::default()
    };
    shared.set_followers(
        &[OutputDeviceSettings::default()],
        &plan,
        &[(CushionMode::Manual, cushion_ms)],
    );
    let bus = shared.follower_bus(0);
    bus.set_device_channels(channels);
    bus.reset_playback();
    bus.set_cushion(CushionMode::Manual, cushion_ms);
    bus.set_source_rate(rate);
    shared
        .follower_ring_active(0)
        .store(true, Ordering::Relaxed);
    shared
}

#[test]
fn follower_mixer_plays_the_device_frames_from_its_ring() {
    let shared = one_follower(2, 100.0);
    let bus = shared.follower_bus(0);
    bus.set_target_frames(64);
    for _ in 0..5_000 {
        bus.push_lanes(&[0.5, -0.25]);
    }

    let mut mixer = FollowerMixer::new(0, 48_000, 2);
    mixer.publish_channels(&shared);
    assert_eq!(shared.followers[0].channels(), 2);
    let mut out = vec![[0.0f32; 2]; 64];
    mixer.render(&shared, 64, |index, mapped| {
        write_frame(&mut out[index], mapped);
    });
    // After priming through a deep cushion, the ring's frames play as is:
    // the clock side already mapped them.
    assert_eq!(*out.last().unwrap(), [0.5, -0.25]);
}

#[test]
fn every_lane_of_a_multichannel_follower_plays() {
    let shared = one_follower(6, 100.0);
    let bus = shared.follower_bus(0);
    assert_eq!(bus.ring.lanes(), 6);
    let frame = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6];
    for _ in 0..5_000 {
        bus.push_lanes(&frame);
    }
    let mut mixer = FollowerMixer::new(0, 48_000, 6);
    let mut out = vec![[0.0f32; 6]; 64];
    mixer.render(&shared, 64, |index, mapped| {
        write_frame(&mut out[index], mapped);
    });
    assert_eq!(*out.last().unwrap(), frame);
}

#[test]
fn a_follower_underrun_does_not_stop_the_clock() {
    let shared = one_follower(2, 1.0);
    shared.running.store(true, Ordering::Relaxed);
    let bus = shared.follower_bus(0);
    // 1 ms at 48 kHz → 48-frame cushion. Leave priming, then run dry.
    for _ in 0..48 {
        bus.push_lanes(&[0.4, 0.4]);
    }

    let mut mixer = FollowerMixer::new(0, 48_000, 2);
    mixer.render(&shared, 128, |_index, _mapped| {});
    assert!(
        bus.underruns.load(Ordering::Relaxed) >= 1,
        "empty follower ring should count an underrun"
    );
    assert!(
        shared.running.load(Ordering::Relaxed),
        "follower underrun must not stop the bridge"
    );
    assert_eq!(shared.underruns.load(Ordering::Relaxed), 0);
    let snapshot = shared.snapshot();
    assert!(snapshot.follower_underruns[0] >= 1, "per follower");
    assert_eq!(snapshot.underruns, 0, "not the clock output's");
}

#[test]
fn a_missing_follower_shows_in_the_snapshot_until_it_reopens() {
    let shared = one_follower(2, 10.0);
    let bus = shared.follower_bus(0);
    bus.missing.store(true, Ordering::Relaxed);
    assert_eq!(
        shared.snapshot().follower_missing,
        [true, false, false, false]
    );
    bus.reset_playback();
    assert!(!shared.snapshot().follower_missing[0]);
}

#[test]
fn a_ring_held_far_above_its_cushion_pins_the_drift_loop() {
    let shared = one_follower(2, 200.0);
    shared.drift_enabled.store(true, Ordering::Relaxed);
    let bus = shared.follower_bus(0);
    let target = bus.target_frames();
    assert_eq!(target, 9_600);
    // Just under the purge line: the device seems to run slow, far more
    // than the loop may correct.
    let held = target * 2;
    let mut mixer = FollowerMixer::new(0, 48_000, 2);
    let mut pinned_after = None;
    for block in 0..400 {
        while bus.ring_frames() < held {
            bus.push_lanes(&[0.1, 0.1]);
        }
        mixer.render(&shared, 1_024, |_, _| {});
        if bus.drift_at_limit.load(Ordering::Relaxed) {
            pinned_after = Some(block);
            break;
        }
    }
    assert!(pinned_after.is_some(), "the integral reaches its limit");
    assert!(shared.snapshot().follower_drift_at_limit[0]);
    bus.reset_playback();
    assert!(
        !shared.snapshot().follower_drift_at_limit[0],
        "a reopen clears it"
    );
}

#[test]
fn closing_a_large_cushion_step_does_not_flag_the_drift_loop() {
    // 8 kHz and 256-frame blocks (32 ms): the loop's seconds, fewer frames.
    let shared = one_follower_at(8_000, 2, 200.0);
    shared.drift_enabled.store(true, Ordering::Relaxed);
    let bus = shared.follower_bus(0);
    let mut mixer = FollowerMixer::new(0, 8_000, 2);
    let mut block = |pushed: usize| {
        for _ in 0..pushed {
            bus.push_lanes(&[0.1, 0.1]);
        }
        mixer.render(&shared, 256, |_, _| {});
        bus.drift_at_limit.load(Ordering::Relaxed)
    };
    while bus.ring_frames() < bus.target_frames() {
        bus.push_lanes(&[0.1, 0.1]);
    }
    // The clock side feeds exactly what the device takes: no clock error.
    for _ in 0..100 {
        assert!(!block(256));
    }
    // 30 ms less cushion: far more ring error than the proportional term
    // alone corrects at the limit, so the integral winds up while the loop
    // drains it (about a minute), with nothing wrong with the clock.
    bus.set_cushion(CushionMode::Manual, 170.0);
    let mut closed = None;
    for index in 0..3_000 {
        assert!(!block(256), "flagged {index} blocks after the step");
        if bus.ring_frames() < bus.target_frames() {
            closed.get_or_insert(index);
        }
        if closed.is_some_and(|closed| index > closed + 100) {
            break;
        }
    }
    assert!(closed.is_some(), "the loop closed the step");
    assert_eq!(bus.underruns.load(Ordering::Relaxed), 0);

    // A device that really takes about 2000 ppm more than it is fed still
    // pins it.
    let pinned = (0..3_000).any(|index| block(if index % 2 == 0 { 255 } else { 256 }));
    assert!(pinned, "a real clock error still flags");
}

/// A wide follower with 8192-frame blocks at `rate`: the Safe cushion is
/// not clamped below its floor, and the excess purge still fires.
fn a_wide_ring_holds_a_safe_cushion(rate: u32, channels: usize) {
    let shared = Shared::new(rate);
    shared.set_output(rate, 64, "test");
    shared.drift_enabled.store(false, Ordering::Relaxed);
    let bus = shared.follower_bus(0);
    bus.set_device_channels(channels);
    bus.reset_playback();
    bus.set_cushion(CushionMode::Safe, 10.0);
    bus.set_source_rate(rate);
    // 8192-frame follower blocks: the Safe floor is two of them.
    bus.note_output_block(8_192);
    assert_eq!(bus.floor_frames(), 16_384);
    assert_eq!(bus.target_frames(), 16_384, "not clamped below the floor");
    let purge_line = bus.target_frames() * 2 + 8_192;
    assert!(purge_line < bus.ring.capacity_frames());

    // A ring filled past the purge line is trimmed back to the target.
    shared
        .follower_ring_active(0)
        .store(true, Ordering::Relaxed);
    let frame = vec![0.25f32; channels];
    for _ in 0..purge_line + 20_000 {
        bus.push_lanes(&frame);
    }
    assert_eq!(bus.overruns.load(Ordering::Relaxed), 0);
    let mut mixer = FollowerMixer::new(0, rate, channels);
    mixer.render(&shared, 8_192, |_, _| {});
    let before = bus.ring_frames();
    mixer.render(&shared, 8_192, |_, _| {});
    assert!(
        bus.ring_frames() <= bus.target_frames(),
        "purged from {before} to {}",
        bus.ring_frames()
    );
}

#[test]
fn eight_and_more_channels_hold_a_safe_cushion() {
    a_wide_ring_holds_a_safe_cushion(48_000, 8);
    a_wide_ring_holds_a_safe_cushion(48_000, 12);
    a_wide_ring_holds_a_safe_cushion(192_000, 8);
}

#[test]
fn a_relaid_ring_takes_no_frames_from_an_older_block() {
    let shared = one_follower(2, 10.0);
    let bus = shared.follower_bus(0);
    let generation = bus.generation();
    let frame = super::super::output_map::DeviceFrame::from_lanes(&[0.5, 0.5], 2);
    shared.push_follower_frame(0, generation, &frame);
    assert_eq!(bus.ring_frames(), 1);
    // The stream reopened on another device mid-block.
    bus.set_device_channels(4);
    bus.reset_playback();
    shared.push_follower_frame(0, generation, &frame);
    assert_eq!(bus.ring_frames(), 0);
    shared.push_follower_frame(0, bus.generation(), &frame);
    assert_eq!(bus.ring_frames(), 1);
}
