//! One signal path's chain in the mixer, from the split to the sum (see
//! the `mixer` module docs for where it sits in the block).

use std::sync::atomic::Ordering;
use std::time::Instant;

use super::super::delay::{Declick, DelayLine, DelayTap, DECLICK_SECONDS, TAP_FADE_SECONDS};
use super::super::eq::{EqSlot, Equalizer};
use super::super::gate::{GateSlot, NoiseGate};
use super::super::path::{DspTime, PathControls, PathState};
use super::super::plugins::MAX_ALIGN;
use super::ramp::{PathRamps, Ramp};

/// The chain of one signal path, from the split to the sum: routing,
/// channel enable and mono (ramped), the pre EQ, a PRE gate, the plugin
/// inserts, a POST gate and the post EQ.  [`OutputMixer`] owns one per
/// possible path, preallocated, and runs only the active ones; every path
/// gets the same stereo frame from the clock side.
pub(crate) struct PathProcessor {
    /// One block of this path's taps, kept between callbacks so the plugin
    /// inserts can work on the whole block without allocating.
    pub(super) block: Vec<[f32; 4]>,
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
    pub(super) gain: Ramp,
    /// The slot was being removed when this block began.
    pub(super) fading: bool,
    /// Frames a removed, silent path still runs so its alignment delay
    /// lines play out; `None` until its fade reaches silence.
    pub(super) drain: Option<u32>,
    /// The chain latency (frames) read at the start of the block: what the
    /// alignment counts for this path, and what a missed chain lock is
    /// replaced by.
    latency: u32,
    /// The same, before clamping to [`MAX_ALIGN`] (for the report).
    pub(super) latency_raw: u32,
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
const CHAIN_CHUNK: usize = super::super::plugins::MAX_BLOCK;

impl PathProcessor {
    pub(super) fn new(rate: u32) -> Self {
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
    pub(super) fn prepare(&mut self, frames: usize) {
        self.reserve_block(frames);
        if self.plugin_l.len() < frames {
            self.plugin_l.resize(frames, 0.0);
            self.plugin_r.resize(frames, 0.0);
        }
    }

    /// Makes room for one block.  Only a host that hands over a larger block
    /// than prepared allocates here, once.
    pub(super) fn reserve_block(&mut self, frames: usize) {
        if self.block.len() < frames {
            self.block.resize(frames, [0.0; 4]);
        }
    }

    /// Forgets which settings the EQs and gate were built from, so the next
    /// block redesigns them (for a new rate).  Version 0 is never published.
    pub(super) fn invalidate_settings(&mut self) {
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
    pub(super) fn begin_block(
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
    pub(super) fn stage_input(&mut self, index: usize, left: f32, right: f32) {
        self.block[index] = [left, right, 0.0, 0.0];
    }

    /// The front of the chain over a block of staged input frames, in
    /// place: routing, channel enable and mono (with their ramps), then the
    /// pre EQ and a PRE gate.  The result waits in `block` for the plugin
    /// inserts.  Each path's state only moves with its own frames, so
    /// running it per block instead of per frame gives the same samples,
    /// and the mixer can time each path as one piece.
    pub(super) fn run_front(&mut self, frames: usize) {
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
    pub(super) fn add_dsp_time(&mut self, since: Instant, now: Instant) -> Instant {
        self.dsp_block_ns += now.saturating_duration_since(since).as_nanos() as u64;
        now
    }

    /// Smooths this block's DSP time into the published one, folds it into
    /// the decaying peak ([`DSP_PEAK_SECONDS`]) and stores both with the
    /// block's length (`frames` at `rate`), the path's budget.  Atomics
    /// only.
    pub(super) fn publish_dsp(&mut self, controls: &PathControls, frames: usize, rate: u32) {
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
    /// not take the chain lock at all (see the `audio::path::plugins` docs), so a
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
    pub(super) fn run_inserts(&mut self, controls: &PathControls, frames: usize) {
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
    pub(super) fn finish_block(&mut self, controls: &PathControls, frames: usize) {
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

/// Which built-in stages run in one block, decided once at its start.
#[derive(Clone, Copy, Default)]
struct BlockPlan {
    pre_eq: bool,
    post_eq: bool,
    gate_pre: bool,
    gate_post: bool,
}
