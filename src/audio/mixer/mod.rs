//! Turning the ring into output frames, shared by every playback backend.
//!
//! The order, per frame:
//! 1. The clock side (ring, underrun recovery, drift trimming, rate
//!    conversion) runs once in [`OutputMixer`].  The PipeWire duplex path
//!    skips it and takes the frame straight from the graph buffers.
//! 2. The split: that one stereo frame goes into every live path slot
//!    ([`PathProcessor`]: routing, EQs, gate, plugin inserts).
//! 3. The path gain: level, mute, solo and a removed path's fade-out, as
//!    one ramp per path.  If any active path is soloed, the others fall
//!    silent.  A path that goes live while the mixer plays fades in from
//!    silence (only the mixer's first block adopts the gains outright), and
//!    a path whose plugins were restored but not opened yet stays silent
//!    rather than playing dry.  A removed path whose fade reached silence is
//!    moved to inactive here, at the end of the block.
//! 4. The master (level, mute, monitor) scales each path's frame, before
//!    the alignment: it is shared by every path, but a master change reaches
//!    a delayed path up to its alignment delay later on that device.  (Kept
//!    there so one path stays bit-identical to the chain before paths.)
//! 5. Alignment: each path's frame goes into that path's delay line; each
//!    device mapper reads every path through its own tap, at the path's
//!    alignment delay on that device (see `output_map`, "Alignment").  The
//!    chain latencies are read once per block.
//! 6. The sum: each device mapper maps every live path with that path's
//!    channel map, level and polarity on that device and adds them per
//!    channel, then applies the device level and mute.  A plain stereo sum
//!    of the paths, after the master but not aligned, feeds the master
//!    meters and the scope.
//!    On the ring path, each generic follower's mapped frame goes into
//!    that follower's ring; its own stream plays it (see `follower`).
//!
//! The plugin chain of a path never drops out of the timing: a chunk whose
//! chain lock the UI holds (rare: only structural edits take it) plays the
//! chain's input delayed by its latency instead, so the sum keeps its
//! phase; that chunk loses the inserts' sound (see
//! [`PathProcessor::run_inserts`] and the `path` module docs).
//!
//! Each path's chain (front, inserts, back) runs as one piece per block
//! and is timed with one monotonic clock read per path and stage (the
//! vDSO / QPC clock `Instant` reads, as the drift relax timer already
//! does; no allocation, no lock).  The smoothed time per block and the
//! block's length are published per path (`PathControls::dsp_time`, with a
//! peak that decays over [`DSP_PEAK_SECONDS`]) for
//! Diagnostics and the Live overload issue.
//!
//! One path at 0 dB (path and device level), unmuted, normal polarity is
//! bit-identical to the chain before paths existed: every unity gain is a
//! copy, not a multiply.

use std::sync::atomic::Ordering;
use std::time::Instant;

use super::delay::{Declick, DelayLine, DelayTap, DECLICK_SECONDS, TAP_FADE_SECONDS};
use super::denormal::DenormalGuard;
use super::eq::{EqSlot, Equalizer};
use super::gate::{GateSlot, NoiseGate};
use super::output_map::{
    DeviceFrame, DeviceMapper, PathLines, PathTiming, MAX_ALIGN_DELAY, MAX_FOLLOWERS,
};
use super::path::{DspTime, LiveSlots, PathControls, PathState, MAX_PATHS};
use super::plugins::MAX_ALIGN;
use super::resampler::FrameResampler;
use super::shared::Shared;

#[cfg(test)]
mod align_tests;
pub(crate) mod ramp;

pub(crate) use ramp::Ramp;
use ramp::{fade_down, fade_up, PathRamps, CONTROL_RAMP_SECONDS, DROPOUT_FADE_SECONDS};

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

/// The chain of one signal path, from the split to the sum: routing,
/// channel enable and mono (ramped), the pre EQ, a PRE gate, the plugin
/// inserts, a POST gate and the post EQ.  [`OutputMixer`] owns one per
/// possible path, preallocated, and runs only the active ones; every path
/// gets the same stereo frame from the clock side.
pub(crate) struct PathProcessor {
    /// One block of this path's taps, kept between callbacks so the plugin
    /// inserts can work on the whole block without allocating.
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
    /// Per-sample ramps for routing, channel enable and mono.  A control
    /// read once per block would step; these move over a few milliseconds
    /// instead.
    ramps: PathRamps,
    /// The first block adopts the live settings directly.  Later blocks ramp
    /// toward them, so opening the bridge does not fade in from the defaults.
    controls_adopted: bool,
    /// Which built-in stages run in the current block.
    plan: BlockPlan,
    /// The slot generation this chain was started for.  A different one in
    /// the controls means the slot was (re)activated: start fresh.
    generation: u64,
    /// Path level, mute, solo and the removal fade as one gain.
    gain: Ramp,
    /// The slot was being removed when this block began.
    fading: bool,
    /// Frames a removed, silent path still runs so its alignment delay
    /// lines play out; `None` until its fade reaches silence.
    drain: Option<u32>,
    /// The chain latency (frames) read at the start of the block: what the
    /// alignment counts for this path, and what a missed chain lock is
    /// replaced by.
    latency: u32,
    /// The same, before clamping to [`MAX_ALIGN`] (for the report).
    latency_raw: u32,
    /// The input of the plugin chain, for the blocks the chain cannot run
    /// (see [`Self::run_inserts`]): read back delayed by the chain latency.
    chain_line: DelayLine,
    chain_tap: DelayTap,
    /// How much of the chain's own output plays: 0 right after a missed
    /// lock, back to 1 over [`CHAIN_RESUME_SECONDS`].
    chain_mix: f32,
    chain_step: f32,
    /// Frames the chain still has to run before its output fades back in
    /// after a missed lock: its inserts missed that input, so for one
    /// latency's worth of frames their delay lines still hold the gap.
    chain_hold: u32,
    /// [`TAP_FADE_SECONDS`] in frames at the current rate.
    tap_fade: f32,
    /// The smoothing of the step into the fallback ([`DECLICK_SECONDS`]
    /// long, `declick_len` frames).
    chain_declick: Declick,
    declick_len: u32,
    /// No chain output has played since the path (re)started: nothing to
    /// smooth a switch from.
    chain_fresh: bool,
    /// The time this block spent on the path so far, and the smoothed time
    /// per block, ns (see [`Self::publish_dsp`]).
    dsp_block_ns: u64,
    dsp_ns: f32,
    /// The decaying peak of the time per block, ns.
    dsp_peak_ns: f32,
}

/// How fast the published DSP time follows the measured one, per block.
/// About 100 blocks (a second at 512 frames / 48 kHz) to settle.
const DSP_SMOOTHING: f32 = 0.02;
/// Time constant of the published DSP peak's decay, seconds.
const DSP_PEAK_SECONDS: f32 = 2.0;

/// How long the chain's output takes to come back after a block it missed
/// (it played its delayed input meanwhile).
const CHAIN_RESUME_SECONDS: f32 = 0.005;

/// The plugin chain runs in chunks of at most this many frames, so the
/// fallback delay line only needs [`MAX_ALIGN`] plus one chunk.
const CHAIN_CHUNK: usize = super::plugins::MAX_BLOCK;

impl PathProcessor {
    fn new(rate: u32) -> Self {
        Self {
            latency: 0,
            latency_raw: 0,
            drain: None,
            chain_line: DelayLine::new(MAX_ALIGN as usize + CHAIN_CHUNK),
            chain_tap: DelayTap::default(),
            chain_mix: 1.0,
            chain_step: 1.0,
            chain_hold: 0,
            tap_fade: TAP_FADE_SECONDS * rate as f32,
            chain_declick: Declick::default(),
            declick_len: (DECLICK_SECONDS * rate as f32) as u32,
            chain_fresh: true,
            dsp_block_ns: 0,
            dsp_ns: 0.0,
            dsp_peak_ns: 0.0,
            block: Vec::new(),
            plugin_l: Vec::new(),
            plugin_r: Vec::new(),
            eq: [Equalizer::new(rate), Equalizer::new(rate)],
            eq_version: [0; 2],
            gate: NoiseGate::new(rate),
            gate_version: 0,
            ramps: PathRamps::new(),
            controls_adopted: false,
            plan: BlockPlan::default(),
            generation: 0,
            gain: Ramp::new(1.0),
            fading: false,
        }
    }

    /// Starts the chain over for a newly activated slot: fresh EQ and gate
    /// state, settings re-read, and ramps that adopt the settings instead of
    /// moving from whatever the slot played before.  No allocation; the
    /// block scratch is kept.
    fn restart(&mut self, rate: u32, generation: u64) {
        self.eq = [Equalizer::new(rate), Equalizer::new(rate)];
        self.gate = NoiseGate::new(rate);
        self.invalidate_settings();
        self.ramps = PathRamps::new();
        self.controls_adopted = false;
        self.generation = generation;
        self.chain_line.reset();
        self.chain_mix = 1.0;
        self.chain_hold = 0;
        self.chain_fresh = true;
        self.chain_declick = Declick::default();
        self.drain = None;
        self.dsp_block_ns = 0;
        self.dsp_ns = 0.0;
        self.dsp_peak_ns = 0.0;
    }

    /// Grows the per-block scratch outside the callback.
    fn prepare(&mut self, frames: usize) {
        self.reserve_block(frames);
        if self.plugin_l.len() < frames {
            self.plugin_l.resize(frames, 0.0);
            self.plugin_r.resize(frames, 0.0);
        }
    }

    /// Makes room for one block.  Only a host that hands over a larger block
    /// than prepared allocates here, once.
    fn reserve_block(&mut self, frames: usize) {
        if self.block.len() < frames {
            self.block.resize(frames, [0.0; 4]);
        }
    }

