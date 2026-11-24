//! The window shell: title bar, status bar, error banner, and the sheet
//! and drawer hosts.
//!
//! The console, the tuner or Setup draws in the middle.  Sessions are the
//! menu; Diagnostics is a sheet.

use std::time::Instant;

use eframe::egui;
use egui::{Align, CornerRadius, Layout, Margin, RichText, Stroke, Vec2};

use crate::app::screen::{format_ms, Drawer, SetupSection, Sheet, SHEET_MIN};
use crate::app::settings::rate_label;
use crate::app::{Page, RigWerkApp};
use crate::audio::{AsioPanelSide, EngineState};
use crate::ui::brand;
use crate::ui::theme::{font, palette, Face, TypeRole};
use crate::ui::widgets::OutlineButton;

const DRAWER_W: f32 = 460.0;
/// How far the sheet's resize handle reaches, above the edge and inside it.
const GRAB: f32 = 10.0;

impl RigWerkApp {
    /// The ASIO control-panel button for one side of the bridge.  Hidden
    /// unless that side is ASIO, disabled until the running bridge has that
    /// driver open; a click only posts a request.
    pub(crate) fn asio_panel_chip(&mut self, ui: &mut egui::Ui, side: AsioPanelSide) {
        let clsid = match side {
            AsioPanelSide::Input => self.active_input().asio_clsid,
            AsioPanelSide::Output => self.selected_output().asio_clsid,
        };
        let Some(clsid) = clsid else {
            return;
        };
        let ready = self.engine.open_asio_driver(side) == Some(clsid);
        let response = OutlineButton::new("Driver panel").enabled(ready).show(ui);
        if !ready {
            let _ = response.on_disabled_hover_text(
                "The driver panel is available once the bridge is running on that ASIO driver.",
            );
        } else if response
            .on_hover_text("Open this ASIO driver's control panel.")
            .clicked()
        {
            self.engine.request_asio_panel(side);
        }
    }

