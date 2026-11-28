//! One [`AudioEngine::start`] replaces `Shared`.  An old bridge may only
//! touch the run it was started with.  Do not open a new run while
//! teardown still owns the device.
//!
//! An ASIO control panel is opened on the bridge thread, never the UI
//! thread.

use std::any::Any;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::asio_panel::AsioPanelSide;
use super::bridge::run_bridge;
use super::cab::{CabKernel, CabPark};
use super::calibration::CalibrationState;
#[cfg(target_os = "linux")]
use super::linux::pipewire;
#[cfg(target_os = "linux")]
use super::linux::sink_monitor::SinkMonitor;
use super::metronome::{click_level_gain, ClickRhythm};
use super::nam::{NamPark, NamStack, PreparedNam};
use super::output_map::{AlignmentReport, OutputDeviceSettings, MAX_EXTRA_OUTPUTS};
use super::path::{DspTime, PathMix, PathState, MAX_PATHS};
use super::plugins::{PluginPref, PluginView, SlotPark};
use super::shared::ExtraOutputPlan;
use super::shared::Shared;
use super::types::{BufferMode, EngineState, Snapshot, StartConfig};
use super::PluginLockReport;

/// A start that never goes live is stuck.  ASIO's own open timeout is
/// six seconds.
const START_TIMEOUT: Duration = Duration::from_secs(12);
/// Output callbacks advance `frames` through silence, so a stuck counter
/// means the output or its thread is gone.
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(4);
/// Teardown that never returns still holds the device.
const STOP_TIMEOUT: Duration = Duration::from_secs(15);
/// Process exit only.  A normal stop polls `Stopping` and does not sleep.
const DROP_STOP_WAIT: Duration = Duration::from_millis(900);

pub struct AudioEngine {
    /// Replaced on every real start.  An older bridge only touches the run
    /// it was started with.
    pub(crate) shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    /// Stop requested, teardown not back.  While one is alive it owns the
    /// device, so `start` must not open another run.
    lingering: Vec<JoinHandle<()>>,
    /// The rate `Shared` is built at.  Devices negotiate their own rates
    /// inside the run.
    sample_rate: u32,
    pub state: EngineState,
    started_at: Option<Instant>,
    stopping_since: Option<Instant>,
    last_frames: u64,
    last_frames_at: Instant,
    last_input_frames: u64,
    last_input_frames_at: Instant,
    input_expected: bool,
    /// Reported only while `Running`, so the panel stays disabled until
    /// that driver is open.
    asio_input_clsid: Option<u128>,
    /// Same lifetime as [`Self::asio_input_clsid`].
    asio_output_clsid: Option<u128>,
    /// The quantum before this engine forced one, restored on stop.
    /// 0 means the store had no force; a failed read does not set
    /// `quantum_forced`.
    #[cfg(target_os = "linux")]
    previous_quantum: u32,
    /// This engine set `clock.force-quantum`.
    #[cfg(target_os = "linux")]
    quantum_forced: bool,
    /// Stop keeps the force until a restore succeeds.
    #[cfg(target_os = "linux")]
    quantum_retry_at: Option<Instant>,
    /// Stays after the status line moves on.
    quantum_note: Option<String>,
    /// Open only for a Low latency PipeWire sink, on the UI thread.
    #[cfg(target_os = "linux")]
    sink_monitor: Option<SinkMonitor>,
    #[cfg(test)]
    test_sink_peak: Option<f32>,
    /// Outlives a bridge thread, so that thread can hand models back
    /// without dropping them.
    nam_park: Arc<NamPark>,
    /// Same lifetime as [`Self::nam_park`].
    cab_park: Arc<CabPark>,
    /// Shared by every run, so a bridge that defers after
    /// [`Self::replace_run`] still has a live owner.
    slot_park: Arc<SlotPark>,
    /// A new run does not inherit a live model.  The UI reinstalls
    /// captures when this changes.
    run_id: u64,
}

