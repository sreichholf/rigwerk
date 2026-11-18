//! Setup: devices on the left, time and preferences on the right.
//!
//! One scrolling screen.  *Done* returns to Live, which is home until the
//! console exists.  The page draws and calls `RigWerkApp`; it does not
//! read `engine.shared`.

mod appearance;
mod clock;
mod devices;
mod help;
mod latency;

use eframe::egui;
use egui::{Align, CornerRadius, Layout, Margin, RichText, Sense, Stroke, Vec2};

use crate::app::screen::SetupSection;
use crate::app::RigWerkApp;
use crate::ui::fonts::Face;
use crate::ui::theme::{font, palette, TypeRole};

impl RigWerkApp {
    pub(crate) fn setup_page(&mut self, ui: &mut egui::Ui) {
        let colours = palette(ui.ctx());
        let section = self.chrome.setup_section;
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("Setup")
                    .font(font(Face::Sans, TypeRole::Heading))
                    .color(colours.text)
                    .strong(),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .button(
                        RichText::new("Done")
                            .font(font(Face::Sans, TypeRole::Control))
                            .color(colours.accent_text),
                    )
                    .on_hover_text("Back to Live")
                    .clicked()
                {
                    self.close_setup();
                }
            });
        });
        ui.add_space(8.0);

        let gap = 16.0;
        let width = ((ui.available_width() - gap) / 2.0).max(240.0);
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            ui.allocate_ui_with_layout(Vec2::new(width, 0.0), Layout::top_down(Align::Min), |ui| {
                ui.set_min_width(width);
                ui.set_max_width(width);
                self.setup_devices(ui, section);
            });
            ui.allocate_ui_with_layout(Vec2::new(width, 0.0), Layout::top_down(Align::Min), |ui| {
                ui.set_min_width(width);
                ui.set_max_width(width);
                self.setup_latency(ui, section);
                self.setup_clock(ui, section);
                self.setup_start(ui);
                self.setup_appearance(ui);
                self.setup_effects(ui);
                self.setup_recordings(ui);
                self.setup_help(ui);
            });
        });
        self.chrome.setup_section = None;
    }
}

/// A setup panel.  `section` scrolls this panel into view when the status
/// bar asked for it.
pub(super) fn panel(
    ui: &mut egui::Ui,
    title: &str,
    wanted: Option<SetupSection>,
    section: Option<SetupSection>,
    body: impl FnOnce(&mut egui::Ui),
) {
    if wanted.is_some() && wanted == section {
        let response = ui.allocate_response(Vec2::ZERO, Sense::hover());
        response.scroll_to_me(Some(Align::TOP));
    }
    let colours = palette(ui.ctx());
    egui::Frame::new()
        .fill(colours.strip)
        .stroke(Stroke::new(1.0, colours.line))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                RichText::new(title)
                    .font(font(Face::Sans, TypeRole::Heading))
                    .color(colours.text),
            );
            ui.add_space(8.0);
            body(ui);
        });
    ui.add_space(12.0);
}

pub(super) fn hint(ui: &mut egui::Ui, text: &str) {
    let colours = palette(ui.ctx());
    ui.label(
        RichText::new(text)
            .font(font(Face::Sans, TypeRole::Secondary))
            .color(colours.text3),
    );
}
