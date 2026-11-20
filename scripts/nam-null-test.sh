#!/usr/bin/env bash
# The NAM migration null test.  Renders a fixed input through the single
# built-in stage of the last build that had it (AmpStage, OLD_COMMIT) and
# then checks that a NamStage built from the migrated setting plays the
# same samples (`a_migrated_stage_nulls_against_the_old_build`).
#
# <dir>/old is a git worktree of OLD_COMMIT.  The script appends a reference
# render test to its src/audio/amp/tests.rs (reset first, never committed)
# and writes <dir>/reference: input.f32, captures/*.nam and one
# <capture>-<case>.f32 per capture and case, raw little-endian f32, stereo
# interleaved.  The captures are the tiny docs WaveNet, a deeper synthetic
# WaveNet, and RIGWERK_NAM_CAPTURE when it names a 48 kHz `.nam`.  The old
# build always uses <dir>/target, whatever CARGO_TARGET_DIR says: Cargo
# gives both trees the same test binary name, so a shared target directory
# would let one build overwrite the other's tests.
set -euo pipefail

OLD_COMMIT=e429fee85f45311e43bde6cf2f2c6261c405a9ab

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <dir>" >&2
  exit 2
fi

repo=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$1"
dir=$(cd "$1" && pwd)
old="$dir/old"
reference="$dir/reference"

if [[ "$(git -C "$old" rev-parse HEAD 2>/dev/null)" != "$OLD_COMMIT" ]]; then
  git -C "$repo" worktree remove --force "$old" 2>/dev/null || rm -rf "$old"
  git -C "$repo" worktree prune
  git -C "$repo" worktree add --detach "$old" "$OLD_COMMIT"
fi
git -C "$old" checkout -- src/audio/amp/tests.rs

cat >> "$old/src/audio/amp/tests.rs" <<'RUST'

/// The null-test reference, appended by scripts/nam-null-test.sh.  The case
/// table matches `a_migrated_stage_nulls_against_the_old_build` on the new
/// build: (from block, enabled, post, input dB, output dB).
mod nam_null_reference {
    use std::path::Path;

    use crate::audio::amp::{prepare_amp_file, AmpControls, AmpFile, AmpSlot, AmpStage};
    use crate::audio::mixer::ramp::CONTROL_RAMP_SECONDS;
    use crate::audio::path::PathControls;

    const RATE: u32 = 48_000;
    const FRAMES: usize = 64;
    const BLOCKS: usize = 1500;
    type Segment = (usize, bool, bool, f32, f32);
    const CASES: &[(&str, &[Segment])] = &[
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

    /// Two sines per side and a little noise from a fixed xorshift seed.
    fn signal() -> Vec<[f32; 2]> {
        let mut state = 0x1234_5678u32;
        let mut noise = move || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as f64 / u32::MAX as f64 * 2.0 - 1.0
        };
        let tau = std::f64::consts::TAU;
        (0..BLOCKS * FRAMES)
            .map(|index| {
                let t = index as f64 / RATE as f64;
                let swell = 0.6 + 0.4 * (tau * 0.7 * t).sin();
                let left = swell * (0.4 * (tau * 110.0 * t).sin() + 0.15 * (tau * 1733.0 * t).sin())
                    + 0.05 * noise();
                let right = swell * (0.3 * (tau * 220.0 * t + 0.5).sin())
                    + 0.1 * (tau * 3001.0 * t).sin()
                    + 0.05 * noise();
                [left as f32, right as f32]
            })
            .collect()
    }

