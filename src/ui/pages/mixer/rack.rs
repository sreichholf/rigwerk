//! Mixer rack: vertical signal chain + inspector for the selected stage.

use eframe::egui;
use egui::{
    Align, Color32, CornerRadius, FontFamily, FontId, Layout, RichText, Sense, Stroke, Vec2,
};

use crate::app::settings::{
    INPUT_GAIN_MAX_DB, INPUT_GAIN_MIN_DB, OUTPUT_GAIN_MAX_DB, OUTPUT_GAIN_MIN_DB,
};
use crate::app::{Page, RigWerkApp};
use crate::audio::{EqSlot, GateSlot, PluginView};
use crate::ui::pages::mixer::chain::{
    bypassed_color, eq_summary, gate_summary, output_summary, stage_color, ChainSelection,
};
use crate::ui::pages::mixer::output_devices::OutputDeviceRow;
use crate::ui::theme::ts;
use crate::ui::theme::{
    with_alpha, AMBER, BLUE, BORDER, BORDER_SOFT, CARD, CARD_DEEP, CARD_RAISED, DIM, GREEN, MUTED,
    RED, TEAL, TEXT,
};
use crate::ui::widgets::{
    arrow_button, db_slider, hinted_tab, meter_row, micro_label, mono, styled_slider,
};

const ROW_H: f32 = 58.0;
const PANE_HEAD_H: f32 = 36.0;
const PANE_PAD_X: f32 = 14.0;

pub(super) enum RackAction {
    Bypass(usize, bool),
    Mix(usize, f32),
    Configure(usize),
    Remove(usize),
    Move(usize, isize),
    OpenPlugins,
    OpenLive,
}

impl RigWerkApp {
    pub(crate) fn mixer_rack(&mut self, ui: &mut egui::Ui) {
        let views = self.engine.plugin_views();
        let devices = self.output_device_rows();
        self.chain_selected = self
            .chain_selected
            .clamp(views.len())
            .clamp_output(devices.len());

        let mut actions: Vec<RackAction> = Vec::new();
        let mut select: Option<ChainSelection> = None;
        let total = ui.available_width();
        let height = (ui.available_height() - 8.0).max(420.0);
        let chain_w = (total * 0.32).clamp(280.0, 340.0);

        ui.horizontal(|ui| {
            ui.label(micro_label("MIXER", TEAL));
            ui.label(
                RichText::new("Signal chain · inspector for the selected stage")
                    .size(ts(10.0))
                    .color(DIM),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add(
                        egui::Button::new(RichText::new("+ Add insert").size(ts(9.5)).color(TEAL))
                            .fill(with_alpha(TEAL, 14))
                            .stroke(Stroke::new(1.0, with_alpha(TEAL, 45)))
                            .corner_radius(CornerRadius::same(7)),
                    )
                    .on_hover_text("Browse the plugin library and add an insert")
                    .clicked()
                {
                    actions.push(RackAction::OpenPlugins);
                }
                ui.add_space(8.0);
                ui.label(mono(
                    format!(
                        "{} INSERT{} · {} OUTPUT{}",
                        views.len(),
                        if views.len() == 1 { "" } else { "S" },
                        devices.len(),
                        if devices.len() == 1 { "" } else { "S" }
                    ),
                    DIM,
                    8.5,
                ));
            });
        });
        ui.add_space(8.0);

        egui::Frame::new()
            .fill(CARD)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(CornerRadius::same(14))
            .show(ui, |ui| {
                ui.set_min_height(height);
                ui.horizontal_top(|ui| {
                    ui.allocate_ui_with_layout(
                        Vec2::new(chain_w, height),
                        Layout::top_down(Align::LEFT),
                        |ui| {
                            ui.set_width(chain_w);
                            ui.painter().rect_filled(
                                ui.max_rect(),
                                CornerRadius {
                                    nw: 14,
                                    ne: 0,
                                    sw: 14,
                                    se: 0,
                                },
                                CARD_DEEP,
                            );
                            ui.add_space(2.0);
                            self.chain_pane(ui, &views, &devices, &mut select, &mut actions);
                        },
                    );

                    ui.allocate_ui_with_layout(
                        Vec2::new((total - chain_w - 4.0).max(320.0), height),
                        Layout::top_down(Align::LEFT),
                        |ui| {
                            ui.set_min_width((total - chain_w - 4.0).max(320.0));
                            ui.add_space(2.0);
                            self.inspector_pane(ui, &views, &devices, &mut select, &mut actions);
                        },
                    );
                });
            });

