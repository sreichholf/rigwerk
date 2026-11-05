//! Rate conversion between the input clock and an output that runs at
//! another rate.

use std::f64::consts::PI;

/// Phase resolution of the precomputed kernel.  The two nearest phases are
/// blended, which keeps the interpolation error near -90 dB.
const PHASES: usize = 512;
/// Fewest and most taps a kernel may have.  The count depends on the rate
/// pair (see [`kernel_taps`]).
const MIN_TAPS: usize = 32;
const MAX_TAPS: usize = 512;
/// Stop-band attenuation of the Kaiser design, in dB.
const STOPBAND_DB: f64 = 80.0;
/// The highest frequency kept flat, in Hz, unless the lower rate's Nyquist
/// is below it.
const PASSBAND_HZ: f64 = 20_000.0;
/// The passband never reaches past this fraction of the lower rate.
const PASSBAND_FRACTION: f64 = 0.45;
/// The fastest source rate the kernel storage is sized for in advance, so
/// that a rate change up to it never allocates.
const PREALLOCATED_SOURCE_RATE: u32 = 192_000;
/// The most the ratio may be trimmed by drift steering.
const MAX_CORRECTION_PPM: f32 = 500.0;

/// The kernel length for a rate pair.
///
/// The passband runs flat to 20 kHz (or 45% of the lower rate).  The stop band
/// starts where content would fold back onto the passband edge, `lower rate -
/// passband`, so everything that could alias into the audible band is down by
/// [`STOPBAND_DB`]; only content that folds into the inaudible range between
/// the passband and the output Nyquist is left partially attenuated.  The
/// transition width is measured in source samples, so downsampling from a
/// high rate needs a proportionally longer kernel.
fn kernel_taps(source_rate: u32, output_rate: u32) -> usize {
    let (pass, stop) = band_edges(source_rate, output_rate);
    let transition = ((stop - pass) / source_rate as f64).max(1e-6);
    // Kaiser's estimate of the length for this attenuation and transition.
    let taps = ((STOPBAND_DB - 7.95) / (14.36 * transition)).ceil() as usize;
    taps.div_ceil(8).saturating_mul(8).clamp(MIN_TAPS, MAX_TAPS)
}

/// Passband edge and stop-band edge, in Hz.
fn band_edges(source_rate: u32, output_rate: u32) -> (f64, f64) {
    let lower = source_rate.min(output_rate) as f64;
    let pass = PASSBAND_HZ.min(PASSBAND_FRACTION * lower);
    (pass, lower - pass)
}

/// The zeroth-order modified Bessel function, for the Kaiser window.
fn bessel_i0(x: f64) -> f64 {
    let half = x / 2.0;
    let mut term = 1.0;
    let mut sum = 1.0;
    for k in 1..64 {
        term *= half / k as f64;
        let add = term * term;
        sum += add;
        if add < sum * 1e-17 {
            break;
        }
    }
    sum
}

/// A power-of-two history that holds the whole window plus the frames fetched
/// ahead of it.
fn history_len(taps: usize) -> usize {
    (taps * 2).next_power_of_two()
}

/// Frame-rate conversion between the capture clock and a playback device
/// that runs at another rate.
///
/// A precomputed Kaiser-windowed sinc kernel band-limits the source to the
/// lower of the two rates, so downsampling filters before it decimates
/// instead of folding content above the output Nyquist back into the audible
/// band.  The kernel is evaluated at the fractional read position through 512
/// phases, each row normalized to unity gain at DC, and the two nearest phases
/// are blended so a slowly changing ratio does not step between them.  The
/// kernel and history storage are sized for any source up to 192 kHz when the
/// resampler is created, so neither `configure` for such a rate nor `next`
/// allocates.  The nominal 1:1 case is left to the caller's bypass unless a
/// drift correction is applied.
pub(crate) struct FrameResampler {
    /// Source frames per output frame at the nominal rates.
    base_step: f64,
    /// The step actually used, including the drift correction.
    step: f64,
    correction_ppm: f32,
    /// Kernel length for the current rate pair, and half of it, which is
    /// also the look-ahead in source frames.
    taps: usize,
    half: usize,
    /// `PHASES + 1` rows of `taps` coefficients.  The extra row is the
    /// fraction-1.0 kernel, so blending from the last phase reaches the next
    /// sample with the same tap alignment instead of wrapping to the phase-0
    /// row one sample early.
    kernel: Vec<f32>,
    /// The source frames around the read position, as a power-of-two ring.
    history: Vec<(f32, f32)>,
    mask: i64,
    /// Absolute source index of the next frame to fetch.
    next_index: i64,
    /// Absolute fractional source position of the next output frame.
    read_pos: f64,
    started: bool,
}

