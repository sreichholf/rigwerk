#![allow(clippy::needless_range_loop)] // Frames of several devices, indexed alike.

//! Latency alignment through the whole mixer: two identical paths, one with
//! a latent insert (a pure delay standing in for a lookahead plugin), on
//! the clock device, the followers of the PipeWire direct path and a generic
//! follower fed through its ring.

use super::super::eq::EqSettings;
use super::super::output_map::{ChannelMap, OutputDeviceSettings, PathMapSettings, UnusedChannels};
use super::super::path::PathMix;
use super::super::shared::{FollowerPlan, Shared};
use super::OutputMixer;

const RATE: u32 = 48_000;
const BLOCK: usize = 256;

/// A low sine: one sample moves it by at most `0.5 * 0.02`.
fn signal(n: usize) -> f32 {
    (n as f32 * 0.02).sin() * 0.5
}

/// `n` frames back, silence before the start.
fn delayed(n: usize, by: usize) -> f32 {
    n.checked_sub(by).map_or(0.0, signal)
}

/// Two identical paths (EQs off, IN 1/2 on L/R), path 0 with a pure delay
/// of `latency` frames, both on every device; `followers` stereo followers.
fn aligned_mixer(latency: u32, followers: usize) -> (OutputMixer, Shared) {
    let shared = Shared::new(RATE);
    shared.set_master(false, true, [true, true], false);
    // The duplex graph: it plays the followers in-graph.
    shared
        .direct
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let off = EqSettings {
        enabled: false,
        ..EqSettings::default()
    };
    let mix = PathMix {
        eqs: [off, off],
        ..PathMix::default()
    };
    shared.restore_path(0, &mix, Vec::new());
    shared.restore_path(1, &mix, Vec::new());
    shared.paths[0].edit_chain(|chain| chain.insert_delay_for_test(latency));
    assert_eq!(shared.paths[0].latency_samples(), latency);
    let plan = FollowerPlan {
        count: followers,
        ..FollowerPlan::default()
    };
    shared.set_followers(
        &vec![OutputDeviceSettings::default(); followers],
        &plan,
        &[],
    );
    (OutputMixer::new(RATE, 2), shared)
}

/// Every device's frames (L, R) over `blocks` direct blocks, starting at
/// input frame `*clock`.  The input is the sine on L and its negative on R.
fn run(
    mixer: &mut OutputMixer,
    shared: &Shared,
    clock: &mut usize,
    blocks: usize,
) -> Vec<Vec<[f32; 2]>> {
    let devices = 1 + shared.follower_count();
    let mut out = vec![Vec::new(); devices];
    for _ in 0..blocks {
        let left: Vec<f32> = (*clock..*clock + BLOCK).map(signal).collect();
        let right: Vec<f32> = left.iter().map(|value| -value).collect();
        mixer.render_direct_devices(shared, &left, &right, |device, _, mapped| {
            out[device].push([mapped.sample(0), mapped.sample(1)]);
        });
        *clock += BLOCK;
    }
    out
}

fn largest_step(frames: &[[f32; 2]], side: usize) -> f32 {
    frames
        .windows(2)
        .map(|pair| (pair[1][side] - pair[0][side]).abs())
        .fold(0.0, f32::max)
}

#[test]
fn aligned_paths_sum_to_twice_the_delayed_signal() {
    const LATENCY: usize = 64;
    let (mut mixer, shared) = aligned_mixer(LATENCY as u32, 0);
    let mut clock = 0;
    let out = run(&mut mixer, &shared, &mut clock, 8);
    for (n, frame) in out[0].iter().enumerate() {
        let want = 2.0 * delayed(n, LATENCY);
        assert_eq!(frame[0], want, "frame {n}");
        assert_eq!(frame[1], -want, "frame {n}");
    }
    // What the mixer applies is published for the UI.
    let report = shared.clock_device.alignment().expect("published");
    assert_eq!(report.delays[0], Some(0));
    assert_eq!(report.delays[1], Some(LATENCY as u32));
    assert_eq!(report.waits_for, Some(0));
    assert_eq!(report.total, LATENCY as u32);
    assert_eq!(report.added, LATENCY as u32);
}