    /// Two layer arrays, gated and not, with dilations: unlike the docs
    /// model this one has a history.  Its weight count is the one nam-rs
    /// asks for; the weights come from a fixed LCG.
    fn synthetic_wavenet() -> String {
        let config = r#""version": "0.5.4", "architecture": "WaveNet", "sample_rate": 48000.0,
            "config": {
                "layers": [
                    {"input_size": 1, "condition_size": 1, "channels": 4, "head_size": 2,
                     "kernel_size": 3, "dilations": [1, 2, 4, 8, 16, 32], "activation": "Tanh",
                     "gated": true, "head_bias": false},
                    {"input_size": 4, "condition_size": 1, "channels": 2, "head_size": 1,
                     "kernel_size": 3, "dilations": [1, 2, 4, 8], "activation": "Tanh",
                     "gated": false, "head_bias": true}
                ],
                "head": null, "head_scale": 0.5
            }"#;
        let empty = format!("{{{config}, \"weights\": []}}");
        let expected = match nam_rs::NamModel::from_json_str(&empty)
            .and_then(|parsed| nam_rs::Model::from_nam(&parsed).map(|_| 0))
        {
            Err(nam_rs::Error::WeightCountMismatch { expected, .. }) => expected,
            other => panic!("expected a weight count mismatch, got {:?}", other.err()),
        };
        let mut state = 0x2545_f491u32;
        let weights: Vec<String> = (0..expected)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                format!("{:.6}", (state >> 8) as f64 / (1u32 << 24) as f64 * 1.2 - 0.6)
            })
            .collect();
        format!("{{{config}, \"weights\": [{}]}}", weights.join(", "))
    }

    fn controls_at(segments: &[Segment], block: usize) -> AmpControls {
        let &(_, enabled, post, input_db, output_db) = segments
            .iter()
            .rev()
            .find(|segment| segment.0 <= block)
            .expect("every case starts at block 0");
        AmpControls {
            enabled,
            slot: if post { AmpSlot::Post } else { AmpSlot::Pre },
            input_db,
            output_db,
            slim: 1.0,
        }
    }

    /// The way the old `PathProcessor` drove the stage: `begin` and then
    /// `process_block` per block, at the mixer's control ramp.
    fn render(capture: &Path, segments: &[Segment], input: &[[f32; 2]]) -> Vec<f32> {
        let prepared = match prepare_amp_file(&capture.to_string_lossy(), RATE, 1.0) {
            AmpFile::Ready(loaded) => loaded.prepared,
            AmpFile::RateMismatch { file_rate, .. } => {
                panic!("{} is a {file_rate} Hz capture, not 48 kHz", capture.display())
            }
            AmpFile::Failed(error) => panic!("{}: {error}", capture.display()),
        };
        let controls = PathControls::new(RATE);
        let mut stage = AmpStage::new(RATE);
        stage.install_prepared(prepared);
        stage.reserve(FRAMES);
        let ramp = CONTROL_RAMP_SECONDS * RATE as f32;
        let mut out = Vec::with_capacity(input.len() * 2);
        for (block, chunk) in input.chunks(FRAMES).enumerate() {
            controls.set_amp(controls_at(segments, block));
            stage.begin(&controls, RATE, ramp);
            let mut frames: Vec<[f32; 4]> =
                chunk.iter().map(|&[l, r]| [l, r, 0.0, 0.0]).collect();
            stage.process_block(&mut frames, FRAMES);
            out.extend(frames.iter().flat_map(|frame| [frame[0], frame[1]]));
        }
        out
    }

    fn write_f32(path: &Path, samples: &[f32]) {
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        std::fs::write(path, bytes).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    }

    #[test]
    fn nam_null_reference() {
        let Some(out) = std::env::var_os("RIGWERK_NAM_REFERENCE_OUT") else {
            return;
        };
        let out = Path::new(&out);
        let captures = out.join("captures");
        std::fs::create_dir_all(&captures).unwrap();
        std::fs::write(captures.join("tiny.nam"), super::TINY).unwrap();
        std::fs::write(captures.join("synthetic.nam"), synthetic_wavenet()).unwrap();
        let input = signal();
        let flat: Vec<f32> = input.iter().flatten().copied().collect();
        write_f32(&out.join("input.f32"), &flat);
        let mut files: Vec<_> = std::fs::read_dir(&captures)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "nam"))
            .collect();
        files.sort();
        for capture in files {
            let stem = capture.file_stem().unwrap().to_string_lossy().into_owned();
            for (name, segments) in CASES {
                let rendered = render(&capture, segments, &input);
                write_f32(&out.join(format!("{stem}-{name}.f32")), &rendered);
                eprintln!("reference: {stem}-{name}");
            }
        }
    }
}
RUST

rm -rf "$reference"
mkdir -p "$reference/captures"
if [[ -n "${RIGWERK_NAM_CAPTURE:-}" ]]; then
  cp "$RIGWERK_NAM_CAPTURE" "$reference/captures/user.nam"
fi

# Runs one test and fails unless exactly that one ran and passed: a filter
# that matches nothing would otherwise pass.
run_one() {
  local log
  log=$(mktemp)
  cargo test --locked "$1" -- --exact --nocapture 2>&1 | tee "$log"
  if ! grep -q "test result: ok. 1 passed" "$log"; then
    rm -f "$log"
    echo "$1 did not run and pass" >&2
    exit 1
  fi
  rm -f "$log"
}

echo "Rendering the reference with $OLD_COMMIT in $old"
(cd "$old" && CARGO_TARGET_DIR="$dir/target" RIGWERK_NAM_REFERENCE_OUT="$reference" \
  run_one audio::amp::tests::nam_null_reference::nam_null_reference)

echo "Comparing the migrated stage in $repo"
(cd "$repo" && RIGWERK_NAM_REFERENCE="$reference" \
  run_one audio::nam::tests::a_migrated_stage_nulls_against_the_old_build)
