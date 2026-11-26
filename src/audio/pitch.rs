//! Built-in pitch shifter, one per signal path, in front of the pre EQ.
//!
//! Two read heads move through a delay line at `2^(semitones/12)` while the
//! write head runs at the sample rate.  Hann windows half a grain apart sum
//! to 1, so the level does not pump.  At unison the heads are parked and the
//! stage is a pure delay of half the grain, sample for sample.
//!
//! The grain is the whole of the quality trade.  A short grain splices more
//! often and is tighter; a long grain is smoother on chords and later.  The
//! delay added is half that grain (8, 16 or 32 ms at any rate), and it is
//! reported with the path latency so the other paths wait for it.  The stage
//! runs inside whatever buffer the bridge already uses: it never asks the
//! device for a longer one.  That is the point of doing this here.  A shifter
//! that can only work on a multi-thousand-sample host buffer delays the
//! whole device by that buffer, tuner and all.
//!
//! Bypass passes the input unchanged and adds no delay, so a path that is
//! not shifting stays as tight as it was.  Turning it on fades to the
//! delayed signal over a few milliseconds and reports the delay at once.
//! While it is on, the dry side of *Wet* is delayed by the same amount, so a
//! partial wet does not comb.
//!
//! Downward shifts are what a drop tuning wants.  The stage sits after
//! routing and before the pre EQ, so the amp hears the new pitch.  It shifts
//! every harmonic with the fundamental, which is the right result for a
//! retune and the wrong one for a formant-corrected voice effect.

use serde::{Deserialize, Serialize};

/// How long a grain is, and therefore how late the stage is.  The delay is
/// half the grain at every rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PitchWindow {
    /// 16 ms grain, 8 ms delay.  Tightest, roughest on chords.
    Tight,
    /// 32 ms grain, 16 ms delay.
    #[default]
    Normal,
    /// 64 ms grain, 32 ms delay.  Smoother chords.
    Smooth,
}

impl PitchWindow {
    /// Grain length, milliseconds.  The delay is half of this.
    pub const fn ms(self) -> f32 {
        match self {
            Self::Tight => 16.0,
            Self::Normal => 32.0,
            Self::Smooth => 64.0,
        }
    }

    fn samples(self, rate: u32) -> f32 {
        (self.ms() * 0.001 * rate.max(1) as f32).clamp(32.0, (LINE - 8) as f32)
    }

    /// Half the grain, in samples at `rate`.
    pub fn latency_samples(self, rate: u32) -> u32 {
        (self.samples(rate) * 0.5).round() as u32
    }
}

/// Soft limits for the UI and for [`PitchSettings::sanitized`].
pub const MIN_SEMITONES: f32 = -12.0;
pub const MAX_SEMITONES: f32 = 12.0;
pub const MIN_CENTS: f32 = -100.0;
pub const MAX_CENTS: f32 = 100.0;

/// Settings the UI and persistence share.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PitchSettings {
    pub enabled: bool,
    /// Interval in semitones, −12 to +12.  Negative is down, the drop-tuning
    /// direction.
    pub semitones: f32,
    /// Fine interval in cents, −100 to +100, on top of [`Self::semitones`].
    pub cents: f32,
    /// How much of the shifted signal replaces the dry one, 0–1.  The dry
    /// side is delayed by the same amount.
    pub wet: f32,
    pub window: PitchWindow,
}

impl Default for PitchSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            semitones: 0.0,
            cents: 0.0,
            wet: 1.0,
            window: PitchWindow::Normal,
        }
    }
}

impl PitchSettings {
    pub fn sanitized(self) -> Self {
        let defaults = Self::default();
        Self {
            enabled: self.enabled,
            semitones: finite_or(self.semitones, defaults.semitones)
                .clamp(MIN_SEMITONES, MAX_SEMITONES),
            cents: finite_or(self.cents, defaults.cents).clamp(MIN_CENTS, MAX_CENTS),
            wet: finite_or(self.wet, defaults.wet).clamp(0.0, 1.0),
            window: self.window,
        }
    }

