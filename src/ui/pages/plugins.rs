//! Plugins: browse installed VST3 and CLAP effects and build the master insert chain.

use eframe::egui;
use egui::{
    Align, Align2, Color32, CornerRadius, FontFamily, FontId, Layout, Margin, RichText, Sense,
    Stroke, Vec2,
};

use crate::app::plugins::PluginCandidate;
use crate::app::RigWerkApp;
use crate::audio::{PluginParamView, PluginView};
use crate::ui::theme::ts;
use crate::ui::theme::{
    with_alpha, AMBER, BLUE, BORDER, BORDER_SOFT, CARD, CARD_DEEP, DIM, GREEN, MUTED, RED, TEAL,
    TEXT,
};
use crate::ui::widgets::{
    arrow_button, card_fixed, ellipsize, ghost_button, horizontal_line, micro_label, mono,
    status_dot, styled_slider,
};

#[derive(Clone, Copy)]
enum ChainAction {
    Bypass(usize, bool),
    Mix(usize, f32),
    Configure(usize),
    Up(usize),
    Down(usize),
    Remove(usize),
}

fn library_row(ui: &mut egui::Ui, candidate: &PluginCandidate, in_chain: bool) -> bool {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 58.0), Sense::click());
    let fill = if response.hovered() && !in_chain {
        with_alpha(TEAL, 10)
    } else {
        CARD_DEEP
    };
    ui.painter().rect_filled(rect, CornerRadius::same(8), fill);
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(8),
        Stroke::new(
            1.0,
            if in_chain {
                with_alpha(TEAL, 35)
            } else {
                BORDER_SOFT
            },
        ),
        egui::StrokeKind::Inside,
    );
    let icon = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 22.0, rect.center().y),
        Vec2::splat(20.0),
    );
    ui.painter()
        .rect_filled(icon, CornerRadius::same(5), with_alpha(TEAL, 14));
    ui.painter().text(
        icon.center(),
        Align2::CENTER_CENTER,
        candidate.format.short(),
        FontId::new(ts(7.5), FontFamily::Monospace),
        TEAL,
    );
    ui.painter().text(
        rect.left_center() + Vec2::new(40.0, -8.0),
        Align2::LEFT_CENTER,
        ellipsize(&candidate.name, 42),
        FontId::new(ts(11.0), FontFamily::Proportional),
        TEXT,
    );
    let detail = if candidate.vendor.trim().is_empty() {
        candidate.category.clone()
    } else {
        format!("{} · {}", candidate.vendor, candidate.category)
    };
    ui.painter().text(
        rect.left_center() + Vec2::new(40.0, 8.0),
        Align2::LEFT_CENTER,
        ellipsize(&detail, 52),
        FontId::new(ts(8.3), FontFamily::Proportional),
        DIM,
    );
    let label = if in_chain { "IN CHAIN" } else { "+ ADD" };
    let color = if in_chain { DIM } else { TEAL };
    let pill = egui::Rect::from_center_size(
        egui::pos2(rect.right() - 44.0, rect.center().y),
        Vec2::new(60.0, 22.0),
    );
    ui.painter()
        .rect_filled(pill, CornerRadius::same(6), with_alpha(color, 14));
    ui.painter().rect_stroke(
        pill,
        CornerRadius::same(6),
        Stroke::new(1.0, with_alpha(color, 45)),
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        pill.center(),
        Align2::CENTER_CENTER,
        label,
        FontId::new(ts(8.0), FontFamily::Monospace),
        color,
    );
    response.clicked() && !in_chain
}

