//! Low-delay phase vocoder.  Every hop `H` the newest `Na` samples are one
//! frame under an asymmetric window (long rise, short fall), so the
//! resolution is that of `Na` and the latency `Ns − 1`.  The spectrum is
//! shifted directly: no time stretch, no resampler.
//!
//! The short fall gives the window a broad skirt (about −21 dB three bins
//! out), so moving bins by regions alone hands a partial's skirt the
//! neighbour's shift.  Each magnitude peak is fitted as a sinusoid against
//! the window's own transform (a table), the other peaks' lobes taken out,
//! and its lobe is rewritten at `r · k0` with the phase advanced by `r`
//! times its measured advance.  The residual moves with its nearest peak
//! (identity phase locking, Laroche and Dolson).  A frame with an onset or
//! a sudden drop moves whole regions only, which keeps its time envelope.
//! A slow trim holds the shifted power to the dry input's.  `wa · ws`
//! overlap-adds to a constant at `H`, so ratio 1 is the input, delayed.
//!
//! Phases are referenced to `t_ref`, the centre of the synthesis window
//! (`Na − Ns/2 − 0.5` from the frame start, the FFT's origin).
//!
//! A long `Na` resolves low partials but keeps the last note in view, so
//! a frame whose input is not steady over it (an onset or a stop) uses a
//! shorter analysis window, zero-padded to `Na`.  Each frame's synthesis
//! window is the Hann of `Ns` over its own analysis window, so any sequence
//! of the two still overlap-adds to a constant (Mauler and Martin's
//! switchable low-delay windows).
//!
//! [`SpectralStates::new`] allocates every mode at 192 kHz and builds the
//! FFT plans and window tables once per process.  Nothing after it does.

use std::f64::consts::TAU;
use std::sync::{Arc, OnceLock};

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};
use serde::{Deserialize, Serialize};

use super::tight::rate_scale;

const MAX_RATE: u32 = 192_000;
/// The power ratio, either way, between the newest stretch of the frame
/// (`2·Ns`, at least `STEADY_SECONDS`) and the rest that picks the short
/// window (6 dB).  A shorter stretch takes a beating chord's trough for a
/// stop; a longer one notices a stop late.
const SWITCH_POWER: f32 = 4.0;
const STEADY_SECONDS: f32 = 0.016;
/// A shift change glides in steps of one hop.
const GLIDE_SECONDS: f32 = 0.020;

/// A frame whose windowed RMS is below this (about −120 dBFS) is silence:
/// it adds nothing to the output, so silence in is exact silence out.
const SILENCE_RMS: f32 = 1e-6;
/// A peak must be within this much (power ratio, −70 dB) of the frame's
/// largest one.
const PEAK_RELATIVE_FLOOR: f32 = 1e-7;
/// The smallest sinusoid amplitude a peak can stand for (about −110 dBFS).
const PEAK_ABSOLUTE_AMPLITUDE: f32 = 3e-6;
/// A peak must be this far over the frame's median bin power (13 dB).
/// Noise bins are exponential, so noise almost never clears it; without it
/// an interface's noise floor fits hundreds of peaks per frame.
const PEAK_OVER_MEDIAN: f32 = 20.0;
/// Positive spectral flux, as a share of the frame's summed magnitude, that
/// counts as an onset.
const ONSET_FLUX: f32 = 0.25;
/// A drop in summed magnitude, as a share of the last frame's, that counts as
/// a note stopping: the frame keeps its time envelope (see `ModeState`).
const OFFSET_DROP: f32 = 0.3;
/// After an onset, how long another cannot reset the phases.
const ONSET_HOLDOFF_SECONDS: f32 = 0.04;
/// How long the level trim of a shifted output takes to follow.
const TRIM_SECONDS: f32 = 0.05;
/// A region scaled back to its analysis energy is never raised more than
/// this (amplitude).
const MAX_REGION_GAIN: f32 = 2.0;

/// The FFT sizes the plan table holds, as powers of two.
const MIN_FFT_LOG2: u32 = 8;
const MAX_FFT_LOG2: u32 = 15;

type Complex32 = Complex<f32>;

/// The spectral engine's resolution and latency trade-off, shortest first
/// in [`Self::ALL`].  An unknown name (a newer build's mode) reads as the
/// default, so a newer save does not make the path unreadable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PitchMode {
    /// About 5 ms; resolves partials about 31 Hz apart while a note holds,
    /// 94 Hz around an onset or a stop.  An older build's 8 ms `fast` did
    /// no better.
    #[serde(alias = "fast")]
    Ultrafast,
    /// About 21 ms; resolves partials about 12 Hz apart while a note holds,
    /// 23 Hz around an onset or a stop.  Palm-mutes smear: its `Ns` is
    /// long.  An older build's 40 ms `ultra` held notes no better.
    #[serde(alias = "ultra")]
    Sustain,
    /// About 11 ms, the default; resolves partials about 23 Hz apart while
    /// a note holds, 47 Hz around an onset or a stop.  An older build's
    /// 21 ms `quality` held notes hardly better and smeared palm-mutes.
    /// Last, because an unknown name reads as it.
    #[default]
    #[serde(alias = "quality", other)]
    Balanced,
}

/// One mode's sizes at one rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Spec {
    /// Analysis window length.
    pub(crate) na: usize,
    /// The analysis window of a frame that is not steady; `na` for a mode
    /// that does not switch.
    pub(crate) short: usize,
    /// FFT size, the power of two at or above `na`.
    pub(crate) n: usize,
    /// Synthesis window length.
    pub(crate) ns: usize,
    /// Hop between frames.
    pub(crate) hop: usize,
    /// What the stage reports and delays by: `ns − 1`.
    pub(crate) latency: u32,
}

impl Spec {
    const fn new(na: usize, short: usize, ns: usize, hop: usize) -> Self {
        Self {
            na,
            short,
            n: na.next_power_of_two(),
            ns,
            hop,
            latency: (ns - 1) as u32,
        }
    }

    /// The samples a cleared state needs before its output has settled.
    pub(super) fn warm_up(self) -> usize {
        self.na + self.ns + self.hop
    }
}

