//! The metronome control and its menu.

use eframe::egui;
use egui::{CornerRadius, RichText, Stroke};

use crate::app::RigWerkApp;
use crate::audio::{
    CLICK_LEVEL_MAX_DB, CLICK_LEVEL_MIN_DB, CLICK_RHYTHMS, METRONOME_BPM_MAX, METRONOME_BPM_MIN,
};
use crate::ui::theme::{font, palette, Face, TypeRole};
use crate::ui::widgets::{format_db, styled_slider, SliderStyle};

impl RigWerkApp {
    /// Tempo and rhythm.  A click starts or stops; the arrow opens the menu.
    pub(crate) fn metronome_control(&mut self, ui: &mut egui::Ui) {
        let colours = palette(ui.ctx());
        let running = self.metronome_running;
        let beat = self.snapshot.metronome_beat;
        let label = if running && beat > 0 {
            format!(
                "{:.0} {} · {beat}",
                self.metronome_bpm,
                self.metronome_rhythm.label()
            )
        } else {
            format!(
                "{:.0} {}",
                self.metronome_bpm,
                self.metronome_rhythm.label()
            )
        };
        let beat_colour = if running && beat == 1 {
            colours.accent_text
        } else if running {
            colours.text
        } else {
            colours.text2
        };
        let button = ui
            .add(
                egui::Button::new(
                    RichText::new(label)
                        .font(font(Face::Sans, TypeRole::Control))
                        .color(beat_colour),
                )
                .fill(if running {
                    colours.accent_bg
                } else {
                    egui::Color32::TRANSPARENT
                })
                .stroke(Stroke::new(1.0, colours.line2))
                .corner_radius(CornerRadius::same(4))
                .min_size(egui::vec2(32.0, 32.0)),
            )
            .on_hover_text(if running {
                "Stop the metronome. It keeps playing when the rig is muted."
            } else {
                "Start the metronome."
            });
        if button.clicked() {
            self.set_metronome_running(!running);
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
            .on_hover_text("Tempo, rhythm and level");
        egui::Popup::menu(&arrow)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
                ui.set_min_width(240.0);
                self.metronome_menu(ui);
            });
    }

    fn metronome_menu(&mut self, ui: &mut egui::Ui) {
        let colours = palette(ui.ctx());
        let mut bpm = self.metronome_bpm;
        ui.label(
            RichText::new("Tempo")
                .font(font(Face::Sans, TypeRole::Secondary))
                .color(colours.text3),
        );
        let response = styled_slider(
            ui,
            &mut bpm,
            METRONOME_BPM_MIN,
            METRONOME_BPM_MAX,
            0.0,
            SliderStyle::named(colours.accent, "Tempo")
                .format(|value| {
                    if value.fract().abs() < 0.05 {
                        format!("{value:.0} BPM")
                    } else {
                        format!("{value:.1} BPM")
                    }
                })
                .unit(" BPM")
                .width(200.0)
                .editable(),
        );
        if response.changed() {
            self.set_metronome_bpm(bpm);
        }
        ui.label(
            RichText::new("Rhythm")
                .font(font(Face::Sans, TypeRole::Secondary))
                .color(colours.text3),
        );
        ui.horizontal_wrapped(|ui| {
            for rhythm in CLICK_RHYTHMS {
                let on = self.metronome_rhythm == rhythm;
                if ui
                    .selectable_label(on, rhythm.label())
                    .on_hover_text(format!("{} rhythm", rhythm.label()))
                    .clicked()
                {
                    self.set_metronome_rhythm(rhythm);
                }
            }
        });
        let mut level = self.metronome_level_db;
        ui.label(
            RichText::new("Level")
                .font(font(Face::Sans, TypeRole::Secondary))
                .color(colours.text3),
        );
        let response = styled_slider(
            ui,
            &mut level,
            CLICK_LEVEL_MIN_DB,
            CLICK_LEVEL_MAX_DB,
            0.0,
            SliderStyle::named(colours.accent, "Click level")
                .format(|value| format_db(value, 1))
                .unit(" dB")
                .width(200.0)
                .editable(),
        );
        if response.changed() {
            self.set_metronome_level_db(level);
        }
    }
}

#[cfg(test)]
mod tests;
