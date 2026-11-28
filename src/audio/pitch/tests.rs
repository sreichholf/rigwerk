//! Debug `cargo test` runs a subset: one rate, a few cases and shifts.
//! The release harness is `harness` (`RIGWERK_PITCH_RENDER`).

mod gate;
mod harness;
mod material;
mod score;
mod spectral;
mod tight_state;

use std::f64::consts::TAU;

use super::*;
use crate::audio::rt_guard::RtGuard;
use gate::{evaluate, failures, on, run, run_delay, Engine};
use material::{case, render};
use tight::spec;

const RATES: [u32; 4] = [44_100, 48_000, 96_000, 192_000];

/// A deterministic test signal: a few sines and a little noise.
fn signal(rate: u32, seconds: f64) -> Vec<f32> {
    let mut rng = material::Lcg(7);
    (0..(seconds * rate as f64) as usize)
        .map(|i| {
            let t = i as f64 / rate as f64;
            let tone = 0.2 * (TAU * 196.0 * t).sin() + 0.1 * (TAU * 523.0 * t).sin();
            (tone + 0.02 * (rng.next() - 0.5)) as f32
        })
        .collect()
}

/// A sine of `freq` Hz at 0.25.
fn sine(rate: u32, freq: f64, seconds: f64) -> Vec<f32> {
    (0..(seconds * rate as f64) as usize)
        .map(|i| (0.25 * (TAU * freq * i as f64 / rate as f64).sin()) as f32)
        .collect()
}

/// Feeds `input` (mono, on both channels) in blocks of `block`, calling
/// `each` before every block with the block's index and the shifter, and
/// returns channel 0 and the latency each block reported.
fn drive(
    shifter: &mut PitchShifter,
    input: &[f32],
    block: usize,
    mut each: impl FnMut(usize, &mut PitchShifter),
) -> (Vec<f32>, Vec<u32>) {
    let mut out = Vec::with_capacity(input.len());
    let mut latencies = Vec::new();
    let mut buf = vec![[0.0f32; 4]; block];
    for (index, chunk) in input.chunks(block).enumerate() {
        each(index, shifter);
        shifter.begin_block();
        latencies.push(shifter.latency_samples());
        for (frame, &x) in buf.iter_mut().zip(chunk) {
            *frame = [x, x, 0.0, 0.0];
        }
        shifter.process(&mut buf, chunk.len());
        out.extend(buf[..chunk.len()].iter().map(|f| f[0]));
    }
    (out, latencies)
}

/// The strongest peak within ±`search` Hz of `freq` in the second half of
/// `out`, Hz.
fn measured_pitch(out: &[f32], rate: u32, freq: f64, search: f64) -> f64 {
    let nfft = 1 << 15;
    let len = nfft.min(out.len() / 2);
    let spec = score::spectrum(out, out.len() - len, len, nfft);
    score::peak_near(&spec, nfft, rate as f64, freq, search)
        .expect("a peak near the target")
        .0
}

fn cents(a: f64, b: f64) -> f64 {
    1200.0 * (a / b).log2()
}

#[test]
fn tight_reports_its_attack_delay_at_every_rate() {
    let base = spec(48_000, DEFAULT_DELAY_MS);
    assert_eq!(base.latency, 192, "4 ms at 48 kHz");
    // The delay is the setting at every rate, not a fixed sample count.
    assert_eq!(spec(44_100, DEFAULT_DELAY_MS).latency, 176);
    assert_eq!(spec(22_050, DEFAULT_DELAY_MS).latency, 88);
    assert_eq!(spec(96_000, DEFAULT_DELAY_MS).latency, 384);
    assert_eq!(spec(192_000, DEFAULT_DELAY_MS).latency, 768);
    // The analysis sizes stay the 48 kHz ones at 44.1 and the 192 kHz ones
    // above 192 kHz; only the latency follows the rate.
    assert_eq!(spec(44_100, DEFAULT_DELAY_MS).window, base.window);
    assert_eq!(spec(44_100, DEFAULT_DELAY_MS).hop, base.hop);
    assert_eq!(
        spec(384_000, DEFAULT_DELAY_MS).window,
        spec(192_000, DEFAULT_DELAY_MS).window
    );
    assert_eq!(
        spec(384_000, DEFAULT_DELAY_MS).hop,
        spec(192_000, DEFAULT_DELAY_MS).hop
    );
    assert_eq!(spec(384_000, DEFAULT_DELAY_MS).latency, 1_536);
    for rate in RATES {
        assert_eq!(
            pitch_latency_samples(PitchSettings::default(), rate),
            spec(rate, DEFAULT_DELAY_MS).latency
        );
    }
}