fn chain_row(ui: &mut egui::Ui, index: usize, view: &PluginView) -> Option<ChainAction> {
    let mut action = None;
    egui::Frame::new()
        .fill(CARD_DEEP)
        .stroke(Stroke::new(
            1.0,
            if view.error.is_some() {
                with_alpha(RED, 55)
            } else if view.bypass {
                BORDER_SOFT
            } else {
                with_alpha(AMBER, 42)
            },
        ))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(9, 7))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(mono(format!("{:02}", index + 1), DIM, 8.5));
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new(ellipsize(&view.name, 34))
                            .size(ts(10.5))
                            .color(if view.bypass { DIM } else { TEXT })
                            .strong(),
                    );
                    ui.label(
                        RichText::new(if view.error.is_some() {
                            view.error.clone().unwrap_or_default()
                        } else if view.vendor.is_empty() {
                            format!("{} · {}", view.format.label(), view.category)
                        } else {
                            format!(
                                "{} · {} · {}",
                                view.format.label(),
                                view.vendor,
                                view.category
                            )
                        })
                        .size(ts(8.2))
                        .color(if view.error.is_some() {
                            RED
                        } else {
                            DIM
                        }),
                    );
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add(
                            egui::Button::new(RichText::new("×").size(ts(13.0)).color(DIM))
                                .fill(Color32::TRANSPARENT)
                                .stroke(Stroke::NONE),
                        )
                        .on_hover_text("Remove this insert")
                        .clicked()
                    {
                        action = Some(ChainAction::Remove(index));
                    }
                    if arrow_button(ui, false, MUTED)
                        .on_hover_text("Move later in the chain")
                        .clicked()
                    {
                        action = Some(ChainAction::Down(index));
                    }
                    if arrow_button(ui, true, MUTED)
                        .on_hover_text("Move earlier in the chain")
                        .clicked()
                    {
                        action = Some(ChainAction::Up(index));
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
                        action = Some(ChainAction::Bypass(index, !bypass));
                    }
                    if ui
                        .add_enabled(
                            view.error.is_none(),
                            egui::Button::new(RichText::new("CFG").size(ts(7.5)).color(BLUE))
                                .fill(with_alpha(BLUE, 14))
                                .stroke(Stroke::new(1.0, with_alpha(BLUE, 45)))
                                .corner_radius(CornerRadius::same(5)),
                        )
                        .on_hover_text(
                            "Open this plugin's controls, or its own interface when available",
                        )
                        .clicked()
                    {
                        action = Some(ChainAction::Configure(index));
                    }
                });
            });
            ui.add_space(2.0);
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
            let mut mix = view.mix;
            if styled_slider(ui, &mut mix, 0.0, 1.0, 0.01, AMBER).changed() {
                action = Some(ChainAction::Mix(index, mix));
            }
        });
    action
}

/// One control in the configure panel.  Compact enough that a long plugin
/// list still reads as a list rather than a wall of default sliders.
fn parameter_card(ui: &mut egui::Ui, parameter: &PluginParamView, accent: Color32) -> Option<f64> {
    let mut changed = None;
    let modified = (parameter.value - parameter.default).abs() > 0.0005;
    egui::Frame::new()
        .fill(CARD_DEEP)
        .stroke(Stroke::new(
            1.0,
            if modified {
                with_alpha(accent, 38)
            } else {
                BORDER_SOFT
            },
        ))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(10, 7))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(ellipsize(&parameter.name, 30))
                        .size(ts(9.8))
                        .color(if parameter.read_only { DIM } else { TEXT }),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if parameter.step_count == 1 {
                        let on = parameter.value >= 0.5;
                        let color = if on { accent } else { DIM };
                        let clicked = ui
                            .add_enabled(
                                !parameter.read_only,
                                egui::Button::new(
                                    RichText::new(if on { "ON" } else { "OFF" })
                                        .size(ts(7.5))
                                        .color(color),
                                )
                                .fill(if on {
                                    with_alpha(accent, 16)
                                } else {
                                    Color32::TRANSPARENT
                                })
                                .stroke(Stroke::new(1.0, with_alpha(color, 45)))
                                .corner_radius(CornerRadius::same(5))
                                .min_size(Vec2::new(34.0, 18.0)),
                            )
                            .clicked();
                        if clicked {
                            changed = Some(if on { 0.0 } else { 1.0 });
                        }
                    } else {
                        let shown = if parameter.display.trim().is_empty() {
                            if parameter.unit.trim().is_empty() {
                                format!("{:.0}%", parameter.value * 100.0)
                            } else {
                                format!("{:.2} {}", parameter.value, parameter.unit)
                            }
                        } else {
                            parameter.display.clone()
                        };
                        ui.label(
                            RichText::new(ellipsize(&shown, 20))
                                .family(FontFamily::Monospace)
                                .size(ts(8.2))
                                .color(if modified { accent } else { MUTED }),
                        )
                        .on_hover_text(format!("Default: {:.0}%", parameter.default * 100.0));
                    }
                });
            });
            if parameter.step_count != 1 {
                ui.add_space(2.0);
                let mut value = parameter.value as f32;
                let step = if parameter.step_count > 1 {
                    1.0 / parameter.step_count as f32
                } else {
                    0.002
                };
                let response = ui
                    .add_enabled_ui(!parameter.read_only, |ui| {
                        styled_slider(ui, &mut value, 0.0, 1.0, step, accent)
                    })
                    .inner;
                if response.changed() {
                    changed = Some(value as f64);
                }
            }
        });
    changed
}

