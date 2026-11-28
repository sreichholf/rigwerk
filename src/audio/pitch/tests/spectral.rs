//! The spectral engine through the stage, and the switches between
//! engines and modes.

use super::*;
use gate::run_settings;

fn spectral(mode: PitchMode, semitones: i8) -> PitchSettings {
    PitchSettings {
        engine: PitchEngine::Spectral,
        mode,
        ..on(semitones)
    }
}

fn latency(settings: PitchSettings, rate: u32) -> u32 {
    if settings.enabled {
        pitch_latency_samples(settings, rate)
    } else {
        0
    }
}

#[test]
fn every_mode_takes_its_sizes_from_the_table() {
    for mode in PitchMode::ALL {
        let base = mode.spec(48_000);
        assert_eq!(base.latency as usize, base.ns - 1);
        assert_eq!(base.n, base.na.next_power_of_two());
        assert_eq!(mode.spec(44_100), base);
        assert_eq!(mode.spec(96_000).na, 2 * base.na);
        assert_eq!(mode.spec(192_000).ns, 4 * base.ns);
        assert_eq!(mode.spec(384_000), mode.spec(192_000));
        assert_eq!(mode.spec(22_050), base);
        assert!(base.ns <= base.short && base.short <= base.na);
        assert_eq!(mode.spec(192_000).short, 4 * base.short);
    }
    // A held note's window is twice an onset's.
    assert_eq!(PitchMode::Ultrafast.spec(48_000).na, 3072);
    assert_eq!(PitchMode::Ultrafast.spec(48_000).short, 1024);
    assert_eq!(PitchMode::Balanced.spec(48_000).short, 2048);
    assert_eq!(PitchMode::Ultrafast.spec(48_000).latency, 255);
    assert_eq!(PitchMode::Balanced.spec(48_000).latency, 511);
    assert_eq!(PitchMode::Sustain.spec(48_000).latency, 1023);
    assert_eq!(PitchMode::Sustain.spec(192_000).n, 32_768);
    // The delay is Tight's only.
    let settings = PitchSettings {
        delay_ms: PITCH_MAX_DELAY_MS,
        ..spectral(PitchMode::Ultrafast, 0)
    };
    assert_eq!(pitch_latency_samples(settings, 48_000), 255);
}

#[test]
fn at_zero_semitones_every_mode_is_the_input_delayed() {
    let rate = 48_000;
    let input = signal(rate, 0.5);
    for mode in PitchMode::ALL {
        let (out, latency) = run_settings(&input, rate, spectral(mode, 0), 1, 128);
        assert_eq!(latency, mode.spec(rate).latency);
        let worst = input
            .iter()
            .zip(&out)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(worst < 1e-4, "{mode:?}: reconstruction error {worst}");
    }
}

#[test]
fn the_spectral_latency_is_minimal_not_only_reported() {
    for rate in RATES {
        for mode in PitchMode::ALL {
            let spec = mode.spec(rate);
            let mut input = vec![0.0f32; spec.na * 2 + spec.ns * 2];
            let at = spec.na + 7;
            input[at] = 0.5;
            let mut shifter = PitchShifter::new(rate, 0);
            shifter.update(spectral(mode, 0), rate);
            let (out, latencies) = drive(&mut shifter, &input, 256, |_, _| {});
            assert!(latencies.iter().all(|&l| l == spec.latency));
            let peak = out
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
                .map(|(i, _)| i)
                .unwrap();
            assert_eq!(peak - at, spec.latency as usize, "{mode:?} @{rate}");
            assert!((out[peak] - 0.5).abs() < 1e-4);
        }
    }
}

