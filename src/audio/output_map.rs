//! Per-device output settings: level, mute and which device channels the
//! two mix sides reach.  Applied after the master (`output_gain` / `mute`).
//!
//! # Mapping rule
//!
//! For each device channel `c` (0-based), with `L` and `R` the mix after the
//! master:
//! - reached by L only: `L`; by R only: `R`; by both: `(L + R) / 2` (the
//!   average, not the sum);
//! - reached by neither: `(L + R) / 2` under [`UnusedChannels::MonoCopy`],
//!   silence under [`UnusedChannels::Silent`];
//! - `c >= MAX_OUTPUT_CHANNELS`: never reached, so treated as unused.
//!
//! A 1-channel device reads any non-empty map as both sides reaching
//! channel 1, so it gets `(L + R) / 2`; an empty map leaves it to
//! [`UnusedChannels`].
//!
//! With the default map (L → 1, R → 2) and `MonoCopy` this is exactly what
//! the output has always written: L and R on channels 1 and 2, their average
//! on every further channel, and the average on a 1-channel device.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

use super::mixer::Ramp;

/// Most device channels a [`ChannelMap`] can address.
pub const MAX_OUTPUT_CHANNELS: usize = 8;

/// Most follower output devices beside the clock device.  Extra entries in a
/// save are dropped on load.  PipeWire preallocates one stereo port pair per
/// slot so add/remove stays live.
pub const MAX_FOLLOWERS: usize = 4;

/// Display name and PipeWire `node.name` of the virtual publish source.
pub const VIRTUAL_OUT_NAME: &str = "RigWerk Out";

/// Session id of the virtual publish follower (`pipewire:RigWerk Out`).
pub const VIRTUAL_OUT_ID: &str = "pipewire:RigWerk Out";

/// Device level range, dB.  The same as the master output fader for now.
pub const DEVICE_LEVEL_MIN_DB: f32 = -36.0;
pub const DEVICE_LEVEL_MAX_DB: f32 = 6.0;

/// One side of the stereo mix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MixSide {
    L,
    R,
}

/// Which device channels Mix L and Mix R reach.  Bit `n` is device channel
/// `n + 1`.  See the module docs for how a map becomes samples; the default
/// (L → 1, R → 2) is today's output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelMap {
    pub left: u8,
    pub right: u8,
}

impl Default for ChannelMap {
    fn default() -> Self {
        Self {
            left: 0b01,
            right: 0b10,
        }
    }
}

impl ChannelMap {
    fn mask(self, side: MixSide) -> u8 {
        match side {
            MixSide::L => self.left,
            MixSide::R => self.right,
        }
    }

    /// Whether `side` reaches device channel `channel` (0-based).  Channels
    /// at or past [`MAX_OUTPUT_CHANNELS`] are never reached.
    pub fn routes(self, side: MixSide, channel: usize) -> bool {
        channel < MAX_OUTPUT_CHANNELS && self.mask(side) & (1 << channel) != 0
    }

    /// Lets `side` reach device channel `channel` (0-based), or stops it.
    /// Channels at or past [`MAX_OUTPUT_CHANNELS`] are ignored.
    pub fn set(&mut self, side: MixSide, channel: usize, on: bool) {
        if channel >= MAX_OUTPUT_CHANNELS {
            return;
        }
        let bit = 1 << channel;
        let mask = if on {
            self.mask(side) | bit
        } else {
            self.mask(side) & !bit
        };
        match side {
            MixSide::L => self.left = mask,
            MixSide::R => self.right = mask,
        }
    }

    /// Neither side reaches any channel.
    pub fn is_empty(self) -> bool {
        self.left == 0 && self.right == 0
    }

    /// Which sides (L, R) reach device channel `channel` (0-based) of a
    /// `channels`-channel device, by the mapping rule in the module docs: a
    /// 1-channel device is reached by both sides whenever the map is not
    /// empty.
    pub fn sides_at(self, channels: usize, channel: usize) -> (bool, bool) {
        if channels == 1 {
            let any = !self.is_empty();
            (any, any)
        } else {
            (
                self.routes(MixSide::L, channel),
                self.routes(MixSide::R, channel),
            )
        }
    }

    /// Whether any mix side reaches any of a `channels`-channel device's
    /// channels.  Bits at or past `channels` don't count.
    pub fn reaches_any(self, channels: usize) -> bool {
        (0..channels.min(MAX_OUTPUT_CHANNELS)).any(|channel| {
            let (left, right) = self.sides_at(channels, channel);
            left || right
        })
    }

