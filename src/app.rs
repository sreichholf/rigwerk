//! This file does not paint.  The frame shell is `ui/frame.rs`.

pub(crate) mod alignment;
pub(crate) mod appearance;
pub(crate) mod bridge;
pub(crate) mod budget;
pub(crate) mod cab;
pub(crate) mod calibration;
pub(crate) mod chain;
pub(crate) mod devices;
pub(crate) mod diagnostics;
pub(crate) mod driver_check;
pub(crate) mod issues;
pub(crate) mod latency;
pub(crate) mod layouts;
pub(crate) mod match_paths;
pub(crate) mod meters;
pub(crate) mod nam;
pub(crate) mod output_devices;
pub(crate) mod paths;
pub(crate) mod plugin_gui;
pub(crate) mod plugins;
pub(crate) mod record;
pub(crate) mod screen;
pub(crate) mod screenshot;
pub(crate) mod sessions;
pub(crate) mod settings;
pub(crate) mod startup;
pub(crate) mod tuner;

use std::path::PathBuf;
use std::sync::atomic::AtomicI32;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use eframe::egui;

use crate::app::devices::{inventory_fingerprint, restore_extra_output};
use crate::app::screenshot::ScreenshotJob;
use crate::app::settings::{
    bridge_buffer_from_pref, bridge_buffer_to_pref, load_preferences, page_from_key, page_key,
    resolved_wasapi_output_mode, store_preferences, store_preferences_backup, unique_sessions,
    ExtraOutputPref, Preferences, Session, PREFERENCES_BACKUP_KEY, PREFERENCES_KEY,
    PREFERENCES_SCHEMA,
};
use crate::app::startup::{persisted_identity, restore_saved_devices, StartupGate};
use crate::audio::{
    self, AudioEngine, BufferMode, EngineState, EqSettings, EqSlot, GateSettings, Inventory,
    OutputDeviceSettings, Snapshot, WasapiOutputMode, MAX_EXTRA_OUTPUTS, MAX_PATHS,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Page {
    Console,
    Tuner,
    Setup,
}

pub(crate) fn initial_page() -> Page {
    page_for_run(&std::env::var("RIGWERK_PAGE").unwrap_or_default())
}

/// An unknown key, and the old page names, open the console.
pub(crate) fn page_for_run(key: &str) -> Page {
    match key.trim().to_lowercase().as_str() {
        "tuner" => Page::Tuner,
        "setup" | "devices" | "guide" => Page::Setup,
        _ => Page::Console,
    }
}

/// Wins over the saved theme for this run and is not saved.
fn theme_override() -> Option<appearance::ThemeChoice> {
    let key = std::env::var("RIGWERK_THEME").ok()?;
    match key.trim().to_lowercase().as_str() {
        "dark" => Some(appearance::ThemeChoice::Dark),
        "light" => Some(appearance::ThemeChoice::Light),
        _ => None,
    }
}

fn restored_page(preferences: &Preferences, screenshot_mode: bool) -> Page {
    if screenshot_mode || std::env::var_os("RIGWERK_PAGE").is_some() {
        return initial_page();
    }
    page_from_key(&preferences.page)
}

const ACTIVE_REPAINT: Duration = Duration::from_millis(16);
/// Idle and Error stay slow so polling, plugin service and device
/// watching keep running while the window is hidden.
const IDLE_REPAINT: Duration = Duration::from_millis(200);

fn repaint_interval(state: EngineState, plugin_editor_open: bool) -> Duration {
    if plugin_editor_open
        || matches!(
            state,
            EngineState::Starting
                | EngineState::Running
                | EngineState::Stopping
                | EngineState::Preview
        )
    {
        ACTIVE_REPAINT
    } else {
        IDLE_REPAINT
    }
}

/// Kept in the hundreds so a failed scan outlasts ordinary bridge chatter.
pub(crate) const ACTIVITY_LOG_CAP: usize = 400;

/// Diagnostics maps this to a theme colour.  The log stores no `Color32`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LogTone {
    Muted,
    Blue,
    Amber,
    Teal,
    Green,
    Red,
}

pub(crate) struct LogEntry {
    pub(crate) at: String,
    pub(crate) text: String,
    pub(crate) tone: LogTone,
}

