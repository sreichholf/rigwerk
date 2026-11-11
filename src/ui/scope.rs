//! Live scope traces and the peak / latency readouts under them.
//!
//! The curves are decimated samples from the capture and playback paths
//! (peak-hold bins), not a decorative oscillator.

use eframe::egui;
use egui::{Align2, Color32, CornerRadius, FontFamily, FontId, Rect, Sense, Stroke, Vec2};

use crate::ui::theme::ts;
use crate::ui::theme::{with_alpha, AMBER, BLUE, BORDER_SOFT, CARD_DEEP, DIM, MUTED, TEAL};

/// Caption for the Live waveform card.
pub(crate) const LEVEL_PREVIEW_LABEL: &str = "SCOPE";

/// Badge drawn on the sketch.
pub(crate) fn scope_badge(active: bool) -> &'static str {
    if active {
        LEVEL_PREVIEW_LABEL
    } else {
        "IDLE"
    }
}

pub(crate) fn draw_scope(ui: &egui::Ui, rect: Rect, input: &[f32], output: &[f32], active: bool) {
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

    if active {
        paint_trace(
            &painter,
            rect,
            input,
            with_alpha(TEAL, 205),
            with_alpha(TEAL, 18),
            true,
        );
        paint_trace(
            &painter,
            rect,
            output,
            with_alpha(BLUE, 190),
            with_alpha(BLUE, 16),
            true,
        );
    } else {
        paint_trace(
            &painter,
            rect,
            &[],
            with_alpha(TEAL, 80),
            Color32::TRANSPARENT,
            false,
        );
    }

    painter.text(
        rect.right_top() + Vec2::new(-9.0, 8.0),
        Align2::RIGHT_TOP,
        scope_badge(active),
        FontId::new(ts(8.5), FontFamily::Monospace),
        if active { MUTED } else { DIM },
    );
}

fn paint_trace(
    painter: &egui::Painter,
    rect: Rect,
    samples: &[f32],
    color: Color32,
    glow: Color32,
    glow_on: bool,
) {
    let points = if samples.is_empty() {
        vec![
            egui::pos2(rect.left(), rect.center().y),
            egui::pos2(rect.right(), rect.center().y),
        ]
    } else {
        let last = (samples.len() - 1).max(1) as f32;
        let half = rect.height() * 0.45;
        samples
            .iter()
            .enumerate()
            .map(|(index, sample)| {
                let t = index as f32 / last;
                let x = rect.left() + rect.width() * t;
                // Linear amplitude: ±1.0 fills the card. Quiet signals look quiet.
                let y = rect.center().y - sample.clamp(-1.0, 1.0) * half;
                egui::pos2(x, y)
            })
            .collect()
    };
    if glow_on {
        painter.add(egui::Shape::line(points.clone(), Stroke::new(5.0, glow)));
    }
    painter.add(egui::Shape::line(points, Stroke::new(1.35, color)));
}

/// Scope readout for RigWerk's own path. Not a round trip; path delay past our
/// node is shown beside it when PipeWire reports one.
pub(crate) const BRIDGE_LATENCY_LABEL: &str = "US";
pub(crate) const PATH_LATENCY_LABEL: &str = "US · AFTER";

/// `peak_in` and `peak_out` are linear amplitudes from the peak meters the
/// caller already holds.
/// `path_delay_ms` is graph/device delay after our playback node when known.
pub(crate) fn scope_stats(
    ui: &mut egui::Ui,
    width: f32,
    peak_in: f32,
    peak_out: f32,
    latency_ms: f32,
    path_delay_ms: Option<f32>,
    frames: u64,
) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 35.0), Sense::hover());
    let column = rect.width() / 4.0;
    let (latency_label, latency_value, latency_bar_ms) = match path_delay_ms {
        Some(after) => (
            PATH_LATENCY_LABEL,
            format!("{latency_ms:.1} · {after:.1}"),
            latency_ms + after,
        ),
        None => (
            BRIDGE_LATENCY_LABEL,
            format!("{latency_ms:.1} ms"),
            latency_ms,
        ),
    };
    for (index, (label, value, color)) in [
        ("PEAK IN", format!("{:.1} dB", amp_to_db(peak_in)), TEAL),
        ("PEAK OUT", format!("{:.1} dB", amp_to_db(peak_out)), BLUE),
        (latency_label, latency_value, AMBER),
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
            FontId::new(ts(7.5), FontFamily::Monospace),
            DIM,
        );
        ui.painter().text(
            egui::pos2(left, rect.top() + 14.0),
            Align2::LEFT_TOP,
            value,
            FontId::new(ts(10.0), FontFamily::Monospace),
            color,
        );
        let bar = Rect::from_min_size(
            egui::pos2(left, rect.bottom() - 4.0),
            Vec2::new(column - 10.0, 2.0),
        );
        ui.painter()
            .rect_filled(bar, CornerRadius::same(1), Color32::from_rgb(28, 34, 46));
        let fill = if index < 2 {
            let peak = if index == 0 { peak_in } else { peak_out };
            level_display_height(peak)
        } else if index == 2 {
            // BT path delay is often >100 ms; keep the bar meaningful past the old 25 ms scale.
            (latency_bar_ms / 200.0).clamp(0.0, 1.0)
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

/// Stretch linear amplitude onto the same −48…0 dBFS range the PEAK bars use.
pub(crate) fn level_display_height(amp: f32) -> f32 {
    ((amp_to_db(amp.clamp(0.0, 1.0)) + 48.0) / 48.0).clamp(0.0, 1.0)
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

#[cfg(test)]
mod tests;
