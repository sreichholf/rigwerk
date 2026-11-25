//! The effect sheet's body (`docs/ui-spec.md` §6.1): what the effect is,
//! its state and latency, *Wet*, *Editor*, *Reset to defaults* and
//! *Remove* above the controls the plugin reports.
//!
//! The header above (`sheets.rs`) already holds the effect's *On* /
//! *Bypass*.  A plugin that draws its own controls, reports none, or did
//! not load says so here instead of showing an empty list.  Every change
//! goes through a `RigWerkApp` method: the sheet never calls the engine.

use eframe::egui;
use egui::{Align, CornerRadius, Layout, RichText, Sense, Stroke, Vec2};

use crate::app::paths::EffectKey;
use crate::app::screen::format_ms;
use crate::app::RigWerkApp;
use crate::audio::PluginParamView;
use crate::ui::fonts::Face;
use crate::ui::theme::{font, palette, Palette, TypeRole};
use crate::ui::widgets::{row_info, styled_slider, SliderStyle, TextField, CONTROL_H};

use crate::ui::widgets::OutlineButton;

const LABEL_W: f32 = 220.0;
const VALUE_W: f32 = 92.0;
const ROW_H: f32 = 28.0;
/// The wet/dry default: the effect's own output.
const WET_DEFAULT: f32 = 100.0;

pub(crate) const STATE_ACTIVE: &str = "Active";
pub(crate) const STATE_BYPASSED: &str = "Bypassed";
pub(crate) const STATE_FAILED: &str = "Didn't load";

pub(crate) const NO_CONTROLS: &str = "This effect reports no controls.";
pub(crate) const OWN_CONTROLS: &str = "This effect draws its own controls.";
pub(crate) const FAILED_CONTROLS: &str = "This effect did not load.";

pub(crate) const WET_HINT: &str = "How much of the effect you hear. Its default is 100 %.";
pub(crate) const RESET_HINT: &str = "Put every control back to the plugin's own default.";
pub(crate) const REMOVE_HINT: &str = "Take this effect out of the chain. Its controls are lost.";

/// The state word and whether it is an error.
pub(crate) fn state_word(bypass: bool, failed: bool) -> &'static str {
    if failed {
        STATE_FAILED
    } else if bypass {
        STATE_BYPASSED
    } else {
        STATE_ACTIVE
    }
}

/// The value a control shows, the plugin's own display first.
pub(crate) fn control_value(parameter: &PluginParamView) -> String {
    if !parameter.display.trim().is_empty() {
        parameter.display.clone()
    } else if parameter.unit.trim().is_empty() {
        format!("{:.0} %", parameter.value * 100.0)
    } else {
        format!("{:.2} {}", parameter.value, parameter.unit)
    }
}

