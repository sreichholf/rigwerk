//! The tuner's non-UI half: note maths, the preset table, YIN pitch
//! detection and the preset-guided harmonic analysis that turns one strum into
//! per-string readings.
//!
//! Nothing here knows about egui.  `app::tuner::TunerState` owns a
//! [`TunerAnalysis`], copies the latest tap window into a scratch buffer and
//! reads the resulting [`TunerFrame`].  The fixed scratch is allocated in
//! `new`; the poly and preview buffers are sized on the first window of each
//! size and reused after that, so a steady-state analysis does not allocate.
//!
//! Input rates up to 192 kHz are supported.  The decimation factor is clamped
//! at 8, so at 192 kHz one decimated sample is still 41.7 µs and YIN's window
//! reaches A0 (tau 873 of an available 890).  Above that A0 falls out of
//! reach — at 384 kHz the lowest measurable note is about 47 Hz — so the
//! tuner does not claim the bottom octave of a bass there.

use std::f32::consts::TAU;
use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

/// Native frames the caller copies from the tap per poll.  Also the widest
/// window the analysis asks for and the cap [`poly_window_len`] clamps to:
/// YIN needs 2048 decimated samples, which is 16384 native frames once the
/// decimation factor reaches 8, and the poly window is never longer.
pub(crate) const TAP_COPY_FRAMES: usize = 16_384;
/// The permanent "centered" mark, independent of the tolerance setting.
pub(crate) const CENTER_CENTS: f32 = 1.0;
pub(crate) const MIN_HZ: f32 = 27.0;
pub(crate) const MAX_HZ: f32 = 1_400.0;

/// Decimated samples YIN works on.
const YIN_WINDOW: usize = 2_048;
/// Accepted frequencies kept for median smoothing.
const HISTORY: usize = 5;
/// Presets never have more strings than this; the frames are fixed-size.
const MAX_STRINGS: usize = 8;
/// Analyses inside the band before InTune/Centered is reported.
const STABLE_FRAMES: u8 = 2;
/// Silent frames after which a string's smoothing history is dropped.
const SILENT_RESET: u8 = 5;
/// Partials scored per string by the poly analysis.
const HARMONICS: usize = 6;
/// On-harmonic level relative to the median inter-harmonic level.
const NULL_RATIO: f32 = 1.8;
/// A peak pinned to the edge of the ±60-cent sweep is not a reading: the true
/// minimum is outside the window the string is allowed to be tuned in.
const EDGE_CENTS: f32 = 59.0;
/// Below this, the second partial is the octave above and usually another
/// string's fundamental, so it is not independent evidence and is skipped.
const LOW_STRING_HZ: f32 = 80.0;
/// Presets whose lowest string is under this get the longer poly window.
const LOW_POLY_HZ: f32 = 60.0;
/// A partial that another string of the preset also claims is still counted,
/// but four times less: in a chord the two lines are a fraction of a bin apart
/// and trusting one drags the row toward its neighbour.
const SHARED_WEIGHT: f32 = 0.25;
/// Candidates this close to the best |cents| are a tie for the matcher, and
/// the one needing the smallest harmonic fold wins.
const MATCH_TIE_CENTS: f32 = 0.5;
/// Below this share of the presence score, a string's clean partials carry no
/// usable evidence and the pitch fit is noise, so the presence fit is used
/// instead.  Whether the row is a ghost is a separate decision, made with
/// [`GHOST_CLEAN_RATIO`].
const CLEAN_EVIDENCE: f32 = 0.1;
/// A row whose clean score is under this share of its presence score is a
/// ghost candidate.  Kept the same as [`CLEAN_EVIDENCE`] so the two stages
/// agree on what "no evidence of its own" means.
const GHOST_CLEAN_RATIO: f32 = CLEAN_EVIDENCE;
/// Zero padding applied to the poly window before the transform.  The window
/// itself is untouched; the padding only samples its spectrum finer.  Without
/// it one bin is 5.86 Hz at 48 kHz, about 148 cents at C2, far more than the
/// rows are asked to resolve.
const SPECTRUM_PADDING: usize = 4;
const MAX_TRANSFORM: usize = 65_536;
const DEFAULT_RATE: u32 = 48_000;

/// Detune of each preview row, lowest string first.  Longer presets cycle it,
/// so a 7- and 8-string chord still shows a spread.
const PREVIEW_DETUNES: [f32; 6] = [-12.4, 2.1, -0.8, 6.2, -3.4, 1.2];

/// Equal-tempered frequency of a MIDI note, with A4 as the reference.
pub(crate) fn midi_to_hz(midi: i16, a4: f32) -> f32 {
    a4 * 2.0f32.powf((midi as f32 - 69.0) / 12.0)
}

/// Signed distance of `frequency` from `target`, in cents.
pub(crate) fn hz_to_cents(frequency: f32, target: f32) -> f32 {
    if frequency <= 0.0 || target <= 0.0 {
        return 0.0;
    }
    1200.0 * (frequency / target).log2()
}

pub(crate) fn cents_to_ratio(cents: f32) -> f32 {
    2.0f32.powf(cents / 1200.0)
}

pub(crate) fn nearest_midi(frequency: f32, a4: f32) -> i16 {
    if frequency <= 0.0 || a4 <= 0.0 {
        return 69;
    }
    let midi = 69.0 + 12.0 * (frequency / a4).log2();
    midi.round().clamp(-128.0, 127.0) as i16
}

const NOTE_NAMES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];

pub(crate) fn note_name(midi: i16) -> &'static str {
    NOTE_NAMES[midi.rem_euclid(12) as usize]
}

pub(crate) fn octave(midi: i16) -> i16 {
    midi.div_euclid(12) - 1
}

/// "C#2" and friends.  The frame never builds one of these per poll; it keeps
/// the name and the octave apart, so only the tests use this.
#[cfg(test)]
pub(crate) fn midi_to_name(midi: i16) -> String {
    format!("{}{}", note_name(midi), octave(midi))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TuningGroup {
    Guitar6,
    Extended,
    Bass,
}

impl TuningGroup {
    pub const ALL: [Self; 3] = [Self::Guitar6, Self::Extended, Self::Bass];

    pub fn label(self) -> &'static str {
        match self {
            Self::Guitar6 => "GUITAR · 6 STRING",
            Self::Extended => "EXTENDED RANGE",
            Self::Bass => "BASS",
        }
    }
}

pub(crate) struct Tuning {
    pub id: &'static str,
    pub name: &'static str,
    pub group: TuningGroup,
    /// Display name and MIDI note, lowest string first.
    pub strings: &'static [(&'static str, i16)],
}

impl Tuning {
    pub(crate) fn string_hz(&self, index: usize, a4: f32) -> f32 {
        self.strings
            .get(index)
            .map(|(_, midi)| midi_to_hz(*midi, a4))
            .unwrap_or(0.0)
    }
}

