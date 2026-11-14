use super::*;
use crate::audio::path::{MAX_PATHS, PATH_LEVEL_MAX_DB};

#[test]
fn a_settled_frame_matches_the_mapper() {
    let mut map = ChannelMap::default();
    map.set(MixSide::R, 2, true);
    map.set(MixSide::L, 3, true);
    let cases = [
        OutputDeviceSettings::default(),
        OutputDeviceSettings {
            level_db: -6.0,
            path_map: PathMapSettings::first(map),
            unused: UnusedChannels::Silent,
            ..OutputDeviceSettings::default()
        },
        OutputDeviceSettings {
            mute: true,
            ..OutputDeviceSettings::default()
        },
    ];
    for settings in cases {
        for channels in [1, 2, 6] {
            let controls = DeviceControls::new();
            controls.set(&settings);
            let mut mapper = DeviceMapper::new(channels);
            mapper.begin_block(&controls, 64.0);
            let mapped = mapper.map(0.5, -0.25);
            let settled = settings.settled_sum(ONE_PATH, [1.0; MAX_PATHS], channels, 0.5, -0.25);
            for (channel, sample) in settled.iter().enumerate() {
                let expected = mapped.sample(channel);
                assert!(
                    (sample - expected).abs() < 1.0e-6,
                    "{settings:?} {channels} ch, channel {channel}: {sample} != {expected}"
                );
            }
        }
    }
}

const RAMP: f32 = 288.0;

fn mapped_device(
    settings: OutputDeviceSettings,
    channels: usize,
) -> (DeviceMapper, DeviceControls) {
    let controls = DeviceControls::new();
    controls.set(&settings);
    let mut mapper = DeviceMapper::new(channels);
    mapper.begin_block(&controls, RAMP);
    (mapper, controls)
}

fn samples(frame: &DeviceFrame, channels: usize) -> Vec<f32> {
    (0..channels).map(|channel| frame.sample(channel)).collect()
}

fn bits(values: &[f32]) -> Vec<u32> {
    values.iter().map(|value| value.to_bits()).collect()
}

/// Awkward inputs: signed zeros, rounding-sensitive pairs, subnormals,
/// hot and huge values.
const PAIRS: [(f32, f32); 12] = [
    (0.5, -0.25),
    (-0.0, 0.0),
    (-0.0, -0.0),
    (0.0, -0.0),
    (0.1, 0.2),
    (1.0e-30, -3.0e-30),
    (1.0e-40, 2.0e-41),
    (1.7, -2.3),
    (f32::MAX, f32::MAX),
    (0.333_333_34, 0.666_666_7),
    (-1.0, 1.0),
    (0.123_456_79, -0.987_654_3),
];

/// Pairs the safety ceiling does not touch.  Hot pairs are covered by
/// [`the_safety_ceiling_holds_a_hot_signal_and_publishes_the_reduction`].
fn under_the_ceiling((left, right): &(f32, f32)) -> bool {
    left.abs().max(right.abs()) <= crate::audio::safety_ceiling_linear()
}

#[test]
fn the_default_map_writes_exactly_what_the_output_always_wrote() {
    for channels in [1, 2, 3, 4, 8, 10] {
        let (mut mapper, controls) = mapped_device(OutputDeviceSettings::default(), channels);
        // Two blocks: the adopted first block and a steady later one.
        for block in 0..2 {
            if block == 1 {
                mapper.begin_block(&controls, RAMP);
            }
            for (left, right) in PAIRS.into_iter().filter(under_the_ceiling) {
                let mut expected = vec![0.0f32; channels];
                old_write_frame(&mut expected, left, right);
                let got = samples(mapper.map(left, right), channels);
                assert_eq!(
                    bits(&got),
                    bits(&expected),
                    "{channels} ch, block {block}, ({left:e}, {right:e})"
                );
            }
        }
    }
}

#[test]
fn the_safety_ceiling_holds_a_hot_signal_and_publishes_the_reduction() {
    let ceiling = crate::audio::safety_ceiling_linear();
    let (mut mapper, controls) = mapped_device(OutputDeviceSettings::default(), 2);
    let quiet = samples(mapper.map(-0.0, 0.25), 2);
    assert_eq!(bits(&quiet), bits(&[-0.0, 0.25]));

    let hot = samples(mapper.map(1.0, -0.5), 2);
    assert_eq!(hot[0], ceiling);
    assert!((hot[1] + 0.5 * ceiling).abs() < 1.0e-6, "{}", hot[1]);
    mapper.end_block(&controls);
    let gr = controls.reduction_db();
    assert!(
        (gr - 1.0).abs() < 0.05,
        "full scale is about 1 dB under: {gr}"
    );

    // Mute is after the ceiling: a snapped mute is silence, and the
    // reduction is still the levelled signal.
    let (mut mapper, controls) = mapped_device(
        OutputDeviceSettings {
            mute: true,
            ..OutputDeviceSettings::default()
        },
        2,
    );
    let silenced = samples(mapper.map(1.0, -1.0), 2);
    assert_eq!(silenced, [0.0, 0.0]);
    mapper.end_block(&controls);
    assert!(
        controls.reduction_db() >= crate::audio::SAFETY_GR_DB,
        "limiter runs before mute: {}",
        controls.reduction_db()
    );
}

#[test]
fn both_sides_on_one_channel_play_their_average() {
    let settings = OutputDeviceSettings {
        path_map: PathMapSettings::first(ChannelMap {
            left: 0b01,
            right: 0b01,
        }),
        unused: UnusedChannels::Silent,
        ..OutputDeviceSettings::default()
    };
    let (mut mapper, _) = mapped_device(settings, 2);
    assert_eq!(samples(mapper.map(0.5, -0.25), 2), [0.125, 0.0]);
}

#[test]
fn silent_leaves_unreached_channels_at_zero() {
    let settings = OutputDeviceSettings {
        path_map: PathMapSettings::first(ChannelMap {
            left: 0b0100,
            right: 0b1000,
        }),
        unused: UnusedChannels::Silent,
        ..OutputDeviceSettings::default()
    };
    let (mut mapper, _) = mapped_device(settings, 4);
    let out = samples(mapper.map(0.5, -0.25), 4);
    assert_eq!(out, [0.0, 0.0, 0.5, -0.25]);
    assert_eq!(bits(&out[..2]), [0, 0], "silence is +0");
}

