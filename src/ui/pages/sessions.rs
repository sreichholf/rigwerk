//! Sessions: save, load and delete patches.

use eframe::egui;
use egui::{
    Align, Align2, CornerRadius, FontFamily, FontId, Layout, Margin, Rect, RichText, Sense, Stroke,
    Vec2,
};

use crate::app::RigWerkApp;
use crate::ui::theme::ts;
use crate::ui::theme::{
    with_alpha, BLUE, BORDER, BORDER_SOFT, CARD, CARD_DEEP, DIM, MUTED, RED, TEAL, TEXT,
};
use crate::ui::widgets::{
    card, ellipsize, ghost_button, micro_label, mono, primary_button, route_summary, small_fact,
};

pub(crate) fn preset_row(
    ui: &mut egui::Ui,
    name: &str,
    note: &str,
    path: &str,
    facts: &str,
    selected: bool,
    confirming_delete: bool,
) -> PresetAction {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 64.0), Sense::click());
    // The delete control sits on top of the row, so it is registered after the
    // row and wins the click.
    let delete_label = if confirming_delete {
        "CONFIRM DELETE"
    } else {
        "DELETE"
    };
    let delete_w = if confirming_delete { 92.0 } else { 46.0 };
    let delete_rect = Rect::from_min_size(
        rect.right_top() + Vec2::new(-delete_w - 6.0, 5.0),
        Vec2::new(delete_w, 17.0),
    );
    let delete = ui.interact(delete_rect, response.id.with("delete"), Sense::click());
    let fill = if selected {
        with_alpha(BLUE, 12)
    } else if response.hovered() {
        with_alpha(TEXT, 6)
    } else {
        CARD_DEEP
    };
    ui.painter().rect_filled(rect, CornerRadius::same(8), fill);
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(8),
        Stroke::new(
            1.0,
            if selected {
                with_alpha(BLUE, 55)
            } else {
                BORDER_SOFT
            },
        ),
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        rect.left_top() + Vec2::new(10.0, 9.0),
        Align2::LEFT_TOP,
        name,
        FontId::new(ts(11.0), FontFamily::Proportional),
        TEXT,
    );
    ui.painter().text(
        rect.left_top() + Vec2::new(10.0, 25.0),
        Align2::LEFT_TOP,
        ellipsize(note, 48),
        FontId::new(ts(8.2), FontFamily::Proportional),
        DIM,
    );
    ui.painter().text(
        rect.left_bottom() + Vec2::new(10.0, -9.0),
        Align2::LEFT_BOTTOM,
        ellipsize(path, 52),
        FontId::new(ts(8.2), FontFamily::Monospace),
        MUTED,
    );
    ui.painter().text(
        rect.right_bottom() + Vec2::new(-10.0, -9.0),
        Align2::RIGHT_BOTTOM,
        facts,
        FontId::new(ts(7.8), FontFamily::Monospace),
        DIM,
    );
    let delete_color = if confirming_delete || delete.hovered() {
        RED
    } else {
        DIM
    };
    if confirming_delete || delete.hovered() {
        ui.painter()
            .rect_filled(delete_rect, CornerRadius::same(5), with_alpha(RED, 22));
    }
    ui.painter().rect_stroke(
        delete_rect,
        CornerRadius::same(5),
        Stroke::new(1.0, with_alpha(delete_color, 70)),
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        delete_rect.center(),
        Align2::CENTER_CENTER,
        delete_label,
        FontId::new(ts(7.3), FontFamily::Monospace),
        delete_color,
    );
    if selected {
        ui.painter().text(
            delete_rect.left_center() + Vec2::new(-8.0, 0.0),
            Align2::RIGHT_CENTER,
            "CURRENT",
            FontId::new(ts(7.3), FontFamily::Monospace),
            BLUE,
        );
    }
    let delete = delete.on_hover_text(if confirming_delete {
        "Click again to delete this patch for good"
    } else {
        "Delete this patch"
    });
    if delete.clicked() {
        PresetAction::Delete
    } else if response.clicked() {
        PresetAction::Load
    } else {
        PresetAction::None
    }
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum PresetAction {
    None,
    Load,
    Delete,
}