    /// This map with `preset` applied to a `channels`-channel device.  Only
    /// channels below `channels` change; bits for channels the device doesn't
    /// have are kept, so they come back if a bigger device is picked again.
    pub fn with_preset(self, preset: MapPreset, channels: usize) -> Self {
        let shown = low_bits(channels);
        let (left, right) = match preset {
            MapPreset::Stereo12 => (0b0001, 0b0010),
            MapPreset::Stereo34 => (0b0100, 0b1000),
            MapPreset::BothPairs => (0b0101, 0b1010),
            MapPreset::MonoToAll => (0xFF, 0xFF),
            MapPreset::Swap => (self.right, self.left),
            MapPreset::Clear => (0, 0),
        };
        Self {
            left: (self.left & !shown) | (left & shown),
            right: (self.right & !shown) | (right & shown),
        }
    }
}

/// Bits of the first `channels` device channels.
fn low_bits(channels: usize) -> u8 {
    if channels >= MAX_OUTPUT_CHANNELS {
        u8::MAX
    } else {
        (1u8 << channels) - 1
    }
}

/// The quick maps the Signal Routing page offers for a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapPreset {
    /// L -> 1, R -> 2.
    Stereo12,
    /// L -> 3, R -> 4.
    Stereo34,
    /// L -> 1 and 3, R -> 2 and 4.
    BothPairs,
    /// Both sides on every channel: each plays (L + R) / 2.
    MonoToAll,
    /// L and R trade channels.
    Swap,
    /// Nothing routed; every channel follows [`UnusedChannels`].
    Clear,
}

impl MapPreset {
    /// The presets that make sense on a `channels`-channel device, in button
    /// order: the pair presets only where the pairs exist, and on a 1-channel
    /// device (which plays the average anyway) only Mono to all and Clear.
    pub fn available(channels: usize) -> &'static [Self] {
        use MapPreset::*;
        match channels {
            0 | 1 => &[MonoToAll, Clear],
            2 | 3 => &[Stereo12, MonoToAll, Swap, Clear],
            _ => &[Stereo12, Stereo34, BothPairs, MonoToAll, Swap, Clear],
        }
    }

    /// The button text on a `channels`-channel device.
    pub fn label(self, channels: usize) -> &'static str {
        match self {
            Self::Stereo12 if channels == 2 => "Stereo",
            Self::Stereo12 => "Stereo 1/2",
            Self::Stereo34 => "Stereo 3/4",
            Self::BothPairs => "1/2 + 3/4",
            Self::MonoToAll => "Mono to all",
            Self::Swap => "Swap L/R",
            Self::Clear => "Clear",
        }
    }
}

/// What a device channel that no mix side reaches gets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnusedChannels {
    Silent,
    /// The L/R average, as the output has always done.  Also what an
    /// unknown stored mode reads as.
    #[default]
    #[serde(other)]
    MonoCopy,
}

/// Level, mute and channel map of one output device, applied after the
/// master.  Missing fields read as the defaults (0 dB, unmuted, L → 1,
/// R → 2, mono copy on the rest), which is today's output.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputDeviceSettings {
    /// Device level, dB.
    pub level_db: f32,
    pub mute: bool,
    pub map: ChannelMap,
    pub unused: UnusedChannels,
}

impl Default for OutputDeviceSettings {
    fn default() -> Self {
        Self {
            level_db: 0.0,
            mute: false,
            map: ChannelMap::default(),
            unused: UnusedChannels::default(),
        }
    }
}

impl OutputDeviceSettings {
    /// A NaN level becomes 0 dB; the level is clamped to
    /// [`DEVICE_LEVEL_MIN_DB`]..=[`DEVICE_LEVEL_MAX_DB`].
    pub fn sanitized(self) -> Self {
        let level_db = if self.level_db.is_nan() {
            0.0
        } else {
            self.level_db
        };
        Self {
            level_db: level_db.clamp(DEVICE_LEVEL_MIN_DB, DEVICE_LEVEL_MAX_DB),
            ..self
        }
    }

    /// How many of a `channels`-channel device's channels play the mix:
    /// those a side reaches, plus the unreached ones under
    /// [`UnusedChannels::MonoCopy`] (they carry the average).  Level and mute
    /// don't count: this is about the routing, not about what is audible.
    pub fn playing_channels(&self, channels: usize) -> usize {
        (0..channels.min(MAX_OUTPUT_CHANNELS))
            .filter(|&channel| {
                channel_weights(self.map, self.unused, channels, channel) != [0.0; 3]
            })
            .count()
    }

