//! Mixer: vertical signal chain and an inspector for the selected stage.
//! It edits the selected signal path; with several paths, chips in the
//! toolbar pick the path, and its chain ends in a Path stage before the
//! shared Output.
//! Live Bridge shows a one-line summary of this page.

pub(crate) mod chain;
pub(crate) mod eq_card;
mod gate_card;
pub(crate) mod output_devices;
mod path_stage;
mod rack;

use eframe::egui;

use crate::app::RigWerkApp;

impl RigWerkApp {
    pub(crate) fn mixer_page(&mut self, ui: &mut egui::Ui) {
        self.mixer_rack(ui);
    }
}
