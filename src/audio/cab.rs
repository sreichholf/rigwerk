//! Built-in cabinet impulse response, one per signal path.
//!
//! The cabinet always sits after the plugin effects and after the built-in
//! NAM blocks, after whichever side of the effects they are on.  It adds no
//! latency: nothing here is reported into path alignment.  The first
//! [`PARTITION`] taps are a direct FIR.  The rest is uniformly partitioned
//! overlap-save: the UI thread plans the FFTs and stores each partition's
//! spectrum on the [`CabKernel`], and the callback only runs those plans on
//! scratch the stage allocated up front.  The callback never reads a file, never
//! resamples, and never drops a [`CabKernel`].  The UI thread builds one
//! and queues it.  The callback only `try_lock`s that queue at the start
//! of a block.  A kernel it replaces is moved into [`CabPark`], and only
//! the UI thread drains that park.
//!
//! Both channels are convolved.  The left taps filter channel 0 and the
//! right taps filter channel 1.  A kernel whose sample rate is not the
//! block rate is retired and not heard.  Bypass fades to the dry sample
//! and then the convolution state is cleared, so a bypassed cabinet does
//! not run.

use std::sync::{Arc, Mutex, PoisonError, TryLockError};

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};
use serde::{Deserialize, Serialize};

use super::mixer::ramp::Ramp;

/// How long bypass, a swap, and a clear crossfade.  Long enough that the
/// switch does not click.
const BYPASS_FADE_SECONDS: f32 = 0.005;

/// Longest cabinet kept, in milliseconds, at rates up to [`CAB_MAX_RATE`].
/// A higher rate still stores [`CAB_MAX_TAPS`] samples, so the kept sound
/// is shorter than 50 ms.  Longer files are cut to this after resampling,
/// and again after the cut filters ring.
pub const CAB_MAX_MS: f32 = 50.0;

/// Highest rate the history is built for.  50 ms at this rate is
/// [`CAB_MAX_TAPS`].
pub(crate) const CAB_MAX_RATE: u32 = 192_000;

/// History and scratch length.  The callback never grows it.
pub const CAB_MAX_TAPS: usize = (CAB_MAX_RATE / 20) as usize;

/// Direct-form head, and the uniform partition.  128 taps is a couple of
/// milliseconds at 48 kHz: enough for the FFT tail to hide behind, and
/// short enough that the head is a small dot product.  The overlap-save
/// transform is twice this, so a partition finishes before its tail is due
/// and the cabinet adds no latency.
const PARTITION: usize = 128;

/// Overlap-save length.  Two partitions, one of them the overlap.
const FFT_LEN: usize = PARTITION * 2;

/// Tail partitions the stage keeps.  The longest IR uses every one.
const TAIL_PARTS: usize = (CAB_MAX_TAPS - PARTITION).div_ceil(PARTITION);

/// Level trim, the same span as the NAM trims.
pub const CAB_MIN_LEVEL_DB: f32 = -24.0;
pub const CAB_MAX_LEVEL_DB: f32 = 24.0;

/// Low cut.  0 is off.
pub const CAB_MAX_LOW_CUT_HZ: f32 = 500.0;

/// A non-zero high cut sits in this span.  0 is off.  A value below the
/// minimum clamps up to it.
pub const CAB_MIN_HIGH_CUT_HZ: f32 = 1_000.0;
pub const CAB_MAX_HIGH_CUT_HZ: f32 = 20_000.0;

/// Samples quieter than this at the start of an IR are a dead preamble.
const LEADING_SILENCE: f32 = 1.0e-5;
/// How much of that preamble may be removed.  The body of the IR stays.
const LEADING_TRIM_MS: f32 = 10.0;
/// Half-Hann fade on the end of the stored kernel.  A tail that has not
/// died away otherwise ends in a step, and that step rings.
const TAIL_FADE_MS: f32 = 5.0;
/// How long each cut filter may ring, on silence, before the 50 ms cut.
const FILTER_RING_MS: f32 = 20.0;
/// A peak below this is not a speaker.  Do not boost it.
const UNUSABLE_PEAK: f32 = 1.0e-6;
/// Sum of squares of both channels, on the stored kernel.  One gain for
/// both, so a mono file stored twice keeps its balance and each channel
/// carries half of this.
const TARGET_ENERGY: f32 = 1.0;
/// Below this the IR is silence with a numerical peak.  Do not boost it.
const UNUSABLE_ENERGY: f32 = 1.0e-12;

/// Kaiser-windowed sinc.  A few hundred taps, and the window is shaped for
/// about [`SINC_STOPBAND_DB`] of attenuation.  This runs once, on the UI
/// thread, while an impulse is loaded.  It is not the live clock bridge
/// in `audio/resampler.rs`.
const SINC_TAPS: usize = 255;
const SINC_HALF: f64 = SINC_TAPS as f64 / 2.0;
/// Stopband the Kaiser beta is calculated for.
const SINC_STOPBAND_DB: f64 = 100.0;

/// How many kernels the park can hold without allocating.  The callback
/// refuses to grow it.
const PARK_SLOTS: usize = 16;

/// Numeric controls the mix and the callback share.  The file path is not
/// here: copying it into the callback would allocate, and [`PathMix`] stays
/// [`Copy`].
///
/// [`PathMix`]: super::path::PathMix
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CabControls {
    /// Header On/Bypass.  Off skips the FIR.
    pub enabled: bool,
    /// Output trim, dB.
    pub level_db: f32,
    /// True inverts the kernel.
    pub polarity: bool,
    /// High-pass, Hz.  0 leaves the low end alone.
    pub low_cut_hz: f32,
    /// Low-pass, Hz.  0 leaves the high end alone.
    pub high_cut_hz: f32,
}

impl Default for CabControls {
    fn default() -> Self {
        Self {
            enabled: false,
            level_db: 0.0,
            polarity: false,
            low_cut_hz: 0.0,
            high_cut_hz: 0.0,
        }
    }
}

impl CabControls {
    pub fn sanitized(self) -> Self {
        Self {
            enabled: self.enabled,
            level_db: finite_or(self.level_db, 0.0).clamp(CAB_MIN_LEVEL_DB, CAB_MAX_LEVEL_DB),
            polarity: self.polarity,
            low_cut_hz: finite_or(self.low_cut_hz, 0.0).clamp(0.0, CAB_MAX_LOW_CUT_HZ),
            high_cut_hz: sanitize_high_cut(self.high_cut_hz),
        }
    }
}

/// Settings persistence and the app share, including the impulse path.
/// Not [`Copy`]: the path is a [`String`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CabSettings {
    pub enabled: bool,
    pub level_db: f32,
    pub polarity: bool,
    pub low_cut_hz: f32,
    pub high_cut_hz: f32,
    /// Empty means no impulse.  Whitespace is cleared by [`Self::sanitized`].
    pub file: String,
}

