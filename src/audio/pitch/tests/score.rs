//! The phase 0 scores (§4.2 of the plan), computed against the synth's
//! known partials.  Every function takes the output already lined up with
//! the input (the reported latency removed).

use std::f64::consts::TAU;

use rustfft::num_complex::Complex;
use rustfft::FftPlanner;

use super::material::{partials_at, partials_spread_at, Case, Kind};

/// A Hann-windowed magnitude spectrum of `signal[start..start + len]`,
/// zero-padded to `nfft`.  Magnitudes are scaled so a sine of amplitude `a`
/// peaks at about `a`.
pub(super) fn spectrum(signal: &[f32], start: usize, len: usize, nfft: usize) -> Vec<f64> {
    let mut buf = vec![Complex::new(0.0f64, 0.0); nfft];
    let mut sum = 0.0;
    for (i, slot) in buf.iter_mut().enumerate().take(len.min(nfft)) {
        let w = 0.5 - 0.5 * (TAU * (i as f64 + 0.5) / len as f64).cos();
        sum += w;
        let x = signal.get(start + i).copied().unwrap_or(0.0) as f64;
        *slot = Complex::new(x * w, 0.0);
    }
    FftPlanner::new().plan_fft_forward(nfft).process(&mut buf);
    buf[..=nfft / 2]
        .iter()
        .map(|c| c.norm() * 2.0 / sum)
        .collect()
}

/// The largest local maximum within `search` Hz of `freq`, refined by a
/// parabola on the log magnitude: its frequency and magnitude.
pub(super) fn peak_near(
    spec: &[f64],
    nfft: usize,
    rate: f64,
    freq: f64,
    search: f64,
) -> Option<(f64, f64)> {
    let bin_hz = rate / nfft as f64;
    let lo = (((freq - search) / bin_hz).floor() as isize).max(1) as usize;
    let hi = (((freq + search) / bin_hz).ceil() as usize).min(spec.len() - 2);
    let mut best: Option<usize> = None;
    for k in lo..=hi {
        if spec[k] >= spec[k - 1]
            && spec[k] >= spec[k + 1]
            && best.is_none_or(|b| spec[k] > spec[b])
        {
            best = Some(k);
        }
    }
    let k = best?;
    let (a, b, c) = (
        (spec[k - 1] + 1e-12).ln(),
        (spec[k] + 1e-12).ln(),
        (spec[k + 1] + 1e-12).ln(),
    );
    let denom = a - 2.0 * b + c;
    let delta = if denom < 0.0 {
        0.5 * (a - c) / denom
    } else {
        0.0
    };
    let mag = (b - 0.25 * (a - c) * delta).exp();
    Some(((k as f64 + delta) * bin_hz, mag))
}

/// The power-weighted centre of `spec` within `reach` Hz of `freq`, and the
/// largest magnitude there.
fn centroid(spec: &[f64], nfft: usize, rate: f64, freq: f64, reach: f64) -> Option<(f64, f64)> {
    let bin_hz = rate / nfft as f64;
    let lo = (((freq - reach) / bin_hz).floor() as isize).max(1) as usize;
    let hi = (((freq + reach) / bin_hz).ceil() as usize).min(spec.len() - 1);
    let mut power = 0.0;
    let mut moment = 0.0;
    let mut peak = 0.0f64;
    for (k, &m) in spec.iter().enumerate().take(hi + 1).skip(lo) {
        power += m * m;
        moment += m * m * k as f64;
        peak = peak.max(m);
    }
    (power > 0.0).then(|| (moment / power * bin_hz, peak))
}

/// Whether `freq` is at least `min_hz` from DC and from every frequency in
/// `others` (itself excluded by index `skip` in the target list).
fn isolated(
    freq: f64,
    min_hz: f64,
    targets: &[(f64, f64)],
    skip: usize,
    sources: &[(f64, f64)],
) -> bool {
    freq >= min_hz
        && targets
            .iter()
            .enumerate()
            .all(|(i, &(f, _))| i == skip || (f - freq).abs() >= min_hz)
        && sources.iter().all(|&(f, _)| (f - freq).abs() >= min_hz)
}

