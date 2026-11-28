//! Mono in, mono out: `(L + R) * 0.5`, written to both channels.  One
//! channel routed loses 6 dB in that sum.
//!
//! The DSP is the delay-line shifter in `tight.rs`.  Reported latency is
//! the constant `D` while on, bypassed too.  Period estimates are offset
//! by `slot · H / MAX_PATHS`, so the paths do not estimate in one block.
//!
//! [`PitchShifter::new`] allocates for 192 kHz.  `update`, `begin_block`,
//! and `process` do not: a rate or delay change fades out, recomputes
//! inside that storage, and fades in.
//!
//! Switching on is a pre-warmed dip that lands at a block boundary.
//! The reported latency changes on that block, or on a delay change.
//! A non-finite value clears the state.

use serde::{Deserialize, Serialize};

use super::mixer::ramp::Ramp;
use super::path::MAX_PATHS;

mod tight;

use tight::TightState;

pub const PITCH_MIN_SEMITONES: i8 = -12;
pub const PITCH_MAX_SEMITONES: i8 = 12;
pub const PITCH_MIN_CENTS: f32 = -50.0;
pub const PITCH_MAX_CENTS: f32 = 50.0;
pub const PITCH_MIN_DELAY_MS: f32 = 4.0;
pub const PITCH_MAX_DELAY_MS: f32 = 40.0;

/// An old save without a delay keeps the delay it played at.
const DEFAULT_DELAY_MS: f32 = 4.0;

const SWITCH_FADE_SECONDS: f32 = 0.008;
/// Between the shifted signal and the delayed dry signal.
const BYPASS_FADE_SECONDS: f32 = 0.015;
const WET_RAMP_SECONDS: f32 = 0.006;

pub(crate) fn pitch_latency_samples(rate: u32, delay_ms: f32) -> u32 {
    tight::spec(rate, delay_ms).latency
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PitchSettings {
    /// Off is out of the chain: no processing, no latency.
    pub enabled: bool,
    /// Delayed dry, still at the reported latency.
    pub bypass: bool,
    pub semitones: i8,
    pub cents: f32,
    /// The rest is dry at the same delay.
    pub wet: f32,
    /// Whole ms at every rate.  Longer splices less often, and hears
    /// every attack that much later.
    pub delay_ms: f32,
}

/// A derived default would give `wet` 0.
impl Default for PitchSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            bypass: false,
            semitones: 0,
            cents: 0.0,
            wet: 1.0,
            delay_ms: DEFAULT_DELAY_MS,
        }
    }
}

impl PitchSettings {
    pub(crate) fn sanitized(self) -> Self {
        let defaults = Self::default();
        Self {
            enabled: self.enabled,
            bypass: self.bypass,
            semitones: self
                .semitones
                .clamp(PITCH_MIN_SEMITONES, PITCH_MAX_SEMITONES),
            cents: finite_or(self.cents, defaults.cents).clamp(PITCH_MIN_CENTS, PITCH_MAX_CENTS),
            wet: finite_or(self.wet, defaults.wet).clamp(0.0, 1.0),
            delay_ms: finite_or(self.delay_ms, defaults.delay_ms)
                .round()
                .clamp(PITCH_MIN_DELAY_MS, PITCH_MAX_DELAY_MS),
        }
    }

    /// For `skip_serializing_if`: a path that never touched the stage saves
    /// nothing.
    pub fn is_unset(&self) -> bool {
        *self == Self::default()
    }

    /// The shift as a frequency ratio in octaves (`log2 r`).
    fn octaves(self) -> f32 {
        (self.semitones as f32 + self.cents / 100.0) / 12.0
    }
}

fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        fallback
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Switch {
    Steady,
    /// Feeding the state until it is warm; the dry signal still plays.
    Warming,
    FadingOut,
    /// Faded out; the switch lands at the next block boundary.
    Silent,
    FadingIn,
}

pub(crate) struct PitchShifter {
    tight: TightState,
    rate: u32,
    slot: usize,
    /// Off, the input passes untouched.
    on: bool,
    next: bool,
    switch: Switch,
    /// Samples the state still needs before switching on.
    warm_left: usize,
    fade: Ramp,
    wet: Ramp,
    bypass: Ramp,
    /// The absolute sample count the period estimates are scheduled on.
    count: u64,
    delay_ms: f32,
    /// A new rate and delay that land once what plays has faded out.
    pending: Option<(u32, f32)>,
    /// The first `update` adopts its settings without a dip.
    configured: bool,
}