#[test]
fn a_mono_device_reads_any_map_as_both_sides() {
    let swapped = OutputDeviceSettings {
        path_map: PathMapSettings::first(ChannelMap {
            left: 0b10,
            right: 0,
        }),
        unused: UnusedChannels::Silent,
        ..OutputDeviceSettings::default()
    };
    let (mut mapper, _) = mapped_device(swapped, 1);
    assert_eq!(samples(mapper.map(0.5, -0.25), 1), [0.125]);

    let empty = OutputDeviceSettings {
        path_map: PathMapSettings::first(ChannelMap { left: 0, right: 0 }),
        ..swapped
    };
    let (mut mapper, _) = mapped_device(empty, 1);
    assert_eq!(samples(mapper.map(0.5, -0.25), 1), [0.0]);
    let (mut mapper, _) = mapped_device(
        OutputDeviceSettings {
            unused: UnusedChannels::MonoCopy,
            ..empty
        },
        1,
    );
    assert_eq!(samples(mapper.map(0.5, -0.25), 1), [0.125]);
}

#[test]
fn channels_past_the_maximum_are_unused() {
    let all = OutputDeviceSettings {
        path_map: PathMapSettings::first(ChannelMap {
            left: 0xff,
            right: 0,
        }),
        ..OutputDeviceSettings::default()
    };
    let (mut mapper, _) = mapped_device(all, 10);
    let out = samples(mapper.map(0.5, -0.25), 10);
    assert_eq!(&out[..8], &[0.5; 8]);
    assert_eq!(&out[8..], &[0.125; 2]);

    let silent = OutputDeviceSettings {
        unused: UnusedChannels::Silent,
        ..all
    };
    let (mut mapper, _) = mapped_device(silent, 10);
    let out = samples(mapper.map(0.5, -0.25), 10);
    assert_eq!(&out[8..], &[0.0; 2]);
    // Past the device's own channels there is nothing to play.
    assert_eq!(mapper.map(0.5, -0.25).sample(10), 0.0);
}

#[test]
fn the_device_level_scales_after_the_map() {
    let settings = OutputDeviceSettings {
        level_db: -6.0,
        ..OutputDeviceSettings::default()
    };
    let (mut mapper, _) = mapped_device(settings, 3);
    let gain = 10.0f32.powf(-6.0 / 20.0);
    let out = samples(mapper.map(0.5, -0.25), 3);
    assert_eq!(out, [0.5 * gain, -0.25 * gain, 0.125 * gain]);
    assert!((gain - 0.501).abs() < 0.001);
}

/// Runs `frames` frames of a constant mix through one block.
fn run(
    mapper: &mut DeviceMapper,
    controls: &DeviceControls,
    channels: usize,
    frames: usize,
) -> Vec<Vec<f32>> {
    mapper.begin_block(controls, RAMP);
    let out = (0..frames)
        .map(|_| samples(mapper.map(0.5, -0.25), channels))
        .collect();
    mapper.end_block(controls);
    out
}

fn largest_step(out: &[Vec<f32>], channel: usize) -> f32 {
    out.windows(2)
        .map(|pair| (pair[1][channel] - pair[0][channel]).abs())
        .fold(0.0, f32::max)
}

#[test]
fn device_mute_fades_to_silence() {
    let (mut mapper, controls) = mapped_device(OutputDeviceSettings::default(), 2);
    let _adopted = run(&mut mapper, &controls, 2, 16);
    controls.set(&OutputDeviceSettings {
        mute: true,
        ..OutputDeviceSettings::default()
    });
    let out = run(&mut mapper, &controls, 2, 400);
    assert!(out[0][0] > 0.49 && out[0][0] < 0.5);
    assert!(largest_step(&out, 0) < 0.5 / 100.0);
    let settled = out
        .iter()
        .position(|frame| frame.iter().all(|value| *value == 0.0))
        .expect("mute reaches silence");
    assert!(settled <= RAMP as usize + 1, "{settled}");
    assert!(out[settled..]
        .iter()
        .all(|frame| frame.iter().all(|value| *value == 0.0)));
    let meters = controls.meters();
    assert!(meters[0] > 0.49, "the block's peak includes the fade");
}

#[test]
fn a_map_change_crossfades_and_settles() {
    let (mut mapper, controls) = mapped_device(OutputDeviceSettings::default(), 4);
    let before = run(&mut mapper, &controls, 4, 16);
    assert_eq!(before[15], [0.5, -0.25, 0.125, 0.125]);

    // Swap the sides and silence the rest.
    let swapped = OutputDeviceSettings {
        path_map: PathMapSettings::first(ChannelMap {
            left: 0b10,
            right: 0b01,
        }),
        unused: UnusedChannels::Silent,
        ..OutputDeviceSettings::default()
    };
    controls.set(&swapped);
    let mut out = run(&mut mapper, &controls, 4, 100);
    // A block boundary mid-ramp continues the same slope.
    out.extend(run(&mut mapper, &controls, 4, 300));
    let full = 0.75;
    for channel in 0..4 {
        let step = largest_step(&out, channel);
        assert!(step < full / 100.0, "channel {channel} stepped by {step}");
    }
    assert!(out[0][0] > 0.49, "the first sample barely moves");
    let mid = &out[144];
    assert!(mid[0] > -0.2 && mid[0] < 0.45, "{mid:?}");
    assert_eq!(*out.last().unwrap(), [-0.25, 0.5, 0.0, 0.0]);
    let settled = out
        .iter()
        .position(|frame| frame[..] == [-0.25, 0.5, 0.0, 0.0])
        .expect("the new map is reached");
    assert!(settled <= RAMP as usize + 1, "{settled}");
    assert!(out[settled..]
        .iter()
        .all(|frame| frame[..] == [-0.25, 0.5, 0.0, 0.0]));
}