/// `engine` is declared first and would take the plugins down while an
/// editor is still attached.  Detach the view before that drop.
impl Drop for RigWerkApp {
    fn drop(&mut self) {
        self.plugin_gui = None;
    }
}

pub struct RigWerkApp {
    pub(crate) page: Page,
    /// The tuner logs its mute layer when this differs from `page`.
    pub(crate) last_page: Page,
    pub(crate) engine: AudioEngine,
    pub(crate) tuner: tuner::TunerState,
    pub(crate) devices: devices::Devices,
    pub(crate) sample_rate: u32,
    pub(crate) buffer_size: usize,
    pub(crate) input_gain: f32,
    pub(crate) output_gain: f32,
    pub(crate) input_channels: [bool; 2],
    pub(crate) mute: bool,
    /// Mixed onto device channels, so the master mute does not stop it.
    pub(crate) metronome_running: bool,
    pub(crate) metronome_bpm: f32,
    pub(crate) metronome_rhythm: crate::audio::ClickRhythm,
    pub(crate) metronome_level_db: f32,
    pub(crate) theme: appearance::ThemeChoice,
    pub(crate) ui_size: f32,
    pub(crate) effect_double_click: settings::EffectOpen,
    pub(crate) last_click_mask: u8,
    pub(crate) chrome: screen::Chrome,
    /// Re-pushed only when the tuner's auto-mute changes it.
    pub(crate) metronome_pushed: Option<bool>,
    pub(crate) drift: bool,
    /// On Windows this stays aligned with [`Self::wasapi_output_mode`]
    /// so a cross-platform save still round-trips.
    pub(crate) low_latency_output: bool,
    pub(crate) wasapi_output_mode: crate::audio::WasapiOutputMode,
    /// 0 follows the input.
    pub(crate) output_rate: u32,
    pub(crate) output_buffer: usize,
    pub(crate) bridge_buffer_mode: BufferMode,
    pub(crate) bridge_buffer_ms: f32,
    /// At least one is always `Some`.
    pub(crate) paths: [Option<paths::SignalPath>; MAX_PATHS],
    /// Display order, which is also the save order.
    pub(crate) path_order: Vec<usize>,
    pub(crate) selected_path: usize,
    /// Clock output first, then the extra outputs.  Runtime only.
    pub(crate) output_trim_origins: [output_devices::OutputTrimOrigin; MAX_EXTRA_OUTPUTS + 1],
    pub(crate) clock_output: OutputDeviceSettings,
    pub(crate) extra_outputs: Vec<ExtraOutputPref>,
    pub(crate) loudness_measurement: Option<output_devices::LoudnessMeasurement>,
    pub(crate) match_paths: match_paths::MatchPathsSettings,
    /// A live slot.  Saved as a position.
    pub(crate) match_paths_reference: Option<usize>,
    pub(crate) match_paths_result: Option<match_paths::MatchPathsResult>,
    pub(crate) match_paths_ticks: Vec<bool>,
    pub(crate) match_paths_status: Option<String>,
    pub(crate) eq_edit: EqSlot,
    pub(crate) chain_selected: chain::ChainSelection,
    /// Not persisted.
    pub(crate) routing_device: usize,
    /// Falls back to the selected path.
    pub(crate) routing_preset_path: Option<usize>,
    /// For "Custom · edited from <layout>".
    pub(crate) routing_last_layout: Option<crate::app::layouts::PathLayout>,
    pub(crate) issue_state: crate::app::issues::IssueState,
    /// `None` appends.
    pub(crate) plugin_effect_at: Option<usize>,
    pub(crate) meters: meters::Meters,
    pub(crate) snapshot: Snapshot,
    /// Recomputed at startup, when the output changes, and once the bridge
    /// reports its real period.
    #[cfg(target_os = "linux")]
    pub(crate) quantum: Option<audio::QuantumInfo>,
    pub(crate) session_name: String,
    pub(crate) record_folder: String,
    pub(crate) nam_folder: String,
    /// Not refreshed every frame.
    pub(crate) nam_catalog: nam::NamCatalog,
    /// Scan before drawing.
    pub(crate) nam_rescan: bool,
    pub(crate) native_window: Option<nam::NativeWindow>,
    /// A held chord pastes once.
    pub(crate) nam_paste_down: bool,
    /// Started the first time a block needs one.
    pub(crate) nam_loader: Option<nam::NamLoader>,
    /// Waiting because the path already runs
    /// [`MAX_NAM_BLOCKS`](crate::audio::MAX_NAM_BLOCKS) of DSP.
    pub(crate) nam_confirm: Option<usize>,
    /// Dismissed or acted on once.
    pub(crate) nam_warning: Option<nam::NamLoadWarning>,
    pub(crate) cab_folder: String,
    /// Not refreshed every frame.
    pub(crate) cab_catalog: cab::CabCatalog,
    /// Scan before drawing.
    pub(crate) cab_rescan: bool,
    /// A held chord pastes once.
    pub(crate) cab_paste_down: bool,
    pub(crate) record_click: bool,
    pub(crate) sessions: Vec<Session>,
    pub(crate) selected_session: Option<usize>,
    pub(crate) pending_delete: Option<usize>,
    pub(crate) log: Vec<LogEntry>,
    pub(crate) started: Instant,
    pub(crate) screenshot: Option<ScreenshotJob>,
    pub(crate) frames: u64,
    pub(crate) show_about: bool,
    pub(crate) auto_start_bridge: bool,
    pub(crate) bridge_signature: u64,
    /// Start again only once the engine is Idle: the old driver is still
    /// releasing its device.
    pub(crate) bridge_restart_pending: bool,
    pub(crate) driver_checked: bool,
    /// `None` as the reason is a device that went away while no bridge ran.
    pub(crate) device_alert: Option<(String, Option<issues::StopReason>)>,
    pub(crate) driver_probe: Option<driver_check::DriverCheck>,
    pub(crate) driver_probe_job: Option<mpsc::Receiver<driver_check::DriverCheck>>,
    /// So the DRIVER line still shows the true step after Stop.
    pub(crate) last_output_driver: Option<(String, driver_check::DriverActual)>,
    pub(crate) plugin_browser: plugins::PluginBrowser,
    pub(crate) plugin_editor: Option<paths::EffectKey>,
    pub(crate) plugin_gui: Option<plugin_gui::PluginGuiWindow>,
    /// A CLAP editor whose window reports no DPI is scaled by this.
    pub(crate) ui_scale: f32,
    pub(crate) plugin_param_filter: String,
    /// Written at the start of the next save so that save cannot lose it.
    preferences_backup: Option<(String, String)>,
    /// Above [`PREFERENCES_SCHEMA`], this session writes only the backup.
    stored_schema_version: u32,
    pub(crate) measured_latency: Option<latency::MeasuredLatency>,
    pub(crate) calibration: calibration::CalibrationUi,
    /// So the UI does not ask the device again on every frame.
    #[cfg(target_os = "linux")]
    period_cache: Option<(String, bool, usize, u32)>,
    #[cfg(target_os = "linux")]
    quantum_checked_key: Option<(String, bool, usize)>,
    #[cfg(target_os = "linux")]
    quantum_live_at: Instant,
    /// So the window can paint before hosts are probed.
    pub(crate) startup: Option<StartupGate>,
    #[cfg(debug_assertions)]
    rt_allocations_logged: bool,
}

