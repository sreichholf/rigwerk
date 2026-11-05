//! Turning the ring into output frames: routing, gain, drift trimming,
//! underrun recovery, rate conversion and the built-in EQ, shared by every
//! output path.

use std::sync::atomic::Ordering;
use std::time::Instant;

use super::denormal::DenormalGuard;
use super::eq::{EqSlot, Equalizer};
use super::gate::{GateSlot, NoiseGate};
use super::resampler::FrameResampler;
use super::shared::Shared;

/// The most drift steering may trim the conversion ratio.  500 ppm is under
/// one cent of pitch.  Real clock offsets between two interfaces are a few
/// tens of ppm, so the loop normally runs far inside this limit.
const DRIFT_LIMIT_PPM: f32 = 500.0;
/// The same limit in ratio units, for the integral term.
const DRIFT_LIMIT_RATIO: f32 = DRIFT_LIMIT_PPM * 1e-6;
/// Time constant of the low-pass on the ring-fill error.  The fill is read
/// once per output block and jumps by a whole input block whenever the input
/// delivers, so the raw reading carries a sawtooth of up to one input block
/// that says nothing about the clocks.
const DRIFT_FILTER_SECONDS: f32 = 0.5;
/// Proportional gain, in ratio per second of filtered fill error (60 ppm per
/// millisecond).  Low enough that the filtered block sawtooth moves the ratio
/// by tens of ppm, not across the whole range.
const DRIFT_KP: f32 = 0.06;
/// Integral gain, in ratio per second² of filtered fill error.  With the
/// proportional gain the loop is damped (about 0.87) with a settling time of
/// a few tens of seconds: the integral learns the steady clock offset and the
/// ratio then moves only by a few ppm.
const DRIFT_KI: f32 = 0.0012;

/// How long an underrun takes to fade the held frame to silence, and how long
/// playback takes to fade back in once the cushion has refilled.  Long enough
/// that neither edge clicks, short enough not to smear the note around it.
const DROPOUT_FADE_SECONDS: f32 = 0.0025;

/// A control change is ramped over this long, whatever block size the host
/// hands over: long enough to hide the step, short enough to feel immediate.
const CONTROL_RAMP_SECONDS: f32 = 0.006;

/// One control value moving linearly to its target, one sample at a time.
/// The remaining step is kept between callbacks, so a ramp that spans several
/// blocks keeps its slope instead of restarting at every block boundary.
#[derive(Clone, Copy)]
struct Ramp {
    current: f32,
    target: f32,
    step: f32,
}

impl Ramp {
    fn new(value: f32) -> Self {
        Self {
            current: value,
            target: value,
            step: 0.0,
        }
    }

    /// Adopts a value without moving: used for the first block of a mixer.
    fn snap(&mut self, target: f32) {
        self.current = target;
        self.target = target;
        self.step = 0.0;
    }

    /// Points the ramp at `target`; the move takes `samples` samples.  A
    /// target that did not change leaves a ramp that is still moving alone.
    fn set_target(&mut self, target: f32, samples: f32) {
        if target == self.target {
            return;
        }
        self.target = target;
        let step = if samples > 0.0 {
            (target - self.current) / samples
        } else {
            0.0
        };
        // A step below one representable unit would never arrive.  Snap that
        // (inaudible) remainder instead of moving forever.
        if target == self.current || step == 0.0 || self.current + step == self.current {
            self.current = target;
            self.step = 0.0;
        } else {
            self.step = step;
        }
    }

    /// Moves one sample and returns the value to apply.  Once the target is
    /// reached it returns exactly the target for the rest of the block, so a
    /// settled ramp adds no noise to the audio.
    fn next(&mut self) -> f32 {
        if self.current != self.target {
            let next = self.current + self.step;
            if (self.step > 0.0 && next >= self.target) || (self.step < 0.0 && next <= self.target)
            {
                self.current = self.target;
                self.step = 0.0;
            } else {
                self.current = next;
            }
        }
        self.current
    }
}

/// All per-sample control ramps of the output stage.  Routing is linear, so a
/// route change is eight gains moving between 0 and 1; channel enable and mono
/// work the same way, and mute, monitor and the master level fold into one
/// effective output gain so they compose without a step.
struct MixRamps {
    route: [[Ramp; 4]; 2],
    output_enabled: [Ramp; 2],
    mono: Ramp,
    gain: Ramp,
}

