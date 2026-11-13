//! Live Bridge: the page layout and the activity strip.  Each card lives in
//! its own file under `live/`.  Input and Output share one row, the same
//! width and the same height.  The bridge is the strip under that row: one
//! line when a PipeWire node has no ring, and the cushion when it does.
//! The record row (`live/record.rs`) sits directly under that strip.  The
//! Issues strip (`live/issues.rs`) sits on top while there are issues,
//! and "Show on <card>" there rings the bridge strip or the Output card.
//! An ASIO side's Panel chip posts a request; the bridge thread opens the
//! driver's control panel.

mod bridge_card;
mod calibration;
mod chain_strip;
mod driver_line;
mod health;
mod input_card;
mod issues;
mod output_card;
mod record;

use eframe::egui;
use egui::{Align, CornerRadius, Layout, Margin, RichText, Stroke, Vec2};

use crate::app::issues::IssueCard;
use crate::app::{LogTone, Page, RigWerkApp};
use crate::audio::{AsioPanelSide, ClickRhythm};
use crate::ui::theme::ts;
use crate::ui::theme::{log_colour, with_alpha, AMBER, BLUE, BORDER, CARD, DIM, MUTED, TEAL};
use crate::ui::widgets::{card_fixed, ellipsize, micro_label, mono, status_dot, tab_button_sized};

