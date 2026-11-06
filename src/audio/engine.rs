//! The engine the UI talks to: start, stop, poll and live settings.  The
//! bridge itself runs on its own thread.

use std::any::Any;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::bridge::run_bridge;
#[cfg(target_os = "linux")]
use super::linux::pipewire;
use super::output_map::{AlignmentReport, OutputDeviceSettings};
use super::path::{PathMix, PathState, MAX_PATHS};
use super::plugins::{PluginPref, PluginView};
use super::shared::FollowerPlan;
use super::shared::Shared;
use super::types::{CushionMode, EngineState, Snapshot, StartConfig};

/// A driver open can take seconds (ASIO's own timeout is six); a start that
/// never reports live is treated as stuck rather than left on "Linking".
const START_TIMEOUT: Duration = Duration::from_secs(12);
/// Output callbacks advance `frames` even while the input is silent, so a
/// counter that stays put means the output or its thread is gone.
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(4);
/// A driver that never returns from teardown still holds its device.  Surface
/// that instead of leaving the UI on `Stopping` indefinitely.
const STOP_TIMEOUT: Duration = Duration::from_secs(15);
/// Last-resort wait when the engine itself is going away.  Normal stops never
/// sleep here; the UI polls `Stopping` instead.
const DROP_STOP_WAIT: Duration = Duration::from_millis(900);

pub struct AudioEngine {
    /// The session the UI reads and writes.  Every real start replaces it, so
    /// an older bridge can only ever touch the session it was started with.
    pub(crate) shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    /// The thread of the current session.
    handle: Option<JoinHandle<()>>,
    /// Bridge threads whose stop was requested but whose driver has not
    /// returned from teardown yet.  While one is alive it still owns its audio
    /// device, so `start` must not open a second session.
    lingering: Vec<JoinHandle<()>>,
    /// Rate each session's `Shared` is built at.  Devices negotiate their own
    /// working rates inside the session.
    sample_rate: u32,
    pub state: EngineState,
    /// When `Starting` began, for the start timeout.
    started_at: Option<Instant>,
    /// When `Stopping` began, for the teardown watchdog.
    stopping_since: Option<Instant>,
    /// Frames at the last heartbeat check and when they last advanced.
    last_frames: u64,
    last_frames_at: Instant,
    /// Input frames at the last heartbeat check and when they last advanced.
    last_input_frames: u64,
    last_input_frames_at: Instant,
    /// Whether this session should be receiving input blocks while it runs.
    input_expected: bool,
    /// The quantum that was forced before RigWerk changed it, restored on stop.
    /// Meaningful only while `quantum_forced` is set. 0 means the store had no
    /// force, which is different from a read that failed (that does not set
    /// `quantum_forced`).
    #[cfg(target_os = "linux")]
    previous_quantum: u32,
    /// Whether this engine is the one that set `clock.force-quantum`.
    #[cfg(target_os = "linux")]
    quantum_forced: bool,
    /// When a restore failed, the next time to try again. Stop keeps owning
    /// the force until a restore succeeds.
    #[cfg(target_os = "linux")]
    quantum_retry_at: Option<Instant>,
    /// A quantum request or restore that failed. The live status line moves
    /// on; this stays until a later attempt succeeds.
    quantum_note: Option<String>,
}