    pub(crate) fn title_bar(&mut self, ui: &mut egui::Ui) {
        let colours = palette(ui.ctx());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.set_height(32.0);
            let mark = brand::texture(ui.ctx(), brand::Mark::Compact);
            let response = ui
                .add(
                    egui::Image::new(egui::load::SizedTexture::new(mark.id(), Vec2::splat(24.0)))
                        .sense(egui::Sense::click())
                        .alt_text("About RigWerk"),
                )
                .on_hover_text("About RigWerk");
            if response.clicked() {
                self.open_about();
            }
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
                self.open_about();
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
        });
    }

    fn screen_button(&mut self, ui: &mut egui::Ui, label: &str, on: bool, page: Page) {
        let widget = bar_button(ui.ctx(), label, on);
        if ui.add(widget).clicked() {
            self.page = if on { Page::Console } else { page };
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
        let (label, on) = if self.mute {
            ("Muted", true)
        } else {
            ("Mute", false)
        };
        let mute = bar_button(ui.ctx(), label, on);
        if ui.add(mute).on_hover_text("Mute the master").clicked() {
            self.mute = !self.mute;
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
                if banner_button(ui, "Rescan").clicked() {
                    self.rescan();
                }
                if banner_button(ui, "Open Setup").clicked() {
                    self.open_setup(SetupSection::Devices);
                }
                if banner_button(ui, "Dismiss").clicked() {
                    self.device_alert = None;
                }
            } else if banner_button(ui, "Dismiss").clicked() {
                self.dismiss_error_banner();
            }
        });
    }

    /// The one line a preset that loaded heavier than the DSP budget gets:
    /// what every path's NAM blocks project together, the heaviest blocks
    /// and their paths, and the one click that bypasses the suggested set.
    /// It never bypasses anything by itself.
    pub(crate) fn nam_banner(&mut self, ui: &mut egui::Ui) {
        let colours = palette(ui.ctx());
        let Some(warning) = self.nam_warning.as_ref() else {
            return;
        };
        let heaviest = warning
            .blocks
            .iter()
            .take(warning.suggested.clamp(1, 3))
            .map(|block| {
                format!(
                    "{} · {} at {:.0} %",
                    block.path,
                    block.name,
                    block.share * 100.0
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let text = format!(
            "The NAM blocks project {:.0} % of the block.  Heaviest: {heaviest}.",
            warning.load * 100.0
        );
        let label = format!("Bypass suggested ({})", warning.suggested);
        let mut bypass = false;
        let mut dismiss = false;
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(text)
                    .font(font(Face::Sans, TypeRole::Body))
                    .color(colours.warn),
            );
            if banner_button(ui, &label).clicked() {
                bypass = true;
            }
            if banner_button(ui, "Dismiss").clicked() {
                dismiss = true;
            }
        });
        if bypass {
            // Checks again: the banner stays while the paths still do not
            // fit.
            self.bypass_suggested_nam();
        } else if dismiss {
            self.nam_warning = None;
        }
    }

    pub(crate) fn banner_open(&self) -> bool {
        self.device_alert.is_some()
            || self.nam_warning.is_some()
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
        // `content_rect` is already the console, under the title bar and
        // above the status bar.  The sheet grows up into that area.
        let screen = ctx.content_rect();
        let available = screen.height().max(1.0);
        let mut height = self.sheet_draw_height(available);
        let top = screen.bottom() - height;
        // Half the grab sits in the gap above the sheet, so the edge
        // itself is the handle.  At the top of the console it stays inside.
        let overhang = GRAB.min((top - screen.top()).max(0.0));
        let origin = egui::pos2(screen.left(), top - overhang);
        egui::Area::new(egui::Id::unique("sheet"))
            .order(egui::Order::Middle)
            .fixed_pos(origin)
            .constrain(false)
            .show(ctx, |ui| {
                ui.set_width(screen.width());
                let fill =
                    egui::Rect::from_min_max(egui::pos2(screen.left(), top), screen.right_bottom());
                // Under the header and the body, so a click on the empty
                // part of the sheet does not land on a strip behind it.
                let _ = ui.interact(fill, ui.scope_id().with("sheet_fill"), egui::Sense::click());
                let (_grab_rect, grab) = ui.allocate_exact_size(
                    Vec2::new(screen.width(), overhang + GRAB),
                    egui::Sense::drag(),
                );
                // Drag only, so the shared hand rule does not cover it.
                // The vertical resize is this edge's own cursor.
                let grab = grab
                    .on_hover_text("Drag to change the height")
                    .on_hover_and_drag_cursor(egui::CursorIcon::ResizeVertical);
                let reported = height;
                grab.widget_info(move || {
                    egui::WidgetInfo::slider(true, f64::from(reported), "Sheet height")
                });
                if grab.dragged() {
                    let low = available.min(SHEET_MIN);
                    height = (height - grab.drag_delta().y).clamp(low, available);
                    self.chrome.sheet_height = height;
                }
                ui.painter()
                    .rect_filled(fill, CornerRadius::ZERO, colours.sheet);
                ui.painter()
                    .hline(fill.x_range(), fill.top(), Stroke::new(1.0, colours.line));
                let grip = egui::Rect::from_center_size(
                    egui::pos2(fill.center().x, fill.top()),
                    Vec2::new(36.0, 4.0),
                );
                ui.painter().rect_filled(
                    grip,
                    CornerRadius::same(2),
                    if grab.hovered() || grab.dragged() {
                        colours.text3
                    } else {
                        colours.line2
                    },
                );
                egui::Frame::new()
                    .inner_margin(Margin::symmetric(16, 4))
                    .show(ui, |ui| {
                        ui.set_width((screen.width() - 32.0).max(0.0));
                        ui.set_min_height((height - GRAB - 8.0).max(0.0));
                        self.sheet_header(ui);
                        self.sheet_body(ui, (height - 56.0).max(40.0));
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
                        let drawer = self.chrome.drawer;
                        ui.horizontal(|ui| {
                            // The browser's own header names its path.
                            if drawer == Some(Drawer::Issues) {
                                ui.label(
                                    RichText::new("Issues")
                                        .font(font(Face::Sans, TypeRole::Title))
                                        .color(colours.text)
                                        .strong(),
                                );
                            }
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if banner_button(ui, "×").on_hover_text("Close").clicked() {
                                    self.close_drawer();
                                }
                            });
                        });
                        match drawer {
                            Some(Drawer::Issues) => self.issues_strip(ui, width),
                            Some(Drawer::Browser) => self.effect_browser(ui),
                            None => {}
                        }
                    });
            });
    }
}

fn banner_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let colours = palette(ui.ctx());
    ui.button(
        RichText::new(label)
            .font(font(Face::Sans, TypeRole::Control))
            .color(colours.text),
    )
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
        return "No issues".to_owned();
    }
    let errors = issues
        .iter()
        .filter(|issue| issue.severity == crate::app::issues::IssueSeverity::Error)
        .count();
    let warnings = issues.len() - errors;
    let plural = |count: usize, word: &str| {
        if count == 1 {
            format!("1 {word}")
        } else {
            format!("{count} {word}s")
        }
    };
    match (errors, warnings) {
        (0, warnings) => plural(warnings, "warning"),
        (errors, 0) => plural(errors, "error"),
        (errors, warnings) => format!(
            "{} · {}",
            plural(errors, "error"),
            plural(warnings, "warning")
        ),
    }
}

#[cfg(test)]
mod tests;