#[test]
fn at_zero_semitones_the_output_is_the_input_delayed() {
    let rate = 48_000;
    let input = signal(rate, 0.5);
    let (out, latency) = run(&input, rate, 0, 1, 128);
    assert_eq!(latency, spec(rate, DEFAULT_DELAY_MS).latency);
    let worst = input
        .iter()
        .zip(&out)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max);
    assert!(worst < 1e-4, "reconstruction error {worst}");
}

#[test]
fn the_latency_is_minimal_not_only_reported() {
    for rate in RATES {
        let spec = spec(rate, DEFAULT_DELAY_MS);
        let latency = spec.latency as usize;
        let mut input = vec![0.0f32; spec.window * 2 + latency * 2 + 2];
        let at = spec.window + 7;
        input[at] = 0.5;
        let mut shifter = PitchShifter::new(rate, 0);
        shifter.update(on(0), rate);
        let (out, latencies) = drive(&mut shifter, &input, 256, |_, _| {});
        assert!(latencies.iter().all(|&l| l == spec.latency));
        let peak = out
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .map(|(i, _)| i)
            .unwrap();
        assert_eq!(peak - at, latency, "@{rate}");
        assert!((out[peak] - 0.5).abs() < 1e-4);
    }
}

#[test]
fn tight_hears_every_click_at_its_reported_latency_at_a_shift() {
    let rate = 48_000;
    let input = render(&case("clicks"), rate, 1.0);
    let spec = spec(rate, DEFAULT_DELAY_MS);
    for semitones in [-2i8, -5, -12, 5, 12] {
        let (out, latency) = run(&input, rate, semitones, 0, 256);
        assert_eq!(latency, spec.latency);
        let mut raw = vec![0.0; latency as usize];
        raw.extend_from_slice(&out);
        let measured =
            score::measured_delay(&input, &raw, rate, spec.latency as usize + 2 * spec.hop);
        // An onset re-anchors the heads and reads the attack at a ratio of
        // 1: exactly the reported latency, whatever the shift.
        assert_eq!(measured, latency as usize, "{semitones:+}");
    }
}

#[test]
fn tight_keeps_a_chug_tight_and_its_gap_clean_an_octave_down() {
    let rate = 48_000;
    for name in ["chugs", "chugs200"] {
        let c = case(name);
        let input = render(&c, rate, 1.0);
        let (out, _) = run(&input, rate, -12, 0, 256);
        let (env_in, env_out) = (score::envelope(&input, rate), score::envelope(&out, rate));
        for onset in score::onsets(&c, rate) {
            let (rise_in, pre_in) = score::attack(&env_in, rate, onset);
            let (rise, pre) = score::attack(&env_out, rate, onset);
            // The attack is read at its own speed, not twice as slowly (the
            // fast chugs' envelopes have second bumps that move a rise).
            if name == "chugs" && rise_in > 3.0 {
                assert!(
                    rise < rise_in + 1.5,
                    "{name} @{onset}: {rise:.1} ms against {rise_in:.1}"
                );
            }
            // The last chug's tail is not read late into the gap: within
            // 4 dB of the input's own (the sweep reads it up to 4 ms late).
            assert!(
                pre < (pre_in + 4.0).max(-30.0),
                "{name} @{onset}: {pre:.1} dB against {pre_in:.1}"
            );
        }
    }
}

