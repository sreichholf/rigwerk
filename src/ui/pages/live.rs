//! Live Bridge: the page layout and the activity strip.  Each card lives in
//! its own file under `live/`.

mod bridge_card;
mod chain_strip;
mod driver_line;
mod health;
mod input_card;
mod output_card;

use eframe::egui;
use egui::{Align, CornerRadius, Layout, RichText, Stroke, Vec2};

use crate::app::{Page, RigWerkApp};
use crate::ui::theme::ts;
use crate::ui::theme::{with_alpha, AMBER, BLUE, BORDER, CARD, CARD_RAISED, DIM, MUTED, RED, TEAL};
use crate::ui::widgets::{card_fixed, connector, ellipsize, micro_label, mono, status_dot};

impl RigWerkApp {
    pub(crate) fn live_page(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let total = ui.available_width();
        let gap = 20.0;
        // Two 20 px connectors plus the four 8 px item gaps around them.
        let usable = (total - gap * 2.0 - 32.0).max(600.0);
        // Input and output are the two ends of the bridge: same width, same
        // height, with the bridge card between them.
        let bridge_w = usable * 0.26;
        let input_w = (usable - bridge_w) / 2.0;
        let output_w = input_w;
        // All three cards on this line share one exact height; their contents
        // differ, but the outer frames no longer do.
        let card_h = 408.0;

        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                Vec2::new(input_w, card_h),
                Layout::top_down(Align::LEFT),
                |ui| {
                    ui.set_min_size(Vec2::new(input_w, card_h));
                    card_fixed(ui, CARD, BORDER, card_h, |ui| {
                        ui.set_width(input_w - 32.0);
                        self.input_card(ui);
                    });
                },
            );
            connector(ui, card_h, self.is_active(), TEAL);
            ui.allocate_ui_with_layout(
                Vec2::new(bridge_w, card_h),
                Layout::top_down(Align::LEFT),
                |ui| {
                    ui.set_min_size(Vec2::new(bridge_w, card_h));
                    card_fixed(ui, CARD_RAISED, with_alpha(AMBER, 46), card_h, |ui| {
                        ui.set_width(bridge_w - 32.0);
                        self.bridge_card(ui, ctx);
                    });
                },
            );
            connector(ui, card_h, self.is_active(), BLUE);
            ui.allocate_ui_with_layout(
                Vec2::new(output_w, card_h),
                Layout::top_down(Align::LEFT),
                |ui| {
                    ui.set_min_size(Vec2::new(output_w, card_h));
                    card_fixed(ui, CARD, BORDER, card_h, |ui| {
                        ui.set_width(output_w - 32.0);
                        self.output_card(ui);
                    });
                },
            );
        });

        ui.add_space(14.0);
        // The full mixer and EQ live on their own page; this line sums them up.
        self.chain_strip(ui, total);
        ui.add_space(14.0);

        let scope_w = (total * 0.62).clamp(440.0, (total - 288.0).max(440.0));
        let health_w = (total - scope_w - 8.0).max(280.0);
        // The scope and the health tiles share this line, so they share one
        // exact height too; the scope's spare room stays at the bottom.
        let row_h = 312.0;
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                Vec2::new(scope_w, row_h),
                Layout::top_down(Align::LEFT),
                |ui| {
                    ui.set_min_size(Vec2::new(scope_w, row_h));
                    card_fixed(ui, CARD, BORDER, row_h, |ui| {
                        self.scope_card(ui, ctx, scope_w - 32.0);
                    });
                },
            );
            ui.allocate_ui_with_layout(
                Vec2::new(health_w, row_h),
                Layout::top_down(Align::LEFT),
                |ui| {
                    ui.set_min_size(Vec2::new(health_w, row_h));
                    card_fixed(ui, CARD, BORDER, row_h, |ui| {
                        self.health_card(ui, health_w - 32.0);
                    });
                },
            );
        });

        ui.add_space(11.0);
        self.activity_strip(ui, total);
    }

    pub(crate) fn activity_strip(&mut self, ui: &mut egui::Ui, width: f32) {
        let latest = self
            .log
            .last()
            .map(|entry| (entry.at.clone(), entry.text.clone(), entry.tone))
            .unwrap_or_else(|| ("00:00".to_owned(), "Bridge ready.".to_owned(), MUTED));
        let latest = if let Some(error) = self.visible_error() {
            (latest.0, format!("Bridge error: {error}"), RED)
        } else {
            latest
        };
        // Tight frame + one row whose height is the button. Labels are placed
        // inside that same height with Align::Center so they share the button's
        // vertical mid (plain ui.label sits optically high against button padding).
        egui::Frame::new()
            .fill(CARD)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(CornerRadius::same(13))
            .inner_margin(egui::Margin::symmetric(15, 8))
            .show(ui, |ui| {
                ui.set_width(width - 32.0);
                ui.horizontal(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let guide = ui.add(
                            egui::Button::new(
                                RichText::new("Open diagnostics").size(ts(9.5)).color(TEAL),
                            )
                            .fill(with_alpha(TEAL, 16))
                            .stroke(Stroke::new(1.0, with_alpha(TEAL, 45)))
                            .corner_radius(CornerRadius::same(7)),
                        );
                        if guide.clicked() {
                            self.page = Page::Diagnostics;
                        }
                        ui.add_space(8.0);
                        let row_h = guide.rect.height();
                        let row_w = ui.available_width();
                        ui.allocate_ui_with_layout(
                            Vec2::new(row_w, row_h),
                            Layout::left_to_right(Align::Center),
                            |ui| {
                                ui.set_min_height(row_h);
                                ui.set_max_height(row_h);
                                ui.label(micro_label("RECENT ACTIVITY", DIM));
                                ui.add_space(8.0);
                                ui.label(mono(&latest.0, DIM, 8.5));
                                status_dot(ui, latest.2);
                                ui.label(
                                    RichText::new(ellipsize(&latest.1, 96))
                                        .size(ts(9.2))
                                        .color(MUTED),
                                );
                            },
                        );
                    });
                });
            });
    }
}
