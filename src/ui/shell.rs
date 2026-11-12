//! The window shell around every page: sidebar and header.

use crate::app::LogTone;
use eframe::egui;
use egui::{Align, Color32, CornerRadius, FontFamily, Layout, RichText, Sense, Stroke, Vec2};

use crate::app::{Page, RigWerkApp};
use crate::audio::EngineState;
use crate::ui::icons::{draw_logo, Icon};
use crate::ui::theme::path_colour;
use crate::ui::theme::ts;
use crate::ui::theme::{
    with_alpha, AMBER, BLUE, BORDER, BORDER_SOFT, CARD_DEEP, CARD_RAISED, DIM, GREEN, MUTED, RED,
    TEAL, TEXT,
};
use crate::ui::widgets::{micro_label, mono, nav_item, nav_path_item, status_dot, status_pill};

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
        // The entries scroll when the window is too short for all of them
        // (four path sub-entries at the minimum height), so the Audio Path
        // card below stays in view.
        let nav_h = (ui.available_height() - 170.0).max(120.0);
        egui::ScrollArea::vertical()
            .id_salt("sidebar_nav")
            .max_height(nav_h)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                if nav_item(
                    ui,
                    Icon::Wave,
                    "Live Bridge",
                    "Bridge",
                    self.page == Page::Live,
                ) {
                    target = Some(Page::Live);
                }
                // With several paths, Mixer gets one sub-entry per path, and the
                // selected path's entry is the highlighted one.
                let paths = self.path_infos();
                let several = paths.len() > 1;
                let mixer_hint = if several {
                    format!("{} paths", paths.len())
                } else {
                    "Chain + EQ".to_owned()
                };
                if nav_item(
                    ui,
                    Icon::Mixer,
                    "Mixer",
                    &mixer_hint,
                    self.page == Page::Mixer && !several,
                ) {
                    target = Some(Page::Mixer);
                }
                if several {
                    for info in &paths {
                        if nav_path_item(
                            ui,
                            path_colour(info.colour_index),
                            &info.name,
                            &info.summary(),
                            self.page == Page::Mixer && info.selected,
                        )
                        .on_hover_text(format!("Edit path {} in the Mixer", info.name))
                        .clicked()
                        {
                            self.select_path(info.slot);
                            target = Some(Page::Mixer);
                        }
                    }
                }
                if nav_item(
                    ui,
                    Icon::Tune,
                    "Tuner",
                    "Chromatic + poly",
                    self.page == Page::Tuner,
                ) {
                    target = Some(Page::Tuner);
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
                if nav_item(
                    ui,
                    Icon::Log,
                    "Diagnostics",
                    "Log",
                    self.page == Page::Diagnostics,
                ) {
                    target = Some(Page::Diagnostics);
                }
            });
        if let Some(page) = target {
            self.page = page;
        }

        let remaining = ui.available_height();
        if remaining > 170.0 {
            ui.add_space((remaining - 164.0).max(16.0));
        }

        // Lock the outer width first. Setting width inside a margined Frame to
        // available_width() grows past the sidebar and clips the right border.
        let path_w = ui.available_width();
        let (path_rect, _) = ui.allocate_exact_size(Vec2::new(path_w, 88.0), Sense::hover());
        ui.painter()
            .rect_filled(path_rect, CornerRadius::same(12), CARD_DEEP);
        ui.painter().rect_stroke(
            path_rect,
            CornerRadius::same(12),
            Stroke::new(1.0, BORDER_SOFT),
            egui::StrokeKind::Inside,
        );
        let mut path = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(path_rect.shrink(12.0))
                .layout(Layout::top_down(Align::LEFT)),
        );
        path.set_clip_rect(path_rect.shrink(12.0));
        path.horizontal(|ui| {
            ui.label(micro_label("AUDIO PATH", DIM));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                status_dot(ui, if self.is_active() { GREEN } else { DIM });
            });
        });
        path.add_space(4.0);
        path.horizontal(|ui| {
            let input_backend = self.active_input().backend.clone();
            let output_backend = self.selected_output().backend.clone();
            ui.label(mono(format!("{input_backend} IN"), TEAL, 10.0));
            ui.label(mono("->", DIM, 10.0));
            ui.label(mono(format!("{output_backend} OUT"), BLUE, 10.0));
        });
        path.add_space(2.0);
        path.label(
            RichText::new("Any input. Any output. Two clocks.")
                .size(ts(9.5))
                .color(MUTED),
        );

        ui.add_space(9.0);
        ui.horizontal(|ui| {
            ui.label(mono(
                format!(
                    "v{} · {}",
                    env!("CARGO_PKG_VERSION"),
                    self.inventory.os.to_lowercase()
                ),
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
                "Signal chain top to bottom · inspector for the selected stage.".to_owned(),
            ),
            Page::Tuner => (
                "Tuner".to_owned(),
                "Chromatic, strobe, and every string at once.".to_owned(),
            ),
            Page::Routing => (
                "Signal Routing".to_owned(),
                "Choose exactly where each channel lands.".to_owned(),
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
                "Browse VST3 and CLAP effects · inserts are edited on the Mixer.".to_owned(),
            ),
            Page::Sessions => (
                "Sessions".to_owned(),
                "Save a device pair and recall it before the next take.".to_owned(),
            ),
            Page::Guide => (
                "Setup Guide".to_owned(),
                "A short path from guitar cable to speakers.".to_owned(),
            ),
            Page::Diagnostics => (
                "Diagnostics".to_owned(),
                "Activity log, scan notes and the bridge status. Copy or export them as text."
                    .to_owned(),
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
                let stopping = self.engine.state == EngineState::Stopping;
                let label = if stopping {
                    "Stopping…"
                } else if active {
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
                if button.clicked() && !stopping {
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
                    EngineState::Stopping => ("Stopping", AMBER),
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
                        if auto_start {
                            LogTone::Teal
                        } else {
                            LogTone::Muted
                        },
                    );
                }
            });
        });
    }
}
