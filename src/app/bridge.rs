//! A restart waits until the engine is Idle: the previous run still owns
//! the device.

use std::sync::Arc;

use crate::app::devices::is_wrapper_driver;
use crate::app::issues::StopReason;
use crate::app::LogTone;
use crate::app::RigWerkApp;
use crate::audio::{
    self, AsioPanelSide, DspTime, EngineState, PluginLockReport, PluginView, StartConfig,
};

impl RigWerkApp {
    #[cfg(debug_assertions)]
    pub(crate) fn audio_thread_allocations(&self) -> u64 {
        audio::rt_guard::audio_thread_allocations()
    }

    #[cfg(debug_assertions)]
    pub(crate) fn log_audio_thread_allocations(&mut self) {
        if self.rt_allocations_logged || self.audio_thread_allocations() == 0 {
            return;
        }
        self.rt_allocations_logged = true;
        self.push_log(
            "The audio thread allocated or freed memory (debug build). See Diagnostics, Performance.",
            LogTone::Amber,
        );
    }

    pub(crate) fn is_active(&self) -> bool {
        matches!(
            self.engine.state,
            EngineState::Preview
                | EngineState::Starting
                | EngineState::Running
                | EngineState::Stopping
        )
    }

    pub(crate) fn engine_state(&self) -> EngineState {
        self.engine.state
    }

    /// Empty before the first run.
    pub(crate) fn engine_bridge_status(&self) -> String {
        self.engine.bridge_status()
    }

    pub(crate) fn engine_quantum_note(&self) -> Option<&str> {
        self.engine.quantum_note()
    }

    /// `None` while no mixer runs.
    pub(crate) fn engine_total_dsp_time(&self) -> Option<DspTime> {
        self.engine.total_dsp_time()
    }

    /// `None` while no mixer runs the path.
    pub(crate) fn engine_path_dsp_time(&self, slot: usize) -> Option<DspTime> {
        self.engine.path_dsp_time(slot)
    }

    pub(crate) fn engine_plugin_views(&self, slot: usize) -> Arc<[PluginView]> {
        self.engine.plugin_views(slot)
    }

    pub(crate) fn engine_plugin_lock_report(&self, slot: usize) -> PluginLockReport {
        self.engine.plugin_lock_report(slot)
    }

    /// The bridge thread owns the call.  This only sets the flag.
    pub(crate) fn request_asio_panel(&mut self, side: AsioPanelSide) {
        self.engine.request_asio_panel(side);
    }

    /// A poisoned chain stays poisoned.
    pub(crate) fn reset_plugin_lock_counters(&mut self) {
        self.engine.reset_plugin_lock_counters();
    }

    /// `None` when the side is not ASIO.  `Some(true)` when that driver
    /// is open.
    pub(crate) fn driver_panel(&self, side: AsioPanelSide) -> Option<bool> {
        let clsid = match side {
            AsioPanelSide::Input => self.active_input().asio_clsid,
            AsioPanelSide::Output => self.selected_output().asio_clsid,
        }?;
        Some(self.engine.open_asio_driver(side) == Some(clsid))
    }

    /// While Stopping, a late error from that session is teardown noise.
    pub(crate) fn visible_error(&self) -> Option<&str> {
        if self.engine.state == EngineState::Stopping {
            return None;
        }
        self.snapshot.error.as_deref()
    }