impl FrameResampler {
    pub(crate) fn new(source_rate: u32, output_rate: u32) -> Self {
        // Reserve for the longest kernel any source up to 192 kHz needs at
        // this output rate (downsampling from the fastest source is the
        // worst case), so later rate changes stay inside this storage.
        let output_rate = output_rate.max(1);
        let most_taps = kernel_taps(PREALLOCATED_SOURCE_RATE, output_rate)
            .max(kernel_taps(source_rate.max(1), output_rate));
        let mut resampler = Self {
            base_step: 1.0,
            step: 1.0,
            correction_ppm: 0.0,
            taps: 0,
            half: 0,
            kernel: Vec::with_capacity((PHASES + 1) * most_taps),
            history: Vec::with_capacity(history_len(most_taps)),
            mask: 0,
            next_index: 0,
            read_pos: 0.0,
            started: false,
        };
        resampler.configure(source_rate, output_rate);
        resampler
    }

    /// Rebuilds the kernel for a new rate pair and starts over.  It reuses
    /// the storage reserved by `new`, so this only allocates for a source
    /// faster than 192 kHz.
    pub(crate) fn configure(&mut self, source_rate: u32, output_rate: u32) {
        let source_rate = source_rate.max(1);
        let output_rate = output_rate.max(1);
        self.base_step = source_rate as f64 / output_rate as f64;
        self.taps = kernel_taps(source_rate, output_rate);
        self.half = self.taps / 2;
        self.kernel.clear();
        self.kernel.resize((PHASES + 1) * self.taps, 0.0);
        let history = history_len(self.taps);
        self.history.clear();
        self.history.resize(history, (0.0, 0.0));
        self.mask = history as i64 - 1;
        self.build_kernel(source_rate, output_rate);
        self.set_correction_ppm(0.0);
        self.reset();
    }

    /// True when the output cannot simply take source frames one for one:
    /// either the rates differ or drift steering has trimmed the ratio.
    pub(crate) fn active(&self) -> bool {
        self.correction_ppm != 0.0 || (self.base_step - 1.0).abs() > 1e-9
    }

    /// Trims the conversion ratio by `ppm` parts per million.  A positive
    /// figure consumes source frames a little faster.  This is how the mixer
    /// follows two free-running clocks without dropping or repeating frames.
    pub(crate) fn set_correction_ppm(&mut self, ppm: f32) {
        self.correction_ppm = if ppm.is_finite() {
            ppm.clamp(-MAX_CORRECTION_PPM, MAX_CORRECTION_PPM)
        } else {
            0.0
        };
        self.step = self.base_step * (1.0 + self.correction_ppm as f64 * 1e-6);
    }

    /// Forgets the history.  The next output starts at the next source frame.
    pub(crate) fn reset(&mut self) {
        self.read_pos = 0.0;
        self.next_index = 0;
        self.started = false;
    }

