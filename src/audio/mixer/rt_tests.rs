//! NAM edits must not allocate or free on the audio thread.  Dropping
//! a model there is a free.
//!
//! Each block has its own guard.  What the edit allocates is the UI
//! thread.

use super::super::nam::{
    prepare_nam_json, prepare_nam_json_at, NamBlockControls, NamFile, NamPosition, NamStack,
};
use super::super::rt_guard::RtGuard;
use super::super::shared::Shared;
use super::super::types::BufferMode;
use super::OutputMixer;

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

fn model() -> super::super::nam::PreparedNam {
    match prepare_nam_json(TINY, 48_000, 1.0) {
        NamFile::Ready(loaded) => loaded.prepared,
        _ => panic!("the docs model should load"),
    }
}

/// The docs model built for an oversampled group.
pub(super) fn scaled_model() -> super::super::nam::PreparedNam {
    match prepare_nam_json_at(TINY, 48_000, 1.0, 2) {
        NamFile::Ready(loaded) => loaded.prepared,
        _ => panic!("the docs model should load"),
    }
}

fn block(id: u32) -> NamBlockControls {
    NamBlockControls {
        id,
        ..NamBlockControls::default()
    }
}

fn stack(ids: &[u32]) -> NamStack {
    let mut stack = NamStack::default();
    for id in ids {
        stack.push(block(*id));
    }
    stack
}

/// A mixer on a running bridge with `frames` frames of a quiet signal
/// buffered, ready for its first block.
fn running_mixer(frames: usize) -> (OutputMixer, Shared) {
    let shared = Shared::new(48_000);
    shared.set_source_rate(48_000);
    shared.set_bridge_buffer(BufferMode::Fixed, 100.0);
    shared.set_mix(false, false, [true, true], [true, true], false);
    for _ in 0..frames * 4 {
        shared.push_frame(0.05, -0.05);
    }
    let mut mixer = OutputMixer::new(48_000, 2);
    mixer.prepare(frames);
    (mixer, shared)
}

/// The buffer is outside the guard.  A release build counts nothing,
/// so the reads below are 0 == 0 there.
fn render_one_block(mixer: &mut OutputMixer, shared: &Shared, frames: usize, out: &mut [Vec<f32>]) {
    let guard = RtGuard::enter();
    mixer.render(shared, frames, |index, mapped| {
        for (channel, sample) in out[index].iter_mut().enumerate() {
            *sample = mapped.sample(channel);
        }
    });
    assert_eq!(
        guard.allocations(),
        0,
        "the block allocated on the audio thread"
    );
    assert_eq!(guard.frees(), 0, "the block freed on the audio thread");
    drop(guard);
}

#[test]
fn a_block_with_nam_blocks_allocates_nothing() {
    let frames = 256;
    let (mut mixer, shared) = running_mixer(frames);
    shared.path(0).set_nam(&stack(&[0, 1, 2]));
    // The first block adopts the settings and takes the queued model; the
    // scratch was grown by `prepare`.
    shared.path(0).queue_nam(0, model());
    let mut out = vec![vec![0.0f32; 2]; frames];
    mixer.render(&shared, frames, |index, mapped| {
        for (channel, sample) in out[index].iter_mut().enumerate() {
            *sample = mapped.sample(channel);
        }
    });

    for _ in 0..4 {
        render_one_block(&mut mixer, &shared, frames, &mut out);
    }
}

#[test]
fn oversampled_nam_blocks_allocate_and_free_nothing() {
    let frames = 256;
    let (mut mixer, shared) = running_mixer(frames);
    shared.path(0).set_nam(&stack(&[0, 1]));
    shared.path(0).queue_nam(0, scaled_model());
    shared.path(0).queue_nam(1, scaled_model());
    let mut out = vec![vec![0.0f32; 2]; frames];
    // The first block takes both models and starts their warm-up; the rest
    // run the 2x pair, a swap, and a bypass through the block paths.
    mixer.render(&shared, frames, |index, mapped| {
        for (channel, sample) in out[index].iter_mut().enumerate() {
            *sample = mapped.sample(channel);
        }
    });
    for _ in 0..40 {
        render_fed(&mut mixer, &shared, frames, &mut out);
    }
    // Replacing one scaled model keeps the pair's history moving.
    shared.path(0).queue_nam(1, scaled_model());
    for _ in 0..40 {
        render_fed(&mut mixer, &shared, frames, &mut out);
    }
    let mut bypassed = NamStack::default();
    bypassed.push(NamBlockControls {
        enabled: false,
        ..block(0)
    });
    bypassed.push(block(1));
    shared.path(0).set_nam(&bypassed);
    for _ in 0..4 {
        render_fed(&mut mixer, &shared, frames, &mut out);
    }
}