#[test]
fn a_sine_lands_on_its_target_in_every_mode() {
    let rate = 48_000;
    for mode in PitchMode::ALL {
        // Source and target at least three bins of Na from DC.
        let lowest = 3.0 * rate as f64 / mode.spec(rate).na as f64;
        for freq in [82.41, 110.0, 196.0, 330.0] {
            for semitones in [-2i8, -5, -12] {
                let target = freq * gate::ratio(semitones);
                if target < lowest {
                    continue;
                }
                let input = sine(rate, freq, 0.8);
                let (out, _) = run_settings(&input, rate, spectral(mode, semitones), 0, 256);
                let got = measured_pitch(&out, rate, target, target * 0.03);
                let error = cents(got, target);
                assert!(
                    error.abs() < 2.0,
                    "{mode:?} {freq} Hz {semitones:+}: {error:.2} cents"
                );
            }
        }
    }
}

#[test]
fn resolved_chords_move_every_partial_and_leave_none() {
    let rate = 48_000;
    for (mode, freqs) in [
        (PitchMode::Ultrafast, vec![300.0, 520.0]),
        // Only a held note's long window resolves these.
        (PitchMode::Ultrafast, vec![110.0, 165.0, 220.0]),
        (PitchMode::Balanced, vec![196.0, 262.0, 330.0]),
        (PitchMode::Balanced, vec![82.41, 123.48, 164.81]),
        (PitchMode::Sustain, vec![82.41, 123.48, 164.81]),
    ] {
        let input: Vec<f32> = (0..(0.8 * rate as f64) as usize)
            .map(|i| {
                let t = i as f64 / rate as f64;
                freqs
                    .iter()
                    .map(|f| 0.15 * (TAU * f * t).sin())
                    .sum::<f64>() as f32
            })
            .collect();
        let semitones = -2;
        let ratio = gate::ratio(semitones);
        let (out, _) = run_settings(&input, rate, spectral(mode, semitones), 0, 256);
        let nfft = 1 << 15;
        let spec = score::spectrum(&out, out.len() - nfft / 2, nfft / 2, nfft);
        for &f in &freqs {
            let target = f * ratio;
            let (got, mag) =
                score::peak_near(&spec, nfft, rate as f64, target, 3.0).expect("target");
            assert!(cents(got, target).abs() < 5.0, "{mode:?} {f}: {got}");
            assert!(mag > 0.15 * 0.8, "{mode:?} {f}: target level {mag}");
            let bin = (f / rate as f64 * nfft as f64).round() as usize;
            let left = spec[bin - 2..=bin + 2].iter().copied().fold(0.0, f64::max);
            assert!(left < 0.15 * 0.03, "{mode:?} {f}: leftover {left}");
        }
    }
}

#[test]
fn the_spectral_level_holds_across_shifts() {
    let rate = 48_000;
    let chord = case("e7s9");
    let input = render(&chord, rate, 1.0);
    for semitones in [-12i8, -7, -2, 5] {
        let settings = spectral(PitchMode::Balanced, semitones);
        let (out, _) = run_settings(&input, rate, settings, 0, 256);
        let level = score::level_error(&chord, &input, &out, rate);
        assert!(level.abs() <= 1.0, "{semitones:+}: {level:.2} dB");
    }
}

#[test]
fn the_spectral_output_does_not_depend_on_the_block_size() {
    let rate = 48_000;
    let input = signal(rate, 0.3);
    let settings = spectral(PitchMode::Balanced, -5);
    for slot in [0, 2] {
        let reference = run_settings(&input, rate, settings, slot, 32).0;
        for block in [64, 128, 1024] {
            let out = run_settings(&input, rate, settings, slot, block).0;
            assert!(out == reference, "slot {slot}, {block}-frame blocks differ");
        }
    }
    let other = run_settings(&input, rate, settings, 1, 32).0;
    assert!(other != run_settings(&input, rate, settings, 0, 32).0);
}

#[test]
fn spectral_silence_in_is_silence_out() {
    let rate = 48_000;
    let input = vec![0.0f32; rate as usize / 2];
    for mode in PitchMode::ALL {
        let (out, _) = run_settings(&input, rate, spectral(mode, -5), 0, 256);
        assert!(out.iter().all(|&x| x == 0.0), "{mode:?}");
    }
}

