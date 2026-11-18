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
use egui::{Align, CornerRadius, Layout, Margin, RichText, Stroke, Vec2};

use crate::app::RigWerkApp;
use crate::ui::fonts::Face;
use crate::ui::theme::{font, palette, TypeRole};
use crate::ui::widgets::row_info;

impl RigWerkApp {
    pub(crate) fn mixer_page(&mut self, ui: &mut egui::Ui) {
        self.open_browser_for_screenshot();
        // The page is already inside the central panel, so a side panel
        // cannot open here.  Until the drawer host lands, *+ Add effect*
        // shows the browser as a column on the right.
        self.open_browser_for_screenshot();
        if !self.plugin_browser.open {
            self.mixer_rack(ui);
            return;
        }
        if ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            self.plugin_browser.open = false;
            self.mixer_rack(ui);
            return;
        }
        let gap = 12.0;
        let browser_w = 460.0_f32.min((ui.available_width() * 0.42).max(280.0));
        let rack_w = (ui.available_width() - browser_w - gap).max(200.0);
        let height = ui.available_height();
        let mut close = false;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            ui.allocate_ui_with_layout(
                Vec2::new(rack_w, height),
                Layout::top_down(Align::Min),
                |ui| {
                    ui.set_width(rack_w);
                    ui.set_min_height(height);
                    self.mixer_rack(ui);
                },
            );
            ui.allocate_ui_with_layout(
                Vec2::new(browser_w, height),
                Layout::top_down(Align::Min),
                |ui| {
                    let colours = palette(ui.ctx());
                    egui::Frame::new()
                        .fill(colours.sheet)
                        .stroke(Stroke::new(1.0, colours.line))
                        .corner_radius(CornerRadius::same(8))
                        .inner_margin(Margin::same(12))
                        .show(ui, |ui| {
                            ui.set_width((browser_w - 24.0).max(80.0));
                            ui.horizontal(|ui| {
                                ui.add_space((ui.available_width() - 28.0).max(0.0));
                                let response = ui.add(
                                    egui::Button::new(
                                        RichText::new("×")
                                            .font(font(Face::Sans, TypeRole::Title))
                                            .color(colours.text2),
                                    )
                                    .frame(false),
                                );
                                row_info(&response, egui::Role::Button, true, "Close", None, None);
                                if response.clicked() {
                                    close = true;
                                }
                            });
                            self.effect_browser(ui);
                        });
                },
            );
        });
        if close {
            self.plugin_browser.open = false;
        }
    }
}
