use serde::de::IgnoredAny;
use serde::Deserialize;

use super::path_pref::{default_path_name, sanitize_paths, PathPref};
use super::serde_util::{
    placeholder_unreadable, skip_unreadable, skip_unreadable_option, Placeholder,
};
use super::session::Session;
use crate::app::appearance::{sanitize_ui_size, ThemeChoice, UI_SIZE_DEFAULT};
use crate::app::latency::MeasuredLatency;
use crate::app::plugins::{sanitize_effect_ids, EffectId};
use crate::app::screen::{SHEET_MAX, SHEET_MIN};
use crate::app::Page;
use crate::audio::{
    sanitize_send, BufferMode, DeviceItem, EqSettings, GateSettings, LegacyNamSettings,
    OutputDeviceSettings, PathFeed, PluginPref, WasapiOutputMode, MAX_EXTRA_OUTPUTS, MAX_PATHS,
    SEND_OFF_DB, VIRTUAL_OUTPUT_ID, VIRTUAL_OUTPUT_NAME,
};

/// Layout 1 files have no `paths` and load as one path.
pub(crate) const PREFERENCES_SCHEMA: u32 = 2;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct Preferences {
    /// Missing reads as 0.  A newer file is read and never written over.
    #[serde(default)]
    pub(crate) schema_version: u32,
    pub(crate) auto_start_bridge: bool,
    #[serde(deserialize_with = "skip_unreadable")]
    pub(crate) sessions: Vec<Session>,
    pub(crate) session_name: String,
    pub(crate) page: String,
    /// 0 pre, 1 post.
    pub(crate) eq_edit: u8,
    pub(crate) output_tab_low_latency: bool,
    pub(crate) selected_session: Option<usize>,
    /// The name is what the choice follows: a skipped session moves the
    /// index.  An older save falls back to the index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) selected_session_name: Option<String>,
    /// An empty id and backend are a save from before ids.  The name is
    /// only the display fallback.
    pub(crate) asio_name: Option<String>,
    pub(crate) asio_id: String,
    pub(crate) asio_backend: String,
    pub(crate) input_name: Option<String>,
    pub(crate) input_id: String,
    pub(crate) input_backend: String,
    pub(crate) output_name: Option<String>,
    pub(crate) output_id: String,
    pub(crate) output_backend: String,
    pub(crate) use_asio_input: bool,
    pub(crate) sample_rate: u32,
    pub(crate) buffer_size: usize,
    pub(crate) input_gain: f32,
    pub(crate) output_gain: f32,
    pub(crate) input_channels: [bool; 2],
    /// Path 1 from a file with no `paths`.  Read only.
    #[serde(skip_serializing)]
    pub(crate) output_channels: [bool; 2],
    /// Path 1 from a file with no `paths`.  Read only.
    #[serde(skip_serializing)]
    pub(crate) mono: bool,
    pub(crate) mute: bool,
    pub(crate) drift: bool,
    /// Older saves and Linux round-trip through this.  On Windows
    /// [`Self::wasapi_output_mode`] is the choice.
    pub(crate) low_latency_output: bool,
    /// Missing on an older save: derived from [`Self::low_latency_output`].
    #[serde(default)]
    pub(crate) wasapi_output_mode: Option<WasapiOutputMode>,
    /// Path 1 from a file with no `paths`.  Read only.
    #[serde(skip_serializing)]
    pub(crate) routes: [[bool; 4]; 2],
    /// 0 follows the input.
    pub(crate) output_rate: u32,
    pub(crate) output_buffer: usize,
    /// Bridge buffer: 0 Auto, 1 Minimum, 2 Fixed (size in `bridge_buffer_ms`).
    #[serde(rename = "cushion_mode")]
    pub(crate) bridge_buffer_mode: u8,
    #[serde(rename = "cushion_ms")]
    pub(crate) bridge_buffer_ms: f32,
    /// Path 1's pre-EQ from a file with no `paths`.  A one-EQ save loads here.
    #[serde(skip_serializing)]
    pub(crate) eq: EqSettings,
    /// Path 1 from a file with no `paths`.  Read only.
    #[serde(skip_serializing)]
    pub(crate) eq_post: EqSettings,
    /// Path 1 from a file with no `paths`.  Read only.
    #[serde(default, skip_serializing)]
    pub(crate) gate: GateSettings,
    /// Path 1's chain from a file with no `paths`.  Read only.
    #[serde(deserialize_with = "skip_unreadable", skip_serializing)]
    pub(crate) plugins: Vec<PluginPref>,
    /// A file with none loads its top-level mix fields as path 1.  An
    /// unreadable path, device row or send keeps its slot, so nothing
    /// after it moves.  One past a cap is dropped and not reported.
    #[serde(deserialize_with = "placeholder_unreadable")]
    pub(crate) paths: Vec<PathPref>,
    pub(crate) selected_path: usize,
    /// Missing on an older save: the reference matches the target.
    #[serde(default, skip_serializing_if = "crate::app::match_paths::is_default")]
    pub(crate) match_paths: crate::app::match_paths::MatchPathsSettings,
    #[serde(rename = "clock_device")]
    pub(crate) clock_output: OutputDeviceSettings,
    #[serde(deserialize_with = "skip_unreadable", rename = "followers")]
    pub(crate) extra_outputs: Vec<ExtraOutputPref>,
    #[serde(default, deserialize_with = "skip_unreadable_option")]
    pub(crate) measured_latency: Option<MeasuredLatency>,
    /// An unknown id falls back to chromatic.
    pub(crate) tuner_tuning: String,
    /// A4 in Hz, clamped to 415–466.
    pub(crate) tuner_reference_hz: f32,
    /// 0 sum, 1 left, 2 right.
    pub(crate) tuner_channel: u8,
    /// 0 strobe, 1 needle.
    pub(crate) tuner_display: u8,
    /// 0 single note, 1 all strings.
    pub(crate) tuner_listen: u8,
    pub(crate) tuner_auto_mute: bool,
    /// 0 tight, 1 standard, 2 live.
    pub(crate) tuner_tolerance: u8,
    /// Empty is replaced with the default recordings folder.
    #[serde(default)]
    pub(crate) record_folder: String,
    /// Not written while empty, so an older save stays byte-identical.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) amp_folder: String,
    /// Not written while empty, so an older save stays byte-identical.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) cab_folder: String,
    #[serde(default)]
    pub(crate) record_click: bool,
    /// Not part of a session, and not saved.  A launch starts the
    /// click stopped.
    #[serde(skip)]
    pub(crate) metronome_running: bool,
    pub(crate) metronome_bpm: f32,
    pub(crate) metronome_rhythm: crate::audio::ClickRhythm,
    pub(crate) metronome_level_db: f32,
    #[serde(default)]
    pub(crate) theme: ThemeChoice,
    /// Clamped in [`sanitize_preferences`].
    #[serde(default = "default_ui_size")]
    pub(crate) ui_size: f32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) effect_favourites: Vec<EffectId>,
    /// [`sanitize_preferences`] keeps ten.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) effect_recent: Vec<EffectId>,
    /// 0 means 40% of the console area.
    #[serde(default, skip_serializing_if = "sheet_height_unset")]
    pub(crate) sheet_height: f32,
    #[serde(default, skip_serializing_if = "effect_open_is_editor")]
    pub(crate) effect_double_click: EffectOpen,
    /// Restored when an output's Metronome toggle is switched back on.
    #[serde(default, skip_serializing_if = "click_mask_unused")]
    pub(crate) last_click_mask: u8,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum EffectOpen {
    Controls,
    #[default]
    #[serde(other)]
    Editor,
}

