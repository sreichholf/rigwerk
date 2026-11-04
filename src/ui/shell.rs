//! The window shell around every page: sidebar and header.

use eframe::egui;
use egui::{Align, Color32, CornerRadius, FontFamily, Layout, Margin, RichText, Stroke};

use crate::app::{Page, RigWerkApp};
use crate::audio::EngineState;
use crate::ui::icons::{draw_logo, Icon};
use crate::ui::theme::ts;
use crate::ui::theme::{
    with_alpha, AMBER, BLUE, BORDER, BORDER_SOFT, CARD_DEEP, CARD_RAISED, DIM, GREEN, MUTED, RED,
    TEAL, TEXT,
};
use crate::ui::widgets::{micro_label, mono, nav_item, status_dot, status_pill};

impl RigWerkApp {
    pub(crate) fn sidebar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            draw_logo(ui);
            ui.add_space(2.0);
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("RIGWERK")
                        .size(ts(15.5))
                        .color(TEXT)
                        .strong()
                        .family(FontFamily::Proportional),
                );
                ui.label(
                    RichText::new("independent bridge")
                        .size(ts(8.5))
                        .color(DIM)
                        .strong(),
                );
            });
        });

        ui.add_space(32.0);
        ui.label(micro_label("WORKSPACE", DIM));
        ui.add_space(8.0);

        let mut target = None;
        if nav_item(
            ui,
            Icon::Wave,
            "Live Bridge",
            "Bridge",
            self.page == Page::Live,
        ) {
            target = Some(Page::Live);
        }
        if nav_item(
            ui,
            Icon::Mixer,
            "Mixer",
            "Chain + EQ",
            self.page == Page::Mixer,
        ) {
            target = Some(Page::Mixer);
        }
        if nav_item(
            ui,
            Icon::Plug,
            "Plugins",
            "VST3 + CLAP",
            self.page == Page::Plugins,
        ) {
            target = Some(Page::Plugins);
        }
        if nav_item(
            ui,
            Icon::Grid,
            "Signal Routing",
            "Routing",
            self.page == Page::Routing,
        ) {
            target = Some(Page::Routing);
        }
        if nav_item(
            ui,
            Icon::Device,
            "Devices",
            "Devices",
            self.page == Page::Devices,
        ) {
            target = Some(Page::Devices);
        }
        if nav_item(
            ui,
            Icon::Layers,
            "Sessions",
            "Sessions",
            self.page == Page::Sessions,
        ) {
            target = Some(Page::Sessions);
        }
        if nav_item(
            ui,
            Icon::Help,
            "Setup Guide",
            "Guide",
            self.page == Page::Guide,
        ) {
            target = Some(Page::Guide);
        }
        if let Some(page) = target {
            self.page = page;
        }

        let remaining = ui.available_height();
        if remaining > 170.0 {
            ui.add_space((remaining - 164.0).max(16.0));
        }

        egui::Frame::new()
            .fill(CARD_DEEP)
            .stroke(Stroke::new(1.0, BORDER_SOFT))
            .corner_radius(CornerRadius::same(12))
            .inner_margin(Margin::same(12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(micro_label("AUDIO PATH", DIM));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        status_dot(ui, if self.is_active() { GREEN } else { DIM });
                    });
                });
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    let input_backend = self.active_input().backend.clone();
                    let output_backend = self.selected_output().backend.clone();
                    ui.label(mono(format!("{input_backend} IN"), TEAL, 10.0));
                    ui.label(mono("→", DIM, 10.0));
                    ui.label(mono(format!("{output_backend} OUT"), BLUE, 10.0));
                });
                ui.add_space(2.0);
                ui.label(
                    RichText::new("Any input. Any output. Two clocks.")
                        .size(ts(9.5))
                        .color(MUTED),
                );
            });

        ui.add_space(9.0);
        ui.horizontal(|ui| {
            ui.label(mono(
                format!("v0.2 · {}", self.inventory.os.to_lowercase()),
                DIM,
                9.0,
            ));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add(
                        egui::Button::new(RichText::new("about").size(ts(9.5)).color(DIM))
                            .fill(Color32::TRANSPARENT)
                            .stroke(Stroke::new(1.0, BORDER_SOFT))
                            .corner_radius(CornerRadius::same(6)),
                    )
                    .clicked()
                {
                    self.show_about = true;
                }
            });
        });
    }

    pub(crate) fn header(&mut self, ui: &mut egui::Ui) {
        let low = self.inventory.low_latency_label.clone();
        let (title, subtitle): (String, String) = match self.page {
            Page::Live => (
                "Live Bridge".to_owned(),
                "Any input on one clock. Any output on another.".to_owned(),
            ),
            Page::Mixer => (
                "Mixer".to_owned(),
                "Input, pre EQ, plugins, post EQ and output, in the order the sound flows."
                    .to_owned(),
            ),
            Page::Routing => (
                "Signal Routing".to_owned(),
                "Choose exactly where each input channel lands.".to_owned(),
            ),
            Page::Devices => (
                "Devices".to_owned(),
                if cfg!(target_os = "windows") {
                    "Every protocol, both directions: ASIO, WASAPI, DirectSound and MME.".to_owned()
                } else {
                    format!("{low} capture, system playback, kept apart.")
                },
            ),
            Page::Plugins => (
                "Plugins".to_owned(),
                "Add VST3 and CLAP effects to the master insert chain.".to_owned(),
            ),
            Page::Sessions => (
                "Sessions".to_owned(),
                "Save a device pair and recall it before the next take.".to_owned(),
            ),
            Page::Guide => (
                "Setup Guide".to_owned(),
                "A short path from guitar cable to speakers.".to_owned(),
            ),
        };

        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new(title)
                        .size(ts(27.0))
                        .strong()
                        .color(TEXT)
                        .family(FontFamily::Proportional),
                );
                ui.label(RichText::new(subtitle).size(ts(11.5)).color(MUTED));
            });

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let active = self.is_active();
                let label = if active {
                    "Stop bridge"
                } else {
                    "Start bridge"
                };
                let fill = if active { CARD_RAISED } else { TEAL };
                let text_color = if active {
                    RED
                } else {
                    Color32::from_rgb(7, 20, 17)
                };
                let button = ui.add_sized(
                    [112.0, 36.0],
                    egui::Button::new(
                        RichText::new(label)
                            .size(ts(12.0))
                            .color(text_color)
                            .strong(),
                    )
                    .fill(fill)
                    .stroke(Stroke::new(
                        1.0,
                        if active {
                            BORDER
                        } else {
                            with_alpha(Color32::WHITE, 30)
                        },
                    ))
                    .corner_radius(CornerRadius::same(9)),
                );
                if button.clicked() {
                    if active {
                        self.stop_bridge();
                    } else {
                        self.start_bridge();
                    }
                }
                ui.add_space(4.0);
                let (state_text, state_color) = match self.engine.state {
                    EngineState::Preview => ("Preview active", AMBER),
                    EngineState::Running => ("Bridge live", GREEN),
                    EngineState::Starting => ("Linking", BLUE),
                    EngineState::Error => ("Needs attention", RED),
                    EngineState::Idle => ("Ready", MUTED),
                };
                status_pill(ui, state_text, state_color);
                ui.add_space(8.0);
                let mut auto_start = self.auto_start_bridge;
                if ui
                    .checkbox(
                        &mut auto_start,
                        RichText::new("auto-start").size(ts(9.5)).color(MUTED),
                    )
                    .on_hover_text(
                        "Start the bridge automatically when RigWerk opens. Off by default.",
                    )
                    .changed()
                {
                    self.auto_start_bridge = auto_start;
                    self.push_log(
                        if auto_start {
                            "Bridge will start automatically next launch."
                        } else {
                            "Bridge will stay stopped when RigWerk opens."
                        },
                        if auto_start { TEAL } else { MUTED },
                    );
                }
            });
        });
    }
}
