//! The latency budget: every part of the path that is known, where its
//! number comes from, and the last measured loopback.
//!
//! Everything here is read from the live snapshot and the app's own settings;
//! nothing is invented.  A part is `Reported` (the driver, the graph or a
//! plugin says so), `Setting` (RigWerk's own cushion), `Derived` (added and
//! converted here with the buffer and rate the bridge really runs at) or
//! `Measured` (the loopback calibration played a click and heard it come
//! back).  The estimate is the sum of the reported and setting parts; it is
//! not a loopback, and every label on screen and in the diagnostics text says
//! so.
//!
//! The measured loopback and the estimate cover different legs of the path:
//! the measurement starts at the output callback and ends at the input
//! callback, while the estimate adds the input block, cushion, chain, output
//! step and after-node delay.  They are shown side by side with that
//! difference stated, never subtracted from each other.
//!
//! The stored measurement is deliberately tied to the conditions it was made
//! under (see `app::latency`): a different input, output, rate, buffer/step or
//! output mode marks it stale instead of silently reusing a number that no
//! longer describes this rig.

use crate::app::bridge::LatencyParts;
use crate::app::latency::{rate_label, MeasuredLatency};
use crate::app::RigWerkApp;

/// How the measured leg and the estimated path differ, in one sentence for
/// the Diagnostics card, the panel and the copied text.
pub(crate) const LEGS_NOTE: &str = "The measured loopback runs from the output callback through the converters, the cable or microphone and back to the input callback. The estimated path adds the input block, bridge cushion, selected chain, output step and any graph delay after RigWerk's node, and leaves out the converters and alignment trims. They cover different legs, so the measurement is not an error on the estimate.";

/// Where one budget number comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LatencySource {
    /// Played a click and heard it come back.
    Measured,
    /// The driver, the graph or a plugin reported it.
    Reported,
    /// RigWerk's own buffer choice, not a hardware reading.
    Setting,
    /// Added or converted here from reported parts.
    Derived,
}

impl LatencySource {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Measured => "measured",
            Self::Reported => "reported",
            Self::Setting => "setting",
            Self::Derived => "estimated",
        }
    }
}

/// One line of the budget.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BudgetPart {
    pub(crate) label: &'static str,
    pub(crate) detail: String,
    pub(crate) ms: Option<f32>,
    pub(crate) source: LatencySource,
}

impl BudgetPart {
    pub(crate) fn value_text(&self) -> String {
        match self.ms {
            Some(ms) => format!("{ms:.1} ms"),
            None => "unknown".to_owned(),
        }
    }
}

/// The whole breakdown, as the Diagnostics page, the Live card and the
/// diagnostics text draw it.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct LatencyBudget {
    pub(crate) parts: Vec<BudgetPart>,
    /// The sum of the reported and setting parts.  Not a loopback.
    pub(crate) estimate_ms: f32,
    /// The stored measurement, if any.
    pub(crate) measured: Option<MeasuredLatency>,
    /// Why the stored measurement no longer applies.
    pub(crate) stale: Option<String>,
}

impl LatencyBudget {
    /// One line per part, the estimate and the measurement, for Copy and
    /// Export.  The words "estimate" and "measured" are not interchangeable
    /// here on purpose.
    pub(crate) fn text_lines(&self) -> Vec<String> {
        let mut lines = vec!["Latency budget:".to_owned()];
        for part in &self.parts {
            lines.push(format!(
                "- {}: {} ({}) — {}",
                part.label,
                part.value_text(),
                part.source.label(),
                part.detail
            ));
        }
        lines.push(format!(
            "= Estimated path: {:.1} ms ({}; includes the chain and the after-node delay, so it differs from the Live US, which is the bridge alone)",
            self.estimate_ms,
            LatencySource::Derived.label()
        ));
        match &self.measured {
            Some(measured) => lines.push(format!(
                "- Measured loopback: {} ({})",
                measured.summary(self.stale.as_deref()),
                LatencySource::Measured.label()
            )),
            None => lines.push(
                "- Measured loopback: not measured yet (run the loopback calibration on Live)"
                    .to_owned(),
            ),
        }
        lines.push(format!("  {LEGS_NOTE}"));
        lines
    }
}