impl RigWerkApp {
    /// The effect sheet body for effect `index` of path `path`.
    pub(crate) fn effect_sheet(&mut self, ui: &mut egui::Ui, path: usize, index: usize) {
        let views = self.engine_plugin_views(path);
        let Some(view) = views.get(index) else {
            return;
        };
        let name = view.name.clone();
        let format = view.format.label().to_owned();
        let vendor = view.vendor.clone();
        let category = view.category.clone();
        let error = view.error.clone();
        let bypass = view.bypass;
        let has_editor = view.has_editor;
        let parameters = view.parameters.clone();
        let latency = view.latency;
        let rate = if self.snapshot.output_rate > 0 {
            self.snapshot.output_rate
        } else {
            self.sample_rate.max(1)
        };
        let colours = palette(ui.ctx());
        let key = EffectKey {
            path,
            effect: index,
        };

        let state = state_word(bypass, error.is_some());
        let mut mix = view.mix * 100.0;
        let mut reset = false;
        let mut remove = false;
        let mut editor = false;
        let mut changes: Vec<(u32, f64)> = Vec::new();

        ui.horizontal(|ui| {
            ui.set_min_height(CONTROL_H);
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.label(
                RichText::new(format!("{format} · {vendor} · {category}"))
                    .font(font(Face::Sans, TypeRole::Secondary))
                    .color(colours.text3),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if OutlineButton::new("Remove")
                    .show(ui)
                    .on_hover_text(REMOVE_HINT)
                    .clicked()
                {
                    remove = true;
                }
                if OutlineButton::new("Reset to defaults")
                    .enabled(!parameters.is_empty())
                    .show(ui)
                    .on_hover_text(RESET_HINT)
                    .clicked()
                {
                    reset = true;
                }
                let label = if has_editor { "Editor" } else { "Controls" };
                if OutlineButton::new(label)
                    .enabled(error.is_none() && has_editor)
                    .show(ui)
                    .on_hover_text(if has_editor {
                        "Open the editor the plugin draws itself"
                    } else {
                        "This effect has no editor of its own"
                    })
                    .clicked()
                {
                    editor = true;
                }
            });
        });
        ui.add_space(6.0);

        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 14.0;
            let ink = if error.is_some() {
                colours.error_text
            } else if bypass {
                colours.text3
            } else {
                colours.accent_text
            };
            ui.label(
                RichText::new(state)
                    .font(font(Face::SansMedium, TypeRole::Body))
                    .color(ink),
            );
            let latency = if latency == 0 {
                "No latency".to_owned()
            } else {
                format!(
                    "Latency {}",
                    format_ms(latency as f32 * 1000.0 / rate as f32)
                )
            };
            ui.label(
                RichText::new(latency)
                    .font(font(Face::Mono, TypeRole::Secondary))
                    .color(colours.text2),
            )
            .on_hover_text(
                "Added delay while the effect processes, in samples and at the current rate",
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new(format!("{mix:.0} %"))
                        .font(font(Face::Mono, TypeRole::Secondary))
                        .color(if (mix - WET_DEFAULT).abs() < 0.5 {
                            colours.text2
                        } else {
                            colours.accent_text
                        }),
                );
            });
        });
        ui.add_space(2.0);
        let mut wet = mix;
        let slider_w = (ui.available_width() - 120.0).max(80.0);
        let wet_text = format!("{wet:.0} %");
        styled_slider(
            ui,
            &mut wet,
            0.0,
            100.0,
            1.0,
            SliderStyle::named(colours.accent, "Wet")
                .value(wet_text)
                .width(slider_w),
        )
        .on_hover_text(WET_HINT);
        mix = wet;

        if let Some(error) = &error {
            ui.add_space(8.0);
            ui.label(
                RichText::new(error)
                    .font(font(Face::Sans, TypeRole::Body))
                    .color(colours.error_text),
            )
            .on_hover_text("The plugin reported this while it was loading");
        }

        ui.add_space(10.0);
        if parameters.is_empty() {
            ui.add_space(6.0);
            ui.label(
                RichText::new(if error.is_some() {
                    FAILED_CONTROLS
                } else if has_editor {
                    OWN_CONTROLS
                } else {
                    NO_CONTROLS
                })
                .font(font(Face::SansMedium, TypeRole::Body))
                .color(if error.is_some() {
                    colours.error_text
                } else {
                    colours.text2
                }),
            );
        } else {
            controls_header(ui, parameters.len(), &mut self.plugin_param_filter, colours);
            let needle = self.plugin_param_filter.to_lowercase();
            let shown: Vec<&PluginParamView> = parameters
                .iter()
                .filter(|parameter| {
                    needle.is_empty() || parameter.name.to_lowercase().contains(&needle)
                })
                .collect();
            ui.add_space(4.0);
            if shown.is_empty() {
                ui.label(
                    RichText::new(format!(
                        "No control matches “{}”.",
                        self.plugin_param_filter.trim()
                    ))
                    .font(font(Face::Sans, TypeRole::Body))
                    .color(colours.text3),
                );
            } else {
                for parameter in shown {
                    ui.push_id(parameter.id, |ui| {
                        if let Some(value) = control_row(ui, parameter, colours) {
                            changes.push((parameter.id, value));
                        }
                    });
                }
            }
        }

        if remove {
            self.remove_effect(path, index);
            return;
        }
        if reset {
            self.reset_plugin_parameters(key);
        }
        if editor {
            if has_editor {
                self.open_plugin_editor(index);
            } else {
                self.open_sheet(crate::app::screen::Sheet::Effect { path, index });
            }
        }
        if (mix - view.mix * 100.0).abs() > 0.25 {
            self.set_plugin_mix(index, mix / 100.0);
        }
        for (id, value) in changes {
            self.set_plugin_parameter(key, id, value);
        }
        let _ = name;
    }
}

