//! One line on Live Bridge summing up the Mixer page: each stage of the
//! selected path's chain in order, with a way to open the full mixer.
//! With several paths the label names the path and counts them ("Main ·
//! 3 paths"), and the Output chip adds what the alignment delays the
//! output by (`RigWerkApp::added_alignment_ms`).

use eframe::egui;
use egui::{Align, CornerRadius, Layout, RichText, Sense, Stroke, Vec2};

use crate::app::{Page, RigWerkApp};
use crate::audio::EqSlot;
use crate::ui::pages::mixer::chain::live_chain_entries;
use crate::ui::theme::ts;
use crate::ui::theme::{with_alpha, BORDER, BORDER_SOFT, CARD, CARD_DEEP, DIM, MUTED, TEAL, TEXT};
use crate::ui::widgets::{card_fixed, ellipsize, micro_label, mono};

/// The chain label's second line with several paths: "Main · 3 paths".
pub(crate) fn path_count_label(name: &str, paths: usize) -> String {
    format!("{name} · {paths} paths")
}

/// The Output chip with alignment: "+0.0 dB · +2.7 ms".
pub(crate) fn output_summary(summary: &str, added_ms: f32) -> String {
    format!("{summary} · +{added_ms:.1} ms")
}

impl RigWerkApp {
    pub(crate) fn chain_strip(&mut self, ui: &mut egui::Ui, width: f32) {
        let views = self.engine.plugin_views(self.selected_path);
        let mut stages = live_chain_entries(
            self.input_gain,
            &self.path().mix.eqs[EqSlot::Pre.index()],
            &views,
            &self.path().mix.gate,
            &self.path().mix.eqs[EqSlot::Post.index()],
            self.output_gain,
            self.mute,
        );
        let paths = self.path_count();
        let added = self.added_alignment_ms();
        if added >= 0.05 {
            if let Some(output) = stages.last_mut() {
                output.summary = output_summary(&output.summary, added);
            }
        }
        let path_line = (paths > 1).then(|| path_count_label(&self.path().name, paths));

        let mut open: Option<crate::ui::pages::mixer::chain::ChainSelection> = None;
        card_fixed(ui, CARD, BORDER, 74.0, |ui| {
            ui.set_width(width - 32.0);
            ui.horizontal_centered(|ui| {
                match &path_line {
                    Some(line) => {
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 2.0;
                            ui.label(micro_label("CHAIN", TEAL));
                            ui.label(mono(ellipsize(line, 18), MUTED, 8.5));
                        })
                        .response
                        .on_hover_text(format!(
                            "{line}. This strip shows the path the Mixer edits; pick another there or in the sidebar."
                        ));
                    }
                    None => {
                        ui.label(micro_label("CHAIN", TEAL)).on_hover_text(
                            "The master chain on the Mixer page, in the order the sound flows.",
                        );
                    }
                }
                ui.add_space(8.0);
                let button_w = 108.0;
                let button_gap = 16.0;
                let arrow_w = 18.0;
                let chain_w = (ui.available_width() - button_w - button_gap).max(0.0);
                let arrows = (stages.len().saturating_sub(1)) as f32 * arrow_w;
                let stage_w = ((chain_w - arrows) / stages.len().max(1) as f32).max(72.0);
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
                for (index, entry) in stages.iter().enumerate() {
                    if index > 0 {
                        let (arrow, _) =
                            ui.allocate_exact_size(Vec2::new(arrow_w, 40.0), Sense::hover());
                        ui.painter().text(
                            arrow.center(),
                            egui::Align2::CENTER_CENTER,
                            "->",
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
                                with_alpha(entry.color, 90)
                            } else if entry.live {
                                with_alpha(entry.color, 45)
                            } else {
                                BORDER_SOFT
                            },
                        ),
                        egui::StrokeKind::Inside,
                    );
                    ui.painter().text(
                        rect.left_top() + Vec2::new(9.0, 7.0),
                        egui::Align2::LEFT_TOP,
                        entry.label,
                        egui::FontId::new(ts(7.5), egui::FontFamily::Monospace),
                        if entry.live { entry.color } else { DIM },
                    );
                    ui.painter().text(
                        rect.left_bottom() + Vec2::new(9.0, -7.0),
                        egui::Align2::LEFT_BOTTOM,
                        &entry.summary,
                        egui::FontId::new(ts(10.0), egui::FontFamily::Proportional),
                        if entry.live { TEXT } else { MUTED },
                    );
                    let hint = if entry.selection
                        == crate::ui::pages::mixer::chain::ChainSelection::Output
                        && added >= 0.05
                    {
                        format!(
                            "Open this stage on the Mixer page. Aligning the paths adds up to {added:.1} ms on an output device."
                        )
                    } else {
                        "Open this stage on the Mixer page".to_owned()
                    };
                    if response.on_hover_text(hint).clicked()
                    {
                        open = Some(entry.selection);
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
                        open = Some(self.chain_selected);
                    }
                });
            });
        });
        if let Some(selection) = open {
            self.select_chain(selection);
            self.page = Page::Mixer;
        }
    }
}

#[cfg(test)]
mod tests;
