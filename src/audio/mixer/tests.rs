use std::sync::atomic::Ordering;

use super::super::eq::{EqSettings, EqSlot};
use super::super::output::write_frame;
use super::super::output_map::{
    old_write_frame, ChannelMap, OutputDeviceSettings, PathMapSettings, UnusedChannels,
};
use super::super::shared::Shared;
use super::super::types::CushionMode;
use super::{route_frame, OutputMixer, Ramp, CONTROL_RAMP_SECONDS};

#[test]
fn default_routes_keep_channels_apart() {
    let routes = [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0]];
    assert_eq!(route_frame(0.5, -0.25, &routes), [0.5, -0.25, 0.0, 0.0]);
}

#[test]
fn a_tap_can_receive_both_inputs() {
    let routes = [[0.0, 1.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0]];
    assert_eq!(route_frame(0.5, 0.25, &routes), [0.0, 0.75, 0.0, 0.0]);
}

#[test]
fn partial_routes_scale_each_input() {
    let routes = [[0.5, 0.0, 0.0, 0.0], [0.0, 0.5, 0.0, 0.0]];
    assert_eq!(route_frame(0.5, -0.25, &routes), [0.25, -0.125, 0.0, 0.0]);
}

#[test]
fn muted_matrix_is_silent() {
    let routes = [[0.0; 4]; 2];
    assert_eq!(route_frame(0.5, 0.5, &routes), [0.0; 4]);
}

#[test]
fn ramp_lands_on_its_target_and_holds() {
    let mut ramp = Ramp::new(0.0);
    ramp.set_target(1.0, 4.0);
    let values: Vec<f32> = (0..6).map(|_| ramp.next()).collect();
    assert_eq!(values, vec![0.25, 0.5, 0.75, 1.0, 1.0, 1.0]);
}

#[test]
fn ramp_keeps_its_slope_when_the_target_is_repeated() {
    let mut ramp = Ramp::new(0.0);
    ramp.set_target(1.0, 4.0);
    assert_eq!(ramp.next(), 0.25);
    // A block boundary that repeats the same target must not restart the
    // move from wherever the ramp currently sits.
    ramp.set_target(1.0, 4.0);
    assert_eq!(ramp.next(), 0.5);
}

#[test]
fn ramp_retargets_from_where_it_is() {
    let mut ramp = Ramp::new(0.0);
    ramp.set_target(1.0, 4.0);
    assert_eq!(ramp.next(), 0.25);
    ramp.set_target(0.0, 2.0);
    assert_eq!(ramp.next(), 0.125);
    assert_eq!(ramp.next(), 0.0);
}

#[test]
fn snap_adopts_a_value_without_moving() {
    let mut ramp = Ramp::new(1.0);
    ramp.snap(0.25);
    assert_eq!(ramp.next(), 0.25);
    assert_eq!(ramp.next(), 0.25);
}

/// A 1:1 bridge with drift off, a deep cushion and the EQs bypassed, so a
/// rendered frame is the mix itself and not the resampler or a filter.
fn steady_mixer(signal: (f32, f32)) -> (OutputMixer, Shared) {
    let shared = Shared::new(48_000);
    shared.set_source_rate(48_000);
    shared.set_cushion(CushionMode::Manual, 100.0);
    shared.set_mix(false, true, false, [true, true], [true, true], false);
    let bypassed = EqSettings {
        enabled: false,
        ..EqSettings::default()
    };
    shared.set_eq(EqSlot::Pre, bypassed);
    shared.set_eq(EqSlot::Post, bypassed);
    for _ in 0..5_000 {
        shared.push_frame(signal.0, signal.1);
    }
    (OutputMixer::new(48_000, 2), shared)
}

fn render(mixer: &mut OutputMixer, shared: &Shared, frames: usize) -> Vec<[f32; 4]> {
    let mut out = vec![[0.0f32; 4]; frames];
    mixer.render_taps(shared, frames, |index, frame| out[index] = frame);
    out
}

fn store_gain(shared: &Shared, gain: f32) {
    shared.output_gain.store(gain.to_bits(), Ordering::Relaxed);
}

/// A varied stereo signal, so the comparison sees the whole chain move.
fn test_signal(frames: usize) -> (Vec<f32>, Vec<f32>) {
    let left = (0..frames)
        .map(|n| (n as f32 * 0.031).sin() * 0.6)
        .collect();
    let right = (0..frames)
        .map(|n| (n as f32 * 0.017).cos() * 0.4)
        .collect();
    (left, right)
}

/// Settings that exercise routing, mono, both EQs and a level ramp.
fn busy_settings(shared: &Shared) {
    shared.set_source_rate(48_000);
    shared.set_cushion(CushionMode::Manual, 2.0);
    shared.set_mix(false, true, false, [true, true], [true, true], false);
    shared.set_routes([[true, false, true, false], [false, true, false, true]]);
    let mut pre = EqSettings::default();
    pre.bands[1].gain_db = 6.0;
    let mut post = EqSettings::default();
    post.bands[3].gain_db = -4.0;
    shared.set_eq(EqSlot::Pre, pre);
    shared.set_eq(EqSlot::Post, post);
    store_gain(shared, 0.8);
}

#[test]
fn the_direct_path_matches_the_ring_path_at_one_clock() {
    let frames = 256;
    let blocks = 6;
    let (left, right) = test_signal(frames * blocks);

    // Ring path: each block's input is pushed just before it renders, as
    // a capture callback in the same cycle would.  The 2 ms cushion is
    // below one block, so priming ends at once, nothing is purged and
    // every frame comes straight through.
    let ring_shared = Shared::new(48_000);
    busy_settings(&ring_shared);
    let mut ring_mixer = OutputMixer::new(48_000, 2);
    ring_mixer.prepare(frames);

    let direct_shared = Shared::new(48_000);
    busy_settings(&direct_shared);
    let mut direct_mixer = OutputMixer::new(48_000, 2);
    direct_mixer.prepare(frames);

    for block in 0..blocks {
        if block == 3 {
            // A level change mid-run must ramp identically on both.
            store_gain(&ring_shared, 0.3);
            store_gain(&direct_shared, 0.3);
        }
        let span = block * frames..(block + 1) * frames;
        for (l, r) in left[span.clone()].iter().zip(&right[span.clone()]) {
            ring_shared.push_frame(*l, *r);
        }
        let ring = render(&mut ring_mixer, &ring_shared, frames);
        let mut direct = vec![[0.0f32; 4]; frames];
        direct_mixer.render_direct_taps(
            &direct_shared,
            &left[span.clone()],
            &right[span],
            |index, frame| direct[index] = frame,
        );
        assert_eq!(ring, direct, "block {block} differs");
    }
    assert_eq!(
        ring_shared.frames.load(Ordering::Relaxed),
        direct_shared.frames.load(Ordering::Relaxed)
    );
    assert_eq!(ring_shared.underruns.load(Ordering::Relaxed), 0);
    assert_eq!(direct_shared.underruns.load(Ordering::Relaxed), 0);
    assert_eq!(
        ring_shared.out_l.load(Ordering::Relaxed),
        direct_shared.out_l.load(Ordering::Relaxed)
    );
}

