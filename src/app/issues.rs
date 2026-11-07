//! Live Issues: the judgments the Live page shows in one strip (see
//! `docs/mockups/live-issues-plan.md`).  Readings stay on the cards; a
//! problem becomes one row here, with a severity, where it is and one
//! action that only runs on a click.
//!
//! An error means the audio itself is about to break (a block nearly
//! late); anything that only colours the sound, such as a comb filter, is
//! a warning.
//!
//! Producers so far are the signal paths':
//! - unaligned paths that add up on one device channel (the Signal Routing
//!   `Sum` warning, `app::alignment`): a warning,
//! - a device playing a path whose chain latency is past what the
//!   alignment covers ([`MAX_ALIGN`]),
//! - an unmuted path that no unmuted device plays, also when it is the only
//!   path (a muted one is parked on purpose, e.g. right after Add path, and
//!   does not warn),
//! - the paths' DSP time not fitting the block (the graph quantum on the
//!   PipeWire duplex path, the output block otherwise), with hysteresis so
//!   the row does not flicker: the smoothed share raises the warning, the
//!   decaying peak share the error.  Both shares come from the mixer's
//!   total per block (`AudioEngine::total_dsp_time`), never from adding up
//!   the paths' peaks, which may be seconds apart; the per-path times only
//!   name the heaviest path.

use std::cell::Cell;

use crate::app::alignment::RoutingWarningKind;
use crate::app::RigWerkApp;
use crate::audio::MAX_ALIGN;

/// The smoothed DSP share of the block that raises the warning, and the
/// share it has to drop under to clear it.
pub(crate) const DSP_WARN_SHARE: f32 = 0.70;
pub(crate) const DSP_WARN_CLEAR: f32 = 0.60;
/// The peak DSP share that raises the error (the block is nearly late),
/// and the share it has to drop under to clear it.
pub(crate) const DSP_ERROR_SHARE: f32 = 0.95;
pub(crate) const DSP_ERROR_CLEAR: f32 = 0.85;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum IssueSeverity {
    Error,
    Warning,
}

/// The DSP overload latches, kept between frames for the hysteresis.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct DspAlarm {
    pub(crate) warning: bool,
    pub(crate) error: bool,
}

impl DspAlarm {
    /// The latches after one reading of the smoothed (`avg`) and peak
    /// share of the block.
    pub(crate) fn next(self, avg: f32, peak: f32) -> Self {
        Self {
            warning: if self.warning {
                avg >= DSP_WARN_CLEAR
            } else {
                avg >= DSP_WARN_SHARE
            },
            error: if self.error {
                peak >= DSP_ERROR_CLEAR
            } else {
                peak >= DSP_ERROR_SHARE
            },
        }
    }

    pub(crate) fn severity(self) -> Option<IssueSeverity> {
        if self.error {
            Some(IssueSeverity::Error)
        } else if self.warning {
            Some(IssueSeverity::Warning)
        } else {
            None
        }
    }
}

/// The issue state the strip keeps between frames (not persisted).
#[derive(Debug, Default)]
pub(crate) struct IssueState {
    pub(crate) dsp: Cell<DspAlarm>,
}

/// What an issue's button does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IssueAction {
    /// Turn the alignment on for output device `device`.
    AlignDevice(usize),
    /// Open Signal Routing with output device `device` selected.
    ShowRouting(usize),
    /// Open Signal Routing (no device in particular).
    OpenRouting,
    OpenDiagnostics,
}

