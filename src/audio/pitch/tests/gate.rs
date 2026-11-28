//! One score row and the gate it is held to.  The always-on tests and
//! the release harness both use these.

use std::fmt;

use super::super::tight::{rate_scale, spec};
use super::super::{PitchSettings, PitchShifter, DEFAULT_DELAY_MS};
use super::material::{beats, render, Case, Kind};
use super::score;

/// How a row was rendered: the shifter, or an outside reference.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Engine {
    Tight,
    /// Rubber Band R3, offline.
    Reference,
}

impl fmt::Display for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Engine::Tight => write!(f, "Tight"),
            Engine::Reference => write!(f, "R3"),
        }
    }
}

/// The analysis length that decides what counts as resolved: 4096 samples
/// at 48 kHz, partials about 23 Hz apart, scaled with the rate.  Tight has
/// no resolution of its own (it moves the whole waveform), but its splices
/// spread each partial into sidebands at the splice rate.
pub(super) fn scoring_na(rate: u32) -> usize {
    4096 * rate_scale(rate)
}

/// The shifter on at `semitones`.
pub(super) fn on(semitones: i8) -> PitchSettings {
    PitchSettings {
        enabled: true,
        semitones,
        ..PitchSettings::default()
    }
}

/// The scores of one case × engine × shift.
#[derive(Clone, Debug)]
pub(super) struct Row {
    pub(super) case: &'static str,
    pub(super) kind: Kind,
    pub(super) engine: Engine,
    pub(super) semitones: i8,
    pub(super) rate: u32,
    /// Score 1: resolved partials scored, the rest, median and 95th
    /// percentile pitch error in cents.
    pub(super) scored: usize,
    pub(super) unresolved: usize,
    pub(super) median: f64,
    pub(super) p95: f64,
    /// Score 2, dB against the loudest target partial.
    pub(super) leftover: f64,
    /// Score 3, dB.
    pub(super) level: f64,
    /// Score 4, dB.
    pub(super) distance: f64,
    /// Score 5: the worst onset's rise time (output, input), ms, and the
    /// pre-echo (output, input), dB, of the onset whose output is furthest
    /// over its allowance.
    pub(super) attack: Option<(f64, f64, f64, f64)>,
    /// Score 5, release: the worst stop's time (output, reference), ms.
    pub(super) release: Option<(f64, f64)>,
    /// Score 6: the mean and the largest roughness above the reference, dB.
    pub(super) roughness: Option<(f64, f64)>,
    /// Score 7: the measured and the reported delay, samples.
    pub(super) delay: Option<(usize, u32)>,
    /// A moving note's pitch track: median and 95th percentile, cents.
    pub(super) track: Option<(f64, f64)>,
    /// The beat score: the mean and the lowest correlation of a beating
    /// partial's level track with the reference's, and how many.
    pub(super) beat: Option<(f64, f64, usize)>,
    /// The flutter score: the resolved partials' mean frequency wander,
    /// cents, of the output and its excess over the reference's.
    pub(super) flutter: Option<(f64, f64)>,
}

/// The ratio of a shift in semitones.
pub(super) fn ratio(semitones: i8) -> f64 {
    2f64.powf(semitones as f64 / 12.0)
}

/// Runs `input` through a shifter at `semitones` from slot `slot`, in
/// blocks of `block`.  Returns the output with the reported latency
/// removed, and that latency.
pub(super) fn run(
    input: &[f32],
    rate: u32,
    semitones: i8,
    slot: usize,
    block: usize,
) -> (Vec<f32>, u32) {
    run_delay(input, rate, semitones, DEFAULT_DELAY_MS, slot, block)
}

/// [`run`] with the delay set to `delay_ms`.
pub(super) fn run_delay(
    input: &[f32],
    rate: u32,
    semitones: i8,
    delay_ms: f32,
    slot: usize,
    block: usize,
) -> (Vec<f32>, u32) {
    let settings = PitchSettings {
        delay_ms,
        ..on(semitones)
    };
    run_settings(input, rate, settings, slot, block)
}

/// [`run`] with any settings.
pub(super) fn run_settings(
    input: &[f32],
    rate: u32,
    settings: PitchSettings,
    slot: usize,
    block: usize,
) -> (Vec<f32>, u32) {
    let mut shifter = PitchShifter::new(rate, slot);
    shifter.update(settings, rate);
    let latency = shifter.latency_samples();
    let total = input.len() + latency as usize;
    let mut out = Vec::with_capacity(total);
    let mut buf = vec![[0.0f32; 4]; block];
    let mut i = 0;
    while i < total {
        let frames = block.min(total - i);
        for (j, frame) in buf[..frames].iter_mut().enumerate() {
            let x = input.get(i + j).copied().unwrap_or(0.0);
            *frame = [x, x, 0.0, 0.0];
        }
        shifter.begin_block();
        shifter.process(&mut buf, frames);
        out.extend(buf[..frames].iter().map(|f| f[0]));
        i += frames;
    }
    (out.split_off(latency as usize), latency)
}

