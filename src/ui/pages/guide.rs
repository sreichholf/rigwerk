//! Setup Guide: the steps and platform notes.

use eframe::egui;
use egui::{
    Align, Color32, CornerRadius, FontFamily, Layout, Margin, Rect, RichText, Sense, Stroke, Vec2,
};

use crate::app::RigWerkApp;
use crate::ui::theme::{
    with_alpha, AMBER, BLUE, BORDER, BORDER_SOFT, CARD, CARD_DEEP, GREEN, MUTED, TEAL, TEXT,
};
use crate::ui::widgets::{card, horizontal_line, micro_label, pill, status_dot};

pub(crate) fn draw_step_glyph(ui: &mut egui::Ui, rect: Rect, number: &str, color: Color32) {
    let painter = ui.painter().with_clip_rect(rect);
    painter.rect_filled(rect, CornerRadius::same(7), CARD_DEEP);
    let y = rect.center().y;
    match number {
        "01" => {
            for index in 0..4 {
                let x = rect.left() + 14.0 + index as f32 * 17.0;
                let h = [8.0, 20.0, 28.0, 13.0][index];
                painter.line_segment(
                    [egui::pos2(x, y + h / 2.0), egui::pos2(x, y - h / 2.0)],
                    Stroke::new(2.0, with_alpha(color, 220)),
                );
            }
        }
        "02" => {
            let rect_a = Rect::from_center_size(
                rect.left_center() + Vec2::new(34.0, 0.0),
                Vec2::new(28.0, 18.0),
            );
            let rect_b = Rect::from_center_size(
                rect.right_center() + Vec2::new(-34.0, 0.0),
                Vec2::new(28.0, 18.0),
            );
            painter.rect_stroke(
                rect_a,
                CornerRadius::same(4),
                Stroke::new(1.2, with_alpha(color, 180)),
                egui::StrokeKind::Inside,
            );
            painter.rect_stroke(
                rect_b,
                CornerRadius::same(4),
                Stroke::new(1.2, with_alpha(color, 180)),
                egui::StrokeKind::Inside,
            );
            painter.line_segment(
                [rect_a.right_center(), rect_b.left_center()],
                Stroke::new(1.0, with_alpha(TEXT, 110)),
            );
        }
        _ => {
            let center = rect.center();
            painter.circle_stroke(center, 12.0, Stroke::new(1.1, with_alpha(color, 150)));
            painter.circle_filled(center, 3.0, color);
            painter.line_segment(
                [center, center + Vec2::new(6.0, 0.0)],
                Stroke::new(1.5, color),
            );
        }
    }
}

pub(crate) fn os_card(
    ui: &mut egui::Ui,
    title: &str,
    tagline: &str,
    bullets: &[&str],
    width: f32,
    color: Color32,
) {
    ui.allocate_ui_with_layout(
        Vec2::new(width, 190.0),
        Layout::top_down(Align::LEFT),
        |ui| {
            ui.set_min_size(Vec2::new(width, 190.0));
            card(ui, CARD, BORDER, |ui| {
                ui.set_width(width - 32.0);
                ui.horizontal(|ui| {
                    ui.label(micro_label(title, color));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        status_dot(ui, GREEN);
                    });
                });
                ui.add_space(6.0);
                ui.label(RichText::new(tagline).size(10.5).color(MUTED));
                ui.add_space(8.0);
                horizontal_line(ui, BORDER_SOFT);
                ui.add_space(6.0);
                for bullet in bullets {
                    ui.horizontal(|ui| {
                        status_dot(ui, color);
                        ui.label(RichText::new(*bullet).size(9.3).color(MUTED));
                    });
                }
            });
        },
    );
}

pub(crate) fn note_card(ui: &mut egui::Ui, title: &str, text: &str, color: Color32, width: f32) {
    ui.allocate_ui_with_layout(
        Vec2::new(width, 96.0),
        Layout::top_down(Align::LEFT),
        |ui| {
            ui.set_min_size(Vec2::new(width, 96.0));
            card(ui, CARD_DEEP, BORDER_SOFT, |ui| {
                ui.set_width(width - 32.0);
                ui.horizontal(|ui| {
                    ui.label(micro_label(title, color));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        status_dot(ui, color);
                    });
                });
                ui.add_space(6.0);
                ui.label(RichText::new(text).size(9.8).color(MUTED));
            });
        },
    );
}