#[test]
fn adding_removing_and_reordering_nam_blocks_allocates_nothing() {
    let frames = 256;
    let (mut mixer, shared) = running_mixer(frames);
    shared.path(0).set_nam(&stack(&[0, 1]));
    shared.path(0).queue_nam(0, model());
    let mut out = vec![vec![0.0f32; 2]; frames];
    mixer.render(&shared, frames, |index, mapped| {
        for (channel, sample) in out[index].iter_mut().enumerate() {
            *sample = mapped.sample(channel);
        }
    });

    // The edits are the UI thread.  Only the render after each one is
    // counted.
    let mut grown = stack(&[0, 1, 2]);
    shared.path(0).set_nam(&grown);
    shared.path(0).queue_nam(2, model());
    render_one_block(&mut mixer, &shared, frames, &mut out);

    grown.move_block(2, -1);
    shared.path(0).set_nam(&grown);
    render_one_block(&mut mixer, &shared, frames, &mut out);

    let mut bypassed = NamStack::default();
    bypassed.push(block(0));
    bypassed.push(NamBlockControls {
        enabled: false,
        ..block(1)
    });
    bypassed.push(block(2));
    shared.path(0).set_nam(&bypassed);
    render_one_block(&mut mixer, &shared, frames, &mut out);

    let mut moved = NamStack::default();
    moved.push(block(0));
    moved.push(NamBlockControls {
        position: NamPosition::Post,
        ..block(2)
    });
    shared.path(0).set_nam(&moved);
    render_one_block(&mut mixer, &shared, frames, &mut out);
}

/// Tops the input ring up outside the guard, so a long test does not run
/// the bridge dry, then renders one guarded block.
fn render_fed(mixer: &mut OutputMixer, shared: &Shared, frames: usize, out: &mut [Vec<f32>]) {
    for _ in 0..frames {
        shared.push_frame(0.05, -0.05);
    }
    render_one_block(mixer, shared, frames, out);
}

/// A mixer whose path 0 plays block 0 with the docs model, warmed up and
/// faded in.
fn playing_block_zero(frames: usize) -> (OutputMixer, Shared, Vec<Vec<f32>>) {
    let (mut mixer, shared) = running_mixer(frames);
    shared.path(0).set_nam(&stack(&[0]));
    shared.path(0).queue_nam(0, model());
    let mut out = vec![vec![0.0f32; 2]; frames];
    mixer.render(&shared, frames, |index, mapped| {
        for (channel, sample) in out[index].iter_mut().enumerate() {
            *sample = mapped.sample(channel);
        }
    });
    // The warm-up and the 35 ms swap fade: well inside 12 blocks of 256.
    for _ in 0..12 {
        render_fed(&mut mixer, &shared, frames, &mut out);
    }
    (mixer, shared, out)
}

#[test]
fn replacing_an_audible_model_allocates_nothing() {
    let frames = 256;
    let (mut mixer, shared, mut out) = playing_block_zero(frames);
    // The second model is built here, on the UI side; the crossfade from
    // the first one and its retirement into the park are the audio side.
    shared.path(0).queue_nam(0, model());
    for _ in 0..16 {
        render_fed(&mut mixer, &shared, frames, &mut out);
    }
    assert_eq!(
        shared.path(0).nam_park().len(),
        1,
        "the first model was faded out and parked"
    );
}

#[test]
fn removing_an_audible_block_allocates_nothing() {
    let frames = 256;
    let (mut mixer, shared, mut out) = playing_block_zero(frames);
    shared.path(0).set_nam(&NamStack::default());
    // The block fades to dry over 10 ms, then its model is parked.
    render_fed(&mut mixer, &shared, frames, &mut out);
    assert_eq!(
        shared.path(0).nam_park().len(),
        0,
        "an audible block fades out before it is parked"
    );
    for _ in 0..8 {
        render_fed(&mut mixer, &shared, frames, &mut out);
    }
    assert_eq!(
        shared.path(0).nam_park().len(),
        1,
        "the removed block's model was parked"
    );
}