    /// One settled frame of a `channels`-channel device under these settings
    /// (no ramps): what the callback writes once a change has crossfaded.
    /// Channels past the device or past [`MAX_OUTPUT_CHANNELS`] read 0. For
    /// the UI, e.g. preview meters; the callback uses [`DeviceMapper`].
    pub(crate) fn settled_frame(
        &self,
        channels: usize,
        left: f32,
        right: f32,
    ) -> [f32; MAX_OUTPUT_CHANNELS] {
        let gain = if self.mute {
            0.0
        } else {
            level_gain(self.level_db)
        };
        let sources = [left, right, (left + right) * 0.5];
        let mut frame = [0.0; MAX_OUTPUT_CHANNELS];
        for (channel, sample) in frame.iter_mut().enumerate().take(channels) {
            let weights = channel_weights(self.map, self.unused, channels, channel);
            *sample = gain
                * weights
                    .iter()
                    .zip(sources)
                    .map(|(weight, source)| weight * source)
                    .sum::<f32>();
        }
        frame
    }
}

/// Linear gain of a device level in dB.  0 dB is exactly 1.0, so the default
/// level leaves every sample untouched.
fn level_gain(level_db: f32) -> f32 {
    if level_db == 0.0 {
        1.0
    } else {
        10.0f32.powf(level_db / 20.0)
    }
}

/// How much of `L`, `R` and `(L + R) / 2` device channel `channel` (0-based)
/// of a `channels`-channel device carries, following the module docs.
/// Exactly one weight is 1 (or none, for a silent channel), so a settled map
/// copies one value instead of summing.
fn channel_weights(
    map: ChannelMap,
    unused: UnusedChannels,
    channels: usize,
    channel: usize,
) -> [f32; 3] {
    match map.sides_at(channels, channel) {
        (true, false) => [1.0, 0.0, 0.0],
        (false, true) => [0.0, 1.0, 0.0],
        (true, true) => [0.0, 0.0, 1.0],
        (false, false) => match unused {
            UnusedChannels::MonoCopy => [0.0, 0.0, 1.0],
            UnusedChannels::Silent => [0.0, 0.0, 0.0],
        },
    }
}

