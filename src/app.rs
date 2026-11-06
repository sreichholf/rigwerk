//! The RigWerk window: app state, start-up, the frame loop and saving.
//!
//! Logic is grouped in `app/` (settings, devices, bridge control, patches,
//! screenshots); everything drawn on screen lives in `ui/`.

pub(crate) mod bridge;
pub(crate) mod devices;
pub(crate) mod driver_check;
pub(crate) mod plugin_gui;
pub(crate) mod plugins;
pub(crate) mod presets;
pub(crate) mod screenshot;
pub(crate) mod settings;

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use eframe::egui;
use egui::{Align2, Color32, Margin, RichText, Vec2};

use crate::app::devices::{
    default_index, device_restore_note, held_device, inventory_fingerprint, restore_device,
    HeldDevice,
};
use crate::app::screenshot::ScreenshotJob;
use crate::app::settings::{
    cushion_from_pref, cushion_to_pref, load_preferences, page_from_key, page_key,
    preferences_backup_key, store_preferences, unique_presets, FollowerPref, Preferences, Preset,
    PREFERENCES_KEY, PREFERENCES_SCHEMA,
};
use crate::audio::{
    self, AudioEngine, CushionMode, EngineState, EqSettings, EqSlot, GateSettings, Inventory,
    OutputDeviceSettings, Snapshot, MAX_OUTPUT_CHANNELS,
};
use crate::ui::theme::ts;
use crate::ui::theme::{
    configure_style, with_alpha, AMBER, BG, BLUE, BORDER, CARD, DIM, GREEN, MUTED, SIDEBAR, TEAL,
    TEXT,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Page {
    Live,
    Mixer,
    Routing,
    Devices,
    Sessions,
    Plugins,
    Guide,
    Diagnostics,
}

/// Test hook for screenshots of a specific page: `RIGWERK_PAGE=devices`.
/// Normal launches start on the page that was open when the app was closed.
pub(crate) fn initial_page() -> Page {
    match std::env::var("RIGWERK_PAGE")
        .unwrap_or_default()
        .to_lowercase()
        .as_str()
    {
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

/// Test hook for screenshots of a Mixer stage: `RIGWERK_MIXER_STAGE=output`
/// (or `device` for the clock device). Normal launches open the last EQ.
fn initial_mixer_stage() -> Option<crate::ui::pages::mixer::chain::ChainSelection> {
    let key = std::env::var("RIGWERK_MIXER_STAGE").ok()?;
    crate::ui::pages::mixer::chain::ChainSelection::from_key(&key.to_lowercase())
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

pub(crate) struct LogEntry {
    pub(crate) at: String,
    pub(crate) text: String,
    pub(crate) tone: Color32,
}

pub struct RigWerkApp {
    pub(crate) page: Page,
    pub(crate) engine: AudioEngine,
    pub(crate) inventory: Inventory,
    pub(crate) asio_idx: usize,
    pub(crate) input_idx: usize,
    pub(crate) output_idx: usize,
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
    /// REAL-style output: smallest WASAPI engine step the driver allows.
    pub(crate) low_latency_output: bool,
    pub(crate) routes: [[bool; 4]; 2],
    /// Output side rate (0 follows the input) and buffer.
    pub(crate) output_rate: u32,
    pub(crate) output_buffer: usize,
    /// Which output list is shown: the low-latency driver's (ASIO) or the
    /// system's.  Follows the selected output unless that list is empty.
    pub(crate) output_tab_low_latency: bool,
    pub(crate) cushion_mode: CushionMode,
    /// Manual cushion size in milliseconds.
    pub(crate) cushion_ms: f32,
    /// The two built-in EQs on the master mix, indexed by [`EqSlot`]: before
    /// and after the plugin inserts.
    pub(crate) eqs: [EqSettings; 2],
    /// Built-in noise gate (PRE or POST inserts; default POST).
    pub(crate) gate: GateSettings,
    /// Level, mute and channel map of the clock device (the selected output),
    /// pushed to the engine every frame.
    pub(crate) clock_device: OutputDeviceSettings,
    /// Extra output devices. Stored only; nothing plays them yet.
    pub(crate) followers: Vec<FollowerPref>,
    /// Last EQ slot shown in the inspector (also persisted as `eq_edit`).
    pub(crate) eq_edit: EqSlot,
    /// Selected stage in the Mixer rack / Live chain strip.
    pub(crate) chain_selected: crate::ui::pages::mixer::chain::ChainSelection,
    /// Where the next Plugins-page add should land (`None` = append).
    pub(crate) plugin_insert_at: Option<usize>,
    pub(crate) meter_in_l: f32,
    pub(crate) meter_in_r: f32,
    pub(crate) meter_out_l: f32,
    pub(crate) meter_out_r: f32,
    pub(crate) peak_in_l: f32,
    pub(crate) peak_in_r: f32,
    pub(crate) peak_out_l: f32,
    pub(crate) peak_out_r: f32,
    /// Smoothed meter and peak per clock-device channel, from
    /// [`Snapshot::out_channels`].
    pub(crate) meter_out_channels: [f32; MAX_OUTPUT_CHANNELS],
    pub(crate) peak_out_channels: [f32; MAX_OUTPUT_CHANNELS],
    /// Decimated capture/playback waveform for the Live scope.
    pub(crate) scope_in: Vec<f32>,
    pub(crate) scope_out: Vec<f32>,
    pub(crate) snapshot: Snapshot,
    /// Linux: PipeWire quantum compared with the ALSA period.  Recomputed at
    /// startup, when the output changes and once the bridge reports its real
    /// period.
    #[cfg(target_os = "linux")]
    pub(crate) quantum: Option<audio::QuantumInfo>,
    pub(crate) session_name: String,
    pub(crate) sessions: Vec<Preset>,
    pub(crate) selected_session: Option<usize>,
    pub(crate) pending_delete: Option<usize>,
    pub(crate) log: Vec<LogEntry>,
    pub(crate) started: Instant,
    pub(crate) screenshot: Option<ScreenshotJob>,
    pub(crate) frames: u64,
    pub(crate) show_about: bool,
    pub(crate) auto_start_bridge: bool,
    pub(crate) last_meter_tick: Instant,
    pub(crate) inventory_fingerprint: u64,
    pub(crate) last_device_scan: Instant,
    pub(crate) device_scan: Option<mpsc::Receiver<Inventory>>,
    pub(crate) manual_scan: bool,
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
    /// Index of the insert whose control panel is open.
    pub(crate) plugin_editor: Option<usize>,
    /// The plugin's own editor window, when it has one open.
    pub(crate) plugin_gui: Option<plugin_gui::PluginGuiWindow>,
    /// Filter typed into the configure panel.
    pub(crate) plugin_param_filter: String,
    /// Open the plugin's own interface straight from the insert when it has one.
    pub(crate) prefer_plugin_interface: bool,
    /// A copy of stored preferences that could not be fully read, written
    /// under its own key on the next save so that save cannot lose it.
    preferences_backup: Option<(String, String)>,
    /// Stored low-latency input kept when startup did not find it.
    asio_held: Option<HeldDevice>,
    /// Stored system input kept when startup did not find it.
    input_held: Option<HeldDevice>,
    /// Stored output kept when startup did not find it.
    output_held: Option<HeldDevice>,
    /// Consecutive scans that did not list the selected input. One miss is
    /// not an unplug: a busy device fails a probe the same way a missing one
    /// does. The bridge stops only after a second miss.
    input_misses: u8,
    /// Consecutive scans that did not list the selected output.
    output_misses: u8,
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
    /// the window can paint before hosts are probed.
    startup: Option<StartupGate>,
}

/// Saved device choices applied once the first inventory arrives.
struct StartupGate {
    preferences: Preferences,
}

impl eframe::App for RigWerkApp {
    /// Non-painting maintenance.  eframe also calls this while the window is
    /// hidden or minimized, with no egui pass, so engine watchdogs, device
    /// watching and plugin main-thread/timer servicing keep running.  The
    /// timed repaint request at the end is what keeps those hidden passes
    /// coming: 16 ms while the bridge is live or a plugin editor is open,
    /// otherwise 200 ms.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.meters(ctx);
        self.watch_devices();
        self.poll_plugin_scan();
        self.engine.service_plugins();
        self.poll_plugin_gui();
        self.sync_bridge_config();
        let plugin_editor_open = self.plugin_editor.is_some() || self.plugin_gui.is_some();
        let interval = if self.startup.is_some() {
            Duration::from_millis(16)
        } else {
            repaint_interval(self.engine.state, plugin_editor_open)
        };
        ctx.request_repaint_after(interval);
    }

    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = root.ctx().clone();
        // Screenshots need a painted frame, so they stay on the drawing side.
        self.handle_screenshot(&ctx);

        egui::Panel::left("rigwerk_nav")
            .exact_size(224.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(
                egui::Frame::new()
                    .fill(SIDEBAR)
                    .inner_margin(Margin::symmetric(16, 22)),
            )
            .show(root, |ui| {
                self.sidebar(ui);
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG).inner_margin(Margin {
                left: 26,
                right: 26,
                top: 20,
                bottom: 18,
            }))
            .show(root, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        self.header(ui);
                        ui.add_space(14.0);
                        match self.page {
                            Page::Live => self.live_page(ui, &ctx),
                            Page::Mixer => self.mixer_page(ui),
                            Page::Routing => self.routing_page(ui),
                            Page::Devices => self.devices_page(ui),
                            Page::Sessions => self.sessions_page(ui),
                            Page::Plugins => self.plugins_page(ui),
                            Page::Guide => self.guide_page(ui),
                            Page::Diagnostics => self.diagnostics_page(ui),
                        }
                        ui.add_space(18.0);
                    });
            });

        if self.show_about {
            // Fixed outer size so the window frame (and its right border) stays
            // fully on-screen; inner content is capped below that.
            egui::Window::new("About RigWerk")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                .fixed_size(Vec2::new(420.0, 300.0))
                .frame(
                    egui::Frame::new()
                        .fill(CARD)
                        .stroke(egui::Stroke::new(1.0, BORDER))
                        .corner_radius(egui::CornerRadius::same(12))
                        .inner_margin(Margin::same(16)),
                )
                .show(&ctx, |ui| {
                    ui.set_max_width(388.0);
                    ui.heading("RigWerk");
                    ui.label(
                        RichText::new(
                            "An independent-clock audio bridge. Any input and any output can sit on opposite sides, whether they speak ASIO, WASAPI, DirectSound or MME.",
                        )
                        .color(MUTED),
                    );
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new(
                            "ASIO and system-host inputs and outputs are wired on Windows. On Linux, PipeWire is the low-latency backend when ALSA's default plugin is PipeWire, and direct hw: devices are the other low-latency path. PulseAudio stays on the system side. RigWerk compares the ALSA period with the PipeWire quantum and reports a collision instead of hiding it.",
                        )
                        .size(ts(10.5))
                        .color(GREEN),
                    );
                    ui.label(
                        RichText::new(
                            "Select any input and output; whether one interface can do both jobs at once depends on its driver, so RigWerk reports what the driver says instead of guessing.",
                        )
                        .size(ts(10.0))
                        .color(MUTED),
                    );
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("Version 0.2 · preview build · working title").color(DIM),
                    );
                    ui.add_space(8.0);
                    if ui
                        .add(
                            egui::Button::new(RichText::new("Close").color(TEAL))
                                .fill(with_alpha(TEAL, 14))
                                .stroke(egui::Stroke::new(1.0, with_alpha(TEAL, 45)))
                                .corner_radius(egui::CornerRadius::same(7)),
                        )
                        .clicked()
                    {
                        self.show_about = false;
                    }
                });
        }

        self.plugin_editor_window(&ctx);

        if self.startup.is_some() {
            egui::Window::new("Starting RigWerk")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                .fixed_size(Vec2::new(360.0, 120.0))
                .frame(
                    egui::Frame::new()
                        .fill(CARD)
                        .stroke(egui::Stroke::new(1.0, BORDER))
                        .corner_radius(egui::CornerRadius::same(12))
                        .inner_margin(Margin::same(18)),
                )
                .show(&ctx, |ui| {
                    ui.label(
                        RichText::new("Finding audio devices…")
                            .size(ts(14.0))
                            .color(TEXT),
                    );
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(
                            "The window is ready. Device lists fill in as soon as the scan finishes.",
                        )
                        .size(ts(11.0))
                        .color(MUTED),
                    );
                });
        }

        if let Some(message) = self.device_alert.clone() {
            egui::Window::new("Audio device disconnected")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                .show(&ctx, |ui| {
                    ui.set_width(380.0);
                    ui.label(RichText::new(message).size(ts(12.5)).color(TEXT));
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(
                            "Plug the device back in (or pick another one on the Devices page), then start the bridge again.",
                        )
                        .size(ts(10.5))
                        .color(MUTED),
                    );
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui.button("Rescan devices").clicked() {
                            self.rescan();
                        }
                        if ui.button("Dismiss").clicked() {
                            self.device_alert = None;
                        }
                    });
                });
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        // Placeholder rows must not overwrite the real stored pair while the
        // first scan is still running.
        if self.startup.is_some() {
            return;
        }
        // A side whose stored identity startup did not find keeps that
        // name, id, and backend. The on-screen row can still be the fallback.
        // An id match, a legacy name match, and a side with nothing stored
        // write the live device.
        let (asio_name, asio_id, asio_backend) =
            persisted_identity(self.asio_held.as_ref(), Some(self.selected_asio()));
        let (input_name, input_id, input_backend) = persisted_identity(
            self.input_held.as_ref(),
            self.inventory.inputs.get(self.input_idx),
        );
        let (output_name, output_id, output_backend) =
            persisted_identity(self.output_held.as_ref(), Some(self.selected_output()));
        let preferences = Preferences {
            schema_version: PREFERENCES_SCHEMA,
            auto_start_bridge: self.auto_start_bridge,
            sessions: self.sessions.clone(),
            session_name: self.session_name.clone(),
            page: page_key(self.page).to_owned(),
            eq_edit: self.eq_edit.index() as u8,
            output_tab_low_latency: self.output_tab_low_latency,
            selected_session: self.selected_session,
            asio_name,
            asio_id,
            asio_backend,
            input_name,
            input_id,
            input_backend,
            output_name,
            output_id,
            output_backend,
            use_asio_input: self.use_asio_input,
            sample_rate: self.sample_rate,
            buffer_size: self.buffer_size,
            input_gain: self.input_gain,
            output_gain: self.output_gain,
            input_channels: self.input_channels,
            output_channels: self.output_channels,
            mono: self.mono,
            mute: self.mute,
            drift: self.drift,
            monitor: self.monitor,
            low_latency_output: self.low_latency_output,
            routes: self.routes,
            output_rate: self.output_rate,
            output_buffer: self.output_buffer,
            cushion_mode: cushion_to_pref(self.cushion_mode),
            cushion_ms: self.cushion_ms,
            eq: self.eqs[EqSlot::Pre.index()],
            eq_post: self.eqs[EqSlot::Post.index()],
            gate: self.gate,
            plugins: self.engine.plugin_prefs(),
            prefer_plugin_interface: self.prefer_plugin_interface,
            clock_device: self.clock_device,
            followers: self.followers.clone(),
        };
        store_preferences(storage, &preferences, &mut self.preferences_backup);
    }
}