impl RigWerkApp {
    pub(crate) fn live_page(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let total = ui.available_width();

        // Judgments first: one strip, only while there is something to say.
        self.issues_strip(ui, total);

        self.live_io_pair(ui, total);
        ui.add_space(10.0);
        self.bridge_card(ui, ctx);
        ui.add_space(10.0);
        self.record_row(ui);

        ui.add_space(10.0);
        self.publish_banner(ui, total);
        ui.add_space(10.0);
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

    /// Input and Output, equal width.  The Output card's content sets the
    /// height; the Input card uses that height on the next frame so the two
    /// frames share a bottom edge.  One frame of lag is invisible next to
    /// the screenshot settle.
    fn live_io_pair(&mut self, ui: &mut egui::Ui, total: f32) {
        let gap = 14.0;
        let card_w = ((total - gap) / 2.0).max(240.0);
        let id = ui.scope_id().with("live_io_h");
        let min_h = ui
            .ctx()
            .data(|data| data.get_temp::<f32>(id).unwrap_or(0.0));
        let mut output_h = 0.0;
        let mut output_rect = egui::Rect::NOTHING;
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            ui.allocate_ui_with_layout(
                Vec2::new(card_w, 0.0),
                Layout::top_down(Align::Min),
                |ui| {
                    ui.set_min_width(card_w);
                    ui.set_max_width(card_w);
                    self.io_card(ui, min_h, |app, ui| app.input_card(ui));
                },
            );
            let output = ui.allocate_ui_with_layout(
                Vec2::new(card_w, 0.0),
                Layout::top_down(Align::Min),
                |ui| {
                    ui.set_min_width(card_w);
                    ui.set_max_width(card_w);
                    output_h = self.io_card(ui, 0.0, |app, ui| app.output_card(ui));
                },
            );
            output_rect = output.response.rect;
        });
        self.card_highlight_ring(ui, output_rect, IssueCard::Output);
        if (output_h - min_h).abs() > 1.0 {
            ui.ctx().data_mut(|data| data.insert_temp(id, output_h));
            ui.ctx().request_repaint();
        }
    }

    /// One Live card.  `min_h` stretches a shorter card down to its sibling.
    /// Returns the outer height, which is the content height when `min_h` is 0.
    fn io_card(
        &mut self,
        ui: &mut egui::Ui,
        min_h: f32,
        body: impl FnOnce(&mut Self, &mut egui::Ui),
    ) -> f32 {
        let response = egui::Frame::new()
            .fill(CARD)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(CornerRadius::same(13))
            .inner_margin(Margin::symmetric(15, 15))
            .show(ui, |ui| {
                let inner = (ui.available_width()).max(0.0);
                ui.set_min_width(inner);
                ui.set_max_width(inner);
                if min_h > 0.0 {
                    ui.set_min_height((min_h - 30.0).max(0.0));
                }
                body(self, ui);
            });
        response.response.rect.height()
    }

    /// ASIO control-panel chip for one bridge side.  Hidden unless that side
    /// is an ASIO driver.  Disabled until the running bridge has that driver
    /// open; the click only posts a request.
    pub(crate) fn asio_panel_chip(
        &mut self,
        ui: &mut egui::Ui,
        side: AsioPanelSide,
        color: egui::Color32,
    ) {
        let Some(clsid) = self.asio_panel_clsid(side) else {
            return;
        };
        let ready = self.engine.open_asio_driver(side) == Some(clsid);
        let response = ui
            .add_enabled_ui(ready, |ui| tab_button_sized(ui, "Panel", ready, color, 9.0))
            .inner;
        if !ready {
            let _ = response.on_disabled_hover_text(
                "The panel is available once the bridge is running on that ASIO driver.",
            );
        } else if response
            .on_hover_text("Open this ASIO driver's control panel.")
            .clicked()
        {
            self.engine.request_asio_panel(side);
        }
    }

    fn asio_panel_clsid(&self, side: AsioPanelSide) -> Option<u128> {
        match side {
            AsioPanelSide::Input => self.active_input().asio_clsid,
            AsioPanelSide::Output => self.selected_output().asio_clsid,
        }
    }

    /// Publish status for RigWerk Out: on, off, or missing.
    pub(crate) fn publish_banner(&mut self, ui: &mut egui::Ui, width: f32) {
        use crate::app::chain::ChainSelection;
        use crate::audio::VIRTUAL_OUT_NAME;
        use crate::ui::theme::GREEN;

        let virtual_index = self.followers.iter().position(|f| f.virtual_out);
        let published = virtual_index
            .and_then(|index| self.followers.get(index))
            .is_some_and(|f| f.publish);
        let (fill, border, text, button) = if published {
            (
                with_alpha(GREEN, 14),
                with_alpha(GREEN, 55),
                format!("Published · {VIRTUAL_OUT_NAME} · PipeWire source"),
                "ON",
            )
        } else if virtual_index.is_some() {
            (
                CARD,
                BORDER,
                format!("{VIRTUAL_OUT_NAME} is off."),
                "Publish",
            )
        } else {
            (
                CARD,
                BORDER,
                "Not publishing. Other apps can't capture the mix.".to_owned(),
                "Publish…",
            )
        };
        egui::Frame::new()
            .fill(fill)
            .stroke(Stroke::new(1.0, border))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(egui::Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.set_width(width - 32.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(text).size(ts(11.0)).color(MUTED));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let go = ui.add(
                            egui::Button::new(
                                RichText::new(button).size(ts(9.5)).color(if published {
                                    GREEN
                                } else {
                                    BLUE
                                }),
                            )
                            .fill(with_alpha(if published { GREEN } else { BLUE }, 14))
                            .stroke(Stroke::new(
                                1.0,
                                with_alpha(if published { GREEN } else { BLUE }, 45),
                            ))
                            .corner_radius(CornerRadius::same(7)),
                        );
                        if go.clicked() {
                            if let Some(index) = virtual_index {
                                if let Some(follower) = self.followers.get_mut(index) {
                                    if !follower.publish {
                                        follower.publish = true;
                                    }
                                }
                                self.chain_selected = ChainSelection::OutputDevice(index + 1);
                            } else {
                                self.chain_selected = ChainSelection::AddOutput;
                            }
                            self.page = Page::Mixer;
                        }
                    });
                });
            });
    }

    pub(crate) fn activity_strip(&mut self, ui: &mut egui::Ui, width: f32) {
        let latest = self
            .log
            .last()
            .map(|entry| (entry.at.clone(), entry.text.clone(), entry.tone))
            .unwrap_or_else(|| {
                (
                    "00:00".to_owned(),
                    "Bridge ready.".to_owned(),
                    LogTone::Muted,
                )
            });
        let latest = if let Some(error) = self.visible_error() {
            (latest.0, format!("Bridge error: {error}"), LogTone::Red)
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
                                ui.label(micro_label("BEAT", DIM));
                                ui.add_space(4.0);
                                match self.snapshot.metronome_beat {
                                    0 => {
                                        ui.label(mono("—", DIM, 11.0));
                                    }
                                    1 => {
                                        ui.label(mono("1", AMBER, 11.0));
                                    }
                                    beat => {
                                        ui.label(mono(beat.to_string(), TEAL, 11.0));
                                    }
                                }
                                ui.add_space(6.0);
                                // Beat 0 is "—", so the selected signature is
                                // fine.  A sounding beat uses the rhythm the
                                // mixer published with it.
                                let signature = if self.snapshot.metronome_beat == 0 {
                                    self.metronome_rhythm.label()
                                } else {
                                    ClickRhythm::from_u8(self.snapshot.metronome_rhythm).label()
                                };
                                ui.label(mono(signature, DIM, 11.0));
                                ui.add_space(12.0);
                                ui.label(micro_label("RECENT ACTIVITY", DIM));
                                ui.add_space(8.0);
                                ui.label(mono(&latest.0, DIM, 8.5));
                                status_dot(ui, log_colour(latest.2));
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