        if let Some(selection) = select {
            self.select_chain(selection);
        }
        for action in actions {
            match action {
                RackAction::Bypass(index, bypass) => self.set_plugin_bypass(index, bypass),
                RackAction::Mix(index, mix) => self.set_plugin_mix(index, mix),
                RackAction::Configure(index) => self.open_plugin_editor(index),
                RackAction::Remove(index) => {
                    self.remove_plugin(index);
                    self.chain_selected = self
                        .chain_selected
                        .after_removal(index, self.engine.plugin_views().len());
                }
                RackAction::Move(index, delta) => {
                    self.move_plugin(index, delta);
                    if let ChainSelection::Plugin(current) = self.chain_selected {
                        let target = (current as isize + delta).clamp(
                            0,
                            self.engine.plugin_views().len().saturating_sub(1) as isize,
                        ) as usize;
                        if current == index {
                            self.chain_selected = ChainSelection::Plugin(target);
                        }
                    }
                }
                RackAction::OpenPlugins => {
                    self.plugin_insert_at = self
                        .chain_selected
                        .plugin_insert_at(self.engine.plugin_views().len());
                    self.page = Page::Plugins;
                }
                RackAction::OpenLive => self.page = Page::Live,
            }
        }
    }

    fn chain_pane(
        &mut self,
        ui: &mut egui::Ui,
        views: &[PluginView],
        devices: &[OutputDeviceRow],
        select: &mut Option<ChainSelection>,
        actions: &mut Vec<RackAction>,
    ) {
        pane_head(ui, "SIGNAL CHAIN", Some("plugs reorder only"));
        ui.add_space(8.0);

        let (pre_summary, pre_on) = eq_summary(&self.eqs[EqSlot::Pre.index()]);
        let (post_summary, post_on) = eq_summary(&self.eqs[EqSlot::Post.index()]);
        let (gate_text, gate_on) = gate_summary(&self.gate);
        let input_summary = format!("{:+.1} dB · gain", self.input_gain);
        let output_summary = output_summary(self.output_gain, self.mute, devices.len());
        let gate_pre = self.gate.slot == GateSlot::Pre;

        let chain_scroll_h = (ui.available_height() - 36.0).max(160.0);
        egui::ScrollArea::vertical()
            .id_salt("mixer_chain_scroll")
            .max_height(chain_scroll_h)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.set_width((ui.available_width() - 8.0).max(200.0));
                ui.add_space(4.0);

                if stage_row(
                    ui,
                    ChainSelection::Input,
                    "Input",
                    &input_summary,
                    true,
                    self.chain_selected == ChainSelection::Input,
                ) {
                    *select = Some(ChainSelection::Input);
                }

                if stage_row(
                    ui,
                    ChainSelection::PreEq,
                    "Pre EQ",
                    &pre_summary,
                    pre_on,
                    self.chain_selected == ChainSelection::PreEq,
                ) {
                    *select = Some(ChainSelection::PreEq);
                }

                if gate_pre
                    && stage_row(
                        ui,
                        ChainSelection::Gate,
                        "Gate",
                        &gate_text,
                        gate_on,
                        self.chain_selected == ChainSelection::Gate,
                    )
                {
                    *select = Some(ChainSelection::Gate);
                }

                let plugins_selected = matches!(
                    self.chain_selected,
                    ChainSelection::Plugins | ChainSelection::Plugin(_)
                );
                let group_h = 28.0;
                let (group_rect, group_response) = ui
                    .allocate_exact_size(Vec2::new(ui.available_width(), group_h), Sense::click());
                if self.chain_selected == ChainSelection::Plugins {
                    ui.painter().rect_filled(
                        group_rect,
                        CornerRadius::same(8),
                        with_alpha(TEAL, 18),
                    );
                    ui.painter().rect_stroke(
                        group_rect,
                        CornerRadius::same(8),
                        Stroke::new(1.0, with_alpha(TEAL, 55)),
                        egui::StrokeKind::Inside,
                    );
                }
                paint_chevron(
                    ui,
                    egui::pos2(group_rect.left() + 12.0, group_rect.center().y),
                    if plugins_selected { TEXT } else { AMBER },
                );
                ui.painter().text(
                    egui::pos2(group_rect.left() + 24.0, group_rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    format!("Plugins · {}", views.len()),
                    FontId::new(ts(10.0), FontFamily::Proportional),
                    if plugins_selected { TEXT } else { AMBER },
                );
                ui.painter().text(
                    egui::pos2(group_rect.right() - 10.0, group_rect.center().y),
                    egui::Align2::RIGHT_CENTER,
                    "reorder",
                    FontId::new(ts(9.5), FontFamily::Proportional),
                    DIM,
                );
                if group_response
                    .on_hover_text("Insert group — reorder plugins below")
                    .clicked()
                {
                    *select = Some(ChainSelection::Plugins);
                }

                ui.indent("plugin_inserts", |ui| {
                    if views.is_empty() {
                        ui.label(RichText::new("No inserts yet").size(ts(10.5)).color(MUTED));
                    } else {
                        for (index, view) in views.iter().enumerate() {
                            if insert_row(
                                ui,
                                index,
                                view,
                                self.chain_selected == ChainSelection::Plugin(index),
                                actions,
                            ) {
                                *select = Some(ChainSelection::Plugin(index));
                            }
                        }
                    }
                    ui.add_space(4.0);
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new("+ add insert").size(ts(10.0)).color(DIM),
                            )
                            .fill(Color32::TRANSPARENT)
                            .stroke(Stroke::new(1.0, BORDER_SOFT))
                            .corner_radius(CornerRadius::same(8))
                            .min_size(Vec2::new(ui.available_width(), 30.0)),
                        )
                        .clicked()
                    {
                        actions.push(RackAction::OpenPlugins);
                    }
                });

                if !gate_pre
                    && stage_row(
                        ui,
                        ChainSelection::Gate,
                        "Gate",
                        &gate_text,
                        gate_on,
                        self.chain_selected == ChainSelection::Gate,
                    )
                {
                    *select = Some(ChainSelection::Gate);
                }

                if stage_row(
                    ui,
                    ChainSelection::PostEq,
                    "Post EQ",
                    &post_summary,
                    post_on,
                    self.chain_selected == ChainSelection::PostEq,
                ) {
                    *select = Some(ChainSelection::PostEq);
                }

                if stage_row(
                    ui,
                    ChainSelection::Output,
                    "Output",
                    &output_summary,
                    !self.mute,
                    self.chain_selected == ChainSelection::Output,
                ) {
                    *select = Some(ChainSelection::Output);
                }
                ui.indent("output_devices", |ui| {
                    self.output_device_children(ui, devices, select);
                });
            });

        ui.add_space(8.0);
        let active = views
            .iter()
            .filter(|view| !view.bypass && view.error.is_none())
            .count();
        ui.horizontal(|ui| {
            ui.add_space(PANE_PAD_X);
            ui.label(
                RichText::new(format!(
                    "6 stages · {active} active insert{} · {} output{}",
                    if active == 1 { "" } else { "s" },
                    devices.len(),
                    if devices.len() == 1 { "" } else { "s" }
                ))
                .size(ts(10.0))
                .color(DIM),
            );
        });
    }

    fn inspector_pane(
        &mut self,
        ui: &mut egui::Ui,
        views: &[PluginView],
        devices: &[OutputDeviceRow],
        select: &mut Option<ChainSelection>,
        actions: &mut Vec<RackAction>,
    ) {
        pane_head(
            ui,
            if matches!(
                self.chain_selected,
                ChainSelection::OutputDevice(_) | ChainSelection::AddOutput
            ) {
                "INSPECTOR · OUTPUT DEVICE"
            } else {
                "INSPECTOR"
            },
            None,
        );
        ui.add_space(8.0);
        let body_h = (ui.available_height() - 8.0).max(120.0);
        let body_w = (ui.available_width() - PANE_PAD_X * 2.0).max(200.0);
        ui.allocate_ui_with_layout(
            Vec2::new(ui.available_width(), body_h),
            Layout::top_down(Align::LEFT),
            |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(PANE_PAD_X);
                    ui.allocate_ui_with_layout(
                        Vec2::new(body_w, body_h),
                        Layout::top_down(Align::LEFT),
                        |ui| {
                            ui.set_width(body_w);
                            egui::ScrollArea::vertical()
                                .id_salt("mixer_inspector_scroll")
                                .max_height(body_h)
                                .auto_shrink([false, true])
                                .show(ui, |ui| {
                                    ui.set_width(body_w);
                                    match self.chain_selected {
                                        ChainSelection::Input => self.input_inspector(ui),
                                        ChainSelection::PreEq => self.eq_inspector(ui, EqSlot::Pre),
                                        ChainSelection::PostEq => {
                                            self.eq_inspector(ui, EqSlot::Post)
                                        }
                                        ChainSelection::Gate => self.gate_inspector(ui),
                                        ChainSelection::Output => {
                                            self.output_inspector(ui, devices, select)
                                        }
                                        ChainSelection::OutputDevice(index) => {
                                            match devices.get(index) {
                                                Some(row) => {
                                                    self.output_device_inspector(ui, row, actions)
                                                }
                                                None => self.output_inspector(ui, devices, select),
                                            }
                                        }
                                        ChainSelection::AddOutput => {
                                            self.add_output_inspector(ui);
                                        }
                                        ChainSelection::Plugins => {
                                            plugins_group_inspector(ui, views, actions);
                                        }
                                        ChainSelection::Plugin(index) => {
                                            if let Some(view) = views.get(index) {
                                                plugin_inspector(ui, index, view, actions);
                                            } else {
                                                plugins_group_inspector(ui, views, actions);
                                            }
                                        }
                                    }
                                });
                        },
                    );
                });
            },
        );
    }

    fn input_inspector(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Input").size(ts(16.0)).color(TEXT).strong());
        ui.label(
            RichText::new("Anchor stage · first in the fixed chain")
                .size(ts(11.0))
                .color(DIM),
        );
        ui.add_space(12.0);
        ui.label(mono(format!("{:+.1} dB", self.input_gain), TEXT, 12.0));
        if db_slider(
            ui,
            &mut self.input_gain,
            INPUT_GAIN_MIN_DB,
            INPUT_GAIN_MAX_DB,
            (TEAL, "Input gain"),
        ) {
            self.engine.set_gains(self.input_gain, self.output_gain);
        }
        ui.add_space(10.0);
        meter_row(ui, "L", self.meter_in_l, self.peak_in_l, TEAL);
        meter_row(ui, "R", self.meter_in_r, self.peak_in_r, TEAL);
    }

    fn output_inspector(
        &mut self,
        ui: &mut egui::Ui,
        devices: &[OutputDeviceRow],
        select: &mut Option<ChainSelection>,
    ) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Output").size(ts(16.0)).color(TEXT).strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let mute = self.mute;
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new(if mute { "MUTED" } else { "MUTE" })
                                .size(ts(9.0))
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
                    .on_hover_text("Silence every output device without stopping the bridge.")
                    .clicked()
                {
                    self.mute = !mute;
                }
            });
        });
        ui.label(
            RichText::new("Master stage · the finished mix, sent to every device below.")
                .size(ts(11.0))
                .color(DIM),
        );
        ui.add_space(12.0);
        ui.label(mono(format!("{:+.1} dB", self.output_gain), TEXT, 12.0));
        if db_slider(
            ui,
            &mut self.output_gain,
            OUTPUT_GAIN_MIN_DB,
            OUTPUT_GAIN_MAX_DB,
            (BLUE, "Output gain"),
        ) {
            self.engine.set_gains(self.input_gain, self.output_gain);
        }
        ui.add_space(10.0);
        meter_row(ui, "L", self.meter_out_l, self.peak_out_l, BLUE);
        meter_row(ui, "R", self.meter_out_r, self.peak_out_r, BLUE);
        ui.add_space(6.0);
        ui.label(
            RichText::new(
                "Mute here silences every device. Per-device level and mute sit on each device.",
            )
            .size(ts(10.5))
            .color(MUTED),
        );
        ui.add_space(12.0);
        self.output_devices_block(ui, devices, select);
    }

    pub(crate) fn select_chain(&mut self, selection: ChainSelection) {
        self.chain_selected = selection;
        if let Some(slot) = selection.eq_slot() {
            self.eq_edit = slot;
        }
    }
}

