//! A saved session and helpers that operate on sessions.

use super::path_pref::{default_path_name, sanitize_paths, PathPref};
use super::preferences::{
    ExtraOutputPref, DEFAULT_BRIDGE_BUFFER_MS, DEFAULT_BUFFER, DEFAULT_INPUT_GAIN,
    DEFAULT_OUTPUT_GAIN, DEFAULT_SAMPLE_RATE,
};
use super::serde_util::skip_unreadable;
use crate::audio::{EqSettings, GateSettings, OutputDeviceSettings, PluginPref, WasapiOutputMode};

/// A saved session.  Every field has a default, so a session saved by an older or
/// newer version (or with a field missing) still loads.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct Session {
    pub(crate) name: String,
    pub(crate) note: String,
    /// Display name of the input. Sessions that only stored a name leave
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
    /// Output side; 0 in sessions saved before the sides were separate means
    /// "follow the input".
    pub(crate) output_rate: u32,
    pub(crate) output_buffer: usize,
    /// Path 1's EQs in sessions from before paths existed (pre and post
    /// effects).  Sessions from before an EQ existed have none and keep the
    /// current one.  Read, never written (see [`Self::paths`]).
    #[serde(skip_serializing)]
    pub(crate) eq: Option<EqSettings>,
    #[serde(skip_serializing)]
    pub(crate) eq_post: Option<EqSettings>,
    /// Path 1's gate in sessions from before paths.  Older sessions have none
    /// and keep the current one.
    #[serde(default, skip_serializing)]
    pub(crate) gate: Option<GateSettings>,
    /// Input fader, dB. Missing on older sessions; the default is 0 dB.
    pub(crate) input_gain: f32,
    /// Output fader, dB. Missing on older sessions; the default is −1.5 dB.
    pub(crate) output_gain: f32,
    pub(crate) input_channels: [bool; 2],
    /// Path 1's channel enable in sessions from before paths.  Read only.
    #[serde(skip_serializing)]
    pub(crate) output_channels: [bool; 2],
    /// Path 1's mono in sessions from before paths.  Read only.
    #[serde(skip_serializing)]
    pub(crate) mono: bool,
    /// The master mute.
    pub(crate) mute: bool,
    pub(crate) drift: bool,
    /// Linux low-latency / legacy Windows REAL-style switch.
    pub(crate) low_latency_output: bool,
    /// Windows WASAPI clock mode. Missing on older sessions → derived from
    /// [`Self::low_latency_output`].
    #[serde(default)]
    pub(crate) wasapi_output_mode: Option<WasapiOutputMode>,
    /// Path 1's input routing in sessions from before paths.  Read only.
    #[serde(skip_serializing)]
    pub(crate) routes: [[bool; 4]; 2],
    /// Bridge buffer: 0 Auto, 1 Minimum, 2 Fixed (size in `bridge_buffer_ms`).
    #[serde(rename = "cushion_mode")]
    pub(crate) bridge_buffer_mode: u8,
    #[serde(rename = "cushion_ms")]
    pub(crate) bridge_buffer_ms: f32,
    /// Path 1's effect chain in sessions from before paths.  An entry that
    /// cannot be read is skipped.  Read only.
    #[serde(default, deserialize_with = "skip_unreadable", skip_serializing)]
    pub(crate) plugins: Vec<PluginPref>,
    /// The signal paths.  Sessions from before paths have none; their
    /// top-level mix fields become path 1 ([`Self::migrate_paths`]).
    pub(crate) paths: Vec<PathPref>,
    /// Clock output level, mute and channel map.  Older sessions have none,
    /// which loads as today's output (L → 1, R → 2, mono copy on the rest).
    /// Applying such a session resets it on purpose: a session must restore the
    /// routing, so this is not an `Option` like the EQs and the gate.  The
    /// click mask is written (the type is the one preferences store) but
    /// never applied: the click is not part of a session.  Loading one gives
    /// each device the mask that physical device has now, as the clock or
    /// an extra output (matched by id and backend), or none if it is not live.
    #[serde(rename = "clock_device")]
    pub(crate) clock_output: OutputDeviceSettings,
    /// Extra output devices.  An entry that cannot be read is skipped.
    /// Older sessions have none; applying one clears the list on purpose,
    /// for the same reason as [`Self::clock_output`].  Their click masks
    /// are handled the same way.
    #[serde(default, deserialize_with = "skip_unreadable", rename = "followers")]
    pub(crate) extra_outputs: Vec<ExtraOutputPref>,
}

impl Default for Session {
    /// What a saved session falls back to field by field.  Empty device names
    /// match no device, so loading such a session keeps the current one.
    fn default() -> Self {
        Self {
            name: "Untitled session".to_owned(),
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
            drift: true,
            low_latency_output: true,
            wasapi_output_mode: Some(WasapiOutputMode::RealStyle),
            routes: [[true, false, false, false], [false, true, false, false]],
            bridge_buffer_mode: 0,
            bridge_buffer_ms: DEFAULT_BRIDGE_BUFFER_MS,
            plugins: Vec::new(),
            paths: Vec::new(),
            clock_output: OutputDeviceSettings::default(),
            extra_outputs: Vec::new(),
        }
    }
}

impl Session {
    /// Moves a session from before paths onto `paths`: its top-level mix
    /// fields become path 1.  What such a session did not store (an EQ or
    /// gate from before they existed) is taken from `current`, so applying
    /// it keeps that setting, as it always did.  A session that has paths is
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
            amp: current.amp.clone(),
            cab: current.cab.clone(),
            plugins: std::mem::take(&mut self.plugins),
            chain_bypass: false,
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

/// Keeps the first (newest) session for each name, so older saves made before
/// names were matched collapse into one entry.
pub(crate) fn unique_sessions(sessions: &[Session]) -> Vec<Session> {
    let mut unique: Vec<Session> = Vec::new();
    for session in sessions {
        let name = session.name.trim();
        if !unique
            .iter()
            .any(|kept| kept.name.trim().eq_ignore_ascii_case(name))
        {
            unique.push(session.clone());
        }
    }
    unique
}
