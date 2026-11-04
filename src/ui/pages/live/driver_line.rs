//! One line under each clock row: what the driver really runs at, and why it
//! differs from the chosen setting.  The explanation is in the hover text.
//! On the output side it also offers "Check driver", which asks Windows about
//! the driver without opening any audio.

use eframe::egui;
use egui::{Align, CornerRadius, Layout, RichText, Sense, Stroke, Vec2};

use crate::app::driver_check::{DriverActual, DriverTone};
use crate::app::settings::rate_label;
use crate::ui::theme::ts;
use crate::ui::theme::{with_alpha, AMBER, DIM, GREEN, MUTED};
use crate::ui::widgets::{micro_label, mono};

/// Draws the line.  `checking` is `Some(busy)` only on the output side, where
/// a stand-alone check is possible; the return value says it was clicked.
/// `stopped` makes it plain that a reading was taken without the bridge
/// running, so the value is not mistaken for a live measurement.
pub(crate) fn driver_line(
    ui: &mut egui::Ui,
    actual: Option<&DriverActual>,
    idle: &str,
    stopped: bool,
    checking: Option<bool>,
) -> bool {
    let mut recheck = false;
    ui.horizontal(|ui| {
        ui.label(micro_label("DRIVER", DIM));
        let (text, color, hover) = if let Some(actual) = actual {
            let color = match actual.tone {
                DriverTone::Good => GREEN,
                DriverTone::Notice => AMBER,
            };
            let mut hover = if actual.notes.is_empty() {
                "The driver runs exactly as chosen.".to_owned()
            } else {
                actual.notes.join("\n\n")
            };
            if stopped {
                hover = format!(
                    "Checked while the bridge is stopped: no audio is being played.\n\n{hover}"
                );
            }
            let reading = format!("{} · {} smp", rate_label(actual.rate), actual.frames);
            (
                if stopped {
                    format!("last check: {reading}")
                } else {
                    reading
                },
                color,
                hover,
            )
        } else {
            let hover = if stopped {
                "No reading yet. Check driver asks the sound system which rate and step this output allows; it opens no audio and does not start the bridge."
                    .to_owned()
            } else {
                String::new()
            };
            (idle.to_owned(), MUTED, hover)
        };
        let sense = if checking.is_some() {
            Sense::click()
        } else {
            Sense::hover()
        };
        let response = ui.add(egui::Label::new(mono(text, color, 8.8)).sense(sense));
        let response = if hover.is_empty() {
            response
        } else {
            response.on_hover_text(hover)
        };
        if checking.is_some() && response.clicked() {
            recheck = true;
        }
        if let Some(busy) = checking {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if busy {
                    ui.label(mono("checking driver…".to_owned(), MUTED, 8.0));
                } else {
                    let button = egui::Button::new(
                        RichText::new("Check driver").size(ts(8.0)).color(AMBER),
                    )
                    .fill(with_alpha(AMBER, 14))
                    .stroke(Stroke::new(1.0, with_alpha(AMBER, 50)))
                    .corner_radius(CornerRadius::same(6))
                    .min_size(Vec2::new(0.0, 18.0));
                    if ui
                        .add(button)
                        .on_hover_text(
                            "Ask the sound system which rate and step this output's driver allows. No audio is opened and the bridge stays stopped.",
                        )
                        .clicked()
                    {
                        recheck = true;
                    }
                }
            });
        }
    });
    recheck
}
