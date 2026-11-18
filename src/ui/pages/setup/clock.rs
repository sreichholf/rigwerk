//! Drift, drift correction, the bridge buffer and dropouts.

use eframe::egui;
use egui::{Align, FontFamily, FontId, Layout, Rect, RichText};

use crate::app::screen::SetupSection;
use crate::app::settings::{BRIDGE_BUFFER_MS_MAX, BRIDGE_BUFFER_MS_MIN};
use crate::app::{LogTone, RigWerkApp};
use crate::audio::{BufferMode, EngineState};
use crate::ui::pages::setup::{hint, panel};
use crate::ui::theme::ts;
use crate::ui::theme::{palette, AMBER, DIM, TEXT};
use crate::ui::widgets::{hinted_tab, micro_label, mono, segmented, styled_slider};

impl RigWerkApp {
    pub(super) fn setup_clock(&mut self, ui: &mut egui::Ui, section: Option<SetupSection>) {
        let direct = self.engine.state == EngineState::Running && self.snapshot.direct;
        let fill = self.bridge_fill(ui.ctx());
        let dropouts = self.bridge_dropouts();
        let drift = if self.engine.state == EngineState::Running {
            format!("{:+.1} ppm", self.snapshot.drift_ppm)
        } else {
            "—".to_owned()
        };
        panel(ui, "Clock", section, Some(SetupSection::Clock), |ui| {
            ui.horizontal(|ui| {
                hint(ui, "Drift");
                ui.label(RichText::new(drift).color(palette(ui.ctx()).text));
            });
            ui.add_space(6.0);
            hint(ui, "Drift correction");
            ui.add_space(4.0);
            let mut index = usize::from(!self.drift);
            if segmented(ui, &mut index, &["On", "Off"]).changed() {
                self.drift = index == 0;
                self.push_log(
                    if self.drift {
                        "Drift correction on."
                    } else {
                        "Drift correction off."
                    },
                    if self.drift {
                        LogTone::Green
                    } else {
                        LogTone::Amber
                    },
                );
            }
            ui.add_space(8.0);
            if direct {
                hint(ui, "One clock: no bridge buffer needed.");
            } else {
                hint(ui, "Bridge buffer");
                ui.add_space(4.0);
                let mut mode = self.bridge_buffer_mode;
                bridge_buffer_mode_picker(ui, &mut mode);
                if mode != self.bridge_buffer_mode {
                    self.bridge_buffer_mode = mode;
                    self.push_log(
                        match mode {
                            BufferMode::Auto => "Bridge buffer: Auto.".to_owned(),
                            BufferMode::Minimum => "Bridge buffer: Minimum.".to_owned(),
                            BufferMode::Fixed => {
                                format!("Bridge buffer: Fixed, {:.1} ms.", self.bridge_buffer_ms)
                            }
                        },
                        LogTone::Amber,
                    );
                    self.apply_bridge_buffer();
                }
                if self.bridge_buffer_mode == BufferMode::Fixed {
                    let mut ms = self.bridge_buffer_ms;
                    let response = fixed_bridge_buffer_size(ui, &mut ms);
                    if response.changed() {
                        self.bridge_buffer_ms = ms;
                        self.apply_bridge_buffer();
                    }
                    if response.drag_stopped() || (response.changed() && !response.dragged()) {
                        self.push_log(
                            format!("Bridge buffer fixed at {:.1} ms.", self.bridge_buffer_ms),
                            LogTone::Amber,
                        );
                    }
                }
                ui.add_space(6.0);
                hint(
                    ui,
                    &format!("Fill {:.0}% · {}", fill * 100.0, dropout_text(dropouts)),
                );
            }
            if direct {
                ui.add_space(4.0);
                hint(ui, &dropout_text(dropouts));
            }
        });
    }
}

fn dropout_text(count: u64) -> String {
    match count {
        1 => "1 dropout".to_owned(),
        n => format!("{n} dropouts"),
    }
}

/// Auto, Minimum, Fixed, in the order they are offered.
fn bridge_buffer_choices() -> [(BufferMode, &'static str, &'static str); 3] {
    [
        (
            BufferMode::Auto,
            "Auto",
            "Twice the input buffer, at least two output blocks. Grows 256 samples per dropout, shrinks 64 after 4 calm seconds.",
        ),
        (
            BufferMode::Minimum,
            "Minimum",
            "One input block plus one output block, the least that can work. Recovers from dropouts in smaller, quicker steps.",
        ),
        (
            BufferMode::Fixed,
            "Fixed",
            "A fixed size that never grows on its own. Lower it until dropouts start, then go one notch back up.",
        ),
    ]
}

/// Width of the three mode tabs on one row, including the gaps between them.
fn bridge_buffer_tab_row_width(ui: &egui::Ui) -> f32 {
    let font = FontId::new(ts(8.2), FontFamily::Proportional);
    let pad = ui.spacing().button_padding.x * 2.0;
    let gap = ui.spacing().item_spacing.x;
    let mut width = 0.0;
    for (index, (_, label, _)) in bridge_buffer_choices().iter().enumerate() {
        if index > 0 {
            width += gap;
        }
        let text = ui
            .painter()
            .layout_no_wrap((*label).to_owned(), font.clone(), TEXT);
        width += text.size().x + pad;
    }
    width
}

/// The three tabs fit the strip when this is true.  What is left of the line
/// after the readings is the width that matters.
fn bridge_buffer_modes_fit(ui: &egui::Ui) -> bool {
    bridge_buffer_tab_row_width(ui) <= ui.available_width()
}

/// Tabs when they fit.  A combo of the same three modes when they do not, so
/// the last label is not clipped.
fn bridge_buffer_mode_picker(ui: &mut egui::Ui, mode: &mut BufferMode) -> Rect {
    let choices = bridge_buffer_choices();
    if bridge_buffer_modes_fit(ui) {
        ui.horizontal(|ui| {
            for (option, label, hint) in choices {
                if hinted_tab(ui, label, *mode == option, AMBER, hint) {
                    *mode = option;
                }
            }
        })
        .response
        .rect
    } else {
        let (_, label, hint) = choices
            .iter()
            .find(|(option, _, _)| *option == *mode)
            .copied()
            .unwrap_or(choices[0]);
        let available = ui.available_width();
        egui::ComboBox::from_id_salt("bridge_buffer_mode")
            .width(available)
            .truncate()
            .selected_text(RichText::new(label).size(ts(8.2)).color(AMBER).strong())
            .show_ui(ui, |ui| {
                for (option, label, hint) in choices {
                    ui.selectable_value(mode, option, label).on_hover_text(hint);
                }
            })
            .response
            .on_hover_text(hint)
            .rect
    }
}

/// The fixed size.  The number and the slider; the caller stores and applies it.
fn fixed_bridge_buffer_size(ui: &mut egui::Ui, ms: &mut f32) -> egui::Response {
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(micro_label("SIZE", DIM));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(mono(format!("{ms:.1} ms"), TEXT, 10.0));
        });
    });
    styled_slider(
        ui,
        ms,
        BRIDGE_BUFFER_MS_MIN,
        BRIDGE_BUFFER_MS_MAX,
        0.5,
        AMBER,
    )
}

#[cfg(test)]
mod tests;