    /// No shift at all: the stage, when on, is a pure delay.
    pub fn is_unison(self) -> bool {
        self.semitones.abs() < 1.0e-4 && self.cents.abs() < 1.0e-3
    }

    /// Output frequency over input frequency.
    fn ratio(self) -> f32 {
        2.0f32.powf((self.semitones + self.cents / 100.0) / 12.0)
    }

    /// The delay in samples while the stage is on, else 0.
    pub fn latency_samples(self, rate: u32) -> u32 {
        if self.enabled {
            self.window.latency_samples(rate)
        } else {
            0
        }
    }
}

/// `−2 st`, `+2 st`, `0 st`, and `−2 st −15 ct` when the cents are set.
/// The minus is the real minus, as the levels use.
pub fn format_interval(semitones: f32, cents: f32) -> String {
    let text = signed_unit(semitones, "st");
    if cents.abs() < 0.5 {
        text
    } else {
        format!("{text} {}", signed_unit(cents, "ct"))
    }
}

/// Semitones the way the sheet's slider reads them.
pub fn format_semitones(semitones: f32) -> String {
    signed_unit(semitones, "st")
}

/// Cents the way the sheet's slider reads them.
pub fn format_cents(cents: f32) -> String {
    signed_unit(cents, "ct")
}

fn signed_unit(value: f32, unit: &str) -> String {
    let rounded = value.round();
    if rounded == 0.0 {
        format!("0 {unit}")
    } else if rounded > 0.0 {
        format!("+{rounded:.0} {unit}")
    } else {
        format!("−{:.0} {unit}", rounded.abs())
    }
}

fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        fallback
    }
}

/// Delay line length.  A 64 ms grain at 192 kHz is 12 288 samples; the line
/// is the next power of two, with room for the interpolator.
const LINE: usize = 1 << 14;
const MASK: usize = LINE - 1;

/// How fast a control glides to its new value, milliseconds.
const RATIO_MS: f32 = 20.0;
const WET_MS: f32 = 10.0;
const ENGAGE_MS: f32 = 8.0;
const UNITY_MS: f32 = 12.0;
/// How long a grain change takes to crossfade, milliseconds.
const GRAIN_MS: f32 = 20.0;

/// A second grain fading in over the one already running.
struct Pending {
    grain: f32,
    phase: f32,
    blend: f32,
}

/// Running shifter.  The delay line is allocated in [`Self::new`]; the
/// callback only writes and reads it.
pub(crate) struct PitchShifter {
    settings: PitchSettings,
    rate: u32,
    line: [Box<[f32]>; 2],
    /// Index of the sample written last.  Starts at [`MASK`], so the first
    /// write lands at 0.
    write: usize,
    /// Phase of the first head, 0 to 1.  The second head is half a grain
    /// away.  Parked at 0.5, where one window is 1 and the other is 0, so
    /// leaving unison starts from the pure delay.
    phase: f32,
    grain: f32,
    pending: Option<Pending>,
    ratio: f32,
    wet: f32,
    /// 0 passes the input, 1 plays the shifted signal.
    engage: f32,
    /// 1 plays the pure delay.  Used at unison, where two frozen heads
    /// would comb.
    unity: f32,
    ratio_step: f32,
    wet_step: f32,
    engage_step: f32,
    unity_step: f32,
    grain_step: f32,
    /// The first [`Self::update`] snaps.  Later ones glide.
    ready: bool,
}

impl PitchShifter {
    pub(crate) fn new(rate: u32) -> Self {
        Self {
            settings: PitchSettings::default(),
            rate: rate.max(1),
            line: [zero_line(), zero_line()],
            write: MASK,
            phase: 0.5,
            grain: PitchWindow::Normal.samples(rate.max(1)),
            pending: None,
            ratio: 1.0,
            wet: 1.0,
            engage: 0.0,
            unity: 1.0,
            ratio_step: approach_step(RATIO_MS, rate.max(1)),
            wet_step: approach_step(WET_MS, rate.max(1)),
            engage_step: approach_step(ENGAGE_MS, rate.max(1)),
            unity_step: approach_step(UNITY_MS, rate.max(1)),
            grain_step: 1.0 / (GRAIN_MS * 0.001 * rate.max(1) as f32),
            ready: false,
        }
    }