#[test]
fn default_device_settings_write_what_the_output_always_wrote() {
    let frames = 256;
    let blocks = 6;
    let (left, right) = test_signal(frames * blocks);
    for channels in [1, 2, 4, 8] {
        // The same chain twice: one hands over the taps, one the mapped
        // device frame.  A level ramp mid-run must not change that.
        let taps_shared = Shared::new(48_000);
        busy_settings(&taps_shared);
        let mut taps_mixer = OutputMixer::new(48_000, channels);
        taps_mixer.prepare(frames);
        let device_shared = Shared::new(48_000);
        busy_settings(&device_shared);
        let mut device_mixer = OutputMixer::new(48_000, channels);
        device_mixer.prepare(frames);

        for block in 0..blocks {
            if block == 3 {
                store_gain(&taps_shared, 0.3);
                store_gain(&device_shared, 0.3);
            }
            let span = block * frames..(block + 1) * frames;
            for (l, r) in left[span.clone()].iter().zip(&right[span]) {
                taps_shared.push_frame(*l, *r);
                device_shared.push_frame(*l, *r);
            }
            let taps = render(&mut taps_mixer, &taps_shared, frames);
            let mut device = vec![0.0f32; frames * channels];
            device_mixer.render(&device_shared, frames, |index, mapped| {
                for channel in 0..channels {
                    device[index * channels + channel] = mapped.sample(channel);
                }
            });
            let mut expected = vec![0.0f32; frames * channels];
            for (frame, tap) in expected.chunks_mut(channels).zip(&taps) {
                old_write_frame(frame, tap[0], tap[1]);
            }
            let bits = |values: &[f32]| values.iter().map(|v| v.to_bits()).collect::<Vec<_>>();
            assert_eq!(
                bits(&device),
                bits(&expected),
                "{channels} ch, block {block}"
            );
        }
        // Per-channel meters: channel 1 and 2 carry the master L/R peaks.
        let meters = device_shared.clock_device.meters();
        if channels >= 2 {
            assert_eq!(
                meters[0],
                f32::from_bits(device_shared.out_l.load(Ordering::Relaxed))
            );
            assert_eq!(
                meters[1],
                f32::from_bits(device_shared.out_r.load(Ordering::Relaxed))
            );
        }
        assert!(meters[..channels].iter().all(|meter| *meter > 0.0));
        assert!(meters[channels..].iter().all(|meter| *meter == 0.0));
    }
}

#[test]
fn a_ten_channel_device_fills_every_channel_through_write_frame() {
    let (_, shared) = steady_mixer((0.5, -0.25));
    let mut mixer = OutputMixer::new(48_000, 10);
    mixer.publish_device_channels(&shared);
    assert_eq!(shared.clock_device.channels(), 10);
    let frames = 64;
    let mut data = vec![f32::NAN; frames * 10];
    mixer.render(&shared, frames, |index, mapped| {
        write_frame(&mut data[index * 10..(index + 1) * 10], mapped);
    });
    for frame in data.chunks(10) {
        let mut expected = [0.0f32; 10];
        old_write_frame(&mut expected, 0.5, -0.25);
        assert_eq!(frame, expected);
    }
    // Channels 9 and 10 are past the map and get the unused value.
    shared.clock_device.set(&OutputDeviceSettings {
        unused: UnusedChannels::Silent,
        ..OutputDeviceSettings::default()
    });
    // 8 blocks of 64 outlast the 6 ms ramp.
    for _ in 0..8 {
        mixer.render(&shared, frames, |index, mapped| {
            write_frame(&mut data[index * 10..(index + 1) * 10], mapped);
        });
    }
    let last = &data[(frames - 1) * 10..];
    assert_eq!(last[..2], [0.5, -0.25]);
    assert!(last[2..].iter().all(|value| *value == 0.0));
}

#[test]
fn the_device_map_follows_the_master_and_crossfades() {
    let (mut mixer, shared) = steady_mixer((0.5, -0.25));
    let render_device = |mixer: &mut OutputMixer, shared: &Shared, frames: usize| {
        let mut out = vec![[0.0f32; 2]; frames];
        mixer.render(shared, frames, |index, mapped| {
            out[index] = [mapped.sample(0), mapped.sample(1)];
        });
        out
    };
    store_gain(&shared, 0.5);
    let adopted = render_device(&mut mixer, &shared, 16);
    assert_eq!(adopted[0], [0.25, -0.125]);

    // Swap the sides at -6 dB: a crossfade, then the swapped, scaled pair.
    let gain = 10.0f32.powf(-6.0 / 20.0);
    shared.clock_device.set(&OutputDeviceSettings {
        level_db: -6.0,
        path_map: crate::audio::PathMapSettings::first(ChannelMap {
            left: 0b10,
            right: 0b01,
        }),
        ..OutputDeviceSettings::default()
    });
    let out = render_device(&mut mixer, &shared, 600);
    let step = out
        .windows(2)
        .map(|pair| (pair[1][0] - pair[0][0]).abs())
        .fold(0.0, f32::max);
    assert!(step < 0.375 / 100.0, "stepped by {step}");
    assert_eq!(*out.last().unwrap(), [-0.125 * gain, 0.25 * gain]);
    // The master meters stay before the device stage.
    assert_eq!(f32::from_bits(shared.out_l.load(Ordering::Relaxed)), 0.25);
}

#[test]
fn the_direct_path_follows_a_new_graph_rate() {
    let shared = Shared::new(48_000);
    busy_settings(&shared);
    let mut mixer = OutputMixer::new(48_000, 2);
    mixer.prepare(128);
    let (left, right) = test_signal(128);
    mixer.render_direct_taps(&shared, &left, &right, |_, _| {});
    mixer.set_direct_rate(44_100);
    let mut at_new_rate = vec![[0.0f32; 4]; 128];
    mixer.render_direct_taps(&shared, &left, &right, |index, frame| {
        at_new_rate[index] = frame
    });

    // A mixer built at 44.1 kHz from the start, after the same history.
    let reference_shared = Shared::new(44_100);
    busy_settings(&reference_shared);
    let mut reference = OutputMixer::new(44_100, 2);
    reference.prepare(128);
    reference.render_direct_taps(&reference_shared, &left, &right, |_, _| {});
    let mut expected = vec![[0.0f32; 4]; 128];
    reference.render_direct_taps(&reference_shared, &left, &right, |index, frame| {
        expected[index] = frame
    });
    // The filter state differs (it ran at another rate), so compare the
    // tail, after the EQ memory has mostly converged, loosely.
    for (got, want) in at_new_rate[96..].iter().zip(&expected[96..]) {
        assert!((got[0] - want[0]).abs() < 0.05, "{got:?} vs {want:?}");
    }
}

