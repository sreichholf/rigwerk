//! Loudness measurement for one output device, ITU-R BS.1770-4 / EBU R128.
//!
//! The mixer pushes each device's finished frame here, after its level, trim,
//! safety ceiling and mute, and without the metronome.  The meter keeps the
//! K-weighted mean square in 100 ms steps and publishes three readings:
//! momentary (400 ms), short-term (3 s) and integrated (gated 400 ms blocks
//! over the last 60 s).  Integrated is a 60 s window rather than the whole
//! run, so its fixed storage stays small and it follows a changed rig.
//!
//! One [`LoudnessMeter`] lives in each `DeviceMapper`.  It is preallocated,
//! never locks and never allocates, and all of its state is owned by the
//! callback that feeds it.  A reading below the absolute gate is published
//! as [`SILENCE_LUFS`], which the UI draws as no reading.
//!
//! # K-weighting
//!
//! The two biquads are the standard's high shelf and RLB high-pass.  Their
//! coefficients come from the analog prototypes with a bilinear transform,
//! so every supported rate gets the same curve; at 48 kHz they reproduce the
//! published coefficients.  A 1 kHz sine at 0 dBFS peak therefore reads
//! about -3.0 LUFS, and at -20 dBFS about -23.0 LUFS.

use super::output_map::MAX_OUTPUT_CHANNELS;

/// The absolute gate, LUFS.  A reading at or below this is silence.
pub(crate) const SILENCE_LUFS: f32 = -70.0;

/// 100 ms measurement step.  A 400 ms block is four steps, short-term is 30
/// steps, and the integrated history is 600 steps (60 s).
const STEP_MILLIS: u32 = 100;
const MOMENTARY_STEPS: usize = 4;
const SHORT_TERM_STEPS: usize = 30;
const HISTORY_BLOCKS: usize = 600;

/// Loudness offset of the standard: `-0.691 + 10 log10(mean square)`.
const LOUDNESS_OFFSET: f64 = -0.691;

/// The absolute and relative gates of the integrated measurement, LU.
const ABSOLUTE_GATE_LUFS: f64 = -70.0;
const RELATIVE_GATE_LU: f64 = -10.0;

/// One published set of readings for a device.  `trim_db` is the automatic
/// loudness trim the mapper applied in that block, not a measurement.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LoudnessReading {
    /// 400 ms window, LUFS.
    pub momentary_lufs: f32,
    /// 3 s window, LUFS.
    pub short_term_lufs: f32,
    /// Gated 400 ms blocks over the last 60 s, LUFS.
    pub integrated_lufs: f32,
    /// The automatic loudness trim in dB.  0 when automatic level is off or
    /// the trim has not moved; a settled trim stays at its value.
    pub trim_db: f32,
}

impl Default for LoudnessReading {
    fn default() -> Self {
        Self {
            momentary_lufs: SILENCE_LUFS,
            short_term_lufs: SILENCE_LUFS,
            integrated_lufs: SILENCE_LUFS,
            trim_db: 0.0,
        }
    }
}

impl LoudnessReading {
    /// Whether the meter has a momentary reading at all.  A silent device
    /// still has a trim, so only the loudness figure decides this.
    #[cfg(test)]
    pub(crate) fn has_reading(&self) -> bool {
        self.momentary_lufs > SILENCE_LUFS
    }
}

/// A second-order section in transposed direct form II, `a0` normalised to 1.
#[derive(Clone, Copy, Debug, Default)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

/// The two K-weighting sections of one channel.
#[derive(Clone, Copy, Debug, Default)]
struct KWeight {
    shelf: Biquad,
    high_pass: Biquad,
}

impl KWeight {
    fn new(rate: f32) -> Self {
        // Analog prototypes from BS.1770-4.  The shelf's `Vb` is the
        // standard's exponent, which places its mid-band gain correctly.
        let shelf = high_shelf(rate);
        let high_pass = high_pass(rate);
        Self { shelf, high_pass }
    }

    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        self.high_pass.process(self.shelf.process(x))
    }
}

/// The standard's high shelf, built from its analog prototype.
fn high_shelf(rate: f32) -> Biquad {
    const GAIN_DB: f64 = 3.999843853973347;
    const Q: f64 = 0.7071752369554196;
    const HZ: f64 = 1681.974450955533;
    let k = (std::f64::consts::PI * HZ / f64::from(rate)).tan();
    let vh = 10.0f64.powf(GAIN_DB / 20.0);
    let vb = vh.powf(0.4996667741545416);
    let a0 = 1.0 + k / Q + k * k;
    Biquad {
        b0: ((vh + vb * k / Q + k * k) / a0) as f32,
        b1: (2.0 * (k * k - vh) / a0) as f32,
        b2: ((vh - vb * k / Q + k * k) / a0) as f32,
        a1: (2.0 * (k * k - 1.0) / a0) as f32,
        a2: ((1.0 - k / Q + k * k) / a0) as f32,
        z1: 0.0,
        z2: 0.0,
    }
}