impl MixRamps {
    fn new() -> Self {
        Self {
            route: [[Ramp::new(0.0); 4]; 2],
            output_enabled: [Ramp::new(1.0); 2],
            mono: Ramp::new(0.0),
            gain: Ramp::new(1.0),
        }
    }

    /// First block: adopt the live settings directly, so starting a bridge
    /// neither fades in nor jumps to them.
    fn snap(&mut self, routes: &[[f32; 4]; 2], enabled: [f32; 2], mono: f32, gain: f32) {
        for (row, targets) in self.route.iter_mut().zip(routes) {
            for (ramp, target) in row.iter_mut().zip(targets) {
                ramp.snap(*target);
            }
        }
        for (ramp, target) in self.output_enabled.iter_mut().zip(enabled) {
            ramp.snap(target);
        }
        self.mono.snap(mono);
        self.gain.snap(gain);
    }

    fn set_targets(
        &mut self,
        routes: &[[f32; 4]; 2],
        enabled: [f32; 2],
        mono: f32,
        gain: f32,
        samples: f32,
    ) {
        for (row, targets) in self.route.iter_mut().zip(routes) {
            for (ramp, target) in row.iter_mut().zip(targets) {
                ramp.set_target(*target, samples);
            }
        }
        for (ramp, target) in self.output_enabled.iter_mut().zip(enabled) {
            ramp.set_target(target, samples);
        }
        self.mono.set_target(mono, samples);
        self.gain.set_target(gain, samples);
    }

    /// Routing, channel enable and mono for one input frame.  This is applied
    /// before the EQ and the plugin chain, where those controls always acted.
    fn next_input(&mut self, left: f32, right: f32) -> [f32; 4] {
        let mut routes = [[0.0f32; 4]; 2];
        for (input, row) in routes.iter_mut().enumerate() {
            for (tap, value) in row.iter_mut().enumerate() {
                *value = self.route[input][tap].next();
            }
        }
        let mut out = route_frame(left, right, &routes);
        out[0] *= self.output_enabled[0].next();
        out[1] *= self.output_enabled[1].next();
        let mono = self.mono.next();
        if mono != 0.0 {
            let sum = (out[0] + out[1]) * 0.5;
            for value in out.iter_mut() {
                *value += (sum - *value) * mono;
            }
        }
        out
    }

    /// The effective master gain for one output frame.  Mute, monitor and the
    /// level slider all fold into this value, so any combination of them can
    /// change without a discontinuity.
    fn next_output_gain(&mut self) -> f32 {
        self.gain.next()
    }
}

/// Takes the next frame from the bridge.  An empty ring returns the previous
/// frame (so the resampler's window stays continuous) and marks the callback
/// as late, so the mixer fades out and the buffer can grow for the next round.
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
    /// Playback gain around an underrun, 0..=1.  It is 1 while the ring
    /// delivers.  When the ring runs dry the last frame is held and this
    /// falls to 0 over [`DROPOUT_FADE_SECONDS`] (into the re-priming silence
    /// if the block ends first); when priming ends it rises back to 1 over
    /// the same time.  It starts at 1, so a ring that is already full on the
    /// first block plays straight away; a bridge that primes for a while
    /// fades in from that silence like any other recovery.  Both ends snap,
    /// so float rounding never leaves a finished fade a hair short of 0 or 1.
    fade_gain: f32,
    /// The frame held while fading out after an underrun.
    held: (f32, f32),
    /// Whether any ring frame has been played yet.  Only a ring that is full
    /// before the first frame starts at full level instead of fading in.
    played: bool,
    /// The ring ran dry in this block.  The rest of the block fades the held
    /// frame out instead of picking up whatever trickles in meanwhile; the
    /// next block re-primes as before.
    dropping: bool,
    priming: bool,
    started: bool,
    // Drift correction steers the resampler ratio from the ring-fill error
    // instead of dropping or repeating frames.  `drift_ppm` is the value the
    // resampler was last trimmed by, reported to the UI as the clock offset;
    // `drift_integral` is the slow part of the loop, which holds the learned
    // clock offset; `drift_error` is the low-passed fill error, `None` until
    // the first reading after priming.
    drift_ppm: f32,
    drift_integral: f32,
    drift_error: Option<f32>,
    /// Whether the last block went through the resampler.  Switching between
    /// it and the 1:1 bypass resets it, so it never replays stale history.
    resampling: bool,
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
    /// Built-in noise gate and the settings version it was last updated from.
    gate: NoiseGate,
    gate_version: u64,
    /// Per-sample ramps for routing, channel enable, mono and the output
    /// level.  A control read once per block would step; these move over a
    /// few milliseconds instead.
    ramps: MixRamps,
    /// The first block adopts the live settings directly.  Later blocks ramp
    /// toward them, so opening the bridge does not fade in from the defaults.
    controls_adopted: bool,
}

