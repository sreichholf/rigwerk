//! Per hardware pair, not per session.  A change of device, rate, block
//! or output mode marks the result stale instead of reusing it.
//! [`MeasuredLatency::sanitized`] drops a stored value that cannot
//! describe a real path.

use serde::Deserialize;

use crate::app::settings::rate_label;
use crate::app::RigWerkApp;
use crate::audio::{OutputMode, MAX_ROUND_TRIP_MS};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LoopbackKind {
    /// The cleanest measurement: no room or microphone in the way.
    #[default]
    Cable,
    /// A rig with no return path.
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

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct MeasuredLatency {
    pub(crate) latency_ms: f32,
    /// At [`Self::input_rate`], the rate the click arrived at.
    pub(crate) samples: u32,
    pub(crate) input_id: String,
    pub(crate) input_name: String,
    pub(crate) output_id: String,
    pub(crate) output_name: String,
    /// What the bridge actually ran at, not what was asked for.
    pub(crate) input_rate: u32,
    /// What the bridge actually ran at, not what was asked for.
    pub(crate) output_rate: u32,
    pub(crate) input_buffer: u32,
    pub(crate) output_step: u32,
    /// The stored token, not the display label.
    pub(crate) output_mode: String,
    /// Unix seconds, UTC.
    pub(crate) measured_at: u64,
    /// Normalised correlation of the loopback click, 0..1.
    pub(crate) confidence: f32,
    pub(crate) loopback: LoopbackKind,
}

impl MeasuredLatency {
    /// A damaged entry is skipped, not fatal.
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

    /// Device identity is the id, not the display name: a renamed device
    /// measured the same path.
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

    pub(crate) fn summary(&self, stale: Option<&str>) -> String {
        let mut text = format!(
            "{:.1} ms · {} smp · OUT {} → {} → IN {} · {} / {} · {} in, {} out · {}",
            self.latency_ms,
            self.samples,
            display_name(&self.output_name, &self.output_id),
            self.loopback.label(),
            display_name(&self.input_name, &self.input_id),
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
    /// Display names are not compared: a renamed device is the same device.
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

/// Howard Hinnant's `civil_from_days`, proleptic Gregorian.
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
    /// From the running snapshot while a bridge runs, otherwise the
    /// planned settings.
    pub(crate) fn latency_conditions(&self) -> LatencyConditions {
        let parts = self.latency_parts();
        LatencyConditions {
            input_id: self.active_input().id.clone(),
            input_name: self.active_input().name.clone(),
            output_id: self.selected_output().id.clone(),
            output_name: self.selected_output().name.clone(),
            input_rate: parts.input_rate,
            output_rate: parts.output_rate,
            input_buffer: parts.input_block,
            output_step: parts.output_step,
            output_mode: self.output_mode_key().to_owned(),
        }
    }

    pub(crate) fn output_mode_key(&self) -> &'static str {
        output_mode_key(self.output_mode())
    }

    /// While the bridge runs, the mode the driver opened.  A refused
    /// exclusive or low-latency open is the fallback, not what was asked.
    pub(crate) fn output_mode(&self) -> OutputMode {
        if self.engine.state == crate::audio::EngineState::Running {
            if let Some(opened) = self.snapshot.opened_output_mode {
                return opened;
            }
        }
        self.requested_output_mode()
    }

    fn requested_output_mode(&self) -> OutputMode {
        if self.selected_output().asio_clsid.is_some() {
            return OutputMode::Asio;
        }
        let (low_latency, exclusive) = self.output_start_flags();
        if cfg!(target_os = "windows") {
            if exclusive {
                OutputMode::Exclusive
            } else if low_latency {
                OutputMode::RealStyle
            } else {
                OutputMode::Shared
            }
        } else if low_latency {
            OutputMode::LowLatency
        } else {
            OutputMode::Standard
        }
    }
}

/// Saved measurements hold these strings, so a token never changes when
/// the mode's label does.
pub(crate) fn output_mode_key(mode: OutputMode) -> &'static str {
    match mode {
        OutputMode::Standard => "standard",
        OutputMode::LowLatency => "low-latency",
        OutputMode::Shared => "shared",
        OutputMode::RealStyle => "real-style",
        OutputMode::Exclusive => "exclusive",
        OutputMode::Asio => "asio",
    }
}

pub(crate) fn output_mode_from_key(key: &str) -> Option<OutputMode> {
    Some(match key {
        "standard" => OutputMode::Standard,
        "low-latency" => OutputMode::LowLatency,
        "shared" => OutputMode::Shared,
        "real-style" => OutputMode::RealStyle,
        "exclusive" => OutputMode::Exclusive,
        "asio" => OutputMode::Asio,
        _ => return None,
    })
}

/// Display text.  The stored key stays [`output_mode_key`].
pub(crate) fn output_mode_phrase(mode: OutputMode) -> &'static str {
    match mode {
        OutputMode::Standard => "Standard",
        OutputMode::LowLatency => "Low latency",
        OutputMode::Shared => crate::audio::WasapiOutputMode::Shared.label(),
        OutputMode::RealStyle => crate::audio::WasapiOutputMode::RealStyle.label(),
        OutputMode::Exclusive => crate::audio::WasapiOutputMode::Exclusive.label(),
        OutputMode::Asio => "ASIO",
    }
}

#[cfg(test)]
mod tests;