    /// Any change means the bridge must be re-opened.
    pub(crate) fn current_bridge_signature(&self) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        self.active_input().id.hash(&mut hasher);
        self.selected_output().id.hash(&mut hasher);
        self.sample_rate.hash(&mut hasher);
        self.buffer_size.hash(&mut hasher);
        self.low_latency_output.hash(&mut hasher);
        self.wasapi_output_mode.hash(&mut hasher);
        self.output_rate.hash(&mut hasher);
        self.output_buffer.hash(&mut hasher);
        hasher.finish()
    }

    pub(crate) fn start_bridge(&mut self) {
        if self.startup.is_some() {
            self.push_log("Still finding audio devices…", LogTone::Muted);
            return;
        }
        if self.engine.state == EngineState::Stopping {
            self.push_log(
                "The previous bridge is still releasing its audio device.",
                LogTone::Amber,
            );
            return;
        }
        if self.is_preview() && self.engine.teardown_pending() {
            // Preview would replace the state and clear the teardown watchdog
            // while the previous driver still owns its device.
            self.push_log(
                "The previous bridge is still releasing its audio device. Demo starts once it has finished.",
                LogTone::Amber);
            return;
        }
        self.bridge_restart_pending = false;
        self.cancel_loudness_measurement();
        self.bridge_signature = self.current_bridge_signature();
        // A plugin may change a value without reporting it.
        self.sync_plugin_editor();
        self.driver_checked = false;
        self.issue_state.new_run();
        self.issue_state.stopped = None;
        self.apply_gains();
        self.apply_metronome();
        if self.is_preview() {
            self.engine.start_preview();
            self.push_log("Demo device: animated, no hardware opened.", LogTone::Amber);
            return;
        }

        let input = self.active_input().clone();
        let output = self.selected_output().clone();
        #[cfg(target_os = "linux")]
        let pipewire_low_latency = self.low_latency_output && audio::is_pipewire_device(&output);
        #[cfg(not(target_os = "linux"))]
        let pipewire_low_latency = false;
        let (low_latency_request, exclusive_output) = self.output_start_flags();
        let config = StartConfig {
            asio_clsid: input.asio_clsid,
            input_id: if input.asio_clsid.is_some() {
                None
            } else {
                Some(input.id.clone())
            },
            output_id: output.id.clone(),
            output_asio_clsid: output.asio_clsid,
            sample_rate: self.sample_rate,
            buffer_size: self.buffer_size,
            output_rate: (self.output_rate > 0).then_some(self.output_rate),
            output_buffer: self.output_buffer,
            asio_wrapper: input.asio_clsid.is_some() && is_wrapper_driver(&input),
            low_latency_request,
            exclusive_output,
            pipewire_low_latency,
        };
        self.push_log(
            format!(
                "Linking {} input to {} output…",
                input.backend, output.backend
            ),
            LogTone::Blue,
        );
        // Read the planned period before the bridge opens the same device.
        // Doing it afterwards races the stream open and can fail the probe.
        #[cfg(target_os = "linux")]
        self.refresh_quantum_check(true);
        self.engine.start(config);
        // The new run did not inherit the live model.  A rate that
        // arrives later is picked up by `sync_nam_models`.
        self.sync_nam_models();
        self.sync_cab_files();
    }

    pub(crate) fn stop_bridge(&mut self) {
        self.bridge_restart_pending = false;
        self.bridge_signature = 0;
        self.cancel_loudness_measurement();
        self.engine.stop();
        self.issue_state.new_run();
        #[cfg(target_os = "linux")]
        self.refresh_quantum_if_idle();
        if self.engine.state == EngineState::Stopping {
            self.push_log(
                "Stopping the bridge — the devices release when the driver finishes.",
                LogTone::Muted,
            );
        } else {
            self.push_log("Bridge stopped. Devices released.", LogTone::Muted);
        }
    }

    /// Stopping is asynchronous, so the new start waits until the old
    /// driver has released the device.
    pub(crate) fn sync_bridge_config(&mut self) {
        if self.bridge_restart_pending && self.engine.state == EngineState::Idle {
            self.bridge_restart_pending = false;
            self.start_bridge();
            return;
        }
        if self.engine.state == EngineState::Stopping
            || !self.is_active()
            || self.bridge_signature == 0
        {
            return;
        }
        if self.current_bridge_signature() == self.bridge_signature {
            return;
        }
        self.push_log(
            "Bridge settings changed — reconnecting with the new setup.",
            LogTone::Blue,
        );
        self.stop_bridge();
        self.bridge_restart_pending = true;
    }

    /// The alert now, and the issues row once it is dismissed.
    pub(crate) fn alert_stopped_bridge(&mut self, alert: String, reason: StopReason) {
        self.issue_state.new_run();
        self.issue_state.stopped = Some((reason, alert.clone()));
        self.device_alert = Some((alert, Some(reason)));
    }

    /// The mute flag from before any stop this pass.  Recomputing it
    /// after a disconnect would change the frame the device was lost.
    pub(crate) fn publish_engine(&mut self) -> bool {
        // Preview draws its levels here, so a second check would leave
        // those meters moving while the tuner layer holds.
        let output_muted = self.mute || self.tuner.auto_muted(self.page, self.engine.state);
        self.engine
            .set_master(output_muted, self.input_channels, self.drift);
        // The click skips the master mute, but not the tuner's layer.
        self.follow_metronome_audible();
        self.push_paths_to_engine();
        self.push_devices_to_engine();
        self.engine.poll();
        self.snapshot = self.engine.snapshot();
        self.poll_loudness_measurement();
        #[cfg(target_os = "linux")]
        {
            self.refresh_quantum_actual();
            self.refresh_quantum_if_idle();
        }
        self.remember_output_driver();
        self.poll_output_driver_check();
        self.watch_output_driver();
        self.check_driver_settings();
        if let Some(error) = self.visible_error().map(str::to_owned) {
            let message = format!("Bridge error: {error}");
            if !self.log.iter().any(|entry| entry.text == message) {
                self.push_log(message, LogTone::Red);
            }
        }
        // The status line moves on.  Copy the failure into the log once.
        if let Some(note) = self.engine.quantum_note().map(str::to_owned) {
            if !self.log.iter().any(|entry| entry.text == note) {
                self.push_log(note, LogTone::Amber);
            }
        }
        if self.engine.state != EngineState::Stopping
            && self.snapshot.device_lost
            && self.device_alert.is_none()
            && self.bridge_signature != 0
        {
            // A callback failure arrives as `device_lost` too.  The device
            // is fine, so do not ask for a reconnect.
            let reason = self
                .snapshot
                .error
                .clone()
                .unwrap_or_else(|| "A device used by the bridge was disconnected".to_owned());
            self.engine.stop();
            self.engine.clear_error();
            self.bridge_signature = 0;
            self.bridge_restart_pending = false;
            if let Some(failure) = self.snapshot.callback_failure {
                self.push_log(
                    "A real-time audio callback failed - bridge stopped.",
                    LogTone::Red,
                );
                self.alert_stopped_bridge(
                    format!(
                        "{reason}. The bridge was stopped so the audio engine does not keep a failed callback running."
                    ),
                    StopReason::Callback(failure),
                );
            } else {
                self.push_log(
                    "A bridge device was disconnected — bridge stopped.",
                    LogTone::Red,
                );
                self.alert_stopped_bridge(
                    format!(
                        "{reason}. The bridge was stopped so the audio engine does not keep an invalid device open. Reconnect the device, then start the bridge again."
                    ),
                    StopReason::DeviceLost,
                );
            }
        }
        output_muted
    }

    /// `output_muted` is the flag [`Self::publish_engine`] captured
    /// before this pass.
    pub(crate) fn smooth_meters(&mut self, time: f64, output_muted: bool) {
        let time = time as f32;
        let dt = self.meters.tick();
        let smooth = |current: f32, target: f32| {
            let speed = if target > current { 26.0 } else { 6.5 };
            current + (target - current) * (1.0 - (-speed * dt).exp())
        };

        let (in_l, in_r, out_l, out_r) = if self.engine.state == EngineState::Preview {
            let base = (time * 2.7).sin() * 0.16 + 0.32;
            let pick = (time * 1.31).sin().max(0.0).powf(6.0);
            let phrase = 0.22 + 0.34 * (time * 0.43).sin().abs();
            let left = (base + pick * phrase).clamp(0.05, 0.92);
            let right = (left * (0.86 + 0.12 * (time * 0.7).sin())).clamp(0.04, 0.92);
            let out_scale = 10.0f32.powf(self.output_gain / 20.0);
            (left, right, left * out_scale, right * out_scale)
        } else if self.engine.state == EngineState::Running {
            (
                self.snapshot.in_l,
                self.snapshot.in_r,
                self.snapshot.out_l,
                self.snapshot.out_r,
            )
        } else {
            (0.0, 0.0, 0.0, 0.0)
        };

        self.meters.in_l = smooth(self.meters.in_l, in_l);
        self.meters.in_r = smooth(self.meters.in_r, in_r);
        let mute_scale = if output_muted { 0.0 } else { 1.0 };
        self.meters.out_l = smooth(self.meters.out_l, out_l * mute_scale);
        self.meters.out_r = smooth(self.meters.out_r, out_r * mute_scale);
        self.meters.peak_in_l = self.meters.peak_in_l.max(self.meters.in_l * 0.98).max(0.0);
        self.meters.peak_in_r = self.meters.peak_in_r.max(self.meters.in_r * 0.98).max(0.0);
        self.meters.peak_out_l = self
            .meters
            .peak_out_l
            .max(self.meters.out_l * 0.98)
            .max(0.0);
        self.meters.peak_out_r = self
            .meters
            .peak_out_r
            .max(self.meters.out_r * 0.98)
            .max(0.0);
        // After the map and the level.  Preview has no engine reading.
        let live = self.live_path_mask();
        let path_gains = self.path_gains();
        let channel_targets = match self.engine.state {
            EngineState::Running => self.snapshot.out_channels,
            EngineState::Preview => self.clock_output.settled_sum(
                live,
                path_gains,
                self.selected_output().channels.max(1),
                out_l * mute_scale,
                out_r * mute_scale,
            ),
            _ => [0.0; audio::MAX_OUTPUT_CHANNELS],
        };
        for (channel, meter) in self.meters.out_channels.iter_mut().enumerate() {
            let target = channel_targets[channel].abs();
            *meter = smooth(*meter, target);
            let peak = &mut self.meters.peak_out_channels[channel];
            *peak = peak.max(*meter * 0.98).max(0.0);
        }
        let extra_output_targets = match self.engine.state {
            EngineState::Running => self.snapshot.extra_output_channels,
            EngineState::Preview => std::array::from_fn(|index| {
                self.extra_outputs
                    .get(index)
                    .map(|pref| {
                        pref.settings.settled_sum(
                            live,
                            path_gains,
                            2,
                            out_l * mute_scale,
                            out_r * mute_scale,
                        )
                    })
                    .unwrap_or([0.0; audio::MAX_OUTPUT_CHANNELS])
            }),
            _ => [[0.0; audio::MAX_OUTPUT_CHANNELS]; audio::MAX_EXTRA_OUTPUTS],
        };
        for (index, channel_meters) in self.meters.extra_output_channels.iter_mut().enumerate() {
            for (channel, meter) in channel_meters.iter_mut().enumerate() {
                let target = extra_output_targets[index][channel].abs();
                *meter = smooth(*meter, target);
                let peak = &mut self.meters.peak_extra_output_channels[index][channel];
                *peak = peak.max(*meter * 0.98).max(0.0);
            }
        }
        // After the chain, level, mute and solo, before the master.
        // Preview has no chain.
        for (slot, gain) in path_gains.into_iter().enumerate() {
            let (left, right) = match self.engine.state {
                EngineState::Running => self.engine.path_meters(slot),
                EngineState::Preview => (in_l, in_r),
                _ => (0.0, 0.0),
            };
            for (side, target) in [left, right].into_iter().enumerate() {
                let meter = &mut self.meters.path_levels[slot][side];
                *meter = smooth(*meter, target.abs() * gain);
                let peak = &mut self.meters.peak_path_levels[slot][side];
                *peak = peak.max(*meter * 0.98).max(0.0);
            }
        }
        let decay = (-2.3 * dt).exp();
        for peaks in &mut self.meters.peak_path_levels {
            for peak in peaks {
                *peak *= decay;
            }
        }
        for peak in &mut self.meters.peak_out_channels {
            *peak *= decay;
        }
        for peaks in &mut self.meters.peak_extra_output_channels {
            for peak in peaks {
                *peak *= decay;
            }
        }
        self.meters.peak_in_l *= decay;
        self.meters.peak_in_r *= decay;
        self.meters.peak_out_l *= decay;
        self.meters.peak_out_r *= decay;

        if self.meters.scope_in.len() != crate::audio::SCOPE_LEN {
            self.meters.scope_in.resize(crate::audio::SCOPE_LEN, 0.0);
            self.meters.scope_out.resize(crate::audio::SCOPE_LEN, 0.0);
        }
        if self.engine.state == EngineState::Running {
            self.engine
                .copy_scope(&mut self.meters.scope_in, &mut self.meters.scope_out);
        } else {
            self.meters.scope_in.fill(0.0);
            self.meters.scope_out.fill(0.0);
        }
    }

    /// While stopped, a planned buffer stands in so a leftover bridge
    /// buffer is not shown.  A native PipeWire path publishes the rate
    /// and step it asked for first, so that half is `*_read` only once
    /// the graph has published its own.
    pub(crate) fn latency_parts(&self) -> LatencyParts {
        let running = self.engine.state == EngineState::Running;
        let pick = |live: u32, planned: u32| if running && live > 0 { live } else { planned };
        let planned_output_rate = if self.output_rate > 0 {
            self.output_rate
        } else {
            self.sample_rate
        };
        // The PipeWire duplex node has no bridge buffer: input and output
        // run in the same graph cycle.
        let bridge_buffer = if running && self.snapshot.direct {
            0
        } else if running && self.snapshot.target_frames > 0 {
            self.snapshot.target_frames as u32
        } else {
            (self.buffer_size * 2) as u32
        };
        LatencyParts {
            running,
            direct: running && self.snapshot.direct,
            input_block: pick(self.snapshot.input_block_frames, self.buffer_size as u32),
            bridge_buffer,
            output_step: pick(self.snapshot.output_step_frames, self.output_buffer as u32),
            input_rate: pick(self.snapshot.input_rate, self.sample_rate).max(1),
            output_rate: pick(self.snapshot.output_rate, planned_output_rate).max(1),
            input_reported: running && self.snapshot.input_block_frames > 0,
            output_reported: running && self.snapshot.output_step_reported,
            input_rate_reported: running && self.snapshot.input_rate_reported,
            output_rate_reported: running && self.snapshot.output_rate_reported,
        }
    }

    /// Input block, bridge buffer and output step.  AD/DA, the driver,
    /// plugins and alignment are not included.
    pub(crate) fn latency_ms(&self) -> f32 {
        self.latency_parts().bridge_ms()
    }

    /// `None` until both are readings.  The quantum asked for before
    /// the first block is not a step a capture or a NAM block can use.
    pub(crate) fn output_step(&self) -> Option<(u32, f32)> {
        if self.engine.state != EngineState::Running {
            return None;
        }
        if !self.snapshot.output_rate_reported || !self.snapshot.output_step_reported {
            return None;
        }
        let frames = self.snapshot.output_step_frames;
        let rate = self.snapshot.output_rate;
        (frames > 0 && rate > 0).then(|| (frames, frames as f32 / rate as f32 * 1000.0))
    }

    /// Live: not part of the bridge signature.
    pub(crate) fn push_extra_outputs_to_engine(&mut self) {
        use crate::app::settings::{bridge_buffer_from_pref, rematch_windows_virtual_output};
        use crate::audio::{ExtraOutputLink, ExtraOutputPlan, MAX_EXTRA_OUTPUTS};

        rematch_windows_virtual_output(&mut self.extra_outputs, &self.devices.inventory.outputs);

        let count = self.extra_outputs.len().min(MAX_EXTRA_OUTPUTS);
        let mut settings = Vec::with_capacity(count);
        let mut bridge_buffers = Vec::with_capacity(count);
        let mut plan = ExtraOutputPlan {
            count,
            ..ExtraOutputPlan::default()
        };
        for (index, extra_output) in self.extra_outputs.iter().take(count).enumerate() {
            settings.push(extra_output.settings);
            bridge_buffers.push((
                bridge_buffer_from_pref(extra_output.bridge_buffer_mode),
                extra_output.bridge_buffer_ms,
            ));
            let node_name = if extra_output.virtual_output {
                String::new()
            } else {
                #[cfg(target_os = "linux")]
                {
                    audio::linux_node_name(&extra_output.id)
                        .unwrap_or("")
                        .to_owned()
                }
                #[cfg(not(target_os = "linux"))]
                {
                    let _ = &extra_output.id;
                    String::new()
                }
            };
            // PipeWire RigWerk Out has no cpal id (in-graph).  The Windows
            // VB-Cable virtual output keeps the WASAPI render id so the
            // supervisor can open it while it is on.
            let device_id = if extra_output.virtual_output {
                #[cfg(target_os = "windows")]
                {
                    extra_output.id.clone()
                }
                #[cfg(not(target_os = "windows"))]
                {
                    String::new()
                }
            } else {
                extra_output.id.clone()
            };
            plan.slots[index] = ExtraOutputLink {
                device_id,
                node_name,
                virtual_output: extra_output.virtual_output,
                virtual_output_on: extra_output.virtual_output_on,
            };
        }
        self.engine
            .set_extra_outputs(&settings, &plan, &bridge_buffers);
    }

    pub(crate) fn set_low_latency_output(&mut self, on: bool) {
        self.low_latency_output = on;
        self.push_log(
            if on {
                "Low latency on: asks PipeWire for a 64-sample buffer and ALSA for its smallest buffer."
            } else {
                "Low latency off: the clock output uses its buffer size."
            },
            if on { LogTone::Amber } else { LogTone::Muted },
        );
    }

    /// `low_latency_output` stays true only for REAL-style, so an old
    /// save still round-trips.
    pub(crate) fn set_wasapi_output_mode(&mut self, mode: audio::WasapiOutputMode) {
        self.wasapi_output_mode = mode;
        self.low_latency_output = matches!(mode, audio::WasapiOutputMode::RealStyle);
    }

    pub(crate) fn set_sample_rate(&mut self, rate: u32) {
        self.sample_rate = crate::app::settings::sanitize_sample_rate(rate);
    }

    pub(crate) fn set_buffer_size(&mut self, buffer: usize) {
        self.buffer_size = crate::app::settings::sanitize_buffer_size(buffer);
    }

    /// 0 follows the input.
    pub(crate) fn set_output_rate(&mut self, rate: u32) {
        self.output_rate = crate::app::settings::sanitize_output_rate(rate);
    }

    /// 0 is the default buffer here.  Only an old saved session reads
    /// 0 as "follow the input".
    pub(crate) fn set_output_buffer(&mut self, buffer: usize) {
        self.output_buffer = crate::app::settings::sanitize_buffer_size(buffer);
    }

    pub(crate) fn set_drift(&mut self, on: bool) {
        self.drift = on;
    }

    /// The tuner's auto-mute stays a layer on top.
    pub(crate) fn toggle_mute(&mut self) {
        self.mute = !self.mute;
    }

    pub(crate) fn apply_gains(&self) {
        self.engine.set_gains(self.input_gain, self.output_gain);
    }

    pub(crate) fn set_input_gain(&mut self, gain_db: f32) {
        let gain_db = if gain_db.is_nan() { 0.0 } else { gain_db };
        self.input_gain = gain_db.clamp(
            crate::app::settings::INPUT_GAIN_MIN_DB,
            crate::app::settings::INPUT_GAIN_MAX_DB,
        );
        self.apply_gains();
    }

    pub(crate) fn set_master_level(&mut self, level_db: f32) {
        let level_db = if level_db.is_nan() { 0.0 } else { level_db };
        self.output_gain = level_db.clamp(
            crate::app::settings::OUTPUT_GAIN_MIN_DB,
            crate::app::settings::OUTPUT_GAIN_MAX_DB,
        );
        self.apply_gains();
    }

    pub(crate) fn set_input_channel(&mut self, index: usize, on: bool) {
        if let Some(channel) = self.input_channels.get_mut(index) {
            *channel = on;
        }
    }

    /// The master mute leaves it ticking.  The tuner's auto-mute
    /// silences it: tuning needs quiet.
    pub(crate) fn metronome_audible(&self) -> bool {
        self.metronome_running && !self.tuner.auto_muted(self.page, self.engine.state)
    }

    /// The engine gets [`Self::metronome_audible`], so leaving the
    /// tuner restarts the click.
    pub(crate) fn apply_metronome(&mut self) {
        let audible = self.metronome_audible();
        self.metronome_pushed = Some(audible);
        self.engine.set_metronome(
            audible,
            self.metronome_bpm,
            self.metronome_rhythm,
            self.metronome_level_db,
        );
    }

    /// Only when [`Self::metronome_audible`] moved.  The setters and
    /// startup push their own edits.
    pub(crate) fn follow_metronome_audible(&mut self) {
        if self.metronome_pushed != Some(self.metronome_audible()) {
            self.apply_metronome();
        }
    }

    pub(crate) fn set_metronome_running(&mut self, running: bool) {
        self.metronome_running = running;
        self.apply_metronome();
    }

    pub(crate) fn set_metronome_bpm(&mut self, bpm: f32) {
        self.metronome_bpm = audio::clamp_metronome_bpm(bpm);
        self.apply_metronome();
    }

    pub(crate) fn set_metronome_rhythm(&mut self, rhythm: audio::ClickRhythm) {
        self.metronome_rhythm = rhythm;
        self.apply_metronome();
    }

    pub(crate) fn set_metronome_level_db(&mut self, level_db: f32) {
        self.metronome_level_db = audio::clamp_click_level_db(level_db);
        self.apply_metronome();
    }

    pub(crate) fn apply_bridge_buffer(&self) {
        self.engine
            .set_bridge_buffer(self.bridge_buffer_mode, self.bridge_buffer_ms);
    }

    pub(crate) fn set_bridge_buffer_mode(&mut self, mode: audio::BufferMode) {
        self.bridge_buffer_mode = mode;
        self.apply_bridge_buffer();
    }

    pub(crate) fn set_bridge_buffer_ms(&mut self, ms: f32) {
        self.bridge_buffer_ms = crate::app::settings::sanitize_bridge_buffer_ms(ms);
        self.apply_bridge_buffer();
    }

    /// An ASIO clock ignores both.  On Windows the mode enum decides.
    /// On Linux `low_latency_output` is the request and ALSA's smallest
    /// period.  Exclusive stays off away from Windows.
    pub(crate) fn output_start_flags(&self) -> (bool, bool) {
        if self.selected_output().asio_clsid.is_some() {
            return (false, false);
        }
        #[cfg(target_os = "windows")]
        {
            match self.wasapi_output_mode {
                audio::WasapiOutputMode::Shared => (false, false),
                audio::WasapiOutputMode::RealStyle => (true, false),
                audio::WasapiOutputMode::Exclusive => (false, true),
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            (self.low_latency_output, false)
        }
    }
}

