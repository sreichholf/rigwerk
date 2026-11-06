//! What survives a restart: preferences, saved patches and the choices
//! offered for rates and buffers.

use serde::de::{Deserializer, IgnoredAny};
use serde::Deserialize;

use super::Page;
use crate::audio::{CushionMode, EqSettings, GateSettings, OutputDeviceSettings, PluginPref};

/// The layout version written with the preferences.  Saves from before the
/// version existed read as 0.  Raise it, and migrate in
/// [`load_preferences`], when a change cannot be covered by field defaults.
pub(crate) const PREFERENCES_SCHEMA: u32 = 1;

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
    pub(crate) output_channels: [bool; 2],
    pub(crate) mono: bool,
    pub(crate) mute: bool,
    pub(crate) drift: bool,
    pub(crate) monitor: bool,
    /// REAL-style output (the setting kept its older name so saved
    /// preferences carry over).
    pub(crate) low_latency_output: bool,
    pub(crate) routes: [[bool; 4]; 2],
    /// Output side rate; 0 follows the input.
    pub(crate) output_rate: u32,
    pub(crate) output_buffer: usize,
    /// Cushion: 0 Safe, 1 Low latency, 2 Manual (size in `cushion_ms`).
    pub(crate) cushion_mode: u8,
    pub(crate) cushion_ms: f32,
    /// The EQ before the plugin inserts (saves from before there were two
    /// EQs load into this one).  Flat when missing.
    pub(crate) eq: EqSettings,
    /// The EQ after the plugin inserts.
    pub(crate) eq_post: EqSettings,
    /// Built-in noise gate. Missing on older saves → default (off, POST).
    #[serde(default)]
    pub(crate) gate: GateSettings,
    /// The master insert chain (paths and settings, not live plugins).  An
    /// entry that cannot be read is skipped, not fatal.
    #[serde(deserialize_with = "skip_unreadable")]
    pub(crate) plugins: Vec<PluginPref>,
    /// Open a plugin's own interface directly instead of RigWerk's control
    /// panel, when the plugin has one that can be embedded.
    pub(crate) prefer_plugin_interface: bool,
    /// Level, mute and channel map of the clock device (`output_id`).
    pub(crate) clock_device: OutputDeviceSettings,
    /// Extra output devices.  An entry that cannot be read is skipped.
    #[serde(deserialize_with = "skip_unreadable")]
    pub(crate) followers: Vec<FollowerPref>,
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
            routes: [[true, false, false, false], [false, true, false, false]],
            output_rate: 0,
            output_buffer: DEFAULT_BUFFER,
            cushion_mode: 0,
            cushion_ms: DEFAULT_CUSHION_MS,
            eq: EqSettings::default(),
            eq_post: EqSettings::default(),
            gate: GateSettings::default(),
            plugins: Vec::new(),
            prefer_plugin_interface: false,
            clock_device: OutputDeviceSettings::default(),
            followers: Vec::new(),
        }
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

/// An extra output device that plays the finished mix on its own clock.
/// Stored only; nothing plays followers yet.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct FollowerPref {
    /// [`crate::audio::DeviceItem::id`] and its backend; the name is only the
    /// display fallback.
    pub(crate) id: String,
    pub(crate) backend: String,
    pub(crate) name: String,
    pub(crate) settings: OutputDeviceSettings,
    /// Cushion: 0 Safe, 1 Low latency, 2 Manual (size in `cushion_ms`).
    pub(crate) cushion_mode: u8,
    pub(crate) cushion_ms: f32,
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
        }
    }
}

impl FollowerPref {
    pub(crate) fn sanitized(mut self) -> Self {
        self.settings = self.settings.sanitized();
        self.cushion_ms = finite_cushion(self.cushion_ms);
        self
    }
}