/// Score 1 and 2 together: the pitch error (cents) of every resolved
/// target partial loud enough to measure, how many were not resolved, and
/// the worst leftover (dB against the loudest target partial).
pub(super) struct PartialScore {
    pub(super) errors: Vec<f64>,
    pub(super) unresolved: usize,
    pub(super) leftover_db: f64,
}

/// The measurement length for the sustain spectrum.
const MEASURE_NFFT: usize = 1 << 15;

pub(super) fn partial_score(
    case: &Case,
    out: &[f32],
    rate: u32,
    ratio: f64,
    na: usize,
) -> PartialScore {
    let fs = rate as f64;
    let (from, to) = case.sustain;
    let start = (from * fs) as usize;
    let len = (((to - from) * fs) as usize).min(MEASURE_NFFT);
    let centre = from + len as f64 / fs / 2.0;
    let nfft = MEASURE_NFFT.max(len.next_power_of_two());
    let spec = spectrum(out, start, len, nfft);
    let merged = partials_spread_at(case, ratio, centre);
    let targets: Vec<(f64, f64)> = merged.iter().map(|p| (p.freq, p.amp)).collect();
    let sources = partials_at(case, 1.0, centre);
    let min_hz = 2.0 * fs / na as f64;
    let loudest = targets.iter().map(|p| p.1).fold(0.0, f64::max);
    let mut errors = Vec::new();
    let mut unresolved = 0;
    // The measurement's own resolution: its Hann lobe reaches two bins of
    // the measured length either side.
    let lobe_hz = 2.0 * fs / len as f64;
    for (i, &(freq, amp)) in targets.iter().enumerate() {
        if freq >= fs * 0.45 || amp < loudest * 0.01 {
            continue;
        }
        if !isolated(freq, min_hz, &targets, i, &sources) {
            unresolved += 1;
            continue;
        }
        let width = merged[i].width;
        if width > 0.0 {
            // A beating pair (or a unison): its power-weighted centre over
            // its components and the measurement's lobe, as the target's.
            let reach = width + lobe_hz + freq * (2f64.powf(0.25 / 12.0) - 1.0);
            let error = match centroid(&spec, nfft, fs, freq, reach.min(min_hz * 0.5)) {
                Some((f, mag)) if mag > amp * 0.1 => 1200.0 * (f / freq).log2(),
                _ => 100.0,
            };
            errors.push(error);
            continue;
        }
        // Search a quarter tone either side, never past half the gap.
        let search = (freq * (2f64.powf(0.5 / 12.0) - 1.0)).min(min_hz * 0.5);
        match peak_near(&spec, nfft, fs, freq, search) {
            Some((f, mag)) if mag > amp * 0.1 => {
                errors.push(1200.0 * (f / freq).log2());
            }
            _ => errors.push(100.0),
        }
    }
    let mut leftover_db = f64::NEG_INFINITY;
    // Against the loudest target partial as synthesised: the level score
    // keeps the output there.
    let reference = loudest;
    for &(freq, amp) in &sources {
        if freq >= fs * 0.45 || amp < loudest * 0.01 || freq < min_hz {
            continue;
        }
        if targets.iter().any(|&(f, _)| (f - freq).abs() < min_hz) {
            continue;
        }
        let bin = (freq / fs * nfft as f64).round() as usize;
        let mag = spec[bin.saturating_sub(2)..=(bin + 2).min(spec.len() - 1)]
            .iter()
            .copied()
            .fold(0.0, f64::max);
        leftover_db = leftover_db.max(20.0 * (mag / reference).log10());
    }
    PartialScore {
        errors,
        unresolved,
        leftover_db,
    }
}

/// The median and the 95th percentile of `|values|`.
pub(super) fn median_p95(values: &[f64]) -> (f64, f64) {
    if values.is_empty() {
        return (0.0, 0.0);
    }
    let mut sorted: Vec<f64> = values.iter().map(|v| v.abs()).collect();
    sorted.sort_by(f64::total_cmp);
    let at = |q: f64| sorted[((sorted.len() - 1) as f64 * q).round() as usize];
    (at(0.5), at(0.95))
}