impl RigWerkApp {
    /// eframe calls this while the window is hidden, so it keeps running
    /// with no egui pass.
    pub(crate) fn run_logic(&mut self, ctx: &egui::Context) {
        self.poll_calibration();
        self.log_tuner_page();
        self.tuner.poll(
            &self.engine,
            self.sample_rate,
            self.page,
            self.screenshot.is_some(),
            self.started.elapsed().as_secs_f32(),
        );
        self.watch_devices();
        self.poll_plugin_scan();
        self.engine.service_plugins();
        self.poll_nam_loader();
        self.engine.flush_parks();
        self.sync_nam_models();
        self.sync_cab_files();
        self.ui_scale = ctx.pixels_per_point();
        self.poll_plugin_gui();
        self.sync_bridge_config();
        self.tick_issues(Instant::now());
        #[cfg(debug_assertions)]
        self.log_audio_thread_allocations();
        let plugin_editor_open = self.plugin_editor.is_some() || self.plugin_gui.is_some();
        let interval = if self.startup.is_some() {
            Duration::from_millis(16)
        } else {
            repaint_interval(self.engine.state, plugin_editor_open)
        };
        ctx.request_repaint_after(interval);
    }

    /// A newer build's file stays byte-identical; only the backup is
    /// written.  A file too damaged to yield a layout number is replaced,
    /// and its original text stays in that backup.
    pub(crate) fn save_to(&mut self, storage: &mut dyn eframe::Storage) {
        // A screenshot run must not store its fixtures as the session.
        if self.screenshot.is_some() {
            return;
        }
        // Before the gate: a quit during the first scan must keep the
        // original text a damaged load could not read.
        store_preferences_backup(storage, &mut self.preferences_backup);
        // A newer build's layout is not this build's to write.
        if self.stored_schema_version > PREFERENCES_SCHEMA {
            return;
        }
        // Placeholder rows must not overwrite the stored pair while the
        // first scan is still running.
        if self.startup.is_some() {
            return;
        }
        // An open editor may have changed a value without reporting it.
        // A side the scan did not find keeps its stored identity.  The
        // on-screen row can still be the fallback.
        let (asio_name, asio_id, asio_backend) =
            persisted_identity(self.devices.asio_held.as_ref(), Some(self.selected_asio()));
        let (input_name, input_id, input_backend) = persisted_identity(
            self.devices.input_held.as_ref(),
            self.devices.inventory.inputs.get(self.devices.input_idx),
        );
        let (output_name, output_id, output_backend) = persisted_identity(
            self.devices.output_held.as_ref(),
            Some(self.selected_output()),
        );
        let (paths, order) = self.saved_paths();
        let selected_path = self.saved_selected_path(&order);
        let (clock_output, extra_outputs) = self.saved_devices(&order);
        let preferences = Preferences {
            schema_version: PREFERENCES_SCHEMA,
            auto_start_bridge: self.auto_start_bridge,
            sessions: self.sessions.clone(),
            session_name: self.session_name.clone(),
            page: page_key(self.page).to_owned(),
            eq_edit: self.eq_edit.index() as u8,
            output_tab_low_latency: self.devices.output_tab_low_latency,
            selected_session: self.selected_session,
            selected_session_name: self
                .selected_session
                .and_then(|index| self.sessions.get(index))
                .map(|session| session.name.clone()),
            asio_name,
            asio_id,
            asio_backend,
            input_name,
            input_id,
            input_backend,
            output_name,
            output_id,
            output_backend,
            use_asio_input: self.devices.use_asio_input,
            sample_rate: self.sample_rate,
            buffer_size: self.buffer_size,
            input_gain: self.input_gain,
            output_gain: self.output_gain,
            input_channels: self.input_channels,
            mute: self.mute,
            drift: self.drift,
            low_latency_output: self.low_latency_output,
            wasapi_output_mode: Some(if cfg!(target_os = "windows") {
                self.wasapi_output_mode
            } else {
                WasapiOutputMode::from_low_latency(self.low_latency_output)
            }),
            output_rate: self.output_rate,
            output_buffer: self.output_buffer,
            bridge_buffer_mode: bridge_buffer_to_pref(self.bridge_buffer_mode),
            bridge_buffer_ms: self.bridge_buffer_ms,
            match_paths: self.saved_match_paths(&order),
            paths,
            selected_path,
            clock_output,
            extra_outputs,
            measured_latency: self.measured_latency.clone(),
            tuner_tuning: self.tuner.preset_id().to_owned(),
            tuner_reference_hz: self.tuner.reference_hz,
            tuner_channel: self.tuner.channel.to_u8(),
            tuner_display: self.tuner.display.to_u8(),
            tuner_listen: self.tuner.listen.to_u8(),
            tuner_auto_mute: self.tuner.auto_mute,
            tuner_tolerance: self.tuner.tolerance.to_u8(),
            record_folder: self.record_folder.clone(),
            amp_folder: self.nam_folder.clone(),
            cab_folder: self.cab_folder.clone(),
            record_click: self.record_click,
            metronome_bpm: self.metronome_bpm,
            metronome_rhythm: self.metronome_rhythm,
            metronome_level_db: self.metronome_level_db,
            theme: self.theme,
            ui_size: self.ui_size,
            effect_double_click: self.effect_double_click,
            effect_favourites: self.plugin_browser.favourites.clone(),
            effect_recent: self.plugin_browser.recent.clone(),
            sheet_height: self.chrome.sheet_height,
            last_click_mask: self.last_click_mask,
            // Read from old saves and never written.
            output_channels: [true, true],
            mono: false,
            routes: [[true, false, false, false], [false, true, false, false]],
            eq: EqSettings::default(),
            eq_post: EqSettings::default(),
            gate: GateSettings::default(),
            plugins: Vec::new(),
            // A launch starts with the click stopped.
            metronome_running: false,
        };
        store_preferences(storage, &preferences);
    }
}

