//! Signal Routing: which input reaches which output tap.

use eframe::egui;
use egui::{
    Align, Align2, Color32, CornerRadius, FontFamily, FontId, Layout, Rect, RichText, Sense,
    Stroke, Vec2,
};

use crate::app::RigWerkApp;
use crate::ui::theme::ts;
use crate::ui::theme::{
    with_alpha, AMBER, BLUE, BORDER, BORDER_SOFT, CARD, CARD_DEEP, DIM, MUTED, TEAL, TEXT,
};
use crate::ui::widgets::{
    card, horizontal_line, micro_label, mono, route_summary, status_dot, tab_button_sized,
};

pub(crate) fn route_cell(ui: &mut egui::Ui, enabled: bool, input: usize, output: usize) -> bool {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(58.0, 48.0), Sense::click());
    let base = if enabled {
        with_alpha(TEAL, 18)
    } else if response.hovered() {
        with_alpha(TEXT, 7)
    } else {
        CARD_DEEP
    };
    ui.painter().rect_filled(rect, CornerRadius::same(8), base);
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(8),
        Stroke::new(
            1.0,
            if enabled {
                with_alpha(TEAL, 75)
            } else {
                BORDER_SOFT
            },
        ),
        egui::StrokeKind::Inside,
    );
    if enabled {
        ui.painter()
            .circle_filled(rect.center(), 9.0, with_alpha(TEAL, 26));
        ui.painter().circle_filled(rect.center(), 3.3, TEAL);
    } else {
        ui.painter().circle_stroke(
            rect.center(),
            7.0,
            Stroke::new(1.0, Color32::from_rgb(49, 60, 77)),
        );
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            format!("{}{}", input + 1, output + 1),
            FontId::new(ts(7.5), FontFamily::Monospace),
            Color32::from_rgb(53, 64, 80),
        );
    }
    response.clicked()
}

pub(crate) fn tap_row(ui: &mut egui::Ui, output: usize, active: bool, level: f32, color: Color32) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 18.0), Sense::hover());
    ui.painter().text(
        rect.left_center() + Vec2::new(2.0, 0.0),
        Align2::LEFT_CENTER,
        format!("OUT {}", output + 1),
        FontId::new(ts(8.5), FontFamily::Monospace),
        if active { MUTED } else { DIM },
    );
    let bar = Rect::from_min_size(
        egui::pos2(rect.left() + 58.0, rect.center().y - 2.0),
        Vec2::new((rect.width() - 106.0).max(20.0), 4.0),
    );
    ui.painter()
        .rect_filled(bar, CornerRadius::same(2), Color32::from_rgb(25, 32, 44));
    if active {
        ui.painter().rect_filled(
            Rect::from_min_size(
                bar.min,
                Vec2::new(bar.width() * level.clamp(0.0, 1.0), bar.height()),
            ),
            CornerRadius::same(2),
            color,
        );
    }
    ui.painter().text(
        rect.right_center() + Vec2::new(-2.0, 0.0),
        Align2::RIGHT_CENTER,
        if active { "ROUTED" } else { "MUTED" },
        FontId::new(ts(7.0), FontFamily::Monospace),
        if active { color } else { DIM },
    );
}

