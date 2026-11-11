//! The measured loopback and the conditions it was measured under.
//!
//! A [`MeasuredLatency`] is per hardware pair, not per session: it stores the
//! input and output, the rates the bridge really ran at, the input block and
//! output step, and how the output opened.  [`MeasuredLatency::stale_reason`]
//! changes one of those and the result is marked stale instead of being
//! silently reused.  The estimate is added separately; nothing in this file
//! estimates anything.
//!
//! The type is stored in the preferences, so every field reads through
//! `#[serde(default)]` and [`MeasuredLatency::sanitized`] drops a value that
//! cannot describe a real path.

use serde::Deserialize;

use crate::app::RigWerkApp;
use crate::audio::{EngineState, MAX_ROUND_TRIP_MS};

/// How the person closed the loop.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LoopbackKind {
    /// A cable from the output to the input.  The default: the cleanest
    /// measurement, with no room or microphone in the way.
    #[default]
    Cable,
    /// A microphone in front of the speaker, for a rig with no return path.
    Microphone,
}

impl LoopbackKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Cable => "cable",
            Self::Microphone => "microphone",
        }
    }

    pub(crate) fn advice(self) -> &'static str {
        match self {
            Self::Cable => {
                "Connect a cable from the chosen output to the chosen input. The click leaves on the output and returns on the input, so the measurement includes both converters and the device buffers; there is no room in between."
            }
            Self::Microphone => {
                "Put a microphone in front of the speaker on the chosen output and point it at the chosen input. The room, the speaker and the microphone add to the loopback's delay."
            }
        }
    }
}

/// A measured loopback, with the conditions it was measured under.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct MeasuredLatency {
    /// The measured loopback, milliseconds.
    pub(crate) latency_ms: f32,
    /// The same delay in samples at [`Self::input_rate`], the rate the click
    /// arrived at.
    pub(crate) samples: u32,
    pub(crate) input_id: String,
    pub(crate) input_name: String,
    pub(crate) output_id: String,
    pub(crate) output_name: String,
    /// The rates the bridge actually ran at.
    pub(crate) input_rate: u32,
    pub(crate) output_rate: u32,
    /// The block the input delivered, frames.
    pub(crate) input_buffer: u32,
    /// The step the output ran at, frames.
    pub(crate) output_step: u32,
    /// The output mode this was measured in: `real-style`, `shared`,
    /// `exclusive`, `asio`, `low-latency` or `standard`.
    pub(crate) output_mode: String,
    /// Unix seconds, UTC.
    pub(crate) measured_at: u64,
    /// The normalised correlation of the loopback click, 0..1.
    pub(crate) confidence: f32,
    pub(crate) loopback: LoopbackKind,
}

impl MeasuredLatency {
    /// Drops a stored measurement that cannot describe a real path.  A
    /// damaged or older entry is skipped, not fatal, like every other saved
    /// list entry.
    pub(crate) fn sanitized(mut self) -> Option<Self> {
        if !self.latency_ms.is_finite()
            || self.latency_ms <= 0.0
            || self.latency_ms > MAX_ROUND_TRIP_MS
        {
            return None;
        }
        if self.samples == 0
            || self.input_rate == 0
            || self.output_rate == 0
            || self.input_id.trim().is_empty()
            || self.output_id.trim().is_empty()
        {
            return None;
        }
        if !self.confidence.is_finite() {
            self.confidence = 0.0;
        }
        self.confidence = self.confidence.clamp(0.0, 1.0);
        Some(self)
    }

    /// Why this measurement no longer describes the rig, or `None` while it
    /// still does.  Device identity is the id, not the display name: a
    /// renamed device measured the same path.
    pub(crate) fn stale_reason(&self, now: &LatencyConditions) -> Option<String> {
        if self.input_id != now.input_id {
            return Some(format!(
                "the input changed to {}",
                display_name(&now.input_name, &now.input_id)
            ));
        }
        if self.output_id != now.output_id {
            return Some(format!(
                "the output changed to {}",
                display_name(&now.output_name, &now.output_id)
            ));
        }
        if self.input_rate != now.input_rate {
            return Some(format!(
                "the input rate changed from {} to {}",
                rate_label(self.input_rate),
                rate_label(now.input_rate)
            ));
        }
        if self.output_rate != now.output_rate {
            return Some(format!(
                "the output rate changed from {} to {}",
                rate_label(self.output_rate),
                rate_label(now.output_rate)
            ));
        }
        if self.input_buffer != now.input_buffer {
            return Some(format!(
                "the input block changed from {} to {} frames",
                self.input_buffer, now.input_buffer
            ));
        }
        if self.output_step != now.output_step {
            return Some(format!(
                "the output step changed from {} to {} frames",
                self.output_step, now.output_step
            ));
        }
        if self.output_mode != now.output_mode {
            return Some(format!(
                "the output mode changed from {} to {}",
                self.output_mode, now.output_mode
            ));
        }
        None
    }

