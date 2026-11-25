//! The RigWerk window: app state, start-up, and the logic and save the frame calls.
//!
//! Logic is grouped in `app/` (settings, devices, bridge control, recording,
//! sessions, screenshots, tuner, theme and size).  Drawing belongs in `ui/`.  The frame shell
//! (`impl eframe::App`) lives in `ui/frame.rs`; this file does not paint.

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
    /// The home screen: Input, the paths, Master and the outputs side by
    /// side.
    Console,
    /// The tuner, a screen of its own.
    Tuner,
    /// Devices and the guide, one scrolling screen.
    Setup,
}

/// Test hook for screenshots of a specific page: `RIGWERK_PAGE=console`.
/// Normal launches start on the page that was open when the app was closed.
pub(crate) fn initial_page() -> Page {
    page_for_run(&std::env::var("RIGWERK_PAGE").unwrap_or_default())
}

/// The page a `RIGWERK_PAGE` value opens.  Anything else, and the old page
/// names, open the console.
pub(crate) fn page_for_run(key: &str) -> Page {
    match key.trim().to_lowercase().as_str() {
        "tuner" => Page::Tuner,
        "setup" | "devices" | "guide" => Page::Setup,
        _ => Page::Console,
    }
}

/// Test hook for screenshots in one theme: `RIGWERK_THEME=light` (or
/// `dark`).  It wins over the saved theme for this run and is not saved.
fn theme_override() -> Option<appearance::ThemeChoice> {
    let key = std::env::var("RIGWERK_THEME").ok()?;
    match key.trim().to_lowercase().as_str() {
        "dark" => Some(appearance::ThemeChoice::Dark),
        "light" => Some(appearance::ThemeChoice::Light),
        _ => None,
    }
}

/// The page to open on launch: the one left open last time, unless a
/// screenshot run or `RIGWERK_PAGE` asks for a specific page.
fn restored_page(preferences: &Preferences, screenshot_mode: bool) -> Page {
    if screenshot_mode || std::env::var_os("RIGWERK_PAGE").is_some() {
        return initial_page();
    }
    page_from_key(&preferences.page)
}

/// Display-rate tick while the bridge is live or a plugin editor is open.
const ACTIVE_REPAINT: Duration = Duration::from_millis(16);
/// Slow tick while idle. `logic` still runs, so hidden-window servicing continues.
const IDLE_REPAINT: Duration = Duration::from_millis(200);

/// How soon `logic` should run again.
///
/// Starting, Running, Stopping, and Preview, and an open plugin editor, need
/// a 16 ms tick. Idle and Error stay on a 200 ms tick so engine polling,
/// plugin main-thread service, and device watching keep running.
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

/// How many activity lines the Diagnostics page keeps. Older lines drop
/// off the front. This stays in the hundreds so a failed scan is still
/// there after ordinary bridge chatter.
pub(crate) const ACTIVITY_LOG_CAP: usize = 400;

/// How an activity-log line is drawn.  Diagnostics maps this to a theme
/// colour; the log itself does not store a `Color32`.
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

/// Closes a plugin's editor window before the fields drop: `engine` is
/// declared first and would take the plugins down while their editor is
/// still attached.  This way the view is detached, then the plugin
/// deactivated and destroyed, on exit as everywhere else.
impl Drop for RigWerkApp {
    fn drop(&mut self) {
        self.plugin_gui = None;
    }
}

