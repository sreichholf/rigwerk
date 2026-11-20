//! Built-in noise gate / downward expander, one per signal path.
//!
//! Causal and feed-forward: each sample's gain depends only on the current
//! and past samples.  That means zero added buffering latency, and it also
//! means the very first moment of a transient can leak at the closed gain
//! before the detector opens — that is the physics of a look-ahead-free gate,
//! not a bug.
//!
//! Per sample, linked stereo:
//! 1. Peak detector on `max(|L|, |R|)` with its own attack/release ballistics.
//! 2. Schmitt trigger (open / close thresholds) plus a hold timer.
//! 3. One-pole smooth of the gain toward 1 (open/hold) or the range floor
//!    (closed), using the gate's attack when rising and release when falling.
//!
//! The PRE/POST slot is where this stage sits relative to the plugin effects;
//! the DSP itself is the same in either place.
//!
//! The key is what the detector hears.  [`GateKey::Signal`] is the signal
//! being gated.  [`GateKey::Input`] is the path's dry input, before the
//! chain: after a high-gain amp the noise floor is loud enough to hold a
//! gate open, while the guitar's own level still says when a note ends.
//! The mixer hands that key in delayed by the chain latency, so it lines up
//! with what the gate cuts ([`NoiseGate::process_keyed`]).

use serde::{Deserialize, Serialize};

/// Where the gate sits relative to the plugin effects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum GateSlot {
    /// Before effects: gates single-coil / interface hiss into amp sims.
    Pre,
    /// After effects: gates idle roar / gain-stage hiss.  The default.
    #[default]
    Post,
}

/// What the gate's detector listens to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum GateKey {
    /// The signal being gated.  The default.
    #[default]
    Signal,
    /// The path's dry input, before the chain, lined up with the chain's
    /// latency.
    Input,
}

/// Soft limits for the UI and for [`GateSettings::sanitized`].
pub const MIN_THRESHOLD_DB: f32 = -80.0;
pub const MAX_THRESHOLD_DB: f32 = -10.0;
pub const MIN_RANGE_DB: f32 = -80.0;
pub const MAX_RANGE_DB: f32 = 0.0;
pub const MIN_HYSTERESIS_DB: f32 = 0.0;
pub const MAX_HYSTERESIS_DB: f32 = 24.0;
pub const MIN_ATTACK_MS: f32 = 0.1;
pub const MAX_ATTACK_MS: f32 = 100.0;
pub const MIN_RELEASE_MS: f32 = 1.0;
pub const MAX_RELEASE_MS: f32 = 2_000.0;
pub const MIN_HOLD_MS: f32 = 0.0;
pub const MAX_HOLD_MS: f32 = 1_000.0;

/// Detector envelope times.  Fast attack so peaks register immediately; a
/// short release keeps the envelope from collapsing between waveform cycles
/// of a sustained note (which would chatter the Schmitt trigger).
const DETECTOR_ATTACK_MS: f32 = 0.5;
const DETECTOR_RELEASE_MS: f32 = 20.0;

/// Settings the UI and persistence share.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GateSettings {
    pub enabled: bool,
    pub slot: GateSlot,
    /// What the detector listens to.
    pub key: GateKey,
    /// Open threshold in dBFS.  Signal above this opens a closed gate.
    pub threshold_db: f32,
    /// Gain when fully closed, in dB (0 = pass-through, −80 ≈ mute).
    pub range_db: f32,
    /// How far below the open threshold the close threshold sits, in dB.
    pub hysteresis_db: f32,
    /// Gain-smoothing time constant while opening, milliseconds.
    pub attack_ms: f32,
    /// Gain-smoothing time constant while closing, milliseconds.
    pub release_ms: f32,
    /// Time the gate stays open after the detector drops below the close
    /// threshold, before release begins.  Milliseconds.
    pub hold_ms: f32,
}

impl Default for GateSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            slot: GateSlot::Post,
            key: GateKey::Signal,
            threshold_db: -48.0,
            range_db: -60.0,
            hysteresis_db: 6.0,
            attack_ms: 1.2,
            release_ms: 120.0,
            hold_ms: 40.0,
        }
    }
}

