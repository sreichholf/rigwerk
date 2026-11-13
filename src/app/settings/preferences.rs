//! Launch preferences, follower prefs, bridge settings and load/store.

use serde::de::IgnoredAny;
use serde::Deserialize;

use super::path_pref::{default_path_name, sanitize_paths, PathPref};
use super::preset::Preset;
use super::serde_util::{skip_unreadable, skip_unreadable_option};
use crate::app::latency::MeasuredLatency;
use crate::app::Page;
use crate::audio::{
    CushionMode, DeviceItem, EqSettings, GateSettings, OutputDeviceSettings, PluginPref,
    WasapiOutputMode, MAX_FOLLOWERS, VIRTUAL_OUT_ID, VIRTUAL_OUT_NAME,
};

/// 2: signal paths (`paths`, per-device `path_map`); layout 1 files load
/// as one path.
pub(crate) const PREFERENCES_SCHEMA: u32 = 2;

/// Launch preferences that survive restarts.  The bridge stays stopped unless
/// the person explicitly asks for it to start with the app.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct Preferences {
    /// See [`PREFERENCES_SCHEMA`]; missing (older saves) reads as 0.
    #[serde(default)]
    pub(crate) schema_version: u32,
    pub(crate) auto_start_bridge: bool,
    /// A saved patch that cannot be read is skipped, not fatal.
    #[serde(deserialize_with = "skip_unreadable")]
    pub(crate) sessions: Vec<Preset>,
    pub(crate) session_name: String,
    /// The page that was open when the app was last used.
    pub(crate) page: String,
    /// Which of the two EQs the Mixer page was editing: 0 pre, 1 post.
    pub(crate) eq_edit: u8,
    /// Whether the output list was showing the low-latency tab.
    pub(crate) output_tab_low_latency: bool,
    /// The patch (profile) that was selected in the Sessions list.
    pub(crate) selected_session: Option<usize>,
    /// Selected devices. `*_id` is [`crate::audio::DeviceItem::id`] (cpal's
    /// `DeviceId` text, or `asio:{clsid}` / `asio-out:{clsid}`) and
    /// `*_backend` is that item's backend. The name is only the display
    /// fallback. Saves from before ids existed leave the id and backend empty.
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
    /// Files from before paths existed: path 1's settings.  Read, never
    /// written (see [`Self::paths`]).
    #[serde(skip_serializing)]
    pub(crate) output_channels: [bool; 2],
    /// Files from before paths existed: path 1's settings.  Read, never
    /// written (see [`Self::paths`]).
    #[serde(skip_serializing)]
    pub(crate) mono: bool,
    /// The master mute.
    pub(crate) mute: bool,
    pub(crate) drift: bool,
    pub(crate) monitor: bool,
    /// Linux low-latency output, and the pre-exclusive Windows REAL-style
    /// switch.  Kept so older saves and Linux prefs round-trip.  On Windows
    /// the authoritative choice is [`Self::wasapi_output_mode`].
    pub(crate) low_latency_output: bool,
    /// Windows WASAPI clock mode.  Missing on older saves → derived from
    /// [`Self::low_latency_output`] in [`sanitize_preferences`].
    #[serde(default)]
    pub(crate) wasapi_output_mode: Option<WasapiOutputMode>,
    /// Files from before paths existed: path 1's settings.  Read, never
    /// written (see [`Self::paths`]).
    #[serde(skip_serializing)]
    pub(crate) routes: [[bool; 4]; 2],
    /// Output side rate; 0 follows the input.
    pub(crate) output_rate: u32,
    pub(crate) output_buffer: usize,
    /// Cushion: 0 Safe, 1 Low latency, 2 Manual (size in `cushion_ms`).
    pub(crate) cushion_mode: u8,
    pub(crate) cushion_ms: f32,
    /// Path 1's EQ before the plugin inserts in files from before paths
    /// existed (saves from before there were two EQs load into this one).
    /// Read, never written.
    #[serde(skip_serializing)]
    pub(crate) eq: EqSettings,
    /// Path 1's EQ after the inserts, in files from before paths.
    #[serde(skip_serializing)]
    pub(crate) eq_post: EqSettings,
    /// Path 1's gate, in files from before paths.
    #[serde(default, skip_serializing)]
    pub(crate) gate: GateSettings,
    /// Path 1's insert chain, in files from before paths.  An entry that
    /// cannot be read is skipped, not fatal.
    #[serde(deserialize_with = "skip_unreadable", skip_serializing)]
    pub(crate) plugins: Vec<PluginPref>,
    /// The signal paths, in order.  Files from before paths existed have
    /// none: their top-level mix fields load as path 1 (see
    /// [`Self::path_prefs`]).  New files write only this.
    pub(crate) paths: Vec<PathPref>,
    /// The path the Mixer and Plugins pages edit (a position in `paths`).
    pub(crate) selected_path: usize,
    /// Open a plugin's own interface directly instead of RigWerk's control
    /// panel, when the plugin has one that can be embedded.
    pub(crate) prefer_plugin_interface: bool,
    /// Level, mute and channel map of the clock device (`output_id`).
    pub(crate) clock_device: OutputDeviceSettings,
    /// Extra output devices.  An entry that cannot be read is skipped.
    #[serde(deserialize_with = "skip_unreadable")]
    pub(crate) followers: Vec<FollowerPref>,
    /// The last measured loopback, with the input, output, rates, buffers
    /// and output mode it was made under.  Missing on older saves; an entry
    /// that cannot be read is treated as missing (the app re-measures).
    #[serde(default, deserialize_with = "skip_unreadable_option")]
    pub(crate) measured_latency: Option<MeasuredLatency>,
    /// Tuner preset id, or
    /// [`DEFAULT_PRESET_ID`](crate::audio::DEFAULT_PRESET_ID) for chromatic.
    /// An unknown id falls back to chromatic.
    pub(crate) tuner_tuning: String,
    /// A4 in Hz, inside the 415–466 the page offers.
    pub(crate) tuner_reference_hz: f32,
    /// 0 sum, 1 left, 2 right.
    pub(crate) tuner_channel: u8,
    /// 0 strobe, 1 needle.
    pub(crate) tuner_display: u8,
    /// 0 single note, 1 all strings.
    pub(crate) tuner_listen: u8,
    /// Mute the bridge output while the tuner page is open.
    pub(crate) tuner_auto_mute: bool,
    /// 0 tight, 1 standard, 2 live; the labels carry the cent values.
    pub(crate) tuner_tolerance: u8,
    /// Directory takes are written into.  Empty is replaced with the default
    /// recordings folder by [`sanitize_preferences`].  The writer creates it.
    #[serde(default)]
    pub(crate) record_folder: String,
    /// Metronome transport.  Not part of a patch: applying one does not
    /// start, stop or retune the click.  Missing fields take the defaults
    /// below (off, 120 BPM, 4/4, −12 dB).  `metronome_running` is neither
    /// saved nor read: a launch always starts the click stopped, and an old
    /// file that still carries the field loads (unknown fields are ignored).
    #[serde(skip)]
    pub(crate) metronome_running: bool,
    pub(crate) metronome_bpm: f32,
    pub(crate) metronome_rhythm: crate::audio::ClickRhythm,
    pub(crate) metronome_level_db: f32,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            schema_version: PREFERENCES_SCHEMA,
            auto_start_bridge: false,
            sessions: Vec::new(),
            session_name: "My guitar rig".to_owned(),
            page: "live".to_owned(),
            eq_edit: 0,
            output_tab_low_latency: cfg!(target_os = "linux"),
            selected_session: None,
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
            monitor: true,
            low_latency_output: true,
            wasapi_output_mode: Some(WasapiOutputMode::RealStyle),
            routes: [[true, false, false, false], [false, true, false, false]],
            output_rate: 0,
            output_buffer: DEFAULT_BUFFER,
            cushion_mode: 0,
            cushion_ms: DEFAULT_CUSHION_MS,
            eq: EqSettings::default(),
            eq_post: EqSettings::default(),
            gate: GateSettings::default(),
            plugins: Vec::new(),
            paths: Vec::new(),
            selected_path: 0,
            prefer_plugin_interface: false,
            clock_device: OutputDeviceSettings::default(),
            followers: Vec::new(),
            measured_latency: None,
            tuner_tuning: crate::audio::DEFAULT_PRESET_ID.to_owned(),
            tuner_reference_hz: 440.0,
            tuner_channel: 0,
            tuner_display: 0,
            tuner_listen: 0,
            tuner_auto_mute: true,
            tuner_tolerance: 1,
            record_folder: String::new(),
            metronome_running: false,
            metronome_bpm: crate::audio::METRONOME_BPM_DEFAULT,
            metronome_rhythm: crate::audio::ClickRhythm::FourFour,
            metronome_level_db: crate::audio::CLICK_LEVEL_DEFAULT_DB,
        }
    }
}