#[test]
fn the_first_block_adopts_the_live_level() {
    let (mut mixer, shared) = steady_mixer((0.5, -0.25));
    store_gain(&shared, 0.25);
    let out = render(&mut mixer, &shared, 64);
    assert!(out.iter().all(|frame| frame == &[0.125, -0.0625, 0.0, 0.0]));
}

#[test]
fn a_level_change_ramps_across_blocks_and_then_holds() {
    let (mut mixer, shared) = steady_mixer((0.5, -0.25));
    let adopted = render(&mut mixer, &shared, 32);
    assert_eq!(adopted[0], [0.5, -0.25, 0.0, 0.0]);

    store_gain(&shared, 0.5);
    let ramp_frames = (CONTROL_RAMP_SECONDS * 48_000.0).round() as usize;
    let first = render(&mut mixer, &shared, 100);
    let second = render(&mut mixer, &shared, ramp_frames);
    let joined = [first.clone(), second.clone()].concat();

    assert!(first[0][0] < 0.5 && first[0][0] > 0.49);
    let step = (first[0][0] - first[1][0]).abs();
    let full_drop = 0.5 - 0.25;
    assert!(step > 0.0 && step < full_drop / 50.0);
    // The block boundary continues the same slope instead of jumping
    // back toward the old level or landing on the new one early.
    let boundary = (first[99][0] - second[0][0]).abs();
    assert!(boundary < step * 1.5);

    let settled_at = joined
        .iter()
        .position(|frame| frame[0] == 0.25)
        .expect("ramp reaches the new level");
    assert!((settled_at as i32 - (ramp_frames as i32 - 1)).abs() <= 2);
    assert!(joined[settled_at..].iter().all(|frame| frame[0] == 0.25));
    assert!(joined[settled_at..].iter().all(|frame| frame[1] == -0.125));
}

#[test]
fn mute_fades_out_and_a_new_level_fades_back_in() {
    let (mut mixer, shared) = steady_mixer((0.5, 0.0));
    let _adopted = render(&mut mixer, &shared, 16);
    store_gain(&shared, 0.5);
    shared.set_mix(true, true, false, [true, true], [true, true], false);
    let muted = render(&mut mixer, &shared, 320);
    assert!(muted[0][0] > 0.4);
    assert!(muted.last().unwrap()[0] == 0.0);
    assert!(muted.iter().all(|frame| frame[1] == 0.0));

    shared.set_mix(false, true, false, [true, true], [true, true], false);
    let restored = render(&mut mixer, &shared, 320);
    assert!(restored[0][0] > 0.0 && restored[0][0] < 0.2);
    assert_eq!(restored.last().unwrap()[0], 0.25);
}

#[test]
fn monitor_off_fades_to_silence() {
    let (mut mixer, shared) = steady_mixer((0.5, 0.0));
    let _adopted = render(&mut mixer, &shared, 16);
    shared.set_mix(false, false, false, [true, true], [true, true], false);
    let out = render(&mut mixer, &shared, 320);
    assert!(out[0][0] > 0.4);
    assert_eq!(out.last().unwrap()[0], 0.0);
}

#[test]
fn a_route_change_crossfades_and_leaves_other_taps_put() {
    let (mut mixer, shared) = steady_mixer((0.5, 0.0));
    let adopted = render(&mut mixer, &shared, 16);
    assert_eq!(adopted[0], [0.5, 0.0, 0.0, 0.0]);

    // Keep left on tap 1, and move it off tap 0.  Tap 1 fades in; tap 0
    // fades out; nothing else starts carrying the input.
    shared.set_routes([[false, true, false, false], [false, false, false, false]]);
    let out = render(&mut mixer, &shared, 320);
    assert!(out[0][0] > 0.4 && out[0][1] < 0.1);
    assert_eq!(*out.last().unwrap(), [0.0, 0.5, 0.0, 0.0]);
    assert!(out.iter().all(|frame| frame[2] == 0.0 && frame[3] == 0.0));
    let mid = out[out.len() / 2];
    assert!(mid[0] > 0.1 && mid[0] < 0.4);
    assert!(mid[1] > 0.1 && mid[1] < 0.4);
}

#[test]
fn disabling_a_channel_fades_that_tap_only() {
    let (mut mixer, shared) = steady_mixer((0.5, -0.25));
    let _adopted = render(&mut mixer, &shared, 16);
    shared.set_mix(false, true, false, [true, true], [false, true], false);
    let out = render(&mut mixer, &shared, 320);
    assert!(out.iter().all(|frame| frame[1] == -0.25));
    assert!(out[0][0] > 0.4);
    assert_eq!(out.last().unwrap()[0], 0.0);
}

/// The largest sample-to-sample move on tap 0 across a run of blocks.
fn largest_step(out: &[[f32; 4]]) -> f32 {
    out.windows(2)
        .map(|pair| (pair[1][0] - pair[0][0]).abs())
        .fold(0.0, f32::max)
}

/// Renders `blocks` blocks of 256 frames into one run.
fn render_blocks(mixer: &mut OutputMixer, shared: &Shared, blocks: usize) -> Vec<[f32; 4]> {
    (0..blocks)
        .flat_map(|_| render(mixer, shared, 256))
        .collect()
}

fn underrun_fades_out(drift: bool) {
    let (mut mixer, shared) = steady_mixer((0.8, 0.8));
    shared.set_mix(false, true, false, [true, true], [true, true], drift);
    // 5 000 frames drain in a little under 20 blocks; the 20th runs dry
    // part way through.
    let out = render_blocks(&mut mixer, &shared, 24);
    assert!(shared.underruns.load(Ordering::Relaxed) >= 1);
    assert!((out[100][0] - 0.8).abs() < 1.0e-3, "{}", out[100][0]);
    assert_eq!(out.last().unwrap()[0], 0.0, "the dropout ends in silence");
    let step = largest_step(&out);
    assert!(step < 0.05, "underrun stepped by {step}");
}

#[test]
fn an_underrun_fades_the_held_frame_to_silence() {
    underrun_fades_out(false);
}

#[test]
fn an_underrun_through_the_resampler_fades_too() {
    underrun_fades_out(true);
}

#[test]
fn playback_fades_back_in_after_an_underrun() {
    let (mut mixer, shared) = steady_mixer((0.8, 0.8));
    let mut out = render_blocks(&mut mixer, &shared, 24);
    assert!(shared.underruns.load(Ordering::Relaxed) >= 1);
    let resumed_from = out.len();
    for _ in 0..6_000 {
        shared.push_frame(0.8, 0.8);
    }
    out.extend(render_blocks(&mut mixer, &shared, 4));
    let first = out[resumed_from..]
        .iter()
        .position(|frame| frame[0] != 0.0)
        .map(|index| out[resumed_from + index][0])
        .expect("playback resumes");
    assert!(first > 0.0 && first < 0.05, "resumed at {first}");
    assert_eq!(out.last().unwrap()[0], 0.8);
    let step = largest_step(&out);
    assert!(step < 0.05, "recovery stepped by {step}");
}

