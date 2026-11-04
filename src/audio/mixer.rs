//! Turning the ring into output frames: routing, gain, drift trimming,
//! underrun recovery, rate conversion and the built-in EQ, shared by every
//! output path.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use super::eq::{EqSlot, Equalizer};
use super::resampler::FrameResampler;
use super::shared::Shared;

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
    // Drift correction counts the frames it drops or repeats and reports the
    // resulting clock difference in ppm instead of showing a made-up number.
    skipped: i64,
    duplicated: i64,
    measured_frames: u64,
    measured_net: i64,
    measured_at: Instant,
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
            skipped: 0,
            duplicated: 0,
            measured_frames: 0,
            measured_net: 0,
            measured_at: Instant::now(),
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
            self.resampler = FrameResampler::new(source, self.output_rate);
        }
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
        let needs_resample = self.resampler.active();
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
        let slack = (target / 2).max(32);

        // A smaller cushion chosen while playing takes effect at once instead
        // of being trimmed one frame per block.  Normal input bursts never
        // reach this far above the target.
        if !self.priming {
            let excess_line = target * 2 + block_source_frames;
            let fill = shared.ring_frames();
            if fill > excess_line {
                for _ in 0..fill - target {
                    if shared.pop_frame().is_none() {
                        break;
                    }
                }
                self.resampler.reset();
            }
        }

        // Do not drain the ring before the input has had time to fill it,
        // otherwise playback starts with a burst of underruns.
        if self.priming && shared.ring_frames() >= target {
            self.priming = false;
        }

        let mut inject: Option<(f32, f32)> = None;
        if !self.priming && drift {
            let fill = shared.ring_frames();
            if fill > target + slack {
                // Output clock is slower than the input clock: drop one frame
                // so latency stays near the target.
                if shared.pop_frame().is_some() {
                    self.skipped += 1;
                }
            } else if fill + 1 < target.saturating_sub(slack) {
                // Output clock is faster: repeat the previous frame.
                self.duplicated += 1;
                inject = Some(self.last);
            }
        }

        let mut peak_l = 0.0f32;
        let mut peak_r = 0.0f32;
        let mut missed = false;
        if self.block.len() < frames {
            self.block.resize(frames, [0.0; 4]);
        }
        for index in 0..frames {
            let last = self.last;
            let (left, right) = if let Some(pair) = inject.take() {
                pair
            } else if self.priming {
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
            self.stable_since = Instant::now();
        } else if !self.priming && self.stable_since.elapsed() >= shared.relax_after() {
            shared.relax_target();
            self.stable_since = Instant::now();
        }
        shared.out_l.store(peak_l.to_bits(), Ordering::Relaxed);
        shared.out_r.store(peak_r.to_bits(), Ordering::Relaxed);

        self.measured_frames += frames as u64;
        self.measured_net += self.duplicated - self.skipped;
        self.skipped = 0;
        self.duplicated = 0;
        if self.measured_at.elapsed() >= Duration::from_secs(1) {
            let ppm = if drift && self.measured_frames > 0 {
                self.measured_net as f32 / self.measured_frames as f32 * 1_000_000.0
            } else {
                0.0
            };
            shared.drift_ppm.store(ppm.to_bits(), Ordering::Relaxed);
            self.measured_frames = 0;
            self.measured_net = 0;
            self.measured_at = Instant::now();
        }
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