/// One output device's settings as the audio callback reads them, plus its
/// per-channel meters.  The UI stores, the callback loads once per block.
pub(crate) struct DeviceControls {
    left: AtomicU8,
    right: AtomicU8,
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
            left: AtomicU8::new(0),
            right: AtomicU8::new(0),
            silent: AtomicBool::new(false),
            gain: AtomicU32::new(1.0f32.to_bits()),
            mute: AtomicBool::new(false),
            meters: std::array::from_fn(|_| AtomicU32::new(0.0f32.to_bits())),
            channels: AtomicU32::new(0),
        };
        controls.set(&OutputDeviceSettings::default());
        controls
    }

    pub(crate) fn set(&self, settings: &OutputDeviceSettings) {
        let settings = settings.sanitized();
        self.left.store(settings.map.left, Ordering::Relaxed);
        self.right.store(settings.map.right, Ordering::Relaxed);
        self.silent
            .store(settings.unused == UnusedChannels::Silent, Ordering::Relaxed);
        self.gain
            .store(level_gain(settings.level_db).to_bits(), Ordering::Relaxed);
        self.mute.store(settings.mute, Ordering::Relaxed);
    }

    /// Copies the settings (not the meters) of a previous session.
    pub(crate) fn adopt_settings_from(&self, previous: &Self) {
        let copy_u8 = |to: &AtomicU8, from: &AtomicU8| {
            to.store(from.load(Ordering::Relaxed), Ordering::Relaxed);
        };
        let copy_bool = |to: &AtomicBool, from: &AtomicBool| {
            to.store(from.load(Ordering::Relaxed), Ordering::Relaxed);
        };
        copy_u8(&self.left, &previous.left);
        copy_u8(&self.right, &previous.right);
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

    /// The map, unused mode and effective gain (0 when muted) right now.
    fn load(&self) -> (ChannelMap, UnusedChannels, f32) {
        let map = ChannelMap {
            left: self.left.load(Ordering::Relaxed),
            right: self.right.load(Ordering::Relaxed),
        };
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
        (map, unused, gain)
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
/// Per block: [`Self::begin_block`] reads the controls once and points the
/// ramps at them, [`Self::map`] runs per frame, [`Self::end_block`] publishes
/// the meters.  Every weight and the device gain move over the same ramp as
/// the mixer's routes, so a map, level or mute change crossfades.  Settled
/// weights are exactly 0 or 1 and the gain exactly its target, so the
/// default settings write the very samples the output always wrote.
pub(crate) struct DeviceMapper {
    channels: usize,
    /// Weights of `[L, R, (L + R) / 2]` per channel slot.
    weights: [[Ramp; 3]; SLOTS],
    gain: Ramp,
    adopted: bool,
    frame: DeviceFrame,
    peaks: [f32; MAX_OUTPUT_CHANNELS],
}

impl DeviceMapper {
    pub(crate) fn new(channels: usize) -> Self {
        Self {
            channels,
            weights: [[Ramp::new(0.0); 3]; SLOTS],
            gain: Ramp::new(1.0),
            adopted: false,
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

    /// Reads the controls for this block.  The first block adopts them
    /// directly; later changes take `ramp_samples` samples.
    pub(crate) fn begin_block(&mut self, controls: &DeviceControls, ramp_samples: f32) {
        let (map, unused, gain) = controls.load();
        for slot in self.slots() {
            // The shared slot is channel MAX_OUTPUT_CHANNELS, which no map
            // reaches.
            let targets = channel_weights(map, unused, self.channels, slot);
            for (ramp, target) in self.weights[slot].iter_mut().zip(targets) {
                if self.adopted {
                    ramp.set_target(target, ramp_samples);
                } else {
                    ramp.snap(target);
                }
            }
        }
        if self.adopted {
            self.gain.set_target(gain, ramp_samples);
        } else {
            self.gain.snap(gain);
        }
        self.adopted = true;
        self.peaks = [0.0; MAX_OUTPUT_CHANNELS];
    }

    /// Maps one frame of the finished mix (after the master) onto the
    /// device's channels.
    #[inline]
    pub(crate) fn map(&mut self, left: f32, right: f32) -> &DeviceFrame {
        let sources = [left, right, (left + right) * 0.5];
        let gain = self.gain.next();
        // Unity copies, so 0 dB never touches a sample in any FP mode.
        let level = |value: f32| if gain == 1.0 { value } else { value * gain };
        let mapped = self.channels.min(MAX_OUTPUT_CHANNELS);
        for channel in 0..mapped {
            let value = level(blend(&mut self.weights[channel], sources));
            self.frame.samples[channel] = value;
            self.peaks[channel] = self.peaks[channel].max(value.abs());
        }
        if self.channels > MAX_OUTPUT_CHANNELS {
            self.frame.samples[EXTRA] = level(blend(&mut self.weights[EXTRA], sources));
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
pub(super) fn old_write_frame(frame: &mut [f32], left: f32, right: f32) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_settled_frame_matches_the_mapper() {
        let mut map = ChannelMap::default();
        map.set(MixSide::R, 2, true);
        map.set(MixSide::L, 3, true);
        let cases = [
            OutputDeviceSettings::default(),
            OutputDeviceSettings {
                level_db: -6.0,
                map,
                unused: UnusedChannels::Silent,
                ..OutputDeviceSettings::default()
            },
            OutputDeviceSettings {
                mute: true,
                ..OutputDeviceSettings::default()
            },
        ];
        for settings in cases {
            for channels in [1, 2, 6] {
                let controls = DeviceControls::new();
                controls.set(&settings);
                let mut mapper = DeviceMapper::new(channels);
                mapper.begin_block(&controls, 64.0);
                let mapped = mapper.map(0.5, -0.25);
                let settled = settings.settled_frame(channels, 0.5, -0.25);
                for (channel, sample) in settled.iter().enumerate() {
                    let expected = mapped.sample(channel);
                    assert!(
                        (sample - expected).abs() < 1.0e-6,
                        "{settings:?} {channels} ch, channel {channel}: {sample} != {expected}"
                    );
                }
            }
        }
    }

    const RAMP: f32 = 288.0;

    fn mapped_device(
        settings: OutputDeviceSettings,
        channels: usize,
    ) -> (DeviceMapper, DeviceControls) {
        let controls = DeviceControls::new();
        controls.set(&settings);
        let mut mapper = DeviceMapper::new(channels);
        mapper.begin_block(&controls, RAMP);
        (mapper, controls)
    }

    fn samples(frame: &DeviceFrame, channels: usize) -> Vec<f32> {
        (0..channels).map(|channel| frame.sample(channel)).collect()
    }

    fn bits(values: &[f32]) -> Vec<u32> {
        values.iter().map(|value| value.to_bits()).collect()
    }

    /// Awkward inputs: signed zeros, rounding-sensitive pairs, subnormals,
    /// hot and huge values.
    const PAIRS: [(f32, f32); 12] = [
        (0.5, -0.25),
        (-0.0, 0.0),
        (-0.0, -0.0),
        (0.0, -0.0),
        (0.1, 0.2),
        (1.0e-30, -3.0e-30),
        (1.0e-40, 2.0e-41),
        (1.7, -2.3),
        (f32::MAX, f32::MAX),
        (0.333_333_34, 0.666_666_7),
        (-1.0, 1.0),
        (0.123_456_79, -0.987_654_3),
    ];

    #[test]
    fn the_default_map_writes_exactly_what_the_output_always_wrote() {
        for channels in [1, 2, 3, 4, 8, 10] {
            let (mut mapper, controls) = mapped_device(OutputDeviceSettings::default(), channels);
            // Two blocks: the adopted first block and a steady later one.
            for block in 0..2 {
                if block == 1 {
                    mapper.begin_block(&controls, RAMP);
                }
                for (left, right) in PAIRS {
                    let mut expected = vec![0.0f32; channels];
                    old_write_frame(&mut expected, left, right);
                    let got = samples(mapper.map(left, right), channels);
                    assert_eq!(
                        bits(&got),
                        bits(&expected),
                        "{channels} ch, block {block}, ({left:e}, {right:e})"
                    );
                }
            }
        }
    }

    #[test]
    fn both_sides_on_one_channel_play_their_average() {
        let settings = OutputDeviceSettings {
            map: ChannelMap {
                left: 0b01,
                right: 0b01,
            },
            unused: UnusedChannels::Silent,
            ..OutputDeviceSettings::default()
        };
        let (mut mapper, _) = mapped_device(settings, 2);
        assert_eq!(samples(mapper.map(0.5, -0.25), 2), [0.125, 0.0]);
    }

    #[test]
    fn silent_leaves_unreached_channels_at_zero() {
        let settings = OutputDeviceSettings {
            map: ChannelMap {
                left: 0b0100,
                right: 0b1000,
            },
            unused: UnusedChannels::Silent,
            ..OutputDeviceSettings::default()
        };
        let (mut mapper, _) = mapped_device(settings, 4);
        let out = samples(mapper.map(0.5, -0.25), 4);
        assert_eq!(out, [0.0, 0.0, 0.5, -0.25]);
        assert_eq!(bits(&out[..2]), [0, 0], "silence is +0");
    }

    #[test]
    fn a_mono_device_reads_any_map_as_both_sides() {
        let swapped = OutputDeviceSettings {
            map: ChannelMap {
                left: 0b10,
                right: 0,
            },
            unused: UnusedChannels::Silent,
            ..OutputDeviceSettings::default()
        };
        let (mut mapper, _) = mapped_device(swapped, 1);
        assert_eq!(samples(mapper.map(0.5, -0.25), 1), [0.125]);

        let empty = OutputDeviceSettings {
            map: ChannelMap { left: 0, right: 0 },
            ..swapped
        };
        let (mut mapper, _) = mapped_device(empty, 1);
        assert_eq!(samples(mapper.map(0.5, -0.25), 1), [0.0]);
        let (mut mapper, _) = mapped_device(
            OutputDeviceSettings {
                unused: UnusedChannels::MonoCopy,
                ..empty
            },
            1,
        );
        assert_eq!(samples(mapper.map(0.5, -0.25), 1), [0.125]);
    }

    #[test]
    fn channels_past_the_maximum_are_unused() {
        let all = OutputDeviceSettings {
            map: ChannelMap {
                left: 0xff,
                right: 0,
            },
            ..OutputDeviceSettings::default()
        };
        let (mut mapper, _) = mapped_device(all, 10);
        let out = samples(mapper.map(0.5, -0.25), 10);
        assert_eq!(&out[..8], &[0.5; 8]);
        assert_eq!(&out[8..], &[0.125; 2]);

        let silent = OutputDeviceSettings {
            unused: UnusedChannels::Silent,
            ..all
        };
        let (mut mapper, _) = mapped_device(silent, 10);
        let out = samples(mapper.map(0.5, -0.25), 10);
        assert_eq!(&out[8..], &[0.0; 2]);
        // Past the device's own channels there is nothing to play.
        assert_eq!(mapper.map(0.5, -0.25).sample(10), 0.0);
    }

    #[test]
    fn the_device_level_scales_after_the_map() {
        let settings = OutputDeviceSettings {
            level_db: -6.0,
            ..OutputDeviceSettings::default()
        };
        let (mut mapper, _) = mapped_device(settings, 3);
        let gain = 10.0f32.powf(-6.0 / 20.0);
        let out = samples(mapper.map(0.5, -0.25), 3);
        assert_eq!(out, [0.5 * gain, -0.25 * gain, 0.125 * gain]);
        assert!((gain - 0.501).abs() < 0.001);
    }

    /// Runs `frames` frames of a constant mix through one block.
    fn run(
        mapper: &mut DeviceMapper,
        controls: &DeviceControls,
        channels: usize,
        frames: usize,
    ) -> Vec<Vec<f32>> {
        mapper.begin_block(controls, RAMP);
        let out = (0..frames)
            .map(|_| samples(mapper.map(0.5, -0.25), channels))
            .collect();
        mapper.end_block(controls);
        out
    }

    fn largest_step(out: &[Vec<f32>], channel: usize) -> f32 {
        out.windows(2)
            .map(|pair| (pair[1][channel] - pair[0][channel]).abs())
            .fold(0.0, f32::max)
    }

    #[test]
    fn device_mute_fades_to_silence() {
        let (mut mapper, controls) = mapped_device(OutputDeviceSettings::default(), 2);
        let _adopted = run(&mut mapper, &controls, 2, 16);
        controls.set(&OutputDeviceSettings {
            mute: true,
            ..OutputDeviceSettings::default()
        });
        let out = run(&mut mapper, &controls, 2, 400);
        assert!(out[0][0] > 0.49 && out[0][0] < 0.5);
        assert!(largest_step(&out, 0) < 0.5 / 100.0);
        let settled = out
            .iter()
            .position(|frame| frame.iter().all(|value| *value == 0.0))
            .expect("mute reaches silence");
        assert!(settled <= RAMP as usize + 1, "{settled}");
        assert!(out[settled..]
            .iter()
            .all(|frame| frame.iter().all(|value| *value == 0.0)));
        let meters = controls.meters();
        assert!(meters[0] > 0.49, "the block's peak includes the fade");
    }

    #[test]
    fn a_map_change_crossfades_and_settles() {
        let (mut mapper, controls) = mapped_device(OutputDeviceSettings::default(), 4);
        let before = run(&mut mapper, &controls, 4, 16);
        assert_eq!(before[15], [0.5, -0.25, 0.125, 0.125]);

        // Swap the sides and silence the rest.
        let swapped = OutputDeviceSettings {
            map: ChannelMap {
                left: 0b10,
                right: 0b01,
            },
            unused: UnusedChannels::Silent,
            ..OutputDeviceSettings::default()
        };
        controls.set(&swapped);
        let mut out = run(&mut mapper, &controls, 4, 100);
        // A block boundary mid-ramp continues the same slope.
        out.extend(run(&mut mapper, &controls, 4, 300));
        let full = 0.75;
        for channel in 0..4 {
            let step = largest_step(&out, channel);
            assert!(step < full / 100.0, "channel {channel} stepped by {step}");
        }
        assert!(out[0][0] > 0.49, "the first sample barely moves");
        let mid = &out[144];
        assert!(mid[0] > -0.2 && mid[0] < 0.45, "{mid:?}");
        assert_eq!(*out.last().unwrap(), [-0.25, 0.5, 0.0, 0.0]);
        let settled = out
            .iter()
            .position(|frame| frame[..] == [-0.25, 0.5, 0.0, 0.0])
            .expect("the new map is reached");
        assert!(settled <= RAMP as usize + 1, "{settled}");
        assert!(out[settled..]
            .iter()
            .all(|frame| frame[..] == [-0.25, 0.5, 0.0, 0.0]));
    }

    #[test]
    fn meters_hold_each_channels_block_peak() {
        let settings = OutputDeviceSettings {
            map: ChannelMap {
                left: 0b001,
                right: 0b100,
            },
            unused: UnusedChannels::Silent,
            ..OutputDeviceSettings::default()
        };
        let controls = DeviceControls::new();
        controls.set(&settings);
        let mut mapper = DeviceMapper::new(3);
        mapper.begin_block(&controls, RAMP);
        mapper.map(0.25, 0.1);
        mapper.map(-0.75, -0.5);
        mapper.map(0.5, 0.2);
        mapper.end_block(&controls);
        let meters = controls.meters();
        assert_eq!(meters[..3], [0.75, 0.0, 0.5]);
        assert!(meters[3..].iter().all(|meter| *meter == 0.0));
        mapper.publish_channels(&controls);
        assert_eq!(controls.channels(), 3);

        // The next block starts its peaks over.
        mapper.begin_block(&controls, RAMP);
        mapper.map(0.1, 0.0);
        mapper.end_block(&controls);
        assert_eq!(controls.meters()[..3], [0.1, 0.0, 0.0]);

        controls.reset();
        assert_eq!(controls.meters(), [0.0; MAX_OUTPUT_CHANNELS]);
        assert_eq!(controls.channels(), 0);
    }

    #[test]
    fn controls_store_the_sanitized_level_and_copy_to_a_new_session() {
        let controls = DeviceControls::new();
        let (map, unused, gain) = controls.load();
        assert_eq!(
            (map, unused, gain),
            (ChannelMap::default(), UnusedChannels::MonoCopy, 1.0)
        );
        let settings = OutputDeviceSettings {
            level_db: 40.0,
            mute: false,
            map: ChannelMap {
                left: 0b1100,
                right: 0b0011,
            },
            unused: UnusedChannels::Silent,
        };
        controls.set(&settings);
        let next = DeviceControls::new();
        next.adopt_settings_from(&controls);
        let (map, unused, gain) = next.load();
        assert_eq!(map, settings.map);
        assert_eq!(unused, UnusedChannels::Silent);
        assert_eq!(gain, 10.0f32.powf(DEVICE_LEVEL_MAX_DB / 20.0));
        next.set(&OutputDeviceSettings {
            mute: true,
            ..settings
        });
        assert_eq!(next.load().2, 0.0);
    }

    #[test]
    fn channel_map_routes_and_sets_device_channels() {
        let mut map = ChannelMap::default();
        assert!(map.routes(MixSide::L, 0) && !map.routes(MixSide::L, 1));
        assert!(map.routes(MixSide::R, 1) && !map.routes(MixSide::R, 0));
        assert!(!map.routes(MixSide::L, MAX_OUTPUT_CHANNELS));
        map.set(MixSide::L, 7, true);
        assert_eq!(map.left, 0b1000_0001);
        map.set(MixSide::R, MAX_OUTPUT_CHANNELS, true);
        assert_eq!(map.right, 0b10);
        map.set(MixSide::L, 0, false);
        map.set(MixSide::L, 7, false);
        map.set(MixSide::R, 1, false);
        assert!(map.is_empty());
        assert!(!ChannelMap::default().is_empty());
    }

    #[test]
    fn device_level_is_clamped_and_nan_is_zero() {
        let level = |level_db| {
            OutputDeviceSettings {
                level_db,
                ..OutputDeviceSettings::default()
            }
            .sanitized()
            .level_db
        };
        assert_eq!(level(20.0), 6.0);
        assert_eq!(level(-100.0), -36.0);
        assert_eq!(level(f32::NAN), 0.0);
        assert_eq!(level(f32::INFINITY), DEVICE_LEVEL_MAX_DB);
        assert_eq!(level(-3.0), -3.0);
    }

    #[test]
    fn unmute_returns_to_exact_unity_and_the_default_output() {
        let (mut mapper, controls) = mapped_device(OutputDeviceSettings::default(), 4);
        let _adopted = run(&mut mapper, &controls, 4, 16);
        controls.set(&OutputDeviceSettings {
            mute: true,
            ..OutputDeviceSettings::default()
        });
        let _muted = run(&mut mapper, &controls, 4, 400);
        controls.set(&OutputDeviceSettings::default());
        let _back = run(&mut mapper, &controls, 4, 400);
        assert_eq!(mapper.gain.next(), 1.0);
        mapper.begin_block(&controls, RAMP);
        for (left, right) in PAIRS {
            let mut expected = vec![0.0f32; 4];
            old_write_frame(&mut expected, left, right);
            let got = samples(mapper.map(left, right), 4);
            assert_eq!(bits(&got), bits(&expected), "({left:e}, {right:e})");
        }
    }

    #[test]
    fn a_level_change_ramps_and_settles_exactly_on_the_target() {
        let (mut mapper, controls) = mapped_device(OutputDeviceSettings::default(), 2);
        let _adopted = run(&mut mapper, &controls, 2, 16);
        controls.set(&OutputDeviceSettings {
            level_db: -6.0,
            ..OutputDeviceSettings::default()
        });
        let gain = 10.0f32.powf(-6.0 / 20.0);
        let out = run(&mut mapper, &controls, 2, 400);
        assert!(out[0][0] < 0.5 && out[0][0] > 0.49);
        assert!(largest_step(&out, 0) < 0.25 / 100.0);
        let target = [0.5 * gain, -0.25 * gain];
        let settled = out
            .iter()
            .position(|frame| frame[..] == target)
            .expect("the level settles");
        assert!(settled <= RAMP as usize + 1, "{settled}");
        assert!(out[settled..].iter().all(|frame| frame[..] == target));
        assert_eq!(mapper.gain.next(), gain);
    }

    #[test]
    fn silent_mono_device_with_an_empty_map_is_silent() {
        let settings = OutputDeviceSettings {
            map: ChannelMap { left: 0, right: 0 },
            unused: UnusedChannels::Silent,
            ..OutputDeviceSettings::default()
        };
        let (mut mapper, controls) = mapped_device(settings, 1);
        let out = run(&mut mapper, &controls, 1, 8);
        assert!(out.iter().all(|frame| frame[0].to_bits() == 0));
        assert_eq!(controls.meters()[0], 0.0);
    }

    #[test]
    fn the_channel_count_is_published_at_open() {
        let controls = DeviceControls::new();
        assert_eq!(controls.channels(), 0, "unknown until a stream opens");
        DeviceMapper::new(6).publish_channels(&controls);
        assert_eq!(controls.channels(), 6);
        controls.reset();
        assert_eq!(controls.channels(), 0);
    }

    fn map(left: u8, right: u8) -> ChannelMap {
        ChannelMap { left, right }
    }

    #[test]
    fn presets_fit_the_channel_count() {
        use MapPreset::*;
        assert_eq!(MapPreset::available(1), &[MonoToAll, Clear]);
        assert_eq!(MapPreset::available(2), &[Stereo12, MonoToAll, Swap, Clear]);
        assert_eq!(MapPreset::available(3), &[Stereo12, MonoToAll, Swap, Clear]);
        for channels in [4, 6, 8] {
            assert_eq!(
                MapPreset::available(channels),
                &[Stereo12, Stereo34, BothPairs, MonoToAll, Swap, Clear]
            );
        }
        assert_eq!(Stereo12.label(2), "Stereo");
        assert_eq!(Stereo12.label(4), "Stereo 1/2");
        assert_eq!(BothPairs.label(4), "1/2 + 3/4");
    }

    #[test]
    fn presets_set_the_shown_channels() {
        let default = ChannelMap::default();
        assert_eq!(default.with_preset(MapPreset::Stereo12, 4), map(0b01, 0b10));
        assert_eq!(
            default.with_preset(MapPreset::Stereo34, 4),
            map(0b0100, 0b1000)
        );
        assert_eq!(
            default.with_preset(MapPreset::BothPairs, 4),
            map(0b0101, 0b1010)
        );
        assert_eq!(
            default.with_preset(MapPreset::MonoToAll, 4),
            map(0b1111, 0b1111)
        );
        assert_eq!(
            default.with_preset(MapPreset::MonoToAll, 8),
            map(0xFF, 0xFF)
        );
        assert_eq!(default.with_preset(MapPreset::Swap, 2), map(0b10, 0b01));
        assert_eq!(default.with_preset(MapPreset::Clear, 2), map(0, 0));
        assert_eq!(
            default.with_preset(MapPreset::MonoToAll, 1),
            map(0b01, 0b11),
            "only channel 1 changes on a 1-channel device"
        );
    }

    #[test]
    fn presets_keep_channels_the_device_does_not_have() {
        // Out 3/4 set on a 4-channel interface, now seen as stereo.
        let wide = map(0b0101, 0b1010);
        assert_eq!(wide.with_preset(MapPreset::Clear, 2), map(0b0100, 0b1000));
        assert_eq!(wide.with_preset(MapPreset::Swap, 2), map(0b0110, 0b1001));
        // Not offered on 2 channels; it would only clear Out 1/2.
        assert_eq!(
            wide.with_preset(MapPreset::Stereo34, 2),
            map(0b0100, 0b1000)
        );
    }

    #[test]
    fn a_mono_device_is_reached_by_any_map() {
        assert_eq!(map(0b0100, 0).sides_at(1, 0), (true, true));
        assert_eq!(map(0, 0).sides_at(1, 0), (false, false));
        assert_eq!(ChannelMap::default().sides_at(2, 1), (false, true));
        assert!(map(0b0100, 0).reaches_any(1));
        assert!(
            !map(0b0100, 0).reaches_any(2),
            "channel 3 is past the device"
        );
        assert!(map(0b0100, 0).reaches_any(4));
        assert!(!map(0, 0).reaches_any(8));
    }

    #[test]
    fn playing_channels_count_reached_and_copied_ones() {
        let settings = |map, unused| OutputDeviceSettings {
            map,
            unused,
            ..OutputDeviceSettings::default()
        };
        let copy = UnusedChannels::MonoCopy;
        let silent = UnusedChannels::Silent;
        assert_eq!(settings(ChannelMap::default(), copy).playing_channels(2), 2);
        assert_eq!(settings(ChannelMap::default(), copy).playing_channels(4), 4);
        assert_eq!(
            settings(ChannelMap::default(), silent).playing_channels(4),
            2
        );
        assert_eq!(settings(map(0b0100, 0b1000), silent).playing_channels(4), 2);
        assert_eq!(settings(map(0, 0), silent).playing_channels(4), 0);
        assert_eq!(settings(map(0, 0), copy).playing_channels(4), 4);
        assert_eq!(settings(map(0b0100, 0), silent).playing_channels(1), 1);
        assert_eq!(settings(map(0, 0), silent).playing_channels(1), 0);
        let muted = OutputDeviceSettings {
            mute: true,
            ..OutputDeviceSettings::default()
        };
        assert_eq!(muted.playing_channels(2), 2, "mute is not routing");
    }
}