impl GateSettings {
    pub fn sanitized(self) -> Self {
        let defaults = Self::default();
        Self {
            enabled: self.enabled,
            slot: self.slot,
            key: self.key,
            threshold_db: finite_or(self.threshold_db, defaults.threshold_db)
                .clamp(MIN_THRESHOLD_DB, MAX_THRESHOLD_DB),
            range_db: finite_or(self.range_db, defaults.range_db).clamp(MIN_RANGE_DB, MAX_RANGE_DB),
            hysteresis_db: finite_or(self.hysteresis_db, defaults.hysteresis_db)
                .clamp(MIN_HYSTERESIS_DB, MAX_HYSTERESIS_DB),
            attack_ms: finite_or(self.attack_ms, defaults.attack_ms)
                .clamp(MIN_ATTACK_MS, MAX_ATTACK_MS),
            release_ms: finite_or(self.release_ms, defaults.release_ms)
                .clamp(MIN_RELEASE_MS, MAX_RELEASE_MS),
            hold_ms: finite_or(self.hold_ms, defaults.hold_ms).clamp(MIN_HOLD_MS, MAX_HOLD_MS),
        }
    }

    pub fn is_active(self) -> bool {
        self.enabled
    }
}

fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        fallback
    }
}

/// Open / close thresholds as linear amplitudes, plus the closed-gain floor.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Levels {
    open: f32,
    close: f32,
    floor: f32,
}

impl Levels {
    fn from_settings(settings: &GateSettings) -> Self {
        let open = db_to_amp(settings.threshold_db);
        // Close threshold is hysteresis_db below the open threshold.  Clamp
        // so a huge hysteresis cannot invert the Schmitt pair.
        let close = db_to_amp(settings.threshold_db - settings.hysteresis_db).min(open);
        let floor = db_to_amp(settings.range_db).clamp(0.0, 1.0);
        Self { open, close, floor }
    }
}

#[inline]
fn db_to_amp(db: f32) -> f32 {
    10.0f32.powf(db / 20.0)
}

