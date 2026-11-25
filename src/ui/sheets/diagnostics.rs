//! Diagnostics: the activity log, scan notes, the bridge status line, the
//! signal paths' DSP time (per path, against the block it must fit), the
//! chunks the audio thread could not run a path's chain (it did not hold
//! the chain's runtime), the chunks it found an effect paused, and, in a
//! debug build, how often the audio callbacks allocated.
//!
//! The header pill still follows the engine state.  This page shows the
//! bridge's own status string beside that, and keeps scan notes and quantum
//! failures that the live status line replaces.

use crate::app::LogTone;
use std::path::{Path, PathBuf};

use eframe::egui;
use egui::{Color32, CornerRadius, RichText, Stroke};

use crate::app::{RigWerkApp, ACTIVITY_LOG_CAP};
use crate::ui::fonts::Face;
use crate::ui::theme::{font, palette, Palette, TypeRole};
use crate::ui::widgets::{engine_state_label, status_dot};

use crate::ui::widgets::OutlineButton;

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

/// The debug build's count of what the audio callbacks allocated.
#[cfg(debug_assertions)]
fn allocation_text(count: u64) -> String {
    format!("Audio-thread allocations and frees (debug build): {count}")
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

/// The skip (or pause) share as text.  A non-zero skip that rounds to nothing reads
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

/// Why the chain count and the effect counts differ; the card and the export
/// say the same thing.
const COUNTS_CAPTION: &str = "Chain counts are chunks the path tried to process. Effect counts cover only chunks the chain ran. An effect pauses while it restarts, and stays paused after a failed start.";

/// The line under a chain whose effects were never reached.  A bypassed
/// effect or one at mix 0 never runs its plugin, which is normal, and a
/// chain that never ran also keeps the effects out.
const NO_EFFECT_TRIED: &str = "(no effect ran — bypassed, at mix 0, or the chain never ran)";

/// Whether the path got chunks through its chain but none through an effect,
/// so nothing below the chain line explains what happened.  A poisoned chain
/// never processed its chunks, so it does not read as "no effect ran".  A
/// bypassed effect or one at mix 0 never runs its plugin either, which is
/// normal, so the line names those causes instead of
/// reading as a fault.
fn no_effect_ran(report: &crate::audio::PluginLockReport) -> bool {
    !report.chain_poisoned
        && report.chain.attempts > 0
        && report.effects.iter().all(|effect| effect.attempts == 0)
}

/// One path's chain line: its skip share, or why there is none.  A dead
/// chain (a panicking edit) is not contention, so its counters would read
/// as a misleading skip rate.
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

/// The log dot on the active palette.
fn log_mark(colours: &Palette, tone: LogTone) -> Color32 {
    match tone {
        LogTone::Muted => colours.text2,
        LogTone::Blue => colours.meter,
        LogTone::Amber => colours.warn,
        LogTone::Teal => colours.accent,
        LogTone::Green => colours.path[2],
        LogTone::Red => colours.error,
    }
}

/// A path's saved colour on the active palette.
fn path_mark(colours: &Palette, index: u8) -> Color32 {
    colours.path[index as usize % colours.path.len()]
}

fn sans(text: impl Into<String>, colour: Color32, role: TypeRole) -> RichText {
    RichText::new(text)
        .font(font(Face::Sans, role))
        .color(colour)
}

fn mono_line(text: impl Into<String>, colour: Color32, role: TypeRole) -> RichText {
    RichText::new(text)
        .font(font(Face::Mono, role))
        .color(colour)
}

fn kicker(text: &str, colours: &Palette) -> RichText {
    RichText::new(text)
        .font(font(Face::SansSemibold, TypeRole::GridHeader))
        .color(colours.text3)
}

/// A group on the sheet: strip fill, a line, 4 pt corners.
fn group(ui: &mut egui::Ui, width: f32, body: impl FnOnce(&mut egui::Ui)) {
    let colours = palette(ui.ctx());
    egui::Frame::new()
        .fill(colours.strip)
        .stroke(Stroke::new(1.0, colours.line))
        .corner_radius(CornerRadius::same(4))
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            // 12 pt margin and a 1 pt stroke on each side.
            ui.set_width((width - 26.0).max(0.0));
            body(ui);
        });
}