#[test]
fn an_underrun_that_fades_out_by_the_block_end_resumes_next_block() {
    let (mut mixer, shared) = steady_mixer((0.8, 0.8));
    // 5 000 frames: four 1024 blocks leave 904, so the fifth runs dry at
    // sample 904 and its 120-sample fade ends exactly at the block end.
    let _played = (0..5)
        .map(|_| render(&mut mixer, &shared, 1024))
        .collect::<Vec<_>>();
    assert_eq!(shared.underruns.load(Ordering::Relaxed), 1);
    for _ in 0..6_000 {
        shared.push_frame(0.8, 0.8);
    }
    let resumed = render(&mut mixer, &shared, 1024);
    assert!(
        resumed[0][0] > 0.0 && resumed[0][0] < 0.05,
        "{}",
        resumed[0][0]
    );
    assert_eq!(resumed[200][0], 0.8);
}

#[test]
fn a_short_priming_fades_in_from_silence() {
    let shared = Shared::new(48_000);
    shared.set_source_rate(48_000);
    shared.set_cushion(CushionMode::Manual, 100.0);
    shared.set_mix(false, true, false, [true, true], [true, true], false);
    let bypassed = EqSettings {
        enabled: false,
        ..EqSettings::default()
    };
    shared.set_eq(EqSlot::Pre, bypassed);
    shared.set_eq(EqSlot::Post, bypassed);
    let mut mixer = OutputMixer::new(48_000, 2);
    // Block 1: nothing has arrived yet.
    let empty = render(&mut mixer, &shared, 64);
    assert!(empty.iter().all(|frame| frame[0] == 0.0));
    // Block 2: the cushion is full and playback fades in from about 0.
    for _ in 0..6_000 {
        shared.push_frame(0.8, 0.8);
    }
    let out = render(&mut mixer, &shared, 64);
    assert!(out[0][0] > 0.0 && out[0][0] < 0.05, "{}", out[0][0]);
    assert!(largest_step(&out) < 0.05);
}

/// Folds every output sample (as bits) into one FNV-1a hash.
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn u32(&mut self, value: u32) {
        for byte in value.to_le_bytes() {
            self.0 ^= u64::from(byte);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }

    fn f32(&mut self, value: f32) {
        self.u32(value.to_bits());
    }
}

/// A deterministic, libm-free test signal: a triangle plus LCG noise.
struct Source {
    state: u32,
    phase: u32,
}

impl Source {
    fn new() -> Self {
        Self {
            state: 0x1234_5678,
            phase: 0,
        }
    }

    fn next(&mut self) -> (f32, f32) {
        self.state = self
            .state
            .wrapping_mul(1_664_525)
            .wrapping_add(1_013_904_223);
        let noise = (self.state >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
        self.phase = (self.phase + 1) % 200;
        let triangle = (self.phase as f32 - 100.0).abs() / 100.0 - 0.5;
        (triangle * 0.9 + noise * 0.2, noise * 0.6 - triangle * 0.3)
    }
}

/// The ring path through the resampler with drift on, every built-in
/// stage, control changes mid-run, a device map crossfade and an
/// underrun.  Feeds every device sample and meter into `hash`.
fn golden_ring_render(hash: &mut Fnv) {
    use super::super::gate::{GateSettings, GateSlot};
    let shared = Shared::new(48_000);
    shared.set_source_rate(44_100);
    shared.set_cushion(CushionMode::Manual, 5.0);
    shared.set_mix(false, true, false, [true, true], [true, true], true);
    shared.set_routes([[true, false, true, false], [false, true, false, true]]);
    let mut pre = EqSettings::default();
    pre.bands[1].gain_db = 6.0;
    let mut post = EqSettings::default();
    post.bands[3].gain_db = -4.0;
    shared.set_eq(EqSlot::Pre, pre);
    shared.set_eq(EqSlot::Post, post);
    let mut gate = GateSettings {
        enabled: true,
        slot: GateSlot::Pre,
        threshold_db: -20.0,
        ..GateSettings::default()
    };
    shared.set_gate(gate);
    store_gain(&shared, 0.8);
    let channels = 4;
    let mut mixer = OutputMixer::new(48_000, channels);
    mixer.publish_device_channels(&shared);
    mixer.prepare(256);
    let mut source = Source::new();
    for _ in 0..400 {
        let (l, r) = source.next();
        shared.push_frame(l, r);
    }
    for block in 0..48 {
        match block {
            8 => shared.set_mix(false, true, true, [true, true], [true, true], true),
            12 => shared.set_mix(false, true, false, [true, true], [false, true], true),
            16 => {
                gate.slot = GateSlot::Post;
                shared.set_gate(gate);
            }
            20 => shared.clock_device.set(&OutputDeviceSettings {
                level_db: -3.0,
                path_map: crate::audio::PathMapSettings::first(ChannelMap {
                    left: 0b1010,
                    right: 0b0101,
                }),
                unused: UnusedChannels::Silent,
                ..OutputDeviceSettings::default()
            }),
            24 => store_gain(&shared, 0.3),
            28 => shared.set_routes([[false, true, false, false], [true, false, false, true]]),
            36 => shared.set_mix(true, true, false, [true, true], [true, true], true),
            40 => shared.set_mix(false, true, false, [true, true], [true, true], false),
            _ => {}
        }
        // Blocks 30 and 31 starve the ring; block 33 catches up.
        let pushed = match block {
            30 | 31 => 0,
            33 => 1_200,
            _ => 236 + (block % 3),
        };
        for _ in 0..pushed {
            let (l, r) = source.next();
            shared.push_frame(l, r);
        }
        mixer.render(&shared, 256, |_, mapped| {
            for channel in 0..channels {
                hash.f32(mapped.sample(channel));
            }
        });
        hash.u32(shared.out_l.load(Ordering::Relaxed));
        hash.u32(shared.out_r.load(Ordering::Relaxed));
        hash.u32(shared.drift_ppm.load(Ordering::Relaxed));
        for meter in shared.clock_device.meters() {
            hash.f32(meter);
        }
    }
    let underruns = shared.underruns.load(Ordering::Relaxed);
    assert!(underruns >= 1, "the scenario must run the ring dry");
    hash.u32(underruns as u32);
    hash.u32(shared.frames.load(Ordering::Relaxed) as u32);
}

/// The PipeWire direct path with two followers, each with its own map
/// and level, and control changes mid-run.
fn golden_direct_render(hash: &mut Fnv) {
    use super::super::shared::FollowerPlan;
    let shared = Shared::new(48_000);
    busy_settings(&shared);
    let followers = [
        OutputDeviceSettings {
            level_db: -6.0,
            path_map: crate::audio::PathMapSettings::first(ChannelMap {
                left: 0b10,
                right: 0b01,
            }),
            ..OutputDeviceSettings::default()
        },
        OutputDeviceSettings {
            path_map: crate::audio::PathMapSettings::first(ChannelMap {
                left: 0b11,
                right: 0,
            }),
            unused: UnusedChannels::Silent,
            ..OutputDeviceSettings::default()
        },
    ];
    let plan = FollowerPlan {
        count: 2,
        ..FollowerPlan::default()
    };
    shared.set_followers(&followers, &plan, &[]);
    // The duplex graph maps the followers in this mixer.
    shared.direct.store(true, Ordering::Relaxed);
    let mut mixer = OutputMixer::new(48_000, 2);
    mixer.publish_device_channels(&shared);
    mixer.publish_follower_channels(&shared);
    mixer.prepare(128);
    let mut source = Source::new();
    for block in 0..32 {
        match block {
            6 => store_gain(&shared, 0.5),
            10 => shared.set_followers(
                &[
                    OutputDeviceSettings {
                        level_db: -12.0,
                        ..followers[0]
                    },
                    followers[1],
                ],
                &plan,
                &[],
            ),
            14 => shared.set_mix(false, true, true, [true, true], [true, true], false),
            18 => {
                let mut pre = EqSettings::default();
                pre.bands[0].gain_db = -6.0;
                shared.set_eq(EqSlot::Pre, pre);
            }
            22 => shared.set_routes([[true, true, false, false], [false, true, true, false]]),
            _ => {}
        }
        let (left, right): (Vec<f32>, Vec<f32>) = (0..128).map(|_| source.next()).unzip();
        mixer.render_direct_devices(&shared, &left, &right, |device, index, mapped| {
            hash.u32(device as u32);
            hash.u32(index as u32);
            hash.f32(mapped.sample(0));
            hash.f32(mapped.sample(1));
        });
        hash.u32(shared.out_l.load(Ordering::Relaxed));
        hash.u32(shared.out_r.load(Ordering::Relaxed));
        for follower in &shared.followers[..2] {
            for meter in follower.meters() {
                hash.f32(meter);
            }
        }
    }
}

/// Output recorded before the chain moved into per-path processors: one
/// path must stay bit-identical.  The EQ, gate and resampler designs
/// use the platform's libm, so the recorded hashes are only checked on
/// the platform they were recorded on.
#[test]
fn one_path_is_bit_identical_to_the_recorded_output() {
    let mut ring = Fnv::new();
    golden_ring_render(&mut ring);
    let mut direct = Fnv::new();
    golden_direct_render(&mut direct);
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        assert_eq!(ring.0, 0x9837_e736_8542_4c03, "ring path output changed");
        assert_eq!(
            direct.0, 0x4fa9_c550_12df_70db,
            "direct path output changed"
        );
    }
}

