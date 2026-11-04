//! The master mixer, laid out in the order the sound flows through it:
//! input level → pre EQ → plugin inserts → post EQ → output level.

use eframe::egui;
use egui::{Align, Color32, CornerRadius, Layout, Margin, Rect, RichText, Sense, Stroke, Vec2};

use crate::app::{Page, RigWerkApp};
use crate::audio::{EngineState, EqSlot, PluginView};
use crate::ui::pages::mixer::eq_card::{draw_curve, slot_hint, slot_title};
use crate::ui::theme::ts;
use crate::ui::theme::{
    with_alpha, AMBER, BLUE, BORDER, BORDER_SOFT, CARD, CARD_DEEP, DIM, GREEN, MUTED, RED, TEAL,
    TEXT,
};
use crate::ui::widgets::{
    card_fixed, db_slider, ellipsize, hinted_tab, meter_row, micro_label, mono, status_dot,
    styled_slider,
};

/// Height of every stage in the chain.  Tall enough that an EQ stage holds
/// its header, curve, summary and edit button with a real margin below.
const STAGE_H: f32 = 150.0;
const STRIP_W: f32 = 112.0;
const EQ_W: f32 = 132.0;
const ARROW_W: f32 = 20.0;
const INSERT_W: f32 = 154.0;
/// The insert cards fill the stage, leaving the same 4 px they had before.
const INSERT_H: f32 = STAGE_H - 4.0;

enum MixerAction {
    Bypass(usize, bool),
    Mix(usize, f32),
    Configure(usize),
    Remove(usize),
}

impl RigWerkApp {
    pub(crate) fn mixer_card(&mut self, ui: &mut egui::Ui, width: f32) {
        let views = self.engine.plugin_views();
        let mut actions: Vec<MixerAction> = Vec::new();
        let mut open_plugins = false;

        // The header and the stages need 40 + STAGE_H px inside the frame's
        // 15 px margin, plus the 7 px gap between header and stages.
        card_fixed(ui, CARD, BORDER, STAGE_H + 76.0, |ui| {
            ui.set_width(width - 32.0);
            ui.horizontal(|ui| {
                ui.label(micro_label("MIXER", TEAL));
                ui.add_space(6.0);
                ui.label(
                    RichText::new("Master bus · in the order the sound flows, left to right")
                        .size(ts(10.0))
                        .color(DIM),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new("Manage plugins").size(ts(9.5)).color(TEAL),
                            )
                            .fill(with_alpha(TEAL, 14))
                            .stroke(Stroke::new(1.0, with_alpha(TEAL, 45)))
                            .corner_radius(CornerRadius::same(7)),
                        )
                        .clicked()
                    {
                        open_plugins = true;
                    }
                    ui.add_space(8.0);
                    ui.label(mono(
                        format!(
                            "{} INSERT{}",
                            views.len(),
                            if views.len() == 1 { "" } else { "S" }
                        ),
                        DIM,
                        8.5,
                    ));
                });
            });
            ui.add_space(7.0);