#[test]
fn a_sine_lands_on_its_target() {
    let rate = 48_000;
    // Source and target at least three bins of the period estimate's span
    // from DC.
    let lowest = 3.0 * rate as f64 / spec(rate, DEFAULT_DELAY_MS).window as f64;
    for freq in [82.41, 110.0, 196.0, 330.0] {
        for semitones in [-2i8, -5, -12] {
            let target = freq * gate::ratio(semitones);
            if target < lowest {
                continue;
            }
            let input = sine(rate, freq, 0.8);
            let (out, _) = run(&input, rate, semitones, 0, 256);
            let got = measured_pitch(&out, rate, target, target * 0.03);
            let error = cents(got, target);
            assert!(
                error.abs() < 2.0,
                "{freq} Hz {semitones:+}: {error:.2} cents"
            );
        }
    }
}

#[test]
fn the_output_does_not_depend_on_the_block_size() {
    let rate = 48_000;
    let input = signal(rate, 0.3);
    let chugs = render(&case("chugs"), rate, 1.0);
    for input in [&input, &chugs] {
        for slot in [0, 2] {
            let reference = run(input, rate, -5, slot, 32).0;
            for block in [64, 128, 1024] {
                let out = run(input, rate, -5, slot, block).0;
                assert!(out == reference, "slot {slot}, {block}-frame blocks differ");
            }
        }
    }
    // A staggered slot estimates the period on other samples.
    let other = run(&chugs, rate, -5, 1, 32).0;
    assert!(other != run(&chugs, rate, -5, 0, 32).0);
}

#[test]
fn silence_in_is_silence_out() {
    let rate = 48_000;
    let input = vec![0.0f32; rate as usize / 2];
    let (out, _) = run(&input, rate, -5, 0, 256);
    assert!(out.iter().all(|&x| x == 0.0));
}

#[test]
fn a_non_finite_input_is_recovered_from() {
    let rate = 48_000;
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let mut input = sine(rate, 196.0, 1.0);
        input[rate as usize / 4] = bad;
        let (out, _) = run(&input, rate, -5, 0, 256);
        assert!(out.iter().all(|x| x.is_finite()), "{bad} stuck");
        // Back to the clean output's level by the end.
        let clean = run(&sine(rate, 196.0, 1.0), rate, -5, 0, 256).0;
        let tail = |x: &[f32]| score::rms(x, rate, 0.8, 0.95);
        let db = 20.0 * (tail(&out) / tail(&clean)).log10();
        assert!(db.abs() < 0.5, "{bad}: {db:.2} dB after recovery");
    }
}

#[test]
fn the_default_is_off_and_fully_wet() {
    let s = PitchSettings::default();
    assert!(!s.enabled && !s.bypass);
    assert_eq!(
        (s.semitones, s.cents, s.wet, s.delay_ms),
        (0, 0.0, 1.0, 4.0)
    );
    assert!(s.is_unset());
    let empty: PitchSettings = ron::from_str("()").expect("empty RON");
    assert_eq!(empty, s);
    assert!(!PitchSettings { semitones: -2, ..s }.is_unset());
}

#[test]
fn the_engine_and_mode_read_back_and_unknown_names_read_as_the_defaults() {
    let spectral = |mode| PitchSettings {
        engine: PitchEngine::Spectral,
        mode,
        ..on(-2)
    };
    for (text, want) in [
        (
            "(enabled: true, semitones: -2, engine: spectral, mode: sustain)",
            spectral(PitchMode::Sustain),
        ),
        // Older builds' modes.
        (
            "(enabled: true, semitones: -2, engine: spectral, mode: quality)",
            spectral(PitchMode::Balanced),
        ),
        (
            "(enabled: true, semitones: -2, engine: spectral, mode: fast)",
            spectral(PitchMode::Ultrafast),
        ),
        (
            "(enabled: true, semitones: -2, engine: tight, mode: ultrafast)",
            PitchSettings {
                mode: PitchMode::Ultrafast,
                ..on(-2)
            },
        ),
        ("(enabled: true, semitones: -2, mode: balanced)", on(-2)),
        ("(enabled: true, semitones: -2, engine: granular)", on(-2)),
        (
            "(enabled: true, semitones: -2, engine: spectral, mode: ultra)",
            spectral(PitchMode::Sustain),
        ),
        (
            "(enabled: true, semitones: -2, engine: spectral, mode: mega)",
            spectral(PitchMode::Balanced),
        ),
    ] {
        let s: PitchSettings = ron::from_str(text).expect(text);
        assert_eq!(s, want, "{text}");
    }
    let saved = spectral(PitchMode::Ultrafast);
    let text = ron::to_string(&saved).expect("RON");
    assert_eq!(ron::from_str::<PitchSettings>(&text).expect(&text), saved);
}

