#![allow(clippy::needless_range_loop)] // Frames of several devices, indexed alike.

//! Latency alignment through the whole mixer: two identical paths, one with
//! a latent effect (a pure delay standing in for a lookahead plugin), on
//! the clock output, the extra outputs of the PipeWire direct path and a generic
//! extra output fed through its ring.

use super::super::eq::EqSettings;
use super::super::nam::NamStack;
use super::super::output_map::{ChannelMap, OutputDeviceSettings, PathMapSettings, UnusedChannels};
use super::super::path::PathMix;
use super::super::shared::{ExtraOutputPlan, Shared};
use super::OutputMixer;

const RATE: u32 = 48_000;
const BLOCK: usize = 256;

/// A low sine: one sample moves it by at most `0.4 * 0.02`.  Two copies
/// summing stay under the safety ceiling, so the alignment stays linear.
fn signal(n: usize) -> f32 {
    (n as f32 * 0.02).sin() * 0.4
}

/// `n` frames back, silence before the start.
fn delayed(n: usize, by: usize) -> f32 {
    n.checked_sub(by).map_or(0.0, signal)
}

/// Two identical paths (EQs off, IN 1/2 on L/R), path 0 with a pure delay
/// of `latency` frames, both on every device; `extra_outputs` stereo extra outputs.
fn aligned_mixer(latency: u32, extra_outputs: usize) -> (OutputMixer, Shared) {
    let shared = Shared::new(RATE);
    shared.set_master(false, [true, true], false);
    // The duplex graph: it plays the extra outputs in-graph.
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
    shared.restore_path(0, &mix, &NamStack::default(), Vec::new());
    shared.restore_path(1, &mix, &NamStack::default(), Vec::new());
    shared.paths[0].edit_chain(|chain| chain.insert_delay_for_test(latency));
    assert_eq!(shared.paths[0].latency_samples(), latency);
    let plan = ExtraOutputPlan {
        count: extra_outputs,
        ..ExtraOutputPlan::default()
    };
    shared.set_extra_outputs(
        &vec![OutputDeviceSettings::default(); extra_outputs],
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
    let devices = 1 + shared.extra_output_count();
    // Sized up front: the write closure runs inside the allocation guard.
    let mut out = vec![vec![[0.0f32; 2]; blocks * BLOCK]; devices];
    // The callback must not allocate: the scratch is sized here, as the
    // backends do before the first block.
    mixer.prepare(BLOCK);
    for block in 0..blocks {
        let left: Vec<f32> = (*clock..*clock + BLOCK).map(signal).collect();
        let right: Vec<f32> = left.iter().map(|value| -value).collect();
        mixer.render_direct_devices(shared, &left, &right, |device, index, mapped| {
            out[device][block * BLOCK + index] = [mapped.sample(0), mapped.sample(1)];
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
    let report = shared.clock_output.alignment().expect("published");
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
    shared.clock_output.set(&OutputDeviceSettings {
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
    let report = shared.clock_output.alignment().unwrap();
    assert_eq!(report.delays, [Some(0), Some(0), None, None]);
    assert_eq!(report.waits_for, None);
}

#[test]
fn a_path_on_its_own_device_keeps_its_own_latency() {
    const LATENCY: usize = 96;
    let (mut mixer, shared) = aligned_mixer(LATENCY as u32, 2);
    // Clock output: path 0 only.  Extra output 0: path 1 only.  Extra output 1:
    // both.
    let only = |slot: usize| {
        let mut settings = OutputDeviceSettings::default();
        settings.path_map[1 - slot] = PathMapSettings::unrouted();
        settings
    };
    shared.clock_output.set(&only(0));
    let plan = ExtraOutputPlan {
        count: 2,
        ..ExtraOutputPlan::default()
    };
    shared.set_extra_outputs(&[only(1), OutputDeviceSettings::default()], &plan, &[]);
    let mut clock = 0;
    let out = run(&mut mixer, &shared, &mut clock, 6);
    for n in 0..out[0].len() {
        assert_eq!(out[0][n][0], delayed(n, LATENCY), "clock {n}");
        assert_eq!(out[1][n][0], signal(n), "extra output 0 {n}");
        assert_eq!(
            out[2][n][0],
            2.0 * delayed(n, LATENCY),
            "extra output 1 {n}"
        );
    }
    assert_eq!(
        shared.clock_output.alignment().unwrap().total,
        LATENCY as u32
    );
    let alone = shared.extra_outputs[0].alignment().unwrap();
    assert_eq!((alone.total, alone.added), (0, 0));
    assert_eq!(alone.delays, [None, Some(0), None, None]);
}

#[test]
fn a_generic_extra_output_is_aligned_on_the_clock_callback() {
    use super::super::shared::ExtraOutputLink;
    use super::super::types::BufferMode;
    const LATENCY: usize = 80;
    let (mut mixer, shared) = aligned_mixer(LATENCY as u32, 0);
    // The ring path: input frames through the clock bridge's ring.
    shared
        .direct
        .store(false, std::sync::atomic::Ordering::Relaxed);
    shared.set_source_rate(RATE);
    shared.set_bridge_buffer(BufferMode::Fixed, 100.0);
    for n in 0..8_000 {
        shared.push_frame(signal(n), -signal(n));
    }
    // A generic extra output that plays path 1 only, so it keeps its own (zero)
    // latency, while the clock output waits for path 0.
    let mut only_path_1 = OutputDeviceSettings::default();
    only_path_1.path_map[0] = PathMapSettings::unrouted();
    let mut plan = ExtraOutputPlan {
        count: 1,
        ..ExtraOutputPlan::default()
    };
    plan.slots[0] = ExtraOutputLink {
        device_id: "test:extra-output".to_owned(),
        ..ExtraOutputLink::default()
    };
    shared.set_extra_outputs(&[only_path_1], &plan, &[(BufferMode::Auto, 10.0)]);
    let bus = shared.extra_output_bus(0);
    bus.set_device_channels(2);
    bus.reset_playback();
    shared
        .extra_output_ring_active(0)
        .store(true, std::sync::atomic::Ordering::Relaxed);

    let mut clock = vec![[0.0f32; 2]; 6 * BLOCK];
    mixer.prepare(BLOCK);
    for block in 0..6 {
        mixer.render(&shared, BLOCK, |index, mapped| {
            clock[block * BLOCK + index] = [mapped.sample(0), mapped.sample(1)];
        });
    }
    let mut extra_output = Vec::new();
    let mut frame = [0.0; crate::audio::output_map::FRAME_LANES];
    while bus.pop_frame(&mut frame) {
        extra_output.push([frame[0], frame[1]]);
    }
    assert_eq!(extra_output.len(), clock.len());
    assert!(extra_output.iter().any(|frame| frame[0].abs() > 0.1));
    // The clock output sums both paths, aligned on path 0's latency; the
    // extra output plays path 1 as it comes.
    for n in LATENCY..clock.len() {
        assert_eq!(clock[n][0], 2.0 * extra_output[n - LATENCY][0], "frame {n}");
    }
    assert_eq!(
        shared.clock_output.alignment().unwrap().total,
        LATENCY as u32
    );
    let alone = shared.extra_outputs[0].alignment().unwrap();
    assert_eq!((alone.total, alone.added), (0, 0));
}

#[test]
fn polarity_cancels_and_a_trim_moves_one_path() {
    const LATENCY: usize = 32;
    let (mut mixer, shared) = aligned_mixer(LATENCY as u32, 1);
    // Clock output: path 1 inverted, so the aligned paths cancel.
    let mut inverted = OutputDeviceSettings::default();
    inverted.path_map[1].invert = true;
    shared.clock_output.set(&inverted);
    // Extra output: path 1 trimmed by +1 ms (48 frames) on top of the alignment.
    let mut trimmed = OutputDeviceSettings::default();
    trimmed.path_map[1].trim_ms = 1.0;
    let plan = ExtraOutputPlan {
        count: 1,
        ..ExtraOutputPlan::default()
    };
    shared.set_extra_outputs(&[trimmed], &plan, &[]);
    let mut clock = 0;
    let out = run(&mut mixer, &shared, &mut clock, 6);
    for n in 0..out[0].len() {
        assert_eq!(out[0][n], [0.0, 0.0], "cancels at {n}");
        let want = delayed(n, LATENCY) + delayed(n, LATENCY + 48);
        assert!((out[1][n][0] - want).abs() < 1e-6, "trimmed at {n}");
    }
    assert_eq!(
        shared.extra_outputs[0].alignment().unwrap().delays[1],
        Some(LATENCY as u32 + 48)
    );

    // A negative trim beyond the latency: path 1 would have to play before
    // its input, so path 0 waits instead (the smallest delay is 0).
    trimmed.path_map[1].trim_ms = -1.0;
    shared.set_extra_outputs(&[trimmed], &plan, &[]);
    let _settle = run(&mut mixer, &shared, &mut clock, 8);
    let report = shared.extra_outputs[0].alignment().unwrap();
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
    shared.clock_output.set(&settings);
    let mut clock = 0;
    let before = run(&mut mixer, &shared, &mut clock, 4);
    let start = clock;
    // The effect reports a new latency while the mixer holds the chain.
    shared.paths[0]
        .slot_link(0)
        .expect("one effect")
        .set_test_latency(640);
    let out = run(&mut mixer, &shared, &mut clock, 12);
    // The sine moves by at most 0.01 per frame; the fade adds the gap
    // between the two read positions spread over 15 ms.
    let step = largest_step(&out[0], 1).max(largest_step(&before[0], 1));
    assert!(step < 0.012, "largest step {step}");
    // Settled on the new delay.
    let last = out[0].len() - 1;
    assert_eq!(out[0][last][1], delayed(start + last, 640));
    assert_eq!(
        shared.clock_output.alignment().unwrap().delays[1],
        Some(640)
    );
}

#[test]
fn a_missed_chain_keeps_the_sum_in_time_and_level() {
    const LATENCY: usize = 64;
    let (mut mixer, shared) = aligned_mixer(LATENCY as u32, 0);
    let mut clock = 0;
    let mut out = run(&mut mixer, &shared, &mut clock, 4).remove(0);
    {
        // The mixer let go of the runtime and the UI holds it for a whole
        // block (applying an edit).
        mixer.park_chains_for_test();
        let _held = shared.paths[0].hold_chain_for_test();
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

/// One path with an oversampled NAM block before a pure delay of
/// `latency`, playing only on the clock output.
fn oversampled_chain_mixer(latency: u32) -> (OutputMixer, Shared) {
    use super::super::nam::NamBlockControls;
    let shared = Shared::new(RATE);
    shared.set_master(false, [true, true], false);
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
    let mut stack = NamStack::default();
    stack.push(NamBlockControls::default());
    shared.restore_path(0, &mix, &stack, Vec::new());
    shared.paths[0].queue_nam(0, super::rt_tests::scaled_model());
    shared.paths[0].edit_chain(|chain| chain.insert_delay_for_test(latency));
    let mut mixer = OutputMixer::new(RATE, 2);
    mixer.prepare(BLOCK);
    (mixer, shared)
}

/// The fallback for a missed chain stands in for the plugin chain alone:
/// its input already comes out of the oversampled block, so it delays by
/// the chain's latency, not the whole path's.
#[test]
fn a_missed_chain_after_an_oversampled_block_keeps_its_timing() {
    const LATENCY: u32 = 64;
    let (mut reference, reference_shared) = oversampled_chain_mixer(LATENCY);
    let (mut mixer, shared) = oversampled_chain_mixer(LATENCY);
    let mut clock = 0;
    let mut reference_clock = 0;
    // The model warms up and fades in.
    let mut want = run_left(
        &mut reference,
        &reference_shared,
        &mut reference_clock,
        16,
        signal,
    );
    let mut out = run_left(&mut mixer, &shared, &mut clock, 16, signal);
    assert!(
        shared.paths[0].path_latency_samples() > LATENCY,
        "the block oversamples: {}",
        shared.paths[0].path_latency_samples()
    );
    reference.park_chains_for_test();
    want.extend(run_left(
        &mut reference,
        &reference_shared,
        &mut reference_clock,
        1,
        signal,
    ));
    {
        mixer.park_chains_for_test();
        let _held = shared.paths[0].hold_chain_for_test();
        out.extend(run_left(&mut mixer, &shared, &mut clock, 1, signal));
    }
    want.extend(run_left(
        &mut reference,
        &reference_shared,
        &mut reference_clock,
        4,
        signal,
    ));
    out.extend(run_left(&mut mixer, &shared, &mut clock, 4, signal));
    let peak = want
        .iter()
        .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
    assert!(peak > 0.1, "the path plays: {peak}");
    for (n, (sample, wanted)) in out.iter().zip(&want).enumerate() {
        assert!(
            (sample - wanted).abs() < 2e-3,
            "frame {n}: {sample} vs {wanted}"
        );
    }
}

#[test]
fn one_path_without_latency_has_no_delay() {
    let (mut mixer, shared) = aligned_mixer(0, 0);
    let mut only = OutputDeviceSettings::default();
    only.path_map[1] = PathMapSettings::unrouted();
    shared.clock_output.set(&only);
    let mut clock = 0;
    let out = run(&mut mixer, &shared, &mut clock, 2);
    for (n, frame) in out[0].iter().enumerate() {
        assert_eq!(*frame, [signal(n), -signal(n)], "frame {n}");
    }
}

#[test]
fn only_devices_that_play_publish_an_alignment() {
    let (mut mixer, shared) = aligned_mixer(64, 1);
    // A generic extra output whose stream does not drain its ring yet.
    shared
        .direct
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let mut clock = 0;
    let _ = run(&mut mixer, &shared, &mut clock, 1);
    assert!(shared.clock_output.alignment().is_some());
    assert!(shared.extra_outputs[0].alignment().is_none());
    // Nor one the plan dropped.
    assert!(shared.extra_outputs[1].alignment().is_none());
    // A stopped bridge clears what it published.
    shared.clock_output.clear_alignment();
    assert!(shared.clock_output.alignment().is_none());
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
fn a_missed_chain_fades_into_the_fallback_without_a_click() {
    let _main = super::super::shared::tests::claim_main_thread();
    let (mut mixer, shared) = aligned_mixer(0, 0);
    // Path 0 runs a plugin whose output is nothing like its input (the
    // fake writes a constant); path 1 plays nowhere.
    shared.paths[0].edit_chain(|chain| {
        chain.restore(Vec::new());
        chain.insert_clap_for_test(crate::audio::clap::tests::fake_plugin_for_test(false));
    });
    let mut only = OutputDeviceSettings::default();
    only.path_map[1] = PathMapSettings::unrouted();
    shared.clock_output.set(&only);
    let mut clock = 0;
    let mut out = run(&mut mixer, &shared, &mut clock, 2).remove(0);
    // The fake plugin writes 1 and 2.  The safety ceiling holds the peak,
    // so the left channel comes out at half the ceiling.
    let plugin = crate::audio::safety_ceiling_linear() * 0.5;
    assert!(
        (out.last().unwrap()[0] - plugin).abs() < 1.0e-5,
        "the plugin runs: {}",
        out.last().unwrap()[0]
    );
    {
        mixer.park_chains_for_test();
        let _held = shared.paths[0].hold_chain_for_test();
        out.extend(run(&mut mixer, &shared, &mut clock, 1).remove(0));
    }
    out.extend(run(&mut mixer, &shared, &mut clock, 2).remove(0));
    // From the limited plugin level to the dry sine and back: smoothed both ways.
    let step = largest_step(&out, 0);
    assert!(step < 0.05, "largest step {step}");
    assert!(
        (out.last().unwrap()[0] - plugin).abs() < 1.0e-5,
        "the plugin is back: {}",
        out.last().unwrap()[0]
    );
    // Parked, so the removal applies here and the fake is destroyed on
    // this thread.
    mixer.park_chains_for_test();
    shared.paths[0].edit_chain(|chain| chain.restore(Vec::new()));
}

#[test]
fn a_removed_path_plays_out_its_alignment_delay_before_it_stops() {
    use super::super::path::PathState;
    const LATENCY: usize = 4800;
    let (mut mixer, shared) = aligned_mixer(LATENCY as u32, 0);
    let mut clock = 0;
    let mut out = run(&mut mixer, &shared, &mut clock, 24).remove(0);
    // Path 1 waits 4800 frames for path 0 on the clock output: its fade
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

/// One path with a pure delay of `latency` and a gate keyed on the dry
/// input in `slot`: fast, deep, no hold.
fn keyed_gate_mixer(
    latency: u32,
    slot: super::super::gate::GateSlot,
) -> (OutputMixer, Shared, super::super::gate::GateSettings) {
    use super::super::gate::{GateKey, GateSettings};
    let shared = Shared::new(RATE);
    shared.set_master(false, [true, true], false);
    shared
        .direct
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let off = EqSettings {
        enabled: false,
        ..EqSettings::default()
    };
    let gate = GateSettings {
        enabled: true,
        slot,
        key: GateKey::Input,
        threshold_db: -40.0,
        range_db: -80.0,
        hold_ms: 0.0,
        release_ms: 1.0,
        attack_ms: 0.1,
        ..GateSettings::default()
    };
    let mix = PathMix {
        eqs: [off, off],
        gate,
        ..PathMix::default()
    };
    shared.restore_path(0, &mix, &NamStack::default(), Vec::new());
    shared.paths[0].edit_chain(|chain| chain.insert_delay_for_test(latency));
    let mut mixer = OutputMixer::new(RATE, 2);
    mixer.prepare(BLOCK);
    (mixer, shared, gate)
}

/// The clock output's left channel for `blocks` blocks of `input`, from
/// frame `*clock`.
fn run_left(
    mixer: &mut OutputMixer,
    shared: &Shared,
    clock: &mut usize,
    blocks: usize,
    input: impl Fn(usize) -> f32,
) -> Vec<f32> {
    let mut out = vec![0.0f32; blocks * BLOCK];
    for block in 0..blocks {
        let left: Vec<f32> = (*clock..*clock + BLOCK).map(&input).collect();
        mixer.render_direct_devices(shared, &left, &left, |device, index, mapped| {
            if device == 0 {
                out[block * BLOCK + index] = mapped.sample(0);
            }
        });
        *clock += BLOCK;
    }
    out
}

/// A POST gate keyed on the dry input hears the note a chain latency late,
/// so it opens and closes with the effects' output, not before it.
#[test]
fn a_gate_keyed_on_the_input_lines_up_with_the_chain() {
    use super::super::gate::GateSlot;
    const LATENCY: usize = 4_800;
    const START: usize = 2_048;
    const BURST: usize = 2_048;
    const BLOCKS: usize = (START + LATENCY + BURST) / BLOCK + 8;
    let (mut mixer, shared, _) = keyed_gate_mixer(LATENCY as u32, GateSlot::Post);
    let input = |n: usize| {
        if (START..START + BURST).contains(&n) {
            0.25
        } else {
            0.0
        }
    };
    let mut clock = 0;
    let out = run_left(&mut mixer, &shared, &mut clock, BLOCKS, input);
    // The delayed burst plays through an open gate, from just after it
    // arrives until it ends.  Keyed on the undelayed input, the gate would
    // have closed again 2 048 frames before it arrived.
    let arrives = START + LATENCY;
    for (n, sample) in out
        .iter()
        .enumerate()
        .take(arrives + BURST)
        .skip(arrives + 64)
    {
        assert!((sample - 0.25).abs() < 0.01, "frame {n}: {sample}");
    }
    // Before it arrives the path is silent, keyed or not.
    assert!(out[..arrives].iter().all(|sample| sample.abs() < 1e-6));
}

/// Moved after the effects mid-note, a keyed gate already holds a chain
/// latency of the input, so it stays open instead of hearing silence.
#[test]
fn a_keyed_gate_moved_after_the_effects_keeps_hearing_the_note() {
    use super::super::gate::GateSlot;
    const LATENCY: usize = 4_800;
    let (mut mixer, shared, gate) = keyed_gate_mixer(LATENCY as u32, GateSlot::Pre);
    let mut clock = 0;
    let _ = run_left(&mut mixer, &shared, &mut clock, 40, |_| 0.25);
    shared.paths[0].set_gate(super::super::gate::GateSettings {
        slot: GateSlot::Post,
        ..gate
    });
    // Past one latency: the old empty line would have closed it by then.
    let out = run_left(&mut mixer, &shared, &mut clock, 24, |_| 0.25);
    for (n, sample) in out.iter().enumerate() {
        assert!((sample - 0.25).abs() < 0.01, "frame {n}: {sample}");
    }
}

/// Paths 0 and 2 send to the return in slot 1 at `send_db`; path 0 has a
/// pure delay of `latency`.  Only the paths in `routed` play on the clock
/// output.
fn return_mixer(latency: u32, send_db: f32, routed: [bool; 3]) -> (OutputMixer, Shared) {
    use super::super::path::PathFeed;
    let shared = Shared::new(RATE);
    shared.set_master(false, [true, true], false);
    shared
        .direct
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let off = EqSettings {
        enabled: false,
        ..EqSettings::default()
    };
    let mut sender = PathMix {
        eqs: [off, off],
        ..PathMix::default()
    };
    sender.sends[1] = send_db;
    let ret = PathMix {
        eqs: [off, off],
        feed: PathFeed::Sends,
        ..PathMix::default()
    };
    shared.restore_path(0, &sender, &NamStack::default(), Vec::new());
    shared.restore_path(1, &ret, &NamStack::default(), Vec::new());
    shared.restore_path(2, &sender, &NamStack::default(), Vec::new());
    if latency > 0 {
        shared.paths[0].edit_chain(|chain| chain.insert_delay_for_test(latency));
    }
    let mut device = OutputDeviceSettings::default();
    for (slot, on) in routed.into_iter().enumerate() {
        device.path_map[slot] = if on {
            PathMapSettings::default()
        } else {
            PathMapSettings::unrouted()
        };
    }
    shared.clock_output.set(&device);
    (OutputMixer::new(RATE, 2), shared)
}

/// Lets the send and level ramps settle.
const SETTLE: usize = 2 * BLOCK;

#[test]
fn a_return_plays_its_senders_lined_up_with_each_other() {
    const LATENCY: usize = 64;
    // Half of each: the return hears one copy of the sine in all.
    let half = -20.0 * 2.0f32.log10();
    let (mut mixer, shared) = return_mixer(LATENCY as u32, half, [false, true, true]);
    let mut clock = 0;
    let out = run(&mut mixer, &shared, &mut clock, 8);
    // Path 2 waits for the return, which waits for path 0: the return's
    // one copy and path 2's, both a latency late.
    for (n, frame) in out[0].iter().enumerate().skip(SETTLE) {
        let want = 2.0 * delayed(n, LATENCY);
        assert!((frame[0] - want).abs() < 1e-4, "frame {n}: {frame:?}");
        assert!((frame[1] + want).abs() < 1e-4, "frame {n}: {frame:?}");
    }
    let report = shared.clock_output.alignment().expect("published");
    assert_eq!(report.delays[1], Some(0), "the return is the slowest");
    assert_eq!(report.delays[2], Some(LATENCY as u32));
}

#[test]
fn a_send_is_taken_after_the_senders_mute() {
    let (mut mixer, shared) = return_mixer(0, 0.0, [false, true, false]);
    shared.paths[2].set_mute(true);
    let mut clock = 0;
    let out = run(&mut mixer, &shared, &mut clock, 4);
    for (n, frame) in out[0].iter().enumerate().skip(SETTLE) {
        assert!((frame[0] - signal(n)).abs() < 1e-6, "frame {n}: {frame:?}");
    }
    shared.paths[0].set_mute(true);
    let start = clock;
    let out = run(&mut mixer, &shared, &mut clock, 4);
    for (n, frame) in out[0].iter().enumerate().skip(SETTLE) {
        assert_eq!(frame[0], 0.0, "frame {}", start + n);
    }
}

#[test]
fn a_return_is_solo_safe_and_hears_only_the_soloed_senders() {
    let (mut mixer, shared) = return_mixer(0, 0.0, [false, true, false]);
    shared.paths[0].set_solo(true);
    assert!(shared.any_solo());
    let mut clock = 0;
    let out = run(&mut mixer, &shared, &mut clock, 4);
    // Path 2 is soloed out, so only path 0's send reaches the return,
    // and the return itself still sounds.
    for (n, frame) in out[0].iter().enumerate().skip(SETTLE) {
        assert!((frame[0] - signal(n)).abs() < 1e-6, "frame {n}: {frame:?}");
    }
    // A return's own solo does not silence the senders.
    shared.paths[0].set_solo(false);
    shared.paths[1].set_solo(true);
    assert!(!shared.any_solo());
}

#[test]
fn a_return_ignores_its_routing_and_sends_nothing() {
    let (mut mixer, shared) = return_mixer(0, 0.0, [false, true, false]);
    shared.paths[2].set_mute(true);
    // Routing that would drop the right side, and a send back to path 0:
    // neither applies to a return.
    shared.paths[1].set_routes([[true, false, false, false], [false, false, false, false]]);
    let mut back = [crate::audio::SEND_OFF_DB; crate::audio::MAX_PATHS];
    back[0] = 0.0;
    shared.paths[1].set_sends(back);
    let mut clock = 0;
    let out = run(&mut mixer, &shared, &mut clock, 4);
    for (n, frame) in out[0].iter().enumerate().skip(SETTLE) {
        assert!((frame[0] - signal(n)).abs() < 1e-6, "frame {n}: {frame:?}");
        assert!((frame[1] + signal(n)).abs() < 1e-6, "frame {n}: {frame:?}");
    }
}

#[test]
fn a_returns_own_chain_adds_to_its_wait() {
    const SENDER: usize = 64;
    const OWN: usize = 32;
    let half = -20.0 * 2.0f32.log10();
    let (mut mixer, shared) = return_mixer(SENDER as u32, half, [false, true, true]);
    shared.paths[1].edit_chain(|chain| chain.insert_delay_for_test(OWN as u32));
    let mut clock = 0;
    let out = run(&mut mixer, &shared, &mut clock, 8);
    for (n, frame) in out[0].iter().enumerate().skip(SETTLE) {
        let want = 2.0 * delayed(n, SENDER + OWN);
        assert!((frame[0] - want).abs() < 1e-4, "frame {n}: {frame:?}");
    }
    let report = shared.clock_output.alignment().expect("published");
    assert_eq!(report.delays[1], Some(0));
    assert_eq!(report.delays[2], Some((SENDER + OWN) as u32));
    // What the UI shows while no bridge runs counts the same.
    assert_eq!(shared.alignment_latencies()[1], (SENDER + OWN) as u32);
    assert_eq!(shared.alignment_latencies()[2], 0);
}

#[test]
fn turning_a_send_off_does_not_move_the_alignment() {
    const LATENCY: u32 = 64;
    let (mut mixer, shared) = return_mixer(LATENCY, 0.0, [false, true, true]);
    let mut clock = 0;
    let _ = run(&mut mixer, &shared, &mut clock, 4);
    let before = shared.clock_output.alignment().expect("published");
    assert_eq!(before.delays[2], Some(LATENCY));
    // The slow sender's send off: the return still waits for it.
    shared.paths[0].set_sends([crate::audio::SEND_OFF_DB; crate::audio::MAX_PATHS]);
    let _ = run(&mut mixer, &shared, &mut clock, 4);
    let after = shared.clock_output.alignment().expect("published");
    assert_eq!(after.delays, before.delays);
}

#[test]
fn a_removed_sender_plays_out_its_send_before_it_stops() {
    const LATENCY: u32 = 4_800;
    // Path 0 plays only through the return; path 2 (muted) is slower, so
    // path 0's send waits a whole latency in its tap.
    let (mut mixer, shared) = return_mixer(0, 0.0, [false, true, false]);
    shared.paths[2].edit_chain(|chain| chain.insert_delay_for_test(LATENCY));
    shared.paths[2].set_mute(true);
    let mut clock = 0;
    let _ = run(&mut mixer, &shared, &mut clock, 24);
    assert!(shared.remove_path(0));
    let out = run(&mut mixer, &shared, &mut clock, 40).remove(0);
    // A sine steps by under 0.01 per frame; the fade out of the tap must
    // not do worse than a few of those.
    let step = largest_step(&out, 0);
    assert!(step < 0.03, "step {step}");
    assert!(out[out.len() - 1][0].abs() < 1e-6, "faded out");
}

#[test]
fn a_path_switches_feed_without_a_click() {
    use super::super::path::PathFeed;
    let half = -20.0 * 2.0f32.log10();
    let (mut mixer, shared) = return_mixer(0, half, [false, true, true]);
    let mut clock = 0;
    // The frames on both sides of each switch, so a step at the block
    // boundary counts too.
    let mut out = run(&mut mixer, &shared, &mut clock, 4).remove(0);
    // Path 2 becomes a second return: it fades out, then hears nothing.
    shared.paths[2].set_feed(PathFeed::Sends);
    out.extend(run(&mut mixer, &shared, &mut clock, 8).remove(0));
    let step = largest_step(&out[SETTLE..], 0);
    assert!(step < 0.03, "step {step}");
    // Back on the input: it fades in again.
    shared.paths[2].set_feed(PathFeed::Input);
    let out = [out, run(&mut mixer, &shared, &mut clock, 8).remove(0)].concat();
    let step = largest_step(&out[SETTLE..], 0);
    assert!(step < 0.03, "step {step}");
    let last = out.len() - 1;
    let n = clock - 1;
    assert!((out[last][0] - 2.0 * signal(n) * 0.5 - signal(n)).abs() < 1e-3);
}

#[test]
fn a_return_plays_on_the_ring_path_too() {
    use super::super::types::BufferMode;
    let (mut mixer, shared) = return_mixer(0, 0.0, [false, true, false]);
    shared
        .direct
        .store(false, std::sync::atomic::Ordering::Relaxed);
    shared.paths[2].set_mute(true);
    shared.set_source_rate(RATE);
    shared.set_bridge_buffer(BufferMode::Fixed, 5.0);
    mixer.prepare(BLOCK);
    let mut last = [0.0f32; 2];
    for _ in 0..16 {
        for _ in 0..BLOCK {
            shared.push_frame(0.25, -0.25);
        }
        mixer.render(&shared, BLOCK, |_, mapped| {
            last = [mapped.sample(0), mapped.sample(1)];
        });
    }
    assert!((last[0] - 0.25).abs() < 1e-4, "{last:?}");
    assert!((last[1] + 0.25).abs() < 1e-4, "{last:?}");
}
