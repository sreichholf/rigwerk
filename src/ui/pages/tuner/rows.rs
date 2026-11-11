//! The all-strings card: one row per preset string, each with its own
//! tolerance band, marker and state.  Harmonic ghosts are dimmed and left out
//! of the in-tune count.

use eframe::egui;
use egui::{Align2, CornerRadius, FontFamily, FontId, Rect, RichText, Sense, Stroke, Vec2};

use crate::audio::{StringReading, TuneState, TunerFrame, Tuning};
use crate::ui::theme::ts;
use crate::ui::theme::{with_alpha, BORDER, CARD, DIM, TEAL, TEXT};
use crate::ui::widgets::{card, micro_label, pill};

use super::{paint_tag, signed_cents, state_color, state_label};

pub(super) fn all_strings_card(
    ui: &mut egui::Ui,
    width: f32,
    frame: &TunerFrame,
    preset: Option<&'static Tuning>,
    reference_hz: f32,
    tolerance: f32,
) {
    card(ui, CARD, BORDER, |ui| {
        ui.set_width(width - 32.0);
        ui.spacing_mut().item_spacing.y = 0.0;
        let total = preset.map(|tuning| tuning.strings.len()).unwrap_or(0);
        let (tuned, total) = super::tuning_count(frame, total);
        ui.horizontal(|ui| {
            ui.label(micro_label("ALL STRINGS", TEAL));
            ui.add_space(4.0);
            ui.label(
                RichText::new("Strum across the strings; each row settles on its own.")
                    .size(ts(9.5))
                    .color(DIM),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(tuning) = preset {
                    pill(ui, &tuning.name.to_uppercase(), DIM);
                    ui.add_space(4.0);
                    pill(ui, &format!("{tuned} / {total} in tune"), TEAL);
                }
            });
        });
        ui.add_space(4.0);

        match preset {
            Some(tuning) => {
                for (index, (name, _)) in tuning.strings.iter().enumerate() {
                    row(
                        ui,
                        name,
                        frame.strings.get(index),
                        tuning.string_hz(index, reference_hz),
                        tolerance,
                    );
                    if index + 1 < tuning.strings.len() {
                        ui.add_space(2.0);
                    }
                }
            }
            None => {
                ui.label(
                    RichText::new("Choose a tuning above to see each string.")
                        .size(ts(9.5))
                        .color(DIM),
                );
            }
        }
    });
}

fn row(
    ui: &mut egui::Ui,
    name: &str,
    reading: Option<&StringReading>,
    fallback_hz: f32,
    tolerance: f32,
) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 30.0), Sense::hover());
    let painter = ui.painter();
    let center_y = rect.center().y;
    let ghost = reading.is_some_and(|row| row.harmonic_of.is_some());
    let sounding = reading.is_some_and(|row| row.sounding);
    let state = reading.map(|row| row.state).unwrap_or(TuneState::Silent);
    let cents = reading.map(|row| row.cents).unwrap_or(0.0);
    let color = if ghost { DIM } else { state_color(state) };
    let text_alpha = if ghost { 110 } else { 255 };
    let target_hz = reading.map(|row| row.target_hz).unwrap_or(fallback_hz);

    if sounding && !ghost {
        painter.rect_filled(
            Rect::from_min_size(
                egui::pos2(rect.left(), center_y - 9.0),
                Vec2::new(3.0, 18.0),
            ),
            CornerRadius::same(2),
            with_alpha(color, 200),
        );
    }
    painter.text(
        egui::pos2(rect.left() + 12.0, center_y),
        Align2::LEFT_CENTER,
        reading.map(|row| row.name).unwrap_or(name),
        FontId::new(ts(10.5), FontFamily::Monospace),
        with_alpha(TEXT, text_alpha),
    );
    painter.text(
        egui::pos2(rect.left() + 52.0, center_y),
        Align2::LEFT_CENTER,
        format!("{target_hz:.2}"),
        FontId::new(ts(9.0), FontFamily::Monospace),
        with_alpha(DIM, text_alpha),
    );
    let track_left = rect.left() + 150.0;
    let track_right = (rect.right() - 194.0).max(track_left + 80.0);
    let mid = (track_left + track_right) / 2.0;
    let half = (track_right - track_left) / 2.0;
    painter.line_segment(
        [
            egui::pos2(track_left, center_y),
            egui::pos2(track_right, center_y),
        ],
        Stroke::new(1.0, BORDER),
    );
    let tolerance_x = half * (tolerance / 50.0).clamp(0.0, 1.0);
    painter.rect_filled(
        Rect::from_min_max(
            egui::pos2(mid - tolerance_x, center_y - 3.0),
            egui::pos2(mid + tolerance_x, center_y + 3.0),
        ),
        CornerRadius::same(3),
        with_alpha(TEAL, 26),
    );
    painter.line_segment(
        [
            egui::pos2(mid, center_y - 5.0),
            egui::pos2(mid, center_y + 5.0),
        ],
        Stroke::new(1.0, with_alpha(TEAL, 110)),
    );
    if sounding {
        let x = mid + (cents.clamp(-50.0, 50.0) / 50.0) * half;
        painter.line_segment(
            [egui::pos2(x, center_y - 7.0), egui::pos2(x, center_y + 7.0)],
            Stroke::new(2.0, if ghost { with_alpha(color, 130) } else { color }),
        );
    }

    let cents_text = if reading.is_some_and(|row| row.sounding) {
        signed_cents(cents)
    } else {
        "—".to_owned()
    };
    painter.text(
        egui::pos2(rect.right() - 98.0, center_y),
        Align2::RIGHT_CENTER,
        cents_text,
        FontId::new(ts(9.5), FontFamily::Monospace),
        if reading.is_some() { color } else { DIM },
    );
    let (label, tag_color) = if ghost {
        ("HARMONIC", DIM)
    } else {
        (state_label(state), color)
    };
    paint_tag(
        painter,
        egui::pos2(rect.right(), center_y),
        label,
        tag_color,
    );
}
