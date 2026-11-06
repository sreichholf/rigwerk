//! Realtime device mapping: controls atomics, frames and the mapper.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};

use super::super::mixer::Ramp;
use super::super::path::{LiveSlots, MAX_PATHS};
use super::types::{
    channel_weights, level_gain, ChannelMap, OutputDeviceSettings, UnusedChannels,
    MAX_OUTPUT_CHANNELS,
};

/// One path's channel map on one device, as the callback reads it.
struct PathMap {
    left: AtomicU8,
    right: AtomicU8,
}

impl PathMap {
    fn new(map: ChannelMap) -> Self {
        Self {
            left: AtomicU8::new(map.left),
            right: AtomicU8::new(map.right),
        }
    }

    fn store(&self, map: ChannelMap) {
        self.left.store(map.left, Ordering::Relaxed);
        self.right.store(map.right, Ordering::Relaxed);
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
/// Each signal path slot has its own channel map on the device; level, mute
/// and the unused-channel rule belong to the device.  Only path 0's map can
/// be set so far.
pub(crate) struct DeviceControls {
    maps: [PathMap; MAX_PATHS],
    silent: AtomicBool,
    /// Linear level gain (`f32` bits), already clamped.
    gain: AtomicU32,
    mute: AtomicBool,
    /// Block peak per device channel (`f32` bits), after the map and the
    /// device level.  Channels the device does not have read 0.
    meters: [AtomicU32; MAX_OUTPUT_CHANNELS],
    /// The device's real channel count, published when its stream opens.
    /// 0 = unknown (bridge not running); the UI falls back to the device
    /// inventory's channel count.  ASIO opens at most 4 outputs for now;
    /// the map sees that count.
    channels: AtomicU32,
}

impl DeviceControls {
    pub(crate) fn new() -> Self {
        let controls = Self {
            maps: std::array::from_fn(|_| PathMap::new(ChannelMap::default())),
            silent: AtomicBool::new(false),
            gain: AtomicU32::new(1.0f32.to_bits()),
            mute: AtomicBool::new(false),
            meters: std::array::from_fn(|_| AtomicU32::new(0.0f32.to_bits())),
            channels: AtomicU32::new(0),
        };
        controls.set(&OutputDeviceSettings::default());
        controls
    }

    /// Stores the device settings; `settings.map` is path 0's map.
    pub(crate) fn set(&self, settings: &OutputDeviceSettings) {
        let settings = settings.sanitized();
        self.maps[0].store(settings.map);
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
        }
        copy_bool(&self.silent, &previous.silent);
        copy_bool(&self.mute, &previous.mute);
        self.gain
            .store(previous.gain.load(Ordering::Relaxed), Ordering::Relaxed);
    }

    /// Clears the meters and the channel count for a fresh session.
    pub(crate) fn reset(&self) {
        for meter in &self.meters {
            meter.store(0.0f32.to_bits(), Ordering::Relaxed);
        }
        self.channels.store(0, Ordering::Relaxed);
    }

    pub(crate) fn meters(&self) -> [f32; MAX_OUTPUT_CHANNELS] {
        std::array::from_fn(|channel| f32::from_bits(self.meters[channel].load(Ordering::Relaxed)))
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

    /// Path 0's map, the unused mode and the effective gain (0 when muted)
    /// right now.
    #[cfg(test)]
    pub(super) fn load(&self) -> (ChannelMap, UnusedChannels, f32) {
        let (unused, gain) = self.load_device();
        (self.path_map(0), unused, gain)
    }

    /// The unused mode and the effective gain (0 when muted) right now.
    fn load_device(&self) -> (UnusedChannels, f32) {
        let unused = if self.silent.load(Ordering::Relaxed) {
            UnusedChannels::Silent
        } else {
            UnusedChannels::MonoCopy
        };
        let gain = if self.mute.load(Ordering::Relaxed) {
            0.0
        } else {
            f32::from_bits(self.gain.load(Ordering::Relaxed))
        };
        (unused, gain)
    }
}

/// Slot of the shared value every channel at or past
/// [`MAX_OUTPUT_CHANNELS`] gets: those channels are never reached, so they
/// all carry the same unused-channel value.
const EXTRA: usize = MAX_OUTPUT_CHANNELS;
const SLOTS: usize = MAX_OUTPUT_CHANNELS + 1;

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
}

/// Applies a device's channel map, level and mute to the finished stereo
/// mix, one frame at a time, for whatever buffer layout the backend has.
/// Sized for its channel count when the stream opens; fixed arrays, so the
/// callback never allocates.
///
/// Per block: [`Self::begin_paths`] reads the controls once and points the
/// ramps at them, [`Self::map_paths`] runs per frame, [`Self::end_block`] publishes
/// the meters.  Every weight and the device gain move over the same ramp as
/// the mixer's routes, so a map, level or mute change crossfades.  Settled
/// weights are exactly 0 or 1 and the gain exactly its target, so the
/// default settings write the very samples the output always wrote.
pub(crate) struct DeviceMapper {
    channels: usize,
    /// Weights of `[L, R, (L + R) / 2]` per path slot and channel slot.
    /// Only the live paths' ramps move.
    weights: [[[Ramp; 3]; SLOTS]; MAX_PATHS],
    pub(super) gain: Ramp,
    adopted: bool,
    /// The paths mapped in this block, and in the one before (a path that
    /// just went live adopts its weights instead of ramping from stale ones).
    live: LiveSlots,
    frame: DeviceFrame,
    peaks: [f32; MAX_OUTPUT_CHANNELS],
}

impl DeviceMapper {
    pub(crate) fn new(channels: usize) -> Self {
        Self {
            channels,
            weights: [[[Ramp::new(0.0); 3]; SLOTS]; MAX_PATHS],
            gain: Ramp::new(1.0),
            adopted: false,
            live: LiveSlots::default(),
            frame: DeviceFrame {
                samples: [0.0; SLOTS],
                channels,
            },
            peaks: [0.0; MAX_OUTPUT_CHANNELS],
        }
    }