fn effect_open_is_editor(open: &EffectOpen) -> bool {
    *open == EffectOpen::Editor
}

fn click_mask_unused(mask: &u8) -> bool {
    *mask == 0
}

fn sheet_height_unset(height: &f32) -> bool {
    *height == 0.0
}

fn default_ui_size() -> f32 {
    UI_SIZE_DEFAULT
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            schema_version: PREFERENCES_SCHEMA,
            auto_start_bridge: false,
            sessions: Vec::new(),
            session_name: "My guitar rig".to_owned(),
            page: "console".to_owned(),
            eq_edit: 0,
            output_tab_low_latency: cfg!(target_os = "linux"),
            selected_session: None,
            selected_session_name: None,
            asio_name: None,
            asio_id: String::new(),
            asio_backend: String::new(),
            input_name: None,
            input_id: String::new(),
            input_backend: String::new(),
            output_name: None,
            output_id: String::new(),
            output_backend: String::new(),
            use_asio_input: true,
            sample_rate: DEFAULT_SAMPLE_RATE,
            buffer_size: DEFAULT_BUFFER,
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
            output_rate: 0,
            output_buffer: DEFAULT_BUFFER,
            bridge_buffer_mode: 0,
            bridge_buffer_ms: DEFAULT_BRIDGE_BUFFER_MS,
            eq: EqSettings::default(),
            eq_post: EqSettings::default(),
            gate: GateSettings::default(),
            plugins: Vec::new(),
            paths: Vec::new(),
            selected_path: 0,
            match_paths: crate::app::match_paths::MatchPathsSettings::default(),
            clock_output: OutputDeviceSettings::default(),
            extra_outputs: Vec::new(),
            measured_latency: None,
            tuner_tuning: crate::audio::DEFAULT_PRESET_ID.to_owned(),
            tuner_reference_hz: 440.0,
            tuner_channel: 0,
            tuner_display: 0,
            tuner_listen: 0,
            tuner_auto_mute: true,
            tuner_tolerance: 1,
            record_folder: String::new(),
            amp_folder: String::new(),
            cab_folder: String::new(),
            record_click: false,
            metronome_running: false,
            metronome_bpm: crate::audio::METRONOME_BPM_DEFAULT,
            metronome_rhythm: crate::audio::ClickRhythm::FourFour,
            metronome_level_db: crate::audio::CLICK_LEVEL_DEFAULT_DB,
            theme: ThemeChoice::System,
            ui_size: UI_SIZE_DEFAULT,
            effect_favourites: Vec::new(),
            effect_recent: Vec::new(),
            sheet_height: 0.0,
            effect_double_click: EffectOpen::Editor,
            last_click_mask: 0,
        }
    }
}