impl PitchShifter {
    /// Off the callback (from `PathProcessor::new`): preallocates for
    /// 192 kHz whatever `rate` is.  `slot` staggers the period estimates.
    /// Starts off; the first [`Self::update`] adopts its settings directly.
    pub(crate) fn new(rate: u32, slot: usize) -> Self {
        Self {
            tight: TightState::new(rate, DEFAULT_DELAY_MS),
            rate,
            delay_ms: DEFAULT_DELAY_MS,
            slot: slot % MAX_PATHS,
            on: false,
            next: false,
            switch: Switch::Steady,
            warm_left: 0,
            fade: Ramp::new(1.0),
            wet: Ramp::new(1.0),
            bypass: Ramp::new(0.0),
            count: 0,
            pending: None,
            configured: false,
        }
    }

    /// No allocation.  A switch, a new rate, or a new delay fades out
    /// and lands in a later [`Self::begin_block`].  Silent and off, it
    /// lands at once.
    pub(crate) fn update(&mut self, settings: PitchSettings, rate: u32) {
        let settings = settings.sanitized();
        let target = settings.enabled;
        let first = !self.configured;
        self.configured = true;
        let sizes = (rate, settings.delay_ms);
        let changed = sizes != (self.rate, self.delay_ms);
        if changed || self.pending.is_some() {
            self.pending = changed.then_some(sizes);
            if first || !self.is_active() {
                // Lands off; switching on goes through `start_switch`, which
                // clears the line and warms it up.
                self.land_sizes(false);
            } else {
                if self.pending.is_some() {
                    self.next = target;
                    self.warm_left = 0;
                    if !matches!(self.switch, Switch::FadingOut | Switch::Silent) {
                        self.switch = Switch::FadingOut;
                        self.fade.set_target(0.0, self.fade_samples());
                    }
                }
                self.follow_settings(settings, false);
                if self.pending.is_some() {
                    return;
                }
                self.retarget(target);
                return;
            }
        }
        self.follow_settings(settings, first);
        if first {
            self.on = target;
            self.fade.snap(1.0);
            self.switch = Switch::Steady;
            return;
        }
        self.retarget(target);
    }

    /// The shift, wet and bypass of `settings`; `snap` adopts them at once.
    fn follow_settings(&mut self, settings: PitchSettings, snap: bool) {
        self.tight.set_octaves(settings.octaves(), snap);
        let wet = settings.wet;
        let bypass = if settings.bypass { 1.0 } else { 0.0 };
        if snap {
            self.wet.snap(wet);
            self.bypass.snap(bypass);
        } else {
            let rate = self.rate as f32;
            self.wet.set_target(wet, WET_RAMP_SECONDS * rate);
            self.bypass.set_target(bypass, BYPASS_FADE_SECONDS * rate);
        }
    }

    /// Takes the pending rate and delay, `target` playing, faded in when
    /// on.  A new rate clears the state; a new delay at the same rate
    /// keeps the delay line, so it fades in on the input, not on silence.
    fn land_sizes(&mut self, target: bool) {
        let Some((rate, delay_ms)) = self.pending.take() else {
            return;
        };
        let cleared = rate != self.rate;
        if cleared {
            self.tight.configure(rate, delay_ms);
        } else {
            self.tight.set_delay(delay_ms);
        }
        self.rate = rate;
        self.delay_ms = delay_ms;
        if target && cleared {
            // A cleared line would play `D` of silence and then step into
            // the input: it fills in silence first (off, nothing reported),
            // and the switch on lands at a later block boundary, faded in.
            self.on = false;
            self.next = true;
            self.warm_left = self.tight.spec.latency as usize + self.tight.spec.hop;
            self.fade.snap(0.0);
            self.switch = Switch::Warming;
            return;
        }
        self.on = target;
        self.next = target;
        self.warm_left = 0;
        if target {
            self.fade.snap(0.0);
            self.fade.set_target(1.0, self.fade_samples());
            self.switch = Switch::FadingIn;
        } else if self.switch == Switch::Silent {
            // The output was silent: fade the dry signal back in rather
            // than step to it at the block boundary.  (A size change on a
            // stage that is already off leaves its signal untouched.)
            self.fade.snap(0.0);
            self.fade.set_target(1.0, self.fade_samples());
            self.switch = Switch::FadingIn;
        } else {
            self.fade.snap(1.0);
            self.switch = Switch::Steady;
        }
    }

    fn fade_samples(&self) -> f32 {
        SWITCH_FADE_SECONDS * self.rate as f32
    }

    fn retarget(&mut self, target: bool) {
        let fade = self.fade_samples();
        match self.switch {
            Switch::Steady | Switch::FadingIn => {
                if target != self.on {
                    self.start_switch(target);
                }
            }
            Switch::Warming | Switch::FadingOut | Switch::Silent => {
                if target == self.on {
                    // Back to what plays: stop warming and fade in again.
                    self.switch = Switch::FadingIn;
                    self.fade.set_target(1.0, fade);
                } else if target != self.next {
                    self.start_switch(target);
                }
            }
        }
    }

