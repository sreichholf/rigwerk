//! Start, theme, size, effects and the recordings folder.

use eframe::egui;

use crate::app::appearance::ThemeChoice;
use crate::app::settings::EffectOpen;
use crate::app::RigWerkApp;
use crate::ui::pages::setup::{hint, panel};
use crate::ui::widgets::segmented;

impl RigWerkApp {
    pub(super) fn setup_start(&mut self, ui: &mut egui::Ui) {
        panel(ui, "Start", None, None, |ui| {
            let mut start = self.auto_start_bridge;
            if ui
                .checkbox(&mut start, "Start the bridge when RigWerk opens")
                .changed()
            {
                self.auto_start_bridge = start;
            }
        });
    }

    pub(super) fn setup_appearance(&mut self, ui: &mut egui::Ui) {
        panel(ui, "Appearance", None, None, |ui| {
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
                "The saved theme is kept. The window stays dark until the console replaces the older pages.",
            );
            ui.add_space(8.0);
            hint(ui, "Size");
            ui.add_space(4.0);
            let mut percent = (self.ui_size * 100.0).round();
            if ui
                .add(egui::Slider::new(&mut percent, 80.0..=150.0).suffix(" %"))
                .changed()
            {
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
            let scanning = self.plugin_browser.scanning;
            let label = if scanning {
                "Scanning…"
            } else {
                "Rescan plugins"
            };
            if ui
                .add_enabled(!scanning, egui::Button::new(label))
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
            if ui.text_edit_singleline(&mut folder).changed() {
                self.set_record_folder(folder);
            }
        });
    }
}
