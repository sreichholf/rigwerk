//! Sample-peak safety limiter on one output device.
//!
//! It sits after that device's level and before its mute.  The ceiling is
//! a fixed −1.0 dBFS: a sample above it is held there, and the gain then
//! falls back to unity over a few tens of milliseconds.  There is no
//! lookahead, so the limiter adds no delay and does not move alignment.
//! A signal that stays under the ceiling is copied, not multiplied, so it
//! stays bit-identical aside from the device's own level ramp.
//!
//! One gain for every channel of the device, from the peak of the frame.
//! The state is a handful of floats owned by the device mapper.  [`Self::process`]
//! does not allocate.  The callback that calls it already holds a
//! [`super::denormal::DenormalGuard`] for the whole block.

/// Ceiling, dBFS.  A constant, not a saved setting.
pub(crate) const SAFETY_CEILING_DB: f32 = -1.0;

/// Gain reduction, dB, at which the UI and Live Issues start to say so.
pub(crate) const SAFETY_GR_DB: f32 = 0.5;

/// How fast the gain returns to unity after a peak.  Tens of milliseconds:
/// long enough that the reduction falls back smoothly, short enough that a
/// transient does not keep the device quiet.
const RELEASE_MS: f32 = 40.0;

/// Gain this close to unity snaps to 1, so a finished release copies again.
const UNITY_SNAP: f32 = 0.9995;

/// Linear ceiling, `10^(dB/20)`.
pub(crate) fn safety_ceiling_linear() -> f32 {
    10.0f32.powf(SAFETY_CEILING_DB / 20.0)
}

/// Running limiter for one device.  Constructed with the stream; the
/// callback only calls [`Self::set_rate`] and [`Self::process`].
pub(crate) struct SafetyLimiter {
    ceiling: f32,
    /// Linear gain applied to the frame.  1 means not limiting.
    gain: f32,
    /// One-pole coefficient of the release (`exp(-1 / (τ · rate))`).
    release: f32,
    rate: u32,
}

impl SafetyLimiter {
    pub(crate) fn new(rate: u32) -> Self {
        let mut limiter = Self {
            ceiling: safety_ceiling_linear(),
            gain: 1.0,
            release: 0.0,
            rate: 0,
        };
        limiter.set_rate(rate);
        limiter
    }

    /// Points the release at `rate`.  The gain is kept, so a rate change
    /// does not click.  A rate the limiter already has is a no-op.
    pub(crate) fn set_rate(&mut self, rate: u32) {
        let rate = rate.max(1);
        if rate == self.rate {
            return;
        }
        self.rate = rate;
        let samples = (RELEASE_MS * 0.001 * rate as f32).max(1.0);
        self.release = (-1.0 / samples).exp();
    }

    /// Whether the gain is below unity: a cheap test for the callback,
    /// where [`Self::reduction_db`] would take a logarithm.
    pub(crate) fn is_limiting(&self) -> bool {
        self.gain < 1.0
    }

    /// Gain reduction right now, in dB.  0 when the gain is unity.
    pub(crate) fn reduction_db(&self) -> f32 {
        if self.gain >= 1.0 {
            0.0
        } else if self.gain <= 1.0e-12 {
            120.0
        } else {
            -20.0 * self.gain.log10()
        }
    }

    /// Limits one device frame in place.  Channels share one gain, taken
    /// from the loudest sample.  Unity leaves every bit alone, including a
    /// negative zero.
    #[inline]
    pub(crate) fn process(&mut self, samples: &mut [f32]) {
        let mut peak = 0.0f32;
        for sample in samples.iter() {
            let abs = sample.abs();
            if abs > peak {
                peak = abs;
            }
        }
        let desired = if peak > self.ceiling {
            // A non-finite peak cannot be scaled back (the ratio underflows
            // or is NaN).  Holding writes the ceiling onto that sample.
            if peak.is_finite() {
                self.ceiling / peak
            } else {
                0.0
            }
        } else {
            1.0
        };
        if desired < self.gain {
            // A sample over the ceiling is held there on this very sample.
            self.gain = desired;
        } else if self.gain < 1.0 {
            let released = 1.0 + (self.gain - 1.0) * self.release;
            // Never release past what this frame still needs.
            self.gain = released.min(desired);
            if self.gain >= UNITY_SNAP {
                self.gain = 1.0;
            }
        }
        if self.gain == 1.0 {
            return;
        }
        let ceiling = self.ceiling;
        let gain = self.gain;
        // The peak sample is written as the ceiling itself, so a full-scale
        // frame lands on it exactly instead of a float short of it.
        let hold = gain == desired && desired < 1.0;
        for sample in samples.iter_mut() {
            if hold && sample.abs() == peak {
                *sample = ceiling.copysign(*sample);
            } else {
                *sample *= gain;
            }
        }
    }
}

#[cfg(test)]
mod tests;
