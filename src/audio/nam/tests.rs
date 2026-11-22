//! The NAM stack: sanitising, the published order, loading a capture,
//! bypass, warm-up, the swap fades, the fades of a removal and a reorder,
//! the level match between blocks, and the queue: no model is dropped by
//! the stage, and no command is lost or spun on.

use super::{
    can_oversample, estimate_load, measure_load, prepare_nam_json, prepare_nam_json_at,
    slim_submodel, NamBlockControls, NamFile, NamPosition, NamStack, NamStage, OversampleFilters,
    PreparedNam,
};
use crate::audio::path::PathControls;

/// The tiny WaveNet from the nam-rs crate docs.
const TINY: &str = r#"{
    "version": "0.5.4", "architecture": "WaveNet",
    "config": {
        "layers": [{
            "input_size": 1, "condition_size": 1, "channels": 1, "head_size": 1,
            "kernel_size": 1, "dilations": [1], "activation": "ReLU",
            "gated": false, "head_bias": false
        }],
        "head": null, "head_scale": 1.0
    },
    "weights": [1.0, 2.0, 0.0, 0.0, 1.0, 0.0, 1.0, 1.0]
}"#;

/// [`TINY`] with another copy of itself as its `condition_dsp`.
const TINY_NESTED: &str = r#"{
    "version": "0.5.4", "architecture": "WaveNet",
    "config": {
        "layers": [{
            "input_size": 1, "condition_size": 1, "channels": 1, "head_size": 1,
            "kernel_size": 1, "dilations": [1], "activation": "ReLU",
            "gated": false, "head_bias": false
        }],
        "head": null, "head_scale": 1.0,
        "condition_dsp": {
            "version": "0.5.4", "architecture": "WaveNet",
            "config": {
                "layers": [{
                    "input_size": 1, "condition_size": 1, "channels": 1, "head_size": 1,
                    "kernel_size": 1, "dilations": [1], "activation": "ReLU",
                    "gated": false, "head_bias": false
                }],
                "head": null, "head_scale": 1.0
            },
            "weights": [1.0, 2.0, 0.0, 0.0, 1.0, 0.0, 1.0, 1.0]
        }
    },
    "weights": [1.0, 2.0, 0.0, 0.0, 1.0, 0.0, 1.0, 1.0]
}"#;

/// An LSTM capture with no weights: only its config is read here.
const LSTM: &str = r#"{
    "version": "0.5.4", "architecture": "LSTM",
    "config": { "input_size": 1, "hidden_size": 2, "num_layers": 1 },
    "weights": []
}"#;

fn block(id: u32) -> NamBlockControls {
    NamBlockControls {
        id,
        ..NamBlockControls::default()
    }
}

#[test]
fn sanitized_clamps_a_nan_trim_and_an_out_of_range_slim() {
    let controls = NamBlockControls {
        enabled: true,
        position: NamPosition::Post,
        input_db: f32::NAN,
        output_db: 80.0,
        slim: 4.0,
        level_in_dbu: Some(f32::NAN),
        ..block(3)
    }
    .sanitized();
    assert!(controls.enabled);
    assert_eq!(controls.position, NamPosition::Post);
    assert_eq!(controls.input_db, 0.0);
    assert_eq!(controls.output_db, 24.0);
    assert_eq!(controls.slim, 1.0);
    assert_eq!(controls.level_in_dbu, None);
    assert_eq!(controls.id, 3, "the block keeps its handle");

    let slim = NamBlockControls {
        slim: f32::NAN,
        ..NamBlockControls::default()
    }
    .sanitized();
    assert_eq!(slim.slim, 1.0);
}

#[test]
fn the_stack_keeps_its_two_groups_and_its_ceiling() {
    let mut stack = NamStack::default();
    assert!(stack.push(block(0)));
    assert!(stack.push(NamBlockControls {
        position: NamPosition::Post,
        ..block(1)
    }));
    assert!(stack.push(block(2)), "a second Pre block goes before Post");
    assert_eq!(
        stack.blocks().iter().map(|b| b.id).collect::<Vec<_>>(),
        [0, 2, 1],
        "the Pre group stays in front of the Post group"
    );

    assert!(stack.move_block(1, -1), "2 moves in front of 0");
    assert_eq!(
        stack.blocks().iter().map(|b| b.id).collect::<Vec<_>>(),
        [2, 0, 1]
    );
    assert!(
        !stack.move_block(0, -1),
        "the top of a pre group has no above"
    );
    assert!(
        !stack.move_block(2, 1),
        "the end of a post group has no below"
    );

    assert!(stack.set_position(1, NamPosition::Post));
    assert_eq!(
        stack.blocks().iter().map(|b| b.id).collect::<Vec<_>>(),
        [2, 1, 0]
    );

    let mut full = NamStack::default();
    for id in 0..MAX as u32 {
        assert!(full.push(block(id)));
    }
    assert!(!full.push(block(99)), "sixteen is the most");
    assert_eq!(full.count(), MAX);
}

#[test]
fn sanitized_drops_a_duplicate_id_and_leaves_an_unknown_one() {
    let mut stack = NamStack::default();
    stack.push(block(1));
    stack.push(block(1));
    stack.push(block(2));
    let stack = stack.sanitized();
    assert_eq!(stack.count(), 2);
    assert_eq!(stack.index_of(1), Some(0));
    assert_eq!(stack.index_of(2), Some(1));
    assert_eq!(stack.index_of(3), None);
}

const MAX: usize = super::MAX_NAM_BLOCKS;

fn tiny() -> PreparedNam {
    match prepare_nam_json(TINY, 48_000, 1.0) {
        NamFile::Ready(loaded) => loaded.prepared,
        _ => panic!("the docs model should load at 48 kHz"),
    }
}

/// The docs model built at an oversampling factor.
fn tiny_at(factor: u8) -> PreparedNam {
    match prepare_nam_json_at(TINY, 48_000, 1.0, factor) {
        NamFile::Ready(loaded) => loaded.prepared,
        _ => panic!("the docs model should load at 48 kHz"),
    }
}

fn controls(blocks: &[NamBlockControls]) -> PathControls {
    let controls = PathControls::new(48_000);
    let mut stack = NamStack::default();
    for block in blocks {
        stack.push(*block);
    }
    controls.set_nam(&stack);
    controls
}

/// Runs `blocks` blocks of `frames` frames of a constant `value` through
/// both groups and returns the left channel of the last one.
fn run(
    stage: &mut NamStage,
    controls: &PathControls,
    blocks: usize,
    frames: usize,
    value: f32,
) -> Vec<f32> {
    let all = run_all(stage, controls, blocks, frames, value);
    all[all.len() - frames..].to_vec()
}