/// The static table.  Grouped by how the page lists them: six-string guitar
/// tunings, extended range, then bass.
pub(crate) const PRESETS: &[Tuning] = &[
    Tuning {
        id: "e-standard",
        name: "E standard",
        group: TuningGroup::Guitar6,
        strings: &[
            ("E2", 40),
            ("A2", 45),
            ("D3", 50),
            ("G3", 55),
            ("B3", 59),
            ("E4", 64),
        ],
    },
    Tuning {
        id: "eb-standard",
        name: "Eb standard",
        group: TuningGroup::Guitar6,
        strings: &[
            ("Eb2", 39),
            ("Ab2", 44),
            ("Db3", 49),
            ("Gb3", 54),
            ("Bb3", 58),
            ("Eb4", 63),
        ],
    },
    Tuning {
        id: "d-standard",
        name: "D standard",
        group: TuningGroup::Guitar6,
        strings: &[
            ("D2", 38),
            ("G2", 43),
            ("C3", 48),
            ("F3", 53),
            ("A3", 57),
            ("D4", 62),
        ],
    },
    Tuning {
        id: "c-sharp-standard",
        name: "C# standard",
        group: TuningGroup::Guitar6,
        strings: &[
            ("C#2", 37),
            ("F#2", 42),
            ("B2", 47),
            ("E3", 52),
            ("G#3", 56),
            ("C#4", 61),
        ],
    },
    Tuning {
        id: "c-standard",
        name: "C standard",
        group: TuningGroup::Guitar6,
        strings: &[
            ("C2", 36),
            ("F2", 41),
            ("Bb2", 46),
            ("Eb3", 51),
            ("G3", 55),
            ("C4", 60),
        ],
    },
    Tuning {
        id: "drop-d",
        name: "Drop D",
        group: TuningGroup::Guitar6,
        strings: &[
            ("D2", 38),
            ("A2", 45),
            ("D3", 50),
            ("G3", 55),
            ("B3", 59),
            ("E4", 64),
        ],
    },
    Tuning {
        id: "drop-c-sharp",
        name: "Drop C#",
        group: TuningGroup::Guitar6,
        strings: &[
            ("C#2", 37),
            ("G#2", 44),
            ("C#3", 49),
            ("F#3", 54),
            ("A#3", 58),
            ("D#4", 63),
        ],
    },
    Tuning {
        id: "drop-c",
        name: "Drop C",
        group: TuningGroup::Guitar6,
        strings: &[
            ("C2", 36),
            ("G2", 43),
            ("C3", 48),
            ("F3", 53),
            ("A3", 57),
            ("D4", 62),
        ],
    },
    Tuning {
        id: "drop-b",
        name: "Drop B",
        group: TuningGroup::Guitar6,
        strings: &[
            ("B1", 35),
            ("F#2", 42),
            ("B2", 47),
            ("E3", 52),
            ("G#3", 56),
            ("C#4", 61),
        ],
    },
    Tuning {
        id: "drop-a",
        name: "Drop A",
        group: TuningGroup::Guitar6,
        strings: &[
            ("A1", 33),
            ("E2", 40),
            ("A2", 45),
            ("D3", 50),
            ("F#3", 54),
            ("B3", 59),
        ],
    },
    Tuning {
        id: "7-string-b",
        name: "7-string B",
        group: TuningGroup::Extended,
        strings: &[
            ("B1", 35),
            ("E2", 40),
            ("A2", 45),
            ("D3", 50),
            ("G3", 55),
            ("B3", 59),
            ("E4", 64),
        ],
    },
    Tuning {
        id: "7-string-drop-a",
        name: "7-string Drop A",
        group: TuningGroup::Extended,
        strings: &[
            ("A1", 33),
            ("E2", 40),
            ("A2", 45),
            ("D3", 50),
            ("G3", 55),
            ("B3", 59),
            ("E4", 64),
        ],
    },
    Tuning {
        id: "8-string-f-sharp",
        name: "8-string F#",
        group: TuningGroup::Extended,
        strings: &[
            ("F#1", 30),
            ("B1", 35),
            ("E2", 40),
            ("A2", 45),
            ("D3", 50),
            ("G3", 55),
            ("B3", 59),
            ("E4", 64),
        ],
    },
    Tuning {
        id: "8-string-drop-e",
        name: "8-string Drop E",
        group: TuningGroup::Extended,
        strings: &[
            ("E1", 28),
            ("B1", 35),
            ("E2", 40),
            ("A2", 45),
            ("D3", 50),
            ("G3", 55),
            ("B3", 59),
            ("E4", 64),
        ],
    },
    Tuning {
        id: "bass-4-e",
        name: "4-string E",
        group: TuningGroup::Bass,
        strings: &[("E1", 28), ("A1", 33), ("D2", 38), ("G2", 43)],
    },
    Tuning {
        id: "bass-4-eb",
        name: "4-string Eb",
        group: TuningGroup::Bass,
        strings: &[("Eb1", 27), ("Ab1", 32), ("Db2", 37), ("Gb2", 42)],
    },
    Tuning {
        id: "bass-4-d",
        name: "4-string D",
        group: TuningGroup::Bass,
        strings: &[("D1", 26), ("G1", 31), ("C2", 36), ("F2", 41)],
    },
    Tuning {
        id: "bass-5-b",
        name: "5-string B",
        group: TuningGroup::Bass,
        strings: &[("B0", 23), ("E1", 28), ("A1", 33), ("D2", 38), ("G2", 43)],
    },
    Tuning {
        id: "bass-5-drop-a",
        name: "5-string Drop A",
        group: TuningGroup::Bass,
        strings: &[("A0", 21), ("E1", 28), ("A1", 33), ("D2", 38), ("G2", 43)],
    },
    Tuning {
        id: "bass-6-b",
        name: "6-string B",
        group: TuningGroup::Bass,
        strings: &[
            ("B0", 23),
            ("E1", 28),
            ("A1", 33),
            ("D2", 38),
            ("G2", 43),
            ("C3", 48),
        ],
    },
];

/// The chromatic mode is not a preset: it has no strings, so the page shows a
/// single reading and disables the all-strings view.
pub(crate) const DEFAULT_PRESET_ID: &str = "chromatic";

pub(crate) fn tuning_index(id: &str) -> Option<usize> {
    PRESETS.iter().position(|tuning| tuning.id == id)
}

pub(crate) fn tuning_by_id(id: &str) -> Option<&'static Tuning> {
    PRESETS.iter().find(|tuning| tuning.id == id)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TunerChannel {
    Sum,
    Left,
    Right,
}

impl TunerChannel {
    pub const ALL: [Self; 3] = [Self::Sum, Self::Left, Self::Right];

