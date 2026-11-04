//! Mixer: the master chain in the order the sound flows (input → pre EQ →
//! plugin inserts → post EQ → output) and the EQ editor below it.  Live
//! Bridge only shows a one-line summary of this page.

pub(crate) mod eq_card;
mod mixer_card;

use eframe::egui;

use crate::app::RigWerkApp;

impl RigWerkApp {
    pub(crate) fn mixer_page(&mut self, ui: &mut egui::Ui) {
        let total = ui.available_width();
        self.mixer_card(ui, total);
        ui.add_space(14.0);
        self.eq_card(ui, total);
    }
}
