//! Realtime device mapping: controls atomics, frames and the mapper.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};

use super::super::delay::{DelayLine, DelayTap, TAP_FADE_SECONDS};
use super::super::limiter::SafetyLimiter;
use super::super::mixer::Ramp;
use super::super::path::{LiveSlots, MAX_PATHS};
use super::align::{plan_alignment, trim_frames, AlignmentPlan, AlignmentReport};
use super::types::{ChannelMap, OutputDeviceSettings, UnusedChannels, MAX_OUTPUT_CHANNELS};
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
    /// Block peak per device channel (`f32` bits), after the map, the
    /// device level, the safety ceiling and mute.  Channels the device
    /// does not have read 0.
    meters: [AtomicU32; MAX_OUTPUT_CHANNELS],
    /// Safety-limiter gain reduction, dB (`f32` bits).  0 when the device
    /// is not limiting.  Published with the meters.
    reduction_db: AtomicU32,
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
    /// no mixer has published in this session yet).
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
            meters: std::array::from_fn(|_| AtomicU32::new(0.0f32.to_bits())),
            reduction_db: AtomicU32::new(0.0f32.to_bits()),
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
    }

    /// Copies the settings (not the meters) of a previous session.
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
    }

    /// Clears the meters and the channel count for a fresh session.
    pub(crate) fn reset(&self) {
        for meter in &self.meters {
            meter.store(0.0f32.to_bits(), Ordering::Relaxed);
        }
        self.reduction_db.store(0.0f32.to_bits(), Ordering::Relaxed);
        self.channels.store(0, Ordering::Relaxed);
        self.applied_rate.store(0, Ordering::Relaxed);
    }

    /// Forgets the published alignment: no mixer maps this device now (the
    /// bridge stopped, or a follower that is not fed).  Atomics only.
    pub(crate) fn clear_alignment(&self) {
        self.applied_rate.store(0, Ordering::Release);
    }

    /// The align switch as stored.
    pub(crate) fn align(&self) -> bool {
        self.align.load(Ordering::Relaxed)
    }

    /// The alignment the mapper works with right now (the delays its taps
    /// settle on), or `None` before a mixer published one in this session.
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
    /// a live follower add).
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
/// slot.  A generic follower's ring carries this many lanes at most.
pub(crate) const SLOTS: usize = MAX_OUTPUT_CHANNELS + 1;

/// How many lanes carry every channel of a `channels`-channel device: one
/// per channel up to [`MAX_OUTPUT_CHANNELS`], then the shared slot.
pub(crate) fn frame_lanes(channels: usize) -> usize {
    channels.min(SLOTS)
}

/// One mapped device frame: what each device channel plays.
pub(crate) struct DeviceFrame {
    samples: [f32; SLOTS],
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