    fn start_switch(&mut self, target: bool) {
        self.next = target;
        if target {
            self.tight.clear();
            self.warm_left = self.tight.spec.warm_up();
            self.switch = Switch::Warming;
            self.fade.set_target(1.0, self.fade_samples());
        } else {
            self.warm_left = 0;
            self.switch = Switch::FadingOut;
            self.fade.set_target(0.0, self.fade_samples());
        }
    }

    /// At each block boundary, before the path reads the latency: lands a
    /// switch that has faded out.  No allocation.
    pub(crate) fn begin_block(&mut self) {
        if self.switch == Switch::Silent && self.pending.is_some() {
            self.land_sizes(self.next);
            return;
        }
        if self.switch == Switch::Silent {
            self.on = self.next;
            self.switch = Switch::FadingIn;
            self.fade.set_target(1.0, self.fade_samples());
        }
    }

    /// `D` at the current rate and delay while on (bypassed too), 0 when
    /// off.  Constant between switches; it changes only on the block a
    /// switch or a delay change lands on.
    pub(crate) fn latency_samples(&self) -> u32 {
        if self.on {
            self.tight.spec.latency
        } else {
            0
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        self.on || self.switch != Switch::Steady
    }

    #[inline]
    fn tick(&mut self, x: f32, count: u64) -> (f32, f32, bool) {
        let hop = self.tight.spec.hop;
        let stagger = (self.slot * hop / MAX_PATHS) as u64;
        let due = (count + 1 + stagger).is_multiple_of(hop as u64);
        let (wet, dry) = self.tight.tick(x, due);
        (wet, dry, std::mem::take(&mut self.tight.faulted))
    }

    /// A newly activated slot, or a path that became a return: off at once,
    /// the state cleared, and the next [`Self::update`] adopts its settings
    /// directly, as the first one does (no dip).  No allocation.
    pub(crate) fn restart(&mut self) {
        self.tight.clear();
        self.tight.faulted = false;
        self.on = false;
        self.next = false;
        self.switch = Switch::Steady;
        self.warm_left = 0;
        self.fade.snap(1.0);
        self.configured = false;
    }

    /// Advances the estimate schedule by a block of a path that is not
    /// live, so a slot that comes back keeps the hop phase of the paths
    /// that kept running.  No allocation.
    pub(crate) fn skip(&mut self, frames: usize) {
        self.count += frames as u64;
    }

    /// One block in place on channels 0 and 1.  Off and settled, it returns
    /// without touching the block.  No allocation.
    pub(crate) fn process(&mut self, block: &mut [[f32; 4]], frames: usize) {
        if !self.is_active() {
            self.count += frames as u64;
            return;
        }
        for frame in &mut block[..frames] {
            let count = self.count;
            self.count += 1;
            let [left, right, ..] = *frame;
            let x = (left + right) * 0.5;
            if self.warming() {
                if self.tick(x, count).2 {
                    self.warm_left = self.tight.spec.warm_up();
                }
                self.warm_left = self.warm_left.saturating_sub(1);
            }
            let gain = self.fade.next();
            if self.on {
                let (wet, dry, faulted) = self.tick(x, count);
                if faulted {
                    self.fade.snap(0.0);
                    if matches!(
                        self.switch,
                        Switch::Steady | Switch::FadingIn | Switch::Warming
                    ) {
                        self.fade.set_target(1.0, self.fade_samples());
                        if self.switch == Switch::Steady {
                            self.switch = Switch::FadingIn;
                        }
                    }
                }
                let mix = self.wet.next() * (1.0 - self.bypass.next());
                let y = (dry + (wet - dry) * mix) * gain;
                frame[0] = y;
                frame[1] = y;
            } else {
                frame[0] = left * gain;
                frame[1] = right * gain;
            }
            self.advance_switch();
        }
    }

    #[inline]
    fn warming(&self) -> bool {
        match self.switch {
            Switch::Warming | Switch::FadingOut | Switch::Silent => self.next && !self.on,
            Switch::Steady | Switch::FadingIn => false,
        }
    }

    #[inline]
    fn advance_switch(&mut self) {
        match self.switch {
            Switch::Warming if self.warm_left == 0 => {
                self.switch = Switch::FadingOut;
                self.fade.set_target(0.0, self.fade_samples());
            }
            Switch::FadingOut if self.fade.is_settled_at(0.0) => self.switch = Switch::Silent,
            Switch::FadingIn if self.fade.is_settled_at(1.0) => self.switch = Switch::Steady,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests;
