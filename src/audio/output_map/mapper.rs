//! Realtime device mapping: controls atomics, frames and the mapper.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};

use super::super::delay::{DelayLine, DelayTap, MonoDelayLine, TAP_FADE_SECONDS};
use super::super::limiter::SafetyLimiter;
use super::super::loudness::{LoudnessMeter, LoudnessReading};
use super::super::mixer::Ramp;
use super::super::path::{LiveSlots, MAX_PATHS};
use super::align::{plan_alignment, trim_frames, AlignmentPlan, AlignmentReport, MAX_ALIGN_DELAY};
use super::types::{
    ChannelMap, LoudnessSettings, OutputDeviceSettings, UnusedChannels, MAX_OUTPUT_CHANNELS,
};
use super::weights::{level_gain, path_channel_weights, routed_paths};

/// What the mapper needs to align the paths in one block: each path's
/// chain latency (frames, read once per block) and the rate.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PathTiming {
    pub(crate) latency: [u32; MAX_PATHS],
    pub(crate) rate: u32,
}

impl Default for PathTiming {
    fn default() -> Self {
        Self {
            latency: [0; MAX_PATHS],
            rate: 48_000,
        }
    }
}

/// One delay line per path slot, written once per frame by the mixer and
/// read by every device's taps.
pub(crate) type PathLines = [DelayLine; MAX_PATHS];

/// "Not played here" in the published per-path delays.
const NOT_ROUTED: u32 = u32::MAX;
/// "Waits for no path" in the published `waits_for`.
const NO_PATH: u8 = u8::MAX;

/// How long a learned trim change takes to reach the audio.
const TRIM_RAMP_SECONDS: f32 = 0.2;

/// One path's channel map and signed gain (level and polarity) on one
/// device, as the callback reads it.
struct PathMap {
    left: AtomicU8,
    right: AtomicU8,
    /// Linear gain (`f32` bits), negative when the polarity is flipped.
    gain: AtomicU32,
    /// Alignment trim, ms (`f32` bits), already clamped.
    trim_ms: AtomicU32,
}

impl PathMap {
    fn new(map: ChannelMap) -> Self {
        Self {
            left: AtomicU8::new(map.left),
            right: AtomicU8::new(map.right),
            gain: AtomicU32::new(1.0f32.to_bits()),
            trim_ms: AtomicU32::new(0.0f32.to_bits()),
        }
    }

    fn trim_ms(&self) -> f32 {
        f32::from_bits(self.trim_ms.load(Ordering::Relaxed))
    }

    fn store(&self, map: ChannelMap) {
        self.left.store(map.left, Ordering::Relaxed);
        self.right.store(map.right, Ordering::Relaxed);
    }

    fn gain(&self) -> f32 {
        f32::from_bits(self.gain.load(Ordering::Relaxed))
    }

    fn load(&self) -> ChannelMap {
        ChannelMap {
            left: self.left.load(Ordering::Relaxed),
            right: self.right.load(Ordering::Relaxed),
        }
    }
}

/// One output device's settings as the audio callback reads them, plus its
/// per-channel meters.  The UI stores, the callback loads once per block.
///
/// Each signal path slot has its own channel map, level and polarity on the
/// device; the device level, mute and the unused-channel rule belong to the
/// device.
pub(crate) struct DeviceControls {
    maps: [PathMap; MAX_PATHS],
    silent: AtomicBool,
    /// Linear level gain (`f32` bits), already clamped.
    gain: AtomicU32,
    mute: AtomicBool,
    /// Which channels carry the metronome.  Bit `n` is channel `n`.
    /// 0 adds the tick nowhere.
    click_mask: AtomicU8,
    /// Block peak per device channel (`f32` bits), after the map, the
    /// device level, the safety ceiling, the metronome and mute.  Channels
    /// the device does not have read 0.
    meters: [AtomicU32; MAX_OUTPUT_CHANNELS],
    /// Safety-limiter gain reduction, dB (`f32` bits).  0 when the device
    /// is not limiting.  Published with the meters.
    reduction_db: AtomicU32,
    /// Target and learned trim, as the mapper reads them.
    loudness_target: AtomicU32,
    loudness_learned: AtomicU32,
    /// The learned trim the mapper last applied, dB (`f32` bits).
    loudness_trim: AtomicU32,
    /// Momentary, short-term and integrated loudness, LUFS (`f32` bits).
    loudness_momentary: AtomicU32,
    loudness_short_term: AtomicU32,
    loudness_integrated: AtomicU32,
    /// Set by the UI to clear the meter's windows and integrated history.
    loudness_reset: AtomicBool,
    /// Bumped when a different device moves into this output slot, so the
    /// mapper drops the old device's trim and history.
    device_generation: AtomicU64,
    /// One output Learn in progress: bumped on every start/stop, plus the
    /// integrated reading and how many 400 ms blocks passed the absolute
    /// gate.
    learn_active: AtomicBool,
    learn_generation: AtomicU64,
    learn_integrated: AtomicU32,
    learn_blocks: AtomicU32,
    /// The device's real channel count, published when its stream opens.
    /// 0 = unknown (bridge not running); the UI falls back to the device
    /// inventory's channel count.  ASIO opens at most 4 outputs for now;
    /// the map sees that count.
    channels: AtomicU32,
    /// The align switch (see [`OutputDeviceSettings::align`]).
    align: AtomicBool,
    /// What the mapper last aligned with, for the UI: the delay per path
    /// slot ([`NOT_ROUTED`] when the device does not play it), the path it
    /// waits for ([`NO_PATH`]), each path's chain latency, and the rate (0:
    /// no mixer has published in this run yet).
    applied: [AtomicU32; MAX_PATHS],
    applied_latency: [AtomicU32; MAX_PATHS],
    waits_for: AtomicU8,
    applied_rate: AtomicU32,
}

