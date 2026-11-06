//! Saving, loading and deleting patches (saved device pairs).

use crate::app::devices::{
    device_restore_note, restore_device, restore_input, DeviceRestore, InputGroup,
};
use crate::app::settings::{cushion_from_pref, cushion_to_pref, sanitize_preset, Preset};
use crate::app::RigWerkApp;
use crate::audio::EqSlot;
use crate::ui::theme::{AMBER, BLUE, MUTED, RED, TEAL};

impl RigWerkApp {
    pub(crate) fn save_preset(&mut self) {
        let input = self.active_input().clone();
        let output = self.selected_output().clone();
        let preset = Preset {
            name: self.session_name.trim().to_owned(),
            note: format!("{} → {}", input.backend, output.backend),
            input: input.name,
            input_id: input.id,
            input_backend: input.backend,
            output: output.name,
            output_id: output.id,
            output_backend: output.backend,
            rate: self.sample_rate,
            buffer: self.buffer_size,
            output_rate: self.output_rate,
            output_buffer: self.output_buffer,
            eq: Some(self.eqs[EqSlot::Pre.index()]),
            eq_post: Some(self.eqs[EqSlot::Post.index()]),
            gate: Some(self.gate),
            input_gain: self.input_gain,
            output_gain: self.output_gain,
            input_channels: self.input_channels,
            output_channels: self.output_channels,
            mono: self.mono,
            mute: self.mute,
            monitor: self.monitor,
            drift: self.drift,
            low_latency_output: self.low_latency_output,
            routes: self.routes,
            cushion_mode: cushion_to_pref(self.cushion_mode),
            cushion_ms: self.cushion_ms,
            plugins: self.engine.plugin_prefs(),
            clock_device: self.clock_device,
            followers: self.followers.clone(),
        };
        if preset.name.is_empty() {
            self.push_log("Give the patch a name before saving.", AMBER);
            return;
        }
        self.pending_delete = None;
        if let Some(index) = self.preset_named(&preset.name) {
            let name = preset.name.clone();
            self.sessions[index] = preset;
            self.selected_session = Some(index);
            self.push_log(format!("Updated patch “{name}”."), TEAL);
            return;
        }
        let name = preset.name.clone();
        self.sessions.insert(0, preset);
        self.selected_session = Some(0);
        self.push_log(format!("Saved patch “{name}”."), TEAL);
    }

    /// Patches are matched by name, ignoring case and surrounding spaces.
    pub(crate) fn preset_named(&self, name: &str) -> Option<usize> {
        let name = name.trim();
        self.sessions
            .iter()
            .position(|preset| preset.name.trim().eq_ignore_ascii_case(name))
    }

    pub(crate) fn delete_preset(&mut self, index: usize) {
        self.pending_delete = None;
        if index >= self.sessions.len() {
            return;
        }
        let removed = self.sessions.remove(index);
        self.selected_session = match self.selected_session {
            Some(current) if current == index => None,
            Some(current) if current > index => Some(current - 1),
            other => other,
        };
        self.push_log(format!("Deleted patch “{}”.", removed.name), MUTED);
    }