#[test]
fn a_plugin_insert_on_path_0_runs_inside_the_mixer() {
    let _main = super::super::shared::tests::claim_main_thread();
    let fake = crate::audio::clap::tests::fake_plugin_for_test;
    let (mut mixer, shared) = steady_mixer((0.5, -0.25));
    shared.paths[0].edit_chain(|chain| chain.insert_clap_for_test(fake(false)));
    // The same insert run on its own over the same dry block.
    let reference = Shared::new(48_000);
    reference.paths[0].edit_chain(|chain| chain.insert_clap_for_test(fake(false)));
    let mut left = vec![0.5f32; 64];
    let mut right = vec![-0.25f32; 64];
    reference.paths[0].process_plugins(&mut left, &mut right, 64);

    let out = render(&mut mixer, &shared, 64);
    assert_ne!(out[0][0], 0.5, "the insert ran");
    for (index, frame) in out.iter().enumerate() {
        assert_eq!([frame[0], frame[1]], [left[index], right[index]]);
    }
    let peak = |side: &[f32]| side.iter().fold(0.0f32, |peak, v| peak.max(v.abs()));
    assert_eq!(shared.paths[0].meters(), (peak(&left), peak(&right)));
}

#[test]
fn live_slots_add_up_and_a_reactivated_slot_starts_fresh() {
    use super::super::path::PathState;
    let (mut mixer, shared) = steady_mixer((0.5, -0.25));
    let bypassed = EqSettings {
        enabled: false,
        ..EqSettings::default()
    };
    shared.paths[1].set_eq(EqSlot::Pre, bypassed);
    shared.paths[1].set_eq(EqSlot::Post, bypassed);
    let device = |mixer: &mut OutputMixer, shared: &Shared| {
        let mut out = vec![[0.0f32; 2]; 32];
        mixer.render(shared, 32, |index, mapped| {
            out[index] = [mapped.sample(0), mapped.sample(1)];
        });
        out
    };
    assert_eq!(render(&mut mixer, &shared, 16)[0], [0.5, -0.25, 0.0, 0.0]);

    // A second slot plays the same input: it fades in, then the sum
    // doubles, on the taps and on the device.
    shared.paths[1].set_state(PathState::Active);
    let out = render(&mut mixer, &shared, 400);
    assert!(out[0][0] > 0.5 && out[0][0] < 0.51, "{:?}", out[0]);
    assert!(largest_step(&out) < 0.5 / 100.0);
    assert_eq!(out[399], [1.0, -0.5, 0.0, 0.0]);
    assert!(device(&mut mixer, &shared)
        .iter()
        .all(|f| *f == [1.0, -0.5]));
    // Removing slot 0 keeps it running through its fade-out, after
    // which the mixer makes it inactive; slot 1 stays where it is.
    shared.paths[0].set_state(PathState::FadingOut);
    let out = render(&mut mixer, &shared, 400);
    assert!(out[0][0] > 0.99, "{:?}", out[0]);
    assert_eq!(out[399], [0.5, -0.25, 0.0, 0.0]);
    assert_eq!(shared.paths[0].state(), PathState::Inactive);
    assert_eq!(render(&mut mixer, &shared, 16)[0], [0.5, -0.25, 0.0, 0.0]);
    assert!(device(&mut mixer, &shared)
        .iter()
        .all(|f| *f == [0.5, -0.25]));

    // Slot 0 comes back with mono changed while it was off: a fresh
    // chain adopts the setting at once instead of ramping to it (every
    // tap gets the same mono share while the path fades in).
    shared.paths[0].set_mono(true);
    let generation = shared.paths[0].generation();
    shared.paths[0].set_state(PathState::Active);
    assert_eq!(shared.paths[0].generation(), generation + 1);
    let out = render(&mut mixer, &shared, 400);
    let share = out[0][2];
    assert!(share > 0.0 && share < 0.01, "fades in: {:?}", out[0]);
    assert_eq!(out[0][3], share);
    assert!((out[0][0] - 0.5 - share).abs() < 1.0e-6);
    assert_eq!(out[399], [0.625, -0.125, 0.125, 0.125]);
}