/// [`run`], returning the left channel of every block, end to end.
fn run_all(
    stage: &mut NamStage,
    controls: &PathControls,
    blocks: usize,
    frames: usize,
    value: f32,
) -> Vec<f32> {
    stage.reserve(frames);
    let mut all = Vec::new();
    for _ in 0..blocks {
        stage.begin(controls, 48_000, 64.0);
        let mut buffer = vec![[value, value, 0.0, 0.0]; frames];
        stage.process(&mut buffer, frames, NamPosition::Pre);
        stage.process(&mut buffer, frames, NamPosition::Post);
        all.extend(buffer.iter().map(|frame| frame[0]));
    }
    all
}

/// Runs `blocks` blocks of `frames` of a 1 kHz sine at `amplitude`, starting
/// at global sample `from`, through both groups.  Returns the left channel
/// end to end, and the next global sample.  A constant hides a time-base
/// change; a tone does not.
fn run_tone(
    stage: &mut NamStage,
    controls: &PathControls,
    blocks: usize,
    frames: usize,
    amplitude: f32,
    from: usize,
) -> (Vec<f32>, usize) {
    let sample = |index: usize| {
        amplitude * (std::f64::consts::TAU * 1_000.0 * index as f64 / 48_000.0).sin() as f32
    };
    stage.reserve(frames);
    let mut played = Vec::with_capacity(blocks * frames);
    for block in 0..blocks {
        stage.begin(controls, 48_000, 64.0);
        let start = from + block * frames;
        let mut buffer: Vec<[f32; 4]> = (0..frames)
            .map(|index| {
                let value = sample(start + index);
                [value, value, 0.0, 0.0]
            })
            .collect();
        stage.process(&mut buffer, frames, NamPosition::Pre);
        stage.process(&mut buffer, frames, NamPosition::Post);
        played.extend(buffer.iter().map(|frame| frame[0]));
    }
    (played, from + blocks * frames)
}

/// The largest sample-to-sample step in `samples`, and where it is.
fn largest_step(samples: &[f32]) -> (f32, usize) {
    samples
        .windows(2)
        .enumerate()
        .map(|(index, pair)| ((pair[1] - pair[0]).abs(), index))
        .fold(
            (0.0, 0),
            |best, step| if step.0 > best.0 { step } else { best },
        )
}

fn stack_of(blocks: &[NamBlockControls]) -> NamStack {
    let mut stack = NamStack::default();
    for block in blocks {
        stack.push(*block);
    }
    stack
}

/// What one block of the docs model settles on for a constant `value`.
fn settled(value: f32) -> f32 {
    let controls = controls(&[block(0)]);
    let mut stage = NamStage::new(48_000);
    stage.install_for_test(0, tiny());
    *run(&mut stage, &controls, 1, 64, value).last().unwrap()
}

#[test]
fn a_tiny_wavenet_prewarms_and_stays_finite() {
    let controls = controls(&[block(0)]);
    let mut stage = NamStage::new(48_000);
    stage.install_for_test(0, tiny());
    let out = run(&mut stage, &controls, 1, 32, 0.2);
    assert!(out.iter().all(|sample| sample.is_finite()));
    assert!(
        (settled(0.2) - 0.2).abs() > 1.0e-3,
        "the model shapes the signal"
    );
}

#[test]
fn bypass_and_no_model_are_the_dry_signal_undelayed() {
    let mut stage = NamStage::new(48_000);
    stage.reserve(4);
    stage.install_for_test(0, tiny());
    let mut frames = [[0.0; 4]; 4];
    frames[0][0] = 1.0;
    frames[0][1] = -1.0;
    stage.process(&mut frames, 4, NamPosition::Pre);
    assert_eq!(frames[0][0], 1.0, "bypass adds no delay");
    assert_eq!(frames[0][1], -1.0);

    let mut dry = NamStage::new(48_000);
    let out = run(&mut dry, &controls(&[block(0)]), 2, 4, 0.3);
    assert_eq!(out, [0.3; 4]);
}

#[test]
fn two_blocks_run_one_after_the_other() {
    let once = settled(0.2);
    let twice_expected = {
        // The same model fed the one-block result is what the second block
        // must see.
        let controls = controls(&[block(0)]);
        let mut stage = NamStage::new(48_000);
        stage.install_for_test(0, tiny());
        let out = run(&mut stage, &controls, 20, 64, once);
        out[63]
    };
    let mut stage = NamStage::new(48_000);
    stage.install_for_test(0, tiny());
    stage.install_for_test(1, tiny());
    let out = run(&mut stage, &controls(&[block(0), block(1)]), 40, 64, 0.2);
    assert!(
        (out[63] - twice_expected).abs() < 1.0e-4,
        "two blocks in series: {} against {twice_expected}",
        out[63]
    );
    assert!(
        (once - twice_expected).abs() > 1.0e-4,
        "the second block changes the result"
    );
}

#[test]
fn a_block_only_runs_in_its_own_position() {
    let stack = [
        block(0),
        NamBlockControls {
            position: NamPosition::Post,
            ..block(1)
        },
    ];
    let controls = controls(&stack);
    let mut stage = NamStage::new(48_000);
    stage.install_for_test(0, tiny());
    stage.install_for_test(1, tiny());
    stage.reserve(8);
    stage.begin(&controls, 48_000, 64.0);
    let mut frames = vec![[0.2, 0.2, 0.0, 0.0]; 8];
    stage.process(&mut frames, 8, NamPosition::Pre);
    let pre = frames[7][0];
    stage.process(&mut frames, 8, NamPosition::Post);
    let post = frames[7][0];
    assert!((pre - 0.2).abs() > 1.0e-3, "the Pre block shapes it");
    assert!(
        (post - pre).abs() > 1.0e-3,
        "the Post block shapes it again"
    );
}

#[test]
fn leaving_bypass_warms_the_model_before_it_is_heard() {
    let controls = controls(&[NamBlockControls {
        enabled: false,
        ..block(0)
    }]);
    let mut stage = NamStage::new(48_000);
    stage.install_for_test(0, tiny());
    assert_eq!(run(&mut stage, &controls, 2, 64, 0.2), [0.2; 64]);
    let mut stack = NamStack::default();
    stack.push(block(0));
    controls.set_nam(&stack);
    let first = run(&mut stage, &controls, 1, 1, 0.2);
    assert_eq!(first, [0.2], "the first samples are still dry");
    // After the warm-up and the 10 ms fade the model is all that plays.
    let late = run(&mut stage, &controls, 20, 64, 0.2);
    assert!((late[63] - settled(0.2)).abs() < 1.0e-5);
}

