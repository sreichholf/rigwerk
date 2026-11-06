//! Setup Guide: the steps and platform notes.

use eframe::egui;
use egui::{
    Align, Color32, CornerRadius, FontFamily, Layout, Margin, Rect, RichText, Sense, Stroke, Vec2,
};

use crate::app::RigWerkApp;
use crate::ui::theme::ts;
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
                ui.label(RichText::new(tagline).size(ts(10.5)).color(MUTED));
                ui.add_space(8.0);
                horizontal_line(ui, BORDER_SOFT);
                ui.add_space(6.0);
                for bullet in bullets {
                    ui.horizontal(|ui| {
                        status_dot(ui, color);
                        ui.label(RichText::new(*bullet).size(ts(9.3)).color(MUTED));
                    });
                }
            });
        },
    );
}

pub(crate) fn note_card(ui: &mut egui::Ui, title: &str, text: &str, color: Color32, width: f32) {
    // Tall enough for the latency note and the Windows / Linux path cards.
    let height = 128.0;
    ui.allocate_ui_with_layout(
        Vec2::new(width, height),
        Layout::top_down(Align::LEFT),
        |ui| {
            ui.set_min_size(Vec2::new(width, height));
            card(ui, CARD_DEEP, BORDER_SOFT, |ui| {
                ui.set_width(width - 32.0);
                ui.horizontal(|ui| {
                    ui.label(micro_label(title, color));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        status_dot(ui, color);
                    });
                });
                ui.add_space(6.0);
                ui.label(RichText::new(text).size(ts(9.8)).color(MUTED));
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
            "Pick the driver and the input channels carrying your guitar. PipeWire, direct ALSA, and PulseAudio are listed. JACK appears only when that host is actually available."
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
                                .size(ts(23.0))
                                .strong()
                                .color(TEXT),
                        );
                        ui.label(
                            RichText::new(
                                "RigWerk sits between the driver your interface insists on and the speakers you actually want to use.",
                            )
                            .size(ts(11.5))
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
                                    .size(ts(11.0))
                                    .color(color)
                                    .strong(),
                            );
                            ui.add_space(9.0);
                            ui.label(
                                RichText::new(title)
                                    .size(ts(13.0))
                                    .color(TEXT)
                                    .strong(),
                            );
                            ui.add_space(4.0);
                            ui.label(RichText::new(text).size(ts(10.5)).color(MUTED));
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
                    "ASIO can be the input, the output, or both",
                    "WASAPI followers and VB-Cable publish sit beside the clock device",
                ],
                half,
                TEAL,
            );
            os_card(
                ui,
                "LINUX",
                "PipeWire sources and sinks, plus direct ALSA hardware.",
                &[
                    "Each PipeWire source and sink is its own row",
                    "Followers and RigWerk Out join live when the clock is PipeWire",
                    "hw: and plughw: stay low-latency; the ALSA period is checked against the quantum",
                ],
                half,
                BLUE,
            );
        });

        ui.add_space(10.0);
        let (latency_title, latency_text) = if cfg!(target_os = "windows") {
            (
                "LATENCY OPTIONS",
                "On a WASAPI clock output, pick Shared (normal buffer), REAL-style (smallest shared engine step, other apps keep playing), or Exclusive (this device alone at its exclusive period — often lower than shared, but other apps go silent on that endpoint). ASIO outputs ignore those three and use the driver's own buffer. The OUTPUT card's DRIVER line checks the selected shared-mode driver as soon as the app opens and again when devices change; click \"check again\" after installing a driver. Many drivers only offer their 10 ms default in shared mode; that is the driver's limit, and the MODE line says so when it applies. Windows' in-box \"High Definition Audio Device\" driver often allows about 3 ms shared. If Exclusive is refused, RigWerk falls back to shared output with the chosen buffer.",
            )
        } else {
            (
                "LINUX LATENCY",
                "PipeWire is the primary Linux path. RigWerk opens its own capture and playback nodes and links them to the source and sink you pick, so each device is its own row. On that path the graph is the clock. Low latency sets a 64-frame latency on RigWerk's node (about 1.3 ms at 48 kHz) and, with the same switch, asks the whole graph for a 64-frame quantum. A direct ALSA output still asks for the smallest period the card allows (at least 32 samples), and that period is compared with the quantum: equal or evenly divisible values are a fit, a coarser block or a period that does not divide the quantum is flagged on the OUTPUT card and in the activity log. A quantum above 256 frames is marked as the desktop's shared size. hw: and plughw: bypass the desktop mixer. If PipeWire is not running, the ALSA pipewire device stays in the list.",
            )
        };
        note_card(ui, latency_title, latency_text, AMBER, total);

        ui.add_space(10.0);
        ui.horizontal_top(|ui| {
            note_card(
                ui,
                "WINDOWS PATH",
                "ASIO capture with ASIO or system playback is wired end to end. Mixer can add WASAPI followers of the finished mix; Publish plays into VB-Audio Cable when it is installed (other apps capture CABLE Output). Whether one interface can play while its ASIO driver holds it depends on that driver.",
                GREEN,
                half,
            );
            note_card(
                ui,
                "LINUX PATH",
                "PipeWire is the preferred graph: RigWerk opens its own nodes and can fan the mix to extra sinks plus RigWerk Out. The graph quantum is requested with pw-metadata and follows the desktop's PipeWire settings. Without a PipeWire server, the ALSA pipewire device and direct hw: / plughw: paths remain.",
                BLUE,
                half,
            );
        });
    }
}