/// Scores `out` (lined up with `input`) for `case`.  `raw_latency` is the
/// delay the engine reported, for the click train's latency score.
pub(super) fn evaluate(
    case: &Case,
    engine: Engine,
    semitones: i8,
    rate: u32,
    input: &[f32],
    out: &[f32],
    raw: Option<(&[f32], u32)>,
) -> Row {
    let ratio = ratio(semitones);
    let reference = render(case, rate, ratio);
    let na = scoring_na(rate);
    let mut row = Row {
        case: case.name,
        kind: case.kind,
        engine,
        semitones,
        rate,
        scored: 0,
        unresolved: 0,
        median: 0.0,
        p95: 0.0,
        leftover: f64::NEG_INFINITY,
        level: 0.0,
        distance: 0.0,
        attack: None,
        release: None,
        roughness: None,
        delay: None,
        track: None,
        beat: None,
        flutter: None,
    };
    match case.kind {
        Kind::Notes | Kind::Chord | Kind::Listening => {
            let partials = score::partial_score(case, out, rate, ratio, na);
            let (median, p95) = score::median_p95(&partials.errors);
            row.scored = partials.errors.len();
            row.unresolved = partials.unresolved;
            row.median = median;
            row.p95 = p95;
            row.leftover = partials.leftover_db;
            row.level = score::level_error(case, input, out, rate);
            row.distance = score::spectral_distance(case, out, &reference, rate);
            if beats(case) {
                let c = score::beat_correlations(case, input, out, rate, ratio, na);
                if !c.is_empty() {
                    let mean = c.iter().sum::<f64>() / c.len() as f64;
                    let min = c.iter().copied().fold(f64::INFINITY, f64::min);
                    row.beat = Some((mean, min, c.len()));
                }
            }
            row.flutter =
                score::flutter(case, out, &reference, rate, ratio, na).map(|(o, r)| (o, o - r));
            if case.kind == Kind::Chord {
                let rough = score::roughness(case, out, &reference, rate, ratio, na);
                if !rough.is_empty() {
                    let excess: Vec<f64> = rough.iter().map(|(o, r)| o - r).collect();
                    let mean = excess.iter().sum::<f64>() / excess.len() as f64;
                    let max = excess.iter().copied().fold(f64::NEG_INFINITY, f64::max);
                    row.roughness = Some((mean, max));
                }
            }
        }
        Kind::Moving => {
            row.level = score::level_error(case, input, out, rate);
            row.distance = score::spectral_distance(case, out, &reference, rate);
            let errors = score::moving_errors(case, out, rate, ratio, na);
            row.track = Some(score::median_p95(&errors));
        }
        Kind::Chugs => {
            let env_out = score::envelope(out, rate);
            let env_in = score::envelope(input, rate);
            let mut worst = (0.0f64, 0.0f64, f64::NEG_INFINITY, f64::NEG_INFINITY);
            for onset in score::onsets(case, rate) {
                let (rise, pre) = score::attack(&env_out, rate, onset);
                let (rise_in, pre_in) = score::attack(&env_in, rate, onset);
                if rise - rise_in > worst.0 - worst.1 {
                    worst.0 = rise;
                    worst.1 = rise_in;
                }
                if pre - pre_allowed(pre_in) > worst.2 - pre_allowed(worst.3) {
                    worst.2 = pre;
                    worst.3 = pre_in;
                }
            }
            row.attack = Some(worst);
            row.level = score::level_error(case, input, out, rate);
        }
        Kind::Stops => {
            let env_out = score::envelope(out, rate);
            let env_ref = score::envelope(&reference, rate);
            let mut worst = (0.0f64, 0.0f64);
            for note in &case.notes {
                let stop = ((note.stop.unwrap_or(0.0) + 0.001) * rate as f64) as usize;
                let t = score::release(&env_out, rate, stop);
                let t_ref = score::release(&env_ref, rate, stop);
                if t - t_ref > worst.0 - worst.1 {
                    worst = (t, t_ref);
                }
            }
            row.release = Some(worst);
        }
        Kind::Signal => {
            // A sweep's envelope is flat: only the clicks and the impulse
            // have a delay to measure.
            if let (Some((raw, latency)), true) = (raw, case.name != "sweep") {
                let max_lag = latency as usize + spec(rate, DEFAULT_DELAY_MS).hop * 2;
                row.delay = Some((score::measured_delay(input, raw, rate, max_lag), latency));
            }
        }
    }
    row
}

/// Median and 95th percentile, cents, for single notes and power
/// chords at the drops.  A period-synchronous splice pulls inharmonic
/// partial `k` toward `k·f0` by `(f_k − k·f0)(1 − r)`.  The octave
/// down's other scores, and the up-shifts, stay reported only.
fn in_pitch_gate(semitones: i8) -> Option<(f64, f64)> {
    (-12..=-1).contains(&semitones).then_some((8.0, 20.0))
}