/// The RLB high-pass, built from its analog prototype.
fn high_pass(rate: f32) -> Biquad {
    const Q: f64 = 0.5003270373238773;
    const HZ: f64 = 38.13547087602444;
    let k = (std::f64::consts::PI * HZ / f64::from(rate)).tan();
    let a0 = 1.0 + k / Q + k * k;
    Biquad {
        b0: (1.0 / a0) as f32,
        b1: (-2.0 / a0) as f32,
        b2: (1.0 / a0) as f32,
        a1: (2.0 * (k * k - 1.0) / a0) as f32,
        a2: ((1.0 - k / Q + k * k) / a0) as f32,
        z1: 0.0,
        z2: 0.0,
    }
}

/// Loudness of one mean-square energy, before clamping: the standard's
/// `-0.691 + 10 log10(E)`.  Silence and non-finite values return the
/// absolute gate.
fn loudness(energy: f64) -> f64 {
    if energy <= 0.0 || !energy.is_finite() {
        ABSOLUTE_GATE_LUFS
    } else {
        (LOUDNESS_OFFSET + 10.0 * energy.log10()).max(ABSOLUTE_GATE_LUFS)
    }
}

/// One device's loudness meter.  The callback that owns the device calls
/// [`Self::push`] for every frame and [`Self::reading`] once per block.
pub(crate) struct LoudnessMeter {
    rate: u32,
    channels: usize,
    /// Frames in one 100 ms step.
    step_frames: u32,
    /// Frames left in the current step.
    frames_left: u32,
    /// Per-channel K filter and squared sum for the current step.
    filters: [KWeight; MAX_OUTPUT_CHANNELS],
    sums: [f64; MAX_OUTPUT_CHANNELS],
    /// The last 30 steps' summed channel energies, oldest first in a ring.
    steps: [f32; SHORT_TERM_STEPS],
    step_head: u64,
    /// The last 600 four-step block energies, oldest first in a ring.
    blocks: [f32; HISTORY_BLOCKS],
    block_head: u64,
    block_count: usize,
    reading: LoudnessReading,
}

impl LoudnessMeter {
    pub(crate) fn new(rate: u32) -> Self {
        let rate = rate.max(1);
        Self {
            rate,
            channels: 0,
            step_frames: step_frames(rate),
            frames_left: step_frames(rate),
            filters: [KWeight::default(); MAX_OUTPUT_CHANNELS],
            sums: [0.0; MAX_OUTPUT_CHANNELS],
            steps: [0.0; SHORT_TERM_STEPS],
            step_head: 0,
            blocks: [0.0; HISTORY_BLOCKS],
            block_head: 0,
            block_count: 0,
            reading: LoudnessReading::default(),
        }
    }

    /// Points the meter at a device with `channels` real channels and a new
    /// rate.  A change forgets every measurement: the filters and window
    /// would not describe the new device.
    pub(crate) fn configure(&mut self, rate: u32, channels: usize) {
        let rate = rate.max(1);
        let channels = channels.min(MAX_OUTPUT_CHANNELS);
        if rate == self.rate && channels == self.channels {
            return;
        }
        self.rate = rate;
        self.channels = channels;
        self.step_frames = step_frames(rate);
        self.filters = std::array::from_fn(|_| KWeight::new(rate as f32));
        self.reset();
    }

    /// Forgets every window and reading.  The filters start clean too, so a
    /// reset after a pause does not carry the old signal's state.
    pub(crate) fn reset(&mut self) {
        self.filters = std::array::from_fn(|_| KWeight::new(self.rate as f32));
        self.sums = [0.0; MAX_OUTPUT_CHANNELS];
        self.frames_left = self.step_frames;
        self.steps = [0.0; SHORT_TERM_STEPS];
        self.step_head = 0;
        self.blocks = [0.0; HISTORY_BLOCKS];
        self.block_head = 0;
        self.block_count = 0;
        self.reading = LoudnessReading::default();
    }

    /// One finished frame of `channels` real channels, already after the
    /// device level, trim, ceiling and mute, and without the metronome.
    #[inline]
    pub(crate) fn push(&mut self, frame: &[f32]) {
        let count = frame.len().min(self.channels);
        for (channel, sample) in frame.iter().take(count).enumerate() {
            let weighted = self.filters[channel].process(*sample) as f64;
            self.sums[channel] += weighted * weighted;
        }
        self.frames_left -= 1;
        if self.frames_left == 0 {
            self.finish_step();
        }
    }

