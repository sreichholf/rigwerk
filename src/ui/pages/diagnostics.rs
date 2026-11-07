//! Diagnostics: the activity log, scan notes, the bridge status line and
//! the signal paths' DSP time (per path, against the block it must fit).
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
use crate::ui::theme::{AMBER, BORDER, CARD, DIM, MUTED, RED, TEAL, TEXT};
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
        lines.push("Signal paths (chain latency · DSP time per block):".to_owned());
        for (info, dsp) in self.path_dsp_lines() {
            lines.push(format!(
                "- {}: {:.1} ms · {dsp}{}",
                info.name,
                info.latency_ms,
                if info.mute { " · muted" } else { "" }
            ));
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