    /// Forgets which settings the EQs and gate were built from, so the next
    /// block redesigns them (for a new rate).  Version 0 is never published.
    fn invalidate_settings(&mut self) {
        self.eq_version = [0; 2];
        self.gate_version = 0;
    }

    /// Picks up every UI change made since the last block and decides which
    /// built-in stages run in it.  The first block snaps to the settings;
    /// after that a change takes `ramp_samples`.  `any_solo`: some active
    /// path is soloed, so this one sounds only if it is too.  `fade_in`:
    /// the mixer is already playing, so a path that just went live starts
    /// at silence and ramps up instead of snapping to its level.  Returns
    /// whether the chain started fresh (a newly activated slot).
    fn begin_block(
        &mut self,
        controls: &PathControls,
        rate: u32,
        ramp_samples: f32,
        any_solo: bool,
        fade_in: bool,
    ) -> bool {
        let generation = controls.generation();
        let restarted = generation != self.generation;
        if restarted {
            self.restart(rate, generation);
        }
        self.latency_raw = controls.latency_samples();
        self.latency = self.latency_raw.min(MAX_ALIGN);
        self.tap_fade = TAP_FADE_SECONDS * rate as f32;
        self.declick_len = (DECLICK_SECONDS * rate as f32) as u32;
        self.chain_step = 1.0 / (CHAIN_RESUME_SECONDS * rate as f32).max(1.0);
        self.follow_eq(controls, rate);
        self.follow_gate(controls, rate);
        let adopted = self.controls_adopted;
        self.follow_controls(controls, ramp_samples);
        self.fading = controls.state() == PathState::FadingOut;
        // A chain whose plugins are still being opened (a path just added,
        // duplicated or restored) stays silent rather than playing dry.
        let silent = self.fading
            || controls.muted()
            || (any_solo && !controls.soloed())
            || controls.chain_pending();
        let gain = if silent { 0.0 } else { controls.level_gain() };
        if adopted {
            self.gain.set_target(gain, ramp_samples);
        } else if fade_in {
            self.gain.snap(0.0);
            self.gain.set_target(gain, ramp_samples);
        } else {
            self.gain.snap(gain);
        }
        self.plan = BlockPlan {
            pre_eq: self.eq[EqSlot::Pre.index()].active(),
            post_eq: self.eq[EqSlot::Post.index()].active(),
            gate_pre: self.gate.active() && self.gate.settings().slot == GateSlot::Pre,
            gate_post: self.gate.active() && self.gate.settings().slot == GateSlot::Post,
        };
        restarted
    }

    /// Points the routing, enable and mono ramps at this block's settings.
    fn follow_controls(&mut self, controls: &PathControls, ramp_samples: f32) {
        let coefficient = |flag: bool| if flag { 1.0 } else { 0.0 };
        let mut routes = [[0.0f32; 4]; 2];
        for (input, row) in routes.iter_mut().enumerate() {
            for (tap, coefficient_out) in row.iter_mut().enumerate() {
                *coefficient_out = coefficient(controls.route(input, tap));
            }
        }
        let enabled = [
            coefficient(controls.output_enabled[0].load(Ordering::Relaxed)),
            coefficient(controls.output_enabled[1].load(Ordering::Relaxed)),
        ];
        let mono = coefficient(controls.mono.load(Ordering::Relaxed));
        if self.controls_adopted {
            self.ramps.set_targets(&routes, enabled, mono, ramp_samples);
        } else {
            self.ramps.snap(&routes, enabled, mono);
            self.controls_adopted = true;
        }
    }

    /// Picks up EQ changes made in the UI since the last block.
    fn follow_eq(&mut self, controls: &PathControls, rate: u32) {
        for slot in EqSlot::ALL {
            let index = slot.index();
            let version = controls.eq_version(slot);
            if version == self.eq_version[index] {
                continue;
            }
            if let Some(settings) = controls.try_eq(slot) {
                self.eq[index].update(settings, rate);
                self.eq_version[index] = version;
            }
        }
    }

    /// Picks up gate changes made in the UI since the last block.
    fn follow_gate(&mut self, controls: &PathControls, rate: u32) {
        let version = controls.gate_version();
        if version == self.gate_version {
            return;
        }
        if let Some(settings) = controls.try_gate() {
            self.gate.update(settings, rate);
            self.gate_version = version;
        }
    }

    /// Holds one input frame in `block` for [`Self::run_front`].
    #[inline]
    fn stage_input(&mut self, index: usize, left: f32, right: f32) {
        self.block[index] = [left, right, 0.0, 0.0];
    }

    /// The front of the chain over a block of staged input frames, in
    /// place: routing, channel enable and mono (with their ramps), then the
    /// pre EQ and a PRE gate.  The result waits in `block` for the plugin
    /// inserts.  Each path's state only moves with its own frames, so
    /// running it per block instead of per frame gives the same samples,
    /// and the mixer can time each path as one piece.
    fn run_front(&mut self, frames: usize) {
        for index in 0..frames {
            let [left, right, _, _] = self.block[index];
            // Routing, channel enable and mono move with the ramps, before
            // the EQ and the inserts, which is where those controls act.
            let mut out = self.ramps.next_input(left, right);
            // The chain on the path's mix (taps 1/2), in order: pre EQ,
            // optional PRE gate, plugin inserts, optional POST gate, post
            // EQ.  Built-ins are per-sample → no added latency.
            if self.plan.pre_eq {
                (out[0], out[1]) = self.eq[EqSlot::Pre.index()].process(out[0], out[1]);
            }
            if self.plan.gate_pre {
                (out[0], out[1]) = self.gate.process(out[0], out[1]);
            }
            self.block[index] = out;
        }
    }

    /// Adds the time from `since` to `now` to this block's DSP time and
    /// returns `now`, the start of whatever is timed next.
    #[inline]
    fn add_dsp_time(&mut self, since: Instant, now: Instant) -> Instant {
        self.dsp_block_ns += now.saturating_duration_since(since).as_nanos() as u64;
        now
    }

    /// Smooths this block's DSP time into the published one, folds it into
    /// the decaying peak ([`DSP_PEAK_SECONDS`]) and stores both with the
    /// block's length (`frames` at `rate`), the path's budget.  Atomics
    /// only.
    fn publish_dsp(&mut self, controls: &PathControls, frames: usize, rate: u32) {
        let measured = self.dsp_block_ns.min(u32::MAX as u64) as f32;
        self.dsp_block_ns = 0;
        self.dsp_ns = if self.dsp_ns == 0.0 {
            measured
        } else {
            self.dsp_ns + (measured - self.dsp_ns) * DSP_SMOOTHING
        };
        let seconds = frames as f32 / rate.max(1) as f32;
        let decay = (-seconds / DSP_PEAK_SECONDS).exp();
        self.dsp_peak_ns = measured.max(self.dsp_peak_ns * decay);
        let block_ns = (frames as u64 * 1_000_000_000 / rate.max(1) as u64).min(u32::MAX as u64);
        controls.store_dsp_time(DspTime {
            avg_ns: self.dsp_ns as u32,
            peak_ns: self.dsp_peak_ns as u32,
            block_ns: block_ns as u32,
        });
    }

    /// Runs the plugin inserts over a block that `run_front` filled, in
    /// chunks of at most [`CHAIN_CHUNK`] frames.
    ///
    /// The chain never just drops out.  Mix, bypass and plugin controls do
    /// not take the chain lock at all (see the `path` module docs), so a
    /// miss is rare: the UI holds the lock for a structural edit.  The
    /// chain's input also goes into `chain_line`; a chunk the chain cannot
    /// run plays that input delayed by the chain latency instead.  That
    /// keeps the path's timing in the sum (no comb, no phase jump), but not
    /// its sound: for the missed chunk the path plays dry (its inserts'
    /// level and tone are gone).  The switch into the fallback cannot
    /// crossfade (the chain's output for that chunk does not exist), so its
    /// step is smoothed over [`DECLICK_SECONDS`] instead.  Once the chain
    /// runs again its output fades back in over [`CHAIN_RESUME_SECONDS`],
    /// after one latency's worth of frames (`chain_hold`): the inserts
    /// missed that input, so their own delay lines still hold the gap.  A
    /// chain with no active insert but some latency (all bypassed) still
    /// runs: its skipped inserts delay by their latency.
    fn run_inserts(&mut self, controls: &PathControls, frames: usize) {
        if !controls.has_plugins() && self.latency == 0 {
            // Nothing runs and nothing delays.  The history is stale from
            // here on; a chain that comes back starts from silence.
            self.chain_line.reset();
            self.chain_mix = 1.0;
            self.chain_hold = 0;
            self.chain_fresh = true;
            return;
        }
        if self.chain_mix >= 1.0 {
            // Nothing reads the fallback: it follows the latency at once.
            self.chain_tap.snap(self.latency);
        } else {
            self.chain_tap.set_target(self.latency, self.tap_fade);
        }
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
        let mut offset = 0;
        while offset < frames {
            let count = (frames - offset).min(CHAIN_CHUNK);
            let chunk_l = &mut left[offset..offset + count];
            let chunk_r = &mut right[offset..offset + count];
            for (l, r) in chunk_l.iter().zip(chunk_r.iter()) {
                self.chain_line.push([*l, *r]);
            }
            let ran = controls.process_plugins(chunk_l, chunk_r, count);
            // Into the fallback from (some of) the chain's own output.
            let entering = !ran && self.chain_mix > 0.0 && !self.chain_fresh;
            if !ran {
                self.chain_mix = 0.0;
                self.chain_hold = self.latency;
            }
            if self.chain_mix < 1.0 {
                for index in 0..count {
                    let delayed = self.chain_tap.next(&self.chain_line, count - 1 - index);
                    let mix = self.chain_mix;
                    if mix <= 0.0 {
                        chunk_l[index] = delayed[0];
                        chunk_r[index] = delayed[1];
                    } else {
                        chunk_l[index] = delayed[0] + (chunk_l[index] - delayed[0]) * mix;
                        chunk_r[index] = delayed[1] + (chunk_r[index] - delayed[1]) * mix;
                    }
                    if !ran {
                        continue;
                    }
                    if self.chain_hold > 0 {
                        self.chain_hold -= 1;
                    } else {
                        self.chain_mix = (self.chain_mix + self.chain_step).min(1.0);
                    }
                }
            }
            if entering {
                self.chain_declick
                    .start([chunk_l[0], chunk_r[0]], self.declick_len);
            }
            if self.chain_declick.is_active() {
                for index in 0..count {
                    [chunk_l[index], chunk_r[index]] =
                        self.chain_declick.apply([chunk_l[index], chunk_r[index]]);
                }
            }
            self.chain_declick.note(chunk_l, chunk_r);
            self.chain_fresh = false;
            offset += count;
        }
        for index in 0..frames {
            self.block[index][0] = left[index];
            self.block[index][1] = right[index];
        }
        self.plugin_l = left;
        self.plugin_r = right;
    }