impl Preferences {
    /// A file with no `paths` becomes one path from its top-level mix fields.
    pub(crate) fn path_prefs(&self) -> Vec<PathPref> {
        if !self.paths.is_empty() {
            return self.paths.clone();
        }
        vec![PathPref {
            name: default_path_name(0),
            colour: 0,
            routes: self.routes,
            mono: self.mono,
            output_channels: self.output_channels,
            eq: self.eq,
            eq_post: self.eq_post,
            gate: self.gate,
            nam_blocks: Vec::new(),
            amp: LegacyNamSettings::default(),
            cab: crate::audio::CabSettings::default(),
            plugins: self.plugins.clone(),
            chain_bypass: false,
            level_db: 0.0,
            match_db: 0.0,
            mute: false,
            solo: false,
            feed: crate::audio::PathFeed::Input,
            sends: Vec::new(),
        }]
    }

    /// Clears the top-level mix fields so nothing reads them by mistake.
    pub(crate) fn migrate_paths(&mut self) {
        self.paths = self.path_prefs();
        let legacy = Self::default();
        self.routes = legacy.routes;
        self.mono = legacy.mono;
        self.output_channels = legacy.output_channels;
        self.eq = legacy.eq;
        self.eq_post = legacy.eq_post;
        self.gate = legacy.gate;
        self.plugins = Vec::new();
    }
}

pub(crate) fn bridge_buffer_from_pref(value: u8) -> BufferMode {
    match value {
        1 => BufferMode::Minimum,
        2 => BufferMode::Fixed,
        _ => BufferMode::Auto,
    }
}

pub(crate) fn bridge_buffer_to_pref(mode: BufferMode) -> u8 {
    match mode {
        BufferMode::Auto => 0,
        BufferMode::Minimum => 1,
        BufferMode::Fixed => 2,
    }
}

/// Never the clock output.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct ExtraOutputPref {
    /// PipeWire RigWerk Out uses [`VIRTUAL_OUTPUT_ID`].  On Windows the
    /// VB-Cable keeps the WASAPI render id.
    pub(crate) id: String,
    pub(crate) backend: String,
    pub(crate) name: String,
    pub(crate) settings: OutputDeviceSettings,
    /// 0 Auto, 1 Minimum, 2 Fixed.  Unused on PipeWire: the graph keeps
    /// them in step.
    #[serde(rename = "cushion_mode")]
    pub(crate) bridge_buffer_mode: u8,
    #[serde(rename = "cushion_ms")]
    pub(crate) bridge_buffer_ms: f32,
    /// RigWerk Out, not a hardware device.
    #[serde(rename = "virtual_out")]
    pub(crate) virtual_output: bool,
    /// Ignored for a hardware extra output.  Missing on an old save is off.
    #[serde(rename = "publish")]
    pub(crate) virtual_output_on: bool,
}

impl Default for ExtraOutputPref {
    fn default() -> Self {
        Self {
            id: String::new(),
            backend: String::new(),
            name: String::new(),
            settings: OutputDeviceSettings::default(),
            bridge_buffer_mode: 0,
            bridge_buffer_ms: DEFAULT_BRIDGE_BUFFER_MS,
            virtual_output: false,
            virtual_output_on: false,
        }
    }
}

impl ExtraOutputPref {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn virtual_output() -> Self {
        Self {
            id: VIRTUAL_OUTPUT_ID.to_owned(),
            backend: "PipeWire".to_owned(),
            name: VIRTUAL_OUTPUT_NAME.to_owned(),
            settings: OutputDeviceSettings::default(),
            bridge_buffer_mode: 1,
            bridge_buffer_ms: DEFAULT_BRIDGE_BUFFER_MS,
            virtual_output: true,
            virtual_output_on: true,
        }
    }