    /// Drops the history and the glides.  The next [`Self::update`] snaps.
    /// No allocation.  Called when a path slot starts over.
    pub(crate) fn reset(&mut self, rate: u32) {
        self.line[0].fill(0.0);
        self.line[1].fill(0.0);
        self.write = MASK;
        self.phase = 0.5;
        self.pending = None;
        self.ready = false;
        self.rate = rate.max(1);
    }

    pub(crate) fn update(&mut self, settings: PitchSettings, rate: u32) {
        let settings = settings.sanitized();
        let rate = rate.max(1);
        self.rate = rate;
        self.ratio_step = approach_step(RATIO_MS, rate);
        self.wet_step = approach_step(WET_MS, rate);
        self.engage_step = approach_step(ENGAGE_MS, rate);
        self.unity_step = approach_step(UNITY_MS, rate);
        self.grain_step = 1.0 / (GRAIN_MS * 0.001 * rate as f32);
        let grain = settings.window.samples(rate);
        if !self.ready {
            self.ready = true;
            self.grain = grain;
            self.pending = None;
            self.phase = 0.5;
            self.ratio = settings.ratio();
            self.wet = settings.wet;
            self.engage = if settings.enabled { 1.0 } else { 0.0 };
            self.unity = if settings.is_unison() { 1.0 } else { 0.0 };
        } else {
            self.retarget_grain(grain);
        }
        self.settings = settings;
    }

    /// The delay this block, in samples.  0 while bypassed.  A grain change
    /// reports the new delay at once; the sound crossfades to it.
    pub(crate) fn latency_samples(&self) -> u32 {
        self.settings.latency_samples(self.rate)
    }

