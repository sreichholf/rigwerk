use super::{kernel_taps, FrameResampler};

/// An endless sine at `frequency`, fed one source frame at a time.
fn sine(source_rate: u32, frequency: f32) -> impl FnMut() -> (f32, f32) {
    let mut index = 0u64;
    move || {
        let phase = std::f64::consts::TAU * frequency as f64 * index as f64 / source_rate as f64;
        index += 1;
        (phase.sin() as f32, 0.0)
    }
}

#[test]
fn resampler_converts_44k_to_48k_at_the_right_speed() {
    let mut fetched = 0usize;
    let mut resampler = FrameResampler::new(44_100, 48_000);
    for _ in 0..48_000 {
        resampler.next(|| {
            fetched += 1;
            (0.0, 0.0)
        });
    }
    // One second of output consumes one second of input, plus the
    // window's look-ahead.
    let look_ahead = kernel_taps(44_100, 48_000) / 2;
    assert!(
        (44_100..=44_100 + look_ahead + 1).contains(&fetched),
        "fetched {fetched}"
    );
}

#[test]
fn resampler_consumes_every_source_frame_when_downsampling() {
    let mut fetched = 0usize;
    let mut resampler = FrameResampler::new(96_000, 48_000);
    for _ in 0..4_800 {
        resampler.next(|| {
            fetched += 1;
            (0.0, 0.0)
        });
    }
    // Downsampling must read every intervening frame, not skip them.
    let look_ahead = kernel_taps(96_000, 48_000) / 2 + 1;
    assert!(
        (9_600..=9_600 + look_ahead + 2).contains(&fetched),
        "fetched {fetched}"
    );
}

#[test]
fn resampler_rejects_a_tone_above_the_output_nyquist() {
    // 30 kHz at 96 kHz has no place in a 48 kHz output; a sample-dropping
    // resampler would fold it to 18 kHz at full level.
    let mut source = sine(96_000, 30_000.0);
    let mut resampler = FrameResampler::new(96_000, 48_000);
    let mut peak = 0.0f32;
    for index in 0..4_800 {
        let (left, _) = resampler.next(&mut source);
        if index >= 960 {
            peak = peak.max(left.abs());
        }
    }
    assert!(peak < 0.01, "30 kHz came through at {peak}");
}

#[test]
fn resampler_keeps_a_tone_below_the_output_nyquist() {
    let mut source = sine(96_000, 10_000.0);
    let mut resampler = FrameResampler::new(96_000, 48_000);
    let mut peak = 0.0f32;
    for index in 0..4_800 {
        let (left, _) = resampler.next(&mut source);
        if index >= 960 {
            peak = peak.max(left.abs());
        }
    }
    assert!(peak > 0.95, "10 kHz dropped to {peak}");
}

#[test]
fn resampler_interpolates_a_ramp_when_upsampling() {
    let mut index = 0i64;
    let mut source = move || {
        let value = index as f32;
        index += 1;
        (value, 0.0)
    };
    let mut resampler = FrameResampler::new(44_100, 88_200);
    let output: Vec<f32> = (0..80).map(|_| resampler.next(&mut source).0).collect();
    // Once the window is inside the ramp, the midpoint values come out
    // exactly, half a source frame per output frame.
    for (position, value) in output.iter().enumerate().skip(64) {
        let expected = position as f32 * 0.5;
        assert!(
            (value - expected).abs() < 1e-2,
            "output {position} was {value}, expected {expected}"
        );
    }
}

#[test]
fn resampler_skips_the_kernel_only_at_an_untrimmed_1_to_1_ratio() {
    let mut resampler: FrameResampler = FrameResampler::new(48_000, 48_000);
    assert!(!resampler.active());
    resampler.set_correction_ppm(250.0);
    assert!(resampler.active());
    resampler.set_correction_ppm(0.0);
    assert!(!resampler.active());
}

#[test]
fn drift_correction_keeps_a_1_to_1_bridge_at_unity() {
    let mut resampler = FrameResampler::new(48_000, 48_000);
    resampler.set_correction_ppm(250.0);
    for _ in 0..4_800 {
        let (left, right) = resampler.next(|| (0.5, -0.25));
        assert!((left - 0.5).abs() < 1e-3, "left {left}");
        assert!((right + 0.25).abs() < 1e-3, "right {right}");
    }
}