#[test]
fn mono_crossfades_every_tap_to_the_average() {
    let (mut mixer, shared) = steady_mixer((0.5, -0.25));
    let _adopted = render(&mut mixer, &shared, 16);
    shared.set_routes([[true, false, true, false], [false, true, false, false]]);
    // Adopt the new route on its own before mono, so this test only
    // watches the mono crossfade.  One long block settles the route.
    let _routed = render(&mut mixer, &shared, 320);
    shared.set_mix(false, true, true, [true, true], [true, true], false);
    let out = render(&mut mixer, &shared, 320);
    assert!(out[0][0] > 0.4);
    assert_eq!(*out.last().unwrap(), [0.125; 4]);
    // The third tap was carrying the left input and fades toward the average.
    let mid = out[out.len() / 2];
    assert!(mid[2] > 0.125 && mid[2] < 0.5);
}

#[test]
fn the_ring_path_pushes_finished_frames_into_active_follower_rings() {
    use super::super::shared::{FollowerLink, FollowerPlan};
    use super::super::types::CushionMode;

    let (mut mixer, shared) = steady_mixer((0.5, -0.25));
    shared.set_output(48_000, 64, "test");
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
        &[(CushionMode::Manual, 10.0)],
    );
    shared.follower_bus(0).set_device_channels(2);
    shared.follower_bus(0).reset_playback();
    shared
        .follower_ring_active(0)
        .store(true, Ordering::Relaxed);
    shared
        .follower_bus(0)
        .set_cushion(CushionMode::Manual, 10.0);
    shared.follower_bus(0).set_source_rate(48_000);

    let frames = 64;
    let mut clock = Vec::new();
    mixer.render(&shared, frames, |_, mapped| {
        clock.push([mapped.sample(0), mapped.sample(1)]);
    });
    assert_eq!(
        shared.follower_bus(0).ring_frames(),
        frames,
        "every finished clock frame should land in the follower ring"
    );
    // Default maps on both: the follower carries what the clock plays.
    let mut frame = [0.0; crate::audio::output_map::FRAME_LANES];
    for expected in &clock {
        assert!(shared.follower_bus(0).pop_frame(&mut frame));
        assert_eq!([frame[0], frame[1]], *expected);
    }

    // Duplex path must not push (in-graph fan-out owns followers).
    shared.direct.store(true, Ordering::Relaxed);
    shared.follower_bus(0).reset_playback();
    let _ = render(&mut mixer, &shared, frames);
    assert_eq!(
        shared.follower_bus(0).ring_frames(),
        0,
        "direct mode must not fill generic follower rings"
    );
}

#[test]
fn a_generic_follower_gets_its_own_sum_of_the_paths() {
    use super::super::shared::{FollowerLink, FollowerPlan};
    use super::super::types::CushionMode;

    let (mut mixer, shared) = two_path_mixer((0.5, -0.25), 2);
    // The clock device plays path 0 only; the 4-channel follower plays
    // path 0 on Out 1/2 and path 1, inverted, on Out 3/4.
    shared
        .clock_device
        .set(&device_with(&[(1, PathMapSettings::unrouted())]));
    let follower = device_with(&[(
        1,
        PathMapSettings {
            map: ChannelMap {
                left: 0b0100,
                right: 0b1000,
            },
            invert: true,
            ..PathMapSettings::default()
        },
    )]);
    let mut plan = FollowerPlan {
        count: 1,
        ..FollowerPlan::default()
    };
    plan.slots[0] = FollowerLink {
        device_id: "test:follower".to_owned(),
        ..FollowerLink::default()
    };
    shared.set_followers(&[follower], &plan, &[(CushionMode::Safe, 10.0)]);
    let bus = shared.follower_bus(0);
    bus.set_device_channels(4);
    bus.reset_playback();
    shared
        .follower_ring_active(0)
        .store(true, Ordering::Relaxed);

    let clock = device_frames(&mut mixer, &shared, 2, 600);
    assert_eq!(clock[599], vec![0.5, -0.25]);
    assert_eq!(bus.ring_frames(), 600);
    let mut frame = [0.0; crate::audio::output_map::FRAME_LANES];
    for _ in 0..600 {
        assert!(bus.pop_frame(&mut frame));
    }
    assert_eq!(frame[..4], [0.5, -0.25, -0.5, 0.25]);
    // The clock side publishes the follower's meters, per channel.
    assert_eq!(shared.followers[0].meters()[..4], [0.5, 0.25, 0.5, 0.25]);

    // A follower whose stream stopped is not mapped: its ring stays put
    // and its meters fall to 0.
    shared
        .follower_ring_active(0)
        .store(false, Ordering::Relaxed);
    let _ = device_frames(&mut mixer, &shared, 2, 64);
    assert_eq!(bus.ring_frames(), 0);
    assert_eq!(shared.followers[0].meters()[..4], [0.0; 4]);
}

/// A steady mixer with a second path in slot 1 (EQs bypassed, playing
/// the same input) on a `channels`-channel clock device.
fn two_path_mixer(signal: (f32, f32), channels: usize) -> (OutputMixer, Shared) {
    use super::super::path::PathMix;
    let (_, shared) = steady_mixer(signal);
    let bypassed = EqSettings {
        enabled: false,
        ..EqSettings::default()
    };
    let mix = PathMix {
        eqs: [bypassed, bypassed],
        ..PathMix::default()
    };
    shared.restore_path(1, &mix, Vec::new());
    (OutputMixer::new(48_000, channels), shared)
}

#[test]
fn each_live_path_publishes_its_dsp_time_and_block_budget() {
    let (mut mixer, shared) = two_path_mixer((0.5, -0.25), 2);
    mixer.prepare(480);
    let _ = device_frames(&mut mixer, &shared, 2, 480);
    for slot in 0..2 {
        let time = shared.paths[slot].dsp_time();
        assert_eq!(time.block_ns, 10_000_000, "480 frames at 48 kHz are 10 ms");
        assert!(time.avg_ns < time.block_ns, "a plain chain fits its block");
        assert!(
            time.peak_ns >= time.avg_ns,
            "one block: the peak is that block"
        );
    }
    assert_eq!(
        shared.paths[2].dsp_time(),
        super::super::path::DspTime::default(),
        "an unused slot never runs"
    );
    shared.paths[0].reset_meters();
    assert_eq!(
        shared.paths[0].dsp_time(),
        super::super::path::DspTime::default()
    );
}