impl RigWerkApp {
    pub(crate) fn guide_page(&mut self, ui: &mut egui::Ui) {
        let low_label = self.inventory.low_latency_label.clone();
        let os_badge = format!("{} · {}", self.inventory.os.to_uppercase(), low_label);
        let step_one = "Choose any input";
        let step_one_text = if cfg!(target_os = "windows") {
            "Pick the driver and the input channels carrying your guitar. ASIO, WASAPI, DirectSound or MME — all are listed."
        } else {
            "Pick the driver and the input channels carrying your guitar. JACK, ALSA, PipeWire or PulseAudio — all are listed."
        };
        egui::Frame::new()
            .fill(with_alpha(TEAL, 10))
            .stroke(Stroke::new(1.0, with_alpha(TEAL, 42)))
            .corner_radius(CornerRadius::same(14))
            .inner_margin(Margin::same(18))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("One app. Two clocks.")
                                .size(23.0)
                                .strong()
                                .color(TEXT),
                        );
                        ui.label(
                            RichText::new(
                                "RigWerk sits between the driver your interface insists on and the speakers you actually want to use.",
                            )
                            .size(11.5)
                            .color(MUTED),
                        );
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        pill(ui, &os_badge, TEAL);
                    });
                });
            });

        ui.add_space(12.0);
        let total = ui.available_width();
        let step_w = (total - 16.0) / 3.0;
        ui.horizontal_top(|ui| {
            for (number, title, text, color) in [
                ("01", step_one, step_one_text, TEAL),
                (
                    "02",
                    "Choose any output",
                    "Speakers, headphones, an interface or a virtual cable. It does not need to be the same driver or the same protocol.",
                    BLUE,
                ),
                (
                    "03",
                    "Start the bridge",
                    "The ring buffer absorbs clock drift while both devices keep running at their own rate.",
                    AMBER,
                ),
            ] {
                ui.allocate_ui_with_layout(
                    Vec2::new(step_w, 192.0),
                    Layout::top_down(Align::LEFT),
                    |ui| {
                        ui.set_min_size(Vec2::new(step_w, 192.0));
                        card(ui, CARD, BORDER, |ui| {
                            ui.set_width(step_w - 32.0);
                            ui.label(
                                RichText::new(number)
                                    .family(FontFamily::Monospace)
                                    .size(11.0)
                                    .color(color)
                                    .strong(),
                            );
                            ui.add_space(9.0);
                            ui.label(
                                RichText::new(title)
                                    .size(13.0)
                                    .color(TEXT)
                                    .strong(),
                            );
                            ui.add_space(4.0);
                            ui.label(RichText::new(text).size(10.5).color(MUTED));
                            ui.add_space(10.0);
                            let (rect, _) = ui.allocate_exact_size(
                                Vec2::new(step_w - 32.0, 42.0),
                                Sense::hover(),
                            );
                            draw_step_glyph(ui, rect, number, color);
                        });
                    },
                );
            }
        });

        ui.add_space(12.0);
        let half = (total - 8.0) / 2.0;
        ui.horizontal_top(|ui| {
            os_card(
                ui,
                "WINDOWS",
                "ASIO, WASAPI, DirectSound or MME — on either side.",
                &[
                    "ASIO driver scan reads the Windows registry",
                    "ASIO can now be the output as well as the input",
                    "System-host outputs can stay in shared mode",
                ],
                half,
                TEAL,
            );
            os_card(
                ui,
                "LINUX",
                "Direct ALSA input + PipeWire, PulseAudio or ALSA output.",
                &[
                    "hw:/plughw: inputs are listed on the low-latency side",
                    "Virtual ALSA devices stay on the desktop's sound server",
                    "The bridge opens each side on its own clock",
                ],
                half,
                BLUE,
            );
        });

        ui.add_space(10.0);
        let (latency_title, latency_text) = if cfg!(target_os = "windows") {
            (
                "LATENCY OPTIONS",
                "On a system-host output, the low latency switch runs the WASAPI device at the smallest step its driver allows and queues only that one step, so other apps keep playing. The OUTPUT card's DRIVER line checks the selected driver as soon as the app opens and again when devices change; click \"check again\" after installing a driver. Many drivers only offer their 10 ms default in shared mode; that is the driver's limit, not a RigWerk setting, and the MODE line says so when it applies. Windows' in-box \"High Definition Audio Device\" driver often allows about 3 ms instead. Choose an ASIO output when you want the driver's own low-latency buffer.",
            )
        } else {
            (
                "LINUX LATENCY",
                "The low latency switch asks ALSA for the smallest period the output allows (at least 32 samples) instead of the manual output buffer. Direct hw: outputs can go lower; the default PipeWire or PulseAudio device stays shared, so other apps keep their sound. \"Check driver\" reads the output's supported period range without opening a stream.",
            )
        };
        note_card(ui, latency_title, latency_text, AMBER, total);

        ui.add_space(10.0);
        ui.horizontal_top(|ui| {
            note_card(
                ui,
                "WINDOWS PATH",
                "ASIO capture and ASIO or system playback are wired end to end. Start the bridge with an interface attached to open the real drivers; with no hardware, the same meters animate in preview.",
                GREEN,
                half,
            );
            note_card(
                ui,
                "STILL OPEN",
                "Whether one interface can play while its ASIO driver holds it depends on the driver. The Linux ALSA path is a prototype: its device classification, low-latency period request and driver check are built and tested, but this build has not yet been run with real Linux audio hardware.",
                AMBER,
                half,
            );
        });
    }
}
