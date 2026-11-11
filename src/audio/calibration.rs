//! Measured loopback latency: the click, the capture buffer and the
//! correlator.
//!
//! The app arms one measurement at a time.  The mixer ramps the clock output
//! to silence; the input callback then waits two input buffers so the
//! program material already in the capture pipeline drains, copies raw frames
//! into a preallocated buffer ([`CAPTURE_CAPACITY`], never an allocation on
//! the audio thread) and the mixer plays the click once that capture has
//! settled.  The UI thread correlates once the window is full; nothing here
//! touches a file, a lock or a plugin.
//!
//! Timing: the capture buffer is indexed from the first frame the input
//! callback copied, and that frame is stamped with the monotonic clock.  The
//! mixer stamps the clock sample at which the click starts the same way.  The
//! loopback delay is then `capture start + peak / input rate - click start`,
//! in seconds, so it uses the rate the signal actually arrives at and never
//! assumes the two devices share one.  The stamp is taken at the start of the
//! callback that carries the frame, so the result has the driver's block
//! granularity; it is a measurement of the loop, not a claim of sample
//! accuracy.
//!
//! The state transitions that two threads share use compare-and-exchange:
//! a cancel racing the input's start or the mixer's click cannot leave the
//! clock output ducked.  The app also clears any state it is not tracking.
//!
//! The click is a short linear chirp, 400 Hz to 6 kHz, Hann-windowed, so it
//! has no step at either end (no click of its own) and correlates sharply
//! even when the loop colours the sound.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

/// Length of the chirp, in seconds.
pub(crate) const CLICK_SECONDS: f32 = 0.045;
/// How long the mix takes to reach silence before the click may play.
pub(crate) const DUCK_SECONDS: f32 = 0.03;
/// How long capture runs before the click is emitted: long enough that the
/// pre-click part of the capture is a clean noise floor.
pub(crate) const SETTLE_SECONDS: f32 = 0.08;
/// How much audio a measurement captures.  A loop longer than this reports
/// that it did not fit instead of guessing.
pub(crate) const CAPTURE_SECONDS: f32 = 1.2;
/// The largest click the buffer holds.  At a rate high enough to need more
/// samples than this, the chirp is simply shorter; nothing is allocated on
/// the audio thread.
pub(crate) const MAX_CLICK_SAMPLES: usize = 19_200;
/// The most frames a capture keeps (2 s at 96 kHz).
pub(crate) const CAPTURE_CAPACITY: usize = 192_000;
/// Click level, full scale.  Fixed, so a measurement does not depend on the
/// master fader (the duck applies first, the click after).
pub(crate) const CLICK_LEVEL: f32 = 0.5;
/// A capture at or above this peak is treated as clipped: the loop is too
/// hot for an honest measurement.
pub(crate) const CLIP_LEVEL: f32 = 0.999;
/// A capture quieter than this peak has no usable click (-60 dBFS).  The
/// onset detector also has an adaptive floor; this only rejects a capture
/// with nothing on it at all.
pub(crate) const QUIET_LEVEL: f32 = 0.001;
/// The lowest normalised correlation accepted as "the click is here".  A
/// loud tonal event that is not the chirp stays well below this.
pub(crate) const MIN_SCORE: f32 = 0.35;
/// A delay longer than this is not a loop through a cable or a mic.
pub(crate) const MAX_ROUND_TRIP_MS: f32 = 5_000.0;

const CHIRP_START_HZ: f32 = 400.0;
const CHIRP_END_HZ: f32 = 6_000.0;
const TAU: f32 = std::f32::consts::TAU;

/// Where a measurement is in its life.  Only the audio callbacks and the UI
/// move it, each in its own direction:
///
/// - the app arms `Idle -> Armed`;
/// - the input callback starts the capture (`Armed -> Capturing`) and marks
///   the window full (`Emitting | Tail -> Ready`);
/// - the mixer starts the click (`Capturing -> Emitting`) and finishes it
///   (`Emitting -> Tail`);
/// - the app returns to `Idle` on success, cancel, timeout or error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CalibrationState {
    /// Nothing is being measured.
    Idle = 0,
    /// The app asked for a measurement; no capture frame has arrived yet.
    Armed = 1,
    /// Capture is running and the mix is being ducked; the click waits for
    /// the settle window.
    Capturing = 2,
    /// The click is playing into the ducked output.
    Emitting = 3,
    /// The click has played out; capture is still filling.
    Tail = 4,
    /// The capture window is full and the UI may correlate.
    Ready = 5,
}