#[test]
fn a_queued_model_fades_in_and_a_clear_fades_back_to_dry() {
    let controls = controls(&[block(0)]);
    let mut stage = NamStage::new(48_000);
    assert_eq!(run(&mut stage, &controls, 1, 64, 0.2), [0.2; 64]);
    controls.queue_nam(0, tiny());
    let first = run(&mut stage, &controls, 1, 64, 0.2);
    assert_eq!(first[0], 0.2, "no jump when the model arrives");
    let target = settled(0.2);
    for pair in first.windows(2) {
        let step = (pair[1] - pair[0]).abs();
        assert!(step <= (target - 0.2).abs() / 100.0, "fades, step {step}");
    }
    // The swap fades over 35 ms: 40 blocks of 64 frames is past it.
    let late = run(&mut stage, &controls, 40, 64, 0.2);
    assert!((late[63] - target).abs() < 1.0e-5);

    controls.clear_nam(0);
    let cleared = run(&mut stage, &controls, 1, 64, 0.2);
    assert!((cleared[0] - target).abs() < 1.0e-3, "no jump on clear");
    let late = run(&mut stage, &controls, 40, 64, 0.2);
    assert_eq!(late[63], 0.2);
}

#[test]
fn a_model_keeps_its_block_when_the_stack_is_reordered() {
    let first = [block(0), block(1)];
    let controls = controls(&first);
    let mut stage = NamStage::new(48_000);
    stage.install_for_test(0, tiny());
    stage.install_for_test(1, tiny());
    run(&mut stage, &controls, 2, 8, 0.2);
    assert!(stage.has_model(0) && stage.has_model(1));

    let mut reordered = NamStack::default();
    reordered.push(block(1));
    reordered.push(block(0));
    controls.set_nam(&reordered);
    run(&mut stage, &controls, 1, 8, 0.2);
    assert!(
        stage.has_model(0) && stage.has_model(1),
        "a reorder moves no model"
    );
}

#[test]
fn a_removed_block_parks_its_model() {
    let controls = controls(&[block(0), block(1)]);
    let mut stage = NamStage::new(48_000);
    stage.install_for_test(0, tiny());
    stage.install_for_test(1, tiny());
    run(&mut stage, &controls, 2, 8, 0.2);

    let mut smaller = NamStack::default();
    smaller.push(block(0));
    controls.set_nam(&smaller);
    run(&mut stage, &controls, 1, 8, 0.2);
    assert!(stage.has_model(1), "the block still fades out");
    // The 10 ms fade to dry is 480 samples.
    run(&mut stage, &controls, 10, 64, 0.2);
    assert!(stage.has_model(0));
    assert!(!stage.has_model(1), "the block is gone");
    assert_eq!(
        controls.nam_park().len(),
        1,
        "its model waits for the UI thread"
    );
}

#[test]
fn auto_level_matches_the_block_before_it() {
    let pedal = NamBlockControls {
        level_in_dbu: Some(-18.0),
        level_out_dbu: Some(-6.0),
        ..block(0)
    };
    let amp = NamBlockControls {
        level_in_dbu: Some(-20.0),
        level_out_dbu: Some(4.0),
        ..block(1)
    };
    let mut stack = NamStack::default();
    stack.push(pedal);
    stack.push(amp);
    let matches = stack.level_matches(|_| true);
    assert_eq!(matches[0], 0.0, "the first block has nothing before it");
    assert_eq!(
        matches[1], 14.0,
        "the pedal's 0 dBFS is -6 dBu, the amp's is -20 dBu: 14 dB up"
    );

    let manual = NamBlockControls {
        auto_level: false,
        ..amp
    };
    let mut stack = NamStack::default();
    stack.push(pedal);
    stack.push(manual);
    assert_eq!(stack.level_matches(|_| true)[1], 0.0);

    let uncalibrated = NamBlockControls {
        level_in_dbu: None,
        ..amp
    };
    let mut stack = NamStack::default();
    stack.push(pedal);
    stack.push(uncalibrated);
    assert_eq!(stack.level_matches(|_| true)[1], 0.0);
}

#[test]
fn a_wild_level_match_is_clamped() {
    let mut stack = NamStack::default();
    stack.push(NamBlockControls {
        level_in_dbu: Some(0.0),
        level_out_dbu: Some(200.0),
        ..block(0)
    });
    stack.push(NamBlockControls {
        level_in_dbu: Some(-200.0),
        level_out_dbu: Some(0.0),
        ..block(1)
    });
    assert_eq!(stack.level_matches(|_| true)[1], super::NAM_MAX_MATCH_DB);

    let mut stack = NamStack::default();
    stack.push(NamBlockControls {
        level_in_dbu: Some(0.0),
        level_out_dbu: Some(-200.0),
        ..block(0)
    });
    stack.push(NamBlockControls {
        level_in_dbu: Some(200.0),
        level_out_dbu: Some(0.0),
        ..block(1)
    });
    assert_eq!(stack.level_matches(|_| true)[1], -super::NAM_MAX_MATCH_DB);
}

#[test]
fn a_block_that_does_not_run_is_skipped_by_the_level_match() {
    let pedal = NamBlockControls {
        level_in_dbu: Some(-18.0),
        level_out_dbu: Some(-6.0),
        ..block(0)
    };
    let boost = NamBlockControls {
        level_in_dbu: Some(-10.0),
        level_out_dbu: Some(0.0),
        ..block(1)
    };
    let amp = NamBlockControls {
        level_in_dbu: Some(-20.0),
        level_out_dbu: Some(4.0),
        ..block(2)
    };
    let bypassed = stack_of(&[
        pedal,
        NamBlockControls {
            enabled: false,
            ..boost
        },
        amp,
    ]);
    let matches = bypassed.level_matches(|_| true);
    assert_eq!(matches[1], 0.0, "a bypassed block gets no match");
    assert_eq!(matches[2], 14.0, "the amp matches the pedal, not the boost");

    let running = stack_of(&[pedal, boost, amp]);
    let matches = running.level_matches(|_| true);
    assert_eq!(matches[1], 4.0);
    assert_eq!(matches[2], 20.0, "the amp matches the boost");

    // The stage knows which blocks have a model; one without passes dry.
    let matches = running.level_matches(|id| id != 1);
    assert_eq!(matches[1], 0.0);
    assert_eq!(matches[2], 14.0, "an empty block is skipped too");
}

#[test]
fn removing_a_live_block_fades_it_to_dry_at_its_place() {
    let controls = controls(&[block(0)]);
    let mut stage = NamStage::new(48_000);
    stage.install_for_test(0, tiny());
    let target = settled(0.2);
    let late = run(&mut stage, &controls, 4, 64, 0.2);
    assert!((late[63] - target).abs() < 1.0e-5);

    controls.set_nam(&NamStack::default());
    let mut played = vec![late[63]];
    played.extend(run_all(&mut stage, &controls, 12, 64, 0.2));
    let (step, at) = largest_step(&played);
    assert!(
        step <= (target - 0.2).abs() / 100.0,
        "removing the block jumps {step} at sample {at}"
    );
    assert_eq!(*played.last().unwrap(), 0.2, "it ends dry");
    assert!(!stage.has_model(0), "and then the model is parked");
    assert_eq!(controls.nam_park().len(), 1);
}

