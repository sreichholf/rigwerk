//! Preallocated stereo delay lines and crossfading read taps, for the
//! latency alignment between signal paths and the dry side of a plugin
//! insert.
//!
//! A [`DelayLine`] is a ring of stereo frames, sized once outside the
//! callback; writing and reading never allocate.  A [`DelayTap`] reads one
//! line at a delay that can change while audio runs: a new delay never
//! jumps, it crossfades linearly from the old read position to the new one
//! over [`TAP_FADE_SECONDS`].  A settled tap reads the line as is, so a tap
//! at delay 0 hands back exactly the frame just written, bit for bit.

/// How long a tap takes to move to a new delay.  Long enough that the
/// crossfade does not click, short enough that a latency change settles
/// before it is heard as a smear.
pub(crate) const TAP_FADE_SECONDS: f32 = 0.015;

/// A ring of stereo frames.  [`Self::read`] at `back` 0 is the frame written
/// last.  Frames older than what was written since the last
/// [`Self::reset`] read as silence, so a restarted path never replays what
/// the slot held before.
pub(crate) struct DelayLine {
    frames: Box<[[f32; 2]]>,
    /// Index of the frame written last.
    write: usize,
    /// How many frames were written since the last reset (saturating at the
    /// capacity).
    filled: usize,
}

impl DelayLine {
    /// A line that can delay by up to `max_delay` frames (and hold
    /// `max_delay + 1` frames).  Allocates; call it outside the callback.
    pub(crate) fn new(max_delay: usize) -> Self {
        Self {
            frames: vec![[0.0; 2]; max_delay + 1].into_boxed_slice(),
            write: 0,
            filled: 0,
        }
    }

    /// A line with no storage: writes are dropped and every read is
    /// silence.  Never allocates.
    pub(crate) fn empty() -> Self {
        Self {
            frames: Box::new([]),
            write: 0,
            filled: 0,
        }
    }

    /// The longest delay this line can read.
    #[cfg(test)]
    pub(crate) fn max_delay(&self) -> usize {
        self.frames.len().saturating_sub(1)
    }

    /// Forgets the history: every older frame reads as silence again.
    /// O(1), so the audio thread may call it.
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

    /// The frame written `back` frames before the last one (0 = the last).
    /// Silence past the history or the capacity.
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

/// One read position on a [`DelayLine`] whose delay can move without a
/// jump.  The delay is in frames; [`Self::next`] adds an extra `back`
/// offset for callers that write a whole chunk before reading it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DelayTap {
    /// The delay being read (the new one, during a crossfade).
    current: u32,
    /// The delay faded away from, during a crossfade.
    previous: u32,
    /// The delay asked for; a fade toward it starts with the next read once
    /// the running one has ended.
    target: u32,
    /// Crossfade progress 0..1 from `previous` to `current`; 1 when settled.
    progress: f32,
    /// Progress per sample of the next fade.
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

    /// Moves to `delay` at once, dropping any running fade.
    pub(crate) fn snap(&mut self, delay: u32) {
        *self = Self::new(delay);
    }

    /// Points the tap at `delay`; the move crossfades over `fade_samples`.
    /// A change asked for during a fade waits until that fade ends, so the
    /// read is always a blend of at most two positions.
    pub(crate) fn set_target(&mut self, delay: u32, fade_samples: f32) {
        self.target = delay;
        self.step = if fade_samples > 1.0 {
            1.0 / fade_samples
        } else {
            1.0
        };
    }

    /// The delay this tap settles on.
    #[cfg(test)]
    pub(crate) fn target(&self) -> u32 {
        self.target
    }

    /// The longest delay this tap reads or will read: how many frames a
    /// line must keep running before what was written into it has all come
    /// out through this tap.
    pub(crate) fn longest(&self) -> u32 {
        self.current.max(self.previous).max(self.target)
    }

    /// Whether the tap reads one position (no fade running or pending).
    pub(crate) fn is_settled(&self) -> bool {
        self.progress >= 1.0 && self.target == self.current
    }

    /// Reads one frame `back` frames before the line's last one, at the
    /// tap's delay, and advances a running crossfade by one sample.  A
    /// settled tap reads the line as is.
    #[inline]
    pub(crate) fn next(&mut self, line: &DelayLine, back: usize) -> [f32; 2] {
        if self.progress >= 1.0 {
            if self.target == self.current {
                return line.read(back + self.current as usize);
            }
            self.previous = self.current;
            self.current = self.target;
            self.progress = 0.0;
        }
        let new = line.read(back + self.current as usize);
        let old = line.read(back + self.previous as usize);
        let weight = self.progress;
        self.progress = (self.progress + self.step).min(1.0);
        [
            old[0] + (new[0] - old[0]) * weight,
            old[1] + (new[1] - old[1]) * weight,
        ]
    }
}