#[test]
fn the_spectral_engine_recovers_from_a_non_finite_input() {
    let rate = 48_000;
    let settings = spectral(PitchMode::Balanced, -5);
    let clean = run_settings(&sine(rate, 196.0, 1.0), rate, settings, 0, 256).0;
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let mut input = sine(rate, 196.0, 1.0);
        input[rate as usize / 4] = bad;
        let (out, _) = run_settings(&input, rate, settings, 0, 256);
        assert!(out.iter().all(|x| x.is_finite()), "{bad} stuck");
        let tail = |x: &[f32]| score::rms(x, rate, 0.8, 0.95);
        let db = 20.0 * (tail(&out) / tail(&clean)).log10();
        assert!(db.abs() < 0.5, "{bad}: {db:.2} dB after recovery");
    }
}

#[test]
fn only_the_selected_engine_runs() {
    let rate = 48_000;
    let input = signal(rate, 0.2);
    for settings in [spectral(PitchMode::Balanced, -5), on(-5)] {
        let mut shifter = PitchShifter::new(rate, 0);
        shifter.update(settings, rate);
        drive(&mut shifter, &input, 256, |_, _| {});
        let tight = settings.engine == PitchEngine::Tight;
        assert_eq!(shifter.tight.samples() > 0, tight, "{settings:?}");
        assert_eq!(shifter.spectral.ran(), !tight, "{settings:?}");
    }
}

/// Switches from `from` to `to` at block 30 and checks the dip: the
/// latency changes only on the block of the switch, the output is silent
/// there, the gap stays within the dip, and the output never holds both
/// delays at once.
fn check_switch(from: PitchSettings, to: PitchSettings) {
    let rate = 48_000;
    let block = 128;
    let input = sine(rate, 220.0, 1.0);
    let (before, after) = (latency(from, rate), latency(to, rate));
    assert_ne!(before, after, "the test needs the latency to move");
    let mut shifter = PitchShifter::new(rate, 0);
    shifter.update(from, rate);
    let (out, latencies) = drive(&mut shifter, &input, block, |index, s| {
        if index == 30 {
            s.update(to, rate);
        }
    });
    let switch = latencies
        .iter()
        .position(|&l| l == after)
        .expect("the latency moves");
    assert!(latencies[..switch].iter().all(|&l| l == before));
    assert!(latencies[switch..].iter().all(|&l| l == after));
    let fade = (SWITCH_FADE_SECONDS * rate as f32) as usize;
    assert!(
        out[switch * block - 1].abs() < 1e-6 && out[switch * block].abs() < 0.5 / fade as f32,
        "{from:?} → {to:?}: not silent at the switch"
    );
    let env = score::envelope(&out, rate);
    let start = 30 * block;
    let quiet = |x: &f64| *x < 0.02;
    let first_quiet = start + env[start..].iter().position(quiet).expect("a dip");
    let last_quiet = env.len() - 1 - env.iter().rev().position(quiet).unwrap();
    let ms = rate as usize / 1000;
    let span = last_quiet - first_quiet;
    assert!(
        span <= 2 * fade + block + 2 * ms,
        "{from:?} → {to:?}: quiet for {span} samples"
    );
    assert!(
        out.iter().all(|x| x.abs() <= 0.2501),
        "{from:?} → {to:?}: two delays summed"
    );
}

#[test]
fn a_mode_or_engine_change_dips_without_a_gap_or_a_comb() {
    let off = PitchSettings::default();
    for wet in [1.0, 0.5] {
        let ultrafast = PitchSettings {
            wet,
            ..spectral(PitchMode::Ultrafast, 0)
        };
        let sustain = PitchSettings {
            wet,
            ..spectral(PitchMode::Sustain, 0)
        };
        let tight = PitchSettings { wet, ..on(0) };
        check_switch(ultrafast, sustain);
        check_switch(sustain, ultrafast);
        check_switch(tight, sustain);
        check_switch(ultrafast, tight);
        check_switch(off, ultrafast);
        check_switch(ultrafast, off);
    }
}