/// RMS of `signal[from..to]` (seconds).
pub(super) fn rms(signal: &[f32], rate: u32, from: f64, to: f64) -> f64 {
    let a = (from * rate as f64) as usize;
    let b = ((to * rate as f64) as usize).min(signal.len());
    let sum: f64 = signal[a..b].iter().map(|&x| (x as f64) * (x as f64)).sum();
    (sum / (b - a).max(1) as f64).sqrt()
}

/// Score 3: the output level against the input's over the sustain, dB.
pub(super) fn level_error(case: &Case, input: &[f32], out: &[f32], rate: u32) -> f64 {
    let (from, to) = if case.sustain.1 > case.sustain.0 {
        case.sustain
    } else {
        (0.05, case.seconds - 0.05)
    };
    20.0 * (rms(out, rate, from, to) / rms(input, rate, from, to).max(1e-12)).log10()
}

/// Score 4: the log-spectral distance to the reference over the sustain, dB.
pub(super) fn spectral_distance(case: &Case, out: &[f32], reference: &[f32], rate: u32) -> f64 {
    let fs = rate as f64;
    let (from, to) = if case.sustain.1 > case.sustain.0 {
        case.sustain
    } else {
        (0.05, case.seconds - 0.05)
    };
    let frame = 2048 * (rate as usize / 48_000).max(1);
    let hop = frame / 4;
    let mut start = (from * fs) as usize;
    let end = ((to * fs) as usize).min(out.len());
    let mut total = 0.0;
    let mut frames = 0;
    while start + frame <= end {
        let o = spectrum(out, start, frame, frame);
        let r = spectrum(reference, start, frame, frame);
        let max = r.iter().copied().fold(0.0, f64::max);
        let floor = max * 1e-3;
        let mut sum = 0.0;
        let mut bins = 0;
        for (a, b) in o.iter().zip(&r) {
            if *b > floor {
                let d = 20.0 * ((a + floor * 0.1) / (b + floor * 0.1)).log10();
                sum += d * d;
                bins += 1;
            }
        }
        if bins > 0 {
            total += (sum / bins as f64).sqrt();
            frames += 1;
        }
        start += hop;
    }
    total / frames.max(1) as f64
}

/// A 1 ms RMS envelope.
pub(super) fn envelope(signal: &[f32], rate: u32) -> Vec<f64> {
    let width = (rate as usize / 1000).max(1);
    let mut out = vec![0.0; signal.len()];
    let mut acc = 0.0;
    for i in 0..signal.len() {
        acc += (signal[i] as f64).powi(2);
        if i >= width {
            acc -= (signal[i - width] as f64).powi(2);
        }
        out[i] = (acc.max(0.0) / width as f64).sqrt();
    }
    out
}

/// Score 5 on one onset: the 10–90 % rise time (ms) of `env` from `onset`
/// (samples), and the pre-echo (dB): the energy in the 10 ms before against
/// the onset's peak.
pub(super) fn attack(env: &[f64], rate: u32, onset: usize) -> (f64, f64) {
    let ms = rate as usize / 1000;
    let window = &env[onset..(onset + 30 * ms).min(env.len())];
    let peak = window.iter().copied().fold(0.0, f64::max);
    let first = |level: f64| window.iter().position(|&v| v >= level).unwrap_or(0);
    let rise = (first(0.9 * peak) as f64 - first(0.1 * peak) as f64) / ms as f64;
    let before = &env[onset.saturating_sub(10 * ms)..onset.saturating_sub(ms / 2)];
    let pre = before.iter().map(|v| v * v).sum::<f64>() / before.len().max(1) as f64;
    (rise, 10.0 * (pre / (peak * peak).max(1e-24)).log10())
}