/// How long [`Declick`] takes to fade out the step at a switch between two
/// signals.
pub(crate) const DECLICK_SECONDS: f32 = 0.002;

/// Smooths a hard switch between two signals that do not meet, where no
/// crossfade is possible because the old one is gone (a plugin that stops
/// running, a chain that misses a chunk): the step between where the old
/// signal was heading (its last two frames, extrapolated) and the new
/// signal's first frame is added back as an offset that fades to 0, so the
/// output stays continuous.  Two signals that do meet get (almost) no
/// offset.  The sound still changes; only the click goes.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Declick {
    offset: [f32; 2],
    remaining: u32,
    len: u32,
    /// The last two frames played, oldest first.
    history: [[f32; 2]; 2],
}

impl Declick {
    /// Notes the frames just played (call it after every chunk).
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

    /// Starts smoothing a switch to a signal whose first frame is `first`,
    /// over `len` frames.
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

    /// One frame of the new signal, with what is left of the step added.
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
mod tests {
    use super::{Declick, DelayLine, DelayTap};

    #[test]
    fn a_declick_starts_on_the_old_value_and_fades_to_the_new_signal() {
        let mut declick = Declick::default();
        declick.note(&[1.0, 1.0], &[-1.0, -1.0]);
        declick.start([0.0, 0.0], 4);
        assert_eq!(declick.apply([0.0, 0.0]), [1.0, -1.0]);
        assert_eq!(declick.apply([0.0, 0.0]), [0.75, -0.75]);
        declick.apply([0.0, 0.0]);
        declick.apply([0.0, 0.0]);
        assert!(!declick.is_active());
        assert_eq!(declick.apply([0.5, 0.5]), [0.5, 0.5]);
    }

    #[test]
    fn a_line_delays_by_whole_frames_and_starts_silent() {
        let mut line = DelayLine::new(4);
        assert_eq!(line.max_delay(), 4);
        line.push([1.0, -1.0]);
        assert_eq!(line.read(0), [1.0, -1.0]);
        assert_eq!(line.read(1), [0.0, 0.0]);
        for value in 2..=10 {
            line.push([value as f32, 0.0]);
        }
        assert_eq!(line.read(0)[0], 10.0);
        assert_eq!(line.read(4)[0], 6.0);
        // Past the capacity: silence.
        assert_eq!(line.read(5)[0], 0.0);
        line.reset();
        assert_eq!(line.read(0), [0.0, 0.0]);
        line.push([3.0, 3.0]);
        assert_eq!(line.read(0), [3.0, 3.0]);
        assert_eq!(line.read(1), [0.0, 0.0]);
    }

    #[test]
    fn an_empty_line_reads_silence() {
        let mut line = DelayLine::empty();
        line.push([1.0, 1.0]);
        assert_eq!(line.read(0), [0.0, 0.0]);
        assert_eq!(line.max_delay(), 0);
    }

    #[test]
    fn a_tap_crossfades_to_a_new_delay_and_settles() {
        let mut line = DelayLine::new(64);
        let mut tap = DelayTap::new(0);
        let mut out = Vec::new();
        for index in 0..200 {
            // A ramp, so both read positions differ by a known amount.
            line.push([index as f32, 0.0]);
            if index == 50 {
                tap.set_target(10, 20.0);
            }
            out.push(tap.next(&line, 0)[0]);
        }
        assert_eq!(out[49], 49.0);
        // The fade moves from 0 to 10 frames behind over 20 samples: never a
        // jump bigger than the ramp's own step plus the fade's share.
        let largest = out
            .windows(2)
            .map(|pair| (pair[1] - pair[0]).abs())
            .fold(0.0f32, f32::max);
        assert!(largest <= 1.0, "largest step {largest}");
        assert!(tap.is_settled());
        assert_eq!(out[199], 189.0);
    }

    #[test]
    fn a_change_during_a_fade_follows_once_it_ends() {
        let mut line = DelayLine::new(64);
        let mut tap = DelayTap::new(0);
        for index in 0..100 {
            line.push([index as f32, 0.0]);
            if index == 10 {
                tap.set_target(8, 10.0);
            }
            if index == 12 {
                tap.set_target(16, 10.0);
            }
            tap.next(&line, 0);
        }
        assert!(tap.is_settled());
        assert_eq!(tap.target(), 16);
        assert_eq!(tap.next(&line, 0)[0], 99.0 - 16.0);
    }
}