impl Default for CabSettings {
    fn default() -> Self {
        Self::from_controls(CabControls::default(), String::new())
    }
}

impl CabSettings {
    pub fn from_controls(controls: CabControls, file: String) -> Self {
        Self {
            enabled: controls.enabled,
            level_db: controls.level_db,
            polarity: controls.polarity,
            low_cut_hz: controls.low_cut_hz,
            high_cut_hz: controls.high_cut_hz,
            file,
        }
    }

    /// The controls the callback reads.  The path stays on the UI side.
    pub fn controls(&self) -> CabControls {
        CabControls {
            enabled: self.enabled,
            level_db: self.level_db,
            polarity: self.polarity,
            low_cut_hz: self.low_cut_hz,
            high_cut_hz: self.high_cut_hz,
        }
    }

    pub fn sanitized(&self) -> Self {
        let file = self.file.trim();
        Self::from_controls(self.controls().sanitized(), file.to_owned())
    }

    /// Whether a save can omit this value.  The default is what an old
    /// session already means.
    pub fn is_unset(&self) -> bool {
        self == &Self::default()
    }
}

fn sanitize_high_cut(hz: f32) -> f32 {
    let hz = finite_or(hz, 0.0);
    if hz <= 0.0 {
        0.0
    } else {
        hz.clamp(CAB_MIN_HIGH_CUT_HZ, CAB_MAX_HIGH_CUT_HZ)
    }
}

fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        fallback
    }
}

fn db_to_gain(db: f32) -> f32 {
    10.0f32.powf(db / 20.0)
}

/// What the cabinet row says about a capture's gear type and the IR.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CabNote {
    /// The capture stops before a speaker, and no IR is running.
    NoCabinet,
    /// The capture already includes a speaker, and an IR is running too.
    TwoCabinets,
}

/// `NoCabinet` when the IR is idle and the capture is an amp with no
/// speaker.  `TwoCabinets` when an IR is running on top of a capture that
/// already has one.  A pedal, and anything else, is [`None`]: a pedal is
/// not a missing cabinet.
pub(crate) fn cab_note(gear_type: Option<&str>, ir_active: bool) -> Option<CabNote> {
    let gear = gear_type?;
    let normalized = normalize_gear(gear);
    if normalized.is_empty() {
        return None;
    }
    let has_cabinet = normalized == "full_rig"
        || normalized == "ir"
        || normalized.split('_').any(|token| token == "cab");
    let no_cabinet = matches!(normalized.as_str(), "amp" | "preamp" | "pedal_amp");
    if ir_active && has_cabinet {
        Some(CabNote::TwoCabinets)
    } else if !ir_active && no_cabinet {
        Some(CabNote::NoCabinet)
    } else {
        None
    }
}

/// Trim, ascii lowercase, and `-` to `_`, the way nam-rs reads `gear_type`.
fn normalize_gear(gear: &str) -> String {
    gear.trim().to_ascii_lowercase().replace('-', "_")
}

/// One `.wav` directly in a folder.  Built on the UI thread by
/// [`scan_cab_folder`].  The samples are not decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CabCatalogEntry {
    pub path: String,
    pub name: String,
}

/// Lists `*.wav` files directly in `folder`, not in subfolders.  The name
/// is the file stem.  UI thread only: this does file I/O, and it does not
/// decode the audio.
pub(crate) fn scan_cab_folder(folder: &str) -> Vec<CabCatalogEntry> {
    let folder = folder.trim();
    if folder.is_empty() {
        return Vec::new();
    }
    let Ok(dir) = std::fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for item in dir.flatten() {
        let path = item.path();
        let wav = path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("wav"));
        if !wav || !path.is_file() {
            continue;
        }
        let name = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .filter(|stem| !stem.trim().is_empty())
            .unwrap_or_else(|| "Cabinet".to_owned());
        entries.push(CabCatalogEntry {
            path: path.to_string_lossy().into_owned(),
            name,
        });
    }
    entries.sort_by(|left, right| {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then_with(|| left.path.cmp(&right.path))
    });
    entries
}

/// What loading one impulse produced.
pub(crate) enum CabFile {
    Ready(CabKernel),
    Failed(String),
}

/// The taps the audio thread convolves.  A mono file is stored twice.
/// Built on the UI thread.  Energy-normalized, trimmed, and already filtered.
/// The spectra and the FFT plans are built here too: the callback never
/// plans, and it never drops this value.
pub(crate) struct CabKernel {
    pub left: Vec<f32>,
    pub right: Vec<f32>,
    pub rate: u32,
    /// First [`PARTITION`] taps, reversed and zero-padded.  Index 0 is the
    /// oldest tap, so the head is a dot product of two contiguous slices.
    head_left: Vec<f32>,
    head_right: Vec<f32>,
    /// Tail spectra, [`FFT_LEN`] bins per partition, partition 0 first.
    spec_left: Vec<Complex<f32>>,
    spec_right: Vec<Complex<f32>>,
    tail_parts: usize,
    forward: Arc<dyn Fft<f32>>,
    inverse: Arc<dyn Fft<f32>>,
}

impl CabKernel {
    /// Time-domain taps in, plans and spectra out.  UI thread only: this
    /// plans FFTs and allocates.  `left` and `right` may differ in length;
    /// the shorter one is zero-padded.
    pub(crate) fn from_taps(left: &[f32], right: &[f32], rate: u32) -> Self {
        let frames = left.len().max(right.len());
        let tail = frames.saturating_sub(PARTITION);
        let tail_parts = tail.div_ceil(PARTITION);
        let mut planner = FftPlanner::<f32>::new();
        let forward = planner.plan_fft_forward(FFT_LEN);
        let inverse = planner.plan_fft_inverse(FFT_LEN);
        let mut scratch = vec![Complex::new(0.0, 0.0); fft_scratch_len()];
        Self {
            head_left: reversed_head(left),
            head_right: reversed_head(right),
            spec_left: partition_spectrum(left, tail_parts, forward.as_ref(), &mut scratch),
            spec_right: partition_spectrum(right, tail_parts, forward.as_ref(), &mut scratch),
            tail_parts,
            forward,
            inverse,
            left: left.to_vec(),
            right: right.to_vec(),
            rate,
        }
    }
}

/// `h[PARTITION - 1]` in slot 0, so it lines up with the oldest sample.
fn reversed_head(taps: &[f32]) -> Vec<f32> {
    let mut head = vec![0.0; PARTITION];
    let count = taps.len().min(PARTITION);
    for tap in 0..count {
        head[PARTITION - 1 - tap] = taps[tap];
    }
    head
}

