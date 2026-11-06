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

use serde::{Deserialize, Serialize};

/// Most device channels a [`ChannelMap`] can address.
pub const MAX_OUTPUT_CHANNELS: usize = 8;

/// Device level range, dB.  The same as the master output fader for now.
pub const DEVICE_LEVEL_MIN_DB: f32 = -36.0;
pub const DEVICE_LEVEL_MAX_DB: f32 = 6.0;

/// One side of the stereo mix.
#[cfg_attr(not(test), allow(dead_code))]
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

// Read by the engine and the routing page from step 1b on.
#[cfg_attr(not(test), allow(dead_code))]
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