/// The lowest mean beat score (the correlation of a beating partial's level
/// with the input's) a row in the pitch gate may have.
const BEAT_GATE: f64 = 0.8;

/// The most pre-echo an onset may have: −30 dB, or 3 dB over the input's
/// own when the previous note's tail already reaches that (fast chugs).
fn pre_allowed(pre_in: f64) -> f64 {
    (pre_in + 3.0).max(-30.0)
}

/// What the row fails of §4.3, empty when it passes.
pub(super) fn failures(row: &Row) -> Vec<String> {
    let mut failed = Vec::new();
    // A full chord choruses by design (§3.5): its rows are reported, not
    // held.
    if row.engine == Engine::Reference || row.kind == Kind::Chord {
        return failed;
    }
    let pitch_gate = in_pitch_gate(row.semitones);
    // Attack, release, level and beat stay on the band the tuning was
    // checked at; the octave down's are reported only.
    let held = (-7..=-1).contains(&row.semitones);
    if let (Some((mean, _, _)), true) = (row.beat, held) {
        if mean < BEAT_GATE {
            failed.push(format!("beat {mean:.2} under {BEAT_GATE}"));
        }
    }
    if row.kind == Kind::Notes {
        if let Some((median, p95)) = pitch_gate {
            if row.scored > 0 && (row.median > median || row.p95 > p95) {
                failed.push(format!(
                    "pitch {:.1}/{:.1} c over {median}/{p95}",
                    row.median, row.p95
                ));
            }
            if row.leftover > -30.0 {
                failed.push(format!("leftover {:.1} dB over -30", row.leftover));
            }
        }
    }
    if held && matches!(row.kind, Kind::Notes | Kind::Moving) && row.level.abs() > 1.0 {
        failed.push(format!("level {:.2} dB outside 1", row.level));
    }
    if let Some((rise, rise_in, pre, pre_in)) = row.attack.filter(|_| held) {
        let allowed = (rise_in + 5.0).max(1.5 * rise_in);
        if rise > allowed {
            failed.push(format!("rise {rise:.1} ms over {allowed:.1}"));
        }
        let allowed = pre_allowed(pre_in);
        if pre > allowed {
            failed.push(format!("pre-echo {pre:.1} dB over {allowed:.1}"));
        }
    }
    if let Some((t, t_ref)) = row.release.filter(|_| held) {
        if t > t_ref + 20.0 {
            failed.push(format!("release {t:.1} ms over {:.1}", t_ref + 20.0));
        }
    }
    if let Some((measured, reported)) = row.delay {
        let hop = spec(row.rate, DEFAULT_DELAY_MS).hop as i64;
        let error = measured as i64 - reported as i64;
        if (row.semitones == 0 && error != 0) || error.abs() > hop / 2 {
            failed.push(format!("delay {measured} against {reported}"));
        }
    }
    failed
}

impl Row {
    /// The table header [`Row`]'s `Display` lines up with.
    pub(super) const HEADER: &'static str = "case      engine       st   rate | n/unres  med   p95   | left   level  dist  | rise/in   pre/in      | rel/ref     | rough avg/max | delay/rep  | beat avg/min/n | flut/+ref    | track";
}

impl fmt::Display for Row {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:9} {:12} {:+3} {:6} | {:3}/{:<3} {:5.2} {:5.2} | {:6.1} {:+5.2} {:5.2} |",
            self.case,
            self.engine.to_string(),
            self.semitones,
            self.rate,
            self.scored,
            self.unresolved,
            self.median,
            self.p95,
            self.leftover.max(-199.0),
            self.level,
            self.distance,
        )?;
        match self.attack {
            Some((rise, rise_in, pre, pre_in)) => {
                write!(f, " {rise:4.1}/{rise_in:<4.1} {pre:5.1}/{pre_in:<5.1} |")?
            }
            None => write!(f, " {:9} {:11} |", "", "")?,
        }
        match self.release {
            Some((t, t_ref)) => write!(f, " {t:5.1}/{t_ref:<5.1} |")?,
            None => write!(f, " {:11} |", "")?,
        }
        match self.roughness {
            Some((mean, max)) => write!(f, " {mean:5.2}/{max:<6.2}  |")?,
            None => write!(f, " {:13} |", "")?,
        }
        match self.delay {
            Some((measured, reported)) => write!(f, " {measured:5}/{reported:<5}|")?,
            None => write!(f, " {:11}|", "")?,
        }
        match self.beat {
            Some((mean, min, n)) => write!(f, " {mean:5.2}/{min:5.2}/{n:<2} |")?,
            None => write!(f, " {:14} |", "")?,
        }
        match self.flutter {
            Some((c, excess)) => write!(f, " {c:5.2}/{excess:+6.2} |")?,
            None => write!(f, " {:12} |", "")?,
        }
        if let Some((median, p95)) = self.track {
            write!(f, " {median:.2}/{p95:.2}")?;
        }
        let failed = failures(self);
        if !failed.is_empty() {
            write!(f, "  FAIL: {}", failed.join("; "))?;
        }
        Ok(())
    }
}