    pub(crate) fn apply_preset(&mut self, index: usize) {
        let Some(stored) = self.sessions.get_mut(index) else {
            return;
        };
        // Same check as loading preferences: bad gains, rates and buffers
        // are replaced here, and the stored patch keeps the safe values.
        sanitize_preset(stored);
        let preset = stored.clone();
        self.session_name = preset.name.clone();
        self.sample_rate = preset.rate;
        self.buffer_size = preset.buffer;
        self.output_rate = preset.output_rate;
        self.output_buffer = if preset.output_buffer == 0 {
            preset.buffer
        } else {
            preset.output_buffer
        };
        self.input_gain = preset.input_gain;
        self.output_gain = preset.output_gain;
        self.input_channels = preset.input_channels;
        self.output_channels = preset.output_channels;
        self.mono = preset.mono;
        self.mute = preset.mute;
        self.monitor = preset.monitor;
        self.drift = preset.drift;
        self.low_latency_output = preset.low_latency_output;
        self.routes = preset.routes;
        self.cushion_mode = cushion_from_pref(preset.cushion_mode);
        self.cushion_ms = preset.cushion_ms;
        if let Some(eq) = preset.eq {
            self.eqs[EqSlot::Pre.index()] = eq.sanitized();
        }
        if let Some(eq) = preset.eq_post {
            self.eqs[EqSlot::Post.index()] = eq.sanitized();
        }
        if let Some(gate) = preset.gate {
            self.gate = gate.sanitized();
        }
        self.clock_device = preset.clock_device;
        self.followers = preset.followers.clone();
        self.engine.set_gains(self.input_gain, self.output_gain);
        self.engine.set_mix(
            self.mute,
            self.monitor,
            self.mono,
            self.input_channels,
            self.output_channels,
            self.drift,
        );
        self.engine.set_routes(self.routes);
        self.engine.set_cushion(self.cushion_mode, self.cushion_ms);
        for slot in EqSlot::ALL {
            self.engine.set_eq(slot, self.eqs[slot.index()]);
        }
        self.engine.set_gate(self.gate);
        self.engine.restore_plugins(preset.plugins.clone());
        self.selected_session = Some(index);
        self.pending_delete = None;
        let mut device_missing = false;
        let input_label = if preset.input.is_empty() {
            preset.input_id.as_str()
        } else {
            preset.input.as_str()
        };
        match restore_input(
            &self.inventory.asio,
            &self.inventory.inputs,
            &preset.input_id,
            &preset.input_backend,
            &preset.input,
        ) {
            Some(found) => {
                if let Some(device_index) = found.restore.index() {
                    match found.group {
                        InputGroup::LowLatency => {
                            self.asio_idx = device_index;
                            self.use_asio_input = true;
                            self.accept_asio_device();
                        }
                        InputGroup::System => {
                            self.input_idx = device_index;
                            self.use_asio_input = false;
                            self.accept_input_device();
                        }
                    }
                }
                if self.log_device_restore("Patch input", input_label, found.restore) {
                    device_missing = true;
                }
            }
            None => {
                if self.log_device_restore("Patch input", input_label, DeviceRestore::Missing) {
                    device_missing = true;
                }
            }
        }
        let output_label = if preset.output.is_empty() {
            preset.output_id.as_str()
        } else {
            preset.output.as_str()
        };
        let output_restore = restore_device(
            &self.inventory.outputs,
            &preset.output_id,
            &preset.output_backend,
            &preset.output,
        );
        if let Some(output_index) = output_restore.index() {
            self.output_idx = output_index;
            self.accept_output_device();
        }
        if self.log_device_restore("Patch output", output_label, output_restore) {
            device_missing = true;
        }
        // A saved device that is not connected is an error. The mix above
        // was still applied; do not report the patch as loaded.
        if !device_missing {
            self.push_log(format!("Loaded patch “{}”.", preset.name), BLUE);
        }
    }

    /// Logs a name fallback or a missing device. Returns true when a saved
    /// device was not found (an empty label is "nothing was stored", not a miss).
    fn log_device_restore(&mut self, role: &str, label: &str, restore: DeviceRestore) -> bool {
        let Some(note) = device_restore_note(role, label, restore) else {
            return false;
        };
        let missing = matches!(restore, DeviceRestore::Missing);
        self.push_log(note, if missing { RED } else { AMBER });
        missing
    }
}

#[cfg(test)]
mod tests {
    use crate::app::settings::{
        DEFAULT_BUFFER, DEFAULT_INPUT_GAIN, DEFAULT_SAMPLE_RATE, INPUT_GAIN_MIN_DB,
        MIN_OUTPUT_BUFFER, OUTPUT_GAIN_MAX_DB, RATES,
    };
    use crate::app::Page;
    use crate::app::RigWerkApp;
    use crate::audio::{CushionMode, DeviceItem, PluginFormat, PluginPref};
    use crate::ui::theme::{BLUE, RED};

    fn device(id: &str, name: &str, backend: &str) -> DeviceItem {
        DeviceItem {
            id: id.to_owned(),
            name: name.to_owned(),
            backend: backend.to_owned(),
            channels: 2,
            demo: false,
            preferred: false,
            asio_clsid: None,
        }
    }

    fn test_app() -> RigWerkApp {
        let mut app = crate::app::blank_app();
        app.page = Page::Sessions;
        app.inventory
            .asio
            .push(device("asio:focusrite", "Focusrite", "ASIO"));
        app.inventory.inputs.push(device("hw:Mic", "Mic", "ALSA"));
        app.inventory
            .outputs
            .push(device("hw:Speaker", "Speaker", "ALSA"));
        app
    }