#[test]
fn removing_a_block_between_two_fades_only_that_block() {
    let first = NamBlockControls {
        input_db: 6.0,
        ..block(0)
    };
    let controls = controls(&[first, block(1), block(2)]);
    let mut stage = NamStage::new(48_000);
    for id in 0..3 {
        stage.install_for_test(id, tiny());
    }
    let before = run(&mut stage, &controls, 4, 64, 0.2)[63];

    controls.set_nam(&stack_of(&[first, block(2)]));
    let mut played = vec![before];
    played.extend(run_all(&mut stage, &controls, 12, 64, 0.2));
    let after = *played.last().unwrap();
    assert!((after - before).abs() > 0.05, "the middle block mattered");
    let (step, at) = largest_step(&played);
    assert!(
        step <= (after - before).abs() / 100.0,
        "removing the middle block jumps {step} at sample {at}"
    );
}

/// Plays a stack, publishes `next`, and returns the largest step across the
/// change and the levels before and after it.
fn change_order(first: &[NamBlockControls], next: &[NamBlockControls]) -> (f32, f32, f32) {
    let controls = controls(first);
    let mut stage = NamStage::new(48_000);
    for block in first {
        stage.install_for_test(block.id, tiny());
    }
    let before = run(&mut stage, &controls, 4, 64, 0.2)[63];
    controls.set_nam(&stack_of(next));
    let mut played = vec![before];
    played.extend(run_all(&mut stage, &controls, 12, 64, 0.2));
    let after = *played.last().unwrap();
    for id in next.iter().map(|block| block.id) {
        assert!(stage.has_model(id), "a reorder moves no model");
    }
    (largest_step(&played).0, before, after)
}

#[test]
fn a_reorder_dips_instead_of_jumping() {
    let hot = NamBlockControls {
        input_db: 6.0,
        ..block(0)
    };
    let (step, before, after) = change_order(&[hot, block(1)], &[block(1), hot]);
    assert!((after - before).abs() > 0.05, "the order matters here");
    let bound = before.abs().max(after.abs()) / 100.0;
    assert!(step <= bound, "the reorder steps {step}, more than {bound}");
}

#[test]
fn a_move_after_the_effects_dips_instead_of_jumping() {
    let hot = NamBlockControls {
        input_db: 6.0,
        ..block(0)
    };
    let moved = NamBlockControls {
        position: NamPosition::Post,
        ..hot
    };
    let (step, before, after) = change_order(&[hot, block(1)], &[block(1), moved]);
    assert!((after - before).abs() > 0.05, "the order matters here");
    let bound = before.abs().max(after.abs()) / 100.0;
    assert!(step <= bound, "the move steps {step}, more than {bound}");
}

#[test]
fn reordering_bypassed_blocks_switches_at_once() {
    let off = |id| NamBlockControls {
        enabled: false,
        ..block(id)
    };
    let controls = controls(&[off(0), off(1)]);
    let mut stage = NamStage::new(48_000);
    stage.install_for_test(0, tiny());
    stage.install_for_test(1, tiny());
    run(&mut stage, &controls, 2, 64, 0.2);
    controls.set_nam(&stack_of(&[off(1), off(0)]));
    assert_eq!(
        run(&mut stage, &controls, 1, 64, 0.2),
        [0.2; 64],
        "nothing is heard, so nothing dips"
    );
}

#[test]
fn a_full_queue_drops_a_departed_block_first_and_keeps_the_new_one() {
    use super::{NamCommand, NamPending, PENDING_SLOTS};
    let live_ids: Vec<u32> = (100..100 + MAX as u32).collect();
    let mut live = NamStack::default();
    for id in &live_ids {
        live.push(block(*id));
    }
    let mut pending = NamPending::default();
    // One departed block first, then live ones until the queue is full.
    assert!(pending.queue(7, NamCommand::Clear, &live).is_none());
    for id in 1000..1000 + PENDING_SLOTS as u32 - 1 {
        let id = if id - 1000 < MAX as u32 {
            live_ids[(id - 1000) as usize]
        } else {
            id
        };
        assert!(pending.queue(id, NamCommand::Clear, &live).is_none());
    }
    assert_eq!(pending.len(), PENDING_SLOTS);
    assert!(pending.queue(5000, NamCommand::Clear, &live).is_some());
    assert_eq!(pending.len(), PENDING_SLOTS);
    let ids: Vec<u32> = (0..pending.len())
        .filter_map(|index| pending.at(index).map(|(id, _)| id))
        .collect();
    assert!(!ids.contains(&7), "the departed block lost its place");
    assert!(ids.contains(&5000), "the new command is queued");
    assert!(
        live_ids.iter().all(|id| ids.contains(id)),
        "live blocks keep theirs"
    );
}

#[test]
fn the_width_picks_the_first_submodel_above_it() {
    let steps = [0.33, 0.66, 1.0];
    assert_eq!(slim_submodel(&steps, 0.0), 0);
    assert_eq!(slim_submodel(&steps, 0.5), 1);
    assert_eq!(slim_submodel(&steps, 0.99), 2);
    assert_eq!(slim_submodel(&steps, 1.0), 2);
    assert_eq!(slim_submodel(&[], 0.5), 0);
}

#[test]
fn a_mismatched_sample_rate_does_not_install() {
    let json = format!(
        "{}, \"sample_rate\": 44100.0}}",
        TINY.trim().trim_end_matches('}')
    );
    match prepare_nam_json(&json, 48_000, 1.0) {
        NamFile::RateMismatch { file_rate, .. } => assert_eq!(file_rate, 44_100),
        NamFile::Ready(_) => panic!("a 44.1 kHz capture must not install at 48 kHz"),
        NamFile::Failed(error) => panic!("parse failed: {error}"),
    }
}

#[test]
fn a_measured_model_is_timed_and_left_ready() {
    let loaded = match prepare_nam_json(TINY, 48_000, 1.0) {
        NamFile::Ready(loaded) => loaded,
        _ => panic!("the docs model should load"),
    };
    let mut model = loaded.prepared.model;
    let load = measure_load(&mut model, 48_000, 64, 1);
    assert!(load.mean.is_finite() && load.mean > 0.0);
    assert!(load.peak >= load.mean);
    assert!(load.share() < 1.0, "a tiny model is not a whole block");
    // Measuring leaves the model reset and ready to go live: it must still
    // produce the model's output, not a stale measurement buffer.
    let mut buffer = vec![0.2; 16];
    model.process_buffer(&mut buffer);
    assert!(
        buffer
            .iter()
            .all(|sample| sample.is_finite() && *sample > 0.0),
        "the model still runs after it is measured"
    );
}