#[test]
fn the_total_dsp_peak_is_the_worst_blocks_sum_not_the_sum_of_peaks() {
    use super::path::DspMeter;
    const MS: u64 = 1_000_000;
    // Two paths that each spike once, in different blocks (10 ms each).
    let blocks = [(8 * MS, MS), (MS, 8 * MS), (MS, MS)];
    let (mut first, mut second, mut total) = (
        DspMeter::default(),
        DspMeter::default(),
        DspMeter::default(),
    );
    let mut times = (Default::default(), Default::default(), Default::default());
    for (a, b) in blocks {
        times = (
            first.push(a, 480, 48_000),
            second.push(b, 480, 48_000),
            total.push(a + b, 480, 48_000),
        );
    }
    let (first, second, total) = times;
    assert_eq!(total.block_ns, 10_000_000);
    let summed_peaks = first.peak_ns as u64 + second.peak_ns as u64;
    assert!(
        summed_peaks > 15 * MS,
        "each path kept its spike: {summed_peaks}"
    );
    assert!(
        total.peak_ns as u64 <= 9 * MS && total.peak_ns as u64 > 8 * MS,
        "the worst block took 9 ms: {}",
        total.peak_ns
    );
    assert!(total.peak_share() < 0.95, "no block was nearly late");
}

#[test]
fn a_new_block_length_starts_the_dsp_meter_over() {
    use super::path::DspMeter;
    const MS: u64 = 1_000_000;
    let mut meter = DspMeter::default();
    // 1024 frames at 48 kHz (21.3 ms), each taking 15 ms: 70%.
    for _ in 0..200 {
        meter.push(15 * MS, 1024, 48_000);
    }
    // The quantum drops to 256 frames (5.3 ms) and the load with it.
    let time = meter.push(3 * MS, 256, 48_000);
    assert_eq!(time.block_ns, 5_333_333);
    assert_eq!(time.avg_ns as u64, 3 * MS, "no 15 ms left over");
    assert_eq!(time.peak_ns as u64, 3 * MS, "no 15 ms peak left over");
    assert!(time.peak_share() < 0.95, "{}", time.peak_share());
    // A rate change is a new block length too.
    let time = meter.push(MS, 256, 96_000);
    assert_eq!(time.peak_ns as u64, MS);
    // The same block length keeps smoothing.
    let time = meter.push(2 * MS, 256, 96_000);
    assert!(time.avg_ns as u64 > MS && (time.avg_ns as u64) < 2 * MS);
    assert_eq!(time.peak_ns as u64, 2 * MS);
}

#[test]
fn the_mixer_publishes_the_total_dsp_time_of_its_paths() {
    let (mut mixer, shared) = two_path_mixer((0.5, -0.25), 2);
    mixer.prepare(480);
    assert_eq!(
        shared.total_dsp.load(),
        super::super::path::DspTime::default()
    );
    for _ in 0..4 {
        let _ = device_frames(&mut mixer, &shared, 2, 480);
    }
    let total = shared.total_dsp.load();
    assert_eq!(total.block_ns, 10_000_000);
    assert!(
        total.avg_ns < total.block_ns,
        "two plain chains fit the block"
    );
    let paths: Vec<_> = (0..2).map(|slot| shared.paths[slot].dsp_time()).collect();
    // Each block's total is the sum of the paths' times in that block, so
    // its peak is at most the sum of their peaks (one ns of truncation each).
    assert!(total.peak_ns <= paths[0].peak_ns + paths[1].peak_ns + 2);
    assert!(total.peak_ns + 1 >= paths[0].peak_ns.max(paths[1].peak_ns));
    shared.reset();
    assert_eq!(
        shared.total_dsp.load(),
        super::super::path::DspTime::default()
    );
}

/// `frames` frames of the clock device, as one block.
fn device_frames(
    mixer: &mut OutputMixer,
    shared: &Shared,
    channels: usize,
    frames: usize,
) -> Vec<Vec<f32>> {
    let mut out = vec![vec![0.0f32; channels]; frames];
    mixer.render(shared, frames, |index, mapped| {
        for (channel, sample) in out[index].iter_mut().enumerate() {
            *sample = mapped.sample(channel);
        }
    });
    out
}

fn largest_device_step(out: &[Vec<f32>], channel: usize) -> f32 {
    out.windows(2)
        .map(|pair| (pair[1][channel] - pair[0][channel]).abs())
        .fold(0.0, f32::max)
}

fn device_with(paths: &[(usize, PathMapSettings)]) -> OutputDeviceSettings {
    let mut settings = OutputDeviceSettings::default();
    for (slot, entry) in paths {
        settings.path_map[*slot] = *entry;
    }
    settings
}

#[test]
fn two_paths_add_up_where_their_maps_meet() {
    let (mut mixer, shared) = two_path_mixer((0.5, -0.25), 4);
    // Path 0 on Out 1/2, path 1's L on Out 1 and its R on Out 4.
    shared.clock_device.set(&device_with(&[(
        1,
        PathMapSettings {
            map: ChannelMap {
                left: 0b0001,
                right: 0b1000,
            },
            ..PathMapSettings::default()
        },
    )]));
    let out = device_frames(&mut mixer, &shared, 4, 32);
    // Out 1 sums both L; Out 2 is path 0's R; Out 3 is reached by no
    // path, so it gets the mono copy of both paths mapped here; Out 4 is
    // path 1's R.
    assert_eq!(out[31], [1.0, -0.25, 0.25, -0.25]);
}

#[test]
fn level_and_polarity_apply_per_path_and_device() {
    let (mut mixer, shared) = two_path_mixer((0.5, -0.25), 2);
    let gain = 10.0f32.powf(-6.0 / 20.0);
    shared.clock_device.set(&device_with(&[
        (
            0,
            PathMapSettings {
                level_db: -6.0,
                ..PathMapSettings::default()
            },
        ),
        (
            1,
            PathMapSettings {
                invert: true,
                ..PathMapSettings::default()
            },
        ),
    ]));
    // Follower 0 keeps both paths at unity, normal polarity.
    let plan = super::super::shared::FollowerPlan {
        count: 1,
        ..Default::default()
    };
    shared.set_followers(&[OutputDeviceSettings::default()], &plan, &[]);
    // The duplex graph maps the followers in this mixer.
    shared.direct.store(true, Ordering::Relaxed);
    let mut clock = Vec::new();
    let mut follower = Vec::new();
    mixer.render_direct_devices(&shared, &[0.5; 64], &[-0.25; 64], |device, _, mapped| {
        let frame = [mapped.sample(0), mapped.sample(1)];
        if device == 0 {
            clock.push(frame);
        } else {
            follower.push(frame);
        }
    });
    assert_eq!(clock[63], [0.5 * gain - 0.5, -0.25 * gain + 0.25]);
    assert_eq!(follower[63], [1.0, -0.5]);
    assert_eq!(shared.clock_device.path_gain(1), -1.0);
}

#[test]
fn a_polarity_flip_ramps_through_zero_without_a_click() {
    let (mut mixer, shared) = two_path_mixer((0.5, 0.5), 2);
    let _adopted = device_frames(&mut mixer, &shared, 2, 16);
    shared.clock_device.set(&device_with(&[(
        1,
        PathMapSettings {
            invert: true,
            ..PathMapSettings::default()
        },
    )]));
    let out = device_frames(&mut mixer, &shared, 2, 600);
    assert_eq!(
        out[0][0],
        1.0 - 1.0 / 288.0,
        "the first sample barely moves"
    );
    assert!(out.iter().any(|frame| frame[0].abs() < 0.01), "passes zero");
    assert_eq!(out[599], [0.0, 0.0]);
    assert!(largest_device_step(&out, 0) < 1.0 / 100.0);
}

