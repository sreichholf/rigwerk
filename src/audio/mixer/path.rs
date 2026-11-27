//! One signal path's chain in the mixer, from the split to the sum (see
//! the `mixer` module docs for where it sits in the block).

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

/// The chain of one signal path, from the split to the sum: routing,
/// channel enable and mono (ramped), the pitch shifter (not on a return),
/// the pre EQ, a PRE gate, the NAM blocks
/// that sit before the effects, the plugin effects, the NAM blocks after
/// them, the cabinet, a POST gate and the post EQ.  [`OutputMixer`] owns one per
/// possible path, preallocated, and runs only the active ones; every path
/// gets the same stereo frame from the clock side.
pub(crate) struct PathProcessor {
    /// One block of this path's taps, kept between callbacks so the plugin
    /// effects can work on the whole block without allocating.  Its length is
    /// at least the prepared block size ([`Self::prepare`]); release growth for
    /// a larger host block can make it longer.
    pub(super) block: Vec<[f32; 4]>,
    plugin_l: Vec<f32>,
    plugin_r: Vec<f32>,
    /// The two built-in EQs (pre and post effects), and the settings version
    /// each was last updated from.
    eq: [Equalizer; 2],
    eq_version: [u64; 2],
    /// Built-in noise gate and the settings version it was last updated from.
    gate: NoiseGate,
    gate_version: u64,
    /// The dry input's peak per frame, taken after routing and mono and
    /// before the pitch shifter: the gate's key when it listens to the
    /// input.  The whole block goes in at the front of the chain
    /// ([`Self::run_front`]) before any gate reads it, so frame `index` of a
    /// `frames`-long block reads `frames − 1 − index` plus its delay back.
    /// It holds [`MAX_ALIGN`] plus the largest block ([`Self::reserve_block`]).
    /// Fed while the gate is set to listen to the input, on or off and in
    /// either slot, so turning it on or moving it after the effects
    /// mid-note does not leave the detector a latency of silence.
    key_line: MonoDelayLine,
    /// Where a PRE gate reads the key: the pitch shifter's latency, so the
    /// detector hears the note the gate is cutting.
    pre_key_tap: DelayTap,
    /// Where a POST gate reads it: the whole chain's latency (clamped), so
    /// it does not hear a note a chain's latency early.  Both glide when
    /// the latency moves (a tap nothing reads follows at once).
    post_key_tap: DelayTap,
    /// The first block since the path (re)started snaps both taps to its
    /// latencies instead of gliding from 0.
    key_taps_adopted: bool,
    /// Built-in pitch shifter, first in the chain.  Allocated here for
    /// 192 kHz; never runs on a return.
    pitch: PitchShifter,
    /// The settings version the pitch settings were last read at, and
    /// those settings.
    pitch_version: u64,
    pitch_settings: PitchSettings,
    /// What the shifter was last given (the settings as it runs them, off
    /// on a return, and the rate); `None` makes the next block give it
    /// again.
    pitch_applied: Option<(PitchSettings, u32)>,
    /// The shifter's latency this block, clamped to [`MAX_ALIGN`]: 0 when it
    /// is off or the path is a return.
    pitch_latency: u32,
    /// Built-in NAM blocks.  Their models are taken at the start of the
    /// block; the scratch they process into is grown in
    /// [`Self::reserve_block`].
    nam: NamStage,
    /// Built-in cabinet, after the NAM blocks wherever they sit.  Its kernel
    /// is taken at the start of the block.  The history is allocated in
    /// [`CabStage::new`].
    cab: CabStage,
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
    /// The Match paths meter: after the post EQ, before the path gain, so a
    /// mute, a solo or the trim itself never changes what it hears.  It is
    /// fed only while a measurement runs.
    match_meter: LoudnessMeter,
    /// [`PathControls::match_measure_generation`] as last seen.
    match_generation: u64,
    match_active: bool,
    /// Path level, match, mute, solo and the removal fade as one gain.
    pub(super) gain: Ramp,
    /// That gain per frame of this block ([`Self::fill_gains`]), so a
    /// send can take the path after its level before the sum reads it.
    pub(super) gains: Vec<f32>,
    /// The path is a return this block: it runs on the other paths'
    /// sends, after them, and no solo silences it.  A change of feed is
    /// taken only once the path gain has reached 0, so the chain's input
    /// never switches while it is heard.
    pub(super) is_return: bool,
    /// The slot was being removed when this block began.
    pub(super) fading: bool,
    /// Frames a removed, silent path still runs so its alignment delay
    /// lines play out; `None` until its fade reaches silence.
    pub(super) drain: Option<u32>,
    /// The chain latency (frames) read at the start of the block: what the
    /// alignment counts for this path, and what a missed chain is replaced
    /// by.  The plugin chain's latency plus, for each NAM block that runs an
    /// oversampled model this block, the oversampling pair's delay, plus the
    /// pitch shifter's while it is on.  The cabinet adds none.
    latency: u32,
    /// The same, before clamping to [`MAX_ALIGN`] (for the report).
    pub(super) latency_raw: u32,
    /// The plugin chain's own latency, clamped to [`MAX_ALIGN`]: what the
    /// fallback for a missed chain delays by.  It stands in for the chain
    /// alone; the NAM blocks before it already delayed its input.
    chain_latency: u32,
    /// The path's chain runtime, taken with the first block of the path and
    /// parked again when the path stops being live or this processor drops
    /// (see `plugins::chain`).
    chain: Option<ChainLease>,
    /// The input of the plugin chain, for the blocks the chain cannot run
    /// (see [`Self::run_effects`]): read back delayed by the chain latency.
    chain_line: DelayLine,
    chain_tap: DelayTap,
    /// How much of the chain's own output plays: 0 right after a missed
    /// lock, back to 1 over [`CHAIN_RESUME_SECONDS`].
    chain_mix: f32,
    chain_step: f32,
    /// Frames the chain still has to run before its output fades back in
    /// after a missed lock: its effects missed that input, so for one
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
    /// The time this block spent on the path so far, ns, and its smoothed
    /// time and decaying peak per block (see [`Self::publish_dsp`]).
    dsp_block_ns: u64,
    dsp: DspMeter,
}

