//! A bare `map` loads as path 0.  A routing row that does not parse keeps
//! its slot, silent, so later paths do not move.

use serde::{Deserialize, Serialize};

use super::super::path::{path_level_gain, sanitize_path_level, MAX_PATHS, PATH_LEVEL_MIN_DB};
use super::super::serde_util::read_or_placeholder;
use super::align::{plan_alignment, trim_frames, AlignmentReport};
use super::weights::{level_gain, path_channel_weights, routed_paths};

pub const MAX_OUTPUT_CHANNELS: usize = 8;

/// Extra saved entries are dropped.  One stereo pair is preallocated
/// per slot so add and remove stay live.
pub const MAX_EXTRA_OUTPUTS: usize = 4;

/// Display name and PipeWire `node.name` of the virtual output.
pub const VIRTUAL_OUTPUT_NAME: &str = "RigWerk Out";

pub const VIRTUAL_OUTPUT_ID: &str = "pipewire:RigWerk Out";

pub const DEVICE_LEVEL_MIN_DB: f32 = -36.0;
pub const DEVICE_LEVEL_MAX_DB: f32 = 6.0;

/// -14 is the streaming target and -23 the broadcast one.  A NaN
/// target becomes the default.
pub const LOUDNESS_TARGET_MIN_LUFS: f32 = -30.0;
pub const LOUDNESS_TARGET_MAX_LUFS: f32 = -5.0;
pub const LOUDNESS_TARGET_DEFAULT_LUFS: f32 = -14.0;

/// How far an output trim may move a device from its own level, dB.
pub const LOUDNESS_TRIM_MAX_DB: f32 = 12.0;

/// The trim ramps in the mapper.  Nothing follows the player.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LoudnessSettings {
    pub target_lufs: f32,
    /// Left out of a save while 0.
    #[serde(alias = "learned_db", default, skip_serializing_if = "trim_db_is_zero")]
    pub trim_db: f32,
}

fn trim_db_is_zero(value: &f32) -> bool {
    *value == 0.0
}

impl Default for LoudnessSettings {
    fn default() -> Self {
        Self {
            target_lufs: LOUDNESS_TARGET_DEFAULT_LUFS,
            trim_db: 0.0,
        }
    }
}

