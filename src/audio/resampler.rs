//! [`FrameResampler::plan`] builds the table off the callback.
//! [`FrameResampler::adopt`] only swaps buffers.  A [`KernelHandoff`]
//! carries one to a running mixer: the callback `try_lock`s, never
//! builds or frees a table, and returns the rows it replaced for the
//! poster to drop.  Until a new table is ready the mixer keeps playing
//! the old pair.

use std::f64::consts::PI;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// Phase resolution of the precomputed kernel.  The two nearest phases are
/// blended, which keeps the interpolation error near -90 dB.
const PHASES: usize = 512;
const MIN_TAPS: usize = 32;
const MAX_TAPS: usize = 512;
const STOPBAND_DB: f64 = 80.0;
/// The highest frequency kept flat, in Hz, unless the lower rate's Nyquist
/// is below it.
const PASSBAND_HZ: f64 = 20_000.0;
/// The passband never reaches past this fraction of the lower rate.
const PASSBAND_FRACTION: f64 = 0.45;
/// The fastest source rate the kernel storage is sized for in advance, so
/// that a rate change up to it never allocates.
const PREALLOCATED_SOURCE_RATE: u32 = 192_000;
/// The most the ratio may be trimmed by drift steering.  The drift loop's own
/// limit lives in `audio/mixer/drift.rs` and must not exceed this clamp, which
/// a compile-time assertion there checks.
pub(crate) const MAX_CORRECTION_PPM: f32 = 500.0;

/// Passband to 20 kHz, or 45% of the lower rate.  The stop starts at
/// `lower rate - passband`, so anything that would alias into the
/// audible band is down by [`STOPBAND_DB`].  The transition is in
/// source samples, so a high-rate downsample needs a longer kernel.
fn kernel_taps(source_rate: u32, output_rate: u32) -> usize {
    let (pass, stop) = band_edges(source_rate, output_rate);
    let transition = ((stop - pass) / source_rate as f64).max(1e-6);
    // Kaiser's estimate of the length for this attenuation and transition.
    let taps = ((STOPBAND_DB - 7.95) / (14.36 * transition)).ceil() as usize;
    taps.div_ceil(8).saturating_mul(8).clamp(MIN_TAPS, MAX_TAPS)
}

/// Passband edge and stop-band edge, in Hz.
fn band_edges(source_rate: u32, output_rate: u32) -> (f64, f64) {
    let lower = source_rate.min(output_rate) as f64;
    let pass = PASSBAND_HZ.min(PASSBAND_FRACTION * lower);
    (pass, lower - pass)
}

/// The zeroth-order modified Bessel function, for the Kaiser window.
fn bessel_i0(x: f64) -> f64 {
    let half = x / 2.0;
    let mut term = 1.0;
    let mut sum = 1.0;
    for k in 1..64 {
        term *= half / k as f64;
        let add = term * term;
        sum += add;
        if add < sum * 1e-17 {
            break;
        }
    }
    sum
}

/// One source frame the resampler can carry: the clock bridge's stereo pair,
/// or a generic extra output's device lanes.
pub(crate) trait ResampleFrame: Copy {
    /// The all-zero frame.
    const SILENCE: Self;
    /// `self += sample * coefficient` on the first `lanes` lanes.  A stereo
    /// pair always has both.
    fn add_scaled(&mut self, sample: &Self, coefficient: f32, lanes: usize);
}

impl ResampleFrame for (f32, f32) {
    const SILENCE: Self = (0.0, 0.0);

    #[inline]
    fn add_scaled(&mut self, sample: &Self, coefficient: f32, _lanes: usize) {
        self.0 += sample.0 * coefficient;
        self.1 += sample.1 * coefficient;
    }
}

impl<const N: usize> ResampleFrame for [f32; N] {
    const SILENCE: Self = [0.0; N];

    #[inline]
    fn add_scaled(&mut self, sample: &Self, coefficient: f32, lanes: usize) {
        for (out, value) in self.iter_mut().zip(sample).take(lanes) {
            *out += value * coefficient;
        }
    }
}

/// A power-of-two history that holds the whole window plus the frames fetched
/// ahead of it.
fn history_len(taps: usize) -> usize {
    (taps * 2).next_power_of_two()
}

/// One precomputed kernel table for a rate pair, built off the audio thread
/// and installed by [`FrameResampler::adopt`].
pub(crate) struct Kernel {
    source_rate: u32,
    output_rate: u32,
    taps: usize,
    /// `PHASES + 1` rows of `taps` coefficients, see
    /// [`FrameResampler::kernel`].
    rows: Vec<f32>,
}

