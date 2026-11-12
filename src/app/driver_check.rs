//! What each driver really runs at, compared with the chosen settings.
//!
//! Drivers are free to ignore a requested rate or buffer: an ASIO driver may
//! only accept the sizes set in its own panel, Windows shared mode delivers
//! in its own steps, and a device may be locked to one rate.  The screens show
//! the real values and explain every difference in plain words.

#[cfg(any(target_os = "windows", target_os = "linux"))]
use std::sync::mpsc;
use std::sync::mpsc::TryRecvError;
#[cfg(any(target_os = "windows", target_os = "linux"))]
use std::thread;
#[cfg(target_os = "linux")]
use std::time::Instant;

use crate::app::settings::rate_label;
use crate::app::{LogTone, RigWerkApp};
use crate::audio::EngineState;

/// Whether a driver result is good news or a limitation worth attention.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DriverTone {
    /// The driver runs as chosen, or allows the smaller REAL-style step.
    Good,
    /// The driver differs from the chosen settings in a way worth reading.
    Notice,
}

/// What one side of the running bridge really got.
#[derive(Clone)]
pub(crate) struct DriverActual {
    pub(crate) rate: u32,
    pub(crate) frames: u32,
    /// One plain explanation per difference from the chosen settings; empty
    /// when the driver runs exactly as asked.
    pub(crate) notes: Vec<String>,
    pub(crate) tone: DriverTone,
}

fn block_ms(frames: u32, rate: u32) -> f32 {
    frames as f32 / rate.max(1) as f32 * 1000.0
}

/// The result of a stand-alone output check.  No stream is opened, so this
/// can run before the bridge starts and is repeated whenever the device
/// list changes - the answer to "Check driver" after a driver swap.
pub(crate) struct DriverCheck {
    pub(crate) device_id: String,
    pub(crate) name: String,
    pub(crate) result: Result<DriverActual, String>,
    pub(crate) manual: bool,
}

/// Where a person changes a device's rate outside RigWerk.
fn system_rate_settings() -> &'static str {
    if cfg!(target_os = "windows") {
        "Windows sound settings (device properties › Advanced)"
    } else {
        "the system's sound settings"
    }
}