impl Preferences {
    /// The saved paths, or, for a file from before paths existed, one path
    /// made from its top-level mix fields.
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
            plugins: self.plugins.clone(),
            chain_bypass: false,
            level_db: 0.0,
            mute: false,
            solo: false,
        }]
    }

    /// Moves a file from before paths onto `paths`, and clears the legacy
    /// fields so nothing reads them by mistake.
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

pub(crate) fn cushion_from_pref(value: u8) -> CushionMode {
    match value {
        1 => CushionMode::LowLatency,
        2 => CushionMode::Manual,
        _ => CushionMode::Safe,
    }
}

pub(crate) fn cushion_to_pref(mode: CushionMode) -> u8 {
    match mode {
        CushionMode::Safe => 0,
        CushionMode::LowLatency => 1,
        CushionMode::Manual => 2,
    }
}

/// An extra output device that plays the finished mix.  On PipeWire it is a
/// sink linked from the duplex/native node; [`Self::virtual_out`] is the
/// RigWerk Out source other apps capture.  It is never the clock device.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct FollowerPref {
    /// [`crate::audio::DeviceItem::id`] and its backend; the name is only the
    /// display fallback.  PipeWire RigWerk Out uses [`VIRTUAL_OUT_ID`];
    /// Windows VB-Cable publish keeps the WASAPI render id.
    pub(crate) id: String,
    pub(crate) backend: String,
    pub(crate) name: String,
    pub(crate) settings: OutputDeviceSettings,
    /// Cushion: 0 Safe, 1 Low latency, 2 Manual (size in `cushion_ms`).
    /// Unused on PipeWire followers (the graph keeps them in step).
    pub(crate) cushion_mode: u8,
    pub(crate) cushion_ms: f32,
    /// RigWerk Out: a virtual source endpoint, not a device picker.
    pub(crate) virtual_out: bool,
    /// Whether the virtual source is advertised.  Ignored for hardware
    /// followers.  Missing on old saves → off.
    pub(crate) publish: bool,
}