#[test]
fn meters_hold_each_channels_block_peak() {
    let settings = OutputDeviceSettings {
        path_map: PathMapSettings::first(ChannelMap {
            left: 0b001,
            right: 0b100,
        }),
        unused: UnusedChannels::Silent,
        ..OutputDeviceSettings::default()
    };
    let controls = DeviceControls::new();
    controls.set(&settings);
    let mut mapper = DeviceMapper::new(3);
    mapper.begin_block(&controls, RAMP);
    mapper.map(0.25, 0.1);
    mapper.map(-0.75, -0.5);
    mapper.map(0.5, 0.2);
    mapper.end_block(&controls);
    let meters = controls.meters();
    assert_eq!(meters[..3], [0.75, 0.0, 0.5]);
    assert!(meters[3..].iter().all(|meter| *meter == 0.0));
    mapper.publish_channels(&controls);
    assert_eq!(controls.channels(), 3);

    // The next block starts its peaks over.
    mapper.begin_block(&controls, RAMP);
    mapper.map(0.1, 0.0);
    mapper.end_block(&controls);
    assert_eq!(controls.meters()[..3], [0.1, 0.0, 0.0]);

    controls.reset();
    assert_eq!(controls.meters(), [0.0; MAX_OUTPUT_CHANNELS]);
    assert_eq!(controls.channels(), 0);
}

#[test]
fn controls_store_the_sanitized_level_and_copy_to_a_new_session() {
    let controls = DeviceControls::new();
    let (map, unused, gain) = controls.load();
    assert_eq!(
        (map, unused, gain),
        (ChannelMap::default(), UnusedChannels::MonoCopy, 1.0)
    );
    let settings = OutputDeviceSettings {
        level_db: 40.0,
        mute: false,
        path_map: PathMapSettings::first(ChannelMap {
            left: 0b1100,
            right: 0b0011,
        }),
        unused: UnusedChannels::Silent,
        align: false,
        click_mask: 0,
    };
    controls.set(&settings);
    let next = DeviceControls::new();
    next.adopt_settings_from(&controls);
    let (map, unused, gain) = next.load();
    assert_eq!(map, settings.map(0));
    assert_eq!(unused, UnusedChannels::Silent);
    assert_eq!(gain, 10.0f32.powf(DEVICE_LEVEL_MAX_DB / 20.0));
    next.set(&OutputDeviceSettings {
        mute: true,
        ..settings
    });
    assert_eq!(next.load().2, 0.0);
}

#[test]
fn live_paths_add_up_on_each_channel_and_others_are_ignored() {
    use crate::audio::path::{LiveSlots, PathState, MAX_PATHS};
    let controls = DeviceControls::new();
    let live = LiveSlots::from_states([
        PathState::Active,
        PathState::Inactive,
        PathState::FadingOut,
        PathState::Inactive,
    ]);
    let mut mapper = DeviceMapper::new(3);
    mapper.begin_paths(&controls, RAMP, live, &super::PathTiming::default(), 64);
    let mut frames = [[0.0f32; 2]; MAX_PATHS];
    frames[0] = [0.5, -0.25];
    frames[1] = [9.0, 9.0];
    frames[2] = [0.25, 0.5];
    let out = samples(mapper.map_frames(&frames, 0.0), 3);
    assert_eq!(out, [0.75, 0.25, 0.125 + 0.375]);
    let next = DeviceControls::new();
    next.adopt_settings_from(&controls);
    assert_eq!(next.path_map(2), ChannelMap::default());
}

#[test]
fn unreached_channels_copy_the_paths_mapped_to_the_device() {
    use crate::audio::path::{LiveSlots, PathState};
    let live = LiveSlots::from_states([
        PathState::Active,
        PathState::Active,
        PathState::Inactive,
        PathState::Inactive,
    ]);
    let mut frames = [[0.0f32; 2]; MAX_PATHS];
    frames[0] = [0.5, -0.25];
    frames[1] = [0.25, 0.75];
    let run = |settings: OutputDeviceSettings| {
        let controls = DeviceControls::new();
        controls.set(&settings);
        let mut mapper = DeviceMapper::new(4);
        mapper.begin_paths(&controls, RAMP, live, &super::PathTiming::default(), 64);
        samples(mapper.map_frames(&frames, 0.0), 4)
    };
    // Path 1 is routed nowhere: Out 3/4 copy path 0 only.
    let mut settings = OutputDeviceSettings::default();
    settings.path_map[1] = PathMapSettings::unrouted();
    assert_eq!(run(settings), [0.5, -0.25, 0.125, 0.125]);
    assert_eq!(settings.playing_channels(4, [true, true, false, false]), 4);
    // Path 1 on Out 3 only: Out 4 copies both (mono of each, summed).
    settings.path_map[1].map = ChannelMap {
        left: 0b0100,
        right: 0,
    };
    assert_eq!(run(settings), [0.5, -0.25, 0.25, 0.125 + 0.5]);
    // No path reaches the device: every channel copies every path.
    settings.path_map[0] = PathMapSettings::unrouted();
    settings.path_map[1] = PathMapSettings::unrouted();
    assert_eq!(run(settings), [0.625; 4]);
    settings.unused = UnusedChannels::Silent;
    assert_eq!(run(settings), [0.0; 4]);
    assert_eq!(settings.playing_channels(4, [true, true, false, false]), 0);
}

#[test]
fn path_entries_save_compactly_and_old_maps_load_as_path_one() {
    let old: OutputDeviceSettings =
        ron::from_str("(level_db:-3.0,map:(left:4,right:8),unused:silent)").unwrap();
    assert_eq!(old.map(0), ChannelMap { left: 4, right: 8 });
    assert_eq!(old.path_map[1], PathMapSettings::default());
    assert_eq!(old.level_db, -3.0);
    // Files from before the align switch align.
    assert!(old.align);

    let unaligned = OutputDeviceSettings {
        align: false,
        ..OutputDeviceSettings::default()
    };
    let text = ron::to_string(&unaligned).unwrap();
    assert_eq!(
        ron::from_str::<OutputDeviceSettings>(&text).unwrap(),
        unaligned
    );

    let mut settings = OutputDeviceSettings::default();
    settings.path_map[1] = PathMapSettings {
        map: ChannelMap { left: 1, right: 0 },
        level_db: -6.0,
        invert: true,
        trim_ms: 1.5,
    };
    let text = ron::to_string(&settings).unwrap();
    assert!(!text.contains(",map:("), "no legacy map: {text}");
    let back: OutputDeviceSettings = ron::from_str(&text).unwrap();
    assert_eq!(back, settings);
    // Trailing default entries are left out.
    let text = ron::to_string(&OutputDeviceSettings::default()).unwrap();
    assert!(text.contains("path_map:[]"), "{text}");

    // Reordered for saving: slot 2 becomes entry 1.
    settings.path_map[2].level_db = -9.0;
    let saved = settings.with_path_order(&[0, 2]);
    assert_eq!(saved.path_map[1].level_db, -9.0);
    assert_eq!(saved.path_map[2], PathMapSettings::default());
    // Out-of-range levels and trims are clamped.
    settings.path_map[1].level_db = 40.0;
    settings.path_map[1].trim_ms = f32::NAN;
    let clean = settings.sanitized();
    assert_eq!(clean.path_map[1].level_db, PATH_LEVEL_MAX_DB);
    assert_eq!(clean.path_map[1].trim_ms, 0.0);
}

