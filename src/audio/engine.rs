//! The engine the UI talks to: start, stop, poll and live settings.  The
//! bridge itself runs on its own thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::bridge::run_bridge;
use super::eq::{EqSettings, EqSlot};
use super::plugins::{PluginPref, PluginView};
use super::shared::Shared;
use super::types::{CushionMode, EngineState, Snapshot, StartConfig};

pub struct AudioEngine {
    pub(crate) shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    pub state: EngineState,
}

impl AudioEngine {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            shared: Arc::new(Shared::new(sample_rate)),
            stop: Arc::new(AtomicBool::new(false)),
            handle: None,
            state: EngineState::Idle,
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.shared.snapshot()
    }

    /// Chooses how the cushion is sized.  Takes effect immediately, also while
    /// the bridge runs.
    pub fn set_cushion(&self, mode: CushionMode, manual_ms: f32) {
        self.shared.set_cushion(mode, manual_ms);
    }

    /// Sets one of the built-in EQs.  Takes effect immediately, also while the
    /// bridge runs; calling it with unchanged settings costs nothing.
    pub fn set_eq(&self, slot: EqSlot, settings: EqSettings) {
        self.shared.set_eq(slot, settings);
    }

    pub fn set_gains(&self, input_db: f32, output_db: f32) {
        let in_gain = 10.0f32.powf(input_db / 20.0);
        let out_gain = 10.0f32.powf(output_db / 20.0);
        self.shared
            .input_gain
            .store(in_gain.to_bits(), Ordering::Relaxed);
        self.shared
            .output_gain
            .store(out_gain.to_bits(), Ordering::Relaxed);
    }

    pub fn set_mix(
        &self,
        mute: bool,
        monitor: bool,
        mono: bool,
        input_channels: [bool; 2],
        output_channels: [bool; 2],
        drift: bool,
    ) {
        self.shared
            .set_mix(mute, monitor, mono, input_channels, output_channels, drift);
    }

    pub fn set_routes(&self, routes: [[bool; 4]; 2]) {
        self.shared.set_routes(routes);
    }

    pub fn poll(&mut self) {
        if self.state == EngineState::Starting && self.shared.running.load(Ordering::Relaxed) {
            self.state = EngineState::Running;
        }
        if let Some(error) = self.shared.snapshot().error {
            self.state = EngineState::Error;
            self.shared.set_status(format!("Error: {error}"));
        }
    }

    pub fn start_preview(&mut self) {
        self.stop();
        self.shared.reset();
        self.shared.set_status("Preview bridge · animated signal");
        self.state = EngineState::Preview;
    }

    /// The plugin insert chain as the UI needs it.
    pub fn plugin_views(&self) -> Vec<PluginView> {
        self.shared.plugin_views()
    }

    /// Serves CLAP main-thread callbacks and timers from the UI loop.
    pub fn service_plugins(&self) {
        self.shared.service_plugins();
    }

    pub fn plugin_prefs(&self) -> Vec<PluginPref> {
        self.shared.plugin_prefs()
    }

    /// Restores the saved chain without opening any plugin.
    pub fn restore_plugins(&self, plugins: Vec<PluginPref>) {
        self.shared.restore_plugins(plugins);
    }

    pub fn add_plugin(&self, plugin: PluginPref) -> Result<(), String> {
        self.shared.add_plugin(plugin)
    }

    pub fn remove_plugin(&self, index: usize) {
        self.shared.remove_plugin(index);
    }

    pub fn move_plugin(&self, index: usize, delta: isize) {
        self.shared.move_plugin(index, delta);
    }

    pub fn set_plugin_bypass(&self, index: usize, bypass: bool) {
        self.shared.set_plugin_bypass(index, bypass);
    }

    pub fn set_plugin_mix(&self, index: usize, mix: f32) {
        self.shared.set_plugin_mix(index, mix);
    }

    /// Opens a saved insert on demand so its controls can be edited.
    pub fn ensure_plugin_loaded(&self, index: usize) {
        self.shared.ensure_plugin_loaded(index);
    }

    /// The loaded plugin behind one insert, for its own editor window.
    pub fn plugin_handle(&self, index: usize) -> Option<super::plugins::PluginHandle> {
        self.shared.plugin_handle(index)
    }

    /// Copies control values back after the plugin's own editor changed them.
    pub fn sync_plugin_parameters(&self, index: usize) -> Result<(), String> {
        self.shared.sync_plugin_parameters(index)
    }

    pub fn set_plugin_parameter(&self, index: usize, id: u32, value: f64) -> Result<(), String> {
        self.shared.set_plugin_parameter(index, id, value)
    }

    pub fn reset_plugin_parameters(&self, index: usize) -> Result<(), String> {
        self.shared.reset_plugin_parameters(index)
    }

    pub fn start(&mut self, config: StartConfig) {
        self.stop();
        self.shared.reset();
        // Open the saved inserts at the rate the output will run at, before
        // the first callback asks for frames.
        self.shared
            .prepare_plugins(config.output_rate.unwrap_or(config.sample_rate));
        self.stop = Arc::new(AtomicBool::new(false));
        self.shared.set_status("Opening independent output…");
        self.state = EngineState::Starting;

        let shared = Arc::clone(&self.shared);
        let stop = Arc::clone(&self.stop);
        let handle = thread::Builder::new()
            .name("rigwerk-bridge".to_owned())
            .spawn(move || {
                if let Err(error) = run_bridge(config, Arc::clone(&shared), Arc::clone(&stop)) {
                    shared.set_error(error);
                } else {
                    shared.running.store(false, Ordering::Relaxed);
                    shared.set_status("Bridge stopped");
                }
            });
        match handle {
            Ok(handle) => self.handle = Some(handle),
            Err(error) => {
                self.shared
                    .set_error(format!("Could not start bridge: {error}"));
                self.state = EngineState::Error;
            }
        }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            // A misbehaving ASIO driver can block far longer than a UI frame.
            // Let the bridge thread finish on its own, but never freeze the
            // window while waiting for it.
            let (done_tx, done_rx) = mpsc::channel::<()>();
            let _ = thread::Builder::new()
                .name("rigwerk-wait".to_owned())
                .spawn(move || {
                    let _ = handle.join();
                    let _ = done_tx.send(());
                });
            let _ = done_rx.recv_timeout(Duration::from_millis(900));
        }
        self.shared.running.store(false, Ordering::Relaxed);
        self.shared.device_lost.store(false, Ordering::Relaxed);
        self.shared.set_status("Bridge stopped");
        if let Ok(mut error) = self.shared.error.lock() {
            *error = None;
        }
        self.state = EngineState::Idle;
    }

    /// Clears a stale device error after the bridge has been stopped on
    /// purpose, so the next Start is a clean attempt.
    pub fn clear_error(&mut self) {
        self.shared.device_lost.store(false, Ordering::Relaxed);
        if let Ok(mut error) = self.shared.error.lock() {
            *error = None;
        }
        if self.state == EngineState::Error {
            self.state = EngineState::Idle;
        }
        self.shared.set_status("Bridge stopped");
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.stop();
    }
}
