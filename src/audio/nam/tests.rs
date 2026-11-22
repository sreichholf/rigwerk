//! The NAM stack: sanitising, the published order, loading a capture,
//! bypass, warm-up, the swap fades, the fades of a removal and a reorder,
//! the level match between blocks, and the queue: no model is dropped by
//! the stage, and no command is lost or spun on.

use super::{
    estimate_load, measure_load, prepare_nam_json, slim_submodel, NamBlockControls, NamFile,
    NamPosition, NamStack, NamStage, PreparedNam,
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
    let load = measure_load(&mut model, 48_000, 64);
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
fn the_estimate_follows_the_rate_and_not_the_buffer() {
    let at_48k = estimate_load("WaveNet", 48_000.0).share();
    let at_96k = estimate_load("WaveNet", 96_000.0).share();
    assert!((at_48k - 0.05).abs() < 1.0e-6);
    assert!((at_96k - 0.10).abs() < 1.0e-6);
    assert!(estimate_load("LSTM", 48_000.0).share() < at_48k);
}

#[test]
fn one_block_at_unity_is_the_model_run_by_hand() {
    // The null path of the migration: one block, unity trims, no level
    // match, enabled — the old single stage's math.  It must be the model
    // itself, sample for sample, within the one rounding of the wet mix
    // (`dry + (wet - dry) * 1.0`, see `NamSlot::process_block`).
    let controls = controls(&[block(0)]);
    let mut stage = NamStage::new(48_000);
    stage.install_for_test(0, tiny());
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
                match super::prepare_nam_file(&first.model, rate, first.controls.slim, None) {
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
