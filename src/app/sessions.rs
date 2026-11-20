//! Saving, loading and deleting sessions (saved device pairs).

use crate::app::devices::{
    device_restore_note, restore_device, restore_input, DeviceRestore, InputGroup,
};
use crate::app::settings::{
    bridge_buffer_from_pref, bridge_buffer_to_pref, sanitize_session, PathPref, Session,
};
use crate::app::LogTone;
use crate::app::RigWerkApp;

/// Saved fields a person edits, without the note or the click masks.
fn setup_key(session: &Session) -> String {
    let mut session = session.clone();
    session.note.clear();
    session.clock_output.click_mask = 0;
    for extra in &mut session.extra_outputs {
        extra.settings.click_mask = 0;
    }
    ron::ser::to_string(&session).unwrap_or_default()
}

impl RigWerkApp {
    /// The session `save_session` would write for the current setup.
    pub(crate) fn capture_session(&self) -> Session {
        let input = self.active_input().clone();
        let output = self.selected_output().clone();
        let (paths, order) = self.saved_paths();
        let (clock_output, extra_outputs) = self.saved_devices(&order);
        Session {
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
            drift: self.drift,
            low_latency_output: self.low_latency_output,
            wasapi_output_mode: Some(self.wasapi_output_mode),
            bridge_buffer_mode: bridge_buffer_to_pref(self.bridge_buffer_mode),
            bridge_buffer_ms: self.bridge_buffer_ms,
            paths,
            clock_output,
            extra_outputs,
            // The legacy mix fields are not written.  The metronome stays a
            // preference: applying a session does not retune it.
            ..Session::default()
        }
    }

    /// The setup differs from the saved session of this name, or there is
    /// no saved session with that name.  Click masks are not part of a
    /// session, so they are left out of the comparison.
    pub(crate) fn session_edited(&self) -> bool {
        let Some(index) = self.session_named(&self.session_name) else {
            return true;
        };
        setup_key(&self.sessions[index]) != setup_key(&self.capture_session())
    }

    pub(crate) fn start_new_session(&mut self) {
        self.session_name = "Untitled session".to_owned();
        self.selected_session = None;
        self.pending_delete = None;
        self.chrome.load_confirm = None;
        self.push_log("Started a new session.", LogTone::Muted);
    }

