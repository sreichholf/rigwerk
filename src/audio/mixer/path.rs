//! One path, from the split to the sum.
//!
//! A feed change waits until the path gain is 0.  A missed chain plays its
//! input delayed by the plugin chain's own latency and declicks the step.
//! The gate key is the dry input, delayed so a POST gate does not open a
//! chain latency early.  DSP time restarts when the block length changes.

use std::sync::atomic::Ordering;
use std::time::Instant;

use super::super::cab::CabStage;
use super::super::delay::{
    Declick, DelayLine, DelayTap, MonoDelayLine, DECLICK_SECONDS, TAP_FADE_SECONDS,
};
use super::super::eq::{EqSlot, Equalizer};
use super::super::gate::{GateKey, GateSlot, NoiseGate};
use super::super::loudness::LoudnessMeter;
use super::super::nam::{NamPosition, NamStage};
use super::super::path::{DspTime, PathControls, PathState};
use super::super::pitch::{PitchSettings, PitchShifter};
use super::super::plugins::{ChainLease, MAX_ALIGN};
use super::ramp::{PathRamps, Ramp};

pub(crate) struct PathProcessor {
    /// Kept across callbacks so the effects run the block without
    /// allocating.  At least the prepared size; release may grow it.
    pub(super) block: Vec<[f32; 4]>,
    plugin_l: Vec<f32>,
    plugin_r: Vec<f32>,
    /// Pre and post EQ, and the version each was last built from.
    eq: [Equalizer; 2],
    eq_version: [u64; 2],
    gate: NoiseGate,
    gate_version: u64,
    /// Dry-input peak after routing, before the shifter: the gate key.
    /// Fed whether the gate is on, so turning it on mid-note is not a
    /// latency of silence.
    key_line: MonoDelayLine,
    /// A PRE gate reads the key a pitch latency back, with the note it cuts.
    pre_key_tap: DelayTap,
    /// A POST gate reads the key a chain latency back, so it does not open
    /// early.  A tap nothing reads follows at once.
    post_key_tap: DelayTap,
    /// The first block snaps both taps instead of gliding from 0.
    key_taps_adopted: bool,
    /// First in the chain.  Never runs on a return.
    pitch: PitchShifter,
    pitch_version: u64,
    pitch_settings: PitchSettings,
    /// `None` makes the next block send the settings again.
    pitch_applied: Option<(PitchSettings, u32)>,
    /// 0 on a return, or while the shifter is off.
    pitch_latency: u32,
    /// Models are taken at the start of the block.
    nam: NamStage,
    /// The kernel is taken at the start of the block.  It adds no latency.
    cab: CabStage,
    /// Per sample, so a control read once per block does not step.
    ramps: PathRamps,
    /// The first block snaps, so opening does not fade from the defaults.
    controls_adopted: bool,
    plan: BlockPlan,
    /// A new generation means the slot was activated: start fresh.
    generation: u64,
    /// After the post EQ and before the path gain, so mute, solo and the
    /// trim never move it.  Fed only while a measurement runs.
    match_meter: LoudnessMeter,
    match_generation: u64,
    match_active: bool,
    /// Level, match, mute, solo and the removal fade as one gain.
    pub(super) gain: Ramp,
    /// This block's path gain per frame, so a send can read it first.
    pub(super) gains: Vec<f32>,
    /// Runs on sends, after the input paths, and no solo silences it.
    /// A feed change waits until the gain is 0.
    pub(super) is_return: bool,
    /// The slot was being removed when this block began.
    pub(super) fading: bool,
    /// Frames a removed, silent path still runs so its taps play out.
    /// `None` until the fade reaches silence.
    pub(super) drain: Option<u32>,
    /// What alignment counts: the chain, oversampled NAM and the shifter.
    /// The cabinet adds none.  A missed chain uses `chain_latency`.
    latency: u32,
    /// Unclamped, so the report can flag a chain past [`MAX_ALIGN`].
    pub(super) latency_raw: u32,
    /// The missed-chain fallback delays by this, not the whole path: NAM
    /// before the chain already delayed the input.
    chain_latency: u32,
    /// Taken on the path's first block, parked when the path goes idle.
    chain: Option<ChainLease>,
    /// Chain input, read back delayed when the chain cannot run.
    chain_line: DelayLine,
    chain_tap: DelayTap,
    /// 0 after a missed lock, back to 1 over [`CHAIN_RESUME_SECONDS`].
    chain_mix: f32,
    chain_step: f32,
    /// Frames before the chain fades back: the effects missed that input,
    /// so their delay lines still hold the gap.
    chain_hold: u32,
    tap_fade: f32,
    /// Smooths the step into the fallback.  There is no output to crossfade.
    chain_declick: Declick,
    declick_len: u32,
    /// Nothing has played since the path started: nothing to smooth from.
    chain_fresh: bool,
    dsp_block_ns: u64,
    dsp: DspMeter,
}

