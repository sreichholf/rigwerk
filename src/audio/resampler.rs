//! Rate conversion between the input clock and an output that runs at
//! another rate.

use std::f64::consts::PI;

/// Taps per output phase.  The window covers this many source frames, so the
/// read position needs half of it on either side.
const TAPS: usize = 32;
/// Phase resolution of the precomputed kernel.
const PHASES: usize = 512;
/// Half the kernel width; also the look-ahead the resampler needs.
const HALF: usize = TAPS / 2;
/// Power-of-two ring for the source frames around the read position.
const HISTORY: usize = 64;
const MASK: i64 = HISTORY as i64 - 1;
/// The most the ratio may be trimmed by drift steering.
const MAX_CORRECTION_PPM: f32 = 500.0;

/// Frame-rate conversion between the capture clock and a playback device
/// that runs at another rate.
///
/// A precomputed windowed-sinc kernel band-limits the source to the lower of
/// the two rates, so downsampling filters before it decimates instead of
/// folding everything above the output Nyquist back into the audible band.
/// The kernel is evaluated at the fractional read position through 512
/// phases and its rows are normalized to unity gain at DC; the two nearest
/// phases are blended so a slowly changing ratio does not step between them.
/// Both the kernel and the history ring are fixed size, so `next` never
/// allocates.  The nominal 1:1 case is left to the caller's bypass unless a
/// drift correction is applied.
pub(crate) struct FrameResampler {
    /// Source frames per output frame at the nominal rates.
    base_step: f64,
    /// The step actually used, including the drift correction.
    step: f64,
    correction_ppm: f32,
    /// One windowed-sinc row per phase, normalized to unity gain at DC.  The
    /// extra row is the fraction-1.0 kernel, so blending from the last phase
    /// reaches the next sample with the same tap alignment instead of wrapping
    /// to the phase-0 row one sample early.
    kernel: [[f32; TAPS]; PHASES + 1],
    /// The source frames around the read position, as a ring.
    history: [(f32, f32); HISTORY],
    /// Absolute source index of the next frame to fetch.
    next_index: i64,
    /// Absolute fractional source position of the next output frame.
    read_pos: f64,
    started: bool,
}

impl FrameResampler {
    pub(crate) fn new(source_rate: u32, output_rate: u32) -> Self {
        let mut resampler = Self {
            base_step: 1.0,
            step: 1.0,
            correction_ppm: 0.0,
            kernel: [[0.0; TAPS]; PHASES + 1],
            history: [(0.0, 0.0); HISTORY],
            next_index: 0,
            read_pos: 0.0,
            started: false,
        };
        resampler.configure(source_rate, output_rate);
        resampler
    }

    /// Rebuilds the kernel for a new rate pair and starts over.  The kernel is
    /// filled in place so a rate change does not copy a large temporary
    /// through the audio thread's stack.
    pub(crate) fn configure(&mut self, source_rate: u32, output_rate: u32) {
        let source_rate = source_rate.max(1);
        let output_rate = output_rate.max(1);
        self.base_step = source_rate as f64 / output_rate as f64;
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
            self.history = [first; HISTORY];
            self.next_index = 1;
            self.read_pos = 0.0;
            self.started = true;
        }

        let position = self.read_pos;
        let index = position.floor();
        let index = index as i64;

        // The window reaches `HALF` frames ahead of the read position.  With
        // downsampling the step is larger and this loop eats every source
        // frame in between, so no source material is skipped.
        while self.next_index <= index + HALF as i64 {
            let frame = fetch();
            self.history[(self.next_index & MASK) as usize] = frame;
            self.next_index += 1;
        }

        let phase = (position - index as f64) * PHASES as f64;
        let first_phase = (phase as usize).min(PHASES - 1);
        // The blend target is the next phase, which for the last bin is the
        // fraction-1.0 row at `PHASES`, not a wrap back to phase 0.
        let second_phase = first_phase + 1;
        let blend = (phase - first_phase as f64) as f32;
        let first_row = &self.kernel[first_phase];
        let second_row = &self.kernel[second_phase];

        let first_frame = index - (HALF as i64 - 1);
        let mut left = 0.0f32;
        let mut right = 0.0f32;
        for tap in 0..TAPS {
            let (left_sample, right_sample) =
                self.history[((first_frame + tap as i64) & MASK) as usize];
            let coefficient = first_row[tap] + (second_row[tap] - first_row[tap]) * blend;
            left += left_sample * coefficient;
            right += right_sample * coefficient;
        }

        self.read_pos = position + self.step;
        (left, right)
    }

    /// Builds the windowed-sinc table.  The cutoff sits at half the lower of
    /// the two rates, 5% below it so the Blackman window's transition band has
    /// room below the output Nyquist.  With 32 taps that transition is wide
    /// (about 0.17 source cycles/sample), so content well above the output
    /// Nyquist is rejected strongly but the first few kHz above it only
    /// partially; see the comment on `TAPS`.
    fn build_kernel(&mut self, source_rate: u32, output_rate: u32) {
        let cutoff = 0.5 * (output_rate as f64 / source_rate as f64).min(1.0) * 0.95;
        for phase in 0..=PHASES {
            let fraction = phase as f64 / PHASES as f64;
            let mut sum = 0.0;
            for tap in 0..TAPS {
                // Distance from the read position to this source frame, in
                // source frames.
                let distance = fraction + (HALF as f64 - 1.0 - tap as f64);
                let x = distance / HALF as f64;
                let window = if x.abs() >= 1.0 {
                    0.0
                } else {
                    0.42 + 0.5 * (PI * x).cos() + 0.08 * (2.0 * PI * x).cos()
                };
                let argument = 2.0 * PI * cutoff * distance;
                let sinc = if argument.abs() < 1e-12 {
                    1.0
                } else {
                    argument.sin() / argument
                };
                let value = sinc * window;
                self.kernel[phase][tap] = value as f32;
                sum += value;
            }
            for tap in 0..TAPS {
                self.kernel[phase][tap] = (self.kernel[phase][tap] as f64 / sum) as f32;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::FrameResampler;

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
        assert!((44_100..=44_120).contains(&fetched), "fetched {fetched}");
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
        assert!((9_600..=9_620).contains(&fetched), "fetched {fetched}");
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
}