impl CalibrationState {
    fn from_u32(value: u32) -> Self {
        match value {
            1 => Self::Armed,
            2 => Self::Capturing,
            3 => Self::Emitting,
            4 => Self::Tail,
            5 => Self::Ready,
            _ => Self::Idle,
        }
    }

    /// Whether the clock output should be ducked right now.
    pub(crate) fn ducking(self) -> bool {
        matches!(
            self,
            Self::Armed | Self::Capturing | Self::Emitting | Self::Tail
        )
    }
}

/// A failed capture, as the UI should explain it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum CalibrationFailure {
    /// The capture reached full scale: the loop is too hot to trust.
    Clipped,
    /// The capture never rose above the quiet threshold.
    TooQuiet { level_db: f32 },
    /// The capture fits no click: not looped back, or the wrong device.
    NoPeak { score: f32 },
    /// The capture window is shorter than the click template.
    TooShort,
    /// The click's round trip did not fit in the capture window.
    OutOfWindow,
    /// The capture's elapsed time does not match the frames it holds: an
    /// input block was dropped, so the delay would read short.
    Timing,
}

/// What the correlation found.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CaptureAnalysis {
    /// The capture sample where the click starts.
    pub(crate) peak: usize,
    /// The normalised correlation there, 0..1.
    pub(crate) score: f32,
    /// The capture's peak level, dBFS.
    pub(crate) level_db: f32,
}

/// The click as it is played, at the output rate.  A linear chirp under a
/// Hann window: deterministic, short, and spread over the band so a
/// correlation peak is unambiguous.
pub(crate) fn click_waveform(rate: u32) -> Vec<f32> {
    let rate = rate.max(1);
    let wanted = (CLICK_SECONDS * rate as f32).round() as usize;
    let len = wanted.clamp(64, MAX_CLICK_SAMPLES);
    let seconds = len as f32 / rate as f32;
    let span = CHIRP_END_HZ - CHIRP_START_HZ;
    let mut samples = Vec::with_capacity(len);
    for index in 0..len {
        let time = index as f32 / rate as f32;
        let phase = TAU * (CHIRP_START_HZ * time + span * time * time / (2.0 * seconds));
        let window = 0.5 - 0.5 * (TAU * index as f32 / (len - 1) as f32).cos();
        samples.push(CLICK_LEVEL * window * phase.sin());
    }
    samples
}

/// Works out the round trip from the capture start stamp, the click stamp and
/// the correlation peak.  `None` when the stamps are missing or the delay is
/// not physically plausible.
pub(crate) fn round_trip_ms(
    peak: usize,
    capture_start_ns: u64,
    emitted_ns: u64,
    input_rate: u32,
) -> Option<(f32, u32)> {
    if input_rate == 0 || capture_start_ns == 0 || emitted_ns == 0 {
        return None;
    }
    let peak_ns = capture_start_ns as f64 + peak as f64 * 1e9 / input_rate as f64;
    let delay_ns = peak_ns - emitted_ns as f64;
    if delay_ns < 0.0 {
        return None;
    }
    let ms = (delay_ns / 1e6) as f32;
    if !(0.0..=MAX_ROUND_TRIP_MS).contains(&ms) {
        return None;
    }
    let samples = (delay_ns / 1e9 * input_rate as f64).round();
    if samples < 1.0 {
        return None;
    }
    Some((ms, samples.min(u32::MAX as f64) as u32))
}

