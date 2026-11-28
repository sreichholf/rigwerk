//! Causal, no lookahead: the first sample of a transient leaks at the
//! closed gain.  Physics, not a bug.  Pre and post are the same DSP.
//!
//! [`GateKey::Input`] is the dry input, delayed by the chain latency so it
//! lines up with the cut.  After a high-gain amp the processed floor would
//! hold a signal-keyed gate open.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum GateSlot {
    /// Into amp sims: single-coil and interface hiss.
    Pre,
    /// After the effects: idle roar and gain-stage hiss.
    #[default]
    Post,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum GateKey {
    #[default]
    Signal,
    Input,
}

/// Clamps for the UI and [`GateSettings::sanitized`].
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

/// Detector times.  Fast attack so peaks register at once.  The release
/// stays up between cycles of a sustained note, or the Schmitt chatters.
const DETECTOR_ATTACK_MS: f32 = 0.5;
const DETECTOR_RELEASE_MS: f32 = 20.0;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GateSettings {
    pub enabled: bool,
    pub slot: GateSlot,
    pub key: GateKey,
    pub threshold_db: f32,
    /// 0 dB passes; -80 dB is near mute.
    pub range_db: f32,
    /// Close sits this far below the open threshold, dB.
    pub hysteresis_db: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
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

/// Open, close, and floor as linear gain.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Levels {
    open: f32,
    close: f32,
    floor: f32,
}

impl Levels {
    fn from_settings(settings: &GateSettings) -> Self {
        let open = db_to_amp(settings.threshold_db);
        // `.min(open)` so a huge hysteresis cannot invert the pair.
        let close = db_to_amp(settings.threshold_db - settings.hysteresis_db).min(open);
        let floor = db_to_amp(settings.range_db).clamp(0.0, 1.0);
        Self { open, close, floor }
    }
}

#[inline]
fn db_to_amp(db: f32) -> f32 {
    10.0f32.powf(db / 20.0)
}

/// `exp(-1 / (τ · fs))`.  Knobs are that time constant, not a 10-90% rise.
#[inline]
fn pole_coeff(seconds: f32, sample_rate: u32) -> f32 {
    let samples = (seconds * sample_rate as f32).max(1.0e-6);
    (-1.0 / samples).exp()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GatePhase {
    Closed,
    Open,
    /// Still targets unity while the hold counts down.
    Holding,
}

pub(crate) struct NoiseGate {
    settings: GateSettings,
    sample_rate: u32,
    levels: Levels,
    phase: GatePhase,
    envelope: f32,
    gain: f32,
    /// Deepest `gain` since the last [`Self::take_reduction_db`].
    lowest: f32,
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

    /// Keeps envelope and gain, so a live tweak does not click.
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
            // Start closed at the floor, so the first note must open the
            // gate instead of passing at unity.
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

    #[cfg(test)]
    pub(crate) fn gain(&self) -> f32 {
        self.gain
    }

    /// Once per block.  No allocation.
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

    #[inline]
    pub(crate) fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        self.process_keyed(left, right, left.abs().max(right.abs()))
    }

    /// `key` is a peak (`max(|L|, |R|)`), not a sample.
    #[inline]
    pub(crate) fn process_keyed(&mut self, left: f32, right: f32, key: f32) -> (f32, f32) {
        if !self.active {
            return (left, right);
        }
        let peak = if key.is_finite() { key.abs() } else { 0.0 };
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