/// Kaiser-windowed sinc, so downsampling filters before it decimates.
/// 512 phases, the two nearest blended.  History is sized for any source
/// up to 192 kHz, so [`Self::adopt`] and `next` do not allocate there.
/// [`Self::plan`] may allocate because it runs off the callback.  A 1:1
/// pair is the caller's bypass unless drift steering is on.
///
/// The default frame is a stereo pair.  An extra output uses `[f32; N]`
/// and computes the first [`Self::set_lanes`] lanes.
pub(crate) struct FrameResampler<F: ResampleFrame = (f32, f32)> {
    /// The rate pair the current table was designed for.
    source_rate: u32,
    output_rate: u32,
    /// Source frames per output frame at the nominal rates.
    base_step: f64,
    /// The step actually used, including the drift correction.
    step: f64,
    correction_ppm: f32,
    /// Kernel length for the current rate pair, and half of it, which is
    /// also the look-ahead in source frames.
    taps: usize,
    half: usize,
    /// `PHASES + 1` rows of `taps` coefficients.  The extra row is the
    /// fraction-1.0 kernel, so blending from the last phase reaches the next
    /// sample with the same tap alignment instead of wrapping to the phase-0
    /// row one sample early.
    kernel: Vec<f32>,
    /// The source frames around the read position, as a power-of-two ring.
    history: Vec<F>,
    /// Lanes of `F` that carry signal (ignored for the stereo pair).
    lanes: usize,
    mask: i64,
    /// Absolute source index of the next frame to fetch.
    next_index: i64,
    /// Absolute fractional source position of the next output frame.
    read_pos: f64,
    started: bool,
    #[cfg(test)]
    configure_calls: usize,
}

impl<F: ResampleFrame> FrameResampler<F> {
    pub(crate) fn new(source_rate: u32, output_rate: u32) -> Self {
        // Rows for the longest table a source up to 192 kHz needs here,
        // so a later rate change stays inside this storage.  History is
        // capped at `MAX_TAPS`, so a faster source fits that reservation.
        let output_rate = output_rate.max(1);
        let most_taps = kernel_taps(PREALLOCATED_SOURCE_RATE, output_rate)
            .max(kernel_taps(source_rate.max(1), output_rate));
        let mut resampler = Self {
            source_rate: 0,
            output_rate: 0,
            base_step: 1.0,
            step: 1.0,
            correction_ppm: 0.0,
            taps: 0,
            half: 0,
            kernel: Vec::with_capacity((PHASES + 1) * most_taps),
            history: Vec::with_capacity(history_len(MAX_TAPS)),
            lanes: usize::MAX,
            mask: 0,
            next_index: 0,
            read_pos: 0.0,
            started: false,
            #[cfg(test)]
            configure_calls: 0,
        };
        resampler.configure(source_rate, output_rate);
        resampler
    }

    /// Builds the table for a rate pair on this thread.  This is the only
    /// place that evaluates the window and the sinc, so it must not run on an
    /// audio callback; a callback receives its table through a
    /// [`KernelHandoff`].
    pub(crate) fn plan(source_rate: u32, output_rate: u32) -> Kernel {
        Self::plan_into(source_rate, output_rate, Vec::new())
    }

    /// [`Self::plan`] with the rows filled in a buffer the caller already
    /// owns, so `configure` can reuse the storage `new` reserved.
    fn plan_into(source_rate: u32, output_rate: u32, mut rows: Vec<f32>) -> Kernel {
        let source_rate = source_rate.max(1);
        let output_rate = output_rate.max(1);
        let taps = kernel_taps(source_rate, output_rate);
        rows.clear();
        rows.resize((PHASES + 1) * taps, 0.0);
        build_kernel(&mut rows, taps, source_rate, output_rate);
        Kernel {
            source_rate,
            output_rate,
            taps,
            rows,
        }
    }

    /// Installs a planned table and starts over, swapping the rows in.  It
    /// never allocates, and the rows it replaced are returned so the caller
    /// can retire them off the audio thread.
    pub(crate) fn adopt(&mut self, kernel: Kernel) -> Vec<f32> {
        let Kernel {
            source_rate,
            output_rate,
            taps,
            rows,
        } = kernel;
        self.source_rate = source_rate;
        self.output_rate = output_rate;
        self.base_step = source_rate as f64 / output_rate as f64;
        self.taps = taps;
        self.half = taps / 2;
        let retired = std::mem::replace(&mut self.kernel, rows);
        let history = history_len(taps);
        debug_assert!(
            history <= self.history.capacity(),
            "a kernel for {source_rate} -> {output_rate} needs a resampler sized for it"
        );
        self.history.clear();
        self.history.resize(history, F::SILENCE);
        self.mask = history as i64 - 1;
        self.set_correction_ppm(0.0);
        self.reset();
        retired
    }