/// About 100 blocks to settle.
const DSP_SMOOTHING: f32 = 0.02;
/// Time constant of the published DSP peak's decay, seconds.
pub(super) const DSP_PEAK_SECONDS: f32 = 2.0;

/// No allocation, no lock.  A new block length starts the meter over:
/// a long block's time would read as an overload of a short one.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct DspMeter {
    avg_ns: f32,
    peak_ns: f32,
    frames: usize,
    rate: u32,
}

impl DspMeter {
    pub(super) fn push(&mut self, measured_ns: u64, frames: usize, rate: u32) -> DspTime {
        if (frames, rate) != (self.frames, self.rate) {
            *self = Self {
                frames,
                rate,
                ..Self::default()
            };
        }
        let measured = measured_ns.min(u32::MAX as u64) as f32;
        self.avg_ns = if self.avg_ns == 0.0 {
            measured
        } else {
            self.avg_ns + (measured - self.avg_ns) * DSP_SMOOTHING
        };
        let seconds = frames as f32 / rate.max(1) as f32;
        let decay = (-seconds / DSP_PEAK_SECONDS).exp();
        self.peak_ns = measured.max(self.peak_ns * decay);
        let block_ns = (frames as u64 * 1_000_000_000 / rate.max(1) as u64).min(u32::MAX as u64);
        DspTime {
            avg_ns: self.avg_ns as u32,
            peak_ns: self.peak_ns as u32,
            block_ns: block_ns as u32,
        }
    }
}

/// How long the chain output takes to return after a missed block.
const CHAIN_RESUME_SECONDS: f32 = 0.005;

/// Chunks no longer than this, so the fallback line is [`MAX_ALIGN`]
/// plus one chunk.
const CHAIN_CHUNK: usize = super::super::plugins::MAX_BLOCK;

impl PathProcessor {
    /// `slot` staggers the pitch period estimates.
    pub(super) fn new(rate: u32, slot: usize) -> Self {
        Self {
            latency: 0,
            latency_raw: 0,
            chain_latency: 0,
            drain: None,
            chain: None,
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
            dsp: DspMeter::default(),
            block: Vec::new(),
            plugin_l: Vec::new(),
            plugin_r: Vec::new(),
            eq: [Equalizer::new(rate), Equalizer::new(rate)],
            eq_version: [0; 2],
            gate: NoiseGate::new(rate),
            gate_version: 0,
            key_line: MonoDelayLine::new(0),
            pre_key_tap: DelayTap::default(),
            post_key_tap: DelayTap::default(),
            key_taps_adopted: false,
            pitch: PitchShifter::new(rate, slot),
            pitch_version: 0,
            pitch_settings: PitchSettings::default(),
            pitch_applied: None,
            pitch_latency: 0,
            nam: NamStage::new(rate),
            cab: CabStage::new(rate),
            ramps: PathRamps::new(),
            controls_adopted: false,
            plan: BlockPlan::default(),
            generation: 0,
            match_meter: LoudnessMeter::new(rate),
            match_generation: 0,
            match_active: false,
            gain: Ramp::new(1.0),
            gains: Vec::new(),
            is_return: false,
            fading: false,
        }
    }