    /// Channel slots this device uses: one per channel up to the maximum,
    /// plus the shared slot when it has more.
    fn slots(&self) -> std::iter::Chain<std::ops::Range<usize>, std::option::IntoIter<usize>> {
        let mapped = self.channels.min(MAX_OUTPUT_CHANNELS);
        let extra = (self.channels > MAX_OUTPUT_CHANNELS).then_some(EXTRA);
        (0..mapped).chain(extra)
    }

    /// Path 0 alone, as every caller before paths existed (and the generic
    /// followers, which map the finished stereo mix).
    pub(crate) fn begin_block(&mut self, controls: &DeviceControls, ramp_samples: f32) {
        self.begin_paths(controls, ramp_samples, LiveSlots::main());
    }

    /// Reads the controls for this block, for the `live` path slots.  The
    /// first block (and a path that just went live) adopts them directly;
    /// later changes take `ramp_samples` samples.
    pub(crate) fn begin_paths(
        &mut self,
        controls: &DeviceControls,
        ramp_samples: f32,
        live: LiveSlots,
    ) {
        let (unused, gain) = controls.load_device();
        for path in live.iter() {
            let ramp = self.adopted && self.live.contains(path);
            let map = controls.path_map(path);
            for slot in self.slots() {
                // The shared slot is channel MAX_OUTPUT_CHANNELS, which no
                // map reaches.
                let targets = channel_weights(map, unused, self.channels, slot);
                for (weight, target) in self.weights[path][slot].iter_mut().zip(targets) {
                    if ramp {
                        weight.set_target(target, ramp_samples);
                    } else {
                        weight.snap(target);
                    }
                }
            }
        }
        self.live = live;
        if self.adopted {
            self.gain.set_target(gain, ramp_samples);
        } else {
            self.gain.snap(gain);
        }
        self.adopted = true;
        self.peaks = [0.0; MAX_OUTPUT_CHANNELS];
    }

    /// Path 0 alone: maps one stereo frame with path 0's map.
    pub(crate) fn map(&mut self, left: f32, right: f32) -> &DeviceFrame {
        let mut frames = [[0.0; 2]; MAX_PATHS];
        frames[0] = [left, right];
        self.map_paths(&frames)
    }

    /// Maps one frame of every live path (each after the master) onto the
    /// device's channels and sums them per channel: this is the sum of the
    /// paths.  Slots that are not live are ignored.  One live path gives
    /// exactly its own mapped frame.
    #[inline]
    pub(crate) fn map_paths(&mut self, frames: &[[f32; 2]; MAX_PATHS]) -> &DeviceFrame {
        let mut sources = [[0.0f32; 3]; MAX_PATHS];
        for path in self.live.iter() {
            let [left, right] = frames[path];
            sources[path] = [left, right, (left + right) * 0.5];
        }
        let gain = self.gain.next();
        // Unity copies, so 0 dB never touches a sample in any FP mode.
        let level = |value: f32| if gain == 1.0 { value } else { value * gain };
        let mapped = self.channels.min(MAX_OUTPUT_CHANNELS);
        for channel in 0..mapped {
            let value = level(sum_paths(&mut self.weights, self.live, channel, &sources));
            self.frame.samples[channel] = value;
            self.peaks[channel] = self.peaks[channel].max(value.abs());
        }
        if self.channels > MAX_OUTPUT_CHANNELS {
            self.frame.samples[EXTRA] =
                level(sum_paths(&mut self.weights, self.live, EXTRA, &sources));
        }
        &self.frame
    }

    /// Publishes the device's channel count.  Called when the stream opens,
    /// outside the callback, so the snapshot has it before the first block.
    pub(crate) fn publish_channels(&self, controls: &DeviceControls) {
        controls.set_channels(self.channels);
    }

    /// Publishes this block's per-channel peaks.
    pub(crate) fn end_block(&self, controls: &DeviceControls) {
        for (meter, peak) in controls.meters.iter().zip(self.peaks) {
            meter.store(peak.to_bits(), Ordering::Relaxed);
        }
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