            ui.horizontal_top(|ui| {
                // Stages sit edge to edge; the arrows are the only spacing,
                // so the inserts get exactly what is left in the middle.
                ui.spacing_mut().item_spacing.x = 0.0;
                self.input_stage(ui);
                chain_arrow(ui);
                self.eq_stage(ui, EqSlot::Pre);
                chain_arrow(ui);
                let inserts_w =
                    (ui.available_width() - ARROW_W * 2.0 - EQ_W - STRIP_W).max(INSERT_W);
                ui.allocate_ui_with_layout(
                    Vec2::new(inserts_w, STAGE_H),
                    Layout::top_down(Align::LEFT),
                    |ui| {
                        ui.set_width(inserts_w);
                        ui.spacing_mut().item_spacing.x = 4.0;
                        inserts_stage(ui, &views, &mut actions, &mut open_plugins);
                    },
                );
                chain_arrow(ui);
                self.eq_stage(ui, EqSlot::Post);
                chain_arrow(ui);
                self.output_stage(ui);
            });
        });

        for action in actions {
            match action {
                MixerAction::Bypass(index, bypass) => self.set_plugin_bypass(index, bypass),
                MixerAction::Mix(index, mix) => self.set_plugin_mix(index, mix),
                MixerAction::Configure(index) => self.open_plugin_editor(index),
                MixerAction::Remove(index) => self.remove_plugin(index),
            }
        }
        if open_plugins {
            self.page = Page::Plugins;
        }
    }

    /// First in the chain: the input level and meters.
    fn input_stage(&mut self, ui: &mut egui::Ui) {
        ui.allocate_ui_with_layout(
            Vec2::new(STRIP_W, STAGE_H),
            Layout::top_down(Align::LEFT),
            |ui| {
                ui.set_width(STRIP_W);
                ui.label(micro_label("INPUT", TEAL));
                ui.label(mono(format!("{:+.1} dB", self.input_gain), TEXT, 9.5));
                if db_slider(ui, &mut self.input_gain, -24.0, 18.0, TEAL) {
                    self.engine.set_gains(self.input_gain, self.output_gain);
                }
                meter_row(ui, "L", self.meter_in_l, self.peak_in_l, TEAL);
                meter_row(ui, "R", self.meter_in_r, self.peak_in_r, TEAL);
            },
        );
    }

    /// Last in the chain: the output level, mute and meters.
    fn output_stage(&mut self, ui: &mut egui::Ui) {
        ui.allocate_ui_with_layout(
            Vec2::new(STRIP_W, STAGE_H),
            Layout::top_down(Align::LEFT),
            |ui| {
                ui.set_width(STRIP_W);
                ui.horizontal(|ui| {
                    ui.label(micro_label("OUTPUT", BLUE));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let mute = self.mute;
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new(if mute { "muted" } else { "mute" })
                                        .size(ts(8.0))
                                        .color(if mute { RED } else { GREEN }),
                                )
                                .fill(if mute {
                                    with_alpha(RED, 22)
                                } else {
                                    with_alpha(GREEN, 14)
                                })
                                .stroke(Stroke::new(
                                    1.0,
                                    if mute {
                                        with_alpha(RED, 55)
                                    } else {
                                        with_alpha(GREEN, 35)
                                    },
                                ))
                                .corner_radius(CornerRadius::same(6)),
                            )
                            .clicked()
                        {
                            self.mute = !mute;
                        }
                    });
                });
                ui.label(mono(format!("{:+.1} dB", self.output_gain), TEXT, 9.5));
                if db_slider(ui, &mut self.output_gain, -36.0, 6.0, BLUE) {
                    self.engine.set_gains(self.input_gain, self.output_gain);
                }
                meter_row(ui, "L", self.meter_out_l, self.peak_out_l, BLUE);
                meter_row(ui, "R", self.meter_out_r, self.peak_out_r, BLUE);
            },
        );
    }

    /// One of the two EQs as a compact stage: its curve, an on/bypass switch
    /// and "Edit", which opens it in the EQ card below.
    fn eq_stage(&mut self, ui: &mut egui::Ui, slot: EqSlot) {
        let eq = self.eqs[slot.index()];
        let editing = self.eq_edit == slot;
        let rate = if self.engine.state == EngineState::Running && self.snapshot.output_rate > 0 {
            self.snapshot.output_rate
        } else {
            48_000
        };
        let (rect, _) = ui.allocate_exact_size(Vec2::new(EQ_W, STAGE_H), Sense::hover());
        ui.painter()
            .rect_filled(rect, CornerRadius::same(9), CARD_DEEP);
        ui.painter().rect_stroke(
            rect,
            CornerRadius::same(9),
            Stroke::new(
                1.0,
                if editing {
                    with_alpha(AMBER, 70)
                } else {
                    BORDER_SOFT
                },
            ),
            egui::StrokeKind::Inside,
        );
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect.shrink(8.0))
                .layout(Layout::top_down(Align::LEFT)),
        );
        // Nothing may paint over the stage's border.
        child.set_clip_rect(rect.shrink(8.0));
        let ui = &mut child;
        let mut enabled = eq.enabled;
        ui.horizontal(|ui| {
            ui.label(micro_label(
                slot_title(slot),
                if eq.enabled { AMBER } else { DIM },
            ))
            .on_hover_text(slot_hint(slot));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if hinted_tab(
                    ui,
                    if enabled { "ON" } else { "BYP" },
                    enabled,
                    AMBER,
                    "Switch this EQ on or off. Settings are kept while bypassed.",
                ) {
                    enabled = !enabled;
                }
            });
        });
        let (curve, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 42.0), Sense::hover());
        draw_curve(ui, curve, &eq, rate, false);
        let shaping = eq
            .bands
            .iter()
            .filter(|band| band.on && band.gain_db.abs() > 0.01)
            .count();
        ui.label(
            RichText::new(if !eq.enabled {
                "bypassed".to_owned()
            } else if shaping == 0 {
                "flat".to_owned()
            } else {
                format!(
                    "{shaping} band{} shaping",
                    if shaping == 1 { "" } else { "s" }
                )
            })
            .size(ts(8.5))
            .color(if eq.enabled && shaping > 0 {
                MUTED
            } else {
                DIM
            }),
        );
        // The edit button is pinned to the bottom and centred, so it keeps a
        // visible margin below it and lines up with the stage's axis.
        ui.with_layout(Layout::bottom_up(Align::Center), |ui| {
            ui.add_space(6.0);
            if hinted_tab(
                ui,
                if editing { "EDITING BELOW" } else { "EDIT" },
                editing,
                AMBER,
                "Show this EQ's bands in the EQ card below.",
            ) {
                self.eq_edit = slot;
            }
        });

        if enabled != eq.enabled {
            let mut updated = eq;
            updated.enabled = enabled;
            self.eqs[slot.index()] = updated;
            self.engine.set_eq(slot, updated);
            self.push_log(
                format!(
                    "{} {}.",
                    slot_title(slot),
                    if enabled { "on" } else { "bypassed" }
                ),
                AMBER,
            );
        }
    }
}