impl PitchMode {
    pub const ALL: [PitchMode; 3] = [
        PitchMode::Ultrafast,
        PitchMode::Balanced,
        PitchMode::Sustain,
    ];

    pub(super) fn index(self) -> usize {
        match self {
            PitchMode::Ultrafast => 0,
            PitchMode::Balanced => 1,
            PitchMode::Sustain => 2,
        }
    }

    /// The sizes at 48 kHz: `Na`, the short `Na`, `Ns`, `H`.
    const fn base(self) -> (usize, usize, usize, usize) {
        match self {
            PitchMode::Ultrafast => (3072, 1024, 256, 64),
            PitchMode::Balanced => (4096, 2048, 512, 128),
            PitchMode::Sustain => (8192, 4096, 1024, 256),
        }
    }

    /// The DSP and the latency the UI shows both read this.
    pub(crate) fn spec(self, rate: u32) -> Spec {
        let scale = rate_scale(rate);
        let (na, short, ns, hop) = self.base();
        Spec::new(na * scale, short * scale, ns * scale, hop * scale)
    }
}

/// One FFT size's forward and inverse plans.
struct FftPair {
    forward: Arc<dyn Fft<f32>>,
    inverse: Arc<dyn Fft<f32>>,
}

/// The plans for every size any mode uses at any rate.  Planning allocates,
/// so the table is built once per process, off the audio thread (from the
/// first [`SpectralStates::new`]); the plans are immutable and shared.
fn fft_pair(n: usize) -> &'static FftPair {
    static PLANS: OnceLock<Vec<FftPair>> = OnceLock::new();
    let plans = PLANS.get_or_init(|| {
        let mut planner = FftPlanner::new();
        (MIN_FFT_LOG2..=MAX_FFT_LOG2)
            .map(|log2| FftPair {
                forward: planner.plan_fft_forward(1 << log2),
                inverse: planner.plan_fft_inverse(1 << log2),
            })
            .collect()
    });
    debug_assert!(n.is_power_of_two());
    let index = (n.trailing_zeros().clamp(MIN_FFT_LOG2, MAX_FFT_LOG2) - MIN_FFT_LOG2) as usize;
    &plans[index]
}

/// One spectral peak of the current frame: the sinusoid it stands for, and
/// the region of the residual that moves with it.
#[derive(Clone, Copy, Debug, Default)]
struct Peak {
    /// The bin of the local maximum.
    bin: u32,
    /// The first and last bin of its region.
    lo: u32,
    hi: u32,
    /// The true frequency, in bins (fractional), and its target `r · k0`.
    k0: f64,
    target: f64,
    /// The complex amplitude at `t_ref`, in the spectrum rotated to `t_ref`:
    /// its argument is the sinusoid's analysis phase there.
    amp: Complex32,
    /// The residual region's whole-bin offset, `round((r − 1) · k0)`.
    shift: i32,
    /// The synthesis phase at `t_ref`, and the analysis phase there.
    phase: f64,
    analysis: f64,
    /// The residual region's rotation (synthesis less analysis phase), with
    /// its gain once known.
    rot: Complex32,
    /// The residual region's energy and the part of it the target kept.
    energy: f32,
    kept: f32,
    /// A target past Nyquist: neither the sinusoid nor its region is heard.
    dropped: bool,
}

/// The whole processing state of one mode: input FIFO, spectra, peaks,
/// phases and the overlap-add ring.  Every buffer is sized for the mode at
/// its largest rate when it is built.
struct ModeState {
    spec: Spec,
    fft: &'static FftPair,
    /// The long and the short analysis window.
    windows: &'static [Windows; 2],
    /// Whether the last frame used the short window.
    short: bool,
    /// The input, a power of two at least `na` long.
    fifo: Vec<f32>,
    /// The overlap-add output, a power of two at least `ns` long.
    out: Vec<f32>,
    /// The analysis spectrum (`n`; rotated to `t_ref`, then the residual),
    /// the synthesis spectrum (`n`), and the last frame's rotated analysis
    /// (`n/2 + 1`).
    frame: Vec<Complex32>,
    synth: Vec<Complex32>,
    prev: Vec<Complex32>,
    power: Vec<f32>,
    /// Scratch for the median of `power`.
    rank: Vec<f32>,
    mag: Vec<f32>,
    prev_mag: Vec<f32>,
    /// The peak whose region each bin is in, this frame and the last.
    owner: Vec<u32>,
    prev_owner: Vec<u32>,
    peaks: Vec<Peak>,
    prev_peaks: Vec<Peak>,
    n_peaks: usize,
    n_prev: usize,
    /// Per target bin (offset by `n/2`): the distance to the nearest shifted
    /// peak whose region reaches it, and that peak.
    claim: Vec<(u32, u32)>,
    /// The index of the newest input sample (wrapping).
    t: usize,
    have_prev: bool,
    /// Frames left before another onset may reset the phases.
    holdoff: u32,
    holdoff_frames: u32,
    /// Frames left that reset the phases (after an onset), and that shift
    /// the whole spectrum by regions instead of resynthesising the peaks
    /// (after an onset or a sudden drop), so the frame's time envelope is
    /// kept.
    reset_left: u32,
    region_left: u32,
    transient_frames: u32,
    /// After a drop: until the analysis window has moved past it.
    release_frames: u32,
    /// The newest samples the window choice weighs against the rest.
    stretch: usize,
    /// The shift in octaves, its target, and the step per hop of a glide.
    octaves: f32,
    octaves_target: f32,
    octaves_step: f32,
    glide_hops: f32,
    /// Set when a non-finite value cleared the state; the shifter fades in.
    faulted: bool,
    /// The level trim of a shifted output: the smoothed power of the shifted
    /// output and of the dry input at the same delay, how fast they follow
    /// (per sample), and the gain that holds the first to the second, with
    /// its target (set once per hop).
    wet_power: f32,
    dry_power: f32,
    trim_coeff: f32,
    trim: f32,
    trim_target: f32,
}