#[test]
fn channel_map_routes_and_sets_device_channels() {
    let mut map = ChannelMap::default();
    assert!(map.routes(MixSide::L, 0) && !map.routes(MixSide::L, 1));
    assert!(map.routes(MixSide::R, 1) && !map.routes(MixSide::R, 0));
    assert!(!map.routes(MixSide::L, MAX_OUTPUT_CHANNELS));
    map.set(MixSide::L, 7, true);
    assert_eq!(map.left, 0b1000_0001);
    map.set(MixSide::R, MAX_OUTPUT_CHANNELS, true);
    assert_eq!(map.right, 0b10);
    map.set(MixSide::L, 0, false);
    map.set(MixSide::L, 7, false);
    map.set(MixSide::R, 1, false);
    assert!(map.is_empty());
    assert!(!ChannelMap::default().is_empty());
}

#[test]
fn device_level_is_clamped_and_nan_is_zero() {
    let level = |level_db| {
        OutputDeviceSettings {
            level_db,
            ..OutputDeviceSettings::default()
        }
        .sanitized()
        .level_db
    };
    assert_eq!(level(20.0), 6.0);
    assert_eq!(level(-100.0), -36.0);
    assert_eq!(level(f32::NAN), 0.0);
    assert_eq!(level(f32::INFINITY), DEVICE_LEVEL_MAX_DB);
    assert_eq!(level(-3.0), -3.0);
}

#[test]
fn unmute_returns_to_exact_unity_and_the_default_output() {
    let (mut mapper, controls) = mapped_device(OutputDeviceSettings::default(), 4);
    let _adopted = run(&mut mapper, &controls, 4, 16);
    controls.set(&OutputDeviceSettings {
        mute: true,
        ..OutputDeviceSettings::default()
    });
    let _muted = run(&mut mapper, &controls, 4, 400);
    controls.set(&OutputDeviceSettings::default());
    let _back = run(&mut mapper, &controls, 4, 400);
    assert_eq!(mapper.gain.next(), 1.0);
    mapper.begin_block(&controls, RAMP);
    for (left, right) in PAIRS.into_iter().filter(under_the_ceiling) {
        let mut expected = vec![0.0f32; 4];
        old_write_frame(&mut expected, left, right);
        let got = samples(mapper.map(left, right), 4);
        assert_eq!(bits(&got), bits(&expected), "({left:e}, {right:e})");
    }
}

#[test]
fn a_level_change_ramps_and_settles_exactly_on_the_target() {
    let (mut mapper, controls) = mapped_device(OutputDeviceSettings::default(), 2);
    let _adopted = run(&mut mapper, &controls, 2, 16);
    controls.set(&OutputDeviceSettings {
        level_db: -6.0,
        ..OutputDeviceSettings::default()
    });
    let gain = 10.0f32.powf(-6.0 / 20.0);
    let out = run(&mut mapper, &controls, 2, 400);
    assert!(out[0][0] < 0.5 && out[0][0] > 0.49);
    assert!(largest_step(&out, 0) < 0.25 / 100.0);
    let target = [0.5 * gain, -0.25 * gain];
    let settled = out
        .iter()
        .position(|frame| frame[..] == target)
        .expect("the level settles");
    assert!(settled <= RAMP as usize + 1, "{settled}");
    assert!(out[settled..].iter().all(|frame| frame[..] == target));
    assert_eq!(mapper.gain.next(), gain);
}

#[test]
fn silent_mono_device_with_an_empty_map_is_silent() {
    let settings = OutputDeviceSettings {
        path_map: PathMapSettings::first(ChannelMap { left: 0, right: 0 }),
        unused: UnusedChannels::Silent,
        ..OutputDeviceSettings::default()
    };
    let (mut mapper, controls) = mapped_device(settings, 1);
    let out = run(&mut mapper, &controls, 1, 8);
    assert!(out.iter().all(|frame| frame[0].to_bits() == 0));
    assert_eq!(controls.meters()[0], 0.0);
}

#[test]
fn the_channel_count_is_published_at_open() {
    let controls = DeviceControls::new();
    assert_eq!(controls.channels(), 0, "unknown until a stream opens");
    DeviceMapper::new(6).publish_channels(&controls);
    assert_eq!(controls.channels(), 6);
    controls.reset();
    assert_eq!(controls.channels(), 0);
}

fn map(left: u8, right: u8) -> ChannelMap {
    ChannelMap { left, right }
}

#[test]
fn presets_fit_the_channel_count() {
    use MapPreset::*;
    assert_eq!(MapPreset::available(1), &[MonoToAll, Clear]);
    assert_eq!(MapPreset::available(2), &[Stereo12, MonoToAll, Swap, Clear]);
    assert_eq!(MapPreset::available(3), &[Stereo12, MonoToAll, Swap, Clear]);
    for channels in [4, 6, 8] {
        assert_eq!(
            MapPreset::available(channels),
            &[Stereo12, Stereo34, BothPairs, MonoToAll, Swap, Clear]
        );
    }
    assert_eq!(Stereo12.label(2), "Stereo");
    assert_eq!(Stereo12.label(4), "Stereo 1/2");
    assert_eq!(BothPairs.label(4), "1/2 + 3/4");
}