impl Default for FollowerPref {
    fn default() -> Self {
        Self {
            id: String::new(),
            backend: String::new(),
            name: String::new(),
            settings: OutputDeviceSettings::default(),
            cushion_mode: 0,
            cushion_ms: DEFAULT_CUSHION_MS,
            virtual_out: false,
            publish: false,
        }
    }
}

impl FollowerPref {
    /// A new RigWerk Out follower, published on.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn virtual_out() -> Self {
        Self {
            id: VIRTUAL_OUT_ID.to_owned(),
            backend: "PipeWire".to_owned(),
            name: VIRTUAL_OUT_NAME.to_owned(),
            settings: OutputDeviceSettings::default(),
            cushion_mode: 1,
            cushion_ms: DEFAULT_CUSHION_MS,
            virtual_out: true,
            publish: true,
        }
    }

    pub(crate) fn sanitized(mut self) -> Self {
        self.settings = self.settings.sanitized();
        self.cushion_ms = finite_cushion(self.cushion_ms);
        if self.virtual_out {
            #[cfg(target_os = "windows")]
            {
                // VB-Cable publish: keep the WASAPI render id.  Rewriting to
                // the PipeWire sentinel would stop the supervisor opening it
                // after preference load or session apply.  Drop the sentinel
                // from older / cross-platform saves; inventory rematch fills
                // the live WASAPI id.
                if self.id == VIRTUAL_OUT_ID {
                    self.id.clear();
                }
                if self.name.trim().is_empty() {
                    self.name = VIRTUAL_OUT_NAME.to_owned();
                }
                if self.backend.trim().is_empty() {
                    self.backend = "WASAPI".to_owned();
                }
            }
            #[cfg(not(target_os = "windows"))]
            {
                self.id = VIRTUAL_OUT_ID.to_owned();
                self.backend = "PipeWire".to_owned();
                if self.name.trim().is_empty() {
                    self.name = VIRTUAL_OUT_NAME.to_owned();
                }
            }
        }
        self
    }
}