impl RigWerkApp {
    pub fn new(cc: &eframe::CreationContext<'_>, screenshot: Option<PathBuf>) -> Self {
        // eframe builds the app on its UI thread, which is the thread that
        // services CLAP plugins; record it before any plugin can load.
        crate::audio::clap::mark_main_thread();
        configure_style(&cc.egui_ctx);
        // A screenshot run must be reproducible and must never open audio
        // hardware, so it starts from defaults instead of saved settings.
        let raw_preferences = if screenshot.is_some() {
            None
        } else {
            cc.storage
                .and_then(|storage| storage.get_string(PREFERENCES_KEY))
        };
        let backup_key = preferences_backup_key(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs())
                .unwrap_or_default(),
        );
        let loaded = load_preferences(raw_preferences.as_deref(), &backup_key);
        let stored: Option<Preferences> = loaded.preferences;
        let preferences = stored.clone().unwrap_or_default();
        // The screenshot path is a headless check, not a practice session.
        // Keep real drivers closed there even if auto-start was left on.
        let screenshot_mode = screenshot.is_some();
        // Paint the shell before hosts are probed. The first real inventory
        // arrives on the scan thread; restore and auto-start wait for it.
        let inventory = audio::placeholder_inventory();
        let inventory_fingerprint = inventory_fingerprint(&inventory);
        let restored = restore_saved_devices(&inventory, &preferences);
        let asio_idx = restored.asio_idx;
        let input_idx = restored.input_idx;
        let output_idx = restored.output_idx;
        let use_asio_input = match &stored {
            Some(saved) => saved.use_asio_input,
            None => true,
        };
        let engine = AudioEngine::new(48_000);
        let sessions = unique_presets(&preferences.sessions);
        let selected_session = preferences
            .selected_session
            .filter(|index| *index < sessions.len());

