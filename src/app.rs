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
use std::time::Instant;

use eframe::egui;
use egui::{Align2, Color32, Margin, RichText, Vec2};

use crate::app::devices::{default_index, inventory_fingerprint};
use crate::app::screenshot::ScreenshotJob;
use crate::app::settings::{
    cushion_from_pref, cushion_to_pref, page_from_key, page_key, unique_presets, Preferences,
    Preset, PREFERENCES_KEY,
};
use crate::audio::{self, AudioEngine, CushionMode, EqSettings, EqSlot, Inventory, Snapshot};
use crate::ui::theme::{configure_style, AMBER, BG, BLUE, DIM, GREEN, MUTED, SIDEBAR, TEAL, TEXT};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Page {
    Live,
    Mixer,
    Routing,
    Devices,
    Sessions,
    Plugins,
    Guide,
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
        _ => Page::Live,
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
    /// Which of the two the EQ card is editing.
    pub(crate) eq_edit: EqSlot,
    pub(crate) meter_in_l: f32,
    pub(crate) meter_in_r: f32,
    pub(crate) meter_out_l: f32,
    pub(crate) meter_out_r: f32,
    pub(crate) peak_in_l: f32,
    pub(crate) peak_in_r: f32,
    pub(crate) peak_out_l: f32,
    pub(crate) peak_out_r: f32,
    pub(crate) snapshot: Snapshot,
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
}