    pub(crate) fn sanitized(mut self) -> Self {
        self.settings = self.settings.sanitized();
        self.bridge_buffer_ms = finite_bridge_buffer(self.bridge_buffer_ms);
        if self.virtual_output {
            #[cfg(target_os = "windows")]
            {
                // Keep the WASAPI render id.  Rewriting it to the PipeWire
                // sentinel would stop the supervisor opening the cable.
                // A sentinel from an older save is cleared; rematch fills
                // the live id.
                if self.id == VIRTUAL_OUTPUT_ID {
                    self.id.clear();
                }
                if self.name.trim().is_empty() {
                    self.name = VIRTUAL_OUTPUT_NAME.to_owned();
                }
                if self.backend.trim().is_empty() {
                    self.backend = "WASAPI".to_owned();
                }
            }
            #[cfg(not(target_os = "windows"))]
            {
                self.id = VIRTUAL_OUTPUT_ID.to_owned();
                self.backend = "PipeWire".to_owned();
                if self.name.trim().is_empty() {
                    self.name = VIRTUAL_OUTPUT_NAME.to_owned();
                }
            }
        }
        self
    }
}

fn sanitize_extra_outputs(extra_outputs: &mut Vec<ExtraOutputPref>) {
    let mut seen_virtual = false;
    extra_outputs.retain(|extra_output| {
        if extra_output.virtual_output {
            if seen_virtual {
                return false;
            }
            seen_virtual = true;
        }
        true
    });
    extra_outputs.truncate(MAX_EXTRA_OUTPUTS);
    for extra_output in extra_outputs.iter_mut() {
        *extra_output = extra_output.clone().sanitized();
    }
}