/// A delay change does not touch a spectral path: its latency stays and
/// it does not dip.  Switching to Tight then plays the new delay.
#[test]
fn a_delay_change_on_the_spectral_engine_lands_without_a_dip() {
    let rate = 48_000;
    let input = sine(rate, 220.0, 0.6);
    let balanced = spectral(PitchMode::Balanced, -2);
    let longer = PitchSettings {
        delay_ms: 20.0,
        ..balanced
    };
    let mut shifter = PitchShifter::new(rate, 0);
    shifter.update(balanced, rate);
    let (out, latencies) = drive(&mut shifter, &input, 128, |index, s| {
        if index == 30 {
            s.update(longer, rate);
        }
    });
    assert!(latencies.iter().all(|&l| l == 511));
    let env = score::envelope(&out, rate);
    assert!(env[30 * 128..].iter().all(|&x| x > 0.05), "it dipped");
    let tight = PitchSettings {
        engine: PitchEngine::Tight,
        ..longer
    };
    let (_, latencies) = drive(&mut shifter, &input, 128, |index, s| {
        if index == 0 {
            s.update(tight, rate);
        }
    });
    assert_eq!(*latencies.last().unwrap(), spec(rate, 20.0).latency);
}

#[test]
fn a_rate_change_fades_the_spectral_engine_in_with_the_new_latency() {
    let input = sine(48_000, 220.0, 1.0);
    let settings = spectral(PitchMode::Balanced, -2);
    let mut shifter = PitchShifter::new(48_000, 1);
    shifter.update(settings, 48_000);
    let (out, latencies) = drive(&mut shifter, &input, 256, |index, s| {
        let next = match index {
            40 => 192_000,
            100 => 48_000,
            _ => return,
        };
        s.update(settings, next);
    });
    assert_eq!(latencies[39], 511);
    assert!(latencies[60..100].iter().all(|&l| l == 2047));
    assert_eq!(*latencies.last().unwrap(), 511);
    assert!(out.iter().all(|x| x.is_finite()));
}