impl eframe::App for RigWerkApp {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = root.ctx().clone();
        self.handle_screenshot(&ctx);
        self.meters(&ctx);
        self.watch_devices();
        self.poll_plugin_scan();
        self.engine.service_plugins();
        self.poll_plugin_gui();
        self.sync_bridge_config();
        ctx.request_repaint_after(std::time::Duration::from_millis(16));

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
                        }
                        ui.add_space(18.0);
                    });
            });

        if self.show_about {
            egui::Window::new("About RigWerk")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                .show(&ctx, |ui| {
                    ui.set_width(360.0);
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
                            "ASIO and system-host inputs and outputs are wired on Windows. Linux uses ALSA: direct hw: devices on the low-latency side, and the desktop's PipeWire, PulseAudio or default device on the other.",
                        )
                        .size(10.5)
                        .color(GREEN),
                    );
                    ui.label(
                        RichText::new(
                            "Select any input and output; whether one interface can do both jobs at once depends on its driver, so RigWerk reports what the driver says instead of guessing.",
                        )
                        .size(10.0)
                        .color(MUTED),
                    );
                    ui.add_space(8.0);
                    ui.label(RichText::new("Version 0.2 · preview build · working title").color(DIM));
                    ui.add_space(8.0);
                    if ui.button("Close").clicked() {
                        self.show_about = false;
                    }
                });
        }

        self.plugin_editor_window(&ctx);

        if let Some(message) = self.device_alert.clone() {
            egui::Window::new("Audio device disconnected")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                .show(&ctx, |ui| {
                    ui.set_width(380.0);
                    ui.label(RichText::new(message).size(12.5).color(TEXT));
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(
                            "Plug the device back in (or pick another one on the Devices page), then start the bridge again.",
                        )
                        .size(10.5)
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
        let preferences = Preferences {
            auto_start_bridge: self.auto_start_bridge,
            sessions: self.sessions.clone(),
            session_name: self.session_name.clone(),
            page: page_key(self.page).to_owned(),
            eq_edit: self.eq_edit.index() as u8,
            output_tab_low_latency: self.output_tab_low_latency,
            selected_session: self.selected_session,
            asio_name: Some(self.selected_asio().name.clone()),
            input_name: self
                .inventory
                .inputs
                .get(self.input_idx)
                .map(|device| device.name.clone()),
            output_name: Some(self.selected_output().name.clone()),
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
            plugins: self.engine.plugin_prefs(),
            prefer_plugin_interface: self.prefer_plugin_interface,
        };
        eframe::set_value(storage, PREFERENCES_KEY, &preferences);
    }
}

impl RigWerkApp {
    pub fn new(cc: &eframe::CreationContext<'_>, screenshot: Option<PathBuf>) -> Self {
        configure_style(&cc.egui_ctx);
        let stored: Option<Preferences> = if screenshot.is_some() {
            // A screenshot run must be reproducible and must never open audio
            // hardware, so it starts from defaults instead of saved settings.
            None
        } else {
            cc.storage
                .and_then(|storage| eframe::get_value(storage, PREFERENCES_KEY))
        };
        let preferences = stored.clone().unwrap_or_default();
        // The screenshot path is a headless check, not a practice session.
        // Keep real drivers closed there even if auto-start was left on.
        let screenshot_mode = screenshot.is_some();
        let inventory = audio::scan_devices();
        let inventory_fingerprint = inventory_fingerprint(&inventory);
        // Restore the pair the person left the app with.  Only when nothing
        // has been saved yet (or the saved device is gone) does the system's
        // own default, or the first real device, get used.
        let mut asio_idx = default_index(&inventory.asio);
        let mut output_idx = default_index(&inventory.outputs);
        let mut input_idx = 0usize;
        if let Some(name) = &preferences.asio_name {
            if let Some(index) = inventory
                .asio
                .iter()
                .position(|device| &device.name == name || &device.id == name)
            {
                asio_idx = index;
            }
        }
        if let Some(name) = &preferences.input_name {
            if let Some(index) = inventory
                .inputs
                .iter()
                .position(|device| &device.name == name || &device.id == name)
            {
                input_idx = index;
            }
        }
        if let Some(name) = &preferences.output_name {
            if let Some(index) = inventory
                .outputs
                .iter()
                .position(|device| &device.name == name || &device.id == name)
            {
                output_idx = index;
            }
        }
        let default_use_asio = !(inventory
            .asio
            .first()
            .map(|device| device.demo)
            .unwrap_or(true)
            && inventory
                .inputs
                .first()
                .map(|device| !device.demo)
                .unwrap_or(false));
        let use_asio_input = match &stored {
            Some(saved) => saved.use_asio_input,
            None => default_use_asio,
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
            output_buffer: preferences.output_buffer.max(16),
            output_tab_low_latency: preferences.output_tab_low_latency,
            cushion_mode: cushion_from_pref(preferences.cushion_mode),
            cushion_ms: if preferences.cushion_ms.is_finite() {
                preferences.cushion_ms.clamp(1.0, 40.0)
            } else {
                10.0
            },
            eqs: [preferences.eq.sanitized(), preferences.eq_post.sanitized()],
            eq_edit: if preferences.eq_edit == 1 {
                EqSlot::Post
            } else {
                EqSlot::Pre
            },
            routes: preferences.routes,
            meter_in_l: 0.0,
            meter_in_r: 0.0,
            meter_out_l: 0.0,
            meter_out_r: 0.0,
            peak_in_l: 0.0,
            peak_in_r: 0.0,
            peak_out_l: 0.0,
            peak_out_r: 0.0,
            snapshot: Snapshot::default(),
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
        };
        app.engine.set_gains(app.input_gain, app.output_gain);
        app.engine.set_mix(
            app.mute,
            app.monitor,
            app.mono,
            app.input_channels,
            app.output_channels,
            app.drift,
        );
        app.engine.set_cushion(app.cushion_mode, app.cushion_ms);
        for slot in EqSlot::ALL {
            app.engine.set_eq(slot, app.eqs[slot.index()]);
        }
        app.engine.restore_plugins(preferences.plugins.clone());
        if !screenshot_mode {
            app.begin_plugin_scan();
        }
        let low_label = app.inventory.low_latency_label.clone();
        app.push_log(
            format!("{low_label} or system input: the chosen output opens independently."),
            TEAL,
        );
        if app.inventory.asio.iter().all(|driver| driver.demo) {
            app.push_log(
                format!("Preview driver loaded — connect a {low_label} input to go live."),
                AMBER,
            );
        } else {
            app.push_log(
                format!("{low_label} input detected. Ready to go live."),
                GREEN,
            );
        }
        app.push_log("Output stays independent — shared mode is allowed.", BLUE);
        let input_name = app.active_input().name.clone();
        let output_name = app.selected_output().name.clone();
        let output_hint = if app.selected_output().preferred {
            if cfg!(target_os = "windows") {
                " (Windows default)"
            } else {
                " (system default)"
            }
        } else {
            ""
        };
        app.push_log(
            format!("Ready: {input_name} → {output_name}{output_hint}."),
            TEAL,
        );
        if app.auto_start_bridge {
            app.push_log("Auto-start is on — starting the bridge.", BLUE);
            app.start_bridge();
        } else {
            app.push_log(
                "Bridge stopped at launch. Start it when you're ready.",
                MUTED,
            );
        }
        app
    }

    pub(crate) fn push_log(&mut self, text: impl Into<String>, tone: Color32) {
        let elapsed = self.started.elapsed().as_secs();
        self.log.push(LogEntry {
            at: format!("{:02}:{:02}", elapsed / 60, elapsed % 60),
            text: text.into(),
            tone,
        });
        if self.log.len() > 6 {
            self.log.remove(0);
        }
    }
}