/// Finds the click in a capture.  The onset comes from a short-time energy
/// envelope (the ducked mix leaves the click as the only burst), then the
/// correlation is evaluated over the few milliseconds around it.
pub(crate) fn analyze_capture(
    capture: &[f32],
    template: &[f32],
    rate: u32,
) -> Result<CaptureAnalysis, CalibrationFailure> {
    if capture.len() < template.len().max(64) || template.len() < 32 || rate == 0 {
        return Err(CalibrationFailure::TooShort);
    }
    let level = capture
        .iter()
        .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
    let level_db = if level > 0.0 {
        20.0 * level.log10()
    } else {
        f32::NEG_INFINITY
    };
    if level >= CLIP_LEVEL {
        return Err(CalibrationFailure::Clipped);
    }
    if level < QUIET_LEVEL {
        return Err(CalibrationFailure::TooQuiet { level_db });
    }
    let last_lag = capture.len() - template.len();
    let Some(onset) = find_onset(capture, rate) else {
        return Err(CalibrationFailure::NoPeak { score: 0.0 });
    };
    // The envelope finds where the burst becomes loud; a windowed chirp
    // starts quieter than that, so the correlation looks back over one whole
    // click (and a little forward) to place the real first sample.
    let span = (rate / 500).max(1) as usize;
    let first = onset.saturating_sub(template.len());
    let last = (onset + span).min(last_lag);
    if first > last {
        return Err(CalibrationFailure::TooShort);
    }
    let Some((peak, score)) = best_lag(capture, template, first, last) else {
        return Err(CalibrationFailure::NoPeak { score: 0.0 });
    };
    if score.abs() < MIN_SCORE {
        return Err(CalibrationFailure::NoPeak { score: score.abs() });
    }
    Ok(CaptureAnalysis {
        peak,
        score: score.abs().min(1.0),
        level_db,
    })
}

/// The start of the click's burst.  The ducked mix leaves it as the loudest
/// event in the capture, so the loudest millisecond is found first and then
/// walked back to the leading edge of the burst.  The walk stops at the noise
/// floor or after one click length, which keeps a later room reflection from
/// pulling the search into the tail.
fn find_onset(capture: &[f32], rate: u32) -> Option<usize> {
    let window = (rate / 1000).max(8) as usize;
    let hop = (window / 2).max(1);
    let floor_len = capture.len().min((rate as usize / 20).max(window));
    let floor_rms = rms(&capture[..floor_len]);
    let threshold = (floor_rms * 2.0).max(QUIET_LEVEL * 0.25).max(1e-4);
    let mut loudest = (0usize, 0.0f32);
    let mut start = floor_len;
    while start + window <= capture.len() {
        let level = rms(&capture[start..start + window]);
        if level > loudest.1 {
            loudest = (start, level);
        }
        start += hop;
    }
    if loudest.1 <= threshold {
        return None;
    }
    let back_limit = (CLICK_SECONDS * rate as f32) as usize;
    let mut onset = loudest.0;
    let mut cursor = loudest.0;
    while cursor > 0 && loudest.0 - cursor < back_limit {
        let previous = cursor.saturating_sub(hop);
        if previous + window > capture.len()
            || rms(&capture[previous..previous + window]) <= threshold
        {
            break;
        }
        cursor = previous;
        onset = cursor;
    }
    Some(onset)
}

fn rms(samples: &[f32]) -> f32 {
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32).sqrt()
}

/// The best normalised cross-correlation of `template` against `capture`
/// over the lags `first..=last` (in samples).  The signal window's energy
/// rolls forward with the lag instead of being summed again each time.
fn best_lag(capture: &[f32], template: &[f32], first: usize, last: usize) -> Option<(usize, f32)> {
    let count = template.len();
    if count == 0 || last < first || last + count > capture.len() {
        return None;
    }
    let template_energy: f32 = template.iter().map(|s| s * s).sum();
    if template_energy <= 0.0 {
        return None;
    }
    let normalize = template_energy.sqrt();
    let mut energy: f32 = capture[first..first + count].iter().map(|s| s * s).sum();
    let mut best: Option<(usize, f32)> = None;
    for lag in first..=last {
        if lag > first {
            let out = capture[lag - 1];
            let in_ = capture[lag + count - 1];
            energy = (energy + in_ * in_ - out * out).max(0.0);
        }
        let dot: f32 = template
            .iter()
            .zip(&capture[lag..lag + count])
            .map(|(tap, sample)| tap * sample)
            .sum();
        let score = dot / (energy.sqrt() * normalize).max(f32::MIN_POSITIVE);
        if best.is_none_or(|(_, best_score)| score.abs() > best_score.abs()) {
            best = Some((lag, score));
        }
    }
    best
}

