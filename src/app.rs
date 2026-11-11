//! The RigWerk window: app state, start-up, the frame loop and saving.
//!
//! Logic is grouped in `app/` (settings, devices, bridge control, patches,
//! screenshots); everything drawn on screen lives in `ui/`.

pub(crate) mod alignment;
pub(crate) mod bridge;
pub(crate) mod budget;
pub(crate) mod calibration;
pub(crate) mod cards;
pub(crate) mod devices;
pub(crate) mod driver_check;
pub(crate) mod issues;
pub(crate) mod latency;
pub(crate) mod paths;
pub(crate) mod plugin_gui;
pub(crate) mod plugins;
pub(crate) mod presets;
pub(crate) mod screenshot;
pub(crate) mod settings;
pub(crate) mod startup;

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use eframe::egui;
use egui::{Align2, Color32, Margin, RichText, Vec2};

use crate::app::devices::{inventory_fingerprint, HeldDevice};
use crate::app::screenshot::ScreenshotJob;
use crate::app::settings::{
    cushion_from_pref, cushion_to_pref, load_preferences, page_from_key, page_key,
    preferences_backup_key, resolved_wasapi_output_mode, store_preferences, unique_presets,
    FollowerPref, Preferences, Preset, PREFERENCES_KEY, PREFERENCES_SCHEMA,
};
use crate::app::startup::{persisted_identity, restore_saved_devices, StartupGate};
use crate::audio::{
    self, AudioEngine, CushionMode, EngineState, EqSlot, Inventory, OutputDeviceSettings, Snapshot,
    WasapiOutputMode, MAX_FOLLOWERS, MAX_OUTPUT_CHANNELS, MAX_PATHS,
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
    /// The master mute.
    pub(crate) mute: bool,
    pub(crate) drift: bool,
    pub(crate) monitor: bool,
    /// Linux low-latency output (PipeWire quantum / ALSA smallest period).
    /// On Windows this stays aligned with [`Self::wasapi_output_mode`] for
    /// older code paths and cross-platform preference saves.
    pub(crate) low_latency_output: bool,
    /// Windows WASAPI clock device mode (Shared / REAL-style / Exclusive).
    pub(crate) wasapi_output_mode: crate::audio::WasapiOutputMode,
    /// Output side rate (0 follows the input) and buffer.
    pub(crate) output_rate: u32,
    pub(crate) output_buffer: usize,
    /// Which output list is shown: the low-latency driver's (ASIO) or the
    /// system's.  Follows the selected output unless that list is empty.
    pub(crate) output_tab_low_latency: bool,
    pub(crate) cushion_mode: CushionMode,
    /// Manual cushion size in milliseconds.
    pub(crate) cushion_ms: f32,
    /// The signal paths by engine slot: name, colour, routing, mono,
    /// enable, EQs, gate, level, mute and solo (see `app::paths`).  At
    /// least one is always `Some`.  Pushed to the engine every frame.
    pub(crate) paths: [Option<paths::SignalPath>; MAX_PATHS],
    /// The live path slots in display (and save) order.
    pub(crate) path_order: Vec<usize>,
    /// The path slot the Mixer and Plugins pages edit.
    pub(crate) selected_path: usize,
    /// Level, mute and per-path channel maps of the clock device (the
    /// selected output), pushed to the engine every frame.
    pub(crate) clock_device: OutputDeviceSettings,
    /// Extra output devices. Stored only; nothing plays them yet.
    pub(crate) followers: Vec<FollowerPref>,
    /// Last EQ slot shown in the inspector (also persisted as `eq_edit`).
    pub(crate) eq_edit: EqSlot,
    /// Selected stage in the Mixer rack / Live chain strip.
    pub(crate) chain_selected: crate::ui::pages::mixer::chain::ChainSelection,
    /// Output device shown on Signal Routing (not persisted; apart from the
    /// Mixer's selection).
    pub(crate) routing_device: usize,
    /// Path slot the Signal Routing presets set (page-local; falls back to
    /// the selected path).
    pub(crate) routing_preset_path: Option<usize>,
    /// The routing card applied last, for "Custom · edited from <card>".
    pub(crate) routing_last_card: Option<crate::app::cards::Card>,
    /// The Live Issues strip shows only its header (not persisted).
    pub(crate) live_issues_collapsed: bool,
    /// What the Live Issues keep between frames (the DSP hysteresis, the
    /// session's underruns, a lost device and the errors already shown).
    /// Moved once per tick by `tick_issues`.
    pub(crate) issue_state: crate::app::issues::IssueState,
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
    /// Smoothed meter and peak per follower channel, from
    /// [`Snapshot::follower_channels`].
    pub(crate) meter_follower_channels: [[f32; MAX_OUTPUT_CHANNELS]; MAX_FOLLOWERS],
    pub(crate) peak_follower_channels: [[f32; MAX_OUTPUT_CHANNELS]; MAX_FOLLOWERS],
    /// Per path slot (L, R): after the path's chain, level, mute and solo,
    /// before the master.  Smoothed and peak-held like the other meters.
    pub(crate) meter_paths: [[f32; 2]; MAX_PATHS],
    pub(crate) peak_paths: [[f32; 2]; MAX_PATHS],
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
    /// The insert (path, index) whose control panel is open.
    pub(crate) plugin_editor: Option<paths::InsertKey>,
    /// The plugin's own editor window, when it has one open.
    pub(crate) plugin_gui: Option<plugin_gui::PluginGuiWindow>,
    /// egui's points-to-pixels factor, read every frame.  A CLAP editor
    /// whose window does not report its own DPI is scaled by this.
    pub(crate) ui_scale: f32,
    /// Filter typed into the configure panel.
    pub(crate) plugin_param_filter: String,
    /// Open the plugin's own interface straight from the insert when it has one.
    pub(crate) prefer_plugin_interface: bool,
    /// A copy of stored preferences that could not be fully read, written
    /// under its own key on the next save so that save cannot lose it.
    preferences_backup: Option<(String, String)>,
    /// The last measured loopback and the conditions it was made under
    /// (persisted; see `app::latency` for staleness).
    pub(crate) measured_latency: Option<latency::MeasuredLatency>,
    /// The calibration panel: open, what it is doing, and how the loop is
    /// closed.  Not persisted.
    pub(crate) calibration: calibration::CalibrationUi,
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

impl eframe::App for RigWerkApp {
    /// Non-painting maintenance.  eframe also calls this while the window is
    /// hidden or minimized, with no egui pass, so engine watchdogs, device
    /// watching, plugin main-thread/timer servicing and the Live Issues
    /// state keep running.  The timed repaint request at the end is what
    /// keeps those hidden passes coming: 16 ms while the bridge is live or
    /// a plugin editor is open, otherwise 200 ms.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.meters(ctx);
        self.poll_calibration();
        self.watch_devices();
        self.poll_plugin_scan();
        self.engine.service_plugins();
        self.ui_scale = ctx.pixels_per_point();
        self.poll_plugin_gui();
        self.sync_bridge_config();
        self.tick_issues(Instant::now());
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
                // Mixer and Plugins scroll inside their own panes and fill the
                // remaining height, so they stay outside the page ScrollArea.
                if matches!(self.page, Page::Mixer | Page::Plugins) {
                    self.header(ui);
                    ui.add_space(14.0);
                    if self.page == Page::Mixer {
                        self.mixer_page(ui);
                    } else {
                        self.plugins_page(ui);
                    }
                    return;
                }
                let mut page_scroll = egui::ScrollArea::vertical().auto_shrink([false, false]);
                if let Some(offset) = self
                    .screenshot
                    .as_ref()
                    .and_then(|_| screenshot::screenshot_scroll())
                {
                    page_scroll = page_scroll.vertical_scroll_offset(offset);
                }
                page_scroll.show(ui, |ui| {
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
                .fixed_size(Vec2::new(420.0, 340.0))
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
                            "An independent-clock audio bridge. Capture and playback keep their own rates; a short ring absorbs the drift. Extra outputs can follow the finished mix.",
                        )
                        .color(MUTED),
                    );
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new(
                            "Windows: ASIO and WASAPI (DirectSound and MME when the host lists them). Mixer can add WASAPI followers; Publish uses an installed virtual cable such as VB-Audio Cable. Linux: PipeWire is the low-latency path (own nodes linked to the source and sink you pick), with direct ALSA hw: available and PulseAudio on the system side. PipeWire clocks can add follower sinks and RigWerk Out. The ALSA period is compared with the PipeWire quantum instead of hiding a mismatch.",
                        )
                        .size(ts(10.5))
                        .color(GREEN),
                    );
                    ui.label(
                        RichText::new(
                            "Pick any listed input and output. Whether one interface can do both jobs at once depends on its driver; RigWerk reports what that driver allows instead of guessing.",
                        )
                        .size(ts(10.0))
                        .color(MUTED),
                    );
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(format!(
                            "Version {} · preview build",
                            env!("CARGO_PKG_VERSION")
                        ))
                        .color(DIM),
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
        self.calibration_window(&ctx);

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
        // A VST3 editor's changes may be younger than the last poll (and CLAP
        // has no change feed), so `saved_paths` reads the open editor in full
        // before it lists the inserts.
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
        let (paths, order) = self.saved_paths();
        let selected_path = self.saved_selected_path(&order);
        let (clock_device, followers) = self.saved_devices(&order);
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
            mute: self.mute,
            drift: self.drift,
            monitor: self.monitor,
            low_latency_output: self.low_latency_output,
            wasapi_output_mode: Some(if cfg!(target_os = "windows") {
                self.wasapi_output_mode
            } else {
                WasapiOutputMode::from_low_latency(self.low_latency_output)
            }),
            output_rate: self.output_rate,
            output_buffer: self.output_buffer,
            cushion_mode: cushion_to_pref(self.cushion_mode),
            cushion_ms: self.cushion_ms,
            paths,
            selected_path,
            prefer_plugin_interface: self.prefer_plugin_interface,
            clock_device,
            followers,
            measured_latency: self.measured_latency.clone(),
            // The legacy mix fields are not written.
            ..Preferences::default()
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
            mute: preferences.mute,
            drift: preferences.drift,
            monitor: preferences.monitor,
            low_latency_output: {
                let mode = resolved_wasapi_output_mode(&preferences);
                if cfg!(target_os = "windows") {
                    matches!(mode, crate::audio::WasapiOutputMode::RealStyle)
                } else {
                    preferences.low_latency_output
                }
            },
            wasapi_output_mode: resolved_wasapi_output_mode(&preferences),
            output_rate: preferences.output_rate,
            output_buffer: preferences.output_buffer,
            output_tab_low_latency: preferences.output_tab_low_latency,
            cushion_mode: cushion_from_pref(preferences.cushion_mode),
            cushion_ms: preferences.cushion_ms,
            // Filled by `install_paths` below.
            paths: std::array::from_fn(|_| None),
            path_order: Vec::new(),
            selected_path: 0,
            clock_device: preferences.clock_device.sanitized(),
            followers: {
                let mut followers: Vec<_> = preferences
                    .followers
                    .iter()
                    .cloned()
                    .map(FollowerPref::sanitized)
                    .collect();
                followers.truncate(crate::audio::MAX_FOLLOWERS);
                followers
            },
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
            routing_device: 0,
            routing_preset_path: None,
            routing_last_card: None,
            live_issues_collapsed: false,
            issue_state: Default::default(),
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
            meter_follower_channels: [[0.0; MAX_OUTPUT_CHANNELS]; MAX_FOLLOWERS],
            meter_paths: [[0.0; 2]; MAX_PATHS],
            peak_paths: [[0.0; 2]; MAX_PATHS],
            peak_follower_channels: [[0.0; MAX_OUTPUT_CHANNELS]; MAX_FOLLOWERS],
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
            ui_scale: 1.0,
            plugin_param_filter: String::new(),
            prefer_plugin_interface: preferences.prefer_plugin_interface,
            preferences_backup: loaded.backup,
            measured_latency: preferences.measured_latency.clone(),
            calibration: calibration::CalibrationUi::default(),
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
        app.engine
            .set_master(app.mute, app.monitor, app.input_channels, app.drift);
        // Old files (and no file) load as one path.
        let (clock, followers) = (app.clock_device, app.followers.clone());
        app.install_paths(
            &preferences.path_prefs(),
            clock,
            followers,
            preferences.selected_path,
        );
        app.engine.set_cushion(app.cushion_mode, app.cushion_ms);
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
        crate::app::settings::rematch_windows_virtual_out(
            &mut self.followers,
            &self.inventory.outputs,
        );
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
        mute: false,
        drift: true,
        monitor: true,
        low_latency_output: true,
        wasapi_output_mode: WasapiOutputMode::RealStyle,
        output_rate: 0,
        output_buffer: 128,
        output_tab_low_latency: false,
        cushion_mode: CushionMode::Safe,
        cushion_ms: 10.0,
        paths: std::array::from_fn(|slot| (slot == 0).then(paths::SignalPath::default)),
        path_order: vec![0],
        selected_path: 0,
        clock_device: OutputDeviceSettings::default(),
        followers: Vec::new(),
        eq_edit: EqSlot::Pre,
        chain_selected: crate::ui::pages::mixer::chain::ChainSelection::PreEq,
        routing_device: 0,
        routing_preset_path: None,
        routing_last_card: None,
        live_issues_collapsed: false,
        issue_state: Default::default(),
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
        meter_follower_channels: [[0.0; MAX_OUTPUT_CHANNELS]; MAX_FOLLOWERS],
        meter_paths: [[0.0; 2]; MAX_PATHS],
        peak_paths: [[0.0; 2]; MAX_PATHS],
        peak_follower_channels: [[0.0; MAX_OUTPUT_CHANNELS]; MAX_FOLLOWERS],
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
        ui_scale: 1.0,
        plugin_param_filter: String::new(),
        prefer_plugin_interface: false,
        preferences_backup: None,
        measured_latency: None,
        calibration: calibration::CalibrationUi::default(),
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
mod tests;