    pub(crate) fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        self.write = (self.write + 1) & MASK;
        self.line[0][self.write] = left;
        self.line[1][self.write] = right;
        self.glide();
        if self.engage == 0.0 {
            return (left, right);
        }
        let out_l = self.channel(0);
        let out_r = self.channel(1);
        self.advance_phase();
        // A fully open stage returns the shifted pair as computed.  Blending
        // it back with the input at a coefficient of 1 is not bit-exact.
        if self.engage >= 1.0 {
            return (out_l, out_r);
        }
        (
            left + (out_l - left) * self.engage,
            right + (out_r - right) * self.engage,
        )
    }

    fn retarget_grain(&mut self, grain: f32) {
        if (grain - self.grain).abs() < 0.5 {
            self.pending = None;
            return;
        }
        if let Some(pending) = &mut self.pending {
            pending.grain = grain;
        } else {
            self.pending = Some(Pending {
                grain,
                phase: 0.5,
                blend: 0.0,
            });
        }
    }

    fn glide(&mut self) {
        let settings = self.settings;
        self.ratio = approach(self.ratio, settings.ratio(), self.ratio_step);
        self.wet = approach(self.wet, settings.wet, self.wet_step);
        self.engage = approach(
            self.engage,
            if settings.enabled { 1.0 } else { 0.0 },
            self.engage_step,
        );
        // Stay with the heads until the glide has actually arrived at
        // unison, then fade to the pure delay.  Stopping mid-crossfade
        // would leave the two heads open and comb.
        let want_unity = settings.is_unison() && (self.ratio - 1.0).abs() < 1.0e-4;
        self.unity = approach(
            self.unity,
            if want_unity { 1.0 } else { 0.0 },
            self.unity_step,
        );
        if let Some(pending) = &mut self.pending {
            pending.blend = (pending.blend + self.grain_step).min(1.0);
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.blend >= 1.0)
        {
            let pending = self.pending.take().expect("blend just reached 1");
            self.grain = pending.grain;
            self.phase = pending.phase;
        }
    }

    fn advance_phase(&mut self) {
        // Keep the heads moving while a bypass is still fading out, so the
        // fade is the shifter and not two frozen heads.
        if self.engage == 0.0 {
            return;
        }
        self.phase = wrap(self.phase + phase_step(self.ratio, self.grain));
        if let Some(pending) = &mut self.pending {
            pending.phase = wrap(pending.phase + phase_step(self.ratio, pending.grain));
        }
    }

    fn channel(&self, channel: usize) -> f32 {
        let shifted = self.layer(channel, self.phase, self.grain);
        let shifted = if let Some(pending) = &self.pending {
            let other = self.layer(channel, pending.phase, pending.grain);
            shifted + (other - shifted) * pending.blend
        } else {
            shifted
        };
        if self.wet >= 1.0 {
            return shifted;
        }
        let dry = self.dry(channel);
        if self.wet <= 0.0 {
            return dry;
        }
        dry + (shifted - dry) * self.wet
    }

    /// One grain: the two heads, or the pure delay when parked at unison.
    fn layer(&self, channel: usize, phase: f32, grain: f32) -> f32 {
        let fixed = self.read(channel, half(grain));
        if self.unity >= 1.0 {
            return fixed;
        }
        let voiced = self.voice(channel, phase, grain);
        if self.unity <= 0.0 {
            voiced
        } else {
            voiced + (fixed - voiced) * self.unity
        }
    }

    fn dry(&self, channel: usize) -> f32 {
        let fixed = self.read(channel, half(self.grain));
        if let Some(pending) = &self.pending {
            let other = self.read(channel, half(pending.grain));
            fixed + (other - fixed) * pending.blend
        } else {
            fixed
        }
    }

    fn voice(&self, channel: usize, phase: f32, grain: f32) -> f32 {
        let other = (phase + 0.5).rem_euclid(1.0);
        let weight = hann(phase);
        self.read(channel, phase * grain) * weight
            + self.read(channel, other * grain) * (1.0 - weight)
    }

    fn read(&self, channel: usize, delay: f32) -> f32 {
        let delay = delay.clamp(0.0, (LINE - 4) as f32);
        let int = delay.floor() as usize;
        let frac = delay - int as f32;
        let at = |ago: usize| self.line[channel][self.write.wrapping_sub(ago) & MASK];
        if frac < 1.0e-6 {
            return at(int);
        }
        let older = if int == 0 { at(0) } else { at(int - 1) };
        hermite(older, at(int), at(int + 1), at(int + 2), frac)
    }
}

fn zero_line() -> Box<[f32]> {
    vec![0.0; LINE].into_boxed_slice()
}

fn half(grain: f32) -> f32 {
    (grain * 0.5).round()
}

fn phase_step(ratio: f32, grain: f32) -> f32 {
    (1.0 - ratio) / grain.max(1.0)
}

fn wrap(phase: f32) -> f32 {
    phase.rem_euclid(1.0)
}

/// Hann window over one grain.  Half a grain away it is the complement, so
/// the two heads sum to 1.
fn hann(phase: f32) -> f32 {
    0.5 * (1.0 - (phase * std::f32::consts::TAU).cos())
}

/// 4-point Hermite.  At `frac` 0 this is exactly `y0`.
fn hermite(ym1: f32, y0: f32, y1: f32, y2: f32, frac: f32) -> f32 {
    let c0 = y0;
    let c1 = 0.5 * (y1 - ym1);
    let c2 = ym1 - 2.5 * y0 + 2.0 * y1 - 0.5 * y2;
    let c3 = 0.5 * (y2 - ym1) + 1.5 * (y0 - y1);
    ((c3 * frac + c2) * frac + c1) * frac + c0
}

fn approach_step(ms: f32, rate: u32) -> f32 {
    let samples = (ms * 0.001 * rate as f32).max(1.0);
    1.0 - (-1.0 / samples).exp()
}

fn approach(current: f32, target: f32, step: f32) -> f32 {
    let next = current + (target - current) * step;
    if (next - target).abs() < 1.0e-5 {
        target
    } else {
        next
    }
}

#[cfg(test)]
mod tests;