impl RigWerkApp {
    pub(crate) fn routing_page(&mut self, ui: &mut egui::Ui) {
        let total = ui.available_width();
        let left_w = (total * 0.66).clamp(520.0, (total - 288.0).max(520.0));
        let right_w = (total - left_w - 8.0).max(280.0);

        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                Vec2::new(left_w, 370.0),
                Layout::top_down(Align::LEFT),
                |ui| {
                    ui.set_min_size(Vec2::new(left_w, 370.0));
                    card(ui, CARD, BORDER, |ui| {
                        ui.set_width(left_w - 32.0);
                        ui.horizontal(|ui| {
                            ui.label(micro_label("ROUTING MATRIX", TEAL));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(mono("2 IN → 4 OUT", DIM, 9.5));
                            });
                        });
                        ui.add_space(10.0);

                        ui.horizontal(|ui| {
                            ui.add_space(150.0);
                            for output in 1..=4 {
                                ui.add_sized(
                                    [58.0, 18.0],
                                    egui::Label::new(mono(format!("OUT {output}"), DIM, 9.0)),
                                );
                            }
                        });
                        ui.add_space(2.0);

                        for input in 0..2 {
                            ui.horizontal(|ui| {
                                let (rect, _) =
                                    ui.allocate_exact_size(Vec2::new(142.0, 48.0), Sense::hover());
                                ui.painter()
                                    .rect_filled(rect, CornerRadius::same(8), CARD_DEEP);
                                ui.painter().text(
                                    rect.left_center() + Vec2::new(12.0, -6.0),
                                    Align2::LEFT_CENTER,
                                    format!("IN {}", input + 1),
                                    FontId::new(ts(11.5), FontFamily::Proportional),
                                    TEXT,
                                );
                                ui.painter().text(
                                    rect.left_center() + Vec2::new(12.0, 8.0),
                                    Align2::LEFT_CENTER,
                                    if input == 0 {
                                        "ASIO 1 · guitar"
                                    } else {
                                        "ASIO 2 · spare"
                                    },
                                    FontId::new(ts(8.5), FontFamily::Proportional),
                                    DIM,
                                );
                                for output in 0..4 {
                                    if route_cell(ui, self.routes[input][output], input, output) {
                                        self.routes[input][output] = !self.routes[input][output];
                                        self.push_log(
                                            format!(
                                                "Route IN {} → OUT {} {}.",
                                                input + 1,
                                                output + 1,
                                                if self.routes[input][output] {
                                                    "enabled"
                                                } else {
                                                    "muted"
                                                }
                                            ),
                                            if self.routes[input][output] {
                                                TEAL
                                            } else {
                                                MUTED
                                            },
                                        );
                                    }
                                }
                            });
                            ui.add_space(5.0);
                        }

                        ui.add_space(10.0);
                        horizontal_line(ui, BORDER_SOFT);
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Tip").size(ts(9.5)).color(AMBER).strong());
                            ui.label(
                                RichText::new(
                                    "Keep one dry route for monitoring and one for your recorder.",
                                )
                                .size(ts(10.0))
                                .color(MUTED),
                            );
                        });
                        ui.add_space(9.0);
                        horizontal_line(ui, BORDER_SOFT);
                        ui.add_space(7.0);
                        ui.label(micro_label("OUTPUT TAPS", DIM));
                        ui.add_space(3.0);
                        for output in 0..4 {
                            let active = (0..2).any(|input| self.routes[input][output]);
                            let level = if active {
                                0.28 + self.meter_out_l * (0.35 + output as f32 * 0.07)
                            } else {
                                0.0
                            };
                            tap_row(
                                ui,
                                output,
                                active,
                                level.clamp(0.0, 1.0),
                                if output == 0 {
                                    BLUE
                                } else {
                                    with_alpha(BLUE, 160)
                                },
                            );
                        }
                    });
                },
            );
            ui.allocate_ui_with_layout(
                Vec2::new(right_w, 370.0),
                Layout::top_down(Align::LEFT),
                |ui| {
                    ui.set_min_size(Vec2::new(right_w, 370.0));
                    card(ui, CARD, BORDER, |ui| {
                        ui.set_width(right_w - 32.0);
                        ui.label(micro_label("ACTIVE PATCH", BLUE));
                        ui.add_space(8.0);
                        route_summary(ui, "SOURCE", &self.active_input().name, TEAL);
                        ui.add_space(6.0);
                        route_summary(ui, "DESTINATION", &self.selected_output().name, BLUE);
                        ui.add_space(12.0);
                        horizontal_line(ui, BORDER_SOFT);
                        ui.add_space(9.0);
                        ui.label(micro_label("CHANNEL MODE", DIM));
                        ui.add_space(4.0);
                        let mut mono = self.mono;
                        if tab_button_sized(ui, "Stereo", !mono, BLUE, 11.0).clicked() {
                            mono = false;
                        }
                        if tab_button_sized(ui, "Mono sum", mono, BLUE, 11.0).clicked() {
                            mono = true;
                        }
                        self.mono = mono;
                        ui.add_space(10.0);
                        ui.label(micro_label("MONITORING", DIM));
                        ui.add_space(4.0);
                        ui.checkbox(
                            &mut self.monitor,
                            RichText::new("Direct monitor input")
                                .size(ts(10.5))
                                .color(MUTED),
                        );
                        ui.add_space(10.0);
                        let mut drift = self.drift;
                        if ui
                            .checkbox(
                                &mut drift,
                                RichText::new("Automatic drift correction")
                                    .size(ts(10.5))
                                    .color(MUTED),
                            )
                            .changed()
                        {
                            self.drift = drift;
                        }
                        ui.add_space(10.0);
                        horizontal_line(ui, BORDER_SOFT);
                        ui.add_space(8.0);
                        ui.horizontal_top(|ui| {
                            status_dot(ui, TEAL);
                            ui.add(
                                egui::Label::new(
                                    RichText::new(
                                        "Routes apply live. Out 1 and 2 feed the selected output; 3 and 4 are used on interfaces with four or more channels.",
                                    )
                                    .size(ts(9.0))
                                    .color(MUTED),
                                )
                                .wrap(),
                            );
                        });
                    });
                },
            );
        });
    }
}