pub struct RigWerkApp {
    pub(crate) page: Page,
    /// The page the previous logic pass ran with.  The tuner logs its mute
    /// layer changing when this differs from `page`.
    pub(crate) last_page: Page,
    pub(crate) engine: AudioEngine,
    /// Tuner settings, the analysis behind the current frame and the
    /// auto-mute rule.
    pub(crate) tuner: tuner::TunerState,
    /// The scanned device lists, the selection, and the scan plumbing.
    pub(crate) devices: devices::Devices,
    pub(crate) sample_rate: u32,
    pub(crate) buffer_size: usize,
    pub(crate) input_gain: f32,
    pub(crate) output_gain: f32,
    pub(crate) input_channels: [bool; 2],
    /// The master mute.
    pub(crate) mute: bool,
    /// Metronome transport.  The click is mixed onto device channels, so
    /// the master mute does not stop it.
    pub(crate) metronome_running: bool,
    pub(crate) metronome_bpm: f32,
    pub(crate) metronome_rhythm: crate::audio::ClickRhythm,
    pub(crate) metronome_level_db: f32,
    /// The saved theme (see `app::appearance`).
    pub(crate) theme: appearance::ThemeChoice,
    /// The interface size, egui's zoom factor (0.8–1.5).
    pub(crate) ui_size: f32,
    /// What a double-click on an effect opens.
    pub(crate) effect_double_click: settings::EffectOpen,
    /// The click channels an output's *Metronome* toggle last switched off.
    pub(crate) last_click_mask: u8,
    /// Sheet, drawer, session draft and the error banner.
    pub(crate) chrome: screen::Chrome,
    /// The audible flag [`Self::apply_metronome`] last pushed, so a frame
    /// re-pushes the click only when the tuner's auto-mute changes it.
    pub(crate) metronome_pushed: Option<bool>,
    pub(crate) drift: bool,
    /// Linux low-latency output (PipeWire quantum / ALSA smallest period).
    /// On Windows this stays aligned with [`Self::wasapi_output_mode`] for
    /// older code paths and cross-platform preference saves.
    pub(crate) low_latency_output: bool,
    /// Windows WASAPI clock output mode (Shared / REAL-style / Exclusive).
    pub(crate) wasapi_output_mode: crate::audio::WasapiOutputMode,
    /// Output side rate (0 follows the input) and buffer.
    pub(crate) output_rate: u32,
    pub(crate) output_buffer: usize,
    pub(crate) bridge_buffer_mode: BufferMode,
    /// Fixed bridge buffer size in milliseconds.
    pub(crate) bridge_buffer_ms: f32,
    /// The signal paths by engine slot: name, colour, routing, mono,
    /// enable, EQs, gate, level, mute and solo (see `app::paths`).  At
    /// least one is always `Some`.  Pushed to the engine every frame.
    pub(crate) paths: [Option<paths::SignalPath>; MAX_PATHS],
    /// The live path slots in display (and save) order.
    pub(crate) path_order: Vec<usize>,
    /// The path slot the console, the sheets and the browser edit.
    pub(crate) selected_path: usize,
    /// Where each output's trim came from, clock output first then the extra
    /// outputs.  Runtime only.
    pub(crate) output_trim_origins: [output_devices::OutputTrimOrigin; MAX_EXTRA_OUTPUTS + 1],
    /// Level, mute and per-path channel maps of the clock output (the
    /// selected output), pushed to the engine every frame.
    pub(crate) clock_output: OutputDeviceSettings,
    /// Extra output devices.  Pushed live by `push_extra_outputs_to_engine`.
    pub(crate) extra_outputs: Vec<ExtraOutputPref>,
    /// An output Learn or Match paths measurement in progress.
    pub(crate) loudness_measurement: Option<output_devices::LoudnessMeasurement>,
    /// The Match paths target and the last loaded reference position, saved
    /// with the session.
    pub(crate) match_paths: match_paths::MatchPathsSettings,
    /// The reference path as a live slot.  Saved as a position.
    pub(crate) match_paths_reference: Option<usize>,
    /// The last Match paths measurement, while the sheet offers to apply it.
    pub(crate) match_paths_result: Option<match_paths::MatchPathsResult>,
    /// The sheet's tick boxes, one per row of [`Self::match_paths_result`].
    pub(crate) match_paths_ticks: Vec<bool>,
    /// What the sheet says after Apply or after a failed measurement.
    pub(crate) match_paths_status: Option<String>,
    /// Last EQ slot shown in the inspector (also persisted as `eq_edit`).
    pub(crate) eq_edit: EqSlot,
    /// Selected chain stage on the console.
    pub(crate) chain_selected: chain::ChainSelection,
    /// Output device the output sheet last showed (not persisted; apart from the
    /// Mixer's selection).
    pub(crate) routing_device: usize,
    /// Path slot the output sheet's map presets set (sheet-local; falls back to
    /// the selected path).
    pub(crate) routing_preset_path: Option<usize>,
    /// The routing layout applied last, for "Custom · edited from <layout>".
    pub(crate) routing_last_layout: Option<crate::app::layouts::PathLayout>,
    /// What the issues keep between frames (the DSP hysteresis, the
    /// session's dropouts, a lost device and the errors already shown).
    /// Moved once per tick by `tick_issues`.
    pub(crate) issue_state: crate::app::issues::IssueState,
    /// Where the next Plugins-page add should land (`None` = append).
    pub(crate) plugin_effect_at: Option<usize>,
    pub(crate) meters: meters::Meters,
    pub(crate) snapshot: Snapshot,
    /// Linux: PipeWire quantum compared with the ALSA period.  Recomputed at
    /// startup, when the output changes and once the bridge reports its real
    /// period.
    #[cfg(target_os = "linux")]
    pub(crate) quantum: Option<audio::QuantumInfo>,
    pub(crate) session_name: String,
    /// Directory takes are written into.  Empty is replaced on load; the
    /// writer creates it.
    pub(crate) record_folder: String,
    /// Folder of `.nam` captures.  Empty until the NAM sheet sets one.
    pub(crate) nam_folder: String,
    /// The last scan of [`Self::nam_folder`].  Not refreshed every frame.
    pub(crate) nam_catalog: nam::NamCatalog,
    /// The NAM sheet opened, or the folder changed: scan before drawing.
    pub(crate) nam_rescan: bool,
    /// Viewport the models folder window is parented to.  Set each frame.
    pub(crate) native_window: Option<nam::NativeWindow>,
    /// Ctrl+V was down last frame, so a held chord pastes the models folder once.
    pub(crate) nam_paste_down: bool,
    /// The thread that parses captures, measures them and hands them back.
    /// Started the first time a block needs one.
    pub(crate) nam_loader: Option<nam::NamLoader>,
    /// A block the sheet would add, waiting for its confirmation because the
    /// path already runs [`MAX_NAM_BLOCKS`](crate::audio::MAX_NAM_BLOCKS)
    /// blocks' worth of DSP.
    pub(crate) nam_confirm: Option<usize>,
    /// A preset that loaded heavier than the DSP budget, with the blocks to
    /// offer bypassing.  Dismissed or acted on once.
    pub(crate) nam_warning: Option<nam::NamLoadWarning>,
    /// Folder of `.wav` impulse responses.  Empty until the cab sheet sets one.
    pub(crate) cab_folder: String,
    /// The last scan of [`Self::cab_folder`].  Not refreshed every frame.
    pub(crate) cab_catalog: cab::CabCatalog,
    /// The cab sheet opened, or the folder changed: scan before drawing.
    pub(crate) cab_rescan: bool,
    /// Ctrl+V was down last frame, so a held chord pastes the impulse folder once.
    pub(crate) cab_paste_down: bool,
    /// "Record the click": takes carry the metronome and calibration click.
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
    /// A settings change asked for a new bridge while the old one was still
    /// releasing its device; start again as soon as the engine is Idle.
    pub(crate) bridge_restart_pending: bool,
    /// Whether this bridge start's real driver settings were already logged.
    pub(crate) driver_checked: bool,
    pub(crate) device_alert: Option<String>,
    /// The last stand-alone check of the selected output's driver.
    pub(crate) driver_probe: Option<driver_check::DriverCheck>,
    /// A stand-alone driver check running on its own thread.
    pub(crate) driver_probe_job: Option<mpsc::Receiver<driver_check::DriverCheck>>,
    /// The last real output reading from a running bridge, with its device.
    /// Kept so the DRIVER line still shows the true step after Stop.
    pub(crate) last_output_driver: Option<(String, driver_check::DriverActual)>,
    /// What the plugin browser found and what is being typed into it.
    pub(crate) plugin_browser: plugins::PluginBrowser,
    /// The effect (path, index) whose control panel is open.
    pub(crate) plugin_editor: Option<paths::EffectKey>,
    /// The plugin's own editor window, when it has one open.
    pub(crate) plugin_gui: Option<plugin_gui::PluginGuiWindow>,
    /// egui's points-to-pixels factor, read every frame.  A CLAP editor
    /// whose window does not report its own DPI is scaled by this.
    pub(crate) ui_scale: f32,
    /// Filter typed into the configure panel.
    pub(crate) plugin_param_filter: String,
    /// A copy of stored preferences that could not be fully read, written
    /// under [`PREFERENCES_BACKUP_KEY`] at the start of the next save so that
    /// save cannot lose it.
    preferences_backup: Option<(String, String)>,
    /// The layout number the stored file declared (`PREFERENCES_SCHEMA` when
    /// nothing usable was stored).  Above it, a newer build wrote the file:
    /// this session then writes only the backup and never the primary.
    stored_schema_version: u32,
    /// The last measured loopback and the conditions it was made under
    /// (persisted; see `app::latency` for staleness).
    pub(crate) measured_latency: Option<latency::MeasuredLatency>,
    /// The calibration panel: open, what it is doing, and how the loop is
    /// closed.  Not persisted.
    pub(crate) calibration: calibration::CalibrationUi,
    /// Planned ALSA period for one output setting, so the UI does not ask
    /// the device again on every frame.
    #[cfg(target_os = "linux")]
    period_cache: Option<(String, bool, usize, u32)>,
    /// Output id, low-latency flag, and buffer already compared with PipeWire.
    #[cfg(target_os = "linux")]
    quantum_checked_key: Option<(String, bool, usize)>,
    /// Last time the running bridge re-read PipeWire's graph quantum.
    #[cfg(target_os = "linux")]
    quantum_live_at: Instant,
    /// Preferences kept until the first background device scan finishes, so
    /// the window can paint before hosts are probed.  The frame reads this
    /// to show the startup window.
    pub(crate) startup: Option<StartupGate>,
    /// Whether the log has said the audio thread allocated.  Debug builds
    /// only: release builds do not count.
    #[cfg(debug_assertions)]
    rt_allocations_logged: bool,
}