fn sanitize_followers(followers: &mut [FollowerPref]) {
    for follower in followers {
        *follower = follower.clone().sanitized();
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
pub(crate) fn sanitize_preferences(preferences: &mut Preferences) {
    let settings = validate_bridge_settings(
        bridge_settings_from_preferences(preferences),
        OutputBufferZero::Invalid,
    );
    write_bridge_settings(preferences, settings);
    preferences.eq = preferences.eq.sanitized();
    preferences.eq_post = preferences.eq_post.sanitized();
    preferences.gate = preferences.gate.sanitized();
    preferences.clock_device = preferences.clock_device.sanitized();
    sanitize_followers(&mut preferences.followers);
    for preset in &mut preferences.sessions {
        sanitize_preset(preset);
    }
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
    if let Some(eq) = preset.eq.as_mut() {
        *eq = eq.sanitized();
    }
    if let Some(eq) = preset.eq_post.as_mut() {
        *eq = eq.sanitized();
    }
    if let Some(gate) = preset.gate.as_mut() {
        *gate = gate.sanitized();
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
        "routing" => Page::Routing,
        "devices" => Page::Devices,
        "sessions" => Page::Sessions,
        "plugins" => Page::Plugins,
        "guide" => Page::Guide,
        "diagnostics" => Page::Diagnostics,
        _ => Page::Live,
    }
}

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
    /// The EQs saved with the patch (pre and post inserts).  Patches from
    /// before an EQ existed have none and leave the current one alone.
    pub(crate) eq: Option<EqSettings>,
    pub(crate) eq_post: Option<EqSettings>,
    /// Built-in noise gate. Older patches have none and leave the current one.
    #[serde(default)]
    pub(crate) gate: Option<GateSettings>,
    /// Input fader, dB. Missing on older patches; the default is 0 dB.
    pub(crate) input_gain: f32,
    /// Output fader, dB. Missing on older patches; the default is −1.5 dB.
    pub(crate) output_gain: f32,
    pub(crate) input_channels: [bool; 2],
    pub(crate) output_channels: [bool; 2],
    pub(crate) mono: bool,
    pub(crate) mute: bool,
    pub(crate) monitor: bool,
    pub(crate) drift: bool,
    /// REAL-style / low-latency output. The preference kept this name.
    pub(crate) low_latency_output: bool,
    pub(crate) routes: [[bool; 4]; 2],
    /// Cushion: 0 Safe, 1 Low latency, 2 Manual (size in `cushion_ms`).
    pub(crate) cushion_mode: u8,
    pub(crate) cushion_ms: f32,
    /// The master insert chain. An entry that cannot be read is skipped.
    /// Older patches have none, which loads as an empty chain.
    #[serde(default, deserialize_with = "skip_unreadable")]
    pub(crate) plugins: Vec<PluginPref>,
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
            routes: [[true, false, false, false], [false, true, false, false]],
            cushion_mode: 0,
            cushion_ms: DEFAULT_CUSHION_MS,
            plugins: Vec::new(),
            clock_device: OutputDeviceSettings::default(),
            followers: Vec::new(),
        }
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

/// Reads a list entry by entry, dropping the entries that do not parse, so
/// one damaged patch or insert does not cost the rest of the preferences.
fn skip_unreadable<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Entry<T> {
        Good(T),
        Bad(IgnoredAny),
    }
    let entries = Vec::<Entry<T>>::deserialize(deserializer)?;
    Ok(entries
        .into_iter()
        .filter_map(|entry| match entry {
            Entry::Good(value) => Some(value),
            Entry::Bad(_) => None,
        })
        .collect())
}

/// How many entries the stored lists held before unreadable ones were
/// dropped, so the loss can be reported.
#[derive(Default, Deserialize)]
#[serde(default)]
struct StoredCounts {
    sessions: Vec<IgnoredAny>,
    plugins: Vec<IgnoredAny>,
    followers: Vec<IgnoredAny>,
}