#[test]
fn one_slow_buffer_does_not_set_the_measured_peak() {
    // 199 buffers of 1 ms and one preempted one of 50 ms, in a 10 ms block.
    let mut times = [0.001; 200];
    times[17] = 0.05;
    let load = super::load_from_times(&mut times, 0.01);
    // The outlier alone would read 500 %, and would lift a plain mean to
    // 12.45 %: it is left out of both figures.
    assert!((load.peak - 0.1).abs() < 1.0e-6, "peak {}", load.peak);
    assert!((load.mean - 0.1).abs() < 1.0e-6, "mean {}", load.mean);
}

#[test]
fn a_slow_tail_sets_the_measured_peak() {
    // Every tenth buffer is slow: that is the model, not a preemption.
    let mut times: Vec<f64> = (0..200)
        .map(|index| if index % 10 == 0 { 0.004 } else { 0.001 })
        .collect();
    let load = super::load_from_times(&mut times, 0.01);
    assert!((load.peak - 0.4).abs() < 1.0e-6, "peak {}", load.peak);
    // The 190 fastest: 180 of 1 ms and 10 of 4 ms.
    let mean = (180.0 * 0.001 + 10.0 * 0.004) / 190.0 / 0.01;
    assert!(
        (load.mean - mean as f32).abs() < 1.0e-6,
        "mean {}",
        load.mean
    );
    assert!(load.peak >= load.mean);
}

#[test]
fn the_estimate_follows_the_rate_and_not_the_buffer() {
    let at_48k = estimate_load("WaveNet", 48_000.0).share();
    let at_96k = estimate_load("WaveNet", 96_000.0).share();
    assert!((at_48k - 0.05).abs() < 1.0e-6);
    assert!((at_96k - 0.10).abs() < 1.0e-6);
    assert!(estimate_load("LSTM", 48_000.0).share() < at_48k);
}

#[test]
fn a_factor_2_build_reports_its_factor_and_a_share() {
    // The loader builds the scaled model and leaves a measurement for the
    // sheet.  Its share of the block is about twice the unscaled one's: the
    // exact figure is timed, so this only checks it was measured and that
    // the build really is the factor-2 one.
    let loaded = match super::prepare_parsed(
        nam_rs::NamModel::from_json_str(TINY).expect("the docs model parses"),
        "Capture",
        48_000,
        1.0,
        Some(4096),
        2,
    ) {
        NamFile::Ready(loaded) => loaded,
        _ => panic!("the docs model should load"),
    };
    assert_eq!(loaded.prepared.factor, 2);
    assert!(loaded.oversample, "the docs model scales");
    let load = loaded.load.expect("a load with frames is measured");
    assert!(load.mean > 0.0 && load.peak >= load.mean);
}

#[test]
fn scaling_doubles_every_dilation_and_keeps_the_weights() {
    let parsed = nam_rs::NamModel::from_json_str(TINY_NESTED).expect("the nested capture parses");
    assert!(can_oversample(&parsed));
    let scaled = super::oversampled(&parsed);
    assert_eq!(
        scaled.weights.len(),
        parsed.weights.len(),
        "a dilation scale leaves the weight blob alone"
    );
    let nam_rs::ModelConfig::WaveNet(config) = &scaled.config else {
        panic!("a WaveNet stays one");
    };
    assert_eq!(config.layers[0].dilations, [2]);
    let nested = config.condition_dsp.as_deref().expect("the nested model");
    let nam_rs::ModelConfig::WaveNet(nested) = &nested.config else {
        panic!("the nested model is a WaveNet");
    };
    assert_eq!(nested.layers[0].dilations, [2], "the nested dsp scales too");
    nam_rs::Model::from_nam(&scaled).expect("the scaled capture still builds");

    // Every entry of a longer vector doubles, not just the first.
    let mut longer = parsed;
    let nam_rs::ModelConfig::WaveNet(config) = &mut longer.config else {
        panic!("a WaveNet stays one");
    };
    config.layers[0].dilations = vec![1, 2, 3];
    let scaled = super::oversampled(&longer);
    let nam_rs::ModelConfig::WaveNet(config) = &scaled.config else {
        panic!("a WaveNet stays one");
    };
    assert_eq!(config.layers[0].dilations, [2, 4, 6]);
    assert_eq!(scaled.weights.len(), longer.weights.len());
}

#[test]
fn can_oversample_follows_the_model_shape() {
    let wavenet = nam_rs::NamModel::from_json_str(TINY).expect("the docs model parses");
    assert!(can_oversample(&wavenet), "a plain WaveNet scales");

    let lstm = nam_rs::NamModel::from_json_str(LSTM).expect("the lstm parses");
    assert!(!can_oversample(&lstm), "an LSTM recurs in samples");

    // A layer head with a kernel above 1 is a dilation-1 conv inside
    // nam-rs: scaling the layers alone would leave it at half its time.
    let mut headed = wavenet.clone();
    let nam_rs::ModelConfig::WaveNet(config) = &mut headed.config else {
        panic!("a WaveNet stays one");
    };
    config.layers[0].head_kernel_size = 2;
    assert!(!can_oversample(&headed), "a conv head has no dilation");

    // The post-stack head is the same kind of conv.
    let mut stacked = wavenet;
    let nam_rs::ModelConfig::WaveNet(config) = &mut stacked.config else {
        panic!("a WaveNet stays one");
    };
    config.post_stack_head = Some(nam_rs::PostStackHeadConfig {
        channels: 1,
        out_channels: 1,
        kernel_sizes: vec![2],
        activation: nam_rs::ActivationSpec::Named {
            name: "ReLU".to_owned(),
            negative_slope: None,
        },
    });
    assert!(
        !can_oversample(&stacked),
        "a post-stack head has no dilation"
    );
}

/// One pass through the 2x pair with fresh history.
fn through_the_pair(filters: &OversampleFilters, input: &[f32]) -> Vec<f32> {
    let frames = input.len();
    let mut wide = vec![0.0; 2 * frames];
    let mut output = vec![0.0; frames];
    let mut up = vec![0.0; filters.taps - 1];
    let mut down = vec![0.0; 2 * (filters.taps - 1)];
    filters.upsample(input, &mut wide, &mut up);
    assert!(wide.iter().all(|sample| sample.is_finite()));
    filters.downsample(&wide, &mut output, &mut down);
    output
}

#[test]
fn the_pair_takes_n_in_two_n_through_and_n_out() {
    let filters = OversampleFilters::new(48_000);
    let frames = 512;
    let input: Vec<f32> = (0..frames)
        .map(|index| (index as f32 * 0.01).sin())
        .collect();
    let mut wide = vec![0.0; 2 * frames];
    let mut output = vec![0.0; frames];
    let mut up = vec![0.0; filters.taps - 1];
    let mut down = vec![0.0; 2 * (filters.taps - 1)];
    filters.upsample(&input, &mut wide, &mut up);
    filters.downsample(&wide, &mut output, &mut down);
    assert_eq!(up.len(), filters.taps - 1);
    assert_eq!(down.len(), 2 * (filters.taps - 1));
    assert_eq!(wide.len(), 2 * frames, "N frames become 2N");
    assert_eq!(output.len(), frames, "and come back to N");
    // Length is odd and 1 mod 4, so the pair's delay is a whole number of
    // output samples: (length - 1) / 2.
    assert_eq!(filters.delay, filters.taps - 1);
}