/// Score 5, release: how long (ms) `env` takes after `stop` (samples) to
/// fall 40 dB under its level at the stop.
pub(super) fn release(env: &[f64], rate: u32, stop: usize) -> f64 {
    let ms = rate as usize / 1000;
    let level = env[stop.saturating_sub(5 * ms)..stop]
        .iter()
        .copied()
        .fold(0.0, f64::max);
    let floor = level * 0.01;
    let after = &env[stop..];
    let t = after.iter().position(|&v| v < floor).unwrap_or(after.len());
    t as f64 / ms as f64
}

/// Score 6: per resolved target partial, its amplitude in dB over the
/// sustain at a 5 ms hop with the straight-line trend removed; the standard
/// deviation of what is left.  Returns one value per partial for `out` and
/// `reference` alike.
pub(super) fn roughness(
    case: &Case,
    out: &[f32],
    reference: &[f32],
    rate: u32,
    ratio: f64,
    na: usize,
) -> Vec<(f64, f64)> {
    let fs = rate as f64;
    let (from, to) = case.sustain;
    let centre = (from + to) / 2.0;
    let targets = partials_at(case, ratio, centre);
    let sources = partials_at(case, 1.0, centre);
    let min_hz = 2.0 * fs / na as f64;
    let loudest = targets.iter().map(|p| p.1).fold(0.0, f64::max);
    let mut result = Vec::new();
    for (i, &(freq, amp)) in targets.iter().enumerate() {
        if freq >= fs * 0.45
            || amp < loudest * 0.03
            || !isolated(freq, min_hz, &targets, i, &sources)
        {
            continue;
        }
        let track = |signal: &[f32]| detrended_sd(&level_track(signal, rate, freq, from, to));
        result.push((track(out), track(reference)));
    }
    result
}

/// A partial's level in dB over `from..to` seconds at a 5 ms hop: the
/// signal demodulated at `freq` under a 60 ms Hann, whose first null sits
/// well inside the resolved spacing and which passes beats of a few Hz.
fn level_track(signal: &[f32], rate: u32, freq: f64, from: f64, to: f64) -> Vec<f64> {
    let fs = rate as f64;
    let width = (0.06 * fs) as usize;
    let hop = (0.005 * fs) as usize;
    let window: Vec<f64> = (0..width)
        .map(|j| 0.5 - 0.5 * (TAU * (j as f64 + 0.5) / width as f64).cos())
        .collect();
    let w = TAU * freq / fs;
    let mut levels = Vec::new();
    let mut start = (from * fs) as usize;
    let end = ((to * fs) as usize).min(signal.len());
    while start + width <= end {
        let mut acc = Complex::new(0.0, 0.0);
        for (j, win) in window.iter().enumerate() {
            let n = (start + j) as f64;
            acc += Complex::from_polar(signal[start + j] as f64 * win, -w * n);
        }
        levels.push(20.0 * (acc.norm() + 1e-12).log10());
        start += hop;
    }
    levels
}

/// The beat score: per resolved, beating partial whose beat makes at least
/// one cycle in the sustain and moves the input's level by at least 1 dB
/// (SD), the correlation of the output's level track at the target with the
/// input's at the source, each with its straight-line decay trend removed.
/// Against the input, not the reference: a shifter keeps the envelope in
/// the input's time, so the beat keeps its rate (the reference, rendered at
/// the target pitch, beats `r` times as fast).  1 is the beat kept as it
/// is; 0 is a beat lost (or one the shifter made up).
pub(super) fn beat_correlations(
    case: &Case,
    input: &[f32],
    out: &[f32],
    rate: u32,
    ratio: f64,
    na: usize,
) -> Vec<f64> {
    let fs = rate as f64;
    let (from, to) = case.sustain;
    let centre = (from + to) / 2.0;
    // The same groups in the same order at both pitches.
    let merged = partials_spread_at(case, ratio, centre);
    let source = partials_spread_at(case, 1.0, centre);
    let targets: Vec<(f64, f64)> = merged.iter().map(|p| (p.freq, p.amp)).collect();
    let sources: Vec<(f64, f64)> = source.iter().map(|p| (p.freq, p.amp)).collect();
    let min_hz = 2.0 * fs / na as f64;
    let loudest = targets.iter().map(|p| p.1).fold(0.0, f64::max);
    let mut result = Vec::new();
    for (i, (partial, from_partial)) in merged.iter().zip(&source).enumerate() {
        if partial.freq >= fs * 0.45
            || partial.amp < loudest * 0.03
            || from_partial.width * (to - from) < 1.0
            || !isolated(partial.freq, min_hz, &targets, i, &sources)
            || !isolated(from_partial.freq, min_hz, &sources, i, &[])
        {
            continue;
        }
        let r = detrended(&level_track(input, rate, from_partial.freq, from, to));
        let sd = (r.iter().map(|v| v * v).sum::<f64>() / r.len().max(1) as f64).sqrt();
        if sd < 1.0 {
            continue;
        }
        let o = detrended(&level_track(out, rate, partial.freq, from, to));
        let cross: f64 = o.iter().zip(&r).map(|(a, b)| a * b).sum();
        let norm =
            (o.iter().map(|v| v * v).sum::<f64>() * r.iter().map(|v| v * v).sum::<f64>()).sqrt();
        result.push(if norm > 0.0 { cross / norm } else { 0.0 });
    }
    result
}