fn stage_row(
    ui: &mut egui::Ui,
    selection: ChainSelection,
    name: &str,
    summary: &str,
    live: bool,
    selected: bool,
) -> bool {
    let accent = stage_color(selection);
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::click());
    let hovered = response.hovered();
    if selected {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(10), with_alpha(TEAL, 22));
        ui.painter().rect_stroke(
            rect,
            CornerRadius::same(10),
            Stroke::new(1.0, with_alpha(TEAL, 90)),
            egui::StrokeKind::Inside,
        );
    } else if hovered {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(10), with_alpha(TEXT, 8));
    }

    let node = egui::pos2(rect.left() + 14.0, rect.center().y);
    ui.painter()
        .circle_filled(node, 5.0, bypassed_color(live, accent));
    ui.painter().circle_stroke(
        node,
        5.0,
        Stroke::new(
            2.0,
            if selected {
                with_alpha(TEAL, 80)
            } else {
                CARD_DEEP
            },
        ),
    );

    // Title and summary share a vertical stack with a real gap — not pinned
    // to the row's top and bottom edges, which collapses them together.
    let title_h = ts(12.5) * 1.15;
    let summary_h = ts(10.0) * 1.15;
    let stack_gap = 7.0;
    let stack_h = title_h + stack_gap + summary_h;
    let stack_top = rect.center().y - stack_h * 0.5;
    let text_x = rect.left() + 30.0;
    ui.painter().text(
        egui::pos2(text_x, stack_top),
        egui::Align2::LEFT_TOP,
        name,
        FontId::new(ts(12.5), FontFamily::Proportional),
        if live { TEXT } else { DIM },
    );
    ui.painter().text(
        egui::pos2(text_x, stack_top + title_h + stack_gap),
        egui::Align2::LEFT_TOP,
        summary,
        FontId::new(ts(10.0), FontFamily::Proportional),
        DIM,
    );

    let pill = if live { "ON" } else { "BYP" };
    let pill_color = if live { TEAL } else { DIM };
    ui.painter().text(
        egui::pos2(rect.right() - 22.0, rect.center().y),
        egui::Align2::RIGHT_CENTER,
        pill,
        FontId::new(ts(9.0), FontFamily::Monospace),
        pill_color,
    );
    paint_chevron(
        ui,
        egui::pos2(rect.right() - 10.0, rect.center().y),
        if selected { TEAL } else { DIM },
    );

    response.clicked()
}