impl RigWerkApp {
    /// Device watching, plugin service, and the repaint tick.
    ///
    /// The frame calls this after [`Self::publish_engine`] and
    /// [`Self::smooth_meters`], in that order.  eframe also calls `logic`
    /// while the window is hidden, so this keeps running with no egui pass.
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

    /// Writes preferences.  The frame's `save` forwards here.  A startup
    /// scan still in flight does not overwrite the stored devices, but the
    /// damaged-settings copy is written first either way.  When the load could
    /// read a newer build's layout number, the primary file is left
    /// byte-identical and only the copy is saved; a file too damaged to yield
    /// a number is replaced, with its original text kept in that copy.
    pub(crate) fn save_to(&mut self, storage: &mut dyn eframe::Storage) {
        // A screenshot run starts from defaults and its screens are fixtures,
        // so closing its window must not store that as the session's state.
        if self.screenshot.is_some() {
            return;
        }
        // Before the gate: a quit while the first scan still runs must keep
        // the original text a damaged load could not read.
        store_preferences_backup(storage, &mut self.preferences_backup);
        // A newer build's layout is not this build's to write: leave the
        // stored file byte-identical and save nothing else this session.
        if self.stored_schema_version > PREFERENCES_SCHEMA {
            return;
        }
        // Placeholder rows must not overwrite the real stored pair while the
        // first scan is still running.
        if self.startup.is_some() {
            return;
        }
        // An open editor may have changed a value without reporting it, so
        // `saved_paths` reads it in full before it lists the effects.
        // A side whose stored identity startup did not find keeps that
        // name, id, and backend. The on-screen row can still be the fallback.
        // An id match, a legacy name match, and a side with nothing stored
        // write the live device.
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
            // The legacy mix fields are read from old saves and never written,
            // so these keep the defaults.
            output_channels: [true, true],
            mono: false,
            routes: [[true, false, false, false], [false, true, false, false]],
            eq: EqSettings::default(),
            eq_post: EqSettings::default(),
            gate: GateSettings::default(),
            plugins: Vec::new(),
            // Neither saved nor read: a launch starts with the click stopped.
            metronome_running: false,
        };
        store_preferences(storage, &preferences);
    }
}