#[test]
fn solo_silences_the_other_paths_with_a_ramp() {
    let (mut mixer, shared) = two_path_mixer((0.5, -0.25), 2);
    assert_eq!(render(&mut mixer, &shared, 16)[15], [1.0, -0.5, 0.0, 0.0]);
    shared.paths[1].set_solo(true);
    assert!(shared.any_solo());
    let out = render(&mut mixer, &shared, 600);
    assert!(out[0][0] > 0.99);
    assert_eq!(out[599], [0.5, -0.25, 0.0, 0.0]);
    assert!(largest_step(&out) < 0.5 / 100.0);
    // Soloing both is the same as soloing none.
    shared.paths[0].set_solo(true);
    assert_eq!(render(&mut mixer, &shared, 600)[599], [1.0, -0.5, 0.0, 0.0]);
    // A path fading out does not keep the others silent by its solo.
    shared.paths[0].set_solo(false);
    assert!(shared.remove_path(1));
    assert!(!shared.any_solo());
    let out = render(&mut mixer, &shared, 600);
    assert_eq!(out[599], [0.5, -0.25, 0.0, 0.0]);
}

#[test]
fn path_mute_and_level_ramp_without_a_click() {
    let (mut mixer, shared) = two_path_mixer((0.8, 0.8), 2);
    let _adopted = render(&mut mixer, &shared, 16);
    shared.paths[1].set_mute(true);
    let muted = render_blocks(&mut mixer, &shared, 2);
    assert!(muted[0][0] > 1.59);
    assert_eq!(muted.last().unwrap()[0], 0.8);
    assert!(
        largest_step(&muted) < 0.8 / 100.0,
        "{}",
        largest_step(&muted)
    );

    shared.paths[1].set_mute(false);
    shared.paths[1].set_level_db(-6.0);
    let gain = 10.0f32.powf(-6.0 / 20.0);
    let back = render_blocks(&mut mixer, &shared, 2);
    assert!(largest_step(&back) < 0.8 / 100.0);
    assert_eq!(back.last().unwrap()[0], 0.8 + 0.8 * gain);
    // The path meters stay before the path level.
    assert_eq!(shared.paths[1].meters(), (0.8, 0.8));
}

#[test]
fn a_removed_path_fades_out_and_then_goes_inactive() {
    use super::super::path::PathState;
    let (mut mixer, shared) = two_path_mixer((0.8, 0.8), 2);
    let _adopted = render(&mut mixer, &shared, 16);
    assert!(shared.remove_path(0));
    assert_eq!(shared.paths[0].state(), PathState::FadingOut);
    assert!(!shared.remove_path(1), "the last active path stays");
    let out = render_blocks(&mut mixer, &shared, 2);
    assert!(out[0][0] > 1.59);
    assert_eq!(out.last().unwrap()[0], 0.8);
    assert!(largest_step(&out) < 0.8 / 100.0);
    assert_eq!(shared.paths[0].state(), PathState::Inactive);
    // A stalled fade (no block ran) is finished by the UI thread.
    assert!(!shared.remove_path(1));
    let added = shared.add_path().expect("slot 0 is free again");
    assert_eq!(added, 0);
    assert!(shared.remove_path(added));
    shared.service_paths();
    assert_eq!(shared.paths[added].state(), PathState::Inactive);
    // With a bridge running, the UI waits for the mixer, and only
    // finishes a fade that is overdue.
    shared.running.store(true, Ordering::Relaxed);
    let added = shared.add_path().unwrap();
    assert!(shared.remove_path(added));
    shared.service_paths();
    assert_eq!(shared.paths[added].state(), PathState::FadingOut);
    shared.paths[added].backdate_removal(std::time::Duration::from_secs(2));
    shared.service_paths();
    assert_eq!(shared.paths[added].state(), PathState::Inactive);
}

#[test]
fn a_duplicate_starts_muted_and_copies_the_chain() {
    use super::super::gate::GateSettings;
    let _main = super::super::shared::tests::claim_main_thread();
    let fake = crate::audio::clap::tests::fake_plugin_for_test;
    let (mut mixer, shared) = steady_mixer((0.5, -0.25));
    shared.set_routes([[false, true, false, false], [true, false, false, false]]);
    let gate = GateSettings {
        threshold_db: -50.0,
        ..GateSettings::default()
    };
    shared.set_gate(gate);
    shared.paths[0].set_level_db(-3.0);
    shared.paths[0].edit_chain(|chain| chain.insert_clap_for_test(fake(false)));
    let before = render(&mut mixer, &shared, 64);

    let copy = shared.duplicate_path(0).expect("a free slot");
    assert_eq!(copy, 1);
    let duplicate = &shared.paths[copy];
    assert!(duplicate.muted() && !duplicate.soloed());
    let mix = duplicate.mix();
    assert_eq!(mix.routes, shared.paths[0].mix().routes);
    assert_eq!(mix.gate, gate);
    assert_eq!(mix.level_db, -3.0);
    let plugins = duplicate.plugins.lock().unwrap().prefs();
    assert_eq!(plugins.len(), 1);
    assert_eq!(plugins, shared.paths[0].plugin_prefs());
    // Added muted: the sum does not move.
    let after = render(&mut mixer, &shared, 64);
    assert_eq!(after[63], before[63]);
}

#[test]
fn a_new_path_stays_silent_until_its_plugins_open_then_fades_in() {
    use super::super::path::PathMix;
    let (mut mixer, shared) = steady_mixer((0.5, -0.25));
    let _adopted = render(&mut mixer, &shared, 16);
    let bypassed = EqSettings {
        enabled: false,
        ..EqSettings::default()
    };
    let mix = PathMix {
        eqs: [bypassed, bypassed],
        ..PathMix::default()
    };
    let pending = crate::audio::PluginPref {
        path: "missing.vst3".to_owned(),
        name: "Missing".to_owned(),
        ..crate::audio::PluginPref::default()
    };
    shared.restore_path(1, &mix, vec![pending]);
    assert!(shared.paths[1].chain_pending());
    let waiting = render(&mut mixer, &shared, 400);
    assert!(waiting.iter().all(|frame| *frame == [0.5, -0.25, 0.0, 0.0]));
    // The UI thread tries to open it (here it fails, so the insert is
    // left out): the path fades in instead of jumping.
    shared.paths[1].ensure_plugin_loaded(0);
    assert!(!shared.paths[1].chain_pending());
    let out = render(&mut mixer, &shared, 400);
    assert!(out[0][0] > 0.5 && out[0][0] < 0.51, "{:?}", out[0]);
    assert!(largest_step(&out) < 0.5 / 100.0);
    assert_eq!(out[399], [1.0, -0.5, 0.0, 0.0]);
}