fn insert_row(
    ui: &mut egui::Ui,
    index: usize,
    view: &PluginView,
    selected: bool,
    actions: &mut Vec<RackAction>,
) -> bool {
    let mut controls_clicked = false;
    let border = if view.error.is_some() {
        with_alpha(RED, 55)
    } else if selected {
        with_alpha(TEAL, 90)
    } else if view.bypass {
        BORDER_SOFT
    } else {
        with_alpha(AMBER, 40)
    };
    // Size the card from the two text lines it holds (plus the gap the
    // vertical stack below puts between them), so neither spills out of it.
    let pad = Vec2::new(8.0, 6.0);
    let title_font = FontId::new(ts(11.5), FontFamily::Proportional);
    let mix_font = FontId::new(ts(9.5), FontFamily::Proportional);
    let text_h = ui.fonts_mut(|fonts| fonts.row_height(&title_font) + fonts.row_height(&mix_font))
        + ui.spacing().item_spacing.y;
    let row_h = (text_h + pad.y * 2.0).max(ui.spacing().interact_size.y + pad.y * 2.0);
    let (rect, row_response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), row_h), Sense::click());
    ui.painter().rect_filled(
        rect,
        CornerRadius::same(8),
        if selected {
            with_alpha(TEAL, 18)
        } else {
            Color32::TRANSPARENT
        },
    );
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(8),
        Stroke::new(1.0, border),
        egui::StrokeKind::Inside,
    );
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(pad))
            .layout(Layout::left_to_right(Align::Center)),
    );
    // `horizontal_centered` spans the full inner height; plain `horizontal`
    // starts as one interact-height line centred in the row, which pushes
    // the two-line stack below the card.
    child.horizontal_centered(|ui| {
        let (grip_rect, grip_response) =
            ui.allocate_exact_size(Vec2::new(14.0, 18.0), Sense::hover());
        paint_grip(ui, grip_rect.center(), DIM);
        let _ = grip_response.on_hover_text("Use the arrows to reorder");
        // Controls first, from the right edge, so the title gets whatever
        // width is left and is truncated to it instead of running under them.
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if remove_button(ui)
                .on_hover_text("Remove this insert")
                .clicked()
            {
                actions.push(RackAction::Remove(index));
                controls_clicked = true;
            }
            ui.separator();
            if arrow_button(ui, false, MUTED)
                .on_hover_text("Move later in the chain")
                .clicked()
            {
                actions.push(RackAction::Move(index, 1));
                controls_clicked = true;
            }
            if arrow_button(ui, true, MUTED)
                .on_hover_text("Move earlier in the chain")
                .clicked()
            {
                actions.push(RackAction::Move(index, -1));
                controls_clicked = true;
            }
            let bypass = view.bypass;
            if hinted_tab(
                ui,
                if bypass { "BYP" } else { "ON" },
                !bypass,
                AMBER,
                if bypass {
                    "Enable this insert"
                } else {
                    "Bypass this insert"
                },
            ) {
                actions.push(RackAction::Bypass(index, !bypass));
                controls_clicked = true;
            }
            ui.vertical(|ui| {
                ui.add(
                    egui::Label::new(
                        RichText::new(format!("{:02} {}", index + 1, view.name))
                            .font(title_font.clone())
                            .color(if view.bypass { DIM } else { TEXT })
                            .strong(),
                    )
                    .truncate(),
                )
                .on_hover_text(view.error.clone().unwrap_or_else(|| view.path.clone()));
                ui.add(
                    egui::Label::new(
                        RichText::new(format!("mix {}%", (view.mix * 100.0).round() as i32))
                            .font(mix_font.clone())
                            .color(DIM),
                    )
                    .truncate(),
                );
            });
        });
    });
    row_response.clicked() && !controls_clicked
}