    pub(crate) fn rename_session(&mut self, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            self.push_log("Give the session a name before renaming.", LogTone::Amber);
            return;
        }
        if let Some(index) = self.session_named(&self.session_name) {
            if self.session_named(name).is_some_and(|other| other != index) {
                self.push_log(
                    format!("A session named “{name}” is already saved."),
                    LogTone::Amber,
                );
                return;
            }
            self.sessions[index].name = name.to_owned();
        }
        self.session_name = name.to_owned();
    }

    pub(crate) fn save_session(&mut self) {
        let session = self.capture_session();
        if session.name.is_empty() {
            self.push_log("Give the session a name before saving.", LogTone::Amber);
            return;
        }
        self.pending_delete = None;
        if let Some(index) = self.session_named(&session.name) {
            let name = session.name.clone();
            self.sessions[index] = session;
            self.selected_session = Some(index);
            self.push_log(format!("Updated session “{name}”."), LogTone::Teal);
            return;
        }
        let name = session.name.clone();
        self.sessions.insert(0, session);
        self.selected_session = Some(0);
        self.push_log(format!("Saved session “{name}”."), LogTone::Teal);
    }

    /// Sessions are matched by name, ignoring case and surrounding spaces.
    pub(crate) fn session_named(&self, name: &str) -> Option<usize> {
        let name = name.trim();
        self.sessions
            .iter()
            .position(|session| session.name.trim().eq_ignore_ascii_case(name))
    }

    pub(crate) fn delete_session(&mut self, index: usize) {
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
        self.push_log(
            format!("Deleted session “{}”.", removed.name),
            LogTone::Muted,
        );
    }

    pub(crate) fn apply_session(&mut self, index: usize) {
        // A session from before paths: path 1 from its mix fields, keeping
        // what it never stored (an EQ or gate) from the first path now.
        let first = self.path_slots()[0];
        let current = self.paths[first]
            .as_ref()
            .map(|path| {
                PathPref::from_mix(
                    path.name.clone(),
                    path.colour,
                    &path.mix,
                    path.nam_settings(),
                    &path.cab_file,
                    Vec::new(),
                )
            })
            .unwrap_or_default();
        let Some(stored) = self.sessions.get_mut(index) else {
            return;
        };
        stored.migrate_paths(&current);
        // Same check as loading preferences: bad gains, rates and buffers
        // are replaced here, and the stored session keeps the safe values.
        sanitize_session(stored);
        let session = stored.clone();
        self.session_name = session.name.clone();
        self.sample_rate = session.rate;
        self.buffer_size = session.buffer;
        self.output_rate = session.output_rate;
        self.output_buffer = if session.output_buffer == 0 {
            session.buffer
        } else {
            session.output_buffer
        };
        self.input_gain = session.input_gain;
        self.output_gain = session.output_gain;
        self.input_channels = session.input_channels;
        self.mute = session.mute;
        self.drift = session.drift;
        let mode = session.wasapi_output_mode.unwrap_or_else(|| {
            crate::audio::WasapiOutputMode::from_low_latency(session.low_latency_output)
        });
        self.set_wasapi_output_mode(mode);
        self.bridge_buffer_mode = bridge_buffer_from_pref(session.bridge_buffer_mode);
        self.bridge_buffer_ms = session.bridge_buffer_ms;
        // The click is not part of a session: it stays with the physical
        // device.  Each device the session uses (its clock and each extra output)
        // gets the mask that device has now, as the clock or as an extra output,
        // matched by id and backend; a device that is not live gets none.
        // The session's clock is the output it restores to, or the current
        // output when that is not found (it stays the clock).
        let current_output = self.selected_output();
        let live: Vec<(&str, &str, u8)> = std::iter::once((
            current_output.id.as_str(),
            current_output.backend.as_str(),
            self.clock_output.click_mask,
        ))
        .chain(self.extra_outputs.iter().map(|live| {
            (
                live.id.as_str(),
                live.backend.as_str(),
                live.settings.click_mask,
            )
        }))
        .collect();
        let live_mask = |id: &str, backend: &str| {
            live.iter()
                .find(|(live_id, live_backend, _)| {
                    *live_id == id && live_backend.eq_ignore_ascii_case(backend)
                })
                .map_or(0, |&(_, _, mask)| mask)
        };
        let session_output = restore_device(
            &self.inventory.outputs,
            &session.output_id,
            &session.output_backend,
            &session.output,
        )
        .index()
        .and_then(|index| self.inventory.outputs.get(index))
        .unwrap_or(current_output);
        let mut clock = session.clock_output;
        clock.click_mask = live_mask(&session_output.id, &session_output.backend);
        let extra_outputs: Vec<_> = session
            .extra_outputs
            .iter()
            .cloned()
            .map(|mut extra_output| {
                extra_output.settings.click_mask =
                    live_mask(&extra_output.id, &extra_output.backend);
                extra_output
            })
            .collect();
        // The device maps are in the session's path order; installing maps
        // them onto the slots the paths land in.
        self.install_paths(&session.paths, clock, extra_outputs, 0);
        self.apply_gains();
        self.engine
            .set_master(self.mute, self.input_channels, self.drift);
        self.apply_bridge_buffer();
        self.selected_session = Some(index);
        self.pending_delete = None;
        let mut device_missing = false;
        let input_label = if session.input.is_empty() {
            session.input_id.as_str()
        } else {
            session.input.as_str()
        };
        match restore_input(
            &self.inventory.asio,
            &self.inventory.inputs,
            &session.input_id,
            &session.input_backend,
            &session.input,
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
                if self.log_device_restore("Session input", input_label, found.restore) {
                    device_missing = true;
                }
            }
            None => {
                if self.log_device_restore("Session input", input_label, DeviceRestore::Missing) {
                    device_missing = true;
                }
            }
        }
        let output_label = if session.output.is_empty() {
            session.output_id.as_str()
        } else {
            session.output.as_str()
        };
        let output_restore = restore_device(
            &self.inventory.outputs,
            &session.output_id,
            &session.output_backend,
            &session.output,
        );
        if let Some(output_index) = output_restore.index() {
            self.output_idx = output_index;
            self.accept_output_device();
        }
        if self.log_device_restore("Session output", output_label, output_restore) {
            device_missing = true;
        }
        // A saved device that is not connected is an error. The mix above
        // was still applied; do not report the session as loaded.
        if !device_missing {
            self.push_log(format!("Loaded session “{}”.", session.name), LogTone::Blue);
        }
    }

    /// Logs a name fallback or a missing device. Returns true when a saved
    /// device was not found (an empty label is "nothing was stored", not a miss).
    fn log_device_restore(&mut self, role: &str, label: &str, restore: DeviceRestore) -> bool {
        let Some(note) = device_restore_note(role, label, restore) else {
            return false;
        };
        let missing = matches!(restore, DeviceRestore::Missing);
        self.push_log(
            note,
            if missing {
                LogTone::Red
            } else {
                LogTone::Amber
            },
        );
        missing
    }
}

#[cfg(test)]
mod tests;
