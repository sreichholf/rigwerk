//! One click generator.  The mixer adds the sample after the safety
//! ceiling; this file does not know about devices.
//!
//! A tempo or rhythm change waits for the next beat, and the leftover phase
//! carries.  A new rhythm starts that bar on the one.  A click still ringing
//! when the next fires moves to a second voice instead of being cut.  No
//! heap, no table, no lock.

use serde::{Deserialize, Serialize};

use super::limiter::safety_ceiling_linear;
use super::output_map::{level_gain, DEVICE_LEVEL_MIN_DB};

pub const METRONOME_BPM_MIN: f32 = 40.0;
pub const METRONOME_BPM_MAX: f32 = 240.0;
pub const METRONOME_BPM_DEFAULT: f32 = 120.0;

/// Same floor as the device fader.
pub const CLICK_LEVEL_MIN_DB: f32 = DEVICE_LEVEL_MIN_DB;
/// 0 dB peaks the accent at the safety ceiling.
pub const CLICK_LEVEL_MAX_DB: f32 = 0.0;
pub const CLICK_LEVEL_DEFAULT_DB: f32 = -12.0;

const ACCENT_HZ: f32 = 1760.0;
const SECONDARY_HZ: f32 = 1175.0;
const PULSE_HZ: f32 = 880.0;
const ENVELOPE_SECONDS: f32 = 0.008;
/// Exact silence below this.  At this τ that is ~74 ms, past a clicky cut.
const ENVELOPE_FLOOR: f32 = 1.0e-4;
const SECONDARY_SCALE: f32 = 0.45;
const PULSE_SCALE: f32 = 0.28;

/// The one is [`ClickWeight::Accent`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClickWeight {
    Accent,
    Secondary,
    Pulse,
}

use ClickWeight::{Accent as A, Pulse as P, Secondary as S};

/// Discriminant is the value stored on the shared transport.  Unknown reads
/// as [`Self::FourFour`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum ClickRhythm {
    ThreeFour = 1,
    TwoFour = 2,
    SixEight = 3,
    TwoTwo = 4,
    TwelveEight = 5,
    FiveFour = 6,
    SevenEight = 7,
    NineEight = 8,
    SixFour = 9,
    SevenFour = 10,
    ThreeEight = 11,
    FiveEight = 12,
    #[default]
    #[serde(other)]
    FourFour = 0,
}

/// Offered order, most common first.
pub const CLICK_RHYTHMS: [ClickRhythm; 13] = [
    ClickRhythm::FourFour,
    ClickRhythm::ThreeFour,
    ClickRhythm::TwoFour,
    ClickRhythm::SixEight,
    ClickRhythm::TwoTwo,
    ClickRhythm::TwelveEight,
    ClickRhythm::FiveFour,
    ClickRhythm::SevenEight,
    ClickRhythm::NineEight,
    ClickRhythm::SixFour,
    ClickRhythm::SevenFour,
    ClickRhythm::ThreeEight,
    ClickRhythm::FiveEight,
];

impl ClickRhythm {
    pub fn from_u8(raw: u8) -> Self {
        CLICK_RHYTHMS
            .into_iter()
            .find(|rhythm| *rhythm as u8 == raw)
            .unwrap_or(Self::FourFour)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::FourFour => "4/4",
            Self::ThreeFour => "3/4",
            Self::TwoFour => "2/4",
            Self::SixEight => "6/8",
            Self::TwoTwo => "2/2",
            Self::TwelveEight => "12/8",
            Self::FiveFour => "5/4",
            Self::SevenEight => "7/8",
            Self::NineEight => "9/8",
            Self::SixFour => "6/4",
            Self::SevenFour => "7/4",
            Self::ThreeEight => "3/8",
            Self::FiveEight => "5/8",
        }
    }

    pub fn clicks(self) -> u8 {
        self.pattern().len() as u8
    }

    /// Beat 1 is the accent.  Inner accents follow the usual grouping: 4/4
    /// on 3, 6/8, 6/4, 12/8 and 9/8 on each dotted group, 5/4 and 5/8 as
    /// 3+2, 7/8 as 2+2+3, 7/4 as 4+3.
    fn weight(self, beat: u8) -> ClickWeight {
        let pattern = self.pattern();
        let index = beat.saturating_sub(1) as usize;
        pattern.get(index).copied().unwrap_or(A)
    }

    fn pattern(self) -> &'static [ClickWeight] {
        match self {
            Self::FourFour => &[A, P, S, P],
            Self::ThreeFour | Self::ThreeEight => &[A, P, P],
            Self::TwoFour | Self::TwoTwo => &[A, P],
            Self::SixEight | Self::SixFour => &[A, P, P, S, P, P],
            Self::TwelveEight => &[A, P, P, S, P, P, S, P, P, S, P, P],
            Self::FiveFour | Self::FiveEight => &[A, P, P, S, P],
            Self::SevenEight => &[A, P, S, P, S, P, P],
            Self::NineEight => &[A, P, P, S, P, P, S, P, P],
            Self::SevenFour => &[A, P, P, P, S, P, P],
        }
    }
}

