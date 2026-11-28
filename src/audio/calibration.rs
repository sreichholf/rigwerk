//! Loopback delay is `capture start + peak / input rate - click start`.
//! The two devices do not share a clock, and the stamp is the callback
//! start, so the result has block granularity.
//!
//! Shared transitions are compare-and-exchange, so a cancel cannot leave
//! the output ducked.  The phase word and the capture count carry the arm
//! generation: a callback still inside `capture_frame` cannot publish
//! into the next window.  The mixer latches one click buffer; `arm`
//! writes the other.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

pub(crate) const CLICK_SECONDS: f32 = 0.045;
/// The click waits until the mix has reached silence.
pub(crate) const DUCK_SECONDS: f32 = 0.03;
/// Long enough that the pre-click capture is a noise floor.
pub(crate) const SETTLE_SECONDS: f32 = 0.08;
/// A longer loop reports that it did not fit.
pub(crate) const CAPTURE_SECONDS: f32 = 1.2;
/// A longer chirp is cut.  The audio thread never allocates one.
pub(crate) const MAX_CLICK_SAMPLES: usize = 19_200;
/// 2 s at 96 kHz.
pub(crate) const CAPTURE_CAPACITY: usize = 192_000;
/// Fixed, so the measurement ignores the master fader.
pub(crate) const CLICK_LEVEL: f32 = 0.5;
/// At or above this the loop is too hot to trust.
pub(crate) const CLIP_LEVEL: f32 = 0.999;
/// −60 dBFS.  The onset detector has its own floor; this only rejects a
/// capture with nothing on it.
pub(crate) const QUIET_LEVEL: f32 = 0.001;
/// A loud tone that is not the chirp stays below this.
pub(crate) const MIN_SCORE: f32 = 0.35;
/// Longer than a loop through a cable or a mic.
pub(crate) const MAX_ROUND_TRIP_MS: f32 = 5_000.0;