    /// Fresh state for a newly activated slot.  No allocation; the scratch
    /// is kept.
    fn restart(&mut self, rate: u32, generation: u64) {
        self.eq = [Equalizer::new(rate), Equalizer::new(rate)];
        self.gate = NoiseGate::new(rate);
        self.key_line.reset();
        self.key_taps_adopted = false;
        self.pitch.restart();
        self.pitch_settings = PitchSettings::default();
        self.nam.restart(rate);
        self.cab.restart(rate);
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
        self.dsp = DspMeter::default();
        self.match_meter.configure(rate, 2);
        self.match_meter.reset();
        self.match_active = false;
    }

    /// Parks the runtime so the UI can edit and release plugins itself.
    pub(super) fn release_chain(&mut self) {
        self.chain = None;
    }

    /// Keeps the pitch schedule in phase with the paths that kept running,
    /// so a slot that comes back does not estimate with another path.
    pub(super) fn skip_idle(&mut self, frames: usize) {
        self.pitch.skip(frames);
    }

    /// Grows the scratch outside the callback.
    pub(super) fn prepare(&mut self, frames: usize) {
        self.reserve_block(frames);
        if self.plugin_l.len() < frames {
            self.plugin_l.resize(frames, 0.0);
            self.plugin_r.resize(frames, 0.0);
        }
    }

    /// Room for one block, sized before the stream opens so the callback
    /// does not allocate.  A larger block allocates once; debug asserts it.
    pub(super) fn reserve_block(&mut self, frames: usize) {
        if self.block.len() < frames {
            self.block.resize(frames, [0.0; 4]);
        }
        let key_capacity = MAX_ALIGN as usize + frames;
        if self.key_line.max_delay() < key_capacity {
            // A larger block: the key history starts over.
            self.key_line = MonoDelayLine::new(key_capacity);
        }
        if self.gains.len() < frames {
            self.gains.resize(frames, 0.0);
        }
        self.nam.reserve(frames);
        self.cab.reserve(frames);
    }

    /// The next block rebuilds the EQs, gate and shifter.  Version 0 is
    /// never published, so this always looks like a change.
    pub(super) fn invalidate_settings(&mut self) {
        self.eq_version = [0; 2];
        self.gate_version = 0;
        self.pitch_version = 0;
        self.pitch_applied = None;
        self.nam.invalidate();
        self.cab.invalidate();
    }

