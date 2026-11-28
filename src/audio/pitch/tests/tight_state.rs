use std::f64::consts::TAU;

use super::super::tight::{rate_scale, spec, TightState, BUFFER_LEN};
use super::super::{DEFAULT_DELAY_MS, PITCH_MAX_DELAY_MS};
use super::material::Lcg;

/// A state at `rate` snapped to `semitones` plus `cents`.
fn state(rate: u32, semitones: f32, cents: f32) -> TightState {
    let mut state = TightState::new(rate, DEFAULT_DELAY_MS);
    state.set_octaves((semitones + cents / 100.0) / 12.0, true);
    state
}

/// One sample on the shifter's hop grid (slot 0); the wet and dry output.
fn tick(state: &mut TightState, rate: u32, x: f32) -> (f32, f32) {
    let due = (state.samples() + 1) % spec(rate, DEFAULT_DELAY_MS).hop as i64 == 0;
    state.tick(x, due)
}

/// Noise low-passed at about 1.5 kHz (four one-poles), so a head's
/// interpolation keeps its level.
fn noise(rate: u32, seconds: f64, level: f64, seed: u64) -> Vec<f32> {
    let mut rng = Lcg(seed);
    let k = 1.0 - (-TAU * 1500.0 / rate as f64).exp();
    let mut z = [0.0f64; 4];
    (0..(seconds * rate as f64) as usize)
        .map(|_| {
            let mut x = rng.next() - 0.5;
            for z in &mut z {
                *z += k * (x - *z);
                x = *z;
            }
            (level * 8.0 * x) as f32
        })
        .collect()
}

/// A struck chord of `notes` (Hz, eight harmonics each, 0.3 s decay) at
/// every multiple of `every` seconds from `from` on, over `input`.
fn strike(input: &mut [f32], rate: u32, notes: &[f64], from: f64, every: f64) {
    // Five decay constants: below -40 dB, so each strike stops there.
    let one: Vec<f32> = (0..(1.5 * rate as f64) as usize)
        .map(|i| {
            let t = i as f64 / rate as f64;
            let ring: f64 = notes
                .iter()
                .flat_map(|f| (1..=8).map(move |k| (*f, k as f64)))
                .map(|(f, k)| (TAU * f * k * t).sin() / k)
                .sum();
            (0.3 * ring * (-t / 0.3).exp()) as f32
        })
        .collect();
    let mut onset = from;
    while onset < input.len() as f64 / rate as f64 {
        let start = (onset * rate as f64) as usize;
        for (slot, x) in input[start..].iter_mut().zip(&one) {
            *slot += x;
        }
        onset += every;
    }
}

/// An attack on an upward shift leaves the head near the input; the next
/// splice's fade must end before the outgoing head reads past it, or it
/// reads the stale end of the line.
#[test]
fn no_audible_head_reads_nearer_the_input_than_the_minimum() {
    let chord = [82.41, 123.47, 164.81, 207.65, 246.94, 329.63];
    let power = [82.41, 123.47];
    for (rate, semitones, cents, notes) in [
        (48_000, 12.0, 0.0, &chord[..]),
        (48_000, 7.0, 0.0, &chord[..]),
        (48_000, 5.0, 0.0, &power[..]),
        (48_000, 12.0, 0.0, &[282.0][..]),
        (48_000, 12.0, 50.0, &chord[..]),
        (96_000, 12.0, 50.0, &chord[..]),
    ] {
        // A second of noise first, so the line holds stale audio.
        let mut input = noise(rate, 3.0, 0.1, 3);
        strike(&mut input, rate, notes, 1.0, 0.4);
        let mut state = state(rate, semitones, cents);
        let min_delay = (12 * rate_scale(rate)) as f64;
        let mut nearest = f64::INFINITY;
        for &x in &input {
            tick(&mut state, rate, x);
            if let Some((near, _)) = state.read_delay() {
                nearest = nearest.min(near);
            }
        }
        assert!(
            nearest >= min_delay,
            "{rate} Hz {semitones:+}.{cents} on {notes:?}: a head read {nearest:.1} samples behind the input"
        );
    }
}

/// The estimate hears the newest samples: a new note's period replaces the
/// last one's once the 24 ms window holds only the new note, within a hop
/// and the estimate's spread (28.7 ms).
#[test]
fn the_period_follows_a_note_change_within_the_window() {
    let rate = 48_000;
    let (first, second) = (330.0, 392.0);
    let change = (0.3 * rate as f64) as usize;
    let input: Vec<f32> = (0..(0.6 * rate as f64) as usize)
        .map(|i| {
            let (f, t) = if i < change {
                (first, i as f64 / rate as f64)
            } else {
                (second, (i - change) as f64 / rate as f64)
            };
            (0.2 * (TAU * f * t).sin() + 0.1 * (TAU * 2.0 * f * t).sin()) as f32
        })
        .collect();
    let mut state = state(rate, -5.0, 0.0);
    let want = rate as f64 / second;
    let mut settled = None;
    for (i, &x) in input.iter().enumerate() {
        tick(&mut state, rate, x);
        let near = (state.period() / want - 1.0).abs() < 0.02;
        if i >= change {
            if !near {
                settled = None;
            } else if settled.is_none() {
                settled = Some(i - change);
            }
        }
    }
    let settled = settled.expect("the period settles on the new note");
    let ms = settled as f64 * 1000.0 / rate as f64;
    assert!(ms < 30.0, "the period took {ms:.1} ms to follow");
}

