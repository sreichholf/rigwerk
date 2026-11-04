//! What survives a restart: preferences, saved patches and the choices
//! offered for rates and buffers.

use super::Page;
use crate::audio::{CushionMode, EqSettings, PluginPref};

/// Launch preferences that survive restarts.  The bridge stays stopped unless
/// the person explicitly asks for it to start with the app.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct Preferences {
    pub(crate) auto_start_bridge: bool,
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
    pub(crate) asio_name: Option<String>,
    pub(crate) input_name: Option<String>,
    pub(crate) output_name: Option<String>,
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
    /// The master insert chain (paths and settings, not live plugins).
    pub(crate) plugins: Vec<PluginPref>,
    /// Open a plugin's own interface directly instead of RigWerk's control
    /// panel, when the plugin has one that can be embedded.
    pub(crate) prefer_plugin_interface: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            auto_start_bridge: false,
            sessions: Vec::new(),
            session_name: "My guitar rig".to_owned(),
            page: "live".to_owned(),
            eq_edit: 0,
            output_tab_low_latency: cfg!(target_os = "linux"),
            selected_session: None,
            asio_name: None,
            input_name: None,
            output_name: None,
            use_asio_input: true,
            sample_rate: 48_000,
            buffer_size: 128,
            input_gain: 0.0,
            output_gain: -1.5,
            input_channels: [true, true],
            output_channels: [true, true],
            mono: false,
            mute: false,
            drift: true,
            monitor: true,
            low_latency_output: true,
            routes: [[true, false, false, false], [false, true, false, false]],
            output_rate: 0,
            output_buffer: 128,
            cushion_mode: 0,
            cushion_ms: 10.0,
            eq: EqSettings::default(),
            eq_post: EqSettings::default(),
            plugins: Vec::new(),
            prefer_plugin_interface: false,
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

/// Rates offered on either side of the bridge.
pub(crate) const RATES: [u32; 4] = [44_100, 48_000, 88_200, 96_000];

/// Buffer sizes offered on either side of the bridge, in samples.
pub(crate) const BUFFERS: [usize; 5] = [32, 64, 128, 256, 512];

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
        _ => Page::Live,
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct Preset {
    pub(crate) name: String,
    pub(crate) note: String,
    pub(crate) input: String,
    pub(crate) output: String,
    pub(crate) rate: u32,
    pub(crate) buffer: usize,
    /// Output side; 0 in patches saved before the sides were separate means
    /// "follow the input".
    #[serde(default)]
    pub(crate) output_rate: u32,
    #[serde(default)]
    pub(crate) output_buffer: usize,
    /// The EQs saved with the patch (pre and post inserts).  Patches from
    /// before an EQ existed have none and leave the current one alone.
    #[serde(default)]
    pub(crate) eq: Option<EqSettings>,
    #[serde(default)]
    pub(crate) eq_post: Option<EqSettings>,
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