impl ModeState {
    /// Builds the state for `max` (the mode's largest sizes) and configures
    /// it for `windows` at `rate`.  Off the audio thread.
    fn new(max: Spec, windows: &'static [Windows; 2], rate: u32) -> Self {
        let spec = windows[0].spec;
        let half = max.n / 2;
        let mut state = Self {
            spec,
            fft: fft_pair(spec.n),
            windows,
            short: false,
            fifo: vec![0.0; max.na.next_power_of_two()],
            out: vec![0.0; max.ns.next_power_of_two()],
            frame: vec![Complex32::default(); max.n],
            synth: vec![Complex32::default(); max.n],
            prev: vec![Complex32::default(); half + 1],
            power: vec![0.0; half + 1],
            rank: vec![0.0; half + 1],
            mag: vec![0.0; half + 1],
            prev_mag: vec![0.0; half + 1],
            owner: vec![0; half + 1],
            prev_owner: vec![0; half + 1],
            peaks: vec![Peak::default(); half / 2 + 2],
            prev_peaks: vec![Peak::default(); half / 2 + 2],
            n_peaks: 0,
            n_prev: 0,
            claim: vec![(u32::MAX, 0); max.n + half + 1],
            t: 0,
            have_prev: false,
            holdoff: 0,
            holdoff_frames: 0,
            reset_left: 0,
            region_left: 0,
            transient_frames: 1,
            release_frames: 1,
            stretch: 1,
            octaves: 0.0,
            octaves_target: 0.0,
            octaves_step: 0.0,
            glide_hops: 1.0,
            faulted: false,
            wet_power: 0.0,
            dry_power: 0.0,
            trim_coeff: 0.0,
            trim: 1.0,
            trim_target: 1.0,
        };
        state.configure(windows, rate);
        state
    }

    /// Takes new sizes inside the preallocated storage and clears the state.
    /// No allocation.
    fn configure(&mut self, windows: &'static [Windows; 2], rate: u32) {
        let spec = windows[0].spec;
        debug_assert!(spec.na <= self.fifo.len() && spec.ns <= self.out.len());
        debug_assert!(spec.n <= self.frame.len());
        self.spec = spec;
        self.windows = windows;
        self.fft = fft_pair(spec.n);
        let seconds_per_hop = spec.hop as f32 / rate.max(1) as f32;
        self.holdoff_frames = (ONSET_HOLDOFF_SECONDS / seconds_per_hop).ceil() as u32;
        self.glide_hops = (GLIDE_SECONDS / seconds_per_hop).round().max(1.0);
        // Until the onset has passed the centre of the synthesis window.
        self.transient_frames = (spec.ns / 2).div_ceil(spec.hop) as u32 + 1;
        self.release_frames = spec.na.div_ceil(spec.hop) as u32;
        let steady = (STEADY_SECONDS * rate.max(1) as f32) as usize;
        self.stretch = (2 * spec.ns).max(steady).min(spec.na - spec.short);
        self.trim_coeff = (1.0 / (TRIM_SECONDS * rate.max(1) as f32)).min(1.0);
        self.clear();
    }

    /// Empties the FIFO, the output and the phase state.  No allocation.
    fn clear(&mut self) {
        self.fifo.fill(0.0);
        self.out.fill(0.0);
        self.have_prev = false;
        self.short = false;
        self.n_prev = 0;
        self.holdoff = 0;
        self.reset_left = 0;
        self.region_left = 0;
        self.wet_power = 0.0;
        self.dry_power = 0.0;
        self.trim = 1.0;
        self.trim_target = 1.0;
    }