impl RigWerkApp {
    /// Re-reads PipeWire's quantum and compares it with the period this output
    /// will ask ALSA for.  Runs at startup, when the device changes and when
    /// the bridge starts, so a collision is visible before any audio flows.
    #[cfg(target_os = "linux")]
    pub(crate) fn refresh_quantum_check(&mut self, log: bool) {
        if matches!(
            self.engine.state,
            EngineState::Starting | EngineState::Running | EngineState::Stopping
        ) {
            return;
        }
        let output = self.selected_output().clone();
        if output.demo || !crate::audio::is_pipewire_device(&output) {
            self.quantum = None;
            self.quantum_checked_key = None;
            return;
        }
        let period = self.cached_output_period(&output.id);
        let next = crate::audio::pipewire_check_output(&output, period);
        let changed = match (&self.quantum, &next) {
            (Some(previous), Some(next)) => {
                previous.quantum != next.quantum
                    || previous.alsa_period != next.alsa_period
                    || previous.fits != next.fits
            }
            (None, None) => false,
            _ => true,
        };
        self.quantum = next;
        self.quantum_checked_key = Some((
            output.id.clone(),
            self.low_latency_output,
            self.output_buffer,
        ));
        if log && changed {
            if let Some(info) = &self.quantum {
                let tone = if info.fits && info.low_latency {
                    LogTone::Green
                } else {
                    LogTone::Amber
                };
                if !self.log.iter().any(|entry| entry.text == info.note) {
                    self.push_log(info.note.clone(), tone);
                }
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    #[allow(dead_code)]
    pub(crate) fn refresh_quantum_check(&mut self, _log: bool) {}

    /// Once the output runs, keep the graph quantum and the delivered block in
    /// sync. The idle check freezes while Running, so without this the
    /// quantum issue can stick on a pre-start 1024/48 kHz reading after the
    /// bridge has forced a smaller quantum (or the graph rate has moved).
    #[cfg(target_os = "linux")]
    pub(crate) fn refresh_quantum_actual(&mut self) {
        if self.engine.state != EngineState::Running {
            return;
        }
        let output = self.selected_output().clone();
        if output.demo || !crate::audio::is_pipewire_device(&output) {
            return;
        }
        let frames = self.snapshot.output_step_frames;
        let period = if frames > 0 {
            frames
        } else {
            self.quantum
                .as_ref()
                .map(|info| info.alsa_period)
                .unwrap_or(0)
        };
        // pw-metadata is a process spawn — at most once a second while live.
        let due = self.quantum_live_at.elapsed() >= std::time::Duration::from_secs(1);
        if due || self.quantum.is_none() {
            self.quantum_live_at = Instant::now();
            self.quantum = crate::audio::pipewire_check_output(&output, period);
            return;
        }
        if let Some(info) = self.quantum.clone() {
            if period > 0 && info.alsa_period != period {
                self.quantum = Some(info.with_period(period));
            }
        }
    }

    /// The period this output will ask for. Cached until the device, the
    /// low-latency switch, or the buffer changes, and never read while the
    /// bridge is opening that same device.
    #[cfg(target_os = "linux")]
    fn cached_output_period(&mut self, device_id: &str) -> u32 {
        if crate::audio::is_native_id(device_id) {
            return crate::audio::node_latency_frames(self.low_latency_output, self.output_buffer);
        }
        if let Some((id, low_latency, buffer, period)) = &self.period_cache {
            if id == device_id
                && *low_latency == self.low_latency_output
                && *buffer == self.output_buffer
            {
                return *period;
            }
        }
        let period = crate::audio::planned_output_period(
            device_id,
            self.low_latency_output,
            self.output_buffer,
        );
        self.period_cache = Some((
            device_id.to_owned(),
            self.low_latency_output,
            self.output_buffer,
            period,
        ));
        period
    }

    /// Compares PipeWire once per output setting, and only while the bridge
    /// is not holding the device. A running bridge updates the period from
    /// the snapshot instead of opening the PCM again.
    #[cfg(target_os = "linux")]
    pub(crate) fn refresh_quantum_if_idle(&mut self) {
        if matches!(
            self.engine.state,
            EngineState::Starting | EngineState::Running | EngineState::Stopping
        ) {
            return;
        }
        let output = self.selected_output();
        if output.demo || !crate::audio::is_pipewire_device(output) {
            if self.quantum.is_some() {
                self.quantum = None;
            }
            self.quantum_checked_key = None;
            return;
        }
        let key = (
            output.id.clone(),
            self.low_latency_output,
            self.output_buffer,
        );
        if self.quantum_checked_key.as_ref() == Some(&key) {
            return;
        }
        self.refresh_quantum_check(true);
    }

    #[cfg(not(target_os = "linux"))]
    #[allow(dead_code)]
    pub(crate) fn refresh_quantum_if_idle(&mut self) {}

    /// One short line for the Live page MODE / OUTPUT footers: graph block
    /// size in samples and how long that block lasts. Backend name is already
    /// on the cards above; "quantum" is PipeWire jargon that does not help here.
    pub(crate) fn quantum_summary(&self) -> Option<String> {
        #[cfg(target_os = "linux")]
        {
            self.quantum.as_ref().map(|info| {
                if info.available {
                    let ms = info.quantum as f32 / info.rate.max(1) as f32 * 1000.0;
                    format!("block {} smp · {:.1} ms", info.quantum, ms)
                } else {
                    "graph size unknown".to_owned()
                }
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }

    /// Hover for the MODE / OUTPUT graph-block line.
    pub(crate) fn graph_block_hint(&self) -> String {
        #[cfg(target_os = "linux")]
        {
            if let Some(info) = self.quantum.as_ref() {
                if info.available {
                    let ms = info.quantum as f32 / info.rate.max(1) as f32 * 1000.0;
                    let mut text = format!(
                        "PipeWire graph block: {} samples lasting {:.1} ms at {:.1} kHz. \
                         That is one chunk the desktop audio graph processes — not RigWerk's \
                         end-to-end latency (see LATENCY / HEARD above).",
                        info.quantum,
                        ms,
                        info.rate as f32 / 1000.0
                    );
                    if let Some(problem) = self.quantum_problem() {
                        text.push(' ');
                        text.push_str(&problem);
                    }
                    return text;
                }
            }
            "PipeWire graph block size is not available yet.".to_owned()
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.output_mode_text()
        }
    }

    /// The user-facing warning when the ALSA period and quantum collide or the
    /// quantum is too large to call low latency.
    pub(crate) fn quantum_problem(&self) -> Option<String> {
        #[cfg(target_os = "linux")]
        {
            self.quantum
                .as_ref()
                .filter(|info| !info.fits || !info.low_latency)
                .map(|info| info.note.clone())
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }

    /// Whether the stand-alone check can read the selected output now: the
    /// bridge is not running and the output is a real ALSA / PipeWire or
    /// WASAPI device (ASIO reads its own driver while it runs).
    pub(crate) fn output_driver_checkable(&self) -> bool {
        let output = self.selected_output();
        self.engine.state != EngineState::Running
            && !output.demo
            && output.asio_clsid.is_none()
            && (cfg!(target_os = "linux") || output.backend.eq_ignore_ascii_case("WASAPI"))
    }

    /// Whether the clock output asks for a smaller step than its buffer:
    /// Low latency on Linux, REAL-style or Exclusive on Windows.  An ASIO
    /// output has no such mode.
    pub(crate) fn wants_low_latency_output(&self) -> bool {
        self.selected_output().asio_clsid.is_none()
            && if cfg!(target_os = "windows") {
                self.wasapi_output_mode.wants_low_latency()
            } else {
                self.low_latency_output
            }
    }

    /// The input driver's real rate and block size, once the bridge runs and
    /// the first block has arrived.
    pub(crate) fn input_actual(&self) -> Option<DriverActual> {
        let rate = self.snapshot.input_rate;
        let frames = self.snapshot.input_block_frames;
        if self.engine.state != EngineState::Running || rate == 0 || frames == 0 {
            return None;
        }
        let asio = self.active_input().asio_clsid.is_some();
        let mut notes = Vec::new();
        if rate != self.sample_rate {
            notes.push(format!(
                "The input runs at {} instead of {}. The device or its driver decides the rate; RigWerk converts it for the output. To avoid converting, set {} in {}.",
                rate_label(rate),
                rate_label(self.sample_rate),
                rate_label(self.sample_rate),
                if asio {
                    "the ASIO driver's control panel"
                } else {
                    system_rate_settings()
                }
            ));
        }
        if frames as usize != self.buffer_size {
            notes.push(if asio {
                format!(
                    "The ASIO driver did not accept {} samples and delivers {} ({:.1} ms). Set the buffer in the driver's own control panel; it lists the sizes it accepts.",
                    self.buffer_size,
                    frames,
                    block_ms(frames, rate)
                )
            } else {
                format!(
                    "This input delivers blocks of {} samples ({:.1} ms) whatever buffer is asked for: the system's shared mode works in its own steps. For smaller blocks use {}.",
                    frames,
                    block_ms(frames, rate),
                    if cfg!(target_os = "windows") {
                        "an ASIO driver"
                    } else if cfg!(target_os = "linux") {
                        "a direct ALSA hardware input (hw: or plughw:)"
                    } else {
                        "JACK"
                    }
                )
            });
        }
        let tone = if notes.is_empty() {
            DriverTone::Good
        } else {
            DriverTone::Notice
        };
        Some(DriverActual {
            rate,
            frames,
            notes,
            tone,
        })
    }

    /// The output driver's real rate and step, once the bridge runs and the
    /// output has asked for audio.
    pub(crate) fn output_actual(&self) -> Option<DriverActual> {
        let rate = self.snapshot.output_rate;
        let frames = self.snapshot.output_step_frames;
        if self.engine.state != EngineState::Running || rate == 0 || frames == 0 {
            // Stopped: prefer a fresh stand-alone check, then the last real
            // reading from a running bridge.  A failed check is not a driver
            // failure — the bridge itself may play fine — so it falls through
            // instead of replacing the reading with "unavailable".
            let selected = self.selected_output().id.as_str();
            if let Some(check) = self.driver_probe.as_ref() {
                if check.device_id == selected {
                    if let Ok(actual) = &check.result {
                        return Some(actual.clone());
                    }
                }
            }
            if let Some((device_id, actual)) = &self.last_output_driver {
                if device_id == selected {
                    let mut actual = actual.clone();
                    actual.notes.push(
                        "This is the last reading from a running bridge. The stand-alone check could not read the output just now.".to_owned(),
                    );
                    return Some(actual);
                }
            }
            return None;
        }
        let mode = &self.snapshot.output_mode;
        let asio = self.selected_output().asio_clsid.is_some();
        let mut notes = Vec::new();

        let input_rate = self.snapshot.input_rate;
        if self.output_rate > 0 && rate != self.output_rate {
            notes.push(format!(
                "The output runs at {} instead of {}. {} RigWerk converts to it.",
                rate_label(rate),
                rate_label(self.output_rate),
                if asio {
                    "The ASIO driver decides its rate."
                } else {
                    "Shared outputs mix at the rate set for the device in the system."
                }
            ));
        } else if self.output_rate == 0 && input_rate > 0 && rate != input_rate {
            notes.push(format!(
                "The output runs at {} while the input runs at {}, so RigWerk converts between them. Setting both devices to the same rate in {} avoids converting.",
                rate_label(rate),
                rate_label(input_rate),
                system_rate_settings()
            ));
        }

        if mode.starts_with("REAL-style") {
            // REAL-style asks for the driver's smallest step, not the chosen
            // buffer; only a step above it, or a driver with no smaller step
            // at all, is worth a word.
            if mode.contains("another app holds") {
                notes.push(format!(
                    "REAL-style is on, but another app holds this device at {} samples ({:.1} ms), above the driver's minimum. Close other audio apps on this device and restart the bridge.",
                    frames,
                    block_ms(frames, rate)
                ));
            } else if mode.contains("no smaller step") {
                notes.push(format!(
                    "This output offers only {:.1} ms steps in shared mode: that is the driver's own minimum, so no app (including the REAL tool) can go below it here while other apps share the device. For a smaller step, install Windows' in-box \"High Definition Audio Device\" driver for this output, or choose an ASIO output. The bridge still runs at this minimum instead of a larger buffer.",
                    block_ms(frames, rate)
                ));
            }
        } else if mode.starts_with("Exclusive") {
            // Exclusive uses the driver's exclusive period, not the chosen
            // shared output buffer.  A size mismatch is expected and not a
            // shared-mode mixing note.
        } else if mode.starts_with("Low latency") {
            // Low latency deliberately asks for a different size than the
            // manual output buffer, so a difference is not a surprise here.
        } else if frames as usize != self.output_buffer {
            let mut note = if asio {
                format!(
                    "The ASIO output driver did not accept {} samples and uses {} ({:.1} ms). Set the buffer in the driver's own control panel.",
                    self.output_buffer,
                    frames,
                    block_ms(frames, rate)
                )
            } else {
                format!(
                    "This output works in steps of {} samples ({:.1} ms), not the {} chosen: the system mixes shared outputs in its own steps.",
                    frames,
                    block_ms(frames, rate),
                    self.output_buffer
                )
            };
            if let Some(reason) = mode.split("Exclusive refused: ").nth(1) {
                note.push_str(&format!(" Exclusive mode was refused: {reason}."));
            } else if let Some(reason) = mode.split("REAL-style refused: ").nth(1) {
                note.push_str(&format!(" REAL-style could not go lower: {reason}."));
            }
            notes.push(note);
        }
        let tone = if notes.is_empty() {
            DriverTone::Good
        } else {
            DriverTone::Notice
        };
        Some(DriverActual {
            rate,
            frames,
            notes,
            tone,
        })
    }

    /// Once per bridge start, as soon as both sides are known: a log line
    /// for each side that does not run as chosen.
    pub(crate) fn check_driver_settings(&mut self) {
        if self.driver_checked {
            return;
        }
        let (Some(input), Some(output)) = (self.input_actual(), self.output_actual()) else {
            return;
        };
        self.driver_checked = true;
        for (side, actual) in [("Input", input), ("Output", output)] {
            if !actual.notes.is_empty() {
                self.push_log(
                    format!(
                        "{side} driver runs {} · {} smp, not as chosen — hover its DRIVER line for why.",
                        rate_label(actual.rate),
                        actual.frames
                    ),
                    LogTone::Amber);
            }
        }
        if let Some(text) = self.quantum_problem() {
            if !self.log.iter().any(|entry| entry.text == text) {
                self.push_log(text, LogTone::Amber);
            }
        }
    }

    /// Keeps the true output reading for the stopped DRIVER line, so Stop
    /// does not turn a working REAL-style driver back into a warning.
    pub(crate) fn remember_output_driver(&mut self) {
        if self.engine.state != EngineState::Running {
            return;
        }
        let device_id = self.selected_output().id.clone();
        let Some(actual) = self.output_actual() else {
            return;
        };
        let changed = self
            .last_output_driver
            .as_ref()
            .is_none_or(|(id, previous)| {
                id != &device_id
                    || previous.rate != actual.rate
                    || previous.frames != actual.frames
                    || previous.tone != actual.tone
            });
        if changed {
            self.last_output_driver = Some((device_id, actual));
        }
    }

    /// Checks the selected output's driver on its own thread.  It only asks
    /// Windows which engine steps the driver allows; no stream is opened, so
    /// this is safe before the bridge runs and costs no audio.
    pub(crate) fn start_output_driver_check(&mut self, manual: bool) {
        #[cfg(target_os = "windows")]
        {
            if matches!(
                self.engine.state,
                EngineState::Running | EngineState::Starting | EngineState::Stopping
            ) {
                if manual {
                    self.push_log(
                        "Stop the bridge before checking its output driver.",
                        LogTone::Amber,
                    );
                }
                return;
            }
            let (demo, asio, wasapi, id, name) = {
                let output = self.selected_output();
                (
                    output.demo,
                    output.asio_clsid.is_some(),
                    output.backend.eq_ignore_ascii_case("WASAPI"),
                    output.id.clone(),
                    output.name.clone(),
                )
            };
            if demo || asio || !wasapi || self.driver_probe_job.is_some() {
                return;
            }
            let (sender, receiver) = mpsc::channel();
            let log_name = name.clone();
            let spawned = thread::Builder::new()
                .name("rigwerk-driver-check".to_owned())
                .spawn(move || {
                    let result = probe_output(&id);
                    let _ = sender.send(DriverCheck {
                        device_id: id,
                        name,
                        result,
                        manual,
                    });
                });
            match spawned {
                Ok(_) => {
                    if manual {
                        self.push_log(
                            format!("Checking {log_name}'s output driver…"),
                            LogTone::Blue,
                        );
                    }
                    self.driver_probe_job = Some(receiver);
                }
                Err(error) => {
                    if manual {
                        self.push_log(
                            format!("Could not start the driver check: {error}"),
                            LogTone::Amber,
                        );
                    }
                }
            }
        }
        #[cfg(target_os = "linux")]
        {
            if matches!(
                self.engine.state,
                EngineState::Running | EngineState::Starting | EngineState::Stopping
            ) {
                if manual {
                    self.push_log(
                        "Stop the bridge before checking its output driver.",
                        LogTone::Amber,
                    );
                }
                return;
            }
            let (demo, asio, id, name, asked_rate, low_latency, buffer_frames) = {
                let output = self.selected_output();
                let asked_rate = if self.output_rate == 0 {
                    self.sample_rate
                } else {
                    self.output_rate
                };
                (
                    output.demo,
                    output.asio_clsid.is_some(),
                    output.id.clone(),
                    output.name.clone(),
                    asked_rate,
                    self.low_latency_output,
                    self.output_buffer,
                )
            };
            if demo || asio || self.driver_probe_job.is_some() {
                return;
            }
            let (sender, receiver) = mpsc::channel();
            let log_name = name.clone();
            let spawned = thread::Builder::new()
                .name("rigwerk-driver-check".to_owned())
                .spawn(move || {
                    let result = probe_output(&id, asked_rate, low_latency, buffer_frames);
                    let _ = sender.send(DriverCheck {
                        device_id: id,
                        name,
                        result,
                        manual,
                    });
                });
            match spawned {
                Ok(_) => {
                    if manual {
                        self.push_log(
                            format!("Checking {log_name}'s output driver…"),
                            LogTone::Blue,
                        );
                    }
                    self.driver_probe_job = Some(receiver);
                }
                Err(error) => {
                    if manual {
                        self.push_log(
                            format!("Could not start the driver check: {error}"),
                            LogTone::Amber,
                        );
                    }
                }
            }
        }
        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        {
            let _ = manual;
        }
    }

    /// Picks up a finished stand-alone check and logs it when the person
    /// asked for it, or when it found a problem they need to know about.
    pub(crate) fn poll_output_driver_check(&mut self) {
        let received = self.driver_probe_job.as_ref().map(|job| job.try_recv());
        match received {
            Some(Ok(check)) => {
                self.driver_probe_job = None;
                let name = check.name.clone();
                // A check at launch or after a device change is news only
                // when it found the smaller REAL-style step; a manual check
                // always reports its result.
                let first_look = self
                    .driver_probe
                    .as_ref()
                    .is_none_or(|previous| previous.device_id != check.device_id);
                match &check.result {
                    Ok(actual)
                        if check.manual || (first_look && actual.tone == DriverTone::Good) =>
                    {
                        self.push_log(
                            format!(
                                "{name}: driver verified — smallest step {} smp ({:.1} ms).",
                                actual.frames,
                                block_ms(actual.frames, actual.rate)
                            ),
                            LogTone::Green,
                        );
                    }
                    Ok(_) => {}
                    Err(reason) => {
                        let message = format!("{name}: {reason}");
                        if !self.log.iter().any(|entry| entry.text == message) {
                            self.push_log(message, LogTone::Amber);
                        }
                    }
                }
                self.driver_probe = Some(check);
            }
            Some(Err(TryRecvError::Disconnected)) => {
                self.driver_probe_job = None;
            }
            Some(Err(TryRecvError::Empty)) => {}
            None => {}
        }
    }

    /// Keeps the stand-alone output check pointed at the selected device: it
    /// runs once at launch, again when another output is chosen, and after
    /// the device list changes.
    pub(crate) fn watch_output_driver(&mut self) {
        #[cfg(target_os = "windows")]
        {
            if self.driver_probe_job.is_some() {
                return;
            }
            if matches!(
                self.engine.state,
                EngineState::Preview
                    | EngineState::Starting
                    | EngineState::Running
                    | EngineState::Stopping
            ) {
                return;
            }
            let (demo, asio, wasapi, id) = {
                let output = self.selected_output();
                (
                    output.demo,
                    output.asio_clsid.is_some(),
                    output.backend.eq_ignore_ascii_case("WASAPI"),
                    output.id.clone(),
                )
            };
            if demo || asio || !wasapi {
                return;
            }
            if self
                .driver_probe
                .as_ref()
                .is_some_and(|check| check.device_id == id)
            {
                return;
            }
            self.start_output_driver_check(false);
        }
        #[cfg(target_os = "linux")]
        {
            if self.driver_probe_job.is_some() {
                return;
            }
            if matches!(
                self.engine.state,
                EngineState::Preview
                    | EngineState::Starting
                    | EngineState::Running
                    | EngineState::Stopping
            ) {
                return;
            }
            let (demo, asio, id) = {
                let output = self.selected_output();
                (output.demo, output.asio_clsid.is_some(), output.id.clone())
            };
            if demo || asio {
                return;
            }
            if self
                .driver_probe
                .as_ref()
                .is_some_and(|check| check.device_id == id)
            {
                return;
            }
            self.start_output_driver_check(false);
        }
        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        {
            return;
        }
    }
}

/// A cpal device id is stored as `WASAPI:{endpoint}` (the host name, then the
/// endpoint).  Windows' device enumerator wants only the endpoint part.
#[cfg(any(target_os = "windows", test))]
fn wasapi_endpoint(device_id: &str) -> &str {
    device_id
        .strip_prefix("WASAPI:")
        .or_else(|| device_id.split_once(':').map(|(_, rest)| rest))
        .unwrap_or(device_id)
}

/// A stand-alone ALSA check: the rate the output defaults to, the period range
/// the driver reports, and what the low-latency switch will ask for.  No stream
/// is opened, so it is safe before the bridge starts.
#[cfg(target_os = "linux")]
fn probe_output(
    id: &str,
    asked_rate: u32,
    low_latency: bool,
    buffer_frames: usize,
) -> Result<DriverActual, String> {
    use cpal::traits::{DeviceTrait, HostTrait};

    if crate::audio::is_native_id(id) {
        let frames = crate::audio::node_latency_frames(low_latency, buffer_frames);
        let rate = asked_rate.max(1);
        let node = id.strip_prefix("pipewire:").unwrap_or(id);
        let mut notes = vec![format!(
            "This is the PipeWire sink \"{node}\". RigWerk links its own playback node to it and asks that node for {frames} frames ({:.1} ms at {}). The graph stays the clock, so other apps keep playing.",
            block_ms(frames, rate),
            rate_label(rate)
        )];
        let quantum_device = crate::audio::DeviceItem {
            id: id.to_owned(),
            name: String::new(),
            backend: "PIPEWIRE".to_owned(),
            channels: 2,
            demo: false,
            preferred: false,
            asio_clsid: None,
        };
        let mut tone = DriverTone::Good;
        if let Some(info) = crate::audio::pipewire_check_output(&quantum_device, frames) {
            let low = info.fits && info.low_latency;
            notes.push(info.note);
            if !low {
                tone = DriverTone::Notice;
            }
        }
        return Ok(DriverActual {
            rate,
            frames,
            notes,
            tone,
        });
    }

    let device_id: cpal::DeviceId = id
        .parse()
        .map_err(|error| format!("Output device id: {error}"))?;
    let host =
        cpal::host_from_id(device_id.host()).map_err(|error| format!("Output host: {error}"))?;
    let device = host
        .device_by_id(&device_id)
        .ok_or_else(|| "The selected output disappeared".to_owned())?;
    let config = device
        .default_output_config()
        .map_err(|error| format!("No usable output configuration: {error}"))?;
    let rate = config.sample_rate();
    let frames = crate::audio::smallest_output_buffer(&config, 32) as u32;
    let direct = crate::audio::alsa_pcm_is_direct(id);
    let mut notes = Vec::new();
    match config.buffer_size() {
        cpal::SupportedBufferSize::Range { min, max } => {
            notes.push(format!(
                "ALSA reports periods from {} to {} frames. Low latency asks for {} ({:.1} ms at {}); if that is refused, the normal output buffer is used.",
                min,
                max,
                frames,
                block_ms(frames, rate),
                rate_label(rate)
            ));
        }
        cpal::SupportedBufferSize::Unknown => {
            notes.push(format!(
                "This ALSA output leaves its period size open. Low latency asks for {} frames ({:.1} ms at {}); if that is refused, the normal output buffer is used.",
                frames,
                block_ms(frames, rate),
                rate_label(rate)
            ));
        }
    }
    if direct {
        notes.push(
            "This is a direct hardware PCM (`hw:` or `plughw:`), so it bypasses the desktop mixer and can hold the card exclusively."
                .to_owned(),
        );
    } else {
        notes.push(
            "This is a virtual ALSA device (usually the desktop's PipeWire or PulseAudio server). Other apps keep playing, but the server adds its own buffering."
                .to_owned(),
        );
    }
    let quantum_device = crate::audio::DeviceItem {
        id: id.to_owned(),
        name: String::new(),
        backend: "ALSA".to_owned(),
        channels: 0,
        demo: false,
        preferred: false,
        asio_clsid: None,
    };
    let mut tone = if direct {
        DriverTone::Good
    } else {
        DriverTone::Notice
    };
    if let Some(info) = crate::audio::pipewire_check_output(&quantum_device, frames) {
        let low = info.fits && info.low_latency;
        notes.push(info.note);
        if !low {
            tone = DriverTone::Notice;
        }
    }
    Ok(DriverActual {
        rate,
        frames,
        notes,
        tone,
    })
}

/// A stand-alone check of one output driver: its real rate and the smallest
/// step it allows.  Notes say what that means for REAL-style.
#[cfg(target_os = "windows")]
fn probe_output(id: &str) -> Result<DriverActual, String> {
    let info = crate::audio::probe_output_driver(wasapi_endpoint(id))?;
    let rate = info.rate;
    let min = info.min_frames;
    let default = info.default_frames;
    let mut notes = Vec::new();
    let tone = if min >= default {
        notes.push(format!(
            "This output's driver offers only {:.1} ms steps in shared mode — its own minimum, so REAL-style cannot go below it here. If the device is still on its vendor driver, Windows' in-box \"High Definition Audio Device\" driver often allows about 3 ms; an ASIO output can also go lower.",
            block_ms(min, rate)
        ));
        DriverTone::Notice
    } else {
        notes.push(format!(
            "Driver check passed: this driver allows a {:.1} ms step ({} samples), down from its {:.1} ms default. Starting the bridge uses the smaller step.",
            block_ms(min, rate),
            min,
            block_ms(default, rate)
        ));
        DriverTone::Good
    };
    Ok(DriverActual {
        rate,
        frames: min,
        notes,
        tone,
    })
}

#[cfg(test)]
mod tests;
