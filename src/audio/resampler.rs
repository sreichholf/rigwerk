//! Rate conversion between the input clock and an output that runs at
//! another rate.

/// Frame-rate conversion between the capture clock and a playback device
/// that runs at another rate.  A 4-point cubic (Catmull-Rom) curve through
/// the neighbouring frames keeps far more of the top end than straight-line
/// interpolation, at the cost of two frames of look-ahead.  The common 1:1
/// case is bypassed entirely.
pub(crate) struct FrameResampler {
    step: f64,
    phase: f64,
    /// The frame before, the two frames around, and the frame after the
    /// current read position.
    taps: [(f32, f32); 4],
    ready: bool,
}

impl FrameResampler {
    pub(crate) fn new(source_rate: u32, output_rate: u32) -> Self {
        Self {
            step: source_rate as f64 / output_rate.max(1) as f64,
            phase: 0.0,
            taps: [(0.0, 0.0); 4],
            ready: false,
        }
    }

    pub(crate) fn active(&self) -> bool {
        (self.step - 1.0).abs() > 1e-6
    }

    pub(crate) fn reset(&mut self) {
        self.phase = 0.0;
        self.ready = false;
    }

    pub(crate) fn next(&mut self, mut fetch: impl FnMut() -> (f32, f32)) -> (f32, f32) {
        if !self.ready {
            let first = fetch();
            self.taps = [first, first, fetch(), fetch()];
            self.ready = true;
            self.phase = 0.0;
        }
        while self.phase >= 1.0 {
            self.taps.rotate_left(1);
            self.taps[3] = fetch();
            self.phase -= 1.0;
        }
        let t = self.phase as f32;
        let [a, b, c, d] = self.taps;
        let converted = (cubic(t, a.0, b.0, c.0, d.0), cubic(t, a.1, b.1, c.1, d.1));
        self.phase += self.step;
        converted
    }
}

/// Catmull-Rom interpolation between `x0` and `x1` at position `t` (0..1).
pub(crate) fn cubic(t: f32, before: f32, x0: f32, x1: f32, after: f32) -> f32 {
    let c1 = 0.5 * (x1 - before);
    let c2 = before - 2.5 * x0 + 2.0 * x1 - 0.5 * after;
    let c3 = 0.5 * (after - before) + 1.5 * (x0 - x1);
    ((c3 * t + c2) * t + c1) * t + x0
}

#[cfg(test)]
mod tests {
    use super::{cubic, FrameResampler};

    #[test]
    fn resampler_halves_frames_for_a_96k_source_at_48k() {
        let mut source = (0..32).map(|value| (value as f32, 0.0));
        let mut resampler = FrameResampler::new(96_000, 48_000);
        let output: Vec<f32> = (0..4)
            .map(|_| resampler.next(|| source.next().unwrap()).0)
            .collect();
        assert_eq!(output, vec![0.0, 2.0, 4.0, 6.0]);
    }

    #[test]
    fn resampler_interpolates_for_a_44k_source_at_88k() {
        let mut source = (0..32).map(|value| (value as f32, 0.0));
        let mut resampler = FrameResampler::new(44_100, 88_200);
        let output: Vec<f32> = (0..8)
            .map(|_| resampler.next(|| source.next().unwrap()).0)
            .collect();
        // Once past the first frame, a straight ramp comes out exactly.
        assert_eq!(&output[2..], &[1.0, 1.5, 2.0, 2.5, 3.0, 3.5]);
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
        // One second of output consumes one second of input (plus look-ahead).
        assert!((44_100..=44_104).contains(&fetched), "fetched {fetched}");
    }

    #[test]
    fn cubic_follows_a_curve_more_closely_than_a_straight_line() {
        // Samples of x² at -1, 0, 1, 2; the true value at 0.5 is 0.25.
        let value = cubic(0.5, 1.0, 0.0, 1.0, 4.0);
        let linear = 0.5;
        assert!((value - 0.25).abs() < (linear - 0.25f32).abs());
    }
}