#[test]
fn an_old_save_without_a_delay_plays_at_4_ms() {
    let s: PitchSettings =
        ron::from_str("(enabled: true, bypass: false, semitones: -2, cents: 0.0, wet: 1.0)")
            .expect("old RON");
    assert_eq!(s.delay_ms, 4.0);
    assert_eq!(s, on(-2));
}

#[test]
fn sanitized_clamps_every_field() {
    let wild = PitchSettings {
        enabled: true,
        bypass: true,
        semitones: 40,
        cents: f32::NAN,
        wet: 3.0,
        delay_ms: f32::NAN,
        engine: PitchEngine::Spectral,
        mode: PitchMode::Sustain,
    }
    .sanitized();
    assert_eq!(wild.semitones, PITCH_MAX_SEMITONES);
    assert_eq!(wild.cents, 0.0);
    assert_eq!(wild.wet, 1.0);
    assert_eq!(wild.delay_ms, DEFAULT_DELAY_MS);
    assert!(wild.enabled && wild.bypass);
    let low = PitchSettings {
        semitones: -100,
        cents: -400.0,
        wet: f32::NEG_INFINITY,
        delay_ms: 0.5,
        ..PitchSettings::default()
    }
    .sanitized();
    assert_eq!(low.semitones, PITCH_MIN_SEMITONES);
    assert_eq!(low.cents, PITCH_MIN_CENTS);
    assert_eq!(low.wet, 1.0);
    assert_eq!(low.delay_ms, PITCH_MIN_DELAY_MS);
    let delay = |delay_ms| {
        PitchSettings {
            delay_ms,
            ..PitchSettings::default()
        }
        .sanitized()
        .delay_ms
    };
    assert_eq!(delay(500.0), PITCH_MAX_DELAY_MS);
    assert_eq!(delay(f32::INFINITY), DEFAULT_DELAY_MS);
    assert_eq!(delay(12.4), 12.0, "whole ms");
}

#[test]
fn off_passes_the_input_bit_for_bit() {
    let rate = 48_000;
    let mut shifter = PitchShifter::new(rate, 0);
    shifter.update(
        PitchSettings {
            semitones: -5,
            ..PitchSettings::default()
        },
        rate,
    );
    let mut block: Vec<[f32; 4]> = (0..512)
        .map(|i| [(i as f32).sin(), (i as f32).cos(), 0.3, -0.3])
        .collect();
    let before = block.clone();
    shifter.begin_block();
    shifter.process(&mut block, 512);
    assert_eq!(block, before);
    assert_eq!(shifter.latency_samples(), 0);
}

#[test]
fn bypass_plays_the_delayed_input_and_does_not_step() {
    let rate = 48_000;
    let input = sine(rate, 196.0, 0.6);
    let mut shifter = PitchShifter::new(rate, 0);
    shifter.update(on(-5), rate);
    let flip = 40;
    let (out, latencies) = drive(&mut shifter, &input, 128, |index, s| {
        if index == flip {
            s.update(
                PitchSettings {
                    bypass: true,
                    ..on(-5)
                },
                rate,
            );
        }
    });
    let latency = spec(rate, DEFAULT_DELAY_MS).latency as usize;
    assert!(latencies.iter().all(|&l| l as usize == latency));
    // Once the crossfade is over, the delayed dry signal exactly.
    let settled = flip * 128 + (BYPASS_FADE_SECONDS * rate as f32) as usize + 1;
    for i in settled..out.len() {
        assert!((out[i] - input[i - latency]).abs() < 1e-6, "sample {i}");
    }
    // No step: the largest sample-to-sample move stays that of the sines.
    let step = out
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0f32, f32::max);
    let slope = 0.25 * TAU as f32 * 196.0 / rate as f32;
    assert!(step < 1.5 * slope, "a step of {step}");
}