    pub fn label(self) -> &'static str {
        match self {
            Self::Sum => "Sum",
            Self::Left => "L",
            Self::Right => "R",
        }
    }

    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Left,
            2 => Self::Right,
            _ => Self::Sum,
        }
    }

    pub fn to_u8(self) -> u8 {
        match self {
            Self::Sum => 0,
            Self::Left => 1,
            Self::Right => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TunerTolerance {
    Tight,
    Standard,
    Live,
}

impl TunerTolerance {
    pub const ALL: [Self; 3] = [Self::Tight, Self::Standard, Self::Live];

    pub fn cents(self) -> f32 {
        match self {
            Self::Tight => 3.0,
            Self::Standard => 5.0,
            Self::Live => 10.0,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Tight => "±3",
            Self::Standard => "±5",
            Self::Live => "±10",
        }
    }

    pub fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Tight,
            2 => Self::Live,
            _ => Self::Standard,
        }
    }

    pub fn to_u8(self) -> u8 {
        match self {
            Self::Tight => 0,
            Self::Standard => 1,
            Self::Live => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TuneState {
    Silent,
    Centered,
    InTune,
    Flat,
    Sharp,
}

pub(crate) struct TunerReading {
    pub valid: bool,
    /// The measured frequency ("detected"), after median smoothing.
    pub frequency: f32,
    /// The note or string being tuned to.
    pub target_hz: f32,
    pub cents: f32,
    pub midi: i16,
    pub string: Option<usize>,
    /// 0..1 clarity from YIN.
    pub confidence: f32,
    /// dBFS of the analysed input window, not of this note.  It is the same
    /// level the string rows carry, and it is what the page shows in its
    /// LEVEL meter.
    pub level_db: f32,
    pub state: TuneState,
}

impl TunerReading {
    pub(crate) fn silent() -> Self {
        Self {
            valid: false,
            frequency: 0.0,
            target_hz: 0.0,
            cents: 0.0,
            midi: 0,
            string: None,
            confidence: 0.0,
            level_db: -120.0,
            state: TuneState::Silent,
        }
    }
}

pub(crate) struct StringReading {
    pub string: usize,
    pub name: &'static str,
    pub midi: i16,
    pub target_hz: f32,
    pub cents: f32,
    /// The input window's level, repeated here for row layout.  It is not a
    /// per-string energy; the page must not draw it as one.
    pub level_db: f32,
    /// 0..1: the share of the on-harmonic level the inter-harmonic floor does
    /// not explain.
    pub confidence: f32,
    pub sounding: bool,
    /// Set when this string only appears through another string's harmonic.
    /// The page dims the row and leaves it out of the in-tune count; it is a
    /// heuristic, not a claim that the string is not ringing.
    pub harmonic_of: Option<usize>,
    pub state: TuneState,
}

pub(crate) struct TunerFrame {
    pub mono: TunerReading,
    /// One row per preset string, in preset order.  Empty for chromatic and
    /// when the analysis had too little audio.
    pub strings: Vec<StringReading>,
}

/// The tolerance band with hysteresis: a reading inside the band latches
/// InTune after `STABLE_FRAMES`, and only a reading past `tolerance + 1`
/// leaves.  Centered is reported from inside that latch when the reading is
/// also within the permanent ±1 mark, so it needs both the stable count and
/// the narrow band.
#[derive(Clone, Copy)]
struct StateLatch {
    in_tune: bool,
    stable: u8,
}

impl StateLatch {
    fn new() -> Self {
        Self {
            in_tune: false,
            stable: 0,
        }
    }

    fn reset(&mut self) {
        *self = Self::new();
    }

    fn update(&mut self, cents: f32, tolerance: f32) -> TuneState {
        let abs = cents.abs();
        if abs <= tolerance {
            self.stable = self.stable.saturating_add(1);
        } else {
            self.stable = 0;
        }
        if self.in_tune && abs > tolerance + 1.0 {
            self.in_tune = false;
        }
        if !self.in_tune && abs <= tolerance && self.stable >= STABLE_FRAMES {
            self.in_tune = true;
        }
        if self.in_tune {
            if abs <= CENTER_CENTS && self.stable >= STABLE_FRAMES {
                TuneState::Centered
            } else {
                TuneState::InTune
            }
        } else {
            match classify(cents, tolerance) {
                // Inside the band, but not long enough to report yet.
                TuneState::Centered | TuneState::InTune => {
                    if cents < 0.0 {
                        TuneState::Flat
                    } else {
                        TuneState::Sharp
                    }
                }
                other => other,
            }
        }
    }
}

fn classify(cents: f32, tolerance: f32) -> TuneState {
    let abs = cents.abs();
    if abs <= CENTER_CENTS {
        TuneState::Centered
    } else if abs <= tolerance {
        TuneState::InTune
    } else if cents < 0.0 {
        TuneState::Flat
    } else {
        TuneState::Sharp
    }
}

fn select_sample(interleaved: &[f32], frame: usize, channel: TunerChannel) -> f32 {
    let left = interleaved[frame * 2];
    let right = interleaved[frame * 2 + 1];
    match channel {
        TunerChannel::Sum => (left + right) * 0.5,
        TunerChannel::Left => left,
        TunerChannel::Right => right,
    }
}

/// Two cascaded one-pole low-passes ahead of the 3:1–8:1 decimation that keeps
/// YIN's difference function cheap; factor 1 skips the filter.  The corner
/// sits at 0.42 of the decimated rate, so the highest note the tuner shows
/// still passes while content that would alias folds away below the analysis
/// band.
struct Decimator {
    factor: usize,
    rate: u32,
    coefficient: f32,
    z1: f32,
    z2: f32,
}

impl Decimator {
    fn new() -> Self {
        Self {
            factor: 0,
            rate: 0,
            coefficient: 0.0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    /// The decimation factor for `rate`, resetting the filter when the rate or
    /// the factor changed.  Factor 1 means the filter is skipped.
    fn prepare(&mut self, rate: u32) -> usize {
        let factor = ((rate / 12_000).clamp(1, 8)) as usize;
        if factor != self.factor || rate != self.rate {
            self.factor = factor;
            self.rate = rate;
            self.z1 = 0.0;
            self.z2 = 0.0;
            self.coefficient = if factor > 1 {
                let corner = 0.42 * (rate as f32 / factor as f32);
                (1.0 - (-TAU * corner / rate as f32).exp()).clamp(0.0, 1.0)
            } else {
                0.0
            };
        }
        factor
    }

    fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }

    /// Decimates the newest `out.len() * factor` frames of the selected
    /// channel into `out`.  The phase restarts at the beginning of the window,
    /// so `out.len()` samples are always produced; the filter state carries
    /// over, which only ever costs a sample or two of transient at the seam.
    fn window(
        &mut self,
        interleaved: &[f32],
        frames: usize,
        channel: TunerChannel,
        out: &mut [f32],
    ) -> usize {
        let factor = self.factor.max(1);
        let needed = out.len() * factor;
        if frames < needed || interleaved.len() < frames * 2 {
            return 0;
        }
        let start = frames - needed;
        let mut written = 0;
        let mut phase = 0;
        for frame in start..frames {
            let sample = select_sample(interleaved, frame, channel);
            let value = if factor == 1 {
                sample
            } else {
                self.z1 += self.coefficient * (sample - self.z1);
                self.z2 += self.coefficient * (self.z1 - self.z2);
                self.z2
            };
            if phase == 0 {
                out[written] = value;
                written += 1;
            }
            phase = (phase + 1) % factor;
        }
        written
    }
}

/// Synthesizes a decaying harmonic series for each `(frequency, amplitude)`
/// into `mono`.  A rotating complex oscillator keeps the cost to a few
/// multiplies per sample, and starting every partial in phase makes the sum
/// look like a plucked string rather than a random-phase blob.
fn synthesize(mono: &mut [f32], tones: &[(f32, f32)], rate: u32) {
    mono.fill(0.0);
    let decay = (-3.0f32 / mono.len() as f32).exp();
    for &(frequency, amplitude) in tones {
        for harmonic in 1..=HARMONICS {
            let omega = TAU * frequency * harmonic as f32 / rate as f32;
            let (sin_step, cos_step) = omega.sin_cos();
            let mut real = 1.0f32;
            let mut imaginary = 0.0f32;
            let mut envelope = amplitude / harmonic as f32;
            for sample in mono.iter_mut() {
                *sample += imaginary * envelope;
                let next_real = real * cos_step - imaginary * sin_step;
                let next_imaginary = real * sin_step + imaginary * cos_step;
                real = next_real;
                imaginary = next_imaginary;
                envelope *= decay;
            }
        }
    }
}

fn interleave_into(mono: &[f32], interleaved: &mut [f32]) {
    for (index, sample) in mono.iter().enumerate().take(interleaved.len() / 2) {
        interleaved[index * 2] = *sample;
        interleaved[index * 2 + 1] = *sample;
    }
}

/// Window the poly stage transforms.  Guitar presets use 0.17 s; a preset
/// whose lowest string is under 60 Hz gets 0.34 s, because at 41 Hz one
/// 0.17 s bin is over 200 cents and a neighbour's partial inside that lobe
/// pulls the row several cents.  Both fit inside [`TAP_COPY_FRAMES`], which is
/// also the cap: at 96 kHz 16384 frames is 170 ms, so a bass preset gets the
/// same window a guitar gets at 48 kHz; at 192 kHz that cap is only 85 ms (one
/// cell 11.7 Hz), so poly rows at that rate are coarse — the YIN reading stays
/// exact, the all-strings view does not.  Above 192 kHz neither is promised.
fn poly_window_len(rate: u32, lowest_hz: f32) -> usize {
    let seconds = if lowest_hz < LOW_POLY_HZ { 0.34 } else { 0.17 };
    ((rate as f32 * seconds) as usize)
        .next_power_of_two()
        .clamp(4_096, TAP_COPY_FRAMES)
}

/// YIN's cumulative mean normalised difference function over one window.
/// Returns the interpolated frequency and the clarity, or `None` when the
/// window is not periodic enough to call a pitch.
fn yin(mono: &[f32], rate: u32, difference: &mut [f32], cmndf: &mut [f32]) -> Option<(f32, f32)> {
    let window = mono.len();
    if window < 64 || rate == 0 {
        return None;
    }
    let rate = rate as f32;
    let tau_max = ((rate / MIN_HZ) as usize + 2).min(window / 2);
    if tau_max < 3 || difference.len() <= tau_max || cmndf.len() <= tau_max {
        return None;
    }

    for tau in 1..=tau_max {
        let mut sum = 0.0f32;
        for index in 0..window - tau {
            let delta = mono[index] - mono[index + tau];
            sum += delta * delta;
        }
        difference[tau] = sum;
    }

    cmndf[0] = 1.0;
    let mut running = 0.0f32;
    for tau in 1..=tau_max {
        running += difference[tau];
        cmndf[tau] = if running > 0.0 {
            difference[tau] * tau as f32 / running
        } else {
            1.0
        };
    }

    let tau_min = ((rate / MAX_HZ) as usize).max(2).min(tau_max);
    let mut chosen = None;
    for tau in tau_min..=tau_max {
        if cmndf[tau] < 0.15 {
            // The first dip is the period; the ones after it are its
            // multiples.  Walk down to the bottom of that dip.
            let mut bottom = tau;
            while bottom < tau_max && cmndf[bottom + 1] < cmndf[bottom] {
                bottom += 1;
            }
            chosen = Some(bottom);
            break;
        }
    }
    let tau = match chosen {
        Some(tau) => tau,
        None => {
            let mut best = tau_min;
            for candidate in tau_min + 1..=tau_max {
                if cmndf[candidate] < cmndf[best] {
                    best = candidate;
                }
            }
            // Without a clear dip there is no pitch, only a noise floor.
            if cmndf[best] >= 0.30 {
                return None;
            }
            best
        }
    };

    let clarity = (1.0 - cmndf[tau]).clamp(0.0, 1.0);
    if clarity < 0.5 {
        return None;
    }
    let mut refined = tau as f32;
    if tau > 1 && tau < tau_max {
        // Interpolate the difference function, not the cumulative one: the
        // division by the running sum tilts the dip and biases the estimate
        // high, which at 880 Hz is already more than a cent.
        let previous = difference[tau - 1];
        let current = difference[tau];
        let next = difference[tau + 1];
        let denominator = previous - 2.0 * current + next;
        if denominator.abs() > 1e-12 {
            let offset = 0.5 * (previous - next) / denominator;
            if offset.abs() <= 1.0 {
                refined = tau as f32 + offset;
            }
        }
    }
    let frequency = rate / refined;
    if !(MIN_HZ..=MAX_HZ).contains(&frequency) {
        return None;
    }
    Some((frequency, clarity))
}

/// True when `weaker` sits within two cents of a whole-number multiple of
/// `stronger`, i.e. the weaker string's target is also a harmonic of the
/// stronger one.
fn harmonic_of(weaker: f32, stronger: f32) -> bool {
    (2..=8u32).any(|multiple| hz_to_cents(weaker, stronger * multiple as f32).abs() <= 2.0)
}

pub(crate) struct TunerAnalysis {
    frame: TunerFrame,
    decimator: Decimator,
    /// YIN_WINDOW decimated samples.
    mono: Vec<f32>,
    /// Difference function and its cumulative normalisation, `tau_max + 1`.
    difference: Vec<f32>,
    cmndf: Vec<f32>,
    /// The last accepted frequencies, as a small ring.
    history: [f32; HISTORY],
    history_len: usize,
    history_next: usize,
    sorted: [f32; HISTORY],
    mono_latch: StateLatch,
    /// Rate the last analysis ran at, so `preview` can synthesize at it.
    rate: u32,
    /// The poly FFT, replanned only when the window size changes.
    fft: Option<Arc<dyn Fft<f32>>>,
    fft_len: usize,
    spectrum: Vec<Complex<f32>>,
    fft_scratch: Vec<Complex<f32>>,
    magnitude: Vec<f32>,
    bin_width: f32,
    poly_prev_cents: [f32; MAX_STRINGS],
    poly_prev_valid: [bool; MAX_STRINGS],
    poly_silent: [u8; MAX_STRINGS],
    poly_latches: [StateLatch; MAX_STRINGS],
    poly_strings: usize,
    /// The preset the smoothing history belongs to.  Two presets can have the
    /// same string count, so the id is what makes a change a change.
    poly_preset: Option<&'static str>,
    /// Preview synthesis buffers, reused between polls.  `preview_voice` holds
    /// the single string the mono stage listens to, `preview_mono` the chord.
    preview_voice: Vec<f32>,
    preview_mono: Vec<f32>,
    preview_interleaved: Vec<f32>,
}

impl TunerAnalysis {
    pub(crate) fn new() -> Self {
        Self {
            frame: TunerFrame {
                mono: TunerReading::silent(),
                strings: Vec::with_capacity(MAX_STRINGS),
            },
            decimator: Decimator::new(),
            mono: vec![0.0; YIN_WINDOW],
            difference: vec![0.0; YIN_WINDOW / 2 + 4],
            cmndf: vec![0.0; YIN_WINDOW / 2 + 4],
            history: [0.0; HISTORY],
            history_len: 0,
            history_next: 0,
            sorted: [0.0; HISTORY],
            mono_latch: StateLatch::new(),
            rate: 0,
            fft: None,
            fft_len: 0,
            spectrum: Vec::new(),
            fft_scratch: Vec::new(),
            magnitude: Vec::new(),
            bin_width: 1.0,
            poly_prev_cents: [0.0; MAX_STRINGS],
            poly_prev_valid: [false; MAX_STRINGS],
            poly_silent: [0; MAX_STRINGS],
            poly_latches: [StateLatch::new(); MAX_STRINGS],
            poly_strings: 0,
            poly_preset: None,
            preview_voice: Vec::new(),
            preview_mono: Vec::new(),
            preview_interleaved: Vec::new(),
        }
    }

    pub(crate) fn frame(&self) -> &TunerFrame {
        &self.frame
    }

    /// Forgets every history.  Called when the page stops looking and when the
    /// preset changes, so a stale reading cannot leak into the new one.
    pub(crate) fn clear(&mut self) {
        self.frame.mono = TunerReading::silent();
        self.frame.strings.clear();
        self.history_len = 0;
        self.history_next = 0;
        self.mono_latch.reset();
        self.reset_poly_history();
        self.poly_strings = 0;
        self.poly_preset = None;
        self.decimator.reset();
    }

    /// One poll: a window of interleaved input, and the settings the page is
    /// showing.  Sets both the mono reading and, for a preset, the string
    /// rows.  Nothing here allocates after the first window of each size.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn analyse(
        &mut self,
        interleaved: &[f32],
        frames: usize,
        rate: u32,
        preset: Option<&'static Tuning>,
        channel: TunerChannel,
        reference_hz: f32,
        tolerance: TunerTolerance,
    ) {
        self.frame.strings.clear();
        let rate = if rate == 0 { DEFAULT_RATE } else { rate };
        let level_db = self.analyse_mono(
            interleaved,
            frames,
            rate,
            preset,
            channel,
            reference_hz,
            tolerance,
        );
        self.analyse_poly(
            interleaved,
            frames,
            rate,
            preset,
            channel,
            reference_hz,
            tolerance,
            level_db,
        );
    }

    /// The YIN stage over one window.  Returns the input level in dBFS, which
    /// is what `level_db` on the frame reports (it is the level of the input
    /// window, not of each string).  A window that is too short or holds a
    /// non-finite sample reports -120 dBFS and silence; a finite but quiet
    /// window keeps its measured level and is silenced by the RMS gate.
    #[allow(clippy::too_many_arguments)]
    fn analyse_mono(
        &mut self,
        interleaved: &[f32],
        frames: usize,
        rate: u32,
        preset: Option<&'static Tuning>,
        channel: TunerChannel,
        reference_hz: f32,
        tolerance: TunerTolerance,
    ) -> f32 {
        self.rate = rate;
        let factor = self.decimator.prepare(rate);
        let needed = YIN_WINDOW * factor;
        if frames < needed || interleaved.len() < frames * 2 {
            self.mono_latch.reset();
            self.frame.mono = TunerReading::silent();
            return -120.0;
        }

        let written = self
            .decimator
            .window(interleaved, frames, channel, &mut self.mono);
        if written != YIN_WINDOW {
            self.mono_latch.reset();
            self.frame.mono = TunerReading::silent();
            return -120.0;
        }

        let count = YIN_WINDOW as f32;
        let mean = self.mono.iter().sum::<f32>() / count;
        for sample in &mut self.mono {
            *sample -= mean;
        }
        let rms = (self.mono.iter().map(|s| s * s).sum::<f32>() / count).sqrt();
        // A NaN or an infinity anywhere in the window poisons the filter state
        // for every later block, so the decimator is restarted rather than
        // letting one bad sample silence the tuner for the rest of the session.
        if !rms.is_finite() || self.mono.iter().any(|sample| !sample.is_finite()) {
            self.decimator.reset();
            self.mono_latch.reset();
            self.frame.mono = TunerReading::silent();
            self.reset_poly_history();
            return -120.0;
        }
        let level_db = 20.0 * rms.max(1e-9).log10();
        let dec_rate = rate / factor as u32;

        let detected = if rms < 1e-3 {
            // ~-60 dBFS: quieter than any useful pluck, and YIN would only be
            // chasing the noise floor.
            None
        } else {
            yin(&self.mono, dec_rate, &mut self.difference, &mut self.cmndf)
        };

        let mut mono = TunerReading::silent();
        mono.level_db = level_db;
        if let Some((frequency, clarity)) = detected {
            let median = self.push_history(frequency, reference_hz);
            let (midi, target_hz, cents, string) = self.match_note(median, preset, reference_hz);
            let state = self.mono_latch.update(cents, tolerance.cents());
            mono = TunerReading {
                valid: true,
                frequency: median,
                target_hz,
                cents,
                midi,
                string,
                confidence: clarity,
                level_db,
                state,
            };
        } else {
            self.mono_latch.reset();
        }
        self.frame.mono = mono;
        level_db
    }

    /// Matches the measured frequency to a string and folds harmonics onto
    /// it, or falls back to the nearest chromatic note.
    ///
    /// Selection is: smallest distance to a string target first, then — among
    /// candidates within [`MATCH_TIE_CENTS`] of that best — the smallest
    /// divisor, i.e. the reading that needs no harmonic folding.  An exact
    /// octave is both a played string and its neighbour's harmonic; the string
    /// itself is what was plucked.  A twelfth above the lowest string is *not*
    /// an exact integer multiple in equal temperament (B3 sits 1.96 cents flat
    /// of 3×E2), so it still folds to the lower string.
    ///
    /// A deliberately pure twelfth is therefore indistinguishable from the
    /// lower string's third harmonic; separating them would need a
    /// target-string lock, which this pitch-only layer does not keep.
    fn match_note(
        &self,
        frequency: f32,
        preset: Option<&'static Tuning>,
        reference_hz: f32,
    ) -> (i16, f32, f32, Option<usize>) {
        if let Some(tuning) = preset {
            // (string index, divisor, cents, target Hz)
            let mut candidates = [(0usize, 0u32, 0.0f32, 0.0f32); MAX_STRINGS * 4];
            let mut count = 0;
            let mut best_abs = f32::INFINITY;
            for (index, (_, midi)) in tuning.strings.iter().enumerate() {
                let target = midi_to_hz(*midi, reference_hz);
                // A picked harmonic is a whole-number multiple of the open
                // string, so try the measured pitch at 1x..4x.
                for divisor in 1..=4u32 {
                    let cents = hz_to_cents(frequency / divisor as f32, target);
                    if cents.abs() > 60.0 || count >= candidates.len() {
                        continue;
                    }
                    candidates[count] = (index, divisor, cents, target);
                    count += 1;
                    best_abs = best_abs.min(cents.abs());
                }
            }
            let mut chosen: Option<(usize, u32, f32, f32)> = None;
            for &candidate in &candidates[..count] {
                let (index, divisor, cents, target) = candidate;
                if cents.abs() > best_abs + MATCH_TIE_CENTS {
                    continue;
                }
                let replace = match chosen {
                    None => true,
                    Some((chosen_index, chosen_divisor, chosen_cents, _)) => {
                        divisor < chosen_divisor
                            || (divisor == chosen_divisor
                                && (cents.abs() < chosen_cents.abs() - 1e-4
                                    || ((cents.abs() - chosen_cents.abs()).abs() <= 1e-4
                                        && index < chosen_index)))
                    }
                };
                if replace {
                    chosen = Some((index, divisor, cents, target));
                }
            }
            if let Some((index, _, cents, target_hz)) = chosen {
                return (tuning.strings[index].1, target_hz, cents, Some(index));
            }
        }

        // Chromatic.  There is deliberately no octave folding here: YIN's
        // first-dip rule already refuses the half-period dip for a periodic
        // signal, and folding on a single clarity estimate would report an
        // octave down for every clean low note.  The preset matcher is where
        // the extra evidence (the string targets) exists.
        let midi = nearest_midi(frequency, reference_hz);
        let target_hz = midi_to_hz(midi, reference_hz);
        (midi, target_hz, hz_to_cents(frequency, target_hz), None)
    }

    /// Median of the last accepted frequencies, after dropping the history
    /// when the pitch jumped more than a semitone (a new note, not a wobble).
    /// The state bracket restarts with it: an InTune verdict earned by the
    /// previous string must not carry over to the new one.
    fn push_history(&mut self, frequency: f32, reference_hz: f32) -> f32 {
        if self.history_len > 0 {
            let previous = self.median();
            if (nearest_midi(frequency, reference_hz) - nearest_midi(previous, reference_hz)).abs()
                > 1
            {
                self.history_len = 0;
                self.history_next = 0;
                self.mono_latch.reset();
            }
        }
        self.history[self.history_next] = frequency;
        self.history_next = (self.history_next + 1) % HISTORY;
        self.history_len = (self.history_len + 1).min(HISTORY);
        self.median()
    }

    fn median(&mut self) -> f32 {
        let len = self.history_len;
        if len == 0 {
            return 0.0;
        }
        self.sorted[..len].copy_from_slice(&self.history[..len]);
        for index in 1..len {
            let value = self.sorted[index];
            let mut position = index;
            while position > 0 && self.sorted[position - 1] > value {
                self.sorted[position] = self.sorted[position - 1];
                position -= 1;
            }
            self.sorted[position] = value;
        }
        self.sorted[len / 2]
    }

    fn reset_poly_history(&mut self) {
        self.frame.strings.clear();
        self.poly_prev_cents = [0.0; MAX_STRINGS];
        self.poly_prev_valid = [false; MAX_STRINGS];
        self.poly_silent = [0; MAX_STRINGS];
        for latch in &mut self.poly_latches {
            latch.reset();
        }
    }

    /// Magnitude at an arbitrary frequency, fitted through the three bins
    /// around the nearest one.  Linear magnitude interpolation biases the peak
    /// of a Hann-windowed partial toward the bin centre — ±44 cents of error
    /// at C2 — so the score curve is fitted instead.
    fn spectrum_at(&self, frequency: f32) -> f32 {
        let position = frequency / self.bin_width;
        if position < 0.0 {
            return 0.0;
        }
        let nearest = position.round() as usize;
        if nearest >= self.magnitude.len() {
            return 0.0;
        }
        let (left, center, right) = if nearest == 0 {
            (
                0.0,
                self.magnitude[0],
                self.magnitude.get(1).copied().unwrap_or(0.0),
            )
        } else if nearest + 1 >= self.magnitude.len() {
            (self.magnitude[nearest - 1], self.magnitude[nearest], 0.0)
        } else {
            (
                self.magnitude[nearest - 1],
                self.magnitude[nearest],
                self.magnitude[nearest + 1],
            )
        };
        let offset = position - nearest as f32;
        // Fit in the log domain: a Hann lobe is nearly parabolic in log
        // magnitude across its main lobe, where a linear-magnitude fit bends
        // off by several cents at the lowest notes.
        const FLOOR: f32 = 1e-9;
        let (left, center, right) = (
            left.max(FLOOR).ln(),
            center.max(FLOOR).ln(),
            right.max(FLOOR).ln(),
        );
        let curve = 0.5 * (left + right) - center;
        let slope = 0.5 * (right - left);
        (center + slope * offset + curve * offset * offset)
            .exp()
            .max(0.0)
    }

    /// The weighted sum of the first [`HARMONICS`] partials at `cents` from
    /// the string's target.  The caller supplies the weights: the presence
    /// table uses 1/sqrt(k) with the fundamental always full, while the pitch
    /// table uses k over the partials that belong to this string alone.
    fn string_score(&self, target: f32, cents: f32, weights: &[f32; HARMONICS]) -> f32 {
        let ratio = cents_to_ratio(cents);
        let mut score = 0.0;
        for harmonic in 1..=HARMONICS {
            score += weights[harmonic - 1] * self.spectrum_at(target * harmonic as f32 * ratio);
        }
        score
    }

    /// Best detune for one string under one weight table: a 1-cent coarse
    /// sweep over the ±60 window, a 0.1-cent sweep around the winner, then a
    /// parabola through the three fine points.
    fn fit_cents(&self, target: f32, weights: &[f32; HARMONICS]) -> f32 {
        let mut cents = 0.0f32;
        let mut best = 0.0f32;
        for candidate in -60..=60 {
            let value = self.string_score(target, candidate as f32, weights);
            if value > best {
                best = value;
                cents = candidate as f32;
            }
        }
        let coarse = cents;
        best = 0.0;
        for step in -10..=10 {
            let candidate = coarse + step as f32 * 0.1;
            let value = self.string_score(target, candidate, weights);
            if value > best {
                best = value;
                cents = candidate;
            }
        }
        let left = self.string_score(target, cents - 0.1, weights);
        let right = self.string_score(target, cents + 0.1, weights);
        let denominator = left - 2.0 * best + right;
        if denominator.abs() > 1e-12 {
            let offset = 0.5 * (left - right) / denominator;
            if offset.abs() <= 1.0 {
                cents += offset * 0.1;
            }
        }
        cents
    }

    /// The unweighted mean level on the partials at the measured detune.
    /// Unweighted on purpose: the weights steer the cents estimate, not the
    /// presence test.
    fn harmonic_level(&self, target: f32, cents: f32) -> f32 {
        let ratio = cents_to_ratio(cents);
        let total: f32 = (1..=HARMONICS)
            .map(|harmonic| self.spectrum_at(target * harmonic as f32 * ratio))
            .sum();
        total / HARMONICS as f32
    }

    /// The median level halfway between the partials: a null with no string in
    /// it.  The median, not the mean, because in a chord several midpoints are
    /// exactly another string's partial (C2's 1.5× is G2's fundamental, F3's
    /// midpoints are C3's harmonics); the median ignores the occupied points
    /// while a noise floor fills all six and reads about the same on both
    /// sides.
    fn inter_harmonic_level(&self, target: f32) -> f32 {
        let mut levels = [0.0f32; HARMONICS];
        for harmonic in 1..=HARMONICS {
            levels[harmonic - 1] = self.spectrum_at(target * (harmonic as f32 + 0.5));
        }
        levels.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        (levels[HARMONICS / 2 - 1] + levels[HARMONICS / 2]) * 0.5
    }

    #[allow(clippy::too_many_arguments)]
    fn analyse_poly(
        &mut self,
        interleaved: &[f32],
        frames: usize,
        rate: u32,
        preset: Option<&'static Tuning>,
        channel: TunerChannel,
        reference_hz: f32,
        tolerance: TunerTolerance,
        level_db: f32,
    ) {
        self.frame.strings.clear();
        let Some(tuning) = preset else {
            self.reset_poly_history();
            self.poly_strings = 0;
            self.poly_preset = None;
            return;
        };
        if tuning.strings.is_empty() || tuning.strings.len() > MAX_STRINGS {
            self.reset_poly_history();
            self.poly_strings = 0;
            self.poly_preset = None;
            return;
        }
        // A different preset is a different set of strings even when the count
        // matches (E standard and Drop D are both six): smoothing, silence and
        // the latches belong to the preset that produced them.
        if tuning.strings.len() != self.poly_strings || Some(tuning.id) != self.poly_preset {
            self.reset_poly_history();
            self.poly_strings = tuning.strings.len();
            self.poly_preset = Some(tuning.id);
        }

        let window = poly_window_len(rate, midi_to_hz(tuning.strings[0].1, reference_hz));
        if frames < window || interleaved.len() < frames * 2 {
            self.reset_poly_history();
            return;
        }
        let transform = (window * SPECTRUM_PADDING).min(MAX_TRANSFORM);
        if self.fft_len != transform {
            let mut planner = FftPlanner::<f32>::new();
            let fft = planner.plan_fft_forward(transform);
            self.fft_scratch = vec![Complex::new(0.0, 0.0); fft.get_inplace_scratch_len()];
            self.spectrum = vec![Complex::new(0.0, 0.0); transform];
            self.magnitude = vec![0.0; transform / 2];
            self.fft = Some(fft);
            self.fft_len = transform;
        }
        self.bin_width = rate as f32 / transform as f32;

        let start = frames - window;
        for sample in &mut self.spectrum {
            *sample = Complex::new(0.0, 0.0);
        }
        let mut finite = true;
        for index in 0..window {
            let sample = select_sample(interleaved, start + index, channel);
            finite &= sample.is_finite();
            let hann = 0.5 - 0.5 * (TAU * index as f32 / window as f32).cos();
            self.spectrum[index] = Complex::new(sample * hann, 0.0);
        }
        if !finite {
            self.reset_poly_history();
            return;
        }
        if let Some(fft) = self.fft.as_ref() {
            fft.process_with_scratch(&mut self.spectrum, &mut self.fft_scratch);
        }
        // Normalise to amplitude: a Hann-windowed sine of amplitude A peaks at
        // A * W / 4 in the raw transform, where W is the windowed length.  The
        // zero padding only samples that peak's skirt finer; it does not change
        // its height, so the divisor is the window, not the transform length.
        let scale = 4.0 / window as f32;
        for bin in 0..transform / 2 {
            self.magnitude[bin] = self.spectrum[bin].norm() * scale;
        }

        let count = tuning.strings.len();
        // One resolution cell: two lines closer than this land in the same
        // Hann lobe, so a partial within two cells of another string's
        // partial is not this string's alone.  The test is in hertz because a
        // cell is a fixed number of hertz, not a fixed number of cents.
        let cell_hz = rate as f32 / window as f32;
        // Two weight tables.  `presence` feeds the "is this string ringing"
        // test with a 1/sqrt(k) shape, so the fundamental always carries full
        // weight (in an octave pair it is exactly what the player plucked).
        // `pitch` feeds the cents estimate and keeps only the partials that
        // belong to this string alone, weighted by k: partial k is k times
        // finer in cents per bin, which is what makes a low string measurable
        // at all — at A0 on a 96 kHz window one bin spans some 340 cents.
        let mut presence = [[0.0f32; HARMONICS]; MAX_STRINGS];
        let mut pitch = [[0.0f32; HARMONICS]; MAX_STRINGS];
        for (index, (_, midi)) in tuning.strings.iter().enumerate() {
            let fundamental = midi_to_hz(*midi, reference_hz);
            let low = fundamental < LOW_STRING_HZ;
            for harmonic in 1..=HARMONICS {
                let partial = fundamental * harmonic as f32;
                let shared =
                    tuning
                        .strings
                        .iter()
                        .enumerate()
                        .any(|(other_index, (_, other_midi))| {
                            other_index != index
                                && (1..=HARMONICS).any(|other_harmonic| {
                                    let other = midi_to_hz(*other_midi, reference_hz)
                                        * other_harmonic as f32;
                                    (partial - other).abs() < 2.0 * cell_hz
                                })
                        });
                let base = 1.0 / (harmonic as f32).sqrt();
                presence[index][harmonic - 1] = if harmonic == 1 {
                    1.0
                } else if harmonic == 2 && low {
                    // The octave above; usually the next string's fundamental.
                    0.0
                } else if shared {
                    base * SHARED_WEIGHT
                } else {
                    base
                };
                // A shared partial is another string's line as well; in a
                // chord it sits between the two detunes and would bias the
                // estimate, so the pitch fit leaves it out.
                pitch[index][harmonic - 1] = if shared {
                    0.0
                } else if harmonic == 1 {
                    1.0
                } else {
                    harmonic as f32
                };
            }
        }

        let mut best_cents = [0.0f32; MAX_STRINGS];
        let mut best_scores = [0.0f32; MAX_STRINGS];
        let mut on_levels = [0.0f32; MAX_STRINGS];
        let mut off_levels = [0.0f32; MAX_STRINGS];
        let mut confidences = [0.0f32; MAX_STRINGS];
        let mut fundamentals = [0.0f32; MAX_STRINGS];
        let mut clean_scores = [0.0f32; MAX_STRINGS];
        for (index, (_, midi)) in tuning.strings.iter().enumerate() {
            let target = midi_to_hz(*midi, reference_hz);

            // The pitch fit keeps only this string's own partials, which is
            // what places the detune precisely.  It has two failure modes at
            // the bottom of a long-scale preset: its argmax can run off the
            // end of the sweep, and a string with no clean partial energy has
            // nothing to fit at all.  In both cases the presence fit — which
            // always keeps h1 — is used instead.  Only when the presence fit
            // is edge-pinned as well does the row stay silent; the level,
            // null, loudest-share and fundamental-share gates decide
            // independently.
            let mut cents = self.fit_cents(target, &pitch[index]);
            let mut score = self.string_score(target, cents, &presence[index]);
            let mut clean = self.string_score(target, cents, &pitch[index]);
            if cents.abs() >= EDGE_CENTS || clean < CLEAN_EVIDENCE * score {
                cents = self.fit_cents(target, &presence[index]);
                score = self.string_score(target, cents, &presence[index]);
                clean = self.string_score(target, cents, &pitch[index]);
            }

            let on_level = self.harmonic_level(target, cents);
            let off_level = self.inter_harmonic_level(target);
            best_cents[index] = cents;
            best_scores[index] = score;
            clean_scores[index] = clean;
            on_levels[index] = on_level;
            off_levels[index] = off_level;
            confidences[index] = if on_level > 0.0 {
                (1.0 - off_level / on_level).clamp(0.0, 1.0)
            } else {
                0.0
            };
            fundamentals[index] = self.spectrum_at(target * cents_to_ratio(cents));
        }
        // A string is only believed when it is close enough to the loudest one
        // to be part of the same strum: Hann sidelobes of another string are
        // 60 dB down but still produce a non-zero peak somewhere in the band.
        let mut loudest = 0.0f32;
        for score in best_scores.iter().take(count) {
            loudest = loudest.max(*score);
        }

        for (index, (name, midi)) in tuning.strings.iter().enumerate() {
            let target = midi_to_hz(*midi, reference_hz);
            let score = best_scores[index];
            let mut cents = best_cents[index];
            // A string that only shows up through another string's harmonic
            // has no fundamental of its own: the weighted partial sum can
            // still peak, so the fundamental has to carry a real share of it.
            let sounding = level_db > -50.0
                && score > 1e-4
                && on_levels[index] > NULL_RATIO * off_levels[index]
                && score > 0.05 * loudest
                && fundamentals[index] > 0.1 * score
                && best_cents[index].abs() < EDGE_CENTS;

            let was_sounding = self.poly_prev_valid[index] && self.poly_silent[index] == 0;
            if sounding {
                if was_sounding {
                    // Smooth only when the string was already ringing, so a
                    // fresh pluck snaps to its measured cents.
                    cents =
                        self.poly_prev_cents[index] + 0.35 * (cents - self.poly_prev_cents[index]);
                }
                self.poly_prev_cents[index] = cents;
                self.poly_prev_valid[index] = true;
                self.poly_silent[index] = 0;
            } else {
                let silent = self.poly_silent[index];
                self.poly_silent[index] = silent.saturating_add(1).min(SILENT_RESET);
                if self.poly_silent[index] >= SILENT_RESET {
                    self.poly_prev_valid[index] = false;
                    self.poly_prev_cents[index] = 0.0;
                }
            }

            let state = if sounding {
                self.poly_latches[index].update(cents, tolerance.cents())
            } else {
                self.poly_latches[index].reset();
                TuneState::Silent
            };
            self.frame.strings.push(StringReading {
                string: index,
                name,
                midi: *midi,
                target_hz: target,
                cents,
                level_db,
                confidence: confidences[index],
                sounding,
                harmonic_of: None,
                state,
            });
        }

        // Rank the sounding strings by how much fundamental they carry, then
        // mark the weaker ones that are only a multiple of a stronger one.
        let mut order = [0usize; MAX_STRINGS];
        let mut order_len = 0;
        for index in 0..count {
            if !self.frame.strings[index].sounding {
                continue;
            }
            let mut position = order_len;
            while position > 0 && fundamentals[order[position - 1]] < fundamentals[index] {
                order[position] = order[position - 1];
                position -= 1;
            }
            order[position] = index;
            order_len += 1;
        }
        for slot in 1..order_len {
            let weaker = order[slot];
            // A ghost carries its alleged pitch only through the stronger
            // string's harmonic series; a real string also has energy on the
            // partials that belong to it alone.  Without that check a played
            // high E in an 8-string reads as a ghost of the low E, because
            // its fundamental is the low string's fourth harmonic.
            if clean_scores[weaker] >= GHOST_CLEAN_RATIO * best_scores[weaker] {
                continue;
            }
            for stronger in &order[..slot] {
                if fundamentals[weaker] >= 0.6 * fundamentals[*stronger] {
                    continue;
                }
                let weaker_hz = midi_to_hz(tuning.strings[weaker].1, reference_hz);
                let stronger_hz = midi_to_hz(tuning.strings[*stronger].1, reference_hz);
                if harmonic_of(weaker_hz, stronger_hz) {
                    self.frame.strings[weaker].harmonic_of = Some(*stronger);
                    break;
                }
            }
        }
    }

    /// Synthesizes the preset's chord (or one A2 for chromatic) and runs it
    /// through the same analysis path, so Preview looks like a live bridge
    /// without opening hardware.  `elapsed` drives a slow detune wobble.
    pub(crate) fn preview(
        &mut self,
        preset: Option<&'static Tuning>,
        reference_hz: f32,
        tolerance: TunerTolerance,
        elapsed: f32,
    ) {
        let rate = if self.rate == 0 {
            DEFAULT_RATE
        } else {
            self.rate
        };
        let factor = ((rate / 12_000).clamp(1, 8)) as usize;
        let lowest_hz = match preset {
            Some(tuning) => midi_to_hz(tuning.strings[0].1, reference_hz),
            None => midi_to_hz(45, reference_hz),
        };
        let frames = (YIN_WINDOW * factor).max(poly_window_len(rate, lowest_hz));
        if self.preview_mono.len() != frames {
            self.preview_voice = vec![0.0; frames];
            self.preview_mono = vec![0.0; frames];
            self.preview_interleaved = vec![0.0; frames * 2];
        }

        let wobble = (elapsed * 0.9).sin();
        let mut tones = [(0.0f32, 0.0f32); MAX_STRINGS];
        let mut tone_count = 0;
        match preset {
            Some(tuning) => {
                for (index, (_, midi)) in tuning.strings.iter().enumerate() {
                    if index >= MAX_STRINGS {
                        break;
                    }
                    let cents = PREVIEW_DETUNES[index % PREVIEW_DETUNES.len()] + wobble;
                    tones[index] = (
                        midi_to_hz(*midi, reference_hz) * cents_to_ratio(cents),
                        0.18 / ((index + 1) as f32).sqrt(),
                    );
                    tone_count += 1;
                }
            }
            None => {
                // Chromatic: one A2, slightly sharp, so the reading sits off
                // center.
                tones[0] = (midi_to_hz(45, reference_hz) * cents_to_ratio(3.0), 0.18);
                tone_count = 1;
            }
        }
        // The mono stage listens to one string, not the chord: a strummed
        // chord is not one pitch, and YIN would lock onto a common
        // subharmonic (Drop C read as C1) instead of a string.
        let voice = match preset {
            Some(tuning) => (
                midi_to_hz(tuning.strings[0].1, reference_hz)
                    * cents_to_ratio(PREVIEW_DETUNES[0] + wobble),
                0.18,
            ),
            None => tones[0],
        };

        let mut voice_buffer = std::mem::take(&mut self.preview_voice);
        let mut chord_buffer = std::mem::take(&mut self.preview_mono);
        let mut interleaved = std::mem::take(&mut self.preview_interleaved);

        synthesize(&mut voice_buffer, &[voice], rate);
        interleave_into(&voice_buffer, &mut interleaved);
        let level_db = self.analyse_mono(
            &interleaved,
            frames,
            rate,
            preset,
            TunerChannel::Sum,
            reference_hz,
            tolerance,
        );

        synthesize(&mut chord_buffer, &tones[..tone_count], rate);
        interleave_into(&chord_buffer, &mut interleaved);
        self.analyse_poly(
            &interleaved,
            frames,
            rate,
            preset,
            TunerChannel::Sum,
            reference_hz,
            tolerance,
            level_db,
        );

        self.preview_voice = voice_buffer;
        self.preview_mono = chord_buffer;
        self.preview_interleaved = interleaved;
    }

    /// A fixed frame for headless screenshots: Drop C on the preview detunes.
    /// No hardware and no audio.
    pub(crate) fn demo(
        &mut self,
        preset: Option<&'static Tuning>,
        reference_hz: f32,
        tolerance: TunerTolerance,
    ) {
        let tuning = preset
            .or_else(|| tuning_by_id("drop-c"))
            .unwrap_or(&PRESETS[0]);

        self.reset_poly_history();
        self.frame.strings.clear();
        self.poly_strings = tuning.strings.len();
        self.poly_preset = Some(tuning.id);
        for (index, (name, midi)) in tuning.strings.iter().enumerate() {
            let cents = PREVIEW_DETUNES[index % PREVIEW_DETUNES.len()];
            // The demo is a real frame, not a pictures-only one: its states
            // come from the same band rules the live rows use, so a tighter
            // tolerance turns more of it amber.
            let state = classify(cents, tolerance.cents());
            let target_hz = midi_to_hz(*midi, reference_hz);
            self.frame.strings.push(StringReading {
                string: index,
                name,
                midi: *midi,
                target_hz,
                cents,
                level_db: -18.4,
                confidence: 0.9,
                sounding: true,
                harmonic_of: None,
                state,
            });
        }

        let first = &self.frame.strings[0];
        self.frame.mono = TunerReading {
            valid: true,
            frequency: first.target_hz * cents_to_ratio(first.cents),
            target_hz: first.target_hz,
            cents: first.cents,
            midi: first.midi,
            string: Some(0),
            confidence: 0.86,
            level_db: -18.4,
            state: first.state,
        };
    }
}

#[cfg(test)]
mod tests;