    fn log_lines(app: &RigWerkApp) -> Vec<&str> {
        app.log.iter().map(|entry| entry.text.as_str()).collect()
    }

    #[test]
    fn a_missing_device_is_an_error_and_the_mix_still_applies() {
        let mut app = test_app();
        let routes = [[false, true, false, false], [true, false, false, true]];
        app.sessions.push(crate::app::settings::Preset {
            name: "Gone".to_owned(),
            input: "Other interface".to_owned(),
            input_id: "asio:missing".to_owned(),
            input_backend: "ASIO".to_owned(),
            output: "Other speakers".to_owned(),
            output_id: "hw:missing".to_owned(),
            output_backend: "ALSA".to_owned(),
            input_gain: 4.5,
            output_gain: -8.0,
            routes,
            input_channels: [true, false],
            output_channels: [false, true],
            mono: true,
            ..crate::app::settings::Preset::default()
        });
        app.input_gain = -12.0;
        app.asio_idx = 0;
        app.output_idx = 0;

        app.apply_preset(0);

        assert_eq!(app.input_gain, 4.5);
        assert_eq!(app.output_gain, -8.0);
        assert_eq!(app.routes, routes);
        assert_eq!(app.input_channels, [true, false]);
        assert_eq!(app.output_channels, [false, true]);
        assert!(app.mono);
        assert_eq!(app.asio_idx, 0);
        assert_eq!(app.output_idx, 0);
        assert_eq!(app.active_input().id, "asio:focusrite");
        assert_eq!(app.selected_output().id, "hw:Speaker");
        let lines = log_lines(&app);
        assert!(
            lines.iter().any(|line| line.contains("was not found")),
            "{lines:?}"
        );
        assert!(
            lines.iter().all(|line| !line.contains("Loaded patch")),
            "{lines:?}"
        );
        assert!(app
            .log
            .iter()
            .any(|entry| entry.text.contains("was not found") && entry.tone == RED));
        let linear = app.engine.shared.input_gain();
        assert!(linear.is_finite());
        assert!((linear - 10.0f32.powf(4.5 / 20.0)).abs() < 1e-4);
    }

    #[test]
    fn applying_a_preset_rejects_nan_gain_and_unknown_rate() {
        let mut app = test_app();
        app.sessions.push(crate::app::settings::Preset {
            name: "Hot".to_owned(),
            input: "Focusrite".to_owned(),
            input_id: "asio:focusrite".to_owned(),
            input_backend: "ASIO".to_owned(),
            output: "Speaker".to_owned(),
            output_id: "hw:Speaker".to_owned(),
            output_backend: "ALSA".to_owned(),
            input_gain: f32::NAN,
            output_gain: f32::INFINITY,
            rate: 22_050,
            buffer: 7,
            output_rate: 12_345,
            output_buffer: 4,
            ..crate::app::settings::Preset::default()
        });

        app.apply_preset(0);

        assert!(app.input_gain.is_finite());
        assert_eq!(app.input_gain, DEFAULT_INPUT_GAIN);
        assert!(app.input_gain >= INPUT_GAIN_MIN_DB);
        assert_eq!(app.output_gain, OUTPUT_GAIN_MAX_DB);
        assert!(app.output_gain.is_finite());
        assert_eq!(app.sample_rate, DEFAULT_SAMPLE_RATE);
        assert_ne!(app.sample_rate, 22_050);
        assert!(RATES.contains(&app.sample_rate));
        assert_eq!(app.buffer_size, DEFAULT_BUFFER);
        assert_ne!(app.buffer_size, 7);
        assert_eq!(app.output_rate, DEFAULT_SAMPLE_RATE);
        assert_ne!(app.output_rate, 12_345);
        assert_eq!(app.output_buffer, DEFAULT_BUFFER);
        assert!(app.output_buffer >= MIN_OUTPUT_BUFFER);
        assert_ne!(app.output_buffer, 4);
        assert_eq!(app.sessions[0].input_gain, DEFAULT_INPUT_GAIN);
        assert_eq!(app.sessions[0].rate, DEFAULT_SAMPLE_RATE);
        let input_linear = app.engine.shared.input_gain();
        let output_linear = app.engine.shared.output_gain();
        assert!(input_linear.is_finite() && input_linear > 0.0);
        assert!(output_linear.is_finite() && output_linear > 0.0);
        assert!((input_linear - 1.0).abs() < 1e-5);
        assert!(log_lines(&app)
            .iter()
            .any(|line| line.contains("Loaded patch")));
        assert_eq!(
            app.log
                .iter()
                .find(|entry| entry.text.contains("Loaded patch"))
                .map(|entry| entry.tone),
            Some(BLUE)
        );
    }