impl RigWerkApp {
    /// The control panel for one insert.  It is a compact, centred card: the
    /// plugin's parameters on top, and a way into the plugin's own editor when
    /// it has one.
    pub(crate) fn plugin_editor_window(&mut self, ctx: &egui::Context) {
        let Some(index) = self.plugin_editor else {
            return;
        };
        let views = self.engine.plugin_views();
        let Some(view) = views.get(index) else {
            self.plugin_editor = None;
            self.plugin_gui = None;
            return;
        };
        let name = view.name.clone();
        let vendor = view.vendor.clone();
        let category = view.category.clone();
        let format = view.format;
        let error = view.error.clone();
        let bypass = view.bypass;
        let has_editor = view.has_editor;
        let parameters = view.parameters.clone();
        let gui_is_open = self
            .plugin_gui
            .as_ref()
            .is_some_and(|gui| gui.index == index);

        let mut open = true;
        let mut close = false;
        let mut reset = false;
        let mut toggle_bypass = false;
        let mut open_gui = false;
        let mut changes: Vec<(u32, f64)> = Vec::new();
        let escape =
            ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape));

        egui::Window::new("Plugin controls")
            .id(egui::Id::unique("rigwerk_plugin_controls"))
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .movable(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .default_width(548.0)
            .frame(
                egui::Frame::new()
                    .fill(CARD)
                    .stroke(Stroke::new(1.0, BORDER))
                    .corner_radius(CornerRadius::same(14))
                    .inner_margin(Margin::same(18))
                    .shadow(egui::epaint::Shadow {
                        offset: [0, 10],
                        blur: 28,
                        spread: 0,
                        color: Color32::from_black_alpha(120),
                    }),
            )
            .open(&mut open)
            .show(ctx, |ui| {
                ui.set_width(548.0);

                // Header: the effect's identity, then a quiet close button.
                ui.horizontal(|ui| {
                    let (icon, _) = ui.allocate_exact_size(Vec2::splat(34.0), Sense::hover());
                    ui.painter()
                        .rect_filled(icon, CornerRadius::same(9), with_alpha(TEAL, 14));
                    ui.painter().rect_stroke(
                        icon,
                        CornerRadius::same(9),
                        Stroke::new(1.0, with_alpha(TEAL, 38)),
                        egui::StrokeKind::Inside,
                    );
                    ui.painter().text(
                        icon.center(),
                        Align2::CENTER_CENTER,
                        format.short(),
                        FontId::new(ts(9.0), FontFamily::Monospace),
                        TEAL,
                    );
                    ui.add_space(2.0);
                    ui.vertical(|ui| {
                        ui.add_space(1.0);
                        ui.label(
                            RichText::new(ellipsize(&name, 40))
                                .size(ts(15.0))
                                .strong()
                                .color(TEXT),
                        );
                        let detail = if vendor.trim().is_empty() {
                            category.clone()
                        } else {
                            format!("{vendor} · {category}")
                        };
                        ui.label(
                            RichText::new(ellipsize(&detail, 52))
                                .size(ts(9.2))
                                .color(DIM),
                        );
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new("×").size(ts(15.0)).color(MUTED),
                                )
                                .fill(Color32::TRANSPARENT)
                                .stroke(Stroke::NONE)
                                .min_size(Vec2::new(24.0, 24.0)),
                            )
                            .on_hover_text("Close this panel")
                            .clicked()
                        {
                            close = true;
                        }
                    });
                });

                ui.add_space(11.0);
                horizontal_line(ui, BORDER_SOFT);
                ui.add_space(10.0);

                // Status, bypass, and the way into the plugin's own interface.
                let (status_text, status_color) = if error.is_some() {
                    ("PROBLEM", RED)
                } else if bypass {
                    ("BYPASSED", DIM)
                } else {
                    ("ACTIVE", AMBER)
                };
                ui.horizontal(|ui| {
                    status_dot(ui, status_color);
                    ui.label(mono(status_text, status_color, 7.8));
                    ui.add_space(4.0);
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new(if bypass { "Enable" } else { "Bypass" })
                                    .size(ts(8.2))
                                    .color(if bypass { TEXT } else { MUTED }),
                            )
                            .fill(if bypass {
                                with_alpha(TEAL, 12)
                            } else {
                                Color32::TRANSPARENT
                            })
                            .stroke(Stroke::new(
                                1.0,
                                if bypass { with_alpha(TEAL, 40) } else { BORDER_SOFT },
                            ))
                            .corner_radius(CornerRadius::same(6))
                            .min_size(Vec2::new(54.0, 22.0)),
                        )
                        .on_hover_text(if bypass {
                            "Let this insert process audio again"
                        } else {
                            "Leave the insert in place but pass audio through"
                        })
                        .clicked()
                    {
                        toggle_bypass = true;
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if has_editor {
                            if ui
                                .add(
                                    egui::Button::new(
                                        RichText::new("Open plugin interface")
                                            .size(ts(9.0))
                                            .color(if gui_is_open { MUTED } else { BLUE }),
                                    )
                                    .fill(with_alpha(BLUE, if gui_is_open { 6 } else { 14 }))
                                    .stroke(Stroke::new(
                                        1.0,
                                        with_alpha(BLUE, if gui_is_open { 22 } else { 45 }),
                                    ))
                                    .corner_radius(CornerRadius::same(6))
                                    .min_size(Vec2::new(150.0, 26.0)),
                                )
                                .on_hover_text(
                                    "Open the interface the plugin draws itself. Changes made there are saved with this insert too.",
                                )
                                .clicked()
                            {
                                open_gui = true;
                            }
                        }
                    });
                });

                if let Some(error) = &error {
                    ui.add_space(9.0);
                    egui::Frame::new()
                        .fill(with_alpha(RED, 10))
                        .stroke(Stroke::new(1.0, with_alpha(RED, 38)))
                        .corner_radius(CornerRadius::same(7))
                        .inner_margin(Margin::symmetric(10, 7))
                        .show(ui, |ui| {
                            ui.label(RichText::new(error).size(ts(9.2)).color(RED));
                        });
                }

                ui.add_space(10.0);

                if parameters.is_empty() {
                    ui.add_space(18.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            RichText::new(if has_editor {
                                "This effect draws its own controls."
                            } else {
                                "This effect reports no adjustable controls."
                            })
                            .size(ts(11.0))
                            .strong()
                            .color(TEXT),
                        );
                        ui.add_space(4.0);
                        ui.label(
                            RichText::new(if has_editor {
                                "Use Open plugin interface above; its settings are saved with this insert."
                            } else if error.is_some() {
                                "The effect did not load, so there is nothing to control yet."
                            } else {
                                "Its saved defaults still apply."
                            })
                            .size(ts(9.2))
                            .color(MUTED),
                        );
                    });
                    ui.add_space(18.0);
                } else {
                    let needle = self.plugin_param_filter.to_lowercase();
                    let shown: Vec<&PluginParamView> = parameters
                        .iter()
                        .filter(|parameter| {
                            needle.is_empty() || parameter.name.to_lowercase().contains(&needle)
                        })
                        .collect();

                    ui.horizontal(|ui| {
                        ui.label(micro_label(
                            &format!(
                                "{} CONTROL{}",
                                parameters.len(),
                                if parameters.len() == 1 { "" } else { "S" }
                            ),
                            DIM,
                        ));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if parameters.len() > 6 {
                                ui.add_sized(
                                    [190.0, 24.0],
                                    egui::TextEdit::singleline(
                                        &mut self.plugin_param_filter,
                                    )
                                    .hint_text("Find a control…")
                                    .text_color(TEXT),
                                );
                            }
                        });
                    });
                    ui.add_space(6.0);

                    ui.style_mut().spacing.scroll.floating = true;
                    ui.style_mut().spacing.scroll.floating_width = 3.0;
                    ui.style_mut().spacing.scroll.floating_allocated_width = 0.0;
                    egui::ScrollArea::vertical()
                        .id_salt("plugin_parameter_scroll")
                        .max_height(368.0)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.set_width((ui.available_width() - 8.0).max(240.0));
                            if shown.is_empty() {
                                ui.add_space(16.0);
                                ui.vertical_centered(|ui| {
                                    ui.label(
                                        RichText::new(format!(
                                            "No control matches “{}”.",
                                            self.plugin_param_filter.trim()
                                        ))
                                        .size(ts(9.5))
                                        .color(MUTED),
                                    );
                                    if ui
                                        .add(
                                            egui::Button::new(
                                                RichText::new("Clear filter")
                                                    .size(ts(8.5))
                                                    .color(TEAL),
                                            )
                                            .fill(Color32::TRANSPARENT)
                                            .stroke(Stroke::new(1.0, BORDER_SOFT))
                                            .corner_radius(CornerRadius::same(5)),
                                        )
                                        .clicked()
                                    {
                                        self.plugin_param_filter.clear();
                                    }
                                });
                            } else {
                                for parameter in shown {
                                    ui.push_id(parameter.id, |ui| {
                                        if let Some(value) = parameter_card(
                                            ui,
                                            parameter,
                                            if parameter.read_only { DIM } else { AMBER },
                                        ) {
                                            changes.push((parameter.id, value));
                                        }
                                    });
                                    ui.add_space(4.0);
                                }
                            }
                        });
                }

                ui.add_space(10.0);
                horizontal_line(ui, BORDER_SOFT);
                ui.add_space(7.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            !parameters.is_empty(),
                            egui::Button::new(
                                RichText::new("Reset to defaults").size(ts(9.0)).color(AMBER),
                            )
                            .fill(with_alpha(AMBER, 10))
                            .stroke(Stroke::new(1.0, with_alpha(AMBER, 40)))
                            .corner_radius(CornerRadius::same(6))
                            .min_size(Vec2::new(108.0, 26.0)),
                        )
                        .on_hover_text("Put every control back to the plugin's own default")
                        .clicked()
                    {
                        reset = true;
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new("Changes apply live and are saved with this insert.")
                                .size(ts(8.2))
                                .color(DIM),
                        );
                    });
                });
            });

        for (id, value) in changes {
            self.set_plugin_parameter(index, id, value);
        }
        if toggle_bypass {
            self.set_plugin_bypass(index, !bypass);
        }
        if open_gui {
            self.open_plugin_gui(index);
        }
        if reset {
            self.reset_plugin_parameters(index);
        }
        if close || escape || !open {
            self.plugin_editor = None;
            self.close_plugin_gui();
            self.plugin_param_filter.clear();
        }
    }

    pub(crate) fn plugins_page(&mut self, ui: &mut egui::Ui) {
        let views = self.engine.plugin_views();
        let library = self.plugin_browser.library.clone();
        let scanning = self.plugin_browser.scanning;
        let needle = self.plugin_browser.search.to_lowercase();
        let filtered: Vec<PluginCandidate> = library
            .iter()
            .filter(|candidate| {
                needle.is_empty()
                    || candidate.name.to_lowercase().contains(&needle)
                    || candidate.vendor.to_lowercase().contains(&needle)
                    || candidate.category.to_lowercase().contains(&needle)
            })
            .cloned()
            .collect();

        let total = ui.available_width();
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!(
                    "{} plugin{} found",
                    library.len(),
                    if library.len() == 1 { "" } else { "s" }
                ))
                .size(ts(10.5))
                .color(DIM),
            );
            ui.add_space(12.0);
            let mut prefer = self.prefer_plugin_interface;
            if ui
                .checkbox(
                    &mut prefer,
                    RichText::new("Prefer plugin interface")
                        .size(ts(9.5))
                        .color(MUTED),
                )
                .on_hover_text(
                    "When an effect has its own interface that RigWerk can embed, CFG opens it directly instead of the control panel.",
                )
                .changed()
            {
                self.prefer_plugin_interface = prefer;
                self.push_log(
                    if prefer {
                        "Plugin interfaces will open directly when available."
                    } else {
                        "The RigWerk control panel will open first."
                    },
                    if prefer { TEAL } else { MUTED },
                );
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ghost_button(ui, if scanning { "Scanning…" } else { "Rescan" }, 112.0)
                    && !scanning
                {
                    self.begin_plugin_scan();
                }
                ui.add_space(7.0);
                let response = ui.add_sized(
                    [220.0, 26.0],
                    egui::TextEdit::singleline(&mut self.plugin_browser.search)
                        .hint_text("Filter by name or maker…")
                        .text_color(TEXT),
                );
                let _ = response;
            });
        });
        if let Some((notice, at)) = &self.plugin_browser.notice {
            if at.elapsed().as_secs() < 8 {
                ui.add_space(3.0);
                ui.label(RichText::new(notice).size(ts(9.5)).color(
                    if notice.to_lowercase().contains("could not")
                        || notice.to_lowercase().contains("failed")
                    {
                        RED
                    } else {
                        GREEN
                    },
                ));
            }
        }
        ui.add_space(8.0);

        let left_w = (total * 0.56).clamp(420.0, (total - 330.0).max(420.0));
        let right_w = (total - left_w - 8.0).max(300.0);
        let mut add_index: Option<usize> = None;
        let mut actions: Vec<ChainAction> = Vec::new();
        let mut add_path = false;

        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                Vec2::new(left_w, 540.0),
                Layout::top_down(Align::LEFT),
                |ui| {
                    ui.set_min_size(Vec2::new(left_w, 540.0));
                    card_fixed(ui, CARD, BORDER, 540.0, |ui| {
                        ui.set_width(left_w - 32.0);
                        ui.horizontal(|ui| {
                            ui.label(micro_label("PLUGIN LIBRARY", TEAL));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(mono(
                                    format!("{} SHOWN", filtered.len()),
                                    DIM,
                                    8.5,
                                ));
                            });
                        });
                        ui.add_space(3.0);
                        ui.label(
                            RichText::new(
                                "Standard VST3 and CLAP folders are scanned. Click a plugin to add it to the master insert chain; CFG opens its controls and its own interface, when it has one.",
                            )
                            .size(ts(9.5))
                            .color(MUTED),
                        );
                        ui.add_space(7.0);
                        horizontal_line(ui, BORDER_SOFT);
                        ui.add_space(6.0);
                        let view_height = 402.0;
                        egui::ScrollArea::vertical()
                            .id_salt("plugin_library_scroll")
                            .max_height(view_height)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                if scanning && filtered.is_empty() {
                                    ui.add_space(24.0);
                                    ui.horizontal(|ui| {
                                        status_dot(ui, AMBER);
                                        ui.label(
                                            RichText::new("Scanning VST3 and CLAP folders…")
                                                .size(ts(10.0))
                                                .color(MUTED),
                                        );
                                    });
                                } else if filtered.is_empty() {
                                    ui.add_space(20.0);
                                    ui.label(
                                        RichText::new("No plugins found here yet.")
                                            .size(ts(10.5))
                                            .color(MUTED)
                                            .strong(),
                                    );
                                    ui.add_space(3.0);
                                    ui.label(
                                        RichText::new(
                                            "Install a .vst3 or .clap plugin in its standard folder, or add one by path on the right, then rescan.",
                                        )
                                        .size(ts(9.5))
                                        .color(DIM),
                                    );
                                } else {
                                    let in_chain = |candidate: &PluginCandidate| {
                                        views.iter().any(|view| {
                                            view.path
                                                == candidate.path.to_string_lossy().into_owned()
                                                && view.format == candidate.format
                                                && view.class_id == candidate.class_id
                                        })
                                    };
                                    for (index, candidate) in filtered.iter().enumerate() {
                                        if library_row(ui, candidate, in_chain(candidate)) {
                                            add_index = Some(index);
                                        }
                                        ui.add_space(4.0);
                                    }
                                }
                            });
                    });
                },
            );
            ui.allocate_ui_with_layout(
                Vec2::new(right_w, 540.0),
                Layout::top_down(Align::LEFT),
                |ui| {
                    ui.set_min_size(Vec2::new(right_w, 540.0));
                    card_fixed(ui, CARD, BORDER, 540.0, |ui| {
                        ui.set_width(right_w - 32.0);
                        ui.horizontal(|ui| {
                            ui.label(micro_label("MASTER INSERT CHAIN", BLUE));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(mono(
                                    format!("{} ACTIVE", views.iter().filter(|v| !v.bypass && v.error.is_none()).count()),
                                    if views.is_empty() { DIM } else { BLUE },
                                    8.5,
                                ));
                            });
                        });
                        ui.add_space(3.0);
                        ui.label(
                            RichText::new(
                                "Top to bottom, after routing and gain. Bypass keeps a slot in place.",
                            )
                            .size(ts(9.5))
                            .color(MUTED),
                        );
                        ui.add_space(7.0);
                        horizontal_line(ui, BORDER_SOFT);
                        ui.add_space(6.0);
                        egui::ScrollArea::vertical()
                            .id_salt("plugin_chain_scroll")
                            .max_height(356.0)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                if views.is_empty() {
                                    ui.add_space(24.0);
                                    ui.horizontal(|ui| {
                                        status_dot(ui, DIM);
                                        ui.label(
                                            RichText::new("The master bus is dry.")
                                                .size(ts(10.5))
                                                .color(MUTED),
                                        );
                                    });
                                    ui.add_space(3.0);
                                    ui.label(
                                        RichText::new(
                                            "Add an effect from the library and it processes the final mix.",
                                        )
                                        .size(ts(9.5))
                                        .color(DIM),
                                    );
                                } else {
                                    for (index, view) in views.iter().enumerate() {
                                        if let Some(action) = chain_row(ui, index, view) {
                                            actions.push(action);
                                        }
                                        ui.add_space(5.0);
                                    }
                                }
                            });
                        ui.add_space(7.0);
                        horizontal_line(ui, BORDER_SOFT);
                        ui.add_space(7.0);
                        ui.label(micro_label("ADD FROM PATH", DIM));
                        ui.add_space(3.0);
                        ui.horizontal(|ui| {
                            ui.add_sized(
                                [ui.available_width() - 58.0, 26.0],
                                egui::TextEdit::singleline(&mut self.plugin_browser.path_input)
                                    .hint_text("C:\\…\\MyEffect.vst3 or MyEffect.clap")
                                    .text_color(TEXT),
                            );
                            if ui
                                .add_sized(
                                    [50.0, 26.0],
                                    egui::Button::new(
                                        RichText::new("Add").size(ts(10.0)).color(TEAL),
                                    )
                                    .fill(with_alpha(TEAL, 14))
                                    .stroke(Stroke::new(1.0, with_alpha(TEAL, 45)))
                                    .corner_radius(CornerRadius::same(7)),
                                )
                                .clicked()
                            {
                                add_path = true;
                            }
                        });
                    });
                },
            );
        });

        if let Some(index) = add_index {
            if let Some(candidate) = filtered.get(index).cloned() {
                self.add_plugin_candidate(&candidate);
            }
        }
        if add_path {
            self.add_plugin_from_path();
        }
        for action in actions {
            match action {
                ChainAction::Bypass(index, bypass) => self.set_plugin_bypass(index, bypass),
                ChainAction::Mix(index, mix) => self.set_plugin_mix(index, mix),
                ChainAction::Configure(index) => self.open_plugin_editor(index),
                ChainAction::Up(index) => self.move_plugin(index, -1),
                ChainAction::Down(index) => self.move_plugin(index, 1),
                ChainAction::Remove(index) => self.remove_plugin(index),
            }
        }
    }
}