/// Switches the stage on (or off, when `on_before`) at block 30 and checks
/// the dip: the latency changes only on the block of the switch, the
/// output is silent there, the gap stays within the dip, and the output
/// never holds both delays at once.
fn check_switch(on_before: bool, wet: f32) {
    let rate = 48_000;
    let block = 128;
    let input = sine(rate, 220.0, 1.0);
    let settings = |enabled: bool| PitchSettings {
        enabled,
        wet,
        ..on(0)
    };
    let latency = |enabled: bool| {
        if enabled {
            spec(rate, DEFAULT_DELAY_MS).latency
        } else {
            0
        }
    };
    let on_after = !on_before;
    let mut shifter = PitchShifter::new(rate, 0);
    shifter.update(settings(on_before), rate);
    let (out, latencies) = drive(&mut shifter, &input, block, |index, s| {
        if index == 30 {
            s.update(settings(on_after), rate);
        }
    });
    let switch = latencies
        .iter()
        .position(|&l| l == latency(on_after))
        .expect("the latency moves");
    assert!(latencies[..switch].iter().all(|&l| l == latency(on_before)));
    assert!(latencies[switch..].iter().all(|&l| l == latency(on_after)));
    // At the switch both sides are silent.
    let fade = (SWITCH_FADE_SECONDS * rate as f32) as usize;
    assert!(
        out[switch * block - 1].abs() < 1e-6 && out[switch * block].abs() < 0.5 / fade as f32,
        "not silent at the switch"
    );
    // The quiet span, on a 1 ms envelope after the request: the fades
    // either side and at most one block.
    let env = score::envelope(&out, rate);
    let after = 30 * block;
    let quiet = |x: &f64| *x < 0.02;
    let first_quiet = after + env[after..].iter().position(quiet).expect("a dip");
    let last_quiet = env.len() - 1 - env.iter().rev().position(quiet).unwrap();
    let ms = rate as usize / 1000;
    let span = last_quiet - first_quiet;
    let warm = if on_after {
        spec(rate, DEFAULT_DELAY_MS).warm_up()
    } else {
        0
    };
    assert!(
        span <= 2 * fade + block + 2 * ms,
        "quiet for {span} samples"
    );
    assert!(
        first_quiet >= 30 * block + warm,
        "the dip came before the warm-up"
    );
    // The sine's amplitude never exceeds one path: no two delays summed.
    assert!(out.iter().all(|x| x.abs() <= 0.2501), "two delays summed");
}

#[test]
fn switching_on_and_off_dips_without_a_gap_or_a_comb() {
    for wet in [1.0, 0.5] {
        check_switch(false, wet);
        check_switch(true, wet);
    }
}

/// A restart turns the stage off at once, and the next update adopts the
/// settings without a dip.
#[test]
fn a_restart_is_off_at_once_and_the_next_update_needs_no_dip() {
    let rate = 48_000;
    let block = 128;
    let input = signal(rate, 0.1);
    let mut shifter = PitchShifter::new(rate, 0);
    shifter.update(on(-5), rate);
    drive(&mut shifter, &input, block, |_, _| {});
    shifter.restart();
    assert!(!shifter.is_active());
    assert_eq!(shifter.latency_samples(), 0);
    assert_eq!(shifter.tight.samples(), 0);
    // Off: the input passes bit for bit.
    let (out, _) = drive(&mut shifter, &input[..block], block, |_, _| {});
    assert_eq!(out, input[..block]);
    // The next update lands at once, on the full level.
    shifter.update(on(-2), rate);
    let (_, latencies) = drive(&mut shifter, &input, block, |_, _| {});
    let want = spec(rate, DEFAULT_DELAY_MS).latency;
    assert!(latencies.iter().all(|&l| l == want));
    assert!(shifter.switch == Switch::Steady);
    assert!(shifter.fade.is_settled_at(1.0));
}

