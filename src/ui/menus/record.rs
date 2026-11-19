//! The record control and its menu.

use eframe::egui;
use egui::{CornerRadius, RichText, Stroke};

use crate::app::record::{lost_samples_label, record_source};
use crate::app::RigWerkApp;
use crate::audio::{EngineState, RecordPhase, RecordSource};
use crate::ui::theme::{font, palette, Face, TypeRole};

impl RigWerkApp {
    /// *Rec*, the elapsed time, and the menu of what the take contains.
    pub(crate) fn record_control(&mut self, ui: &mut egui::Ui) {
        let colours = palette(ui.ctx());
        let status = self.engine.recording_status();
        let recording = matches!(
            status.phase,
            RecordPhase::Recording | RecordPhase::Arming | RecordPhase::Finishing
        );
        let running = self.engine.state == EngineState::Running || recording;
        let label = if recording {
            let elapsed = crate::app::record::format_elapsed(status.frames, status.rate);
            format!("Rec {elapsed}")
        } else {
            "Rec".to_owned()
        };
        let text = if recording {
            colours.error
        } else {
            colours.text
        };
        let button = ui
            .add_enabled(
                running,
                egui::Button::new(
                    RichText::new(label)
                        .font(font(Face::Sans, TypeRole::Control))
                        .color(text),
                )
                .fill(if recording {
                    colours.accent_bg
                } else {
                    egui::Color32::TRANSPARENT
                })
                .stroke(Stroke::new(1.0, colours.line2))
                .corner_radius(CornerRadius::same(4))
                .min_size(egui::vec2(32.0, 32.0)),
            )
            .on_hover_text(if recording {
                "Stop and close the take."
            } else if running {
                "Record what the clock output is playing."
            } else {
                "Start the bridge to record."
            });
        if button.clicked() {
            if recording {
                self.stop_recording();
            } else {
                self.start_recording();
            }
        }
        let arrow = ui
            .add(
                egui::Button::new(
                    RichText::new("▾")
                        .font(font(Face::Sans, TypeRole::Control))
                        .color(colours.text2),
                )
                .fill(egui::Color32::TRANSPARENT)
                .stroke(Stroke::NONE)
                .min_size(egui::vec2(22.0, 32.0)),
            )
            .on_hover_text("What the take records");
        egui::Popup::menu(&arrow)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
                ui.set_min_width(280.0);
                self.record_menu(ui, &status);
            });
    }

    fn record_menu(&mut self, ui: &mut egui::Ui, status: &crate::audio::RecordStatus) {
        let colours = palette(ui.ctx());
        ui.label(
            RichText::new(self.record_source_line())
                .font(font(Face::Sans, TypeRole::Secondary))
                .color(colours.text2),
        );
        if status.overruns > 0 {
            ui.label(
                RichText::new(lost_samples_label(status.overruns))
                    .font(font(Face::Mono, TypeRole::Secondary))
                    .color(colours.error_text),
            );
        }
        let mut with_click = self.record_click;
        if ui
            .checkbox(&mut with_click, "Record the metronome and test clicks")
            .on_hover_text("Applies right away, including during a take.")
            .changed()
        {
            self.set_record_click(with_click);
        }
        if let Some(path) = &status.path {
            ui.label(
                RichText::new(path)
                    .font(font(Face::Mono, TypeRole::Secondary))
                    .color(colours.text2),
            );
            if ui
                .button(
                    RichText::new("Copy path")
                        .font(font(Face::Sans, TypeRole::Control))
                        .color(colours.text),
                )
                .clicked()
            {
                ui.ctx().copy_text(path.clone());
            }
        }
    }

    fn record_source_line(&self) -> String {
        match record_source(&self.extra_outputs) {
            RecordSource::Clock => "Records the clock output.".to_owned(),
            RecordSource::ExtraOutput(index) => {
                let name = self
                    .extra_outputs
                    .get(index)
                    .map(|extra| {
                        if extra.name.is_empty() {
                            extra.id.clone()
                        } else {
                            extra.name.clone()
                        }
                    })
                    .unwrap_or_else(|| "virtual output".to_owned());
                format!("Records the virtual output ({name}).")
            }
        }
    }
}