    /// The measurement in the diagnostics/summary form: value, conditions,
    /// date.  `stale` is the reason from [`Self::stale_reason`].
    pub(crate) fn summary(&self, stale: Option<&str>) -> String {
        let mut text = format!(
            "{:.1} ms · {} smp · OUT → {} → IN · {} → {} · {} / {} · {} in, {} out · {}",
            self.latency_ms,
            self.samples,
            self.loopback.label(),
            display_name(&self.input_name, &self.input_id),
            display_name(&self.output_name, &self.output_id),
            rate_label(self.input_rate),
            rate_label(self.output_rate),
            self.input_buffer,
            self.output_step,
            self.output_mode
        );
        text.push_str(&format!(" · {}", utc_stamp(self.measured_at)));
        if let Some(reason) = stale {
            text.push_str(&format!(" · stale: {reason}"));
        }
        text
    }
}

/// What the rig is set to right now.  Built from the running snapshot when a
/// bridge runs, from the app's planned settings otherwise.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct LatencyConditions {
    pub(crate) input_id: String,
    pub(crate) input_name: String,
    pub(crate) output_id: String,
    pub(crate) output_name: String,
    pub(crate) input_rate: u32,
    pub(crate) output_rate: u32,
    pub(crate) input_buffer: u32,
    pub(crate) output_step: u32,
    pub(crate) output_mode: String,
}

impl LatencyConditions {
    /// Whether two readings describe the same measurement rig.  Display names
    /// are deliberately not compared: a renamed device is the same device.
    pub(crate) fn same_rig(&self, other: &Self) -> bool {
        self.input_id == other.input_id
            && self.output_id == other.output_id
            && self.input_rate == other.input_rate
            && self.output_rate == other.output_rate
            && self.input_buffer == other.input_buffer
            && self.output_step == other.output_step
            && self.output_mode == other.output_mode
    }
}

pub(crate) fn display_name(name: &str, id: &str) -> String {
    if name.trim().is_empty() {
        id.to_owned()
    } else {
        name.to_owned()
    }
}

pub(crate) fn rate_label(rate: u32) -> String {
    format!("{:.1} kHz", rate as f32 / 1000.0)
}

/// Formats Unix seconds as `YYYY-MM-DD HH:MM UTC`, without a date crate.
/// The civil-date arithmetic is Howard Hinnant's `civil_from_days`.
pub(crate) fn utc_stamp(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let seconds = unix_secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        seconds / 3_600,
        seconds % 3_600 / 60
    )
}

/// `days` since 1970-01-01 to a proleptic Gregorian date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (year + i64::from(month <= 2), month, day)
}

impl RigWerkApp {
    /// What the rig is set to now, for measuring and for staleness.
    pub(crate) fn latency_conditions(&self) -> LatencyConditions {
        let running = self.engine.state == EngineState::Running;
        let snapshot = &self.snapshot;
        let planned_output_rate = if self.output_rate > 0 {
            self.output_rate
        } else {
            self.sample_rate
        };
        LatencyConditions {
            input_id: self.active_input().id.clone(),
            input_name: self.active_input().name.clone(),
            output_id: self.selected_output().id.clone(),
            output_name: self.selected_output().name.clone(),
            input_rate: pick_live(running, snapshot.input_rate, self.sample_rate).max(1),
            output_rate: pick_live(running, snapshot.output_rate, planned_output_rate).max(1),
            input_buffer: pick_live(
                running,
                snapshot.input_block_frames,
                self.buffer_size as u32,
            ),
            output_step: pick_live(
                running,
                snapshot.output_step_frames,
                self.output_buffer as u32,
            ),
            output_mode: self.output_mode_key().to_owned(),
        }
    }

    /// A short, stable name for how the clock output opens, so a measurement
    /// made in one mode is not reused in another.  The long status line is
    /// for display; this is the comparison key.
    pub(crate) fn output_mode_key(&self) -> &'static str {
        if self.selected_output().asio_clsid.is_some() {
            return "asio";
        }
        if cfg!(target_os = "windows") {
            match self.wasapi_output_mode {
                crate::audio::WasapiOutputMode::Exclusive => "exclusive",
                crate::audio::WasapiOutputMode::RealStyle => "real-style",
                crate::audio::WasapiOutputMode::Shared => "shared",
            }
        } else if self.low_latency_output {
            "low-latency"
        } else {
            "standard"
        }
    }
}

/// The running reading, or the planned setting when the bridge is not (or
/// has not reported one yet).  Mirrors the choices in `latency_ms`.
fn pick_live(running: bool, live: u32, planned: u32) -> u32 {
    if running && live > 0 {
        live
    } else {
        planned
    }
}

#[cfg(test)]
mod tests;