/// The "×" that removes an insert: dim until hovered, then red, since it is
/// the one destructive control on the row.
fn remove_button(ui: &mut egui::Ui) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::click());
    let hovered = response.hovered();
    if hovered {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(4), with_alpha(RED, 30));
    }
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        "×",
        FontId::new(ts(13.0), FontFamily::Proportional),
        if hovered { RED } else { DIM },
    );
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn plugins_group_inspector(ui: &mut egui::Ui, views: &[PluginView], actions: &mut Vec<RackAction>) {
    ui.label(RichText::new("Plugins").size(ts(16.0)).color(TEXT).strong());
    ui.label(
        RichText::new("Insert group · reorder on the left · library on the Plugins page")
            .size(ts(11.0))
            .color(DIM),
    );
    ui.add_space(12.0);
    let active = views
        .iter()
        .filter(|view| !view.bypass && view.error.is_none())
        .count();
    ui.label(mono(
        format!("{active} / {} active", views.len()),
        TEXT,
        14.0,
    ));
    ui.add_space(12.0);
    if ui
        .add(
            egui::Button::new(RichText::new("+ Add insert").size(ts(11.0)).color(TEAL))
                .fill(with_alpha(TEAL, 14))
                .stroke(Stroke::new(1.0, with_alpha(TEAL, 45)))
                .corner_radius(CornerRadius::same(8))
                .min_size(Vec2::new(160.0, 32.0)),
        )
        .clicked()
    {
        actions.push(RackAction::OpenPlugins);
    }
    ui.add_space(10.0);
    ui.label(
        RichText::new("Select an insert row for bypass, mix, and its native interface.")
            .size(ts(11.0))
            .color(MUTED),
    );
}

