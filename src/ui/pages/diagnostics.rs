//! Diagnostics: the activity log, scan notes, the bridge status line, the
//! signal paths' DSP time (per path, against the block it must fit) and the
//! plugin locks the audio thread skipped.
//!
//! The header pill still follows the engine state. This page shows the
//! bridge's own status string beside that, and keeps scan notes and quantum
//! failures that the live status line replaces.

use std::path::{Path, PathBuf};

use eframe::egui;
use egui::RichText;

use crate::app::{RigWerkApp, ACTIVITY_LOG_CAP};
use crate::audio::EngineState;
use crate::ui::theme::ts;
use crate::ui::theme::{AMBER, BORDER, CARD, DIM, GREEN, MUTED, RED, TEAL, TEXT};
use crate::ui::widgets::{card, ghost_button, micro_label, mono, status_dot};

/// One path's DSP line: "0.21 ms avg · 0.40 ms peak of 10.7 ms · 2% / 4%",
/// or why there is none.
pub(crate) fn dsp_text(dsp: Option<crate::audio::DspTime>) -> String {
    match dsp {
        Some(time) if time.block_ns > 0 => format!(
            "{:.2} ms avg · {:.2} ms peak of {:.1} ms · {:.0}% / {:.0}%",
            time.avg_ns as f32 / 1e6,
            time.peak_ns as f32 / 1e6,
            time.block_ns as f32 / 1e6,
            time.avg_share() * 100.0,
            time.peak_share() * 100.0
        ),
        _ => "not running".to_owned(),
    }
}

/// "0 chunks", "1 chunk", "N chunks": the count with its unit, so a single
/// chunk never reads as a plural.
fn chunk_text(count: u64) -> String {
    if count == 1 {
        "1 chunk".to_owned()
    } else {
        format!("{count} chunks")
    }
}

/// The skip share as text.  A non-zero skip that rounds to nothing reads
/// "<1%", so a rare skip is not reported as none.
fn percent_text(stats: &crate::audio::LockStats) -> String {
    // Before any attempt `share` is 0, so the zero-skip case covers it.
    if stats.skipped() == 0 {
        return "0%".to_owned();
    }
    let percent = stats.share() * 100.0;
    if percent < 1.0 {
        "<1%".to_owned()
    } else {
        format!("{percent:.0}%")
    }
}

/// Why the chain count and the insert counts differ; the card and the export
/// say the same thing.
const COUNTS_CAPTION: &str = "Chain counts are chunks the path tried to process. Insert counts cover only chunks where the chain lock was free.";

/// The line under a chain whose inserts were never reached.  A bypassed
/// insert or one at mix 0 never tries its plugin lock, which is normal, and
/// a chain lock that was always busy also keeps the inserts out.
const NO_INSERT_TRIED: &str =
    "(no insert tried its lock — bypassed, at mix 0, or the chain never ran)";

/// Whether the path got chunks through its chain but none through an insert,
/// so nothing below the chain line explains what happened.  A poisoned chain
/// never processed its chunks, so it does not read as "no insert tried its
/// lock".  A bypassed insert or one at mix 0 never tries its plugin lock
/// either, which is normal, so the line names those causes instead of
/// reading as a fault.
fn no_insert_ran(report: &crate::audio::PluginLockReport) -> bool {
    !report.chain_poisoned
        && report.chain.attempts > 0
        && report.inserts.iter().all(|insert| insert.attempts == 0)
}

/// One path's chain lock line: its skip share, or why there is none.  A
/// poisoned chain is not contention, so its counters would read as a
/// misleading skip rate.
fn chain_lock_text(report: &crate::audio::PluginLockReport) -> String {
    if report.chain_poisoned {
        "chain unavailable after an internal error".to_owned()
    } else {
        format!(
            "chain: {} skipped of {} ({})",
            report.chain.skipped(),
            chunk_text(report.chain.attempts),
            percent_text(&report.chain)
        )
    }
}