#[test]
fn a_tone_comes_back_delayed_at_unity() {
    let filters = OversampleFilters::new(48_000);
    let delay = filters.delay;
    let frames = 4096;
    let rate = 48_000.0f64;
    let input: Vec<f32> = (0..frames)
        .map(|index| (std::f64::consts::TAU * 1_000.0 * index as f64 / rate).sin() as f32)
        .collect();
    let output = through_the_pair(&filters, &input);
    let mut worst = 0.0f32;
    for index in (4 * delay + 128)..frames {
        worst = worst.max((output[index] - input[index - delay]).abs());
    }
    assert!(
        worst < 3.2e-5,
        "1 kHz at unity and delayed by {delay}: worst {worst:e} (about -90 dB)"
    );
}

#[test]
fn an_ultrasonic_tone_is_gone_before_it_folds() {
    // 30 kHz at 96 kHz would fold to 18 kHz if the decimator let it pass.
    let filters = OversampleFilters::new(48_000);
    let frames = 2048;
    let input: Vec<f32> = (0..frames)
        .map(|index| (std::f64::consts::TAU * 30_000.0 * index as f64 / 96_000.0).sin() as f32)
        .collect();
    let mut output = vec![0.0; frames / 2];
    let mut down = vec![0.0; 2 * (filters.taps - 1)];
    filters.downsample(&input, &mut output, &mut down);
    let peak = output
        .iter()
        .skip(4 * filters.delay)
        .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
    assert!(
        peak <= 1.1e-4,
        "30 kHz after the downsampler: {peak:e} (about -80 dB)"
    );
}

#[test]
fn two_oversampled_blocks_add_both_delays_and_delay_a_bypass() {
    let delay = OversampleFilters::new(48_000).delay;
    let controls = controls(&[
        NamBlockControls {
            enabled: false,
            ..block(0)
        },
        NamBlockControls {
            enabled: false,
            ..block(1)
        },
    ]);
    let mut stage = NamStage::new(48_000);
    stage.install_for_test(0, tiny_at(2));
    stage.install_for_test(1, tiny_at(2));
    assert_eq!(stage.filters.delay, delay);
    assert_eq!(stage.latency_samples(), 2 * delay as u32, "one block each");

    let (amplitude, frames) = (0.3f32, 128);
    let (played, _) = run_tone(&mut stage, &controls, 32, frames, amplitude, 0);
    let total = 2 * delay;
    let tone = |index: usize| {
        amplitude * (std::f64::consts::TAU * 1_000.0 * index as f64 / 48_000.0).sin() as f32
    };
    // Past the two 10 ms time-base ramps and the cold history: the rest is
    // the input delayed by both pairs, not the undelayed input.
    let settled = 8 * frames;
    for (index, sample) in played.iter().enumerate().skip(settled) {
        let want = tone(index - total);
        assert!(
            (sample - want).abs() < 1.0e-5,
            "sample {index}: bypass is the input delayed by {total}, got {sample}"
        );
    }
    // The time base moves over the fade, so no sample jumps by the delay.
    let natural = amplitude * (std::f64::consts::TAU * 1_000.0 / 48_000.0) as f32;
    let (step, at) = largest_step(&played);
    assert!(
        step <= natural * 2.0,
        "the time base steps {step} at sample {at}, the tone's own is {natural}"
    );
}

#[test]
fn a_swap_to_oversampling_has_no_step_at_its_boundary() {
    let controls = controls(&[block(0)]);
    let mut stage = NamStage::new(48_000);
    stage.install_for_test(0, tiny());
    let (amplitude, frames) = (0.3f32, 64);
    let (before, at) = run_tone(&mut stage, &controls, 40, frames, amplitude, 0);
    assert_eq!(stage.latency_samples(), 0);
    controls.queue_nam(0, tiny_at(2));
    let (after, _) = run_tone(&mut stage, &controls, 90, frames, amplitude, at);
    assert_eq!(
        stage.latency_samples(),
        stage.filters.delay as u32,
        "the scaled block reports its delay"
    );
    let mut played = before;
    played.extend(after);
    // The tone's own biggest step, taken well past the transition: a jump
    // of the 60-sample delay would be far larger than this.
    let steady = largest_step(&played[60 * frames..]).0;
    let (step, at) = largest_step(&played);
    assert!(
        step <= steady * 2.0,
        "the fade steps {step} at sample {at}, the steady tone {steady}"
    );
}

#[test]
fn a_fade_back_to_one_keeps_the_delay_until_it_settles() {
    let controls = controls(&[block(0)]);
    let mut stage = NamStage::new(48_000);
    stage.install_for_test(0, tiny_at(2));
    let (amplitude, frames) = (0.3f32, 64);
    let (before, at) = run_tone(&mut stage, &controls, 40, frames, amplitude, 0);
    let delay = stage.filters.delay as u32;
    assert_eq!(stage.latency_samples(), delay);
    controls.queue_nam(0, tiny_at(1));
    // The swap fades over 35 ms (1680 samples at 48 kHz): eight blocks of
    // 64 are well inside it, and the block must still delay both sides.
    let mut played = before;
    let (fading, at) = run_tone(&mut stage, &controls, 8, frames, amplitude, at);
    played.extend(fading);
    assert_eq!(
        stage.latency_samples(),
        delay,
        "the delay stays for the whole fade back"
    );
    // Past the fade the unscaled model is alone and the delay is gone.
    let (after, _) = run_tone(&mut stage, &controls, 40, frames, amplitude, at);
    played.extend(after);
    assert_eq!(stage.latency_samples(), 0);
    let steady = largest_step(&played[80 * frames..]).0;
    let (step, at) = largest_step(&played);
    assert!(
        step <= steady * 2.0,
        "the fade back steps {step} at sample {at}, the steady tone {steady}"
    );
}

#[test]
fn a_settled_bypass_keeps_its_dry_history() {
    // A bypassed capture still counts in its group, so its dry side can start
    // delaying without a model fade to hide a cold line.  The line must have
    // been running all along.
    let controls = controls(&[NamBlockControls {
        enabled: false,
        ..block(0)
    }]);
    let mut stage = NamStage::new(48_000);
    stage.install_for_test(0, tiny());
    let _ = run_tone(&mut stage, &controls, 8, 128, 0.3, 0);
    let slot = stage.slot(0).expect("the block has a slot");
    assert!(slot.dry_l.len > 0, "the block delays at this rate");
    assert!(
        slot.dry_l.samples.iter().any(|sample| *sample != 0.0),
        "a settled bypass still feeds the dry line"
    );
}