impl DeviceControls {
    pub(crate) fn new() -> Self {
        let controls = Self {
            maps: std::array::from_fn(|_| PathMap::new(ChannelMap::default())),
            silent: AtomicBool::new(false),
            gain: AtomicU32::new(1.0f32.to_bits()),
            mute: AtomicBool::new(false),
            click_mask: AtomicU8::new(0),
            meters: std::array::from_fn(|_| AtomicU32::new(0.0f32.to_bits())),
            reduction_db: AtomicU32::new(0.0f32.to_bits()),
            loudness_target: AtomicU32::new(super::types::LOUDNESS_TARGET_DEFAULT_LUFS.to_bits()),
            loudness_learned: AtomicU32::new(0.0f32.to_bits()),
            loudness_trim: AtomicU32::new(0.0f32.to_bits()),
            loudness_momentary: AtomicU32::new(super::super::SILENCE_LUFS.to_bits()),
            loudness_short_term: AtomicU32::new(super::super::SILENCE_LUFS.to_bits()),
            loudness_integrated: AtomicU32::new(super::super::SILENCE_LUFS.to_bits()),
            loudness_reset: AtomicBool::new(false),
            device_generation: AtomicU64::new(0),
            learn_active: AtomicBool::new(false),
            learn_generation: AtomicU64::new(0),
            learn_integrated: AtomicU32::new(super::super::SILENCE_LUFS.to_bits()),
            learn_blocks: AtomicU32::new(0),
            channels: AtomicU32::new(0),
            align: AtomicBool::new(true),
            applied: std::array::from_fn(|_| AtomicU32::new(NOT_ROUTED)),
            applied_latency: std::array::from_fn(|_| AtomicU32::new(0)),
            waits_for: AtomicU8::new(NO_PATH),
            applied_rate: AtomicU32::new(0),
        };
        controls.set(&OutputDeviceSettings::default());
        controls
    }

    /// Stores the device settings, every path slot's map, level and
    /// polarity included.
    pub(crate) fn set(&self, settings: &OutputDeviceSettings) {
        let settings = settings.sanitized();
        for (to, from) in self.maps.iter().zip(&settings.path_map) {
            to.store(from.map);
            to.gain.store(from.gain().to_bits(), Ordering::Relaxed);
            to.trim_ms.store(from.trim_ms.to_bits(), Ordering::Relaxed);
        }
        self.align.store(settings.align, Ordering::Relaxed);
        self.silent
            .store(settings.unused == UnusedChannels::Silent, Ordering::Relaxed);
        self.gain
            .store(level_gain(settings.level_db).to_bits(), Ordering::Relaxed);
        self.mute.store(settings.mute, Ordering::Relaxed);
        self.click_mask
            .store(settings.click_mask, Ordering::Relaxed);
        self.set_loudness(&settings.loudness);
    }

    /// Stores the loudness target and learned trim, sanitized.
    fn set_loudness(&self, settings: &LoudnessSettings) {
        let settings = settings.sanitized();
        self.loudness_target
            .store(settings.target_lufs.to_bits(), Ordering::Relaxed);
        self.loudness_learned
            .store(settings.trim_db.to_bits(), Ordering::Relaxed);
    }

    /// Reads the loudness target and learned trim.
    pub(crate) fn loudness_settings(&self) -> LoudnessSettings {
        LoudnessSettings {
            target_lufs: f32::from_bits(self.loudness_target.load(Ordering::Relaxed)),
            trim_db: f32::from_bits(self.loudness_learned.load(Ordering::Relaxed)),
        }
    }

    /// Publishes one block's loudness reading.
    pub(crate) fn publish_loudness(&self, reading: LoudnessReading) {
        self.loudness_momentary
            .store(reading.momentary_lufs.to_bits(), Ordering::Relaxed);
        self.loudness_short_term
            .store(reading.short_term_lufs.to_bits(), Ordering::Relaxed);
        self.loudness_integrated
            .store(reading.integrated_lufs.to_bits(), Ordering::Relaxed);
        self.loudness_trim
            .store(reading.trim_db.to_bits(), Ordering::Relaxed);
    }

    /// The latest loudness reading.
    pub(crate) fn loudness(&self) -> LoudnessReading {
        LoudnessReading {
            momentary_lufs: f32::from_bits(self.loudness_momentary.load(Ordering::Relaxed)),
            short_term_lufs: f32::from_bits(self.loudness_short_term.load(Ordering::Relaxed)),
            integrated_lufs: f32::from_bits(self.loudness_integrated.load(Ordering::Relaxed)),
            trim_db: f32::from_bits(self.loudness_trim.load(Ordering::Relaxed)),
        }
    }

    /// Clears the meter's windows and integrated history on the next block.
    pub(crate) fn request_loudness_reset(&self) {
        self.loudness_reset.store(true, Ordering::Relaxed);
    }

    /// Takes a pending reset request.
    pub(crate) fn take_loudness_reset(&self) -> bool {
        self.loudness_reset.swap(false, Ordering::Relaxed)
    }

    /// The identity generation of the device in this slot.
    pub(crate) fn device_generation(&self) -> u64 {
        self.device_generation.load(Ordering::Relaxed)
    }

    /// Marks that a different device now lives in this slot.
    pub(crate) fn note_new_device(&self) {
        self.device_generation.fetch_add(1, Ordering::Relaxed);
        self.stop_learn();
    }

    /// Starts an output Learn.
    pub(crate) fn start_learn(&self) {
        self.learn_active.store(true, Ordering::Relaxed);
        self.learn_generation.fetch_add(1, Ordering::Relaxed);
        self.learn_integrated
            .store(super::super::SILENCE_LUFS.to_bits(), Ordering::Relaxed);
        self.learn_blocks.store(0, Ordering::Relaxed);
    }

    /// Stops an output Learn and clears its published reading.
    pub(crate) fn stop_learn(&self) {
        self.learn_active.store(false, Ordering::Relaxed);
        self.learn_generation.fetch_add(1, Ordering::Relaxed);
        self.learn_integrated
            .store(super::super::SILENCE_LUFS.to_bits(), Ordering::Relaxed);
        self.learn_blocks.store(0, Ordering::Relaxed);
    }

    /// Whether an output Learn is running.
    pub(crate) fn learn_active(&self) -> bool {
        self.learn_active.load(Ordering::Relaxed)
    }

    /// The Learn generation, bumped on every start and stop.
    pub(crate) fn learn_generation(&self) -> u64 {
        self.learn_generation.load(Ordering::Relaxed)
    }

    /// Publishes one Learn window's integrated reading and gated block count.
    pub(crate) fn publish_learn(&self, integrated_lufs: f32, blocks: u32) {
        self.learn_integrated
            .store(integrated_lufs.to_bits(), Ordering::Relaxed);
        self.learn_blocks.store(blocks, Ordering::Relaxed);
    }

    /// The Learn reading while it is running.
    pub(crate) fn learn_reading(&self) -> Option<(f32, u32)> {
        self.learn_active().then(|| {
            (
                f32::from_bits(self.learn_integrated.load(Ordering::Relaxed)),
                self.learn_blocks.load(Ordering::Relaxed),
            )
        })
    }

