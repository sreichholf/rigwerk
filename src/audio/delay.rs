//! Latency alignment between paths, and the dry side of an effect.  Lines
//! are sized once outside the callback; reads and writes never allocate.
//! A delay change crossfades over [`TAP_FADE_SECONDS`] instead of jumping.
//! A settled tap at delay 0 returns the frame just written, bit for bit.

/// Tap move time.  Long enough not to click, short enough not to smear.
pub(crate) const TAP_FADE_SECONDS: f32 = 0.015;

/// [`Self::read`] at `back` 0 is the newest frame.  A [`Self::reset`] makes
/// older frames read as silence, so a restarted path does not replay them.
pub(crate) struct DelayLine {
    frames: Box<[[f32; 2]]>,
    write: usize,
    filled: usize,
}

impl DelayLine {
    /// Allocates.  Call it outside the callback.
    pub(crate) fn new(max_delay: usize) -> Self {
        Self {
            frames: vec![[0.0; 2]; max_delay + 1].into_boxed_slice(),
            write: 0,
            filled: 0,
        }
    }

    /// No storage and no allocation.
    pub(crate) fn empty() -> Self {
        Self {
            frames: Box::new([]),
            write: 0,
            filled: 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn max_delay(&self) -> usize {
        self.frames.len().saturating_sub(1)
    }

    /// O(1): the audio thread may call it.
    pub(crate) fn reset(&mut self) {
        self.filled = 0;
    }

    #[inline]
    pub(crate) fn push(&mut self, frame: [f32; 2]) {
        let len = self.frames.len();
        if len == 0 {
            return;
        }
        self.write += 1;
        if self.write >= len {
            self.write = 0;
        }
        self.frames[self.write] = frame;
        if self.filled < len {
            self.filled += 1;
        }
    }

    #[inline]
    pub(crate) fn read(&self, back: usize) -> [f32; 2] {
        if back >= self.filled {
            return [0.0; 2];
        }
        let index = if back <= self.write {
            self.write - back
        } else {
            self.write + self.frames.len() - back
        };
        self.frames[index]
    }
}

/// [`DelayLine`] of one sample per frame.  Same rules.
pub(crate) struct MonoDelayLine {
    samples: Box<[f32]>,
    write: usize,
    filled: usize,
}

impl MonoDelayLine {
    /// Allocates.  Call it outside the callback.
    pub(crate) fn new(max_delay: usize) -> Self {
        Self {
            samples: vec![0.0; max_delay + 1].into_boxed_slice(),
            write: 0,
            filled: 0,
        }
    }

    pub(crate) fn max_delay(&self) -> usize {
        self.samples.len().saturating_sub(1)
    }

    /// O(1): the audio thread may call it.
    pub(crate) fn reset(&mut self) {
        self.filled = 0;
    }

    #[inline]
    pub(crate) fn push(&mut self, sample: f32) {
        let len = self.samples.len();
        if len == 0 {
            return;
        }
        self.write += 1;
        if self.write >= len {
            self.write = 0;
        }
        self.samples[self.write] = sample;
        if self.filled < len {
            self.filled += 1;
        }
    }

    #[inline]
    pub(crate) fn read(&self, back: usize) -> f32 {
        if back >= self.filled {
            return 0.0;
        }
        let index = if back <= self.write {
            self.write - back
        } else {
            self.write + self.samples.len() - back
        };
        self.samples[index]
    }
}

/// [`Self::next`]'s `back` is for a caller that writes a chunk, then reads.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DelayTap {
    /// New delay.  A fade reads it from the first sample, weighted by
    /// `progress`.
    current: u32,
    previous: u32,
    target: u32,
    progress: f32,
    step: f32,
}

impl Default for DelayTap {
    fn default() -> Self {
        Self::new(0)
    }
}

impl DelayTap {
    pub(crate) fn new(delay: u32) -> Self {
        Self {
            current: delay,
            previous: delay,
            target: delay,
            progress: 1.0,
            step: 1.0,
        }
    }

