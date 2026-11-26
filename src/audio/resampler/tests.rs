use super::super::rt_guard::RtGuard;
use super::{kernel_taps, FrameResampler, KernelHandoff};

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

/// A source above `PREALLOCATED_SOURCE_RATE` (ASIO allows up to 768 kHz)
/// outgrows the kernel-row reservation, but its history is still inside the
/// separate `MAX_TAPS` reservation: `adopt` must not allocate or free.
#[test]
fn adopting_above_the_preallocated_source_rate_reuses_the_history() {
    let mut resampler: FrameResampler = FrameResampler::new(48_000, 44_100);
    let history = resampler.history.as_ptr();
    let kernel = FrameResampler::<(f32, f32)>::plan(384_000, 44_100);
    let guard = RtGuard::enter();
    let retired = resampler.adopt(kernel);
    assert_eq!(guard.allocations(), 0, "adopt allocated");
    assert_eq!(guard.frees(), 0, "adopt freed");
    assert_eq!(
        history,
        resampler.history.as_ptr(),
        "the history reallocated"
    );
    drop(guard);
    drop(retired);
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

/// A planned table installed by `adopt` is the same filter `configure`
/// would have built, on an integer pair, a non-integer ratio and a
/// downsampling pair.
#[test]
fn an_adopted_kernel_plays_like_a_configured_one() {
    for (source_rate, output_rate) in [(48_000, 48_000), (44_100, 48_000), (96_000, 44_100)] {
        let mut configured = FrameResampler::new(48_000, 48_000);
        configured.configure(source_rate, output_rate);
        let mut adopted = FrameResampler::new(48_000, 48_000);
        let kernel = FrameResampler::<(f32, f32)>::plan(source_rate, output_rate);
        drop(adopted.adopt(kernel));

        let mut configured_source = sine(source_rate, 1_000.0);
        let mut adopted_source = sine(source_rate, 1_000.0);
        for frame in 0..400 {
            let expected = configured.next(&mut configured_source);
            let played = adopted.next(&mut adopted_source);
            assert_eq!(
                played.0.to_bits(),
                expected.0.to_bits(),
                "{source_rate} -> {output_rate}, frame {frame}"
            );
            assert_eq!(
                played.1.to_bits(),
                expected.1.to_bits(),
                "{source_rate} -> {output_rate}, frame {frame}"
            );
        }
    }
}

/// The handoff keeps one pending table, retires the rows a callback replaced
/// for the poster to drop, and leaves a table for the pair already playing
/// alone instead of restarting its history.
#[test]
fn the_kernel_handoff_retires_rows_and_leaves_a_matching_table() {
    let handoff = KernelHandoff::new();
    handoff.post(FrameResampler::<(f32, f32)>::plan(44_100, 48_000));
    handoff.post(FrameResampler::<(f32, f32)>::plan(96_000, 44_100));
    assert_eq!(
        handoff.pending_pair(),
        Some((96_000, 44_100)),
        "the stale pending table was replaced"
    );

    let mut resampler: FrameResampler = FrameResampler::new(48_000, 44_100);
    let built = resampler.configure_calls();
    handoff.try_adopt(&mut resampler);
    assert_eq!(resampler.pair(), (96_000, 44_100));
    assert_eq!(handoff.pending_pair(), None);
    assert_eq!(resampler.configure_calls(), built, "adopt built nothing");
    assert!(
        handoff.retired_len().is_some(),
        "the rows the callback replaced wait for the poster"
    );

    // The next post frees them on this thread and installs its own table.
    handoff.post(FrameResampler::<(f32, f32)>::plan(48_000, 48_000));
    assert_eq!(handoff.retired_len(), None);
    assert_eq!(handoff.pending_pair(), Some((48_000, 48_000)));

    // A table for the pair already playing would reset the history, so it is
    // not adopted at all.
    let mut playing: FrameResampler = FrameResampler::new(44_100, 48_000);
    handoff.post(FrameResampler::<(f32, f32)>::plan(44_100, 48_000));
    handoff.try_adopt(&mut playing);
    assert_eq!(handoff.pending_pair(), Some((44_100, 48_000)));
    assert_eq!(playing.pair(), (44_100, 48_000));
}
