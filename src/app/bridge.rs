//! Starting, stopping and re-opening the bridge, and the live numbers the
//! screens show about it.

use std::time::Instant;

use crate::app::devices::is_wrapper_driver;
use crate::app::RigWerkApp;
use crate::audio::{self, EngineState, StartConfig};
use crate::ui::theme::{AMBER, BLUE, MUTED, RED};

impl RigWerkApp {
    pub(crate) fn is_active(&self) -> bool {
        matches!(
            self.engine.state,
            EngineState::Preview
                | EngineState::Starting
                | EngineState::Running
                | EngineState::Stopping
        )
    }

    /// The error worth showing.  While a stop is still releasing the device, a
    /// late error written by that same session is teardown noise, not a bridge
    /// failure; once a newer session starts it owns the snapshot again.
    pub(crate) fn visible_error(&self) -> Option<&str> {
        if self.engine.state == EngineState::Stopping {
            return None;
        }
        self.snapshot.error.as_deref()
    }

    /// Identifies the exact routing the running bridge was opened with.  Any
    /// change to this value means the bridge must be re-opened to take effect.
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
            self.push_log("Still finding audio devices…", MUTED);
            return;
        }
        if self.engine.state == EngineState::Stopping {
            self.push_log(
                "The previous bridge is still releasing its audio device.",
                AMBER,
            );
            return;
        }
        if self.is_preview() && self.engine.teardown_pending() {
            // Preview would replace the state and clear the teardown watchdog
            // while the previous driver still owns its device.
            self.push_log(
                "The previous bridge is still releasing its audio device. Preview starts once it has finished.",
                AMBER,
            );
            return;
        }
        self.bridge_restart_pending = false;
        self.bridge_signature = self.current_bridge_signature();
        // The open editor's newest edit may be younger than the last 250 ms
        // poll, and the new session is rebuilt from the saved inserts (their
        // preferences and state blobs), so read the editor in full here.
        self.sync_plugin_editor();
        self.driver_checked = false;
        self.issue_state.new_session();
        self.issue_state.device_lost = None;
        self.engine.set_gains(self.input_gain, self.output_gain);
        if self.is_preview() {
            self.engine.start_preview();
            self.push_log(
                "Preview bridge active — animated, no hardware opened.",
                AMBER,
            );
            return;
        }

        let input = self.active_input().clone();
        let output = self.selected_output().clone();
        #[cfg(target_os = "linux")]
        let pipewire_low_latency = self.low_latency_output && audio::is_pipewire_device(&output);
        #[cfg(not(target_os = "linux"))]
        let pipewire_low_latency = false;
        let clock_host = output.asio_clsid.is_none();
        #[cfg(target_os = "windows")]
        let (real_style_output, exclusive_output) = if clock_host {
            match self.wasapi_output_mode {
                audio::WasapiOutputMode::Shared => (false, false),
                audio::WasapiOutputMode::RealStyle => (true, false),
                audio::WasapiOutputMode::Exclusive => (false, true),
            }
        } else {
            (false, false)
        };
        #[cfg(not(target_os = "windows"))]
        let (real_style_output, exclusive_output) = (self.low_latency_output && clock_host, false);
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
            real_style_output,
            exclusive_output,
            pipewire_low_latency,
        };
        self.push_log(
            format!(
                "Linking {} input to {} output…",
                input.backend, output.backend
            ),
            BLUE,
        );
        // Read the planned period before the bridge opens the same device.
        // Doing it afterwards races the stream open and can fail the probe.
        #[cfg(target_os = "linux")]
        self.refresh_quantum_check(true);
        self.engine.start(config);
    }

    pub(crate) fn stop_bridge(&mut self) {
        self.bridge_restart_pending = false;
        self.bridge_signature = 0;
        self.engine.stop();
        self.issue_state.new_session();
        #[cfg(target_os = "linux")]
        self.refresh_quantum_if_idle();
        if self.engine.state == EngineState::Stopping {
            self.push_log(
                "Stopping the bridge — the devices release when the driver finishes.",
                MUTED,
            );
        } else {
            self.push_log("Bridge stopped. Devices released.", MUTED);
        }
    }

    /// Re-opens the bridge as soon as the device, rate or buffer selection
    /// changes, instead of waiting for the person to stop and start it.
    /// Stopping is asynchronous, so the new start waits until the old driver
    /// has actually released the device.
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
            BLUE,
        );
        self.stop_bridge();
        self.bridge_restart_pending = true;
    }

    /// The bridge stopped because a device went away: the alert window now,
    /// and the Live Issues row once that is dismissed.
    pub(crate) fn alert_lost_device(&mut self, alert: String) {
        self.issue_state.new_session();
        self.issue_state.device_lost = Some(alert.clone());
        self.device_alert = Some(alert);
    }

    pub(crate) fn meters(&mut self, ctx: &egui::Context) {
        self.engine
            .set_master(self.mute, self.monitor, self.input_channels, self.drift);
        self.push_paths_to_engine();
        self.push_devices_to_engine();
        self.engine.poll();
        self.snapshot = self.engine.snapshot();
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
                self.push_log(message, RED);
            }
        }
        // A quantum failure is stored beside the live status line, which moves
        // on to "Opening…" or "Bridge stopped". Copy it into the log once so
        // it survives that.
        if let Some(note) = self.engine.quantum_note().map(str::to_owned) {
            if !self.log.iter().any(|entry| entry.text == note) {
                self.push_log(note, AMBER);
            }
        }
        if self.engine.state != EngineState::Stopping
            && self.snapshot.device_lost
            && self.device_alert.is_none()
            && self.bridge_signature != 0
        {
            let reason = self
                .snapshot
                .error
                .clone()
                .unwrap_or_else(|| "A device used by the bridge was disconnected".to_owned());
            self.engine.stop();
            self.engine.clear_error();
            self.bridge_signature = 0;
            self.bridge_restart_pending = false;
            self.push_log("A bridge device was disconnected — bridge stopped.", RED);
            self.alert_lost_device(format!(
                "{reason}. The bridge was stopped so the audio engine does not keep an invalid device open. Reconnect the device, then start the bridge again."
            ));
        }
        let time = ctx.input(|input| input.time) as f32;
        let dt = self
            .last_meter_tick
            .elapsed()
            .as_secs_f32()
            .clamp(0.001, 0.1);
        self.last_meter_tick = Instant::now();
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

        self.meter_in_l = smooth(self.meter_in_l, in_l);
        self.meter_in_r = smooth(self.meter_in_r, in_r);
        let mute_scale = if self.mute { 0.0 } else { 1.0 };
        self.meter_out_l = smooth(self.meter_out_l, out_l * mute_scale);
        self.meter_out_r = smooth(self.meter_out_r, out_r * mute_scale);
        self.peak_in_l = self.peak_in_l.max(self.meter_in_l * 0.98).max(0.0);
        self.peak_in_r = self.peak_in_r.max(self.meter_in_r * 0.98).max(0.0);
        self.peak_out_l = self.peak_out_l.max(self.meter_out_l * 0.98).max(0.0);
        self.peak_out_r = self.peak_out_r.max(self.meter_out_r * 0.98).max(0.0);
        // Per clock-device channel, after its map and level. Preview has no
        // engine reading, so the stand-in L/R goes through the same map.
        let live = self.live_path_mask();
        let path_gains = self.path_gains();
        let channel_targets = match self.engine.state {
            EngineState::Running => self.snapshot.out_channels,
            EngineState::Preview => self.clock_device.settled_sum(
                live,
                path_gains,
                self.selected_output().channels.max(1),
                out_l * mute_scale,
                out_r * mute_scale,
            ),
            _ => [0.0; audio::MAX_OUTPUT_CHANNELS],
        };
        for (channel, meter) in self.meter_out_channels.iter_mut().enumerate() {
            let target = channel_targets[channel].abs();
            *meter = smooth(*meter, target);
            let peak = &mut self.peak_out_channels[channel];
            *peak = peak.max(*meter * 0.98).max(0.0);
        }
        // Followers / RigWerk Out: same smoothing as the clock device.
        let follower_targets = match self.engine.state {
            EngineState::Running => self.snapshot.follower_channels,
            EngineState::Preview => std::array::from_fn(|index| {
                self.followers
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
            _ => [[0.0; audio::MAX_OUTPUT_CHANNELS]; audio::MAX_FOLLOWERS],
        };
        for (index, meters) in self.meter_follower_channels.iter_mut().enumerate() {
            for (channel, meter) in meters.iter_mut().enumerate() {
                let target = follower_targets[index][channel].abs();
                *meter = smooth(*meter, target);
                let peak = &mut self.peak_follower_channels[index][channel];
                *peak = peak.max(*meter * 0.98).max(0.0);
            }
        }
        // Path meters: the engine's peak after the path's chain, times its
        // level, mute and solo (what the path adds to the sum), before the
        // master.  Preview has no chain, so the stand-in input goes through.
        for (slot, gain) in path_gains.into_iter().enumerate() {
            let (left, right) = match self.engine.state {
                EngineState::Running => self.engine.path_meters(slot),
                EngineState::Preview => (in_l, in_r),
                _ => (0.0, 0.0),
            };
            for (side, target) in [left, right].into_iter().enumerate() {
                let meter = &mut self.meter_paths[slot][side];
                *meter = smooth(*meter, target.abs() * gain);
                let peak = &mut self.peak_paths[slot][side];
                *peak = peak.max(*meter * 0.98).max(0.0);
            }
        }
        let decay = (-2.3 * dt).exp();
        for peaks in &mut self.peak_paths {
            for peak in peaks {
                *peak *= decay;
            }
        }
        for peak in &mut self.peak_out_channels {
            *peak *= decay;
        }
        for peaks in &mut self.peak_follower_channels {
            for peak in peaks {
                *peak *= decay;
            }
        }
        self.peak_in_l *= decay;
        self.peak_in_r *= decay;
        self.peak_out_l *= decay;
        self.peak_out_r *= decay;

        if self.scope_in.len() != crate::audio::SCOPE_LEN {
            self.scope_in.resize(crate::audio::SCOPE_LEN, 0.0);
            self.scope_out.resize(crate::audio::SCOPE_LEN, 0.0);
        }
        if self.engine.state == EngineState::Running {
            self.engine
                .copy_scope(&mut self.scope_in, &mut self.scope_out);
        } else {
            self.scope_in.fill(0.0);
            self.scope_out.fill(0.0);
        }
    }

    /// The parts of RigWerk's own path, as `latency_ms` and the latency
    /// budget read them.  While stopped, the requested buffer and a planned
    /// cushion of twice that buffer stand in, so a cushion left over from the
    /// last run is not shown.
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
        let cushion = if running && self.snapshot.direct {
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
            cushion,
            output_step: pick(self.snapshot.output_step_frames, self.output_buffer as u32),
            input_rate: pick(self.snapshot.input_rate, self.sample_rate).max(1),
            output_rate: pick(self.snapshot.output_rate, planned_output_rate).max(1),
            input_reported: running && self.snapshot.input_block_frames > 0,
            output_reported: running && self.snapshot.output_step_frames > 0,
        }
    }

    /// RigWerk's own path: input block + cushion + output step.  AD/DA, the
    /// driver, plugins, alignment trims, and anything past our playback node
    /// are not included; see `app::budget` for the full reading.
    pub(crate) fn latency_ms(&self) -> f32 {
        self.latency_parts().bridge_ms()
    }

    /// Delay past our playback node (graph filters + device), when PipeWire
    /// has reported it. Bluetooth A2DP often shows ~100–200 ms here.
    pub(crate) fn path_delay_ms(&self) -> Option<f32> {
        if self.engine.state != EngineState::Running {
            return None;
        }
        self.snapshot.path_delay_ms
    }

    /// `latency_ms` plus path delay when known — a better "what you hear" guess
    /// than the bridge alone. Still not a measured ear-to-mic round trip.
    pub(crate) fn heard_latency_ms(&self) -> Option<f32> {
        Some(self.latency_ms() + self.path_delay_ms()?)
    }

    /// Short readout for tiles: bridge alone, or `us · after` when the path is known.
    pub(crate) fn latency_caption(&self) -> String {
        match self.path_delay_ms() {
            Some(after) => format!("{:.1} · {:.1} ms", self.latency_ms(), after),
            None => format!("{:.1} ms", self.latency_ms()),
        }
    }

    /// The step the running output really got, in frames and milliseconds.
    pub(crate) fn output_step(&self) -> Option<(u32, f32)> {
        if self.engine.state != EngineState::Running {
            return None;
        }
        let frames = self.snapshot.output_step_frames;
        let rate = self.snapshot.output_rate;
        (frames > 0 && rate > 0).then(|| (frames, frames as f32 / rate as f32 * 1000.0))
    }

    /// Pushes follower settings, cushions and the link / open plan into the
    /// engine.  Live: not part of the bridge signature.
    pub(crate) fn push_followers_to_engine(&mut self) {
        use crate::app::settings::{cushion_from_pref, rematch_windows_virtual_out};
        use crate::audio::{FollowerLink, FollowerPlan, MAX_FOLLOWERS};

        rematch_windows_virtual_out(&mut self.followers, &self.inventory.outputs);

        let count = self.followers.len().min(MAX_FOLLOWERS);
        let mut settings = Vec::with_capacity(count);
        let mut cushions = Vec::with_capacity(count);
        let mut plan = FollowerPlan {
            count,
            ..FollowerPlan::default()
        };
        for (index, follower) in self.followers.iter().take(count).enumerate() {
            settings.push(follower.settings);
            cushions.push((
                cushion_from_pref(follower.cushion_mode),
                follower.cushion_ms,
            ));
            let node_name = if follower.virtual_out {
                String::new()
            } else {
                #[cfg(target_os = "linux")]
                {
                    audio::linux_node_name(&follower.id)
                        .unwrap_or("")
                        .to_owned()
                }
                #[cfg(not(target_os = "linux"))]
                {
                    let _ = &follower.id;
                    String::new()
                }
            };
            // PipeWire RigWerk Out has no cpal id (in-graph).  Windows
            // VB-Cable publish keeps the WASAPI render id so the supervisor
            // can open it while publish is on.
            let device_id = if follower.virtual_out {
                #[cfg(target_os = "windows")]
                {
                    follower.id.clone()
                }
                #[cfg(not(target_os = "windows"))]
                {
                    String::new()
                }
            } else {
                follower.id.clone()
            };
            plan.slots[index] = FollowerLink {
                device_id,
                node_name,
                virtual_out: follower.virtual_out,
                publish: follower.publish,
            };
        }
        self.engine.set_followers(&settings, &plan, &cushions);
    }

    /// How the output runs right now, or how it will try to run.
    pub(crate) fn output_mode_text(&self) -> String {
        if self.engine.state == EngineState::Running && !self.snapshot.output_mode.is_empty() {
            return self.snapshot.output_mode.clone();
        }
        if self.selected_output().asio_clsid.is_some() {
            "ASIO output · driver buffer".to_owned()
        } else if cfg!(target_os = "windows") {
            match self.wasapi_output_mode {
                audio::WasapiOutputMode::Exclusive => {
                    "Exclusive: driver's smallest exclusive period, this device only (shared output buffer if refused)"
                        .to_owned()
                }
                audio::WasapiOutputMode::RealStyle => {
                    "REAL-style: the driver's smallest shared step, one step queued (normal output if refused)"
                        .to_owned()
                }
                audio::WasapiOutputMode::Shared => {
                    format!("Shared output · {} smp buffer", self.output_buffer)
                }
            }
        } else if self.low_latency_output {
            format!(
                "Low latency: PipeWire {}-frame quantum, ALSA's smallest usable period (normal output buffer if refused)",
                audio::PIPEWIRE_LOW_QUANTUM
            )
        } else {
            format!("Standard output · {} smp buffer", self.output_buffer)
        }
    }

    /// Turns the Low latency output on or off, and logs it.
    pub(crate) fn set_low_latency_output(&mut self, on: bool) {
        self.low_latency_output = on;
        self.push_log(
            if on {
                "Low-latency output on: PipeWire low quantum + smallest ALSA period when allowed."
            } else {
                "Low-latency output off: normal output buffer, PipeWire quantum released."
            },
            if on { AMBER } else { MUTED },
        );
    }

    /// Apply a Windows WASAPI clock mode and keep the legacy bool aligned
    /// (`true` only for REAL-style, matching old saves).
    pub(crate) fn set_wasapi_output_mode(&mut self, mode: audio::WasapiOutputMode) {
        self.wasapi_output_mode = mode;
        self.low_latency_output = matches!(mode, audio::WasapiOutputMode::RealStyle);
    }
}

/// The bridge's own latency, in the terms `latency_ms` has always used.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LatencyParts {
    /// Whether a bridge runs, for the "planned" labels.
    pub(crate) running: bool,
    /// Whether input and output share one PipeWire graph cycle (no ring).
    pub(crate) direct: bool,
    /// The block the input really delivers, frames.
    pub(crate) input_block: u32,
    /// The ring's fill target, frames (0 on the duplex node).
    pub(crate) cushion: u32,
    /// The step the output really runs at, frames.
    pub(crate) output_step: u32,
    pub(crate) input_rate: u32,
    pub(crate) output_rate: u32,
    /// The input block is the driver's, not the size asked for while stopped.
    pub(crate) input_reported: bool,
    /// The output step is the driver's, not the size asked for while stopped.
    pub(crate) output_reported: bool,
}

impl LatencyParts {
    /// Input block + cushion at the input rate, plus the output step at the
    /// output rate.
    pub(crate) fn bridge_ms(&self) -> f32 {
        ((self.input_block + self.cushion) as f32 / self.input_rate.max(1) as f32
            + self.output_step as f32 / self.output_rate.max(1) as f32)
            * 1000.0
    }
}

#[cfg(test)]
mod tests;