    /// The analysis window of the current frame.
    fn window(&self) -> &'static Windows {
        &self.windows[usize::from(self.short)]
    }

    /// A non-finite value was seen: clear, and tell the shifter.
    fn fault(&mut self) {
        self.clear();
        self.faulted = true;
    }

    /// Points the shift at `octaves`, gliding one step per hop, or jumping
    /// there when `snap`.
    fn set_octaves(&mut self, octaves: f32, snap: bool) {
        if snap {
            self.octaves = octaves;
            self.octaves_target = octaves;
            self.octaves_step = 0.0;
        } else if octaves != self.octaves_target {
            self.octaves_target = octaves;
            self.octaves_step = (octaves - self.octaves).abs() / self.glide_hops;
        }
    }

    /// One input sample.  Returns the shifted output and the dry input, both
    /// `ns − 1` samples late.  `due` computes a frame on this sample.
    #[inline]
    fn tick(&mut self, x: f32, due: bool, scratch: &mut [Complex32]) -> (f32, f32) {
        let x = if x.is_finite() {
            x
        } else {
            self.fault();
            0.0
        };
        let fifo_mask = self.fifo.len() - 1;
        let out_mask = self.out.len() - 1;
        self.t = self.t.wrapping_add(1);
        self.fifo[self.t & fifo_mask] = x;
        if due {
            self.compute_frame(scratch);
        }
        let oldest = self.t.wrapping_sub(self.spec.ns - 1);
        let wet = std::mem::take(&mut self.out[oldest & out_mask]);
        let dry = self.fifo[oldest & fifo_mask];
        if self.octaves == 0.0 {
            // At a ratio of 1 the output is the input: no trim.
            self.trim = 1.0;
            self.trim_target = 1.0;
            return (wet, dry);
        }
        let c = self.trim_coeff;
        self.wet_power += (wet * wet - self.wet_power) * c;
        self.dry_power += (dry * dry - self.dry_power) * c;
        self.trim += (self.trim_target - self.trim) * c;
        (wet * self.trim, dry)
    }

    /// One analysis/synthesis frame ending at the newest sample, added into
    /// the output ring.
    fn compute_frame(&mut self, scratch: &mut [Complex32]) {
        let Spec {
            na, n, ns, short, ..
        } = self.spec;
        let half = n / 2;
        let fifo_mask = self.fifo.len() - 1;

        // The glide moves one step per hop and lands exactly on its target.
        if self.octaves != self.octaves_target {
            let diff = self.octaves_target - self.octaves;
            if diff.abs() <= self.octaves_step {
                self.octaves = self.octaves_target;
            } else {
                self.octaves += self.octaves_step.copysign(diff);
            }
        }

        let start = self.t.wrapping_sub(na - 1);
        // The window: is the input steady over the long one?
        let was_short = self.short;
        let mut rising = false;
        if short < na {
            let split = na - self.stretch;
            let (mut old, mut new) = (0.0f32, 0.0f32);
            for i in 0..na {
                let x = self.fifo[start.wrapping_add(i) & fifo_mask];
                if i < split {
                    old += x * x;
                } else {
                    new += x * x;
                }
            }
            let (old, new) = (old / split as f32, new / self.stretch as f32);
            self.short = new > SWITCH_POWER * old || old > SWITCH_POWER * new;
            rising = new > old;
        }
        // The two windows' spectra do not compare: on a switch the window's
        // own edge is the onset or the drop.
        let switched = self.short != was_short;
        let windows = self.window();
        let mut energy = 0.0f32;
        for (i, (slot, w)) in self.frame[..na].iter_mut().zip(&windows.wa).enumerate() {
            let x = self.fifo[start.wrapping_add(i) & fifo_mask] * w;
            energy += x * x;
            *slot = Complex32::new(x, 0.0);
        }
        self.frame[na..n].fill(Complex32::default());
        if energy < SILENCE_RMS * SILENCE_RMS * windows.wa_energy {
            self.have_prev = false;
            self.n_prev = 0;
            return;
        }
        self.fft
            .forward
            .process_with_scratch(&mut self.frame[..n], scratch);

        // Rotate to t_ref; magnitudes, the onset flux, and the non-finite
        // check after the forward transform.
        let mut max_power = 0.0f32;
        let mut flux = 0.0f32;
        let mut drop = 0.0f32;
        let mut total = 0.0f32;
        let mut prev_total = 0.0f32;
        for k in 0..=half {
            let x = self.frame[k] * windows.ref_rot[k];
            self.frame[k] = x;
            let p = x.norm_sqr();
            let m = p.sqrt();
            self.power[k] = p;
            self.mag[k] = m;
            max_power = max_power.max(p);
            total += m;
            prev_total += self.prev_mag[k];
            flux += (m - self.prev_mag[k]).max(0.0);
            drop += (self.prev_mag[k] - m).max(0.0);
        }
        if !total.is_finite() {
            self.fault();
            return;
        }
        let (onset_seen, drop_seen) = if switched {
            (self.short && rising, self.short && !rising)
        } else {
            (flux > ONSET_FLUX * total, drop > OFFSET_DROP * prev_total)
        };
        let onset = !self.have_prev || (self.holdoff == 0 && onset_seen);
        if onset {
            self.holdoff = self.holdoff_frames;
            self.reset_left = self.transient_frames;
            self.region_left = self.transient_frames;
        } else {
            self.holdoff = self.holdoff.saturating_sub(1);
            if self.have_prev && drop_seen {
                self.region_left = self.region_left.max(self.release_frames);
            }
        }
        let reset = self.reset_left > 0;
        let regions = self.region_left > 0;
        self.reset_left = self.reset_left.saturating_sub(1);
        self.region_left = self.region_left.saturating_sub(1);

        self.find_peaks(max_power);
        let octaves = self.octaves;
        self.track_peaks(reset, octaves);
        self.prev[..=half].copy_from_slice(&self.frame[..=half]);

        let synth = &mut self.synth[..n];
        synth.fill(Complex32::default());
        if octaves == 0.0 || self.n_peaks == 0 {
            // At a ratio of 1 the frame passes as analysed: perfect
            // reconstruction, and the phases above stay on the analysis.
            // So does a frame with no peak (noise under the floors).
            synth[..=half].copy_from_slice(&self.frame[..=half]);
            synth[0] *= 0.5;
            synth[half] *= 0.5;
        } else {
            if !regions {
                self.synthesise_peaks();
            }
            if !self.shift_residual(regions) {
                self.fault();
                return;
            }
        }

        // Back from t_ref.  The real signal is twice the real part of the
        // positive half, and the inverse transform is not normalised.
        let synth = &mut self.synth[..n];
        let scale = 2.0 / n as f32;
        // The level trim: merging partials the frame cannot separate, and
        // their overlap-add, lose a little level; the smoothed power of what
        // is heard is held to the dry input's.
        if self.wet_power > SILENCE_RMS * SILENCE_RMS {
            self.trim_target = (self.dry_power / self.wet_power).sqrt().clamp(0.5, 2.0);
        }
        for (y, rot) in synth[..=half].iter_mut().zip(&windows.ref_rot) {
            *y *= rot.conj();
        }
        // DC and Nyquist of the positive half are real.
        synth[0] = Complex32::new(synth[0].re, 0.0);
        synth[half] = Complex32::new(synth[half].re, 0.0);
        self.fft.inverse.process_with_scratch(synth, scratch);
        let tail = &self.synth[na - ns..na];
        let check: f32 = tail.iter().map(|c| c.re.abs()).sum();
        if !check.is_finite() {
            self.fault();
            return;
        }
        let out_mask = self.out.len() - 1;
        let first = self.t.wrapping_sub(ns - 1);
        for (j, (c, w)) in tail.iter().zip(&windows.ws).enumerate() {
            self.out[first.wrapping_add(j) & out_mask] += c.re * scale * w;
        }
        self.end_frame();
    }

    /// Keeps this frame's magnitudes, regions and peaks for the next one.
    fn end_frame(&mut self) {
        std::mem::swap(&mut self.mag, &mut self.prev_mag);
        std::mem::swap(&mut self.owner, &mut self.prev_owner);
        std::mem::swap(&mut self.peaks, &mut self.prev_peaks);
        self.n_prev = self.n_peaks;
        self.have_prev = true;
    }

    /// Local maxima over two bins of `Na` either side, above the floors, and
    /// the region of each (split at the lowest bin between two peaks).  A
    /// peak the last frame had within a bin continues as a plain local
    /// maximum: two partials a few bins apart beat, and a bin between them
    /// can outgrow one of them for a frame.
    fn find_peaks(&mut self, max_power: f32) {
        let n = self.spec.n;
        let half = n / 2;
        let windows = self.window();
        let near = (n / windows.support).max(1);
        let radius = 2 * near;
        let absolute = (PEAK_ABSOLUTE_AMPLITUDE * 0.5 * windows.wa_sum).powi(2);
        let rank = &mut self.rank[..half - 1];
        rank.copy_from_slice(&self.power[1..half]);
        let middle = rank.len() / 2;
        let (_, median, _) = rank.select_nth_unstable_by(middle, f32::total_cmp);
        let floor = (max_power * PEAK_RELATIVE_FLOOR)
            .max(absolute)
            .max(*median * PEAK_OVER_MEDIAN);
        let power = &self.power[..=half];
        let mut count = 0;
        let capacity = self.peaks.len();
        let mut k = 1;
        while k < half && count < capacity {
            let p = power[k];
            if p > floor {
                let is_max = |reach: usize| {
                    let lo = k.saturating_sub(reach);
                    let hi = (k + reach).min(half);
                    power[lo..k].iter().all(|&q| q < p) && power[k + 1..=hi].iter().all(|&q| q <= p)
                };
                let continues = self.have_prev && {
                    let q = self.prev_owner[k] as usize;
                    q < self.n_prev && (self.prev_peaks[q].bin as usize).abs_diff(k) <= near
                };
                if is_max(radius) || (continues && is_max(near)) {
                    self.peaks[count] = Peak {
                        bin: k as u32,
                        ..Peak::default()
                    };
                    count += 1;
                    k += near;
                    continue;
                }
            }
            k += 1;
        }
        self.n_peaks = count;
        if count == 0 {
            return;
        }
        // Regions: split at the lowest bin between two peaks.
        self.peaks[0].lo = 0;
        for i in 0..count - 1 {
            let a = self.peaks[i].bin as usize;
            let b = self.peaks[i + 1].bin as usize;
            let mut split = a;
            let mut lowest = f32::INFINITY;
            for (offset, &q) in power[a + 1..b].iter().enumerate() {
                if q < lowest {
                    lowest = q;
                    split = a + 1 + offset;
                }
            }
            self.peaks[i].hi = split as u32;
            self.peaks[i + 1].lo = split as u32 + 1;
        }
        self.peaks[count - 1].hi = half as u32;
        for (index, peak) in self.peaks[..count].iter().enumerate() {
            for owner in &mut self.owner[peak.lo as usize..=peak.hi as usize] {
                *owner = index as u32;
            }
        }
    }

    /// Each peak's true frequency, its complex amplitude with the other
    /// peaks' lobes taken out, its synthesis phase and its target.
    fn track_peaks(&mut self, reset: bool, octaves: f32) {
        let Spec { n, hop, .. } = self.spec;
        let half = n / 2;
        let ratio = (octaves as f64).exp2();
        let bin_rad = TAU / n as f64;
        let windows = self.window();
        let radius = (n / windows.support).max(1) as i64;
        let reach = windows.lobe_reach;
        let count = self.n_peaks;

        // A first guess from a parabola through the log power, then two
        // sweeps that fit each peak's lobe (frequency and complex amplitude)
        // to its bins with the other peaks' lobes, and the mirror images of
        // the peaks near DC, taken out.
        for index in 0..count {
            let k = self.peaks[index].bin as usize;
            let (a, b, c) = (
                (self.power[k - 1] + 1e-30).ln() as f64,
                (self.power[k] + 1e-30).ln() as f64,
                (self.power[k + 1] + 1e-30).ln() as f64,
            );
            let denom = a - 2.0 * b + c;
            let delta = if denom < 0.0 {
                (0.5 * (a - c) / denom).clamp(-0.5, 0.5)
            } else {
                0.0
            };
            let peak = &mut self.peaks[index];
            peak.k0 = k as f64 + delta;
            peak.amp = self.frame[k] / windows.lobe(-delta);
        }
        let width = (n / windows.support).max(1);
        debug_assert!(width <= MAX_FIT_WIDTH);
        let mut isolated = [Complex32::default(); 2 * MAX_FIT_WIDTH + 1];
        let fit_floor = self.power[..=half].iter().copied().fold(0.0f32, f32::max) * FIT_FLOOR;
        for sweep in 0..FIT_SWEEPS {
            for index in 0..count {
                let k = self.peaks[index].bin as usize;
                // A weak peak keeps its first guess: its lobe hardly
                // reaches the others.
                if self.power[k] < fit_floor {
                    continue;
                }
                let lo = k.saturating_sub(width);
                let hi = (k + width).min(half);
                let bins = &mut isolated[..=hi - lo];
                for (slot, j) in bins.iter_mut().zip(lo..=hi) {
                    *slot = self.frame[j];
                }
                let reach_bins = reach as usize + width + 1;
                let first = self.peaks[..index]
                    .iter()
                    .rposition(|q| (k - q.bin as usize) > reach_bins)
                    .map_or(0, |q| q + 1);
                for (q, other) in self.peaks[first..count].iter().enumerate() {
                    let q = q + first;
                    if other.bin as usize > k + reach_bins {
                        break;
                    }
                    if q == index {
                        continue;
                    }
                    for (slot, j) in bins.iter_mut().zip(lo..=hi) {
                        *slot -= other.amp * windows.lobe(j as f64 - other.k0);
                    }
                }
                for other in &self.peaks[..count] {
                    if other.k0 >= reach + width as f64 {
                        break;
                    }
                    for (slot, j) in bins.iter_mut().zip(lo..=hi) {
                        *slot -= other.amp.conj() * windows.lobe(j as f64 + other.k0);
                    }
                }
                // The first sweep searches a bin either side of the peak's
                // bin; the second half a bin around the first's answer.
                let (centre, span, grid) = if sweep == 0 {
                    (k as f64, 1.0, FIT_GRID)
                } else {
                    (self.peaks[index].k0, 0.5, 2)
                };
                let (k0, amp) = windows.fit(bins, lo, centre, span, grid);
                let peak = &mut self.peaks[index];
                peak.k0 = k0;
                peak.amp = amp;
            }
        }

        // Phases and targets.
        for index in 0..count {
            let peak = self.peaks[index];
            let k = peak.bin as usize;
            let analysis = (peak.amp.im as f64).atan2(peak.amp.re as f64);
            // The predecessor: of the last frame's peak whose region held this
            // bin and its neighbours, the nearest, if it was within two bins
            // of `Na`; otherwise the peak starts from its analysis phase.
            let matched = if reset || !self.have_prev || self.n_prev == 0 {
                None
            } else {
                let q = (self.prev_owner[k] as usize).min(self.n_prev - 1);
                self.prev_peaks[q.saturating_sub(1)..(q + 2).min(self.n_prev)]
                    .iter()
                    .min_by_key(|p| (p.bin as i64 - k as i64).abs())
                    .filter(|p| (p.bin as i64 - k as i64).abs() <= 2 * radius)
                    .copied()
            };
            let phase = match matched {
                // The sinusoid's own phase advance since the last frame,
                // unwrapped around the estimate, times the ratio.  It
                // telescopes over frames, so the output frequency is r times
                // the input's even when the maximum moves a bin.
                Some(prev) if octaves != 0.0 => {
                    let expected = peak.k0 * bin_rad * hop as f64;
                    let advance = expected + wrap(analysis - prev.analysis - expected);
                    wrap(prev.phase + ratio * advance)
                }
                _ => analysis,
            };
            let target = ratio * peak.k0;
            let theta = phase - analysis;
            let peak = &mut self.peaks[index];
            peak.analysis = analysis;
            peak.phase = phase;
            peak.target = target;
            peak.shift = ((ratio - 1.0) * peak.k0).round() as i32;
            peak.dropped = target > half as f64 - reach;
            peak.rot = Complex32::new(theta.cos() as f32, theta.sin() as f32);
        }
    }

    /// Takes every peak's lobe out of the analysis (leaving the residual in
    /// `frame`) and writes it at its target, exactly, with the synthesis
    /// phase.  A lobe that reaches below DC writes its mirror image too.
    fn synthesise_peaks(&mut self) {
        let half = self.spec.n / 2;
        let windows = self.window();
        let reach = windows.lobe_reach;
        for peak in &self.peaks[..self.n_peaks] {
            // The residual.
            let lo = (peak.k0 - reach).ceil().max(0.0) as usize;
            let hi = ((peak.k0 + reach).floor() as usize).min(half);
            for k in lo..=hi {
                self.frame[k] -= peak.amp * windows.lobe(k as f64 - peak.k0);
            }
            if peak.k0 < reach {
                for k in 0..=((reach - peak.k0).floor() as usize).min(half) {
                    self.frame[k] -= peak.amp.conj() * windows.lobe(k as f64 + peak.k0);
                }
            }
            if peak.dropped {
                continue;
            }
            let amp = peak.amp.norm();
            let out = Complex32::new(amp * peak.phase.cos() as f32, amp * peak.phase.sin() as f32);
            let lo = (peak.target - reach).ceil().max(0.0) as usize;
            let hi = ((peak.target + reach).floor() as usize).min(half);
            for t in lo..=hi {
                self.synth[t] += out * windows.lobe(t as f64 - peak.target);
            }
            // The mirror image below DC; bin 0 takes only the positive part
            // (the real part is taken at the end).
            if peak.target < reach {
                for t in 1..=((reach - peak.target).floor() as usize).min(half) {
                    self.synth[t] += out.conj() * windows.lobe(t as f64 + peak.target);
                }
            }
        }
    }

    /// Moves the residual's regions to their targets in whole bins, with
    /// their peak's rotation; bins below DC fold back with their phase
    /// conjugated.  On a region frame (`claims`, the whole spectrum) each
    /// target bin takes the region whose shifted peak is nearest and each
    /// region is scaled back to its energy; after the peaks were
    /// resynthesised what is left is small, and the regions only add.
    /// Returns false on a non-finite value.
    fn shift_residual(&mut self, claims: bool) -> bool {
        let n = self.spec.n;
        let half = n / 2;
        let offset = half as i32;
        if !claims {
            let synth = &mut self.synth[..n];
            for peak in &self.peaks[..self.n_peaks] {
                if peak.dropped {
                    continue;
                }
                for k in peak.lo as i32..=peak.hi as i32 {
                    let mut v = self.frame[k as usize] * peak.rot;
                    if k == 0 || k == half as i32 {
                        v *= 0.5;
                    }
                    let t = k + peak.shift;
                    if t < 0 {
                        synth[(-t) as usize] += v.conj();
                    } else if t <= half as i32 {
                        synth[t as usize] += v;
                    }
                }
            }
            return synth[..=half]
                .iter()
                .map(|c| c.re.abs() + c.im.abs())
                .sum::<f32>()
                .is_finite();
        }
        for (p, x) in self.power[..=half].iter_mut().zip(&self.frame[..=half]) {
            *p = x.norm_sqr();
        }
        let claims = &mut self.claim[..n + half + 1];
        claims.fill((u32::MAX, 0));
        for (index, peak) in self.peaks[..self.n_peaks].iter_mut().enumerate() {
            if peak.dropped {
                continue;
            }
            let target = peak.bin as i32 + peak.shift;
            let mut energy = 0.0;
            for k in peak.lo as i32..=peak.hi as i32 {
                energy += self.power[k as usize];
                let t = k + peak.shift;
                let distance = (t - target).unsigned_abs();
                let claim = &mut claims[(t + offset) as usize];
                if distance < claim.0 {
                    *claim = (distance, index as u32);
                }
            }
            peak.energy = energy;
        }
        for index in 0..self.n_peaks {
            let peak = self.peaks[index];
            if peak.dropped {
                continue;
            }
            let mut kept = 0.0;
            for k in peak.lo as i32..=peak.hi as i32 {
                if claims[(k + peak.shift + offset) as usize].1 == index as u32 {
                    kept += self.power[k as usize];
                }
            }
            self.peaks[index].kept = kept;
        }
        let synth = &mut self.synth[..n];
        let mut check = 0.0f32;
        for (index, peak) in self.peaks[..self.n_peaks].iter().enumerate() {
            if peak.dropped || peak.kept <= 0.0 {
                continue;
            }
            let gain = (peak.energy / peak.kept).sqrt().min(MAX_REGION_GAIN);
            let rot = peak.rot * gain;
            for k in peak.lo as i32..=peak.hi as i32 {
                let t = k + peak.shift;
                if claims[(t + offset) as usize].1 != index as u32 {
                    continue;
                }
                let mut v = self.frame[k as usize] * rot;
                // The positive half of a real spectrum: DC and Nyquist count
                // half.
                if k == 0 || k == half as i32 {
                    v *= 0.5;
                }
                check += v.re.abs() + v.im.abs();
                if t < 0 {
                    synth[(-t) as usize] += v.conj();
                } else if t <= half as i32 {
                    synth[t as usize] += v;
                }
            }
        }
        check += synth[..=half].iter().map(|c| c.re.abs()).sum::<f32>();
        check.is_finite()
    }
}