#[test]
fn a_rate_change_fades_out_then_in_with_the_new_latency() {
    let (low, high) = (192, 768);
    let input = sine(48_000, 220.0, 1.0);
    let mut shifter = PitchShifter::new(48_000, 1);
    shifter.update(on(-2), 48_000);
    let (out, latencies) = drive(&mut shifter, &input, 256, |index, s| {
        let next = match index {
            40 => 192_000,
            100 => 48_000,
            _ => return,
        };
        s.update(on(-2), next);
    });
    let lands = |from: usize, want: u32| {
        (from..from + 16)
            .find(|&index| latencies[index] == want)
            .expect("the rate lands")
    };
    assert_eq!(latencies[39], low);
    // The old sizes play on while they fade out: no cut, no step.
    assert_eq!(latencies[40], low);
    assert!(out[40 * 256..40 * 256 + 64].iter().any(|x| x.abs() > 0.05));
    let steps = |from: usize, to: usize| {
        out[from * 256..to * 256]
            .windows(2)
            .map(|pair| (pair[1] - pair[0]).abs())
            .fold(0.0f32, f32::max)
    };
    let up = lands(40, high);
    assert!(latencies[up..100].iter().all(|&l| l == high));
    let down = lands(100, low);
    assert!(latencies[down..].iter().all(|&l| l == low));
    assert!(out.iter().all(|x| x.is_finite()));
    // Up to the landing, at most the sine's own step at 48 kHz:
    // 0.25 · 2π · 220 / 48000.
    assert!(steps(39, up) < 0.008, "{}", steps(39, up));
    assert!(steps(99, down) < 0.008, "{}", steps(99, down));
    // Fades in from silence on a line that has filled since the clear, so
    // the input does not step in after `D` of silence.
    assert!(out[up * 256].abs() < 1e-3 && out[down * 256].abs() < 1e-3);
    assert!(out[up * 256..up * 256 + 1024]
        .iter()
        .any(|x| x.abs() > 0.01));
    assert!(steps(up, up + 20) < 0.008, "{}", steps(up, up + 20));
    assert!(steps(down, down + 20) < 0.008, "{}", steps(down, down + 20));
    // And is back to the sine's level later on.
    assert!(out[160 * 256..].iter().any(|x| x.abs() > 0.2));
}

/// A delay change plays like a rate change: the old delay fades out, the
/// new latency lands on the block after the silence, and the input fades
/// in at the new delay without a step.  The delay line is kept, so it fades
/// in on the input, not on silence.
#[test]
fn a_delay_change_fades_out_then_in_with_the_new_latency() {
    let rate = 48_000;
    let block = 256;
    let input = sine(rate, 220.0, 1.0);
    let at = |delay_ms| PitchSettings { delay_ms, ..on(0) };
    let latency = |delay_ms| {
        pitch_latency_samples(
            PitchSettings {
                delay_ms,
                ..PitchSettings::default()
            },
            rate,
        )
    };
    let (short, long) = (latency(4.0), latency(PITCH_MAX_DELAY_MS));
    let mut shifter = PitchShifter::new(rate, 0);
    shifter.update(at(4.0), rate);
    let (out, latencies) = drive(&mut shifter, &input, block, |index, s| match index {
        40 => s.update(at(PITCH_MAX_DELAY_MS), rate),
        100 => s.update(at(4.0), rate),
        _ => {}
    });
    let lands = |from: usize, want: u32| {
        (from..from + 16)
            .find(|&index| latencies[index] == want)
            .expect("the delay lands")
    };
    assert_eq!(latencies[40], short, "the old delay plays on");
    let up = lands(40, long);
    assert!(latencies[..up].iter().all(|&l| l == short));
    assert!(latencies[up..100].iter().all(|&l| l == long));
    let down = lands(100, short);
    assert!(latencies[down..].iter().all(|&l| l == short));
    for landed in [up, down] {
        assert!(out[landed * block - 1].abs() < 1e-6, "silent at the change");
    }
    // No step anywhere: at most the sine's own, 0.25 · 2π · 220 / 48000.
    let step = out
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).abs())
        .fold(0.0f32, f32::max);
    assert!(step < 0.008, "a step of {step}");
    // Settled, at 0 semitones, the input delayed by what is reported.
    let fade = (SWITCH_FADE_SECONDS * rate as f32) as usize;
    for (from, to, delay) in [(up, 100, long), (down, latencies.len(), short)] {
        for i in from * block + fade + 1..(to * block).min(out.len()) {
            let want = input[i - delay as usize];
            assert!(
                (out[i] - want).abs() < 1e-4,
                "sample {i}: {} vs {want}",
                out[i]
            );
        }
    }
}

