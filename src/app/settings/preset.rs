//! A saved patch and helpers that operate on patches.

use super::path_pref::{default_path_name, sanitize_paths, PathPref};
use super::preferences::{
    FollowerPref, DEFAULT_BUFFER, DEFAULT_CUSHION_MS, DEFAULT_INPUT_GAIN, DEFAULT_OUTPUT_GAIN,
    DEFAULT_SAMPLE_RATE,
};
use super::serde_util::skip_unreadable;
use crate::audio::{EqSettings, GateSettings, OutputDeviceSettings, PluginPref, WasapiOutputMode};

/// A saved patch.  Every field has a default, so a patch saved by an older or
/// newer version (or with a field missing) still loads.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct Preset {
    pub(crate) name: String,
    pub(crate) note: String,
    /// Display name of the input. Patches that only stored a name leave
    /// [`Self::input_id`] and [`Self::input_backend`] empty.
    pub(crate) input: String,
    /// [`crate::audio::DeviceItem::id`] of the input.
    pub(crate) input_id: String,
    /// [`crate::audio::DeviceItem::backend`] stored with [`Self::input_id`].
    pub(crate) input_backend: String,
    /// Display name of the output. Same fallback rule as [`Self::input`].
    pub(crate) output: String,
    /// [`crate::audio::DeviceItem::id`] of the output.
    pub(crate) output_id: String,
    /// [`crate::audio::DeviceItem::backend`] stored with [`Self::output_id`].
    pub(crate) output_backend: String,
    pub(crate) rate: u32,
    pub(crate) buffer: usize,
    /// Output side; 0 in patches saved before the sides were separate means
    /// "follow the input".
    pub(crate) output_rate: u32,
    pub(crate) output_buffer: usize,
    /// Path 1's EQs in patches from before paths existed (pre and post
    /// inserts).  Patches from before an EQ existed have none and keep the
    /// current one.  Read, never written (see [`Self::paths`]).
    #[serde(skip_serializing)]
    pub(crate) eq: Option<EqSettings>,
    #[serde(skip_serializing)]
    pub(crate) eq_post: Option<EqSettings>,
    /// Path 1's gate in patches from before paths.  Older patches have none
    /// and keep the current one.
    #[serde(default, skip_serializing)]
    pub(crate) gate: Option<GateSettings>,
    /// Input fader, dB. Missing on older patches; the default is 0 dB.
    pub(crate) input_gain: f32,
    /// Output fader, dB. Missing on older patches; the default is −1.5 dB.
    pub(crate) output_gain: f32,
    pub(crate) input_channels: [bool; 2],
    /// Path 1's channel enable in patches from before paths.  Read only.
    #[serde(skip_serializing)]
    pub(crate) output_channels: [bool; 2],
    /// Path 1's mono in patches from before paths.  Read only.
    #[serde(skip_serializing)]
    pub(crate) mono: bool,
    /// The master mute.
    pub(crate) mute: bool,
    pub(crate) monitor: bool,
    pub(crate) drift: bool,
    /// Linux low-latency / legacy Windows REAL-style switch.
    pub(crate) low_latency_output: bool,
    /// Windows WASAPI clock mode. Missing on older patches → derived from
    /// [`Self::low_latency_output`].
    #[serde(default)]
    pub(crate) wasapi_output_mode: Option<WasapiOutputMode>,
    /// Path 1's input routing in patches from before paths.  Read only.
    #[serde(skip_serializing)]
    pub(crate) routes: [[bool; 4]; 2],
    /// Cushion: 0 Safe, 1 Low latency, 2 Manual (size in `cushion_ms`).
    pub(crate) cushion_mode: u8,
    pub(crate) cushion_ms: f32,
    /// Path 1's insert chain in patches from before paths.  An entry that
    /// cannot be read is skipped.  Read only.
    #[serde(default, deserialize_with = "skip_unreadable", skip_serializing)]
    pub(crate) plugins: Vec<PluginPref>,
    /// The signal paths.  Patches from before paths have none; their
    /// top-level mix fields become path 1 ([`Self::migrate_paths`]).
    pub(crate) paths: Vec<PathPref>,
    /// Clock device level, mute and channel map.  Older patches have none,
    /// which loads as today's output (L → 1, R → 2, mono copy on the rest).
    /// Applying such a patch resets it on purpose: a patch must restore the
    /// routing, so this is not an `Option` like the EQs and the gate.
    pub(crate) clock_device: OutputDeviceSettings,
    /// Extra output devices.  An entry that cannot be read is skipped.
    /// Older patches have none; applying one clears the list on purpose,
    /// for the same reason as [`Self::clock_device`].
    #[serde(default, deserialize_with = "skip_unreadable")]
    pub(crate) followers: Vec<FollowerPref>,
}