fn plugin_inspector(
    ui: &mut egui::Ui,
    index: usize,
    view: &PluginView,
    actions: &mut Vec<RackAction>,
) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("{:02} {}", index + 1, view.name))
                .size(ts(16.0))
                .color(TEXT)
                .strong(),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let bypass = view.bypass;
            if hinted_tab(
                ui,
                if bypass { "BYP" } else { "ON" },
                !bypass,
                AMBER,
                if bypass {
                    "Enable this insert"
                } else {
                    "Bypass this insert"
                },
            ) {
                actions.push(RackAction::Bypass(index, !bypass));
            }
        });
    });
    ui.label(
        RichText::new(if view.vendor.is_empty() {
            format!("{} · host chrome · prefer native UI", view.format.label())
        } else {
            format!(
                "{} · {} · host chrome · prefer native UI",
                view.format.label(),
                view.vendor
            )
        })
        .size(ts(11.0))
        .color(DIM),
    );
    if let Some(error) = &view.error {
        ui.add_space(6.0);
        ui.label(RichText::new(error).size(ts(11.0)).color(RED));
    }
    ui.add_space(14.0);

    ui.horizontal(|ui| {
        ui.label(mono("MIX", DIM, 8.0));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(mono(
                format!("{}%", (view.mix * 100.0).round() as i32),
                MUTED,
                10.0,
            ));
        });
    });
    let mut mix = view.mix;
    if styled_slider(
        ui,
        &mut mix,
        0.0,
        1.0,
        0.01,
        (AMBER, format!("{} mix", view.name)),
    )
    .changed()
    {
        actions.push(RackAction::Mix(index, mix));
    }

    ui.add_space(16.0);
    if ui
        .add_enabled(
            view.error.is_none(),
            egui::Button::new(
                RichText::new(if view.has_editor {
                    "Open plugin interface"
                } else {
                    "Open host parameters"
                })
                .size(ts(12.0))
                .color(BLUE),
            )
            .fill(with_alpha(BLUE, 16))
            .stroke(Stroke::new(1.0, with_alpha(BLUE, 55)))
            .corner_radius(CornerRadius::same(8))
            .min_size(Vec2::new(ui.available_width().min(280.0), 36.0)),
        )
        .on_hover_text(
            "Plugins are edited in their own UI when available. Host parameters are the fallback.",
        )
        .clicked()
    {
        actions.push(RackAction::Configure(index));
    }

    ui.add_space(10.0);
    if ui
        .add(
            egui::Button::new(RichText::new("Remove insert").size(ts(11.0)).color(DIM))
                .fill(CARD_RAISED)
                .stroke(Stroke::new(1.0, BORDER_SOFT))
                .corner_radius(CornerRadius::same(8)),
        )
        .clicked()
    {
        actions.push(RackAction::Remove(index));
    }

    ui.add_space(14.0);
    ui.label(
        RichText::new(
            "The inspector stays thin for inserts on purpose — the native dialog is the editor.",
        )
        .size(ts(11.0))
        .color(MUTED),
    );
}