/// How fast the published DSP time follows the measured one, per block.
/// About 100 blocks (a second at 512 frames / 48 kHz) to settle.
const DSP_SMOOTHING: f32 = 0.02;
/// Time constant of the published DSP peak's decay, seconds.
pub(super) const DSP_PEAK_SECONDS: f32 = 2.0;

/// One block's measured DSP time folded into a smoothed time per block
/// ([`DSP_SMOOTHING`]) and a peak that decays over [`DSP_PEAK_SECONDS`].
/// Each path has one, and the mixer one for the total of every path, fed
/// the sum of the paths' times in the same block.  Plain floats: no
/// allocation, no lock.
///
/// The times are absolute, so a new block length (a quantum or rate
/// change) starts the meter over: a 1024-frame block's time would read as
/// an overload of a 256-frame one for seconds.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct DspMeter {
    avg_ns: f32,
    peak_ns: f32,
    /// The block length of the last push.
    frames: usize,
    rate: u32,
}

impl DspMeter {
    /// Folds in one block that took `measured_ns` and lasted `frames` at
    /// `rate`, and returns what to publish.
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

/// How long the chain's output takes to come back after a block it missed
/// (it played its delayed input meanwhile).
const CHAIN_RESUME_SECONDS: f32 = 0.005;

/// The plugin chain runs in chunks of at most this many frames, so the
/// fallback delay line only needs [`MAX_ALIGN`] plus one chunk.
const CHAIN_CHUNK: usize = super::super::plugins::MAX_BLOCK;

impl PathProcessor {
    /// `slot` is the path's index: the pitch shifter staggers its period
    /// estimates by it.
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

    /// Starts the chain over for a newly activated slot: fresh EQ and gate
    /// state, settings re-read, and ramps that adopt the settings instead of
    /// moving from whatever the slot played before.  No allocation; the
    /// block scratch is kept.
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

    /// Parks the chain runtime for a path that is not live, so the UI can
    /// apply its edits (and release a removed path's plugins) itself.
    pub(super) fn release_chain(&mut self) {
        self.chain = None;
    }

    /// A block of a path that is not live: keeps the pitch estimate
    /// schedule in the phase of the paths that kept running, so a slot
    /// that comes back does not estimate together with another path.  No
    /// allocation.
    pub(super) fn skip_idle(&mut self, frames: usize) {
        self.pitch.skip(frames);
    }

    /// Grows the per-block scratch outside the callback.  The size it reaches
    /// is the prepared size: every block the host hands over has to fit it
    /// (see [`Self::reserve_block`]).
    pub(super) fn prepare(&mut self, frames: usize) {
        self.reserve_block(frames);
        if self.plugin_l.len() < frames {
            self.plugin_l.resize(frames, 0.0);
            self.plugin_r.resize(frames, 0.0);
        }
    }

