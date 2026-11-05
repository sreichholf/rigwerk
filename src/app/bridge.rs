//! Starting, stopping and re-opening the bridge, and the live numbers the
//! screens show about it.

use std::time::Instant;

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
        self.engine.set_gate(self.gate);
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

    /// RigWerk's own path: input block + cushion + output step. AD/DA, the
    /// driver, plugins, and anything past our playback node are not included.
    /// While stopped, the requested buffer and a planned cushion of twice that
    /// buffer stand in, so a cushion left over from the last run is not shown.
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
        let input_block = pick(self.snapshot.input_block_frames, self.buffer_size as u32) as f32;
        let target = if running && self.snapshot.target_frames > 0 {
            self.snapshot.target_frames
        } else {
            self.buffer_size * 2
        } as f32;
        ((input_block + target) / input_rate + output_step / output_rate) * 1000.0
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
}

#[cfg(test)]
mod tests {
    use crate::app::RigWerkApp;
    use crate::audio::EngineState;

    /// Input block + cushion at the input rate, plus the output step at the
    /// output rate.  Same terms as `latency_ms`, with no extra fudge.
    fn bridge_ms(
        input_block: f32,
        cushion: f32,
        input_rate: f32,
        output_step: f32,
        output_rate: f32,
    ) -> f32 {
        ((input_block + cushion) / input_rate + output_step / output_rate) * 1000.0
    }

    /// An app with chosen buffer settings and an empty snapshot.  No device
    /// is opened; the engine stays Idle until a test sets its state.
    fn latency_app() -> RigWerkApp {
        let mut app = crate::app::blank_app();
        app.use_asio_input = false;
        app.output_gain = 0.0;
        app.low_latency_output = false;
        app.output_rate = 48_000;
        app.session_name = String::new();
        app
    }

    #[test]
    fn running_latency_uses_the_delivered_input_block() {
        let mut app = latency_app();
        app.buffer_size = 128;
        app.sample_rate = 96_000;
        app.output_rate = 44_100;
        app.output_buffer = 64;
        app.engine.state = EngineState::Running;
        app.snapshot.input_block_frames = 480;
        app.snapshot.input_rate = 48_000;
        app.snapshot.output_rate = 48_000;
        app.snapshot.output_step_frames = 128;
        app.snapshot.target_frames = 960;

        let ms = app.latency_ms();
        let delivered = bridge_ms(480.0, 960.0, 48_000.0, 128.0, 48_000.0);
        let requested = bridge_ms(128.0, 960.0, 48_000.0, 128.0, 48_000.0);
        assert!(
            (ms - delivered).abs() < 0.01,
            "running latency {ms} did not use the 480-frame block ({delivered})"
        );
        assert!(
            (ms - requested).abs() > 1.0,
            "running latency {ms} still used the requested 128-frame block"
        );
        assert!(
            (ms - (delivered + 1.5)).abs() > 1.0,
            "running latency {ms} still adds the 1.5 ms fudge"
        );
    }

    #[test]
    fn running_latency_falls_back_until_the_device_reports() {
        let mut app = latency_app();
        app.buffer_size = 128;
        app.output_buffer = 64;
        app.engine.state = EngineState::Running;
        app.snapshot.input_block_frames = 0;
        app.snapshot.target_frames = 0;
        app.snapshot.input_rate = 48_000;
        app.snapshot.output_rate = 48_000;
        app.snapshot.output_step_frames = 64;

        let ms = app.latency_ms();
        let planned = bridge_ms(128.0, 256.0, 48_000.0, 64.0, 48_000.0);
        assert!(
            (ms - planned).abs() < 0.01,
            "running latency {ms} did not fall back to the requested block and cushion ({planned})"
        );
    }

    #[test]
    fn stopped_latency_ignores_the_previous_cushion() {
        let mut app = latency_app();
        app.buffer_size = 128;
        app.output_buffer = 256;
        app.sample_rate = 48_000;
        app.output_rate = 0;
        app.engine.state = EngineState::Idle;
        app.snapshot.target_frames = 4_096;
        app.snapshot.input_block_frames = 480;
        app.snapshot.input_rate = 96_000;
        app.snapshot.output_rate = 44_100;
        app.snapshot.output_step_frames = 512;

        let ms = app.latency_ms();
        let planned = bridge_ms(128.0, 256.0, 48_000.0, 256.0, 48_000.0);
        let leftover = bridge_ms(128.0, 4_096.0, 48_000.0, 256.0, 48_000.0);
        assert!(
            (ms - planned).abs() < 0.01,
            "stopped latency {ms} did not use the planned cushion ({planned})"
        );
        assert!(
            (ms - leftover).abs() > 1.0,
            "stopped latency {ms} kept the previous session's cushion"
        );
    }

    #[test]
    fn heard_latency_adds_the_pipewire_path_delay() {
        let mut app = latency_app();
        app.buffer_size = 128;
        app.sample_rate = 48_000;
        app.output_buffer = 64;
        app.engine.state = EngineState::Running;
        app.snapshot.input_block_frames = 64;
        app.snapshot.input_rate = 48_000;
        app.snapshot.output_rate = 48_000;
        app.snapshot.output_step_frames = 64;
        app.snapshot.target_frames = 128;
        app.snapshot.path_delay_ms = Some(147.5);

        let us = app.latency_ms();
        assert!((us - bridge_ms(64.0, 128.0, 48_000.0, 64.0, 48_000.0)).abs() < 0.01);
        assert!((app.path_delay_ms().unwrap() - 147.5).abs() < 0.01);
        assert!((app.heard_latency_ms().unwrap() - (us + 147.5)).abs() < 0.01);
        assert_eq!(app.latency_caption(), format!("{us:.1} · 147.5 ms"));
    }

    #[test]
    fn path_delay_is_ignored_while_stopped() {
        let mut app = latency_app();
        app.engine.state = EngineState::Idle;
        app.snapshot.path_delay_ms = Some(147.5);
        assert_eq!(app.path_delay_ms(), None);
        assert_eq!(app.heard_latency_ms(), None);
        assert!(app.latency_caption().ends_with(" ms"));
        assert!(!app.latency_caption().contains('·'));
    }
}