/// FFT of each tail partition, zero-padded to [`FFT_LEN`].  Partition 0 is
/// the taps that start at [`PARTITION`].
fn partition_spectrum(
    taps: &[f32],
    parts: usize,
    forward: &dyn Fft<f32>,
    scratch: &mut [Complex<f32>],
) -> Vec<Complex<f32>> {
    let mut spec = vec![Complex::new(0.0, 0.0); parts * FFT_LEN];
    let mut buf = vec![Complex::new(0.0, 0.0); FFT_LEN];
    for part in 0..parts {
        buf.fill(Complex::new(0.0, 0.0));
        let start = PARTITION + part * PARTITION;
        for (index, bin) in buf.iter_mut().enumerate().take(PARTITION) {
            if let Some(sample) = taps.get(start + index) {
                *bin = Complex::new(*sample, 0.0);
            }
        }
        forward.process_with_scratch(&mut buf, scratch);
        let base = part * FFT_LEN;
        spec[base..base + FFT_LEN].copy_from_slice(&buf);
    }
    spec
}

fn fft_scratch_len() -> usize {
    let mut planner = FftPlanner::<f32>::new();
    let forward = planner.plan_fft_forward(FFT_LEN);
    let inverse = planner.plan_fft_inverse(FFT_LEN);
    forward
        .get_inplace_scratch_len()
        .max(inverse.get_inplace_scratch_len())
}

/// Reads `path`, resamples to `rate` when the file's rate differs, and
/// bakes polarity and the cuts into the taps.  Test helper: the app
/// rebakes a cached read through [`prepare_cab_samples`].
#[cfg(test)]
pub(crate) fn prepare_cab_file(
    path: &str,
    rate: u32,
    low_cut_hz: f32,
    high_cut_hz: f32,
    polarity: bool,
) -> CabFile {
    match load_cab_channels(path, rate) {
        Ok((left, right)) => {
            prepare_cab_samples(&left, &right, rate, low_cut_hz, high_cut_hz, polarity)
        }
        Err(error) => CabFile::Failed(error),
    }
}

/// Decodes the first one or two channels and resamples them to `rate`.
/// UI thread only.  The cuts are not applied: a later change rebakes from
/// this with [`prepare_cab_samples`].
pub(crate) fn load_cab_channels(path: &str, rate: u32) -> Result<(Vec<f32>, Vec<f32>), String> {
    let mut reader = hound::WavReader::open(path).map_err(|error| wav_message(&error))?;
    let spec = reader.spec();
    if spec.channels == 0 || spec.sample_rate == 0 {
        return Err(not_wav());
    }
    let channels = spec.channels as usize;
    let limit = source_frames_needed(spec.sample_rate, rate) * channels;
    let interleaved = match spec.sample_format {
        hound::SampleFormat::Float => {
            read_samples(reader.samples::<f32>(), limit, |sample| sample)?
        }
        hound::SampleFormat::Int => {
            let scale = int_full_scale(spec.bits_per_sample);
            if scale == 0.0 {
                return Err(not_wav());
            }
            read_samples(reader.samples::<i32>(), limit, |sample| {
                sample as f32 / scale
            })?
        }
    };
    if interleaved.is_empty() {
        return Err(empty_ir());
    }
    let frames = interleaved.len() / channels;
    if frames == 0 {
        return Err(empty_ir());
    }
    let mut left = Vec::with_capacity(frames);
    let mut right = Vec::with_capacity(frames);
    for frame in 0..frames {
        let base = frame * channels;
        left.push(finite_sample(interleaved[base]));
        if channels >= 2 {
            right.push(finite_sample(interleaved[base + 1]));
        }
    }
    if channels < 2 {
        let left = resample(&left, spec.sample_rate, rate);
        if left.is_empty() {
            return Err(empty_ir());
        }
        let right = left.clone();
        return Ok((left, right));
    }
    let left = resample(&left, spec.sample_rate, rate);
    let right = resample(&right, spec.sample_rate, rate);
    if left.is_empty() || right.is_empty() {
        return Err(empty_ir());
    }
    Ok((left, right))
}

/// Trims, normalizes, and bakes polarity and the cuts into taps that are
/// already at `rate`.  UI thread only.  A file is decoded with
/// [`load_cab_channels`] and then passed here.
pub(crate) fn prepare_cab_samples(
    left: &[f32],
    right: &[f32],
    rate: u32,
    low_cut_hz: f32,
    high_cut_hz: f32,
    polarity: bool,
) -> CabFile {
    let rate = rate.max(1);
    let mut left = left.to_vec();
    let mut right = if right.is_empty() {
        left.clone()
    } else {
        right.to_vec()
    };
    let frames = left.len().min(right.len());
    left.truncate(frames);
    right.truncate(frames);
    // Trim the dead preamble before the 50 ms cut, or that silence spends
    // part of the tail the speaker still needs.
    trim_leading(&mut left, &mut right, rate);
    let limit = max_taps(rate);
    left.truncate(limit);
    right.truncate(limit);
    if left.is_empty() {
        return CabFile::Failed(empty_ir());
    }
    if energy_normalize(&mut left, &mut right).is_err() {
        return CabFile::Failed(not_usable());
    }
    if polarity {
        for sample in left.iter_mut().chain(right.iter_mut()) {
            *sample = -*sample;
        }
    }
    let low = sanitize_high_pass(low_cut_hz);
    let high = sanitize_high_cut(high_cut_hz);
    if low > 0.0 || high > 0.0 {
        let ring = ((rate as f32) * FILTER_RING_MS / 1000.0) as usize;
        left.extend(std::iter::repeat_n(0.0, ring));
        right.extend(std::iter::repeat_n(0.0, ring));
        if low > 0.0 {
            if let Some(filter) = highpass(low, rate) {
                apply_biquad(filter, &mut left);
                apply_biquad(filter, &mut right);
            }
        }
        if high > 0.0 {
            if let Some(filter) = lowpass(high, rate) {
                apply_biquad(filter, &mut left);
                apply_biquad(filter, &mut right);
            }
        }
        left.truncate(limit);
        right.truncate(limit);
    }
    if left.is_empty() {
        return CabFile::Failed(empty_ir());
    }
    apply_tail_fade(&mut left, rate);
    apply_tail_fade(&mut right, rate);
    // The cuts, and the fade, change the energy the first pass set.
    if energy_normalize(&mut left, &mut right).is_err() {
        return CabFile::Failed(not_usable());
    }
    CabFile::Ready(CabKernel::from_taps(&left, &right, rate))
}

/// Fades the last [`TAIL_FADE_MS`] with a half-Hann window, from 1 down to
/// 0.  A kernel shorter than the fade is left alone: that is the attack,
/// not a truncated tail.  A tail that is already silent stays as it is.
fn apply_tail_fade(samples: &mut [f32], rate: u32) {
    let fade = ((rate.max(1) as f32) * TAIL_FADE_MS / 1000.0).round() as usize;
    if fade < 2 || samples.len() <= fade {
        return;
    }
    let start = samples.len() - fade;
    let span = (fade - 1) as f32;
    for (index, sample) in samples.iter_mut().enumerate().skip(start) {
        let t = (index - start) as f32 / span;
        let window = 0.5 * (1.0 + (std::f32::consts::PI * t).cos());
        *sample *= window;
    }
}