#[test]
fn back_to_back_swaps_allocate_and_free_nothing() {
    let frames = 256;
    let (mut mixer, shared, mut out) = playing_block_zero(frames);
    shared.path(0).queue_nam(0, model());
    render_fed(&mut mixer, &shared, frames, &mut out);
    render_fed(&mut mixer, &shared, frames, &mut out);
    // A third model while the second still fades in over the first.
    shared.path(0).queue_nam(0, model());
    for _ in 0..24 {
        render_fed(&mut mixer, &shared, frames, &mut out);
    }
    assert_eq!(
        shared.path(0).nam_park().len(),
        2,
        "both replaced models were parked"
    );
}

#[test]
fn clearing_during_a_swap_allocates_and_frees_nothing() {
    let frames = 256;
    let (mut mixer, shared, mut out) = playing_block_zero(frames);
    shared.path(0).queue_nam(0, model());
    render_fed(&mut mixer, &shared, frames, &mut out);
    render_fed(&mut mixer, &shared, frames, &mut out);
    shared.path(0).clear_nam(0);
    for _ in 0..24 {
        render_fed(&mut mixer, &shared, frames, &mut out);
    }
    assert_eq!(
        shared.path(0).nam_park().len(),
        2,
        "the model fading out and the one fading in were both parked"
    );
}

/// The pitch shifter on at `semitones`.
fn pitch(semitones: i8) -> super::super::pitch::PitchSettings {
    super::super::pitch::PitchSettings {
        enabled: true,
        semitones,
        ..super::super::pitch::PitchSettings::default()
    }
}

/// Every change the pitch sheet can make, between blocks, with the stage on:
/// semitones, wet, bypass and delay, and switching it off and on.
#[test]
fn pitch_changes_allocate_and_free_nothing() {
    use super::super::pitch::PitchSettings;
    let frames = 256;
    let (mut mixer, shared) = running_mixer(frames);
    let mut out = vec![vec![0.0f32; 2]; frames];
    let changes = [
        pitch(-2),
        pitch(-12),
        PitchSettings {
            wet: 0.5,
            ..pitch(5)
        },
        PitchSettings {
            bypass: true,
            ..pitch(5)
        },
        PitchSettings {
            wet: 0.5,
            ..pitch(3)
        },
        PitchSettings {
            delay_ms: 40.0,
            ..pitch(3)
        },
        PitchSettings::default(),
        pitch(-7),
    ];
    for settings in changes {
        let mut mix = shared.path(0).mix();
        mix.pitch = settings;
        shared.path(0).set_mix(&mix);
        // Long enough for a switch to warm, dip and land.
        for _ in 0..24 {
            render_fed(&mut mixer, &shared, frames, &mut out);
        }
    }
}

/// The stage on, the graph rate moved 48 → 192 → 48 kHz in place, as the
/// PipeWire duplex filter does (`set_direct_rate` inside the callback).
#[test]
fn a_rate_change_with_the_pitch_shifter_on_allocates_nothing() {
    let frames = 256;
    let shared = Shared::new(48_000);
    shared.set_mix(false, false, [true, true], [true, true], false);
    shared
        .direct
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let mut mixer = OutputMixer::new(48_000, 2);
    mixer.prepare(frames);
    let left: Vec<f32> = (0..frames).map(|n| (n as f32 * 0.03).sin() * 0.3).collect();
    let mut peak = 0.0f32;
    let mut mix = shared.path(0).mix();
    mix.pitch = pitch(-5);
    shared.path(0).set_mix(&mix);
    for rate in [48_000, 192_000, 48_000] {
        // Long enough for the switch on to warm, dip and land.
        for block in 0..32 {
            let guard = RtGuard::enter();
            if block == 0 {
                mixer.set_direct_rate(rate);
            }
            mixer.render_direct(&shared, &left, &left, |_, mapped| {
                peak = peak.max(mapped.sample(0).abs());
            });
            assert_eq!(guard.allocations(), 0, "@{rate} allocated");
            assert_eq!(guard.frees(), 0, "@{rate} freed");
            drop(guard);
        }
        assert_eq!(
            shared.path(0).path_latency_samples(),
            super::super::pitch::pitch_latency_samples(rate, 4.0),
            "@{rate}"
        );
    }
    assert!(peak.is_finite() && peak > 0.01, "the path plays: {peak}");
}
