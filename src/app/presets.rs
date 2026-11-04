//! Saving, loading and deleting patches (saved device pairs).

use crate::app::settings::Preset;
use crate::app::RigWerkApp;
use crate::audio::EqSlot;
use crate::ui::theme::{AMBER, BLUE, MUTED, TEAL};

impl RigWerkApp {
    pub(crate) fn save_preset(&mut self) {
        let preset = Preset {
            name: self.session_name.trim().to_owned(),
            note: format!(
                "{} → {}",
                self.active_input().backend,
                self.selected_output().backend
            ),
            input: self.active_input().name.clone(),
            output: self.selected_output().name.clone(),
            rate: self.sample_rate,
            buffer: self.buffer_size,
            output_rate: self.output_rate,
            output_buffer: self.output_buffer,
            eq: Some(self.eqs[EqSlot::Pre.index()]),
            eq_post: Some(self.eqs[EqSlot::Post.index()]),
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
        let Some(preset) = self.sessions.get(index).cloned() else {
            return;
        };
        self.session_name = preset.name.clone();
        self.sample_rate = preset.rate;
        self.buffer_size = preset.buffer;
        self.output_rate = preset.output_rate;
        self.output_buffer = if preset.output_buffer == 0 {
            preset.buffer
        } else {
            preset.output_buffer
        };
        if let Some(eq) = preset.eq {
            self.eqs[EqSlot::Pre.index()] = eq.sanitized();
        }
        if let Some(eq) = preset.eq_post {
            self.eqs[EqSlot::Post.index()] = eq.sanitized();
        }
        self.selected_session = Some(index);
        self.pending_delete = None;
        if let Some(asio_index) = self
            .inventory
            .asio
            .iter()
            .position(|device| device.name == preset.input)
        {
            self.asio_idx = asio_index;
            self.use_asio_input = true;
        } else if let Some(input_index) = self
            .inventory
            .inputs
            .iter()
            .position(|device| device.name == preset.input)
        {
            self.input_idx = input_index;
            self.use_asio_input = false;
        }
        if let Some(output_index) = self
            .inventory
            .outputs
            .iter()
            .position(|device| device.name == preset.output)
        {
            self.output_idx = output_index;
        }
        self.push_log(format!("Loaded patch “{}”.", preset.name), BLUE);
    }
}