        let mut app = Self {
            page: restored_page(&preferences, screenshot_mode),
            engine,
            inventory,
            asio_idx,
            input_idx,
            output_idx,
            use_asio_input,
            // Rates, buffers, gains and the cushion were checked in
            // `load_preferences` (`validate_bridge_settings`). The defaults
            // used when nothing was stored are already inside those ranges.
            sample_rate: preferences.sample_rate,
            buffer_size: preferences.buffer_size,
            input_gain: preferences.input_gain,
            output_gain: preferences.output_gain,
            input_channels: preferences.input_channels,
            output_channels: preferences.output_channels,
            mono: preferences.mono,
            mute: preferences.mute,
            drift: preferences.drift,
            monitor: preferences.monitor,
            low_latency_output: preferences.low_latency_output,
            output_rate: preferences.output_rate,
            output_buffer: preferences.output_buffer,
            output_tab_low_latency: preferences.output_tab_low_latency,
            cushion_mode: cushion_from_pref(preferences.cushion_mode),
            cushion_ms: preferences.cushion_ms,
            eqs: [preferences.eq.sanitized(), preferences.eq_post.sanitized()],
            gate: preferences.gate.sanitized(),
            clock_device: preferences.clock_device.sanitized(),
            followers: preferences
                .followers
                .iter()
                .cloned()
                .map(FollowerPref::sanitized)
                .collect(),
            eq_edit: if preferences.eq_edit == 1 {
                EqSlot::Post
            } else {
                EqSlot::Pre
            },
            chain_selected: crate::ui::pages::mixer::chain::ChainSelection::from_eq_edit(
                if preferences.eq_edit == 1 {
                    EqSlot::Post
                } else {
                    EqSlot::Pre
                },
            ),
            plugin_insert_at: None,
            routes: preferences.routes,
            meter_in_l: 0.0,
            meter_in_r: 0.0,
            meter_out_l: 0.0,
            meter_out_r: 0.0,
            peak_in_l: 0.0,
            peak_in_r: 0.0,
            peak_out_l: 0.0,
            peak_out_r: 0.0,
            meter_out_channels: [0.0; MAX_OUTPUT_CHANNELS],
            peak_out_channels: [0.0; MAX_OUTPUT_CHANNELS],
            scope_in: vec![0.0; audio::SCOPE_LEN],
            scope_out: vec![0.0; audio::SCOPE_LEN],
            snapshot: Snapshot::default(),
            #[cfg(target_os = "linux")]
            quantum: None,
            session_name: preferences.session_name.clone(),
            sessions,
            selected_session,
            pending_delete: None,
            log: Vec::new(),
            started: Instant::now(),
            screenshot: screenshot.map(|path| ScreenshotJob {
                path,
                requested: false,
            }),
            frames: 0,
            show_about: false,
            auto_start_bridge: preferences.auto_start_bridge && !screenshot_mode,
            last_meter_tick: Instant::now(),
            inventory_fingerprint,
            last_device_scan: Instant::now(),
            device_scan: None,
            manual_scan: false,
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
            plugin_param_filter: String::new(),
            prefer_plugin_interface: preferences.prefer_plugin_interface,
            preferences_backup: loaded.backup,
            asio_held: restored.asio_held,
            input_held: restored.input_held,
            output_held: restored.output_held,
            input_misses: 0,
            output_misses: 0,
            #[cfg(target_os = "linux")]
            period_cache: None,
            #[cfg(target_os = "linux")]
            quantum_checked_key: None,
            #[cfg(target_os = "linux")]
            quantum_live_at: Instant::now(),
            startup: Some(StartupGate {
                preferences: preferences.clone(),
            }),
        };
        if let Some(stage) = initial_mixer_stage() {
            app.select_chain(stage);
        }
        app.engine.set_gains(app.input_gain, app.output_gain);
        app.engine.set_mix(
            app.mute,
            app.monitor,
            app.mono,
            app.input_channels,
            app.output_channels,
            app.drift,
        );
        app.engine.set_clock_device(&app.clock_device);
        app.engine.set_cushion(app.cushion_mode, app.cushion_ms);
        for slot in EqSlot::ALL {
            app.engine.set_eq(slot, app.eqs[slot.index()]);
        }
        app.engine.set_gate(app.gate);
        app.engine.restore_plugins(preferences.plugins.clone());
        if !screenshot_mode {
            app.begin_plugin_scan();
        }
        app.push_log("Looking for audio devices…", BLUE);
        for notice in loaded.notices {
            app.push_log(notice, AMBER);
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
        self.inventory = fresh;
        self.inventory_fingerprint = inventory_fingerprint(&self.inventory);
        let restored = restore_saved_devices(&self.inventory, &gate.preferences);
        self.asio_idx = restored.asio_idx;
        self.input_idx = restored.input_idx;
        self.output_idx = restored.output_idx;
        self.asio_held = restored.asio_held;
        self.input_held = restored.input_held;
        self.output_held = restored.output_held;

        let low_label = self.inventory.low_latency_label.clone();
        self.push_log(
            format!("{low_label} or system input: the chosen output opens independently."),
            TEAL,
        );
        if self.inventory.asio.iter().all(|driver| driver.demo) {
            self.push_log(
                format!("Preview driver loaded — connect a {low_label} input to go live."),
                AMBER,
            );
        } else {
            self.push_log(
                format!("{low_label} input detected. Ready to go live."),
                GREEN,
            );
        }
        self.push_log("Output stays independent — shared mode is allowed.", BLUE);
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
            TEAL,
        );
        for note in restored.notes {
            self.push_log(note, AMBER);
        }
        self.log_scan_notes(&scan_notes);
        #[cfg(target_os = "linux")]
        self.refresh_quantum_check(self.screenshot.is_none());
        self.driver_probe = None;
        self.driver_probe_job = None;
        self.last_output_driver = None;
        self.start_output_driver_check(false);
        if self.auto_start_bridge {
            self.push_log("Auto-start is on — starting the bridge.", BLUE);
            self.start_bridge();
        } else {
            self.push_log(
                "Bridge stopped at launch. Start it when you're ready.",
                MUTED,
            );
        }
    }

    pub(crate) fn push_log(&mut self, text: impl Into<String>, tone: Color32) {
        let elapsed = self.started.elapsed().as_secs();
        self.log.push(LogEntry {
            at: format!("{:02}:{:02}", elapsed / 60, elapsed % 60),
            text: text.into(),
            tone,
        });
        let overflow = self.log.len().saturating_sub(ACTIVITY_LOG_CAP);
        if overflow > 0 {
            self.log.drain(0..overflow);
        }
    }
}