fn sanitize_followers(followers: &mut Vec<FollowerPref>) {
    let mut seen_virtual = false;
    followers.retain(|follower| {
        if follower.virtual_out {
            if seen_virtual {
                return false;
            }
            seen_virtual = true;
        }
        true
    });
    followers.truncate(MAX_FOLLOWERS);
    for follower in followers.iter_mut() {
        *follower = follower.clone().sanitized();
    }
}

/// Bind Windows VB-Cable publish to the live WASAPI render id from inventory.
///
/// Older saves and cross-platform presets may still carry
/// [`VIRTUAL_OUT_ID`]; a missing or non-WASAPI id also needs a rematch when
/// CABLE Input is present.
pub(crate) fn rematch_windows_virtual_out(followers: &mut [FollowerPref], outputs: &[DeviceItem]) {
    #[cfg(target_os = "windows")]
    {
        for follower in followers.iter_mut() {
            if !follower.virtual_out {
                continue;
            }
            let present = outputs.iter().any(|device| device.id == follower.id);
            let stale = follower.id.is_empty()
                || follower.id == VIRTUAL_OUT_ID
                || !crate::audio::is_generic_follower(&follower.id)
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
            follower.id = cable.id.clone();
            follower.backend = cable.backend.clone();
            if follower.name.trim().is_empty() {
                follower.name = VIRTUAL_OUT_NAME.to_owned();
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (followers, outputs);
    }
}

/// Rates offered on either side of the bridge.
pub(crate) const RATES: [u32; 4] = [44_100, 48_000, 88_200, 96_000];

/// Buffer sizes offered on either side of the bridge, in samples.
pub(crate) const BUFFERS: [usize; 5] = [32, 64, 128, 256, 512];

/// The rate a bad or missing input rate is replaced with. It is one of [`RATES`].
pub(crate) const DEFAULT_SAMPLE_RATE: u32 = 48_000;

/// The buffer a bad size is replaced with. It is one of [`BUFFERS`].
pub(crate) const DEFAULT_BUFFER: usize = 128;

/// Smallest output step that is still usable. The bridge used to raise
/// anything smaller with `output_buffer.max(16)`; that floor lives here now.
pub(crate) const MIN_OUTPUT_BUFFER: usize = 16;

/// Largest buffer kept when it is not one of [`BUFFERS`]. Equal to the last
/// offered size, so a corrupt huge value is not asked of the device.
pub(crate) const MAX_SANE_BUFFER: usize = 512;

/// Input fader, in dB. The Live card and the Mixer strip use this same range.
pub(crate) const INPUT_GAIN_MIN_DB: f32 = -24.0;
pub(crate) const INPUT_GAIN_MAX_DB: f32 = 18.0;
/// Output fader, in dB. Same range as the Live and Mixer output sliders.
pub(crate) const OUTPUT_GAIN_MIN_DB: f32 = -36.0;
pub(crate) const OUTPUT_GAIN_MAX_DB: f32 = 6.0;

/// What a missing or non-finite gain becomes before it is clamped.
pub(crate) const DEFAULT_INPUT_GAIN: f32 = 0.0;
pub(crate) const DEFAULT_OUTPUT_GAIN: f32 = -1.5;

/// Manual cushion slider, in milliseconds. Same ends as the Live card.
pub(crate) const CUSHION_MS_MIN: f32 = 1.0;
pub(crate) const CUSHION_MS_MAX: f32 = 40.0;
pub(crate) const DEFAULT_CUSHION_MS: f32 = 10.0;

/// How a stored `output_buffer` of 0 is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutputBufferZero {
    /// Preferences never used 0 as a sentinel. It is not a buffer size.
    Invalid,
    /// Patches saved before the output side existed stored 0 to mean
    /// "use the input buffer".
    FollowInput,
}

/// The numbers that must be safe before they reach the bridge or `10^(dB/20)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct BridgeSettings {
    pub(crate) sample_rate: u32,
    pub(crate) buffer_size: usize,
    /// `0` means the output clock follows the input.
    pub(crate) output_rate: u32,
    pub(crate) output_buffer: usize,
    pub(crate) input_gain: f32,
    pub(crate) output_gain: f32,
    pub(crate) cushion_ms: f32,
}

