//! Start, theme, size, effects and the recordings folder.

use eframe::egui;
use egui::RichText;

use crate::app::appearance::ThemeChoice;
use crate::app::settings::EffectOpen;
use crate::app::RigWerkApp;
use crate::ui::fonts::Face;
use crate::ui::pages::setup::{hint, panel};
use crate::ui::theme::{font, palette, TypeRole};
use crate::ui::widgets::{segmented, styled_slider, SliderStyle, TextField};

impl RigWerkApp {
    pub(super) fn setup_start(&mut self, ui: &mut egui::Ui) {
        panel(ui, "Start", None, None, |ui| {
            let colours = palette(ui.ctx());
            let mut start = self.auto_start_bridge;
            if ui
                .checkbox(
                    &mut start,
                    RichText::new("Start the bridge when RigWerk opens")
                        .font(font(Face::Sans, TypeRole::Body))
                        .color(colours.text),
                )
                .changed()
            {
                self.auto_start_bridge = start;
            }
        });
    }

    pub(super) fn setup_appearance(&mut self, ui: &mut egui::Ui) {
        panel(ui, "Appearance", None, None, |ui| {
            let colours = palette(ui.ctx());
            hint(ui, "Theme");
            ui.add_space(4.0);
            let mut theme = match self.theme {
                ThemeChoice::System => 0,
                ThemeChoice::Dark => 1,
                ThemeChoice::Light => 2,
            };
            if segmented(ui, &mut theme, &["System", "Dark", "Light"]).changed() {
                let choice = match theme {
                    1 => ThemeChoice::Dark,
                    2 => ThemeChoice::Light,
                    _ => ThemeChoice::System,
                };
                self.set_theme(ui.ctx(), choice);
            }
            hint(
                ui,
                "The window follows this choice. System uses what the desktop reports, and dark when it reports nothing.",
            );
            ui.add_space(8.0);
            hint(ui, "Size");
            ui.add_space(4.0);
            let mut percent = (self.ui_size * 100.0).round();
            let response = styled_slider(
                ui,
                &mut percent,
                80.0,
                150.0,
                0.0,
                SliderStyle::named(colours.accent, "Size")
                    .format(|value| {
                        if value.fract().abs() < 0.05 {
                            format!("{value:.0} %")
                        } else {
                            format!("{value:.1} %")
                        }
                    })
                    .unit(" %")
                    .editable(),
            );
            if response.changed() {
                self.set_ui_size(ui.ctx(), percent / 100.0);
            }
        });
    }

    pub(super) fn setup_effects(&mut self, ui: &mut egui::Ui) {
        panel(ui, "Effects", None, None, |ui| {
            hint(ui, "Double-click an effect opens");
            ui.add_space(4.0);
            let mut index = match self.effect_double_click {
                EffectOpen::Editor => 0,
                EffectOpen::Controls => 1,
            };
            if segmented(ui, &mut index, &["Editor", "Controls"]).changed() {
                self.effect_double_click = if index == 0 {
                    EffectOpen::Editor
                } else {
                    EffectOpen::Controls
                };
            }
            ui.add_space(8.0);
            hint(ui, "Plugin folders");
            for folder in crate::app::plugins::plugin_scan_folders() {
                hint(ui, &folder);
            }
            ui.add_space(6.0);
            let colours = palette(ui.ctx());
            let scanning = self.plugin_browser.scanning;
            let label = if scanning {
                "Scanning…"
            } else {
                "Rescan plugins"
            };
            if ui
                .add_enabled(
                    !scanning,
                    egui::Button::new(
                        RichText::new(label)
                            .font(font(Face::Sans, TypeRole::Control))
                            .color(colours.text),
                    ),
                )
                .clicked()
            {
                self.begin_plugin_scan();
            }
        });
    }

    pub(super) fn setup_recordings(&mut self, ui: &mut egui::Ui) {
        panel(ui, "Recordings", None, None, |ui| {
            hint(ui, "Folder");
            ui.add_space(4.0);
            let mut folder = self.record_folder.clone();
            if TextField::new(&mut folder, "Recordings folder")
                .show(ui)
                .changed()
            {
                self.set_record_folder(folder);
            }
        });
    }
}

#[cfg(test)]
mod tests;
