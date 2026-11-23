//! Per-device output settings types: level, mute and channel map.

use serde::{Deserialize, Serialize};

use super::super::path::{path_level_gain, sanitize_path_level, MAX_PATHS};
use super::align::{plan_alignment, trim_frames, AlignmentReport};
use super::weights::{level_gain, path_channel_weights, routed_paths};

/// Most device channels a [`ChannelMap`] can address.
pub const MAX_OUTPUT_CHANNELS: usize = 8;

/// Most extra outputs beside the clock output.  Extra entries in a
/// save are dropped on load.  PipeWire preallocates one stereo port pair per
/// slot so add/remove stays live.
pub const MAX_EXTRA_OUTPUTS: usize = 4;

/// Display name and PipeWire `node.name` of the virtual output.
pub const VIRTUAL_OUTPUT_NAME: &str = "RigWerk Out";

/// Session id of the virtual output (`pipewire:RigWerk Out`).
pub const VIRTUAL_OUTPUT_ID: &str = "pipewire:RigWerk Out";

/// Device level range, dB.  The same as the master output fader for now.
pub const DEVICE_LEVEL_MIN_DB: f32 = -36.0;
pub const DEVICE_LEVEL_MAX_DB: f32 = 6.0;

/// Loudness target range, LUFS.  -14 is the streaming target; -23 is the
/// broadcast one.  A NaN target becomes the default.
pub const LOUDNESS_TARGET_MIN_LUFS: f32 = -30.0;
pub const LOUDNESS_TARGET_MAX_LUFS: f32 = -5.0;
pub const LOUDNESS_TARGET_DEFAULT_LUFS: f32 = -14.0;

/// How far a learned trim or a path match may move a device from its own
/// level, dB.
pub const LOUDNESS_TRIM_MAX_DB: f32 = 12.0;

/// One output's loudness setting: the target Learn measures against and the
/// trim a past Learn saved.  The trim is applied through the mapper's ramp;
/// nothing follows the player.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LoudnessSettings {
    /// Target integrated loudness, LUFS.
    pub target_lufs: f32,
    /// Learned output trim, dB.  Left out of a save while 0.
    #[serde(default, skip_serializing_if = "learned_db_is_zero")]
    pub learned_db: f32,
}

fn learned_db_is_zero(value: &f32) -> bool {
    *value == 0.0
}

impl Default for LoudnessSettings {
    fn default() -> Self {
        Self {
            target_lufs: LOUDNESS_TARGET_DEFAULT_LUFS,
            learned_db: 0.0,
        }
    }
}

impl LoudnessSettings {
    /// Whether this is the default, so a save can leave the field out and
    /// keep an older file's bytes unchanged.
    fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// A NaN target becomes the default; the target is clamped to the range
    /// the sheet offers.  A NaN learned trim becomes 0 and is clamped to
    /// ±[`LOUDNESS_TRIM_MAX_DB`].
    pub fn sanitized(self) -> Self {
        let target = if self.target_lufs.is_nan() {
            LOUDNESS_TARGET_DEFAULT_LUFS
        } else {
            self.target_lufs
                .clamp(LOUDNESS_TARGET_MIN_LUFS, LOUDNESS_TARGET_MAX_LUFS)
        };
        let learned_db = if self.learned_db.is_nan() {
            0.0
        } else {
            self.learned_db
                .clamp(-LOUDNESS_TRIM_MAX_DB, LOUDNESS_TRIM_MAX_DB)
        };
        Self {
            target_lufs: target,
            learned_db,
        }
    }
}

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
pub(crate) fn low_bits(channels: usize) -> u8 {
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

/// The alignment trim range, ms.
pub const PATH_TRIM_MAX_MS: f32 = 5.0;

/// Range of the loudness-match trim on one path and device, dB.
pub const PATH_MATCH_MAX_DB: f32 = 24.0;

/// One signal path's place in one device's sum: its channel map, its level
/// there, its polarity, and the alignment trim on top of the automatic
/// alignment.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PathMapSettings {
    pub map: ChannelMap,
    /// The path's level on this device, dB.
    pub level_db: f32,
    /// Polarity flip.
    pub invert: bool,
    /// Alignment trim, ms (±[`PATH_TRIM_MAX_MS`]): extra delay (positive)
    /// or less delay (negative) for this path on this device, on top of
    /// the automatic alignment.
    pub trim_ms: f32,
    /// Loudness-match trim, dB (±[`PATH_MATCH_MAX_DB`]), on top of the
    /// level and kept apart from it.
    #[serde(default, skip_serializing_if = "match_db_is_zero")]
    pub match_db: f32,
}

fn match_db_is_zero(value: &f32) -> bool {
    *value == 0.0
}

