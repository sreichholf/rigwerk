//! The logo and the sidebar icons, painted as vectors.

use eframe::egui;
use egui::{Align2, Color32, CornerRadius, FontFamily, FontId, Rect, Sense, Stroke, Vec2};

use crate::ui::theme::ts;
use crate::ui::theme::{with_alpha, BLUE, TEAL};

#[derive(Clone, Copy)]
pub(crate) enum Icon {
    Wave,
    Grid,
    Device,
    Layers,
    Mixer,
    Plug,
    Help,
    Log,
}

pub(crate) fn draw_logo(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(36.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(10), with_alpha(TEAL, 22));
    painter.rect_stroke(
        rect,
        CornerRadius::same(10),
        Stroke::new(1.0, with_alpha(TEAL, 90)),
        egui::StrokeKind::Inside,
    );
    let center = rect.center();
    let points_a: Vec<egui::Pos2> = (0..18)
        .map(|index| {
            let x = rect.left() + 8.0 + index as f32 * 1.1;
            let phase = index as f32 / 17.0 * std::f32::consts::TAU;
            egui::pos2(x, center.y + phase.sin() * 5.0)
        })
        .collect();
    let points_b: Vec<egui::Pos2> = (0..18)
        .map(|index| {
            let x = rect.left() + 8.0 + index as f32 * 1.1;
            let phase = index as f32 / 17.0 * std::f32::consts::TAU + 1.8;
            egui::pos2(x, center.y + phase.sin() * 3.0)
        })
        .collect();
    painter.add(egui::Shape::line(points_a, Stroke::new(1.4, TEAL)));
    painter.add(egui::Shape::line(
        points_b,
        Stroke::new(1.0, with_alpha(BLUE, 190)),
    ));
}

pub(crate) fn paint_icon(painter: &egui::Painter, rect: Rect, icon: Icon, color: Color32) {
    let center = rect.center();
    let stroke = Stroke::new(1.4, color);
    match icon {
        Icon::Wave => {
            for (index, height) in [5.0, 10.0, 15.0, 9.0, 4.0].iter().enumerate() {
                let x = center.x - 10.0 + index as f32 * 5.0;
                painter.line_segment(
                    [
                        egui::pos2(x, center.y - height / 2.0),
                        egui::pos2(x, center.y + height / 2.0),
                    ],
                    Stroke::new(1.5, color),
                );
            }
        }
        Icon::Grid => {
            for x in [-5.0, 5.0] {
                for y in [-5.0, 5.0] {
                    painter.circle_filled(center + Vec2::new(x, y), 2.2, color);
                }
            }
            painter.line_segment(
                [center + Vec2::new(-5.0, 0.0), center + Vec2::new(5.0, 0.0)],
                Stroke::new(1.0, with_alpha(color, 90)),
            );
        }
        Icon::Device => {
            let body = Rect::from_center_size(center, Vec2::new(17.0, 13.0));
            painter.rect_stroke(
                body,
                CornerRadius::same(3),
                stroke,
                egui::StrokeKind::Inside,
            );
            painter.circle_filled(center + Vec2::new(-4.5, 0.0), 2.5, color);
            painter.circle_stroke(center + Vec2::new(4.5, 0.0), 2.5, Stroke::new(1.0, color));
        }
        Icon::Layers => {
            for offset in [4.0, 0.0, -4.0] {
                let layer =
                    Rect::from_center_size(center + Vec2::new(0.0, offset), Vec2::new(16.0, 7.0));
                painter.rect_stroke(
                    layer,
                    CornerRadius::same(2),
                    Stroke::new(1.1, with_alpha(color, 255 - (offset.abs() as u8 * 25))),
                    egui::StrokeKind::Inside,
                );
            }
        }
        Icon::Mixer => {
            for (index, height) in [7.0, 13.0, 9.0, 15.0].iter().enumerate() {
                let x = center.x - 7.5 + index as f32 * 5.0;
                painter.line_segment(
                    [
                        egui::pos2(x, center.y - height / 2.0),
                        egui::pos2(x, center.y + height / 2.0),
                    ],
                    Stroke::new(1.3, color),
                );
                painter.circle_filled(egui::pos2(x, center.y - height / 2.0 + 2.0), 1.8, color);
            }
        }
        Icon::Plug => {
            // A plug: body with two prongs and a cable.
            let body = Rect::from_center_size(center + Vec2::new(0.0, 1.5), Vec2::new(12.0, 9.0));
            painter.rect_stroke(
                body,
                CornerRadius::same(2),
                stroke,
                egui::StrokeKind::Inside,
            );
            for x in [-3.0, 3.0] {
                painter.line_segment(
                    [
                        egui::pos2(center.x + x, body.top()),
                        egui::pos2(center.x + x, body.top() - 4.5),
                    ],
                    stroke,
                );
            }
            painter.line_segment(
                [
                    egui::pos2(center.x, body.bottom()),
                    egui::pos2(center.x, body.bottom() + 3.5),
                ],
                stroke,
            );
        }
        Icon::Help => {
            painter.circle_stroke(center, 8.0, Stroke::new(1.2, color));
            painter.text(
                center,
                Align2::CENTER_CENTER,
                "?",
                FontId::new(ts(12.0), FontFamily::Proportional),
                color,
            );
        }
        Icon::Log => {
            for offset in [-5.0, 0.0, 5.0] {
                painter.line_segment(
                    [
                        center + Vec2::new(-7.0, offset),
                        center + Vec2::new(7.0, offset),
                    ],
                    Stroke::new(1.4, color),
                );
            }
        }
    }
}