    /// The rest of the path after the inserts: the POST gate and the post
    /// EQ, in place, then the path meters.
    fn finish_block(&mut self, controls: &PathControls, frames: usize) {
        let mut peak_l = 0.0f32;
        let mut peak_r = 0.0f32;
        for out in &mut self.block[..frames] {
            if self.plan.gate_post {
                (out[0], out[1]) = self.gate.process(out[0], out[1]);
            }
            if self.plan.post_eq {
                (out[0], out[1]) = self.eq[EqSlot::Post.index()].process(out[0], out[1]);
            }
            peak_l = peak_l.max(out[0].abs());
            peak_r = peak_r.max(out[1].abs());
        }
        controls.store_meters(peak_l, peak_r);
    }
}

/// Turns the ring into output frames for any playback path (cpal, ASIO,
/// REAL-style WASAPI or PipeWire): underrun recovery, drift trimming and
/// conversion from whatever rate the input really runs at (the clock side,
/// once), then each active signal path ([`PathProcessor`]), the sum, the
/// master level and mute, and last each output device's channel map, level
/// and mute.
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
    /// One chain per path slot; only the `live` ones run.
    paths: [PathProcessor; MAX_PATHS],
    /// The slots that run in the current block, read once at its start.
    live: LiveSlots,
    /// One delay line per path slot: each live path's frame after the
    /// master is written here, and every device mapper reads it through its
    /// own alignment tap.
    lines: PathLines,
    /// The master: mute, monitor and the output level folded into one
    /// effective gain, ramped per sample so they compose without a step.
    gain: Ramp,
    /// The first block adopts the live master gain directly.
    gain_adopted: bool,
    /// The clock device's channel map, level and mute, after the master.
    device: DeviceMapper,
    /// Follower devices: stereo port pairs on the PipeWire duplex graph,
    /// or a generic follower's device channels, mapped here and pushed into
    /// its ring.  Only the first `live_followers` slots are written each
    /// block.
    followers: [DeviceMapper; MAX_FOLLOWERS],
    /// `shared.follower_count()` as read at the start of the block.
    live_followers: usize,
    /// The generic followers whose rings this block feeds (ring path only;
    /// see [`Shared::feeds_follower`]), with the ring's layout generation
    /// as read at the start of the block.
    feeding: [Option<u64>; MAX_FOLLOWERS],
}