    /// Makes room for one block's taps, key line, gains and model scratch.  The
    /// scratch is sized before the stream opens, for the largest block the host
    /// can deliver (its announced buffer, or a generous constant when it leaves
    /// the size to its default: see `OutputMixer::prepare`), so the callback
    /// never allocates.  `OutputMixer::begin_block` asserts that invariant.  A
    /// host that hands over a larger block allocates here, once: release keeps
    /// that block playing rather than dropping it, but a block past the
    /// prepared size is a host bug, and the debug build says so.  The
    /// `plugin_l`/`plugin_r` buffers are grown by [`Self::prepare`] and
    /// `run_effects`, not here.
    pub(super) fn reserve_block(&mut self, frames: usize) {
        if self.block.len() < frames {
            self.block.resize(frames, [0.0; 4]);
        }
        let key_capacity = MAX_ALIGN as usize + frames;
        if self.key_line.max_delay() < key_capacity {
            // A larger block than before: the key's history starts over.
            self.key_line = MonoDelayLine::new(key_capacity);
        }
        if self.gains.len() < frames {
            self.gains.resize(frames, 0.0);
        }
        self.nam.reserve(frames);
        self.cab.reserve(frames);
    }

    /// Forgets which settings the EQs, gate and pitch shifter were built
    /// from, so the next block redesigns them (for a new rate: that is how a
    /// rate change reaches the shifter).  Version 0 is never published.
    pub(super) fn invalidate_settings(&mut self) {
        self.eq_version = [0; 2];
        self.gate_version = 0;
        self.pitch_version = 0;
        self.pitch_applied = None;
        self.nam.invalidate();
        self.cab.invalidate();
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
        // Applies the UI's chain edits first, so the latency read below is
        // the chain this block runs.
        let unopened = controls.begin_chain(&mut self.chain);
        self.nam.begin(controls, rate, ramp_samples);
        // Whether this block is a return comes before the latency: the pitch
        // shifter never runs on one.  A change of feed is taken only once
        // the path gain has reached 0.
        let adopted = self.controls_adopted;
        let wanted = controls.is_return();
        if !adopted || self.gain.is_settled_at(0.0) {
            self.is_return = wanted;
        }
        let switching = wanted != self.is_return;
        // Before the latency read, so a switch the shifter lands at this
        // block boundary is counted in this block.
        self.follow_pitch(controls, rate);
        self.pitch.begin_block();
        let pitch_latency = if self.is_return {
            0
        } else {
            self.pitch.latency_samples()
        };
        self.pitch_latency = pitch_latency.min(MAX_ALIGN);
        // Plugin, NAM and pitch latency add: an oversampled block and the
        // shifter really delay their output, so this path does not delay
        // it again.
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
        // A chain whose plugins are still being opened (a path just added,
        // duplicated or restored) stays silent rather than playing dry.  A
        // return is solo-safe.
        let soloed_out = any_solo && !controls.soloed() && !self.is_return;
        let silent = self.fading || controls.muted() || soloed_out || unopened || switching;
        // Level and match are one ramped gain, so a match trim never steps.
        // Sends read this gain, so a return follows its senders' matches.
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

    /// The whole path's latency this block, clamped to [`MAX_ALIGN`]: the
    /// plugin chain, the oversampled NAM blocks and the pitch shifter.  The
    /// sends use it to line a return up with its senders.
    pub(super) fn latency(&self) -> u32 {
        self.latency
    }

    /// Advances the path gain over the block into [`Self::gains`].
    pub(super) fn fill_gains(&mut self, frames: usize) {
        let ramp = &mut self.gain;
        for gain in &mut self.gains[..frames] {
            *gain = ramp.next();
        }
    }

    /// Points the routing, enable and mono ramps at this block's settings.
    /// A return's sends arrive as its L and R: its routing is ignored.
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

    /// Picks up pitch changes made in the UI since the last block, and gives
    /// the shifter what it runs: the settings, off on a return, at `rate`.
    /// A busy lock keeps the last settings.  Becoming a return turns the
    /// shifter off at once (the path is silent then).
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

    /// Holds one input frame in `block` for [`Self::run_front`].
    #[inline]
    pub(super) fn stage_input(&mut self, index: usize, left: f32, right: f32) {
        self.block[index] = [left, right, 0.0, 0.0];
    }

    /// The front of the chain over a block of staged input frames, in
    /// place: routing, channel enable and mono (with their ramps), the pitch
    /// shifter, then the pre EQ and a PRE gate.  The result waits in `block`
    /// for the plugin effects.  Each path's state only moves with its own
    /// frames, so running it per block instead of per frame gives the same
    /// samples, and the mixer can time each path as one piece.
    pub(super) fn run_front(&mut self, frames: usize) {
        let keyed = self.plan.gate_keyed;
        if !keyed {
            self.key_line.reset();
        }
        for index in 0..frames {
            let [left, right, _, _] = self.block[index];
            // Routing, channel enable and mono move with the ramps, before
            // the shifter, the EQ and the effects, which is where those
            // controls act.
            let out = self.ramps.next_input(left, right);
            if keyed {
                // The key is the dry input, before the shifter.
                self.key_line.push(out[0].abs().max(out[1].abs()));
            }
            self.block[index] = out;
        }
        // The chain on the path's mix (taps 1/2), in order: pitch shifter,
        // pre EQ, optional PRE gate, the NAM blocks that sit before the
        // plugin effects, plugin effects, the NAM blocks after them, the
        // cabinet, optional POST gate, post EQ.  The EQ and the gate are
        // per-sample; the cabinet is a short direct head then partitioned
        // overlap-save.  Neither adds latency.  The shifter and an
        // oversampled NAM block do; their delay was read at the start of
        // the block.  The shifter comes first so the pre EQ shapes the
        // shifted signal.  Off and settled it leaves the block untouched;
        // it never runs on a return.
        if !self.is_return {
            self.pitch.process(&mut self.block, frames);
        } else {
            // A return never runs the shifter, but its schedule moves on
            // with the paths that do, so the slot keeps its hop phase if it
            // becomes an input path again.
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

    /// Adds the time from `since` to `now` to this block's DSP time and
    /// returns `now`, the start of whatever is timed next.
    #[inline]
    pub(super) fn add_dsp_time(&mut self, since: Instant, now: Instant) -> Instant {
        self.dsp_block_ns += now.saturating_duration_since(since).as_nanos() as u64;
        now
    }

    /// Smooths this block's DSP time into the published one, folds it into
    /// the decaying peak ([`DSP_PEAK_SECONDS`]) and stores both with the
    /// block's length (`frames` at `rate`), the path's budget.  Returns the
    /// block's measured time, ns, for the mixer's total.  Atomics only.
    pub(super) fn publish_dsp(&mut self, controls: &PathControls, frames: usize, rate: u32) -> u64 {
        let measured = self.dsp_block_ns;
        self.dsp_block_ns = 0;
        controls.store_dsp_time(self.dsp.push(measured, frames, rate));
        measured
    }

    /// Runs the plugin effects over a block that `run_front` filled, in
    /// chunks of at most [`CHAIN_CHUNK`] frames.
    ///
    /// The chain never just drops out.  No edit holds a lock the chain
    /// needs (see the `audio::path::plugins` docs), so a miss is rare: the
    /// UI holding the parked runtime for a moment, or another mixer still
    /// holding it.  The chain's input also goes into `chain_line`; a chunk
    /// the chain cannot run plays that input delayed by the plugin chain's
    /// own latency instead (not the path's: the NAM blocks before the chain
    /// already delayed that input).  That
    /// keeps the path's timing in the sum (no comb, no phase jump), but not
    /// its sound: for the missed chunk the path plays dry (its effects'
    /// level and tone are gone).  The switch into the fallback cannot
    /// crossfade (the chain's output for that chunk does not exist), so its
    /// step is smoothed over [`DECLICK_SECONDS`] instead.  Once the chain
    /// runs again its output fades back in over [`CHAIN_RESUME_SECONDS`],
    /// after one latency's worth of frames (`chain_hold`): the effects
    /// missed that input, so their own delay lines still hold the gap.  A
    /// chain with no active effect but some latency (all bypassed) still
    /// runs: its skipped effects delay by their latency.
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
            // Into the fallback from (some of) the chain's own output.
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

    /// The rest of the path after the effects: the NAM blocks that sit after
    /// them, the cabinet, the POST gate and the post EQ, in place, then
    /// the path meters and the gate's reduction.  The cabinet adds no
    /// latency.  A POST gate keyed on the input reads the key a chain
    /// latency back (the pitch shifter's included), the delay the effects'
    /// output has too.  [`Self::run_front`] already put the block's key in
    /// the line.
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

/// Which built-in stages run in one block, decided once at its start.
#[derive(Clone, Copy, Default)]
struct BlockPlan {
    pre_eq: bool,
    post_eq: bool,
    gate_pre: bool,
    gate_post: bool,
    /// The gate listens to the path's dry input.
    gate_keyed: bool,
}