impl LoudnessSettings {
    /// So a save can leave the field out.
    fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// A NaN target becomes the default.  A NaN trim becomes 0.
    pub fn sanitized(self) -> Self {
        let target = if self.target_lufs.is_nan() {
            LOUDNESS_TARGET_DEFAULT_LUFS
        } else {
            self.target_lufs
                .clamp(LOUDNESS_TARGET_MIN_LUFS, LOUDNESS_TARGET_MAX_LUFS)
        };
        let trim_db = if self.trim_db.is_nan() {
            0.0
        } else {
            self.trim_db
                .clamp(-LOUDNESS_TRIM_MAX_DB, LOUDNESS_TRIM_MAX_DB)
        };
        Self {
            target_lufs: target,
            trim_db,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MixSide {
    L,
    R,
}

/// Bit `n` is device channel `n + 1`.
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

    pub fn is_empty(self) -> bool {
        self.left == 0 && self.right == 0
    }

    /// A 1-channel device is reached by both sides unless the map is
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

    /// Bits for channels the device does not have are kept.
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
pub(crate) fn low_bits(channels: usize) -> u8 {
    if channels >= MAX_OUTPUT_CHANNELS {
        u8::MAX
    } else {
        (1u8 << channels) - 1
    }
}

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
    /// Pair presets only where the pairs exist.  A 1-channel device,
    /// which already plays the average, offers Mono to all and Clear.
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
    /// An unknown stored mode reads as this.
    #[default]
    #[serde(other)]
    MonoCopy,
}

pub const PATH_TRIM_MAX_MS: f32 = 5.0;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PathMapSettings {
    pub map: ChannelMap,
    pub level_db: f32,
    pub invert: bool,
    /// Extra delay, or less when negative, on top of the alignment.
    pub trim_ms: f32,
    /// Set while a saved row could not be parsed, so the load can count
    /// it.  Never saved.
    #[serde(skip)]
    pub(crate) unreadable: bool,
}

impl Default for PathMapSettings {
    fn default() -> Self {
        Self {
            map: ChannelMap::default(),
            level_db: 0.0,
            invert: false,
            trim_ms: 0.0,
            unreadable: false,
        }
    }
}

impl PathMapSettings {
    /// `map` as the first path's entry, defaults for the others.
    #[cfg(test)]
    pub fn first(map: ChannelMap) -> [Self; MAX_PATHS] {
        let mut entries = [Self::default(); MAX_PATHS];
        entries[0].map = map;
        entries
    }

    /// What an added (empty) path gets on every device: nothing routed.
    pub fn unrouted() -> Self {
        Self {
            map: ChannelMap { left: 0, right: 0 },
            ..Self::default()
        }
    }

    /// Silent even where unused channels play the mono copy.
    pub fn silent() -> Self {
        Self {
            level_db: PATH_LEVEL_MIN_DB,
            ..Self::unrouted()
        }
    }

    pub fn sanitized(self) -> Self {
        Self {
            level_db: sanitize_path_level(self.level_db),
            trim_ms: if self.trim_ms.is_nan() {
                0.0
            } else {
                self.trim_ms.clamp(-PATH_TRIM_MAX_MS, PATH_TRIM_MAX_MS)
            },
            ..self
        }
    }

    /// The trim in whole frames at `rate` (rounded).
    pub(crate) fn trim_frames(&self, rate: u32) -> i32 {
        trim_frames(self.trim_ms, rate)
    }

    /// 0 dB and normal polarity is exactly 1.
    pub(crate) fn gain(&self) -> f32 {
        let gain = path_level_gain(self.level_db);
        if self.invert {
            -gain
        } else {
            gain
        }
    }
}

/// A bare `map` loads as path 0.  A row that does not parse loads
/// silent in its own slot, so no other path's map moves.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(from = "StoredDeviceSettings", into = "StoredDeviceSettings")]
pub struct OutputDeviceSettings {
    pub level_db: f32,
    pub mute: bool,
    pub unused: UnusedChannels,
    pub path_map: [PathMapSettings; MAX_PATHS],
    /// Off, only the trims apply.  On when a save omits it.
    pub align: bool,
    /// Bit `n` is channel `n`.  0 adds the tick nowhere.
    pub click_mask: u8,
    /// Automatic loudness: target and switch.  Off until the output sheet
    /// turns it on.
    pub loudness: LoudnessSettings,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct StoredDeviceSettings {
    level_db: f32,
    mute: bool,
    /// Legacy single map.  Read, never written.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_map"
    )]
    map: Option<ChannelMap>,
    unused: UnusedChannels,
    /// A row that does not parse keeps its position.
    #[serde(default, deserialize_with = "stored_path_map")]
    path_map: Vec<PathMapSettings>,
    align: bool,
    /// Omitted saves play the click nowhere.
    #[serde(default)]
    click_mask: u8,
    /// Omitted saves are off at -14 LUFS.
    #[serde(default, skip_serializing_if = "LoudnessSettings::is_default")]
    loudness: LoudnessSettings,
}

/// A legacy `map` is stored bare (not as `Some(..)`); reads it as present.
fn present_map<'de, D>(deserializer: D) -> Result<Option<ChannelMap>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    ChannelMap::deserialize(deserializer).map(Some)
}

/// Silent, so the unused-channel fallback cannot play it.  Flagged so
/// the load can count the row.
fn unreadable_row() -> PathMapSettings {
    PathMapSettings {
        unreadable: true,
        ..PathMapSettings::silent()
    }
}

/// A row that does not parse keeps its slot.  Dropping it would move
/// every later path's map.
fn stored_path_map<'de, D>(deserializer: D) -> Result<Vec<PathMapSettings>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    read_or_placeholder(deserializer, unreadable_row, unreadable_row)
}

impl Default for StoredDeviceSettings {
    fn default() -> Self {
        Self::from(OutputDeviceSettings::default())
    }
}

impl From<StoredDeviceSettings> for OutputDeviceSettings {
    fn from(stored: StoredDeviceSettings) -> Self {
        let mut path_map = [PathMapSettings::default(); MAX_PATHS];
        if let Some(map) = stored.map {
            path_map[0].map = map;
        }
        for (slot, saved) in path_map.iter_mut().zip(stored.path_map) {
            *slot = saved;
        }
        Self {
            level_db: stored.level_db,
            mute: stored.mute,
            unused: stored.unused,
            path_map,
            align: stored.align,
            click_mask: stored.click_mask,
            loudness: stored.loudness,
        }
    }
}