/// Off and a delay change that land together: the fade-out ends with the
/// new size pending and `next` false, so the dry signal must fade back in,
/// not step to it at the block boundary.
#[test]
fn switching_off_while_a_delay_change_lands_fades_the_dry_back_in() {
    let rate = 48_000;
    let block = 256;
    // Constant, so any step left after the line has filled is the switch's.
    let input = vec![0.25f32; rate as usize];
    let mut shifter = PitchShifter::new(rate, 0);
    shifter.update(on(0), rate);
    let (out, _) = drive(&mut shifter, &input, block, |index, s| {
        if index == 30 {
            s.update(
                PitchSettings {
                    enabled: false,
                    delay_ms: PITCH_MAX_DELAY_MS / 2.0,
                    ..on(0)
                },
                rate,
            );
        }
    });
    let start = pitch_latency_samples(PitchSettings::default(), rate) as usize + 1;
    let step = out[start..]
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).abs())
        .fold(0.0f32, f32::max);
    assert!(step < 1e-3, "a step of {step}");
}

/// A session can switch a live path's stage on and change its delay in one
/// update: the stage warms up from silence, never plays what the line held
/// when it was last on.
#[test]
fn switching_on_with_a_new_delay_does_not_play_old_input() {
    let rate = 48_000;
    let block = 256;
    let tone = sine(rate, 220.0, 0.5);
    let silence = vec![0.0f32; rate as usize];
    let mut shifter = PitchShifter::new(rate, 0);
    shifter.update(on(0), rate);
    let _ = drive(&mut shifter, &tone, block, |_, _| {});
    shifter.update(PitchSettings::default(), rate);
    let _ = drive(&mut shifter, &silence, block, |_, _| {});
    shifter.update(
        PitchSettings {
            delay_ms: 20.0,
            ..on(0)
        },
        rate,
    );
    let (out, _) = drive(&mut shifter, &silence, block, |_, _| {});
    let loudest = out.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    assert!(loudest < 1e-6, "old input played back at {loudest}");
}

#[test]
fn processing_allocates_and_frees_nothing() {
    let mut shifter = PitchShifter::new(48_000, 3);
    let input = signal(48_000, 0.05);
    let mut buf = vec![[0.0f32; 4]; 256];
    let mut changes = Vec::new();
    for (semitones, wet, bypass) in [(-5, 1.0, false), (-12, 0.5, false), (7, 0.8, true)] {
        changes.push((
            PitchSettings {
                wet,
                bypass,
                ..on(semitones)
            },
            48_000,
        ));
    }
    // The rate moved as the PipeWire duplex filter does: in place.
    changes.push((on(-2), 192_000));
    changes.push((on(12), 192_000));
    changes.push((on(-2), 48_000));
    changes.push((
        PitchSettings {
            delay_ms: PITCH_MAX_DELAY_MS,
            ..on(5)
        },
        48_000,
    ));
    changes.push((
        PitchSettings {
            delay_ms: PITCH_MAX_DELAY_MS,
            ..on(-12)
        },
        192_000,
    ));
    changes.push((PitchSettings::default(), 48_000));
    for (settings, rate) in changes {
        for _ in 0..12 {
            for (frame, &x) in buf.iter_mut().zip(&input) {
                *frame = [x, x, 0.0, 0.0];
            }
            let guard = RtGuard::enter();
            shifter.update(settings, rate);
            shifter.begin_block();
            shifter.process(&mut buf, 256);
            assert_eq!(guard.allocations(), 0, "{settings:?} @{rate} allocated");
            assert_eq!(guard.frees(), 0, "{settings:?} @{rate} freed");
            drop(guard);
        }
    }
}

