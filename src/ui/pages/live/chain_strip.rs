//! One line on Live Bridge summing up the Mixer page: each stage of the
//! master chain in order, with a way to open the full mixer.

use eframe::egui;
use egui::{Align, CornerRadius, Layout, RichText, Sense, Stroke, Vec2};

use crate::app::{Page, RigWerkApp};
use crate::audio::{EqSettings, EqSlot};
use crate::ui::theme::ts;
use crate::ui::theme::{
    with_alpha, AMBER, BLUE, BORDER, BORDER_SOFT, CARD, CARD_DEEP, DIM, MUTED, RED, TEAL, TEXT,
};
use crate::ui::widgets::{card_fixed, ellipsize, micro_label};

/// How one EQ reads in a single word or two.
fn eq_summary(eq: &EqSettings) -> (String, bool) {
    if !eq.enabled {
        return ("bypassed".to_owned(), false);
    }
    let shaping = eq
        .bands
        .iter()
        .filter(|band| band.on && band.gain_db.abs() > 0.01)
        .count();
    if shaping == 0 {
        ("flat".to_owned(), false)
    } else {
        (
            format!("{shaping} band{}", if shaping == 1 { "" } else { "s" }),
            true,
        )
    }
}

impl RigWerkApp {
    pub(crate) fn chain_strip(&mut self, ui: &mut egui::Ui, width: f32) {
        let views = self.engine.plugin_views();
        let active_inserts: Vec<&str> = views
            .iter()
            .filter(|view| !view.bypass && view.error.is_none())
            .map(|view| view.name.as_str())
            .collect();
        let (pre, pre_on) = eq_summary(&self.eqs[EqSlot::Pre.index()]);
        let (post, post_on) = eq_summary(&self.eqs[EqSlot::Post.index()]);
        let inserts = if views.is_empty() {
            "none".to_owned()
        } else if active_inserts.is_empty() {
            format!("{} bypassed", views.len())
        } else if active_inserts.len() == 1 {
            ellipsize(active_inserts[0], 18)
        } else {
            format!("{} active", active_inserts.len())
        };
        let stages = [
            ("INPUT", format!("{:+.1} dB", self.input_gain), TEAL, true),
            ("PRE EQ", pre, AMBER, pre_on),
            ("PLUGINS", inserts, AMBER, !active_inserts.is_empty()),
            ("POST EQ", post, AMBER, post_on),
            (
                "OUTPUT",
                if self.mute {
                    "muted".to_owned()
                } else {
                    format!("{:+.1} dB", self.output_gain)
                },
                if self.mute { RED } else { BLUE },
                true,
            ),
        ];

        let mut open = false;
        card_fixed(ui, CARD, BORDER, 74.0, |ui| {
            ui.set_width(width - 32.0);
            ui.horizontal_centered(|ui| {
                ui.label(micro_label("CHAIN", TEAL)).on_hover_text(
                    "The master chain on the Mixer page, in the order the sound flows.",
                );
                ui.add_space(8.0);
                // The stages get their own area of exact width, so the button
                // always keeps the same gap to the last stage.
                let button_w = 108.0;
                let button_gap = 16.0;
                let arrow_w = 18.0;
                let chain_w = (ui.available_width() - button_w - button_gap).max(0.0);
                let arrows = (stages.len() - 1) as f32 * arrow_w;
                let stage_w = ((chain_w - arrows) / stages.len() as f32).max(72.0);
                let (chain_rect, _) =
                    ui.allocate_exact_size(Vec2::new(chain_w, 40.0), Sense::hover());
                let mut chain = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(chain_rect)
                        .layout(Layout::left_to_right(Align::Center)),
                );
                chain.spacing_mut().item_spacing.x = 0.0;
                let outer = ui;
                let ui = &mut chain;
                for (index, (label, value, color, live)) in stages.iter().enumerate() {
                    if index > 0 {
                        let (arrow, _) =
                            ui.allocate_exact_size(Vec2::new(arrow_w, 40.0), Sense::hover());
                        ui.painter().text(
                            arrow.center(),
                            egui::Align2::CENTER_CENTER,
                            "→",
                            egui::FontId::new(ts(11.0), egui::FontFamily::Monospace),
                            DIM,
                        );
                    }
                    let (rect, response) =
                        ui.allocate_exact_size(Vec2::new(stage_w, 40.0), Sense::click());
                    let hovered = response.hovered();
                    ui.painter()
                        .rect_filled(rect, CornerRadius::same(8), CARD_DEEP);
                    ui.painter().rect_stroke(
                        rect,
                        CornerRadius::same(8),
                        Stroke::new(
                            1.0,
                            if hovered {
                                with_alpha(*color, 90)
                            } else if *live {
                                with_alpha(*color, 45)
                            } else {
                                BORDER_SOFT
                            },
                        ),
                        egui::StrokeKind::Inside,
                    );
                    ui.painter().text(
                        rect.left_top() + Vec2::new(9.0, 7.0),
                        egui::Align2::LEFT_TOP,
                        *label,
                        egui::FontId::new(ts(7.5), egui::FontFamily::Monospace),
                        if *live { *color } else { DIM },
                    );
                    ui.painter().text(
                        rect.left_bottom() + Vec2::new(9.0, -7.0),
                        egui::Align2::LEFT_BOTTOM,
                        value,
                        egui::FontId::new(ts(10.0), egui::FontFamily::Proportional),
                        if *live { TEXT } else { MUTED },
                    );
                    if response.on_hover_text("Open the Mixer page").clicked() {
                        open = true;
                    }
                }
                let ui = outer;
                ui.add_space(button_gap);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new("Open mixer").size(ts(10.0)).color(TEAL),
                            )
                            .fill(with_alpha(TEAL, 14))
                            .stroke(Stroke::new(1.0, with_alpha(TEAL, 45)))
                            .corner_radius(CornerRadius::same(7))
                            .min_size(Vec2::new(button_w, 30.0)),
                        )
                        .clicked()
                    {
                        open = true;
                    }
                });
            });
        });
        if open {
            self.page = Page::Mixer;
        }
    }
}