impl OutputMixer {
    pub(crate) fn new(output_rate: u32) -> Self {
        let output_rate = output_rate.max(1);
        Self {
            output_rate,
            source_rate: output_rate,
            resampler: FrameResampler::new(output_rate, output_rate),
            last: (0.0, 0.0),
            fade_gain: 1.0,
            held: (0.0, 0.0),
            played: false,
            dropping: false,
            // Wait for the ring to reach its target before draining it.
            priming: true,
            started: false,
            drift_ppm: 0.0,
            drift_integral: 0.0,
            drift_error: None,
            resampling: false,
            stable_since: Instant::now(),
            block: Vec::new(),
            plugin_l: Vec::new(),
            plugin_r: Vec::new(),
            eq: [Equalizer::new(output_rate), Equalizer::new(output_rate)],
            eq_version: [0; 2],
            gate: NoiseGate::new(output_rate),
            gate_version: 0,
            ramps: MixRamps::new(),
            controls_adopted: false,
        }
    }

    /// Grows the per-block scratch while the bridge is still opening, so the
    /// first callback (and the first block after a plugin is added) never has
    /// to allocate.  A host that later hands over a larger block still works;
    /// that one block then grows the scratch.
    pub(crate) fn prepare(&mut self, frames: usize) {
        if self.block.len() < frames {
            self.block.resize(frames, [0.0; 4]);
        }
        if self.plugin_l.len() < frames {
            self.plugin_l.resize(frames, 0.0);
            self.plugin_r.resize(frames, 0.0);
        }
    }