impl Default for PathMapSettings {
    fn default() -> Self {
        Self {
            map: ChannelMap::default(),
            level_db: 0.0,
            invert: false,
            trim_ms: 0.0,
            match_db: 0.0,
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

    pub fn sanitized(self) -> Self {
        Self {
            level_db: sanitize_path_level(self.level_db),
            trim_ms: if self.trim_ms.is_nan() {
                0.0
            } else {
                self.trim_ms.clamp(-PATH_TRIM_MAX_MS, PATH_TRIM_MAX_MS)
            },
            match_db: if self.match_db.is_nan() {
                0.0
            } else {
                self.match_db.clamp(-PATH_MATCH_MAX_DB, PATH_MATCH_MAX_DB)
            },
            ..self
        }
    }

    /// The trim in whole frames at `rate` (rounded).
    pub(crate) fn trim_frames(&self, rate: u32) -> i32 {
        trim_frames(self.trim_ms, rate)
    }

    /// The signed linear gain the device applies to this path: the level,
    /// negated when the polarity is flipped.  0 dB and normal polarity is
    /// exactly 1.0.
    pub(crate) fn gain(&self) -> f32 {
        let mut gain = path_level_gain(self.level_db);
        if self.match_db != 0.0 {
            gain *= 10.0f32.powf(self.match_db / 20.0);
        }
        if self.invert {
            -gain
        } else {
            gain
        }
    }
}

/// Level, mute, click mask and channel maps of one output device, applied
/// after the master.  Missing fields read as the defaults (0 dB, unmuted,
/// no click, L → 1, R → 2 for every path, mono copy on the rest), which is
/// today's output.
///
/// `path_map` is indexed by path slot in memory.  Saved, it is indexed by
/// the path's position in the saved `paths` list (the app compacts it), and
/// trailing default entries are left out.  Files from before paths existed
/// stored one `map`; it loads as the first path's map.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(from = "StoredDeviceSettings", into = "StoredDeviceSettings")]
pub struct OutputDeviceSettings {
    /// Device level, dB.
    pub level_db: f32,
    pub mute: bool,
    pub unused: UnusedChannels,
    /// Each path's map, level, polarity and trim on this device.
    pub path_map: [PathMapSettings; MAX_PATHS],
    /// Latency alignment: every path routed here waits for the slowest one
    /// (see [`plan_alignment`]).  Off, only the trims apply.  On by default,
    /// and in files from before it existed.
    pub align: bool,
    /// Which device channels carry the metronome.  Bit `n` is channel `n`
    /// (the UI's channel `n + 1`).  0 adds the tick nowhere.  Not a path
    /// setting: reordering paths leaves it alone.
    pub click_mask: u8,
    /// Automatic loudness: target and switch.  Off until the output sheet
    /// turns it on.
    pub loudness: LoudnessSettings,
}

/// The saved form of [`OutputDeviceSettings`].
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct StoredDeviceSettings {
    level_db: f32,
    mute: bool,
    /// Path 1's map in files from before paths existed.  Read, never written.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_map"
    )]
    map: Option<ChannelMap>,
    unused: UnusedChannels,
    path_map: Vec<PathMapSettings>,
    align: bool,
    /// Missing on older saves: the click plays nowhere.
    #[serde(default)]
    click_mask: u8,
    /// Missing on older saves: off at -14 LUFS.
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
    /// Path `slot`'s channel map on this device.
    pub fn map(&self, slot: usize) -> ChannelMap {
        self.path_map[slot.min(MAX_PATHS - 1)].map
    }

    /// A NaN level becomes 0 dB; the level is clamped to
    /// [`DEVICE_LEVEL_MIN_DB`]..=[`DEVICE_LEVEL_MAX_DB`].  Each path's level
    /// and trim are clamped too.
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

    /// Which of the `live` paths this device plays on any of its
    /// `channels` channels (by their own map, or through the mono copy).
    /// The routing decides, not what is audible: muted and un-soloed paths
    /// count.  These are the paths the alignment lines up.
    pub(crate) fn routed_paths(
        &self,
        live: [bool; MAX_PATHS],
        channels: usize,
    ) -> [bool; MAX_PATHS] {
        let maps = self.path_map.map(|entry| entry.map);
        routed_paths(&maps, live, self.unused, channels)
    }

    /// The alignment this device gets at `rate` when the `live` paths have
    /// `latency` (chain latency in frames): what the mixer applies, worked
    /// out from the settings.  For the UI when no bridge runs; the mixer
    /// publishes its own (see `AudioEngine::device_alignment`).
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

    /// One settled frame of a `channels`-channel device under these settings
    /// (no ramps): every `live` path plays the same stereo frame, scaled by
    /// `path_gains` (its level, mute and solo) and by its level and polarity
    /// on this device, and the paths add up as in the callback.  Channels
    /// past the device or past [`MAX_OUTPUT_CHANNELS`] read 0.  For the UI's
    /// preview meters; the callback uses [`DeviceMapper`](super::DeviceMapper).
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