/// A small arrow between two stages of the chain.
fn chain_arrow(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ARROW_W, STAGE_H), Sense::hover());
    let y = rect.top() + STAGE_H * 0.42;
    let stroke = Stroke::new(1.2, with_alpha(DIM, 200));
    let left = rect.left() + 4.0;
    let right = rect.right() - 4.0;
    ui.painter()
        .line_segment([egui::pos2(left, y), egui::pos2(right, y)], stroke);
    ui.painter().line_segment(
        [egui::pos2(right - 4.0, y - 3.5), egui::pos2(right, y)],
        stroke,
    );
    ui.painter().line_segment(
        [egui::pos2(right - 4.0, y + 3.5), egui::pos2(right, y)],
        stroke,
    );
}

/// The middle of the chain: the plugin insert cards, or a hint to add one.
fn inserts_stage(
    ui: &mut egui::Ui,
    views: &[PluginView],
    actions: &mut Vec<MixerAction>,
    open_plugins: &mut bool,
) {
    if views.is_empty() {
        egui::Frame::new()
            .fill(CARD_DEEP)
            .stroke(Stroke::new(1.0, BORDER_SOFT))
            .corner_radius(CornerRadius::same(9))
            .inner_margin(Margin::symmetric(14, 12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.set_min_height(STAGE_H - 24.0);
                ui.label(micro_label("PLUGIN INSERTS", DIM));
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    status_dot(ui, DIM);
                    ui.label(
                        RichText::new("No inserts yet")
                            .size(ts(10.5))
                            .color(MUTED)
                            .strong(),
                    );
                });
                ui.label(
                    RichText::new("Plugins run here, between the pre and post EQ.")
                        .size(ts(9.0))
                        .color(DIM),
                );
                ui.add_space(8.0);
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new("+ Add a plugin").size(ts(9.5)).color(TEAL),
                        )
                        .fill(with_alpha(TEAL, 14))
                        .stroke(Stroke::new(1.0, with_alpha(TEAL, 45)))
                        .corner_radius(CornerRadius::same(7)),
                    )
                    .clicked()
                {
                    *open_plugins = true;
                }
            });
        return;
    }

    egui::ScrollArea::horizontal()
        .id_salt("mixer_inserts")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                for (index, view) in views.iter().enumerate() {
                    insert_card(ui, index, view, actions);
                }
                ui.allocate_ui_with_layout(
                    Vec2::new(38.0, INSERT_H),
                    Layout::top_down(Align::Center),
                    |ui| {
                        ui.add_space((INSERT_H - 36.0) / 2.0);
                        if ui
                            .add(
                                egui::Button::new(RichText::new("+").size(ts(18.0)).color(TEAL))
                                    .fill(with_alpha(TEAL, 10))
                                    .stroke(Stroke::new(1.0, with_alpha(TEAL, 45)))
                                    .corner_radius(CornerRadius::same(9))
                                    .min_size(Vec2::new(36.0, 36.0)),
                            )
                            .on_hover_text("Add a VST3 or CLAP plugin")
                            .clicked()
                        {
                            *open_plugins = true;
                        }
                    },
                );
            });
        });
}

