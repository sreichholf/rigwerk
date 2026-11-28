//! Device maps on the callback.  No allocation.  The tick is added after
//! the safety ceiling; the click-free lanes are copied before it.

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

/// The UI stores; the callback loads once per block.
pub(crate) struct DeviceControls {
    maps: [PathMap; MAX_PATHS],
    silent: AtomicBool,
    gain: AtomicU32,
    mute: AtomicBool,
    /// Which channels carry the metronome.  Bit `n` is channel `n`.
    /// 0 adds the tick nowhere.
    click_mask: AtomicU8,
    /// After the tick and mute.  A channel the device lacks reads 0.
    meters: [AtomicU32; MAX_OUTPUT_CHANNELS],
    /// Safety-limiter gain reduction, dB (`f32` bits).  0 when the device
    /// is not limiting.  Published with the meters.
    reduction_db: AtomicU32,
    loudness_target: AtomicU32,
    loudness_learned: AtomicU32,
    loudness_trim: AtomicU32,
    loudness_momentary: AtomicU32,
    loudness_short_term: AtomicU32,
    loudness_integrated: AtomicU32,
    /// Set by the UI to clear the meter's windows and integrated history.
    loudness_reset: AtomicBool,
    /// Bumped when a different device moves into this output slot, so the
    /// mapper drops the old device's trim and history.
    device_generation: AtomicU64,
    /// One Learn.  `learn_generation` bumps on every start and stop.
    learn_active: AtomicBool,
    learn_generation: AtomicU64,
    learn_integrated: AtomicU32,
    learn_blocks: AtomicU32,
    /// 0 means unknown, so the UI uses the inventory count.
    channels: AtomicU32,
    align: AtomicBool,
    /// [`NOT_ROUTED`] when not played, [`NO_PATH`] when it waits for
    /// none.  Rate 0 means nothing has published this run.
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

    fn set_loudness(&self, settings: &LoudnessSettings) {
        let settings = settings.sanitized();
        self.loudness_target
            .store(settings.target_lufs.to_bits(), Ordering::Relaxed);
        self.loudness_learned
            .store(settings.trim_db.to_bits(), Ordering::Relaxed);
    }

    pub(crate) fn loudness_settings(&self) -> LoudnessSettings {
        LoudnessSettings {
            target_lufs: f32::from_bits(self.loudness_target.load(Ordering::Relaxed)),
            trim_db: f32::from_bits(self.loudness_learned.load(Ordering::Relaxed)),
        }
    }

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

    pub(crate) fn loudness(&self) -> LoudnessReading {
        LoudnessReading {
            momentary_lufs: f32::from_bits(self.loudness_momentary.load(Ordering::Relaxed)),
            short_term_lufs: f32::from_bits(self.loudness_short_term.load(Ordering::Relaxed)),
            integrated_lufs: f32::from_bits(self.loudness_integrated.load(Ordering::Relaxed)),
            trim_db: f32::from_bits(self.loudness_trim.load(Ordering::Relaxed)),
        }
    }

    pub(crate) fn request_loudness_reset(&self) {
        self.loudness_reset.store(true, Ordering::Relaxed);
    }

    pub(crate) fn take_loudness_reset(&self) -> bool {
        self.loudness_reset.swap(false, Ordering::Relaxed)
    }

    pub(crate) fn device_generation(&self) -> u64 {
        self.device_generation.load(Ordering::Relaxed)
    }

    pub(crate) fn note_new_device(&self) {
        self.device_generation.fetch_add(1, Ordering::Relaxed);
        self.stop_learn();
    }

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

    pub(crate) fn learn_active(&self) -> bool {
        self.learn_active.load(Ordering::Relaxed)
    }

    pub(crate) fn learn_generation(&self) -> u64 {
        self.learn_generation.load(Ordering::Relaxed)
    }

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