    /// Copies the settings (not the meters) of a previous run.
    pub(crate) fn adopt_settings_from(&self, previous: &Self) {
        let copy_bool = |to: &AtomicBool, from: &AtomicBool| {
            to.store(from.load(Ordering::Relaxed), Ordering::Relaxed);
        };
        for (to, from) in self.maps.iter().zip(&previous.maps) {
            to.store(from.load());
            to.gain
                .store(from.gain.load(Ordering::Relaxed), Ordering::Relaxed);
            to.trim_ms
                .store(from.trim_ms.load(Ordering::Relaxed), Ordering::Relaxed);
        }
        copy_bool(&self.silent, &previous.silent);
        copy_bool(&self.align, &previous.align);
        copy_bool(&self.mute, &previous.mute);
        self.gain
            .store(previous.gain.load(Ordering::Relaxed), Ordering::Relaxed);
        self.click_mask.store(
            previous.click_mask.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.loudness_target.store(
            previous.loudness_target.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.loudness_learned.store(
            previous.loudness_learned.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
    }

    /// Clears the meters and the channel count for a fresh run.
    pub(crate) fn reset(&self) {
        for meter in &self.meters {
            meter.store(0.0f32.to_bits(), Ordering::Relaxed);
        }
        self.reduction_db.store(0.0f32.to_bits(), Ordering::Relaxed);
        self.loudness_trim
            .store(0.0f32.to_bits(), Ordering::Relaxed);
        self.loudness_momentary
            .store(super::super::SILENCE_LUFS.to_bits(), Ordering::Relaxed);
        self.loudness_short_term
            .store(super::super::SILENCE_LUFS.to_bits(), Ordering::Relaxed);
        self.loudness_integrated
            .store(super::super::SILENCE_LUFS.to_bits(), Ordering::Relaxed);
        self.loudness_reset.store(false, Ordering::Relaxed);
        self.learn_active.store(false, Ordering::Relaxed);
        self.learn_generation.fetch_add(1, Ordering::Relaxed);
        self.learn_integrated
            .store(super::super::SILENCE_LUFS.to_bits(), Ordering::Relaxed);
        self.learn_blocks.store(0, Ordering::Relaxed);
        self.channels.store(0, Ordering::Relaxed);
        self.applied_rate.store(0, Ordering::Relaxed);
    }

    /// Forgets the published alignment: no mixer maps this device now (the
    /// bridge stopped, or an extra output that is not fed).  Atomics only.
    pub(crate) fn clear_alignment(&self) {
        self.applied_rate.store(0, Ordering::Release);
    }

    /// The align switch as stored.
    pub(crate) fn align(&self) -> bool {
        self.align.load(Ordering::Relaxed)
    }

    /// The alignment the mapper works with right now (the delays its taps
    /// settle on), or `None` before a mixer published one in this run.
    pub(crate) fn alignment(&self) -> Option<AlignmentReport> {
        let rate = self.applied_rate.load(Ordering::Acquire);
        if rate == 0 {
            return None;
        }
        let mut plan = AlignmentPlan::default();
        for (delay, applied) in plan.delays.iter_mut().zip(&self.applied) {
            let value = applied.load(Ordering::Relaxed);
            *delay = (value != NOT_ROUTED).then_some(value);
        }
        let waits_for = self.waits_for.load(Ordering::Relaxed);
        plan.waits_for = (waits_for != NO_PATH).then_some(waits_for as usize);
        let latency =
            std::array::from_fn(|path| self.applied_latency[path].load(Ordering::Relaxed));
        Some(AlignmentReport::new(plan, self.align(), latency, rate))
    }

    /// Publishes the alignment the mapper works with (audio thread; atomics
    /// only).
    fn publish_alignment(&self, plan: &AlignmentPlan, latency: &[u32; MAX_PATHS], rate: u32) {
        for (applied, delay) in self.applied.iter().zip(plan.delays) {
            applied.store(delay.unwrap_or(NOT_ROUTED), Ordering::Relaxed);
        }
        for (applied, latency) in self.applied_latency.iter().zip(latency) {
            applied.store(*latency, Ordering::Relaxed);
        }
        self.waits_for.store(
            plan.waits_for.map_or(NO_PATH, |path| path as u8),
            Ordering::Relaxed,
        );
        self.applied_rate.store(rate.max(1), Ordering::Release);
    }

    pub(crate) fn meters(&self) -> [f32; MAX_OUTPUT_CHANNELS] {
        std::array::from_fn(|channel| f32::from_bits(self.meters[channel].load(Ordering::Relaxed)))
    }

    /// Safety-limiter gain reduction in dB.  0 when the device is not
    /// limiting.
    pub(crate) fn reduction_db(&self) -> f32 {
        f32::from_bits(self.reduction_db.load(Ordering::Relaxed))
    }

    fn store_reduction(&self, db: f32) {
        let db = if db.is_finite() { db.max(0.0) } else { 0.0 };
        self.reduction_db.store(db.to_bits(), Ordering::Relaxed);
    }

    /// The device's real channel count; 0 = unknown (bridge not running),
    /// so the UI falls back to the device inventory's channel count.  ASIO
    /// opens at most 4 outputs for now; the map sees that count.
    pub(crate) fn channels(&self) -> u32 {
        self.channels.load(Ordering::Relaxed)
    }

    /// Publishes a known channel count outside the callback (stream open or
    /// a live extra output add).
    pub(crate) fn set_channels(&self, channels: usize) {
        self.channels
            .store(channels.min(u32::MAX as usize) as u32, Ordering::Relaxed);
    }

    /// One path's channel map on this device right now.
    pub(crate) fn path_map(&self, path: usize) -> ChannelMap {
        self.maps[path].load()
    }

    /// One path's signed gain (level and polarity) on this device.
    #[cfg(test)]
    pub(crate) fn path_gain(&self, path: usize) -> f32 {
        self.maps[path].gain()
    }

    /// Path 0's map, the unused mode and the effective gain (0 when muted)
    /// right now.
    #[cfg(test)]
    pub(super) fn load(&self) -> (ChannelMap, UnusedChannels, f32) {
        let (unused, gain) = self.load_device();
        (self.path_map(0), unused, gain)
    }

    /// The click mask latched for a block.  Bit `n` is channel `n`.
    pub(crate) fn click_mask(&self) -> u8 {
        self.click_mask.load(Ordering::Relaxed)
    }

    /// The unused mode, the level gain and the mute gain (0 or 1) right now.
    /// Mute is its own ramp so the safety limiter can sit between the level
    /// and the fade to silence.
    fn load_level_and_mute(&self) -> (UnusedChannels, f32, f32) {
        let unused = if self.silent.load(Ordering::Relaxed) {
            UnusedChannels::Silent
        } else {
            UnusedChannels::MonoCopy
        };
        let level = f32::from_bits(self.gain.load(Ordering::Relaxed));
        let mute = if self.mute.load(Ordering::Relaxed) {
            0.0
        } else {
            1.0
        };
        (unused, level, mute)
    }

    /// The unused mode and the effective gain (0 when muted) right now.
    #[cfg(test)]
    fn load_device(&self) -> (UnusedChannels, f32) {
        let (unused, level, mute) = self.load_level_and_mute();
        (unused, if mute == 0.0 { 0.0 } else { level })
    }
}

/// Slot of the shared value every channel at or past
/// [`MAX_OUTPUT_CHANNELS`] gets: those channels are never reached, so they
/// all carry the same unused-channel value.
const EXTRA: usize = MAX_OUTPUT_CHANNELS;
/// Samples a [`DeviceFrame`] holds: one per mapped channel plus the shared
/// slot.  A generic extra output's ring carries this many lanes at most.
pub(crate) const SLOTS: usize = MAX_OUTPUT_CHANNELS + 1;

/// How many lanes carry every channel of a `channels`-channel device: one
/// per channel up to [`MAX_OUTPUT_CHANNELS`], then the shared slot.
pub(crate) fn frame_lanes(channels: usize) -> usize {
    channels.min(SLOTS)
}

/// One mapped device frame: what each device channel plays, and the same
/// frame without the metronome tick or the calibration click (what a take
/// records while "Record the click" is off).  Both share the level, the
/// music ceiling, the mute and the calibration duck.
pub(crate) struct DeviceFrame {
    samples: [f32; SLOTS],
    /// The click-free frame, lane for lane.  Equal to `samples` while no
    /// tick sounds and the click limiter is at unity.
    record: [f32; SLOTS],
    channels: usize,
}

impl DeviceFrame {
    /// The sample for device channel `channel` (0-based).  A channel past
    /// the device's own count reads silence.
    #[inline]
    pub(crate) fn sample(&self, channel: usize) -> f32 {
        if channel >= self.channels {
            0.0
        } else {
            self.samples[channel.min(EXTRA)]
        }
    }

    /// [`Self::sample`] of the click-free frame: no metronome tick and no
    /// calibration click.
    #[inline]
    pub(crate) fn record_sample(&self, channel: usize) -> f32 {
        if channel >= self.channels {
            0.0
        } else {
            self.record[channel.min(EXTRA)]
        }
    }

    /// This frame with the calibration duck applied and the click added to
    /// `mapped` (bit `n` = device channel `n`) after the scaling, so the
    /// measurement keeps its fixed level regardless of the device fader and
    /// leaves on the channels the clock map really plays.  An empty mask
    /// falls back to channel 0 (and 1 when the device has it).  The
    /// click-free frame gets the duck but never the click.
    #[inline]
    pub(crate) fn ducked(&self, gain: f32, click: f32, mapped: u32) -> Self {
        let mut frame = Self {
            samples: self.samples,
            record: self.record,
            channels: self.channels,
        };
        if gain == 1.0 && click == 0.0 {
            // Unity must not touch a sample: `x * 1.0 + 0.0` would turn a
            // negative zero into a positive one and break bit identity.
            return frame;
        }
        let mask = if mapped == 0 { 0b11 } else { mapped };
        let lanes = frame_lanes(self.channels);
        for (channel, sample) in frame.samples[..lanes].iter_mut().enumerate() {
            *sample *= gain;
            if channel < MAX_OUTPUT_CHANNELS && mask & (1 << channel) != 0 {
                *sample += click;
            }
        }
        if gain != 1.0 {
            for sample in &mut frame.record[..lanes] {
                *sample *= gain;
            }
        }
        frame
    }

    /// The frame as lanes, [`frame_lanes`] of them for its channel count;
    /// lane `n` is channel `n`, the last lane of a device with more than
    /// [`MAX_OUTPUT_CHANNELS`] channels is the shared slot.
    #[inline]
    pub(crate) fn lanes(&self) -> &[f32] {
        &self.samples[..frame_lanes(self.channels)]
    }

    /// Rebuilds a frame for a `channels`-channel device from its lanes (see
    /// [`Self::lanes`]).  Missing lanes read silence.  The click-free frame
    /// is the same lanes, so whatever click they hold stays in a take.
    #[inline]
    pub(crate) fn from_lanes(lanes: &[f32], channels: usize) -> Self {
        Self::from_both_lanes(lanes, lanes, channels)
    }

    /// The click-free frame as lanes, laid out like [`Self::lanes`].
    #[inline]
    pub(crate) fn record_lanes(&self) -> &[f32] {
        &self.record[..frame_lanes(self.channels)]
    }

    /// Rebuilds a frame from its lanes and its click-free lanes (see
    /// [`Self::lanes`] and [`Self::record_lanes`]).  Missing lanes read
    /// silence.
    #[inline]
    pub(crate) fn from_both_lanes(lanes: &[f32], record: &[f32], channels: usize) -> Self {
        let count = frame_lanes(channels);
        let mut samples = [0.0; SLOTS];
        let wet = count.min(lanes.len());
        samples[..wet].copy_from_slice(&lanes[..wet]);
        let mut clean = [0.0; SLOTS];
        let dry = count.min(record.len());
        clean[..dry].copy_from_slice(&record[..dry]);
        Self {
            samples,
            record: clean,
            channels,
        }
    }
}

/// Applies a device's channel map, level and mute to the finished stereo
/// mix, one frame at a time, for whatever buffer layout the backend has.
/// Sized for its channel count when the stream opens; fixed arrays, so the
/// callback never allocates.
///
/// Per block: [`Self::begin_paths`] reads the controls once and points the
/// ramps at them, [`Self::map_paths`] runs per frame, [`Self::end_block`] publishes
/// the meters.  Every weight, each path's gain on the device and the device
/// gain move over the same ramp as the mixer's routes, so a map, level,
/// polarity or mute change crossfades.  Settled
/// weights are exactly 0 or 1 and the gain exactly its target, so the
/// default settings write the very samples the output always wrote, while
/// the sample stays under the safety ceiling.  The ceiling runs after the
/// level and before the mute ramp.  The metronome is added after that
/// ceiling and before the mute, on the click mask, and a zero sample
/// does not write a lane.  The lanes it touched then pass through their
/// own limiter, so the sum is gained down instead of flat-topped, and a
/// channel the click missed is left alone.  That limiter keeps running on
/// the masked lanes between ticks, so its release after a click is a ramp,
/// not a step back to unity; once it is back at unity a zero tick skips
/// it.  A mask change while it still reduces keeps it on the old lanes
/// too until a block starts with it back at unity, so no lane steps out
/// of its gain.  A lane the tick just moved to takes the gain at once, and
/// a dropped lane stays ducked by later ticks until the limiter gets back
/// to unity between them.  [`Self::map_paths`] delays the tick by the
/// alignment delay a dry, untrimmed path gets on the device first, and the
/// calibration duck ([`Self::duck_click`]) acts after that delay, on ticks already queued.
/// The frame's click-free lanes are copied after the music ceiling, before
/// the tick and its limiter, and get the same mute.
pub(crate) struct DeviceMapper {
    channels: usize,
    /// Weights of `[L, R, (L + R) / 2]` per path slot and channel slot.
    /// Only the live paths' ramps move.
    weights: [[[Ramp; 3]; SLOTS]; MAX_PATHS],
    /// Each path's signed gain on this device (level and polarity).  A
    /// polarity flip ramps through zero.
    path_gain: [Ramp; MAX_PATHS],
    pub(super) gain: Ramp,
    /// Mute, ramped to silence on its own so the limiter sees the levelled
    /// signal.  Unity copies.
    mute_gain: Ramp,
    /// Sample-peak ceiling after the level.  Preallocated; no per-block alloc.
    limiter: SafetyLimiter,
    /// Ceiling for the lanes the click was added to.  Separate from
    /// [`Self::limiter`] so a hot tick does not duck a channel it is not on.
    click_limiter: SafetyLimiter,
    adopted: bool,
    /// The paths mapped in this block, and in the one before (a path that
    /// just went live adopts its weights instead of ramping from stale ones).
    live: LiveSlots,
    /// One read tap per path on that path's delay line: the path's
    /// alignment delay on this device.  A delay change crossfades.
    taps: [DelayTap; MAX_PATHS],
    /// The paths this device played in the last block.  A path that starts
    /// playing here snaps its tap (its weights fade in from 0 anyway).
    routed: [bool; MAX_PATHS],
    /// Device channels any live path's map reaches (bit `n` = channel `n`),
    /// read with the block's controls.  The calibration click goes there.
    mapped: u32,
    /// Click mask for this block (bit `n` = channel `n`).  Not the path map:
    /// 0 adds the tick nowhere.
    click_mask: u8,
    /// Lanes the click limiter runs on: [`Self::click_mask`], plus the
    /// lanes of an earlier mask while the limiter has not released to
    /// unity since that mask changed.  Collapses to the mask at the first
    /// block that starts with the limiter at unity.
    click_lanes: u8,
    /// The metronome samples this mapper was handed, newest last, so the
    /// tick can wait the device's alignment delay like the music does.
    /// Preallocated for [`MAX_ALIGN_DELAY`] samples.
    click_line: MonoDelayLine,
    /// Reads [`Self::click_line`] at the delay a dry, untrimmed path gets
    /// here.  A change crossfades like a path's tap while ticks are
    /// queued, and moves at once while the line is silent.
    click_tap: DelayTap,
    /// Zero ticks pushed since the last non-zero one.  Once that is past
    /// everything the tap reads, the line is reset and no longer written
    /// or read until a tick arrives.
    click_quiet: usize,
    /// Whether [`Self::map_paths`] ran since the last block began, or that
    /// block had no frames.  A block with frames that did not map this
    /// device left its click line behind, so the next block resets it
    /// instead of playing those ticks late.
    click_mapped: bool,
    /// The calibration duck on the tick, after the click line.  0 while a
    /// measurement ducks the output, so ticks already queued are silenced
    /// with the ones not yet generated.
    click_duck: Ramp,
    /// The learned output trim, applied before the device level fader and
    /// the ceiling.  Unity copies.
    trim: Ramp,
    trim_db: f32,
    /// The published loudness, after the fader, ceiling and mute.
    loudness: LoudnessMeter,
    /// Output Learn: the device sum after the learned trim and before the
    /// fader, ceiling and mute.
    learn_meter: LoudnessMeter,
    rate: u32,
    block_frames: usize,
    /// [`DeviceControls::device_generation`] as last seen.
    device_generation: u64,
    /// [`DeviceControls::learn_generation`] as last seen.
    learn_generation: u64,
    learn_active: bool,
    frame: DeviceFrame,
    peaks: [f32; MAX_OUTPUT_CHANNELS],
}

impl DeviceMapper {
    pub(crate) fn new(channels: usize) -> Self {
        Self {
            channels,
            weights: [[[Ramp::new(0.0); 3]; SLOTS]; MAX_PATHS],
            path_gain: [Ramp::new(1.0); MAX_PATHS],
            gain: Ramp::new(1.0),
            mute_gain: Ramp::new(1.0),
            limiter: SafetyLimiter::new(48_000),
            click_limiter: SafetyLimiter::new(48_000),
            adopted: false,
            live: LiveSlots::default(),
            taps: [DelayTap::default(); MAX_PATHS],
            routed: [false; MAX_PATHS],
            // The default map reaches both channels.
            mapped: 0b11,
            click_mask: 0,
            click_lanes: 0,
            click_line: MonoDelayLine::new(MAX_ALIGN_DELAY as usize),
            click_tap: DelayTap::default(),
            click_quiet: usize::MAX,
            click_mapped: false,
            click_duck: Ramp::new(1.0),
            trim: Ramp::new(1.0),
            trim_db: 0.0,
            loudness: LoudnessMeter::new(48_000),
            learn_meter: LoudnessMeter::new(48_000),
            rate: 48_000,
            block_frames: 0,
            device_generation: 0,
            learn_generation: 0,
            learn_active: false,
            frame: DeviceFrame {
                samples: [0.0; SLOTS],
                record: [0.0; SLOTS],
                channels,
            },
            peaks: [0.0; MAX_OUTPUT_CHANNELS],
        }
    }

    /// Follows a device whose channel count changed (a generic extra output's
    /// stream reopened on another device).  Fixed storage, so this is safe
    /// in the callback; the next block adopts the controls outright.
    pub(crate) fn set_channels(&mut self, channels: usize) {
        if channels != self.channels {
            self.channels = channels;
            self.frame.channels = channels;
            self.adopted = false;
            self.loudness.configure(self.rate, channels);
            self.loudness.reset();
            self.learn_meter.configure(self.rate, channels);
            self.learn_meter.reset();
        }
    }

    /// Channel slots this device uses: one per channel up to the maximum,
    /// plus the shared slot when it has more.
    fn slots(&self) -> std::iter::Chain<std::ops::Range<usize>, std::option::IntoIter<usize>> {
        let mapped = self.channels.min(MAX_OUTPUT_CHANNELS);
        let extra = (self.channels > MAX_OUTPUT_CHANNELS).then_some(EXTRA);
        (0..mapped).chain(extra)
    }

    /// Path 0 alone, as every caller before paths existed.
    #[cfg(test)]
    pub(crate) fn begin_block(&mut self, controls: &DeviceControls, ramp_samples: f32) {
        self.begin_paths(
            controls,
            ramp_samples,
            LiveSlots::main(),
            &PathTiming::default(),
            1,
        );
    }

    /// Reads the controls for this block, for the `live` path slots.  The
    /// first block (and a path that just went live) adopts them directly;
    /// later changes take `ramp_samples` samples.  Also works out each
    /// path's alignment delay on this device from `timing`, points the taps
    /// at it (see [`plan_alignment`]) and publishes it for the UI.
    /// `frames` is the block's length: a block of 0 frames maps nothing and
    /// keeps the queued ticks.
    pub(crate) fn begin_paths(
        &mut self,
        controls: &DeviceControls,
        ramp_samples: f32,
        live: LiveSlots,
        timing: &PathTiming,
        frames: usize,
    ) {
        let (unused, level, mute) = controls.load_level_and_mute();
        self.click_mask = controls.click_mask();
        self.click_lanes = if self.click_limiter.is_limiting() {
            self.click_lanes | self.click_mask
        } else {
            self.click_mask
        };
        if !self.click_mapped {
            // Not mapped last block (an extra output whose ring was not fed, or
            // a mapper that just went live): what the line holds is stale.
            self.silence_click_line();
        }
        // An empty block has nothing to map, so it does not count as a gap.
        self.click_mapped = frames == 0;
        self.limiter.set_rate(timing.rate);
        self.click_limiter.set_rate(timing.rate);
        self.rate = timing.rate.max(1);
        self.block_frames = frames;
        self.loudness.configure(self.rate, self.channels);
        self.learn_meter.configure(self.rate, self.channels);
        let generation = controls.device_generation();
        if generation != self.device_generation {
            self.device_generation = generation;
            self.reset_loudness();
        }
        if controls.take_loudness_reset() {
            self.reset_loudness();
        }
        let learn_generation = controls.learn_generation();
        if learn_generation != self.learn_generation {
            self.learn_generation = learn_generation;
            self.learn_meter.reset();
        }
        self.learn_active = controls.learn_active();
        let loudness = controls.loudness_settings();
        self.trim_db = loudness.trim_db;
        let learned_target = 10.0f32.powf(loudness.trim_db / 20.0);
        if self.adopted {
            self.trim
                .set_target(learned_target, TRIM_RAMP_SECONDS * self.rate as f32);
        } else {
            self.trim.snap(learned_target);
        }
        let maps: [ChannelMap; MAX_PATHS] = std::array::from_fn(|path| controls.path_map(path));
        let mask = live.mask();
        let mut mapped = 0u32;
        for path in live.iter() {
            for channel in 0..self.channels.min(MAX_OUTPUT_CHANNELS) {
                let (left, right) = maps[path].sides_at(self.channels, channel);
                if left || right {
                    mapped |= 1 << channel;
                }
            }
        }
        // A device with no live path routed anywhere still gets a click on
        // its first channel (or two) rather than none at all.
        self.mapped = if mapped == 0 { 0b11 } else { mapped };
        self.align_paths(controls, &maps, mask, unused, timing);
        for path in live.iter() {
            let ramp = self.adopted && self.live.contains(path);
            for slot in self.slots() {
                // The shared slot is channel MAX_OUTPUT_CHANNELS, which no
                // map reaches.
                let targets = path_channel_weights(&maps, mask, unused, self.channels, slot, path);
                for (weight, target) in self.weights[path][slot].iter_mut().zip(targets) {
                    if ramp {
                        weight.set_target(target, ramp_samples);
                    } else {
                        weight.snap(target);
                    }
                }
            }
            let path_gain = controls.maps[path].gain();
            if ramp {
                self.path_gain[path].set_target(path_gain, ramp_samples);
            } else {
                self.path_gain[path].snap(path_gain);
            }
        }
        self.live = live;
        if self.adopted {
            self.gain.set_target(level, ramp_samples);
            self.mute_gain.set_target(mute, ramp_samples);
        } else {
            self.gain.snap(level);
            self.mute_gain.snap(mute);
        }
        self.adopted = true;
        self.peaks = [0.0; MAX_OUTPUT_CHANNELS];
    }

    /// Points the calibration duck on the tick at silence while `ducking`,
    /// and back at unity after; the move takes `ramp_samples`.  It applies
    /// after the click line, so a tick queued before the measurement does
    /// not play into it.  While the line is silent nothing is sounding or
    /// queued here, so the duck moves at once: a tick that starts in this
    /// block is already ducked.  Call it once per block, after
    /// [`Self::begin_paths`].
    pub(crate) fn duck_click(&mut self, ducking: bool, ramp_samples: f32) {
        let target = if ducking { 0.0 } else { 1.0 };
        if self.click_line_silent() {
            self.click_duck.snap(target);
        } else {
            self.click_duck.set_target(target, ramp_samples);
        }
    }

    /// Forgets every queued tick.  O(1); the line then reads silence and is
    /// skipped until the next non-zero tick.  The mixer calls it for a
    /// extra output slot that is not live, so one that comes back does not
    /// replay what it held.
    pub(crate) fn silence_click_line(&mut self) {
        self.click_line.reset();
        self.click_quiet = usize::MAX;
    }

    /// Whether every position the click tap reads (or will read) holds a
    /// zero: the last non-zero tick is older than the longest delay.
    pub(crate) fn click_line_silent(&self) -> bool {
        self.click_quiet > self.click_tap.longest() as usize
    }

    /// The tick this frame, after the device's alignment delay and the
    /// calibration duck.  While the line holds only zeros, a zero tick is
    /// neither written nor read, and the tap moves straight to its target:
    /// there is nothing to crossfade, and a fade left pending would read
    /// the next tick at two positions.
    #[inline]
    fn delayed_click(&mut self, click: f32) -> f32 {
        let duck = self.click_duck.next();
        if self.click_line_silent() {
            if !self.click_tap.is_settled() {
                self.click_tap.snap(self.click_tap.target());
            }
            if click == 0.0 {
                return 0.0;
            }
        }
        if click == 0.0 {
            self.click_quiet = self.click_quiet.saturating_add(1);
        } else {
            self.click_quiet = 0;
        }
        self.click_line.push(click);
        let delayed = self.click_tap.next_mono(&self.click_line, 0);
        if self.click_line_silent() {
            // The last tick has come out of every read position.  Anything
            // older reads silence after this too, so a longer delay later
            // cannot replay it.
            self.silence_click_line();
        }
        if delayed == 0.0 || duck == 1.0 {
            delayed
        } else {
            delayed * duck
        }
    }

    /// Points each path's tap at its alignment delay on this device.  A
    /// path the device keeps playing crossfades to a new delay; one that
    /// starts playing here (and every path in the first block) snaps.
    fn align_paths(
        &mut self,
        controls: &DeviceControls,
        maps: &[ChannelMap; MAX_PATHS],
        mask: [bool; MAX_PATHS],
        unused: UnusedChannels,
        timing: &PathTiming,
    ) {
        let routed = routed_paths(maps, mask, unused, self.channels);
        let trims: [i32; MAX_PATHS] =
            std::array::from_fn(|path| trim_frames(controls.maps[path].trim_ms(), timing.rate));
        let plan = plan_alignment(controls.align(), routed, timing.latency, trims);
        let fade = TAP_FADE_SECONDS * timing.rate as f32;
        for (path, delay) in plan.delays.iter().enumerate() {
            let Some(delay) = *delay else {
                continue;
            };
            if self.adopted && self.routed[path] {
                self.taps[path].set_target(delay, fade);
            } else {
                self.taps[path].snap(delay);
            }
        }
        // The tick has no chain latency and no trim, so it waits as long as
        // a dry, untrimmed path would here: it lands with the aligned music,
        // and trimming one path does not move it.
        let click_delay = plan.dry;
        if self.adopted {
            self.click_tap.set_target(click_delay, fade);
        } else {
            self.click_tap.snap(click_delay);
        }
        self.routed = routed;
        controls.publish_alignment(&plan, &timing.latency, timing.rate);
    }

    /// Path 0 alone: maps one stereo frame with path 0's map.
    #[cfg(test)]
    pub(crate) fn map(&mut self, left: f32, right: f32) -> &DeviceFrame {
        let mut frames = [[0.0; 2]; MAX_PATHS];
        frames[0] = [left, right];
        self.map_frames(&frames, 0.0)
    }

    /// Maps the current frame of every live path onto the device's
    /// channels: each path is read from its delay line through this
    /// device's tap (its alignment delay here), then mapped and summed as
    /// in [`Self::map_frames`].  A settled tap at delay 0 reads the frame
    /// just written, so an unaligned path is exactly its own frame.  The
    /// metronome sample `click` waits the delay a dry, untrimmed path gets
    /// on this device, so it lines up with the aligned music.  Call it
    /// once per frame: each call writes the click line.
    #[inline]
    pub(crate) fn map_paths(&mut self, lines: &PathLines, click: f32) -> &DeviceFrame {
        let mut frames = [[0.0f32; 2]; MAX_PATHS];
        for path in self.live.iter() {
            frames[path] = self.taps[path].next(&lines[path], 0);
        }
        self.click_mapped = true;
        let click = self.delayed_click(click);
        self.map_frames(&frames, click)
    }

    /// Maps one frame of every live path (each after the master and its
    /// alignment) onto the device's channels, scaled by the path's level
    /// and polarity on this device, and sums them per channel: this is the
    /// sum of the paths.  Slots that are not live are ignored.  One live
    /// path at 0 dB and normal polarity gives exactly its own mapped frame.
    /// `click` is the metronome sample.  It is added after the ceiling and
    /// before the mute, and only when it is not exactly 0.  The masked
    /// lanes pass the click limiter on every frame while it still reduces,
    /// tick or not, so a reduction a tick left behind releases smoothly.
    /// A zero tick with that limiter at unity skips it: it would only copy.
    #[inline]
    pub(crate) fn map_frames(
        &mut self,
        frames: &[[f32; 2]; MAX_PATHS],
        click: f32,
    ) -> &DeviceFrame {
        let mut sources = [[0.0f32; 3]; MAX_PATHS];
        for path in self.live.iter() {
            let [left, right] = frames[path];
            let mut source = [left, right, (left + right) * 0.5];
            let path_gain = self.path_gain[path].next();
            // Unity copies, so 0 dB never touches a sample.
            if path_gain != 1.0 {
                for value in &mut source {
                    *value *= path_gain;
                }
            }
            sources[path] = source;
        }
        let gain = self.gain.next();
        let trim = self.trim.next();
        // Unity copies, so 0 dB and a settled trim never touch a sample in
        // any FP mode.  The learned trim sits before the fader; Learn and
        // Match measure here, so moving the fader never changes them.
        let learned = |value: f32| if trim == 1.0 { value } else { value * trim };
        let level = |value: f32| {
            let value = learned(value);
            if gain == 1.0 {
                value
            } else {
                value * gain
            }
        };
        let mapped = self.channels.min(MAX_OUTPUT_CHANNELS);
        let mut learn_frame = [0.0f32; MAX_OUTPUT_CHANNELS];
        for (channel, learn_slot) in learn_frame.iter_mut().take(mapped).enumerate() {
            let sum = sum_paths(&mut self.weights, self.live, channel, &sources);
            self.frame.samples[channel] = level(sum);
            *learn_slot = learned(sum);
        }
        if self.channels > MAX_OUTPUT_CHANNELS {
            let sum = sum_paths(&mut self.weights, self.live, EXTRA, &sources);
            self.frame.samples[EXTRA] = level(sum);
        }
        // Level, then the safety ceiling, then the metronome, then mute.
        // Mute already ramps to silence, so the music limiter sees the
        // music the fader would play.  The tick is not part of that gain:
        // a hot tick on one lane must not duck the others.  It is added
        // after the ceiling, and only those lanes pass through a second
        // limiter, so music that shares a lane is gained down instead of
        // flattened and the reduction is published.  A zero sample is not
        // added (`x + 0.0` would clear a negative zero).  The masked lanes
        // go through that limiter even without a tick while it is still
        // releasing: skipping it then would put the music back at unity in
        // one sample.  Released, with no tick, it would only copy (the music
        // limiter already holds those lanes under the same ceiling), so the
        // gather is skipped.  Lanes a mask change just dropped stay in it
        // until it is back at unity; the tick goes on the mask only.  The
        // click-free lanes are copied before the tick and get only the mute.
        let lanes = frame_lanes(self.channels);
        self.limiter.process(&mut self.frame.samples[..lanes]);
        self.frame.record[..lanes].copy_from_slice(&self.frame.samples[..lanes]);
        if click != 0.0 || self.click_limiter.is_limiting() {
            let mut gathered = [0.0f32; MAX_OUTPUT_CHANNELS];
            let mut which = [0usize; MAX_OUTPUT_CHANNELS];
            let mut count = 0usize;
            let mask = self.click_mask;
            let limited = self.click_lanes;
            let clicked = self.channels.min(MAX_OUTPUT_CHANNELS);
            for channel in 0..clicked {
                if (limited & (1u8 << channel)) == 0 {
                    continue;
                }
                which[count] = channel;
                gathered[count] = if click == 0.0 || (mask & (1u8 << channel)) == 0 {
                    self.frame.samples[channel]
                } else {
                    self.frame.samples[channel] + click
                };
                count += 1;
            }
            self.click_limiter.process(&mut gathered[..count]);
            for slot in 0..count {
                self.frame.samples[which[slot]] = gathered[slot];
            }
        }
        let mute = self.mute_gain.next();
        if mute != 1.0 {
            for sample in &mut self.frame.samples[..lanes] {
                *sample *= mute;
            }
            for sample in &mut self.frame.record[..lanes] {
                *sample *= mute;
            }
        }
        // Learn sees the whole device after the learned trim and before the
        // fader, ceiling and mute.  It is not the stream's final loudness.
        if self.learn_active {
            self.learn_meter.push(&learn_frame[..mapped]);
        }
        // Loudness sees the click-free lanes after their mute, so the
        // metronome never moves the reading.  A device with more than
        // MAX_OUTPUT_CHANNELS channels keeps one shared lane past the
        // mapped ones; the meter counts the real channels only.
        self.loudness.push(&self.frame.record[..mapped]);
        for channel in 0..mapped {
            let value = self.frame.samples[channel].abs();
            self.peaks[channel] = self.peaks[channel].max(value);
        }
        &self.frame
    }

    /// Publishes the device's channel count.  Called when the stream opens,
    /// outside the callback, so the snapshot has it before the first block.
    pub(crate) fn publish_channels(&self, controls: &DeviceControls) {
        controls.set_channels(self.channels);
    }

    /// The longest alignment delay path `path` is read at on this device:
    /// what a removed path still has to play here after its fade.
    pub(crate) fn tap_delay(&self, path: usize) -> u32 {
        self.taps[path].longest()
    }

    /// Device channels any live path's map reaches this block (bit `n` =
    /// channel `n`), or the 0/1 fallback when nothing is routed.
    pub(crate) fn mapped_channels(&self) -> u32 {
        self.mapped
    }

    /// Clears the published meter and the Learn window.  The learned trim is
    /// a setting and is not touched.
    fn reset_loudness(&mut self) {
        self.loudness.reset();
        self.learn_meter.reset();
    }

    /// Publishes this block's per-channel peaks, loudness and any Learn
    /// reading.  Nothing moves on its own; the learned trim only ramps
    /// when its setting changes.
    pub(crate) fn end_block(&mut self, controls: &DeviceControls) {
        if controls.take_loudness_reset() {
            self.reset_loudness();
        }
        for (meter, peak) in controls.meters.iter().zip(self.peaks) {
            meter.store(peak.to_bits(), Ordering::Relaxed);
        }
        let music = self.limiter.reduction_db();
        let tick = self.click_limiter.reduction_db();
        controls.store_reduction(music.max(tick));

        if self.learn_active {
            let (integrated, blocks) = self.learn_meter.integrated_with_blocks();
            controls.publish_learn(integrated, blocks as u32);
        } else {
            controls.publish_learn(super::super::SILENCE_LUFS, 0);
        }
        controls.publish_loudness(self.loudness.reading(self.trim_db));
    }
}

/// One channel slot summed over the live paths.  The sum starts from the
/// first path's value itself, so one path passes bit for bit; no live path
/// is silence (+0).
#[inline]
fn sum_paths(
    weights: &mut [[[Ramp; 3]; SLOTS]; MAX_PATHS],
    live: LiveSlots,
    slot: usize,
    sources: &[[f32; 3]; MAX_PATHS],
) -> f32 {
    let mut value: Option<f32> = None;
    for path in live.iter() {
        let term = blend(&mut weights[path][slot], sources[path]);
        value = Some(value.map_or(term, |sum| sum + term));
    }
    value.unwrap_or(0.0)
}

/// Advances one slot's three weight ramps and mixes the sources with them.
/// Zero weights are skipped and a weight of 1 copies, so a settled slot
/// returns its one source bit for bit (and silence as +0).
#[inline]
fn blend(weights: &mut [Ramp; 3], sources: [f32; 3]) -> f32 {
    let mut value: Option<f32> = None;
    for (ramp, source) in weights.iter_mut().zip(sources) {
        let weight = ramp.next();
        if weight != 0.0 {
            let term = if weight == 1.0 {
                source
            } else {
                weight * source
            };
            value = Some(value.map_or(term, |sum| sum + term));
        }
    }
    value.unwrap_or(0.0)
}

#[cfg(test)]
/// The output's write rule before device maps existed (cpal
/// `write_frame`): L and R on channels 1 and 2, their average on the
/// rest and on a mono device.  The default map must match it bit for bit.
pub(crate) fn old_write_frame(frame: &mut [f32], left: f32, right: f32) {
    let rest = (left + right) * 0.5;
    match frame.len() {
        0 => {}
        1 => frame[0] = rest,
        _ => {
            frame[0] = left;
            frame[1] = right;
            for sample in frame.iter_mut().skip(2) {
                *sample = rest;
            }
        }
    }
}