/// Indices and held identities from one startup restore.
struct RestoredDevices {
    asio_idx: usize,
    input_idx: usize,
    output_idx: usize,
    asio_held: Option<HeldDevice>,
    input_held: Option<HeldDevice>,
    output_held: Option<HeldDevice>,
    notes: Vec<String>,
}

/// Selects the saved devices, and holds any stored identity the scan did not confirm.
///
/// The window still shows a fallback (the system default, or the first real
/// device; the system input stays at index 0) so the app can be used. `save`
/// writes a held identity back instead of that fallback.
fn restore_saved_devices(inventory: &Inventory, preferences: &Preferences) -> RestoredDevices {
    let mut asio_idx = default_index(&inventory.asio);
    let mut output_idx = default_index(&inventory.outputs);
    let mut input_idx = 0usize;
    let asio_restore = restore_device(
        &inventory.asio,
        &preferences.asio_id,
        &preferences.asio_backend,
        preferences.asio_name.as_deref().unwrap_or(""),
    );
    if let Some(index) = asio_restore.index() {
        asio_idx = index;
    }
    let input_restore = restore_device(
        &inventory.inputs,
        &preferences.input_id,
        &preferences.input_backend,
        preferences.input_name.as_deref().unwrap_or(""),
    );
    if let Some(index) = input_restore.index() {
        input_idx = index;
    }
    let output_restore = restore_device(
        &inventory.outputs,
        &preferences.output_id,
        &preferences.output_backend,
        preferences.output_name.as_deref().unwrap_or(""),
    );
    if let Some(index) = output_restore.index() {
        output_idx = index;
    }
    let mut notes = Vec::new();
    for (role, name, id, restore) in [
        (
            "Low-latency input",
            preferences.asio_name.as_deref().unwrap_or(""),
            preferences.asio_id.as_str(),
            asio_restore,
        ),
        (
            "Input",
            preferences.input_name.as_deref().unwrap_or(""),
            preferences.input_id.as_str(),
            input_restore,
        ),
        (
            "Output",
            preferences.output_name.as_deref().unwrap_or(""),
            preferences.output_id.as_str(),
            output_restore,
        ),
    ] {
        let label = if !name.is_empty() { name } else { id };
        if let Some(note) = device_restore_note(role, label, restore) {
            notes.push(note);
        }
    }
    RestoredDevices {
        asio_idx,
        input_idx,
        output_idx,
        asio_held: held_device(
            asio_restore,
            preferences.asio_name.clone(),
            preferences.asio_id.clone(),
            preferences.asio_backend.clone(),
        ),
        input_held: held_device(
            input_restore,
            preferences.input_name.clone(),
            preferences.input_id.clone(),
            preferences.input_backend.clone(),
        ),
        output_held: held_device(
            output_restore,
            preferences.output_name.clone(),
            preferences.output_id.clone(),
            preferences.output_backend.clone(),
        ),
        notes,
    }
}