/// One check for saved preferences and for applying a patch.
///
/// Gains that are NaN become the ordinary defaults, then every gain is
/// clamped to the fader range. ±infinity lands on the nearer end, so the
/// value passed to `10^(dB/20)` is always finite.
///
/// `sample_rate` must be one of [`RATES`]; anything else becomes 48 kHz.
/// `output_rate` may be 0 (match the input) or one of [`RATES`]; anything
/// else becomes 48 kHz.
///
/// A buffer in [`BUFFERS`] is kept. Any other size is kept only when it is
/// at least [`MIN_OUTPUT_BUFFER`] and no larger than [`MAX_SANE_BUFFER`]
/// (a positive step the output path already allowed, up to the biggest
/// offered size). Anything else becomes [`DEFAULT_BUFFER`] and is not
/// applied raw. With [`OutputBufferZero::FollowInput`], a 0 output buffer
/// stays 0 so the caller can substitute the input buffer.
///
/// A NaN cushion becomes 10 ms. ±infinity is clamped to the manual slider
/// (1–40 ms).
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
        cushion_ms: finite_cushion(settings.cushion_ms),
    }
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

fn finite_cushion(ms: f32) -> f32 {
    let ms = if ms.is_nan() { DEFAULT_CUSHION_MS } else { ms };
    ms.clamp(CUSHION_MS_MIN, CUSHION_MS_MAX)
}

fn bridge_settings_from_preferences(preferences: &Preferences) -> BridgeSettings {
    BridgeSettings {
        sample_rate: preferences.sample_rate,
        buffer_size: preferences.buffer_size,
        output_rate: preferences.output_rate,
        output_buffer: preferences.output_buffer,
        input_gain: preferences.input_gain,
        output_gain: preferences.output_gain,
        cushion_ms: preferences.cushion_ms,
    }
}

fn write_bridge_settings(preferences: &mut Preferences, settings: BridgeSettings) {
    preferences.sample_rate = settings.sample_rate;
    preferences.buffer_size = settings.buffer_size;
    preferences.output_rate = settings.output_rate;
    preferences.output_buffer = settings.output_buffer;
    preferences.input_gain = settings.input_gain;
    preferences.output_gain = settings.output_gain;
    preferences.cushion_ms = settings.cushion_ms;
}

/// Checks the preference numbers and every saved patch. Called from
/// [`load_preferences`]; applying a patch checks again.
///
/// The tuner's A4 reference is clamped to the range the page offers, and a
/// NaN becomes 440 Hz, so the pitch maths never sees it.
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
    // Tempo and level are clamped here.  The app setters clamp a live edit
    // the same way; the engine stores those values as they are.  A loaded
    // rhythm is already a real bar (`ClickRhythm`'s serde other is 4/4).
    preferences.metronome_bpm = crate::audio::clamp_metronome_bpm(preferences.metronome_bpm);
    preferences.metronome_level_db =
        crate::audio::clamp_click_level_db(preferences.metronome_level_db);
    preferences.migrate_paths();
    sanitize_paths(&mut preferences.paths);
    let current = preferences.paths[0].clone();
    preferences.selected_path = preferences
        .selected_path
        .min(preferences.paths.len().saturating_sub(1));
    preferences.clock_device = preferences.clock_device.sanitized();
    sanitize_followers(&mut preferences.followers);
    // A measurement that cannot describe a real path is dropped; the app
    // then asks for a new one instead of showing a number that never was.
    preferences.measured_latency = preferences
        .measured_latency
        .take()
        .and_then(MeasuredLatency::sanitized);
    for preset in &mut preferences.sessions {
        preset.migrate_paths(&current);
        sanitize_preset(preset);
    }
}

/// Fills a missing Windows mode from the legacy bool (`true` → REAL-style).
pub(crate) fn migrate_wasapi_output_mode(
    mode: &mut Option<WasapiOutputMode>,
    low_latency_output: bool,
) {
    if mode.is_none() {
        *mode = Some(WasapiOutputMode::from_low_latency(low_latency_output));
    }
}