impl AudioEngine {
    pub fn new(sample_rate: u32) -> Self {
        let shared = Shared::new(sample_rate);
        let nam_park = Arc::clone(shared.nam_park());
        let cab_park = Arc::clone(shared.cab_park());
        let slot_park = Arc::clone(shared.plugin_park());
        Self {
            shared: Arc::new(shared),
            nam_park,
            cab_park,
            slot_park,
            run_id: 1,
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
            asio_input_clsid: None,
            asio_output_clsid: None,
            #[cfg(target_os = "linux")]
            previous_quantum: 0,
            #[cfg(target_os = "linux")]
            quantum_forced: false,
            #[cfg(target_os = "linux")]
            quantum_retry_at: None,
            quantum_note: None,
            #[cfg(target_os = "linux")]
            sink_monitor: None,
            #[cfg(test)]
            test_sink_peak: None,
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.shared.snapshot()
    }

    /// 0 until the input has written a whole window.
    pub(crate) fn copy_tap(&self, out: &mut [f32], frames: usize) -> usize {
        self.shared.copy_tap(out, frames)
    }

    /// The requested rate, until the input has reported its own.
    pub(crate) fn active_rate(&self) -> u32 {
        match self.shared.source_rate.load(Ordering::Relaxed) {
            0 => self.sample_rate,
            rate => rate,
        }
    }

    pub(crate) fn copy_scope(&self, input: &mut [f32], output: &mut [f32]) {
        self.shared.copy_scope(input, output);
    }

    /// The header pill follows [`EngineState`], not this line.
    pub(crate) fn bridge_status(&self) -> String {
        self.shared
            .status
            .lock()
            .map(|status| status.clone())
            .unwrap_or_default()
    }

    pub(crate) fn quantum_note(&self) -> Option<&str> {
        self.quantum_note.as_deref()
    }

    /// The bridge thread that owns the driver opens the panel between
    /// callbacks.
    pub(crate) fn request_asio_panel(&self, side: AsioPanelSide) {
        self.shared.asio_panel.request(side);
    }

    /// `None` unless `Running`, so the panel stays disabled until the
    /// driver is open.
    pub(crate) fn open_asio_driver(&self, side: AsioPanelSide) -> Option<u128> {
        if self.state != EngineState::Running {
            return None;
        }
        match side {
            AsioPanelSide::Input => self.asio_input_clsid,
            AsioPanelSide::Output => self.asio_output_clsid,
        }
    }

    /// An already-open panel still finishes.
    fn forget_asio_panel(&mut self) {
        self.asio_input_clsid = None;
        self.asio_output_clsid = None;
        self.shared.asio_panel.clear_requests();
    }

    /// `None` leaves the silence check inert.
    pub(crate) fn sink_monitor_peak(&mut self, now: Instant) -> Option<f32> {
        #[cfg(test)]
        if self.test_sink_peak.is_some() {
            return self.test_sink_peak;
        }
        #[cfg(target_os = "linux")]
        if let Some(monitor) = self.sink_monitor.as_mut() {
            return monitor.read(now);
        }
        let _ = now;
        None
    }

    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn set_test_sink_peak(&mut self, peak: Option<f32>) {
        self.test_sink_peak = peak;
    }

    fn close_sink_monitor(&mut self) {
        #[cfg(target_os = "linux")]
        {
            self.sink_monitor = None;
        }
    }

    /// A later status write may replace the line.  The note remains.
    #[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
    pub(crate) fn note_quantum_failure(&mut self, note: String) {
        self.shared.set_status(note.clone());
        self.quantum_note = Some(note);
    }

    /// Preview must wait: the old hardware may still be open.
    pub fn teardown_pending(&self) -> bool {
        self.handle
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
            || self.lingering.iter().any(|handle| !handle.is_finished())
    }

    /// Live, including while the bridge runs.
    pub fn set_bridge_buffer(&self, mode: BufferMode, fixed_ms: f32) {
        self.shared.set_bridge_buffer(mode, fixed_ms);
    }

    /// The level is a linear gain: 0 dB is 1, so the accent peak is the
    /// safety ceiling.  Tempo and level are already clamped.
    pub fn set_metronome(&self, running: bool, bpm: f32, rhythm: ClickRhythm, level_db: f32) {
        let gain = click_level_gain(level_db);
        self.shared
            .metronome_running
            .store(running, Ordering::Relaxed);
        self.shared
            .metronome_bpm
            .store(bpm.to_bits(), Ordering::Relaxed);
        self.shared
            .metronome_rhythm
            .store(rhythm as u8, Ordering::Relaxed);
        self.shared
            .metronome_gain
            .store(gain.to_bits(), Ordering::Relaxed);
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

    pub fn set_master(&self, mute: bool, input_channels: [bool; 2], drift: bool) {
        self.shared.set_master(mute, input_channels, drift);
    }

    /// Ignored unless the slot is active.
    pub fn set_path_mix(&self, slot: usize, mix: &PathMix) {
        let path = self.shared.path(slot);
        if path.state() == PathState::Active {
            path.set_mix(mix);
        }
    }

    /// The plugins are restored, not opened.
    pub fn restore_path(
        &self,
        slot: usize,
        mix: &PathMix,
        nam: &NamStack,
        plugins: Vec<PluginPref>,
    ) {
        if slot < MAX_PATHS {
            self.shared.restore_path(slot, mix, nam, plugins);
        }
    }

    pub fn add_path(&self) -> Option<usize> {
        self.shared.add_path()
    }

    /// The caller copies the device maps.
    pub fn duplicate_path(&self, source: usize) -> Option<usize> {
        self.shared.duplicate_path(source)
    }

    /// `false` for the last active path, or a slot that is not active.
    pub fn remove_path(&self, slot: usize) -> bool {
        self.shared.remove_path(slot)
    }

    pub(crate) fn path_state(&self, slot: usize) -> PathState {
        self.shared.path(slot).state()
    }

    /// Bypassed effects count.  Never a return.
    pub fn path_latency_samples(&self, slot: usize) -> u32 {
        self.shared.path(slot).path_latency_samples()
    }

    /// As alignment counts it.  A return also waits for its slowest sender.
    /// The cabinet adds none.
    pub fn path_latencies(&self) -> [u32; MAX_PATHS] {
        self.shared.alignment_latencies()
    }

    pub(crate) fn opened_output_rate(&self) -> u32 {
        self.shared.output_rate.load(Ordering::Relaxed)
    }

    pub(crate) fn run_id(&self) -> u64 {
        self.run_id
    }

    /// UI thread only.  Called once a frame, so the parks do not fill.
    pub(crate) fn flush_parks(&self) {
        self.nam_park.flush();
        self.cab_park.flush();
    }

    pub(crate) fn path_generation(&self, slot: usize) -> u64 {
        self.shared.path(slot).generation()
    }

    /// UI thread only.
    pub(crate) fn queue_nam(&self, slot: usize, id: u32, prepared: PreparedNam) {
        if slot < MAX_PATHS {
            self.shared.path(slot).queue_nam(id, prepared);
        }
    }

    /// UI thread only.
    pub(crate) fn clear_nam(&self, slot: usize, id: u32) {
        if slot < MAX_PATHS {
            self.shared.path(slot).clear_nam(id);
        }
    }

    /// The models stay with the run that loaded them.
    pub(crate) fn set_path_nam(&self, slot: usize, nam: &NamStack) {
        if slot < MAX_PATHS {
            let path = self.shared.path(slot);
            if path.state() == PathState::Active {
                path.set_nam(nam);
            }
        }
    }

    /// UI thread only.
    pub(crate) fn path_nam(&self, slot: usize) -> NamStack {
        if slot < MAX_PATHS {
            self.shared.path(slot).nam()
        } else {
            NamStack::default()
        }
    }

    /// UI thread only.
    pub(crate) fn queue_cab(&self, slot: usize, kernel: CabKernel) {
        if slot < MAX_PATHS {
            self.shared.path(slot).queue_cab(kernel);
        }
    }

    /// UI thread only.
    pub(crate) fn clear_cab(&self, slot: usize) {
        if slot < MAX_PATHS {
            self.shared.path(slot).clear_cab();
        }
    }

    /// 0 is the clock output, then the extra outputs.  `None` while no
    /// mixer has published one; the UI then uses the settings.
    pub fn device_alignment(&self, device: usize) -> Option<AlignmentReport> {
        if !self.shared.running.load(Ordering::Relaxed) {
            return None;
        }
        let controls = match device {
            0 => &self.shared.clock_output,
            extra_output if extra_output <= self.shared.extra_output_count() => {
                &self.shared.extra_outputs[extra_output - 1]
            }
            _ => return None,
        };
        controls.alignment()
    }

    /// The chains' rate when no bridge runs.
    pub fn plugin_rate(&self) -> u32 {
        self.shared.path(0).chain_rate()
    }

    pub fn path_meters(&self, slot: usize) -> (f32, f32) {
        self.shared.path(slot).meters()
    }

    pub fn path_dsp_time(&self, slot: usize) -> Option<DspTime> {
        if !self.shared.running.load(Ordering::Relaxed) {
            return None;
        }
        let time = self.shared.path(slot).dsp_time();
        (time.block_ns > 0).then_some(time)
    }

    /// Peaks from different blocks do not add.  `None` while no mixer runs.
    pub fn total_dsp_time(&self) -> Option<DspTime> {
        if !self.shared.running.load(Ordering::Relaxed) {
            return None;
        }
        let time = self.shared.total_dsp.load();
        (time.block_ns > 0).then_some(time)
    }

    /// No lock on the sample path.
    pub fn plugin_lock_report(&self, path: usize) -> PluginLockReport {
        self.shared.path(path).plugin_lock_report()
    }

    pub fn reset_plugin_lock_counters(&self) {
        for slot in 0..MAX_PATHS {
            self.shared.path(slot).reset_plugin_lock_counters();
        }
    }

    /// Only before a bridge runs.
    pub fn screenshot_path_latency(&self, slot: usize, samples: u32) {
        self.shared.path(slot).screenshot_latency(samples);
    }

    /// Refuses while no bridge runs: the click is played, and the mix
    /// ducked, on the audio thread.  `click` is already at the output rate.
    pub fn arm_calibration(
        &self,
        click: &[f32],
        input_rate: u32,
        input_block: u32,
        output_step: u32,
    ) -> Result<(), String> {
        if !self.shared.running.load(Ordering::Relaxed) {
            return Err(
                "Start the bridge first: the loopback needs the input and output open.".to_owned(),
            );
        }
        self.shared
            .calibration
            .arm(click, input_rate, input_block, output_step)
    }

    pub fn calibration_state(&self) -> CalibrationState {
        self.shared.calibration.state()
    }

    /// No preference was changed.  The mixer ramps the mix back.
    pub fn cancel_calibration(&self) {
        self.shared.calibration.finish();
    }

    /// UI thread, after `Ready`.
    pub fn calibration_capture(&self, left: &mut Vec<f32>, right: &mut Vec<f32>) -> usize {
        self.shared.calibration.copy_capture(left, right)
    }

    pub fn calibration_progress(&self) -> (usize, usize) {
        let calibration = &self.shared.calibration;
        (calibration.captured(), calibration.limit())
    }

    pub fn calibration_capture_start_ns(&self) -> u64 {
        self.shared.calibration.capture_start_ns()
    }

    pub fn calibration_emitted_ns(&self) -> u64 {
        self.shared.calibration.emitted_ns()
    }

    pub fn calibration_capture_end_ns(&self) -> u64 {
        self.shared.calibration.capture_end_ns()
    }

    /// Live, with a short crossfade, including while the bridge runs.
    pub fn set_clock_output(&self, settings: &OutputDeviceSettings) {
        self.shared.clock_output.set(settings);
    }

    /// 0 is the clock output, then the extra outputs.
    pub fn reset_loudness(&self, device: usize) {
        self.shared.reset_loudness(device);
    }

    pub(crate) fn start_match_paths(&self) {
        self.shared.start_match_paths();
    }

    pub(crate) fn stop_match_paths(&self) {
        self.shared.stop_match_paths();
    }

    pub(crate) fn path_match_measurement(&self, slot: usize) -> (f32, u32) {
        self.shared.path_match_measurement(slot)
    }

    pub(crate) fn start_output_learn(&self, device: usize) {
        self.shared.start_output_learn(device);
    }

    pub(crate) fn stop_output_learn(&self, device: usize) {
        self.shared.stop_output_learn(device);
    }

    pub(crate) fn loudness_learn(&self, device: usize) -> Option<(f32, u32)> {
        self.shared.loudness_learn(device)
    }

    /// Live.  Not part of the bridge signature.
    pub fn set_extra_outputs(
        &self,
        settings: &[OutputDeviceSettings],
        plan: &ExtraOutputPlan,
        bridge_buffers: &[(crate::audio::BufferMode, f32)],
    ) {
        self.shared
            .set_extra_outputs(settings, plan, bridge_buffers);
    }

    pub fn poll(&mut self) {
        #[cfg(target_os = "linux")]
        self.retry_quantum_release();
        self.reap_finished_bridge();
        let current_finished = self.reap_current_bridge();

        // A user stop owns the state until teardown confirms.  A late error
        // must not turn `Stopping` into `Error`.
        if self.state == EngineState::Stopping {
            if self.shared.asio_panel.is_open() {
                // `controlPanel` is still on the bridge thread.  This wait is
                // not a driver that refused to release the device.
                self.stopping_since = Some(Instant::now());
                return;
            }
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
                    // Park the live thread first, so the next start sees
                    // teardown pending instead of a second run.
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
                // Some drivers pause callbacks until the panel closes.
                // That wait is not a stalled device.
                if self.shared.asio_panel.is_open() {
                    let now = Instant::now();
                    self.last_frames_at = now;
                    self.last_input_frames_at = now;
                }
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
        // A timed-out bridge keeps the old `Arc`.  Preview still gets its
        // own run, so that bridge cannot write into what the UI shows.
        self.replace_run();
        self.shared.reset();
        self.shared.set_status("Demo bridge · animated signal");
        self.started_at = None;
        self.stopping_since = None;
        self.input_expected = false;
        self.last_input_frames = 0;
        self.last_input_frames_at = Instant::now();
        self.state = EngineState::Preview;
    }

    pub fn plugin_views(&self, path: usize) -> Arc<[PluginView]> {
        self.shared.path(path).plugin_views()
    }

    /// CLAP main-thread callbacks, and path removals whose fade has ended.
    pub fn service_plugins(&self) {
        self.shared.service_paths();
        self.shared.service_plugins();
    }

    /// Captures fresh plugin state.
    pub fn plugin_prefs(&self, path: usize) -> Vec<PluginPref> {
        self.shared.path(path).plugin_prefs()
    }

    /// Does not open any plugin.
    #[cfg(test)]
    pub fn restore_plugins(&self, path: usize, plugins: Vec<PluginPref>) {
        self.shared.path(path).restore_plugins(plugins);
    }

    /// No bridge.  For app tests.
    #[cfg(test)]
    pub(crate) fn publish_path_match_for_test(&self, slot: usize, lufs: f32, blocks: u32) {
        self.shared.paths[slot].publish_match_measurement(lufs, blocks);
    }

    /// For app tests of [`Self::open_asio_driver`].
    #[cfg(test)]
    pub(crate) fn set_asio_driver_for_test(&mut self, side: AsioPanelSide, clsid: Option<u128>) {
        match side {
            AsioPanelSide::Input => self.asio_input_clsid = clsid,
            AsioPanelSide::Output => self.asio_output_clsid = clsid,
        }
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

    /// So `add_path` does not reach through [`Self::shared`].
    pub(crate) fn path_mix(&self, slot: usize) -> PathMix {
        self.shared.path(slot).mix()
    }

    pub fn set_plugin_mix(&self, path: usize, index: usize, mix: f32) {
        self.shared.path(path).set_plugin_mix(index, mix);
    }

    /// A saved effect stays unloaded until its controls are edited.
    pub fn ensure_plugin_loaded(&self, path: usize, index: usize) {
        self.shared.path(path).ensure_plugin_loaded(index);
    }

    pub fn plugin_handle(&self, path: usize, index: usize) -> Option<super::plugins::PluginHandle> {
        self.shared.path(path).plugin_handle(index)
    }

    /// Catches a change the plugin did not report.  The service frames
    /// follow the editor as it goes.
    pub fn sync_plugin_parameters(&self, path: usize, index: usize) {
        self.shared.path(path).sync_plugin_parameters(index);
    }

    /// Marks the saved state for capture when the editor closes.
    pub fn finish_plugin_editor(&self, path: usize, index: usize) {
        let controls = self.shared.path(path);
        controls.sync_plugin_parameters(index);
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

    pub(crate) fn arm_recording(
        &self,
        path: std::path::PathBuf,
        source: crate::audio::record::RecordSource,
    ) -> bool {
        let (rate, channels) = match &source {
            crate::audio::record::RecordSource::Clock => (
                self.shared.output_rate.load(Ordering::Relaxed),
                self.shared.clock_output.channels() as usize,
            ),
            crate::audio::record::RecordSource::ExtraOutput(slot) => {
                if *slot >= MAX_EXTRA_OUTPUTS {
                    return false;
                }
                (
                    self.extra_output_recording_rate(*slot),
                    self.shared.extra_outputs[*slot].channels() as usize,
                )
            }
        };
        if rate == 0 || channels == 0 {
            return false;
        }
        self.shared.recorder.arm(path, rate, channels, source)
    }

    /// Does not join.  The writer kept its own `Arc` at arm.
    pub(crate) fn disarm_recording(&self) {
        self.shared.recorder.disarm();
    }

    /// From the next frame, including mid-take.  A new run inherits it.
    pub(crate) fn set_record_click(&self, with_click: bool) {
        self.shared.recorder.set_with_click(with_click);
    }

    pub(crate) fn recording_status(&self) -> crate::audio::RecordStatus {
        self.shared.recorder.status()
    }

    /// The opened device rate.  With none, the clock rate is used only for
    /// an in-graph duplex extra output, which does not resample.  A missing
    /// generic rate is not guessed.
    fn extra_output_recording_rate(&self, slot: usize) -> u32 {
        let opened = self.shared.extra_output_bus(slot).playback_rate();
        if opened > 0 {
            return opened;
        }
        if self.shared.direct.load(Ordering::Relaxed) {
            self.shared.output_rate.load(Ordering::Relaxed)
        } else {
            0
        }
    }

    /// Older bridges keep the `Arc` they were started with.
    fn replace_run(&mut self) {
        // The old writer finishes on the `Arc` it kept at arm.  Live models
        // do not move: the UI reinstalls them because `run_id` changed.
        // Parked kernels are dropped here, on the UI thread.
        self.disarm_recording();
        self.run_id = self.run_id.wrapping_add(1);
        self.nam_park.flush();
        self.cab_park.flush();
        let previous = Arc::clone(&self.shared);
        let mut run = Shared::with_park(
            self.sample_rate,
            Arc::clone(&self.nam_park),
            Arc::clone(&self.cab_park),
            Arc::clone(&self.slot_park),
        );
        run.adopt_settings_from(&previous);
        self.shared = Arc::new(run);
        drop(previous);
    }

    pub fn start(&mut self, config: StartConfig) {
        self.stop();
        if self.lingering.iter().any(|handle| !handle.is_finished()) {
            // Teardown still owns the device.  A second open would put two
            // streams on it.  The error belongs to the current run.
            self.shared.set_error(
                "The previous bridge is still shutting down and has not released the audio device. Start again once it has finished.",
            );
            self.enter_error();
            return;
        }
        self.reap_finished_bridge();

        self.replace_run();

        self.shared.reset();
        // Before the output opens, so the first cycle is already the
        // requested size.  A missing pw-metadata is not fatal.  If the
        // previous force cannot be read, nothing is written: a later
        // restore must not invent 0.
        #[cfg(target_os = "linux")]
        if config.pipewire_low_latency {
            self.apply_quantum_force();
        }
        // Only Low latency can starve the sink.
        #[cfg(target_os = "linux")]
        {
            self.sink_monitor = if config.pipewire_low_latency {
                SinkMonitor::open(
                    &config.output_id,
                    config.output_rate.unwrap_or(config.sample_rate),
                )
            } else {
                None
            };
        }
        // Plugin load stays on the UI thread.  A device that opens at
        // another rate is reloaded here by `service_plugins`.
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
        // The heartbeat holds the input side to a block only when this
        // start asked for an input.
        self.input_expected = config.input_id.is_some() || config.asio_clsid.is_some();
        self.asio_input_clsid = config.asio_clsid;
        self.asio_output_clsid = config.output_asio_clsid;

        // This bridge's `Arc` is the new run, so its writes cannot reach a
        // later start.
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
                        shared.clear_metronome_beat();
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

    /// Does not wait for the driver.  `poll` joins it, then `Idle`.
    pub fn stop(&mut self) {
        self.disarm_recording();
        self.forget_asio_panel();
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
            // Must not stay ducked while the bridge is stopping.
            self.shared.calibration.finish();
            self.release_quantum();
            self.close_sink_monitor();
            return;
        }

        self.finish_stop();
    }

    fn finish_stop(&mut self) {
        self.forget_asio_panel();
        self.shared.running.store(false, Ordering::Relaxed);
        self.shared.calibration.finish();
        // What the mixer published is stale.
        for path in &self.shared.paths {
            path.store_dsp_time(DspTime::default());
        }
        self.shared.total_dsp.store(DspTime::default());
        self.shared.clock_output.clear_alignment();
        for extra_output in &self.shared.extra_outputs {
            extra_output.clear_alignment();
        }
        self.shared.device_lost.store(false, Ordering::Relaxed);
        // Cleared last.  While a panicked callback's stream still lived,
        // the flag kept it out of the chain.
        self.shared.clear_callback_failure();
        // A published beat would sit on the Live strip until the next run.
        self.shared.clear_metronome_beat();
        self.release_quantum();
        self.close_sink_monitor();
        self.shared.set_status("Bridge stopped");
        if let Ok(mut error) = self.shared.error.lock() {
            *error = None;
        }
        self.started_at = None;
        self.stopping_since = None;
        self.input_expected = false;
        self.state = EngineState::Idle;
    }

    /// The join result is ignored: that `Shared` is no longer the one the
    /// UI reads.
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

    /// The caller chooses `Stopping` or `Error`.
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

    /// A live thread with no device, for the start backstop.
    #[cfg(test)]
    pub(crate) fn park_lingering_for_test(&mut self, handle: JoinHandle<()>) {
        self.lingering.push(handle);
    }

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

    /// Ends the run.  A live bridge would keep the device and block the
    /// next start.
    fn enter_error(&mut self) {
        self.disarm_recording();
        self.forget_asio_panel();
        self.stop.store(true, Ordering::Relaxed);
        self.state = EngineState::Error;
        self.started_at = None;
        self.stopping_since = None;
        self.release_quantum();
        self.close_sink_monitor();
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
                    self.note_quantum_failure(format!("PipeWire graph buffer not set: {error}"));
                }
            },
            Err(error) => {
                self.note_quantum_failure(format!("PipeWire graph buffer not set: {error}"));
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
                        "PipeWire graph buffer {} frames ({:.1} ms)…",
                        state.quantum,
                        state.quantum as f32 / state.rate.max(1) as f32 * 1000.0
                    ));
                } else {
                    self.note_quantum_failure(
                        "PipeWire accepted the graph buffer but did not report it back, and restoring the previous value failed. It will be restored when the bridge stops.".to_owned(),
                    );
                }
            }
            QuantumForce::RolledBack => {
                self.quantum_forced = false;
                self.quantum_retry_at = None;
                self.note_quantum_failure(
                    "PipeWire accepted the graph buffer but did not report it back, so the previous graph buffer was restored.".to_owned(),
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
                        "PipeWire graph buffer was not restored: {error}"
                    ));
                    self.quantum_retry_at = Some(Instant::now() + Duration::from_secs(2));
                }
            }
        }
    }

    pub fn clear_error(&mut self) {
        self.shared.device_lost.store(false, Ordering::Relaxed);
        // Not `clear_callback_failure`.  A panicked run still tearing down
        // must keep its flag, or its streams would drive the chain again.
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
        // Process exit only.  A detached driver thread must not outlive it.
        let deadline = Instant::now() + DROP_STOP_WAIT;
        let mut alive = false;
        for handle in self.lingering.drain(..) {
            while !handle.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            if handle.is_finished() {
                let _ = handle.join();
            } else {
                alive = true;
            }
        }
        if alive {
            // Teardown drops the mixer on that thread and parks models.
            // It must not be the last owner of the parks.  These clones
            // are never dropped.
            std::mem::forget(Arc::clone(&self.nam_park));
            std::mem::forget(Arc::clone(&self.cab_park));
            std::mem::forget(Arc::clone(&self.slot_park));
        } else {
            // Every bridge has joined, so this thread may destroy what they
            // parked.
            self.nam_park.flush();
            self.cab_park.flush();
            self.slot_park.drain();
        }
    }
}

#[cfg(test)]
mod tests;