#[test]
fn presets_set_the_shown_channels() {
    let default = ChannelMap::default();
    assert_eq!(default.with_preset(MapPreset::Stereo12, 4), map(0b01, 0b10));
    assert_eq!(
        default.with_preset(MapPreset::Stereo34, 4),
        map(0b0100, 0b1000)
    );
    assert_eq!(
        default.with_preset(MapPreset::BothPairs, 4),
        map(0b0101, 0b1010)
    );
    assert_eq!(
        default.with_preset(MapPreset::MonoToAll, 4),
        map(0b1111, 0b1111)
    );
    assert_eq!(
        default.with_preset(MapPreset::MonoToAll, 8),
        map(0xFF, 0xFF)
    );
    assert_eq!(default.with_preset(MapPreset::Swap, 2), map(0b10, 0b01));
    assert_eq!(default.with_preset(MapPreset::Clear, 2), map(0, 0));
    assert_eq!(
        default.with_preset(MapPreset::MonoToAll, 1),
        map(0b01, 0b11),
        "only channel 1 changes on a 1-channel device"
    );
}

#[test]
fn presets_keep_channels_the_device_does_not_have() {
    // Out 3/4 set on a 4-channel interface, now seen as stereo.
    let wide = map(0b0101, 0b1010);
    assert_eq!(wide.with_preset(MapPreset::Clear, 2), map(0b0100, 0b1000));
    assert_eq!(wide.with_preset(MapPreset::Swap, 2), map(0b0110, 0b1001));
    // Not offered on 2 channels; it would only clear Out 1/2.
    assert_eq!(
        wide.with_preset(MapPreset::Stereo34, 2),
        map(0b0100, 0b1000)
    );
}

#[test]
fn a_mono_device_is_reached_by_any_map() {
    assert_eq!(map(0b0100, 0).sides_at(1, 0), (true, true));
    assert_eq!(map(0, 0).sides_at(1, 0), (false, false));
    assert_eq!(ChannelMap::default().sides_at(2, 1), (false, true));
    assert!(map(0b0100, 0).reaches_any(1));
    assert!(
        !map(0b0100, 0).reaches_any(2),
        "channel 3 is past the device"
    );
    assert!(map(0b0100, 0).reaches_any(4));
    assert!(!map(0, 0).reaches_any(8));
}

/// Path 0 alone.
const ONE_PATH: [bool; MAX_PATHS] = [true, false, false, false];

#[test]
fn playing_channels_count_reached_and_copied_ones() {
    let settings = |map, unused| OutputDeviceSettings {
        path_map: PathMapSettings::first(map),
        unused,
        ..OutputDeviceSettings::default()
    };
    let copy = UnusedChannels::MonoCopy;
    let silent = UnusedChannels::Silent;
    assert_eq!(
        settings(ChannelMap::default(), copy).playing_channels(2, ONE_PATH),
        2
    );
    assert_eq!(
        settings(ChannelMap::default(), copy).playing_channels(4, ONE_PATH),
        4
    );
    assert_eq!(
        settings(ChannelMap::default(), silent).playing_channels(4, ONE_PATH),
        2
    );
    assert_eq!(
        settings(map(0b0100, 0b1000), silent).playing_channels(4, ONE_PATH),
        2
    );
    assert_eq!(settings(map(0, 0), silent).playing_channels(4, ONE_PATH), 0);
    assert_eq!(settings(map(0, 0), copy).playing_channels(4, ONE_PATH), 4);
    assert_eq!(
        settings(map(0b0100, 0), silent).playing_channels(1, ONE_PATH),
        1
    );
    assert_eq!(settings(map(0, 0), silent).playing_channels(1, ONE_PATH), 0);
    let muted = OutputDeviceSettings {
        mute: true,
        ..OutputDeviceSettings::default()
    };
    assert_eq!(
        muted.playing_channels(2, ONE_PATH),
        2,
        "mute is not routing"
    );
}

fn one_path(left: f32, right: f32) -> [[f32; 2]; MAX_PATHS] {
    let mut frames = [[0.0; 2]; MAX_PATHS];
    frames[0] = [left, right];
    frames
}

fn lane_bits(frame: &[f32]) -> Vec<u32> {
    frame.iter().map(|sample| sample.to_bits()).collect()
}

#[test]
fn the_tick_lands_only_on_the_masked_channels() {
    let settings = OutputDeviceSettings {
        unused: UnusedChannels::Silent,
        click_mask: 0b1100,
        ..OutputDeviceSettings::default()
    };
    let frames = one_path(0.4, -0.2);
    let (mut with_tick, _) = mapped_device(settings, 4);
    let (mut without, _) = mapped_device(settings, 4);
    let ticked = samples(with_tick.map_frames(&frames, 0.25), 4);
    let quiet = samples(without.map_frames(&frames, 0.0), 4);
    assert_eq!(lane_bits(&ticked[..2]), lane_bits(&quiet[..2]));
    assert_ne!(ticked[2].to_bits(), quiet[2].to_bits());
    assert_ne!(ticked[3].to_bits(), quiet[3].to_bits());
    assert_eq!(ticked[2], 0.25);
    assert_eq!(ticked[3], 0.25);
}

#[test]
fn a_zero_click_sample_does_not_write_a_lane() {
    let (mut mapper, _) = mapped_device(OutputDeviceSettings::default(), 4);
    let frames = one_path(-0.0, -0.0);
    let out = samples(mapper.map_frames(&frames, 0.0), 4);
    // Channels 1 and 2 copy the inputs.  The unmapped pair is the mono
    // copy, which stays a negative zero.  Adding 0.0 would have cleared it.
    assert_eq!(out[0].to_bits(), (-0.0f32).to_bits());
    assert_eq!(out[1].to_bits(), (-0.0f32).to_bits());
    assert_eq!(out[2].to_bits(), (-0.0f32).to_bits());
    assert_eq!(out[3].to_bits(), (-0.0f32).to_bits());
}

#[test]
fn a_zero_mask_adds_the_tick_nowhere() {
    let settings = OutputDeviceSettings {
        click_mask: 0,
        ..OutputDeviceSettings::default()
    };
    let frames = one_path(0.4, -0.2);
    let (mut with_tick, _) = mapped_device(settings, 4);
    let (mut without, _) = mapped_device(settings, 4);
    let ticked = samples(with_tick.map_frames(&frames, 0.5), 4);
    let quiet = samples(without.map_frames(&frames, 0.0), 4);
    assert_eq!(lane_bits(&ticked), lane_bits(&quiet));
}

