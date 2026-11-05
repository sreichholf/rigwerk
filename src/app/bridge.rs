//! Starting, stopping and re-opening the bridge, and the live numbers the
//! screens show about it.

use std::time::Instant;

use eframe::egui;
use egui::Color32;

use crate::app::devices::is_wrapper_driver;
use crate::app::RigWerkApp;
use crate::audio::EqSlot;
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
        self.output_rate.hash(&mut hasher);
        self.output_buffer.hash(&mut hasher);
        hasher.finish()
    }

    pub(crate) fn start_bridge(&mut self) {
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
        self.driver_checked = false;
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
            real_style_output: self.low_latency_output && output.asio_clsid.is_none(),
            pipewire_low_latency,
        };
        self.push_log(
            format!(
                "Linking {} input to {} output…",
                input.backend, output.backend
            ),
            BLUE,
        );
        self.engine.start(config);
        #[cfg(target_os = "linux")]
        self.refresh_quantum_check(true);
    }

    pub(crate) fn stop_bridge(&mut self) {
        self.bridge_restart_pending = false;
        self.bridge_signature = 0;
        self.engine.stop();
        #[cfg(target_os = "linux")]
        self.refresh_quantum_check(false);
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

    pub(crate) fn meters(&mut self, ctx: &egui::Context) {
        self.engine.set_mix(
            self.mute,
            self.monitor,
            self.mono,
            self.input_channels,
            self.output_channels,
            self.drift,
        );
        self.engine.set_routes(self.routes);
        for slot in EqSlot::ALL {
            self.engine.set_eq(slot, self.eqs[slot.index()]);
        }
        self.engine.poll();
        self.snapshot = self.engine.snapshot();
        #[cfg(target_os = "linux")]
        self.refresh_quantum_actual();
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
            self.device_alert = Some(format!(
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
        let decay = (-2.3 * dt).exp();
        self.peak_in_l *= decay;
        self.peak_in_r *= decay;
        self.peak_out_l *= decay;
        self.peak_out_r *= decay;
    }

    /// Input block + ring cushion (both at the input's real rate) + the output
    /// step (at the output's real rate).  Before the bridge runs, the chosen
    /// settings stand in for what the devices will report.
    pub(crate) fn latency_ms(&self) -> f32 {
        let running = self.engine.state == EngineState::Running;
        let pick = |live: u32, planned: u32| if running && live > 0 { live } else { planned };
        let input_rate = pick(self.snapshot.input_rate, self.sample_rate).max(1) as f32;
        let planned_output_rate = if self.output_rate > 0 {
            self.output_rate
        } else {
            self.sample_rate
        };
        let output_rate = pick(self.snapshot.output_rate, planned_output_rate).max(1) as f32;
        let output_step = pick(self.snapshot.output_step_frames, self.output_buffer as u32) as f32;
        let target = if self.snapshot.target_frames > 0 {
            self.snapshot.target_frames
        } else {
            self.buffer_size * 2
        } as f32;
        ((self.buffer_size as f32 + target) / input_rate + output_step / output_rate) * 1000.0 + 1.5
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

    /// How the output runs right now, or how it will try to run.
    pub(crate) fn output_mode_text(&self) -> String {
        if self.engine.state == EngineState::Running && !self.snapshot.output_mode.is_empty() {
            return self.snapshot.output_mode.clone();
        }
        if self.selected_output().asio_clsid.is_some() {
            "ASIO output · driver buffer".to_owned()
        } else if self.low_latency_output {
            if cfg!(target_os = "windows") {
                "REAL-style: the driver's smallest step, one step queued (normal output if refused)"
                    .to_owned()
            } else {
                format!(
                    "Low latency: PipeWire {}-frame quantum, ALSA's smallest usable period (normal output buffer if refused)",
                    audio::PIPEWIRE_LOW_QUANTUM
                )
            }
        } else {
            format!("Standard output · {} smp buffer", self.output_buffer)
        }
    }

    pub(crate) fn status_message(&self) -> (String, Color32) {
        if self.engine.state == EngineState::Stopping {
            return (
                "Stopping the bridge — waiting for the audio device to release.".to_owned(),
                MUTED,
            );
        }
        if let Some(error) = self.visible_error() {
            return (format!("Bridge error: {error}"), RED);
        }
        self.log
            .last()
            .map(|entry| (entry.text.clone(), entry.tone))
            .unwrap_or_else(|| ("Bridge ready.".to_owned(), MUTED))
    }
}