/// *Controls n* and the filter field from eight controls on.
fn controls_header(ui: &mut egui::Ui, count: usize, filter: &mut String, colours: &Palette) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("Controls {count}"))
                .font(font(Face::SansMedium, TypeRole::Secondary))
                .color(colours.text3),
        );
        if count >= 8 {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                TextField::new(filter, "Find a control")
                    .hint("Find a control…")
                    .width(220.0)
                    .show(ui);
            });
        }
    });
}

/// One control: name, its switch or slider, and the value the plugin
/// shows.  `None` while it is unchanged.
fn control_row(ui: &mut egui::Ui, parameter: &PluginParamView, colours: &Palette) -> Option<f64> {
    let mut changed = None;
    ui.allocate_ui_with_layout(
        Vec2::new(ui.available_width(), ROW_H),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.set_height(ROW_H);
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.allocate_ui_with_layout(
                Vec2::new(LABEL_W, ROW_H),
                Layout::left_to_right(Align::Center),
                |ui| {
                    ui.set_width(LABEL_W);
                    let name_font = font(Face::Sans, TypeRole::Body);
                    let shown = crate::ui::widgets::fit_text(
                        ui,
                        &parameter.name,
                        &name_font,
                        (LABEL_W - 4.0).max(8.0),
                    );
                    ui.add(
                        egui::Label::new(RichText::new(shown).font(name_font).color(
                            if parameter.read_only {
                                colours.text3
                            } else {
                                colours.text2
                            },
                        ))
                        .truncate(),
                    )
                    .on_hover_text(&parameter.name);
                },
            );
            if parameter.step_count == 1 {
                let on = parameter.value >= 0.5;
                let (fill, stroke, ink) = if on {
                    (colours.accent_bg, colours.accent, colours.accent_text)
                } else {
                    (colours.slot, colours.line2, colours.text2)
                };
                let width = (ui.available_width() - VALUE_W - 8.0).max(40.0);
                let (rect, response) =
                    ui.allocate_exact_size(Vec2::new(width, ROW_H), Sense::click());
                ui.painter().rect_filled(rect, CornerRadius::same(4), fill);
                ui.painter().rect_stroke(
                    rect,
                    CornerRadius::same(4),
                    Stroke::new(1.0, stroke),
                    egui::StrokeKind::Inside,
                );
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    if on { "On" } else { "Off" },
                    font(Face::SansMedium, TypeRole::Body),
                    ink,
                );
                row_info(
                    &response,
                    egui::Role::CheckBox,
                    !parameter.read_only,
                    &parameter.name,
                    Some(on),
                    None,
                );
                if response.clicked() && !parameter.read_only {
                    changed = Some(if on { 0.0 } else { 1.0 });
                }
            } else {
                let step = if parameter.step_count > 1 {
                    1.0 / f64::from(parameter.step_count)
                } else {
                    0.002
                };
                let mut value = parameter.value as f32;
                let slider_w = (ui.available_width() - VALUE_W - 8.0).max(80.0);
                let response = ui
                    .add_enabled_ui(!parameter.read_only, |ui| {
                        styled_slider(
                            ui,
                            &mut value,
                            0.0,
                            1.0,
                            step as f32,
                            SliderStyle::named(colours.accent, parameter.name.as_str())
                                .value(control_value(parameter))
                                .width(slider_w),
                        )
                    })
                    .inner;
                if response.changed() {
                    changed = Some(f64::from(value));
                }
            }
            ui.allocate_ui_with_layout(
                Vec2::new(VALUE_W, ROW_H),
                Layout::right_to_left(Align::Center),
                |ui| {
                    ui.set_width(VALUE_W);
                    let shown = control_value(parameter);
                    ui.add(
                        egui::Label::new(
                            RichText::new(&shown)
                                .font(font(Face::Mono, TypeRole::Secondary))
                                .color(if (parameter.value - parameter.default).abs() > 0.0005 {
                                    colours.accent_text
                                } else {
                                    colours.text2
                                }),
                        )
                        .truncate(),
                    )
                    .on_hover_text(format!(
                        "{shown}\nDefault {:.0} %",
                        parameter.default * 100.0
                    ));
                },
            );
        },
    );
    changed
}

#[cfg(test)]
mod tests;