fn pane_head(ui: &mut egui::Ui, title: &str, hint: Option<&str>) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, PANE_HEAD_H), Sense::hover());
    let inner = rect.shrink2(Vec2::new(PANE_PAD_X, 0.0));
    ui.painter().text(
        egui::pos2(inner.left(), inner.center().y),
        egui::Align2::LEFT_CENTER,
        title,
        FontId::new(ts(9.0), FontFamily::Proportional),
        MUTED,
    );
    if let Some(hint) = hint {
        ui.painter().text(
            egui::pos2(inner.right(), inner.center().y),
            egui::Align2::RIGHT_CENTER,
            hint,
            FontId::new(ts(9.5), FontFamily::Proportional),
            DIM,
        );
    }
    ui.painter().hline(
        inner.x_range(),
        rect.bottom() - 0.5,
        Stroke::new(1.0, BORDER_SOFT),
    );
}

/// Right-pointing chevron — selection opens the inspector to the right, not
/// an accordion below, so it never flips down. Painted as lines because
/// egui's default font has no ▸.
fn paint_chevron(ui: &egui::Ui, center: egui::Pos2, color: Color32) {
    let stroke = Stroke::new(1.3, color);
    let tip = egui::pos2(center.x + 3.0, center.y);
    ui.painter()
        .line_segment([egui::pos2(center.x - 1.5, center.y - 3.5), tip], stroke);
    ui.painter()
        .line_segment([tip, egui::pos2(center.x - 1.5, center.y + 3.5)], stroke);
}

/// Six-dot grip painted as circles — egui's default font has no ⠿.
fn paint_grip(ui: &egui::Ui, center: egui::Pos2, color: Color32) {
    for col in 0..2 {
        for row in 0..3 {
            let x = center.x + (col as f32 - 0.5) * 4.0;
            let y = center.y + (row as f32 - 1.0) * 4.0;
            ui.painter()
                .circle_filled(egui::pos2(x, y), 1.1, with_alpha(color, 200));
        }
    }
}