impl RigWerkApp {
    pub(crate) fn sessions_page(&mut self, ui: &mut egui::Ui) {
        let total = ui.available_width();
        let left_w = (total * 0.42).clamp(340.0, (total - 300.0).max(340.0));
        let right_w = (total - left_w - 8.0).max(300.0);
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                Vec2::new(left_w, 330.0),
                Layout::top_down(Align::LEFT),
                |ui| {
                    ui.set_min_size(Vec2::new(left_w, 330.0));
                    card(ui, CARD, BORDER, |ui| {
                        ui.set_width(left_w - 32.0);
                        ui.label(micro_label("CURRENT PATCH", TEAL));
                        ui.add_space(8.0);
                        egui::Frame::new()
                            .fill(CARD_DEEP)
                            .stroke(Stroke::new(1.0, BORDER_SOFT))
                            .corner_radius(CornerRadius::same(8))
                            .inner_margin(Margin::symmetric(10, 6))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.session_name)
                                        .frame(egui::Frame::NONE)
                                        .text_color(TEXT)
                                        .hint_text("Name this patch")
                                        .font(FontId::new(ts(14.0), FontFamily::Proportional)),
                                );
                            });
                        ui.add_space(9.0);
                        route_summary(ui, "IN", &self.active_input().name, TEAL);
                        ui.add_space(5.0);
                        route_summary(ui, "OUT", &self.selected_output().name, BLUE);
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            small_fact(ui, "RATE", &format!("{} Hz", self.sample_rate));
                            small_fact(ui, "BUFFER", &format!("{} smp", self.buffer_size));
                            small_fact(ui, "LATENCY", &format!("{:.1} ms", self.latency_ms()));
                        });
                        ui.add_space(12.0);
                        let save_label = if self.preset_named(&self.session_name).is_some() {
                            "Update saved patch"
                        } else {
                            "Save as new patch"
                        };
                        if primary_button(ui, save_label, TEAL, ui.available_width()) {
                            self.save_preset();
                        }
                        ui.add_space(5.0);
                        if ghost_button(ui, "Start from blank", ui.available_width()) {
                            self.session_name = "Untitled patch".to_owned();
                            self.push_log("New blank patch ready.", MUTED);
                        }
                    });
                },
            );
            ui.allocate_ui_with_layout(
                Vec2::new(right_w, 330.0),
                Layout::top_down(Align::LEFT),
                |ui| {
                    ui.set_min_size(Vec2::new(right_w, 330.0));
                    card(ui, CARD, BORDER, |ui| {
                        ui.set_width(right_w - 32.0);
                        ui.horizontal(|ui| {
                            ui.label(micro_label("SAVED PATCHES", DIM));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(mono(format!("{}", self.sessions.len()), DIM, 9.5));
                            });
                        });
                        ui.add_space(7.0);
                        if self.sessions.is_empty() {
                            egui::Frame::new()
                                .fill(CARD_DEEP)
                                .stroke(Stroke::new(1.0, BORDER_SOFT))
                                .corner_radius(CornerRadius::same(9))
                                .inner_margin(Margin::symmetric(18, 24))
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.vertical_centered(|ui| {
                                        ui.label(
                                            RichText::new("+")
                                                .size(ts(25.0))
                                                .color(TEAL)
                                                .strong(),
                                        );
                                        ui.add_space(3.0);
                                        ui.label(
                                            RichText::new("No saved patches yet")
                                                .size(ts(12.5))
                                                .color(TEXT)
                                                .strong(),
                                        );
                                        ui.add_space(5.0);
                                        ui.label(
                                            RichText::new(
                                                "Name the input and output pair above, then save it here for the next take.",
                                            )
                                            .size(ts(9.5))
                                            .color(MUTED),
                                        );
                                    });
                                });
                            ui.add_space(12.0);
                            ui.vertical_centered(|ui| {
                                ui.label(
                                    RichText::new(
                                        "Start the bridge, play a few notes, save the pairing you like.",
                                    )
                                    .size(ts(8.8))
                                    .color(DIM),
                                );
                            });
                        } else {
                            let sessions: Vec<(String, String, String, String, u32, usize)> = self
                                .sessions
                                .iter()
                                .map(|preset| {
                                    (
                                        preset.name.clone(),
                                        preset.note.clone(),
                                        preset.input.clone(),
                                        preset.output.clone(),
                                        preset.rate,
                                        preset.buffer,
                                    )
                                })
                                .collect();
                            egui::ScrollArea::vertical()
                                .id_salt("saved_patches")
                                .max_height(236.0)
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    let mut action = None;
                                    for (index, (name, note, input, output, rate, buffer)) in
                                        sessions.iter().enumerate()
                                    {
                                        match preset_row(
                                            ui,
                                            name,
                                            note,
                                            &format!("{input}  →  {output}"),
                                            &format!("{rate} Hz · {buffer} smp"),
                                            Some(index) == self.selected_session,
                                            Some(index) == self.pending_delete,
                                        ) {
                                            PresetAction::None => {}
                                            choice => action = Some((index, choice)),
                                        }
                                        ui.add_space(3.0);
                                    }
                                    match action {
                                        Some((index, PresetAction::Load)) => {
                                            self.apply_preset(index)
                                        }
                                        Some((index, PresetAction::Delete)) => {
                                            if self.pending_delete == Some(index) {
                                                self.delete_preset(index);
                                            } else {
                                                self.pending_delete = Some(index);
                                            }
                                        }
                                        _ => {}
                                    }
                                });
                        }
                        ui.add_space(8.0);
                        ui.label(
                            RichText::new("Patches are saved on this computer.")
                                .size(ts(9.0))
                                .color(DIM),
                        );
                    });
                },
            );
        });
    }
}