/// The mode to apply on Windows after load / sanitize.
pub(crate) fn resolved_wasapi_output_mode(preferences: &Preferences) -> WasapiOutputMode {
    preferences
        .wasapi_output_mode
        .unwrap_or_else(|| WasapiOutputMode::from_low_latency(preferences.low_latency_output))
}

pub(crate) fn sanitize_preset(preset: &mut Preset) {
    let settings = validate_bridge_settings(
        BridgeSettings {
            sample_rate: preset.rate,
            buffer_size: preset.buffer,
            output_rate: preset.output_rate,
            output_buffer: preset.output_buffer,
            input_gain: preset.input_gain,
            output_gain: preset.output_gain,
            cushion_ms: preset.cushion_ms,
        },
        OutputBufferZero::FollowInput,
    );
    preset.rate = settings.sample_rate;
    preset.buffer = settings.buffer_size;
    preset.output_rate = settings.output_rate;
    preset.output_buffer = settings.output_buffer;
    preset.input_gain = settings.input_gain;
    preset.output_gain = settings.output_gain;
    preset.cushion_ms = settings.cushion_ms;
    migrate_wasapi_output_mode(&mut preset.wasapi_output_mode, preset.low_latency_output);
    if let Some(eq) = preset.eq.as_mut() {
        *eq = eq.sanitized();
    }
    if let Some(eq) = preset.eq_post.as_mut() {
        *eq = eq.sanitized();
    }
    if let Some(gate) = preset.gate.as_mut() {
        *gate = gate.sanitized();
    }
    if !preset.paths.is_empty() {
        sanitize_paths(&mut preset.paths);
    }
    preset.clock_device = preset.clock_device.sanitized();
    sanitize_followers(&mut preset.followers);
}
pub(crate) fn rate_label(rate: u32) -> String {
    format!("{:.1} kHz", rate as f32 / 1000.0)
}

pub(crate) const PREFERENCES_KEY: &str = "rigwerk-preferences";

/// Pages are stored by name so the saved preferences stay readable and keep
/// working when pages are added or reordered.
pub(crate) fn page_key(page: Page) -> &'static str {
    match page {
        Page::Live => "live",
        Page::Mixer => "mixer",
        Page::Tuner => "tuner",
        Page::Routing => "routing",
        Page::Devices => "devices",
        Page::Sessions => "sessions",
        Page::Plugins => "plugins",
        Page::Guide => "guide",
        Page::Diagnostics => "diagnostics",
    }
}

pub(crate) fn page_from_key(key: &str) -> Page {
    match key.trim().to_lowercase().as_str() {
        "mixer" => Page::Mixer,
        "tuner" => Page::Tuner,
        "routing" => Page::Routing,
        "devices" => Page::Devices,
        "sessions" => Page::Sessions,
        "plugins" => Page::Plugins,
        "guide" => Page::Guide,
        "diagnostics" => Page::Diagnostics,
        _ => Page::Live,
    }
}
/// How many entries the stored lists held before unreadable ones were
/// dropped, so the loss can be reported.
#[derive(Default, Deserialize)]
#[serde(default)]
struct StoredCounts {
    sessions: Vec<IgnoredAny>,
    plugins: Vec<IgnoredAny>,
    followers: Vec<IgnoredAny>,
    paths: Vec<StoredPathCounts>,
    /// Present when a measurement was stored, readable or not.
    measured_latency: Option<IgnoredAny>,
}

/// How many inserts one stored path held.
#[derive(Default, Deserialize)]
#[serde(default)]
struct StoredPathCounts {
    plugins: Vec<IgnoredAny>,
}