/// One plugin insert: number, bypass, controls, remove, name and dry/wet mix.
fn insert_card(ui: &mut egui::Ui, index: usize, view: &PluginView, actions: &mut Vec<MixerAction>) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(INSERT_W, INSERT_H), Sense::hover());
    let border = if view.error.is_some() {
        with_alpha(RED, 55)
    } else if view.bypass {
        BORDER_SOFT
    } else {
        with_alpha(AMBER, 45)
    };
    ui.painter()
        .rect_filled(rect, CornerRadius::same(9), CARD_DEEP);
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(9),
        Stroke::new(1.0, border),
        egui::StrokeKind::Inside,
    );
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink(9.0))
            .layout(Layout::top_down(Align::LEFT)),
    );
    child.set_clip_rect(clip(rect));
    let ui = &mut child;
    ui.horizontal(|ui| {
        ui.label(mono(format!("{:02}", index + 1), DIM, 8.0));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui
                .add(
                    egui::Button::new(RichText::new("×").size(ts(12.0)).color(DIM))
                        .fill(Color32::TRANSPARENT)
                        .stroke(Stroke::NONE),
                )
                .on_hover_text("Remove this insert")
                .clicked()
            {
                actions.push(MixerAction::Remove(index));
            }
            let bypass = view.bypass;
            if ui
                .add(
                    egui::Button::new(
                        RichText::new(if bypass { "BYP" } else { "ON" })
                            .size(ts(7.5))
                            .color(if bypass { DIM } else { AMBER }),
                    )
                    .fill(if bypass {
                        Color32::TRANSPARENT
                    } else {
                        with_alpha(AMBER, 16)
                    })
                    .stroke(Stroke::new(
                        1.0,
                        if bypass {
                            BORDER_SOFT
                        } else {
                            with_alpha(AMBER, 45)
                        },
                    ))
                    .corner_radius(CornerRadius::same(5)),
                )
                .on_hover_text(if bypass {
                    "Enable this insert"
                } else {
                    "Bypass this insert"
                })
                .clicked()
            {
                actions.push(MixerAction::Bypass(index, !bypass));
            }
            if ui
                .add_enabled(
                    view.error.is_none(),
                    egui::Button::new(RichText::new("CFG").size(ts(7.5)).color(BLUE))
                        .fill(with_alpha(BLUE, 14))
                        .stroke(Stroke::new(1.0, with_alpha(BLUE, 45)))
                        .corner_radius(CornerRadius::same(5)),
                )
                .on_hover_text("Open this plugin's controls, or its own interface when available")
                .clicked()
            {
                actions.push(MixerAction::Configure(index));
            }
        });
    });
    ui.add_space(3.0);
    ui.label(
        RichText::new(ellipsize(&view.name, 21))
            .size(ts(10.0))
            .color(if view.bypass { DIM } else { TEXT })
            .strong(),
    );
    ui.label(
        RichText::new(ellipsize(
            if view.vendor.is_empty() {
                &view.category
            } else {
                &view.vendor
            },
            24,
        ))
        .size(ts(8.0))
        .color(if view.error.is_some() { RED } else { DIM }),
    )
    .on_hover_text(view.error.clone().unwrap_or_else(|| view.path.clone()));
    // The mix control stays at the foot of the card when the stage is taller
    // than the text above it needs.
    ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
        ui.add_space(3.0);
        let mut mix = view.mix;
        if styled_slider(ui, &mut mix, 0.0, 1.0, 0.01, AMBER).changed() {
            actions.push(MixerAction::Mix(index, mix));
        }
        ui.horizontal(|ui| {
            ui.label(mono("MIX", DIM, 7.5));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(mono(
                    format!("{}%", (view.mix * 100.0).round() as i32),
                    MUTED,
                    8.0,
                ));
            });
        });
    });
}

/// Keeps a card's contents inside its rounded border.
fn clip(rect: Rect) -> Rect {
    rect.shrink(8.0)
}