impl RigWerkApp {
    /// The breakdown Diagnostics and Copy/Export show.
    pub(crate) fn latency_budget(&self) -> LatencyBudget {
        let parts_of_bridge = self.latency_parts();
        let mut parts = Vec::new();
        parts.push(input_block_part(&parts_of_bridge));
        parts.push(cushion_part(&parts_of_bridge));
        let chain = self.path_infos().into_iter().find(|info| info.selected);
        parts.push(BudgetPart {
            label: "PLUGIN CHAIN",
            detail: match &chain {
                Some(info) if info.latency_samples > 0 => format!(
                    "{}: {} samples at {}",
                    info.name,
                    info.latency_samples,
                    rate_label(self.engine.plugin_rate().max(1))
                ),
                Some(info) => format!("{}: no insert reports latency", info.name),
                None => "no selected path".to_owned(),
            },
            ms: chain.as_ref().map(|info| info.latency_ms),
            source: LatencySource::Reported,
        });
        parts.push(BudgetPart {
            label: "OUTPUT STEP",
            detail: format!(
                "{} frames at {}{}",
                parts_of_bridge.output_step,
                rate_label(parts_of_bridge.output_rate),
                planned_suffix(parts_of_bridge.running)
            ),
            ms: Some(frames_ms(
                parts_of_bridge.output_step,
                parts_of_bridge.output_rate,
            )),
            source: LatencySource::Reported,
        });
        parts.push(BudgetPart {
            label: "AFTER PLAYBACK NODE",
            detail: match (parts_of_bridge.running, self.snapshot.path_delay_ms) {
                (true, Some(ms)) => format!(
                    "{ms:.1} ms reported by the graph (filters and the device after RigWerk's node)"
                ),
                _ => {
                    "unknown on this output (PipeWire reports it; other backends do not)".to_owned()
                }
            },
            ms: self
                .snapshot
                .path_delay_ms
                .filter(|_| parts_of_bridge.running),
            source: LatencySource::Reported,
        });
        let estimate_ms: f32 = parts.iter().filter_map(|part| part.ms).sum();
        let stale = self
            .measured_latency
            .as_ref()
            .and_then(|measured| measured.stale_reason(&self.latency_conditions()));
        LatencyBudget {
            parts,
            estimate_ms,
            measured: self.measured_latency.clone(),
            stale,
        }
    }
}

fn planned_suffix(running: bool) -> &'static str {
    if running {
        ""
    } else {
        " (planned)"
    }
}

fn input_block_part(parts: &LatencyParts) -> BudgetPart {
    BudgetPart {
        label: "INPUT BLOCK",
        detail: format!(
            "{} frames at {}{}",
            parts.input_block,
            rate_label(parts.input_rate),
            planned_suffix(parts.running)
        ),
        ms: Some(frames_ms(parts.input_block, parts.input_rate)),
        source: LatencySource::Reported,
    }
}

fn cushion_part(parts: &LatencyParts) -> BudgetPart {
    BudgetPart {
        label: "BRIDGE CUSHION",
        detail: if parts.direct {
            "none: the duplex node runs both sides in one graph cycle".to_owned()
        } else {
            format!(
                "{} frames at {} · RigWerk's own fill target, not a hardware reading{}",
                parts.cushion,
                rate_label(parts.input_rate),
                planned_suffix(parts.running)
            )
        },
        ms: Some(frames_ms(parts.cushion, parts.input_rate)),
        source: LatencySource::Setting,
    }
}

fn frames_ms(frames: u32, rate: u32) -> f32 {
    frames as f32 / rate.max(1) as f32 * 1000.0
}

#[cfg(test)]
mod tests;