impl OutputMixer {
    /// A mixer for an output device with `device_channels` channels.
    pub(crate) fn new(output_rate: u32, device_channels: usize) -> Self {
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
            paths: std::array::from_fn(|_| PathProcessor::new(output_rate)),
            live: LiveSlots::default(),
            lines: std::array::from_fn(|_| DelayLine::new(MAX_ALIGN_DELAY as usize)),
            gain: Ramp::new(1.0),
            gain_adopted: false,
            device: DeviceMapper::new(device_channels),
            // A PipeWire follower is a stereo port pair; a generic follower
            // takes its device's count when its ring starts.
            followers: std::array::from_fn(|_| DeviceMapper::new(2)),
            live_followers: 0,
            feeding: [None; MAX_FOLLOWERS],
        }
    }

    /// Publishes the output device's channel count.  Call when the stream
    /// opens, outside the callback, so the snapshot has it at once.
    pub(crate) fn publish_device_channels(&self, shared: &Shared) {
        self.device.publish_channels(&shared.clock_device);
    }

    /// Publishes each live follower's channel count as this mixer maps it:
    /// a stereo port pair on the PipeWire duplex graph.  (A generic
    /// follower's stream publishes its device's count when it opens.)
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn publish_follower_channels(&self, shared: &Shared) {
        let count = shared.follower_count();
        for index in 0..count {
            self.followers[index].publish_channels(&shared.followers[index]);
        }
    }

    /// Grows the per-block scratch of every path while the bridge is still
    /// opening, so the first callback (and the first block after a plugin or
    /// a path is added) never has to allocate.  A host that later hands
    /// over a larger block still works; that one block then grows the
    /// scratch.
    pub(crate) fn prepare(&mut self, frames: usize) {
        for path in &mut self.paths {
            path.prepare(frames);
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

    /// Picks up every UI change made since the last block: which slots
    /// are live, each live path's chain settings, the master gain and the
    /// device controls.  Makes room for `frames` in every live path.
    /// Shared by the ring path and the direct path.
    fn begin_block(&mut self, shared: &Shared, frames: usize) {
        let samples = CONTROL_RAMP_SECONDS * self.output_rate as f32;
        self.live = shared.live_slots();
        let any_solo = shared.any_solo();
        // Only the mixer's first block adopts the path gains outright.
        let fade_in = self.gain_adopted;
        let mut timing = PathTiming {
            latency: [0; MAX_PATHS],
            rate: self.output_rate,
        };
        for slot in self.live.iter() {
            let path = &mut self.paths[slot];
            let restarted = path.begin_block(
                &shared.paths[slot],
                self.output_rate,
                samples,
                any_solo,
                fade_in,
            );
            if restarted {
                // A reused slot never replays what it held before.
                self.lines[slot].reset();
            }
            // Unclamped, so the report can flag a chain past MAX_ALIGN; the
            // alignment itself clamps.
            timing.latency[slot] = path.latency_raw;
            path.reserve_block(frames);
        }
        let silent = shared.mute.load(Ordering::Relaxed) || !shared.monitor.load(Ordering::Relaxed);
        let gain = if silent { 0.0 } else { shared.output_gain() };
        if self.gain_adopted {
            self.gain.set_target(gain, samples);
        } else {
            self.gain.snap(gain);
            self.gain_adopted = true;
        }
        self.device
            .begin_paths(&shared.clock_device, samples, self.live, &timing);
        // Every follower is summed and mapped here, like the clock device:
        // in-graph on the PipeWire duplex path, otherwise for a generic
        // follower's ring.  One without a running stream maps nothing and
        // its meters read 0.
        self.live_followers = shared.follower_count();
        let direct = shared.direct.load(Ordering::Relaxed);
        for index in 0..self.live_followers {
            let feeding = shared.feeds_follower(index);
            self.feeding[index] = feeding.then(|| shared.follower_bus(index).generation());
            let channels = if feeding {
                shared.follower_bus(index).device_channels()
            } else {
                2
            };
            self.followers[index].set_channels(channels);
            self.followers[index].begin_paths(
                &shared.followers[index],
                samples,
                self.live,
                &timing,
            );
            if !feeding && !direct {
                // Mapped, but nothing plays it: no alignment to show.  (The
                // duplex graph plays every follower in-graph.)
                shared.followers[index].clear_alignment();
            }
        }
        for follower in &shared.followers[self.live_followers..] {
            follower.clear_alignment();
        }
    }

    /// Splits one input frame into every live path.  Their chains run in
    /// [`Self::finish_block`].
    #[inline]
    fn stage_front(&mut self, index: usize, left: f32, right: f32) {
        for slot in self.live.iter() {
            self.paths[slot].stage_input(index, left, right);
        }
    }

    /// The chains for a block that `stage_front` filled: each live path's
    /// front (routing, pre EQ, PRE gate), inserts, POST gate and post EQ,
    /// each path timed (its DSP time, see [`PathProcessor::publish_dsp`]:
    /// one clock read per path and stage, no allocation, no lock), then
    /// the master level on
    /// each path's frame, the meters and the scope.  Each finished frame
    /// goes to `write` with the clock device's mapper, the live followers'
    /// mappers, every live path's frame (the mappers sum them) and the
    /// stereo sum of the paths' taps, and the frame counter advances.
    fn finish_block(&mut self, shared: &Shared, frames: usize, mut write: impl WriteFrame) {
        let live = self.live;
        // Every front runs before any plugin can touch the float mode.
        let mut mark = Instant::now();
        for slot in live.iter() {
            self.paths[slot].run_front(frames);
            mark = self.paths[slot].add_dsp_time(mark, Instant::now());
        }
        for slot in live.iter() {
            self.paths[slot].run_inserts(&shared.paths[slot], frames);
            mark = self.paths[slot].add_dsp_time(mark, Instant::now());
        }

        // A plugin may have cleared flush-to-zero; the POST gate, post EQ
        // and the master still decay into silence and need it.  Dropped
        // before the caller's guard, so the thread's original mode still
        // comes back last.
        let _post_denormals = DenormalGuard::new();
        mark = Instant::now();
        for slot in live.iter() {
            self.paths[slot].finish_block(&shared.paths[slot], frames);
            mark = self.paths[slot].add_dsp_time(mark, Instant::now());
            self.paths[slot].publish_dsp(&shared.paths[slot], frames, self.output_rate);
        }

        let followers = &mut self.followers[..self.live_followers];
        let mut peak_l = 0.0f32;
        let mut peak_r = 0.0f32;
        for index in 0..frames {
            // Mute, monitor and the level are one ramp, so any of them can
            // move without a step.  It scales each path before the device
            // maps sum them, so one path is exactly `(x * gain) * weight`.
            let gain = self.gain.next();
            // The stereo sum for the meters and the scope.  It starts from
            // the first path's frame itself, so one path passes bit for bit.
            let mut sum: Option<[f32; 4]> = None;
            for slot in live.iter() {
                let path = &mut self.paths[slot];
                let mut out = path.block[index];
                // Path level, mute, solo and fade; unity copies.
                let path_gain = path.gain.next();
                if path_gain != 1.0 {
                    for value in out.iter_mut() {
                        *value *= path_gain;
                    }
                }
                for value in out.iter_mut() {
                    *value *= gain;
                }
                self.lines[slot].push([out[0], out[1]]);
                sum = Some(match sum {
                    None => out,
                    Some(mut sum) => {
                        for (total, value) in sum.iter_mut().zip(out) {
                            *total += value;
                        }
                        sum
                    }
                });
            }
            let out = sum.unwrap_or([0.0; 4]);
            peak_l = peak_l.max(out[0].abs());
            peak_r = peak_r.max(out[1].abs());
            shared.push_scope_out(out[0], out[1]);
            write(&mut self.device, followers, index, &self.lines, out);
        }

        shared.frames.fetch_add(frames as u64, Ordering::Relaxed);
        shared.out_l.store(peak_l.to_bits(), Ordering::Relaxed);
        shared.out_r.store(peak_r.to_bits(), Ordering::Relaxed);
        self.device.end_block(&shared.clock_device);
        for index in 0..self.live_followers {
            self.followers[index].end_block(&shared.followers[index]);
        }
        shared.note_follower_producer_block(frames);
        // A removed path that has faded to silence keeps running until its
        // delay lines have played out on every device (the taps still hold
        // up to one alignment delay of it at full level), then stops from
        // the next block.
        for slot in live.iter() {
            let path = &self.paths[slot];
            if !path.fading {
                self.paths[slot].drain = None;
                continue;
            }
            if !path.gain.is_settled_at(0.0) {
                continue;
            }
            let drain = match path.drain {
                Some(left) => left.saturating_sub(frames as u32),
                // Went silent in this block: from here, the longest tap on
                // the clock device or a live follower still has to play out.
                None => self.followers[..self.live_followers]
                    .iter()
                    .map(|mapper| mapper.tap_delay(slot))
                    .fold(self.device.tap_delay(slot), u32::max),
            };
            self.paths[slot].drain = Some(drain);
            if drain == 0 {
                shared.paths[slot].finish_fade();
            }
        }
    }

    /// Like [`Self::render_direct`], but each finished frame is handed over
    /// once per live device: index 0 is the clock device, then each follower.
    /// Used by the PipeWire duplex/native multi-sink path.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn render_direct_devices(
        &mut self,
        shared: &Shared,
        input_left: &[f32],
        input_right: &[f32],
        mut write: impl FnMut(usize, usize, &DeviceFrame),
    ) {
        self.render_direct_with(
            shared,
            input_left,
            input_right,
            |device, followers, index, paths, _| {
                write(0, index, device.map_paths(paths));
                for (follower, mapper) in followers.iter_mut().enumerate() {
                    write(follower + 1, index, mapper.map_paths(paths));
                }
            },
        );
    }

    /// Follows the graph rate on the direct path, where input and output run
    /// in the same callback at one rate.  Cheap and allocation-free, so it can
    /// run on the audio thread: the EQ and gate redesign their coefficients on
    /// the next block and the control ramps take the new rate.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn set_direct_rate(&mut self, rate: u32) {
        let rate = rate.max(1);
        if rate == self.output_rate {
            return;
        }
        self.output_rate = rate;
        self.source_rate = rate;
        // The next block re-reads every path's EQs and gate and `update`
        // redesigns them for the new rate.
        for path in &mut self.paths {
            path.invalidate_settings();
        }
    }

    /// Renders one block straight from an input block that arrived in the
    /// same graph cycle: no ring, no cushion, no priming, no drift steering
    /// and no rate conversion, because both sides share one clock and rate.
    /// Everything after the ring read is the same chain as [`Self::render`].
    /// Each frame reaches `write` mapped onto the device's channels.
    #[allow(dead_code)] // Single-device direct path; duplex fans out via devices.
    pub(crate) fn render_direct(
        &mut self,
        shared: &Shared,
        input_left: &[f32],
        input_right: &[f32],
        mut write: impl FnMut(usize, &DeviceFrame),
    ) {
        self.render_direct_with(
            shared,
            input_left,
            input_right,
            |device, _, index, paths, _| write(index, device.map_paths(paths)),
        );
    }

    /// [`Self::render_direct`] handing over the four mixer taps instead of
    /// the device frame.
    #[cfg(test)]
    fn render_direct_taps(
        &mut self,
        shared: &Shared,
        input_left: &[f32],
        input_right: &[f32],
        mut write: impl FnMut(usize, [f32; 4]),
    ) {
        self.render_direct_with(shared, input_left, input_right, |_, _, index, _, out| {
            write(index, out)
        });
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn render_direct_with(
        &mut self,
        shared: &Shared,
        input_left: &[f32],
        input_right: &[f32],
        write: impl WriteFrame,
    ) {
        // The same flush-to-zero mode as `render`, for the EQ, gate and
        // inserts of this block.
        let _denormals = DenormalGuard::new();
        let frames = input_left.len().min(input_right.len());
        self.begin_block(shared, frames);
        for index in 0..frames {
            self.stage_front(index, input_left[index], input_right[index]);
        }
        self.finish_block(shared, frames, write);
        // There is no clock offset to report on a single clock.
        self.drift_ppm = 0.0;
        shared.drift_ppm.store(0.0f32.to_bits(), Ordering::Relaxed);
    }

    /// Renders `frames` output frames, handing each one to `write` mapped
    /// onto the device's channels.  The backend only copies the frame into
    /// its own buffer layout and sample format.
    pub(crate) fn render(
        &mut self,
        shared: &Shared,
        frames: usize,
        mut write: impl FnMut(usize, &DeviceFrame),
    ) {
        self.render_with(shared, frames, |device, _, index, paths, _| {
            write(index, device.map_paths(paths))
        });
    }

    /// [`Self::render`] handing over the four mixer taps (only 0/1 carry the
    /// processed mix) instead of the device frame.
    #[cfg(test)]
    fn render_taps(
        &mut self,
        shared: &Shared,
        frames: usize,
        mut write: impl FnMut(usize, [f32; 4]),
    ) {
        self.render_with(shared, frames, |_, _, index, _, out| write(index, out));
    }

    fn render_with(&mut self, shared: &Shared, frames: usize, mut write: impl WriteFrame) {
        // Decaying EQ and gate states must not drop into the slow subnormal
        // range; the inserts run inside this block and get the same mode.
        let _denormals = DenormalGuard::new();
        self.follow_source_rate(shared);
        self.begin_block(shared, frames);
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

        let mut missed = false;
        let fade_step = 1.0 / (DROPOUT_FADE_SECONDS * self.output_rate as f32).max(1.0);
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
            self.stage_front(index, left, right);
        }
        // Each generic follower gets its own sum of the paths, mapped onto
        // its device channels, through its ring (see `follower`).
        let feeding = self.feeding;
        self.finish_block(shared, frames, |device, followers, index, paths, out| {
            write(device, followers, index, paths, out);
            for (slot, mapper) in followers.iter_mut().enumerate() {
                if let Some(generation) = feeding[slot] {
                    shared.push_follower_frame(slot, generation, mapper.map_paths(paths));
                }
            }
        });

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
        // Report the ratio trim the resampler actually ran with, not a
        // synthetic frame count.
        shared
            .drift_ppm
            .store(self.drift_ppm.to_bits(), Ordering::Relaxed);
    }
}

/// Receives one finished frame: the clock device's mapper, the live
/// followers' mappers, the paths' delay lines (each holding the path's
/// frame after the master as its newest entry) for the mappers to align
/// and sum, and the unaligned stereo sum of the paths' four taps.
trait WriteFrame: FnMut(&mut DeviceMapper, &mut [DeviceMapper], usize, &PathLines, [f32; 4]) {}

impl<F> WriteFrame for F where
    F: FnMut(&mut DeviceMapper, &mut [DeviceMapper], usize, &PathLines, [f32; 4])
{
}

