//! Diagnostics: the activity log, scan notes and the bridge status line.
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
mod tests {
    use std::path::PathBuf;

    use crate::app::blank_app;
    use crate::ui::theme::MUTED;

    #[test]
    fn the_report_includes_status_notes_and_the_log() {
        let mut app = blank_app();
        app.inventory
            .notes
            .push("ASIO registry scan: boom".to_owned());
        app.push_log("Bridge ready.", MUTED);
        app.engine.note_quantum_failure(
            "PipeWire quantum was not restored: pw-metadata not available".to_owned(),
        );
        app.engine.shared.set_status("Bridge stopped");

        let text = app.diagnostics_text();
        assert!(text.contains("Bridge status: Bridge stopped"), "{text}");
        assert!(
            text.contains("PipeWire quantum was not restored: pw-metadata not available"),
            "{text}"
        );
        assert!(text.contains("ASIO registry scan: boom"), "{text}");
        assert!(text.contains("Bridge ready."), "{text}");
        assert!(text.contains("cap 400"), "{text}");

        let path: PathBuf = std::env::temp_dir().join(format!(
            "rigwerk-diagnostics-test-{}-report.txt",
            std::process::id()
        ));
        app.write_diagnostics(&path).expect("export");
        let written = std::fs::read_to_string(&path).expect("read export");
        assert_eq!(written, text);
        let _ = std::fs::remove_file(&path);
    }
}