impl From<OutputDeviceSettings> for StoredDeviceSettings {
    fn from(settings: OutputDeviceSettings) -> Self {
        let used = settings
            .path_map
            .iter()
            .rposition(|entry| *entry != PathMapSettings::default())
            .map_or(0, |last| last + 1);
        Self {
            level_db: settings.level_db,
            mute: settings.mute,
            map: None,
            unused: settings.unused,
            path_map: settings.path_map[..used].to_vec(),
            align: settings.align,
            click_mask: settings.click_mask,
            loudness: settings.loudness,
        }
    }
}

impl Default for OutputDeviceSettings {
    fn default() -> Self {
        Self {
            level_db: 0.0,
            mute: false,
            unused: UnusedChannels::default(),
            path_map: [PathMapSettings::default(); MAX_PATHS],
            align: true,
            click_mask: 0,
            loudness: LoudnessSettings::default(),
        }
    }
}

impl OutputDeviceSettings {
    pub fn map(&self, slot: usize) -> ChannelMap {
        self.path_map[slot.min(MAX_PATHS - 1)].map
    }

    pub fn sanitized(self) -> Self {
        let level_db = if self.level_db.is_nan() {
            0.0
        } else {
            self.level_db
        };
        Self {
            level_db: level_db.clamp(DEVICE_LEVEL_MIN_DB, DEVICE_LEVEL_MAX_DB),
            path_map: self.path_map.map(PathMapSettings::sanitized),
            loudness: self.loudness.sanitized(),
            ..self
        }
    }

    /// This device with the paths reordered for saving: entry `n` is slot
    /// `order[n]`'s, the rest are defaults.
    pub fn with_path_order(&self, order: &[usize]) -> Self {
        let mut path_map = [PathMapSettings::default(); MAX_PATHS];
        for (to, &slot) in path_map.iter_mut().zip(order) {
            *to = self.path_map[slot.min(MAX_PATHS - 1)];
        }
        Self { path_map, ..*self }
    }

    /// Routing decides, not what is audible, so muted paths count.
    pub(crate) fn routed_paths(
        &self,
        live: [bool; MAX_PATHS],
        channels: usize,
    ) -> [bool; MAX_PATHS] {
        let maps = self.path_map.map(|entry| entry.map);
        routed_paths(&maps, live, self.unused, channels)
    }

    /// For the UI when no bridge runs.  The mixer publishes its own.
    pub(crate) fn alignment(
        &self,
        live: [bool; MAX_PATHS],
        channels: usize,
        latency: [u32; MAX_PATHS],
        rate: u32,
    ) -> AlignmentReport {
        let routed = self.routed_paths(live, channels);
        let trims = self.path_map.map(|entry| entry.trim_frames(rate));
        AlignmentReport::new(
            plan_alignment(self.align, routed, latency, trims),
            self.align,
            latency,
            rate,
        )
    }

    /// Which of the `live` paths sound on device channel `channel` (by
    /// their own map, or through the mono copy on an unreached channel).
    pub(crate) fn paths_on_channel(
        &self,
        live: [bool; MAX_PATHS],
        channels: usize,
        channel: usize,
    ) -> [bool; MAX_PATHS] {
        let maps = self.path_map.map(|entry| entry.map);
        std::array::from_fn(|path| {
            path_channel_weights(&maps, live, self.unused, channels, channel, path) != [0.0; 3]
        })
    }

    /// For the UI preview.  The callback uses
    /// [`DeviceMapper`](super::DeviceMapper).
    pub(crate) fn settled_sum(
        &self,
        live: [bool; MAX_PATHS],
        path_gains: [f32; MAX_PATHS],
        channels: usize,
        left: f32,
        right: f32,
    ) -> [f32; MAX_OUTPUT_CHANNELS] {
        let device_gain = if self.mute {
            0.0
        } else {
            level_gain(self.level_db)
        };
        let maps = self.path_map.map(|entry| entry.map);
        let sources = [left, right, (left + right) * 0.5];
        let mut frame = [0.0; MAX_OUTPUT_CHANNELS];
        for (channel, sample) in frame.iter_mut().enumerate().take(channels) {
            let mut sum = 0.0;
            for path in (0..MAX_PATHS).filter(|&path| live[path]) {
                let weights =
                    path_channel_weights(&maps, live, self.unused, channels, channel, path);
                let mixed: f32 = weights
                    .iter()
                    .zip(sources)
                    .map(|(weight, source)| weight * source)
                    .sum();
                sum += mixed * path_gains[path] * self.path_map[path].gain();
            }
            *sample = device_gain * sum;
        }
        frame
    }
}