impl AudioEngine {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            shared: Arc::new(Shared::new(sample_rate)),
            stop: Arc::new(AtomicBool::new(false)),
            handle: None,
            lingering: Vec::new(),
            sample_rate,
            state: EngineState::Idle,
            started_at: None,
            stopping_since: None,
            last_frames: 0,
            last_frames_at: Instant::now(),
            last_input_frames: 0,
            last_input_frames_at: Instant::now(),
            input_expected: false,
            #[cfg(target_os = "linux")]
            previous_quantum: 0,
            #[cfg(target_os = "linux")]
            quantum_forced: false,
            #[cfg(target_os = "linux")]
            quantum_retry_at: None,
            quantum_note: None,
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.shared.snapshot()
    }

    /// Decimated in/out waveform bins for the Live scope (oldest → newest).
    pub(crate) fn copy_scope(&self, input: &mut [f32], output: &mut [f32]) {
        self.shared.copy_scope(input, output);
    }

    /// The live bridge status line. The header pill does not read this; it
    /// follows [`EngineState`], so an error pill stays an error pill.
    pub(crate) fn bridge_status(&self) -> String {
        self.shared
            .status
            .lock()
            .map(|status| status.clone())
            .unwrap_or_default()
    }

    /// A quantum failure that should stay visible after the status line moves on.
    pub(crate) fn quantum_note(&self) -> Option<&str> {
        self.quantum_note.as_deref()
    }

    /// Remembers a quantum failure and publishes it as the current status.
    /// A later status write may replace that line; the note remains.
    #[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
    pub(crate) fn note_quantum_failure(&mut self, note: String) {
        self.shared.set_status(note.clone());
        self.quantum_note = Some(note);
    }

    /// Whether a bridge thread from an earlier start is still alive and may
    /// still own its audio device.  Preview must wait for it instead of
    /// replacing the state while the old hardware is still open.
    pub fn teardown_pending(&self) -> bool {
        self.handle
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
            || self.lingering.iter().any(|handle| !handle.is_finished())
    }

    /// Chooses how the cushion is sized.  Takes effect immediately, also while
    /// the bridge runs.
    pub fn set_cushion(&self, mode: CushionMode, manual_ms: f32) {
        self.shared.set_cushion(mode, manual_ms);
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

    /// Master mute, monitor, input channel enable and drift steering.
    pub fn set_master(&self, mute: bool, monitor: bool, input_channels: [bool; 2], drift: bool) {
        self.shared.set_master(mute, monitor, input_channels, drift);
    }

    /// Sets one path's routing, mono, enable, EQs, gate, level, mute and
    /// solo.  Live, ramped; unchanged settings cost nothing.  Ignored for a
    /// slot that is not active.
    pub fn set_path_mix(&self, slot: usize, mix: &PathMix) {
        let path = self.shared.path(slot);
        if path.state() == PathState::Active {
            path.set_mix(mix);
        }
    }

    /// Puts a path with these settings and plugins into `slot` and makes it
    /// active (used when a session or patch is loaded).  The plugins are
    /// restored, not opened.
    pub fn restore_path(&self, slot: usize, mix: &PathMix, plugins: Vec<PluginPref>) {
        if slot < MAX_PATHS {
            self.shared.restore_path(slot, mix, plugins);
        }
    }

    /// Adds an empty, muted path in a free slot, live.  Returns the slot.
    pub fn add_path(&self) -> Option<usize> {
        self.shared.add_path()
    }

    /// Adds a muted copy of path `source` (chain, plugins with their state)
    /// in a free slot, live.  Returns the slot.  The caller copies the
    /// device maps.
    pub fn duplicate_path(&self, source: usize) -> Option<usize> {
        self.shared.duplicate_path(source)
    }

    /// Removes path `slot` with a short fade-out.  `false` for the last
    /// active path or a slot that is not active.
    pub fn remove_path(&self, slot: usize) -> bool {
        self.shared.remove_path(slot)
    }

    /// Where path slot `slot` is in its life.
    pub(crate) fn path_state(&self, slot: usize) -> PathState {
        self.shared.path(slot).state()
    }

    /// Path `slot`'s plugin chain latency in samples (bypassed inserts
    /// count).
    pub fn path_latency_samples(&self, slot: usize) -> u32 {
        self.shared.path(slot).latency_samples()
    }

    /// Every path slot's chain latency in samples (0 for unused slots), as
    /// the alignment counts it.
    pub fn path_latencies(&self) -> [u32; MAX_PATHS] {
        std::array::from_fn(|slot| self.shared.path(slot).latency_samples())
    }

    /// Output device `device`'s alignment (0 = clock device, then the
    /// followers) as the running mixer applies it: the delay each path
    /// gets there, the path it waits for and the device's total latency
    /// from the chains and the alignment.  `None` while no mixer has
    /// published one for this device (bridge stopped, or a follower that
    /// is not live); the UI then works it out from the settings
    /// (`OutputDeviceSettings::alignment`).
    pub fn device_alignment(&self, device: usize) -> Option<AlignmentReport> {
        if !self.shared.running.load(Ordering::Relaxed) {
            return None;
        }
        let controls = match device {
            0 => &self.shared.clock_device,
            follower if follower <= self.shared.follower_count() => {
                &self.shared.followers[follower - 1]
            }
            _ => return None,
        };
        controls.alignment()
    }

    /// The rate alignment delays are counted at when no bridge runs: the
    /// plugin chains' rate (the last output rate, or the session's).
    pub fn plugin_rate(&self) -> u32 {
        self.shared.path(0).chain_rate()
    }

    /// Path `slot`'s meters: the last block's peaks after its chain, before
    /// the path level and the master.
    pub fn path_meters(&self, slot: usize) -> (f32, f32) {
        self.shared.path(slot).meters()
    }

    /// Sets the clock device's channel map, level and mute (sanitized here).
    /// Takes effect immediately with a short crossfade, also while the
    /// bridge runs.
    pub fn set_clock_device(&self, settings: &OutputDeviceSettings) {
        self.shared.clock_device.set(settings);
    }

    /// Sets follower levels, mutes, maps, cushions and the link / open plan.
    /// Live: not part of the bridge signature.
    pub fn set_followers(
        &self,
        settings: &[OutputDeviceSettings],
        plan: &FollowerPlan,
        cushions: &[(crate::audio::CushionMode, f32)],
    ) {
        self.shared.set_followers(settings, plan, cushions);
    }

    /// Advances the session's observed state.  This is where a stuck start, a
    /// dead output or a bridge thread that ended on its own becomes visible;
    /// the UI calls it once a frame.
    pub fn poll(&mut self) {
        #[cfg(target_os = "linux")]
        self.retry_quantum_release();
        self.reap_finished_bridge();
        let current_finished = self.reap_current_bridge();

        // A user stop owns the state until the old thread confirms teardown.
        // A late error from that same session must not turn Stopping into
        // Error; the watchdog is the only failure path while stopping.
        if self.state == EngineState::Stopping {
            if self.handle.is_none() && self.lingering.is_empty() {
                self.finish_stop();
            } else if self
                .stopping_since
                .is_some_and(|since| since.elapsed() >= STOP_TIMEOUT)
            {
                self.shared.set_error(format!(
                    "The audio driver has not released the device after {} seconds. It is still shutting down; restart RigWerk before starting the bridge again.",
                    STOP_TIMEOUT.as_secs()
                ));
                self.enter_error();
            }
            return;
        }

        if self.current_error().is_some() {
            self.enter_error();
            return;
        }

        if current_finished && matches!(self.state, EngineState::Starting | EngineState::Running) {
            if self.stop.load(Ordering::Relaxed) {
                self.finish_stop();
                return;
            }
            let message = if self.state == EngineState::Starting {
                "The bridge stopped before it became live. The audio driver closed the session; start the bridge again or choose another device."
            } else {
                "The bridge stopped unexpectedly. The audio driver ended the audio session; start the bridge again or choose another device."
            };
            self.shared.set_error(message);
            self.enter_error();
            return;
        }

        match self.state {
            EngineState::Starting => {
                if self.shared.running.load(Ordering::Relaxed) {
                    self.state = EngineState::Running;
                    self.last_frames = self.shared.frames.load(Ordering::Relaxed);
                    self.last_frames_at = Instant::now();
                    self.last_input_frames = self.shared.input_frames.load(Ordering::Relaxed);
                    self.last_input_frames_at = Instant::now();
                } else if self
                    .started_at
                    .is_some_and(|started| started.elapsed() >= START_TIMEOUT)
                {
                    self.stop.store(true, Ordering::Relaxed);
                    // Park the still-live thread before publishing the error,
                    // so the next Start sees teardown pending instead of a
                    // second session on the same device.
                    let pending = self.park_current_bridge() || !self.lingering.is_empty();
                    if pending {
                        self.shared.set_error(format!(
                            "The bridge did not become live within {} seconds. The audio driver is still shutting down and has not released the device; start again after it finishes, or restart RigWerk.",
                            START_TIMEOUT.as_secs()
                        ));
                    } else {
                        self.shared.set_error(format!(
                            "The bridge did not become live within {} seconds. The audio driver may be stuck opening the device; stop the bridge, then start it again or choose another device.",
                            START_TIMEOUT.as_secs()
                        ));
                    }
                    self.enter_error();
                }
            }
            EngineState::Running => {
                let frames = self.shared.frames.load(Ordering::Relaxed);
                if frames != self.last_frames {
                    self.last_frames = frames;
                    self.last_frames_at = Instant::now();
                }
                let input_frames = self.shared.input_frames.load(Ordering::Relaxed);
                if input_frames != self.last_input_frames {
                    self.last_input_frames = input_frames;
                    self.last_input_frames_at = Instant::now();
                }
                let output_stalled = self.last_frames_at.elapsed() >= HEARTBEAT_TIMEOUT;
                let input_stalled =
                    self.input_expected && self.last_input_frames_at.elapsed() >= HEARTBEAT_TIMEOUT;
                if output_stalled || input_stalled {
                    self.stop.store(true, Ordering::Relaxed);
                    let message = match (output_stalled, input_stalled) {
                        (true, true) => format!(
                            "The bridge stopped passing audio: no input or output blocks arrived for {} seconds. A device or its driver may have stalled; stop the bridge and start it again.",
                            HEARTBEAT_TIMEOUT.as_secs()
                        ),
                        (_, true) => format!(
                            "The bridge stopped receiving audio: no input blocks arrived for {} seconds. The input device or its driver may have stalled; stop the bridge and start it again.",
                            HEARTBEAT_TIMEOUT.as_secs()
                        ),
                        _ => format!(
                            "The bridge stopped producing audio: no output frames arrived for {} seconds. The output device or its driver may have stalled; stop the bridge and start it again.",
                            HEARTBEAT_TIMEOUT.as_secs()
                        ),
                    };
                    self.shared.set_error(message);
                    self.enter_error();
                }
            }
            EngineState::Idle
            | EngineState::Preview
            | EngineState::Stopping
            | EngineState::Error => {}
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
        self.started_at = None;
        self.stopping_since = None;
        self.input_expected = false;
        self.last_input_frames = 0;
        self.last_input_frames_at = Instant::now();
        self.state = EngineState::Preview;
    }

    /// Path `path`'s plugin insert chain as the UI needs it.
    pub fn plugin_views(&self, path: usize) -> Arc<[PluginView]> {
        self.shared.path(path).plugin_views()
    }

    /// Serves CLAP main-thread callbacks and timers from the UI loop, and
    /// finishes path removals.
    pub fn service_plugins(&self) {
        self.shared.service_paths();
        self.shared.service_plugins();
    }

    /// Path `path`'s insert chain as it should be saved, with fresh plugin
    /// state.
    pub fn plugin_prefs(&self, path: usize) -> Vec<PluginPref> {
        self.shared.path(path).plugin_prefs()
    }

    /// Restores path `path`'s saved chain without opening any plugin.
    #[cfg(test)]
    pub fn restore_plugins(&self, path: usize, plugins: Vec<PluginPref>) {
        self.shared.path(path).restore_plugins(plugins);
    }

    // `push_slot` hands a mismatched slot back so it can be dropped unlocked.
    #[allow(clippy::result_large_err)]
    pub fn add_plugin(&self, path: usize, plugin: PluginPref) -> Result<(), String> {
        self.shared.path(path).add_plugin(plugin)
    }

    pub fn remove_plugin(&self, path: usize, index: usize) {
        self.shared.path(path).remove_plugin(index);
    }

    pub fn move_plugin(&self, path: usize, index: usize, delta: isize) {
        self.shared.path(path).move_plugin(index, delta);
    }

    pub fn set_plugin_bypass(&self, path: usize, index: usize, bypass: bool) {
        self.shared.path(path).set_plugin_bypass(index, bypass);
    }

    pub fn set_plugin_mix(&self, path: usize, index: usize, mix: f32) {
        self.shared.path(path).set_plugin_mix(index, mix);
    }

    /// Opens a saved insert on demand so its controls can be edited.
    pub fn ensure_plugin_loaded(&self, path: usize, index: usize) {
        self.shared.path(path).ensure_plugin_loaded(index);
    }

    /// The loaded plugin behind one insert, for its own editor window.
    pub fn plugin_handle(&self, path: usize, index: usize) -> Option<super::plugins::PluginHandle> {
        self.shared.path(path).plugin_handle(index)
    }

    /// Copies control values back after the plugin's own editor changed them.
    pub fn sync_plugin_parameters(&self, path: usize, index: usize) -> Result<(), String> {
        self.shared.path(path).sync_plugin_parameters(index)
    }

    /// Copies the editor's last changes back and marks the insert's saved
    /// state for capture, when its editor closes.
    pub fn finish_plugin_editor(&self, path: usize, index: usize) {
        let controls = self.shared.path(path);
        let _ = controls.sync_plugin_parameters(index);
        controls.mark_plugin_state_stale(index);
    }

    pub fn set_plugin_parameter(
        &self,
        path: usize,
        index: usize,
        id: u32,
        value: f64,
    ) -> Result<(), String> {
        self.shared
            .path(path)
            .set_plugin_parameter(index, id, value)
    }

    pub fn reset_plugin_parameters(&self, path: usize, index: usize) -> Result<(), String> {
        self.shared.path(path).reset_plugin_parameters(index)
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
        if self.lingering.iter().any(|handle| !handle.is_finished()) {
            // The driver ignored the stop flag and still owns the old device
            // session.  Opening the device again would put two streams on the
            // same hardware, so fail visibly instead while it finishes.  The
            // error belongs to the current session, which is the old one.
            self.shared.set_error(
                "The previous bridge is still shutting down and has not released the audio device. Start again once it has finished.",
            );
            self.enter_error();
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
        // The previous force is read first. If that read fails, nothing is
        // written, because a later restore would not know the real value and
        // must not invent 0.
        #[cfg(target_os = "linux")]
        if config.pipewire_low_latency {
            self.apply_quantum_force();
        }
        // Plugin load stays on this thread (the UI thread), not on the
        // bridge.  The saved inserts open at the planned rate before the
        // first callback.  If the device opens at another rate, `set_output`
        // publishes it and `service_plugins` reloads the chain here.
        self.shared
            .prepare_plugins(config.output_rate.unwrap_or(config.sample_rate));
        self.stop = Arc::new(AtomicBool::new(false));
        self.shared.set_status("Opening independent output…");
        self.state = EngineState::Starting;
        self.started_at = Some(Instant::now());
        self.stopping_since = None;
        self.last_frames = 0;
        self.last_frames_at = Instant::now();
        self.last_input_frames = 0;
        self.last_input_frames_at = Instant::now();
        // Every real bridge path opens an input; the heartbeat only holds the
        // input side to it when the start actually asked for one.
        self.input_expected = config.input_id.is_some() || config.asio_clsid.is_some();

        // The closure gets the new session's `Arc` and nothing older, so this
        // bridge's writes cannot reach a later start either.
        let shared = Arc::clone(&self.shared);
        let stop = Arc::clone(&self.stop);
        let handle = thread::Builder::new()
            .name("rigwerk-bridge".to_owned())
            .spawn(move || {
                let result = panic::catch_unwind(AssertUnwindSafe(|| {
                    run_bridge(config, shared.clone(), stop)
                }));
                match result {
                    Ok(Ok(())) => {
                        shared.running.store(false, Ordering::Relaxed);
                        shared.set_status("Bridge stopped");
                    }
                    Ok(Err(error)) => shared.set_error(error),
                    Err(payload) => shared.set_error(format!(
                        "The bridge thread panicked: {}",
                        panic_text(&*payload)
                    )),
                }
            });
        match handle {
            Ok(handle) => self.handle = Some(handle),
            Err(error) => {
                self.shared
                    .set_error(format!("Could not start bridge: {error}"));
                self.enter_error();
            }
        }
    }

    /// Asks the bridge to stop without waiting for its driver.  The thread is
    /// moved to `lingering`; `poll` joins it once it finishes and only then
    /// moves the engine back to `Idle`.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.reap_finished_bridge();

        let live = self.park_current_bridge() || !self.lingering.is_empty();
        if live {
            if self.state != EngineState::Stopping {
                self.state = EngineState::Stopping;
                self.stopping_since = Some(Instant::now());
                self.shared.set_status("Stopping bridge…");
            } else if self.stopping_since.is_none() {
                self.stopping_since = Some(Instant::now());
            }
            self.started_at = None;
            self.shared.running.store(false, Ordering::Relaxed);
            self.release_quantum();
            return;
        }

        self.finish_stop();
    }

    /// Enters `Idle` after every bridge thread has confirmed teardown.
    fn finish_stop(&mut self) {
        self.shared.running.store(false, Ordering::Relaxed);
        // No mixer runs any more: what it published is stale.
        self.shared.clock_device.clear_alignment();
        for follower in &self.shared.followers {
            follower.clear_alignment();
        }
        self.shared.device_lost.store(false, Ordering::Relaxed);
        self.release_quantum();
        self.shared.set_status("Bridge stopped");
        if let Ok(mut error) = self.shared.error.lock() {
            *error = None;
        }
        self.started_at = None;
        self.stopping_since = None;
        self.input_expected = false;
        self.state = EngineState::Idle;
    }

    /// Joins a bridge that finished after its stop was requested, releasing
    /// the old session it kept alive.  The result is deliberately ignored:
    /// with a preview or a later session the `Shared` it wrote to is no
    /// longer the one the UI reads.
    fn reap_finished_bridge(&mut self) {
        let mut index = 0;
        while index < self.lingering.len() {
            if self.lingering[index].is_finished() {
                let handle = self.lingering.swap_remove(index);
                let _ = handle.join();
            } else {
                index += 1;
            }
        }
    }

    /// Moves the current thread to `lingering` when it is still alive.
    /// Returns whether it was parked (the caller decides what state that
    /// means; `stop` enters `Stopping`, a failed start enters `Error`).
    fn park_current_bridge(&mut self) -> bool {
        let Some(handle) = self.handle.take() else {
            return false;
        };
        if handle.is_finished() {
            let _ = handle.join();
            false
        } else {
            self.lingering.push(handle);
            true
        }
    }

    /// Joins the current session's thread if it ended, so a clean exit or a
    /// panic cannot leave it running invisibly.  Returns whether it ended.
    fn reap_current_bridge(&mut self) -> bool {
        if !self.handle.as_ref().is_some_and(JoinHandle::is_finished) {
            return false;
        }
        if let Some(handle) = self.handle.take() {
            if handle.join().is_err() && self.current_error().is_none() {
                self.shared
                    .set_error("The bridge thread panicked; start the bridge again.");
            }
        }
        true
    }

    fn current_error(&self) -> Option<String> {
        self.shared
            .error
            .lock()
            .ok()
            .and_then(|error| error.clone())
    }

    /// Publishes an error as the visible engine state and gives a forced
    /// PipeWire quantum back.
    /// Every error ends the session: a bridge thread that reported a failure
    /// but is still alive (a WASAPI or ASIO output that stopped, a device
    /// loss) would otherwise keep its devices and block the next start.
    fn enter_error(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.state = EngineState::Error;
        self.started_at = None;
        self.stopping_since = None;
        self.release_quantum();
        if let Some(error) = self.current_error() {
            self.shared.set_status(format!("Error: {error}"));
        }
    }

    #[cfg(target_os = "linux")]
    fn apply_quantum_force(&mut self) {
        use super::linux::pipewire::{QuantumForce, State};

        let previous = pipewire::read_forced_quantum();
        let mut wrote = false;
        let mut confirmed: Option<State> = None;
        let mut restored = false;
        match &previous {
            Ok(value) => match pipewire::force_low_quantum() {
                Ok(state) => {
                    wrote = true;
                    if state.is_none() {
                        restored = pipewire::restore_quantum(*value).is_ok();
                    }
                    confirmed = state;
                }
                Err(error) => {
                    self.note_quantum_failure(format!("PipeWire quantum not set: {error}"));
                }
            },
            Err(error) => {
                self.note_quantum_failure(format!("PipeWire quantum not set: {error}"));
            }
        }
        match pipewire::decide_quantum_force(previous.ok(), wrote, confirmed.is_some(), restored) {
            QuantumForce::Owned { previous } => {
                self.previous_quantum = previous;
                self.quantum_forced = true;
                self.quantum_retry_at = None;
                if let Some(state) = confirmed {
                    self.quantum_note = None;
                    self.shared.set_status(format!(
                        "PipeWire quantum {} frames ({:.1} ms)…",
                        state.quantum,
                        state.quantum as f32 / state.rate.max(1) as f32 * 1000.0
                    ));
                } else {
                    self.note_quantum_failure(
                        "PipeWire accepted the quantum but did not report it back, and restoring the previous value failed. It will be restored when the bridge stops.".to_owned(),
                    );
                }
            }
            QuantumForce::RolledBack => {
                self.quantum_forced = false;
                self.quantum_retry_at = None;
                self.note_quantum_failure(
                    "PipeWire accepted the quantum but did not report it back, so the previous quantum was restored.".to_owned(),
                );
            }
            QuantumForce::Untouched => {
                self.quantum_forced = false;
                self.quantum_retry_at = None;
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn retry_quantum_release(&mut self) {
        if !self.quantum_forced
            || matches!(self.state, EngineState::Running | EngineState::Starting)
        {
            return;
        }
        let due = self.quantum_retry_at.is_none_or(|at| Instant::now() >= at);
        if due {
            self.release_quantum();
        }
    }

    fn release_quantum(&mut self) {
        #[cfg(target_os = "linux")]
        if self.quantum_forced {
            match pipewire::restore_quantum(self.previous_quantum) {
                Ok(()) => {
                    self.quantum_note = None;
                    self.previous_quantum = 0;
                    self.quantum_forced = false;
                    self.quantum_retry_at = None;
                }
                Err(error) => {
                    self.note_quantum_failure(format!(
                        "PipeWire quantum was not restored: {error}"
                    ));
                    self.quantum_retry_at = Some(Instant::now() + Duration::from_secs(2));
                }
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
        if self.state == EngineState::Error && self.handle.is_none() && self.lingering.is_empty() {
            self.state = EngineState::Idle;
        }
        self.shared.set_status("Bridge stopped");
    }
}

fn panic_text(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("unknown panic")
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.stop();
        // Shutdown is the one place a short wait is worth it: the process is
        // going away and a detached driver thread must not outlive it.
        let deadline = Instant::now() + DROP_STOP_WAIT;
        for handle in self.lingering.drain(..) {
            while !handle.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            if handle.is_finished() {
                let _ = handle.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AudioEngine;

    #[test]
    fn the_bridge_status_is_part_of_the_snapshot() {
        let engine = AudioEngine::new(48_000);
        engine.shared.set_status("Opening independent output…");
        assert_eq!(engine.bridge_status(), "Opening independent output…");
        assert_eq!(engine.snapshot().status, "Opening independent output…");
    }

    #[test]
    fn a_quantum_failure_survives_bridge_stopped() {
        let mut engine = AudioEngine::new(48_000);
        engine.note_quantum_failure(
            "PipeWire quantum was not restored: pw-metadata not available".to_owned(),
        );
        engine.stop();
        assert_eq!(
            engine.quantum_note(),
            Some("PipeWire quantum was not restored: pw-metadata not available")
        );
        assert_eq!(engine.bridge_status(), "Bridge stopped");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_missing_metadata_tool_is_kept_when_the_quantum_is_restored() {
        let tool_missing = std::process::Command::new("pw-metadata")
            .arg("-n")
            .arg("settings")
            .output()
            .is_err();
        let mut engine = AudioEngine::new(48_000);
        engine.quantum_forced = true;
        engine.previous_quantum = 1024;
        engine.stop();
        assert_eq!(engine.bridge_status(), "Bridge stopped");
        if tool_missing {
            let note = engine
                .quantum_note()
                .expect("a missing pw-metadata must be reported");
            assert!(
                note.starts_with("PipeWire quantum was not restored:"),
                "{note}"
            );
        }
    }
}