/// "1 saved patch(es) and 2 plugin insert(s)": only the lists that lost
/// entries, or `None` when nothing was skipped.
fn skipped_summary(
    sessions: usize,
    plugins: usize,
    followers: usize,
    measurements: usize,
) -> Option<String> {
    let parts: Vec<String> = [
        (sessions, "saved patch(es)"),
        (plugins, "plugin insert(s)"),
        (followers, "output device(s)"),
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

/// The outcome of reading the stored preferences.
pub(crate) struct LoadedPreferences {
    /// `None` when nothing was stored or the text could not be read at all.
    pub(crate) preferences: Option<Preferences>,
    /// `(key, original text)` to keep beside the preferences on the next
    /// save, set whenever the next save would lose something that was
    /// stored: an unreadable file, skipped entries, or a newer layout.
    pub(crate) backup: Option<(String, String)>,
    /// What went wrong, for the app log.
    pub(crate) notices: Vec<String>,
}

/// The key a copy of damaged preferences is kept under.  The time makes it
/// unique, so a later failure never replaces an earlier copy.
pub(crate) fn preferences_backup_key(unix_secs: u64) -> String {
    format!("{PREFERENCES_KEY}-backup-{unix_secs}")
}

/// Parses the stored preferences (eframe keeps them as RON), tolerating
/// damaged entries.  Never loses data silently: whatever the next save would
/// drop is returned as a backup together with a notice for the log.
pub(crate) fn load_preferences(raw: Option<&str>, backup_key: &str) -> LoadedPreferences {
    let Some(raw) = raw else {
        return LoadedPreferences {
            preferences: None,
            backup: None,
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
                notices: vec![format!(
                    "Saved settings could not be read ({error}); starting from defaults. \
                     The original is kept in the settings file as “{backup_key}”."
                )],
            };
        }
    };
    let mut notices = Vec::new();
    let counts: StoredCounts = ron::from_str(raw).unwrap_or_default();
    // List lengths before sanitizing: a follower the cap drops is not an
    // unreadable entry, and the duplicate virtual out is dropped on purpose.
    let sessions_loaded = preferences.sessions.len();
    let plugins_loaded = preferences.plugins.len()
        + preferences
            .paths
            .iter()
            .map(|path| path.plugins.len())
            .sum::<usize>();
    let followers_loaded = preferences.followers.len();
    // Sanitizing can still drop a measurement that parsed but cannot describe
    // a real path; the measurement notice is counted after it runs.
    sanitize_preferences(&mut preferences);
    let skipped_sessions = counts.sessions.len().saturating_sub(sessions_loaded);
    let stored_plugins = counts.plugins.len()
        + counts
            .paths
            .iter()
            .map(|path| path.plugins.len())
            .sum::<usize>();
    let skipped_plugins = stored_plugins.saturating_sub(plugins_loaded);
    let skipped_followers = counts.followers.len().saturating_sub(followers_loaded);
    let skipped_measurements =
        usize::from(counts.measured_latency.is_some() && preferences.measured_latency.is_none());
    if let Some(skipped) = skipped_summary(
        skipped_sessions,
        skipped_plugins,
        skipped_followers,
        skipped_measurements,
    ) {
        notices.push(format!(
            "Skipped {skipped} that could not be read. The original settings are kept as \
             “{backup_key}”."
        ));
    }
    if preferences.schema_version > PREFERENCES_SCHEMA {
        notices.push(format!(
            "Settings were saved by a newer RigWerk (layout {}); what this version does not \
             know is kept in the original, saved as “{backup_key}”.",
            preferences.schema_version
        ));
    }
    // Layout 1 without `paths` is migrated by `sanitize_preferences`; the
    // other field defaults cover older layouts.
    preferences.schema_version = PREFERENCES_SCHEMA;
    LoadedPreferences {
        preferences: Some(preferences),
        backup: if notices.is_empty() { None } else { backup },
        notices,
    }
}

/// Writes the preferences, first keeping any backup the load asked for so
/// the original text is never overwritten unseen.  The backup is written
/// once, then cleared.
pub(crate) fn store_preferences(
    storage: &mut dyn eframe::Storage,
    preferences: &Preferences,
    backup: &mut Option<(String, String)>,
) {
    if let Some((key, raw)) = backup.take() {
        storage.set_string(&key, raw);
    }
    // Only `paths` is written: anything still held in the legacy mix fields
    // (preferences or a patch from before paths) moves there first.
    let mut preferences = preferences.clone();
    preferences.migrate_paths();
    if let Some(current) = preferences.paths.first().cloned() {
        for preset in &mut preferences.sessions {
            preset.migrate_paths(&current);
        }
    }
    eframe::set_value(storage, PREFERENCES_KEY, &preferences);
}