    /// The rate pair the current table was designed for.
    pub(crate) fn pair(&self) -> (u32, u32) {
        (self.source_rate, self.output_rate)
    }

    /// Builds the kernel for a new rate pair and starts over.  It fills the
    /// storage reserved by `new` where it can, and only allocates for a rate
    /// pair outside that sizing: call it from a setup thread, never from a
    /// callback.
    pub(crate) fn configure(&mut self, source_rate: u32, output_rate: u32) {
        #[cfg(test)]
        {
            self.configure_calls += 1;
        }
        let rows = std::mem::take(&mut self.kernel);
        let kernel = Self::plan_into(source_rate, output_rate, rows);
        let _retired = self.adopt(kernel);
    }

    #[cfg(test)]
    pub(crate) fn configure_calls(&self) -> usize {
        self.configure_calls
    }

    /// True when the output cannot simply take source frames one for one:
    /// either the rates differ or drift steering has trimmed the ratio.
    pub(crate) fn active(&self) -> bool {
        self.correction_ppm != 0.0 || (self.base_step - 1.0).abs() > 1e-9
    }

    /// Trims the conversion ratio by `ppm` parts per million.  A positive
    /// figure consumes source frames a little faster.  This is how the mixer
    /// follows two free-running clocks without dropping or repeating frames.
    pub(crate) fn set_correction_ppm(&mut self, ppm: f32) {
        self.correction_ppm = if ppm.is_finite() {
            ppm.clamp(-MAX_CORRECTION_PPM, MAX_CORRECTION_PPM)
        } else {
            0.0
        };
        self.step = self.base_step * (1.0 + self.correction_ppm as f64 * 1e-6);
    }

    /// Computes only the first `lanes` lanes of each frame from now on (the
    /// others come out silent).
    pub(crate) fn set_lanes(&mut self, lanes: usize) {
        self.lanes = lanes;
    }

    /// Forgets the history.  The next output starts at the next source frame.
    pub(crate) fn reset(&mut self) {
        self.read_pos = 0.0;
        self.next_index = 0;
        self.started = false;
    }

    pub(crate) fn next(&mut self, mut fetch: impl FnMut() -> F) -> F {
        if !self.started {
            // Hold the first frame for the history the read position starts
            // inside of, so the first output is that frame rather than a step
            // from zero.
            let first = fetch();
            self.history.fill(first);
            self.next_index = 1;
            self.read_pos = 0.0;
            self.started = true;
        }

        let position = self.read_pos;
        let index = position.floor() as i64;
        let half = self.half as i64;

        // The window reaches `half` frames ahead of the read position.  With
        // downsampling the step is larger and this loop eats every source
        // frame in between, so no source material is skipped.
        while self.next_index <= index + half {
            let frame = fetch();
            self.history[(self.next_index & self.mask) as usize] = frame;
            self.next_index += 1;
        }

        let phase = (position - index as f64) * PHASES as f64;
        let first_phase = (phase as usize).min(PHASES - 1);
        // The blend target is the next phase, which for the last bin is the
        // fraction-1.0 row at `PHASES`, not a wrap back to phase 0.
        let blend = (phase - first_phase as f64) as f32;
        let taps = self.taps;
        let first_row = &self.kernel[first_phase * taps..(first_phase + 1) * taps];
        let second_row = &self.kernel[(first_phase + 1) * taps..(first_phase + 2) * taps];

        let first_frame = index - (half - 1);
        let mut out = F::SILENCE;
        for (tap, (first, second)) in first_row.iter().zip(second_row).enumerate() {
            let sample = &self.history[((first_frame + tap as i64) & self.mask) as usize];
            let coefficient = first + (second - first) * blend;
            out.add_scaled(sample, coefficient, self.lanes);
        }

        self.read_pos = position + self.step;
        out
    }
}

/// Builds the Kaiser-windowed sinc table for `taps`, with the cutoff halfway
/// between the band edges of [`band_edges`].
fn build_kernel(rows: &mut [f32], taps: usize, source_rate: u32, output_rate: u32) {
    let (pass, stop) = band_edges(source_rate, output_rate);
    let cutoff = 0.5 * (pass + stop) / source_rate as f64;
    let beta = 0.1102 * (STOPBAND_DB - 8.7);
    let scale = 1.0 / bessel_i0(beta);
    let half = (taps / 2) as f64;
    for phase in 0..=PHASES {
        let fraction = phase as f64 / PHASES as f64;
        let row = &mut rows[phase * taps..(phase + 1) * taps];
        let mut sum = 0.0;
        for (tap, value) in row.iter_mut().enumerate() {
            // Distance from the read position to this source frame, in
            // source frames.
            let distance = fraction + (half - 1.0 - tap as f64);
            let x = distance / half;
            let window = if x.abs() >= 1.0 {
                0.0
            } else {
                bessel_i0(beta * (1.0 - x * x).sqrt()) * scale
            };
            let argument = 2.0 * PI * cutoff * distance;
            let sinc = if argument.abs() < 1e-12 {
                1.0
            } else {
                argument.sin() / argument
            };
            let coefficient = sinc * window;
            *value = coefficient as f32;
            sum += coefficient;
        }
        for value in row.iter_mut() {
            *value = (*value as f64 / sum) as f32;
        }
    }
}