/// Returning to a ratio of 1 fades to a head at exactly `D` shaped by the
/// heads' measured correlation: uncorrelated heads keep their power.
#[test]
fn the_return_to_unison_keeps_the_level_of_uncorrelated_heads() {
    let rate = 48_000;
    let fade = rate as usize / 100;
    let input = noise(rate, 6.0, 0.2, 11);
    let shifted = -5.0 / 12.0;
    let mut state = state(rate, -5.0, 0.0);
    let (mut wet_power, mut dry_power) = (0.0f64, 0.0f64);
    let mut i = 0;
    let feed = |state: &mut TightState, i: &mut usize| {
        let out = tick(state, rate, input[*i]);
        *i += 1;
        out
    };
    let mut returns = 0;
    while i + rate as usize / 4 < input.len() {
        // A while at the shift, then back to 1 once no splice runs.
        for _ in 0..rate as usize / 5 {
            feed(&mut state, &mut i);
        }
        while state.fading() {
            feed(&mut state, &mut i);
        }
        state.set_octaves(0.0, true);
        for n in 0..fade {
            let (wet, dry) = feed(&mut state, &mut i);
            if (fade / 4..3 * fade / 4).contains(&n) {
                wet_power += (wet as f64).powi(2);
                dry_power += (dry as f64).powi(2);
            }
        }
        returns += 1;
        state.set_octaves(shifted, true);
    }
    let db = 10.0 * (wet_power / dry_power).log10();
    assert!(returns >= 20, "{returns} returns");
    assert!(db.abs() < 0.5, "the return dips by {db:.2} dB");
}

/// A finite input that squares to infinity clears the state like a
/// non-finite one: the onsets are called again after it.
#[test]
fn an_input_too_loud_to_square_does_not_stop_the_onsets() {
    let rate = 48_000;
    let latency = (spec(rate, DEFAULT_DELAY_MS).latency) as usize;
    let mut input = vec![0.0f32; rate as usize];
    input[rate as usize / 20] = 1e20;
    let clicks: Vec<usize> = (2..10).map(|n| n * rate as usize / 10).collect();
    for &at in &clicks {
        input[at] = 0.5;
    }
    let mut state = state(rate, 5.0, 0.0);
    let out: Vec<f32> = input.iter().map(|&x| tick(&mut state, rate, x).0).collect();
    assert!(out.iter().all(|x| x.is_finite()));
    for &at in &clicks {
        let peak = (at..at + 2 * latency)
            .max_by(|&a, &b| out[a].abs().total_cmp(&out[b].abs()))
            .unwrap();
        assert_eq!(peak - at, latency, "the click at {at}");
    }
}

/// At the longest delay at 192 kHz every audible head reads within the
/// delay line: no nearer the input than the minimum, never back past what
/// the line holds (Catmull-Rom reads two samples further).
#[test]
fn at_the_longest_delay_every_head_reads_within_the_line() {
    let rate = 192_000;
    let chord = [82.41, 123.47, 164.81, 207.65, 246.94, 329.63];
    // Drop A's power chord: a common period near the estimate's 20 Hz floor.
    let low = [55.0, 82.41];
    for semitones in [-12.0, 12.0] {
        for notes in [&chord[..], &low[..]] {
            let mut input = noise(rate, 2.5, 0.1, 5);
            strike(&mut input, rate, notes, 0.5, 0.7);
            let mut state = TightState::new(rate, PITCH_MAX_DELAY_MS);
            state.set_octaves(semitones / 12.0, true);
            let min_delay = (12 * rate_scale(rate)) as f64;
            let (mut nearest, mut furthest) = (f64::INFINITY, 0.0f64);
            for &x in &input {
                let (wet, _) = tick(&mut state, rate, x);
                assert!(wet.is_finite());
                if let Some((near, far)) = state.read_delay() {
                    nearest = nearest.min(near);
                    furthest = furthest.max(far);
                }
            }
            println!(
                "{semitones:+} on {notes:?}: heads read {nearest:.0} … {furthest:.0} samples back"
            );
            assert!(
                nearest >= min_delay,
                "{semitones:+} on {notes:?}: a head read {nearest:.1} samples behind the input"
            );
            assert!(
                furthest + 2.0 < BUFFER_LEN as f64,
                "{semitones:+} on {notes:?}: a head read {furthest:.1} samples back"
            );
        }
    }
}