/// An older save may still carry [`VIRTUAL_OUTPUT_ID`].  A missing or
/// non-WASAPI id is rematched when CABLE Input is present.
pub(crate) fn rematch_windows_virtual_output(
    extra_outputs: &mut [ExtraOutputPref],
    outputs: &[DeviceItem],
) {
    #[cfg(target_os = "windows")]
    {
        for extra_output in extra_outputs.iter_mut() {
            if !extra_output.virtual_output {
                continue;
            }
            // Id equality only.  A name or backend match would hide the
            // missing id this rebind has to see.
            let present = outputs.iter().any(|device| device.id == extra_output.id);
            let stale = extra_output.id.is_empty()
                || extra_output.id == VIRTUAL_OUTPUT_ID
                || !crate::audio::is_generic_extra_output(&extra_output.id)
                || !present;
            if !stale {
                continue;
            }
            let Some(cable) = outputs
                .iter()
                .find(|device| crate::audio::is_vb_cable_render(&device.name))
            else {
                continue;
            };
            extra_output.id = cable.id.clone();
            extra_output.backend = cable.backend.clone();
            if extra_output.name.trim().is_empty() {
                extra_output.name = VIRTUAL_OUTPUT_NAME.to_owned();
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (extra_outputs, outputs);
    }
}

pub(crate) const RATES: [u32; 4] = [44_100, 48_000, 88_200, 96_000];

pub(crate) const BUFFERS: [usize; 5] = [32, 64, 128, 256, 512];

pub(crate) const DEFAULT_SAMPLE_RATE: u32 = 48_000;

pub(crate) const DEFAULT_BUFFER: usize = 128;

pub(crate) const MIN_OUTPUT_BUFFER: usize = 16;

/// A size outside [`BUFFERS`] is kept only up to this, so a corrupt value
/// is not asked of the device.
pub(crate) const MAX_SANE_BUFFER: usize = 512;

pub(crate) const INPUT_GAIN_MIN_DB: f32 = -24.0;
pub(crate) const INPUT_GAIN_MAX_DB: f32 = 18.0;
pub(crate) const OUTPUT_GAIN_MIN_DB: f32 = -36.0;
pub(crate) const OUTPUT_GAIN_MAX_DB: f32 = 6.0;

pub(crate) const DEFAULT_INPUT_GAIN: f32 = 0.0;
pub(crate) const DEFAULT_OUTPUT_GAIN: f32 = -1.5;

pub(crate) const BRIDGE_BUFFER_MS_MIN: f32 = 1.0;
pub(crate) const BRIDGE_BUFFER_MS_MAX: f32 = 40.0;
pub(crate) const DEFAULT_BRIDGE_BUFFER_MS: f32 = 10.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutputBufferZero {
    /// Preferences never stored 0.  It is not a buffer size.
    Invalid,
    /// An old session stored 0 to mean the input buffer.
    FollowInput,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct BridgeSettings {
    pub(crate) sample_rate: u32,
    pub(crate) buffer_size: usize,
    /// `0` means the output clock follows the input.
    pub(crate) output_rate: u32,
    pub(crate) output_buffer: usize,
    pub(crate) input_gain: f32,
    pub(crate) output_gain: f32,
    pub(crate) bridge_buffer_ms: f32,
}

/// NaN becomes the default.  ±infinity clamps, so `10^(dB/20)` stays
/// finite.  A 0 output buffer stays 0 for [`OutputBufferZero::FollowInput`].
pub(crate) fn validate_bridge_settings(
    settings: BridgeSettings,
    output_buffer_zero: OutputBufferZero,
) -> BridgeSettings {
    BridgeSettings {
        sample_rate: known_rate(settings.sample_rate, false),
        buffer_size: known_buffer(settings.buffer_size, false),
        output_rate: known_rate(settings.output_rate, true),
        output_buffer: known_buffer(
            settings.output_buffer,
            output_buffer_zero == OutputBufferZero::FollowInput,
        ),
        input_gain: finite_gain(
            settings.input_gain,
            DEFAULT_INPUT_GAIN,
            INPUT_GAIN_MIN_DB,
            INPUT_GAIN_MAX_DB,
        ),
        output_gain: finite_gain(
            settings.output_gain,
            DEFAULT_OUTPUT_GAIN,
            OUTPUT_GAIN_MIN_DB,
            OUTPUT_GAIN_MAX_DB,
        ),
        bridge_buffer_ms: finite_bridge_buffer(settings.bridge_buffer_ms),
    }
}

pub(crate) fn sanitize_sample_rate(rate: u32) -> u32 {
    known_rate(rate, false)
}

pub(crate) fn sanitize_output_rate(rate: u32) -> u32 {
    known_rate(rate, true)
}

pub(crate) fn sanitize_buffer_size(buffer: usize) -> usize {
    known_buffer(buffer, false)
}

pub(crate) fn sanitize_bridge_buffer_ms(ms: f32) -> f32 {
    finite_bridge_buffer(ms)
}

fn known_rate(rate: u32, allow_match_input: bool) -> u32 {
    if allow_match_input && rate == 0 {
        return 0;
    }
    if RATES.contains(&rate) {
        rate
    } else {
        DEFAULT_SAMPLE_RATE
    }
}

fn known_buffer(buffer: usize, allow_follow_input: bool) -> usize {
    if allow_follow_input && buffer == 0 {
        return 0;
    }
    if BUFFERS.contains(&buffer) {
        return buffer;
    }
    if (MIN_OUTPUT_BUFFER..=MAX_SANE_BUFFER).contains(&buffer) {
        return buffer;
    }
    DEFAULT_BUFFER
}

fn finite_gain(value: f32, fallback: f32, min: f32, max: f32) -> f32 {
    let value = if value.is_nan() { fallback } else { value };
    value.clamp(min, max)
}

fn finite_bridge_buffer(ms: f32) -> f32 {
    let ms = if ms.is_nan() {
        DEFAULT_BRIDGE_BUFFER_MS
    } else {
        ms
    };
    ms.clamp(BRIDGE_BUFFER_MS_MIN, BRIDGE_BUFFER_MS_MAX)
}

fn bridge_settings_from_preferences(preferences: &Preferences) -> BridgeSettings {
    BridgeSettings {
        sample_rate: preferences.sample_rate,
        buffer_size: preferences.buffer_size,
        output_rate: preferences.output_rate,
        output_buffer: preferences.output_buffer,
        input_gain: preferences.input_gain,
        output_gain: preferences.output_gain,
        bridge_buffer_ms: preferences.bridge_buffer_ms,
    }
}

fn write_bridge_settings(preferences: &mut Preferences, settings: BridgeSettings) {
    preferences.sample_rate = settings.sample_rate;
    preferences.buffer_size = settings.buffer_size;
    preferences.output_rate = settings.output_rate;
    preferences.output_buffer = settings.output_buffer;
    preferences.input_gain = settings.input_gain;
    preferences.output_gain = settings.output_gain;
    preferences.bridge_buffer_ms = settings.bridge_buffer_ms;
}

/// A NaN A4 becomes 440 Hz, so the pitch maths never sees it.
pub(crate) fn sanitize_preferences(preferences: &mut Preferences) {
    let settings = validate_bridge_settings(
        bridge_settings_from_preferences(preferences),
        OutputBufferZero::Invalid,
    );
    write_bridge_settings(preferences, settings);
    migrate_wasapi_output_mode(
        &mut preferences.wasapi_output_mode,
        preferences.low_latency_output,
    );
    preferences.tuner_reference_hz =
        crate::app::tuner::sanitise_reference(preferences.tuner_reference_hz);
    preferences.record_folder =
        crate::app::record::resolved_record_folder(&preferences.record_folder);
    preferences.amp_folder = crate::app::nam::normalize_models_folder(&preferences.amp_folder);
    preferences.cab_folder = crate::app::nam::normalize_models_folder(&preferences.cab_folder);
    // The engine stores these as they are.  A loaded rhythm is already a
    // real bar (`ClickRhythm`'s serde other is 4/4).
    preferences.metronome_bpm = crate::audio::clamp_metronome_bpm(preferences.metronome_bpm);
    preferences.metronome_level_db =
        crate::audio::clamp_click_level_db(preferences.metronome_level_db);
    preferences.ui_size = sanitize_ui_size(preferences.ui_size);
    sanitize_effect_ids(&mut preferences.effect_favourites, false);
    sanitize_effect_ids(&mut preferences.effect_recent, true);
    if !preferences.sheet_height.is_finite() || preferences.sheet_height < 0.0 {
        preferences.sheet_height = 0.0;
    } else if preferences.sheet_height > 0.0 {
        preferences.sheet_height = preferences.sheet_height.clamp(SHEET_MIN, SHEET_MAX);
    }
    preferences.migrate_paths();
    sanitize_paths(&mut preferences.paths);
    let current = preferences.paths[0].clone();
    preferences.selected_path = preferences
        .selected_path
        .min(preferences.paths.len().saturating_sub(1));
    preferences.match_paths = preferences.match_paths.sanitized();
    preferences.clock_output = preferences.clock_output.sanitized();
    sanitize_extra_outputs(&mut preferences.extra_outputs);
    // A measurement that cannot describe a real path is dropped.
    preferences.measured_latency = preferences
        .measured_latency
        .take()
        .and_then(MeasuredLatency::sanitized);
    for session in &mut preferences.sessions {
        session.migrate_paths(&current);
        sanitize_session(session);
    }
}

/// A missing mode comes from `low_latency_output` (`true` is REAL-style).
pub(crate) fn migrate_wasapi_output_mode(
    mode: &mut Option<WasapiOutputMode>,
    low_latency_output: bool,
) {
    if mode.is_none() {
        *mode = Some(WasapiOutputMode::from_low_latency(low_latency_output));
    }
}

pub(crate) fn resolved_wasapi_output_mode(preferences: &Preferences) -> WasapiOutputMode {
    preferences
        .wasapi_output_mode
        .unwrap_or_else(|| WasapiOutputMode::from_low_latency(preferences.low_latency_output))
}

pub(crate) fn sanitize_session(session: &mut Session) {
    let settings = validate_bridge_settings(
        BridgeSettings {
            sample_rate: session.rate,
            buffer_size: session.buffer,
            output_rate: session.output_rate,
            output_buffer: session.output_buffer,
            input_gain: session.input_gain,
            output_gain: session.output_gain,
            bridge_buffer_ms: session.bridge_buffer_ms,
        },
        OutputBufferZero::FollowInput,
    );
    session.rate = settings.sample_rate;
    session.buffer = settings.buffer_size;
    session.output_rate = settings.output_rate;
    session.output_buffer = settings.output_buffer;
    session.input_gain = settings.input_gain;
    session.output_gain = settings.output_gain;
    session.bridge_buffer_ms = settings.bridge_buffer_ms;
    migrate_wasapi_output_mode(&mut session.wasapi_output_mode, session.low_latency_output);
    if let Some(eq) = session.eq.as_mut() {
        *eq = eq.sanitized();
    }
    if let Some(eq) = session.eq_post.as_mut() {
        *eq = eq.sanitized();
    }
    if let Some(gate) = session.gate.as_mut() {
        *gate = gate.sanitized();
    }
    if !session.paths.is_empty() {
        sanitize_paths(&mut session.paths);
    }
    session.match_paths = session.match_paths.sanitized();
    session.clock_output = session.clock_output.sanitized();
    sanitize_extra_outputs(&mut session.extra_outputs);
}
pub(crate) fn rate_label(rate: u32) -> String {
    if rate.is_multiple_of(1000) {
        format!("{} kHz", rate / 1000)
    } else {
        format!("{:.1} kHz", rate as f32 / 1000.0)
    }
}

pub(crate) const PREFERENCES_KEY: &str = "rigwerk-preferences";

/// Stored by name, so adding or reordering pages does not move a save.
pub(crate) fn page_key(page: Page) -> &'static str {
    match page {
        Page::Console => "console",
        Page::Tuner => "tuner",
        Page::Setup => "setup",
    }
}

pub(crate) fn page_from_key(key: &str) -> Page {
    match key.trim().to_lowercase().as_str() {
        "tuner" => Page::Tuner,
        "setup" | "devices" | "guide" => Page::Setup,
        // Old page names, and a key this build does not know, open the
        // console.  `console` and `setup` stay, so an older build still
        // has a page to open.
        _ => Page::Console,
    }
}
/// Counts every stored entry, readable or not, so a loss can be reported.
#[derive(Default, Deserialize)]
#[serde(default)]
struct StoredCounts {
    sessions: Vec<IgnoredAny>,
    plugins: Vec<IgnoredAny>,
    followers: Vec<IgnoredAny>,
    /// An unreadable path contributes no effects.  The others still count.
    #[serde(deserialize_with = "skip_unreadable")]
    paths: Vec<StoredPathCounts>,
    /// Set when a measurement was stored, readable or not.
    measured_latency: Option<IgnoredAny>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct StoredPathCounts {
    plugins: Vec<IgnoredAny>,
}

/// No path this build writes carries it.  It lives only until
/// [`load_preferences`] renames the slot for its position.
const UNREADABLE_PATH_SENTINEL: &str = "\u{0}unreadable path";

/// Only lists the load kept, after the cap, so a dropped path is not
/// reported as kept.
fn name_unreadable_paths(paths: &mut [PathPref]) -> usize {
    let mut replaced = 0;
    for (position, path) in paths.iter_mut().enumerate() {
        if path.name == UNREADABLE_PATH_SENTINEL {
            path.name = format!("Unreadable path {}", position + 1);
            replaced += 1;
        }
    }
    replaced
}

/// A dropped device, or a row past the cap, is not reported.
fn take_unreadable_rows(settings: &mut OutputDeviceSettings) -> usize {
    let mut replaced = 0;
    for row in &mut settings.path_map {
        if row.unreadable {
            row.unreadable = false;
            replaced += 1;
        }
    }
    replaced
}

/// A return's sends, an all-off list, and sends past [`MAX_PATHS`] keep
/// no position, so they are not counted.
fn count_replaced_sends(path: &PathPref) -> usize {
    if path.feed == PathFeed::Sends {
        return 0;
    }
    let mut replaced = 0;
    let mut any_live = false;
    for &level in path.sends.iter().take(MAX_PATHS) {
        if level.is_nan() {
            replaced += 1;
        } else if sanitize_send(level) > SEND_OFF_DB {
            any_live = true;
        }
    }
    if any_live {
        replaced
    } else {
        0
    }
}

fn count_replaced_sends_in(paths: &[PathPref]) -> usize {
    paths.iter().take(MAX_PATHS).map(count_replaced_sends).sum()
}

impl Placeholder for PathPref {
    /// Muted, with every send off: a live default would double the input,
    /// and a return would come back as an input.  The name is replaced
    /// before anything reads it.
    fn placeholder() -> Self {
        Self {
            name: UNREADABLE_PATH_SENTINEL.to_owned(),
            mute: true,
            ..Self::default()
        }
    }
}

fn skipped_summary(
    sessions: usize,
    plugins: usize,
    extra_outputs: usize,
    measurements: usize,
) -> Option<String> {
    let parts: Vec<String> = [
        (sessions, "saved session(s)"),
        (plugins, "plugin effect(s)"),
        (extra_outputs, "output device(s)"),
        (measurements, "measurement(s)"),
    ]
    .into_iter()
    .filter(|(count, _)| *count > 0)
    .map(|(count, what)| format!("{count} {what}"))
    .collect();
    match parts.as_slice() {
        [] => None,
        [one] => Some(one.clone()),
        [rest @ .., last] => Some(format!("{} and {last}", rest.join(", "))),
    }
}

pub(crate) struct LoadedPreferences {
    /// `None` when nothing was stored, or the text could not be read at all.
    pub(crate) preferences: Option<Preferences>,
    /// Original text to keep when a save would drop something that was
    /// stored, including a newer build's file, which is never overwritten.
    pub(crate) backup: Option<(String, String)>,
    /// Above [`PREFERENCES_SCHEMA`], a newer build wrote the file, so this
    /// session writes the backup and never the primary.
    pub(crate) stored_schema_version: u32,
    pub(crate) notices: Vec<String>,
}

/// eframe cannot list its keys, so one backup is overwritten rather than
/// leaving every damaged load behind.
pub(crate) const PREFERENCES_BACKUP_KEY: &str = "rigwerk-preferences-backup";

/// Whatever the next save would drop comes back as a backup.  A newer
/// file is read through field defaults; the value here carries this
/// build's number, so a migration is never told it is a newer layout.
pub(crate) fn load_preferences(raw: Option<&str>, backup_key: &str) -> LoadedPreferences {
    let Some(raw) = raw else {
        return LoadedPreferences {
            preferences: None,
            backup: None,
            stored_schema_version: PREFERENCES_SCHEMA,
            notices: Vec::new(),
        };
    };
    let backup = Some((backup_key.to_owned(), raw.to_owned()));
    let mut preferences: Preferences = match ron::from_str(raw) {
        Ok(preferences) => preferences,
        Err(error) => {
            return LoadedPreferences {
                preferences: None,
                backup,
                stored_schema_version: PREFERENCES_SCHEMA,
                notices: vec![format!(
                    "Saved settings could not be read ({error}); starting from defaults. \
                     The original is kept as “{backup_key}”; a later damaged load replaces it."
                )],
            };
        }
    };
    let mut notices = Vec::new();
    // The value carries this build's number from here on.  The stored
    // number stays beside it.
    let stored_schema_version = preferences.schema_version;
    preferences.schema_version = PREFERENCES_SCHEMA;
    // A failed count is not "nothing lost": the original text is kept
    // when the counts cannot vouch for it.
    let counts = ron::from_str::<StoredCounts>(raw);
    let counts_unreadable = counts.is_err();
    let counts = counts.unwrap_or_default();
    // Counted before sanitizing.  A dropped session is no longer in
    // `sessions`, and the clamp turns the marker into off.
    let mut replaced_sends = count_replaced_sends_in(&preferences.paths);
    for session in &preferences.sessions {
        replaced_sends += count_replaced_sends_in(&session.paths);
    }
    // Before sanitizing: a cap drop, and a duplicate virtual output, are
    // not unreadable entries.
    let sessions_loaded = preferences.sessions.len();
    let plugins_loaded = preferences.plugins.len()
        + preferences
            .paths
            .iter()
            .map(|path| path.plugins.len())
            .sum::<usize>();
    let extra_outputs_loaded = preferences.extra_outputs.len();
    // Sanitizing can still drop a measurement that parsed.
    sanitize_preferences(&mut preferences);
    // After the cap, so a path past it is not reported as kept.
    let mut replaced_paths = name_unreadable_paths(&mut preferences.paths);
    for session in &mut preferences.sessions {
        replaced_paths += name_unreadable_paths(&mut session.paths);
    }
    // After sanitizing, so a row in a dropped device or past the cap is gone.
    let mut replaced_routes = take_unreadable_rows(&mut preferences.clock_output);
    for extra_output in &mut preferences.extra_outputs {
        replaced_routes += take_unreadable_rows(&mut extra_output.settings);
    }
    for session in &mut preferences.sessions {
        replaced_routes += take_unreadable_rows(&mut session.clock_output);
        for extra_output in &mut session.extra_outputs {
            replaced_routes += take_unreadable_rows(&mut extra_output.settings);
        }
    }
    let skipped_sessions = counts.sessions.len().saturating_sub(sessions_loaded);
    let stored_plugins = counts.plugins.len()
        + counts
            .paths
            .iter()
            .map(|path| path.plugins.len())
            .sum::<usize>();
    let skipped_plugins = stored_plugins.saturating_sub(plugins_loaded);
    let skipped_extra_outputs = counts.followers.len().saturating_sub(extra_outputs_loaded);
    let skipped_measurements =
        usize::from(counts.measured_latency.is_some() && preferences.measured_latency.is_none());
    if let Some(skipped) = skipped_summary(
        skipped_sessions,
        skipped_plugins,
        skipped_extra_outputs,
        skipped_measurements,
    ) {
        notices.push(format!(
            "Skipped {skipped} that could not be read. The original settings are kept as \
             “{backup_key}”."
        ));
    }
    if replaced_paths > 0 || replaced_routes > 0 || replaced_sends > 0 {
        let mut replaced = Vec::new();
        if replaced_paths > 0 {
            replaced.push(format!(
                "{replaced_paths} saved signal path(s) (a muted “Unreadable path” slot with \
                 every send off)"
            ));
        }
        if replaced_routes > 0 {
            replaced.push(format!(
                "{replaced_routes} saved routing row(s) (off on that device)"
            ));
        }
        if replaced_sends > 0 {
            replaced.push(format!("{replaced_sends} saved send(s) (off)"));
        }
        notices.push(format!(
            "Could not read {}; each keeps its position, so no path, route or send moves to \
             another. The original settings are kept as “{backup_key}”.",
            replaced.join(", ")
        ));
    }
    if counts_unreadable {
        notices.push(format!(
            "The stored list sizes could not be counted, so this load cannot promise that \
             nothing was lost. The original settings are kept as “{backup_key}”."
        ));
    }
    if stored_schema_version > PREFERENCES_SCHEMA {
        notices.push(format!(
            "Settings were saved by a newer RigWerk (layout {stored_schema_version}); this \
             version keeps a copy as “{backup_key}” and leaves the original untouched, so \
             changes made in this session will not be saved."
        ));
    }
    // A newer file is not saved back over, so the value stays this
    // build's layout.
    LoadedPreferences {
        preferences: Some(preferences),
        backup: if notices.is_empty() { None } else { backup },
        stored_schema_version,
        notices,
    }
}

/// Before the startup-scan gate, so a quit during that scan cannot drop
/// the copy.
pub(crate) fn store_preferences_backup(
    storage: &mut dyn eframe::Storage,
    backup: &mut Option<(String, String)>,
) {
    if let Some((key, raw)) = backup.take() {
        storage.set_string(&key, raw);
    }
}

pub(crate) fn store_preferences(storage: &mut dyn eframe::Storage, preferences: &Preferences) {
    // Only `paths` is written.  Top-level mix fields move there first.
    let mut preferences = preferences.clone();
    preferences.migrate_paths();
    if let Some(current) = preferences.paths.first().cloned() {
        for session in &mut preferences.sessions {
            session.migrate_paths(&current);
        }
    }
    eframe::set_value(storage, PREFERENCES_KEY, &preferences);
}