pub(crate) fn clamp_metronome_bpm(bpm: f32) -> f32 {
    if bpm.is_nan() {
        METRONOME_BPM_DEFAULT
    } else {
        bpm.clamp(METRONOME_BPM_MIN, METRONOME_BPM_MAX)
    }
}

pub(crate) fn clamp_click_level_db(db: f32) -> f32 {
    if db.is_nan() {
        CLICK_LEVEL_DEFAULT_DB
    } else {
        db.clamp(CLICK_LEVEL_MIN_DB, CLICK_LEVEL_MAX_DB)
    }
}

/// 0 dB is unity, so the mixer only scales.
pub(crate) fn click_level_gain(db: f32) -> f32 {
    level_gain(clamp_click_level_db(db))
}

#[derive(Clone, Copy, Debug)]
struct Voice {
    on: bool,
    /// Samples since this click fired.
    n: u32,
    weight: ClickWeight,
}

impl Voice {
    const SILENT: Self = Self {
        on: false,
        n: 0,
        weight: ClickWeight::Accent,
    };

    /// Exact `0.0` once the envelope is below the floor.
    #[inline]
    fn next(&mut self, rate: u32, ceiling: f32) -> f32 {
        if !self.on {
            return 0.0;
        }
        let rate = rate.max(1) as f32;
        let t = self.n as f32 / rate;
        let env = (-t / ENVELOPE_SECONDS).exp();
        if env < ENVELOPE_FLOOR {
            self.on = false;
            return 0.0;
        }
        let (hz, scale) = match self.weight {
            ClickWeight::Accent => (ACCENT_HZ, 1.0),
            ClickWeight::Secondary => (SECONDARY_HZ, SECONDARY_SCALE),
            ClickWeight::Pulse => (PULSE_HZ, PULSE_SCALE),
        };
        let sample = scale * ceiling * (std::f32::consts::TAU * hz * t).sin() * env;
        self.n = self.n.saturating_add(1);
        sample
    }
}

/// The mixer holds the only one.  [`Self::latch`] and [`Self::scale`] once
/// per block, [`Self::next`] once per sample.  None of those allocate.
pub(crate) struct Metronome {
    running: bool,
    bpm: f32,
    pending_bpm: f32,
    rhythm: ClickRhythm,
    pending_rhythm: ClickRhythm,
    /// Waiting for the beat boundary.
    retune: bool,
    /// Samples into the current beat.  0 right after a fire.
    phase: f64,
    /// Samples in the current beat.
    interval: f64,
    /// 0 when stopped, otherwise `1..=clicks` of the last fire.
    beat: u8,
    rate: u32,
    voice: Voice,
    /// The previous click while it still rings.  Only a restart soon after
    /// a stop overlaps; a third click that early replaces this one.
    tail: Voice,
    /// Linear safety ceiling, read once.
    ceiling: f32,
}