#[test]
fn the_preallocation_stays_within_its_budget() {
    let shifter = PitchShifter::new(48_000, 0);
    let kb = shifter.tight.bytes() as f64 / 1e3;
    let mb = (shifter.tight.bytes() + shifter.spectral.bytes()) as f64 / 1e6;
    println!("one shifter preallocates {mb:.2} MB, Tight {kb:.1} kB");
    assert!((130.0..150.0).contains(&kb), "Tight {kb:.1} kB");
    // The module docs quote this figure.
    assert!(
        (5.9..6.6).contains(&mb),
        "{mb:.2} MB; update the //! of pitch.rs"
    );
}

/// Beating partials (two polarisations, a fraction of a Hz to a few Hz
/// apart) keep their beat through a drop: the shifted partial's level
/// follows the input's.
#[test]
fn a_beating_note_keeps_its_beat() {
    let rate = 48_000;
    let c = case("a2beat");
    let input = render(&c, rate, 1.0);
    let (out, _) = run(&input, rate, -5, 0, 256);
    let row = evaluate(&c, Engine::Tight, -5, rate, &input, &out, None);
    let (mean, min, n) = row.beat.expect("beating partials scored");
    assert!(n >= 5 && mean > 0.9 && min > 0.8, "{row}");
    assert!(failures(&row).is_empty(), "{row}");
}

/// A longer *Delay* leaves the ringing chord's splices further apart, so it
/// choruses less: the roughness above the reference drops from the 4 ms
/// default to 20 ms.
#[test]
fn a_longer_delay_choruses_a_ringing_chord_less() {
    let rate = 48_000;
    let semitones = -5;
    let chord = case("emaj7");
    let input = render(&chord, rate, 1.0);
    let ratio = gate::ratio(semitones);
    let reference = render(&chord, rate, ratio);
    let na = gate::scoring_na(rate);
    let excess = |delay_ms| {
        let (out, _) = run_delay(&input, rate, semitones, delay_ms, 0, 256);
        let rough = score::roughness(&chord, &out, &reference, rate, ratio, na);
        assert!(!rough.is_empty(), "no partial resolved at {delay_ms} ms");
        rough
            .iter()
            .map(|(out, reference)| out - reference)
            .sum::<f64>()
            / rough.len() as f64
    };
    let short = excess(4.0);
    let long = excess(20.0);
    assert!(long < short, "4 ms {short:.2} dB, 20 ms {long:.2} dB");
}

/// The always-on part of the §4.3 gate: single notes, power chords, the
/// chugs and the stops, at 48 kHz.  The release harness runs the whole
/// table.
#[test]
fn the_phase_0_gate_holds_for_the_synthetic_cases() {
    let rate = 48_000;
    let mut rows = Vec::new();
    for (name, semitones) in [
        ("e2", -5),
        ("a2", -2),
        ("b1", -7),
        ("e5", -5),
        ("chugs", -7),
        ("chugs200", -5),
        ("lowpower", -7),
        ("stops", -5),
        ("e2", -12),
    ] {
        let c = case(name);
        let input = render(&c, rate, 1.0);
        let (out, _) = run(&input, rate, semitones, 0, 256);
        rows.push(evaluate(
            &c,
            Engine::Tight,
            semitones,
            rate,
            &input,
            &out,
            None,
        ));
    }
    for row in &rows {
        assert!(failures(row).is_empty(), "{row}");
    }
}