    fn finish_step(&mut self) {
        self.frames_left = self.step_frames;
        let mut energy = 0.0f64;
        let frames = self.step_frames as f64;
        for channel in 0..self.channels {
            energy += self.sums[channel] / frames;
            self.sums[channel] = 0.0;
        }
        let energy = energy as f32;
        self.steps[(self.step_head % SHORT_TERM_STEPS as u64) as usize] = energy;
        self.step_head = self.step_head.wrapping_add(1);

        // A 400 ms block is the mean of the last four 100 ms steps.
        if self.step_head >= MOMENTARY_STEPS as u64 {
            let mut block = 0.0f64;
            for offset in 0..MOMENTARY_STEPS {
                let index = (self.step_head - MOMENTARY_STEPS as u64 + offset as u64)
                    % SHORT_TERM_STEPS as u64;
                block += self.steps[index as usize] as f64;
            }
            let block = (block / MOMENTARY_STEPS as f64) as f32;
            self.blocks[(self.block_head % HISTORY_BLOCKS as u64) as usize] = block;
            self.block_head = self.block_head.wrapping_add(1);
            self.block_count = (self.block_count + 1).min(HISTORY_BLOCKS);
        }
        self.reading = self.measure();
    }

    /// The current momentary, short-term and integrated figures.
    fn measure(&self) -> LoudnessReading {
        LoudnessReading {
            momentary_lufs: self.momentary(),
            short_term_lufs: self.short_term(),
            integrated_lufs: self.integrated(),
            trim_db: self.reading.trim_db,
        }
    }

    fn momentary(&self) -> f32 {
        if self.step_head < MOMENTARY_STEPS as u64 {
            return SILENCE_LUFS;
        }
        let mut energy = 0.0f64;
        for offset in 0..MOMENTARY_STEPS {
            let index =
                (self.step_head - MOMENTARY_STEPS as u64 + offset as u64) % SHORT_TERM_STEPS as u64;
            energy += self.steps[index as usize] as f64;
        }
        loudness(energy / MOMENTARY_STEPS as f64).max(SILENCE_LUFS as f64) as f32
    }

    fn short_term(&self) -> f32 {
        if self.step_head < SHORT_TERM_STEPS as u64 {
            return SILENCE_LUFS;
        }
        let mut energy = 0.0f64;
        for offset in 0..SHORT_TERM_STEPS {
            let index = (self.step_head - SHORT_TERM_STEPS as u64 + offset as u64)
                % SHORT_TERM_STEPS as u64;
            energy += self.steps[index as usize] as f64;
        }
        loudness(energy / SHORT_TERM_STEPS as f64).max(SILENCE_LUFS as f64) as f32
    }

    /// Integrated loudness over the last 60 s, gated exactly as BS.1770-4
    /// says: drop blocks below the absolute gate, then those more than 10 LU
    /// below the gated mean.  The count is the 400 ms blocks that passed the
    /// absolute gate, for a caller that needs to know how much of the window
    /// was real signal.
    pub(crate) fn integrated_with_blocks(&self) -> (f32, usize) {
        if self.block_count == 0 {
            return (SILENCE_LUFS, 0);
        }
        let start = self.block_head.saturating_sub(self.block_count as u64);
        let block = |offset: usize| -> f64 {
            let index = (start + offset as u64) % HISTORY_BLOCKS as u64;
            self.blocks[index as usize] as f64
        };
        let mut absolute_sum = 0.0f64;
        let mut absolute_count = 0usize;
        for offset in 0..self.block_count {
            let energy = block(offset);
            if loudness(energy) > ABSOLUTE_GATE_LUFS {
                absolute_sum += energy;
                absolute_count += 1;
            }
        }
        if absolute_count == 0 {
            return (SILENCE_LUFS, 0);
        }
        let relative_gate = loudness(absolute_sum / absolute_count as f64) + RELATIVE_GATE_LU;
        let mut sum = 0.0f64;
        let mut count = 0usize;
        for offset in 0..self.block_count {
            let energy = block(offset);
            let level = loudness(energy);
            if level > ABSOLUTE_GATE_LUFS && level > relative_gate {
                sum += energy;
                count += 1;
            }
        }
        if count == 0 {
            (SILENCE_LUFS, absolute_count)
        } else {
            (
                loudness(sum / count as f64).max(SILENCE_LUFS as f64) as f32,
                absolute_count,
            )
        }
    }

    fn integrated(&self) -> f32 {
        self.integrated_with_blocks().0
    }

    /// The latest reading with `trim_db` filled in.  The caller publishes it
    /// once per block; the audio thread never allocates here.
    pub(crate) fn reading(&self, trim_db: f32) -> LoudnessReading {
        LoudnessReading {
            trim_db,
            ..self.reading
        }
    }
}

fn step_frames(rate: u32) -> u32 {
    (u64::from(rate) * u64::from(STEP_MILLIS) / 1000).max(1) as u32
}

#[cfg(test)]
mod tests;