fn sanitize_high_pass(hz: f32) -> f32 {
    finite_or(hz, 0.0).clamp(0.0, CAB_MAX_LOW_CUT_HZ)
}

/// [`CAB_MAX_MS`] at `rate`, and never more than [`CAB_MAX_TAPS`].  Above
/// [`CAB_MAX_RATE`] the tap cap wins, so the kept length is under 50 ms.
fn max_taps(rate: u32) -> usize {
    let taps = ((rate.max(1) as f32) * CAB_MAX_MS / 1000.0).round() as usize;
    taps.min(CAB_MAX_TAPS)
}

/// Source frames that can still affect the first 50 ms at `target`, plus
/// the sinc's look-ahead.  The rest of a long file is not decoded.
fn source_frames_needed(file_rate: u32, target_rate: u32) -> usize {
    let lead = ((target_rate.max(1) as f32) * LEADING_TRIM_MS / 1000.0).ceil() as usize;
    let outputs = max_taps(target_rate)
        .saturating_add(lead)
        .saturating_add(SINC_TAPS);
    let file_rate = file_rate.max(1) as u64;
    let target_rate = target_rate.max(1) as u64;
    let inputs = outputs as u64 * file_rate / target_rate + 1;
    (inputs as usize).saturating_add(SINC_TAPS)
}

fn read_samples<T>(
    samples: impl Iterator<Item = Result<T, hound::Error>>,
    limit: usize,
    map: impl Fn(T) -> f32,
) -> Result<Vec<f32>, String> {
    let mut out = Vec::new();
    for sample in samples {
        let sample = sample.map_err(|error| wav_message(&error))?;
        out.push(map(sample));
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

fn finite_sample(sample: f32) -> f32 {
    if sample.is_finite() {
        sample
    } else {
        0.0
    }
}

fn int_full_scale(bits: u16) -> f32 {
    let bits = bits.clamp(1, 32);
    if bits >= 32 {
        2_147_483_648.0
    } else {
        (1u32 << (bits - 1)) as f32
    }
}

fn wav_message(error: &hound::Error) -> String {
    match error {
        hound::Error::IoError(_) => couldnt_read(),
        _ => not_wav(),
    }
}

fn couldnt_read() -> String {
    "Couldn't read the impulse response.".to_owned()
}

fn not_wav() -> String {
    "This file is not a WAV.".to_owned()
}

fn empty_ir() -> String {
    "This impulse response is empty.".to_owned()
}

fn not_usable() -> String {
    "This impulse response is not usable.".to_owned()
}

fn trim_leading(left: &mut Vec<f32>, right: &mut Vec<f32>, rate: u32) {
    let max_trim = ((rate.max(1) as f32) * LEADING_TRIM_MS / 1000.0) as usize;
    let frames = left.len().min(right.len());
    let mut drop = 0;
    while drop < frames
        && drop < max_trim
        && left[drop].abs() < LEADING_SILENCE
        && right[drop].abs() < LEADING_SILENCE
    {
        drop += 1;
    }
    if drop > 0 {
        left.drain(..drop);
        right.drain(..drop);
    }
}

/// Scales both channels by one gain so their combined energy is
/// [`TARGET_ENERGY`].  A near-silent IR is left alone and reported
/// unusable, rather than boosted into a speaker.
fn energy_normalize(left: &mut [f32], right: &mut [f32]) -> Result<(), ()> {
    let mut energy = 0.0f32;
    let mut peak = 0.0f32;
    for sample in left.iter().chain(right.iter()) {
        energy += sample * sample;
        peak = peak.max(sample.abs());
    }
    if !energy.is_finite() || !peak.is_finite() || peak < UNUSABLE_PEAK || energy < UNUSABLE_ENERGY
    {
        return Err(());
    }
    let scale = (TARGET_ENERGY / energy).sqrt();
    if !scale.is_finite() {
        return Err(());
    }
    for sample in left.iter_mut().chain(right.iter_mut()) {
        *sample *= scale;
    }
    Ok(())
}

/// Kaiser-windowed sinc.  The cutoff is 0.9 times the lower Nyquist, and
/// the window is [`SINC_STOPBAND_DB`].  The same rate is a copy.
fn resample(input: &[f32], src_rate: u32, dst_rate: u32) -> Vec<f32> {
    if input.is_empty() {
        return Vec::new();
    }
    let src_rate = src_rate.max(1);
    let dst_rate = dst_rate.max(1);
    if src_rate == dst_rate {
        return input.to_vec();
    }
    let src = f64::from(src_rate);
    let dst = f64::from(dst_rate);
    let out_len = ((input.len() as f64) * dst / src).round().max(1.0) as usize;
    let nyquist = 0.5 * src.min(dst);
    let fc = 0.9 * nyquist / src;
    // Kaiser beta for a stopband past 50 dB: 0.1102 * (A - 8.7).
    // The window is tabulated once.  A Bessel series per tap would rerun
    // it for every output sample.
    let beta = 0.1102 * (SINC_STOPBAND_DB - 8.7);
    let windows = kaiser_window(beta);
    let mut out = vec![0.0; out_len];
    for (index, sample) in out.iter_mut().enumerate() {
        let src_pos = index as f64 * src / dst;
        let first = (src_pos.floor() as isize) - (SINC_TAPS as isize / 2);
        let mut acc = 0.0;
        for tap in 0..SINC_TAPS {
            let at = first + tap as isize;
            let offset = src_pos - at as f64;
            let unit = offset / SINC_HALF;
            if unit.abs() >= 1.0 {
                continue;
            }
            let weight = window_at(&windows, unit.abs());
            let sinc = if offset.abs() < 1.0e-8 {
                2.0 * fc
            } else {
                (2.0 * std::f64::consts::PI * fc * offset).sin() / (std::f64::consts::PI * offset)
            };
            if at >= 0 {
                if let Some(value) = input.get(at as usize) {
                    acc += f64::from(*value) * sinc * weight;
                }
            }
        }
        *sample = acc as f32;
    }
    out
}

/// Kaiser window samples for `unit` in `0..=1`, linearly interpolated.
/// One Bessel series per entry, not one per tap.
const KAISER_STEPS: usize = 4096;

fn kaiser_window(beta: f64) -> Vec<f64> {
    let scale = bessel_i0(beta);
    let mut table = Vec::with_capacity(KAISER_STEPS + 1);
    for step in 0..=KAISER_STEPS {
        let unit = step as f64 / KAISER_STEPS as f64;
        let inside = (1.0 - unit * unit).max(0.0).sqrt();
        table.push(bessel_i0(beta * inside) / scale);
    }
    table
}

fn window_at(table: &[f64], unit: f64) -> f64 {
    let last = (table.len() - 1) as f64;
    let pos = (unit * last).clamp(0.0, last);
    let index = pos.floor() as usize;
    let next = (index + 1).min(table.len() - 1);
    let frac = pos - index as f64;
    table[index] + (table[next] - table[index]) * frac
}

/// Modified Bessel I0.  The series is short: the Kaiser beta used here is
/// about 10.
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let quarter = x * x / 4.0;
    for k in 1..40 {
        term *= quarter / (k as f64 * k as f64);
        sum += term;
        if term < sum * 1.0e-15 {
            break;
        }
    }
    sum
}

#[derive(Clone, Copy)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
}

