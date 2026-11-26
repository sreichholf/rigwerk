use super::{
    format_cents, format_interval, format_semitones, PitchSettings, PitchShifter, PitchWindow,
    MAX_CENTS, MAX_SEMITONES, MIN_CENTS, MIN_SEMITONES,
};

const RATE: u32 = 48_000;

fn shifter(settings: PitchSettings) -> PitchShifter {
    let mut shifter = PitchShifter::new(RATE);
    shifter.update(settings, RATE);
    shifter
}

fn enabled(semitones: f32, window: PitchWindow) -> PitchSettings {
    PitchSettings {
        enabled: true,
        semitones,
        window,
        ..PitchSettings::default()
    }
}

fn run(shifter: &mut PitchShifter, input: &[f32]) -> Vec<f32> {
    input
        .iter()
        .map(|&sample| shifter.process(sample, -sample).0)
        .collect()
}

#[test]
fn windows_delay_by_half_a_grain_at_any_buffer() {
    // 16 / 32 / 64 ms grains at 48 kHz, half of each.  All three are far
    // under a 2048-sample host buffer.  The block-size test below shows
    // the result does not depend on one.
    let cases = [
        (PitchWindow::Tight, 384),
        (PitchWindow::Normal, 768),
        (PitchWindow::Smooth, 1536),
    ];
    for (window, latency) in cases {
        assert_eq!(window.latency_samples(RATE), latency);
        let mut shifter = shifter(enabled(0.0, window));
        assert_eq!(shifter.latency_samples(), latency);
        let latency = latency as usize;
        let mut input = vec![0.0; latency + 8];
        input[0] = 1.0;
        let output = run(&mut shifter, &input);
        assert_eq!(output[latency], 1.0, "{window:?} lands on its delay");
        assert!(
            output[..latency].iter().all(|sample| *sample == 0.0),
            "{window:?} stays silent until the delay"
        );
    }
}

#[test]
fn bypass_is_bit_exact_and_adds_no_delay() {
    let mut shifter = shifter(PitchSettings::default());
    assert_eq!(shifter.latency_samples(), 0);
    let output = run(&mut shifter, &[0.25, -0.5, 0.75, 1.0]);
    assert_eq!(output, vec![0.25, -0.5, 0.75, 1.0]);
}

#[test]
fn unison_is_a_pure_delay_and_the_channels_stay_apart() {
    let mut shifter = shifter(enabled(0.0, PitchWindow::Tight));
    let latency = shifter.latency_samples() as usize;
    for n in 0..latency + 32 {
        let left = n as f32 * 0.001;
        let right = -(n as f32) * 0.002;
        let (out_l, out_r) = shifter.process(left, right);
        if n >= latency {
            assert_eq!(out_l, (n - latency) as f32 * 0.001);
            assert_eq!(out_r, -((n - latency) as f32) * 0.002);
        }
    }
}

#[test]
fn wet_zero_is_the_delayed_dry_signal_even_when_shifted() {
    let mut shifter = shifter(PitchSettings {
        wet: 0.0,
        ..enabled(-5.0, PitchWindow::Tight)
    });
    let latency = shifter.latency_samples() as usize;
    let mut input = vec![0.0; latency + 4];
    input[0] = 0.8;
    let output = run(&mut shifter, &input);
    assert_eq!(output[latency], 0.8);
    assert_eq!(output[latency + 1], 0.0);
}

#[test]
fn small_blocks_match_a_sample_at_a_time() {
    let settings = enabled(-3.0, PitchWindow::Normal);
    let mut input = Vec::with_capacity(RATE as usize / 4);
    for n in 0..input.capacity() {
        input.push((n as f32 * 0.07).sin() * 0.4);
    }
    let mut by_sample = shifter(settings);
    let one = run(&mut by_sample, &input);
    let mut by_block = shifter(settings);
    let mut blocked = Vec::with_capacity(input.len());
    for chunk in input.chunks(32) {
        blocked.extend(run(&mut by_block, chunk));
    }
    assert_eq!(one, blocked);
}

#[test]
fn a_sine_lands_on_the_shifted_frequency() {
    // Drop tunings, an octave down, and an octave up.  The analysis window
    // starts after the grain has been splicing for a while.
    let cases = [(-12.0, 240.0), (-5.0, 240.0), (-2.0, 480.0), (12.0, 240.0)];
    for (semitones, source) in cases {
        let target = source * 2.0f32.powf(semitones / 12.0);
        let mut shifter = shifter(enabled(semitones, PitchWindow::Normal));
        let total = RATE as usize;
        let mut output = Vec::with_capacity(total);
        for n in 0..total {
            let sample = (std::f32::consts::TAU * source * n as f32 / RATE as f32).sin() * 0.5;
            output.push(shifter.process(sample, sample).0);
        }
        let heard = &output[RATE as usize / 4..RATE as usize / 4 + 8192];
        let at_target = power_at(heard, target);
        let at_source = power_at(heard, source);
        assert!(
            at_target > at_source * 10.0,
            "{semitones} st: target {target:.1} Hz power {at_target:.3} vs source {source:.1} Hz {at_source:.3}"
        );
        let peak = heard.iter().map(|sample| sample.abs()).fold(0.0, f32::max);
        assert!(peak < 0.75, "{semitones} st peaked at {peak}");
        assert!(heard.iter().all(|sample| sample.is_finite()));
    }
}

#[test]
fn settings_reject_a_nan_and_an_out_of_range_interval() {
    let settings = PitchSettings {
        enabled: true,
        semitones: f32::NAN,
        cents: 400.0,
        wet: -2.0,
        window: PitchWindow::Smooth,
    }
    .sanitized();
    assert!(settings.enabled);
    assert_eq!(settings.semitones, 0.0);
    assert_eq!(settings.cents, MAX_CENTS);
    assert_eq!(settings.wet, 0.0);
    assert_eq!(settings.window, PitchWindow::Smooth);
    let high = PitchSettings {
        semitones: 40.0,
        cents: -400.0,
        wet: 5.0,
        ..PitchSettings::default()
    }
    .sanitized();
    assert_eq!(high.semitones, MAX_SEMITONES);
    assert_eq!(high.cents, MIN_CENTS);
    assert_eq!(high.wet, 1.0);
    assert!(!PitchSettings::default().enabled);
    assert!(enabled(0.0, PitchWindow::Tight).is_unison());
    assert!(!enabled(-2.0, PitchWindow::Tight).is_unison());
}

#[test]
fn the_interval_reads_with_a_real_minus() {
    assert_eq!(format_interval(-2.0, 0.0), "−2 st");
    assert_eq!(format_interval(2.0, 0.0), "+2 st");
    assert_eq!(format_interval(0.0, 0.0), "0 st");
    assert_eq!(format_interval(-2.0, -15.0), "−2 st −15 ct");
    assert_eq!(format_semitones(-12.0), "−12 st");
    assert_eq!(format_cents(10.0), "+10 ct");
    assert_eq!(MIN_SEMITONES, -12.0);
}

/// Goertzel power at `freq`.  A Hann window keeps a strong neighbour from
/// leaking into the bin we are checking.
fn power_at(samples: &[f32], freq: f32) -> f32 {
    let omega = std::f32::consts::TAU * freq / RATE as f32;
    let coeff = 2.0 * omega.cos();
    let mut s1 = 0.0f32;
    let mut s2 = 0.0f32;
    let last = samples.len().saturating_sub(1) as f32;
    for (n, sample) in samples.iter().enumerate() {
        let window = 0.5 * (1.0 - (std::f32::consts::TAU * n as f32 / last).cos());
        let s0 = sample * window + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    power.abs()
}