#[test]
fn one_block_at_unity_is_the_model_run_by_hand() {
    // The null path of the migration: one block, unity trims, no level
    // match, enabled — the old single stage's math.  It must be the model
    // itself, sample for sample, within the one rounding of the wet mix
    // (`dry + (wet - dry) * 1.0`, see `NamSlot::process_block`).  One block
    // is a group of one: the oversampler is present but never engaged, and
    // the block reports no added latency.
    let controls = controls(&[block(0)]);
    let mut stage = NamStage::new(48_000);
    stage.install_for_test(0, tiny());
    assert_eq!(stage.latency_samples(), 0, "one block adds no latency");
    let reference_model = match prepare_nam_json(TINY, 48_000, 1.0) {
        NamFile::Ready(loaded) => loaded.prepared.model,
        _ => panic!("the docs model should load"),
    };
    let mut reference = reference_model;
    let frames = 64;
    let blocks = 40;
    stage.reserve(frames);
    let signal = |index: usize| (index as f32 * 0.017).sin() * 0.3;
    let mut played = Vec::new();
    let mut expected = Vec::new();
    for count in 0..blocks {
        let mut buffer: Vec<[f32; 4]> = (0..frames)
            .map(|index| {
                let sample = signal(count * frames + index);
                [sample, sample, 0.0, 0.0]
            })
            .collect();
        let mut mono: Vec<f32> = buffer.iter().map(|frame| frame[0]).collect();
        stage.begin(&controls, 48_000, 64.0);
        stage.process(&mut buffer, frames, NamPosition::Pre);
        reference.process_buffer(&mut mono);
        played.extend(buffer.iter().map(|frame| frame[0]));
        expected.extend(mono);
    }
    for (index, (got, want)) in played.iter().zip(&expected).enumerate() {
        assert!(
            (got - want).abs() < 1.0e-6,
            "sample {index}: {got} against the model's {want}"
        );
    }
}

// The migration null test against the old build.  `scripts/nam-null-test.sh`
// renders the reference with the single stage this stack replaced; without
// `RIGWERK_NAM_REFERENCE` the test passes without doing anything.

/// The largest difference the migrated stack may have from the old stage.
/// Both builds run the same ramps and the same mix; the level match is
/// exactly 1.0 for one block.  The one difference is the vendored nam-rs,
/// whose vectorized `tanh` moves a standard WaveNet capture by up to 1.2e-5
/// when it is driven near 0 dBFS (4.3e-6 on this test's signal), the
/// synthetic one by 2e-7 and the ReLU one not at all.  This covers that,
/// not a change in behaviour.
const NULL_TOLERANCE: f32 = 2.0e-5;

/// The reference's cases: (from block, enabled, post, input dB, output dB).
/// Must match the table `scripts/nam-null-test.sh` writes into the old build.
type NullSegment = (usize, bool, bool, f32, f32);
const NULL_CASES: &[(&str, &[NullSegment])] = &[
    ("unity", &[(0, true, false, 0.0, 0.0)]),
    (
        "trim",
        &[(0, true, false, 6.0, -3.5), (750, true, false, -4.5, 2.5)],
    ),
    ("post", &[(0, true, true, -2.0, 1.0)]),
    (
        "bypass",
        &[
            (0, true, false, 0.0, 0.0),
            (500, false, false, 0.0, 0.0),
            (1000, true, false, 0.0, 0.0),
        ],
    ),
];
const NULL_FRAMES: usize = 64;

fn read_f32(path: &std::path::Path) -> Vec<f32> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

/// The old stage's setting for `block`, migrated the way a saved path is.
fn migrated_at(segments: &[NullSegment], block: usize, model: &str) -> super::NamBlockSettings {
    let &(_, enabled, post, input_db, output_db) = segments
        .iter()
        .rev()
        .find(|segment| segment.0 <= block)
        .expect("every case starts at block 0");
    super::NamBlockSettings::from_legacy(&super::LegacyNamSettings {
        enabled,
        slot: if post {
            NamPosition::Post
        } else {
            NamPosition::Pre
        },
        input_db,
        output_db,
        slim: 1.0,
        model_path: model.to_owned(),
    })
}

#[test]
fn a_migrated_stage_nulls_against_the_old_build() {
    let Some(reference) = std::env::var_os("RIGWERK_NAM_REFERENCE") else {
        return;
    };
    let reference = std::path::Path::new(&reference);
    let rate = 48_000;
    let ramp = crate::audio::mixer::ramp::CONTROL_RAMP_SECONDS * rate as f32;
    let input = read_f32(&reference.join("input.f32"));
    let mut captures: Vec<_> = std::fs::read_dir(reference.join("captures"))
        .expect("the reference has a captures folder")
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "nam"))
        .collect();
    captures.sort();
    assert!(!captures.is_empty(), "the reference has no captures");
    for capture in &captures {
        let stem = capture.file_stem().unwrap().to_string_lossy().into_owned();
        let model = capture.to_string_lossy().into_owned();
        for (name, segments) in NULL_CASES {
            let expected = read_f32(&reference.join(format!("{stem}-{name}.f32")));
            assert_eq!(expected.len(), input.len(), "{stem}-{name}: length");
            let first = migrated_at(segments, 0, &model);
            let prepared =
                match super::prepare_nam_file(&first.model, rate, first.controls.slim, None, 1) {
                    NamFile::Ready(loaded) => loaded.prepared,
                    NamFile::RateMismatch { file_rate, .. } => panic!("{stem}: {file_rate} Hz"),
                    NamFile::Failed(error) => panic!("{stem}: {error}"),
                };
            // The bypass fade is 10 ms here and was 5 ms in the old stage, so
            // after a toggle the two differ until both fades are done: the
            // warm-up (whole blocks), the 10 ms fade, and a block either side.
            // An LSTM (no receptive field) keeps the state it stopped with,
            // and the two stopped at different samples, so a toggle case
            // cannot null for it at all.
            let field = prepared.model.receptive_field();
            let toggles: Vec<usize> = segments
                .windows(2)
                .filter(|pair| pair[0].1 != pair[1].1)
                .map(|pair| pair[1].0 * NULL_FRAMES)
                .collect();
            if !toggles.is_empty() && field == 0 {
                eprintln!("{stem}-{name}: skipped, a toggle cannot null for an LSTM");
                continue;
            }
            let fade = (super::BYPASS_FADE_SECONDS * rate as f32) as usize;
            let window = field.div_ceil(NULL_FRAMES) * NULL_FRAMES + 2 * NULL_FRAMES + fade;
            let controls = PathControls::new(rate);
            let mut stage = NamStage::new(rate);
            stage.install_for_test(0, prepared);
            stage.reserve(NULL_FRAMES);
            let mut played = Vec::with_capacity(input.len());
            for (block, chunk) in input.chunks(2 * NULL_FRAMES).enumerate() {
                let settings = migrated_at(segments, block, &model);
                let mut stack = NamStack::default();
                stack.push(
                    NamBlockControls {
                        id: 0,
                        ..settings.controls
                    }
                    .sanitized(),
                );
                controls.set_nam(&stack);
                stage.begin(&controls, rate, ramp);
                let mut frames: Vec<[f32; 4]> = chunk
                    .chunks_exact(2)
                    .map(|pair| [pair[0], pair[1], 0.0, 0.0])
                    .collect();
                stage.process(&mut frames, NULL_FRAMES, settings.controls.position);
                played.extend(frames.iter().flat_map(|frame| [frame[0], frame[1]]));
            }
            let mut max = 0.0f32;
            let mut shaped = 0.0f32;
            for (index, (got, want)) in played.iter().zip(&expected).enumerate() {
                let frame = index / 2;
                if toggles.iter().any(|&at| (at..at + window).contains(&frame)) {
                    continue;
                }
                max = max.max((got - want).abs());
                shaped = shaped.max((want - input[index]).abs());
            }
            eprintln!("{stem}-{name}: max difference {max:e}");
            assert!(
                shaped > 1.0e-3,
                "{stem}-{name}: the reference is the dry input"
            );
            assert!(
                max <= NULL_TOLERANCE,
                "{stem}-{name}: {max:e} from the old build, above {NULL_TOLERANCE:e}"
            );
        }
    }
}