/// "1 saved patch(es) and 2 plugin insert(s)": only the lists that lost
/// entries, or `None` when nothing was skipped.
fn skipped_summary(sessions: usize, plugins: usize, followers: usize) -> Option<String> {
    let parts: Vec<String> = [
        (sessions, "saved patch(es)"),
        (plugins, "plugin insert(s)"),
        (followers, "output device(s)"),
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
    let skipped_sessions = counts
        .sessions
        .len()
        .saturating_sub(preferences.sessions.len());
    let skipped_plugins = counts
        .plugins
        .len()
        .saturating_sub(preferences.plugins.len());
    let skipped_followers = counts
        .followers
        .len()
        .saturating_sub(preferences.followers.len());
    if let Some(skipped) = skipped_summary(skipped_sessions, skipped_plugins, skipped_followers) {
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
    // Older layouts need no migration yet: the field defaults cover them.
    preferences.schema_version = PREFERENCES_SCHEMA;
    for plugin in &mut preferences.plugins {
        if plugin.name.trim().is_empty() {
            plugin.name = std::path::Path::new(&plugin.path)
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Plugin".to_owned());
        }
    }
    // Rates, buffers, gains and the cushion are checked here, and again
    // when a patch is applied. A NaN gain never reaches `10^(dB/20)`.
    sanitize_preferences(&mut preferences);
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
    eframe::set_value(storage, PREFERENCES_KEY, preferences);
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::audio::{ChannelMap, GateSlot, PluginFormat, UnusedChannels};

    const BACKUP: &str = "rigwerk-preferences-backup-1";

    /// eframe's file storage in memory: the same key/string interface, and
    /// `set_value` writes RON into it exactly as the app's autosave does.
    #[derive(Default)]
    struct MemoryStorage(HashMap<String, String>);

    impl eframe::Storage for MemoryStorage {
        fn get_string(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
        fn set_string(&mut self, key: &str, value: String) {
            self.0.insert(key.to_owned(), value);
        }
        fn remove_string(&mut self, key: &str) {
            self.0.remove(key);
        }
        fn flush(&mut self) {}
    }

    fn preset(name: &str) -> Preset {
        Preset {
            name: name.to_owned(),
            note: "ASIO → WASAPI".to_owned(),
            input: "Focusrite USB ASIO".to_owned(),
            input_id: "asio:focusrite".to_owned(),
            input_backend: "ASIO".to_owned(),
            output: "Speakers".to_owned(),
            output_id: "wasapi:{speakers}".to_owned(),
            output_backend: "WASAPI".to_owned(),
            rate: 96_000,
            buffer: 64,
            output_rate: 48_000,
            output_buffer: 256,
            eq: Some(EqSettings::default()),
            eq_post: None,
            ..Preset::default()
        }
    }

    fn plugin(path: &str, format: PluginFormat) -> PluginPref {
        PluginPref {
            path: path.to_owned(),
            format,
            class_id: Some("ABCD".to_owned()),
            name: "Amp".to_owned(),
            vendor: "Vendor".to_owned(),
            category: "Fx".to_owned(),
            bypass: true,
            mix: 0.5,
            params: ron::from_str("[(id: 7, value: 0.25)]").expect("params"),
            state: None,
        }
    }

    fn stored(preferences: &Preferences) -> String {
        let mut storage = MemoryStorage::default();
        store_preferences(&mut storage, preferences, &mut None);
        eframe::Storage::get_string(&storage, PREFERENCES_KEY).expect("stored")
    }

    #[test]
    fn round_trip_keeps_everything() {
        let mut verb = plugin("C:/Plugins/Verb.clap", PluginFormat::Clap);
        verb.state = ron::from_str(
            r#"Some((version: 1, data: "AAEC/w==", params: [(id: 7, value: 0.25)]))"#,
        )
        .expect("state");
        let preferences = Preferences {
            sessions: vec![preset("Clean"), preset("Lead")],
            plugins: vec![
                plugin("C:/Plugins/Amp.vst3", PluginFormat::Vst3),
                verb.clone(),
            ],
            sample_rate: 96_000,
            session_name: "Gig".to_owned(),
            ..Preferences::default()
        };
        let loaded = load_preferences(Some(&stored(&preferences)), BACKUP);
        assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
        assert!(loaded.backup.is_none());
        let back = loaded.preferences.expect("readable");
        assert_eq!(back.schema_version, PREFERENCES_SCHEMA);
        assert_eq!(back.sample_rate, 96_000);
        assert_eq!(back.session_name, "Gig");
        assert_eq!(back.sessions.len(), 2);
        assert_eq!(back.sessions[1].name, "Lead");
        assert_eq!(back.sessions[0].input_id, "asio:focusrite");
        assert_eq!(back.sessions[0].input_backend, "ASIO");
        assert_eq!(back.sessions[0].output_id, "wasapi:{speakers}");
        assert_eq!(back.sessions[0].output_backend, "WASAPI");
        assert_eq!(back.sessions[0].output_buffer, 256);
        assert_eq!(back.sessions[0].eq, Some(EqSettings::default()));
        assert!(back.sessions[0].eq_post.is_none());
        assert_eq!(back.plugins.len(), 2);
        assert_eq!(back.plugins[1].format, PluginFormat::Clap);
        assert_eq!(back.plugins[1].class_id.as_deref(), Some("ABCD"));
        assert!(back.plugins[1].bypass);
        assert_eq!(back.plugins[1].mix, 0.5);
        assert_eq!(back.plugins[1].params[0].id, 7);
        assert_eq!(back.plugins[1].state, verb.state);
        assert!(back.plugins[0].state.is_none());
    }

    #[test]
    fn damaged_plugin_state_keeps_the_insert() {
        let raw = r#"(plugins:[(path:"C:/Plugins/Amp.clap",format:clap,name:"Amp",params:[(id:3,value:0.5)],state:Some((version:"one",data:12))),(path:"C:/Plugins/Old.vst3",name:"Old")])"#;
        let loaded = load_preferences(Some(raw), BACKUP);
        assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
        let preferences = loaded.preferences.expect("readable");
        assert_eq!(preferences.plugins.len(), 2);
        assert!(preferences.plugins[0].state.is_none());
        assert_eq!(preferences.plugins[0].params[0].id, 3);
        assert!(preferences.plugins[1].state.is_none());
    }

    #[test]
    fn preset_missing_fields_still_loads() {
        // Written before `note`, `buffer` and the output side existed, and
        // before the schema version: used to fail the whole preferences.
        let raw = r#"(auto_start_bridge:true,sessions:[(name:"Old",input:"In",output:"Out",rate:44100)],session_name:"Mine",sample_rate:44100)"#;
        let loaded = load_preferences(Some(raw), BACKUP);
        assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
        let preferences = loaded.preferences.expect("readable");
        assert!(preferences.auto_start_bridge);
        assert_eq!(preferences.schema_version, PREFERENCES_SCHEMA);
        assert_eq!(preferences.sessions.len(), 1);
        let old = &preferences.sessions[0];
        assert_eq!(old.name, "Old");
        assert_eq!(old.rate, 44_100);
        assert_eq!(old.buffer, 128);
        assert_eq!(old.note, "");
        assert_eq!(old.input_id, "");
        assert_eq!(old.input_backend, "");
        assert_eq!(old.output_id, "");
        assert_eq!(old.output_backend, "");
        assert_eq!(old.output_rate, 0);
        // Fields added with the mix: missing means the ordinary defaults,
        // and a 0 output buffer still means "use the input buffer".
        assert_eq!(old.output_buffer, 0);
        assert_eq!(old.input_gain, DEFAULT_INPUT_GAIN);
        assert_eq!(old.output_gain, DEFAULT_OUTPUT_GAIN);
        assert_eq!(old.input_channels, [true, true]);
        assert_eq!(old.output_channels, [true, true]);
        assert!(!old.mono);
        assert!(!old.mute);
        assert!(old.monitor);
        assert!(old.drift);
        assert!(old.low_latency_output);
        assert_eq!(
            old.routes,
            [[true, false, false, false], [false, true, false, false]]
        );
        assert_eq!(old.cushion_mode, 0);
        assert_eq!(old.cushion_ms, DEFAULT_CUSHION_MS);
        assert!(old.plugins.is_empty());
        assert!(old.eq.is_none());
        assert!(old.eq_post.is_none());
        assert!(old.gate.is_none());
    }

    #[test]
    fn gate_round_trips_in_preferences_and_session_presets() {
        let gate = GateSettings {
            enabled: true,
            slot: GateSlot::Pre,
            threshold_db: -42.5,
            range_db: -55.0,
            hysteresis_db: 8.0,
            attack_ms: 2.5,
            release_ms: 180.0,
            hold_ms: 55.0,
        };
        let mut session = preset("Gated");
        session.gate = Some(gate);
        session.eq_post = Some(EqSettings::default());
        let preferences = Preferences {
            gate,
            sessions: vec![session],
            ..Preferences::default()
        };
        let loaded = load_preferences(Some(&stored(&preferences)), BACKUP);
        assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
        let back = loaded.preferences.expect("readable");
        assert_eq!(back.gate, gate);
        assert_eq!(back.sessions[0].gate, Some(gate));
        assert!(back.sessions[0].eq_post.is_some());
    }

    #[test]
    fn gate_missing_from_old_saves_uses_defaults() {
        let raw = r#"(auto_start_bridge:false,session_name:"Old",sample_rate:48000)"#;
        let loaded = load_preferences(Some(raw), BACKUP);
        let preferences = loaded.preferences.expect("readable");
        assert_eq!(preferences.gate, GateSettings::default());
        assert_eq!(preferences.gate.slot, GateSlot::Post);
        assert!(!preferences.gate.enabled);
    }

    #[test]
    fn gate_knobs_are_sanitized_on_load() {
        let raw = r#"(
            gate:(
                enabled:true,
                slot:pre,
                threshold_db:-200.0,
                range_db:10.0,
                hysteresis_db:NaN,
                attack_ms:0.0,
                release_ms:9999.0,
                hold_ms:-1.0
            ),
            sessions:[(
                name:"BadGate",
                input:"In",
                output:"Out",
                rate:48000,
                gate:(
                    enabled:true,
                    slot:post,
                    threshold_db:inf,
                    range_db:-200.0,
                    hysteresis_db:100.0,
                    attack_ms:NaN,
                    release_ms:0.0,
                    hold_ms:inf
                )
            )]
        )"#;
        let loaded = load_preferences(Some(raw), BACKUP);
        let preferences = loaded.preferences.expect("readable");
        let defaults = GateSettings::default();
        assert!(preferences.gate.enabled);
        assert_eq!(preferences.gate.slot, GateSlot::Pre);
        assert_eq!(
            preferences.gate.threshold_db,
            crate::audio::GATE_MIN_THRESHOLD_DB
        );
        assert_eq!(preferences.gate.range_db, crate::audio::GATE_MAX_RANGE_DB);
        assert_eq!(preferences.gate.hysteresis_db, defaults.hysteresis_db);
        assert_eq!(preferences.gate.attack_ms, crate::audio::GATE_MIN_ATTACK_MS);
        assert_eq!(
            preferences.gate.release_ms,
            crate::audio::GATE_MAX_RELEASE_MS
        );
        assert_eq!(preferences.gate.hold_ms, crate::audio::GATE_MIN_HOLD_MS);
        let session_gate = preferences.sessions[0].gate.expect("gate");
        assert_eq!(session_gate.threshold_db, defaults.threshold_db);
        assert_eq!(session_gate.range_db, crate::audio::GATE_MIN_RANGE_DB);
        assert_eq!(
            session_gate.hysteresis_db,
            crate::audio::GATE_MAX_HYSTERESIS_DB
        );
        assert_eq!(session_gate.attack_ms, defaults.attack_ms);
        assert_eq!(session_gate.release_ms, crate::audio::GATE_MIN_RELEASE_MS);
        assert_eq!(session_gate.hold_ms, defaults.hold_ms);
    }

    #[test]
    fn corrupt_gains_rates_and_buffers_are_not_applied_raw() {
        let raw = r#"(
            input_gain:NaN,
            output_gain:inf,
            sample_rate:22050,
            buffer_size:7,
            output_rate:12345,
            output_buffer:1,
            cushion_ms:NaN,
            sessions:[(
                name:"Bad",
                input:"In",
                output:"Out",
                rate:1,
                buffer:0,
                output_rate:7,
                output_buffer:9,
                input_gain:NaN,
                output_gain:-inf,
                cushion_ms:inf
            )]
        )"#;
        let loaded = load_preferences(Some(raw), BACKUP);
        assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
        let preferences = loaded.preferences.expect("readable");
        assert!(preferences.input_gain.is_finite());
        assert_eq!(preferences.input_gain, DEFAULT_INPUT_GAIN);
        assert!((INPUT_GAIN_MIN_DB..=INPUT_GAIN_MAX_DB).contains(&preferences.input_gain));
        assert!(preferences.output_gain.is_finite());
        assert_eq!(preferences.output_gain, OUTPUT_GAIN_MAX_DB);
        assert_eq!(preferences.sample_rate, DEFAULT_SAMPLE_RATE);
        assert_ne!(preferences.sample_rate, 22_050);
        assert_eq!(preferences.buffer_size, DEFAULT_BUFFER);
        assert_ne!(preferences.buffer_size, 7);
        assert_eq!(preferences.output_rate, DEFAULT_SAMPLE_RATE);
        assert_ne!(preferences.output_rate, 12_345);
        assert!(RATES.contains(&preferences.output_rate));
        assert_eq!(preferences.output_buffer, DEFAULT_BUFFER);
        assert!(preferences.output_buffer >= MIN_OUTPUT_BUFFER);
        assert_ne!(preferences.output_buffer, 1);
        assert_eq!(preferences.cushion_ms, DEFAULT_CUSHION_MS);

        let preset = &preferences.sessions[0];
        assert_eq!(preset.rate, DEFAULT_SAMPLE_RATE);
        assert_ne!(preset.rate, 1);
        assert_eq!(preset.buffer, DEFAULT_BUFFER);
        assert_ne!(preset.buffer, 0);
        assert_eq!(preset.output_rate, DEFAULT_SAMPLE_RATE);
        assert_ne!(preset.output_buffer, 9);
        assert_eq!(preset.output_buffer, DEFAULT_BUFFER);
        assert!(preset.output_buffer >= MIN_OUTPUT_BUFFER);
        assert!(preset.input_gain.is_finite());
        assert_eq!(preset.input_gain, DEFAULT_INPUT_GAIN);
        assert_eq!(preset.output_gain, OUTPUT_GAIN_MIN_DB);
        assert!(preset.output_gain.is_finite());
        assert_eq!(preset.cushion_ms, CUSHION_MS_MAX);

        // A listed size is kept, and so is a step that is not on the combo
        // but still inside the output floor and the largest offered size.
        let kept = validate_bridge_settings(
            BridgeSettings {
                sample_rate: 96_000,
                buffer_size: 64,
                output_rate: 0,
                output_buffer: 16,
                input_gain: 100.0,
                output_gain: -100.0,
                cushion_ms: 7.5,
            },
            OutputBufferZero::Invalid,
        );
        assert_eq!(kept.sample_rate, 96_000);
        assert_eq!(kept.buffer_size, 64);
        assert_eq!(kept.output_rate, 0);
        assert_eq!(kept.output_buffer, MIN_OUTPUT_BUFFER);
        assert_eq!(kept.input_gain, INPUT_GAIN_MAX_DB);
        assert_eq!(kept.output_gain, OUTPUT_GAIN_MIN_DB);
        assert_eq!(kept.cushion_ms, 7.5);

        let huge = validate_bridge_settings(
            BridgeSettings {
                sample_rate: 48_000,
                buffer_size: 4_096,
                output_rate: 48_000,
                output_buffer: 4_096,
                input_gain: 0.0,
                output_gain: 0.0,
                cushion_ms: 10.0,
            },
            OutputBufferZero::FollowInput,
        );
        assert_eq!(huge.buffer_size, DEFAULT_BUFFER);
        assert_ne!(huge.buffer_size, 4_096);
        assert_eq!(huge.output_buffer, DEFAULT_BUFFER);
        assert!(RATES.contains(&DEFAULT_SAMPLE_RATE));
        assert!(BUFFERS.contains(&DEFAULT_BUFFER));
        assert_eq!(MAX_SANE_BUFFER, *BUFFERS.last().unwrap());
    }

    #[test]
    fn a_full_preset_round_trips_routes_gains_and_plugins() {
        let mut full = preset("Gig");
        full.input_gain = 6.0;
        full.output_gain = -12.0;
        full.input_channels = [true, false];
        full.output_channels = [false, true];
        full.mono = true;
        full.mute = true;
        full.monitor = false;
        full.drift = false;
        full.low_latency_output = false;
        full.routes = [[false, true, false, true], [true, false, true, false]];
        full.cushion_mode = 2;
        full.cushion_ms = 7.5;
        full.plugins = vec![plugin("C:/Plugins/Amp.vst3", PluginFormat::Vst3)];
        let preferences = Preferences {
            sessions: vec![full.clone()],
            ..Preferences::default()
        };
        let loaded = load_preferences(Some(&stored(&preferences)), BACKUP);
        assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
        let back = &loaded.preferences.expect("readable").sessions[0];
        assert_eq!(back.input_id, full.input_id);
        assert_eq!(back.input_backend, full.input_backend);
        assert_eq!(back.output_id, full.output_id);
        assert_eq!(back.output_backend, full.output_backend);
        assert_eq!(back.input_gain, 6.0);
        assert_eq!(back.output_gain, -12.0);
        assert_eq!(back.input_channels, [true, false]);
        assert_eq!(back.output_channels, [false, true]);
        assert!(back.mono);
        assert!(back.mute);
        assert!(!back.monitor);
        assert!(!back.drift);
        assert!(!back.low_latency_output);
        assert_eq!(back.routes, full.routes);
        assert_eq!(back.cushion_mode, 2);
        assert_eq!(back.cushion_ms, 7.5);
        assert_eq!(back.plugins.len(), 1);
        assert_eq!(back.plugins[0].path, "C:/Plugins/Amp.vst3");
        assert_eq!(back.plugins[0].mix, 0.5);
        assert_eq!(back.rate, 96_000);
        assert_eq!(back.buffer, 64);
    }

    #[test]
    fn corrupt_preset_is_skipped_and_the_rest_kept() {
        let raw = r#"(sessions:[(name:"Good one",rate:48000,buffer:64),(name:12,rate:"fast"),"junk",(name:"Good two")],session_name:"Mine",buffer_size:256,plugins:[(path:"C:/A.vst3",name:"A")])"#;
        let loaded = load_preferences(Some(raw), BACKUP);
        let preferences = loaded.preferences.expect("readable");
        let names: Vec<_> = preferences
            .sessions
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, ["Good one", "Good two"]);
        assert_eq!(preferences.sessions[0].buffer, 64);
        assert_eq!(preferences.session_name, "Mine");
        assert_eq!(preferences.buffer_size, 256);
        assert_eq!(preferences.plugins.len(), 1);
        assert_eq!(loaded.notices.len(), 1);
        assert!(
            loaded.notices[0].contains("Skipped 2 saved patch"),
            "{}",
            loaded.notices[0]
        );
        // The skipped entries would vanish on the next save: keep the original.
        assert_eq!(loaded.backup, Some((BACKUP.to_owned(), raw.to_owned())));
    }

    #[test]
    fn corrupt_plugin_entry_is_skipped_and_missing_name_filled_in() {
        let raw = r#"(sessions:[(name:"Clean")],plugins:[(path:"C:/Plugins/Amp.vst3",format:clap,name:"Amp",mix:0.75),(name:"No path"),(path:"C:/B.vst3",name:"B",mix:"loud"),(path:"C:/Plugins/Verb.vst3")])"#;
        let loaded = load_preferences(Some(raw), BACKUP);
        let preferences = loaded.preferences.expect("readable");
        assert_eq!(preferences.sessions.len(), 1);
        assert_eq!(preferences.plugins.len(), 2);
        assert_eq!(preferences.plugins[0].name, "Amp");
        assert_eq!(preferences.plugins[0].format, PluginFormat::Clap);
        assert_eq!(preferences.plugins[0].mix, 0.75);
        assert_eq!(preferences.plugins[1].name, "Verb");
        assert_eq!(preferences.plugins[1].mix, 1.0);
        assert!(
            loaded.notices[0].contains("2 plugin insert"),
            "{}",
            loaded.notices[0]
        );
        assert!(loaded.backup.is_some());
    }

    #[test]
    fn unreadable_preferences_are_kept_not_overwritten() {
        let raw = r#"(sessions:[(name:"Clean")],sample_rate:"#;
        let mut storage = MemoryStorage::default();
        eframe::Storage::set_string(&mut storage, PREFERENCES_KEY, raw.to_owned());

        let mut loaded = load_preferences(
            eframe::Storage::get_string(&storage, PREFERENCES_KEY).as_deref(),
            BACKUP,
        );
        assert!(loaded.preferences.is_none());
        assert_eq!(loaded.notices.len(), 1);
        assert!(loaded.notices[0].contains(BACKUP));

        // The app's autosave with the defaults it fell back to.
        store_preferences(&mut storage, &Preferences::default(), &mut loaded.backup);
        assert_eq!(storage.0.get(BACKUP).map(String::as_str), Some(raw));
        let saved = storage.0.get(PREFERENCES_KEY).expect("saved");
        assert!(load_preferences(Some(saved), BACKUP).preferences.is_some());

        // Written once: later saves do not touch the copy again.
        assert!(loaded.backup.is_none());
        storage
            .0
            .insert(BACKUP.to_owned(), "moved by the user".to_owned());
        store_preferences(&mut storage, &Preferences::default(), &mut loaded.backup);
        assert_eq!(storage.0[BACKUP], "moved by the user");
    }

    #[test]
    fn newer_layout_keeps_a_copy() {
        let raw = r#"(schema_version:99,session_name:"Future",some_new_field:[1,2])"#;
        let loaded = load_preferences(Some(raw), BACKUP);
        let preferences = loaded.preferences.expect("readable");
        assert_eq!(preferences.session_name, "Future");
        assert_eq!(preferences.schema_version, PREFERENCES_SCHEMA);
        assert_eq!(loaded.notices.len(), 1);
        assert!(loaded.backup.is_some());
    }

    #[test]
    fn the_diagnostics_page_round_trips() {
        assert_eq!(page_key(Page::Diagnostics), "diagnostics");
        assert!(page_from_key("diagnostics") == Page::Diagnostics);
        assert!(page_from_key(" Diagnostics ") == Page::Diagnostics);
    }

    fn output_devices() -> (OutputDeviceSettings, Vec<FollowerPref>) {
        let clock = OutputDeviceSettings {
            level_db: -6.0,
            mute: true,
            map: ChannelMap {
                left: 0b0101,
                right: 0b1010,
            },
            unused: UnusedChannels::Silent,
        };
        let followers = vec![
            FollowerPref {
                id: "hw:Phones".to_owned(),
                backend: "ALSA".to_owned(),
                name: "Phones".to_owned(),
                settings: OutputDeviceSettings {
                    level_db: -3.0,
                    ..OutputDeviceSettings::default()
                },
                cushion_mode: 2,
                cushion_ms: 20.0,
            },
            FollowerPref {
                id: "pw:rigwerk-out".to_owned(),
                backend: "PipeWire".to_owned(),
                name: "RigWerk Out".to_owned(),
                settings: OutputDeviceSettings {
                    mute: true,
                    map: ChannelMap { left: 1, right: 1 },
                    ..OutputDeviceSettings::default()
                },
                cushion_mode: 1,
                cushion_ms: DEFAULT_CUSHION_MS,
            },
        ];
        (clock, followers)
    }

    #[test]
    fn old_saves_without_output_devices_use_defaults() {
        let raw = r#"(output_id:"hw:Speaker",output_gain:-6.0,mute:true,sessions:[(name:"Old",output_id:"hw:Speaker")])"#;
        let loaded = load_preferences(Some(raw), BACKUP);
        assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
        let preferences = loaded.preferences.expect("readable");
        let today = OutputDeviceSettings {
            level_db: 0.0,
            mute: false,
            map: ChannelMap {
                left: 0b01,
                right: 0b10,
            },
            unused: UnusedChannels::MonoCopy,
        };
        assert_eq!(OutputDeviceSettings::default(), today);
        assert_eq!(preferences.clock_device, today);
        assert!(preferences.followers.is_empty());
        // The master stays where it was.
        assert_eq!(preferences.output_gain, -6.0);
        assert!(preferences.mute);
        assert_eq!(preferences.sessions[0].clock_device, today);
        assert!(preferences.sessions[0].followers.is_empty());
    }

    #[test]
    fn unknown_unused_channel_mode_reads_as_mono_copy() {
        let raw = r#"(session_name:"Mine",sample_rate:96000,clock_device:(level_db:-2.0,unused:loud),followers:[(id:"hw:A",settings:(unused:silent))])"#;
        let loaded = load_preferences(Some(raw), BACKUP);
        assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
        let preferences = loaded.preferences.expect("readable");
        assert_eq!(preferences.clock_device.unused, UnusedChannels::MonoCopy);
        assert_eq!(preferences.clock_device.level_db, -2.0);
        assert_eq!(
            preferences.followers[0].settings.unused,
            UnusedChannels::Silent
        );
        assert_eq!(preferences.session_name, "Mine");
        assert_eq!(preferences.sample_rate, 96_000);
    }

    #[test]
    fn output_devices_round_trip_in_preferences_and_presets() {
        let (clock, followers) = output_devices();
        let mut session = preset("Two outs");
        session.clock_device = clock;
        session.followers = followers.clone();
        let preferences = Preferences {
            clock_device: clock,
            followers: followers.clone(),
            sessions: vec![session],
            ..Preferences::default()
        };
        let loaded = load_preferences(Some(&stored(&preferences)), BACKUP);
        assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
        let back = loaded.preferences.expect("readable");
        assert_eq!(back.clock_device, clock);
        assert_eq!(back.followers, followers);
        assert_eq!(back.sessions[0].clock_device, clock);
        assert_eq!(back.sessions[0].followers, followers);
    }

    #[test]
    fn unreadable_follower_is_skipped_and_the_rest_kept() {
        let raw = r#"(
            clock_device:(level_db:NaN,map:(left:3)),
            sessions:[
                (name:"Hot",clock_device:(level_db:20.0)),
                (name:"P",followers:[(id:7),(id:"hw:D",settings:(level_db:-100.0))]),
            ],
            followers:[
                (id:"hw:A",name:"A",settings:(unused:silent)),
                (id:"hw:B",settings:(mute:"yes")),
                "junk",
                (id:"hw:C",cushion_ms:inf),
            ],
        )"#;
        let loaded = load_preferences(Some(raw), BACKUP);
        let preferences = loaded.preferences.expect("readable");
        let ids: Vec<_> = preferences
            .followers
            .iter()
            .map(|f| f.id.as_str())
            .collect();
        assert_eq!(ids, ["hw:A", "hw:C"]);
        assert_eq!(
            preferences.followers[0].settings.unused,
            UnusedChannels::Silent
        );
        assert_eq!(preferences.followers[1].cushion_ms, CUSHION_MS_MAX);
        assert_eq!(preferences.clock_device.level_db, 0.0);
        assert_eq!(preferences.clock_device.map.left, 3);
        assert_eq!(preferences.clock_device.map.right, 0b10);
        assert_eq!(preferences.sessions[0].clock_device.level_db, 6.0);
        let session = &preferences.sessions[1].followers;
        assert_eq!(session.len(), 1);
        assert_eq!(session[0].settings.level_db, -36.0);
        assert!(
            loaded.notices[0].starts_with("Skipped 2 output device(s) that could not be read."),
            "{}",
            loaded.notices[0]
        );
        assert!(loaded.backup.is_some());
    }

    #[test]
    fn nothing_stored_is_quiet() {
        let loaded = load_preferences(None, BACKUP);
        assert!(loaded.preferences.is_none());
        assert!(loaded.backup.is_none());
        assert!(loaded.notices.is_empty());
    }
}