    /// `fade_in`: the mixer is already playing, so a path that just went
    /// live ramps up from silence.  Returns whether the slot started fresh.
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
        // First, so the latency below is the chain this block runs.
        let unopened = controls.begin_chain(&mut self.chain);
        self.nam.begin(controls, rate, ramp_samples);
        // Before the latency: the shifter never runs on a return.  A feed
        // change waits until the path gain is 0.
        let adopted = self.controls_adopted;
        let wanted = controls.is_return();
        if !adopted || self.gain.is_settled_at(0.0) {
            self.is_return = wanted;
        }
        let switching = wanted != self.is_return;
        // Before the latency read, so a switch this block is counted.
        self.follow_pitch(controls, rate);
        self.pitch.begin_block();
        let pitch_latency = if self.is_return {
            0
        } else {
            self.pitch.latency_samples()
        };
        self.pitch_latency = pitch_latency.min(MAX_ALIGN);
        // They really delay their output, so this path does not delay
        // them again.
        let chain_latency = controls.latency_samples();
        self.chain_latency = chain_latency.min(MAX_ALIGN);
        self.latency_raw = chain_latency
            .saturating_add(self.nam.latency_samples())
            .saturating_add(pitch_latency);
        self.latency = self.latency_raw.min(MAX_ALIGN);
        if !self.key_taps_adopted {
            self.pre_key_tap.snap(self.pitch_latency);
            self.post_key_tap.snap(self.latency);
            self.key_taps_adopted = true;
        }
        controls.store_path_latency(self.latency_raw);
        self.cab.begin(controls, rate, ramp_samples);
        self.tap_fade = TAP_FADE_SECONDS * rate as f32;
        self.declick_len = (DECLICK_SECONDS * rate as f32) as u32;
        self.chain_step = 1.0 / (CHAIN_RESUME_SECONDS * rate as f32).max(1.0);
        self.follow_eq(controls, rate);
        self.follow_gate(controls, rate);
        let match_generation = controls.match_measure_generation();
        self.match_meter.configure(rate, 2);
        if match_generation != self.match_generation {
            self.match_generation = match_generation;
            self.match_meter.reset();
        }
        self.match_active = controls.match_measuring();
        self.follow_controls(controls, ramp_samples);
        self.fading = controls.state() == PathState::FadingOut;
        // A chain still opening stays silent rather than playing dry.
        // A return is solo-safe.
        let soloed_out = any_solo && !controls.soloed() && !self.is_return;
        let silent = self.fading || controls.muted() || soloed_out || unopened || switching;
        // One gain, so a match trim never steps.  Sends read it, so a
        // return follows its senders' matches.
        let gain = if silent {
            0.0
        } else {
            controls.level_gain() * controls.match_gain()
        };
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
            gate_keyed: self.gate.settings().key == GateKey::Input,
        };
        restarted
    }

    /// The sends use this to line a return up with its senders.
    pub(super) fn latency(&self) -> u32 {
        self.latency
    }

    pub(super) fn fill_gains(&mut self, frames: usize) {
        let ramp = &mut self.gain;
        for gain in &mut self.gains[..frames] {
            *gain = ramp.next();
        }
    }

    /// A return's sends arrive as L and R, so its routing is ignored.
    fn follow_controls(&mut self, controls: &PathControls, ramp_samples: f32) {
        let coefficient = |flag: bool| if flag { 1.0 } else { 0.0 };
        let mut routes = [[0.0f32; 4]; 2];
        for (input, row) in routes.iter_mut().enumerate() {
            for (tap, coefficient_out) in row.iter_mut().enumerate() {
                let on = if self.is_return {
                    input == tap
                } else {
                    controls.route(input, tap)
                };
                *coefficient_out = coefficient(on);
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

    /// A busy lock keeps the last settings.  Becoming a return turns the
    /// shifter off at once.
    fn follow_pitch(&mut self, controls: &PathControls, rate: u32) {
        let version = controls.pitch_version();
        if version != self.pitch_version {
            if let Some(settings) = controls.try_pitch() {
                self.pitch_settings = settings.sanitized();
                self.pitch_version = version;
            }
        }
        let mut settings = self.pitch_settings;
        if self.is_return {
            settings.enabled = false;
        }
        let wanted = Some((settings, rate));
        if self.pitch_applied == wanted {
            return;
        }
        self.pitch_applied = wanted;
        if self.is_return {
            self.pitch.restart();
        } else {
            self.pitch.update(settings, rate);
        }
    }

    #[inline]
    pub(super) fn stage_input(&mut self, index: usize, left: f32, right: f32) {
        self.block[index] = [left, right, 0.0, 0.0];
    }

    /// State only moves with this path's frames, so a block matches a
    /// frame loop.
    pub(super) fn run_front(&mut self, frames: usize) {
        let keyed = self.plan.gate_keyed;
        if !keyed {
            self.key_line.reset();
        }
        for index in 0..frames {
            let [left, right, _, _] = self.block[index];
            // Before the shifter and the EQ: that is where these act.
            let out = self.ramps.next_input(left, right);
            if keyed {
                // The key is the dry input, before the shifter.
                self.key_line.push(out[0].abs().max(out[1].abs()));
            }
            self.block[index] = out;
        }
        // The shifter is first, so the pre EQ shapes the shifted signal.
        // Off, it leaves the block untouched.  The EQ, gate and cabinet
        // add no latency.
        if !self.is_return {
            self.pitch.process(&mut self.block, frames);
        } else {
            // The schedule still moves, so the slot keeps its hop phase
            // if it becomes an input path again.
            self.pitch.skip(frames);
        }
        if self.plan.gate_pre && keyed {
            self.pre_key_tap
                .set_target(self.pitch_latency, self.tap_fade);
        } else {
            self.pre_key_tap.snap(self.pitch_latency);
        }
        if self.plan.pre_eq || self.plan.gate_pre {
            for index in 0..frames {
                let mut out = self.block[index];
                if self.plan.pre_eq {
                    (out[0], out[1]) = self.eq[EqSlot::Pre.index()].process(out[0], out[1]);
                }
                if self.plan.gate_pre {
                    (out[0], out[1]) = if keyed {
                        // Behind the shifter: the key a pitch latency back.
                        let key = self
                            .pre_key_tap
                            .next_mono(&self.key_line, frames - 1 - index);
                        self.gate.process_keyed(out[0], out[1], key)
                    } else {
                        self.gate.process(out[0], out[1])
                    };
                }
                self.block[index] = out;
            }
        }
        self.nam.process(&mut self.block, frames, NamPosition::Pre);
    }

    #[inline]
    pub(super) fn add_dsp_time(&mut self, since: Instant, now: Instant) -> Instant {
        self.dsp_block_ns += now.saturating_duration_since(since).as_nanos() as u64;
        now
    }

    /// Returns the measured time, ns, for the mixer's total.  Atomics only.
    pub(super) fn publish_dsp(&mut self, controls: &PathControls, frames: usize, rate: u32) -> u64 {
        let measured = self.dsp_block_ns;
        self.dsp_block_ns = 0;
        controls.store_dsp_time(self.dsp.push(measured, frames, rate));
        measured
    }

    /// A chunk the chain cannot run plays its input delayed by the plugin
    /// chain's latency, not the path's: NAM before it already delayed that
    /// input.  The step is declicked; there is no chain output to crossfade.
    /// The chain fades back after `chain_hold` frames.  Latency with no
    /// active effect still runs, so a bypass keeps its delay.
    pub(super) fn run_effects(&mut self, controls: &PathControls, frames: usize) {
        if !controls.has_plugins() && self.chain_latency == 0 {
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
            self.chain_tap.snap(self.chain_latency);
        } else {
            self.chain_tap.set_target(self.chain_latency, self.tap_fade);
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
            let ran = controls.run_plugins(&mut self.chain, chunk_l, chunk_r, count);
            // Into the fallback from the chain's own output.
            let entering = !ran && self.chain_mix > 0.0 && !self.chain_fresh;
            if !ran {
                self.chain_mix = 0.0;
                self.chain_hold = self.chain_latency;
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

    /// A keyed POST gate reads the key a chain latency back, including
    /// the shifter.  [`Self::run_front`] already filled the key line.
    pub(super) fn finish_block(&mut self, controls: &PathControls, frames: usize) {
        self.nam.process(&mut self.block, frames, NamPosition::Post);
        self.cab.process_block(&mut self.block, frames);
        if self.plan.gate_post && self.plan.gate_keyed {
            self.post_key_tap.set_target(self.latency, self.tap_fade);
        } else {
            self.post_key_tap.snap(self.latency);
        }
        let mut peak_l = 0.0f32;
        let mut peak_r = 0.0f32;
        for (index, out) in self.block[..frames].iter_mut().enumerate() {
            if self.plan.gate_post {
                (out[0], out[1]) = if self.plan.gate_keyed {
                    let key = self
                        .post_key_tap
                        .next_mono(&self.key_line, frames - 1 - index);
                    self.gate.process_keyed(out[0], out[1], key)
                } else {
                    self.gate.process(out[0], out[1])
                };
            }
            if self.plan.post_eq {
                (out[0], out[1]) = self.eq[EqSlot::Post.index()].process(out[0], out[1]);
            }
            if self.match_active {
                self.match_meter.push(&out[..2]);
            }
            peak_l = peak_l.max(out[0].abs());
            peak_r = peak_r.max(out[1].abs());
        }
        if self.match_active {
            let (integrated, blocks) = self.match_meter.integrated_with_blocks();
            controls.publish_match_measurement(integrated, blocks as u32);
        }
        controls.store_meters(peak_l, peak_r);
        controls.store_gate_reduction(self.gate.take_reduction_db());
    }
}

/// Decided once at the start of the block.
#[derive(Clone, Copy, Default)]
struct BlockPlan {
    pre_eq: bool,
    post_eq: bool,
    gate_pre: bool,
    gate_post: bool,
    /// The gate listens to the path's dry input.
    gate_keyed: bool,
}