/// Wraps a phase to −π … π.
fn wrap(phase: f64) -> f64 {
    phase - TAU * (phase / TAU).round()
}

/// The window pair of one spec, and the analysis window's transform near
/// its main lobe.  Built once per process for every mode at every scale
/// ([`windows_for`]).
struct Windows {
    spec: Spec,
    /// How many of the newest samples `wa` covers: `na` or `short`.
    support: usize,
    /// Length `na`, zero before its support: a long rise and a short fall.
    wa: Vec<f32>,
    /// Length `ns`, under the last `ns` samples of the frame.
    ws: Vec<f32>,
    /// `Σ wa²` and `Σ wa`.
    wa_energy: f32,
    wa_sum: f32,
    /// `W(δ)`: the transform of `wa`, referenced to `t_ref`, `δ` bins from
    /// its centre, for `δ` in 0 … `lobe_reach` at `LOBE_STEPS` per bin.  A
    /// sinusoid of amplitude `A` and phase `Φ` at `t_ref` puts
    /// `A/2 · e^{iΦ} · W(k − k0)` in bin `k` of the rotated spectrum.
    lobe: Vec<Complex32>,
    lobe_reach: f64,
    /// `e^{i·2π·k·t_ref/n}` for bins 0 … `n/2`: rotates a spectrum (whose
    /// phases are referenced to the frame start) to `t_ref`.
    ref_rot: Vec<Complex32>,
}

