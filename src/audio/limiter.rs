//! Sample-peak safety limiter, after the device level and before mute.
//! No lookahead, so it adds no delay.  Under the ceiling the frame is
//! copied, not multiplied, and stays bit-identical.  One gain for every
//! channel, from the frame peak.  [`Self::process`] does not allocate.
//! The caller already holds a [`super::denormal::DenormalGuard`].

/// Ceiling, dBFS.  A constant, not a preference.
pub(crate) const SAFETY_CEILING_DB: f32 = -1.0;

/// Gain reduction, dB, where the UI starts to report it.
pub(crate) const SAFETY_GR_DB: f32 = 0.5;

/// Release to unity.  Long enough to be smooth, short enough not to linger.
const RELEASE_MS: f32 = 40.0;

/// Gain this close to unity snaps to 1, so a finished release copies again.
const UNITY_SNAP: f32 = 0.9995;

pub(crate) fn safety_ceiling_linear() -> f32 {
    10.0f32.powf(SAFETY_CEILING_DB / 20.0)
}

/// Built with the stream.  The callback only calls [`Self::set_rate`] and
/// [`Self::process`].
pub(crate) struct SafetyLimiter {
    ceiling: f32,
    gain: f32,
    /// One-pole release coefficient, `exp(-1 / (τ · rate))`.
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

    /// Keeps the gain, so a rate change does not click.
    pub(crate) fn set_rate(&mut self, rate: u32) {
        let rate = rate.max(1);
        if rate == self.rate {
            return;
        }
        self.rate = rate;
        let samples = (RELEASE_MS * 0.001 * rate as f32).max(1.0);
        self.release = (-1.0 / samples).exp();
    }

    /// No logarithm: the callback uses this instead of [`Self::reduction_db`].
    pub(crate) fn is_limiting(&self) -> bool {
        self.gain < 1.0
    }

    pub(crate) fn reduction_db(&self) -> f32 {
        if self.gain >= 1.0 {
            0.0
        } else if self.gain <= 1.0e-12 {
            120.0
        } else {
            -20.0 * self.gain.log10()
        }
    }

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
            // A non-finite peak cannot be scaled (the ratio underflows or is
            // NaN).  The hold below writes the ceiling onto that sample.
            if peak.is_finite() {
                self.ceiling / peak
            } else {
                0.0
            }
        } else {
            1.0
        };
        if desired < self.gain {
            // Held on this sample: there is no lookahead.
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
        // Write the peak as the ceiling itself, so a full-scale frame lands
        // on it exactly instead of a float short of it.
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