    /// Points every mix ramp at the settings for this block.  The first block
    /// snaps, so a bridge opens at the current mix; after that a change takes
    /// [`CONTROL_RAMP_SECONDS`], however long the block itself is.
    fn follow_controls(&mut self, shared: &Shared) {
        let coefficient = |flag: bool| if flag { 1.0 } else { 0.0 };
        let mut routes = [[0.0f32; 4]; 2];
        for (input, row) in routes.iter_mut().enumerate() {
            for (output, coefficient_out) in row.iter_mut().enumerate() {
                *coefficient_out =
                    coefficient(shared.routes[input * 4 + output].load(Ordering::Relaxed));
            }
        }
        let enabled = [
            coefficient(shared.output_enabled[0].load(Ordering::Relaxed)),
            coefficient(shared.output_enabled[1].load(Ordering::Relaxed)),
        ];
        let mono = coefficient(shared.mono.load(Ordering::Relaxed));
        let silent = shared.mute.load(Ordering::Relaxed) || !shared.monitor.load(Ordering::Relaxed);
        let gain = if silent { 0.0 } else { shared.output_gain() };
        let samples = CONTROL_RAMP_SECONDS * self.output_rate as f32;
        if self.controls_adopted {
            self.ramps
                .set_targets(&routes, enabled, mono, gain, samples);
        } else {
            self.ramps.snap(&routes, enabled, mono, gain);
            self.controls_adopted = true;
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

    /// Picks up gate changes made in the UI since the last block.
    fn follow_gate(&mut self, shared: &Shared) {
        let version = shared.gate_version();
        if version == self.gate_version {
            return;
        }
        if let Some(settings) = shared.try_gate() {
            self.gate.update(settings, self.output_rate);
            self.gate_version = version;
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
            self.drift_error = None;
        }
    }

    /// Steers the conversion ratio from the ring-fill error.  A fill above
    /// the target makes the resampler consume source frames a little faster,
    /// one below it a little slower; the integral term learns the steady
    /// clock offset.  Returns the applied correction in ppm, which is zero
    /// whenever drift is off or the ring is still priming.
    fn steer_drift(&mut self, shared: &Shared, target: usize, seconds: f32, drift: bool) -> f32 {
        if !drift {
            self.drift_ppm = 0.0;
            self.drift_integral = 0.0;
            self.drift_error = None;
            return 0.0;
        }
        if self.priming {
            // Nothing is drained while priming.  The learned clock offset in
            // the integral stays: the clocks have not changed, and keeping it
            // is what stops a small cushion from underrunning again.
            self.drift_ppm = 0.0;
            self.drift_error = None;
            return 0.0;
        }
        // Fill error as seconds of audio, so the gains do not depend on the
        // rate or the block size.  The loop works in ratio units and only
        // converts to ppm for the resampler and the readout.
        let raw = (shared.ring_frames() as f32 - target as f32) / self.source_rate as f32;
        let error = match self.drift_error {
            Some(filtered) => {
                let weight = (seconds / DRIFT_FILTER_SECONDS).min(1.0);
                filtered + (raw - filtered) * weight
            }
            None => raw,
        };
        self.drift_error = Some(error);
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
        // Decaying EQ and gate states must not drop into the slow subnormal
        // range; the inserts run inside this block and get the same mode.
        let _denormals = DenormalGuard::new();
        self.follow_source_rate(shared);
        self.follow_eq(shared);
        self.follow_gate(shared);
        self.follow_controls(shared);
        let pre_eq = self.eq[EqSlot::Pre.index()].active();
        let post_eq = self.eq[EqSlot::Post.index()].active();
        let gate_pre = self.gate.active() && self.gate.settings().slot == GateSlot::Pre;
        let gate_post = self.gate.active() && self.gate.settings().slot == GateSlot::Post;
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
        // its window holds at most about a millisecond, so the purge becomes
        // a short crossfade through the kernel instead of a restart.
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
        // After an underrun the held frame must have faded out first (at most
        // DROPOUT_FADE_SECONDS), so a quick refill fades in from silence
        // instead of jumping from the half-faded held frame to new audio.
        // Only a ring that is full before anything played starts at full level.
        let faded = self.fade_gain == 0.0 || !self.played;
        if self.priming && faded && shared.ring_frames() >= target {
            self.priming = false;
        }

        // Steer the conversion ratio rather than splicing frames: the
        // resampler keeps running and its step is trimmed by a few ppm, so a
        // clock offset becomes a tiny pitch offset instead of a click.  With
        // drift off, the step is the exact base ratio.  A 1:1 bridge with
        // drift on still goes through the interpolator so that path exists
        // when the clocks start to differ.
        let seconds = frames as f32 / self.output_rate as f32;
        let drift_ppm = self.steer_drift(shared, target, seconds, drift);
        self.resampler.set_correction_ppm(drift_ppm);
        let needs_resample = self.resampler.active() || drift;
        if needs_resample != self.resampling {
            // Entering the resampler must not reuse history from before the
            // bypass; it restarts at the next ring frame.  Leaving it skips
            // the few frames it had fetched ahead (half its kernel), once,
            // when drift is switched off on a 1:1 bridge.
            self.resampler.reset();
            self.resampling = needs_resample;
        }

        let mut peak_l = 0.0f32;
        let mut peak_r = 0.0f32;
        let mut missed = false;
        let fade_step = 1.0 / (DROPOUT_FADE_SECONDS * self.output_rate as f32).max(1.0);
        if self.block.len() < frames {
            self.block.resize(frames, [0.0; 4]);
        }
        for index in 0..frames {
            let (left, right) = if self.priming || self.dropping {
                // After an underrun: keep fading the held frame down, into
                // the priming silence if it outlasts this block.  A silent
                // held frame (or none yet) has nothing to fade.
                self.fade_gain = if self.held == (0.0, 0.0) {
                    0.0
                } else {
                    fade_down(self.fade_gain, fade_step)
                };
                let out = (self.held.0 * self.fade_gain, self.held.1 * self.fade_gain);
                // Once silent, resume as soon as the cushion is back, inside
                // this block, rather than waiting for the next one.
                if self.priming && self.fade_gain == 0.0 && shared.ring_frames() >= target {
                    self.priming = false;
                }
                out
            } else {
                let last = self.last;
                let mut dry = false;
                let frame = if needs_resample {
                    self.resampler
                        .next(|| take_ring_frame(shared, &mut dry, last))
                } else {
                    take_ring_frame(shared, &mut dry, last)
                };
                self.last = frame;
                if dry {
                    // The ring ran dry: hold this frame and fade it out
                    // from wherever the gain is, instead of holding it at
                    // full level and then dropping to silence.
                    missed = true;
                    self.dropping = true;
                    self.held = frame;
                    self.fade_gain = fade_down(self.fade_gain, fade_step);
                } else {
                    self.played = true;
                    if self.fade_gain < 1.0 {
                        // Back from an underrun: fade in instead of starting
                        // on a step.
                        self.fade_gain = fade_up(self.fade_gain, fade_step);
                    }
                }
                (frame.0 * self.fade_gain, frame.1 * self.fade_gain)
            };
            // Routing, channel enable and mono move with the ramps, before
            // the EQ and the inserts, which is where those controls act.
            let mut out = self.ramps.next_input(left, right);
            // The chain on the master mix (taps 1/2), in order: pre EQ, optional
            // PRE gate, plugin inserts, optional POST gate, post EQ, then the
            // output level and mute.  Built-ins are per-sample → no added latency.
            if pre_eq {
                (out[0], out[1]) = self.eq[EqSlot::Pre.index()].process(out[0], out[1]);
            }
            if gate_pre {
                (out[0], out[1]) = self.gate.process(out[0], out[1]);
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

        // A plugin may have cleared flush-to-zero; the POST gate and post EQ
        // still decay into silence and need it.  Dropped before the outer
        // guard, so the thread's original mode still comes back last.
        let _post_denormals = DenormalGuard::new();
        // POST gate (if selected), post EQ, then the output stage last.
        for index in 0..frames {
            let mut out = self.block[index];
            if gate_post {
                (out[0], out[1]) = self.gate.process(out[0], out[1]);
            }
            if post_eq {
                (out[0], out[1]) = self.eq[EqSlot::Post.index()].process(out[0], out[1]);
            }
            // Mute, monitor and the level are one ramp, so any of them can move
            // without a step.  One call per frame keeps it aligned with the
            // input ramps above.
            let gain = self.ramps.next_output_gain();
            for value in out.iter_mut() {
                *value *= gain;
            }
            peak_l = peak_l.max(out[0].abs());
            peak_r = peak_r.max(out[1].abs());
            shared.push_scope_out(out[0], out[1]);
            write(index, out);
        }

        shared.frames.fetch_add(frames as u64, Ordering::Relaxed);
        if missed {
            shared.underruns.fetch_add(1, Ordering::Relaxed);
            // Grow the target and rebuild the cushion instead of stuttering
            // on every callback.
            shared.raise_target_after_underrun();
            self.priming = true;
            // The held frame keeps fading through the priming silence.
            self.dropping = false;
            self.resampler.reset();
            // The integral (the learned clock offset) survives the underrun;
            // only the filtered fill starts over.
            self.drift_ppm = 0.0;
            self.drift_error = None;
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

/// One step of the underrun fade-out, snapping to 0 instead of stopping a
/// rounding error above it.
fn fade_down(gain: f32, step: f32) -> f32 {
    let next = gain - step;
    if next < step * 0.5 {
        0.0
    } else {
        next
    }
}

/// One step of the fade-in, snapping to 1 the same way.
fn fade_up(gain: f32, step: f32) -> f32 {
    let next = gain + step;
    if next > 1.0 - step * 0.5 {
        1.0
    } else {
        next
    }
}

pub(crate) fn route_frame(left: f32, right: f32, routes: &[[f32; 4]; 2]) -> [f32; 4] {
    let mut out = [0.0f32; 4];
    for (tap, value) in out.iter_mut().enumerate() {
        *value = left * routes[0][tap] + right * routes[1][tap];
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::super::eq::{EqSettings, EqSlot};
    use super::super::shared::Shared;
    use super::super::types::CushionMode;
    use super::{route_frame, OutputMixer, Ramp, CONTROL_RAMP_SECONDS};

    #[test]
    fn default_routes_keep_channels_apart() {
        let routes = [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0]];
        assert_eq!(route_frame(0.5, -0.25, &routes), [0.5, -0.25, 0.0, 0.0]);
    }

    #[test]
    fn a_tap_can_receive_both_inputs() {
        let routes = [[0.0, 1.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0]];
        assert_eq!(route_frame(0.5, 0.25, &routes), [0.0, 0.75, 0.0, 0.0]);
    }

    #[test]
    fn partial_routes_scale_each_input() {
        let routes = [[0.5, 0.0, 0.0, 0.0], [0.0, 0.5, 0.0, 0.0]];
        assert_eq!(route_frame(0.5, -0.25, &routes), [0.25, -0.125, 0.0, 0.0]);
    }

    #[test]
    fn muted_matrix_is_silent() {
        let routes = [[0.0; 4]; 2];
        assert_eq!(route_frame(0.5, 0.5, &routes), [0.0; 4]);
    }

    #[test]
    fn ramp_lands_on_its_target_and_holds() {
        let mut ramp = Ramp::new(0.0);
        ramp.set_target(1.0, 4.0);
        let values: Vec<f32> = (0..6).map(|_| ramp.next()).collect();
        assert_eq!(values, vec![0.25, 0.5, 0.75, 1.0, 1.0, 1.0]);
    }

    #[test]
    fn ramp_keeps_its_slope_when_the_target_is_repeated() {
        let mut ramp = Ramp::new(0.0);
        ramp.set_target(1.0, 4.0);
        assert_eq!(ramp.next(), 0.25);
        // A block boundary that repeats the same target must not restart the
        // move from wherever the ramp currently sits.
        ramp.set_target(1.0, 4.0);
        assert_eq!(ramp.next(), 0.5);
    }

    #[test]
    fn ramp_retargets_from_where_it_is() {
        let mut ramp = Ramp::new(0.0);
        ramp.set_target(1.0, 4.0);
        assert_eq!(ramp.next(), 0.25);
        ramp.set_target(0.0, 2.0);
        assert_eq!(ramp.next(), 0.125);
        assert_eq!(ramp.next(), 0.0);
    }

    #[test]
    fn snap_adopts_a_value_without_moving() {
        let mut ramp = Ramp::new(1.0);
        ramp.snap(0.25);
        assert_eq!(ramp.next(), 0.25);
        assert_eq!(ramp.next(), 0.25);
    }

    /// A 1:1 bridge with drift off, a deep cushion and the EQs bypassed, so a
    /// rendered frame is the mix itself and not the resampler or a filter.
    fn steady_mixer(signal: (f32, f32)) -> (OutputMixer, Shared) {
        let shared = Shared::new(48_000);
        shared.set_source_rate(48_000);
        shared.set_cushion(CushionMode::Manual, 100.0);
        shared.set_mix(false, true, false, [true, true], [true, true], false);
        let bypassed = EqSettings {
            enabled: false,
            ..EqSettings::default()
        };
        shared.set_eq(EqSlot::Pre, bypassed);
        shared.set_eq(EqSlot::Post, bypassed);
        for _ in 0..5_000 {
            shared.push_frame(signal.0, signal.1);
        }
        (OutputMixer::new(48_000), shared)
    }

    fn render(mixer: &mut OutputMixer, shared: &Shared, frames: usize) -> Vec<[f32; 4]> {
        let mut out = vec![[0.0f32; 4]; frames];
        mixer.render(shared, frames, |index, frame| out[index] = frame);
        out
    }

    fn store_gain(shared: &Shared, gain: f32) {
        shared.output_gain.store(gain.to_bits(), Ordering::Relaxed);
    }

    #[test]
    fn the_first_block_adopts_the_live_level() {
        let (mut mixer, shared) = steady_mixer((0.5, -0.25));
        store_gain(&shared, 0.25);
        let out = render(&mut mixer, &shared, 64);
        assert!(out.iter().all(|frame| frame == &[0.125, -0.0625, 0.0, 0.0]));
    }

    #[test]
    fn a_level_change_ramps_across_blocks_and_then_holds() {
        let (mut mixer, shared) = steady_mixer((0.5, -0.25));
        let adopted = render(&mut mixer, &shared, 32);
        assert_eq!(adopted[0], [0.5, -0.25, 0.0, 0.0]);

        store_gain(&shared, 0.5);
        let ramp_frames = (CONTROL_RAMP_SECONDS * 48_000.0).round() as usize;
        let first = render(&mut mixer, &shared, 100);
        let second = render(&mut mixer, &shared, ramp_frames);
        let joined = [first.clone(), second.clone()].concat();

        assert!(first[0][0] < 0.5 && first[0][0] > 0.49);
        let step = (first[0][0] - first[1][0]).abs();
        let full_drop = 0.5 - 0.25;
        assert!(step > 0.0 && step < full_drop / 50.0);
        // The block boundary continues the same slope instead of jumping
        // back toward the old level or landing on the new one early.
        let boundary = (first[99][0] - second[0][0]).abs();
        assert!(boundary < step * 1.5);

        let settled_at = joined
            .iter()
            .position(|frame| frame[0] == 0.25)
            .expect("ramp reaches the new level");
        assert!((settled_at as i32 - (ramp_frames as i32 - 1)).abs() <= 2);
        assert!(joined[settled_at..].iter().all(|frame| frame[0] == 0.25));
        assert!(joined[settled_at..].iter().all(|frame| frame[1] == -0.125));
    }

    #[test]
    fn mute_fades_out_and_a_new_level_fades_back_in() {
        let (mut mixer, shared) = steady_mixer((0.5, 0.0));
        let _adopted = render(&mut mixer, &shared, 16);
        store_gain(&shared, 0.5);
        shared.set_mix(true, true, false, [true, true], [true, true], false);
        let muted = render(&mut mixer, &shared, 320);
        assert!(muted[0][0] > 0.4);
        assert!(muted.last().unwrap()[0] == 0.0);
        assert!(muted.iter().all(|frame| frame[1] == 0.0));

        shared.set_mix(false, true, false, [true, true], [true, true], false);
        let restored = render(&mut mixer, &shared, 320);
        assert!(restored[0][0] > 0.0 && restored[0][0] < 0.2);
        assert_eq!(restored.last().unwrap()[0], 0.25);
    }

    #[test]
    fn monitor_off_fades_to_silence() {
        let (mut mixer, shared) = steady_mixer((0.5, 0.0));
        let _adopted = render(&mut mixer, &shared, 16);
        shared.set_mix(false, false, false, [true, true], [true, true], false);
        let out = render(&mut mixer, &shared, 320);
        assert!(out[0][0] > 0.4);
        assert_eq!(out.last().unwrap()[0], 0.0);
    }

    #[test]
    fn a_route_change_crossfades_and_leaves_other_taps_put() {
        let (mut mixer, shared) = steady_mixer((0.5, 0.0));
        let adopted = render(&mut mixer, &shared, 16);
        assert_eq!(adopted[0], [0.5, 0.0, 0.0, 0.0]);

        // Keep left on tap 1, and move it off tap 0.  Tap 1 fades in; tap 0
        // fades out; nothing else starts carrying the input.
        shared.set_routes([[false, true, false, false], [false, false, false, false]]);
        let out = render(&mut mixer, &shared, 320);
        assert!(out[0][0] > 0.4 && out[0][1] < 0.1);
        assert_eq!(*out.last().unwrap(), [0.0, 0.5, 0.0, 0.0]);
        assert!(out.iter().all(|frame| frame[2] == 0.0 && frame[3] == 0.0));
        let mid = out[out.len() / 2];
        assert!(mid[0] > 0.1 && mid[0] < 0.4);
        assert!(mid[1] > 0.1 && mid[1] < 0.4);
    }

    #[test]
    fn disabling_a_channel_fades_that_tap_only() {
        let (mut mixer, shared) = steady_mixer((0.5, -0.25));
        let _adopted = render(&mut mixer, &shared, 16);
        shared.set_mix(false, true, false, [true, true], [false, true], false);
        let out = render(&mut mixer, &shared, 320);
        assert!(out.iter().all(|frame| frame[1] == -0.25));
        assert!(out[0][0] > 0.4);
        assert_eq!(out.last().unwrap()[0], 0.0);
    }

    /// The largest sample-to-sample move on tap 0 across a run of blocks.
    fn largest_step(out: &[[f32; 4]]) -> f32 {
        out.windows(2)
            .map(|pair| (pair[1][0] - pair[0][0]).abs())
            .fold(0.0, f32::max)
    }

    /// Renders `blocks` blocks of 256 frames into one run.
    fn render_blocks(mixer: &mut OutputMixer, shared: &Shared, blocks: usize) -> Vec<[f32; 4]> {
        (0..blocks)
            .flat_map(|_| render(mixer, shared, 256))
            .collect()
    }

    fn underrun_fades_out(drift: bool) {
        let (mut mixer, shared) = steady_mixer((0.8, 0.8));
        shared.set_mix(false, true, false, [true, true], [true, true], drift);
        // 5 000 frames drain in a little under 20 blocks; the 20th runs dry
        // part way through.
        let out = render_blocks(&mut mixer, &shared, 24);
        assert!(shared.underruns.load(Ordering::Relaxed) >= 1);
        assert!((out[100][0] - 0.8).abs() < 1.0e-3, "{}", out[100][0]);
        assert_eq!(out.last().unwrap()[0], 0.0, "the dropout ends in silence");
        let step = largest_step(&out);
        assert!(step < 0.05, "underrun stepped by {step}");
    }

    #[test]
    fn an_underrun_fades_the_held_frame_to_silence() {
        underrun_fades_out(false);
    }

    #[test]
    fn an_underrun_through_the_resampler_fades_too() {
        underrun_fades_out(true);
    }

    #[test]
    fn playback_fades_back_in_after_an_underrun() {
        let (mut mixer, shared) = steady_mixer((0.8, 0.8));
        let mut out = render_blocks(&mut mixer, &shared, 24);
        assert!(shared.underruns.load(Ordering::Relaxed) >= 1);
        let resumed_from = out.len();
        for _ in 0..6_000 {
            shared.push_frame(0.8, 0.8);
        }
        out.extend(render_blocks(&mut mixer, &shared, 4));
        let first = out[resumed_from..]
            .iter()
            .position(|frame| frame[0] != 0.0)
            .map(|index| out[resumed_from + index][0])
            .expect("playback resumes");
        assert!(first > 0.0 && first < 0.05, "resumed at {first}");
        assert_eq!(out.last().unwrap()[0], 0.8);
        let step = largest_step(&out);
        assert!(step < 0.05, "recovery stepped by {step}");
    }

    #[test]
    fn an_underrun_that_fades_out_by_the_block_end_resumes_next_block() {
        let (mut mixer, shared) = steady_mixer((0.8, 0.8));
        // 5 000 frames: four 1024 blocks leave 904, so the fifth runs dry at
        // sample 904 and its 120-sample fade ends exactly at the block end.
        let _played = (0..5)
            .map(|_| render(&mut mixer, &shared, 1024))
            .collect::<Vec<_>>();
        assert_eq!(shared.underruns.load(Ordering::Relaxed), 1);
        for _ in 0..6_000 {
            shared.push_frame(0.8, 0.8);
        }
        let resumed = render(&mut mixer, &shared, 1024);
        assert!(
            resumed[0][0] > 0.0 && resumed[0][0] < 0.05,
            "{}",
            resumed[0][0]
        );
        assert_eq!(resumed[200][0], 0.8);
    }

    #[test]
    fn a_short_priming_fades_in_from_silence() {
        let shared = Shared::new(48_000);
        shared.set_source_rate(48_000);
        shared.set_cushion(CushionMode::Manual, 100.0);
        shared.set_mix(false, true, false, [true, true], [true, true], false);
        let bypassed = EqSettings {
            enabled: false,
            ..EqSettings::default()
        };
        shared.set_eq(EqSlot::Pre, bypassed);
        shared.set_eq(EqSlot::Post, bypassed);
        let mut mixer = OutputMixer::new(48_000);
        // Block 1: nothing has arrived yet.
        let empty = render(&mut mixer, &shared, 64);
        assert!(empty.iter().all(|frame| frame[0] == 0.0));
        // Block 2: the cushion is full and playback fades in from about 0.
        for _ in 0..6_000 {
            shared.push_frame(0.8, 0.8);
        }
        let out = render(&mut mixer, &shared, 64);
        assert!(out[0][0] > 0.0 && out[0][0] < 0.05, "{}", out[0][0]);
        assert!(largest_step(&out) < 0.05);
    }

    #[test]
    fn mono_crossfades_every_tap_to_the_average() {
        let (mut mixer, shared) = steady_mixer((0.5, -0.25));
        let _adopted = render(&mut mixer, &shared, 16);
        shared.set_routes([[true, false, true, false], [false, true, false, false]]);
        // Adopt the new route on its own before mono, so this test only
        // watches the mono crossfade.  One long block settles the route.
        let _routed = render(&mut mixer, &shared, 320);
        shared.set_mix(false, true, true, [true, true], [true, true], false);
        let out = render(&mut mixer, &shared, 320);
        assert!(out[0][0] > 0.4);
        assert_eq!(*out.last().unwrap(), [0.125; 4]);
        // The third tap was carrying the left input and fades toward the average.
        let mid = out[out.len() / 2];
        assert!(mid[2] > 0.125 && mid[2] < 0.5);
    }
}