fn highpass(hz: f32, rate: u32) -> Option<Biquad> {
    rbj(hz, rate, true)
}

fn lowpass(hz: f32, rate: u32) -> Option<Biquad> {
    rbj(hz, rate, false)
}

/// Butterworth (Q = 1/√2) from the Audio EQ Cookbook.  `high` selects the
/// high-pass.  A corner that cannot sit under Nyquist is skipped.
fn rbj(hz: f32, rate: u32, high: bool) -> Option<Biquad> {
    let fs = rate.max(1) as f32;
    let hz = hz.min(fs * 0.45);
    if hz <= 0.0 {
        return None;
    }
    let q = std::f32::consts::FRAC_1_SQRT_2;
    let w0 = 2.0 * std::f32::consts::PI * hz / fs;
    let (sin, cos) = w0.sin_cos();
    let alpha = sin / (2.0 * q);
    let (b0, b1, b2) = if high {
        let pair = (1.0 + cos) * 0.5;
        (pair, -(1.0 + cos), pair)
    } else {
        let pair = (1.0 - cos) * 0.5;
        (pair, 1.0 - cos, pair)
    };
    let a0 = 1.0 + alpha;
    Some(Biquad {
        b0: b0 / a0,
        b1: b1 / a0,
        b2: b2 / a0,
        a1: (-2.0 * cos) / a0,
        a2: (1.0 - alpha) / a0,
    })
}

fn apply_biquad(filter: Biquad, samples: &mut [f32]) {
    let mut z1 = 0.0;
    let mut z2 = 0.0;
    for sample in samples {
        let x = *sample;
        let y = filter.b0 * x + z1;
        z1 = filter.b1 * x - filter.a1 * y + z2;
        z2 = filter.b2 * x - filter.a2 * y;
        *sample = y;
    }
}

/// What the UI queues for the next block.  `Clear` takes the live kernel
/// out of the processor without the callback destroying it.
pub(crate) enum CabCommand {
    Install(CabKernel),
    Clear,
}

/// Kernels the audio side has let go of.  The UI thread is the only place
/// that drops them ([`CabPark::flush`]).
pub(crate) struct CabPark {
    kernels: Mutex<Vec<CabKernel>>,
}

impl CabPark {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            kernels: Mutex::new(Vec::with_capacity(PARK_SLOTS)),
        })
    }

    /// Drops every parked kernel.  Call it from the UI thread.  The queue
    /// keeps its spare capacity, so the next retire does not allocate.
    pub(crate) fn flush(&self) {
        let mut queue = self.kernels.lock().unwrap_or_else(PoisonError::into_inner);
        let kernels = std::mem::replace(&mut *queue, Vec::with_capacity(PARK_SLOTS));
        drop(queue);
        drop(kernels);
    }

    /// Moves `kernel` into the park.  One try, and no allocation: a full
    /// queue or a busy lock hands the kernel back.  The callback holds it
    /// and tries again next block.
    ///
    /// The kernel is returned by value.  Boxing that `Err` would allocate
    /// on the audio thread, which is what this handshake exists to avoid.
    #[allow(clippy::result_large_err)]
    pub(crate) fn try_retire(&self, kernel: CabKernel) -> Result<(), CabKernel> {
        match self.kernels.try_lock() {
            Ok(mut queue) => park_into(&mut queue, kernel),
            Err(TryLockError::WouldBlock) => Err(kernel),
            Err(TryLockError::Poisoned(poison)) => park_into(&mut poison.into_inner(), kernel),
        }
    }

    /// Moves `kernel` into the park, retrying while the UI holds the lock.
    ///
    /// Not for the realtime callback.  The UI holds this mutex only inside
    /// [`Self::flush`] and does not wait on the caller, so a busy lock
    /// ends.  A full queue cannot grow here: the kernel is leaked rather
    /// than dropped on this thread or spun on forever.  That is the bridge
    /// thread tearing a run down while the park is already full.
    pub(crate) fn retire(&self, kernel: CabKernel) {
        let mut waiting = Some(kernel);
        while let Some(kernel) = waiting.take() {
            match self.kernels.try_lock() {
                Ok(mut queue) => match park_into(&mut queue, kernel) {
                    Ok(()) => return,
                    Err(returned) => {
                        std::mem::forget(returned);
                        return;
                    }
                },
                Err(TryLockError::WouldBlock) => {
                    waiting = Some(kernel);
                    std::thread::yield_now();
                }
                Err(TryLockError::Poisoned(poison)) => {
                    let mut queue = poison.into_inner();
                    match park_into(&mut queue, kernel) {
                        Ok(()) => return,
                        Err(returned) => {
                            std::mem::forget(returned);
                            return;
                        }
                    }
                }
            }
        }
    }
}

/// Pushes `kernel` only when `queue` has spare capacity, so `push` cannot
/// allocate.  `Err` hands it back when the queue is full.
#[allow(clippy::result_large_err)]
fn park_into(queue: &mut Vec<CabKernel>, kernel: CabKernel) -> Result<(), CabKernel> {
    if queue.len() == queue.capacity() {
        Err(kernel)
    } else {
        queue.push(kernel);
        Ok(())
    }
}

