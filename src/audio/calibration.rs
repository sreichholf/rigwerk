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
//! even when the loop colours the sound.  Both input channels are kept.
//! The correlator scores each one and keeps the better peak, so a loop on
//! one side still measures and a polarity-inverted pair does not cancel.
//!
//! The phase word and the capture count carry the arm's generation.  A
//! callback already inside [`Calibration::capture_frame`] when the UI
//! finishes and arms again cannot publish that frame into the new window:
//! its compare-and-exchange names the old generation.  The click lives in
//! two buffers.  The mixer latches one at the start of its block, and
//! [`Calibration::arm`] writes the other.

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

/// Scores each channel and keeps the better peak.  A mono downmix would
/// cancel a polarity-inverted pair and bury a one-sided loop in the other
/// channel; either channel on its own still correlates (an inverted click
/// included, because the score is absolute).
pub(crate) fn analyze_channels(
    left: &[f32],
    right: &[f32],
    template: &[f32],
    rate: u32,
) -> Result<CaptureAnalysis, CalibrationFailure> {
    match (
        analyze_capture(left, template, rate),
        analyze_capture(right, template, rate),
    ) {
        (Ok(left), Ok(right)) => Ok(if left.score >= right.score {
            left
        } else {
            right
        }),
        (Ok(found), Err(_)) | (Err(_), Ok(found)) => Ok(found),
        (Err(left), Err(right)) => Err(prefer_failure(left, right)),
    }
}

