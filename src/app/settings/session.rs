use super::path_pref::{default_path_name, sanitize_paths, PathPref};
use super::preferences::{
    ExtraOutputPref, DEFAULT_BRIDGE_BUFFER_MS, DEFAULT_BUFFER, DEFAULT_INPUT_GAIN,
    DEFAULT_OUTPUT_GAIN, DEFAULT_SAMPLE_RATE,
};
use super::serde_util::{placeholder_unreadable, skip_unreadable};
use crate::audio::{EqSettings, GateSettings, OutputDeviceSettings, PluginPref, WasapiOutputMode};

/// Every field defaults, so a session from another version still loads.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct Session {
    pub(crate) name: String,
    pub(crate) note: String,
    /// A name-only save leaves the id and backend empty.
    pub(crate) input: String,
    pub(crate) input_id: String,
    pub(crate) input_backend: String,
    /// A name-only save leaves the id and backend empty.
    pub(crate) output: String,
    pub(crate) output_id: String,
    pub(crate) output_backend: String,
    pub(crate) rate: u32,
    pub(crate) buffer: usize,
    /// 0 means follow the input.
    pub(crate) output_rate: u32,
    pub(crate) output_buffer: usize,
    /// Path 1 from a session with no `paths`.  None keeps the current EQ.
    #[serde(skip_serializing)]
    pub(crate) eq: Option<EqSettings>,
    /// Path 1 from a session with no `paths`.  None keeps the current EQ.
    #[serde(skip_serializing)]
    pub(crate) eq_post: Option<EqSettings>,
    /// Path 1 from a session with no `paths`.  None keeps the current gate.
    #[serde(default, skip_serializing)]
    pub(crate) gate: Option<GateSettings>,
    pub(crate) input_gain: f32,
    pub(crate) output_gain: f32,
    pub(crate) input_channels: [bool; 2],
    /// Path 1 from a session with no `paths`.  Read only.
    #[serde(skip_serializing)]
    pub(crate) output_channels: [bool; 2],
    /// Path 1 from a session with no `paths`.  Read only.
    #[serde(skip_serializing)]
    pub(crate) mono: bool,
    pub(crate) mute: bool,
    pub(crate) drift: bool,
    /// On Windows [`Self::wasapi_output_mode`] is the choice.
    pub(crate) low_latency_output: bool,
    /// Missing on an older session: derived from [`Self::low_latency_output`].
    #[serde(default)]
    pub(crate) wasapi_output_mode: Option<WasapiOutputMode>,
    /// Path 1 from a session with no `paths`.  Read only.
    #[serde(skip_serializing)]
    pub(crate) routes: [[bool; 4]; 2],
    /// Bridge buffer: 0 Auto, 1 Minimum, 2 Fixed (size in `bridge_buffer_ms`).
    #[serde(rename = "cushion_mode")]
    pub(crate) bridge_buffer_mode: u8,
    #[serde(rename = "cushion_ms")]
    pub(crate) bridge_buffer_ms: f32,
    /// Path 1's chain from a session with no `paths`.  Read only.
    #[serde(default, deserialize_with = "skip_unreadable", skip_serializing)]
    pub(crate) plugins: Vec<PluginPref>,
    /// Missing on an older session: the reference matches the target.
    #[serde(default, skip_serializing_if = "crate::app::match_paths::is_default")]
    pub(crate) match_paths: crate::app::match_paths::MatchPathsSettings,
    /// A session with none becomes path 1 from its mix fields.  An
    /// unreadable path keeps its slot, so the session is not lost.
    #[serde(deserialize_with = "placeholder_unreadable")]
    pub(crate) paths: Vec<PathPref>,
    /// Not an `Option`: an older session loads today's output and applying
    /// it resets the routing on purpose.  The click mask is stored and
    /// never applied.
    #[serde(rename = "clock_device")]
    pub(crate) clock_output: OutputDeviceSettings,
    /// An older session has none, and applying it clears the list on
    /// purpose, as with [`Self::clock_output`].
    #[serde(default, deserialize_with = "skip_unreadable", rename = "followers")]
    pub(crate) extra_outputs: Vec<ExtraOutputPref>,
}

impl Default for Session {
    /// Empty device names match nothing, so the current device stays.
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
            match_paths: crate::app::match_paths::MatchPathsSettings::default(),
            paths: Vec::new(),
            clock_output: OutputDeviceSettings::default(),
            extra_outputs: Vec::new(),
        }
    }
}

impl Session {
    /// An EQ or gate the session never stored comes from `current`.
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
            nam_blocks: current.nam_blocks.clone(),
            amp: current.amp.clone(),
            cab: current.cab.clone(),
            plugins: std::mem::take(&mut self.plugins),
            chain_bypass: false,
            level_db: 0.0,
            match_db: 0.0,
            mute: false,
            solo: false,
            feed: crate::audio::PathFeed::Input,
            sends: Vec::new(),
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

/// The first of a name is the newest.  Older saves collapse to one entry.
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