/// The armed measurement: the preallocated capture, the click and the
/// atomics the three threads share.
pub(crate) struct Calibration {
    /// The clock the stamps are relative to.  Set when the session is built,
    /// so every thread reads the same monotonic time base without a lock.
    epoch: Instant,
    state: AtomicU32,
    /// Monophonic raw capture, `f32` bits.  The input callback writes the
    /// slots below `captured`; only the UI thread reads them, after `Ready`.
    capture: Vec<AtomicU32>,
    captured: AtomicUsize,
    /// Frames kept before the capture window is full.
    limit: AtomicUsize,
    /// Frames of capture before the click may play.
    settle: AtomicUsize,
    /// Set by the mixer when the duck has reached silence.  Capture starts
    /// only after that, and after this many more input frames, so its noise
    /// floor is the input's own and not program material still in the
    /// capture pipeline.
    ducked: AtomicBool,
    hold_left: AtomicUsize,
    capture_start_ns: AtomicU64,
    capture_end_ns: AtomicU64,
    emitted_ns: AtomicU64,
    /// Bumped on every arm, so the mixer knows a new measurement started
    /// even when it never saw the state pass through `Idle` between blocks.
    generation: AtomicU64,
    /// The click as `f32` bits, written by the app while idle.
    click: Vec<AtomicU32>,
    click_len: AtomicUsize,
}

impl Calibration {
    pub(crate) fn new() -> Self {
        Self {
            epoch: Instant::now(),
            state: AtomicU32::new(CalibrationState::Idle as u32),
            capture: (0..CAPTURE_CAPACITY).map(|_| AtomicU32::new(0)).collect(),
            captured: AtomicUsize::new(0),
            limit: AtomicUsize::new(0),
            settle: AtomicUsize::new(0),
            ducked: AtomicBool::new(false),
            hold_left: AtomicUsize::new(0),
            capture_start_ns: AtomicU64::new(0),
            capture_end_ns: AtomicU64::new(0),
            emitted_ns: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            click: (0..MAX_CLICK_SAMPLES).map(|_| AtomicU32::new(0)).collect(),
            click_len: AtomicUsize::new(0),
        }
    }

    pub(crate) fn state(&self) -> CalibrationState {
        CalibrationState::from_u32(self.state.load(Ordering::Acquire))
    }

    /// Which arm this is.  The mixer resets its click position when it
    /// changes, so a retry armed straight after a result still plays.
    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Arms a measurement for an input running at `input_rate`, with
    /// `input_block` frames per input callback and `output_step` frames per
    /// output callback.  Call it from the UI thread; it refuses while another
    /// measurement or its result is still live, so the app cannot overwrite a
    /// capture being correlated.
    pub(crate) fn arm(
        &self,
        click: &[f32],
        input_rate: u32,
        input_block: u32,
        output_step: u32,
    ) -> Result<(), String> {
        if self.state() != CalibrationState::Idle {
            return Err("A measurement is already running.".to_owned());
        }
        if click.is_empty() || click.len() > MAX_CLICK_SAMPLES || input_rate == 0 {
            return Err("The measurement settings are invalid.".to_owned());
        }
        for (slot, sample) in self.click.iter().zip(click) {
            slot.store(sample.to_bits(), Ordering::Relaxed);
        }
        self.click_len.store(click.len(), Ordering::Release);
        self.captured.store(0, Ordering::Relaxed);
        self.ducked.store(false, Ordering::Relaxed);
        // Two input blocks (one being captured while the duck settles, one
        // for the host's queue), or a whole output step if that is longer: a
        // large independent output buffer must not leave its program tail in
        // the onset window either.
        self.hold_left.store(
            (2 * input_block as usize).max(output_step as usize).max(1),
            Ordering::Relaxed,
        );
        self.capture_start_ns.store(0, Ordering::Relaxed);
        self.capture_end_ns.store(0, Ordering::Relaxed);
        self.emitted_ns.store(0, Ordering::Relaxed);
        let limit = (CAPTURE_SECONDS * input_rate as f32).round() as usize;
        self.limit.store(
            limit.clamp(click.len(), CAPTURE_CAPACITY),
            Ordering::Relaxed,
        );
        let settle = (SETTLE_SECONDS * input_rate as f32).round() as usize;
        self.settle
            .store(settle.clamp(1, CAPTURE_CAPACITY / 2), Ordering::Relaxed);
        // Release: the input and output callbacks see the click and the
        // window only after they see `Armed`.
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.state
            .store(CalibrationState::Armed as u32, Ordering::Release);
        Ok(())
    }

    /// Ends a measurement from the UI thread.  The mixer ramps the mix back
    /// as soon as it sees `Idle`; nothing else was ever forced.
    pub(crate) fn finish(&self) {
        self.state
            .store(CalibrationState::Idle as u32, Ordering::Release);
    }