/// The flutter score: per resolved target partial, how far its frequency
/// wanders over the sustain, cents (RMS about its mean), demodulated at a
/// 5 ms hop under a 60 ms Hann as [`roughness`] is; the mean over the
/// partials, for `out` and `reference`.  A time-domain shifter's splice
/// moves a partial's phase when the heads are not in phase for it, which is
/// what a full chord's chorusing is: it shows here, where an amplitude
/// score sees little.
pub(super) fn flutter(
    case: &Case,
    out: &[f32],
    reference: &[f32],
    rate: u32,
    ratio: f64,
    na: usize,
) -> Option<(f64, f64)> {
    let fs = rate as f64;
    let (from, to) = case.sustain;
    let centre = (from + to) / 2.0;
    let targets = partials_at(case, ratio, centre);
    let sources = partials_at(case, 1.0, centre);
    let min_hz = 2.0 * fs / na as f64;
    let loudest = targets.iter().map(|p| p.1).fold(0.0, f64::max);
    let hop_seconds = 0.005;
    let deviation = |signal: &[f32], freq: f64| {
        let phases = phase_track(signal, rate, freq, from, to);
        let cents: Vec<f64> = phases
            .windows(2)
            .map(|w| {
                let mut d = w[1] - w[0];
                d -= TAU * (d / TAU).round();
                let f = freq + d / (TAU * hop_seconds);
                1200.0 * (f.max(1e-3) / freq).log2()
            })
            .collect();
        let mean = cents.iter().sum::<f64>() / cents.len().max(1) as f64;
        (cents.iter().map(|c| (c - mean).powi(2)).sum::<f64>() / cents.len().max(1) as f64).sqrt()
    };
    let mut sum = (0.0, 0.0);
    let mut n = 0;
    for (i, &(freq, amp)) in targets.iter().enumerate() {
        if freq >= fs * 0.45
            || amp < loudest * 0.03
            || !isolated(freq, min_hz, &targets, i, &sources)
        {
            continue;
        }
        sum.0 += deviation(out, freq);
        sum.1 += deviation(reference, freq);
        n += 1;
    }
    (n > 0).then(|| (sum.0 / n as f64, sum.1 / n as f64))
}

/// A partial's phase over `from..to` seconds at a 5 ms hop, demodulated
/// at `freq` under a 60 ms Hann (as [`level_track`]), less the phase
/// `freq` itself turns.
fn phase_track(signal: &[f32], rate: u32, freq: f64, from: f64, to: f64) -> Vec<f64> {
    let fs = rate as f64;
    let width = (0.06 * fs) as usize;
    let hop = (0.005 * fs) as usize;
    let window: Vec<f64> = (0..width)
        .map(|j| 0.5 - 0.5 * (TAU * (j as f64 + 0.5) / width as f64).cos())
        .collect();
    let w = TAU * freq / fs;
    let mut phases = Vec::new();
    let mut start = (from * fs) as usize;
    let end = ((to * fs) as usize).min(signal.len());
    while start + width <= end {
        let mut acc = Complex::new(0.0, 0.0);
        for (j, win) in window.iter().enumerate() {
            let n = (start + j) as f64;
            acc += Complex::from_polar(signal[start + j] as f64 * win, -w * n);
        }
        phases.push(acc.arg());
        start += hop;
    }
    phases
}

