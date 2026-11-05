//! Built-in noise gate / downward expander on the master mix.
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
//! The PRE/POST slot is where this stage sits relative to the plugin inserts;
//! the DSP itself is the same in either place.

use serde::{Deserialize, Serialize};

/// Where the gate sits relative to the plugin inserts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum GateSlot {
    /// Before inserts: gates single-coil / interface hiss into amp sims.
    Pre,
    /// After inserts: gates idle roar / gain-stage hiss.  The default.
    #[default]
    Post,
}

impl GateSlot {
    pub const ALL: [GateSlot; 2] = [GateSlot::Pre, GateSlot::Post];
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
        if !self.active {
            return (left, right);
        }
        let peak = left.abs().max(right.abs());
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
mod tests {
    use super::{
        db_to_amp, pole_coeff, GatePhase, GateSettings, GateSlot, NoiseGate, DETECTOR_ATTACK_MS,
        DETECTOR_RELEASE_MS,
    };

    const RATE: u32 = 48_000;

    fn loud_gate() -> NoiseGate {
        let mut gate = NoiseGate::new(RATE);
        gate.update(
            GateSettings {
                enabled: true,
                threshold_db: -40.0,
                range_db: -60.0,
                hysteresis_db: 6.0,
                attack_ms: 1.0,
                release_ms: 50.0,
                hold_ms: 20.0,
                slot: GateSlot::Post,
            },
            RATE,
        );
        gate
    }

    fn run(gate: &mut NoiseGate, amp: f32, samples: usize) {
        for _ in 0..samples {
            let _ = gate.process(amp, amp);
        }
    }

    #[test]
    fn db_conversion_matches_amplitude() {
        assert!((db_to_amp(0.0) - 1.0).abs() < 1.0e-6);
        assert!((db_to_amp(-20.0) - 0.1).abs() < 1.0e-6);
        assert!((db_to_amp(-40.0) - 0.01).abs() < 1.0e-6);
        assert!((db_to_amp(-60.0) - 0.001).abs() < 1.0e-6);
    }

    #[test]
    fn pole_coeff_is_one_at_zero_time_and_decays() {
        // One sample time constant → e^{-1}.
        let c = pole_coeff(1.0 / RATE as f32, RATE);
        assert!((c - (-1.0f32).exp()).abs() < 1.0e-5);
        // Longer τ → closer to 1 (slower).
        assert!(pole_coeff(0.1, RATE) > pole_coeff(0.01, RATE));
    }

    #[test]
    fn disabled_gate_is_unity() {
        let mut gate = NoiseGate::new(RATE);
        let (l, r) = gate.process(0.5, -0.25);
        assert_eq!((l, r), (0.5, -0.25));
        assert!(!gate.active());
    }

    #[test]
    fn silence_stays_at_the_range_floor() {
        let mut gate = loud_gate();
        run(&mut gate, 0.0, RATE as usize / 10);
        assert_eq!(gate.phase(), GatePhase::Closed);
        let floor = db_to_amp(-60.0);
        assert!(
            (gate.gain() - floor).abs() < 1.0e-4,
            "gain {} vs floor {}",
            gate.gain(),
            floor
        );
        let (l, _) = gate.process(0.0, 0.0);
        assert!(l.abs() < 1.0e-9);
    }

    #[test]
    fn a_loud_signal_opens_toward_unity() {
        let mut gate = loud_gate();
        // Well above −40 dBFS open threshold (0.01).
        run(&mut gate, 0.25, RATE as usize / 50);
        assert_eq!(gate.phase(), GatePhase::Open);
        assert!(gate.gain() > 0.95, "gain {}", gate.gain());
        let (l, r) = gate.process(0.25, 0.25);
        assert!((l - 0.25 * gate.gain()).abs() < 1.0e-6);
        assert!((r - 0.25 * gate.gain()).abs() < 1.0e-6);
    }

    #[test]
    fn hysteresis_keeps_a_closed_gate_closed_between_thresholds() {
        let mut gate = loud_gate();
        // Open at −40 dB (0.01), close at −46 dB (~0.005).  Midway ≈ 0.007.
        let mid = db_to_amp(-43.0);
        run(&mut gate, mid, RATE as usize / 10);
        assert_eq!(gate.phase(), GatePhase::Closed);
        assert!(gate.gain() < 0.05);
    }

    #[test]
    fn hysteresis_keeps_an_open_gate_open_between_thresholds() {
        let mut gate = loud_gate();
        run(&mut gate, 0.25, RATE as usize / 50);
        assert_eq!(gate.phase(), GatePhase::Open);
        let mid = db_to_amp(-43.0);
        // Stay in the hysteresis band long enough that a no-hysteresis gate
        // would have closed; ours must remain Open (or Holding only after
        // dropping below close — mid is above close).
        run(&mut gate, mid, RATE as usize / 10);
        assert_eq!(gate.phase(), GatePhase::Open);
        assert!(gate.gain() > 0.9);
    }

    /// Silence long enough for the detector envelope to fall below the close
    /// threshold (−46 dB from a 0.25 peak with 20 ms detector release).
    fn wait_until_holding(gate: &mut NoiseGate) {
        // ~100 ms of silence is several detector time constants below close.
        for _ in 0..(RATE as usize / 10) {
            let _ = gate.process(0.0, 0.0);
            if gate.phase() == GatePhase::Holding || gate.phase() == GatePhase::Closed {
                break;
            }
        }
    }

