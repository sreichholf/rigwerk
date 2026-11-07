//! Noise-gate inspector: threshold, range, hysteresis, attack/release/hold, slot.

use eframe::egui;
use egui::{Align, Color32, Layout, RichText, Vec2};

use crate::app::RigWerkApp;
use crate::audio::{
    GateSettings, GateSlot, GATE_MAX_ATTACK_MS, GATE_MAX_HOLD_MS, GATE_MAX_HYSTERESIS_DB,
    GATE_MAX_RANGE_DB, GATE_MAX_RELEASE_MS, GATE_MAX_THRESHOLD_DB, GATE_MIN_ATTACK_MS,
    GATE_MIN_HOLD_MS, GATE_MIN_HYSTERESIS_DB, GATE_MIN_RANGE_DB, GATE_MIN_RELEASE_MS,
    GATE_MIN_THRESHOLD_DB,
};
use crate::ui::theme::ts;
use crate::ui::theme::{AMBER, BLUE, DIM, GREEN, MUTED, TEAL, TEXT};
use crate::ui::widgets::{hinted_tab, mono, styled_slider};

/// Wide enough for "HYSTERESIS" in the mono micro label.
const LABEL_W: f32 = 88.0;
const READOUT_W: f32 = 56.0;
/// Gap between label / slider / readout.
const COL_GAP: f32 = 8.0;
/// Breathing room so the grip circle and readout clear the pane edge.
const RIGHT_INSET: f32 = 12.0;

impl RigWerkApp {
    pub(crate) fn gate_inspector(&mut self, ui: &mut egui::Ui) {
        let current = self.path().mix.gate;
        let mut gate = current;
        let mut reset = false;

        ui.horizontal(|ui| {
            ui.label(RichText::new("Gate").size(ts(16.0)).color(TEXT).strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if hinted_tab(
                    ui,
                    "RESET",
                    false,
                    MUTED,
                    "Restore threshold, range, timing and slot to the defaults. On/off is kept.",
                ) {
                    gate = GateSettings {
                        enabled: gate.enabled,
                        ..GateSettings::default()
                    };
                    reset = true;
                }
                ui.add_space(4.0);
                if hinted_tab(
                    ui,
                    if gate.enabled { "ON" } else { "BYP" },
                    gate.enabled,
                    AMBER,
                    "Switch the gate on or off. Settings are kept while bypassed.",
                ) {
                    gate.enabled = !gate.enabled;
                }
            });
        });
        ui.label(
            RichText::new("Causal · no added latency · first transient can leak before it opens")
                .size(ts(11.0))
                .color(DIM),
        );
        ui.add_space(10.0);

        // One width for every row so tracks and readouts share a right edge,
        // with a small inset from the inspector pane border.
        let panel_w = ui.available_width();
        let row_w = (panel_w - RIGHT_INSET).max(200.0);
        let slider_w = (row_w - LABEL_W - READOUT_W - COL_GAP * 2.0).max(80.0);