#[test]
fn without_alignment_the_paths_comb() {
    const LATENCY: usize = 64;
    let (mut mixer, shared) = aligned_mixer(LATENCY as u32, 0);
    shared.clock_device.set(&OutputDeviceSettings {
        align: false,
        ..OutputDeviceSettings::default()
    });
    let mut clock = 0;
    let out = run(&mut mixer, &shared, &mut clock, 8);
    let mut worst = 0.0f32;
    for (n, frame) in out[0].iter().enumerate() {
        let comb = delayed(n, LATENCY) + signal(n);
        assert!((frame[0] - comb).abs() < 1e-6, "frame {n}");
        worst = worst.max((frame[0] - 2.0 * delayed(n, LATENCY)).abs());
    }
    assert!(worst > 0.3, "the sum differs from the aligned one: {worst}");
    let report = shared.clock_device.alignment().unwrap();
    assert_eq!(report.delays, [Some(0), Some(0), None, None]);
    assert_eq!(report.waits_for, None);
}

#[test]
fn a_path_on_its_own_device_keeps_its_own_latency() {
    const LATENCY: usize = 96;
    let (mut mixer, shared) = aligned_mixer(LATENCY as u32, 2);
    // Clock device: path 0 only.  Follower 0: path 1 only.  Follower 1:
    // both.
    let only = |slot: usize| {
        let mut settings = OutputDeviceSettings::default();
        settings.path_map[1 - slot] = PathMapSettings::unrouted();
        settings
    };
    shared.clock_device.set(&only(0));
    let plan = FollowerPlan {
        count: 2,
        ..FollowerPlan::default()
    };
    shared.set_followers(&[only(1), OutputDeviceSettings::default()], &plan, &[]);
    let mut clock = 0;
    let out = run(&mut mixer, &shared, &mut clock, 6);
    for n in 0..out[0].len() {
        assert_eq!(out[0][n][0], delayed(n, LATENCY), "clock {n}");
        assert_eq!(out[1][n][0], signal(n), "follower 0 {n}");
        assert_eq!(out[2][n][0], 2.0 * delayed(n, LATENCY), "follower 1 {n}");
    }
    assert_eq!(
        shared.clock_device.alignment().unwrap().total,
        LATENCY as u32
    );
    let alone = shared.followers[0].alignment().unwrap();
    assert_eq!((alone.total, alone.added), (0, 0));
    assert_eq!(alone.delays, [None, Some(0), None, None]);
}

#[test]
fn a_generic_follower_is_aligned_on_the_clock_callback() {
    use super::super::shared::FollowerLink;
    use super::super::types::CushionMode;
    const LATENCY: usize = 80;
    let (mut mixer, shared) = aligned_mixer(LATENCY as u32, 0);
    // The ring path: input frames through the clock bridge's ring.
    shared
        .direct
        .store(false, std::sync::atomic::Ordering::Relaxed);
    shared.set_source_rate(RATE);
    shared.set_cushion(CushionMode::Manual, 100.0);
    for n in 0..8_000 {
        shared.push_frame(signal(n), -signal(n));
    }
    // A generic follower that plays path 1 only, so it keeps its own (zero)
    // latency, while the clock device waits for path 0.
    let mut only_path_1 = OutputDeviceSettings::default();
    only_path_1.path_map[0] = PathMapSettings::unrouted();
    let mut plan = FollowerPlan {
        count: 1,
        ..FollowerPlan::default()
    };
    plan.slots[0] = FollowerLink {
        device_id: "test:follower".to_owned(),
        ..FollowerLink::default()
    };
    shared.set_followers(&[only_path_1], &plan, &[(CushionMode::Safe, 10.0)]);
    let bus = shared.follower_bus(0);
    bus.set_device_channels(2);
    bus.reset_playback();
    shared
        .follower_ring_active(0)
        .store(true, std::sync::atomic::Ordering::Relaxed);

    let mut clock = Vec::new();
    for _ in 0..6 {
        mixer.render(&shared, BLOCK, |_, mapped| {
            clock.push([mapped.sample(0), mapped.sample(1)]);
        });
    }
    let mut follower = Vec::new();
    let mut frame = [0.0; crate::audio::output_map::FRAME_LANES];
    while bus.pop_frame(&mut frame) {
        follower.push([frame[0], frame[1]]);
    }
    assert_eq!(follower.len(), clock.len());
    assert!(follower.iter().any(|frame| frame[0].abs() > 0.1));
    // The clock device sums both paths, aligned on path 0's latency; the
    // follower plays path 1 as it comes.
    for n in LATENCY..clock.len() {
        assert_eq!(clock[n][0], 2.0 * follower[n - LATENCY][0], "frame {n}");
    }
    assert_eq!(
        shared.clock_device.alignment().unwrap().total,
        LATENCY as u32
    );
    let alone = shared.followers[0].alignment().unwrap();
    assert_eq!((alone.total, alone.added), (0, 0));
}