/// The standard deviation of `values` around their least-squares line.
fn detrended_sd(values: &[f64]) -> f64 {
    let rest = detrended(values);
    (rest.iter().map(|v| v * v).sum::<f64>() / rest.len().max(1) as f64).sqrt()
}

/// `values` less their least-squares line.
fn detrended(values: &[f64]) -> Vec<f64> {
    let n = values.len() as f64;
    if n < 3.0 {
        return vec![0.0; values.len()];
    }
    let mean_x = (n - 1.0) / 2.0;
    let mean_y = values.iter().sum::<f64>() / n;
    let mut sxy = 0.0;
    let mut sxx = 0.0;
    for (i, &y) in values.iter().enumerate() {
        let dx = i as f64 - mean_x;
        sxy += dx * (y - mean_y);
        sxx += dx * dx;
    }
    let slope = sxy / sxx;
    values
        .iter()
        .enumerate()
        .map(|(i, &y)| y - mean_y - slope * (i as f64 - mean_x))
        .collect()
}

/// Score 7: the delay (samples) at which the 1 ms envelope of `out` best
/// matches that of `input`, searched over `0..=max_lag`.
pub(super) fn measured_delay(input: &[f32], out: &[f32], rate: u32, max_lag: usize) -> usize {
    let a = envelope(input, rate);
    let b = envelope(out, rate);
    let mut best = (0, f64::NEG_INFINITY);
    for lag in 0..=max_lag {
        let score: f64 = a.iter().zip(b.iter().skip(lag)).map(|(x, y)| x * y).sum();
        if score > best.1 {
            best = (lag, score);
        }
    }
    best.0
}

/// The pitch track of a moving note: per 20 ms, the strongest peak near the
/// expected target frequency of its lowest resolved partial, against that
/// frequency, cents.
pub(super) fn moving_errors(
    case: &Case,
    out: &[f32],
    rate: u32,
    ratio: f64,
    na: usize,
) -> Vec<f64> {
    let fs = rate as f64;
    let note = &case.notes[0];
    let min_hz = 2.0 * fs / na as f64;
    // The lowest partial whose target is clear of DC and of its neighbours.
    let Some(partial) = note.partials.iter().find(|p| {
        let f = p.freq * ratio;
        f >= 2.0 * min_hz && (f - p.freq).abs() >= min_hz
    }) else {
        return Vec::new();
    };
    let frame = 4096 * (rate as usize / 48_000).max(1);
    let hop = (0.02 * fs) as usize;
    let mut errors = Vec::new();
    let mut start = (case.sustain.0 * fs) as usize;
    let end = ((case.sustain.1 * fs) as usize).min(out.len());
    while start + frame <= end {
        let t = (start + frame / 2) as f64 / fs - note.onset;
        let expected = partial.freq * ratio * 2f64.powf(note.motion.semitones(t) / 12.0);
        let spec = spectrum(out, start, frame, frame * 2);
        if let Some((f, _)) = peak_near(&spec, frame * 2, fs, expected, expected * 0.03) {
            errors.push(1200.0 * (f / expected).log2());
        } else {
            errors.push(100.0);
        }
        start += hop;
    }
    errors
}

/// Whether the case's onsets, for the attack score, are the note onsets.
pub(super) fn onsets(case: &Case, rate: u32) -> Vec<usize> {
    let mut list: Vec<usize> = case
        .notes
        .iter()
        .map(|n| (n.onset * rate as f64).round() as usize)
        .collect();
    list.sort_unstable();
    list.dedup();
    debug_assert!(case.kind == Kind::Chugs || !list.is_empty());
    list
}