/// The failure that tells the person more.  A clip outranks a miss, and a
/// miss outranks silence; two misses keep the closer score.
fn prefer_failure(left: CalibrationFailure, right: CalibrationFailure) -> CalibrationFailure {
    let rank = |failure: &CalibrationFailure| match failure {
        CalibrationFailure::Clipped => 3,
        CalibrationFailure::NoPeak { .. } => 2,
        CalibrationFailure::TooQuiet { .. } => 1,
        CalibrationFailure::TooShort
        | CalibrationFailure::OutOfWindow
        | CalibrationFailure::Timing => 0,
    };
    let (rank_left, rank_right) = (rank(&left), rank(&right));
    if rank_left != rank_right {
        return if rank_left > rank_right { left } else { right };
    }
    match (left, right) {
        (
            CalibrationFailure::NoPeak { score: left },
            CalibrationFailure::NoPeak { score: right },
        ) => CalibrationFailure::NoPeak {
            score: left.max(right),
        },
        (
            CalibrationFailure::TooQuiet { level_db: left },
            CalibrationFailure::TooQuiet { level_db: right },
        ) => CalibrationFailure::TooQuiet {
            level_db: left.max(right),
        },
        (chosen, _) => chosen,
    }
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

/// Generation in the high bits, [`CalibrationState`] in the low 8.
/// A compare-and-exchange names both, so a callback still inside the
/// previous arm cannot move the new one.
fn pack(generation: u64, state: CalibrationState) -> u64 {
    (generation << 8) | state as u64
}

fn unpack(word: u64) -> (u64, CalibrationState) {
    (word >> 8, CalibrationState::from_u32((word & 0xff) as u32))
}

/// The next arm id.  Zero is the session's initial idle word, so a bump
/// that would wrap skips it.
fn next_generation(generation: u64) -> u64 {
    let next = generation.wrapping_add(1) & 0xffff_ffff;
    if next == 0 {
        1
    } else {
        next
    }
}

/// The armed measurement: the preallocated capture, the click and the
/// atomics the three threads share.
pub(crate) struct Calibration {
    /// The clock the stamps are relative to.  Set when the session is built,
    /// so every thread reads the same monotonic time base without a lock.
    epoch: Instant,
    /// [`pack`] of the arm generation and the [`CalibrationState`].
    phase: AtomicU64,
    /// Raw capture, `f32` bits, one buffer per channel.  The input callback
    /// writes the slots below the published count; only the UI thread reads
    /// them, after `Ready`.
    capture_l: Vec<AtomicU32>,
    capture_r: Vec<AtomicU32>,
    /// High 32 bits are the arm generation, low 32 the number of frames
    /// published.  Publishing is a compare-and-exchange on the whole word,
    /// so a stale callback cannot advance the next arm's count.
    captured: AtomicU64,
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
    /// The click as `f32` bits, two copies.  `arm` fills the side that is
    /// not published; the mixer reads only the side it latched.
    click: [Vec<AtomicU32>; 2],
    click_len: [AtomicUsize; 2],
    /// Which click buffer the mixer should latch.  0 or 1.
    click_side: AtomicU32,
}

impl Calibration {
    pub(crate) fn new() -> Self {
        let channel = || (0..CAPTURE_CAPACITY).map(|_| AtomicU32::new(0)).collect();
        let click = || (0..MAX_CLICK_SAMPLES).map(|_| AtomicU32::new(0)).collect();
        Self {
            epoch: Instant::now(),
            phase: AtomicU64::new(pack(0, CalibrationState::Idle)),
            capture_l: channel(),
            capture_r: channel(),
            captured: AtomicU64::new(0),
            limit: AtomicUsize::new(0),
            settle: AtomicUsize::new(0),
            ducked: AtomicBool::new(false),
            hold_left: AtomicUsize::new(0),
            capture_start_ns: AtomicU64::new(0),
            capture_end_ns: AtomicU64::new(0),
            emitted_ns: AtomicU64::new(0),
            click: [click(), click()],
            click_len: [AtomicUsize::new(0), AtomicUsize::new(0)],
            click_side: AtomicU32::new(0),
        }
    }

    fn phase(&self) -> (u64, CalibrationState) {
        unpack(self.phase.load(Ordering::Acquire))
    }

    pub(crate) fn state(&self) -> CalibrationState {
        self.phase().1
    }

    /// Which arm this is.  The mixer resets its click position when it
    /// changes, so a retry armed straight after a result still plays.
    pub(crate) fn generation(&self) -> u64 {
        self.phase().0
    }

    /// Bumps the generation and clears every counter a stale callback might
    /// still publish.  The phase word itself is stored by the caller, last,
    /// so a callback that sees the new state also sees the cleared counters
    /// and the click buffer.
    fn retire(&self) -> u64 {
        let generation = next_generation(self.generation());
        self.captured.store(generation << 32, Ordering::Relaxed);
        self.ducked.store(false, Ordering::Relaxed);
        self.hold_left.store(0, Ordering::Relaxed);
        self.capture_start_ns.store(0, Ordering::Relaxed);
        self.capture_end_ns.store(0, Ordering::Relaxed);
        self.emitted_ns.store(0, Ordering::Relaxed);
        self.limit.store(0, Ordering::Relaxed);
        self.settle.store(0, Ordering::Relaxed);
        generation
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
        // Write the buffer the mixer is not reading.  It latches `click_side`
        // when it notices the new generation, which is published below.
        let side = (self.click_side.load(Ordering::Relaxed) ^ 1) as usize;
        for (slot, sample) in self.click[side].iter().zip(click) {
            slot.store(sample.to_bits(), Ordering::Relaxed);
        }
        self.click_len[side].store(click.len(), Ordering::Release);
        let generation = self.retire();
        // Two input blocks (one being captured while the duck settles, one
        // for the host's queue), or a whole output step if that is longer: a
        // large independent output buffer must not leave its program tail in
        // the onset window either.
        self.hold_left.store(
            (2 * input_block as usize).max(output_step as usize).max(1),
            Ordering::Relaxed,
        );
        let limit = (CAPTURE_SECONDS * input_rate as f32).round() as usize;
        self.limit.store(
            limit.clamp(click.len(), CAPTURE_CAPACITY),
            Ordering::Relaxed,
        );
        let settle = (SETTLE_SECONDS * input_rate as f32).round() as usize;
        self.settle
            .store(settle.clamp(1, CAPTURE_CAPACITY / 2), Ordering::Relaxed);
        self.click_side.store(side as u32, Ordering::Release);
        // Release: the input and output callbacks see the click and the
        // window only after they see `Armed`.
        self.phase
            .store(pack(generation, CalibrationState::Armed), Ordering::Release);
        Ok(())
    }

    /// Ends a measurement from the UI thread.  The mixer ramps the mix back
    /// as soon as it sees `Idle`; nothing else was ever forced.  The
    /// generation moves too, so a callback already past its state check
    /// cannot publish into whatever is armed next.
    pub(crate) fn finish(&self) {
        let generation = self.retire();
        self.phase
            .store(pack(generation, CalibrationState::Idle), Ordering::Release);
    }

    /// One input frame, raw (before gain and channel enable), from whichever
    /// capture callback owns this side.  One relaxed load on the idle path.
    /// Both channels are stored; the correlator picks the better one.
    pub(crate) fn capture_frame(&self, left: f32, right: f32) {
        let (generation, state) = self.phase();
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
                // A newer arm replaces this hold.  The exchange names the
                // value we read, so it cannot count that arm's frames down.
                if self.generation() != generation {
                    return;
                }
                let _ = self.hold_left.compare_exchange(
                    hold,
                    hold - 1,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                );
                return;
            }
            let start = self.now_ns();
            // The generation is part of the word, so this cannot move a
            // newer arm out of `Armed`.
            if self
                .phase
                .compare_exchange(
                    pack(generation, CalibrationState::Armed),
                    pack(generation, CalibrationState::Capturing),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                return;
            }
            self.capture_start_ns.store(start, Ordering::Release);
            if self.generation() != generation || self.state() != CalibrationState::Capturing {
                // Cancelled, or a new arm landed, between the transition and
                // the stamp.  That arm clears the stamp itself.
                return;
            }
        }
        self.commit_frame(generation, left, right);
    }

    /// Publishes one frame for `generation`.  `false` when a newer arm owns
    /// the count, or the window is already full.  The sample is stored
    /// before the count moves, and the count moves only if it still names
    /// this generation and this index.
    pub(crate) fn commit_frame(&self, generation: u64, left: f32, right: f32) -> bool {
        let word = self.captured.load(Ordering::Acquire);
        if word >> 32 != generation {
            return false;
        }
        let index = word as u32 as usize;
        let limit = self.limit.load(Ordering::Relaxed);
        if index >= limit || index >= CAPTURE_CAPACITY {
            return false;
        }
        self.capture_l[index].store(left.to_bits(), Ordering::Relaxed);
        self.capture_r[index].store(right.to_bits(), Ordering::Relaxed);
        let next = (generation << 32) | (index as u64 + 1);
        if self
            .captured
            .compare_exchange(word, next, Ordering::Release, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        if index + 1 >= limit {
            self.capture_end_ns.store(self.now_ns(), Ordering::Release);
            self.mark_ready(generation);
        }
        true
    }

    /// Closes the window once the click has been handed to the output.
    /// A cancel or a newer arm wins: the exchange names this generation.
    fn mark_ready(&self, generation: u64) {
        let word = self.phase.load(Ordering::Acquire);
        if word >> 8 != generation {
            return;
        }
        let state = unpack(word).1;
        if !matches!(state, CalibrationState::Emitting | CalibrationState::Tail) {
            return;
        }
        if self
            .phase
            .compare_exchange(
                pack(generation, state),
                pack(generation, CalibrationState::Ready),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
            && state == CalibrationState::Emitting
        {
            let _ = self.phase.compare_exchange(
                pack(generation, CalibrationState::Tail),
                pack(generation, CalibrationState::Ready),
                Ordering::AcqRel,
                Ordering::Acquire,
            );
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
            && self.captured() >= self.settle.load(Ordering::Relaxed)
    }

    /// Records the clock sample that carries the click's first frame, if the
    /// measurement is still capturing.  `false` when a cancel or a newer arm
    /// landed first; the mixer then plays nothing and lets the duck ramp home.
    pub(crate) fn note_emitting(&self, at: Instant) -> bool {
        let (generation, state) = self.phase();
        if state != CalibrationState::Capturing {
            return false;
        }
        let ns = at
            .saturating_duration_since(self.epoch)
            .as_nanos()
            .min(u64::MAX as u128) as u64;
        // Before the transition.  The exchange releases, so an input callback
        // that sees `Emitting` also sees the stamp.  A losing exchange leaves
        // the stamp behind; `retire` clears it, and the next winning exchange
        // on this thread writes the stamp for that arm before it publishes
        // `Emitting`.
        self.emitted_ns.store(ns, Ordering::Release);
        self.phase
            .compare_exchange(
                pack(generation, CalibrationState::Capturing),
                pack(generation, CalibrationState::Emitting),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// The click has played out; now only the capture window is left.
    pub(crate) fn note_emitted(&self) {
        let (generation, state) = self.phase();
        if state != CalibrationState::Emitting {
            return;
        }
        let _ = self.phase.compare_exchange(
            pack(generation, CalibrationState::Emitting),
            pack(generation, CalibrationState::Tail),
            Ordering::AcqRel,
            Ordering::Relaxed,
        );
    }

    /// Which click buffer [`CalibrationEmitter`] should read.  Published
    /// before the phase word, so a mixer that sees the new generation also
    /// sees this.
    pub(crate) fn click_side(&self) -> u32 {
        self.click_side.load(Ordering::Acquire) & 1
    }

    pub(crate) fn click_sample(&self, side: usize, index: usize) -> f32 {
        let side = side & 1;
        f32::from_bits(self.click[side][index.min(MAX_CLICK_SAMPLES - 1)].load(Ordering::Relaxed))
    }

    pub(crate) fn click_len(&self, side: usize) -> usize {
        self.click_len[side & 1].load(Ordering::Acquire)
    }

    pub(crate) fn captured(&self) -> usize {
        let word = self.captured.load(Ordering::Acquire);
        if word >> 32 != self.generation() {
            return 0;
        }
        (word as u32 as usize).min(CAPTURE_CAPACITY)
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

    /// Copies both captured channels for the correlator.  UI thread only.
    pub(crate) fn copy_capture(&self, left: &mut Vec<f32>, right: &mut Vec<f32>) -> usize {
        let len = self.captured();
        left.clear();
        right.clear();
        left.reserve(len);
        right.reserve(len);
        for index in 0..len {
            left.push(f32::from_bits(
                self.capture_l[index].load(Ordering::Relaxed),
            ));
            right.push(f32::from_bits(
                self.capture_r[index].load(Ordering::Relaxed),
            ));
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
    /// The click buffer latched for `generation`.  `next` reads this side
    /// even if `arm` publishes the other one mid-block.
    click_side: usize,
    click_len: usize,
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
            click_side: 0,
            click_len: 0,
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
        // One load: the generation and the state share a word, and a second
        // load could observe a newer arm than the first.
        let (generation, state) = calibration.phase();
        if generation != self.generation {
            // A new arm, even one that arrived without an idle block in
            // between: start a fresh click.  The gain is not snapped back;
            // a cancel mid-duck still has to ramp home.  The side is the
            // one `arm` published for this generation; the other buffer is
            // what a later arm may already be writing.
            self.generation = generation;
            self.click_pos = 0;
            self.emitting = false;
            self.finished = false;
            self.ducked = false;
            self.click_side = calibration.click_side() as usize;
            self.click_len = calibration.click_len(self.click_side);
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
            let sample = if self.click_pos < self.click_len {
                calibration.click_sample(self.click_side, self.click_pos)
            } else {
                0.0
            };
            self.click_pos += 1;
            if self.click_pos >= self.click_len {
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
