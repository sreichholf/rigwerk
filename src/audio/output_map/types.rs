//! Per-device output settings types: level, mute and channel map.

use serde::{Deserialize, Serialize};

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
pub(super) fn level_gain(level_db: f32) -> f32 {
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
pub(super) fn channel_weights(
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