/// How far a lobe reaches either side (bins of `Na`; about −40 dB), and the
/// table's resolution.
const LOBE_BINS: usize = 8;
const LOBE_STEPS: usize = 64;
/// The lobe fit: bins either side (at most), grid steps per bin over ±1
/// bin, and the sweeps over all peaks.
const MAX_FIT_WIDTH: usize = 4;
const FIT_GRID: usize = 3;
const FIT_SWEEPS: usize = 2;
/// Peaks this far (power) under the frame's largest keep their first guess.
const FIT_FLOOR: f32 = 1e-5;

impl Windows {
    /// `wa · ws` is a Hann window of `ns`, scaled to overlap-add to 1 at the
    /// hop (perfect reconstruction at a ratio of 1).  The rise of `wa` is a
    /// Hann's rising half over `support − ns/2`; its fall, like that of `ws`,
    /// is the square root of the Hann's falling half.  Off the audio thread.
    fn new(spec: Spec, support: usize) -> Self {
        let Spec { na, n, ns, hop, .. } = spec;
        debug_assert!(ns <= support && support <= na);
        let hann = |j: usize| -> f64 {
            let s = (std::f64::consts::PI * (j as f64 + 0.5) / ns as f64).sin();
            s * s
        };
        let pad = na - support;
        let rise = support - ns / 2;
        let wa: Vec<f64> = (0..na)
            .map(|i| {
                if i < pad {
                    0.0
                } else if i - pad < rise {
                    let i = i - pad;
                    let s = (std::f64::consts::FRAC_PI_2 * (i as f64 + 0.5) / rise as f64).sin();
                    s * s
                } else {
                    hann(i - (na - ns)).sqrt()
                }
            })
            .collect();
        // The Hann of `ns` sums to ns / (2 hop) at the hop.
        let cola = ns as f64 / (2.0 * hop as f64);
        let ws = (0..ns)
            .map(|j| (hann(j) / (wa[na - ns + j] * cola)) as f32)
            .collect();
        let t_ref = (na - ns / 2) as f64 - 0.5;
        let lobe_reach = (LOBE_BINS * n / support) as f64;
        let steps = (LOBE_BINS * n / support) * LOBE_STEPS;
        let lobe = (0..=steps + 1)
            .map(|j| {
                let delta = j as f64 / LOBE_STEPS as f64;
                let step = Complex::from_polar(1.0f64, -TAU * delta / n as f64);
                let mut phasor = Complex::from_polar(1.0f64, TAU * delta * t_ref / n as f64);
                let mut acc = Complex::new(0.0f64, 0.0);
                for _ in 0..pad {
                    phasor *= step;
                }
                for w in &wa[pad..] {
                    acc += phasor * *w;
                    phasor *= step;
                }
                Complex32::new(acc.re as f32, acc.im as f32)
            })
            .collect();
        let ref_rot = (0..=n / 2)
            .map(|k| {
                // k · t_ref reduced modulo n before it becomes an angle.
                let whole = ((k * (na - ns / 2)) % n) as f64 - 0.5 * k as f64;
                let angle = TAU * whole / n as f64;
                Complex32::new(angle.cos() as f32, angle.sin() as f32)
            })
            .collect();
        Self {
            spec,
            support,
            wa_energy: wa.iter().map(|w| w * w).sum::<f64>() as f32,
            wa_sum: wa.iter().sum::<f64>() as f32,
            wa: wa.into_iter().map(|w| w as f32).collect(),
            ws,
            lobe,
            lobe_reach,
            ref_rot,
        }
    }