    pub(crate) fn reduction_db(&self) -> f32 {
        f32::from_bits(self.reduction_db.load(Ordering::Relaxed))
    }

    fn store_reduction(&self, db: f32) {
        let db = if db.is_finite() { db.max(0.0) } else { 0.0 };
        self.reduction_db.store(db.to_bits(), Ordering::Relaxed);
    }

    pub(crate) fn channels(&self) -> u32 {
        self.channels.load(Ordering::Relaxed)
    }

    /// Publishes a known channel count outside the callback (stream open or
    /// a live extra output add).
    pub(crate) fn set_channels(&self, channels: usize) {
        self.channels
            .store(channels.min(u32::MAX as usize) as u32, Ordering::Relaxed);
    }

    pub(crate) fn path_map(&self, path: usize) -> ChannelMap {
        self.maps[path].load()
    }

    #[cfg(test)]
    pub(crate) fn path_gain(&self, path: usize) -> f32 {
        self.maps[path].gain()
    }

    #[cfg(test)]
    pub(super) fn load(&self) -> (ChannelMap, UnusedChannels, f32) {
        let (unused, gain) = self.load_device();
        (self.path_map(0), unused, gain)
    }

    pub(crate) fn click_mask(&self) -> u8 {
        self.click_mask.load(Ordering::Relaxed)
    }

    /// Mute is its own ramp so the limiter sits between level and fade.
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

    #[cfg(test)]
    fn load_device(&self) -> (UnusedChannels, f32) {
        let (unused, level, mute) = self.load_level_and_mute();
        (unused, if mute == 0.0 { 0.0 } else { level })
    }
}

/// Channels past [`MAX_OUTPUT_CHANNELS`] share one slot.  A map never
/// reaches them.
const EXTRA: usize = MAX_OUTPUT_CHANNELS;
/// Samples a [`DeviceFrame`] holds: one per mapped channel plus the shared
/// slot.  A generic extra output's ring carries this many lanes at most.
pub(crate) const SLOTS: usize = MAX_OUTPUT_CHANNELS + 1;

/// How many lanes carry every channel of a `channels`-channel device: one
/// per channel up to [`MAX_OUTPUT_CHANNELS`], then the shared slot.
pub(crate) fn frame_lanes(channels: usize) -> usize {
    channels.min(SLOTS)
}

/// Played lanes, and click-free lanes with no tick and no calibration
/// click.  Both share the level, the music ceiling, the mute and the
/// duck.
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

    #[inline]
    pub(crate) fn record_sample(&self, channel: usize) -> f32 {
        if channel >= self.channels {
            0.0
        } else {
            self.record[channel.min(EXTRA)]
        }
    }

    /// Duck, then add the click on `mapped`, so its level ignores the
    /// fader.  An empty mask falls back to channels 0 and 1.  The
    /// click-free frame is ducked and never gets the click.  Unity is
    /// returned untouched: `x * 1.0` would clear a negative zero.
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

    /// Lane `n` is channel `n`.  Past [`MAX_OUTPUT_CHANNELS`], the last
    /// lane is the shared slot.
    #[inline]
    pub(crate) fn lanes(&self) -> &[f32] {
        &self.samples[..frame_lanes(self.channels)]
    }

    /// The click-free frame is these lanes, so a click in them stays in
    /// a take.
    #[inline]
    pub(crate) fn from_lanes(lanes: &[f32], channels: usize) -> Self {
        Self::from_both_lanes(lanes, lanes, channels)
    }

    #[inline]
    pub(crate) fn record_lanes(&self) -> &[f32] {
        &self.record[..frame_lanes(self.channels)]
    }

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