/// Same terms as `latency_ms`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LatencyParts {
    pub(crate) running: bool,
    /// Input and output share one PipeWire graph cycle, so there is no ring.
    pub(crate) direct: bool,
    pub(crate) input_block: u32,
    /// 0 on the duplex node.
    pub(crate) bridge_buffer: u32,
    pub(crate) output_step: u32,
    pub(crate) input_rate: u32,
    pub(crate) output_rate: u32,
    /// The size is the driver's.  The rate may still be the one asked
    /// for until [`Self::input_rate_reported`].
    pub(crate) input_reported: bool,
    /// The driver's or the graph's, not the quantum asked for first.
    pub(crate) output_reported: bool,
    /// A reading, not the rate asked for before the stream negotiated.
    pub(crate) input_rate_reported: bool,
    pub(crate) output_rate_reported: bool,
}

impl LatencyParts {
    /// The label names both numbers, so a measured block over a rate
    /// the path only asked for is still shown as planned.
    pub(crate) fn input_read(&self) -> bool {
        self.input_reported && self.input_rate_reported
    }

    /// Same reason as [`Self::input_read`].
    pub(crate) fn output_read(&self) -> bool {
        self.output_reported && self.output_rate_reported
    }

    pub(crate) fn bridge_ms(&self) -> f32 {
        ((self.input_block + self.bridge_buffer) as f32 / self.input_rate.max(1) as f32
            + self.output_step as f32 / self.output_rate.max(1) as f32)
            * 1000.0
    }
}

#[cfg(test)]
mod tests;