    /// The least-squares fit of one lobe to `bins` (bin `lo` onwards): the
    /// frequency within `span` bins of `centre`, and the complex amplitude.
    /// A grid over ±`span`, then a parabola through the best three.
    fn fit(
        &self,
        bins: &[Complex32],
        lo: usize,
        centre: f64,
        span: f64,
        grid: usize,
    ) -> (f64, Complex32) {
        // For a frequency, the best amplitude and what it leaves.
        let solve = |k0: f64| -> (f64, Complex32) {
            let mut cross = Complex32::default();
            let mut norm = 0.0f32;
            let mut total = 0.0f32;
            for (j, x) in bins.iter().enumerate() {
                let w = self.lobe((lo + j) as f64 - k0);
                cross += w.conj() * x;
                norm += w.norm_sqr();
                total += x.norm_sqr();
            }
            let amp = cross / norm.max(f32::MIN_POSITIVE);
            (
                (total - cross.norm_sqr() / norm.max(f32::MIN_POSITIVE)) as f64,
                amp,
            )
        };
        let step = span / grid as f64;
        let mut best = (0usize, f64::INFINITY);
        let mut errors = [0.0f64; 2 * FIT_GRID + 1];
        let errors = &mut errors[..2 * grid + 1];
        for (i, slot) in errors.iter_mut().enumerate() {
            let k0 = centre + (i as f64 - grid as f64) * step;
            *slot = solve(k0).0;
            if *slot < best.1 {
                best = (i, *slot);
            }
        }
        let i = best.0.clamp(1, 2 * grid - 1);
        let (a, b, c) = (errors[i - 1], errors[i], errors[i + 1]);
        let denom = a - 2.0 * b + c;
        let offset = if denom > 0.0 {
            (0.5 * (a - c) / denom).clamp(-1.0, 1.0)
        } else {
            0.0
        };
        let k0 = centre + (i as f64 - grid as f64 + offset) * step;
        (k0, solve(k0).1)
    }