#[test]
fn switching_engines_allocates_and_frees_nothing() {
    let mut shifter = PitchShifter::new(48_000, 3);
    let input = signal(48_000, 0.05);
    let mut buf = vec![[0.0f32; 4]; 256];
    let changes = [
        (spectral(PitchMode::Ultrafast, -5), 48_000),
        (spectral(PitchMode::Sustain, -12), 48_000),
        (on(-2), 48_000),
        (spectral(PitchMode::Balanced, 7), 192_000),
        (spectral(PitchMode::Sustain, -2), 192_000),
        (spectral(PitchMode::Ultrafast, 3), 44_100),
        (PitchSettings::default(), 48_000),
    ];
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

/// An interface's noise floor fits no peak: without the median floor it
/// fitted hundreds per frame, several times the work of a note.  A note
/// over that noise keeps its peaks and its pitch.
#[test]
fn a_noise_floor_fits_no_peak_and_a_note_over_it_still_shifts() {
    let rate = 48_000;
    let mut rng = material::Lcg(3);
    let noise: Vec<f32> = (0..rate as usize)
        .map(|_| ((rng.next() - 0.5) * 3.4e-3) as f32)
        .collect();
    let tone: Vec<f32> = sine(rate, 196.0, 1.0)
        .iter()
        .zip(&noise)
        .map(|(a, b)| a + b)
        .collect();
    let target = 196.0 * gate::ratio(-5);
    for mode in PitchMode::ALL {
        let mut shifter = PitchShifter::new(rate, 0);
        shifter.update(spectral(mode, -5), rate);
        drive(&mut shifter, &noise, 128, |_, _| {});
        assert_eq!(shifter.spectral.peaks(mode.index()), 0, "{mode:?}");
        let (out, _) = drive(&mut shifter, &tone, 128, |_, _| {});
        assert!(shifter.spectral.peaks(mode.index()) > 0, "{mode:?}");
        let got = measured_pitch(&out, rate, target, target * 0.03);
        assert!(cents(got, target).abs() < 2.0, "{mode:?}: {got} Hz");
    }
}

#[test]
fn a_held_note_takes_the_long_window_and_an_onset_or_a_stop_the_short() {
    let rate = 48_000;
    let silence = vec![0.0f32; rate as usize / 2];
    for mode in [
        PitchMode::Ultrafast,
        PitchMode::Balanced,
        PitchMode::Sustain,
    ] {
        let index = mode.index();
        let mut shifter = PitchShifter::new(rate, 0);
        shifter.update(spectral(mode, -5), rate);
        drive(&mut shifter, &silence, 128, |_, _| {});
        assert!(!shifter.spectral.short(index), "{mode:?}: silence");
        drive(&mut shifter, &sine(rate, 82.41, 0.01), 128, |_, _| {});
        assert!(shifter.spectral.short(index), "{mode:?}: onset");
        drive(&mut shifter, &sine(rate, 82.41, 0.5), 128, |_, _| {});
        assert!(!shifter.spectral.short(index), "{mode:?}: held");
        // The window choice weighs the newest 2·Ns, at least 16 ms.
        let stop = (2 * mode.spec(rate).ns).max(rate as usize / 50);
        drive(&mut shifter, &silence[..stop], 128, |_, _| {});
        assert!(shifter.spectral.short(index), "{mode:?}: stop");
    }
}

#[test]
fn at_zero_semitones_window_switches_still_pass_the_input() {
    let rate = 48_000;
    // Bursts with hard edges, so the frames keep switching windows.
    let input: Vec<f32> = signal(rate, 1.0)
        .iter()
        .enumerate()
        .map(|(i, &x)| if (i / 2400) % 3 == 0 { 0.0 } else { x })
        .collect();
    for mode in PitchMode::ALL {
        let (out, _) = run_settings(&input, rate, spectral(mode, 0), 1, 128);
        let worst = input
            .iter()
            .zip(&out)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(worst < 1e-4, "{mode:?}: reconstruction error {worst}");
    }
}

/// The held partial's power at `freq` through a Hann of 2048 from `start`.
fn partial_power(signal: &[f32], rate: u32, freq: f64, start: usize) -> f64 {
    let n = 2048;
    let (mut re, mut im) = (0.0, 0.0);
    for (i, &x) in signal[start..start + n].iter().enumerate() {
        let w = (std::f64::consts::PI * (i as f64 + 0.5) / n as f64)
            .sin()
            .powi(2);
        let phase = TAU * freq * (start + i) as f64 / rate as f64;
        re += x as f64 * w * phase.cos();
        im -= x as f64 * w * phase.sin();
    }
    re * re + im * im
}

/// A note picked over a ringing string restarts its own phases, not the
/// string's: restarting both dipped the string by 10–20 dB.
#[test]
fn a_new_note_leaves_a_ringing_string_alone() {
    use material::{midi, render, string, Case, Kind};
    let rate = 48_000;
    let case = |notes| Case {
        name: "legato",
        kind: Kind::Notes,
        seconds: 1.4,
        notes,
        sustain: (0.3, 1.3),
        signal: None,
        merge_hz: 0.0,
    };
    // A2 rings; F4's partials stay clear of A2's first two.
    let held = || string(midi(45), 0.05, 0.5, 4.0, 1);
    let alone = render(&case(vec![held()]), rate, 1.0);
    let both = render(
        &case(vec![held(), string(midi(65), 0.8, 0.5, 1.0, 2)]),
        rate,
        1.0,
    );
    for mode in PitchMode::ALL {
        let (a, _) = run_settings(&alone, rate, spectral(mode, -5), 0, 256);
        let (b, _) = run_settings(&both, rate, spectral(mode, -5), 0, 256);
        for harmonic in [1.0, 2.0] {
            let freq = 110.0 * harmonic * gate::ratio(-5);
            let mut worst = 0.0f64;
            for start in (0.75 * rate as f64) as usize..(1.15 * rate as f64) as usize {
                if start % 128 != 0 {
                    continue;
                }
                let ratio =
                    partial_power(&b, rate, freq, start) / partial_power(&a, rate, freq, start);
                worst = worst.max((10.0 * ratio.log10()).abs());
            }
            assert!(worst < 3.0, "{mode:?} harmonic {harmonic}: {worst:.1} dB");
        }
    }
}