#[test]
fn polarity_cancels_and_a_trim_moves_one_path() {
    const LATENCY: usize = 32;
    let (mut mixer, shared) = aligned_mixer(LATENCY as u32, 1);
    // Clock device: path 1 inverted, so the aligned paths cancel.
    let mut inverted = OutputDeviceSettings::default();
    inverted.path_map[1].invert = true;
    shared.clock_device.set(&inverted);
    // Follower: path 1 trimmed by +1 ms (48 frames) on top of the alignment.
    let mut trimmed = OutputDeviceSettings::default();
    trimmed.path_map[1].trim_ms = 1.0;
    let plan = FollowerPlan {
        count: 1,
        ..FollowerPlan::default()
    };
    shared.set_followers(&[trimmed], &plan, &[]);
    let mut clock = 0;
    let out = run(&mut mixer, &shared, &mut clock, 6);
    for n in 0..out[0].len() {
        assert_eq!(out[0][n], [0.0, 0.0], "cancels at {n}");
        let want = delayed(n, LATENCY) + delayed(n, LATENCY + 48);
        assert!((out[1][n][0] - want).abs() < 1e-6, "trimmed at {n}");
    }
    assert_eq!(
        shared.followers[0].alignment().unwrap().delays[1],
        Some(LATENCY as u32 + 48)
    );

    // A negative trim beyond the latency: path 1 would have to play before
    // its input, so path 0 waits instead (the smallest delay is 0).
    trimmed.path_map[1].trim_ms = -1.0;
    shared.set_followers(&[trimmed], &plan, &[]);
    let _settle = run(&mut mixer, &shared, &mut clock, 8);
    let report = shared.followers[0].alignment().unwrap();
    assert_eq!(report.delays[0], Some(48 - LATENCY as u32));
    assert_eq!(report.delays[1], Some(0));
}

#[test]
fn a_latency_change_crossfades_the_other_path_without_a_click() {
    let (mut mixer, shared) = aligned_mixer(64, 0);
    // Path 0 on Out 1 only, path 1 on Out 2 only: Out 2 shows path 1's tap
    // alone, which follows path 0's latency.
    let mut settings = OutputDeviceSettings {
        unused: UnusedChannels::Silent,
        ..OutputDeviceSettings::default()
    };
    settings.path_map[0].map = ChannelMap {
        left: 0b01,
        right: 0,
    };
    settings.path_map[1].map = ChannelMap {
        left: 0b10,
        right: 0,
    };
    shared.clock_device.set(&settings);
    let mut clock = 0;
    let before = run(&mut mixer, &shared, &mut clock, 4);
    let start = clock;
    shared.paths[0].edit_chain(|chain| {
        chain
            .slot_mut_for_test(0)
            .test_delay
            .as_mut()
            .unwrap()
            .latency = 640;
        chain.refresh_latency();
    });
    let out = run(&mut mixer, &shared, &mut clock, 12);
    // The sine moves by at most 0.01 per frame; the fade adds the gap
    // between the two read positions spread over 15 ms.
    let step = largest_step(&out[0], 1).max(largest_step(&before[0], 1));
    assert!(step < 0.012, "largest step {step}");
    // Settled on the new delay.
    let last = out[0].len() - 1;
    assert_eq!(out[0][last][1], delayed(start + last, 640));
    assert_eq!(
        shared.clock_device.alignment().unwrap().delays[1],
        Some(640)
    );
}

#[test]
fn a_missed_chain_lock_keeps_the_sum_in_time_and_level() {
    const LATENCY: usize = 64;
    let (mut mixer, shared) = aligned_mixer(LATENCY as u32, 0);
    let mut clock = 0;
    let mut out = run(&mut mixer, &shared, &mut clock, 4).remove(0);
    {
        // The UI holds the chain lock for a whole block (an edit).
        let _held = shared.paths[0].plugins.lock().unwrap();
        out.extend(run(&mut mixer, &shared, &mut clock, 1).remove(0));
    }
    out.extend(run(&mut mixer, &shared, &mut clock, 6).remove(0));
    for (n, frame) in out.iter().enumerate() {
        let want = 2.0 * delayed(n, LATENCY);
        // The step into the fallback is smoothed from where the chain's
        // output was heading: a second-order error, far below a click.
        assert!(
            (frame[0] - want).abs() < 1e-3,
            "frame {n}: {frame:?} vs {want}"
        );
        assert!((frame[1] + want).abs() < 1e-3, "frame {n}");
    }
}