        ui.allocate_ui_with_layout(Vec2::new(row_w, 0.0), Layout::top_down(Align::LEFT), |ui| {
            ui.set_width(row_w);

            ui.horizontal(|ui| {
                ui.label(RichText::new("Slot").size(ts(11.0)).color(MUTED));
                ui.add_space(8.0);
                for slot in GateSlot::ALL {
                    let label = match slot {
                        GateSlot::Pre => "PRE",
                        GateSlot::Post => "POST",
                    };
                    let hint = match slot {
                        GateSlot::Pre => {
                            "Before inserts: gates single-coil / interface hiss into amp sims."
                        }
                        GateSlot::Post => {
                            "After inserts (default): gates idle roar / gain-stage hiss."
                        }
                    };
                    if hinted_tab(ui, label, gate.slot == slot, TEAL, hint) {
                        gate.slot = slot;
                    }
                }
            });
            ui.add_space(10.0);

            knob_row(
                ui,
                row_w,
                slider_w,
                "THRESHOLD",
                &mut gate.threshold_db,
                GATE_MIN_THRESHOLD_DB,
                GATE_MAX_THRESHOLD_DB,
                0.5,
                AMBER,
                Unit::Db,
                "Open threshold. Signal above this opens a closed gate.",
            );
            knob_row(
                ui,
                row_w,
                slider_w,
                "RANGE",
                &mut gate.range_db,
                GATE_MIN_RANGE_DB,
                GATE_MAX_RANGE_DB,
                1.0,
                AMBER,
                Unit::Db,
                "Gain when fully closed. 0 dB is pass-through; −80 dB is near mute.",
            );
            knob_row(
                ui,
                row_w,
                slider_w,
                "HYSTERESIS",
                &mut gate.hysteresis_db,
                GATE_MIN_HYSTERESIS_DB,
                GATE_MAX_HYSTERESIS_DB,
                0.5,
                GREEN,
                Unit::Db,
                "Close threshold sits this far below the open threshold. Stops chatter.",
            );
            knob_row(
                ui,
                row_w,
                slider_w,
                "ATTACK",
                &mut gate.attack_ms,
                GATE_MIN_ATTACK_MS,
                GATE_MAX_ATTACK_MS,
                0.1,
                TEAL,
                Unit::Ms,
                "Gain time constant while opening (~63% of the remaining gap).",
            );
            knob_row(
                ui,
                row_w,
                slider_w,
                "RELEASE",
                &mut gate.release_ms,
                GATE_MIN_RELEASE_MS,
                GATE_MAX_RELEASE_MS,
                1.0,
                BLUE,
                Unit::Ms,
                "Gain time constant while closing.",
            );
            knob_row(
                ui,
                row_w,
                slider_w,
                "HOLD",
                &mut gate.hold_ms,
                GATE_MIN_HOLD_MS,
                GATE_MAX_HOLD_MS,
                1.0,
                MUTED,
                Unit::Ms,
                "Keep the gate open this long after the detector drops below the close threshold.",
            );

            ui.add_space(10.0);
            ui.label(
                RichText::new(match gate.slot {
                    GateSlot::Pre => "PRE · before inserts",
                    GateSlot::Post => "POST · after inserts",
                })
                .size(ts(10.5))
                .color(MUTED),
            );
        });

        let gate = gate.sanitized();
        if gate != current {
            if reset {
                self.push_log("Gate reset to defaults.", AMBER);
            } else if gate.enabled != current.enabled {
                self.push_log(
                    if gate.enabled {
                        "Gate on.".to_owned()
                    } else {
                        "Gate bypassed.".to_owned()
                    },
                    AMBER,
                );
            } else if gate.slot != current.slot {
                self.push_log(
                    match gate.slot {
                        GateSlot::Pre => "Gate slot: PRE inserts.",
                        GateSlot::Post => "Gate slot: POST inserts.",
                    },
                    AMBER,
                );
            }
            self.path_mut().mix.gate = gate;
            self.push_paths_to_engine();
        }
    }
}

enum Unit {
    Db,
    Ms,
}

impl Unit {
    fn format(self, value: f32) -> String {
        match self {
            Self::Db => format!("{value:.0} dB"),
            Self::Ms if value < 10.0 => format!("{value:.1} ms"),
            Self::Ms => format!("{value:.0} ms"),
        }
    }
}

/// Fixed-width row: label | slider | readout. Every row shares `row_w` /
/// `slider_w` so tracks end on one vertical line and the readout sits just
/// inside the inspector's right inset.
#[allow(clippy::too_many_arguments)]
fn knob_row(
    ui: &mut egui::Ui,
    row_w: f32,
    slider_w: f32,
    label: &str,
    value: &mut f32,
    min: f32,
    max: f32,
    step: f32,
    color: Color32,
    unit: Unit,
    hint: &str,
) {
    ui.allocate_ui_with_layout(
        Vec2::new(row_w, 26.0),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.set_width(row_w);
            ui.allocate_ui_with_layout(
                Vec2::new(LABEL_W, 22.0),
                Layout::left_to_right(Align::Center),
                |ui| {
                    ui.set_width(LABEL_W);
                    ui.label(mono(label, DIM, 8.0));
                },
            );
            ui.add_space(COL_GAP);
            ui.allocate_ui_with_layout(
                Vec2::new(slider_w, 22.0),
                Layout::left_to_right(Align::Center),
                |ui| {
                    ui.set_width(slider_w);
                    let _ = styled_slider(ui, value, min, max, step, (color, label.to_owned()))
                        .on_hover_text(hint);
                },
            );
            ui.add_space(COL_GAP);
            ui.allocate_ui_with_layout(
                Vec2::new(READOUT_W, 22.0),
                Layout::right_to_left(Align::Center),
                |ui| {
                    ui.set_width(READOUT_W);
                    ui.label(mono(unit.format(*value), MUTED, 9.0));
                },
            );
        },
    );
    ui.add_space(4.0);
}