/// The cabinet stage inside one [`PathProcessor`](super::mixer::path::PathProcessor).
///
/// The convolution state is sized in [`Self::new`] for [`CAB_MAX_TAPS`] at
/// [`CAB_MAX_RATE`].  The callback never grows it.  [`Self::begin`] is the
/// only place the callback touches the queue, and it only `try_lock`s.
pub(crate) struct CabStage {
    kernel: Option<CabKernel>,
    /// The kernel fading out during a swap or a clear.
    outgoing: Option<CabKernel>,
    /// A kernel [`CabPark::try_retire`] could not take.  Held until a later
    /// block.  Two slots: the one already waiting, and the one just replaced.
    held: [Option<CabKernel>; 2],
    park: Option<Arc<CabPark>>,
    /// The rate `kernel` was built for.  0 when it must not run.
    kernel_rate: u32,
    /// The block rate [`Self::begin`] last saw.
    rate: u32,
    settings: CabControls,
    /// The controls version last applied.  0 until the first block, which
    /// the published version never is.
    version: u64,
    adopted: bool,
    level: Ramp,
    /// Cabinet against dry: 0 is bypass.
    wet: Ramp,
    wet_target: f32,
    /// New kernel against the one it replaced: 1 when no swap runs.
    swap: Ramp,
    xfade_samples: f32,
    /// Doubled head history per channel.  Sample `i` is written at `i` and
    /// at `i + PARTITION`, so the last partition is one contiguous slice.
    head: [Vec<f32>; 2],
    /// The input partition being filled, per channel.
    part_in: [Vec<f32>; 2],
    /// The previous input partition: the overlap-save prefix.
    prev: [Vec<f32>; 2],
    /// Input spectra, [`TAIL_PARTS`] of [`FFT_LEN`].  Shared by both kernels.
    spectra: [Vec<Complex<f32>>; 2],
    /// Tail contribution of the partition in progress.
    tail_live: [Vec<f32>; 2],
    tail_old: [Vec<f32>; 2],
    fft_buf: Vec<Complex<f32>>,
    fft_acc: Vec<Complex<f32>>,
    fft_scratch: Vec<Complex<f32>>,
    /// Next sample inside the partition, `0..PARTITION`.
    part_pos: usize,
    /// Next spectrum slot to write.
    spec_pos: usize,
    /// The state is already zero.  Bypass does not fill it again.
    history_silent: bool,
}

impl CabStage {
    pub(crate) fn new(rate: u32) -> Self {
        let rate = rate.max(1);
        Self {
            kernel: None,
            outgoing: None,
            held: [None, None],
            park: None,
            kernel_rate: 0,
            rate,
            settings: CabControls::default(),
            version: 0,
            adopted: false,
            level: Ramp::new(1.0),
            wet: Ramp::new(0.0),
            wet_target: 0.0,
            swap: Ramp::new(1.0),
            xfade_samples: BYPASS_FADE_SECONDS * rate as f32,
            head: [vec![0.0; PARTITION * 2], vec![0.0; PARTITION * 2]],
            part_in: [vec![0.0; PARTITION], vec![0.0; PARTITION]],
            prev: [vec![0.0; PARTITION], vec![0.0; PARTITION]],
            spectra: [
                vec![Complex::new(0.0, 0.0); TAIL_PARTS * FFT_LEN],
                vec![Complex::new(0.0, 0.0); TAIL_PARTS * FFT_LEN],
            ],
            tail_live: [vec![0.0; PARTITION], vec![0.0; PARTITION]],
            tail_old: [vec![0.0; PARTITION], vec![0.0; PARTITION]],
            fft_buf: vec![Complex::new(0.0, 0.0); FFT_LEN],
            fft_acc: vec![Complex::new(0.0, 0.0); FFT_LEN],
            fft_scratch: vec![Complex::new(0.0, 0.0); fft_scratch_len()],
            part_pos: 0,
            spec_pos: 0,
            history_silent: true,
        }
    }

    /// The convolution state is allocated in [`Self::new`], for the longest
    /// IR at [`CAB_MAX_RATE`].  Nothing here grows: a larger host block does
    /// not change the partition size.
    pub(crate) fn reserve(&mut self, _frames: usize) {
        debug_assert_eq!(self.spectra[0].len(), TAIL_PARTS * FFT_LEN);
    }

    /// Forgets the applied controls so the next [`Self::begin`] reads them
    /// again.  Does not touch the kernel.
    pub(crate) fn invalidate(&mut self) {
        self.adopted = false;
    }

    /// A slot was activated again.  The live kernel belongs to the path
    /// that just left: park it, and let the UI queue the new one.  A kernel
    /// that cannot be parked yet does not run; [`Self::begin`] parks it later.
    pub(crate) fn restart(&mut self, rate: u32) {
        if !self.retire_active() {
            self.kernel_rate = 0;
        }
        self.retire_outgoing();
        self.clear_history();
        self.adopted = false;
        let rate = rate.max(1);
        self.rate = rate;
        self.xfade_samples = BYPASS_FADE_SECONDS * rate as f32;
        self.level = Ramp::new(1.0);
        self.wet = Ramp::new(0.0);
        self.wet_target = 0.0;
        self.swap = Ramp::new(1.0);
    }

    /// Takes a queued kernel and applies controls.  Callback: `try_lock`
    /// only.
    pub(crate) fn begin(
        &mut self,
        controls: &super::path::PathControls,
        rate: u32,
        ramp_samples: f32,
    ) {
        if self.park.is_none() {
            self.park = Some(Arc::clone(controls.cab_park()));
        }
        let rate = rate.max(1);
        self.rate = rate;
        self.xfade_samples = BYPASS_FADE_SECONDS * rate as f32;
        self.drain_held();
        if self.swap.is_settled_at(1.0) {
            self.retire_outgoing();
        }
        if self.kernel.is_some() && self.kernel_rate != rate {
            // A rate change restarts the device: there is nothing to fade,
            // and this callback does not resample.  The old history belongs
            // to the old rate, so the replacement fades in from dry.
            self.retire_active();
            self.retire_outgoing();
            self.clear_history();
            self.wet.snap(0.0);
            self.wet_target = 0.0;
        }
        self.take_handoff(controls);
        if self.swap.is_settled_at(0.0) {
            self.swap.set_target(1.0, self.xfade_samples.max(1.0));
        }
        self.follow_settings(controls, ramp_samples);
    }

    /// One block of the path, in place on channels 0 and 1.  No allocation.
    /// A settled bypass does not run the convolution.
    pub(crate) fn process_block(&mut self, block: &mut [[f32; 4]], frames: usize) {
        let frames = frames.min(block.len());
        let swap_live = self.outgoing.is_some() && !self.swap.is_settled_at(1.0);
        if self.wet_target == 0.0 && self.wet.is_settled_at(0.0) && !swap_live {
            self.clear_history();
            return;
        }
        let rate = self.rate;
        let new_ok = self
            .kernel
            .as_ref()
            .is_some_and(|kernel| kernel.rate == rate);
        let old_ok = self
            .outgoing
            .as_ref()
            .is_some_and(|kernel| swap_live && kernel.rate == rate);
        // A finished Clear leaves the wet target up and both kernels gone.
        // Drop the history now, or the next Install crossfades over it.
        if !new_ok && !old_ok {
            self.clear_history();
            for _ in 0..frames {
                let _ = self.level.next();
                let _ = self.swap.next();
                let _ = self.wet.next();
            }
            return;
        }
        for frame in block.iter_mut().take(frames) {
            let gain = self.level.next();
            let swap = self.swap.next();
            let mix = self.wet.next();
            let dry = [frame[0], frame[1]];
            if new_ok || old_ok {
                let (new, old) = self.convolve_frame(dry, new_ok, old_ok);
                for channel in 0..2 {
                    let new = if new_ok {
                        new[channel] * gain
                    } else {
                        dry[channel]
                    };
                    let old = if old_ok {
                        old[channel] * gain
                    } else {
                        dry[channel]
                    };
                    let wet = old + (new - old) * swap;
                    frame[channel] = dry[channel] + (wet - dry[channel]) * mix;
                }
            }
        }
        if self.wet_target == 0.0 && self.wet.is_settled_at(0.0) && self.swap.is_settled_at(1.0) {
            self.clear_history();
        }
    }

