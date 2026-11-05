//! Turning the ring into output frames: routing, gain, drift trimming,
//! underrun recovery, rate conversion and the built-in EQ, shared by every
//! output path.

use std::sync::atomic::Ordering;
use std::time::Instant;

use super::eq::{EqSlot, Equalizer};
use super::resampler::FrameResampler;
use super::shared::Shared;

/// The most drift steering may trim the conversion ratio.  500 ppm is under
/// one cent of pitch, so even the clamp point stays inaudible.
const DRIFT_LIMIT_PPM: f32 = 500.0;
/// The same limit in ratio units, for the integral term.
const DRIFT_LIMIT_RATIO: f32 = DRIFT_LIMIT_PPM * 1e-6;
/// Proportional gain, in ratio per second of ring-fill error (500 ppm per
/// millisecond of error).  Together with the integral it damps a fill
/// excursion without stepping the pitch.
const DRIFT_KP: f32 = 0.5;
/// Integral gain, in ratio per second² of ring-fill error.  With the
/// proportional gain this gives a four-second integral time constant, so a
/// steady clock offset is removed slowly; this is what keeps ratio changes
/// inaudible.
const DRIFT_KI: f32 = 0.125;

/// Takes the next frame from the bridge.  A short gap holds the previous
/// sample instead of clicking to silence, and marks the callback as late so
/// the buffer can grow for the next round.
pub(crate) fn take_ring_frame(shared: &Shared, missed: &mut bool, last: (f32, f32)) -> (f32, f32) {
    match shared.pop_frame() {
        Some(pair) => pair,
        None => {
            *missed = true;
            last
        }
    }
}

/// Turns the ring into output frames for any playback path (cpal, ASIO or
/// REAL-style WASAPI): routing, gain, drift trimming, underrun recovery and
/// conversion from whatever rate the input really runs at.
pub(crate) struct OutputMixer {
    output_rate: u32,
    source_rate: u32,
    resampler: FrameResampler,
    last: (f32, f32),
    priming: bool,
    started: bool,
    // Drift correction steers the resampler ratio from the ring-fill error
    // instead of dropping or repeating frames.  `drift_ppm` is the value the
    // resampler was last trimmed by, reported to the UI as the clock offset;
    // `drift_integral` is the slow part of the loop.
    drift_ppm: f32,
    drift_integral: f32,
    stable_since: Instant,
    /// One output block, kept between callbacks so the VST3 inserts can work
    /// on the finished stereo mix without allocating.
    block: Vec<[f32; 4]>,
    plugin_l: Vec<f32>,
    plugin_r: Vec<f32>,
    /// The two built-in EQs (pre and post inserts), and the settings version
    /// each was last updated from.
    eq: [Equalizer; 2],
    eq_version: [u64; 2],
}

impl OutputMixer {
    pub(crate) fn new(output_rate: u32) -> Self {
        let output_rate = output_rate.max(1);
        Self {
            output_rate,
            source_rate: output_rate,
            resampler: FrameResampler::new(output_rate, output_rate),
            last: (0.0, 0.0),
            // Wait for the ring to reach its target before draining it.
            priming: true,
            started: false,
            drift_ppm: 0.0,
            drift_integral: 0.0,
            stable_since: Instant::now(),
            block: Vec::new(),
            plugin_l: Vec::new(),
            plugin_r: Vec::new(),
            eq: [Equalizer::new(output_rate), Equalizer::new(output_rate)],
            eq_version: [0; 2],
        }
    }

    /// Grows the per-block scratch while the bridge is still opening, so the
    /// first driver callback never has to allocate.
    #[cfg(target_os = "windows")]
    pub(crate) fn prepare(&mut self, frames: usize) {
        if self.block.len() < frames {
            self.block.resize(frames, [0.0; 4]);
        }
        if self.plugin_l.len() < frames {
            self.plugin_l.resize(frames, 0.0);
            self.plugin_r.resize(frames, 0.0);
        }
    }

    /// Picks up EQ changes made in the UI since the last block.
    fn follow_eq(&mut self, shared: &Shared) {
        for slot in EqSlot::ALL {
            let index = slot.index();
            let version = shared.eq_version(slot);
            if version == self.eq_version[index] {
                continue;
            }
            if let Some(settings) = shared.try_eq(slot) {
                self.eq[index].update(settings, self.output_rate);
                self.eq_version[index] = version;
            }
        }
    }

