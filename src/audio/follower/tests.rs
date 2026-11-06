//! Unit tests for generic follower playback (ring → map).

use std::sync::atomic::Ordering;

use super::super::output::write_frame;
use super::super::output_map::{old_write_frame, OutputDeviceSettings};
use super::super::shared::{FollowerLink, FollowerPlan, Shared};
use super::super::types::CushionMode;
use super::mixer::FollowerMixer;

#[test]
fn follower_mixer_writes_the_default_stereo_map() {
    let shared = Shared::new(48_000);
    shared.set_output(48_000, 64, "test");
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
        &[(CushionMode::Manual, 100.0)],
    );
    shared
        .follower_ring_active(0)
        .store(true, Ordering::Relaxed);
    let bus = shared.follower_bus(0);
    bus.reset_playback();
    bus.set_cushion(CushionMode::Manual, 100.0);
    bus.set_source_rate(48_000);
    bus.set_target_frames(64);
    for _ in 0..5_000 {
        bus.push_frame(0.5, -0.25);
    }

    let mut mixer = FollowerMixer::new(0, 48_000, 2);
    mixer.publish_channels(&shared);
    let mut out = vec![[0.0f32; 2]; 64];
    mixer.render(&shared, 64, |index, mapped| {
        write_frame(&mut out[index], mapped);
    });
    // After priming through a deep cushion, frames match the default map.
    let expected = {
        let mut frame = [0.0f32; 2];
        old_write_frame(&mut frame, 0.5, -0.25);
        frame
    };
    assert_eq!(*out.last().unwrap(), expected);
}

#[test]
fn a_follower_underrun_does_not_stop_the_clock() {
    let shared = Shared::new(48_000);
    shared.set_output(48_000, 64, "test");
    shared.running.store(true, Ordering::Relaxed);
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
        &[(CushionMode::Manual, 1.0)],
    );
    shared
        .follower_ring_active(0)
        .store(true, Ordering::Relaxed);
    let bus = shared.follower_bus(0);
    bus.reset_playback();
    bus.set_source_rate(48_000);
    // 1 ms at 48 kHz → 48-frame cushion. Leave priming, then run dry.
    bus.set_cushion(CushionMode::Manual, 1.0);
    for _ in 0..48 {
        bus.push_frame(0.4, 0.4);
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
}