/// One planned kernel table on its way to a running resampler.
pub(crate) struct KernelSlot {
    pending: Option<Kernel>,
    /// Rows a callback replaced and must not free itself.
    retired: Option<Vec<f32>>,
}

/// [`Self::post`] runs off the audio thread and drops retired rows, so
/// the callback never frees a table.  [`Self::try_adopt`] only
/// `try_lock`s, keeps the current table when the lock is busy, and
/// leaves a table for the pair already playing rather than resetting
/// its history.  `ready` makes the usual block one relaxed load.
pub(crate) struct KernelHandoff {
    slot: Mutex<KernelSlot>,
    ready: AtomicBool,
}

impl KernelHandoff {
    pub(crate) fn new() -> Self {
        Self {
            slot: Mutex::new(KernelSlot {
                pending: None,
                retired: None,
            }),
            ready: AtomicBool::new(false),
        }
    }

    /// Hands a planned table over, dropping whatever was left from before.
    /// Call it off the audio thread.
    pub(crate) fn post(&self, kernel: Kernel) {
        let Ok(mut slot) = self.slot.lock() else {
            return;
        };
        // This is the non-realtime side: the table the callback replaced is
        // freed here, not there.
        slot.retired = None;
        slot.pending = Some(kernel);
        self.ready.store(true, Ordering::Relaxed);
    }

    /// Installs the pending table when it is for another rate pair.  Call it
    /// from the callback: the lock is only tried, never waited on.
    pub(crate) fn try_adopt<F: ResampleFrame>(&self, resampler: &mut FrameResampler<F>) {
        if !self.ready.load(Ordering::Relaxed) {
            return;
        }
        let Ok(mut slot) = self.slot.try_lock() else {
            return;
        };
        let Some(kernel) = slot.pending.as_ref() else {
            self.ready.store(false, Ordering::Relaxed);
            return;
        };
        let current = resampler.pair();
        let usable =
            kernel.output_rate == current.1 && (kernel.source_rate, kernel.output_rate) != current;
        if !usable {
            // Already playing (adopting would reset its history) or planned
            // for another output rate.  Park it for the poster, so the
            // block is one relaxed load again until the next post.
            if slot.retired.is_none() {
                slot.retired = slot.pending.take().map(|kernel| kernel.rows);
            }
            self.ready.store(false, Ordering::Relaxed);
            return;
        }
        if slot.retired.is_some() {
            // Belt and braces: `post` clears the retired rows before it sets
            // a pending table, so a block should never see both.  If it ever
            // does, wait for the next post rather than free them here.
            return;
        }
        let kernel = slot.pending.take().expect("checked above");
        slot.retired = Some(resampler.adopt(kernel));
        self.ready.store(false, Ordering::Relaxed);
    }

    /// Drops a pending and a retired table on this thread.  Called when a run
    /// resets; never from a callback.
    pub(crate) fn clear(&self) {
        if let Ok(mut slot) = self.slot.lock() {
            slot.pending = None;
            slot.retired = None;
        }
        self.ready.store(false, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn pending_pair(&self) -> Option<(u32, u32)> {
        self.slot.lock().ok().and_then(|slot| {
            slot.pending
                .as_ref()
                .map(|kernel| (kernel.source_rate, kernel.output_rate))
        })
    }

    #[cfg(test)]
    pub(crate) fn ready_for_test(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }

    /// Takes the pending table out, for a test that wants the callback to
    /// find no kernel.
    #[cfg(test)]
    pub(crate) fn take_pending(&self) -> Option<Kernel> {
        let pending = self
            .slot
            .lock()
            .ok()
            .and_then(|mut slot| slot.pending.take());
        self.ready.store(false, Ordering::Relaxed);
        pending
    }

    #[cfg(test)]
    pub(crate) fn retired_len(&self) -> Option<usize> {
        self.slot
            .lock()
            .ok()
            .and_then(|slot| slot.retired.as_ref().map(Vec::len))
    }

    /// Holds the slot so a test can make the callback's `try_lock` fail.
    #[cfg(test)]
    pub(crate) fn hold_for_test(&self) -> std::sync::MutexGuard<'_, KernelSlot> {
        self.slot.lock().expect("kernel handoff")
    }
}

#[cfg(test)]
mod tests;