#[test]
fn device_mute_ramps_the_tick_out() {
    let settings = OutputDeviceSettings {
        unused: UnusedChannels::Silent,
        click_mask: 0b0100,
        ..OutputDeviceSettings::default()
    };
    let (mut mapper, controls) = mapped_device(settings, 4);
    let frames = one_path(0.4, -0.2);
    // Adopt the unmuted device, then point mute at silence.
    let _ = mapper.map_frames(&frames, 0.2);
    controls.set(&OutputDeviceSettings {
        mute: true,
        ..settings
    });
    mapper.begin_block(&controls, RAMP);
    let tick = 0.2;
    let mut lane = Vec::new();
    for _ in 0..(RAMP as usize + 2) {
        lane.push(samples(mapper.map_frames(&frames, tick), 4)[2]);
    }
    assert!(
        lane[0] > 0.0 && lane[0] < tick,
        "not a hard cut: {}",
        lane[0]
    );
    assert!(lane[1] > 0.0 && lane[1] < lane[0], "the ramp keeps moving");
    // The step is 1/288, which is not exact in f32, so the ramp lands on 0
    // on the sample that crosses the target: one sample past the nominal length.
    let settled = lane
        .iter()
        .position(|sample| *sample == 0.0)
        .expect("mute reaches silence");
    assert!(settled <= RAMP as usize + 1, "{settled}");
    assert!(lane[settled..].iter().all(|sample| *sample == 0.0));
}

#[test]
fn the_limiter_does_not_pump_when_the_tick_hits_the_ceiling() {
    let settings = OutputDeviceSettings {
        path_map: PathMapSettings::first(ChannelMap {
            left: 0b0001,
            right: 0,
        }),
        unused: UnusedChannels::Silent,
        click_mask: 0b0100,
        ..OutputDeviceSettings::default()
    };
    let (mut mapper, controls) = mapped_device(settings, 4);
    let out = samples(mapper.map_frames(&one_path(0.5, 0.0), 2.0), 4);
    assert_eq!(out[0].to_bits(), 0.5f32.to_bits());
    assert_eq!(
        out[2].to_bits(),
        crate::audio::safety_ceiling_linear().to_bits()
    );
    mapper.end_block(&controls);
    // The music limiter did not move (channel 0 is the sample it was given).
    // The click's own limiter did, and that reduction is what gets published.
    assert!(controls.reduction_db() > 0.0, "{}", controls.reduction_db());
    let again = samples(mapper.map_frames(&one_path(0.5, 0.0), 0.0), 4);
    assert_eq!(again[0].to_bits(), 0.5f32.to_bits());
}

#[test]
fn a_click_sharing_a_channel_is_limited_and_other_channels_are_not() {
    let settings = OutputDeviceSettings {
        path_map: PathMapSettings::first(ChannelMap {
            left: 0b0011,
            right: 0,
        }),
        unused: UnusedChannels::Silent,
        click_mask: 0b0001,
        ..OutputDeviceSettings::default()
    };
    let (mut mapper, controls) = mapped_device(settings, 2);
    let frames = one_path(0.5, 0.0);
    let hot = samples(mapper.map_frames(&frames, 0.8), 2);
    let ceiling = crate::audio::safety_ceiling_linear();
    assert_eq!(hot[0].to_bits(), ceiling.to_bits());
    assert_eq!(hot[1].to_bits(), 0.5f32.to_bits());
    // 0.05 sits under the ceiling on top of the 0.5.  A hard clip would
    // leave that sum alone.  The limiter still holds the gain from the
    // sample before, so this one is quieter, and channel 2 is not.
    let released = samples(mapper.map_frames(&frames, 0.05), 2);
    assert!(
        released[0] < 0.5,
        "the shared lane is gained down, not flat-topped: {}",
        released[0]
    );
    assert_eq!(released[1].to_bits(), 0.5f32.to_bits());
    mapper.end_block(&controls);
    assert!(controls.reduction_db() > 0.0);
}

#[test]
fn the_click_limiter_releases_smoothly_after_the_click_ends() {
    use crate::audio::metronome::Metronome;
    let settings = OutputDeviceSettings {
        path_map: PathMapSettings::first(ChannelMap {
            left: 0b0001,
            right: 0,
        }),
        unused: UnusedChannels::Silent,
        click_mask: 0b0001,
        ..OutputDeviceSettings::default()
    };
    let (mut mapper, _) = mapped_device(settings, 2);
    // Hot music under the ceiling, and a 0 dB accent on the same lane.
    let music = 0.8;
    let frames = one_path(music, 0.0);
    let mut metronome = Metronome::new();
    metronome.latch(true, 120.0, crate::audio::ClickRhythm::FourFour, 48_000);
    let mut gains = Vec::new();
    let mut end = None;
    // The next beat is 24 000 samples away.
    for index in 0..20_000 {
        let tick = metronome.next();
        if tick == 0.0 && index > 0 && end.is_none() {
            end = Some(index);
        }
        let out = mapper.map_frames(&frames, tick).sample(0);
        gains.push(out / (music + tick));
    }
    let end = end.expect("the click dies away");
    assert!(
        gains[end - 1] < 0.99,
        "the click left a reduction behind: {}",
        gains[end - 1]
    );
    let step = (gains[end] - gains[end - 1]).abs();
    assert!(
        step < 1.0e-3,
        "the lane gain steps by {step} at the click's end"
    );
    let largest = gains[end..]
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).abs())
        .fold(0.0f32, f32::max);
    assert!(largest < 1.0e-3, "the release is a ramp: {largest}");
    assert_eq!(gains[19_999], 1.0, "and it gets back to unity");
}