    /// One input frame, raw (before gain and channel enable), from whichever
    /// capture callback owns this side.  One relaxed load on the idle path.
    pub(crate) fn capture_frame(&self, left: f32, right: f32) {
        let state = self.state();
        if !matches!(
            state,
            CalibrationState::Armed
                | CalibrationState::Capturing
                | CalibrationState::Emitting
                | CalibrationState::Tail
        ) {
            return;
        }
        if state == CalibrationState::Armed {
            if !self.ducked.load(Ordering::Relaxed) {
                // The mix is still fading out; its tail would be part of the
                // capture's noise floor.
                return;
            }
            let hold = self.hold_left.load(Ordering::Relaxed);
            if hold > 0 {
                self.hold_left.store(hold - 1, Ordering::Relaxed);
                return;
            }
            let start = self.now_ns();
            // A cancel that landed first wins; this frame is dropped instead
            // of leaving the state ducked.
            if self
                .state
                .compare_exchange(
                    CalibrationState::Armed as u32,
                    CalibrationState::Capturing as u32,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                return;
            }
            self.capture_start_ns.store(start, Ordering::Release);
            if self.state() != CalibrationState::Capturing {
                // Cancelled between the transition and the stamp.
                return;
            }
        }
        let index = self.captured.load(Ordering::Relaxed);
        let limit = self.limit.load(Ordering::Relaxed);
        if index >= limit || index >= CAPTURE_CAPACITY {
            return;
        }
        let mono = 0.5 * (left + right);
        self.capture[index].store(mono.to_bits(), Ordering::Relaxed);
        // Release: whoever sees the higher count may read the sample.
        self.captured.store(index + 1, Ordering::Release);
        if index + 1 >= limit {
            self.capture_end_ns.store(self.now_ns(), Ordering::Release);
            let current = self.state();
            if matches!(current, CalibrationState::Emitting | CalibrationState::Tail) {
                // Never overwrite a cancel with `Ready`.
                if self
                    .state
                    .compare_exchange(
                        current as u32,
                        CalibrationState::Ready as u32,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_err()
                    && current == CalibrationState::Emitting
                {
                    let _ = self.state.compare_exchange(
                        CalibrationState::Tail as u32,
                        CalibrationState::Ready as u32,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                }
            }
        }
    }

    /// The mixer reached silence on the clock output; the input may start
    /// capturing.  Atomics only, so the output callback may call it.
    pub(crate) fn note_ducked(&self) {
        self.ducked.store(true, Ordering::Relaxed);
    }

    /// The mixer's half: whether the capture has run long enough that the
    /// click may play.
    pub(crate) fn settled(&self) -> bool {
        self.state() == CalibrationState::Capturing
            && self.captured.load(Ordering::Acquire) >= self.settle.load(Ordering::Relaxed)
    }

    /// Records the clock sample that carries the click's first frame, if the
    /// measurement is still capturing.  `false` when a cancel landed first;
    /// the mixer then plays nothing and lets the duck ramp home.
    pub(crate) fn note_emitting(&self, at: Instant) -> bool {
        let ns = at
            .saturating_duration_since(self.epoch)
            .as_nanos()
            .min(u64::MAX as u128) as u64;
        // Published before the state transition: the CAS releases, so a
        // reader that sees `Emitting` also sees the stamp.  A losing CAS
        // leaves the stamp behind, and `arm` clears it for the next run.
        self.emitted_ns.store(ns, Ordering::Release);
        self.state
            .compare_exchange(
                CalibrationState::Capturing as u32,
                CalibrationState::Emitting as u32,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// The click has played out; now only the capture window is left.
    pub(crate) fn note_emitted(&self) {
        let _ = self.state.compare_exchange(
            CalibrationState::Emitting as u32,
            CalibrationState::Tail as u32,
            Ordering::AcqRel,
            Ordering::Relaxed,
        );
    }

    pub(crate) fn click_sample(&self, index: usize) -> f32 {
        f32::from_bits(self.click[index.min(MAX_CLICK_SAMPLES - 1)].load(Ordering::Relaxed))
    }

    pub(crate) fn click_len(&self) -> usize {
        self.click_len.load(Ordering::Relaxed)
    }

    pub(crate) fn captured(&self) -> usize {
        self.captured.load(Ordering::Acquire).min(CAPTURE_CAPACITY)
    }

    /// The capture frames the current window holds (0 when idle).
    pub(crate) fn limit(&self) -> usize {
        self.limit.load(Ordering::Relaxed)
    }

    pub(crate) fn capture_start_ns(&self) -> u64 {
        self.capture_start_ns.load(Ordering::Acquire)
    }

    /// The input stamp of the frame that made the capture window full.
    pub(crate) fn capture_end_ns(&self) -> u64 {
        self.capture_end_ns.load(Ordering::Acquire)
    }

    /// Overrides the end stamp in tests, which fill the window without
    /// spending its real time.
    #[cfg(test)]
    pub(crate) fn set_capture_end_ns_for_test(&self, ns: u64) {
        self.capture_end_ns.store(ns, Ordering::Release);
    }

    pub(crate) fn emitted_ns(&self) -> u64 {
        self.emitted_ns.load(Ordering::Acquire)
    }

    /// Copies the captured mono signal for the correlator.  UI thread only.
    pub(crate) fn copy_capture(&self, out: &mut Vec<f32>) -> usize {
        let len = self.captured();
        out.clear();
        out.reserve(len);
        for slot in &self.capture[..len] {
            out.push(f32::from_bits(slot.load(Ordering::Relaxed)));
        }
        len
    }

    fn now_ns(&self) -> u64 {
        Instant::now()
            .saturating_duration_since(self.epoch)
            .as_nanos()
            .min(u64::MAX as u128) as u64
    }
}

impl Default for Calibration {
    fn default() -> Self {
        Self::new()
    }
}

/// The mixer's side of a measurement: the duck ramp and the click position.
/// Owned by [`super::mixer::OutputMixer`], so only the one output callback
/// touches it.
pub(crate) struct CalibrationEmitter {
    /// Current mix gain, 1 = untouched.  It moves continuously so neither
    /// the duck nor the restore clicks.
    gain: f32,
    click_pos: usize,
    emitting: bool,
    finished: bool,
    /// The arm this emitter has been serving.
    generation: u64,
    /// Whether the caller has been told the duck reached silence.
    ducked: bool,
}

impl CalibrationEmitter {
    pub(crate) fn new() -> Self {
        Self {
            gain: 1.0,
            click_pos: 0,
            emitting: false,
            finished: false,
            generation: 0,
            ducked: false,
        }
    }

    /// Called once per block, before any frame: picks the gain target and its
    /// step, and starts the click once capture has settled.  Returns
    /// `(target, step)` for [`Self::next`].
    pub(crate) fn begin(
        &mut self,
        calibration: &Calibration,
        rate: u32,
        now: Instant,
    ) -> (f32, f32) {
        let state = calibration.state();
        let generation = calibration.generation();
        if generation != self.generation {
            // A new arm, even one that arrived without an idle block in
            // between: start a fresh click.  The gain is not snapped back;
            // a cancel mid-duck still has to ramp home.
            self.generation = generation;
            self.click_pos = 0;
            self.emitting = false;
            self.finished = false;
            self.ducked = false;
        }
        if state == CalibrationState::Idle {
            self.emitting = false;
            self.finished = false;
        } else if state == CalibrationState::Ready {
            // The window closed before the click played out; stop emitting
            // and let the duck ramp home.
            self.emitting = false;
            self.finished = true;
        }
        if state.ducking() && !self.emitting && !self.finished && calibration.settled() {
            // A cancel may land between the settle check and the transition;
            // only play the click if this measurement still owns the state.
            if calibration.note_emitting(now) {
                self.emitting = true;
            } else {
                self.finished = true;
            }
        }
        let target = if state.ducking() { 0.0 } else { 1.0 };
        let step = 1.0 / (DUCK_SECONDS * rate.max(1) as f32).max(1.0);
        (target, step)
    }

    /// One frame: the duck gain to apply and the click sample to add.
    pub(crate) fn next(&mut self, calibration: &Calibration, target: f32, step: f32) -> (f32, f32) {
        self.gain = if self.gain < target {
            (self.gain + step).min(target)
        } else {
            (self.gain - step).max(target)
        };
        if self.gain == 0.0 && !self.ducked {
            self.ducked = true;
            calibration.note_ducked();
        }
        let click = if self.emitting {
            let sample = calibration.click_sample(self.click_pos);
            self.click_pos += 1;
            if self.click_pos >= calibration.click_len() {
                self.emitting = false;
                self.finished = true;
                calibration.note_emitted();
            }
            sample
        } else {
            0.0
        };
        (self.gain, click)
    }
}

#[cfg(test)]
mod tests;