/// Which built-in stages run in one block, decided once at its start.
#[derive(Clone, Copy, Default)]
struct BlockPlan {
    pre_eq: bool,
    post_eq: bool,
    gate_pre: bool,
    gate_post: bool,
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
    use super::super::output::write_frame;
    use super::super::output_map::{
        old_write_frame, ChannelMap, OutputDeviceSettings, PathMapSettings, UnusedChannels,
    };
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
        (OutputMixer::new(48_000, 2), shared)
    }

    fn render(mixer: &mut OutputMixer, shared: &Shared, frames: usize) -> Vec<[f32; 4]> {
        let mut out = vec![[0.0f32; 4]; frames];
        mixer.render_taps(shared, frames, |index, frame| out[index] = frame);
        out
    }

    fn store_gain(shared: &Shared, gain: f32) {
        shared.output_gain.store(gain.to_bits(), Ordering::Relaxed);
    }

    /// A varied stereo signal, so the comparison sees the whole chain move.
    fn test_signal(frames: usize) -> (Vec<f32>, Vec<f32>) {
        let left = (0..frames)
            .map(|n| (n as f32 * 0.031).sin() * 0.6)
            .collect();
        let right = (0..frames)
            .map(|n| (n as f32 * 0.017).cos() * 0.4)
            .collect();
        (left, right)
    }

    /// Settings that exercise routing, mono, both EQs and a level ramp.
    fn busy_settings(shared: &Shared) {
        shared.set_source_rate(48_000);
        shared.set_cushion(CushionMode::Manual, 2.0);
        shared.set_mix(false, true, false, [true, true], [true, true], false);
        shared.set_routes([[true, false, true, false], [false, true, false, true]]);
        let mut pre = EqSettings::default();
        pre.bands[1].gain_db = 6.0;
        let mut post = EqSettings::default();
        post.bands[3].gain_db = -4.0;
        shared.set_eq(EqSlot::Pre, pre);
        shared.set_eq(EqSlot::Post, post);
        store_gain(shared, 0.8);
    }

    #[test]
    fn the_direct_path_matches_the_ring_path_at_one_clock() {
        let frames = 256;
        let blocks = 6;
        let (left, right) = test_signal(frames * blocks);

        // Ring path: each block's input is pushed just before it renders, as
        // a capture callback in the same cycle would.  The 2 ms cushion is
        // below one block, so priming ends at once, nothing is purged and
        // every frame comes straight through.
        let ring_shared = Shared::new(48_000);
        busy_settings(&ring_shared);
        let mut ring_mixer = OutputMixer::new(48_000, 2);
        ring_mixer.prepare(frames);

        let direct_shared = Shared::new(48_000);
        busy_settings(&direct_shared);
        let mut direct_mixer = OutputMixer::new(48_000, 2);
        direct_mixer.prepare(frames);

        for block in 0..blocks {
            if block == 3 {
                // A level change mid-run must ramp identically on both.
                store_gain(&ring_shared, 0.3);
                store_gain(&direct_shared, 0.3);
            }
            let span = block * frames..(block + 1) * frames;
            for (l, r) in left[span.clone()].iter().zip(&right[span.clone()]) {
                ring_shared.push_frame(*l, *r);
            }
            let ring = render(&mut ring_mixer, &ring_shared, frames);
            let mut direct = vec![[0.0f32; 4]; frames];
            direct_mixer.render_direct_taps(
                &direct_shared,
                &left[span.clone()],
                &right[span],
                |index, frame| direct[index] = frame,
            );
            assert_eq!(ring, direct, "block {block} differs");
        }
        assert_eq!(
            ring_shared.frames.load(Ordering::Relaxed),
            direct_shared.frames.load(Ordering::Relaxed)
        );
        assert_eq!(ring_shared.underruns.load(Ordering::Relaxed), 0);
        assert_eq!(direct_shared.underruns.load(Ordering::Relaxed), 0);
        assert_eq!(
            ring_shared.out_l.load(Ordering::Relaxed),
            direct_shared.out_l.load(Ordering::Relaxed)
        );
    }

    #[test]
    fn default_device_settings_write_what_the_output_always_wrote() {
        let frames = 256;
        let blocks = 6;
        let (left, right) = test_signal(frames * blocks);
        for channels in [1, 2, 4, 8] {
            // The same chain twice: one hands over the taps, one the mapped
            // device frame.  A level ramp mid-run must not change that.
            let taps_shared = Shared::new(48_000);
            busy_settings(&taps_shared);
            let mut taps_mixer = OutputMixer::new(48_000, channels);
            taps_mixer.prepare(frames);
            let device_shared = Shared::new(48_000);
            busy_settings(&device_shared);
            let mut device_mixer = OutputMixer::new(48_000, channels);
            device_mixer.prepare(frames);

            for block in 0..blocks {
                if block == 3 {
                    store_gain(&taps_shared, 0.3);
                    store_gain(&device_shared, 0.3);
                }
                let span = block * frames..(block + 1) * frames;
                for (l, r) in left[span.clone()].iter().zip(&right[span]) {
                    taps_shared.push_frame(*l, *r);
                    device_shared.push_frame(*l, *r);
                }
                let taps = render(&mut taps_mixer, &taps_shared, frames);
                let mut device = vec![0.0f32; frames * channels];
                device_mixer.render(&device_shared, frames, |index, mapped| {
                    for channel in 0..channels {
                        device[index * channels + channel] = mapped.sample(channel);
                    }
                });
                let mut expected = vec![0.0f32; frames * channels];
                for (frame, tap) in expected.chunks_mut(channels).zip(&taps) {
                    old_write_frame(frame, tap[0], tap[1]);
                }
                let bits = |values: &[f32]| values.iter().map(|v| v.to_bits()).collect::<Vec<_>>();
                assert_eq!(
                    bits(&device),
                    bits(&expected),
                    "{channels} ch, block {block}"
                );
            }
            // Per-channel meters: channel 1 and 2 carry the master L/R peaks.
            let meters = device_shared.clock_device.meters();
            if channels >= 2 {
                assert_eq!(
                    meters[0],
                    f32::from_bits(device_shared.out_l.load(Ordering::Relaxed))
                );
                assert_eq!(
                    meters[1],
                    f32::from_bits(device_shared.out_r.load(Ordering::Relaxed))
                );
            }
            assert!(meters[..channels].iter().all(|meter| *meter > 0.0));
            assert!(meters[channels..].iter().all(|meter| *meter == 0.0));
        }
    }

    #[test]
    fn a_ten_channel_device_fills_every_channel_through_write_frame() {
        let (_, shared) = steady_mixer((0.5, -0.25));
        let mut mixer = OutputMixer::new(48_000, 10);
        mixer.publish_device_channels(&shared);
        assert_eq!(shared.clock_device.channels(), 10);
        let frames = 64;
        let mut data = vec![f32::NAN; frames * 10];
        mixer.render(&shared, frames, |index, mapped| {
            write_frame(&mut data[index * 10..(index + 1) * 10], mapped);
        });
        for frame in data.chunks(10) {
            let mut expected = [0.0f32; 10];
            old_write_frame(&mut expected, 0.5, -0.25);
            assert_eq!(frame, expected);
        }
        // Channels 9 and 10 are past the map and get the unused value.
        shared.clock_device.set(&OutputDeviceSettings {
            unused: UnusedChannels::Silent,
            ..OutputDeviceSettings::default()
        });
        // 8 blocks of 64 outlast the 6 ms ramp.
        for _ in 0..8 {
            mixer.render(&shared, frames, |index, mapped| {
                write_frame(&mut data[index * 10..(index + 1) * 10], mapped);
            });
        }
        let last = &data[(frames - 1) * 10..];
        assert_eq!(last[..2], [0.5, -0.25]);
        assert!(last[2..].iter().all(|value| *value == 0.0));
    }

    #[test]
    fn the_device_map_follows_the_master_and_crossfades() {
        let (mut mixer, shared) = steady_mixer((0.5, -0.25));
        let render_device = |mixer: &mut OutputMixer, shared: &Shared, frames: usize| {
            let mut out = vec![[0.0f32; 2]; frames];
            mixer.render(shared, frames, |index, mapped| {
                out[index] = [mapped.sample(0), mapped.sample(1)];
            });
            out
        };
        store_gain(&shared, 0.5);
        let adopted = render_device(&mut mixer, &shared, 16);
        assert_eq!(adopted[0], [0.25, -0.125]);

        // Swap the sides at -6 dB: a crossfade, then the swapped, scaled pair.
        let gain = 10.0f32.powf(-6.0 / 20.0);
        shared.clock_device.set(&OutputDeviceSettings {
            level_db: -6.0,
            path_map: crate::audio::PathMapSettings::first(ChannelMap {
                left: 0b10,
                right: 0b01,
            }),
            ..OutputDeviceSettings::default()
        });
        let out = render_device(&mut mixer, &shared, 600);
        let step = out
            .windows(2)
            .map(|pair| (pair[1][0] - pair[0][0]).abs())
            .fold(0.0, f32::max);
        assert!(step < 0.375 / 100.0, "stepped by {step}");
        assert_eq!(*out.last().unwrap(), [-0.125 * gain, 0.25 * gain]);
        // The master meters stay before the device stage.
        assert_eq!(f32::from_bits(shared.out_l.load(Ordering::Relaxed)), 0.25);
    }

    #[test]
    fn the_direct_path_follows_a_new_graph_rate() {
        let shared = Shared::new(48_000);
        busy_settings(&shared);
        let mut mixer = OutputMixer::new(48_000, 2);
        mixer.prepare(128);
        let (left, right) = test_signal(128);
        mixer.render_direct_taps(&shared, &left, &right, |_, _| {});
        mixer.set_direct_rate(44_100);
        let mut at_new_rate = vec![[0.0f32; 4]; 128];
        mixer.render_direct_taps(&shared, &left, &right, |index, frame| {
            at_new_rate[index] = frame
        });

        // A mixer built at 44.1 kHz from the start, after the same history.
        let reference_shared = Shared::new(44_100);
        busy_settings(&reference_shared);
        let mut reference = OutputMixer::new(44_100, 2);
        reference.prepare(128);
        reference.render_direct_taps(&reference_shared, &left, &right, |_, _| {});
        let mut expected = vec![[0.0f32; 4]; 128];
        reference.render_direct_taps(&reference_shared, &left, &right, |index, frame| {
            expected[index] = frame
        });
        // The filter state differs (it ran at another rate), so compare the
        // tail, after the EQ memory has mostly converged, loosely.
        for (got, want) in at_new_rate[96..].iter().zip(&expected[96..]) {
            assert!((got[0] - want[0]).abs() < 0.05, "{got:?} vs {want:?}");
        }
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
        let mut mixer = OutputMixer::new(48_000, 2);
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

    /// Folds every output sample (as bits) into one FNV-1a hash.
    struct Fnv(u64);

    impl Fnv {
        fn new() -> Self {
            Self(0xcbf2_9ce4_8422_2325)
        }

        fn u32(&mut self, value: u32) {
            for byte in value.to_le_bytes() {
                self.0 ^= u64::from(byte);
                self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
            }
        }

        fn f32(&mut self, value: f32) {
            self.u32(value.to_bits());
        }
    }

    /// A deterministic, libm-free test signal: a triangle plus LCG noise.
    struct Source {
        state: u32,
        phase: u32,
    }

    impl Source {
        fn new() -> Self {
            Self {
                state: 0x1234_5678,
                phase: 0,
            }
        }

        fn next(&mut self) -> (f32, f32) {
            self.state = self
                .state
                .wrapping_mul(1_664_525)
                .wrapping_add(1_013_904_223);
            let noise = (self.state >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
            self.phase = (self.phase + 1) % 200;
            let triangle = (self.phase as f32 - 100.0).abs() / 100.0 - 0.5;
            (triangle * 0.9 + noise * 0.2, noise * 0.6 - triangle * 0.3)
        }
    }

    /// The ring path through the resampler with drift on, every built-in
    /// stage, control changes mid-run, a device map crossfade and an
    /// underrun.  Feeds every device sample and meter into `hash`.
    fn golden_ring_render(hash: &mut Fnv) {
        use super::super::gate::{GateSettings, GateSlot};
        let shared = Shared::new(48_000);
        shared.set_source_rate(44_100);
        shared.set_cushion(CushionMode::Manual, 5.0);
        shared.set_mix(false, true, false, [true, true], [true, true], true);
        shared.set_routes([[true, false, true, false], [false, true, false, true]]);
        let mut pre = EqSettings::default();
        pre.bands[1].gain_db = 6.0;
        let mut post = EqSettings::default();
        post.bands[3].gain_db = -4.0;
        shared.set_eq(EqSlot::Pre, pre);
        shared.set_eq(EqSlot::Post, post);
        let mut gate = GateSettings {
            enabled: true,
            slot: GateSlot::Pre,
            threshold_db: -20.0,
            ..GateSettings::default()
        };
        shared.set_gate(gate);
        store_gain(&shared, 0.8);
        let channels = 4;
        let mut mixer = OutputMixer::new(48_000, channels);
        mixer.publish_device_channels(&shared);
        mixer.prepare(256);
        let mut source = Source::new();
        for _ in 0..400 {
            let (l, r) = source.next();
            shared.push_frame(l, r);
        }
        for block in 0..48 {
            match block {
                8 => shared.set_mix(false, true, true, [true, true], [true, true], true),
                12 => shared.set_mix(false, true, false, [true, true], [false, true], true),
                16 => {
                    gate.slot = GateSlot::Post;
                    shared.set_gate(gate);
                }
                20 => shared.clock_device.set(&OutputDeviceSettings {
                    level_db: -3.0,
                    path_map: crate::audio::PathMapSettings::first(ChannelMap {
                        left: 0b1010,
                        right: 0b0101,
                    }),
                    unused: UnusedChannels::Silent,
                    ..OutputDeviceSettings::default()
                }),
                24 => store_gain(&shared, 0.3),
                28 => shared.set_routes([[false, true, false, false], [true, false, false, true]]),
                36 => shared.set_mix(true, true, false, [true, true], [true, true], true),
                40 => shared.set_mix(false, true, false, [true, true], [true, true], false),
                _ => {}
            }
            // Blocks 30 and 31 starve the ring; block 33 catches up.
            let pushed = match block {
                30 | 31 => 0,
                33 => 1_200,
                _ => 236 + (block % 3),
            };
            for _ in 0..pushed {
                let (l, r) = source.next();
                shared.push_frame(l, r);
            }
            mixer.render(&shared, 256, |_, mapped| {
                for channel in 0..channels {
                    hash.f32(mapped.sample(channel));
                }
            });
            hash.u32(shared.out_l.load(Ordering::Relaxed));
            hash.u32(shared.out_r.load(Ordering::Relaxed));
            hash.u32(shared.drift_ppm.load(Ordering::Relaxed));
            for meter in shared.clock_device.meters() {
                hash.f32(meter);
            }
        }
        let underruns = shared.underruns.load(Ordering::Relaxed);
        assert!(underruns >= 1, "the scenario must run the ring dry");
        hash.u32(underruns as u32);
        hash.u32(shared.frames.load(Ordering::Relaxed) as u32);
    }

    /// The PipeWire direct path with two followers, each with its own map
    /// and level, and control changes mid-run.
    fn golden_direct_render(hash: &mut Fnv) {
        use super::super::shared::FollowerPlan;
        let shared = Shared::new(48_000);
        busy_settings(&shared);
        let followers = [
            OutputDeviceSettings {
                level_db: -6.0,
                path_map: crate::audio::PathMapSettings::first(ChannelMap {
                    left: 0b10,
                    right: 0b01,
                }),
                ..OutputDeviceSettings::default()
            },
            OutputDeviceSettings {
                path_map: crate::audio::PathMapSettings::first(ChannelMap {
                    left: 0b11,
                    right: 0,
                }),
                unused: UnusedChannels::Silent,
                ..OutputDeviceSettings::default()
            },
        ];
        let plan = FollowerPlan {
            count: 2,
            ..FollowerPlan::default()
        };
        shared.set_followers(&followers, &plan, &[]);
        // The duplex graph maps the followers in this mixer.
        shared.direct.store(true, Ordering::Relaxed);
        let mut mixer = OutputMixer::new(48_000, 2);
        mixer.publish_device_channels(&shared);
        mixer.publish_follower_channels(&shared);
        mixer.prepare(128);
        let mut source = Source::new();
        for block in 0..32 {
            match block {
                6 => store_gain(&shared, 0.5),
                10 => shared.set_followers(
                    &[
                        OutputDeviceSettings {
                            level_db: -12.0,
                            ..followers[0]
                        },
                        followers[1],
                    ],
                    &plan,
                    &[],
                ),
                14 => shared.set_mix(false, true, true, [true, true], [true, true], false),
                18 => {
                    let mut pre = EqSettings::default();
                    pre.bands[0].gain_db = -6.0;
                    shared.set_eq(EqSlot::Pre, pre);
                }
                22 => shared.set_routes([[true, true, false, false], [false, true, true, false]]),
                _ => {}
            }
            let (left, right): (Vec<f32>, Vec<f32>) = (0..128).map(|_| source.next()).unzip();
            mixer.render_direct_devices(&shared, &left, &right, |device, index, mapped| {
                hash.u32(device as u32);
                hash.u32(index as u32);
                hash.f32(mapped.sample(0));
                hash.f32(mapped.sample(1));
            });
            hash.u32(shared.out_l.load(Ordering::Relaxed));
            hash.u32(shared.out_r.load(Ordering::Relaxed));
            for follower in &shared.followers[..2] {
                for meter in follower.meters() {
                    hash.f32(meter);
                }
            }
        }
    }

    /// Output recorded before the chain moved into per-path processors: one
    /// path must stay bit-identical.  The EQ, gate and resampler designs
    /// use the platform's libm, so the recorded hashes are only checked on
    /// the platform they were recorded on.
    #[test]
    fn one_path_is_bit_identical_to_the_recorded_output() {
        let mut ring = Fnv::new();
        golden_ring_render(&mut ring);
        let mut direct = Fnv::new();
        golden_direct_render(&mut direct);
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            assert_eq!(ring.0, 0x9837_e736_8542_4c03, "ring path output changed");
            assert_eq!(
                direct.0, 0x4fa9_c550_12df_70db,
                "direct path output changed"
            );
        }
    }

    #[test]
    fn a_plugin_insert_on_path_0_runs_inside_the_mixer() {
        let _main = super::super::shared::tests::claim_main_thread();
        let fake = crate::audio::clap::tests::fake_plugin_for_test;
        let (mut mixer, shared) = steady_mixer((0.5, -0.25));
        shared.paths[0].edit_chain(|chain| chain.insert_clap_for_test(fake(false)));
        // The same insert run on its own over the same dry block.
        let reference = Shared::new(48_000);
        reference.paths[0].edit_chain(|chain| chain.insert_clap_for_test(fake(false)));
        let mut left = vec![0.5f32; 64];
        let mut right = vec![-0.25f32; 64];
        reference.paths[0].process_plugins(&mut left, &mut right, 64);

        let out = render(&mut mixer, &shared, 64);
        assert_ne!(out[0][0], 0.5, "the insert ran");
        for (index, frame) in out.iter().enumerate() {
            assert_eq!([frame[0], frame[1]], [left[index], right[index]]);
        }
        let peak = |side: &[f32]| side.iter().fold(0.0f32, |peak, v| peak.max(v.abs()));
        assert_eq!(shared.paths[0].meters(), (peak(&left), peak(&right)));
    }

    #[test]
    fn live_slots_add_up_and_a_reactivated_slot_starts_fresh() {
        use super::super::path::PathState;
        let (mut mixer, shared) = steady_mixer((0.5, -0.25));
        let bypassed = EqSettings {
            enabled: false,
            ..EqSettings::default()
        };
        shared.paths[1].set_eq(EqSlot::Pre, bypassed);
        shared.paths[1].set_eq(EqSlot::Post, bypassed);
        let device = |mixer: &mut OutputMixer, shared: &Shared| {
            let mut out = vec![[0.0f32; 2]; 32];
            mixer.render(shared, 32, |index, mapped| {
                out[index] = [mapped.sample(0), mapped.sample(1)];
            });
            out
        };
        assert_eq!(render(&mut mixer, &shared, 16)[0], [0.5, -0.25, 0.0, 0.0]);

        // A second slot plays the same input: it fades in, then the sum
        // doubles, on the taps and on the device.
        shared.paths[1].set_state(PathState::Active);
        let out = render(&mut mixer, &shared, 400);
        assert!(out[0][0] > 0.5 && out[0][0] < 0.51, "{:?}", out[0]);
        assert!(largest_step(&out) < 0.5 / 100.0);
        assert_eq!(out[399], [1.0, -0.5, 0.0, 0.0]);
        assert!(device(&mut mixer, &shared)
            .iter()
            .all(|f| *f == [1.0, -0.5]));
        // Removing slot 0 keeps it running through its fade-out, after
        // which the mixer makes it inactive; slot 1 stays where it is.
        shared.paths[0].set_state(PathState::FadingOut);
        let out = render(&mut mixer, &shared, 400);
        assert!(out[0][0] > 0.99, "{:?}", out[0]);
        assert_eq!(out[399], [0.5, -0.25, 0.0, 0.0]);
        assert_eq!(shared.paths[0].state(), PathState::Inactive);
        assert_eq!(render(&mut mixer, &shared, 16)[0], [0.5, -0.25, 0.0, 0.0]);
        assert!(device(&mut mixer, &shared)
            .iter()
            .all(|f| *f == [0.5, -0.25]));

        // Slot 0 comes back with mono changed while it was off: a fresh
        // chain adopts the setting at once instead of ramping to it (every
        // tap gets the same mono share while the path fades in).
        shared.paths[0].set_mono(true);
        let generation = shared.paths[0].generation();
        shared.paths[0].set_state(PathState::Active);
        assert_eq!(shared.paths[0].generation(), generation + 1);
        let out = render(&mut mixer, &shared, 400);
        let share = out[0][2];
        assert!(share > 0.0 && share < 0.01, "fades in: {:?}", out[0]);
        assert_eq!(out[0][3], share);
        assert!((out[0][0] - 0.5 - share).abs() < 1.0e-6);
        assert_eq!(out[399], [0.625, -0.125, 0.125, 0.125]);
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

    #[test]
    fn the_ring_path_pushes_finished_frames_into_active_follower_rings() {
        use super::super::shared::{FollowerLink, FollowerPlan};
        use super::super::types::CushionMode;

        let (mut mixer, shared) = steady_mixer((0.5, -0.25));
        shared.set_output(48_000, 64, "test");
        let mut plan = FollowerPlan {
            count: 1,
            ..FollowerPlan::default()
        };
        plan.slots[0] = FollowerLink {
            device_id: "test:follower".to_owned(),
            ..FollowerLink::default()
        };
        shared.set_followers(
            &[OutputDeviceSettings::default()],
            &plan,
            &[(CushionMode::Manual, 10.0)],
        );
        shared.follower_bus(0).set_device_channels(2);
        shared.follower_bus(0).reset_playback();
        shared
            .follower_ring_active(0)
            .store(true, Ordering::Relaxed);
        shared
            .follower_bus(0)
            .set_cushion(CushionMode::Manual, 10.0);
        shared.follower_bus(0).set_source_rate(48_000);

        let frames = 64;
        let mut clock = Vec::new();
        mixer.render(&shared, frames, |_, mapped| {
            clock.push([mapped.sample(0), mapped.sample(1)]);
        });
        assert_eq!(
            shared.follower_bus(0).ring_frames(),
            frames,
            "every finished clock frame should land in the follower ring"
        );
        // Default maps on both: the follower carries what the clock plays.
        let mut frame = [0.0; crate::audio::output_map::FRAME_LANES];
        for expected in &clock {
            assert!(shared.follower_bus(0).pop_frame(&mut frame));
            assert_eq!([frame[0], frame[1]], *expected);
        }

        // Duplex path must not push (in-graph fan-out owns followers).
        shared.direct.store(true, Ordering::Relaxed);
        shared.follower_bus(0).reset_playback();
        let _ = render(&mut mixer, &shared, frames);
        assert_eq!(
            shared.follower_bus(0).ring_frames(),
            0,
            "direct mode must not fill generic follower rings"
        );
    }

    #[test]
    fn a_generic_follower_gets_its_own_sum_of_the_paths() {
        use super::super::shared::{FollowerLink, FollowerPlan};
        use super::super::types::CushionMode;

        let (mut mixer, shared) = two_path_mixer((0.5, -0.25), 2);
        // The clock device plays path 0 only; the 4-channel follower plays
        // path 0 on Out 1/2 and path 1, inverted, on Out 3/4.
        shared
            .clock_device
            .set(&device_with(&[(1, PathMapSettings::unrouted())]));
        let follower = device_with(&[(
            1,
            PathMapSettings {
                map: ChannelMap {
                    left: 0b0100,
                    right: 0b1000,
                },
                invert: true,
                ..PathMapSettings::default()
            },
        )]);
        let mut plan = FollowerPlan {
            count: 1,
            ..FollowerPlan::default()
        };
        plan.slots[0] = FollowerLink {
            device_id: "test:follower".to_owned(),
            ..FollowerLink::default()
        };
        shared.set_followers(&[follower], &plan, &[(CushionMode::Safe, 10.0)]);
        let bus = shared.follower_bus(0);
        bus.set_device_channels(4);
        bus.reset_playback();
        shared
            .follower_ring_active(0)
            .store(true, Ordering::Relaxed);

        let clock = device_frames(&mut mixer, &shared, 2, 600);
        assert_eq!(clock[599], vec![0.5, -0.25]);
        assert_eq!(bus.ring_frames(), 600);
        let mut frame = [0.0; crate::audio::output_map::FRAME_LANES];
        for _ in 0..600 {
            assert!(bus.pop_frame(&mut frame));
        }
        assert_eq!(frame[..4], [0.5, -0.25, -0.5, 0.25]);
        // The clock side publishes the follower's meters, per channel.
        assert_eq!(shared.followers[0].meters()[..4], [0.5, 0.25, 0.5, 0.25]);

        // A follower whose stream stopped is not mapped: its ring stays put
        // and its meters fall to 0.
        shared
            .follower_ring_active(0)
            .store(false, Ordering::Relaxed);
        let _ = device_frames(&mut mixer, &shared, 2, 64);
        assert_eq!(bus.ring_frames(), 0);
        assert_eq!(shared.followers[0].meters()[..4], [0.0; 4]);
    }

    /// A steady mixer with a second path in slot 1 (EQs bypassed, playing
    /// the same input) on a `channels`-channel clock device.
    fn two_path_mixer(signal: (f32, f32), channels: usize) -> (OutputMixer, Shared) {
        use super::super::path::PathMix;
        let (_, shared) = steady_mixer(signal);
        let bypassed = EqSettings {
            enabled: false,
            ..EqSettings::default()
        };
        let mix = PathMix {
            eqs: [bypassed, bypassed],
            ..PathMix::default()
        };
        shared.restore_path(1, &mix, Vec::new());
        (OutputMixer::new(48_000, channels), shared)
    }

    #[test]
    fn each_live_path_publishes_its_dsp_time_and_block_budget() {
        let (mut mixer, shared) = two_path_mixer((0.5, -0.25), 2);
        mixer.prepare(480);
        let _ = device_frames(&mut mixer, &shared, 2, 480);
        for slot in 0..2 {
            let time = shared.paths[slot].dsp_time();
            assert_eq!(time.block_ns, 10_000_000, "480 frames at 48 kHz are 10 ms");
            assert!(time.avg_ns < time.block_ns, "a plain chain fits its block");
            assert!(
                time.peak_ns >= time.avg_ns,
                "one block: the peak is that block"
            );
        }
        assert_eq!(
            shared.paths[2].dsp_time(),
            super::DspTime::default(),
            "an unused slot never runs"
        );
        shared.paths[0].reset_meters();
        assert_eq!(shared.paths[0].dsp_time(), super::DspTime::default());
    }

    /// `frames` frames of the clock device, as one block.
    fn device_frames(
        mixer: &mut OutputMixer,
        shared: &Shared,
        channels: usize,
        frames: usize,
    ) -> Vec<Vec<f32>> {
        let mut out = vec![vec![0.0f32; channels]; frames];
        mixer.render(shared, frames, |index, mapped| {
            for (channel, sample) in out[index].iter_mut().enumerate() {
                *sample = mapped.sample(channel);
            }
        });
        out
    }

    fn largest_device_step(out: &[Vec<f32>], channel: usize) -> f32 {
        out.windows(2)
            .map(|pair| (pair[1][channel] - pair[0][channel]).abs())
            .fold(0.0, f32::max)
    }

    fn device_with(paths: &[(usize, PathMapSettings)]) -> OutputDeviceSettings {
        let mut settings = OutputDeviceSettings::default();
        for (slot, entry) in paths {
            settings.path_map[*slot] = *entry;
        }
        settings
    }

    #[test]
    fn two_paths_add_up_where_their_maps_meet() {
        let (mut mixer, shared) = two_path_mixer((0.5, -0.25), 4);
        // Path 0 on Out 1/2, path 1's L on Out 1 and its R on Out 4.
        shared.clock_device.set(&device_with(&[(
            1,
            PathMapSettings {
                map: ChannelMap {
                    left: 0b0001,
                    right: 0b1000,
                },
                ..PathMapSettings::default()
            },
        )]));
        let out = device_frames(&mut mixer, &shared, 4, 32);
        // Out 1 sums both L; Out 2 is path 0's R; Out 3 is reached by no
        // path, so it gets the mono copy of both paths mapped here; Out 4 is
        // path 1's R.
        assert_eq!(out[31], [1.0, -0.25, 0.25, -0.25]);
    }

    #[test]
    fn level_and_polarity_apply_per_path_and_device() {
        let (mut mixer, shared) = two_path_mixer((0.5, -0.25), 2);
        let gain = 10.0f32.powf(-6.0 / 20.0);
        shared.clock_device.set(&device_with(&[
            (
                0,
                PathMapSettings {
                    level_db: -6.0,
                    ..PathMapSettings::default()
                },
            ),
            (
                1,
                PathMapSettings {
                    invert: true,
                    ..PathMapSettings::default()
                },
            ),
        ]));
        // Follower 0 keeps both paths at unity, normal polarity.
        let plan = super::super::shared::FollowerPlan {
            count: 1,
            ..Default::default()
        };
        shared.set_followers(&[OutputDeviceSettings::default()], &plan, &[]);
        // The duplex graph maps the followers in this mixer.
        shared.direct.store(true, Ordering::Relaxed);
        let mut clock = Vec::new();
        let mut follower = Vec::new();
        mixer.render_direct_devices(&shared, &[0.5; 64], &[-0.25; 64], |device, _, mapped| {
            let frame = [mapped.sample(0), mapped.sample(1)];
            if device == 0 {
                clock.push(frame);
            } else {
                follower.push(frame);
            }
        });
        assert_eq!(clock[63], [0.5 * gain - 0.5, -0.25 * gain + 0.25]);
        assert_eq!(follower[63], [1.0, -0.5]);
        assert_eq!(shared.clock_device.path_gain(1), -1.0);
    }

    #[test]
    fn a_polarity_flip_ramps_through_zero_without_a_click() {
        let (mut mixer, shared) = two_path_mixer((0.5, 0.5), 2);
        let _adopted = device_frames(&mut mixer, &shared, 2, 16);
        shared.clock_device.set(&device_with(&[(
            1,
            PathMapSettings {
                invert: true,
                ..PathMapSettings::default()
            },
        )]));
        let out = device_frames(&mut mixer, &shared, 2, 600);
        assert_eq!(
            out[0][0],
            1.0 - 1.0 / 288.0,
            "the first sample barely moves"
        );
        assert!(out.iter().any(|frame| frame[0].abs() < 0.01), "passes zero");
        assert_eq!(out[599], [0.0, 0.0]);
        assert!(largest_device_step(&out, 0) < 1.0 / 100.0);
    }

    #[test]
    fn solo_silences_the_other_paths_with_a_ramp() {
        let (mut mixer, shared) = two_path_mixer((0.5, -0.25), 2);
        assert_eq!(render(&mut mixer, &shared, 16)[15], [1.0, -0.5, 0.0, 0.0]);
        shared.paths[1].set_solo(true);
        assert!(shared.any_solo());
        let out = render(&mut mixer, &shared, 600);
        assert!(out[0][0] > 0.99);
        assert_eq!(out[599], [0.5, -0.25, 0.0, 0.0]);
        assert!(largest_step(&out) < 0.5 / 100.0);
        // Soloing both is the same as soloing none.
        shared.paths[0].set_solo(true);
        assert_eq!(render(&mut mixer, &shared, 600)[599], [1.0, -0.5, 0.0, 0.0]);
        // A path fading out does not keep the others silent by its solo.
        shared.paths[0].set_solo(false);
        assert!(shared.remove_path(1));
        assert!(!shared.any_solo());
        let out = render(&mut mixer, &shared, 600);
        assert_eq!(out[599], [0.5, -0.25, 0.0, 0.0]);
    }

    #[test]
    fn path_mute_and_level_ramp_without_a_click() {
        let (mut mixer, shared) = two_path_mixer((0.8, 0.8), 2);
        let _adopted = render(&mut mixer, &shared, 16);
        shared.paths[1].set_mute(true);
        let muted = render_blocks(&mut mixer, &shared, 2);
        assert!(muted[0][0] > 1.59);
        assert_eq!(muted.last().unwrap()[0], 0.8);
        assert!(
            largest_step(&muted) < 0.8 / 100.0,
            "{}",
            largest_step(&muted)
        );

        shared.paths[1].set_mute(false);
        shared.paths[1].set_level_db(-6.0);
        let gain = 10.0f32.powf(-6.0 / 20.0);
        let back = render_blocks(&mut mixer, &shared, 2);
        assert!(largest_step(&back) < 0.8 / 100.0);
        assert_eq!(back.last().unwrap()[0], 0.8 + 0.8 * gain);
        // The path meters stay before the path level.
        assert_eq!(shared.paths[1].meters(), (0.8, 0.8));
    }

    #[test]
    fn a_removed_path_fades_out_and_then_goes_inactive() {
        use super::super::path::PathState;
        let (mut mixer, shared) = two_path_mixer((0.8, 0.8), 2);
        let _adopted = render(&mut mixer, &shared, 16);
        assert!(shared.remove_path(0));
        assert_eq!(shared.paths[0].state(), PathState::FadingOut);
        assert!(!shared.remove_path(1), "the last active path stays");
        let out = render_blocks(&mut mixer, &shared, 2);
        assert!(out[0][0] > 1.59);
        assert_eq!(out.last().unwrap()[0], 0.8);
        assert!(largest_step(&out) < 0.8 / 100.0);
        assert_eq!(shared.paths[0].state(), PathState::Inactive);
        // A stalled fade (no block ran) is finished by the UI thread.
        assert!(!shared.remove_path(1));
        let added = shared.add_path().expect("slot 0 is free again");
        assert_eq!(added, 0);
        assert!(shared.remove_path(added));
        shared.service_paths();
        assert_eq!(shared.paths[added].state(), PathState::Inactive);
        // With a bridge running, the UI waits for the mixer, and only
        // finishes a fade that is overdue.
        shared.running.store(true, Ordering::Relaxed);
        let added = shared.add_path().unwrap();
        assert!(shared.remove_path(added));
        shared.service_paths();
        assert_eq!(shared.paths[added].state(), PathState::FadingOut);
        shared.paths[added].backdate_removal(std::time::Duration::from_secs(2));
        shared.service_paths();
        assert_eq!(shared.paths[added].state(), PathState::Inactive);
    }

    #[test]
    fn a_duplicate_starts_muted_and_copies_the_chain() {
        use super::super::gate::GateSettings;
        let _main = super::super::shared::tests::claim_main_thread();
        let fake = crate::audio::clap::tests::fake_plugin_for_test;
        let (mut mixer, shared) = steady_mixer((0.5, -0.25));
        shared.set_routes([[false, true, false, false], [true, false, false, false]]);
        let gate = GateSettings {
            threshold_db: -50.0,
            ..GateSettings::default()
        };
        shared.set_gate(gate);
        shared.paths[0].set_level_db(-3.0);
        shared.paths[0].edit_chain(|chain| chain.insert_clap_for_test(fake(false)));
        let before = render(&mut mixer, &shared, 64);

        let copy = shared.duplicate_path(0).expect("a free slot");
        assert_eq!(copy, 1);
        let duplicate = &shared.paths[copy];
        assert!(duplicate.muted() && !duplicate.soloed());
        let mix = duplicate.mix();
        assert_eq!(mix.routes, shared.paths[0].mix().routes);
        assert_eq!(mix.gate, gate);
        assert_eq!(mix.level_db, -3.0);
        let plugins = duplicate.plugins.lock().unwrap().prefs();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins, shared.paths[0].plugin_prefs());
        // Added muted: the sum does not move.
        let after = render(&mut mixer, &shared, 64);
        assert_eq!(after[63], before[63]);
    }

    #[test]
    fn a_new_path_stays_silent_until_its_plugins_open_then_fades_in() {
        use super::super::path::PathMix;
        let (mut mixer, shared) = steady_mixer((0.5, -0.25));
        let _adopted = render(&mut mixer, &shared, 16);
        let bypassed = EqSettings {
            enabled: false,
            ..EqSettings::default()
        };
        let mix = PathMix {
            eqs: [bypassed, bypassed],
            ..PathMix::default()
        };
        let pending = crate::audio::PluginPref {
            path: "missing.vst3".to_owned(),
            name: "Missing".to_owned(),
            ..crate::audio::PluginPref::default()
        };
        shared.restore_path(1, &mix, vec![pending]);
        assert!(shared.paths[1].chain_pending());
        let waiting = render(&mut mixer, &shared, 400);
        assert!(waiting.iter().all(|frame| *frame == [0.5, -0.25, 0.0, 0.0]));
        // The UI thread tries to open it (here it fails, so the insert is
        // left out): the path fades in instead of jumping.
        shared.paths[1].ensure_plugin_loaded(0);
        assert!(!shared.paths[1].chain_pending());
        let out = render(&mut mixer, &shared, 400);
        assert!(out[0][0] > 0.5 && out[0][0] < 0.51, "{:?}", out[0]);
        assert!(largest_step(&out) < 0.5 / 100.0);
        assert_eq!(out[399], [1.0, -0.5, 0.0, 0.0]);
    }
}