    /// Installs a kernel directly.  The next [`Self::begin`] snaps the
    /// bypass instead of fading, once the controls are on.  Tests only.
    #[cfg(test)]
    pub(crate) fn install_prepared(&mut self, kernel: CabKernel) {
        let _ = self.retire_active();
        self.rate = kernel.rate.max(1);
        self.accept(kernel);
        if !self.history_silent {
            self.render_tail(true);
        }
    }

    fn kernel_heard(&self) -> bool {
        self.kernel.is_some() && self.kernel_rate == self.rate && self.kernel_rate != 0
    }

    /// Whether a swap must fade: the cabinet is up, or still moving there.
    fn audible(&self) -> bool {
        self.wet_target > 0.0 || !self.wet.is_settled_at(0.0)
    }

    fn follow_settings(&mut self, controls: &super::path::PathControls, ramp_samples: f32) {
        let version = controls.cab_version();
        if version == self.version && self.adopted {
            return;
        }
        let Some(settings) = controls.try_cab() else {
            return;
        };
        self.settings = settings;
        self.version = version;
        let level = db_to_gain(settings.level_db);
        if !self.adopted {
            self.level.snap(level);
            let wet = if settings.enabled && self.kernel_heard() {
                1.0
            } else {
                0.0
            };
            self.wet.snap(wet);
            self.wet_target = wet;
            self.adopted = true;
            return;
        }
        self.level.set_target(level, ramp_samples);
        if settings.enabled && self.kernel_heard() {
            if self.wet_target == 0.0 {
                self.wet_target = 1.0;
                self.wet.set_target(1.0, self.xfade_samples.max(1.0));
            }
        } else if !settings.enabled && self.wet_target != 0.0 {
            self.wet_target = 0.0;
            self.wet.set_target(0.0, self.xfade_samples.max(1.0));
        }
    }

    fn take_handoff(&mut self, controls: &super::path::PathControls) {
        let Ok(mut slot) = controls.pending_cab_mut() else {
            return;
        };
        let Some(command) = slot.take() else {
            return;
        };
        // One swap at a time.  An outgoing kernel that could not be parked
        // stays put: overwriting it would drop it on this thread.
        if self.outgoing.is_some() || (self.audible() && !self.swap.is_settled_at(1.0)) {
            *slot = Some(command);
            return;
        }
        match command {
            CabCommand::Install(kernel) => {
                if self.audible() {
                    self.outgoing = self.kernel.take();
                    self.kernel_rate = 0;
                    self.swap.snap(0.0);
                    if self.outgoing.is_none() {
                        self.clear_history();
                    } else {
                        // The tail already computed for this partition was
                        // the live kernel's.  It is the outgoing one now.
                        self.hand_tail_to_outgoing();
                    }
                } else if !self.retire_active() {
                    *slot = Some(CabCommand::Install(kernel));
                    return;
                } else {
                    self.clear_history();
                }
                self.accept(kernel);
                self.fade_in_if_enabled();
                if !self.history_silent {
                    self.render_tail(true);
                }
            }
            CabCommand::Clear => {
                if self.audible() {
                    if let Some(kernel) = self.kernel.take() {
                        self.outgoing = Some(kernel);
                        self.swap.snap(0.0);
                        self.hand_tail_to_outgoing();
                        self.tail_live[0].fill(0.0);
                        self.tail_live[1].fill(0.0);
                    } else {
                        self.clear_history();
                    }
                    self.kernel_rate = 0;
                } else if !self.retire_active() {
                    *slot = Some(CabCommand::Clear);
                } else {
                    self.clear_history();
                }
            }
        }
    }

    /// Moves the current partition's tail onto the kernel that is fading
    /// out.  Swapping the buffers does not allocate.
    fn hand_tail_to_outgoing(&mut self) {
        std::mem::swap(&mut self.tail_live, &mut self.tail_old);
    }

    /// A kernel that arrives after On, or after a rate change, has to start
    /// the wet fade itself: the controls version did not change, so
    /// [`Self::follow_settings`] will not look again.
    fn fade_in_if_enabled(&mut self) {
        if !self.adopted || !self.settings.enabled || !self.kernel_heard() || self.wet_target != 0.0
        {
            return;
        }
        self.wet_target = 1.0;
        self.wet.set_target(1.0, self.xfade_samples.max(1.0));
    }

    fn accept(&mut self, kernel: CabKernel) {
        self.kernel_rate = kernel.rate;
        self.kernel = Some(kernel);
    }

    /// One sample of both channels.  The head is direct.  The tail was
    /// computed when the previous partition filled, so this adds no latency.
    /// The partition FFT runs only when [`PARTITION`] new samples are in.
    fn convolve_frame(
        &mut self,
        dry: [f32; 2],
        new_ok: bool,
        old_ok: bool,
    ) -> ([f32; 2], [f32; 2]) {
        let mut new = [0.0; 2];
        let mut old = [0.0; 2];
        let pos = self.part_pos;
        for channel in 0..2 {
            self.head[channel][pos] = dry[channel];
            self.head[channel][pos + PARTITION] = dry[channel];
            self.part_in[channel][pos] = dry[channel];
            if new_ok {
                new[channel] = self.head_dot(channel, true) + self.tail_live[channel][pos];
            }
            if old_ok {
                old[channel] = self.head_dot(channel, false) + self.tail_old[channel][pos];
            }
        }
        self.history_silent = false;
        self.part_pos = pos + 1;
        if self.part_pos == PARTITION {
            self.finish_partition(new_ok, old_ok);
            self.part_pos = 0;
        }
        (new, old)
    }

    /// `h[0]` lines up with the newest sample.  The window is contiguous:
    /// no modulo per tap.
    fn head_dot(&self, channel: usize, newest: bool) -> f32 {
        let kernel = if newest {
            self.kernel.as_ref()
        } else {
            self.outgoing.as_ref()
        };
        let Some(kernel) = kernel else {
            return 0.0;
        };
        let taps = if channel == 0 {
            kernel.head_left.as_slice()
        } else {
            kernel.head_right.as_slice()
        };
        let window = &self.head[channel][self.part_pos + 1..self.part_pos + 1 + PARTITION];
        let mut acc = 0.0;
        for (sample, tap) in window.iter().zip(taps) {
            acc += sample * tap;
        }
        acc
    }