    /// `W(δ)`, interpolated; zero past the reach.  `W(−δ) = conj W(δ)`.
    #[inline]
    fn lobe(&self, delta: f64) -> Complex32 {
        let x = delta.abs() * LOBE_STEPS as f64;
        let i = x as usize;
        if i + 1 >= self.lobe.len() {
            return Complex32::default();
        }
        let frac = (x - i as f64) as f32;
        let value = self.lobe[i] * (1.0 - frac) + self.lobe[i + 1] * frac;
        if delta < 0.0 {
            value.conj()
        } else {
            value
        }
    }
}

/// The long and the short windows of `mode` at `rate`, from a table built
/// once per process for every mode at every scale.  The first call builds
/// it (off the audio thread, from [`SpectralStates::new`]); later calls only
/// read it.
fn windows_for(mode: PitchMode, rate: u32) -> &'static [Windows; 2] {
    static TABLE: OnceLock<Vec<[Windows; 2]>> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        PitchMode::ALL
            .iter()
            .flat_map(|&mode| {
                [48_000, 96_000, 192_000].map(|rate| {
                    let spec = mode.spec(rate);
                    [Windows::new(spec, spec.na), Windows::new(spec, spec.short)]
                })
            })
            .collect()
    });
    let spec = mode.spec(rate);
    table
        .iter()
        .find(|windows| windows[0].spec == spec)
        .expect("every spec is in the table")
}

/// One full state per mode, so a mode change can warm the new one while the
/// old one plays, and the FFT scratch they share.
pub(super) struct SpectralStates {
    modes: [ModeState; 3],
    scratch: Vec<Complex32>,
}

impl SpectralStates {
    /// Off the callback: every mode at the 192 kHz sizes.
    pub(super) fn new(rate: u32) -> Self {
        let modes = PitchMode::ALL
            .map(|mode| ModeState::new(mode.spec(MAX_RATE), windows_for(mode, rate), rate));
        let scratch_len = (MIN_FFT_LOG2..=MAX_FFT_LOG2)
            .map(|log2| 1usize << log2)
            .filter(|&n| n <= PitchMode::Sustain.spec(MAX_RATE).n)
            .map(|n| {
                let pair = fft_pair(n);
                pair.forward
                    .get_inplace_scratch_len()
                    .max(pair.inverse.get_inplace_scratch_len())
            })
            .max()
            .unwrap_or(0);
        Self {
            modes,
            scratch: vec![Complex32::default(); scratch_len],
        }
    }

    /// New sizes inside the preallocated storage; every mode is cleared.
    pub(super) fn configure(&mut self, rate: u32) {
        for (state, mode) in self.modes.iter_mut().zip(PitchMode::ALL) {
            state.configure(windows_for(mode, rate), rate);
        }
    }

    /// What the states and the scratch hold, for the budget test.
    #[cfg(test)]
    pub(super) fn bytes(&self) -> usize {
        use std::mem::size_of_val;
        let mode = |m: &ModeState| {
            size_of_val(&m.fifo[..])
                + size_of_val(&m.out[..])
                + size_of_val(&m.frame[..])
                + size_of_val(&m.synth[..])
                + size_of_val(&m.prev[..])
                + size_of_val(&m.power[..])
                + size_of_val(&m.rank[..])
                + size_of_val(&m.mag[..])
                + size_of_val(&m.prev_mag[..])
                + size_of_val(&m.owner[..])
                + size_of_val(&m.prev_owner[..])
                + size_of_val(&m.peaks[..])
                + size_of_val(&m.prev_peaks[..])
                + size_of_val(&m.claim[..])
        };
        self.modes.iter().map(mode).sum::<usize>() + size_of_val(&self.scratch[..])
    }

    /// Whether any mode has seen a sample since it was built or configured.
    #[cfg(test)]
    pub(super) fn ran(&self) -> bool {
        self.modes.iter().any(|m| m.t != 0)
    }

    /// Whether `mode`'s last frame used the short window.
    #[cfg(test)]
    pub(super) fn short(&self, mode: usize) -> bool {
        self.modes[mode].short
    }

    /// The peaks of `mode`'s last frame.
    #[cfg(test)]
    pub(super) fn peaks(&self, mode: usize) -> usize {
        self.modes[mode].n_prev
    }

    pub(super) fn spec(&self, mode: usize) -> Spec {
        self.modes[mode].spec
    }

    pub(super) fn clear(&mut self, mode: usize) {
        self.modes[mode].clear();
        self.modes[mode].faulted = false;
    }

    pub(super) fn clear_all(&mut self) {
        for mode in 0..self.modes.len() {
            self.clear(mode);
        }
    }

    pub(super) fn set_octaves(&mut self, octaves: f32, snap: bool) {
        for state in &mut self.modes {
            state.set_octaves(octaves, snap);
        }
    }

    /// The shifted output, the dry input at the same delay, and whether a
    /// non-finite value cleared the state.
    #[inline]
    pub(super) fn tick(&mut self, mode: usize, x: f32, due: bool) -> (f32, f32, bool) {
        let state = &mut self.modes[mode];
        let (wet, dry) = state.tick(x, due, &mut self.scratch);
        (wet, dry, std::mem::take(&mut state.faulted))
    }
}

#[cfg(test)]
mod tests;
