//! The signal scopes and their readouts.

use eframe::egui;
use egui::{Align2, Color32, CornerRadius, FontFamily, FontId, Rect, Sense, Stroke, Vec2};

use crate::ui::theme::{with_alpha, AMBER, BLUE, BORDER_SOFT, CARD_DEEP, DIM, MUTED, TEAL, TEXT};

pub(crate) fn draw_scope(
    ui: &egui::Ui,
    rect: Rect,
    ctx: &egui::Context,
    in_l: f32,
    _in_r: f32,
    out_l: f32,
    _out_r: f32,
    active: bool,
) {
    let painter = ui.painter().with_clip_rect(rect);
    painter.rect_filled(rect, CornerRadius::same(9), CARD_DEEP);
    painter.rect_stroke(
        rect,
        CornerRadius::same(9),
        Stroke::new(1.0, BORDER_SOFT),
        egui::StrokeKind::Inside,
    );

    for step in 1..8 {
        let x = rect.left() + rect.width() * step as f32 / 8.0;
        painter.line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            Stroke::new(0.6, Color32::from_rgb(22, 28, 38)),
        );
    }
    for step in 1..4 {
        let y = rect.top() + rect.height() * step as f32 / 4.0;
        painter.line_segment(
            [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
            Stroke::new(0.6, Color32::from_rgb(22, 28, 38)),
        );
    }
    painter.line_segment(
        [
            egui::pos2(rect.left(), rect.center().y),
            egui::pos2(rect.right(), rect.center().y),
        ],
        Stroke::new(0.7, Color32::from_rgb(34, 42, 55)),
    );

    let time = ctx.input(|input| input.time) as f32;
    let make_points = |peak: f32, phase_shift: f32, color: Color32, glow: bool| {
        let amp = if active {
            (0.05 + peak * 0.78).min(0.88) * rect.height() * 0.43
        } else {
            rect.height() * 0.018
        };
        let points: Vec<egui::Pos2> = (0..180)
            .map(|index| {
                let t = index as f32 / 179.0;
                let x = rect.left() + rect.width() * t;
                let envelope =
                    (0.75 + 0.25 * (t * 7.4 + time * 1.7).sin()) * (1.0 - (t - 0.5).abs() * 0.35);
                let phase = t * std::f32::consts::TAU * 2.35 + time * 3.0 + phase_shift;
                let signal = phase.sin() * 0.64
                    + (phase * 2.0 + 0.6).sin() * 0.22
                    + (phase * 3.0 + 1.3).sin() * 0.09;
                egui::pos2(x, rect.center().y + signal * amp * envelope)
            })
            .collect();
        if glow {
            painter.add(egui::Shape::line(
                points.clone(),
                Stroke::new(5.0, with_alpha(color, 18)),
            ));
        }
        painter.add(egui::Shape::line(points, Stroke::new(1.35, color)));
    };

    if active {
        make_points(in_l, 0.0, with_alpha(TEAL, 205), true);
        make_points(out_l, 1.8, with_alpha(BLUE, 190), true);
    } else {
        make_points(0.0, 0.0, with_alpha(TEAL, 80), false);
        make_points(0.0, 1.8, with_alpha(BLUE, 70), false);
    }

    let play_x = rect.left() + rect.width() * (0.18 + (time * 0.055).fract() * 0.64);
    painter.line_segment(
        [
            egui::pos2(play_x, rect.top() + 4.0),
            egui::pos2(play_x, rect.bottom() - 4.0),
        ],
        Stroke::new(0.7, with_alpha(TEXT, 32)),
    );
    painter.text(
        rect.right_top() + Vec2::new(-9.0, 8.0),
        Align2::RIGHT_TOP,
        if active { "LIVE" } else { "IDLE" },
        FontId::new(8.5, FontFamily::Monospace),
        if active { with_alpha(TEAL, 180) } else { DIM },
    );
}