/// The name, id, and backend `save` writes for one side.
fn persisted_identity(
    held: Option<&HeldDevice>,
    live: Option<&audio::DeviceItem>,
) -> (Option<String>, String, String) {
    if let Some(held) = held {
        return (held.name.clone(), held.id.clone(), held.backend.clone());
    }
    match live {
        Some(device) => (
            Some(device.name.clone()),
            device.id.clone(),
            device.backend.clone(),
        ),
        None => (None, String::new(), String::new()),
    }
}

/// An app with empty device lists and no held identity, for tests.
#[cfg(test)]
pub(crate) fn blank_app() -> RigWerkApp {
    RigWerkApp {
        page: Page::Live,
        engine: AudioEngine::new(48_000),
        inventory: Inventory::default(),
        asio_idx: 0,
        input_idx: 0,
        output_idx: 0,
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
        output_tab_low_latency: false,
        cushion_mode: CushionMode::Safe,
        cushion_ms: 10.0,
        eqs: [EqSettings::default(), EqSettings::default()],
        gate: GateSettings::default(),
        clock_device: OutputDeviceSettings::default(),
        followers: Vec::new(),
        eq_edit: EqSlot::Pre,
        chain_selected: crate::ui::pages::mixer::chain::ChainSelection::PreEq,
        plugin_insert_at: None,
        meter_in_l: 0.0,
        meter_in_r: 0.0,
        meter_out_l: 0.0,
        meter_out_r: 0.0,
        peak_in_l: 0.0,
        peak_in_r: 0.0,
        peak_out_l: 0.0,
        peak_out_r: 0.0,
        meter_out_channels: [0.0; MAX_OUTPUT_CHANNELS],
        peak_out_channels: [0.0; MAX_OUTPUT_CHANNELS],
        scope_in: vec![0.0; audio::SCOPE_LEN],
        scope_out: vec![0.0; audio::SCOPE_LEN],
        snapshot: Snapshot::default(),
        #[cfg(target_os = "linux")]
        quantum: None,
        session_name: "My guitar rig".to_owned(),
        sessions: Vec::new(),
        selected_session: None,
        pending_delete: None,
        log: Vec::new(),
        started: Instant::now(),
        screenshot: None,
        frames: 0,
        show_about: false,
        auto_start_bridge: false,
        last_meter_tick: Instant::now(),
        inventory_fingerprint: 0,
        last_device_scan: Instant::now(),
        device_scan: None,
        manual_scan: false,
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
        plugin_param_filter: String::new(),
        prefer_plugin_interface: false,
        preferences_backup: None,
        asio_held: None,
        input_held: None,
        output_held: None,
        input_misses: 0,
        output_misses: 0,
        #[cfg(target_os = "linux")]
        period_cache: None,
        #[cfg(target_os = "linux")]
        quantum_checked_key: None,
        #[cfg(target_os = "linux")]
        quantum_live_at: Instant::now(),
        startup: None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use eframe::App;

    use super::settings::FollowerPref;
    use super::settings::{load_preferences, Preferences, Preset, PREFERENCES_KEY};
    use super::{blank_app, repaint_interval, restore_saved_devices, ACTIVE_REPAINT, IDLE_REPAINT};
    use crate::audio::{
        self, ChannelMap, DeviceItem, EngineState, Inventory, OutputDeviceSettings, UnusedChannels,
    };

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

    fn item(id: &str, name: &str, backend: &str, preferred: bool) -> DeviceItem {
        DeviceItem {
            id: id.to_owned(),
            name: name.to_owned(),
            backend: backend.to_owned(),
            channels: 2,
            demo: false,
            preferred,
            asio_clsid: None,
        }
    }

    fn install(app: &mut super::RigWerkApp, preferences: &Preferences) {
        let restored = restore_saved_devices(&app.inventory, preferences);
        app.asio_idx = restored.asio_idx;
        app.input_idx = restored.input_idx;
        app.output_idx = restored.output_idx;
        app.asio_held = restored.asio_held;
        app.input_held = restored.input_held;
        app.output_held = restored.output_held;
        app.use_asio_input = preferences.use_asio_input;
    }

    fn save_text(app: &mut super::RigWerkApp) -> (String, Preferences) {
        let mut storage = MemoryStorage::default();
        app.save(&mut storage);
        let raw = eframe::Storage::get_string(&storage, PREFERENCES_KEY).expect("preferences file");
        let loaded = load_preferences(Some(&raw), "rigwerk-preferences-backup-test");
        assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
        (raw, loaded.preferences.expect("saved preferences"))
    }

    /// Saved ASIO and output ids are absent. The window shows the fallback devices.
    fn absent_studio() -> super::RigWerkApp {
        let mut app = blank_app();
        app.inventory
            .asio
            .push(item("asio:other", "Other interface", "ASIO", false));
        app.inventory
            .inputs
            .push(item("hw:Mic", "Mic", "ALSA", false));
        app.inventory
            .outputs
            .push(item("default-out", "Speakers", "WASAPI", true));
        app.inventory
            .outputs
            .push(item("phones", "Headphones", "WASAPI", false));
        let preferences = Preferences {
            asio_name: Some("Focusrite USB".to_owned()),
            asio_id: "asio:focusrite".to_owned(),
            asio_backend: "ASIO".to_owned(),
            output_name: Some("Studio Monitors".to_owned()),
            output_id: "asio-out:studio".to_owned(),
            output_backend: "ASIO".to_owned(),
            ..Preferences::default()
        };
        install(&mut app, &preferences);
        app
    }

    #[test]
    fn finish_startup_scan_restores_saved_devices_and_clears_the_gate() {
        let mut app = blank_app();
        app.inventory = audio::placeholder_inventory();
        app.startup = Some(super::StartupGate {
            preferences: Preferences {
                asio_name: Some("Focusrite USB".to_owned()),
                asio_id: "asio:focusrite".to_owned(),
                asio_backend: "ASIO".to_owned(),
                output_name: Some("Speakers".to_owned()),
                output_id: "speakers".to_owned(),
                output_backend: "WASAPI".to_owned(),
                auto_start_bridge: false,
                ..Preferences::default()
            },
        });
        let mut fresh = Inventory {
            os: "test".to_owned(),
            low_latency_label: "ASIO".to_owned(),
            ..Inventory::default()
        };
        fresh
            .asio
            .push(item("asio:focusrite", "Focusrite USB", "ASIO", false));
        fresh
            .outputs
            .push(item("speakers", "Speakers", "WASAPI", true));
        fresh.inputs.push(item("mic", "Mic", "WASAPI", false));
        app.finish_startup_scan(fresh);
        assert!(app.startup.is_none());
        assert_eq!(app.selected_asio().id, "asio:focusrite");
        assert_eq!(app.selected_output().id, "speakers");
        assert!(
            app.log
                .iter()
                .any(|entry| entry.text.contains("Ready: Focusrite USB -> Speakers")),
            "{:?}",
            app.log.iter().map(|entry| &entry.text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn save_writes_the_output_devices() {
        let mut app = blank_app();
        app.clock_device = OutputDeviceSettings {
            level_db: -9.0,
            mute: true,
            map: ChannelMap {
                left: 0b0101,
                right: 0b0110,
            },
            unused: UnusedChannels::Silent,
        };
        app.followers = vec![FollowerPref {
            id: "hw:Phones".to_owned(),
            backend: "ALSA".to_owned(),
            name: "Phones".to_owned(),
            cushion_mode: 1,
            ..FollowerPref::default()
        }];
        let (_, saved) = save_text(&mut app);
        assert_eq!(saved.clock_device, app.clock_device);
        assert_eq!(saved.followers, app.followers);
    }

    #[test]
    fn save_is_skipped_while_startup_scan_is_pending() {
        let mut app = absent_studio();
        app.startup = Some(super::StartupGate {
            preferences: Preferences::default(),
        });
        let mut storage = MemoryStorage::default();
        app.save(&mut storage);
        assert!(eframe::Storage::get_string(&storage, PREFERENCES_KEY).is_none());
    }

    #[test]
    fn repaint_interval_is_fast_only_for_a_live_bridge_or_open_editor() {
        for state in [
            EngineState::Starting,
            EngineState::Running,
            EngineState::Stopping,
            EngineState::Preview,
        ] {
            assert_eq!(repaint_interval(state, false), ACTIVE_REPAINT);
            assert_eq!(repaint_interval(state, true), ACTIVE_REPAINT);
        }
        for state in [EngineState::Idle, EngineState::Error] {
            assert_eq!(repaint_interval(state, false), IDLE_REPAINT);
            assert_eq!(repaint_interval(state, true), ACTIVE_REPAINT);
        }
        assert_eq!(ACTIVE_REPAINT.as_millis(), 16);
        assert_eq!(IDLE_REPAINT.as_millis(), 200);
    }

    #[test]
    fn save_keeps_an_output_id_the_scan_did_not_find() {
        let mut app = absent_studio();
        // The window is usable on the fallback. That row is what an unguarded
        // save would write.
        assert_eq!(app.selected_output().id, "default-out");
        assert_eq!(app.selected_asio().id, "asio:other");
        assert!(
            app.input_held.is_none(),
            "an empty input is not a stored identity"
        );
        let (raw, saved) = save_text(&mut app);
        assert!(raw.contains("asio-out:studio"), "{raw}");
        assert!(!raw.contains("default-out"), "{raw}");
        assert_eq!(saved.output_id, "asio-out:studio");
        assert_eq!(saved.output_name.as_deref(), Some("Studio Monitors"));
        assert_eq!(saved.output_backend, "ASIO");
        assert_eq!(saved.asio_id, "asio:focusrite");
        assert_eq!(saved.asio_name.as_deref(), Some("Focusrite USB"));
        assert_eq!(saved.asio_backend, "ASIO");
        // Nothing was stored for the system input, so the live device is recorded.
        assert_eq!(saved.input_id, "hw:Mic");
        assert_eq!(saved.input_name.as_deref(), Some("Mic"));
        assert_eq!(saved.input_backend, "ALSA");
    }

    #[test]
    fn a_successful_restore_saves_the_current_device() {
        let mut app = blank_app();
        app.inventory
            .outputs
            .push(item("default-out", "Speakers", "WASAPI", true));
        app.inventory
            .outputs
            .push(item("hw:Speaker", "Studio", "ALSA", false));
        install(
            &mut app,
            &Preferences {
                output_name: Some("Old Studio Name".to_owned()),
                output_id: "hw:Speaker".to_owned(),
                output_backend: "ALSA".to_owned(),
                ..Preferences::default()
            },
        );
        assert!(app.output_held.is_none());
        assert_eq!(app.selected_output().id, "hw:Speaker");
        assert_ne!(app.selected_output().id, "default-out");
        let (raw, saved) = save_text(&mut app);
        assert!(raw.contains("hw:Speaker"), "{raw}");
        assert!(!raw.contains("default-out"), "{raw}");
        assert!(!raw.contains("Old Studio Name"), "{raw}");
        assert_eq!(saved.output_id, "hw:Speaker");
        assert_eq!(saved.output_name.as_deref(), Some("Studio"));
        assert_eq!(saved.output_backend, "ALSA");
    }

    #[test]
    fn a_legacy_name_match_gains_an_id_on_save() {
        let mut app = blank_app();
        app.inventory
            .outputs
            .push(item("default-out", "System Default", "WASAPI", true));
        app.inventory
            .outputs
            .push(item("wasapi:{spk}", "Speakers", "WASAPI", false));
        install(
            &mut app,
            &Preferences {
                output_name: Some("Speakers".to_owned()),
                output_id: String::new(),
                output_backend: String::new(),
                ..Preferences::default()
            },
        );
        assert!(app.output_held.is_none());
        assert_eq!(app.selected_output().id, "wasapi:{spk}");
        let (raw, saved) = save_text(&mut app);
        assert!(raw.contains("wasapi:{spk}"), "{raw}");
        assert!(!raw.contains("default-out"), "{raw}");
        assert_eq!(saved.output_id, "wasapi:{spk}");
        assert_eq!(saved.output_name.as_deref(), Some("Speakers"));
        assert_eq!(saved.output_backend, "WASAPI");
    }

    #[test]
    fn a_same_name_device_does_not_replace_a_stored_id() {
        let mut app = blank_app();
        app.inventory
            .outputs
            .push(item("wasapi:{spk}", "Speakers", "WASAPI", true));
        install(
            &mut app,
            &Preferences {
                output_name: Some("Speakers".to_owned()),
                output_id: "asio-out:studio".to_owned(),
                output_backend: "ASIO".to_owned(),
                ..Preferences::default()
            },
        );
        assert_eq!(app.selected_output().id, "wasapi:{spk}");
        assert_eq!(
            app.output_held.as_ref().map(|held| held.id.as_str()),
            Some("asio-out:studio")
        );
        let (raw, saved) = save_text(&mut app);
        assert!(raw.contains("asio-out:studio"), "{raw}");
        assert!(!raw.contains("wasapi:{spk}"), "{raw}");
        assert_eq!(saved.output_id, "asio-out:studio");
        assert_eq!(saved.output_name.as_deref(), Some("Speakers"));
        assert_eq!(saved.output_backend, "ASIO");
    }

    #[test]
    fn an_explicit_output_pick_replaces_the_held_identity() {
        let mut app = absent_studio();
        let (_, kept) = save_text(&mut app);
        assert_eq!(kept.output_id, "asio-out:studio");
        app.output_idx = 1;
        app.accept_output_device();
        assert!(app.output_held.is_none());
        let (_, saved) = save_text(&mut app);
        assert_eq!(saved.output_id, "phones");
        assert_eq!(saved.output_name.as_deref(), Some("Headphones"));
        assert_eq!(saved.output_backend, "WASAPI");
        // The other side was not picked, so its stored identity stays.
        assert_eq!(saved.asio_id, "asio:focusrite");
    }

    #[test]
    fn a_preset_that_finds_the_output_may_be_saved() {
        let mut app = absent_studio();
        assert!(app.output_held.is_some());
        app.sessions.push(Preset {
            name: "Phones".to_owned(),
            input: "Other interface".to_owned(),
            input_id: "asio:other".to_owned(),
            input_backend: "ASIO".to_owned(),
            output: "Headphones".to_owned(),
            output_id: "phones".to_owned(),
            output_backend: "WASAPI".to_owned(),
            ..Preset::default()
        });
        app.apply_preset(0);
        assert!(app.output_held.is_none());
        assert!(app.asio_held.is_none());
        assert_eq!(app.selected_output().id, "phones");
        assert_eq!(app.selected_asio().id, "asio:other");
        let (_, saved) = save_text(&mut app);
        assert_eq!(saved.output_id, "phones");
        assert_eq!(saved.output_backend, "WASAPI");
        assert_eq!(saved.asio_id, "asio:other");
    }

    #[test]
    fn a_preset_that_misses_the_device_keeps_the_stored_id() {
        let mut app = absent_studio();
        app.sessions.push(Preset {
            name: "Gone".to_owned(),
            input: "Missing interface".to_owned(),
            input_id: "asio:missing".to_owned(),
            input_backend: "ASIO".to_owned(),
            output: "Missing monitors".to_owned(),
            output_id: "asio-out:nope".to_owned(),
            output_backend: "ASIO".to_owned(),
            ..Preset::default()
        });
        app.apply_preset(0);
        assert_eq!(
            app.output_held.as_ref().map(|held| held.id.as_str()),
            Some("asio-out:studio")
        );
        assert_eq!(
            app.asio_held.as_ref().map(|held| held.id.as_str()),
            Some("asio:focusrite")
        );
        assert_eq!(app.selected_output().id, "default-out");
        let (raw, saved) = save_text(&mut app);
        assert!(raw.contains("asio-out:studio"), "{raw}");
        assert!(!raw.contains("default-out"), "{raw}");
        assert_eq!(saved.output_id, "asio-out:studio");
        assert_eq!(saved.output_name.as_deref(), Some("Studio Monitors"));
        assert_eq!(saved.asio_id, "asio:focusrite");
        assert_eq!(saved.sessions[0].output_id, "asio-out:nope");
    }

    #[test]
    fn nothing_stored_saves_the_selected_device() {
        let mut app = blank_app();
        app.inventory
            .asio
            .push(item("asio:other", "Other interface", "ASIO", false));
        app.inventory
            .inputs
            .push(item("hw:Mic", "Mic", "ALSA", false));
        app.inventory
            .outputs
            .push(item("default-out", "Speakers", "WASAPI", true));
        install(&mut app, &Preferences::default());
        assert!(app.asio_held.is_none());
        assert!(app.input_held.is_none());
        assert!(app.output_held.is_none());
        let (_, saved) = save_text(&mut app);
        assert_eq!(saved.asio_id, "asio:other");
        assert_eq!(saved.input_id, "hw:Mic");
        assert_eq!(saved.output_id, "default-out");
        assert_eq!(saved.output_name.as_deref(), Some("Speakers"));
    }

    #[test]
    fn the_activity_log_keeps_hundreds_of_entries() {
        use super::ACTIVITY_LOG_CAP;

        assert!(
            (200..=500).contains(&ACTIVITY_LOG_CAP),
            "cap {ACTIVITY_LOG_CAP} is outside 200..=500"
        );
        let mut app = blank_app();
        let extra = 25;
        for index in 0..(ACTIVITY_LOG_CAP + extra) {
            app.push_log(format!("entry {index}"), crate::ui::theme::MUTED);
        }
        assert_eq!(app.log.len(), ACTIVITY_LOG_CAP);
        assert_eq!(app.log[0].text, format!("entry {extra}"));
        let last = format!("entry {}", ACTIVITY_LOG_CAP + extra - 1);
        assert_eq!(
            app.log.last().map(|entry| entry.text.as_str()),
            Some(last.as_str())
        );
        assert_ne!(app.log.len(), 6);
    }

    #[test]
    fn a_scan_failure_is_kept_when_the_device_list_does_not_change() {
        let mut app = blank_app();
        app.inventory_fingerprint = super::devices::inventory_fingerprint(&app.inventory);
        let mut failed = app.inventory.clone();
        failed
            .notes
            .push("ASIO registry scan: registry unavailable".to_owned());
        assert!(!app.apply_inventory(failed));
        assert_eq!(
            app.inventory.notes.as_slice(),
            ["ASIO registry scan: registry unavailable"]
        );
        assert_eq!(
            app.log
                .iter()
                .filter(|entry| entry.text.contains("registry unavailable"))
                .count(),
            1
        );

        let again = app.inventory.clone();
        assert!(!app.apply_inventory(again.clone()));
        assert_eq!(
            app.log
                .iter()
                .filter(|entry| entry.text.contains("registry unavailable"))
                .count(),
            1
        );

        let mut cleared = again;
        cleared.notes.clear();
        assert!(!app.apply_inventory(cleared));
        assert!(app.inventory.notes.is_empty());
        assert!(app
            .log
            .iter()
            .any(|entry| entry.text.contains("registry unavailable")));
    }

    #[test]
    fn a_device_change_still_records_a_scan_note() {
        let mut app = blank_app();
        app.inventory_fingerprint = super::devices::inventory_fingerprint(&app.inventory);
        let mut fresh = app.inventory.clone();
        fresh
            .outputs
            .push(item("default-out", "Speakers", "WASAPI", true));
        fresh.notes.push("ASIO registry scan: boom".to_owned());
        assert!(app.apply_inventory(fresh));
        assert_eq!(app.inventory.notes.as_slice(), ["ASIO registry scan: boom"]);
        assert!(app
            .log
            .iter()
            .any(|entry| entry.text.contains("Device change applied")));
        assert_eq!(
            app.log.last().map(|entry| entry.text.as_str()),
            Some("ASIO registry scan: boom")
        );
    }

    #[test]
    fn one_missed_scan_does_not_stop_the_bridge() {
        let mut app = blank_app();
        app.inventory
            .outputs
            .push(item("hw:Speaker", "Studio", "ALSA", false));
        app.output_idx = 0;
        app.bridge_signature = 7;
        app.inventory_fingerprint = super::devices::inventory_fingerprint(&app.inventory);
        let mut gone = app.inventory.clone();
        gone.outputs.clear();
        assert!(!app.apply_inventory(gone.clone()));
        assert_eq!(app.bridge_signature, 7);
        assert_eq!(app.selected_output().id, "hw:Speaker");
        assert!(app
            .log
            .iter()
            .any(|entry| entry.text.contains("missed one device scan")));
        assert!(app.apply_inventory(gone));
        assert_eq!(app.bridge_signature, 0);
        assert!(app
            .log
            .iter()
            .any(|entry| entry.text.contains("unplugged — bridge stopped")));
    }
}