#[test]
fn a_click_mask_change_while_limiting_does_not_step_a_lane() {
    let mut settings = OutputDeviceSettings {
        path_map: PathMapSettings::first(ChannelMap {
            left: 0b0011,
            right: 0,
        }),
        unused: UnusedChannels::Silent,
        click_mask: 0b0001,
        ..OutputDeviceSettings::default()
    };
    let (mut mapper, controls) = mapped_device(settings, 2);
    let music = 0.8;
    let frames = one_path(music, 0.0);
    // A hot tick on channel 1 leaves a reduction there.
    mapper.map_frames(&frames, 0.8);
    let before = samples(mapper.map_frames(&frames, 0.0), 2);
    assert!(
        before[0] < music,
        "the tick left a reduction: {}",
        before[0]
    );
    assert_eq!(before[1].to_bits(), music.to_bits());

    // The click moves to channel 2 mid-release.
    settings.click_mask = 0b0010;
    controls.set(&settings);
    mapper.begin_block(&controls, RAMP);
    let after = samples(mapper.map_frames(&frames, 0.0), 2);
    // The lane it left keeps releasing instead of jumping back to unity.
    assert!(
        after[0] < music && (after[0] - before[0]).abs() < 1.0e-2,
        "the old lane steps from {} to {}",
        before[0],
        after[0]
    );
    // The limiter gets back to unity on both lanes, then leaves the old
    // lane alone.
    for _ in 0..48_000 {
        mapper.map_frames(&frames, 0.0);
    }
    mapper.begin_block(&controls, RAMP);
    let released = samples(mapper.map_frames(&frames, 0.0), 2);
    assert_eq!(bits(&released), bits(&[music, music]));
    // A tick on the new mask only touches channel 2.
    let ticked = samples(mapper.map_frames(&frames, 0.1), 2);
    assert_eq!(ticked[0].to_bits(), music.to_bits());
    assert!(ticked[1] < music + 0.1 && ticked[1] > music);
}

/// A 4-channel device with the tick alone on channel 3 (bit 2).
fn tick_device() -> OutputDeviceSettings {
    OutputDeviceSettings {
        unused: UnusedChannels::Silent,
        click_mask: 0b0100,
        ..OutputDeviceSettings::default()
    }
}

/// Path 0 with `latency` frames of chain latency, at 48 kHz.
fn latency_timing(latency: u32) -> PathTiming {
    let mut timing = PathTiming::default();
    timing.latency[0] = latency;
    timing
}

/// Maps `frames` silent frames of every path, with a 0.25 tick on each
/// frame in `ticks`, and returns the tick lane (channel 3).
fn tick_lane(mapper: &mut DeviceMapper, ticks: &[usize], frames: usize) -> Vec<f32> {
    use crate::audio::delay::DelayLine;
    let mut lines: PathLines = std::array::from_fn(|_| DelayLine::new(256));
    let mut lane = Vec::new();
    for index in 0..frames {
        for line in &mut lines {
            line.push([0.0, 0.0]);
        }
        let tick = if ticks.contains(&index) { 0.25 } else { 0.0 };
        lane.push(mapper.map_paths(&lines, tick).sample(2));
    }
    lane
}

/// The frames of `lane` that carry anything.
fn ticked(lane: &[f32]) -> Vec<usize> {
    (0..lane.len()).filter(|&i| lane[i] != 0.0).collect()
}

#[test]
fn a_silent_click_line_is_skipped_and_still_aligns_the_next_tick() {
    use crate::audio::delay::DelayLine;
    use crate::audio::path::LiveSlots;
    let controls = DeviceControls::new();
    controls.set(&tick_device());
    let mut mapper = DeviceMapper::new(4);
    // Path 0 is 48 frames late, so a dry path (and the tick) waits 48.
    mapper.begin_paths(&controls, RAMP, LiveSlots::main(), &latency_timing(48), 64);
    let mut lines: PathLines = std::array::from_fn(|_| DelayLine::new(256));
    let mut lane = Vec::new();
    // Two ticks far apart: the line goes quiet (and is skipped) between them.
    for index in 0..400 {
        lines[0].push([0.0, 0.0]);
        let tick = if index == 0 || index == 300 {
            0.25
        } else {
            0.0
        };
        lane.push(mapper.map_paths(&lines, tick).sample(2));
        if index == 100 {
            assert!(
                mapper.click_line_silent(),
                "the line is quiet once the tick has played"
            );
        }
    }
    assert_eq!(ticked(&lane), [48, 348]);
}

#[test]
fn a_click_line_is_silent_after_its_delay_drops_and_settles() {
    use crate::audio::delay::DelayLine;
    use crate::audio::path::LiveSlots;
    let controls = DeviceControls::new();
    controls.set(&tick_device());
    let mut mapper = DeviceMapper::new(4);
    mapper.begin_paths(&controls, RAMP, LiveSlots::main(), &latency_timing(200), 64);
    let mut lines: PathLines = std::array::from_fn(|_| DelayLine::new(256));
    let mut run = |mapper: &mut DeviceMapper, frames: usize, every: Option<usize>| {
        for index in 0..frames {
            lines[0].push([0.0, 0.0]);
            let tick = match every {
                Some(every) if index % every == 0 => 0.25,
                _ => 0.0,
            };
            mapper.map_paths(&lines, tick);
        }
    };
    run(&mut mapper, 300, Some(100));
    // The delay drops from 200 to 10 frames while ticks keep the line busy,
    // so the 15 ms fade runs to its end instead of being snapped.
    mapper.begin_paths(&controls, RAMP, LiveSlots::main(), &latency_timing(10), 64);
    run(&mut mapper, 1_001, Some(100));
    // 50 frames after the last tick: past the 10 frames still read, short
    // of the 200 the tap left.
    run(&mut mapper, 50, None);
    assert!(
        mapper.click_line_silent(),
        "the settled tap still counts the delay it faded away from"
    );
}

#[test]
fn a_block_without_mapping_drops_the_queued_ticks() {
    use crate::audio::path::LiveSlots;
    let controls = DeviceControls::new();
    controls.set(&tick_device());
    let mut mapper = DeviceMapper::new(4);
    let timing = latency_timing(48);
    mapper.begin_paths(&controls, RAMP, LiveSlots::main(), &timing, 64);
    // The tick goes in, but the device stops being mapped before it is due.
    let before = tick_lane(&mut mapper, &[0], 16);
    assert!(before.iter().all(|sample| *sample == 0.0));
    mapper.begin_paths(&controls, RAMP, LiveSlots::main(), &timing, 64);
    mapper.begin_paths(&controls, RAMP, LiveSlots::main(), &timing, 64);
    let lane = tick_lane(&mut mapper, &[], 128);
    assert!(
        lane.iter().all(|sample| *sample == 0.0),
        "a stale tick played: {lane:?}"
    );
}