impl Metronome {
    pub(crate) fn new() -> Self {
        Self {
            running: false,
            bpm: METRONOME_BPM_DEFAULT,
            pending_bpm: METRONOME_BPM_DEFAULT,
            rhythm: ClickRhythm::FourFour,
            pending_rhythm: ClickRhythm::FourFour,
            retune: false,
            phase: 0.0,
            interval: 0.0,
            beat: 0,
            rate: 1,
            voice: Voice::SILENT,
            tail: Voice::SILENT,
            ceiling: safety_ceiling_linear(),
        }
    }

    pub(crate) fn beat(&self) -> u8 {
        self.beat
    }

    /// `ratio` is `new_rate / old_rate`.  The rest of this beat keeps its
    /// length; [`Self::latch`] builds the next beat from the new rate.
    pub(crate) fn scale(&mut self, ratio: f64) {
        if ratio.is_finite() && ratio > 0.0 {
            self.phase *= ratio;
            self.interval *= ratio;
        }
    }

    /// The current beat's rhythm.  A latched change stays pending until
    /// [`Self::fire`].
    pub(crate) fn rhythm(&self) -> ClickRhythm {
        self.rhythm
    }

    /// Does not clamp tempo.  [`clamp_metronome_bpm`] runs when preferences
    /// load and when the field is edited.
    pub(crate) fn latch(&mut self, running: bool, bpm: f32, rhythm: ClickRhythm, rate: u32) {
        let rate = rate.max(1);
        if rate != self.rate {
            // A sounding click counts samples at the old rate.  Rescale, as
            // [`Self::scale`] does the beat, so it keeps its time.
            let ratio = rate as f64 / self.rate as f64;
            for voice in [&mut self.voice, &mut self.tail] {
                voice.n = (voice.n as f64 * ratio).round() as u32;
            }
        }
        self.rate = rate;
        if !running {
            self.running = false;
            self.phase = 0.0;
            self.beat = 0;
            self.retune = false;
            self.pending_bpm = bpm;
            self.pending_rhythm = rhythm;
            return;
        }
        if !self.running {
            self.running = true;
            self.bpm = bpm;
            self.rhythm = rhythm;
            self.pending_bpm = bpm;
            self.pending_rhythm = rhythm;
            self.retune = false;
            self.interval = interval_samples(self.rate, bpm);
            // The next sample increments phase onto the interval and fires.
            self.phase = self.interval - 1.0;
            self.beat = 0;
            return;
        }
        self.pending_bpm = bpm;
        self.pending_rhythm = rhythm;
        self.retune = bpm != self.bpm || rhythm != self.rhythm;
    }

    /// The voice alone: the caller applies click gain.  Exact `0.0` once
    /// the envelope has fallen through the floor.
    #[inline]
    pub(crate) fn next(&mut self) -> f32 {
        if self.running {
            self.phase += 1.0;
            if self.phase >= self.interval {
                self.phase -= self.interval;
                self.fire();
            }
        }
        self.voice()
    }

    fn fire(&mut self) {
        if self.retune {
            if self.pending_rhythm != self.rhythm {
                // The new bar starts here, on the one.
                self.beat = 0;
            }
            self.bpm = self.pending_bpm;
            self.rhythm = self.pending_rhythm;
            self.interval = interval_samples(self.rate, self.bpm);
            self.retune = false;
        }
        let beats = self.rhythm.clicks().max(1);
        if self.beat == 0 || self.beat >= beats {
            self.beat = 1;
        } else {
            self.beat += 1;
        }
        if self.voice.on {
            self.tail = self.voice;
        }
        self.voice = Voice {
            on: true,
            n: 0,
            weight: self.rhythm.weight(self.beat),
        };
    }

    /// A silent tail is not added, so a lone click is its own sample.
    #[inline]
    fn voice(&mut self) -> f32 {
        let sample = self.voice.next(self.rate, self.ceiling);
        if !self.tail.on {
            return sample;
        }
        let tail = self.tail.next(self.rate, self.ceiling);
        if tail == 0.0 {
            sample
        } else {
            sample + tail
        }
    }
}

fn interval_samples(rate: u32, bpm: f32) -> f64 {
    rate.max(1) as f64 * 60.0 / bpm as f64
}

#[cfg(test)]
mod tests;