    /// FFT of `[previous partition | this partition]`, then each live
    /// kernel's tail for the next partition.
    fn finish_partition(&mut self, new_ok: bool, old_ok: bool) {
        let Some(plan) = self.forward_plan() else {
            return;
        };
        for channel in 0..2 {
            for index in 0..PARTITION {
                self.fft_buf[index] = Complex::new(self.prev[channel][index], 0.0);
                self.fft_buf[PARTITION + index] = Complex::new(self.part_in[channel][index], 0.0);
            }
            plan.process_with_scratch(&mut self.fft_buf, &mut self.fft_scratch);
            let base = self.spec_pos * FFT_LEN;
            self.spectra[channel][base..base + FFT_LEN].copy_from_slice(&self.fft_buf);
            self.prev[channel].copy_from_slice(&self.part_in[channel]);
        }
        self.spec_pos += 1;
        if self.spec_pos == TAIL_PARTS {
            self.spec_pos = 0;
        }
        if new_ok {
            self.render_tail(true);
        }
        if old_ok {
            self.render_tail(false);
        }
    }

    fn forward_plan(&self) -> Option<Arc<dyn Fft<f32>>> {
        self.kernel
            .as_ref()
            .map(|kernel| Arc::clone(&kernel.forward))
            .or_else(|| {
                self.outgoing
                    .as_ref()
                    .map(|kernel| Arc::clone(&kernel.forward))
            })
    }

    /// Overlap-save tail for the partition that is about to play.  `spec_pos`
    /// is the next slot to write, so the newest spectrum is the one before it.
    /// Safe to call mid-partition: the current partition's spectrum is not in
    /// the ring yet, which is what this tail was built from.
    fn render_tail(&mut self, newest: bool) {
        let kernel = if newest {
            self.kernel.take()
        } else {
            self.outgoing.take()
        };
        let Some(kernel) = kernel else {
            return;
        };
        let scale = 1.0 / FFT_LEN as f32;
        for channel in 0..2 {
            let spec = if channel == 0 {
                kernel.spec_left.as_slice()
            } else {
                kernel.spec_right.as_slice()
            };
            self.fft_acc.fill(Complex::new(0.0, 0.0));
            for part in 0..kernel.tail_parts {
                let slot = (self.spec_pos + TAIL_PARTS - 1 - part) % TAIL_PARTS;
                let input = &self.spectra[channel][slot * FFT_LEN..(slot + 1) * FFT_LEN];
                let taps = &spec[part * FFT_LEN..(part + 1) * FFT_LEN];
                for bin in 0..FFT_LEN {
                    self.fft_acc[bin] += input[bin] * taps[bin];
                }
            }
            let tail = if newest {
                &mut self.tail_live[channel]
            } else {
                &mut self.tail_old[channel]
            };
            if kernel.tail_parts == 0 {
                tail.fill(0.0);
                continue;
            }
            kernel
                .inverse
                .process_with_scratch(&mut self.fft_acc, &mut self.fft_scratch);
            for (index, sample) in tail.iter_mut().enumerate() {
                *sample = self.fft_acc[PARTITION + index].re * scale;
            }
        }
        if newest {
            self.kernel = Some(kernel);
        } else {
            self.outgoing = Some(kernel);
        }
    }

    fn clear_history(&mut self) {
        if self.history_silent {
            return;
        }
        for channel in 0..2 {
            self.head[channel].fill(0.0);
            self.part_in[channel].fill(0.0);
            self.prev[channel].fill(0.0);
            self.spectra[channel].fill(Complex::new(0.0, 0.0));
            self.tail_live[channel].fill(0.0);
            self.tail_old[channel].fill(0.0);
        }
        self.part_pos = 0;
        self.spec_pos = 0;
        self.history_silent = true;
    }

    /// `false` when the live kernel could not be parked and is still here.
    fn retire_active(&mut self) -> bool {
        let Some(kernel) = self.kernel.take() else {
            self.kernel_rate = 0;
            return true;
        };
        match self.park_kernel(kernel) {
            Ok(()) => {
                self.kernel_rate = 0;
                true
            }
            Err(kernel) => {
                self.kernel = Some(kernel);
                false
            }
        }
    }

    /// Parks the kernel a swap faded out.  One that cannot be parked yet
    /// stays, silent, until a later block.
    fn retire_outgoing(&mut self) {
        if let Some(kernel) = self.outgoing.take() {
            if let Err(kernel) = self.park_kernel(kernel) {
                self.outgoing = Some(kernel);
                // The swap is not finished until the kernel has a slot.
                // Snapping here would let the next Install overwrite it.
                return;
            }
        }
        self.swap.snap(1.0);
    }

    fn drain_held(&mut self) {
        let Some(park) = self.park.clone() else {
            return;
        };
        for slot in &mut self.held {
            let Some(kernel) = slot.take() else {
                continue;
            };
            if let Err(kernel) = park.try_retire(kernel) {
                *slot = Some(kernel);
            }
        }
    }

    /// Moves `kernel` into the park, or into a held slot.  `Err` returns
    /// the kernel when both are busy: the caller still owns it, and it is
    /// not dropped.
    #[allow(clippy::result_large_err)]
    fn park_kernel(&mut self, kernel: CabKernel) -> Result<(), CabKernel> {
        if let Some(park) = &self.park {
            match park.try_retire(kernel) {
                Ok(()) => return Ok(()),
                Err(kernel) => {
                    if let Some(slot) = self.held.iter_mut().find(|slot| slot.is_none()) {
                        *slot = Some(kernel);
                        return Ok(());
                    }
                    return Err(kernel);
                }
            }
        }
        // No park yet: this stage has not been attached to a run.  The
        // caller is the owner thread (a unit test).  The kernel waits in a
        // held slot and drops with the stage.
        if let Some(slot) = self.held.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(kernel);
            Ok(())
        } else {
            Err(kernel)
        }
    }
}

impl Drop for CabStage {
    fn drop(&mut self) {
        // `OutputMixer` (and these processors) is moved into the output
        // callback and dropped when that stream stops, on the bridge
        // thread, after the callback has returned.  This `Drop` is not the
        // realtime callback.  A kernel is therefore moved into the park
        // here and dropped later by `CabPark::flush` on the UI thread.
        // With no park the stage never left the thread that built it (a
        // unit test); that thread may drop the kernel.
        let Some(park) = self.park.clone() else {
            return;
        };
        let kernels = [self.kernel.take(), self.outgoing.take()]
            .into_iter()
            .chain(self.held.iter_mut().map(Option::take))
            .flatten();
        for kernel in kernels {
            park.retire(kernel);
        }
    }
}

#[cfg(test)]
mod tests;