    /// Follows the input's real rate.  It is only known once the input has
    /// opened, and an input that could not take the requested rate reports
    /// its own here.
    pub(crate) fn follow_source_rate(&mut self, shared: &Shared) {
        let source = match shared.source_rate.load(Ordering::Relaxed) {
            0 => self.output_rate,
            rate => rate,
        };
        if source != self.source_rate {
            self.source_rate = source;
            self.resampler.configure(source, self.output_rate);
            self.drift_ppm = 0.0;
            self.drift_integral = 0.0;
        }
    }

    /// Steers the conversion ratio from the ring-fill error.  A fill above
    /// the target makes the resampler consume source frames a little faster,
    /// one below it a little slower; the integral term removes a steady
    /// clock offset over a few seconds.  Returns the applied correction in
    /// ppm, which is zero whenever drift is off or the ring is still priming.
    fn steer_drift(&mut self, shared: &Shared, target: usize, seconds: f32, drift: bool) -> f32 {
        if !drift || self.priming {
            self.drift_ppm = 0.0;
            self.drift_integral = 0.0;
            return 0.0;
        }
        // Fill error as seconds of audio, so the gains do not depend on the
        // rate or the block size.  The loop works in ratio units and only
        // converts to ppm for the resampler and the readout.
        let error = (shared.ring_frames() as f32 - target as f32) / self.source_rate as f32;
        self.drift_integral = (self.drift_integral + DRIFT_KI * error * seconds)
            .clamp(-DRIFT_LIMIT_RATIO, DRIFT_LIMIT_RATIO);
        let correction = DRIFT_KP * error + self.drift_integral;
        self.drift_ppm = (correction * 1_000_000.0).clamp(-DRIFT_LIMIT_PPM, DRIFT_LIMIT_PPM);
        self.drift_ppm
    }

