//! The engine the UI talks to: start, stop, poll and live settings.  The
//! bridge itself runs on its own thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::bridge::run_bridge;
use super::eq::{EqSettings, EqSlot};
#[cfg(target_os = "linux")]
use super::linux::pipewire;
use super::plugins::{PluginPref, PluginView};
use super::shared::Shared;
use super::types::{CushionMode, EngineState, Snapshot, StartConfig};

pub struct AudioEngine {
    /// The session the UI reads and writes.  Every real start replaces it, so
    /// an older bridge can only ever touch the session it was started with.
    pub(crate) shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    /// A bridge that outlived the bounded stop wait.  While it is alive, a
    /// new start must not open a second device session.
    lingering: Option<JoinHandle<()>>,
    /// Rate each session's `Shared` is built at.  Devices negotiate their own
    /// working rates inside the session.
    sample_rate: u32,
    pub state: EngineState,
    /// The quantum that was forced before RigWerk changed it, restored on stop.
    #[cfg(target_os = "linux")]
    previous_quantum: u32,
    /// Whether this engine is the one that set `clock.force-quantum`.
    #[cfg(target_os = "linux")]
    quantum_forced: bool,
}

impl AudioEngine {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            shared: Arc::new(Shared::new(sample_rate)),
            stop: Arc::new(AtomicBool::new(false)),
            handle: None,
            lingering: None,
            sample_rate,
            state: EngineState::Idle,
            #[cfg(target_os = "linux")]
            previous_quantum: 0,
            #[cfg(target_os = "linux")]
            quantum_forced: false,
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
            #[cfg(target_os = "linux")]
            let was_error = self.state == EngineState::Error;
            self.state = EngineState::Error;
            #[cfg(target_os = "linux")]
            if !was_error && self.quantum_forced {
                pipewire::restore_quantum(self.previous_quantum);
                self.previous_quantum = 0;
                self.quantum_forced = false;
            }
            self.shared.set_status(format!("Error: {error}"));
        }
    }

    pub fn start_preview(&mut self) {
        self.stop();
        // A preview starts no callback, but it still gets its own session: a
        // bridge from a stop that timed out keeps the old `Arc<Shared>` and
        // can no longer write status, error or device-loss into what the UI
        // shows as Preview.
        self.replace_session();
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

    /// Swaps in a fresh session that inherits the settings which outlive a
    /// bridge start.  Every older bridge keeps the `Arc<Shared>` it was
    /// started with, so none of them can write into the session the UI now
    /// uses.
    fn replace_session(&mut self) {
        let previous = Arc::clone(&self.shared);
        let mut session = Shared::new(self.sample_rate);
        session.adopt_settings_from(&previous);
        self.shared = Arc::new(session);
        drop(previous);
    }

    pub fn start(&mut self, config: StartConfig) {
        self.stop();
        if self
            .lingering
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
        {
            // The driver ignored the stop flag and still owns the old device
            // session.  Opening the device again would put two streams on the
            // same hardware, so fail visibly instead while it finishes.  The
            // error belongs to the current session, which is the old one.
            self.shared.set_error(
                "The previous bridge is still shutting down and has not released the audio device. Start again once it has finished.",
            );
            self.state = EngineState::Error;
            return;
        }
        self.reap_finished_bridge();

        // Every start gets its own session state.  A bridge that outlived a
        // stop keeps the `Arc<Shared>` it was started with, so it can no
        // longer write `running`, `status`, `error`, `device_lost`, meters or
        // samples into the session this start is about to open.
        self.replace_session();

        self.shared.reset();
        // Ask PipeWire for the low quantum before the output opens, so the
        // first graph cycle already runs at the requested size.  A missing
        // pw-metadata is not fatal: the bridge still opens on the ALSA period.
        #[cfg(target_os = "linux")]
        if config.pipewire_low_latency {
            self.previous_quantum = pipewire::forced_quantum();
            match pipewire::force_low_quantum() {
                Ok(state) => {
                    self.quantum_forced = true;
                    self.shared.set_status(format!(
                        "PipeWire quantum {} frames ({:.1} ms)…",
                        state.quantum,
                        state.quantum as f32 / state.rate.max(1) as f32 * 1000.0
                    ));
                }
                Err(error) => {
                    self.quantum_forced = false;
                    self.shared
                        .set_status(format!("PipeWire quantum not set: {error}"));
                }
            }
        }
        // Open the saved inserts at the rate the output will run at, before
        // the first callback asks for frames.
        self.shared
            .prepare_plugins(config.output_rate.unwrap_or(config.sample_rate));
        self.stop = Arc::new(AtomicBool::new(false));
        self.shared.set_status("Opening independent output…");
        self.state = EngineState::Starting;

        // The closure gets the new session's `Arc` and nothing older, so this
        // bridge's writes cannot reach a later start either.
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
            // Wait a bounded 900 ms; past that, leave the thread to finish on
            // its own and remember the handle instead of blocking the UI
            // forever.
            let deadline = Instant::now() + Duration::from_millis(900);
            while !handle.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            if handle.is_finished() {
                let _ = handle.join();
            } else {
                // Keep the handle so `start` can refuse to open a second
                // device session while this thread still owns the old one.
                self.lingering = Some(handle);
            }
        }
        self.reap_finished_bridge();
        // Everything below touches the current session only.  A bridge that
        // outlived the wait owns that same session, never a newer one.
        self.shared.running.store(false, Ordering::Relaxed);
        self.shared.device_lost.store(false, Ordering::Relaxed);
        // Give PipeWire its own quantum back when the bridge stops.
        #[cfg(target_os = "linux")]
        if self.quantum_forced {
            pipewire::restore_quantum(self.previous_quantum);
            self.previous_quantum = 0;
            self.quantum_forced = false;
        }
        self.shared.set_status("Bridge stopped");
        if let Ok(mut error) = self.shared.error.lock() {
            *error = None;
        }
        self.state = EngineState::Idle;
    }

    /// Joins a bridge that finished after its bounded stop wait, releasing
    /// the old session it kept alive.
    fn reap_finished_bridge(&mut self) {
        if self.lingering.as_ref().is_some_and(JoinHandle::is_finished) {
            if let Some(handle) = self.lingering.take() {
                let _ = handle.join();
            }
        }
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