#[test]
fn fractional_phase_blend_does_not_jump_a_sample() {
    // A +20 ppm trim crosses the last phase bin only when the fractional
    // part is within 1/512 of the next sample.  Blending that bin toward
    // the phase-0 row (instead of a fraction-1.0 row at the same tap
    // alignment) returns the previous sample there, which is a periodic
    // full-scale error rather than a small interpolation error.
    let source_rate = 48_000u32;
    let frequency = 10_000.0f64;
    let step = 1.0 + 20.0f64 * 1e-6;
    let mut source = sine(source_rate, frequency as f32);
    let mut resampler = FrameResampler::new(source_rate, source_rate);
    resampler.set_correction_ppm(20.0);

    let mut position = 0.0f64;
    let mut worst = 0.0f32;
    for output in 0..200_000 {
        let (left, _) = resampler.next(&mut source);
        if output >= 64 {
            let expected =
                (std::f64::consts::TAU * frequency * position / source_rate as f64).sin() as f32;
            worst = worst.max((left - expected).abs());
        }
        position += step;
    }
    assert!(worst < 1e-3, "worst interpolation error {worst}");
}

/// The level of a sine at `frequency` after conversion, in dB, measured
/// as RMS over one second once the window has filled.
fn level_db(source_rate: u32, output_rate: u32, frequency: f32, correction_ppm: f32) -> f64 {
    let mut source = sine(source_rate, frequency);
    let mut resampler = FrameResampler::new(source_rate, output_rate);
    resampler.set_correction_ppm(correction_ppm);
    let skip = output_rate as usize / 10;
    let mut sum = 0.0f64;
    for index in 0..skip + output_rate as usize {
        let (left, _) = resampler.next(&mut source);
        if index >= skip {
            sum += (left as f64) * (left as f64);
        }
    }
    let rms = (sum / output_rate as f64).sqrt();
    20.0 * (rms * std::f64::consts::SQRT_2).log10()
}

#[test]
fn the_passband_is_flat_to_20_khz_for_every_common_pair() {
    for (source, output, ppm) in [
        (44_100, 48_000, 0.0),
        (48_000, 44_100, 0.0),
        (96_000, 48_000, 0.0),
        (192_000, 44_100, 0.0),
        (44_100, 44_100, 100.0),
        (48_000, 48_000, 100.0),
    ] {
        let level = level_db(source, output, 20_000.0, ppm);
        assert!(
            level.abs() < 0.1,
            "{source} -> {output} at 20 kHz: {level:.2} dB"
        );
    }
}

#[test]
fn nothing_folds_into_the_audible_band() {
    // Each tone sits above the output Nyquist at a frequency that would
    // fold to below 20 kHz.
    for (source, output, frequency) in [
        (96_000, 48_000, 28_500.0),  // would fold to 19.5 kHz
        (96_000, 48_000, 40_000.0),  // would fold to 8 kHz
        (192_000, 44_100, 24_500.0), // would fold to 19.6 kHz
        (192_000, 44_100, 30_000.0), // would fold to 14.1 kHz
        (88_200, 44_100, 30_000.0),  // would fold to 14.1 kHz
    ] {
        let level = level_db(source, output, frequency, 0.0);
        assert!(
            level < -70.0,
            "{source} -> {output}: {frequency} Hz came through at {level:.1} dB"
        );
    }
}

#[test]
fn a_rate_change_reuses_the_reserved_kernel_storage() {
    let mut resampler: FrameResampler = FrameResampler::new(48_000, 44_100);
    let kernel = resampler.kernel.as_ptr();
    let history = resampler.history.as_ptr();
    // The worst case the storage is sized for: downsampling from 192 kHz.
    resampler.configure(192_000, 44_100);
    assert_eq!(kernel, resampler.kernel.as_ptr());
    assert_eq!(history, resampler.history.as_ptr());
}

#[test]
fn lane_frames_resample_like_the_stereo_pair() {
    let mut stereo = FrameResampler::new(44_100, 48_000);
    let mut lanes: FrameResampler<[f32; 4]> = FrameResampler::new(44_100, 48_000);
    lanes.set_lanes(3);
    stereo.set_correction_ppm(120.0);
    lanes.set_correction_ppm(120.0);
    let (mut left, mut right) = (sine(44_100, 997.0), sine(44_100, 3_001.0));
    let source: Vec<(f32, f32)> = (0..5_000).map(|_| (left().0, right().0)).collect();
    let (mut stereo_at, mut lanes_at) = (0usize, 0usize);
    for _ in 0..4_096 {
        let pair = stereo.next(|| {
            stereo_at += 1;
            source[stereo_at - 1]
        });
        let frame = lanes.next(|| {
            lanes_at += 1;
            let (l, r) = source[lanes_at - 1];
            [l, r, l - r, 1.0]
        });
        assert_eq!(frame[0].to_bits(), pair.0.to_bits());
        assert_eq!(frame[1].to_bits(), pair.1.to_bits());
        // Lanes past the count stay silent.
        assert_eq!(frame[3], 0.0);
    }
}