/// How many commands still wait in `controls`' queue.
fn pending(controls: &PathControls) -> usize {
    controls
        .pending_nam_mut()
        .map(|pending| pending.len())
        .expect("nothing else holds the queue")
}

/// A stage playing block 0 with a model that has warmed up and faded in.
fn playing_block_zero() -> (PathControls, NamStage) {
    let controls = controls(&[block(0)]);
    let mut stage = NamStage::new(48_000);
    controls.queue_nam(0, tiny());
    run(&mut stage, &controls, 40, 64, 0.2);
    assert!(stage.has_model(0));
    assert_eq!(controls.nam_park().len(), 0);
    (controls, stage)
}

#[test]
fn back_to_back_swaps_park_every_model_they_replace() {
    let (controls, mut stage) = playing_block_zero();
    controls.queue_nam(0, tiny());
    // Three blocks of 64 frames: the second model is fading in over the
    // first one when the third arrives.
    run(&mut stage, &controls, 3, 64, 0.2);
    controls.queue_nam(0, tiny());
    run(&mut stage, &controls, 80, 64, 0.2);
    assert!(stage.has_model(0), "the third model plays");
    assert_eq!(pending(&controls), 0, "and it was installed");
    assert_eq!(
        controls.nam_park().len(),
        2,
        "the first two models were parked, not dropped by the callback"
    );
}

#[test]
fn a_clear_during_a_swap_parks_both_models() {
    let (controls, mut stage) = playing_block_zero();
    controls.queue_nam(0, tiny());
    run(&mut stage, &controls, 3, 64, 0.2);
    controls.clear_nam(0);
    let late = run(&mut stage, &controls, 80, 64, 0.2);
    assert!(!stage.has_model(0), "the clear was applied");
    assert_eq!(late[63], 0.2, "and the block plays dry");
    assert_eq!(
        controls.nam_park().len(),
        2,
        "the model fading out and the one fading in were both parked"
    );
}

#[test]
fn a_swap_cut_short_by_bypass_does_not_bring_its_old_model_back() {
    let (controls, mut stage) = playing_block_zero();
    controls.queue_nam(0, tiny());
    run(&mut stage, &controls, 2, 64, 0.2);
    // Bypass while the swap runs: the 10 ms fade to dry ends long before
    // the 35 ms swap would.
    let off = NamBlockControls {
        enabled: false,
        ..block(0)
    };
    controls.set_nam(&stack_of(&[off]));
    run(&mut stage, &controls, 9, 64, 0.2);
    // A new model for the silent block, and the block back on.
    controls.queue_nam(0, tiny());
    controls.set_nam(&stack_of(&[block(0)]));
    run(&mut stage, &controls, 1, 64, 0.2);
    assert_eq!(
        controls.nam_park().len(),
        2,
        "the unheard old side of the swap was parked before the new model came"
    );
    let late = run(&mut stage, &controls, 80, 64, 0.2);
    assert!((late[63] - settled(0.2)).abs() < 1.0e-5);
    assert_eq!(controls.nam_park().len(), 2);
}

#[test]
fn a_nearly_full_park_ends_the_pass_over_the_commands() {
    use super::PARK_SLOTS;
    let controls = controls(&[block(0)]);
    let park = controls.nam_park();
    for _ in 0..PARK_SLOTS - 1 {
        assert!(park.try_retire(tiny().model).is_ok());
    }
    // Three models for blocks that are not in the stack: one fits.
    for id in 50..53 {
        controls.queue_nam(id, tiny());
    }
    let mut stage = NamStage::new(48_000);
    run(&mut stage, &controls, 1, 64, 0.2);
    assert_eq!(park.len(), PARK_SLOTS);
    assert_eq!(pending(&controls), 2, "the rest wait for room");

    park.flush();
    run(&mut stage, &controls, 1, 64, 0.2);
    assert_eq!(park.len(), 2);
    assert_eq!(pending(&controls), 0);
}

#[test]
fn a_block_in_the_stack_without_runtime_state_keeps_its_model_queued() {
    use super::{PARK_SLOTS, RUN_SLOTS};
    let controls = controls(&[block(200)]);
    let park = controls.nam_park();
    // Every runtime slot is held by a block that left the stack, and none
    // can let go of its model: the park is full.
    let mut stage = NamStage::new(48_000);
    for id in 0..RUN_SLOTS as u32 {
        stage.install_for_test(id, tiny());
    }
    for _ in 0..PARK_SLOTS {
        assert!(park.try_retire(tiny().model).is_ok());
    }
    run(&mut stage, &controls, 1, 64, 0.2);

    // The UI thread makes room and the model for block 200 arrives in the
    // same frame.  The block is shown as loaded, so the UI never queues it
    // again: it must wait for its slot, not go to the park.
    controls.queue_nam(200, tiny());
    park.flush();
    run(&mut stage, &controls, 1, 64, 0.2);
    assert_eq!(pending(&controls), 1, "the model waits for its block");
    assert!(!stage.has_model(200));

    // The departed blocks let go once the park has room, and the block
    // gets its slot and its model.
    run(&mut stage, &controls, 2, 64, 0.2);
    assert_eq!(pending(&controls), 0);
    assert!(stage.has_model(200), "the block plays its model");
    assert_eq!(park.len(), RUN_SLOTS, "every departed model was parked");
    let late = run(&mut stage, &controls, 40, 64, 0.2);
    assert!((late[63] - settled(0.2)).abs() < 1.0e-5);
}