/// No allocation.  Unity copies.  The tick is after the ceiling; the
/// click-free lanes are copied before it.  A mask change keeps the
/// click limiter on the old lanes until it releases, so no lane steps
/// out of its gain.
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
    /// The mask, plus lanes of an older mask until the limiter
    /// releases, so a dropped lane does not step out of its gain.
    click_lanes: u8,
    /// So the tick can wait the device's alignment delay.
    click_line: MonoDelayLine,
    /// A change crossfades while ticks are queued, and snaps while the
    /// line is silent.
    click_tap: DelayTap,
    /// Once this passes every read, the line is reset and skipped.
    click_quiet: usize,
    /// A block with frames that did not map this device left stale
    /// ticks, so the next block resets the line.
    click_mapped: bool,
    /// After the click line, so a tick already waiting is ducked too.
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
    device_generation: u64,
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

    /// Fixed storage, so this is safe on the callback.  The next block
    /// adopts the controls.
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

    fn slots(&self) -> std::iter::Chain<std::ops::Range<usize>, std::option::IntoIter<usize>> {
        let mapped = self.channels.min(MAX_OUTPUT_CHANNELS);
        let extra = (self.channels > MAX_OUTPUT_CHANNELS).then_some(EXTRA);
        (0..mapped).chain(extra)
    }

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

    /// The first block, and a path that just went live, snaps.  A block
    /// of 0 frames maps nothing and keeps the queued ticks.
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

    /// After the click line, so a queued tick does not play into a
    /// measurement.  A silent line snaps, so a tick that starts this
    /// block is already ducked.
    pub(crate) fn duck_click(&mut self, ducking: bool, ramp_samples: f32) {
        let target = if ducking { 0.0 } else { 1.0 };
        if self.click_line_silent() {
            self.click_duck.snap(target);
        } else {
            self.click_duck.set_target(target, ramp_samples);
        }
    }

    /// A slot that comes back must not replay ticks it queued before.
    pub(crate) fn silence_click_line(&mut self) {
        self.click_line.reset();
        self.click_quiet = usize::MAX;
    }

    /// Whether every position the click tap reads (or will read) holds a
    /// zero: the last non-zero tick is older than the longest delay.
    pub(crate) fn click_line_silent(&self) -> bool {
        self.click_quiet > self.click_tap.longest() as usize
    }

    /// While the line is silent the tap snaps: a pending crossfade
    /// would read the next tick at two positions.
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
            // Anything older would replay if a later delay is longer.
            self.silence_click_line();
        }
        if delayed == 0.0 || duck == 1.0 {
            delayed
        } else {
            delayed * duck
        }
    }

    /// A path that starts playing here snaps; one that stays crossfades.
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
        // No chain latency and no trim, so a path trim does not move it.
        let click_delay = plan.dry;
        if self.adopted {
            self.click_tap.set_target(click_delay, fade);
        } else {
            self.click_tap.snap(click_delay);
        }
        self.routed = routed;
        controls.publish_alignment(&plan, &timing.latency, timing.rate);
    }

    #[cfg(test)]
    pub(crate) fn map(&mut self, left: f32, right: f32) -> &DeviceFrame {
        let mut frames = [[0.0; 2]; MAX_PATHS];
        frames[0] = [left, right];
        self.map_frames(&frames, 0.0)
    }

    /// A settled tap at delay 0 reads the frame just written.  Call
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

    /// The tick is added after the ceiling, and only when it is not 0.
    /// A zero tick still runs the click limiter while it is releasing.
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
        // Unity copies, in any FP mode.  Learn and Match measure here,
        // so the fader does not move them.
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
        // Ceiling, then the tick on its own limiter, then mute.  A zero
        // tick is not added (`x + 0.0` clears a negative zero).  The
        // click limiter keeps running while it releases, or the music
        // would return to unity in one sample.  Click-free lanes are
        // copied before the tick and get only the mute.
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
        // Click-free, so the metronome never moves the reading.
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

    /// The learned trim only moves when its setting changes.
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

/// Starts from the first path, so one path passes bit for bit.
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

/// A weight of 1 copies.  Silence is +0.
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
/// The default map must match this bit for bit.
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