impl RigWerkApp {
    /// The one `RigWerkApp` literal.  It retypes [`Preferences::default`],
    /// so the two stay in step: a test on `blank_app` reads these values.
    fn base() -> Self {
        Self {
            page: Page::Console,
            last_page: Page::Console,
            engine: AudioEngine::new(48_000),
            tuner: tuner::TunerState::new(&Preferences::default(), false),
            devices: devices::Devices::new(),
            sample_rate: 48_000,
            buffer_size: 128,
            input_gain: 0.0,
            output_gain: -1.5,
            input_channels: [true, true],
            mute: false,
            metronome_running: false,
            metronome_bpm: crate::audio::METRONOME_BPM_DEFAULT,
            metronome_rhythm: crate::audio::ClickRhythm::FourFour,
            metronome_level_db: crate::audio::CLICK_LEVEL_DEFAULT_DB,
            theme: appearance::ThemeChoice::System,
            ui_size: appearance::UI_SIZE_DEFAULT,
            effect_double_click: settings::EffectOpen::Editor,
            last_click_mask: 0,
            chrome: screen::Chrome::default(),
            metronome_pushed: None,
            drift: true,
            low_latency_output: true,
            wasapi_output_mode: WasapiOutputMode::RealStyle,
            output_rate: 0,
            output_buffer: 128,
            bridge_buffer_mode: BufferMode::Auto,
            bridge_buffer_ms: 10.0,
            // `new` installs the saved paths over this.
            paths: std::array::from_fn(|_| None),
            path_order: Vec::new(),
            selected_path: 0,
            output_trim_origins: [output_devices::OutputTrimOrigin::None; MAX_EXTRA_OUTPUTS + 1],
            clock_output: OutputDeviceSettings::default(),
            extra_outputs: Vec::new(),
            loudness_measurement: None,
            match_paths: match_paths::MatchPathsSettings::default(),
            match_paths_reference: None,
            match_paths_result: None,
            match_paths_ticks: Vec::new(),
            match_paths_status: None,
            eq_edit: EqSlot::Pre,
            chain_selected: chain::ChainSelection::PreEq,
            routing_device: 0,
            routing_preset_path: None,
            routing_last_layout: None,
            issue_state: Default::default(),
            plugin_effect_at: None,
            meters: meters::Meters::new(),
            snapshot: Snapshot::default(),
            #[cfg(target_os = "linux")]
            quantum: None,
            session_name: "My guitar rig".to_owned(),
            record_folder: String::new(),
            nam_folder: String::new(),
            nam_catalog: nam::NamCatalog::default(),
            nam_rescan: true,
            native_window: None,
            nam_paste_down: false,
            nam_loader: None,
            nam_confirm: None,
            nam_warning: None,
            cab_folder: String::new(),
            cab_catalog: cab::CabCatalog::default(),
            cab_rescan: true,
            cab_paste_down: false,
            record_click: false,
            sessions: Vec::new(),
            selected_session: None,
            pending_delete: None,
            log: Vec::new(),
            started: Instant::now(),
            screenshot: None,
            frames: 0,
            show_about: false,
            auto_start_bridge: false,
            bridge_signature: 0,
            bridge_restart_pending: false,
            driver_checked: false,
            device_alert: None,
            driver_probe: None,
            driver_probe_job: None,
            last_output_driver: None,
            plugin_browser: plugins::PluginBrowser::default(),
            plugin_editor: None,
            plugin_gui: None,
            ui_scale: 1.0,
            plugin_param_filter: String::new(),
            preferences_backup: None,
            stored_schema_version: PREFERENCES_SCHEMA,
            measured_latency: None,
            calibration: calibration::CalibrationUi::default(),
            #[cfg(target_os = "linux")]
            period_cache: None,
            #[cfg(target_os = "linux")]
            quantum_checked_key: None,
            #[cfg(target_os = "linux")]
            quantum_live_at: Instant::now(),
            startup: None,
            #[cfg(debug_assertions)]
            rt_allocations_logged: false,
        }
    }