const CHIRP_START_HZ: f32 = 400.0;
const CHIRP_END_HZ: f32 = 6_000.0;
const TAU: f32 = std::f32::consts::TAU;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CalibrationState {
    Idle = 0,
    Armed = 1,
    Capturing = 2,
    Emitting = 3,
    Tail = 4,
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

    pub(crate) fn ducking(self) -> bool {
        matches!(
            self,
            Self::Armed | Self::Capturing | Self::Emitting | Self::Tail
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum CalibrationFailure {
    Clipped,
    TooQuiet {
        level_db: f32,
    },
    /// No loop, or the wrong device.
    NoPeak {
        score: f32,
    },
    TooShort,
    OutOfWindow,
    /// A dropped input block: the delay would read short.
    Timing,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CaptureAnalysis {
    /// Capture sample where the click starts.
    pub(crate) peak: usize,
    /// Normalised correlation, 0..=1.
    pub(crate) score: f32,
    /// Peak level, dBFS.
    pub(crate) level_db: f32,
}

/// Hann-windowed linear chirp, so a coloured loop still has one sharp peak.
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

/// Energy-envelope onset, then correlation over a few milliseconds around
/// it.  The ducked mix leaves the click as the only burst.
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
    // The envelope marks where the burst is loud.  A windowed chirp starts
    // quieter, so the search looks back one click.
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

/// Each channel on its own.  A downmix cancels an inverted pair and buries
/// a one-sided loop.  The score is absolute, so an inverted click still hits.
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

/// The walk back stops after one click, so a room reflection cannot pull
/// the onset into the tail.
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

/// Normalised cross-correlation.  The window energy rolls with the lag.
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

/// Generation in the high bits, state in the low 8, so one exchange
/// names both.
fn pack(generation: u64, state: CalibrationState) -> u64 {
    (generation << 8) | state as u64
}

fn unpack(word: u64) -> (u64, CalibrationState) {
    (word >> 8, CalibrationState::from_u32((word & 0xff) as u32))
}

/// Skips 0: that word is the run's initial idle.
fn next_generation(generation: u64) -> u64 {
    let next = generation.wrapping_add(1) & 0xffff_ffff;
    if next == 0 {
        1
    } else {
        next
    }
}

pub(crate) struct Calibration {
    /// One clock for every thread, with no lock.
    epoch: Instant,
    /// [`pack`] of the arm generation and the state.
    phase: AtomicU64,
    /// `f32` bits.  The input callback writes below the published count;
    /// the UI reads them after `Ready`.
    capture_l: Vec<AtomicU32>,
    capture_r: Vec<AtomicU32>,
    /// Generation in the high 32 bits, frames in the low 32.  One exchange,
    /// so a stale callback cannot advance the next arm's count.
    captured: AtomicU64,
    limit: AtomicUsize,
    settle: AtomicUsize,
    /// Capture starts only after the duck, and after `hold_left` more
    /// frames, so the floor is the input and not program still in the pipe.
    ducked: AtomicBool,
    hold_left: AtomicUsize,
    capture_start_ns: AtomicU64,
    capture_end_ns: AtomicU64,
    emitted_ns: AtomicU64,
    /// `f32` bits, two copies.  `arm` fills the side the mixer has not latched.
    click: [Vec<AtomicU32>; 2],
    click_len: [AtomicUsize; 2],
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

    /// The mixer restarts the click when this changes, so a retry still plays.
    pub(crate) fn generation(&self) -> u64 {
        self.phase().0
    }

    /// Clears counters a stale callback might publish.  The caller stores
    /// the phase word last, so the new state is seen with the clear.
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

    /// UI thread.  Refuses while a measurement or its result is still live,
    /// so a capture being correlated is not overwritten.
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
        // The side the mixer is not reading.  It latches `click_side` when
        // it sees the new generation, published below.
        let side = (self.click_side.load(Ordering::Relaxed) ^ 1) as usize;
        for (slot, sample) in self.click[side].iter().zip(click) {
            slot.store(sample.to_bits(), Ordering::Relaxed);
        }
        self.click_len[side].store(click.len(), Ordering::Release);
        let generation = self.retire();
        // Two input blocks, or one output step if that is longer, so a large
        // output buffer cannot leave its tail in the onset window.
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
        // Release: the click and the window are visible with `Armed`.
        self.phase
            .store(pack(generation, CalibrationState::Armed), Ordering::Release);
        Ok(())
    }

    /// Moves the generation, so a callback past its state check cannot
    /// publish into the next arm.
    pub(crate) fn finish(&self) {
        let generation = self.retire();
        self.phase
            .store(pack(generation, CalibrationState::Idle), Ordering::Release);
    }

    /// One raw frame, before gain.  Idle is one relaxed load.
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
                // The mix is still fading; its tail would sit in the floor.
                return;
            }
            let hold = self.hold_left.load(Ordering::Relaxed);
            if hold > 0 {
                // The exchange names the value read, so a newer arm's hold
                // is not counted down.
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
            // The generation is in the word, so this cannot move a newer arm.
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
                // A cancel or a new arm landed before the stamp.  That arm
                // clears the stamp.
                return;
            }
        }
        self.commit_frame(generation, left, right);
    }

    /// The sample is stored before the count, and the count moves only
    /// while it still names this generation and this index.
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

    /// The exchange names this generation, so a cancel or a newer arm wins.
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

    /// Atomics only, so the output callback may call it.
    pub(crate) fn note_ducked(&self) {
        self.ducked.store(true, Ordering::Relaxed);
    }

    pub(crate) fn settled(&self) -> bool {
        self.state() == CalibrationState::Capturing
            && self.captured() >= self.settle.load(Ordering::Relaxed)
    }

    /// `false` when a cancel or a newer arm landed first.  The mixer then
    /// plays nothing and the duck ramps home.
    pub(crate) fn note_emitting(&self, at: Instant) -> bool {
        let (generation, state) = self.phase();
        if state != CalibrationState::Capturing {
            return false;
        }
        let ns = at
            .saturating_duration_since(self.epoch)
            .as_nanos()
            .min(u64::MAX as u128) as u64;
        // Before the transition, and the exchange releases, so `Emitting`
        // is seen with the stamp.  A loss leaves a stamp `retire` clears;
        // the next win on this thread writes that arm's stamp first.
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

    /// Published before the phase word, so the new generation is seen
    /// with this side.
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

    pub(crate) fn limit(&self) -> usize {
        self.limit.load(Ordering::Relaxed)
    }

    pub(crate) fn capture_start_ns(&self) -> u64 {
        self.capture_start_ns.load(Ordering::Acquire)
    }

    pub(crate) fn capture_end_ns(&self) -> u64 {
        self.capture_end_ns.load(Ordering::Acquire)
    }

    /// Tests fill the window without spending its time.
    #[cfg(test)]
    pub(crate) fn set_capture_end_ns_for_test(&self, ns: u64) {
        self.capture_end_ns.store(ns, Ordering::Release);
    }

    pub(crate) fn emitted_ns(&self) -> u64 {
        self.emitted_ns.load(Ordering::Acquire)
    }

    /// UI thread only.
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

/// Only the output callback touches it.
pub(crate) struct CalibrationEmitter {
    /// 1 is dry.  Ramped, so the duck does not click.
    gain: f32,
    click_pos: usize,
    emitting: bool,
    finished: bool,
    generation: u64,
    /// Set once `note_ducked` has been sent.
    ducked: bool,
    /// Latched for this generation.  `next` keeps it if `arm` publishes
    /// the other side mid-block.
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

    /// Once per block, before any frame.
    pub(crate) fn begin(
        &mut self,
        calibration: &Calibration,
        rate: u32,
        now: Instant,
    ) -> (f32, f32) {
        // One load.  A second could see a newer arm than the first.
        let (generation, state) = calibration.phase();
        if generation != self.generation {
            // A new arm, even with no idle block between.  The gain is not
            // snapped: a cancel mid-duck still ramps home.  This side is the
            // one `arm` published; the other is what a later arm may write.
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
            // The window closed before the click finished.  Ramp the duck home.
            self.emitting = false;
            self.finished = true;
        }
        if state.ducking() && !self.emitting && !self.finished && calibration.settled() {
            // A cancel may land between the settle check and the transition.
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