    pub(crate) fn next(&mut self, mut fetch: impl FnMut() -> (f32, f32)) -> (f32, f32) {
        if !self.started {
            // Hold the first frame for the history the read position starts
            // inside of, so the first output is that frame rather than a step
            // from zero.
            let first = fetch();
            self.history.fill(first);
            self.next_index = 1;
            self.read_pos = 0.0;
            self.started = true;
        }

        let position = self.read_pos;
        let index = position.floor() as i64;
        let half = self.half as i64;

        // The window reaches `half` frames ahead of the read position.  With
        // downsampling the step is larger and this loop eats every source
        // frame in between, so no source material is skipped.
        while self.next_index <= index + half {
            let frame = fetch();
            self.history[(self.next_index & self.mask) as usize] = frame;
            self.next_index += 1;
        }

        let phase = (position - index as f64) * PHASES as f64;
        let first_phase = (phase as usize).min(PHASES - 1);
        // The blend target is the next phase, which for the last bin is the
        // fraction-1.0 row at `PHASES`, not a wrap back to phase 0.
        let blend = (phase - first_phase as f64) as f32;
        let taps = self.taps;
        let first_row = &self.kernel[first_phase * taps..(first_phase + 1) * taps];
        let second_row = &self.kernel[(first_phase + 1) * taps..(first_phase + 2) * taps];

        let first_frame = index - (half - 1);
        let mut left = 0.0f32;
        let mut right = 0.0f32;
        for (tap, (first, second)) in first_row.iter().zip(second_row).enumerate() {
            let (left_sample, right_sample) =
                self.history[((first_frame + tap as i64) & self.mask) as usize];
            let coefficient = first + (second - first) * blend;
            left += left_sample * coefficient;
            right += right_sample * coefficient;
        }

        self.read_pos = position + self.step;
        (left, right)
    }

    /// Builds the Kaiser-windowed sinc table for the current `taps`, with the
    /// cutoff halfway between the band edges of [`band_edges`].
    fn build_kernel(&mut self, source_rate: u32, output_rate: u32) {
        let (pass, stop) = band_edges(source_rate, output_rate);
        let cutoff = 0.5 * (pass + stop) / source_rate as f64;
        let beta = 0.1102 * (STOPBAND_DB - 8.7);
        let scale = 1.0 / bessel_i0(beta);
        let taps = self.taps;
        let half = self.half as f64;
        for phase in 0..=PHASES {
            let fraction = phase as f64 / PHASES as f64;
            let row = &mut self.kernel[phase * taps..(phase + 1) * taps];
            let mut sum = 0.0;
            for (tap, value) in row.iter_mut().enumerate() {
                // Distance from the read position to this source frame, in
                // source frames.
                let distance = fraction + (half - 1.0 - tap as f64);
                let x = distance / half;
                let window = if x.abs() >= 1.0 {
                    0.0
                } else {
                    bessel_i0(beta * (1.0 - x * x).sqrt()) * scale
                };
                let argument = 2.0 * PI * cutoff * distance;
                let sinc = if argument.abs() < 1e-12 {
                    1.0
                } else {
                    argument.sin() / argument
                };
                let coefficient = sinc * window;
                *value = coefficient as f32;
                sum += coefficient;
            }
            for value in row.iter_mut() {
                *value = (*value as f64 / sum) as f32;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{kernel_taps, FrameResampler};

    /// An endless sine at `frequency`, fed one source frame at a time.
    fn sine(source_rate: u32, frequency: f32) -> impl FnMut() -> (f32, f32) {
        let mut index = 0u64;
        move || {
            let phase =
                std::f64::consts::TAU * frequency as f64 * index as f64 / source_rate as f64;
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
        let mut resampler = FrameResampler::new(48_000, 48_000);
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
                let expected = (std::f64::consts::TAU * frequency * position / source_rate as f64)
                    .sin() as f32;
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
        let mut resampler = FrameResampler::new(48_000, 44_100);
        let kernel = resampler.kernel.as_ptr();
        let history = resampler.history.as_ptr();
        // The worst case the storage is sized for: downsampling from 192 kHz.
        resampler.configure(192_000, 44_100);
        assert_eq!(kernel, resampler.kernel.as_ptr());
        assert_eq!(history, resampler.history.as_ptr());
    }
}