    /// `new` sets the inventory and the screenshot flag first.  `blank_app`
    /// never calls this, so a test leaves the engine untouched.
    fn apply_preferences(&mut self, preferences: &Preferences, stored_schema_version: u32) {
        // Every field is named, so a new preference is a compile error
        // until it has a destination here.
        let Preferences {
            // The value in memory is this build's number already.
            schema_version: _,
            auto_start_bridge: _,
            sessions: _,
            session_name: _,
            page: _,
            eq_edit: _,
            output_tab_low_latency: _,
            selected_session: _,
            selected_session_name: _,
            asio_name: _,
            asio_id: _,
            asio_backend: _,
            input_name: _,
            input_id: _,
            input_backend: _,
            output_name: _,
            output_id: _,
            output_backend: _,
            use_asio_input: _,
            sample_rate: _,
            buffer_size: _,
            input_gain: _,
            output_gain: _,
            input_channels: _,
            // Path 1 from a file with no `paths`.  `new` folds these in.
            output_channels: _output_channels,
            mono: _mono,
            mute: _,
            drift: _,
            low_latency_output: _,
            wasapi_output_mode: _,
            routes: _routes,
            output_rate: _,
            output_buffer: _,
            bridge_buffer_mode: _,
            bridge_buffer_ms: _,
            eq: _eq,
            eq_post: _eq_post,
            gate: _gate,
            plugins: _plugins,
            paths: _paths,
            selected_path: _selected_path,
            match_paths: _,
            clock_output: _,
            extra_outputs: _,
            measured_latency: _,
            tuner_tuning: _,
            tuner_reference_hz: _,
            tuner_channel: _,
            tuner_display: _,
            tuner_listen: _,
            tuner_auto_mute: _,
            tuner_tolerance: _,
            record_folder: _,
            amp_folder: _,
            cab_folder: _,
            record_click: _,
            metronome_running: _,
            metronome_bpm: _,
            metronome_rhythm: _,
            metronome_level_db: _,
            theme: _,
            ui_size: _,
            effect_favourites: _effect_favourites,
            effect_recent: _effect_recent,
            sheet_height: _,
            effect_double_click: _,
            last_click_mask: _,
        } = preferences;
        let screenshot = self.screenshot.is_some();
        let restored = restore_saved_devices(&self.devices.inventory, preferences);
        let wasapi_output_mode = resolved_wasapi_output_mode(preferences);
        // Already checked in `load_preferences`.
        self.page = restored_page(preferences, screenshot);
        self.last_page = self.page;
        self.tuner = tuner::TunerState::new(preferences, screenshot);
        self.devices.asio_idx = restored.asio_idx;
        self.devices.input_idx = restored.input_idx;
        self.devices.output_idx = restored.output_idx;
        self.devices.asio_held = restored.asio_held;
        self.devices.input_held = restored.input_held;
        self.devices.output_held = restored.output_held;
        self.devices.use_asio_input = preferences.use_asio_input;
        self.sample_rate = preferences.sample_rate;
        self.buffer_size = preferences.buffer_size;
        self.input_gain = preferences.input_gain;
        self.output_gain = preferences.output_gain;
        self.input_channels = preferences.input_channels;
        self.mute = preferences.mute;
        self.metronome_running = preferences.metronome_running;
        self.metronome_bpm = preferences.metronome_bpm;
        self.metronome_rhythm = preferences.metronome_rhythm;
        self.metronome_level_db = preferences.metronome_level_db;
        self.theme = preferences.theme;
        self.ui_size = preferences.ui_size;
        self.effect_double_click = preferences.effect_double_click;
        self.last_click_mask = preferences.last_click_mask;
        self.chrome = screen::Chrome::at_launch(preferences.sheet_height);
        self.drift = preferences.drift;
        self.low_latency_output = if cfg!(target_os = "windows") {
            matches!(wasapi_output_mode, WasapiOutputMode::RealStyle)
        } else {
            preferences.low_latency_output
        };
        self.wasapi_output_mode = wasapi_output_mode;
        self.output_rate = preferences.output_rate;
        self.output_buffer = preferences.output_buffer;
        self.devices.output_tab_low_latency = preferences.output_tab_low_latency;
        self.bridge_buffer_mode = bridge_buffer_from_pref(preferences.bridge_buffer_mode);
        self.bridge_buffer_ms = preferences.bridge_buffer_ms;
        self.clock_output = preferences.clock_output.sanitized();
        let mut extra_outputs: Vec<_> = preferences
            .extra_outputs
            .iter()
            .cloned()
            .map(ExtraOutputPref::sanitized)
            .collect();
        extra_outputs.truncate(crate::audio::MAX_EXTRA_OUTPUTS);
        self.extra_outputs = extra_outputs;
        // The inventory is still the placeholder.  The first real scan
        // adopts the saved ids.
        self.match_paths = preferences.match_paths.sanitized();
        let eq_edit = if preferences.eq_edit == 1 {
            EqSlot::Post
        } else {
            EqSlot::Pre
        };
        self.eq_edit = eq_edit;
        self.chain_selected = chain::ChainSelection::from_eq_edit(eq_edit);
        self.session_name = preferences.session_name.clone();
        self.record_folder = preferences.record_folder.clone();
        self.nam_folder = preferences.amp_folder.clone();
        self.cab_folder = preferences.cab_folder.clone();
        self.record_click = preferences.record_click;
        // A skipped session or a dropped duplicate moves the index, so
        // the stored name is what the choice follows.  An older file has
        // no name and falls back to the index.
        self.sessions = unique_sessions(&preferences.sessions);
        self.selected_session = preferences
            .selected_session_name
            .as_deref()
            .and_then(|name| self.session_named(name))
            .or_else(|| {
                preferences
                    .selected_session
                    .and_then(|index| preferences.sessions.get(index))
                    .and_then(|session| self.session_named(&session.name))
            });
        self.auto_start_bridge = preferences.auto_start_bridge && !screenshot;
        self.measured_latency = preferences.measured_latency.clone();
        self.stored_schema_version = stored_schema_version;
    }