impl RigWerkApp {
    /// The app before preferences, devices or a run: default settings, empty
    /// device lists and no held identity.
    ///
    /// This is the one `RigWerkApp` literal.  `new` adds the placeholder rows
    /// and the screenshot job before it applies the stored preferences;
    /// `blank_app` takes the base as it is and preinstalls path 0.
    ///
    /// The literal retypes the defaults in [`Preferences::default`] (rate,
    /// buffer, gains, metronome), so the two must be kept in step: a test
    /// that stands on `blank_app` reads these values, not the preference
    /// ones.
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

    /// Writes stored settings into an app from [`Self::base`]: the start page,
    /// the devices, the rates and gains, the mixer settings and the tuner.
    ///
    /// `stored_schema_version` is the number the loaded file declared, which
    /// is kept so a save can tell whether a newer build wrote it.
    ///
    /// The device restore reads `self.devices.inventory`, and the screenshot flag
    /// picks the page, the tuner demo and whether the bridge may start, so
    /// `new` sets both before this runs.  `blank_app` never calls it, so a
    /// test starts from the base and leaves the engine untouched.
    fn apply_preferences(&mut self, preferences: &Preferences, stored_schema_version: u32) {
        // Every field is named so a new preference is a compile error until it
        // has a destination here.  The `_name` bindings are deliberate: those
        // fields belong to `new` or `save_to`, not to the running app.
        let Preferences {
            // Read from the file and passed beside it; the value in memory is
            // this build's number already.
            schema_version: _,
            // Bound unused; read through `preferences` below.
            auto_start_bridge: _,
            sessions: _,
            session_name: _,
            page: _,
            eq_edit: _,
            output_tab_low_latency: _,
            selected_session: _,
            // Read through `preferences` below, beside the index.
            selected_session_name: _,
            // `restore_saved_devices` below reads these nine.
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
            // Path 1 from before paths existed: `new` folds these in through
            // `path_prefs`.
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
            // `install_paths`, called by `new`.
            paths: _paths,
            selected_path: _selected_path,
            match_paths: _,
            clock_output: _,
            extra_outputs: _,
            measured_latency: _,
            // `TunerState::new` reads these.
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
            // `new` copies these into the effect browser.
            effect_favourites: _effect_favourites,
            effect_recent: _effect_recent,
            // `Chrome::at_launch` below reads it.
            sheet_height: _,
            effect_double_click: _,
            last_click_mask: _,
        } = preferences;
        let screenshot = self.screenshot.is_some();
        let restored = restore_saved_devices(&self.devices.inventory, preferences);
        let wasapi_output_mode = resolved_wasapi_output_mode(preferences);
        // Rates, buffers, gains and the bridge buffer were checked in
        // `load_preferences` (`validate_bridge_settings`). The defaults
        // used when nothing was stored are already inside those ranges.
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
        // `ui_size` was clamped in `load_preferences`.
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
        // The inventory is still the placeholder here; the first real scan
        // adopts the saved ids in `finish_startup_scan`.
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
        // `selected_session` indexes the saved list, which the load can
        // shorten: it skips an unreadable session before this point and
        // `unique_sessions` drops a duplicate name.  Either moves the index to
        // another session, so the stored name is what the choice follows; an
        // older file has no name and falls back to the index.
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
        // eframe builds the app on its UI thread, which is the thread that
        // services CLAP plugins; record it before any plugin can load.
        crate::audio::mark_main_thread();
        // A screenshot run must be reproducible and must never open audio
        // hardware, so it starts from defaults instead of saved settings.
        let raw_preferences = if screenshot.is_some() {
            None
        } else {
            cc.storage
                .and_then(|storage| storage.get_string(PREFERENCES_KEY))
        };
        let loaded = load_preferences(raw_preferences.as_deref(), PREFERENCES_BACKUP_KEY);
        let preferences = loaded.preferences.clone().unwrap_or_default();
        // The screenshot path is a headless check, not a practice session.
        // Keep real drivers closed there even if auto-start was left on.
        let screenshot_mode = screenshot.is_some();
        let mut app = Self::base();
        // Paint the shell before hosts are probed. The first real inventory
        // arrives on the scan thread; restore and auto-start wait for it.
        // The restore reads these rows, and the page and the auto-start read
        // the screenshot flag, so both go in before the preferences do.
        app.devices.inventory = audio::placeholder_inventory();
        app.devices.inventory_fingerprint = inventory_fingerprint(&app.devices.inventory);
        app.screenshot = screenshot.map(|path| ScreenshotJob::new(path, screenshot_exit));
        app.apply_preferences(&preferences, loaded.stored_schema_version);
        app.preferences_backup = loaded.backup;
        app.startup = Some(StartupGate {
            preferences: preferences.clone(),
        });
        // egui restored its own memory before this; the saved preference wins.
        // Track B: the effect browser's starred and recent effects.
        app.plugin_browser.favourites = preferences.effect_favourites.clone();
        app.plugin_browser.recent = preferences.effect_recent.clone();
        app.apply_appearance(&cc.egui_ctx, theme_override());
        app.apply_gains();
        app.apply_metronome();
        app.engine.set_record_click(app.record_click);
        app.engine
            .set_master(app.mute, app.input_channels, app.drift);
        // Old files (and no file) load as one path.
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

    /// Applies the first real inventory, then quantum check / auto-start.
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

/// An app with empty device lists and no held identity, for tests.
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