/// One latency-budget line: part, value, where the number comes from, and
/// what it covers.
fn budget_row(
    ui: &mut egui::Ui,
    label: &str,
    value: &str,
    value_color: egui::Color32,
    source: &str,
    detail: &str,
) {
    ui.horizontal_top(|ui| {
        ui.add_sized([150.0, 14.0], egui::Label::new(mono(label, TEXT, 8.8)));
        ui.add_sized(
            [86.0, 14.0],
            egui::Label::new(mono(value, value_color, 9.5)),
        );
        ui.add_sized([64.0, 14.0], egui::Label::new(mono(source, DIM, 8.0)));
        ui.add(egui::Label::new(RichText::new(detail).size(ts(9.5)).color(DIM)).wrap());
    });
}

fn engine_state_label(state: EngineState) -> &'static str {
    match state {
        EngineState::Preview => "Preview active",
        EngineState::Running => "Bridge live",
        EngineState::Starting => "Linking",
        EngineState::Stopping => "Stopping",
        EngineState::Error => "Needs attention",
        EngineState::Idle => "Ready",
    }
}

fn diagnostics_export_path() -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let name = format!("rigwerk-diagnostics-{stamp}.txt");
    let dir = std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Downloads"))
        .or_else(|| {
            std::env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join("Downloads"))
        })
        .unwrap_or_else(std::env::temp_dir);
    dir.join(name)
}

impl RigWerkApp {
    fn bridge_status_line(&self) -> String {
        let status = self.engine.bridge_status();
        if status.is_empty() {
            "Waiting".to_owned()
        } else {
            status
        }
    }

    /// One line per path: name, chain latency, DSP time per block.
    fn path_dsp_lines(&self) -> Vec<(crate::app::paths::PathInfo, String)> {
        self.path_infos()
            .into_iter()
            .map(|info| {
                let dsp = dsp_text(self.engine.path_dsp_time(info.slot));
                (info, dsp)
            })
            .collect()
    }

    /// Plain-text report: oldest activity first, plus the live status and notes.
    pub(crate) fn diagnostics_text(&self) -> String {
        let mut lines = Vec::new();
        lines.push("RigWerk diagnostics".to_owned());
        lines.push(format!("Engine: {}", engine_state_label(self.engine.state)));
        lines.push(format!("Bridge status: {}", self.bridge_status_line()));
        if let Some(error) = self.visible_error() {
            lines.push(format!("Shown error: Bridge error: {error}"));
        }
        match self.engine.quantum_note() {
            Some(note) => lines.push(format!("Quantum: {note}")),
            None => lines.push("Quantum: none".to_owned()),
        }
        lines.extend(self.latency_budget().text_lines());
        lines.push("Signal paths (chain latency · DSP time per block):".to_owned());
        for (info, dsp) in self.path_dsp_lines() {
            lines.push(format!(
                "- {}: {:.1} ms · {dsp}{}",
                info.name,
                info.latency_ms,
                if info.mute { " · muted" } else { "" }
            ));
        }
        let lock_reports: Vec<_> = self
            .path_infos()
            .into_iter()
            .map(|info| {
                let report = self.engine.plugin_lock_report(info.slot);
                let views = self.engine.plugin_views(info.slot);
                (info, report, views)
            })
            .collect();
        lines.push("Plugin locks (skipped / attempted):".to_owned());
        let any_attempts = lock_reports.iter().any(|(_, report, _)| {
            // A poisoned chain has no attempts to show; it is still the only
            // thing the section has to say about that path.
            report.chain_poisoned
                || report.chain.attempts > 0
                || report.inserts.iter().any(|insert| insert.attempts > 0)
        });
        if !any_attempts {
            lines.push("No plugin blocks yet.".to_owned());
        } else {
            for (info, report, views) in &lock_reports {
                let path_attempts = report.chain.attempts
                    + report
                        .inserts
                        .iter()
                        .map(|insert| insert.attempts)
                        .sum::<u64>();
                // Reset clears the attempts but not the poison, so a poisoned
                // chain must stay visible with nothing counted.
                if path_attempts == 0 && !report.chain_poisoned {
                    continue;
                }
                lines.push(format!("- {}: {}", info.name, chain_lock_text(report)));
                if no_insert_ran(report) {
                    lines.push(format!("  - {NO_INSERT_TRIED}"));
                }
                for (index, insert) in report.inserts.iter().enumerate() {
                    if insert.attempts == 0 {
                        continue;
                    }
                    let name = views
                        .get(index)
                        .map(|view| view.name.clone())
                        .unwrap_or_else(|| format!("Insert {}", index + 1));
                    lines.push(format!(
                        "  - {name}: {} of {} ({})",
                        insert.skipped(),
                        chunk_text(insert.attempts),
                        percent_text(insert)
                    ));
                }
            }
            lines.push(COUNTS_CAPTION.to_owned());
        }
        lines.push("Scan notes:".to_owned());
        if self.inventory.notes.is_empty() {
            lines.push("None.".to_owned());
        } else {
            for note in &self.inventory.notes {
                lines.push(format!("- {note}"));
            }
        }
        lines.push(format!(
            "Activity log ({} entries, cap {ACTIVITY_LOG_CAP}):",
            self.log.len()
        ));
        if self.log.is_empty() {
            lines.push("None.".to_owned());
        } else {
            for entry in &self.log {
                lines.push(format!("{}  {}", entry.at, entry.text));
            }
        }
        lines.join("\n")
    }