impl Default for Preset {
    /// What a saved patch falls back to field by field.  Empty device names
    /// match no device, so loading such a patch keeps the current one.
    fn default() -> Self {
        Self {
            name: "Untitled patch".to_owned(),
            note: String::new(),
            input: String::new(),
            input_id: String::new(),
            input_backend: String::new(),
            output: String::new(),
            output_id: String::new(),
            output_backend: String::new(),
            rate: DEFAULT_SAMPLE_RATE,
            buffer: DEFAULT_BUFFER,
            output_rate: 0,
            output_buffer: 0,
            eq: None,
            eq_post: None,
            gate: None,
            input_gain: DEFAULT_INPUT_GAIN,
            output_gain: DEFAULT_OUTPUT_GAIN,
            input_channels: [true, true],
            output_channels: [true, true],
            mono: false,
            mute: false,
            monitor: true,
            drift: true,
            low_latency_output: true,
            wasapi_output_mode: Some(WasapiOutputMode::RealStyle),
            routes: [[true, false, false, false], [false, true, false, false]],
            cushion_mode: 0,
            cushion_ms: DEFAULT_CUSHION_MS,
            plugins: Vec::new(),
            paths: Vec::new(),
            clock_device: OutputDeviceSettings::default(),
            followers: Vec::new(),
        }
    }
}

impl Preset {
    /// Moves a patch from before paths onto `paths`: its top-level mix
    /// fields become path 1.  What such a patch did not store (an EQ or
    /// gate from before they existed) is taken from `current`, so applying
    /// it keeps that setting, as it always did.  A patch that has paths is
    /// left alone.
    pub(crate) fn migrate_paths(&mut self, current: &PathPref) {
        if !self.paths.is_empty() {
            return;
        }
        let legacy = Self::default();
        let mut paths = vec![PathPref {
            name: default_path_name(0),
            colour: 0,
            routes: self.routes,
            mono: self.mono,
            output_channels: self.output_channels,
            eq: self.eq.unwrap_or(current.eq),
            eq_post: self.eq_post.unwrap_or(current.eq_post),
            gate: self.gate.unwrap_or(current.gate),
            plugins: std::mem::take(&mut self.plugins),
            level_db: 0.0,
            mute: false,
            solo: false,
        }];
        sanitize_paths(&mut paths);
        self.paths = paths;
        self.routes = legacy.routes;
        self.mono = legacy.mono;
        self.output_channels = legacy.output_channels;
        self.eq = None;
        self.eq_post = None;
        self.gate = None;
    }
}

/// Keeps the first (newest) patch for each name, so older saves made before
/// names were matched collapse into one entry.
pub(crate) fn unique_presets(presets: &[Preset]) -> Vec<Preset> {
    let mut unique: Vec<Preset> = Vec::new();
    for preset in presets {
        let name = preset.name.trim();
        if !unique
            .iter()
            .any(|kept| kept.name.trim().eq_ignore_ascii_case(name))
        {
            unique.push(preset.clone());
        }
    }
    unique
}
