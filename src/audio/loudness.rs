//! ITU-R BS.1770-4 loudness for one output.  Integrated is the last 60 s,
//! not the whole run, so storage stays fixed.  The K-weighting biquads are
//! bilinear transforms of the standard's analog prototypes.
//!
//! Preallocated.  No lock, no allocation.  The callback that owns the meter
//! is the only writer.  A reading at or below the absolute gate is
//! [`SILENCE_LUFS`].

use super::output_map::MAX_OUTPUT_CHANNELS;

/// Absolute gate, LUFS.  At or below this the UI draws no reading.
pub(crate) const SILENCE_LUFS: f32 = -70.0;

const STEP_MILLIS: u32 = 100;
const MOMENTARY_STEPS: usize = 4;
const SHORT_TERM_STEPS: usize = 30;
const HISTORY_BLOCKS: usize = 600;

/// BS.1770: `-0.691 + 10 log10(mean square)`.
const LOUDNESS_OFFSET: f64 = -0.691;

const ABSOLUTE_GATE_LUFS: f64 = -70.0;
const RELATIVE_GATE_LU: f64 = -10.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LoudnessReading {
    pub momentary_lufs: f32,
    pub short_term_lufs: f32,
    pub integrated_lufs: f32,
    /// Mapper trim for this block, not a loudness figure.
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
    /// Trim does not count: a silent device still has one.
    #[cfg(test)]
    pub(crate) fn has_reading(&self) -> bool {
        self.momentary_lufs > SILENCE_LUFS
    }
}

/// Transposed direct form II, `a0` = 1.
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

#[derive(Clone, Copy, Debug, Default)]
struct KWeight {
    shelf: Biquad,
    high_pass: Biquad,
}

impl KWeight {
    fn new(rate: f32) -> Self {
        let shelf = high_shelf(rate);
        let high_pass = high_pass(rate);
        Self { shelf, high_pass }
    }

    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        self.high_pass.process(self.shelf.process(x))
    }
}

/// BS.1770-4 high shelf.
fn high_shelf(rate: f32) -> Biquad {
    const GAIN_DB: f64 = 3.999843853973347;
    const Q: f64 = 0.7071752369554196;
    const HZ: f64 = 1681.974450955533;
    let k = (std::f64::consts::PI * HZ / f64::from(rate)).tan();
    let vh = 10.0f64.powf(GAIN_DB / 20.0);
    // The standard's exponent.  It places the mid-band gain.
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

/// BS.1770-4 RLB high-pass.
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

fn loudness(energy: f64) -> f64 {
    if energy <= 0.0 || !energy.is_finite() {
        ABSOLUTE_GATE_LUFS
    } else {
        (LOUDNESS_OFFSET + 10.0 * energy.log10()).max(ABSOLUTE_GATE_LUFS)
    }
}

/// [`Self::push`] every frame, [`Self::reading`] once per block.
pub(crate) struct LoudnessMeter {
    rate: u32,
    channels: usize,
    step_frames: u32,
    frames_left: u32,
    filters: [KWeight; MAX_OUTPUT_CHANNELS],
    sums: [f64; MAX_OUTPUT_CHANNELS],
    steps: [f32; SHORT_TERM_STEPS],
    step_head: u64,
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

    /// A rate or channel change forgets every measurement.
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

    /// Filters restart clean, so a pause does not carry the old signal.
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

    /// One finished frame: after level, trim, ceiling and mute, and without
    /// the metronome.
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

    /// Last 60 s.  The count is blocks above the absolute gate.
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

    /// Once per block.  No allocation.
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