    pub(crate) fn write_diagnostics(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| format!("Could not create {}: {error}", parent.display()))?;
            }
        }
        let text = self.diagnostics_text();
        std::fs::write(path, text)
            .map_err(|error| format!("Could not write {}: {error}", path.display()))
    }

    pub(crate) fn diagnostics_page(&mut self, ui: &mut egui::Ui) {
        let width = ui.available_width();
        card(ui, CARD, BORDER, |ui| {
            ui.set_width(width - 32.0);
            ui.label(micro_label("BRIDGE", DIM));
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(mono("ENGINE", DIM, 9.0));
                ui.label(
                    RichText::new(engine_state_label(self.engine.state))
                        .size(ts(12.0))
                        .color(TEXT),
                );
            });
            ui.horizontal(|ui| {
                ui.label(mono("STATUS", DIM, 9.0));
                ui.add(
                    egui::Label::new(
                        RichText::new(self.bridge_status_line())
                            .size(ts(12.0))
                            .color(TEXT),
                    )
                    .wrap(),
                );
            });
            if let Some(error) = self.visible_error() {
                ui.horizontal(|ui| {
                    ui.label(mono("ERROR", DIM, 9.0));
                    ui.add(
                        egui::Label::new(
                            RichText::new(format!("Bridge error: {error}"))
                                .size(ts(11.5))
                                .color(RED),
                        )
                        .wrap(),
                    );
                });
            }
            let quantum = self.engine.quantum_note().map(str::to_owned);
            let (quantum_text, quantum_color) = match quantum.as_deref() {
                Some(note) => (note, AMBER),
                None => ("No restore problem.", MUTED),
            };
            ui.horizontal(|ui| {
                ui.label(mono("GRAPH", DIM, 9.0));
                ui.add(
                    egui::Label::new(
                        RichText::new(quantum_text)
                            .size(ts(11.5))
                            .color(quantum_color),
                    )
                    .wrap(),
                );
            });
        });

        ui.add_space(10.0);
        card(ui, CARD, BORDER, |ui| {
            ui.set_width(width - 32.0);
            ui.horizontal(|ui| {
                ui.label(micro_label(
                    "LATENCY BUDGET · ESTIMATE AND MEASUREMENT",
                    DIM,
                ));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let measuring = self.calibration.is_measuring();
                    let blocker = self.calibration_blocker();
                    if ghost_button(ui, "Measure loopback…", 168.0) {
                        self.calibration.open = true;
                    }
                    if measuring {
                        ui.label(mono("measuring", AMBER, 8.5)).on_hover_text(
                            "The clock output is muted while the click is captured.",
                        );
                    } else if let Some(reason) = blocker {
                        ui.label(mono("bridge needed", DIM, 8.5))
                            .on_hover_text(reason);
                    } else {
                        ui.label(mono("loopback ready", GREEN, 8.5));
                    }
                });
            });
            ui.add_space(6.0);
            let budget = self.latency_budget();
            for part in &budget.parts {
                budget_row(
                    ui,
                    part.label,
                    &part.value_text(),
                    match part.source {
                        crate::app::budget::LatencySource::Measured => TEAL,
                        _ => MUTED,
                    },
                    part.source.label(),
                    &part.detail,
                );
            }
            ui.add_space(3.0);
            budget_row(
                ui,
                "= ESTIMATED PATH",
                &format!("{:.1} ms", budget.estimate_ms),
                AMBER,
                "estimated",
                budget.estimate_gap.as_deref().unwrap_or(
                    "Input block + cushion + plugin chain + output step + after-node delay, at the rates they run at. The Live US readout is the bridge alone, so it differs from this by the chain and the after-node delay; alignment trims are not included.",
                ),
            );
            ui.add_space(3.0);
            match &budget.measured {
                Some(measured) => budget_row(
                    ui,
                    "MEASURED LOOPBACK",
                    &format!("{:.1} ms", measured.latency_ms),
                    if budget.stale.is_some() { AMBER } else { TEAL },
                    "measured",
                    &measured.summary(budget.stale.as_deref()),
                ),
                None => budget_row(
                    ui,
                    "MEASURED LOOPBACK",
                    "not measured",
                    MUTED,
                    "—",
                    "Measure on Live with a cable or microphone loopback. The estimate above is not a measurement.",
                ),
            }
            ui.add_space(4.0);
            ui.add(
                egui::Label::new(
                    RichText::new(crate::app::budget::LEGS_NOTE)
                        .size(ts(10.0))
                        .color(DIM),
                )
                .wrap(),
            );
            ui.add_space(2.0);
            ui.label(
                RichText::new(
                    "Reported means the driver, graph or a plugin said so; planned is the size asked for before the driver has opened; setting is RigWerk's own cushion; estimated is added here from those readings; measured is a click that went out through the loopback and came back.",
                )
                .size(ts(10.0))
                .color(DIM),
            );
        });

        ui.add_space(10.0);
        card(ui, CARD, BORDER, |ui| {
            ui.set_width(width - 32.0);
            ui.label(micro_label("SIGNAL PATHS · DSP TIME", DIM));
            ui.add_space(6.0);
            egui::Grid::new("diagnostics_paths")
                .num_columns(3)
                .spacing([18.0, 4.0])
                .show(ui, |ui| {
                    for header in ["PATH", "CHAIN LATENCY", "DSP PER BLOCK · AVG / PEAK"] {
                        ui.label(mono(header, DIM, 8.5));
                    }
                    ui.end_row();
                    for (info, dsp) in self.path_dsp_lines() {
                        ui.horizontal(|ui| {
                            status_dot(ui, info.colour);
                            ui.label(RichText::new(&info.name).size(ts(11.0)).color(TEXT));
                        });
                        ui.label(mono(format!("{:.1} ms", info.latency_ms), MUTED, 9.5));
                        let time = self.engine.path_dsp_time(info.slot).unwrap_or_default();
                        let color = if time.peak_share() >= crate::app::issues::DSP_ERROR_SHARE {
                            RED
                        } else if time.avg_share() >= crate::app::issues::DSP_WARN_SHARE {
                            AMBER
                        } else {
                            MUTED
                        };
                        ui.label(mono(dsp, color, 9.5));
                        ui.end_row();
                    }
                });
            ui.add_space(4.0);
            ui.label(
                RichText::new(
                    "Time the mixer spends on each path's chain per block (routing, EQs, gate, inserts), smoothed and as a peak that decays over about two seconds, against the block's length: the graph quantum on the PipeWire duplex path, the output block otherwise.",
                )
                .size(ts(10.0))
                .color(DIM),
            );
        });

        ui.add_space(10.0);
        card(ui, CARD, BORDER, |ui| {
            ui.set_width(width - 32.0);
            ui.label(micro_label("PLUGIN LOCKS · SKIPPED / ATTEMPTED", DIM));
            ui.add_space(6.0);
            let reports: Vec<_> = self
                .path_infos()
                .into_iter()
                .map(|info| {
                    let report = self.engine.plugin_lock_report(info.slot);
                    (info, report)
                })
                .collect();
            let any_attempts = reports.iter().any(|(_, report)| {
                // See the export above: a poisoned chain outlives a reset.
                report.chain_poisoned
                    || report.chain.attempts > 0
                    || report.inserts.iter().any(|insert| insert.attempts > 0)
            });
            if !any_attempts {
                ui.label(
                    RichText::new("No plugin blocks yet.")
                        .size(ts(11.0))
                        .color(MUTED),
                );
            } else {
                for (info, report) in &reports {
                    let path_attempts = report.chain.attempts
                        + report
                            .inserts
                            .iter()
                            .map(|insert| insert.attempts)
                            .sum::<u64>();
                    if path_attempts == 0 && !report.chain_poisoned {
                        continue;
                    }
                    ui.horizontal(|ui| {
                        status_dot(ui, info.colour);
                        ui.label(RichText::new(&info.name).size(ts(11.0)).color(TEXT));
                    });
                    ui.horizontal(|ui| {
                        ui.add_space(16.0);
                        ui.label(mono(
                            chain_lock_text(report),
                            if report.chain_poisoned || report.chain.skipped() > 0 {
                                AMBER
                            } else {
                                MUTED
                            },
                            9.5,
                        ));
                    });
                    let views = self.engine.plugin_views(info.slot);
                    if no_insert_ran(report) {
                        ui.horizontal(|ui| {
                            ui.add_space(16.0);
                            ui.label(mono(NO_INSERT_TRIED, MUTED, 9.5));
                        });
                    }
                    for (index, insert) in report.inserts.iter().enumerate() {
                        if insert.attempts == 0 {
                            continue;
                        }
                        let name = views
                            .get(index)
                            .map(|view| view.name.clone())
                            .unwrap_or_else(|| format!("Insert {}", index + 1));
                        ui.horizontal(|ui| {
                            ui.add_space(16.0);
                            ui.label(mono(
                                format!(
                                    "{name}: {} of {} ({})",
                                    insert.skipped(),
                                    chunk_text(insert.attempts),
                                    percent_text(insert)
                                ),
                                if insert.skipped() > 0 { AMBER } else { MUTED },
                                9.5,
                            ));
                        });
                    }
                }
                ui.add_space(6.0);
                ui.label(RichText::new(COUNTS_CAPTION).size(ts(10.0)).color(DIM));
            }
            ui.add_space(6.0);
            if ghost_button(ui, "Reset all", 120.0) {
                self.engine.reset_plugin_lock_counters();
                self.push_log("Plugin lock counters reset.", TEAL);
            }
        });

        ui.add_space(10.0);
        card(ui, CARD, BORDER, |ui| {
            ui.set_width(width - 32.0);
            ui.label(micro_label("SCAN NOTES", DIM));
            ui.add_space(6.0);
            if self.inventory.notes.is_empty() {
                ui.label(RichText::new("No scan notes.").size(ts(11.0)).color(MUTED));
            } else {
                for note in &self.inventory.notes {
                    ui.add(
                        egui::Label::new(RichText::new(note).size(ts(11.5)).color(AMBER)).wrap(),
                    );
                }
            }
        });

        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ghost_button(ui, "Copy", 120.0) {
                let report = self.diagnostics_text();
                ui.ctx().copy_text(report);
                self.push_log("Diagnostics copied to the clipboard.", TEAL);
            }
            if ghost_button(ui, "Export", 120.0) {
                let path = diagnostics_export_path();
                match self.write_diagnostics(&path) {
                    Ok(()) => {
                        self.push_log(format!("Diagnostics exported to {}.", path.display()), TEAL)
                    }
                    Err(error) => self.push_log(error, RED),
                }
            }
            ui.label(mono(
                format!("{} / {ACTIVITY_LOG_CAP}", self.log.len()),
                DIM,
                10.0,
            ));
        });
        ui.label(
            RichText::new("Newest first. Copy and export list the log oldest first.")
                .size(ts(10.0))
                .color(DIM),
        );

        ui.add_space(10.0);
        card(ui, CARD, BORDER, |ui| {
            ui.set_width(width - 32.0);
            ui.label(micro_label("ACTIVITY", DIM));
            ui.add_space(6.0);
            if self.log.is_empty() {
                ui.label(
                    RichText::new("No activity yet.")
                        .size(ts(11.0))
                        .color(MUTED),
                );
            } else {
                for entry in self.log.iter().rev() {
                    ui.horizontal(|ui| {
                        ui.label(mono(&entry.at, DIM, 9.0));
                        status_dot(ui, entry.tone);
                        ui.add(
                            egui::Label::new(
                                RichText::new(&entry.text).size(ts(11.0)).color(MUTED),
                            )
                            .wrap(),
                        );
                    });
                }
            }
        });
    }
}

#[cfg(test)]
mod tests;