    pub fn new(
        cc: &eframe::CreationContext<'_>,
        screenshot: Option<PathBuf>,
        screenshot_exit: Arc<AtomicI32>,
    ) -> Self {
        // This is the thread that services CLAP plugins.  Record it before
        // any plugin can load.
        crate::audio::mark_main_thread();
        // A screenshot run never opens audio hardware, so it starts from
        // defaults.
        let raw_preferences = if screenshot.is_some() {
            None
        } else {
            cc.storage
                .and_then(|storage| storage.get_string(PREFERENCES_KEY))
        };
        let loaded = load_preferences(raw_preferences.as_deref(), PREFERENCES_BACKUP_KEY);
        let preferences = loaded.preferences.clone().unwrap_or_default();
        // Keep real drivers closed even if auto-start was left on.
        let screenshot_mode = screenshot.is_some();
        let mut app = Self::base();
        // Paint before hosts are probed.  Restore and auto-start wait
        // for the scan thread.  The screenshot flag is set first: the
        // page and auto-start read it.
        app.devices.inventory = audio::placeholder_inventory();
        app.devices.inventory_fingerprint = inventory_fingerprint(&app.devices.inventory);
        app.screenshot = screenshot.map(|path| ScreenshotJob::new(path, screenshot_exit));
        app.apply_preferences(&preferences, loaded.stored_schema_version);
        app.preferences_backup = loaded.backup;
        app.startup = Some(StartupGate {
            preferences: preferences.clone(),
        });
        // egui restored its own memory before this; the saved preference wins.
        app.plugin_browser.favourites = preferences.effect_favourites.clone();
        app.plugin_browser.recent = preferences.effect_recent.clone();
        app.apply_appearance(&cc.egui_ctx, theme_override());
        app.apply_gains();
        app.apply_metronome();
        app.engine.set_record_click(app.record_click);
        app.engine
            .set_master(app.mute, app.input_channels, app.drift);
        // An old file, and no file, load as one path.
        let (clock, extra_outputs) = (app.clock_output, app.extra_outputs.clone());
        app.install_paths(
            &preferences.path_prefs(),
            clock,
            extra_outputs,
            preferences.selected_path,
        );
        app.install_match_paths(preferences.match_paths);
        app.reset_output_trim_origins();
        app.apply_bridge_buffer();
        if !screenshot_mode {
            app.begin_plugin_scan();
        }
        app.push_log("Looking for audio devices…", LogTone::Blue);
        for notice in loaded.notices {
            app.push_log(notice, LogTone::Amber);
        }
        app.start_device_scan();
        app
    }

