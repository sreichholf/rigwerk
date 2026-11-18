//! One line under each clock row: what the driver really runs at, and why it
//! differs from the chosen setting.  The explanation is in the hover text.
//! It is a reading only: no amber and no button.  On the output side a
//! click on the line asks the sound system about the driver again without
//! opening any audio; "Output driver not checked yet" and the quantum fit
//! are Live Issues rows (`app::issues`), not this line.

use eframe::egui;
use egui::{Align, Color32, Layout, RichText, Sense};

use crate::app::driver_check::{DriverActual, DriverTone};
use crate::app::settings::rate_label;
use crate::ui::theme::{font, palette, Face, TypeRole};

fn reading(text: impl Into<String>, colour: Color32) -> RichText {
    RichText::new(text)
        .font(font(Face::Mono, TypeRole::Secondary))
        .color(colour)
}

/// Draws the line.  `checking` is `Some(busy)` only on the output side, where
/// a stand-alone check is possible; the return value says the line was
/// clicked to check again.
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
    let colours = palette(ui.ctx());
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("Driver")
                .font(font(Face::Sans, TypeRole::Secondary))
                .color(colours.text3),
        );
        let (text, color, hover) = if let Some(actual) = actual {
            let color = match actual.tone {
                DriverTone::Good => colours.text,
                DriverTone::Notice => colours.text2,
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
            if checking.is_some() {
                hover.push_str("\n\nClick to check the driver again.");
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
            let hover = if stopped && checking.is_some() {
                "No reading yet. Click to ask the sound system which rate and step this output allows; it opens no audio and does not start the bridge."
                    .to_owned()
            } else {
                String::new()
            };
            (idle.to_owned(), colours.text2, hover)
        };
        let sense = if checking.is_some() {
            Sense::click()
        } else {
            Sense::hover()
        };
        let response = ui.add(egui::Label::new(reading(text, color)).sense(sense));
        let response = if hover.is_empty() {
            response
        } else {
            response.on_hover_text(hover)
        };
        if checking == Some(false) && response.clicked() {
            recheck = true;
        }
        if checking == Some(true) {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(reading("checking driver…", colours.text2));
            });
        }
    });
    recheck
}
