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
    mapper.begin_paths(&controls, RAMP, live, &super::PathTiming::default());
    let mut frames = [[0.0f32; 2]; MAX_PATHS];
    frames[0] = [0.5, -0.25];
    frames[1] = [9.0, 9.0];
    frames[2] = [0.25, 0.5];
    let out = samples(mapper.map_frames(&frames), 3);
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
        mapper.begin_paths(&controls, RAMP, live, &super::PathTiming::default());
        samples(mapper.map_frames(&frames), 4)
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
