//! The window shell: title bar, status bar, error banner, and the sheet
//! and drawer hosts.
//!
//! Classic pages still draw in the middle.  *Classic* reaches the ones
//! that have not been replaced.  Sessions and Diagnostics are the menu
//! and the sheet.

use std::time::Instant;

use eframe::egui;
use egui::{Align, CornerRadius, Layout, Margin, RichText, Stroke, Vec2};

use crate::app::screen::{format_ms, Drawer, SetupSection, Sheet, SHEET_MAX, SHEET_MIN};
use crate::app::settings::rate_label;
use crate::app::{Page, RigWerkApp};
use crate::audio::EngineState;
use crate::ui::drawers::issues::issue_counts;
use crate::ui::theme::{font, palette, Face, TypeRole};

const TITLE_H: f32 = 52.0;
const STATUS_H: f32 = 30.0;
const DRAWER_W: f32 = 460.0;

impl RigWerkApp {
    pub(crate) fn title_bar(&mut self, ui: &mut egui::Ui) {
        let colours = palette(ui.ctx());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.set_height(32.0);
            if ui
                .add(
                    egui::Button::new(
                        RichText::new("RigWerk")
                            .font(font(Face::Sans, TypeRole::Title))
                            .color(colours.text)
                            .strong(),
                    )
                    .fill(egui::Color32::TRANSPARENT)
                    .stroke(Stroke::NONE),
                )
                .on_hover_text("About RigWerk")
                .clicked()
            {
                self.show_about = true;
            }
            self.session_button(ui);
            self.bridge_control(ui);
            self.latency_readout(ui);
            self.record_control(ui);
            self.metronome_control(ui);
            self.screen_button(ui, "Tuner", self.page == Page::Tuner, Page::Tuner);
            let on_setup = self.page == Page::Setup;
            let setup = bar_button(ui.ctx(), "Setup", on_setup);
            if ui.add(setup).on_hover_text("Setup").clicked() {
                if on_setup {
                    self.close_setup();
                } else {
                    self.open_setup(SetupSection::Devices);
                }
            }
            self.mute_button(ui);
            self.classic_menu(ui);
        });
    }

    fn screen_button(&mut self, ui: &mut egui::Ui, label: &str, on: bool, page: Page) {
        let widget = bar_button(ui.ctx(), label, on);
        if ui.add(widget).clicked() {
            self.page = if on { Page::Live } else { page };
        }
    }

    fn bridge_control(&mut self, ui: &mut egui::Ui) {
        let colours = palette(ui.ctx());
        let label = bridge_label(self.engine.state);
        let colour = match self.engine.state {
            EngineState::Running => colours.accent,
            EngineState::Error => colours.error,
            EngineState::Preview => colours.warn,
            _ => colours.text3,
        };
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 4.0, colour);
        ui.label(
            RichText::new(label)
                .font(font(Face::Sans, TypeRole::Control))
                .color(if self.engine.state == EngineState::Running {
                    colours.accent_text
                } else {
                    colours.text
                }),
        );
        let stopping = self.engine.state == EngineState::Stopping;
        let active = self.is_active();
        let verb = if active { "Stop" } else { "Start" };
        let start = bar_button(ui.ctx(), verb, active);
        if ui.add_enabled(!stopping, start).clicked() {
            if active {
                self.stop_bridge();
            } else {
                self.start_bridge();
            }
        }
    }

    fn latency_readout(&mut self, ui: &mut egui::Ui) {
        let colours = palette(ui.ctx());
        let estimate = self.heard_estimate();
        ui.label(
            RichText::new(format!("≈ {}", format_ms(estimate.total_ms)))
                .font(font(Face::Mono, TypeRole::Secondary))
                .color(colours.text2),
        )
        .on_hover_text(self.heard_summary());
    }

    fn mute_button(&mut self, ui: &mut egui::Ui) {
        let (label, on) = if !self.monitor {
            ("Monitor off", true)
        } else if self.mute {
            ("Muted", true)
        } else {
            ("Mute", false)
        };
        let mute = bar_button(ui.ctx(), label, on);
        if ui.add(mute).on_hover_text("Mute the master").clicked() && self.monitor {
            self.mute = !self.mute;
        }
    }

    fn classic_menu(&mut self, ui: &mut egui::Ui) {
        let classic = bar_button(ui.ctx(), "Classic ▸", false);
        let button = ui.add(classic);
        let mut target = None;
        egui::Popup::menu(&button)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
                ui.set_min_width(160.0);
                for (label, page) in CLASSIC {
                    if ui.button(*label).clicked() {
                        target = Some(*page);
                    }
                }
            });
        if let Some(page) = target {
            self.page = page;
        }
    }

    pub(crate) fn error_banner(&mut self, ui: &mut egui::Ui) {
        let colours = palette(ui.ctx());
        let device = self.device_alert.clone();
        let error = self
            .chrome
            .error_title
            .clone()
            .filter(|_| !self.chrome.error_dismissed);
        let Some(text) = device.clone().or(error) else {
            return;
        };
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(text)
                    .font(font(Face::Sans, TypeRole::Body))
                    .color(colours.error_text),
            );
            if device.is_some() {
                if ui.button("Rescan").clicked() {
                    self.rescan();
                }
                if ui.button("Open Setup").clicked() {
                    self.open_setup(SetupSection::Devices);
                }
                if ui.button("Dismiss").clicked() {
                    self.device_alert = None;
                }
            } else if ui.button("Dismiss").clicked() {
                self.dismiss_error_banner();
            }
        });
    }

    pub(crate) fn banner_open(&self) -> bool {
        self.device_alert.is_some()
            || (self.chrome.error_title.is_some() && !self.chrome.error_dismissed)
    }

    /// The status bar: readings on the left, the issue count and *Log* on
    /// the right.  The right side is laid out first, so a long log line is
    /// cut short instead of running under it.
    pub(crate) fn status_bar(&mut self, ui: &mut egui::Ui) {
        let colours = palette(ui.ctx());
        let now = Instant::now();
        ui.spacing_mut().item_spacing.x = 14.0;
        ui.spacing_mut().button_padding = egui::vec2(4.0, 0.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.add(status_label(ui.ctx(), "Log ▴")).clicked() {
                if self.chrome.sheet == Some(Sheet::Diagnostics) {
                    self.close_sheet();
                } else {
                    self.open_diagnostics();
                }
            }
            let view = self.issue_view();
            let count = issue_count_label(&view.visible);
            self.note_issue_count(view.visible.len(), now);
            let pulse = self.issue_count_pulsing(now);
            let response = ui.add(
                egui::Button::new(
                    RichText::new(count)
                        .font(font(Face::Mono, TypeRole::Secondary))
                        .color(if pulse {
                            colours.accent_text
                        } else {
                            colours.text
                        }),
                )
                .fill(egui::Color32::TRANSPARENT)
                .stroke(Stroke::NONE),
            );
            if response.clicked() {
                if self.chrome.drawer == Some(Drawer::Issues) {
                    self.close_drawer();
                } else {
                    self.open_issues();
                }
            }
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                let drivers = self.drivers_line();
                let drivers_button = status_label(ui.ctx(), &drivers);
                if ui
                    .add(drivers_button)
                    .on_hover_text("Input, then the clock output")
                    .clicked()
                {
                    self.open_setup(SetupSection::Devices);
                }
                let buffers = self.buffers_line();
                let buffers_button = status_label(ui.ctx(), &buffers);
                if ui
                    .add(buffers_button)
                    .on_hover_text("Input buffer, then the clock output")
                    .clicked()
                {
                    self.open_setup(SetupSection::Devices);
                }
                let latency = self.status_latency();
                let latency_button = status_label(ui.ctx(), &latency);
                let stale = self
                    .measured_latency
                    .as_ref()
                    .and_then(|measured| measured.stale_reason(&self.latency_conditions()));
                let latency_response = ui.add(latency_button);
                if let Some(reason) = &stale {
                    latency_response.clone().on_hover_text(reason);
                }
                if latency_response.clicked() {
                    self.open_setup(SetupSection::Latency);
                }
                let drift = format!("{:+.1} ppm", self.snapshot.drift_ppm);
                let drift_button = status_label(ui.ctx(), &drift);
                if ui.add(drift_button).clicked() {
                    self.open_setup(SetupSection::Clock);
                }
                let dropouts = dropout_line(self.snapshot.dropouts);
                let dropout_text = status_text(ui.ctx(), &dropouts);
                ui.label(dropout_text).on_hover_text(self.dropout_hover());
                if let Some(load) = self.engine.total_dsp_time() {
                    let load_text =
                        status_text(ui.ctx(), &format!("{:.0} %", load.avg_share() * 100.0));
                    ui.label(load_text);
                }
                if let Some(line) = self.recent_log_line(now) {
                    ui.add(
                        egui::Label::new(
                            RichText::new(line)
                                .font(font(Face::Sans, TypeRole::Secondary))
                                .color(colours.text2),
                        )
                        .truncate(),
                    );
                }
            });
        });
    }

    fn drivers_line(&self) -> String {
        let input = self.active_input();
        let output = self.selected_output();
        let in_rate = if self.snapshot.input_rate > 0 {
            self.snapshot.input_rate
        } else {
            self.sample_rate
        };
        let out_rate = if self.snapshot.output_rate > 0 {
            self.snapshot.output_rate
        } else if self.output_rate > 0 {
            self.output_rate
        } else {
            self.sample_rate
        };
        let rates = if in_rate == out_rate {
            rate_label(in_rate)
        } else {
            format!("{} → {}", rate_label(in_rate), rate_label(out_rate))
        };
        if input.backend == output.backend {
            format!("{} · {rates}", input.backend)
        } else {
            format!("{} → {} · {rates}", input.backend, output.backend)
        }
    }

    fn buffers_line(&self) -> String {
        let input = if self.snapshot.input_block_frames > 0 {
            self.snapshot.input_block_frames as usize
        } else {
            self.buffer_size
        };
        let output = if self.snapshot.output_step_frames > 0 {
            self.snapshot.output_step_frames as usize
        } else {
            self.output_buffer
        };
        if input == output {
            format!("{input} smp")
        } else {
            format!("{input} / {output} smp")
        }
    }

    fn status_latency(&self) -> String {
        let estimated = format_ms(self.latency_ms());
        match self.measured_latency.as_ref() {
            Some(measured) => match measured.stale_reason(&self.latency_conditions()) {
                None => format!(
                    "Estimated {estimated} · Measured {}",
                    format_ms(measured.latency_ms)
                ),
                Some(_) => format!("Estimated {estimated} · stale"),
            },
            None => format!("Estimated {estimated} · not measured"),
        }
    }

    fn dropout_hover(&self) -> String {
        let mut lines = vec![format!(
            "Clock output: {}",
            dropout_line(self.snapshot.dropouts)
        )];
        for (index, extra) in self.extra_outputs.iter().enumerate() {
            let count = self
                .snapshot
                .extra_output_dropouts
                .get(index)
                .copied()
                .unwrap_or(0);
            let name = if extra.name.is_empty() {
                extra.id.as_str()
            } else {
                extra.name.as_str()
            };
            lines.push(format!("{name}: {}", dropout_line(count)));
        }
        lines.join("\n")
    }

    pub(crate) fn sheet_host(&mut self, ctx: &egui::Context) {
        self.tidy_sheet();
        if self.chrome.sheet.is_none() {
            return;
        }
        let colours = palette(ctx);
        let screen = ctx.content_rect();
        let available = (screen.height() - TITLE_H - STATUS_H).max(SHEET_MIN);
        let mut height = self.sheet_draw_height(available);
        let top = screen.bottom() - STATUS_H - height;
        let rect = egui::Rect::from_min_max(
            egui::pos2(screen.left(), top),
            egui::pos2(screen.right(), screen.bottom() - STATUS_H),
        );
        egui::Area::new(egui::Id::unique("sheet"))
            .order(egui::Order::Middle)
            .fixed_pos(rect.min)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(colours.sheet)
                    .stroke(Stroke::new(1.0, colours.line))
                    .inner_margin(Margin::symmetric(16, 8))
                    .show(ui, |ui| {
                        ui.set_width((rect.width() - 32.0).max(0.0));
                        ui.set_min_height((height - 16.0).max(0.0));
                        let handle = ui.allocate_exact_size(
                            Vec2::new(ui.available_width(), 8.0),
                            egui::Sense::drag(),
                        );
                        if handle.1.dragged() {
                            height = (height - handle.1.drag_delta().y)
                                .clamp(SHEET_MIN, SHEET_MAX.min(available));
                            self.chrome.sheet_height = height;
                        }
                        ui.painter().hline(
                            handle.0.x_range(),
                            handle.0.center().y,
                            Stroke::new(2.0, colours.line2),
                        );
                        self.sheet_header(ui);
                        self.sheet_body(ui, (height - 64.0).max(40.0));
                    });
            });
    }

    pub(crate) fn drawer_host(&mut self, ctx: &egui::Context) {
        if self.chrome.drawer.is_none() {
            return;
        }
        let colours = palette(ctx);
        let screen = ctx.content_rect();
        let width = DRAWER_W.min(screen.width() * 0.8);
        let drawer = egui::Rect::from_min_max(
            egui::pos2(screen.right() - width, screen.top()),
            screen.right_bottom(),
        );
        let scrim =
            egui::Rect::from_min_max(screen.min, egui::pos2(drawer.left(), screen.bottom()));
        egui::Area::new(egui::Id::unique("drawer_scrim"))
            .order(egui::Order::Foreground)
            .fixed_pos(screen.min)
            .show(ctx, |ui| {
                ui.painter().rect_filled(
                    scrim,
                    CornerRadius::ZERO,
                    egui::Color32::BLACK.gamma_multiply(0.38),
                );
                if ui
                    .interact(scrim, ui.scope_id().with("scrim"), egui::Sense::click())
                    .clicked()
                {
                    self.close_drawer();
                }
            });
        if self.chrome.drawer.is_none() {
            return;
        }
        egui::Area::new(egui::Id::unique("drawer"))
            .order(egui::Order::Foreground)
            .fixed_pos(drawer.min)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(colours.sheet)
                    .stroke(Stroke::new(1.0, colours.line))
                    .inner_margin(Margin::same(12))
                    .show(ui, |ui| {
                        ui.set_width((width - 24.0).max(0.0));
                        ui.set_min_height(drawer.height());
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new("Issues")
                                    .font(font(Face::Sans, TypeRole::Title))
                                    .color(colours.text)
                                    .strong(),
                            );
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui.button("×").on_hover_text("Close").clicked() {
                                    self.close_drawer();
                                }
                            });
                        });
                        if self.chrome.drawer == Some(Drawer::Issues) {
                            self.issues_strip(ui, width);
                        }
                    });
            });
    }
}