    #[test]
    fn saving_and_applying_a_preset_round_trips_the_mix() {
        let mut app = test_app();
        app.session_name = "Gig".to_owned();
        app.input_gain = 6.0;
        app.output_gain = -12.0;
        app.input_channels = [true, false];
        app.output_channels = [false, true];
        app.mono = true;
        app.mute = true;
        app.monitor = false;
        app.drift = false;
        app.low_latency_output = false;
        app.routes = [[false, true, false, true], [true, false, true, false]];
        app.cushion_mode = CushionMode::Manual;
        app.cushion_ms = 7.5;
        app.sample_rate = 96_000;
        app.buffer_size = 64;
        app.output_rate = 48_000;
        app.output_buffer = 256;
        app.engine.restore_plugins(vec![PluginPref {
            path: "C:/Plugins/Amp.vst3".to_owned(),
            format: PluginFormat::Vst3,
            name: "Amp".to_owned(),
            mix: 0.4,
            ..PluginPref::default()
        }]);

        app.save_preset();
        let saved = app.sessions[0].clone();
        assert_eq!(saved.input_id, "asio:focusrite");
        assert_eq!(saved.input_backend, "ASIO");
        assert_eq!(saved.output_id, "hw:Speaker");
        assert_eq!(saved.output_backend, "ALSA");
        assert_eq!(saved.input_gain, 6.0);
        assert_eq!(saved.routes, app.routes);
        assert_eq!(saved.plugins[0].path, "C:/Plugins/Amp.vst3");
        assert_eq!(saved.plugins[0].mix, 0.4);

        app.input_gain = 0.0;
        app.output_gain = 0.0;
        app.routes = [[true, false, false, false], [false, true, false, false]];
        app.mono = false;
        app.mute = false;
        app.monitor = true;
        app.drift = true;
        app.low_latency_output = true;
        app.input_channels = [true, true];
        app.output_channels = [true, true];
        app.cushion_mode = CushionMode::Safe;
        app.cushion_ms = 10.0;
        app.sample_rate = 48_000;
        app.buffer_size = 128;
        app.output_rate = 0;
        app.output_buffer = 128;
        app.engine.restore_plugins(Vec::new());
        app.inventory
            .asio
            .insert(0, device("asio:other", "Other", "ASIO"));
        app.asio_idx = 0;
        app.inventory
            .outputs
            .insert(0, device("hw:Other", "Other out", "ALSA"));
        app.output_idx = 0;

        app.apply_preset(0);

        assert_eq!(app.input_gain, 6.0);
        assert_eq!(app.output_gain, -12.0);
        assert_eq!(app.input_channels, [true, false]);
        assert_eq!(app.output_channels, [false, true]);
        assert!(app.mono);
        assert!(app.mute);
        assert!(!app.monitor);
        assert!(!app.drift);
        assert!(!app.low_latency_output);
        assert_eq!(
            app.routes,
            [[false, true, false, true], [true, false, true, false]]
        );
        assert_eq!(app.cushion_mode, CushionMode::Manual);
        assert_eq!(app.cushion_ms, 7.5);
        assert_eq!(app.sample_rate, 96_000);
        assert_eq!(app.buffer_size, 64);
        assert_eq!(app.output_rate, 48_000);
        assert_eq!(app.output_buffer, 256);
        assert_eq!(app.active_input().id, "asio:focusrite");
        assert_eq!(app.selected_output().id, "hw:Speaker");
        let plugins = app.engine.plugin_prefs();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].path, "C:/Plugins/Amp.vst3");
        assert_eq!(plugins[0].mix, 0.4);
        assert!((app.engine.shared.input_gain() - 10.0f32.powf(6.0 / 20.0)).abs() < 1e-4);
        assert!(log_lines(&app)
            .iter()
            .any(|line| line.contains("Loaded patch")));
    }

    #[test]
    fn an_old_preset_output_buffer_follows_the_input_buffer() {
        let mut app = test_app();
        app.sessions.push(crate::app::settings::Preset {
            name: "Old".to_owned(),
            input: "Focusrite".to_owned(),
            input_id: "asio:focusrite".to_owned(),
            input_backend: "ASIO".to_owned(),
            output: "Speaker".to_owned(),
            output_id: "hw:Speaker".to_owned(),
            output_backend: "ALSA".to_owned(),
            rate: 44_100,
            buffer: 64,
            output_rate: 0,
            output_buffer: 0,
            ..crate::app::settings::Preset::default()
        });
        app.apply_preset(0);
        assert_eq!(app.buffer_size, 64);
        assert_eq!(app.output_buffer, 64);
        assert_eq!(app.sessions[0].output_buffer, 0);
        assert_eq!(app.input_gain, DEFAULT_INPUT_GAIN);
    }

    #[test]
    fn applying_a_preset_restores_gate_settings() {
        use crate::audio::{GateSettings, GateSlot};

        let mut app = test_app();
        let gate = GateSettings {
            enabled: true,
            slot: GateSlot::Pre,
            threshold_db: -36.0,
            range_db: -72.0,
            hysteresis_db: 4.0,
            attack_ms: 3.0,
            release_ms: 200.0,
            hold_ms: 30.0,
        };
        app.sessions.push(crate::app::settings::Preset {
            name: "Gated".to_owned(),
            input: "Focusrite".to_owned(),
            input_id: "asio:focusrite".to_owned(),
            input_backend: "ASIO".to_owned(),
            output: "Speaker".to_owned(),
            output_id: "hw:Speaker".to_owned(),
            output_backend: "ALSA".to_owned(),
            gate: Some(gate),
            ..crate::app::settings::Preset::default()
        });
        app.gate = GateSettings::default();
        app.apply_preset(0);
        assert_eq!(app.gate, gate);
        assert_eq!(app.engine.shared.try_gate(), Some(gate));
    }

    #[test]
    fn presets_carry_the_output_devices() {
        use crate::app::settings::FollowerPref;
        use crate::audio::{ChannelMap, OutputDeviceSettings, UnusedChannels};

        let mut app = test_app();
        let clock = OutputDeviceSettings {
            level_db: -4.5,
            mute: true,
            map: ChannelMap {
                left: 0b0011,
                right: 0b1100,
            },
            unused: UnusedChannels::Silent,
        };
        let followers = vec![
            FollowerPref {
                id: "hw:Phones".to_owned(),
                backend: "ALSA".to_owned(),
                name: "Phones".to_owned(),
                cushion_mode: 2,
                cushion_ms: 15.0,
                ..FollowerPref::default()
            },
            FollowerPref {
                id: "hw:Monitor".to_owned(),
                name: "Monitor".to_owned(),
                ..FollowerPref::default()
            },
        ];
        app.session_name = "Two outs".to_owned();
        app.clock_device = clock;
        app.followers = followers.clone();

        app.save_preset();
        assert_eq!(app.sessions[0].clock_device, clock);
        assert_eq!(app.sessions[0].followers, followers);

        app.clock_device = OutputDeviceSettings::default();
        app.followers.clear();
        app.apply_preset(0);
        assert_eq!(app.clock_device, clock);
        assert_eq!(app.followers, followers);

        // An older patch without them brings back today's output.
        app.sessions.push(crate::app::settings::Preset {
            name: "Old".to_owned(),
            ..crate::app::settings::Preset::default()
        });
        app.apply_preset(1);
        assert_eq!(app.clock_device, OutputDeviceSettings::default());
        assert!(app.followers.is_empty());
    }

    #[test]
    fn saving_a_preset_writes_current_gate() {
        use crate::audio::{GateSettings, GateSlot};

        let mut app = test_app();
        app.session_name = "Mine".to_owned();
        app.asio_idx = 0;
        app.output_idx = 0;
        app.use_asio_input = true;
        app.gate = GateSettings {
            enabled: true,
            slot: GateSlot::Pre,
            threshold_db: -40.0,
            range_db: -50.0,
            hysteresis_db: 5.0,
            attack_ms: 1.5,
            release_ms: 90.0,
            hold_ms: 25.0,
        };
        app.save_preset();
        assert_eq!(app.sessions.len(), 1);
        assert_eq!(app.sessions[0].gate, Some(app.gate));
    }
}