    pub(crate) fn snap(&mut self, delay: u32) {
        *self = Self::new(delay);
    }

    /// A change during a fade waits, so a read blends at most two positions.
    pub(crate) fn set_target(&mut self, delay: u32, fade_samples: f32) {
        self.target = delay;
        self.step = if fade_samples > 1.0 {
            1.0 / fade_samples
        } else {
            1.0
        };
    }

    pub(crate) fn target(&self) -> u32 {
        self.target
    }

    /// How far a line must still run.  The delay faded from counts only
    /// while that fade runs.
    pub(crate) fn longest(&self) -> u32 {
        let fading = if self.progress < 1.0 {
            self.previous
        } else {
            0
        };
        self.current.max(fading).max(self.target)
    }

    pub(crate) fn is_settled(&self) -> bool {
        self.progress >= 1.0 && self.target == self.current
    }

    #[inline]
    pub(crate) fn next(&mut self, line: &DelayLine, back: usize) -> [f32; 2] {
        let Some((current, previous, weight)) = self.advance() else {
            return line.read(back + self.current as usize);
        };
        let new = line.read(back + current);
        let old = line.read(back + previous);
        [
            old[0] + (new[0] - old[0]) * weight,
            old[1] + (new[1] - old[1]) * weight,
        ]
    }

    #[inline]
    pub(crate) fn next_mono(&mut self, line: &MonoDelayLine, back: usize) -> f32 {
        let Some((current, previous, weight)) = self.advance() else {
            return line.read(back + self.current as usize);
        };
        let new = line.read(back + current);
        let old = line.read(back + previous);
        old + (new - old) * weight
    }

    /// `None` when settled.  Otherwise new delay, old delay, and new weight.
    #[inline]
    fn advance(&mut self) -> Option<(usize, usize, f32)> {
        if self.progress >= 1.0 {
            if self.target == self.current {
                return None;
            }
            self.previous = self.current;
            self.current = self.target;
            self.progress = 0.0;
        }
        let weight = self.progress;
        self.progress = (self.progress + self.step).min(1.0);
        Some((self.current as usize, self.previous as usize, weight))
    }
}

pub(crate) const DECLICK_SECONDS: f32 = 0.002;

/// Fades the step when the old signal is gone and cannot be crossfaded
/// (a plugin stops, a chain misses a chunk).  The step is the gap from the
/// extrapolated last two frames to the new first frame.  Only the click
/// goes; the sound still changes.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Declick {
    offset: [f32; 2],
    remaining: u32,
    len: u32,
    /// The last two frames played, oldest first.
    history: [[f32; 2]; 2],
}

impl Declick {
    /// Call after every chunk.
    pub(crate) fn note(&mut self, left: &[f32], right: &[f32]) {
        let count = left.len().min(right.len());
        match count {
            0 => {}
            1 => self.history = [self.history[1], [left[0], right[0]]],
            _ => {
                self.history = [
                    [left[count - 2], right[count - 2]],
                    [left[count - 1], right[count - 1]],
                ]
            }
        }
    }

    pub(crate) fn start(&mut self, first: [f32; 2], len: u32) {
        let [prev, last] = self.history;
        let expected = [2.0 * last[0] - prev[0], 2.0 * last[1] - prev[1]];
        self.offset = [expected[0] - first[0], expected[1] - first[1]];
        self.remaining = len;
        self.len = len;
    }

    pub(crate) fn is_active(&self) -> bool {
        self.remaining > 0
    }

    #[inline]
    pub(crate) fn apply(&mut self, frame: [f32; 2]) -> [f32; 2] {
        if self.remaining == 0 {
            return frame;
        }
        let weight = self.remaining as f32 / self.len as f32;
        self.remaining -= 1;
        [
            frame[0] + self.offset[0] * weight,
            frame[1] + self.offset[1] * weight,
        ]
    }
}

#[cfg(test)]
mod tests;