    /// Renders `frames` output frames, handing each one to `write` as four
    /// tap values.
    pub(crate) fn render(
        &mut self,
        shared: &Shared,
        frames: usize,
        mut write: impl FnMut(usize, [f32; 4]),
    ) {
        self.follow_source_rate(shared);
        self.follow_eq(shared);
        let pre_eq = self.eq[EqSlot::Pre.index()].active();
        let post_eq = self.eq[EqSlot::Post.index()].active();
        let gain = shared.output_gain();
        let silent = shared.mute.load(Ordering::Relaxed) || !shared.monitor.load(Ordering::Relaxed);
        let mono = shared.mono.load(Ordering::Relaxed);
        let out_l = shared.output_enabled[0].load(Ordering::Relaxed);
        let out_r = shared.output_enabled[1].load(Ordering::Relaxed);
        let route =
            |input: usize, output: usize| shared.routes[input * 4 + output].load(Ordering::Relaxed);
        let routes = [
            [route(0, 0), route(0, 1), route(0, 2), route(0, 3)],
            [route(1, 0), route(1, 1), route(1, 2), route(1, 3)],
        ];
        let drift = shared.drift_enabled.load(Ordering::Relaxed);
        // The ring holds input frames; one output block drains this many.
        // The very first block is skipped: a host fills its whole buffer once
        // at start-up, and sizing the cushion from that one block would keep
        // latency several times higher than the steady step needs.
        let block_source_frames =
            (frames as u64 * self.source_rate as u64).div_ceil(self.output_rate as u64) as usize;
        if self.started {
            shared.note_output_block(block_source_frames);
        }
        self.started = true;
        let target = shared.target_frames();

        // A smaller cushion chosen while playing takes effect at once instead
        // of being trimmed one frame per block.  Normal input bursts never
        // reach this far above the target.  The resampler is not reset here:
        // its window holds only a fraction of a millisecond, so leaving it
        // alone avoids a filter transient for a purge that is already rare.
        if !self.priming {
            let excess_line = target * 2 + block_source_frames;
            let fill = shared.ring_frames();
            if fill > excess_line {
                for _ in 0..fill - target {
                    if shared.pop_frame().is_none() {
                        break;
                    }
                }
            }
        }

        // Do not drain the ring before the input has had time to fill it,
        // otherwise playback starts with a burst of underruns.
        if self.priming && shared.ring_frames() >= target {
            self.priming = false;
        }

        // Steer the conversion ratio rather than splicing frames: the
        // resampler keeps running and its step is trimmed by at most
        // `DRIFT_LIMIT_PPM`, so a clock offset becomes an inaudible pitch
        // offset instead of a click.  With drift off, the step is the exact
        // base ratio.  A 1:1 bridge with drift on still goes through the
        // interpolator so that path exists when the clocks start to differ.
        let seconds = frames as f32 / self.output_rate as f32;
        let drift_ppm = self.steer_drift(shared, target, seconds, drift);
        self.resampler.set_correction_ppm(drift_ppm);
        let needs_resample = self.resampler.active() || drift;

        let mut peak_l = 0.0f32;
        let mut peak_r = 0.0f32;
        let mut missed = false;
        if self.block.len() < frames {
            self.block.resize(frames, [0.0; 4]);
        }
        for index in 0..frames {
            let last = self.last;
            let (left, right) = if self.priming {
                (0.0, 0.0)
            } else if needs_resample {
                self.resampler
                    .next(|| take_ring_frame(shared, &mut missed, last))
            } else {
                take_ring_frame(shared, &mut missed, last)
            };
            self.last = (left, right);
            let mut out = route_frame(left, right, &routes);
            if !out_l {
                out[0] = 0.0;
            }
            if !out_r {
                out[1] = 0.0;
            }
            if mono {
                let sum = (out[0] + out[1]) * 0.5;
                out = [sum; 4];
            }
            // The chain on the master mix (taps 1/2), in order: pre EQ, the
            // plugin inserts, post EQ, then the output level and mute.  The
            // EQs work one sample at a time, so they add no latency.
            if pre_eq {
                (out[0], out[1]) = self.eq[EqSlot::Pre.index()].process(out[0], out[1]);
            }
            self.block[index] = out;
        }

        if shared.has_plugins() {
            let mut left = std::mem::take(&mut self.plugin_l);
            let mut right = std::mem::take(&mut self.plugin_r);
            if left.len() < frames {
                left.resize(frames, 0.0);
                right.resize(frames, 0.0);
            }
            for index in 0..frames {
                left[index] = self.block[index][0];
                right[index] = self.block[index][1];
            }
            shared.process_plugins(&mut left, &mut right, frames);
            for index in 0..frames {
                self.block[index][0] = left[index];
                self.block[index][1] = right[index];
            }
            self.plugin_l = left;
            self.plugin_r = right;
        }

        // Post EQ, then the output stage last, like a master fader.
        for index in 0..frames {
            let mut out = self.block[index];
            if post_eq {
                (out[0], out[1]) = self.eq[EqSlot::Post.index()].process(out[0], out[1]);
            }
            if silent {
                out = [0.0; 4];
            } else {
                for value in out.iter_mut() {
                    *value *= gain;
                }
            }
            peak_l = peak_l.max(out[0].abs());
            peak_r = peak_r.max(out[1].abs());
            write(index, out);
        }

        shared.frames.fetch_add(frames as u64, Ordering::Relaxed);
        if missed {
            shared.underruns.fetch_add(1, Ordering::Relaxed);
            // Grow the target and rebuild the cushion instead of stuttering
            // on every callback.
            shared.raise_target_after_underrun();
            self.priming = true;
            self.resampler.reset();
            self.drift_ppm = 0.0;
            self.drift_integral = 0.0;
            self.stable_since = Instant::now();
        } else if !self.priming && self.stable_since.elapsed() >= shared.relax_after() {
            shared.relax_target();
            self.stable_since = Instant::now();
        }
        shared.out_l.store(peak_l.to_bits(), Ordering::Relaxed);
        shared.out_r.store(peak_r.to_bits(), Ordering::Relaxed);
        // Report the ratio trim the resampler actually ran with, not a
        // synthetic frame count.
        shared
            .drift_ppm
            .store(self.drift_ppm.to_bits(), Ordering::Relaxed);
    }
}

pub(crate) fn route_frame(left: f32, right: f32, routes: &[[bool; 4]; 2]) -> [f32; 4] {
    let mut out = [0.0f32; 4];
    for (tap, value) in out.iter_mut().enumerate() {
        if routes[0][tap] {
            *value += left;
        }
        if routes[1][tap] {
            *value += right;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::route_frame;

    #[test]
    fn default_routes_keep_channels_apart() {
        let routes = [[true, false, false, false], [false, true, false, false]];
        assert_eq!(route_frame(0.5, -0.25, &routes), [0.5, -0.25, 0.0, 0.0]);
    }

    #[test]
    fn a_tap_can_receive_both_inputs() {
        let routes = [[false, true, false, false], [false, true, false, false]];
        assert_eq!(route_frame(0.5, 0.25, &routes), [0.0, 0.75, 0.0, 0.0]);
    }

    #[test]
    fn muted_matrix_is_silent() {
        let routes = [[false; 4]; 2];
        assert_eq!(route_frame(0.5, 0.5, &routes), [0.0; 4]);
    }
}