impl IssueAction {
    pub(crate) fn label(self) -> &'static str {
        match self {
            IssueAction::AlignDevice(_) => "Align paths",
            IssueAction::ShowRouting(_) | IssueAction::OpenRouting => "Show in Routing",
            IssueAction::OpenDiagnostics => "Open diagnostics",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LiveIssue {
    pub(crate) severity: IssueSeverity,
    pub(crate) title: String,
    pub(crate) detail: String,
    /// Where it is, e.g. `ROUTING · Scarlett 2i2`.
    pub(crate) place: String,
    pub(crate) action: IssueAction,
}

impl LiveIssue {
    /// The button's accessible name: its label and the issue it acts on.
    pub(crate) fn action_name(&self) -> String {
        format!("{}: {}", self.action.label(), self.title)
    }
}

impl RigWerkApp {
    /// Every current issue, errors first.  Moves the DSP latches.
    pub(crate) fn live_issues(&self) -> Vec<LiveIssue> {
        let mut issues = Vec::new();
        self.unaligned_sum_issues(&mut issues);
        self.clamped_issues(&mut issues);
        self.unheard_path_issues(&mut issues);
        self.dsp_issue(&mut issues);
        issues.sort_by_key(|issue| issue.severity);
        issues
    }

    /// (a) Paths that add up unaligned on a device.
    fn unaligned_sum_issues(&self, issues: &mut Vec<LiveIssue>) {
        if self.path_count() < 2 {
            return;
        }
        for (device, row) in self.output_device_rows().iter().enumerate() {
            let Some(warning) = self
                .routing_warnings(device)
                .into_iter()
                .find(|warning| warning.kind == RoutingWarningKind::Sum)
            else {
                continue;
            };
            let names: Vec<String> = warning
                .paths
                .iter()
                .filter_map(|&slot| self.paths[slot].as_ref().map(|path| path.name.clone()))
                .collect();
            // Only the paths in the sum: others on the device do not comb.
            let arrivals: Vec<f32> = self
                .device_alignment(device)
                .map(|alignment| {
                    warning
                        .paths
                        .iter()
                        .filter_map(|&slot| alignment.arrival_ms[slot])
                        .collect()
                })
                .unwrap_or_default();
            let spread = arrivals.iter().copied().fold(f32::MIN, f32::max)
                - arrivals.iter().copied().fold(f32::MAX, f32::min);
            let spread = if arrivals.is_empty() { 0.0 } else { spread };
            let align = self.device_align(device);
            issues.push(LiveIssue {
                severity: IssueSeverity::Warning,
                title: format!("Paths add up unaligned on {}", row.name),
                detail: format!(
                    "{} meet on {} up to {spread:.1} ms apart: a comb filter (thin, hollow sound). {}",
                    names.join(" and "),
                    crate::ui::pages::routing::channel_list(&warning.channels),
                    if align {
                        "Their trims pull them apart."
                    } else {
                        "Alignment is off on this device."
                    }
                ),
                place: format!("ROUTING · {}", row.name),
                action: if align {
                    IssueAction::ShowRouting(device)
                } else {
                    IssueAction::AlignDevice(device)
                },
            });
        }
    }

    /// A device with alignment on that plays a path longer than the
    /// alignment covers: that path arrives late against the others.
    fn clamped_issues(&self, issues: &mut Vec<LiveIssue>) {
        if self.path_count() < 2 {
            return;
        }
        let infos = self.path_infos();
        let rate = self.engine.plugin_rate().max(1);
        for (device, row) in self.output_device_rows().iter().enumerate() {
            let Some(alignment) = self.device_alignment(device) else {
                continue;
            };
            if !alignment.align || !alignment.clamped || alignment.path_count() < 2 {
                continue;
            }
            let names: Vec<&str> = infos
                .iter()
                .filter(|info| {
                    alignment.per_path[info.slot].is_some() && info.latency_samples > MAX_ALIGN
                })
                .map(|info| info.name.as_str())
                .collect();
            issues.push(LiveIssue {
                severity: IssueSeverity::Warning,
                title: format!("{} is past the alignment limit", names.join(" and ")),
                detail: format!(
                    "Its chain is longer than the {:.0} ms ({MAX_ALIGN} frames) the alignment covers on {}, so it arrives late against the other paths.",
                    MAX_ALIGN as f32 * 1000.0 / rate as f32,
                    row.name
                ),
                place: format!("ROUTING · {}", row.name),
                action: IssueAction::ShowRouting(device),
            });
        }
    }

    /// (b) An unmuted path that no unmuted device plays: its chain runs
    /// for nothing.  A muted path is parked on purpose and does not warn.
    /// Also checked with one path: then nothing is heard at all.
    fn unheard_path_issues(&self, issues: &mut Vec<LiveIssue>) {
        let devices = self.device_settings();
        for info in self.path_infos() {
            if info.mute {
                continue;
            }
            let heard = devices.iter().enumerate().any(|(device, settings)| {
                !settings.mute && self.path_reaches_device(info.slot, device)
            });
            if heard {
                continue;
            }
            let (title, detail) = if info.routed {
                (
                    format!("{} is routed nowhere audible", info.name),
                    "Every device it plays on is muted.".to_owned(),
                )
            } else {
                (
                    format!("{} is routed nowhere", info.name),
                    "Its chain runs, but no device channel plays it. Route it, or mute it."
                        .to_owned(),
                )
            };
            issues.push(LiveIssue {
                severity: IssueSeverity::Warning,
                title,
                detail,
                place: format!("ROUTING · {}", info.name),
                action: IssueAction::OpenRouting,
            });
        }
    }

    /// (c) The paths' DSP time against the block they have to fit in: the
    /// mixer's total per block, so peaks of different blocks never add up.
    fn dsp_issue(&self, issues: &mut Vec<LiveIssue>) {
        let total = self.engine.total_dsp_time().unwrap_or_default();
        let block = total.block_ns as f32;
        let (avg_share, peak_share) = (total.avg_share(), total.peak_share());
        let mut heaviest: Option<(String, f32)> = None;
        let infos = self.path_infos();
        for info in &infos {
            let Some(time) = self.engine.path_dsp_time(info.slot) else {
                continue;
            };
            let most = time.avg_ns as f32;
            if heaviest.as_ref().is_none_or(|(_, top)| most > *top) {
                heaviest = Some((info.name.clone(), most));
            }
        }
        let alarm = self.issue_state.dsp.get().next(avg_share, peak_share);
        self.issue_state.dsp.set(alarm);
        let Some(severity) = alarm.severity() else {
            return;
        };
        let quantum = if self.snapshot.direct {
            "graph quantum"
        } else {
            "output block"
        };
        let (name, most) = heaviest.unwrap_or_default();
        let shares = format!(
            "{:.0}% of the {:.1} ms {quantum} on average, {:.0}% at peak",
            avg_share * 100.0,
            block / 1e6,
            peak_share * 100.0
        );
        let detail = if infos.len() > 1 {
            format!(
                "{shares}; {name} takes {:.2} ms. Remove an insert or raise the buffer.",
                most / 1e6
            )
        } else {
            format!("{shares}. Remove an insert or raise the buffer.")
        };
        issues.push(LiveIssue {
            severity,
            title: if infos.len() > 1 {
                format!("Paths overload the {quantum}")
            } else {
                format!("The chain overloads the {quantum}")
            },
            detail,
            place: format!("DSP · {name}"),
            action: IssueAction::OpenDiagnostics,
        });
    }

    /// Runs an issue's action (a click on its button).
    pub(crate) fn apply_issue_action(&mut self, action: IssueAction) {
        use crate::app::Page;
        match action {
            IssueAction::AlignDevice(device) => {
                self.set_device_align(device, true);
                let name = self
                    .output_device_rows()
                    .get(device)
                    .map_or_else(|| "Output device".to_owned(), |row| row.name.clone());
                self.push_log(
                    format!("{name}: alignment on, the paths line up again."),
                    crate::ui::theme::TEAL,
                );
            }
            IssueAction::ShowRouting(device) => {
                self.routing_device = device;
                self.page = Page::Routing;
            }
            IssueAction::OpenRouting => self.page = Page::Routing,
            IssueAction::OpenDiagnostics => self.page = Page::Diagnostics,
        }
    }
}

#[cfg(test)]
mod tests;