pub(crate) fn draw_mini_scope(ui: &egui::Ui, rect: Rect, ctx: &egui::Context, active: bool) {
    let painter = ui.painter().with_clip_rect(rect);
    painter.rect_filled(rect, CornerRadius::same(9), CARD_DEEP);
    painter.rect_stroke(
        rect,
        CornerRadius::same(9),
        Stroke::new(1.0, BORDER_SOFT),
        egui::StrokeKind::Inside,
    );
    for step in 1..6 {
        let x = rect.left() + rect.width() * step as f32 / 6.0;
        painter.line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            Stroke::new(0.6, Color32::from_rgb(24, 30, 40)),
        );
    }
    let time = ctx.input(|input| input.time) as f32;
    if active {
        let points: Vec<egui::Pos2> = (0..96)
            .map(|index| {
                let t = index as f32 / 95.0;
                let x = rect.left() + rect.width() * t;
                let envelope = (0.4 + 0.6 * (time * 1.4 + t * 3.0).sin().abs())
                    * (1.0 - (t - 0.5).abs() * 0.8);
                let phase = t * std::f32::consts::TAU * 1.8 + time * 2.4;
                let signal = phase.sin() * 0.66 + (phase * 2.0).sin() * 0.18;
                egui::pos2(
                    x,
                    rect.center().y + signal * rect.height() * 0.28 * envelope,
                )
            })
            .collect();
        painter.add(egui::Shape::line(
            points.clone(),
            Stroke::new(6.0, with_alpha(AMBER, 16)),
        ));
        painter.add(egui::Shape::line(points, Stroke::new(1.4, AMBER)));
        let pulse = (time * 0.9).fract();
        painter.circle_filled(
            egui::pos2(rect.left() + rect.width() * pulse, rect.center().y),
            2.4,
            with_alpha(AMBER, 220),
        );
    } else {
        let points: Vec<egui::Pos2> = (0..96)
            .map(|index| {
                let x = rect.left() + rect.width() * index as f32 / 95.0;
                egui::pos2(x, rect.center().y)
            })
            .collect();
        painter.add(egui::Shape::line(
            points,
            Stroke::new(1.0, with_alpha(AMBER, 60)),
        ));
    }
    painter.text(
        rect.left_top() + Vec2::new(8.0, 7.0),
        Align2::LEFT_TOP,
        "AUDIO",
        FontId::new(8.0, FontFamily::Monospace),
        DIM,
    );
    painter.text(
        rect.right_top() + Vec2::new(-8.0, 7.0),
        Align2::RIGHT_TOP,
        if active { "FLOWING" } else { "PAUSED" },
        FontId::new(8.0, FontFamily::Monospace),
        if active { with_alpha(AMBER, 190) } else { DIM },
    );
}

pub(crate) fn scope_stats(
    ui: &mut egui::Ui,
    width: f32,
    input: f32,
    output: f32,
    latency_ms: f32,
    frames: u64,
) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 35.0), Sense::hover());
    let column = rect.width() / 4.0;
    for (index, (label, value, color)) in [
        ("PEAK IN", format!("{:.1} dB", amp_to_db(input)), TEAL),
        ("PEAK OUT", format!("{:.1} dB", amp_to_db(output)), BLUE),
        ("ROUND TRIP", format!("{latency_ms:.1} ms"), AMBER),
        (
            "SAMPLES",
            if frames == 0 {
                "—".to_owned()
            } else {
                format_compact(frames)
            },
            MUTED,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let left = rect.left() + column * index as f32;
        ui.painter().text(
            egui::pos2(left, rect.top() + 2.0),
            Align2::LEFT_TOP,
            label,
            FontId::new(7.8, FontFamily::Proportional),
            DIM,
        );
        ui.painter().text(
            egui::pos2(left, rect.top() + 12.0),
            Align2::LEFT_TOP,
            &value,
            FontId::new(10.0, FontFamily::Monospace),
            color,
        );
        let bar = Rect::from_min_size(
            egui::pos2(left, rect.bottom() - 4.0),
            Vec2::new((column - 18.0).max(22.0), 2.0),
        );
        ui.painter()
            .rect_filled(bar, CornerRadius::same(1), Color32::from_rgb(25, 32, 43));
        let fill = if index < 2 {
            (value
                .split(' ')
                .next()
                .and_then(|value| value.parse::<f32>().ok())
                .map(|db| (db + 48.0) / 48.0)
                .unwrap_or(0.0))
            .clamp(0.0, 1.0)
        } else if index == 2 {
            (latency_ms / 25.0).clamp(0.0, 1.0)
        } else {
            (frames as f32 / 1_000_000.0).clamp(0.0, 1.0)
        };
        ui.painter().rect_filled(
            Rect::from_min_size(bar.min, Vec2::new(bar.width() * fill, bar.height())),
            CornerRadius::same(1),
            with_alpha(color, 205),
        );
    }
}

pub(crate) fn amp_to_db(value: f32) -> f32 {
    if value <= 0.0001 {
        -60.0
    } else {
        (20.0 * value.log10()).clamp(-60.0, 0.0)
    }
}

pub(crate) fn format_compact(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}M", value as f32 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}k", value as f32 / 1_000.0)
    } else {
        format!("{value}")
    }
}