    /// This frame with the calibration duck applied and the click added to
    /// `mapped` (bit `n` = device channel `n`) after the scaling, so the
    /// measurement keeps its fixed level regardless of the device fader and
    /// leaves on the channels the clock map really plays.  An empty mask
    /// falls back to channel 0 (and 1 when the device has it).
    #[inline]
    pub(crate) fn ducked(&self, gain: f32, click: f32, mapped: u32) -> Self {
        let mut frame = Self {
            samples: self.samples,
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
    /// [`Self::lanes`]).  Missing lanes read silence.
    #[inline]
    pub(crate) fn from_lanes(lanes: &[f32], channels: usize) -> Self {
        let mut samples = [0.0; SLOTS];
        let count = frame_lanes(channels).min(lanes.len());
        samples[..count].copy_from_slice(&lanes[..count]);
        Self { samples, channels }
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
/// level and before the mute ramp.
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
            adopted: false,
            live: LiveSlots::default(),
            taps: [DelayTap::default(); MAX_PATHS],
            routed: [false; MAX_PATHS],
            // The default map reaches both channels.
            mapped: 0b11,
            frame: DeviceFrame {
                samples: [0.0; SLOTS],
                channels,
            },
            peaks: [0.0; MAX_OUTPUT_CHANNELS],
        }
    }

    /// Follows a device whose channel count changed (a generic follower's
    /// stream reopened on another device).  Fixed storage, so this is safe
    /// in the callback; the next block adopts the controls outright.
    pub(crate) fn set_channels(&mut self, channels: usize) {
        if channels != self.channels {
            self.channels = channels;
            self.frame.channels = channels;
            self.adopted = false;
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
        );
    }

    /// Reads the controls for this block, for the `live` path slots.  The
    /// first block (and a path that just went live) adopts them directly;
    /// later changes take `ramp_samples` samples.  Also works out each
    /// path's alignment delay on this device from `timing`, points the taps
    /// at it (see [`plan_alignment`]) and publishes it for the UI.
    pub(crate) fn begin_paths(
        &mut self,
        controls: &DeviceControls,
        ramp_samples: f32,
        live: LiveSlots,
        timing: &PathTiming,
    ) {
        let (unused, level, mute) = controls.load_level_and_mute();
        self.limiter.set_rate(timing.rate);
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
        self.routed = routed;
        controls.publish_alignment(&plan, &timing.latency, timing.rate);
    }

    /// Path 0 alone: maps one stereo frame with path 0's map.
    #[cfg(test)]
    pub(crate) fn map(&mut self, left: f32, right: f32) -> &DeviceFrame {
        let mut frames = [[0.0; 2]; MAX_PATHS];
        frames[0] = [left, right];
        self.map_frames(&frames)
    }

    /// Maps the current frame of every live path onto the device's
    /// channels: each path is read from its delay line through this
    /// device's tap (its alignment delay here), then mapped and summed as
    /// in [`Self::map_frames`].  A settled tap at delay 0 reads the frame
    /// just written, so an unaligned path is exactly its own frame.
    #[inline]
    pub(crate) fn map_paths(&mut self, lines: &PathLines) -> &DeviceFrame {
        let mut frames = [[0.0f32; 2]; MAX_PATHS];
        for path in self.live.iter() {
            frames[path] = self.taps[path].next(&lines[path], 0);
        }
        self.map_frames(&frames)
    }

    /// Maps one frame of every live path (each after the master and its
    /// alignment) onto the device's channels, scaled by the path's level
    /// and polarity on this device, and sums them per channel: this is the
    /// sum of the paths.  Slots that are not live are ignored.  One live
    /// path at 0 dB and normal polarity gives exactly its own mapped frame.
    #[inline]
    pub(crate) fn map_frames(&mut self, frames: &[[f32; 2]; MAX_PATHS]) -> &DeviceFrame {
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
        // Unity copies, so 0 dB never touches a sample in any FP mode.
        let level = |value: f32| if gain == 1.0 { value } else { value * gain };
        let mapped = self.channels.min(MAX_OUTPUT_CHANNELS);
        for channel in 0..mapped {
            self.frame.samples[channel] =
                level(sum_paths(&mut self.weights, self.live, channel, &sources));
        }
        if self.channels > MAX_OUTPUT_CHANNELS {
            self.frame.samples[EXTRA] =
                level(sum_paths(&mut self.weights, self.live, EXTRA, &sources));
        }
        // Level, then the safety ceiling, then mute.  Mute already ramps to
        // silence, so the limiter sees the signal the fader would play.
        let lanes = frame_lanes(self.channels);
        self.limiter.process(&mut self.frame.samples[..lanes]);
        let mute = self.mute_gain.next();
        if mute != 1.0 {
            for sample in &mut self.frame.samples[..lanes] {
                *sample *= mute;
            }
        }
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

    /// Publishes this block's per-channel peaks.
    pub(crate) fn end_block(&self, controls: &DeviceControls) {
        for (meter, peak) in controls.meters.iter().zip(self.peaks) {
            meter.store(peak.to_bits(), Ordering::Relaxed);
        }
        controls.store_reduction(self.limiter.reduction_db());
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