const CLASSIC: &[(&str, Page)] = &[
    ("Live", Page::Live),
    ("Mixer", Page::Mixer),
    ("Routing", Page::Routing),
    ("Plugins", Page::Plugins),
    ("Tuner", Page::Tuner),
];

fn bridge_label(state: EngineState) -> &'static str {
    match state {
        EngineState::Idle => "Stopped",
        EngineState::Starting => "Starting…",
        EngineState::Running => "Running",
        EngineState::Preview => "Demo",
        EngineState::Stopping => "Stopping…",
        EngineState::Error => "Error",
    }
}

fn bar_button<'a>(ctx: &egui::Context, label: &'a str, on: bool) -> egui::Button<'a> {
    let colours = palette(ctx);
    let (fill, text) = if on {
        (colours.accent_bg, colours.accent_text)
    } else {
        (egui::Color32::TRANSPARENT, colours.text)
    };
    egui::Button::new(
        RichText::new(label)
            .font(font(Face::Sans, TypeRole::Control))
            .color(text),
    )
    .fill(fill)
    .stroke(Stroke::new(
        1.0,
        if on { colours.accent } else { colours.line2 },
    ))
    .corner_radius(CornerRadius::same(4))
    .min_size(Vec2::new(32.0, 32.0))
}

fn status_text(ctx: &egui::Context, text: &str) -> RichText {
    RichText::new(text)
        .font(font(Face::Mono, TypeRole::Secondary))
        .color(palette(ctx).text2)
}

fn status_label<'a>(ctx: &egui::Context, text: &'a str) -> egui::Button<'a> {
    egui::Button::new(status_text(ctx, text))
        .fill(egui::Color32::TRANSPARENT)
        .stroke(Stroke::NONE)
}

fn dropout_line(count: u64) -> String {
    if count == 1 {
        "1 dropout".to_owned()
    } else {
        format!("{count} dropouts")
    }
}

fn issue_count_label(issues: &[crate::app::issues::LiveIssue]) -> String {
    if issues.is_empty() {
        "No issues".to_owned()
    } else {
        issue_counts(issues)
    }
}

#[cfg(test)]
mod tests;