#[test]
fn a_zero_frame_block_keeps_a_queued_tick() {
    use crate::audio::path::LiveSlots;
    let controls = DeviceControls::new();
    controls.set(&tick_device());
    let mut mapper = DeviceMapper::new(4);
    let timing = latency_timing(48);
    mapper.begin_paths(&controls, RAMP, LiveSlots::main(), &timing, 64);
    let before = tick_lane(&mut mapper, &[0], 16);
    assert!(before.iter().all(|sample| *sample == 0.0));
    // A block of no frames maps nothing, and is no gap.
    mapper.begin_paths(&controls, RAMP, LiveSlots::main(), &timing, 0);
    mapper.begin_paths(&controls, RAMP, LiveSlots::main(), &timing, 64);
    let lane = tick_lane(&mut mapper, &[], 128);
    assert_eq!(ticked(&lane), [32], "the tick plays on time: {lane:?}");
    assert_eq!(lane[32], 0.25);
}

#[test]
fn a_delay_change_while_silent_plays_the_next_tick_once() {
    use crate::audio::path::LiveSlots;
    let controls = DeviceControls::new();
    controls.set(&tick_device());
    let mut mapper = DeviceMapper::new(4);
    mapper.begin_paths(&controls, RAMP, LiveSlots::main(), &latency_timing(0), 64);
    let quiet = tick_lane(&mut mapper, &[], 64);
    assert!(quiet.iter().all(|sample| *sample == 0.0));
    // A plugin on path 0 now reports 48 frames: the tick waits 48 from
    // here.  Nothing is queued, so the first tick must not be read
    // mid-crossfade at both delays.
    mapper.begin_paths(&controls, RAMP, LiveSlots::main(), &latency_timing(48), 64);
    let lane = tick_lane(&mut mapper, &[0], 128);
    assert_eq!(ticked(&lane), [48], "one tick, at the new delay: {lane:?}");
    assert_eq!(lane[48], 0.25);
}

#[test]
fn the_tick_waits_the_device_alignment_delay() {
    use crate::audio::path::LiveSlots;
    // Path 0 is 48 frames late (1 ms at 48 kHz).  With align on, a path
    // with no latency would wait 48 frames on this device, so the tick
    // waits with it and lands with that path's music.
    let run = |latency: u32| {
        let controls = DeviceControls::new();
        controls.set(&tick_device());
        let mut mapper = DeviceMapper::new(4);
        mapper.begin_paths(
            &controls,
            RAMP,
            LiveSlots::main(),
            &latency_timing(latency),
            64,
        );
        tick_lane(&mut mapper, &[0], 128)
    };
    let aligned = run(48);
    assert_eq!(ticked(&aligned), [48], "{aligned:?}");
    assert_eq!(aligned[48], 0.25);
    let unaligned = run(0);
    assert_eq!(unaligned[0], 0.25, "no delay plays the tick at once");
    assert!(unaligned[1..].iter().all(|sample| *sample == 0.0));
}

#[test]
fn a_trim_on_one_path_does_not_move_the_tick() {
    use crate::audio::path::{LiveSlots, PathState};
    let live = LiveSlots::from_states([
        PathState::Active,
        PathState::Active,
        PathState::Inactive,
        PathState::Inactive,
    ]);
    // Path 0 is 48 frames late; path 1 has none and a 1 ms (48 frame)
    // trim, so it waits 96.  A dry path would wait 48: so does the tick.
    let mut settings = tick_device();
    settings.path_map[1].trim_ms = 1.0;
    let controls = DeviceControls::new();
    controls.set(&settings);
    let mut mapper = DeviceMapper::new(4);
    mapper.begin_paths(&controls, RAMP, live, &latency_timing(48), 64);
    let report = controls.alignment().expect("aligned");
    assert_eq!(report.delays[1], Some(96));
    let lane = tick_lane(&mut mapper, &[0], 160);
    assert_eq!(ticked(&lane), [48], "{lane:?}");
}

#[test]
fn with_align_off_a_trim_alone_does_not_delay_the_tick() {
    use crate::audio::path::LiveSlots;
    let run = |trim_ms: f32| {
        let mut settings = OutputDeviceSettings {
            align: false,
            ..tick_device()
        };
        settings.path_map[0].trim_ms = trim_ms;
        let controls = DeviceControls::new();
        controls.set(&settings);
        let mut mapper = DeviceMapper::new(4);
        mapper.begin_paths(&controls, RAMP, LiveSlots::main(), &latency_timing(48), 64);
        ticked(&tick_lane(&mut mapper, &[0], 128))
    };
    // The latency is not aligned and the trim is path 0's own.
    assert_eq!(run(1.0), [0]);
    // A negative trim makes every other path wait instead, so a dry one
    // would wait too: the device's own alignment, not the trim.
    assert_eq!(run(-1.0), [48]);
}

#[test]
fn mono_copy_still_fills_a_channel_the_tick_lands_on() {
    let frames = one_path(0.4, -0.2);
    let tick = 0.25;
    let mono = OutputDeviceSettings {
        unused: UnusedChannels::MonoCopy,
        click_mask: 0b0100,
        ..OutputDeviceSettings::default()
    };
    let silent = OutputDeviceSettings {
        unused: UnusedChannels::Silent,
        ..mono
    };
    let (mut copy, _) = mapped_device(mono, 4);
    let (mut alone, _) = mapped_device(silent, 4);
    let copied = samples(copy.map_frames(&frames, tick), 4);
    let ticked = samples(alone.map_frames(&frames, tick), 4);
    assert_eq!(copied[2], 0.1 + tick);
    assert_eq!(ticked[2], tick);
}

#[test]
fn adopt_settings_from_copies_the_click_mask() {
    let controls = DeviceControls::new();
    controls.set(&OutputDeviceSettings {
        click_mask: 0b1100,
        ..OutputDeviceSettings::default()
    });
    let next = DeviceControls::new();
    next.adopt_settings_from(&controls);
    assert_eq!(next.click_mask(), 0b1100);
}