/// One-pole coefficient for a time constant of `seconds` at `sample_rate`.
///
/// `y += (target - y) * (1 - coeff)` with `coeff = exp(-1 / (τ · fs))` moves
/// ~63 % of the remaining gap in one time constant.  Attack/release knobs are
/// those time constants, not 10–90 % rise times (~2.2 τ).
#[inline]
fn pole_coeff(seconds: f32, sample_rate: u32) -> f32 {
    let samples = (seconds * sample_rate as f32).max(1.0e-6);
    (-1.0 / samples).exp()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GatePhase {
    /// Gain targets the range floor.
    Closed,
    /// Gain targets unity.
    Open,
    /// Still targets unity; hold timer is counting down toward Closed.
    Holding,
}

/// Running gate: detector, Schmitt state, hold counter, smoothed gain.
pub(crate) struct NoiseGate {
    settings: GateSettings,
    sample_rate: u32,
    levels: Levels,
    phase: GatePhase,
    /// Peak envelope of `max(|L|, |R|)`.
    envelope: f32,
    /// Smoothed gain applied to the audio (linear).
    gain: f32,
    /// The lowest `gain` since the last [`Self::take_reduction_db`].
    lowest: f32,
    /// Samples left in [`GatePhase::Holding`].
    hold_left: u32,
    hold_samples: u32,
    attack_coeff: f32,
    release_coeff: f32,
    detector_attack: f32,
    detector_release: f32,
    active: bool,
}

impl NoiseGate {
    pub(crate) fn new(sample_rate: u32) -> Self {
        let mut gate = Self {
            settings: GateSettings::default(),
            sample_rate: sample_rate.max(1),
            levels: Levels::from_settings(&GateSettings::default()),
            phase: GatePhase::Closed,
            envelope: 0.0,
            gain: 1.0,
            lowest: 1.0,
            hold_left: 0,
            hold_samples: 0,
            attack_coeff: 0.0,
            release_coeff: 0.0,
            detector_attack: 0.0,
            detector_release: 0.0,
            active: false,
        };
        gate.update(GateSettings::default(), sample_rate);
        gate
    }

    /// Apply new settings or rate.  Envelope and gain are kept so moving a
    /// knob mid-note does not click; a disable snaps gain to unity and clears
    /// the detector so the next enable starts from a known closed state.
    pub(crate) fn update(&mut self, settings: GateSettings, sample_rate: u32) {
        let settings = settings.sanitized();
        let sample_rate = sample_rate.max(1);
        if settings == self.settings
            && sample_rate == self.sample_rate
            && self.active == settings.is_active()
        {
            return;
        }
        let was_active = self.active;
        self.settings = settings;
        self.sample_rate = sample_rate;
        self.active = settings.is_active();
        self.levels = Levels::from_settings(&settings);
        self.hold_samples = ((settings.hold_ms * 0.001) * sample_rate as f32).round() as u32;
        self.attack_coeff = pole_coeff(settings.attack_ms * 0.001, sample_rate);
        self.release_coeff = pole_coeff(settings.release_ms * 0.001, sample_rate);
        self.detector_attack = pole_coeff(DETECTOR_ATTACK_MS * 0.001, sample_rate);
        self.detector_release = pole_coeff(DETECTOR_RELEASE_MS * 0.001, sample_rate);
        if !self.active {
            self.phase = GatePhase::Closed;
            self.envelope = 0.0;
            self.gain = 1.0;
            self.lowest = 1.0;
            self.hold_left = 0;
        } else if !was_active {
            // Fresh enable: start closed at the floor so the first note has to
            // open the gate rather than passing through at unity.
            self.phase = GatePhase::Closed;
            self.envelope = 0.0;
            self.gain = self.levels.floor;
            self.hold_left = 0;
        }
    }

    pub(crate) fn active(&self) -> bool {
        self.active
    }

    pub(crate) fn settings(&self) -> GateSettings {
        self.settings
    }

    /// Linear gain after the last [`Self::process`] (1 = open, floor = closed).
    #[cfg(test)]
    pub(crate) fn gain(&self) -> f32 {
        self.gain
    }

    /// The deepest gain reduction since the last call, in dB (0 when the
    /// gate stayed open, or processed nothing), and starts the next count.
    /// Once per block; no allocation.
    pub(crate) fn take_reduction_db(&mut self) -> f32 {
        let lowest = std::mem::replace(&mut self.lowest, 1.0);
        (-20.0 * lowest.max(1.0e-6).log10()).max(0.0)
    }

    #[cfg(test)]
    pub(crate) fn envelope(&self) -> f32 {
        self.envelope
    }

    #[cfg(test)]
    fn phase(&self) -> GatePhase {
        self.phase
    }

    /// One frame, keyed on itself.
    #[inline]
    pub(crate) fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        self.process_keyed(left, right, left.abs().max(right.abs()))
    }

    /// One frame, with the detector fed `key` (a peak, `max(|L|, |R|)` of
    /// whatever the gate listens to) instead of the frame itself.
    #[inline]
    pub(crate) fn process_keyed(&mut self, left: f32, right: f32, key: f32) -> (f32, f32) {
        if !self.active {
            return (left, right);
        }
        let peak = if key.is_finite() { key.abs() } else { 0.0 };
        // Peak ballistics: rise fast toward peaks, fall slowly so a sustained
        // note's zero crossings do not collapse the envelope.
        if peak > self.envelope {
            self.envelope = peak + (self.envelope - peak) * self.detector_attack;
        } else {
            self.envelope = peak + (self.envelope - peak) * self.detector_release;
        }
        self.follow_phase();
        let target = match self.phase {
            GatePhase::Closed => self.levels.floor,
            GatePhase::Open | GatePhase::Holding => 1.0,
        };
        // Attack while opening (gain rising), release while closing.
        let coeff = if target > self.gain {
            self.attack_coeff
        } else {
            self.release_coeff
        };
        self.gain = target + (self.gain - target) * coeff;
        self.lowest = self.lowest.min(self.gain);
        (left * self.gain, right * self.gain)
    }

    #[inline]
    fn follow_phase(&mut self) {
        let env = self.envelope;
        match self.phase {
            GatePhase::Closed => {
                if env >= self.levels.open {
                    self.phase = GatePhase::Open;
                    self.hold_left = 0;
                }
            }
            GatePhase::Open => {
                if env < self.levels.close {
                    if self.hold_samples == 0 {
                        self.phase = GatePhase::Closed;
                    } else {
                        self.phase = GatePhase::Holding;
                        self.hold_left = self.hold_samples;
                    }
                }
            }
            GatePhase::Holding => {
                if env >= self.levels.close {
                    // Signal returned during hold — stay open.
                    self.phase = GatePhase::Open;
                    self.hold_left = 0;
                } else if self.hold_left > 0 {
                    self.hold_left -= 1;
                    if self.hold_left == 0 {
                        self.phase = GatePhase::Closed;
                    }
                } else {
                    self.phase = GatePhase::Closed;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