    #[test]
    fn hold_delays_the_close_after_the_signal_drops() {
        let mut gate = loud_gate();
        run(&mut gate, 0.25, RATE as usize / 50);
        assert_eq!(gate.phase(), GatePhase::Open);
        wait_until_holding(&mut gate);
        assert_eq!(gate.phase(), GatePhase::Holding);
        // Hold is 20 ms (= 960 samples). Finish that, then a few more.
        run(&mut gate, 0.0, 1_200);
        assert_eq!(gate.phase(), GatePhase::Closed);
    }

    #[test]
    fn signal_returning_during_hold_reopens() {
        let mut gate = loud_gate();
        run(&mut gate, 0.25, RATE as usize / 50);
        wait_until_holding(&mut gate);
        assert_eq!(gate.phase(), GatePhase::Holding);
        run(&mut gate, 0.25, 50);
        assert_eq!(gate.phase(), GatePhase::Open);
    }

    #[test]
    fn gain_release_follows_the_time_constant_once_closed() {
        let mut gate = NoiseGate::new(RATE);
        gate.update(
            GateSettings {
                enabled: true,
                hold_ms: 0.0,
                release_ms: 50.0,
                attack_ms: 1.0,
                threshold_db: -40.0,
                range_db: -60.0,
                hysteresis_db: 6.0,
                slot: GateSlot::Post,
            },
            RATE,
        );
        run(&mut gate, 0.5, RATE as usize / 20);
        assert_eq!(gate.phase(), GatePhase::Open);
        let open_gain = gate.gain();
        assert!(open_gain > 0.95);

        // Wait until the detector has fallen and the phase is Closed, then
        // measure one release τ of gain motion from a known starting gain.
        wait_until_holding(&mut gate);
        // hold_ms is 0, so Closing is immediate once the envelope is low.
        assert_eq!(gate.phase(), GatePhase::Closed);
        // Snap the smoothed gain back to the open value so the release τ is
        // measured cleanly (envelope already below close).
        gate.gain = open_gain;
        let release_samples = (0.050 * RATE as f32).round() as usize;
        run(&mut gate, 0.0, release_samples);
        let after_release = gate.gain();
        let floor = db_to_amp(-60.0);
        let expected = floor + (open_gain - floor) / std::f32::consts::E;
        assert!(
            (after_release - expected).abs() < 0.05,
            "release {after_release} vs ~{expected}"
        );
    }

    #[test]
    fn attack_rises_most_of_the_way_in_one_time_constant() {
        let mut gate = loud_gate();
        let attack_samples = (0.001 * RATE as f32).round() as usize;
        run(&mut gate, 0.5, attack_samples);
        // From the floor (~0.001) toward 1, one τ ≈ 63% of the gap → ~0.63.
        assert!(
            gate.gain() > 0.55,
            "after one attack τ gain should be well up, got {}",
            gate.gain()
        );
    }

    #[test]
    fn stereo_link_uses_the_louder_channel() {
        let mut gate = loud_gate();
        // Only the right channel is loud.
        for _ in 0..RATE as usize / 50 {
            let _ = gate.process(0.0, 0.25);
        }
        assert_eq!(gate.phase(), GatePhase::Open);
        assert!(gate.envelope() > db_to_amp(-40.0));
    }

    #[test]
    fn default_slot_is_post() {
        assert_eq!(GateSettings::default().slot, GateSlot::Post);
    }

    #[test]
    fn sanitized_clamps_out_of_range_knobs() {
        let s = GateSettings {
            threshold_db: -200.0,
            range_db: 10.0,
            hysteresis_db: 100.0,
            attack_ms: 0.0,
            release_ms: 9_999.0,
            hold_ms: -5.0,
            ..GateSettings::default()
        }
        .sanitized();
        assert_eq!(s.threshold_db, super::MIN_THRESHOLD_DB);
        assert_eq!(s.range_db, super::MAX_RANGE_DB);
        assert_eq!(s.hysteresis_db, super::MAX_HYSTERESIS_DB);
        assert_eq!(s.attack_ms, super::MIN_ATTACK_MS);
        assert_eq!(s.release_ms, super::MAX_RELEASE_MS);
        assert_eq!(s.hold_ms, super::MIN_HOLD_MS);
    }

    #[test]
    fn sanitized_replaces_non_finite_knobs() {
        let defaults = GateSettings::default();
        let s = GateSettings {
            threshold_db: f32::NAN,
            range_db: f32::INFINITY,
            hysteresis_db: f32::NEG_INFINITY,
            attack_ms: f32::NAN,
            release_ms: f32::INFINITY,
            hold_ms: f32::NAN,
            ..defaults
        }
        .sanitized();
        assert_eq!(s.threshold_db, defaults.threshold_db);
        assert_eq!(s.range_db, defaults.range_db);
        assert_eq!(s.hysteresis_db, defaults.hysteresis_db);
        assert_eq!(s.attack_ms, defaults.attack_ms);
        assert_eq!(s.release_ms, defaults.release_ms);
        assert_eq!(s.hold_ms, defaults.hold_ms);
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn detector_constants_are_sensible() {
        assert!(DETECTOR_ATTACK_MS < DETECTOR_RELEASE_MS);
        assert!(DETECTOR_ATTACK_MS <= 1.0);
    }
}