#[test]
fn one_path_without_latency_has_no_delay() {
    let (mut mixer, shared) = aligned_mixer(0, 0);
    let mut only = OutputDeviceSettings::default();
    only.path_map[1] = PathMapSettings::unrouted();
    shared.clock_device.set(&only);
    let mut clock = 0;
    let out = run(&mut mixer, &shared, &mut clock, 2);
    for (n, frame) in out[0].iter().enumerate() {
        assert_eq!(*frame, [signal(n), -signal(n)], "frame {n}");
    }
}

#[test]
fn only_devices_that_play_publish_an_alignment() {
    let (mut mixer, shared) = aligned_mixer(64, 1);
    // A generic follower whose stream does not drain its ring yet.
    shared
        .direct
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let mut clock = 0;
    let _ = run(&mut mixer, &shared, &mut clock, 1);
    assert!(shared.clock_device.alignment().is_some());
    assert!(shared.followers[0].alignment().is_none());
    // Nor one the plan dropped.
    assert!(shared.followers[1].alignment().is_none());
    // A stopped bridge clears what it published.
    shared.clock_device.clear_alignment();
    assert!(shared.clock_device.alignment().is_none());
}

#[test]
fn a_chain_past_the_alignment_limit_is_flagged() {
    use super::super::plugins::MAX_ALIGN;
    let settings = OutputDeviceSettings::default();
    let report = settings.alignment(
        [true, true, false, false],
        2,
        [MAX_ALIGN + 10, 0, 0, 0],
        RATE,
    );
    assert!(report.clamped);
    assert_eq!(report.delays[1], Some(MAX_ALIGN));
    let fine = settings.alignment([true, true, false, false], 2, [64, 0, 0, 0], RATE);
    assert!(!fine.clamped);
}

#[test]
fn a_missed_chain_lock_fades_into_the_fallback_without_a_click() {
    let _main = super::super::shared::tests::claim_main_thread();
    let (mut mixer, shared) = aligned_mixer(0, 0);
    // Path 0 runs a plugin whose output is nothing like its input (the
    // fake writes a constant); path 1 plays nowhere.
    shared.paths[0].edit_chain(|chain| {
        drop(chain.restore(Vec::new()));
        chain.insert_clap_for_test(crate::audio::clap::tests::fake_plugin_for_test(false));
    });
    let mut only = OutputDeviceSettings::default();
    only.path_map[1] = PathMapSettings::unrouted();
    shared.clock_device.set(&only);
    let mut clock = 0;
    let mut out = run(&mut mixer, &shared, &mut clock, 2).remove(0);
    assert_eq!(out.last().unwrap()[0], 1.0, "the plugin runs");
    {
        let _held = shared.paths[0].plugins.lock().unwrap();
        out.extend(run(&mut mixer, &shared, &mut clock, 1).remove(0));
    }
    out.extend(run(&mut mixer, &shared, &mut clock, 2).remove(0));
    // From 1.0 to the dry sine and back: smoothed both ways.
    let step = largest_step(&out, 0);
    assert!(step < 0.05, "largest step {step}");
    assert_eq!(out.last().unwrap()[0], 1.0, "the plugin is back");
    drop(shared.paths[0].edit_chain(|chain| chain.restore(Vec::new())));
}

#[test]
fn a_removed_path_plays_out_its_alignment_delay_before_it_stops() {
    use super::super::path::PathState;
    const LATENCY: usize = 4800;
    let (mut mixer, shared) = aligned_mixer(LATENCY as u32, 0);
    let mut clock = 0;
    let mut out = run(&mut mixer, &shared, &mut clock, 24).remove(0);
    // Path 1 waits 4800 frames for path 0 on the clock device: its fade
    // reaches the device that much later, and must not be cut off.
    assert!(shared.remove_path(1));
    let mut blocks = 0;
    while shared.paths[1].state() != PathState::Inactive {
        out.extend(run(&mut mixer, &shared, &mut clock, 1).remove(0));
        blocks += 1;
        assert!(blocks < 64, "the removal never finished");
    }
    assert!(blocks * BLOCK >= LATENCY, "stopped after {blocks} blocks");
    out.extend(run(&mut mixer, &shared, &mut clock, 4).remove(0));
    let step = largest_step(&out[LATENCY..], 0);
    assert!(step < 0.03, "largest step {step}");
    // Only path 0 is left, at its own latency.
    let last = out.len() - 1;
    assert!((out[last][0] - delayed(last, LATENCY)).abs() < 1e-6);
}