/// One latency-budget line: part, value, where the number comes from, and
/// what it covers.
fn budget_row(
    ui: &mut egui::Ui,
    label: &str,
    value: &str,
    value_color: Color32,
    source: &str,
    detail: &str,
) {
    let colours = palette(ui.ctx());
    ui.horizontal_top(|ui| {
        ui.add_sized(
            [150.0, 16.0],
            egui::Label::new(mono_line(label, colours.text, TypeRole::GridHeader)),
        );
        ui.add_sized(
            [86.0, 16.0],
            egui::Label::new(mono_line(value, value_color, TypeRole::GridHeader)),
        );
        ui.add_sized(
            [80.0, 16.0],
            egui::Label::new(mono_line(source, colours.text3, TypeRole::GridHeader)),
        );
        ui.add(egui::Label::new(sans(detail, colours.text3, TypeRole::GridHeader)).wrap());
    });
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
            Some(note) => lines.push(format!("Graph buffer: {note}")),
            None => lines.push("Graph buffer: none".to_owned()),
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
        #[cfg(debug_assertions)]
        lines.push(allocation_text(self.audio_thread_allocations()));
        lines.push("Plugin blocks (chain skipped, effect paused / attempted):".to_owned());
        let any_attempts = lock_reports.iter().any(|(_, report, _)| {
            // A poisoned chain has no attempts to show; it is still the only
            // thing the section has to say about that path.
            report.chain_poisoned
                || report.chain.attempts > 0
                || report.effects.iter().any(|effect| effect.attempts > 0)
        });
        if !any_attempts {
            lines.push("No plugin blocks yet.".to_owned());
        } else {
            for (info, report, views) in &lock_reports {
                let path_attempts = report.chain.attempts
                    + report
                        .effects
                        .iter()
                        .map(|effect| effect.attempts)
                        .sum::<u64>();
                // Reset clears the attempts but not the poison, so a poisoned
                // chain must stay visible with nothing counted.
                if path_attempts == 0 && !report.chain_poisoned {
                    continue;
                }
                lines.push(format!("- {}: {}", info.name, chain_lock_text(report)));
                if no_effect_ran(report) {
                    lines.push(format!("  - {NO_EFFECT_TRIED}"));
                }
                for (index, effect) in report.effects.iter().enumerate() {
                    if effect.attempts == 0 {
                        continue;
                    }
                    let name = views
                        .get(index)
                        .map(|view| view.name.clone())
                        .unwrap_or_else(|| format!("Effect {}", index + 1));
                    lines.push(format!(
                        "  - {name}: {} paused of {} ({})",
                        effect.skipped(),
                        chunk_text(effect.attempts),
                        percent_text(effect)
                    ));
                }
            }
            lines.push(COUNTS_CAPTION.to_owned());
        }
        lines.push("Scan notes:".to_owned());
        if self.devices.inventory.notes.is_empty() {
            lines.push("None.".to_owned());
        } else {
            for note in &self.devices.inventory.notes {
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

    fn diagnostics_tabs(&mut self, ui: &mut egui::Ui) {
        use crate::app::screen::DiagnosticsTab;
        let colours = palette(ui.ctx());
        let current = self.chrome.diagnostics_tab;
        ui.horizontal_wrapped(|ui| {
            for (label, tab) in [
                ("Log", DiagnosticsTab::Log),
                ("Performance", DiagnosticsTab::Performance),
                ("Latency", DiagnosticsTab::Latency),
                ("Scan", DiagnosticsTab::Scan),
                ("Status", DiagnosticsTab::Status),
            ] {
                let selected = current == tab;
                let text = sans(
                    label,
                    if selected {
                        colours.accent
                    } else {
                        colours.text2
                    },
                    TypeRole::Control,
                );
                if ui.selectable_label(selected, text).clicked() {
                    self.chrome.diagnostics_tab = tab;
                }
            }
        });
        ui.add_space(8.0);
    }

    pub(crate) fn diagnostics_page(&mut self, ui: &mut egui::Ui) {
        let width = ui.available_width();
        let colours = *palette(ui.ctx());
        self.diagnostics_tabs(ui);
        let tab = self.chrome.diagnostics_tab;
        if tab == crate::app::screen::DiagnosticsTab::Status {
            group(ui, width, |ui| {
                ui.label(kicker("BRIDGE", &colours));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(mono_line("ENGINE", colours.text3, TypeRole::GridHeader));
                    ui.label(sans(
                        engine_state_label(self.engine.state),
                        colours.text,
                        TypeRole::Control,
                    ));
                });
                ui.horizontal(|ui| {
                    ui.label(mono_line("STATUS", colours.text3, TypeRole::GridHeader));
                    ui.add(
                        egui::Label::new(sans(
                            self.bridge_status_line(),
                            colours.text,
                            TypeRole::Control,
                        ))
                        .wrap(),
                    );
                });
                if let Some(error) = self.visible_error() {
                    ui.horizontal(|ui| {
                        ui.label(mono_line("ERROR", colours.text3, TypeRole::GridHeader));
                        ui.add(
                            egui::Label::new(sans(
                                format!("Bridge error: {error}"),
                                colours.error_text,
                                TypeRole::Control,
                            ))
                            .wrap(),
                        );
                    });
                }
                let quantum = self.engine.quantum_note().map(str::to_owned);
                let (quantum_text, quantum_color) = match quantum.as_deref() {
                    Some(note) => (note, colours.warn),
                    None => ("No restore problem.", colours.text2),
                };
                ui.horizontal(|ui| {
                    ui.label(mono_line("GRAPH", colours.text3, TypeRole::GridHeader));
                    ui.add(
                        egui::Label::new(sans(quantum_text, quantum_color, TypeRole::Control))
                            .wrap(),
                    );
                });
            });
        }
        if tab == crate::app::screen::DiagnosticsTab::Latency {
            ui.add_space(10.0);
            group(ui, width, |ui| {
                ui.horizontal(|ui| {
                    ui.label(kicker(
                        "LATENCY BUDGET · ESTIMATE AND MEASUREMENT",
                        &colours,
                    ));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let measuring = self.calibration.is_measuring();
                        let blocker = self.calibration_blocker();
                        if OutlineButton::new("Measure loopback…").show(ui).clicked() {
                            self.calibration.open = true;
                        }
                        if measuring {
                            ui.label(mono_line("measuring", colours.warn, TypeRole::GridHeader))
                                .on_hover_text(
                                    "The clock output is muted while the click is captured.",
                                );
                        } else if let Some(reason) = blocker {
                            ui.label(mono_line(
                                "bridge needed",
                                colours.text3,
                                TypeRole::GridHeader,
                            ))
                            .on_hover_text(reason);
                        } else {
                            ui.label(mono_line(
                                "loopback ready",
                                colours.path[2],
                                TypeRole::GridHeader,
                            ));
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
                            crate::app::budget::LatencySource::Measured => colours.accent,
                            _ => colours.text2,
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
                    colours.warn,
                    "estimated",
                    budget.estimate_gap.as_deref().unwrap_or(
                        "Input block + bridge buffer + plugin chain + output step + after-node delay, at the rates they run at. The Live US readout is the bridge alone, so it differs from this by the chain and the after-node delay; alignment trims are not included.",
                    ),
                );
                ui.add_space(3.0);
                match &budget.measured {
                    Some(measured) => budget_row(
                        ui,
                        "MEASURED LOOPBACK",
                        &format!("{:.1} ms", measured.latency_ms),
                        if budget.stale.is_some() {
                            colours.warn
                        } else {
                            colours.accent
                        },
                        "measured",
                        &measured.summary(budget.stale.as_deref()),
                    ),
                    None => budget_row(
                        ui,
                        "MEASURED LOOPBACK",
                        "not measured",
                        colours.text2,
                        "—",
                        "Measure on Live with a cable or microphone loopback. The estimate above is not a measurement.",
                    ),
                }
                ui.add_space(4.0);
                ui.add(
                    egui::Label::new(sans(
                        crate::app::budget::LEGS_NOTE,
                        colours.text3,
                        TypeRole::Secondary,
                    ))
                    .wrap(),
                );
                ui.add_space(2.0);
                ui.label(sans(
                    "Reported means the driver, graph or a plugin said so; planned is the block and rate asked for before the driver or graph has reported the ones it runs at; setting is RigWerk's own bridge buffer; estimated is added here from those readings; measured is a click that went out through the loopback and came back.",
                    colours.text3,
                    TypeRole::Secondary,
                ));
            });
        }
        if tab == crate::app::screen::DiagnosticsTab::Performance {
            ui.add_space(10.0);
            group(ui, width, |ui| {
                ui.label(kicker("SIGNAL PATHS · DSP TIME", &colours));
                ui.add_space(6.0);
                egui::Grid::new("diagnostics_paths")
                    .num_columns(3)
                    .spacing([18.0, 4.0])
                    .show(ui, |ui| {
                        for header in ["PATH", "CHAIN LATENCY", "DSP PER BLOCK · AVG / PEAK"] {
                            ui.label(mono_line(header, colours.text3, TypeRole::GridHeader));
                        }
                        ui.end_row();
                        for (info, dsp) in self.path_dsp_lines() {
                            ui.horizontal(|ui| {
                                status_dot(ui, path_mark(&colours, info.colour_index));
                                ui.label(sans(&info.name, colours.text, TypeRole::Body));
                            });
                            ui.label(mono_line(
                                format!("{:.1} ms", info.latency_ms),
                                colours.text2,
                                TypeRole::GridHeader,
                            ));
                            let time = self.engine.path_dsp_time(info.slot).unwrap_or_default();
                            let color = if time.peak_share() >= crate::app::issues::DSP_ERROR_SHARE
                            {
                                colours.error_text
                            } else if time.avg_share() >= crate::app::issues::DSP_WARN_SHARE {
                                colours.warn
                            } else {
                                colours.text2
                            };
                            ui.label(mono_line(dsp, color, TypeRole::GridHeader));
                            ui.end_row();
                        }
                    });
                ui.add_space(4.0);
                ui.label(sans(
                    "Time the mixer spends on each path's chain per block (routing, EQs, gate, effects), smoothed and as a peak that decays over about two seconds, against the block's length: the graph buffer on the PipeWire duplex path, the output block otherwise.",
                    colours.text3,
                    TypeRole::Secondary,
                ));
                #[cfg(debug_assertions)]
                {
                    let allocations = self.audio_thread_allocations();
                    ui.add_space(6.0);
                    ui.label(mono_line(
                        allocation_text(allocations),
                        if allocations > 0 {
                            colours.warn
                        } else {
                            colours.text2
                        },
                        TypeRole::GridHeader,
                    ));
                }
            });

            ui.add_space(10.0);
            group(ui, width, |ui| {
                ui.label(kicker(
                    "PLUGIN BLOCKS · SKIPPED OR PAUSED / ATTEMPTED",
                    &colours,
                ));
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
                        || report.effects.iter().any(|effect| effect.attempts > 0)
                });
                if !any_attempts {
                    ui.label(sans("No plugin blocks yet.", colours.text2, TypeRole::Body));
                } else {
                    for (info, report) in &reports {
                        let path_attempts = report.chain.attempts
                            + report
                                .effects
                                .iter()
                                .map(|effect| effect.attempts)
                                .sum::<u64>();
                        if path_attempts == 0 && !report.chain_poisoned {
                            continue;
                        }
                        ui.horizontal(|ui| {
                            status_dot(ui, path_mark(&colours, info.colour_index));
                            ui.label(sans(&info.name, colours.text, TypeRole::Body));
                        });
                        ui.horizontal(|ui| {
                            ui.add_space(16.0);
                            ui.label(mono_line(
                                chain_lock_text(report),
                                if report.chain_poisoned || report.chain.skipped() > 0 {
                                    colours.warn
                                } else {
                                    colours.text2
                                },
                                TypeRole::GridHeader,
                            ));
                        });
                        let views = self.engine.plugin_views(info.slot);
                        if no_effect_ran(report) {
                            ui.horizontal(|ui| {
                                ui.add_space(16.0);
                                ui.label(mono_line(
                                    NO_EFFECT_TRIED,
                                    colours.text2,
                                    TypeRole::GridHeader,
                                ));
                            });
                        }
                        for (index, effect) in report.effects.iter().enumerate() {
                            if effect.attempts == 0 {
                                continue;
                            }
                            let name = views
                                .get(index)
                                .map(|view| view.name.clone())
                                .unwrap_or_else(|| format!("Effect {}", index + 1));
                            ui.horizontal(|ui| {
                                ui.add_space(16.0);
                                ui.label(mono_line(
                                    format!(
                                        "{name}: {} paused of {} ({})",
                                        effect.skipped(),
                                        chunk_text(effect.attempts),
                                        percent_text(effect)
                                    ),
                                    if effect.skipped() > 0 {
                                        colours.warn
                                    } else {
                                        colours.text2
                                    },
                                    TypeRole::GridHeader,
                                ));
                            });
                        }
                    }
                    ui.add_space(6.0);
                    ui.label(sans(COUNTS_CAPTION, colours.text3, TypeRole::Secondary));
                }
                ui.add_space(6.0);
                if OutlineButton::new("Reset all").show(ui).clicked() {
                    self.engine.reset_plugin_lock_counters();
                    self.push_log("Plugin block counters reset.", LogTone::Teal);
                }
            });
        }
        if tab == crate::app::screen::DiagnosticsTab::Scan {
            ui.add_space(10.0);
            group(ui, width, |ui| {
                ui.label(kicker("SCAN NOTES", &colours));
                ui.add_space(6.0);
                if self.devices.inventory.notes.is_empty() {
                    ui.label(sans("No scan notes.", colours.text2, TypeRole::Body));
                } else {
                    for note in &self.devices.inventory.notes {
                        ui.add(
                            egui::Label::new(sans(note, colours.warn, TypeRole::Control)).wrap(),
                        );
                    }
                }
            });
        }
        if tab == crate::app::screen::DiagnosticsTab::Log {
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if OutlineButton::new("Copy").show(ui).clicked() {
                    let report = self.diagnostics_text();
                    ui.ctx().copy_text(report);
                    self.push_log("Diagnostics copied to the clipboard.", LogTone::Teal);
                }
                if OutlineButton::new("Export").show(ui).clicked() {
                    let path = diagnostics_export_path();
                    match self.write_diagnostics(&path) {
                        Ok(()) => self.push_log(
                            format!("Diagnostics exported to {}.", path.display()),
                            LogTone::Teal,
                        ),
                        Err(error) => self.push_log(error, LogTone::Red),
                    }
                }
                ui.label(mono_line(
                    format!("{} / {ACTIVITY_LOG_CAP}", self.log.len()),
                    colours.text3,
                    TypeRole::Secondary,
                ));
            });
            ui.label(sans(
                "Newest first. Copy and export list the log oldest first.",
                colours.text3,
                TypeRole::Secondary,
            ));

            ui.add_space(10.0);
            group(ui, width, |ui| {
                ui.label(kicker("ACTIVITY", &colours));
                ui.add_space(6.0);
                if self.log.is_empty() {
                    ui.label(sans("No activity yet.", colours.text2, TypeRole::Body));
                } else {
                    for entry in self.log.iter().rev() {
                        ui.horizontal(|ui| {
                            ui.label(mono_line(&entry.at, colours.text3, TypeRole::GridHeader));
                            status_dot(ui, log_mark(&colours, entry.tone));
                            ui.add(
                                egui::Label::new(sans(&entry.text, colours.text2, TypeRole::Body))
                                    .wrap(),
                            );
                        });
                    }
                }
            });
        }
    }
}

#[cfg(test)]
mod tests;
