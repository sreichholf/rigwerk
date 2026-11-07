//! Saving, loading and deleting patches (saved device pairs).

use crate::app::devices::{
    device_restore_note, restore_device, restore_input, DeviceRestore, InputGroup,
};
use crate::app::settings::{cushion_from_pref, cushion_to_pref, sanitize_preset, PathPref, Preset};
use crate::app::RigWerkApp;
use crate::ui::theme::{AMBER, BLUE, MUTED, RED, TEAL};

impl RigWerkApp {
    pub(crate) fn save_preset(&mut self) {
        let input = self.active_input().clone();
        let output = self.selected_output().clone();
        let (paths, order) = self.saved_paths();
        let (clock_device, followers) = self.saved_devices(&order);
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
            input_gain: self.input_gain,
            output_gain: self.output_gain,
            input_channels: self.input_channels,
            mute: self.mute,
            monitor: self.monitor,
            drift: self.drift,
            low_latency_output: self.low_latency_output,
            wasapi_output_mode: Some(self.wasapi_output_mode),
            cushion_mode: cushion_to_pref(self.cushion_mode),
            cushion_ms: self.cushion_ms,
            paths,
            clock_device,
            followers,
            // The legacy mix fields are not written.
            ..Preset::default()
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
        // A patch from before paths: path 1 from its mix fields, keeping
        // what it never stored (an EQ or gate) from the first path now.
        let first = self.path_slots()[0];
        let current = self.paths[first]
            .as_ref()
            .map(|path| PathPref::from_mix(path.name.clone(), path.colour, &path.mix, Vec::new()))
            .unwrap_or_default();
        let Some(stored) = self.sessions.get_mut(index) else {
            return;
        };
        stored.migrate_paths(&current);
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
        self.mute = preset.mute;
        self.monitor = preset.monitor;
        self.drift = preset.drift;
        let mode = preset.wasapi_output_mode.unwrap_or_else(|| {
            crate::audio::WasapiOutputMode::from_low_latency(preset.low_latency_output)
        });
        self.set_wasapi_output_mode(mode);
        self.cushion_mode = cushion_from_pref(preset.cushion_mode);
        self.cushion_ms = preset.cushion_ms;
        // The device maps are in the patch's path order; installing maps
        // them onto the slots the paths land in.
        self.install_paths(
            &preset.paths,
            preset.clock_device,
            preset.followers.clone(),
            0,
        );
        self.engine.set_gains(self.input_gain, self.output_gain);
        self.engine
            .set_master(self.mute, self.monitor, self.input_channels, self.drift);
        self.engine.set_cushion(self.cushion_mode, self.cushion_ms);
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
mod tests;