    pub(crate) fn finish_startup_scan(&mut self, fresh: Inventory) {
        let Some(gate) = self.startup.take() else {
            let _ = self.apply_inventory(fresh);
            return;
        };
        let scan_notes = fresh.notes.clone();
        self.devices.inventory = fresh;
        self.devices.inventory_fingerprint = inventory_fingerprint(&self.devices.inventory);
        crate::app::settings::rematch_windows_virtual_output(
            &mut self.extra_outputs,
            &self.devices.inventory.outputs,
        );
        for extra_output in &mut self.extra_outputs {
            restore_extra_output(&self.devices.inventory.outputs, extra_output);
        }
        let restored = restore_saved_devices(&self.devices.inventory, &gate.preferences);
        self.devices.asio_idx = restored.asio_idx;
        self.devices.input_idx = restored.input_idx;
        self.devices.output_idx = restored.output_idx;
        self.devices.asio_held = restored.asio_held;
        self.devices.input_held = restored.input_held;
        self.devices.output_held = restored.output_held;

        let low_label = self.devices.inventory.low_latency_label.clone();
        self.push_log(
            format!("{low_label} or system input: the chosen output opens independently."),
            LogTone::Teal,
        );
        if self.devices.inventory.asio.iter().all(|driver| driver.demo) {
            self.push_log(
                format!("Demo driver loaded — connect a {low_label} input to start."),
                LogTone::Amber,
            );
        } else {
            self.push_log(
                format!("{low_label} input detected. Ready to go live."),
                LogTone::Green,
            );
        }
        self.push_log(
            "Output stays independent — shared mode is allowed.",
            LogTone::Blue,
        );
        let input_name = self.active_input().name.clone();
        let output_name = self.selected_output().name.clone();
        let output_hint = if self.selected_output().preferred {
            if cfg!(target_os = "windows") {
                " (Windows default)"
            } else {
                " (system default)"
            }
        } else {
            ""
        };
        self.push_log(
            format!("Ready: {input_name} -> {output_name}{output_hint}."),
            LogTone::Teal,
        );
        for note in restored.notes {
            self.push_log(note, LogTone::Amber);
        }
        self.log_scan_notes(&scan_notes);
        #[cfg(target_os = "linux")]
        self.refresh_quantum_check(self.screenshot.is_none());
        self.driver_probe = None;
        self.driver_probe_job = None;
        self.last_output_driver = None;
        self.start_output_driver_check(false);
        if self.auto_start_bridge {
            self.push_log("Auto-start is on — starting the bridge.", LogTone::Blue);
            self.start_bridge();
        } else {
            self.push_log(
                "Bridge stopped at launch. Start it when you're ready.",
                LogTone::Muted,
            );
        }
    }

    pub(crate) fn push_log(&mut self, text: impl Into<String>, tone: LogTone) {
        let elapsed = self.started.elapsed().as_secs();
        self.log.push(LogEntry {
            at: format!("{:02}:{:02}", elapsed / 60, elapsed % 60),
            text: text.into(),
            tone,
        });
        self.note_log_arrived();
        let overflow = self.log.len().saturating_sub(ACTIVITY_LOG_CAP);
        if overflow > 0 {
            self.log.drain(0..overflow);
        }
    }
}

#[cfg(test)]
pub(crate) fn blank_app() -> RigWerkApp {
    let mut app = RigWerkApp::base();
    // Tests build their own chain: path 0 is there, slot 1 is not, and no
    // folder is scanned until a test asks for it.
    app.paths = std::array::from_fn(|slot| (slot == 0).then(paths::SignalPath::default));
    app.path_order = vec![0];
    app.nam_rescan = false;
    app.cab_rescan = false;
    app
}

#[cfg(test)]
mod tests;
